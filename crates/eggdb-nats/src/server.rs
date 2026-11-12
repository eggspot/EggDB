use crate::protocol::{
    Frame, MAX_PAYLOAD, message, next_frame, pending_frame_bytes, subject_matches, valid_subject,
};
use crate::jetstream::{JetStream, Outgoing};
use eggdb_keyspace::Keyspace;
use eggdb_platform::connection_limits::MemoryBudget;
use eggdb_sql::json::{Json, Node, parse_document};
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

const ACCEPT_POLL: Duration = Duration::from_millis(100);
const JETSTREAM_TICK: Duration = Duration::from_secs(1);
const READ_POLL: Duration = Duration::from_millis(200);
const WRITE_LIMIT: Duration = Duration::from_secs(5);
const CONNECT_WAIT: Duration = Duration::from_secs(10);
const MAX_SUBSCRIPTIONS: usize = 10_000;
const READ_CHUNK_BYTES: usize = 64 * 1024;
const MESSAGE_LINE_BYTES: usize = 64;
pub(crate) const OUT_OF_MEMORY_BUDGET: &str = "Server Memory Budget Exceeded";

/// How the NATS server runs.
#[derive(Clone)]
pub struct ServerConfig {
    /// The user clients connect as.
    pub user: String,
    /// The SHA-256 of that user's password.
    pub password_sha256: [u8; 32],
    /// The most connections served at once.
    pub max_connections: usize,
    /// The memory every connection's unread messages and deliveries hold together, shared
    /// with the server's other listeners (D284, D307).
    pub memory_budget: Arc<MemoryBudget>,
    /// The user NKeys (public keys beginning with `U`) that may sign in by signing the
    /// server's nonce instead of giving the password.
    pub nkeys: Vec<String>,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("user", &self.user)
            .finish_non_exhaustive()
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
    no_responders: bool,
}

const NO_RESPONDERS_HEADER: &[u8] = b"NATS/1.0 503\r\n\r\n";

/// What every connection shares: who subscribes to what, and JetStream when it is on.
pub(crate) struct Broker<'k> {
    clients: Mutex<BTreeMap<u64, Client>>,
    turn: AtomicUsize,
    jetstream: Option<JetStream<'k>>,
}

const SERVER_CLIENT: u64 = 0;

struct Published<'p> {
    from: u64,
    subject: &'p str,
    reply: Option<&'p str>,
    headers: &'p [u8],
    payload: &'p [u8],
    answered: bool,
}

impl<'k> Broker<'k> {
    pub(crate) fn new() -> Self {
        Self {
            clients: Mutex::new(BTreeMap::new()),
            turn: AtomicUsize::new(0),
            jetstream: None,
        }
    }

    pub(crate) fn with_jetstream(keyspace: &'k (dyn Keyspace + Sync)) -> Self {
        Self {
            jetstream: Some(JetStream::new(keyspace)),
            ..Self::new()
        }
    }

    fn send_all(&self, outgoing: Vec<Outgoing>, budget: &MemoryBudget) {
        for message in outgoing {
            self.publish(
                &Published {
                    from: SERVER_CLIENT,
                    subject: &message.subject,
                    reply: message.reply.as_deref(),
                    headers: &message.headers,
                    payload: &message.payload,
                    answered: true,
                },
                budget,
            );
        }
    }

    fn clients(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Client>> {
        self.clients.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish(&self, published: &Published<'_>, budget: &MemoryBudget) {
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
                    Some(queue) => groups
                        .entry(queue.as_str())
                        .or_default()
                        .push((id, sid.clone())),
                    None => targets.push((id, sid.clone())),
                }
            }
        }
        let turn = self.turn.fetch_add(1, Ordering::Relaxed);
        for members in groups.into_values() {
            targets.push(members[turn % members.len()].clone());
        }
        if targets.is_empty()
            && !published.answered
            && let Some(reply) = published.reply
            && let Some(publisher) = clients.get(&published.from)
            && publisher.no_responders
            && publisher.headers
        {
            let answers: Vec<String> = publisher
                .subscriptions
                .iter()
                .filter(|(_, subscription)| subject_matches(&subscription.subject, reply))
                .map(|(sid, _)| sid.clone())
                .collect();
            let writer = Arc::clone(&publisher.writer);
            drop(clients);
            for sid in answers {
                let packet = message(reply, &sid, None, NO_RESPONDERS_HEADER, b"", true);
                let _ = writer
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .write_all(&packet);
            }
            return;
        }
        let mut deliveries: Vec<(SharedWriter, String, bool)> = Vec::with_capacity(targets.len());
        for (id, sid) in targets {
            let Some(client) = clients.get_mut(&id) else {
                continue;
            };
            deliveries.push((Arc::clone(&client.writer), sid.clone(), client.headers));
            if let Some(subscription) = client.subscriptions.get_mut(&sid) {
                subscription.delivered += 1;
                if subscription
                    .max
                    .is_some_and(|max| subscription.delivered >= max)
                {
                    client.subscriptions.remove(&sid);
                }
            }
        }
        drop(clients);
        let mut held = budget.empty_reservation();
        for (writer, sid, headers) in deliveries {
            let packet_bytes = published
                .payload
                .len()
                .saturating_add(published.headers.len())
                .saturating_add(published.subject.len())
                .saturating_add(published.reply.map_or(0, str::len))
                .saturating_add(sid.len())
                .saturating_add(MESSAGE_LINE_BYTES);
            if !held.hold_if_large(packet_bytes) {
                continue;
            }
            let packet = message(
                published.subject,
                &sid,
                published.reply,
                published.headers,
                published.payload,
                headers,
            );
            let _ = writer
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .write_all(&packet);
        }
    }
}

struct Session<'b> {
    broker: &'b Broker<'b>,
    config: &'b ServerConfig,
    id: u64,
    writer: SharedWriter,
    connected: bool,
    verbose: bool,
    nonce: String,
}

fn fresh_nonce(id: u64) -> String {
    use std::hash::{BuildHasher, RandomState};
    let state = RandomState::new();
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&state.hash_one((id, time, 0u8)).to_le_bytes());
    bytes.extend_from_slice(&state.hash_one((id, time, 1u8)).to_le_bytes());
    eggdb_crypto::base64_encode(&bytes)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

enum Next {
    Continue,
    Close,
}

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Json> {
    match &node.value {
        Json::Object(members) => members
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, node)| &node.value),
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
        self.writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write_all(bytes)
    }

    fn error(&self, message: &str) -> io::Result<()> {
        self.send(format!("-ERR '{message}'\r\n").as_bytes())
    }

    fn ok(&self) -> io::Result<()> {
        if self.verbose {
            self.send(b"+OK\r\n")
        } else {
            Ok(())
        }
    }

    fn connect(&mut self, options: &str) -> io::Result<Next> {
        let Ok(options) = parse_document(options) else {
            self.error("Invalid CONNECT options")?;
            return Ok(Next::Close);
        };
        let user = text(&options, "user").unwrap_or_default();
        let password = text(&options, "pass").unwrap_or_default();
        let nkey = text(&options, "nkey");
        let authorized = match text(&options, "auth_token") {
            _ if nkey.is_some() => nkey.as_deref().is_some_and(|nkey| {
                self.config.nkeys.iter().any(|allowed| allowed == nkey)
                    && crate::nkeys::signed_nonce(
                        nkey,
                        &self.nonce,
                        &text(&options, "sig").unwrap_or_default(),
                    )
            }),
            Some(token) if user.is_empty() => eggdb_crypto::constant_time_eq(
                &eggdb_crypto::sha256(token.as_bytes()),
                &self.config.password_sha256,
            ),
            _ => {
                eggdb_crypto::constant_time_eq(user.as_bytes(), self.config.user.as_bytes())
                    && eggdb_crypto::constant_time_eq(
                        &eggdb_crypto::sha256(password.as_bytes()),
                        &self.config.password_sha256,
                    )
            }
        };
        let headers = flag(&options, "headers", false);
        if flag(&options, "no_responders", false) && !headers {
            self.error("no responders requires headers support")?;
            return Ok(Next::Close);
        }
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
                    headers,
                    no_responders: flag(&options, "no_responders", false),
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
            Frame::Publish {
                subject,
                reply,
                headers,
                payload,
            } => {
                let reply_valid = reply
                    .as_deref()
                    .is_none_or(|reply| valid_subject(reply, false));
                if !valid_subject(&subject, false) || !reply_valid {
                    self.error("Invalid Subject")?;
                } else if let Some(jetstream) = self
                    .broker
                    .jetstream
                    .as_ref()
                    .filter(|_| JetStream::handles(&subject))
                {
                    let answers = jetstream.request(&subject, reply.as_deref(), &payload);
                    self.broker.send_all(answers, &self.config.memory_budget);
                    self.ok()?;
                } else {
                    let stored = self.broker.jetstream.as_ref().map_or_else(Vec::new, |jetstream| {
                        jetstream.store(&subject, reply.as_deref(), &headers, &payload)
                    });
                    self.broker.publish(
                        &Published {
                            from: self.id,
                            subject: &subject,
                            reply: reply.as_deref(),
                            headers: &headers,
                            payload: &payload,
                            answered: !stored.is_empty(),
                        },
                        &self.config.memory_budget,
                    );
                    self.broker.send_all(stored, &self.config.memory_budget);
                    self.ok()?;
                }
            }
            Frame::Subscribe {
                subject,
                queue,
                sid,
            } => {
                let queue_valid = queue
                    .as_deref()
                    .is_none_or(|queue| valid_subject(queue, false));
                if !valid_subject(&subject, true) || !queue_valid {
                    self.error("Invalid Subject")?;
                    return Ok(Next::Continue);
                }
                let mut clients = self.broker.clients();
                let Some(client) = clients.get_mut(&self.id) else {
                    return Ok(Next::Close);
                };
                if client.subscriptions.len() >= MAX_SUBSCRIPTIONS
                    && !client.subscriptions.contains_key(&sid)
                {
                    drop(clients);
                    self.error("Maximum Subscriptions Exceeded")?;
                    return Ok(Next::Continue);
                }
                client.subscriptions.insert(
                    sid,
                    Subscription {
                        subject,
                        queue,
                        delivered: 0,
                        max: None,
                    },
                );
                drop(clients);
                self.ok()?;
            }
            Frame::Unsubscribe { sid, max } => {
                let mut clients = self.broker.clients();
                if let Some(client) = clients.get_mut(&self.id) {
                    match (client.subscriptions.get_mut(&sid), max) {
                        (Some(subscription), Some(max)) if subscription.delivered < max => {
                            subscription.max = Some(max)
                        }
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
        let config = self.config;
        let mut held = config.memory_budget.empty_reservation();
        let mut buffer = Vec::new();
        let mut chunk = vec![0; READ_CHUNK_BYTES];
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
            if !held.hold_if_large(pending_frame_bytes(&buffer)) {
                return self.error(OUT_OF_MEMORY_BUDGET);
            }
            match reader.read(&mut chunk) {
                Ok(0) => return Ok(()),
                Ok(read) => {
                    let grown = buffer.len().saturating_add(read);
                    if !held.hold_if_large(grown.max(pending_frame_bytes(&buffer))) {
                        return self.error(OUT_OF_MEMORY_BUDGET);
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
        }
    }
}

fn info(address: SocketAddr, client_id: u64, nonce: &str, jetstream: bool) -> String {
    format!(
        "INFO {{\"server_id\":\"EGGDB\",\"server_name\":\"eggdb\",\"version\":\"2.10.0\",\"proto\":1,\"go\":\"eggdb\",\"host\":\"{}\",\"port\":{},\"headers\":true,\"auth_required\":true,\"jetstream\":{jetstream},\"max_payload\":{MAX_PAYLOAD},\"client_id\":{client_id},\"nonce\":\"{nonce}\"}}\r\n",
        address.ip(),
        address.port()
    )
}

pub(crate) fn serve_stream(
    broker: &Broker<'_>,
    config: &ServerConfig,
    id: u64,
    address: SocketAddr,
    reader: impl Read,
    writer: SharedWriter,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    let mut session = Session {
        broker,
        config,
        id,
        writer,
        connected: false,
        verbose: false,
        nonce: fresh_nonce(id),
    };
    let result = session
        .send(info(address, id, &session.nonce, broker.jetstream.is_some()).as_bytes())
        .and_then(|()| session.run(reader, stop));
    broker.clients().remove(&id);
    result
}

/// Serves NATS clients on `listener` until `stop` turns true, with core NATS only.
pub fn serve(
    listener: &TcpListener,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    serve_broker(Broker::new(), listener, config, stop)
}

/// Serves NATS clients on `listener` until `stop` turns true, with JetStream's streams and
/// consumers kept in `keyspace`.
pub fn serve_with_jetstream(
    keyspace: &(dyn Keyspace + Sync),
    listener: &TcpListener,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    serve_broker(Broker::with_jetstream(keyspace), listener, config, stop)
}

fn serve_broker(
    broker: Broker<'_>,
    listener: &TcpListener,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let active = AtomicUsize::new(0);
    let broker = &broker;
    let next_id = AtomicU64::new(1);
    std::thread::scope(|scope| {
        if let Some(jetstream) = &broker.jetstream {
            let _ = std::thread::Builder::new()
                .name("eggdb-nats-jetstream".to_owned())
                .spawn_scoped(scope, move || {
                    while !stop() {
                        std::thread::sleep(JETSTREAM_TICK);
                        broker.send_all(jetstream.tick(), &config.memory_budget);
                    }
                });
        }
        while !stop() {
            let stream = match eggdb_platform::accept_tcp(listener) {
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
            let active = &active;
            let spawned = std::thread::Builder::new()
                .name("eggdb-nats".to_owned())
                .spawn_scoped(scope, move || {
                    if let Ok(writer) = stream.try_clone() {
                        let shared: SharedWriter = Arc::new(Mutex::new(writer));
                        let _ = serve_stream(
                            broker,
                            config,
                            id,
                            address,
                            ReadHalf(stream),
                            shared,
                            stop,
                        );
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
