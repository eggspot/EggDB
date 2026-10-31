use crate::commands::{Client, Context};
use crate::keyspace::Keyspace;
use crate::protocol::{ProtocolError, Reply, RequestReader};
use crate::pubsub::{Hub, Kind};
use crate::transactions::execute_with_transactions;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
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

struct Connection<'s> {
    id: u64,
    hub: &'s Hub,
    writer: Arc<Mutex<TcpStream>>,
    outbox: Option<(Sender<Vec<u8>>, Arc<AtomicUsize>)>,
}

impl Connection<'_> {
    fn send(&self, bytes: &[u8]) -> io::Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        writer.write_all(bytes)
    }

    fn start_outbox(&mut self, resp3: bool) {
        if self.outbox.is_some() {
            return;
        }
        let (sender, receiver) = std::sync::mpsc::channel::<Vec<u8>>();
        let pending = Arc::new(AtomicUsize::new(0));
        let writer = Arc::clone(&self.writer);
        let forwarded = Arc::clone(&pending);
        let started = std::thread::Builder::new()
            .name("eggdb-resp-push".to_owned())
            .spawn(move || {
                for frame in receiver {
                    forwarded.fetch_sub(1, Ordering::AcqRel);
                    let mut stream = writer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if stream.write_all(&frame).is_err() {
                        return;
                    }
                }
            });
        if started.is_ok() {
            self.hub.attach(self.id, resp3, sender.clone(), Arc::clone(&pending));
            self.outbox = Some((sender, pending));
        }
    }

    fn pubsub(&mut self, client: &Client, name: &str, args: &[Vec<u8>]) -> Option<Vec<Reply>> {
        let kind = match name {
            "subscribe" | "unsubscribe" => Kind::Channel,
            "psubscribe" | "punsubscribe" => Kind::Pattern,
            "publish" => {
                return Some(vec![match args {
                    [channel, message] => Reply::Integer(self.hub.publish(channel, message) as i64),
                    _ => Reply::Error("ERR wrong number of arguments for 'publish' command".to_owned()),
                }]);
            }
            _ => return None,
        };
        let subscribe = name.ends_with("subscribe") && !name.contains("unsub");
        if subscribe && args.is_empty() {
            return Some(vec![Reply::Error(format!("ERR wrong number of arguments for '{name}' command"))]);
        }
        if client.queued.is_some() {
            return None;
        }
        self.start_outbox(client.resp3);
        Some(self.hub.change(self.id, kind, subscribe, args))
    }
}

fn serve_connection(
    stream: TcpStream,
    id: u64,
    hub: &Hub,
    keyspace: &dyn Keyspace,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
) {
    if stream.set_read_timeout(Some(READ_POLL)).is_err() || stream.set_nodelay(true).is_err() {
        return;
    }
    let Ok(writer) = stream.try_clone() else {
        return;
    };
    let mut connection = Connection {
        id,
        hub,
        writer: Arc::new(Mutex::new(writer)),
        outbox: None,
    };
    let mut reader = RequestReader::new(StopAwareStream { stream, stop });
    let mut client = Client {
        authenticated: config.password_sha256.is_none(),
        ..Client::default()
    };
    let mut out = Vec::new();
    loop {
        let arguments = match reader.next() {
            Ok(arguments) => arguments,
            Err(ProtocolError::Closed) => break,
            Err(ProtocolError::Invalid(message)) => {
                Reply::Error(format!("ERR {message}")).encode(&mut out, client.resp3);
                let _ = connection.send(&out);
                break;
            }
        };
        let name = arguments
            .first()
            .map(|first| String::from_utf8_lossy(first).to_ascii_lowercase())
            .unwrap_or_default();
        let subscribed = hub.held(id) > 0;
        let replies = if !client.authenticated && name != "auth" && name != "hello" && name != "quit" {
            vec![Reply::Error("NOAUTH Authentication required.".to_owned())]
        } else if let Some(replies) = connection.pubsub(&client, &name, &arguments[1..]) {
            replies
        } else if subscribed
            && !client.resp3
            && !matches!(name.as_str(), "ping" | "quit" | "reset")
        {
            vec![Reply::Error(format!(
                "ERR Can't execute '{name}': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / RESET are allowed in this context"
            ))]
        } else {
            let context = Context {
                keyspace,
                password_sha256: config.password_sha256.as_ref(),
                now_ms: now_ms(),
            };
            vec![execute_with_transactions(&context, &mut client, &arguments)]
        };
        for reply in &replies {
            reply.encode(&mut out, client.resp3);
        }
        let quitting = name == "quit";
        if quitting || !reader.has_buffered() {
            if connection.send(&out).is_err() {
                break;
            }
            out.clear();
        }
        if quitting {
            break;
        }
    }
    hub.detach(id);
}

/// Serves RESP clients on `listener` from `keyspace`, a thread per connection, until `stop`
/// turns true; returns how many connections were open then.
pub fn serve(
    keyspace: &(dyn Keyspace + Sync),
    listener: &TcpListener,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
    log: &(dyn Fn(&str) + Sync),
) -> io::Result<usize> {
    listener.set_nonblocking(true)?;
    let active = AtomicUsize::new(0);
    let hub = Hub::default();
    let next_id = AtomicU64::new(1);
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
            let hub = &hub;
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            let spawned = std::thread::Builder::new()
                .name("eggdb-resp".to_owned())
                .spawn_scoped(scope, move || {
                    serve_connection(stream, id, hub, keyspace, config, stop);
                    active.fetch_sub(1, Ordering::AcqRel);
                });
            if spawned.is_err() {
                active.fetch_sub(1, Ordering::AcqRel);
            }
        }
    });
    Ok(active.load(Ordering::Acquire))
}
