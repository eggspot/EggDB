use crate::auth::TdsCredentials;
use crate::connection::{refuse_too_many, serve_connection};
use crate::wire::Wire;
use eggdb_sql::KeyValueStore;
use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The connections a server serves at once unless told otherwise.
pub const DEFAULT_MAX_CONNECTIONS: usize = 64;
/// How long a client has from connecting to finishing its login, unless told otherwise.
pub const DEFAULT_AUTHENTICATION_TIMEOUT: Duration = Duration::from_secs(30);
/// The most connections still logging in at once; more are refused with SQL Server's error
/// 17809, so slow or failing logins cannot take every slot.
pub const MAX_UNAUTHENTICATED_CONNECTIONS: usize = 16;
const MAX_REFUSING_THREADS: usize = 8;
pub(crate) const REFUSAL_TIMEOUT: Duration = Duration::from_secs(2);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
const DELAY_PER_RECENT_FAILURE: Duration = Duration::from_millis(250);
const LONGEST_FAILURE_DELAY: Duration = Duration::from_secs(5);

/// What a server serves and to whom.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// The one SQL login that may connect.
    pub user: String,
    /// What checks that login's password.
    pub credentials: TdsCredentials,
    /// The database clients connect to; LOGIN7 and `USE` accept only this name, in any case.
    pub database: String,
    /// The most connections served at once; more are refused with error 17809.
    pub max_connections: usize,
    /// How long a client has from connecting to finishing its login.
    pub authentication_timeout: Duration,
    /// Whether to log the first 200 characters of each batch, for finding out what a client
    /// sends. Off unless asked for; the login and its password are never logged.
    pub log_batches: bool,
}

/// Why a listener was not opened.
#[derive(Debug)]
pub enum ListenError {
    /// The address is not a loopback address (D102): without TLS, a server only listens where
    /// only this machine can connect.
    NotLoopback(SocketAddr),
    /// The operating system refused, for example because the port is in use.
    Io(io::Error),
}

impl fmt::Display for ListenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLoopback(address) => write!(
                f,
                "{address} is not a loopback address; until EggDB has TLS, it listens only on 127.0.0.1, ::1 or another loopback address, so only this machine can connect"
            ),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ListenError {}

/// Opens a TCP listener at `address`, which must be a loopback address (`127.0.0.0/8` or
/// `::1`); any other address is refused before anything is bound (D102). Port 0 picks a free
/// port.
pub fn listen_on_loopback(address: SocketAddr) -> Result<TcpListener, ListenError> {
    if !address.ip().is_loopback() {
        return Err(ListenError::NotLoopback(address));
    }
    TcpListener::bind(address).map_err(ListenError::Io)
}

pub(crate) struct AuthenticationThrottle {
    recent_failures: Mutex<VecDeque<Instant>>,
}

impl AuthenticationThrottle {
    fn new() -> Self {
        Self {
            recent_failures: Mutex::new(VecDeque::new()),
        }
    }

    pub(crate) fn delay_after_failure(&self) -> Duration {
        let now = Instant::now();
        let mut failures = self
            .recent_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while failures
            .front()
            .is_some_and(|failed| now.duration_since(*failed) > FAILURE_WINDOW)
        {
            failures.pop_front();
        }
        if failures.len() < 1024 {
            failures.push_back(now);
        }
        let count = u32::try_from(failures.len()).unwrap_or(u32::MAX);
        DELAY_PER_RECENT_FAILURE
            .saturating_mul(count)
            .min(LONGEST_FAILURE_DELAY)
    }
}

pub(crate) struct Shared<'a> {
    pub(crate) config: &'a ServerConfig,
    pub(crate) stop: &'a (dyn Fn() -> bool + Sync),
    pub(crate) log: &'a (dyn Fn(&str) + Sync),
    pub(crate) throttle: AuthenticationThrottle,
    pub(crate) unknown_user: TdsCredentials,
    pub(crate) unauthenticated: AtomicUsize,
    pub(crate) ended_by_stop: AtomicUsize,
}

pub(crate) struct Slot<'c>(&'c AtomicUsize);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) fn take_slot(counter: &AtomicUsize, limit: usize) -> Option<Slot<'_>> {
    counter
        .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < limit).then_some(count + 1)
        })
        .ok()
        .map(|_| Slot(counter))
}

/// Serves SQL Server's TDS protocol on `listener` over `store` until `stop` returns true, or
/// refuses at once with [`io::ErrorKind::InvalidInput`] when the credentials are for an empty
/// password, since there is no unauthenticated mode: one thread per connection, each with its
/// own SQL session in the SQL Server dialect, at most [`ServerConfig::max_connections`] at
/// once. Every client logs in as [`ServerConfig::user`] with a SQL login, without encryption.
/// When `stop` turns true the server accepts no more connections, lets each finish the batch
/// it is running, closes it, rolls back any open transaction, and returns once every
/// connection has ended, with the number of logged-in connections the stop ended. `log`
/// receives one line per event worth an operator's attention, such as a failed login: the
/// client's address, two spaces, and what happened.
pub fn serve<S: KeyValueStore + Sync>(
    store: &S,
    listener: &TcpListener,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
    log: &(dyn Fn(&str) + Sync),
) -> io::Result<usize> {
    if config.credentials.is_for_empty_password() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the SQL Server server needs a password: it has no unauthenticated mode (D22)",
        ));
    }
    listener.set_nonblocking(true)?;
    let mut secret = [0u8; 32];
    eggdb_platform::secure_random::fill(&mut secret)?;
    let shared = Shared {
        config,
        stop,
        log,
        throttle: AuthenticationThrottle::new(),
        unknown_user: TdsCredentials::matching_nothing(&secret, config.credentials.iterations()),
        unauthenticated: AtomicUsize::new(0),
        ended_by_stop: AtomicUsize::new(0),
    };
    let active = AtomicUsize::new(0);
    let refusing = AtomicUsize::new(0);
    let next_connection_id = AtomicU32::new(51);
    std::thread::scope(|scope| {
        while !stop() {
            let (stream, peer) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL_INTERVAL);
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    log(&format!("accepting a connection failed: {error}"));
                    std::thread::sleep(ACCEPT_POLL_INTERVAL);
                    continue;
                }
            };
            let connection_id = next_connection_id.fetch_add(1, Ordering::Relaxed);
            let shared = &shared;
            match take_slot(&active, config.max_connections) {
                Some(slot) => {
                    let started = std::thread::Builder::new()
                        .name(format!("eggdb-tds-{connection_id}"))
                        .spawn_scoped(scope, move || {
                            let _slot = slot;
                            run_connection(stream, shared, store, connection_id);
                        });
                    if let Err(error) = started {
                        log(&format!("starting a connection thread failed: {error}"));
                    }
                }
                None => {
                    log(&format!(
                        "{peer}  connection refused: {} connections are open (--max-connections)",
                        config.max_connections
                    ));
                    let Some(slot) = take_slot(&refusing, MAX_REFUSING_THREADS) else {
                        continue;
                    };
                    let started = std::thread::Builder::new()
                        .name("eggdb-tds-refuse".into())
                        .spawn_scoped(scope, move || {
                            let _slot = slot;
                            refuse_connection(stream, shared);
                        });
                    if let Err(error) = started {
                        log(&format!("starting a connection thread failed: {error}"));
                    }
                }
            }
        }
    });
    Ok(shared.ended_by_stop.load(Ordering::Acquire))
}

fn run_connection<S: KeyValueStore>(
    stream: TcpStream,
    shared: &Shared<'_>,
    store: &S,
    connection_id: u32,
) {
    let deadline = Instant::now() + shared.config.authentication_timeout;
    let Ok(wire) = Wire::new(stream, shared.stop, Some(deadline)) else {
        return;
    };
    serve_connection(wire, shared, store, connection_id);
}

fn refuse_connection(stream: TcpStream, shared: &Shared<'_>) {
    let deadline = Instant::now() + REFUSAL_TIMEOUT;
    if let Ok(mut wire) = Wire::new(stream, shared.stop, Some(deadline)) {
        refuse_too_many(shared, &mut wire);
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthenticationThrottle, ListenError, listen_on_loopback, take_slot};
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    #[test]
    fn only_loopback_addresses_are_listened_on() {
        for address in [
            "0.0.0.0:0",
            "[::]:0",
            "192.168.1.10:0",
            "8.8.8.8:1433",
            "[::ffff:127.0.0.1]:0",
        ] {
            assert!(
                matches!(
                    listen_on_loopback(address.parse().unwrap()),
                    Err(ListenError::NotLoopback(_))
                ),
                "{address}"
            );
        }
        assert!(listen_on_loopback("127.0.0.1:0".parse().unwrap()).is_ok());
    }

    #[test]
    fn failures_delay_more_the_more_there_were_recently() {
        let throttle = AuthenticationThrottle::new();
        let delays: Vec<Duration> = (0..30).map(|_| throttle.delay_after_failure()).collect();
        assert_eq!(delays[0], Duration::from_millis(250));
        assert_eq!(delays[29], Duration::from_secs(5));
    }

    #[test]
    fn slots_stop_at_the_limit_and_come_back_when_dropped() {
        let counter = AtomicUsize::new(0);
        let first = take_slot(&counter, 1);
        assert!(first.is_some());
        assert!(take_slot(&counter, 1).is_none());
        drop(first);
        assert!(take_slot(&counter, 1).is_some());
    }
}
