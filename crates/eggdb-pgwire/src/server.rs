use crate::backend_keys::BackendKeys;
use crate::connection::{refuse_too_many, serve_connection};
use crate::wire::Wire;
use eggdb_auth::scram::ScramVerifier;
use eggdb_platform::connection_limits::{
    LoginSlots, LoginTicket, MAX_LOGINS_IN_PROGRESS, MemoryBudget,
};
use eggdb_platform::local_socket::LocalListener;
use eggdb_sql::{DatabaseDirectory, FixedDatabases, KeyValueStore};
use eggdb_tls::ServerConnection;
use std::io;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
pub use eggdb_listener::{ListenError, listen, listen_on_loopback};
pub(crate) use eggdb_listener::{AuthenticationThrottle, Slot, take_slot};

/// The connections a server serves at once unless told otherwise.
pub const DEFAULT_MAX_CONNECTIONS: usize = 64;
/// How long a client has from connecting to finishing authentication, unless told otherwise.
pub const DEFAULT_AUTHENTICATION_TIMEOUT: Duration =
    eggdb_platform::connection_limits::DEFAULT_AUTHENTICATION_TIMEOUT;
/// How long an authenticated connection may wait for its next message before it is ended with
/// `57P05`, unless told otherwise.
pub const DEFAULT_IDLE_TIMEOUT: Duration = eggdb_platform::connection_limits::DEFAULT_IDLE_TIMEOUT;
/// The most connections still starting up or authenticating at once. When they are all taken,
/// a new connection takes the slot of the oldest that has waited a second or more, which is
/// dropped, so idle sockets cannot lock logins out; when every one is younger, the new
/// connection is refused with `53300`.
pub const MAX_UNAUTHENTICATED_CONNECTIONS: usize = MAX_LOGINS_IN_PROGRESS;
const MAX_REFUSING_THREADS: usize = 8;
pub(crate) const REFUSAL_TIMEOUT: Duration = Duration::from_millis(250);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// What a server serves and to whom.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// The server's own user, a superuser; roles stored in the database with `LOGIN` and a
    /// password may connect too, with their privileges checked.
    pub user: String,
    /// That user's SCRAM-SHA-256 verifier.
    pub verifier: ScramVerifier,
    /// The database name clients connect to.
    pub database: String,
    /// The most logged-in connections served at once; more are refused with `53300`. Logins
    /// still in progress do not count here: [`MAX_UNAUTHENTICATED_CONNECTIONS`] caps them.
    pub max_connections: usize,
    /// How long a client has from connecting to finishing authentication.
    pub authentication_timeout: Duration,
    /// How long an authenticated connection may wait for its next message before it is ended
    /// with `57P05 idle_session_timeout`; `None` waits for ever.
    pub idle_timeout: Option<Duration>,
    /// The bytes every connection holds together in large messages, prepared statements and
    /// portals; a request that would go past it fails with `53200 out_of_memory`. Share one
    /// budget between the servers of one process.
    pub memory_budget: std::sync::Arc<MemoryBudget>,
    /// The rule file that picks how users other than [`ServerConfig::user`] log in (D185);
    /// without one, stored roles use SCRAM-SHA-256.
    pub auth_rules: Option<std::sync::Arc<eggdb_auth::AuthRules>>,
    /// The TLS setup, when the server offers TLS (D161): `SSLRequest` is then answered `S`,
    /// PostgreSQL 17's direct TLS is accepted, and connections from other machines must use it.
    pub tls: Option<std::sync::Arc<eggdb_tls::ServerConfig>>,
    /// Whether `replication=database` connections may stream logical replication from the
    /// changes the engine captures (D1031); otherwise they are refused.
    pub logical_replication: bool,
}

impl ServerConfig {
    pub(crate) fn verifier_iterations(&self) -> u32 {
        self.verifier.iterations()
    }
}

pub(crate) struct Shared<'a> {
    pub(crate) config: &'a ServerConfig,
    pub(crate) stop: &'a (dyn Fn() -> bool + Sync),
    pub(crate) log: &'a (dyn Fn(&str) + Sync),
    pub(crate) throttle: AuthenticationThrottle,
    pub(crate) secret: [u8; 32],
    pub(crate) backend_keys: BackendKeys,
    pub(crate) logins: LoginSlots,
    pub(crate) connections: AtomicUsize,
    pub(crate) ended_by_stop: AtomicUsize,
    pub(crate) notifications: crate::notifications::NotificationHub,
}

/// Serves the PostgreSQL wire protocol v3 on `listener` over `store` until `stop` returns true:
/// one thread per connection, each with its own SQL session, at most
/// [`ServerConfig::max_connections`] at once. Every client authenticates with SCRAM-SHA-256 as
/// [`ServerConfig::user`]. When `stop` turns true the server accepts no more connections, lets
/// each finish the message it is running, ends it with `57P01 admin_shutdown`, rolls back any
/// open block, and returns once every connection has ended, with the number of authenticated
/// connections the stop ended. `log` receives one line per event worth an operator's
/// attention, such as a failed password: the client's address, two spaces, and PostgreSQL's
/// wording of what happened.
pub fn serve<S: KeyValueStore + Sync>(
    store: &S,
    listener: &TcpListener,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
    log: &(dyn Fn(&str) + Sync),
) -> io::Result<usize> {
    serve_with_local(store, listener, None, config, stop, log)
}

/// [`serve`], also accepting connections on the Unix socket `local`.
pub fn serve_with_local<S: KeyValueStore + Sync>(
    store: &S,
    listener: &TcpListener,
    local: Option<&LocalListener>,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
    log: &(dyn Fn(&str) + Sync),
) -> io::Result<usize> {
    serve_databases(
        &[(config.database.as_str(), store)],
        listener,
        local,
        config,
        stop,
        log,
    )
}

/// [`serve_with_local`] for several databases, each reached by its name in the startup
/// packet (`psql -d <name>`, `\c <name>`); `pg_database` lists them all. The first is
/// [`ServerConfig::database`]'s.
pub fn serve_databases<S: KeyValueStore + Sync>(
    databases: &[(&str, &S)],
    listener: &TcpListener,
    local: Option<&LocalListener>,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
    log: &(dyn Fn(&str) + Sync),
) -> io::Result<usize> {
    serve_directory(
        &FixedDatabases::new(databases),
        listener,
        local,
        config,
        stop,
        log,
    )
}

/// [`serve_with_local`] for the databases of `directory`, each reached by its name in the
/// startup packet, including databases created while the server runs (`CREATE DATABASE`,
/// D270): each connection holds its database for as long as it is connected, and
/// its session creates and drops databases through `directory`. [`ServerConfig::database`]
/// names the database whose stored roles a login to a missing database is checked against.
pub fn serve_directory<D: DatabaseDirectory + Sync>(
    directory: &D,
    listener: &TcpListener,
    local: Option<&LocalListener>,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
    log: &(dyn Fn(&str) + Sync),
) -> io::Result<usize> {
    listener.set_nonblocking(true)?;
    if let Some(local) = local {
        local.set_nonblocking(true)?;
    }
    let mut secret = [0u8; 32];
    eggdb_platform::secure_random::fill(&mut secret)?;
    let shared = Shared {
        config,
        stop,
        log,
        throttle: AuthenticationThrottle::new(),
        secret,
        backend_keys: BackendKeys::new(),
        logins: LoginSlots::default(),
        connections: AtomicUsize::new(0),
        ended_by_stop: AtomicUsize::new(0),
        notifications: crate::notifications::NotificationHub::default(),
    };
    let refusing = AtomicUsize::new(0);
    let next_process_id = AtomicU32::new(1);
    std::thread::scope(|scope| {
        while !stop() {
            let Some(stream) = accept_next(listener, local, log) else {
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
                continue;
            };
            let peer = stream.peer_text();
            let process_id = next_process_id.fetch_add(1, Ordering::Relaxed);
            let shared = &shared;
            let full = shared.connections.load(Ordering::Acquire) >= config.max_connections;
            let login = if full {
                None
            } else {
                shared.logins.admit(peer_ip(&stream))
            };
            match login {
                Some(login) => {
                    let started = std::thread::Builder::new()
                        .name(format!("eggdb-pg-{process_id}"))
                        .stack_size(eggdb_sql::SESSION_STACK_BYTES)
                        .spawn_scoped(scope, move || {
                            eggdb_sql::set_thread_stack_size(eggdb_sql::SESSION_STACK_BYTES);
                            eggdb_platform::prefer_performance_cores_for_current_thread();
                            run_connection(stream, shared, directory, process_id, login);
                        });
                    if let Err(error) = started {
                        log(&format!("starting a connection thread failed: {error}"));
                    }
                }
                None => {
                    if full {
                        log(&format!(
                            "{peer}  connection refused: {} connections are open (--max-connections)",
                            config.max_connections
                        ));
                    } else {
                        log(&format!(
                            "{peer}  connection refused: {MAX_UNAUTHENTICATED_CONNECTIONS} connections are still logging in"
                        ));
                    }
                    let Some(slot) = take_slot(&refusing, MAX_REFUSING_THREADS) else {
                        continue;
                    };
                    let started = std::thread::Builder::new()
                        .name("eggdb-pg-refuse".into())
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

fn accept_next(
    listener: &TcpListener,
    local: Option<&LocalListener>,
    log: &(dyn Fn(&str) + Sync),
) -> Option<ServerConnection> {
    let report = |error: io::Error| {
        if !matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        ) {
            log(&format!("accepting a connection failed: {error}"));
        }
    };
    match eggdb_platform::accept_tcp(listener) {
        Ok((stream, _)) => return Some(ServerConnection::Plain(stream)),
        Err(error) => report(error),
    }
    match local?.accept() {
        Ok((stream, _)) => Some(ServerConnection::Local(stream)),
        Err(error) => {
            report(error);
            None
        }
    }
}

fn peer_ip(stream: &ServerConnection) -> Option<std::net::IpAddr> {
    match stream {
        ServerConnection::Plain(stream) => stream.peer_addr().ok().map(|peer| peer.ip()),
        _ => None,
    }
}

fn run_connection<'a, D: DatabaseDirectory>(
    stream: ServerConnection,
    shared: &'a Shared<'a>,
    directory: &D,
    process_id: u32,
    login: LoginTicket<'a>,
) {
    let _open = eggdb_platform::activity::connection_opened();
    let deadline = Instant::now() + shared.config.authentication_timeout;
    let Ok(wire) = Wire::new(stream, shared.stop, Some(deadline)) else {
        return;
    };
    serve_connection(wire, shared, directory, process_id as i32, login);
}

fn refuse_connection(stream: ServerConnection, shared: &Shared<'_>) {
    let deadline = Instant::now() + REFUSAL_TIMEOUT;
    if let Ok(mut wire) = Wire::new(stream, shared.stop, Some(deadline)) {
        refuse_too_many(&mut wire, shared);
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
            "8.8.8.8:5432",
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
        assert_eq!(delays[1], Duration::from_millis(500));
        assert_eq!(delays[29], Duration::from_secs(5));
    }

    #[test]
    fn slots_stop_at_the_limit_and_come_back_when_dropped() {
        let counter = AtomicUsize::new(0);
        let first = take_slot(&counter, 2);
        let second = take_slot(&counter, 2);
        assert!(first.is_some() && second.is_some());
        assert!(take_slot(&counter, 2).is_none());
        drop(first);
        assert!(take_slot(&counter, 2).is_some());
    }
}
