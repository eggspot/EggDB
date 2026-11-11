use crate::frame::{Frame, next_frame, pending_frame_bytes};
use eggdb_keyspace::Keyspace;
use eggdb_platform::connection_limits::{MemoryBudget, Reservation};
use eggdb_queues::{QueueError, Queues};
use std::cell::Cell;
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
const MOST_STAGED_BYTES: usize = 64 << 20;
const VERSIONS: [&str; 3] = ["1.2", "1.1", "1.0"];
const MESSAGE_HEADER_BYTES: usize = 256;
pub(crate) const OUT_OF_MEMORY_BUDGET: &str =
    "the server's connections already hold their memory budget";
const HEART_BEAT: &[u8] = b"\n";

/// The heart-beats the server offers in `CONNECTED` (STOMP 1.1 and 1.2): `send` is how often it
/// can send them (the specification's `sx`), `expect` how often it wants the client's (`sy`).
/// Zero turns that direction off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeartBeatOffer {
    pub(crate) send: Duration,
    pub(crate) expect: Duration,
}

impl HeartBeatOffer {
    pub(crate) const STANDARD: Self = Self {
        send: Duration::from_secs(10),
        expect: Duration::from_secs(10),
    };

    fn header(self) -> String {
        format!("{},{}", self.send.as_millis(), self.expect.as_millis())
    }
}

/// What a connection agreed to: how often the server sends a heart-beat when it has sent
/// nothing else, and how long the client may stay silent before the server drops it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HeartBeats {
    pub(crate) send_every: Option<Duration>,
    pub(crate) silence_limit: Option<Duration>,
}

fn client_heart_beat(header: &str) -> Option<(u64, u64)> {
    let (send, expect) = header.split_once(',')?;
    Some((send.trim().parse().ok()?, expect.trim().parse().ok()?))
}

/// The heart-beats agreed from the server's `offer` and the client's `heart-beat` header
/// (`cx,cy`): the server sends every max(sx, cy) when both are above zero, and drops a client
/// silent for twice max(cx, sy) when both are above zero; `None` when the header is not two
/// non-negative integers.
pub(crate) fn negotiate(offer: HeartBeatOffer, header: Option<&str>) -> Option<HeartBeats> {
    let (client_send, client_expect) = match header {
        Some(text) => client_heart_beat(text)?,
        None => (0, 0),
    };
    let send_every = (client_expect > 0 && !offer.send.is_zero())
        .then(|| offer.send.max(Duration::from_millis(client_expect)));
    let silence_limit = (client_send > 0 && !offer.expect.is_zero()).then(|| {
        offer
            .expect
            .max(Duration::from_millis(client_send))
            .saturating_mul(2)
    });
    Some(HeartBeats {
        send_every,
        silence_limit,
    })
}

/// How the STOMP server runs.
#[derive(Clone)]
pub struct ServerConfig {
    /// The user clients log in as.
    pub user: String,
    /// The SHA-256 of that user's password.
    pub password_sha256: [u8; 32],
    /// The most connections served at once.
    pub max_connections: usize,
    /// The memory every connection's unread frames, staged transactions and deliveries hold
    /// together, shared with the server's other listeners (D284, D307).
    pub memory_budget: Arc<MemoryBudget>,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

pub(crate) type SharedWriter = Arc<Mutex<dyn Write + Send>>;

struct TopicSubscriber {
    session: u64,
    subscription: String,
    writer: SharedWriter,
    binding: Option<String>,
    selector: Option<crate::selector::Selector>,
}

const SERVER_HEADERS: [&str; 7] = [
    "destination",
    "content-length",
    "transaction",
    "receipt",
    "message-id",
    "subscription",
    "ack",
];

fn forwarded_headers(frame: &Frame) -> Vec<(String, String)> {
    let mut forwarded: Vec<(String, String)> = Vec::new();
    for (name, value) in &frame.headers {
        if SERVER_HEADERS.contains(&name.as_str()) || forwarded.iter().any(|(seen, _)| seen == name) {
            continue;
        }
        forwarded.push((name.clone(), value.clone()));
    }
    forwarded
}

fn binding_matches(pattern: &str, key: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('.').collect();
    let key: Vec<&str> = if key.is_empty() { Vec::new() } else { key.split('.').collect() };
    fn matches(pattern: &[&str], key: &[&str]) -> bool {
        match pattern.split_first() {
            None => key.is_empty(),
            Some((&"#", rest)) => (0..=key.len()).any(|skip| matches(rest, &key[skip..])),
            Some((&"*", rest)) => !key.is_empty() && matches(rest, &key[1..]),
            Some((word, rest)) => key.first() == Some(word) && matches(rest, &key[1..]),
        }
    }
    matches(&pattern, &key)
}

/// What every connection shares: the durable queues, the current topic subscribers and the
/// heart-beats the server offers.
pub(crate) struct Broker<'k> {
    queues: Queues<'k>,
    topics: Mutex<BTreeMap<String, Vec<TopicSubscriber>>>,
    next_topic_message: AtomicU64,
    heart_beat_offer: HeartBeatOffer,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

enum Destination<'d> {
    Queue(&'d str),
    Topic,
    Exchange { exchange: &'d str, key: &'d str },
}

fn destination(text: &str) -> Result<Destination<'_>, String> {
    if let Some(name) = text.strip_prefix("/queue/").filter(|name| !name.is_empty()) {
        return Ok(Destination::Queue(name));
    }
    if let Some(rest) = text.strip_prefix("/exchange/").filter(|rest| !rest.is_empty()) {
        let (exchange, key) = rest.split_once('/').unwrap_or((rest, ""));
        if exchange.is_empty() {
            return Err(format!("the destination {text} names no exchange"));
        }
        return Ok(Destination::Exchange { exchange, key });
    }
    if text
        .strip_prefix("/topic/")
        .is_some_and(|name| !name.is_empty())
    {
        return Ok(Destination::Topic);
    }
    Err(format!(
        "the destination {text} is not supported: use /queue/<name>, /topic/<name> or /exchange/<name>/<key>"
    ))
}

fn queue_failure(error: &QueueError) -> String {
    error.to_string()
}

impl<'k> Broker<'k> {
    pub(crate) fn new(
        keyspace: &'k (dyn Keyspace + Sync),
        heart_beat_offer: HeartBeatOffer,
    ) -> Self {
        Self {
            queues: Queues::new(keyspace),
            topics: Mutex::new(BTreeMap::new()),
            next_topic_message: AtomicU64::new(1),
            heart_beat_offer,
        }
    }

    fn topics(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<TopicSubscriber>>> {
        self.topics.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish_topic(
        &self,
        topic: &str,
        headers: &[(String, String)],
        body: &[u8],
        budget: &MemoryBudget,
    ) {
        let (subscribed_to, key) = match destination(topic) {
            Ok(Destination::Exchange { exchange, key }) => (format!("/exchange/{exchange}"), Some(key)),
            _ => (topic.to_owned(), None),
        };
        let topics = self.topics();
        let Some(subscribers) = topics.get(&subscribed_to) else {
            return;
        };
        let id = self.next_topic_message.fetch_add(1, Ordering::Relaxed);
        let deliveries: Vec<(SharedWriter, String)> = subscribers
            .iter()
            .filter(|subscriber| match (&subscriber.binding, key) {
                (Some(binding), Some(key)) => binding_matches(binding, key),
                _ => true,
            })
            .filter(|subscriber| {
                subscriber
                    .selector
                    .as_ref()
                    .is_none_or(|selector| selector.chooses(headers))
            })
            .map(|subscriber| {
                (
                    Arc::clone(&subscriber.writer),
                    subscriber.subscription.clone(),
                )
            })
            .collect();
        drop(topics);
        let header_bytes: usize = headers
            .iter()
            .map(|(name, value)| name.len() + value.len() + 2)
            .sum();
        let frame_bytes = body
            .len()
            .saturating_add(topic.len())
            .saturating_add(header_bytes)
            .saturating_add(MESSAGE_HEADER_BYTES);
        let mut held = budget.empty_reservation();
        for (writer, subscription) in deliveries {
            if !held.hold_if_large(frame_bytes.saturating_add(subscription.len())) {
                continue;
            }
            let mut frame = Frame::new("MESSAGE")
                .with("destination", topic)
                .with("message-id", &format!("topic-{id}"))
                .with("subscription", &subscription);
            for (name, value) in headers {
                frame = frame.with(name, value);
            }
            let bytes = frame.with_body(body).encode();
            let _ = writer
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .write_all(&bytes);
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
    Send {
        destination: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    Ack(String),
    Nack(String),
    Reject(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settlement {
    Accept,
    Requeue,
    DeadLetter,
}

impl Staged {
    fn body_bytes(&self) -> usize {
        match self {
            Self::Send { body, .. } => body.len(),
            Self::Ack(_) | Self::Nack(_) | Self::Reject(_) => 0,
        }
    }
}

enum Next {
    Continue,
    Close,
}

struct Session<'b, 'k> {
    broker: &'b Broker<'k>,
    config: &'b ServerConfig,
    heart_beats: HeartBeats,
    last_sent: Cell<Instant>,
    staged_held: Reservation<'b>,
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
        self.send_bytes(&frame.encode())
    }

    fn send_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        self.writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write_all(bytes)?;
        self.last_sent.set(Instant::now());
        Ok(())
    }

    fn send_heart_beat_when_due(&self) -> io::Result<()> {
        match self.heart_beats.send_every {
            Some(every) if self.last_sent.get().elapsed() >= every => self.send_bytes(HEART_BEAT),
            _ => Ok(()),
        }
    }

    fn refuse(&self, message: &str, request: Option<&Frame>) -> io::Result<Next> {
        let mut frame = Frame::new("ERROR")
            .with("message", message)
            .with("content-type", "text/plain");
        if let Some(receipt) = request.and_then(|request| request.header("receipt")) {
            frame = frame.with("receipt-id", receipt);
        }
        self.send(&frame.with_body(message.as_bytes()))?;
        Ok(Next::Close)
    }

    fn connect(&mut self, frame: &Frame) -> io::Result<Next> {
        let offered = frame.header("accept-version").unwrap_or("1.0");
        let Some(version) = VERSIONS
            .iter()
            .find(|version| offered.split(',').any(|offer| offer.trim() == **version))
        else {
            let refusal = Frame::new("ERROR").with("version", "1.0,1.1,1.2").with(
                "message",
                "Supported protocol versions are 1.0, 1.1 and 1.2",
            );
            self.send(&refusal)?;
            return Ok(Next::Close);
        };
        let login = frame.header("login").unwrap_or_default();
        let passcode = frame.header("passcode").unwrap_or_default();
        let authorized =
            eggdb_crypto::constant_time_eq(login.as_bytes(), self.config.user.as_bytes())
                && eggdb_crypto::constant_time_eq(
                    &eggdb_crypto::sha256(passcode.as_bytes()),
                    &self.config.password_sha256,
                );
        if !authorized {
            return self.refuse(
                "Authentication failed: login and passcode must be the server's user and password",
                None,
            );
        }
        let offer = self.broker.heart_beat_offer;
        let Some(heart_beats) = negotiate(offer, frame.header("heart-beat")) else {
            return self.refuse(
                "the heart-beat header must be two non-negative integers separated by a comma, such as 10000,10000",
                None,
            );
        };
        self.heart_beats = heart_beats;
        self.connected = true;
        let connected = Frame::new("CONNECTED")
            .with("version", version)
            .with("heart-beat", &offer.header())
            .with("server", "EggDB")
            .with("session", &self.id.to_string());
        self.send(&connected)?;
        Ok(Next::Continue)
    }

    fn store(
        &self,
        destination_text: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<(), String> {
        match destination(destination_text)? {
            Destination::Queue(name) => {
                self.broker
                    .queues
                    .declare(name)
                    .map_err(|error| queue_failure(&error))?;
                self.broker
                    .queues
                    .send(name, body, 0, now_ms())
                    .map_err(|error| queue_failure(&error))?;
            }
            Destination::Topic | Destination::Exchange { .. } => self.broker.publish_topic(
                destination_text,
                headers,
                body,
                &self.config.memory_budget,
            ),
        }
        Ok(())
    }

    fn settle(&mut self, token: &str, settlement: Settlement) -> Result<(), String> {
        let Some(chosen) = self.pending.get(token) else {
            return Err(format!(
                "no message waits for an acknowledgement with id {token}"
            ));
        };
        let cumulative = self
            .subscriptions
            .get(&chosen.subscription)
            .is_some_and(|subscription| subscription.ack == AckMode::Client);
        let (subscription, sequence) = (chosen.subscription.clone(), chosen.sequence);
        let tokens: Vec<String> = if cumulative {
            self.pending
                .iter()
                .filter(|(_, pending)| {
                    pending.subscription == subscription && pending.sequence <= sequence
                })
                .map(|(token, _)| token.clone())
                .collect()
        } else {
            vec![token.to_owned()]
        };
        for token in tokens {
            if let Some(pending) = self.pending.remove(&token) {
                let outcome = match settlement {
                    Settlement::Accept => self.broker.queues.ack(&pending.queue, pending.id),
                    Settlement::Requeue => self.broker.queues.release(&pending.queue, pending.id, now_ms()),
                    Settlement::DeadLetter => self.broker.queues.dead_letter(&pending.queue, pending.id, now_ms()),
                };
                outcome.map_err(|error| queue_failure(&error))?;
            }
        }
        Ok(())
    }

    fn subscribe(&mut self, frame: &Frame) -> Result<(), String> {
        let destination_text = frame
            .header("destination")
            .ok_or("SUBSCRIBE needs a destination header")?;
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
            other => {
                return Err(format!(
                    "the ack mode {other} is not one of auto, client and client-individual"
                ));
            }
        };
        let prefetch = frame
            .header("prefetch-count")
            .and_then(|count| count.parse::<usize>().ok())
            .filter(|count| *count > 0)
            .unwrap_or(PREFETCH)
            .min(PREFETCH);
        let selector = frame
            .header("selector")
            .filter(|text| !text.trim().is_empty())
            .map(crate::selector::Selector::parse)
            .transpose()
            .map_err(|why| format!("the selector cannot be read: {why}"))?;
        let queue = match destination(destination_text)? {
            Destination::Queue(name) => {
                if selector.is_some() {
                    return Err(
                        "selectors apply to /topic/ and /exchange/ subscriptions only".to_owned(),
                    );
                }
                self.broker
                    .queues
                    .declare(name)
                    .map_err(|error| queue_failure(&error))?;
                Some(name.to_owned())
            }
            Destination::Topic => {
                self.broker
                    .topics()
                    .entry(destination_text.to_owned())
                    .or_default()
                    .push(TopicSubscriber {
                        session: self.id,
                        subscription: id.to_owned(),
                        writer: Arc::clone(&self.writer),
                        binding: None,
                        selector,
                    });
                None
            }
            Destination::Exchange { exchange, key } => {
                let binding = if key.is_empty() { "#" } else { key };
                self.broker
                    .topics()
                    .entry(format!("/exchange/{exchange}"))
                    .or_default()
                    .push(TopicSubscriber {
                        session: self.id,
                        subscription: id.to_owned(),
                        writer: Arc::clone(&self.writer),
                        binding: Some(binding.to_owned()),
                        selector,
                    });
                None
            }
        };
        self.subscriptions.insert(
            id.to_owned(),
            Subscription {
                destination: destination_text.to_owned(),
                queue,
                ack,
                prefetch,
            },
        );
        Ok(())
    }

    fn unsubscribe(&mut self, id: &str) -> Result<(), String> {
        let subscription = self
            .subscriptions
            .remove(id)
            .ok_or_else(|| format!("no subscription has the id {id}"))?;
        if subscription.queue.is_none() {
            let key = match destination(&subscription.destination) {
                Ok(Destination::Exchange { exchange, .. }) => format!("/exchange/{exchange}"),
                _ => subscription.destination.clone(),
            };
            let mut topics = self.broker.topics();
            if let Some(subscribers) = topics.get_mut(&key) {
                subscribers.retain(|subscriber| {
                    subscriber.session != self.id || subscriber.subscription != id
                });
            }
        }
        self.release_where(|pending| pending.subscription == id);
        Ok(())
    }

    fn release_where(&mut self, chosen: impl Fn(&Pending) -> bool) {
        let tokens: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, pending)| chosen(pending))
            .map(|(token, _)| token.clone())
            .collect();
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

    fn staged_bytes(&self) -> usize {
        self.transactions
            .values()
            .flatten()
            .map(Staged::body_bytes)
            .sum()
    }

    fn stage(&mut self, transaction: &str, staged: Staged) -> Result<(), String> {
        let held: usize = self.transactions.values().map(Vec::len).sum();
        let held_bytes = self.staged_bytes();
        if held >= MAX_STAGED || held_bytes >= MOST_STAGED_BYTES {
            return Err("the session's transactions hold too many frames".to_owned());
        }
        if !self
            .staged_held
            .hold_if_large(held_bytes.saturating_add(staged.body_bytes()))
        {
            return Err(OUT_OF_MEMORY_BUDGET.to_owned());
        }
        let steps = self
            .transactions
            .get_mut(transaction)
            .ok_or_else(|| format!("the transaction {transaction} was not begun"))?;
        if steps.len() >= MAX_STAGED {
            return Err("the transaction holds too many frames".to_owned());
        }
        steps.push(staged);
        Ok(())
    }

    fn commit(&mut self, transaction: &str) -> Result<(), String> {
        let steps = self
            .transactions
            .remove(transaction)
            .ok_or_else(|| format!("the transaction {transaction} was not begun"))?;
        let mut queued: Vec<(&str, &[u8])> = Vec::new();
        for step in &steps {
            if let Staged::Send {
                destination: text,
                body,
                ..
            } = step
                && let Destination::Queue(name) = destination(text)?
            {
                self.broker
                    .queues
                    .declare(name)
                    .map_err(|error| queue_failure(&error))?;
                queued.push((name, body));
            }
        }
        self.broker
            .queues
            .send_all(&queued, now_ms())
            .map_err(|error| queue_failure(&error))?;
        for step in steps {
            match step {
                Staged::Send {
                    destination: text,
                    headers,
                    body,
                } => {
                    if matches!(
                        destination(&text)?,
                        Destination::Topic | Destination::Exchange { .. }
                    ) {
                        self.broker.publish_topic(
                            &text,
                            &headers,
                            &body,
                            &self.config.memory_budget,
                        );
                    }
                }
                Staged::Ack(token) => self.settle(&token, Settlement::Accept)?,
                Staged::Nack(token) => self.settle(&token, Settlement::Requeue)?,
                Staged::Reject(token) => self.settle(&token, Settlement::DeadLetter)?,
            }
        }
        Ok(())
    }

    fn apply(&mut self, frame: &Frame) -> Result<Next, String> {
        let transaction = frame.header("transaction").map(str::to_owned);
        match frame.command.as_str() {
            "SEND" => {
                let destination_text = frame
                    .header("destination")
                    .ok_or("SEND needs a destination header")?;
                destination(destination_text)?;
                let headers = forwarded_headers(frame);
                match transaction {
                    Some(transaction) => self.stage(
                        &transaction,
                        Staged::Send {
                            destination: destination_text.to_owned(),
                            headers,
                            body: frame.body.clone(),
                        },
                    )?,
                    None => self.store(destination_text, &headers, &frame.body)?,
                }
            }
            "SUBSCRIBE" => self.subscribe(frame)?,
            "UNSUBSCRIBE" => {
                self.unsubscribe(frame.header("id").ok_or("UNSUBSCRIBE needs an id header")?)?
            }
            "ACK" | "NACK" => {
                let token = Self::ack_token(frame)
                    .ok_or("ACK and NACK need an id header")?
                    .to_owned();
                let settlement = match (frame.command.as_str(), frame.header("requeue")) {
                    ("ACK", _) => Settlement::Accept,
                    (_, Some("false")) => Settlement::DeadLetter,
                    _ => Settlement::Requeue,
                };
                match transaction {
                    Some(transaction) => self.stage(
                        &transaction,
                        match settlement {
                            Settlement::Accept => Staged::Ack(token),
                            Settlement::Requeue => Staged::Nack(token),
                            Settlement::DeadLetter => Staged::Reject(token),
                        },
                    )?,
                    None => self.settle(&token, settlement)?,
                }
            }
            "BEGIN" => {
                let transaction = transaction.ok_or("BEGIN needs a transaction header")?;
                if self.transactions.contains_key(&transaction)
                    || self.transactions.len() >= MAX_TRANSACTIONS
                {
                    return Err(format!(
                        "the transaction {transaction} cannot begin: it is already open or too many are"
                    ));
                }
                self.transactions.insert(transaction, Vec::new());
            }
            "COMMIT" => self.commit(&transaction.ok_or("COMMIT needs a transaction header")?)?,
            "ABORT" => {
                let transaction = transaction.ok_or("ABORT needs a transaction header")?;
                self.transactions
                    .remove(&transaction)
                    .ok_or_else(|| format!("the transaction {transaction} was not begun"))?;
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
        let applied = self.apply(frame);
        let staged_bytes = self.staged_bytes();
        if staged_bytes < self.staged_held.bytes() {
            self.staged_held.hold_if_large(staged_bytes);
        }
        match applied {
            Ok(next) => {
                if let Some(receipt) = frame.header("receipt") {
                    self.send(&Frame::new("RECEIPT").with("receipt-id", receipt))?;
                }
                Ok(next)
            }
            Err(message) => self.refuse(&message, Some(frame)),
        }
    }

    fn pump(&mut self) -> io::Result<Next> {
        let waiting: Vec<(String, String, AckMode, usize)> = self
            .subscriptions
            .iter()
            .filter_map(|(id, subscription)| {
                let queue = subscription.queue.clone()?;
                let outstanding = self
                    .pending
                    .values()
                    .filter(|pending| &pending.subscription == id)
                    .count();
                let room = if subscription.ack == AckMode::Auto {
                    subscription.prefetch
                } else {
                    subscription.prefetch.saturating_sub(outstanding)
                };
                (room > 0).then(|| (id.clone(), queue, subscription.ack, room))
            })
            .collect();
        let config = self.config;
        let mut held = config.memory_budget.empty_reservation();
        for (id, queue, ack, room) in waiting {
            let mut batch_bytes = 0usize;
            let mut refused = false;
            let leased = self.broker.queues.receive_fitting(
                &queue,
                room,
                LEASE_MS,
                now_ms(),
                &mut |body_bytes: usize| {
                    let grown = batch_bytes
                        .saturating_add(body_bytes)
                        .saturating_add(MESSAGE_HEADER_BYTES);
                    if held.hold_if_large(grown) {
                        batch_bytes = grown;
                        true
                    } else {
                        refused = true;
                        false
                    }
                },
            );
            let Ok(messages) = leased else {
                held.resize(0);
                continue;
            };
            if refused && messages.is_empty() {
                return self.refuse(OUT_OF_MEMORY_BUDGET, None);
            }
            let delivered = self.deliver(&id, &queue, ack, messages);
            held.resize(0);
            delivered?;
        }
        Ok(Next::Continue)
    }

    fn deliver(
        &mut self,
        id: &str,
        queue: &str,
        ack: AckMode,
        messages: Vec<eggdb_queues::Message>,
    ) -> io::Result<()> {
        let mut messages = messages.into_iter();
        while let Some(message) = messages.next() {
            let message_id = format!("{queue}-{}", message.id);
            let mut frame = Frame::new("MESSAGE")
                .with("destination", &format!("/queue/{queue}"))
                .with("message-id", &message_id)
                .with("subscription", id);
            if message.receive_count > 1 {
                frame = frame.with("redelivered", "true");
            }
            if ack != AckMode::Auto {
                frame = frame.with("ack", &message_id);
            }
            let sent = self.send(&frame.with_body(&message.body));
            if ack == AckMode::Auto {
                if sent.is_ok() {
                    let _ = self.broker.queues.ack(queue, message.id);
                } else {
                    let _ = self.broker.queues.release(queue, message.id, now_ms());
                }
            } else {
                self.next_sequence += 1;
                self.pending.insert(
                    message_id,
                    Pending {
                        subscription: id.to_owned(),
                        queue: queue.to_owned(),
                        id: message.id,
                        sequence: self.next_sequence,
                    },
                );
            }
            if let Err(error) = sent {
                let now = now_ms();
                for unsent in messages {
                    let _ = self.broker.queues.unlease(queue, unsent.id, now);
                }
                return Err(error);
            }
        }
        Ok(())
    }

    fn run(&mut self, mut reader: impl Read, stop: &(dyn Fn() -> bool + Sync)) -> io::Result<()> {
        let started = Instant::now();
        let mut last_received = started;
        let config = self.config;
        let mut held = config.memory_budget.empty_reservation();
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
            let heart_beats = buffer
                .iter()
                .take_while(|&&byte| byte == b'\n' || byte == b'\r')
                .count();
            buffer.drain(..heart_beats);
            if stop() || (!self.connected && started.elapsed() > CONNECT_WAIT) {
                return Ok(());
            }
            if self.connected {
                let silent_too_long = self
                    .heart_beats
                    .silence_limit
                    .is_some_and(|limit| last_received.elapsed() > limit);
                if silent_too_long || matches!(self.pump()?, Next::Close) {
                    return Ok(());
                }
                self.send_heart_beat_when_due()?;
            }
            if !held.hold_if_large(pending_frame_bytes(&buffer)) {
                self.refuse(OUT_OF_MEMORY_BUDGET, None)?;
                return Ok(());
            }
            match reader.read(&mut chunk) {
                Ok(0) => return Ok(()),
                Ok(read) => {
                    let grown = buffer.len().saturating_add(read);
                    if !held.hold_if_large(grown.max(pending_frame_bytes(&buffer))) {
                        self.refuse(OUT_OF_MEMORY_BUDGET, None)?;
                        return Ok(());
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    last_received = Instant::now();
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

pub(crate) fn serve_stream(
    broker: &Broker<'_>,
    config: &ServerConfig,
    id: u64,
    reader: impl Read,
    writer: SharedWriter,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    let mut session = Session {
        broker,
        config,
        heart_beats: HeartBeats::default(),
        last_sent: Cell::new(Instant::now()),
        staged_held: config.memory_budget.empty_reservation(),
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
pub fn serve(
    keyspace: &(dyn Keyspace + Sync),
    listener: &TcpListener,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    serve_offering(keyspace, listener, config, HeartBeatOffer::STANDARD, stop)
}

pub(crate) fn serve_offering(
    keyspace: &(dyn Keyspace + Sync),
    listener: &TcpListener,
    config: &ServerConfig,
    offer: HeartBeatOffer,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let broker = Broker::new(keyspace, offer);
    let active = AtomicUsize::new(0);
    let next_id = AtomicU64::new(1);
    std::thread::scope(|scope| {
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
            let (broker, active) = (&broker, &active);
            let spawned = std::thread::Builder::new()
                .name("eggdb-stomp".to_owned())
                .spawn_scoped(scope, move || {
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
