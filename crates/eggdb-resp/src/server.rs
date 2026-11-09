use crate::blocking::{BlockingCommand, WaiterRegistry};
use crate::commands::{Client, Context};
use crate::keyspace::{EntryVisitor, KeysRead, KeysWrite, Keyspace};
use crate::pops::OnWrongType;
use crate::protocol::{ProtocolError, Reply, RequestReader};
use crate::pubsub::{Hub, Kind, Queued};
use crate::transactions::execute_with_transactions;
use eggdb_platform::connection_limits::{MemoryBudget, UNCOUNTED_BUFFER_BYTES};
use std::cell::RefCell;
use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_POLL: Duration = Duration::from_millis(200);

/// How the RESP server runs. Its `Debug` output never shows the password's hash.
#[derive(Clone)]
pub struct ServerConfig {
    /// The SHA-256 of the password clients give with `AUTH` (as `requirepass` asks), or
    /// `None` for none; the password itself is not kept.
    pub password_sha256: Option<[u8; 32]>,
    /// The most connections served at once.
    pub max_connections: usize,
    /// The memory every connection's requests and replies hold together, shared with the
    /// server's other listeners.
    pub memory_budget: Arc<MemoryBudget>,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerConfig")
            .field("requires_password", &self.password_sha256.is_some())
            .field("max_connections", &self.max_connections)
            .field("memory_budget", &self.memory_budget)
            .finish()
    }
}

const OUT_OF_BUDGET: &str =
    "OOM command not allowed: the server's connections already hold their memory budget";

fn reply_room(budget: &MemoryBudget, held_bytes: usize, pending_bytes: usize) -> usize {
    budget
        .limit_bytes()
        .saturating_sub(budget.used_bytes())
        .saturating_add(held_bytes)
        .saturating_sub(pending_bytes)
        .max(UNCOUNTED_BUFFER_BYTES)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
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
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                other => return other,
            }
        }
    }
}

struct RecordingKeys<'k, 'w> {
    inner: &'k mut dyn KeysWrite,
    written: &'w mut Vec<Vec<u8>>,
}

impl KeysRead for RecordingKeys<'_, '_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        self.inner.get(key)
    }

    fn keys(&self) -> Result<Vec<Vec<u8>>, String> {
        self.inner.keys()
    }

    fn visit_keys(
        &self,
        visit: &mut dyn FnMut(Vec<u8>) -> Result<bool, String>,
    ) -> Result<(), String> {
        self.inner.visit_keys(visit)
    }

    fn scan(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>, String> {
        self.inner.scan(prefix)
    }

    fn visit_prefix(&self, prefix: &[u8], visit: &mut EntryVisitor<'_>) -> Result<(), String> {
        self.inner.visit_prefix(prefix, visit)
    }
}

impl KeysWrite for RecordingKeys<'_, '_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), String> {
        self.inner.put(key, value)?;
        self.written.push(key.to_vec());
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> Result<bool, String> {
        self.inner.delete(key)
    }

    fn clear(&mut self) -> Result<(), String> {
        self.inner.clear()
    }
}

struct NotifyingKeyspace<'k> {
    inner: &'k dyn Keyspace,
    written: RefCell<Vec<(u32, Vec<u8>)>>,
}

impl NotifyingKeyspace<'_> {
    fn take_written(&self) -> Vec<(u32, Vec<u8>)> {
        self.written
            .try_borrow_mut()
            .map(|mut written| std::mem::take(&mut *written))
            .unwrap_or_default()
    }
}

impl Keyspace for NotifyingKeyspace<'_> {
    fn read(
        &self,
        db: u32,
        body: &mut dyn FnMut(&dyn KeysRead) -> Result<(), String>,
    ) -> Result<(), String> {
        self.inner.read(db, body)
    }

    fn write(
        &self,
        db: u32,
        body: &mut dyn FnMut(&mut dyn KeysWrite) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut written = Vec::new();
        self.inner.write(db, &mut |keys| {
            written.clear();
            body(&mut RecordingKeys {
                inner: keys,
                written: &mut written,
            })
        })?;
        if let Ok(mut recorded) = self.written.try_borrow_mut() {
            recorded.extend(written.into_iter().map(|key| (db, key)));
        }
        Ok(())
    }
}

struct Connection<'s> {
    id: u64,
    hub: &'s Hub,
    writer: Arc<Mutex<TcpStream>>,
    outbox: Option<(Sender<Queued>, Arc<AtomicUsize>)>,
}

impl Connection<'_> {
    fn send(&self, bytes: &[u8]) -> io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        writer.write_all(bytes)
    }

    fn peer_closed(&self) -> bool {
        let stream = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if stream.set_nonblocking(true).is_err() {
            return true;
        }
        let mut probe = [0u8; 1];
        let peeked = stream.peek(&mut probe);
        if stream.set_nonblocking(false).is_err() {
            return true;
        }
        match peeked {
            Ok(0) => true,
            Ok(_) => false,
            Err(error) => !matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ),
        }
    }

    fn start_outbox(&mut self, resp3: bool) {
        if self.outbox.is_some() {
            return;
        }
        let (sender, receiver) = std::sync::mpsc::channel::<Queued>();
        let pending = Arc::new(AtomicUsize::new(0));
        let writer = Arc::clone(&self.writer);
        let forwarded = Arc::clone(&pending);
        let started = std::thread::Builder::new()
            .name("eggdb-resp-push".to_owned())
            .spawn(move || {
                for Queued { frame, held } in receiver {
                    forwarded.fetch_sub(1, Ordering::AcqRel);
                    let mut stream = writer
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let written = stream.write_all(&frame);
                    drop(held);
                    if written.is_err() {
                        return;
                    }
                }
            });
        if started.is_ok() {
            self.hub
                .attach(self.id, resp3, sender.clone(), Arc::clone(&pending));
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
                    _ => Reply::Error(
                        "ERR wrong number of arguments for 'publish' command".to_owned(),
                    ),
                }]);
            }
            _ => return None,
        };
        let subscribe = name.ends_with("subscribe") && !name.contains("unsub");
        if subscribe && args.is_empty() {
            return Some(vec![Reply::Error(format!(
                "ERR wrong number of arguments for '{name}' command"
            ))]);
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
    waiters: &WaiterRegistry,
    keyspace: &dyn Keyspace,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
) {
    if stream.set_read_timeout(Some(READ_POLL)).is_err() || stream.set_nodelay(true).is_err() {
        return;
    }
    let notifying = NotifyingKeyspace {
        inner: keyspace,
        written: RefCell::default(),
    };
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
    let mut held = config.memory_budget.empty_reservation();
    loop {
        let read = reader
            .next_request_within(&mut |declared| held.hold_if_large(declared.saturating_mul(2)));
        let arguments = match read {
            Ok(arguments) => arguments,
            Err(ProtocolError::Closed) => break,
            Err(ProtocolError::OverBudget(_)) => {
                Reply::Error(OUT_OF_BUDGET.to_owned()).encode(&mut out, client.resp3);
                let _ = connection.send(&out);
                break;
            }
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
        let replies = if !client.authenticated
            && name != "auth"
            && name != "hello"
            && name != "quit"
        {
            vec![Reply::Error("NOAUTH Authentication required.".to_owned())]
        } else if let Some(replies) = connection.pubsub(&client, &name, &arguments[1..]) {
            replies
        } else if subscribed && !client.resp3 && !matches!(name.as_str(), "ping" | "quit" | "reset")
        {
            vec![Reply::Error(format!(
                "ERR Can't execute '{name}': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / RESET are allowed in this context"
            ))]
        } else {
            let context = Context {
                keyspace: &notifying,
                password_sha256: config.password_sha256.as_ref(),
                now_ms: now_ms(),
                reply_room_bytes: reply_room(&config.memory_budget, held.bytes(), out.len()),
            };
            let blocking = if client.queued.is_none() {
                BlockingCommand::parse(&name, &arguments[1..])
            } else {
                None
            };
            match blocking {
                None => vec![execute_with_transactions(&context, &mut client, &arguments)],
                Some(Err(error)) => vec![Reply::Error(error)],
                Some(Ok(mut command)) => {
                    let db = client.db;
                    match command.attempt(&context, db, OnWrongType::Refuse) {
                        Some(reply) => vec![reply],
                        None => {
                            if !out.is_empty() {
                                if connection.send(&out).is_err() {
                                    break;
                                }
                                out.clear();
                                out.shrink_to(UNCOUNTED_BUFFER_BYTES);
                            }
                            let reply_room_bytes =
                                reply_room(&config.memory_budget, held.bytes(), 0);
                            let mut retry = |command: &mut BlockingCommand| {
                                let context = Context {
                                    keyspace: &notifying,
                                    password_sha256: config.password_sha256.as_ref(),
                                    now_ms: now_ms(),
                                    reply_room_bytes,
                                };
                                command.attempt(&context, db, OnWrongType::Wait)
                            };
                            let mut abandoned = || stop() || connection.peer_closed();
                            match waiters.wait(
                                db,
                                &mut command,
                                READ_POLL,
                                &mut retry,
                                &mut abandoned,
                            ) {
                                Some(reply) => vec![reply],
                                None => break,
                            }
                        }
                    }
                }
            }
        };
        waiters.notify_written(&notifying.take_written());
        let replied_from = out.len();
        for reply in &replies {
            reply.encode(&mut out, client.resp3);
        }
        drop(arguments);
        if !held.hold_if_large(out.len()) {
            out.truncate(replied_from);
            Reply::Error(OUT_OF_BUDGET.to_owned()).encode(&mut out, client.resp3);
            out.shrink_to(UNCOUNTED_BUFFER_BYTES);
            held.hold_if_large(out.len());
        }
        let quitting = name == "quit";
        if quitting || !reader.has_buffered() {
            if connection.send(&out).is_err() {
                break;
            }
            out.clear();
            out.shrink_to(UNCOUNTED_BUFFER_BYTES);
            held.hold_if_large(0);
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
    let hub = Hub::new(Arc::clone(&config.memory_budget));
    let waiters = WaiterRegistry::default();
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
                log(&format!(
                    "{peer}  connection refused: max number of clients reached"
                ));
                let mut stream = stream;
                let _ = stream.write_all(b"-ERR max number of clients reached\r\n");
                continue;
            }
            let active = &active;
            let hub = &hub;
            let waiters = &waiters;
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            let spawned = std::thread::Builder::new()
                .name("eggdb-resp".to_owned())
                .spawn_scoped(scope, move || {
                    serve_connection(stream, id, hub, waiters, keyspace, config, stop);
                    active.fetch_sub(1, Ordering::AcqRel);
                });
            if spawned.is_err() {
                active.fetch_sub(1, Ordering::AcqRel);
            }
        }
    });
    Ok(active.load(Ordering::Acquire))
}
