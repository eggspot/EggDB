use crate::protocol::{Frame, MAX_PAYLOAD, message, next_frame, subject_matches, valid_subject};
use eggdb_sql::json::{Json, Node, parse_document};
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_POLL: Duration = Duration::from_millis(200);
const WRITE_LIMIT: Duration = Duration::from_secs(5);
const CONNECT_WAIT: Duration = Duration::from_secs(10);
const MAX_SUBSCRIPTIONS: usize = 10_000;

/// How the NATS server runs.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// The user clients connect as.
    pub user: String,
    /// The SHA-256 of that user's password.
    pub password_sha256: [u8; 32],
    /// The most connections served at once.
    pub max_connections: usize,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig").field("user", &self.user).finish_non_exhaustive()
    }
}

pub(crate) type SharedWriter = Arc<Mutex<dyn Write + Send>>;

struct Subscription {
    subject: String,
    queue: Option<String>,
    delivered: u64,
    max: Option<u64>,
}

struct Client {
    writer: SharedWriter,
    subscriptions: BTreeMap<String, Subscription>,
    echo: bool,
    headers: bool,
}

/// What every connection shares: who subscribes to what.
pub(crate) struct Broker {
    clients: Mutex<BTreeMap<u64, Client>>,
    turn: AtomicUsize,
}

struct Published<'p> {
    from: u64,
    subject: &'p str,
    reply: Option<&'p str>,
    headers: &'p [u8],
    payload: &'p [u8],
}

impl Broker {
    pub(crate) fn new() -> Self {
        Self { clients: Mutex::new(BTreeMap::new()), turn: AtomicUsize::new(0) }
    }

    fn clients(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Client>> {
        self.clients.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish(&self, published: &Published<'_>) {
        let mut clients = self.clients();
        let mut targets: Vec<(u64, String)> = Vec::new();
        let mut groups: BTreeMap<&str, Vec<(u64, String)>> = BTreeMap::new();
        for (&id, client) in clients.iter() {
            if id == published.from && !client.echo {
                continue;
            }
            for (sid, subscription) in &client.subscriptions {
                if !subject_matches(&subscription.subject, published.subject) {
                    continue;
                }
                match &subscription.queue {
                    Some(queue) => groups.entry(queue.as_str()).or_default().push((id, sid.clone())),
                    None => targets.push((id, sid.clone())),
                }
            }
        }
        let turn = self.turn.fetch_add(1, Ordering::Relaxed);
        for members in groups.into_values() {
            targets.push(members[turn % members.len()].clone());
        }
        for (id, sid) in targets {
            let Some(client) = clients.get_mut(&id) else {
                continue;
            };
            let packet = message(published.subject, &sid, published.reply, published.headers, published.payload, client.headers);
            let _ = client.writer.lock().unwrap_or_else(PoisonError::into_inner).write_all(&packet);
            if let Some(subscription) = client.subscriptions.get_mut(&sid) {
                subscription.delivered += 1;
                if subscription.max.is_some_and(|max| subscription.delivered >= max) {
                    client.subscriptions.remove(&sid);
                }
            }
        }
    }
}

struct Session<'b> {
    broker: &'b Broker,
    config: &'b ServerConfig,
    id: u64,
    writer: SharedWriter,
    connected: bool,
    verbose: bool,
}

enum Next {
    Continue,
    Close,
}

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Json> {
    match &node.value {
        Json::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, node)| &node.value),
        _ => None,
    }
}

fn flag(node: &Node, name: &str, default: bool) -> bool {
    match member(node, name) {
        Some(Json::Boolean(value)) => *value,
        _ => default,
    }
}

fn text<'n>(node: &'n Node, name: &str) -> Option<&'n str> {
    match member(node, name)? {
        Json::String(text) => Some(text),
        _ => None,
    }
}

impl Session<'_> {
    fn send(&self, bytes: &[u8]) -> io::Result<()> {
        self.writer.lock().unwrap_or_else(PoisonError::into_inner).write_all(bytes)
    }

    fn error(&self, message: &str) -> io::Result<()> {
        self.send(format!("-ERR '{message}'\r\n").as_bytes())
    }

    fn ok(&self) -> io::Result<()> {
        if self.verbose { self.send(b"+OK\r\n") } else { Ok(()) }
    }

    fn connect(&mut self, options: &str) -> io::Result<Next> {
        let Ok(options) = parse_document(options) else {
            self.error("Invalid CONNECT options")?;
            return Ok(Next::Close);
        };
        let user = text(&options, "user").unwrap_or_default();
        let password = text(&options, "pass").unwrap_or_default();
        let authorized = eggdb_crypto::constant_time_eq(user.as_bytes(), self.config.user.as_bytes())
            && eggdb_crypto::constant_time_eq(&eggdb_crypto::sha256(password.as_bytes()), &self.config.password_sha256);
        if !authorized {
            self.error("Authorization Violation")?;
            return Ok(Next::Close);
        }
        self.verbose = flag(&options, "verbose", false);
        if !self.connected {
            self.connected = true;
            self.broker.clients().insert(
                self.id,
                Client {
                    writer: Arc::clone(&self.writer),
                    subscriptions: BTreeMap::new(),
                    echo: flag(&options, "echo", true),
                    headers: flag(&options, "headers", false),
                },
            );
        }
        self.ok()?;
        Ok(Next::Continue)
    }

    fn handle(&mut self, frame: Frame) -> io::Result<Next> {
        if !self.connected && !matches!(frame, Frame::Connect(_) | Frame::Ping | Frame::Pong) {
            self.error("Authorization Violation")?;
            return Ok(Next::Close);
        }
        match frame {
            Frame::Connect(options) => return self.connect(&options),
            Frame::Ping => self.send(b"PONG\r\n")?,
            Frame::Pong => {}
            Frame::Publish { subject, reply, headers, payload } => {
                let reply_valid = reply.as_deref().is_none_or(|reply| valid_subject(reply, false));
                if !valid_subject(&subject, false) || !reply_valid {
                    self.error("Invalid Subject")?;
                } else {
                    self.broker.publish(&Published { from: self.id, subject: &subject, reply: reply.as_deref(), headers: &headers, payload: &payload });
                    self.ok()?;
                }
            }
            Frame::Subscribe { subject, queue, sid } => {
                let queue_valid = queue.as_deref().is_none_or(|queue| valid_subject(queue, false));
                if !valid_subject(&subject, true) || !queue_valid {
                    self.error("Invalid Subject")?;
                    return Ok(Next::Continue);
                }
                let mut clients = self.broker.clients();
                let Some(client) = clients.get_mut(&self.id) else {
                    return Ok(Next::Close);
                };
                if client.subscriptions.len() >= MAX_SUBSCRIPTIONS && !client.subscriptions.contains_key(&sid) {
                    drop(clients);
                    self.error("Maximum Subscriptions Exceeded")?;
                    return Ok(Next::Continue);
                }
                client.subscriptions.insert(sid, Subscription { subject, queue, delivered: 0, max: None });
                drop(clients);
                self.ok()?;
            }
            Frame::Unsubscribe { sid, max } => {
                let mut clients = self.broker.clients();
                if let Some(client) = clients.get_mut(&self.id) {
                    match (client.subscriptions.get_mut(&sid), max) {
                        (Some(subscription), Some(max)) if subscription.delivered < max => subscription.max = Some(max),
                        _ => {
                            client.subscriptions.remove(&sid);
                        }
                    }
                }
                drop(clients);
                self.ok()?;
            }
        }
        Ok(Next::Continue)
    }

    fn run(&mut self, mut reader: impl Read, stop: &(dyn Fn() -> bool + Sync)) -> io::Result<()> {
        let started = Instant::now();
        let mut buffer = Vec::new();
        let mut chunk = vec![0; 64 * 1024];
        loop {
            loop {
                match next_frame(&buffer) {
                    Ok(Some((frame, used))) => {
                        buffer.drain(..used);
                        if let Next::Close = self.handle(frame)? {
                            return Ok(());
                        }
                    }
                    Ok(None) => break,
                    Err(problem) => return self.error(problem),
                }
            }
            if stop() || (!self.connected && started.elapsed() > CONNECT_WAIT) {
                return Ok(());
            }
            match reader.read(&mut chunk) {
                Ok(0) => return Ok(()),
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => {}
                Err(error) => return Err(error),
            }
        }
    }
}

fn info(address: SocketAddr, client_id: u64) -> String {
    format!(
        "INFO {{\"server_id\":\"EGGDB\",\"server_name\":\"eggdb\",\"version\":\"2.10.0\",\"proto\":1,\"go\":\"eggdb\",\"host\":\"{}\",\"port\":{},\"headers\":true,\"auth_required\":true,\"max_payload\":{MAX_PAYLOAD},\"client_id\":{client_id}}}\r\n",
        address.ip(),
        address.port()
    )
}

pub(crate) fn serve_stream(
    broker: &Broker,
    config: &ServerConfig,
    id: u64,
    address: SocketAddr,
    reader: impl Read,
    writer: SharedWriter,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    let mut session = Session { broker, config, id, writer, connected: false, verbose: false };
    let result = session.send(info(address, id).as_bytes()).and_then(|()| session.run(reader, stop));
    broker.clients().remove(&id);
    result
}

/// Serves NATS clients on `listener` until `stop` turns true.
pub fn serve(listener: &TcpListener, config: &ServerConfig, stop: &(dyn Fn() -> bool + Sync)) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let broker = Broker::new();
    let active = AtomicUsize::new(0);
    let next_id = AtomicU64::new(1);
    std::thread::scope(|scope| {
        while !stop() {
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(_) => {
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
            };
            if stream.set_nonblocking(false).is_err()
                || stream.set_read_timeout(Some(READ_POLL)).is_err()
                || stream.set_write_timeout(Some(WRITE_LIMIT)).is_err()
                || stream.set_nodelay(true).is_err()
            {
                continue;
            }
            if active.fetch_add(1, Ordering::AcqRel) >= config.max_connections {
                active.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            let (broker, active) = (&broker, &active);
            let spawned = std::thread::Builder::new().name("eggdb-nats".to_owned()).spawn_scoped(scope, move || {
                if let Ok(writer) = stream.try_clone() {
                    let shared: SharedWriter = Arc::new(Mutex::new(writer));
                    let _ = serve_stream(broker, config, id, address, ReadHalf(stream), shared, stop);
                }
                active.fetch_sub(1, Ordering::AcqRel);
            });
            if spawned.is_err() {
                active.fetch_sub(1, Ordering::AcqRel);
            }
        }
    });
    Ok(())
}

struct ReadHalf(TcpStream);

impl Read for ReadHalf {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}
