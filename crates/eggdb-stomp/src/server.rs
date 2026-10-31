use crate::frame::{Frame, next_frame};
use eggdb_keyspace::Keyspace;
use eggdb_queues::{QueueError, Queues};
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_POLL: Duration = Duration::from_millis(50);
const WRITE_LIMIT: Duration = Duration::from_secs(5);
const CONNECT_WAIT: Duration = Duration::from_secs(10);
const LEASE_MS: u64 = 30 * 60 * 1000;
const PREFETCH: usize = 100;
const MAX_SUBSCRIPTIONS: usize = 1_000;
const MAX_TRANSACTIONS: usize = 100;
const MAX_STAGED: usize = 10_000;
const VERSIONS: [&str; 3] = ["1.2", "1.1", "1.0"];

/// How the STOMP server runs.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// The user clients log in as.
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

struct TopicSubscriber {
    session: u64,
    subscription: String,
    writer: SharedWriter,
}

/// What every connection shares: the durable queues and the current topic subscribers.
pub(crate) struct Broker<'k> {
    queues: Queues<'k>,
    topics: Mutex<BTreeMap<String, Vec<TopicSubscriber>>>,
    next_topic_message: AtomicU64,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

enum Destination<'d> {
    Queue(&'d str),
    Topic,
}

fn destination(text: &str) -> Result<Destination<'_>, String> {
    if let Some(name) = text.strip_prefix("/queue/").filter(|name| !name.is_empty()) {
        return Ok(Destination::Queue(name));
    }
    if text.strip_prefix("/topic/").is_some_and(|name| !name.is_empty()) {
        return Ok(Destination::Topic);
    }
    Err(format!("the destination {text} is not supported: use /queue/<name> or /topic/<name>"))
}

fn queue_failure(error: &QueueError) -> String {
    error.to_string()
}

impl<'k> Broker<'k> {
    pub(crate) fn new(keyspace: &'k (dyn Keyspace + Sync)) -> Self {
        Self { queues: Queues::new(keyspace), topics: Mutex::new(BTreeMap::new()), next_topic_message: AtomicU64::new(1) }
    }

    fn topics(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<TopicSubscriber>>> {
        self.topics.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish_topic(&self, topic: &str, content_type: Option<&str>, body: &[u8]) {
        let topics = self.topics();
        let Some(subscribers) = topics.get(topic) else {
            return;
        };
        let id = self.next_topic_message.fetch_add(1, Ordering::Relaxed);
        for subscriber in subscribers {
            let mut frame = Frame::new("MESSAGE")
                .with("destination", topic)
                .with("message-id", &format!("topic-{id}"))
                .with("subscription", &subscriber.subscription);
            if let Some(content_type) = content_type {
                frame = frame.with("content-type", content_type);
            }
            let bytes = frame.with_body(body).encode();
            let _ = subscriber.writer.lock().unwrap_or_else(PoisonError::into_inner).write_all(&bytes);
        }
    }

    fn forget_session(&self, session: u64) {
        let mut topics = self.topics();
        for subscribers in topics.values_mut() {
            subscribers.retain(|subscriber| subscriber.session != session);
        }
        topics.retain(|_, subscribers| !subscribers.is_empty());
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AckMode {
    Auto,
    Client,
    ClientIndividual,
}

struct Subscription {
    destination: String,
    queue: Option<String>,
    ack: AckMode,
    prefetch: usize,
}

struct Pending {
    subscription: String,
    queue: String,
    id: u64,
    sequence: u64,
}

enum Staged {
    Send { destination: String, content_type: Option<String>, body: Vec<u8> },
    Ack(String),
    Nack(String),
}

enum Next {
    Continue,
    Close,
}

struct Session<'b, 'k> {
    broker: &'b Broker<'k>,
    config: &'b ServerConfig,
    id: u64,
    writer: SharedWriter,
    connected: bool,
    subscriptions: BTreeMap<String, Subscription>,
    pending: BTreeMap<String, Pending>,
    next_sequence: u64,
    transactions: BTreeMap<String, Vec<Staged>>,
}

impl Session<'_, '_> {
    fn send(&self, frame: &Frame) -> io::Result<()> {
        self.writer.lock().unwrap_or_else(PoisonError::into_inner).write_all(&frame.encode())
    }

    fn refuse(&self, message: &str, request: Option<&Frame>) -> io::Result<Next> {
        let mut frame = Frame::new("ERROR").with("message", message).with("content-type", "text/plain");
        if let Some(receipt) = request.and_then(|request| request.header("receipt")) {
            frame = frame.with("receipt-id", receipt);
        }
        self.send(&frame.with_body(message.as_bytes()))?;
        Ok(Next::Close)
    }

    fn connect(&mut self, frame: &Frame) -> io::Result<Next> {
        let offered = frame.header("accept-version").unwrap_or("1.0");
        let Some(version) = VERSIONS.iter().find(|version| offered.split(',').any(|offer| offer.trim() == **version)) else {
            let refusal = Frame::new("ERROR").with("version", "1.0,1.1,1.2").with("message", "Supported protocol versions are 1.0, 1.1 and 1.2");
            self.send(&refusal)?;
            return Ok(Next::Close);
        };
        let login = frame.header("login").unwrap_or_default();
        let passcode = frame.header("passcode").unwrap_or_default();
        let authorized = eggdb_crypto::constant_time_eq(login.as_bytes(), self.config.user.as_bytes())
            && eggdb_crypto::constant_time_eq(&eggdb_crypto::sha256(passcode.as_bytes()), &self.config.password_sha256);
        if !authorized {
            return self.refuse("Authentication failed: login and passcode must be the server's user and password", None);
        }
        self.connected = true;
        let connected = Frame::new("CONNECTED")
            .with("version", version)
            .with("heart-beat", "0,0")
            .with("server", "EggDB")
            .with("session", &self.id.to_string());
        self.send(&connected)?;
        Ok(Next::Continue)
    }

    fn store(&self, destination_text: &str, content_type: Option<&str>, body: &[u8]) -> Result<(), String> {
        match destination(destination_text)? {
            Destination::Queue(name) => {
                self.broker.queues.declare(name).map_err(|error| queue_failure(&error))?;
                self.broker.queues.send(name, body, 0, now_ms()).map_err(|error| queue_failure(&error))?;
            }
            Destination::Topic => self.broker.publish_topic(destination_text, content_type, body),
        }
        Ok(())
    }

    fn settle(&mut self, token: &str, accepted: bool) -> Result<(), String> {
        let Some(chosen) = self.pending.get(token) else {
            return Err(format!("no message waits for an acknowledgement with id {token}"));
        };
        let cumulative = self.subscriptions.get(&chosen.subscription).is_some_and(|subscription| subscription.ack == AckMode::Client);
        let (subscription, sequence) = (chosen.subscription.clone(), chosen.sequence);
        let tokens: Vec<String> = if cumulative {
            self.pending
                .iter()
                .filter(|(_, pending)| pending.subscription == subscription && pending.sequence <= sequence)
                .map(|(token, _)| token.clone())
                .collect()
        } else {
            vec![token.to_owned()]
        };
        for token in tokens {
            if let Some(pending) = self.pending.remove(&token) {
                let outcome = if accepted {
                    self.broker.queues.ack(&pending.queue, pending.id)
                } else {
                    self.broker.queues.release(&pending.queue, pending.id, now_ms())
                };
                outcome.map_err(|error| queue_failure(&error))?;
            }
        }
        Ok(())
    }

    fn subscribe(&mut self, frame: &Frame) -> Result<(), String> {
        let destination_text = frame.header("destination").ok_or("SUBSCRIBE needs a destination header")?;
        let id = frame.header("id").ok_or("SUBSCRIBE needs an id header")?;
        if self.subscriptions.contains_key(id) {
            return Err(format!("the subscription id {id} is already in use"));
        }
        if self.subscriptions.len() >= MAX_SUBSCRIPTIONS {
            return Err("too many subscriptions on this connection".to_owned());
        }
        let ack = match frame.header("ack").unwrap_or("auto") {
            "auto" => AckMode::Auto,
            "client" => AckMode::Client,
            "client-individual" => AckMode::ClientIndividual,
            other => return Err(format!("the ack mode {other} is not one of auto, client and client-individual")),
        };
        let prefetch = frame.header("prefetch-count").and_then(|count| count.parse::<usize>().ok()).filter(|count| *count > 0).unwrap_or(PREFETCH).min(PREFETCH);
        let queue = match destination(destination_text)? {
            Destination::Queue(name) => {
                self.broker.queues.declare(name).map_err(|error| queue_failure(&error))?;
                Some(name.to_owned())
            }
            Destination::Topic => {
                self.broker.topics().entry(destination_text.to_owned()).or_default().push(TopicSubscriber {
                    session: self.id,
                    subscription: id.to_owned(),
                    writer: Arc::clone(&self.writer),
                });
                None
            }
        };
        self.subscriptions.insert(id.to_owned(), Subscription { destination: destination_text.to_owned(), queue, ack, prefetch });
        Ok(())
    }

    fn unsubscribe(&mut self, id: &str) -> Result<(), String> {
        let subscription = self.subscriptions.remove(id).ok_or_else(|| format!("no subscription has the id {id}"))?;
        if subscription.queue.is_none() {
            let mut topics = self.broker.topics();
            if let Some(subscribers) = topics.get_mut(&subscription.destination) {
                subscribers.retain(|subscriber| subscriber.session != self.id || subscriber.subscription != id);
            }
        }
        self.release_where(|pending| pending.subscription == id);
        Ok(())
    }

    fn release_where(&mut self, chosen: impl Fn(&Pending) -> bool) {
        let tokens: Vec<String> = self.pending.iter().filter(|(_, pending)| chosen(pending)).map(|(token, _)| token.clone()).collect();
        let now = now_ms();
        for token in tokens {
            if let Some(pending) = self.pending.remove(&token) {
                let _ = self.broker.queues.release(&pending.queue, pending.id, now);
            }
        }
    }

    fn ack_token(frame: &Frame) -> Option<&str> {
        frame.header("id").or_else(|| frame.header("message-id"))
    }

    fn stage(&mut self, transaction: &str, staged: Staged) -> Result<(), String> {
        let steps = self.transactions.get_mut(transaction).ok_or_else(|| format!("the transaction {transaction} was not begun"))?;
        if steps.len() >= MAX_STAGED {
            return Err("the transaction holds too many frames".to_owned());
        }
        steps.push(staged);
        Ok(())
    }

    fn commit(&mut self, transaction: &str) -> Result<(), String> {
        let steps = self.transactions.remove(transaction).ok_or_else(|| format!("the transaction {transaction} was not begun"))?;
        let mut queued: Vec<(&str, &[u8])> = Vec::new();
        for step in &steps {
            if let Staged::Send { destination: text, body, .. } = step
                && let Destination::Queue(name) = destination(text)?
            {
                self.broker.queues.declare(name).map_err(|error| queue_failure(&error))?;
                queued.push((name, body));
            }
        }
        self.broker.queues.send_all(&queued, now_ms()).map_err(|error| queue_failure(&error))?;
        for step in steps {
            match step {
                Staged::Send { destination: text, content_type, body } => {
                    if matches!(destination(&text)?, Destination::Topic) {
                        self.broker.publish_topic(&text, content_type.as_deref(), &body);
                    }
                }
                Staged::Ack(token) => self.settle(&token, true)?,
                Staged::Nack(token) => self.settle(&token, false)?,
            }
        }
        Ok(())
    }

    fn apply(&mut self, frame: &Frame) -> Result<Next, String> {
        let transaction = frame.header("transaction").map(str::to_owned);
        match frame.command.as_str() {
            "SEND" => {
                let destination_text = frame.header("destination").ok_or("SEND needs a destination header")?;
                destination(destination_text)?;
                let content_type = frame.header("content-type");
                match transaction {
                    Some(transaction) => self.stage(
                        &transaction,
                        Staged::Send { destination: destination_text.to_owned(), content_type: content_type.map(str::to_owned), body: frame.body.clone() },
                    )?,
                    None => self.store(destination_text, content_type, &frame.body)?,
                }
            }
            "SUBSCRIBE" => self.subscribe(frame)?,
            "UNSUBSCRIBE" => self.unsubscribe(frame.header("id").ok_or("UNSUBSCRIBE needs an id header")?)?,
            "ACK" | "NACK" => {
                let token = Self::ack_token(frame).ok_or("ACK and NACK need an id header")?.to_owned();
                let accepted = frame.command == "ACK";
                match transaction {
                    Some(transaction) => self.stage(&transaction, if accepted { Staged::Ack(token) } else { Staged::Nack(token) })?,
                    None => self.settle(&token, accepted)?,
                }
            }
            "BEGIN" => {
                let transaction = transaction.ok_or("BEGIN needs a transaction header")?;
                if self.transactions.contains_key(&transaction) || self.transactions.len() >= MAX_TRANSACTIONS {
                    return Err(format!("the transaction {transaction} cannot begin: it is already open or too many are"));
                }
                self.transactions.insert(transaction, Vec::new());
            }
            "COMMIT" => self.commit(&transaction.ok_or("COMMIT needs a transaction header")?)?,
            "ABORT" => {
                let transaction = transaction.ok_or("ABORT needs a transaction header")?;
                self.transactions.remove(&transaction).ok_or_else(|| format!("the transaction {transaction} was not begun"))?;
            }
            "DISCONNECT" => return Ok(Next::Close),
            other => return Err(format!("the command {other} is not a STOMP client frame")),
        }
        Ok(Next::Continue)
    }

    fn handle(&mut self, frame: &Frame) -> io::Result<Next> {
        if matches!(frame.command.as_str(), "CONNECT" | "STOMP") {
            if self.connected {
                return self.refuse("the connection is already established", Some(frame));
            }
            return self.connect(frame);
        }
        if !self.connected {
            return self.refuse("the first frame must be CONNECT or STOMP", Some(frame));
        }
        match self.apply(frame) {
            Ok(next) => {
                if let Some(receipt) = frame.header("receipt") {
                    self.send(&Frame::new("RECEIPT").with("receipt-id", receipt))?;
                }
                Ok(next)
            }
            Err(message) => self.refuse(&message, Some(frame)),
        }
    }

    fn pump(&mut self) -> io::Result<()> {
        let waiting: Vec<(String, String, AckMode, usize)> = self
            .subscriptions
            .iter()
            .filter_map(|(id, subscription)| {
                let queue = subscription.queue.clone()?;
                let outstanding = self.pending.values().filter(|pending| &pending.subscription == id).count();
                let room = if subscription.ack == AckMode::Auto { subscription.prefetch } else { subscription.prefetch.saturating_sub(outstanding) };
                (room > 0).then(|| (id.clone(), queue, subscription.ack, room))
            })
            .collect();
        for (id, queue, ack, room) in waiting {
            let Ok(messages) = self.broker.queues.receive(&queue, room, LEASE_MS, now_ms()) else {
                continue;
            };
            for message in messages {
                let message_id = format!("{queue}-{}", message.id);
                let mut frame = Frame::new("MESSAGE")
                    .with("destination", &format!("/queue/{queue}"))
                    .with("message-id", &message_id)
                    .with("subscription", &id);
                if message.receive_count > 1 {
                    frame = frame.with("redelivered", "true");
                }
                if ack != AckMode::Auto {
                    frame = frame.with("ack", &message_id);
                }
                let sent = self.send(&frame.with_body(&message.body));
                if ack == AckMode::Auto {
                    if sent.is_ok() {
                        let _ = self.broker.queues.ack(&queue, message.id);
                    } else {
                        let _ = self.broker.queues.release(&queue, message.id, now_ms());
                    }
                } else {
                    self.next_sequence += 1;
                    self.pending.insert(message_id, Pending { subscription: id.clone(), queue: queue.clone(), id: message.id, sequence: self.next_sequence });
                }
                sent?;
            }
        }
        Ok(())
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
                        if let Next::Close = self.handle(&frame)? {
                            return Ok(());
                        }
                    }
                    Ok(None) => break,
                    Err(problem) => {
                        self.refuse(problem, None)?;
                        return Ok(());
                    }
                }
            }
            if stop() || (!self.connected && started.elapsed() > CONNECT_WAIT) {
                return Ok(());
            }
            if self.connected {
                self.pump()?;
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

pub(crate) fn serve_stream(broker: &Broker<'_>, config: &ServerConfig, id: u64, reader: impl Read, writer: SharedWriter, stop: &(dyn Fn() -> bool + Sync)) -> io::Result<()> {
    let mut session = Session {
        broker,
        config,
        id,
        writer,
        connected: false,
        subscriptions: BTreeMap::new(),
        pending: BTreeMap::new(),
        next_sequence: 0,
        transactions: BTreeMap::new(),
    };
    let result = session.run(reader, stop);
    session.release_where(|_| true);
    broker.forget_session(id);
    result
}

/// Serves STOMP clients on `listener`, keeping `/queue/` destinations in `keyspace`, until
/// `stop` turns true.
pub fn serve(keyspace: &(dyn Keyspace + Sync), listener: &TcpListener, config: &ServerConfig, stop: &(dyn Fn() -> bool + Sync)) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let broker = Broker::new(keyspace);
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
            let spawned = std::thread::Builder::new().name("eggdb-stomp".to_owned()).spawn_scoped(scope, move || {
                if let Ok(writer) = stream.try_clone() {
                    let shared: SharedWriter = Arc::new(Mutex::new(writer));
                    let _ = serve_stream(broker, config, id, ReadHalf(stream), shared, stop);
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
