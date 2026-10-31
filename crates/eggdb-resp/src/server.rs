use crate::commands::{Client, Context, execute};
use crate::keyspace::Keyspace;
use crate::protocol::{ProtocolError, Reply, RequestReader};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_POLL: Duration = Duration::from_millis(200);

/// How the RESP server runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// The SHA-256 of the password clients give with `AUTH` (as `requirepass` asks), or
    /// `None` for none; the password itself is not kept.
    pub password_sha256: Option<[u8; 32]>,
    /// The most connections served at once.
    pub max_connections: usize,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

struct StopAwareStream<'s> {
    stream: TcpStream,
    stop: &'s (dyn Fn() -> bool + Sync),
}

impl io::Read for StopAwareStream<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if (self.stop)() {
                return Ok(0);
            }
            match self.stream.read(buffer) {
                Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                other => return other,
            }
        }
    }
}

fn serve_connection(stream: TcpStream, keyspace: &dyn Keyspace, config: &ServerConfig, stop: &(dyn Fn() -> bool + Sync)) {
    if stream.set_read_timeout(Some(READ_POLL)).is_err() || stream.set_nodelay(true).is_err() {
        return;
    }
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = RequestReader::new(StopAwareStream { stream, stop });
    let mut client = Client {
        db: 0,
        resp3: false,
        authenticated: config.password_sha256.is_none(),
        name: None,
    };
    let mut out = Vec::new();
    loop {
        let arguments = match reader.next() {
            Ok(arguments) => arguments,
            Err(ProtocolError::Closed) => return,
            Err(ProtocolError::Invalid(message)) => {
                Reply::Error(format!("ERR {message}")).encode(&mut out, client.resp3);
                let _ = writer.write_all(&out);
                return;
            }
        };
        let context = Context {
            keyspace,
            password_sha256: config.password_sha256.as_ref(),
            now_ms: now_ms(),
        };
        let quitting = arguments.first().is_some_and(|name| name.eq_ignore_ascii_case(b"quit"));
        execute(&context, &mut client, &arguments).encode(&mut out, client.resp3);
        if quitting || !reader.has_buffered() {
            if writer.write_all(&out).is_err() {
                return;
            }
            out.clear();
        }
        if quitting {
            return;
        }
    }
}

/// Serves RESP clients on `listener` from `keyspace`, a thread per connection, until `stop`
/// turns true; returns how many connections were open then.
pub fn serve(
    keyspace: &dyn Keyspace,
    listener: &TcpListener,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
    log: &(dyn Fn(&str) + Sync),
) -> io::Result<usize> {
    listener.set_nonblocking(true)?;
    let active = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        while !stop() {
            let (stream, peer) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
                Err(error) => {
                    log(&format!("accepting a connection failed: {error}"));
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
            };
            if stream.set_nonblocking(false).is_err() {
                continue;
            }
            if active.fetch_add(1, Ordering::AcqRel) >= config.max_connections {
                active.fetch_sub(1, Ordering::AcqRel);
                log(&format!("{peer}  connection refused: max number of clients reached"));
                let mut stream = stream;
                let _ = stream.write_all(b"-ERR max number of clients reached\r\n");
                continue;
            }
            let active = &active;
            let spawned = std::thread::Builder::new()
                .name("eggdb-resp".to_owned())
                .spawn_scoped(scope, move || {
                    serve_connection(stream, keyspace, config, stop);
                    active.fetch_sub(1, Ordering::AcqRel);
                });
            if spawned.is_err() {
                active.fetch_sub(1, Ordering::AcqRel);
            }
        }
    });
    Ok(active.load(Ordering::Acquire))
}
