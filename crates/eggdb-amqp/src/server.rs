use crate::codec::{Args, FRAME_BODY, FRAME_HEADER, FRAME_HEARTBEAT, FRAME_MAX, FRAME_METHOD, Frame, Out, Parsed, frame, take_frame};
use eggdb_keyspace::Keyspace;
use eggdb_queues::Queues;
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PROTOCOL_HEADER: &[u8; 8] = b"AMQP\x00\x00\x09\x01";
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_POLL: Duration = Duration::from_millis(50);
const LEASE_MS: u64 = 30 * 60 * 1000;
const MAX_CHANNELS: u16 = 2047;
const MAX_BODY_BYTES: u64 = 16 << 20;

const NOT_FOUND: u16 = 404;
const ACCESS_REFUSED: u16 = 403;
const PRECONDITION_FAILED: u16 = 406;
const FRAME_ERROR: u16 = 501;
const COMMAND_INVALID: u16 = 503;
const CHANNEL_ERROR: u16 = 504;
const NOT_IMPLEMENTED: u16 = 540;

/// How the AMQP server runs.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// The one user clients log in as with PLAIN.
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Direct,
    Fanout,
    Topic,
}

#[derive(Debug, Clone)]
struct Exchange {
    kind: Kind,
    bindings: Vec<(String, String)>,
}

/// What every connection shares: the queues and the exchanges with their bindings.
pub(crate) struct Broker<'s> {
    pub(crate) queues: Queues<'s>,
    exchanges: Mutex<BTreeMap<String, Exchange>>,
    next_name: AtomicU64,
}

impl<'s> Broker<'s> {
    pub(crate) fn new(keyspace: &'s (dyn Keyspace + Sync)) -> Self {
        let mut exchanges = BTreeMap::new();
        for (name, kind) in [("amq.direct", Kind::Direct), ("amq.fanout", Kind::Fanout), ("amq.topic", Kind::Topic)] {
            exchanges.insert(name.to_owned(), Exchange { kind, bindings: Vec::new() });
        }
        Self { queues: Queues::new(keyspace), exchanges: Mutex::new(exchanges), next_name: AtomicU64::new(1) }
    }

    fn route(&self, exchange: &str, routing_key: &str) -> Option<Vec<String>> {
        if exchange.is_empty() {
            return Some(vec![routing_key.to_owned()]);
        }
        let exchanges = self.exchanges.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let found = exchanges.get(exchange)?;
        let mut queues: Vec<String> = found
            .bindings
            .iter()
            .filter(|(key, _)| match found.kind {
                Kind::Direct => key == routing_key,
                Kind::Fanout => true,
                Kind::Topic => topic_matches(key, routing_key),
            })
            .map(|(_, queue)| queue.clone())
            .collect();
        queues.sort();
        queues.dedup();
        Some(queues)
    }
}

/// Whether an AMQP topic pattern (`*` one word, `#` zero or more) matches a routing key.
pub(crate) fn topic_matches(pattern: &str, key: &str) -> bool {
    fn matches(pattern: &[&str], key: &[&str]) -> bool {
        match pattern.split_first() {
            None => key.is_empty(),
            Some((&"#", rest)) => (0..=key.len()).any(|skip| matches(rest, &key[skip..])),
            Some((&"*", rest)) => !key.is_empty() && matches(rest, &key[1..]),
            Some((word, rest)) => key.first() == Some(word) && matches(rest, &key[1..]),
        }
    }
    let pattern: Vec<&str> = pattern.split('.').collect();
    let key: Vec<&str> = if key.is_empty() { Vec::new() } else { key.split('.').collect() };
    pattern.len() <= 64 && matches(&pattern, &key)
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

fn encode_message(exchange: &str, routing_key: &str, properties: &[u8], body: &[u8]) -> Vec<u8> {
    let mut out = Out::default();
    out.shortstr(exchange).shortstr(routing_key).longstr(properties);
    out.bytes.extend_from_slice(body);
    out.bytes
}

fn decode_message(stored: &[u8]) -> Parsed<(String, String, Vec<u8>, Vec<u8>)> {
    let mut args = Args::new(stored);
    let exchange = args.shortstr()?;
    let routing_key = args.shortstr()?;
    let properties = args.longstr()?.to_vec();
    Ok((exchange, routing_key, properties, args.rest().to_vec()))
}

struct Consumer {
    tag: String,
    queue: String,
    no_ack: bool,
}

#[derive(Default)]
struct Channel {
    consumers: Vec<Consumer>,
    prefetch: u16,
    unacked: BTreeMap<u64, (String, u64)>,
    next_tag: u64,
    confirms: Option<u64>,
    publishing: Option<(String, String)>,
    header: Option<(u64, Vec<u8>)>,
    body: Vec<u8>,
}

enum Stage {
    StartSent,
    TuneSent,
    Open,
}

struct Connection<'s, 'b> {
    broker: &'b Broker<'s>,
    config: &'b ServerConfig,
    stage: Stage,
    channels: BTreeMap<u16, Channel>,
    outgoing: VecDeque<Vec<u8>>,
}

fn method(out: &Out, channel: u16) -> Vec<u8> {
    frame(FRAME_METHOD, channel, &out.bytes)
}

impl Connection<'_, '_> {
    fn send(&mut self, channel: u16, out: Out) {
        self.outgoing.push_back(method(&out, channel));
    }

    fn close_connection(&mut self, code: u16, text: &str, class: u16, method_id: u16) {
        let mut out = Out::method(10, 50);
        out.short(code).shortstr(text).short(class).short(method_id);
        self.send(0, out);
    }

    fn close_channel(&mut self, channel: u16, code: u16, text: &str, class: u16, method_id: u16) {
        self.release_channel(channel);
        let mut out = Out::method(20, 40);
        out.short(code).shortstr(text).short(class).short(method_id);
        self.send(channel, out);
    }

    fn release_channel(&mut self, channel: u16) {
        if let Some(state) = self.channels.remove(&channel) {
            let now = now_ms();
            for (_, (queue, id)) in state.unacked {
                let _ = self.broker.queues.release(&queue, id, now);
            }
        }
    }

    fn deliver_message(&mut self, channel: u16, out: Out, stored: &[u8]) -> Parsed<()> {
        let (_, _, properties, body) = decode_message(stored)?;
        self.send(channel, out);
        let mut header = Out::default();
        header.short(60).short(0).longlong(body.len() as u64);
        header.bytes.extend_from_slice(if properties.is_empty() { &[0, 0] } else { &properties });
        self.outgoing.push_back(frame(FRAME_HEADER, channel, &header.bytes));
        for chunk in body.chunks((FRAME_MAX - 8) as usize) {
            self.outgoing.push_back(frame(FRAME_BODY, channel, chunk));
        }
        Ok(())
    }

    fn pump(&mut self) -> Parsed<()> {
        let channels: Vec<u16> = self.channels.keys().copied().collect();
        for channel in channels {
            let consumers: Vec<(String, String, bool)> = match self.channels.get(&channel) {
                Some(state) => state.consumers.iter().map(|consumer| (consumer.tag.clone(), consumer.queue.clone(), consumer.no_ack)).collect(),
                None => continue,
            };
            for (tag, queue, no_ack) in consumers {
                let room = match self.channels.get(&channel) {
                    Some(state) if state.prefetch > 0 => usize::from(state.prefetch).saturating_sub(state.unacked.len()),
                    Some(_) => 64,
                    None => 0,
                };
                if room == 0 {
                    continue;
                }
                let messages = self.broker.queues.receive(&queue, room, LEASE_MS, now_ms()).map_err(|error| error.to_string())?;
                for message in messages {
                    let Some(state) = self.channels.get_mut(&channel) else {
                        break;
                    };
                    state.next_tag += 1;
                    let delivery_tag = state.next_tag;
                    if no_ack {
                        let _ = self.broker.queues.ack(&queue, message.id);
                    } else {
                        state.unacked.insert(delivery_tag, (queue.clone(), message.id));
                    }
                    let (exchange, routing_key, _, _) = decode_message(&message.body)?;
                    let mut out = Out::method(60, 60);
                    out.shortstr(&tag).longlong(delivery_tag).octet(u8::from(message.receive_count > 1)).shortstr(&exchange).shortstr(&routing_key);
                    self.deliver_message(channel, out, &message.body)?;
                }
            }
        }
        Ok(())
    }

    fn acknowledge(&mut self, channel: u16, tag: u64, multiple: bool, outcome: Option<bool>) {
        let Some(state) = self.channels.get_mut(&channel) else {
            return;
        };
        let tags: Vec<u64> = if multiple {
            state.unacked.range(..=if tag == 0 { u64::MAX } else { tag }).map(|(known, _)| *known).collect()
        } else {
            vec![tag]
        };
        let now = now_ms();
        for known in tags {
            if let Some((queue, id)) = state.unacked.remove(&known) {
                match outcome {
                    Some(true) => {
                        let _ = self.broker.queues.release(&queue, id, now);
                    }
                    _ => {
                        let _ = self.broker.queues.ack(&queue, id);
                    }
                }
            }
        }
    }

    fn publish(&mut self, channel: u16) -> Parsed<()> {
        let Some(state) = self.channels.get_mut(&channel) else {
            return Ok(());
        };
        let Some((exchange, routing_key)) = state.publishing.take() else {
            return Ok(());
        };
        let properties = state.header.take().map(|(_, properties)| properties).unwrap_or_default();
        let body = std::mem::take(&mut state.body);
        let confirm = state.confirms.as_mut().map(|sequence| {
            *sequence += 1;
            *sequence
        });
        let Some(queues) = self.broker.route(&exchange, &routing_key) else {
            self.close_channel(channel, NOT_FOUND, &format!("NOT_FOUND - no exchange '{exchange}'"), 60, 40);
            return Ok(());
        };
        let stored = encode_message(&exchange, &routing_key, &properties, &body);
        let now = now_ms();
        for queue in queues {
            if self.broker.queues.exists(&queue).unwrap_or(false) {
                self.broker.queues.send(&queue, &stored, 0, now).map_err(|error| error.to_string())?;
            }
        }
        if let Some(sequence) = confirm {
            let mut out = Out::method(60, 80);
            out.longlong(sequence).octet(0);
            self.send(channel, out);
        }
        Ok(())
    }

    fn handle(&mut self, incoming: Frame) -> Parsed<bool> {
        match incoming.kind {
            FRAME_HEARTBEAT => return Ok(true),
            FRAME_HEADER => {
                let mut args = Args::new(&incoming.payload);
                let _class = args.short()?;
                let _weight = args.short()?;
                let size = args.longlong()?;
                if size > MAX_BODY_BYTES {
                    self.close_connection(FRAME_ERROR, "a message body is larger than 16 MiB", 60, 40);
                    return Ok(false);
                }
                let properties = args.rest().to_vec();
                if let Some(state) = self.channels.get_mut(&incoming.channel) {
                    state.header = Some((size, properties));
                    if size == 0 {
                        return self.publish(incoming.channel).map(|()| true);
                    }
                }
                return Ok(true);
            }
            FRAME_BODY => {
                let complete = match self.channels.get_mut(&incoming.channel) {
                    Some(state) => {
                        state.body.extend_from_slice(&incoming.payload);
                        let expected = state.header.as_ref().map_or(0, |(size, _)| *size);
                        if state.body.len() as u64 > expected {
                            self.close_connection(FRAME_ERROR, "a message body is longer than its header says", 60, 40);
                            return Ok(false);
                        }
                        state.body.len() as u64 == expected
                    }
                    None => false,
                };
                if complete {
                    self.publish(incoming.channel)?;
                }
                return Ok(true);
            }
            FRAME_METHOD => {}
            _ => {
                self.close_connection(FRAME_ERROR, "an unknown frame type", 0, 0);
                return Ok(false);
            }
        }
        let mut args = Args::new(&incoming.payload);
        let class = args.short()?;
        let method_id = args.short()?;
        let channel = incoming.channel;
        match (&self.stage, class, method_id) {
            (Stage::StartSent, 10, 11) => {
                let _properties = args.table()?;
                let mechanism = args.shortstr()?;
                let response = args.longstr()?;
                let mut parts = response.split(|byte| *byte == 0);
                let (_, user, password) = (parts.next(), parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
                let valid = mechanism == "PLAIN"
                    && user == self.config.user.as_bytes()
                    && eggdb_crypto::constant_time_eq(&eggdb_crypto::sha256(password), &self.config.password_sha256);
                if !valid {
                    std::thread::sleep(Duration::from_millis(250));
                    self.close_connection(ACCESS_REFUSED, "ACCESS_REFUSED - Login was refused using authentication mechanism PLAIN", 10, 11);
                    return Ok(false);
                }
                let mut out = Out::method(10, 30);
                out.short(MAX_CHANNELS).long(FRAME_MAX).short(0);
                self.send(0, out);
                self.stage = Stage::TuneSent;
            }
            (Stage::TuneSent, 10, 31) => {}
            (Stage::TuneSent, 10, 40) => {
                let mut out = Out::method(10, 41);
                out.shortstr("");
                self.send(0, out);
                self.stage = Stage::Open;
            }
            (Stage::Open, 10, 50) => {
                self.send(0, Out::method(10, 51));
                return Ok(false);
            }
            (Stage::Open, 10, 51) => return Ok(false),
            (Stage::Open, 20, 10) => {
                if channel == 0 || channel > MAX_CHANNELS || self.channels.contains_key(&channel) {
                    self.close_connection(CHANNEL_ERROR, "CHANNEL_ERROR - channel already open or invalid", 20, 10);
                    return Ok(false);
                }
                self.channels.insert(channel, Channel::default());
                let mut out = Out::method(20, 11);
                out.longstr(&[]);
                self.send(channel, out);
            }
            (Stage::Open, 20, 40) => {
                self.release_channel(channel);
                self.send(channel, Out::method(20, 41));
            }
            (Stage::Open, 20, 41) => {}
            (Stage::Open, ..) if !self.channels.contains_key(&channel) => {
                self.close_connection(CHANNEL_ERROR, "CHANNEL_ERROR - expected 'channel.open'", class, method_id);
                return Ok(false);
            }
            (Stage::Open, 40, 10) => {
                let _reserved = args.short()?;
                let name = args.shortstr()?;
                let kind = args.shortstr()?;
                let passive = args.bit()?;
                let _durable = args.bit()?;
                let _auto_delete = args.bit()?;
                let _internal = args.bit()?;
                let no_wait = args.bit()?;
                let kind = match kind.as_str() {
                    "direct" => Kind::Direct,
                    "fanout" => Kind::Fanout,
                    "topic" => Kind::Topic,
                    other => {
                        self.close_connection(NOT_IMPLEMENTED, &format!("NOT_IMPLEMENTED - exchange type '{other}'"), 40, 10);
                        return Ok(false);
                    }
                };
                let outcome = {
                    let mut exchanges = self.broker.exchanges.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    match exchanges.get(&name) {
                        Some(existing) if existing.kind != kind => Err(PRECONDITION_FAILED),
                        Some(_) => Ok(()),
                        None if passive => Err(NOT_FOUND),
                        None => {
                            exchanges.insert(name.clone(), Exchange { kind, bindings: Vec::new() });
                            Ok(())
                        }
                    }
                };
                match outcome {
                    Ok(()) if !no_wait => self.send(channel, Out::method(40, 11)),
                    Ok(()) => {}
                    Err(code) => self.close_channel(channel, code, &format!("cannot declare exchange '{name}'"), 40, 10),
                }
            }
            (Stage::Open, 50, 10) => {
                let _reserved = args.short()?;
                let mut name = args.shortstr()?;
                let passive = args.bit()?;
                let _durable = args.bit()?;
                let _exclusive = args.bit()?;
                let _auto_delete = args.bit()?;
                let no_wait = args.bit()?;
                if name.is_empty() {
                    name = format!("amq.gen-{}", self.broker.next_name.fetch_add(1, Ordering::Relaxed));
                }
                let exists = self.broker.queues.exists(&name).map_err(|error| error.to_string())?;
                if passive && !exists {
                    self.close_channel(channel, NOT_FOUND, &format!("NOT_FOUND - no queue '{name}'"), 50, 10);
                    return Ok(true);
                }
                if !exists && let Err(error) = self.broker.queues.declare(&name) {
                    self.close_channel(channel, PRECONDITION_FAILED, &error.to_string(), 50, 10);
                    return Ok(true);
                }
                if !no_wait {
                    let depth = self.broker.queues.depth(&name, now_ms()).map_err(|error| error.to_string())?;
                    let mut out = Out::method(50, 11);
                    out.shortstr(&name).long(depth.ready as u32).long(0);
                    self.send(channel, out);
                }
            }
            (Stage::Open, 50, 20) | (Stage::Open, 50, 50) => {
                let _reserved = args.short()?;
                let queue = args.shortstr()?;
                let exchange = args.shortstr()?;
                let routing_key = args.shortstr()?;
                let no_wait = method_id == 20 && args.bit()?;
                let found = {
                    let mut exchanges = self.broker.exchanges.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    match exchanges.get_mut(&exchange) {
                        Some(found) => {
                            found.bindings.retain(|(key, bound)| !(key == &routing_key && bound == &queue));
                            if method_id == 20 {
                                found.bindings.push((routing_key, queue));
                            }
                            true
                        }
                        None => false,
                    }
                };
                if !found {
                    self.close_channel(channel, NOT_FOUND, &format!("NOT_FOUND - no exchange '{exchange}'"), 50, method_id);
                } else if !no_wait {
                    self.send(channel, Out::method(50, method_id + 1));
                }
            }
            (Stage::Open, 50, 30) | (Stage::Open, 50, 40) => {
                let _reserved = args.short()?;
                let queue = args.shortstr()?;
                let outcome = if method_id == 30 { self.broker.queues.purge(&queue) } else { self.broker.queues.delete(&queue) };
                match outcome {
                    Ok(count) => {
                        let mut out = Out::method(50, method_id + 1);
                        out.long(count as u32);
                        self.send(channel, out);
                    }
                    Err(_) => self.close_channel(channel, NOT_FOUND, &format!("NOT_FOUND - no queue '{queue}'"), 50, method_id),
                }
            }
            (Stage::Open, 60, 10) => {
                let _size = args.long()?;
                let count = args.short()?;
                if let Some(state) = self.channels.get_mut(&channel) {
                    state.prefetch = count;
                }
                self.send(channel, Out::method(60, 11));
            }
            (Stage::Open, 60, 20) => {
                let _reserved = args.short()?;
                let queue = args.shortstr()?;
                let mut tag = args.shortstr()?;
                let _no_local = args.bit()?;
                let no_ack = args.bit()?;
                let _exclusive = args.bit()?;
                let no_wait = args.bit()?;
                if !self.broker.queues.exists(&queue).map_err(|error| error.to_string())? {
                    self.close_channel(channel, NOT_FOUND, &format!("NOT_FOUND - no queue '{queue}'"), 60, 20);
                    return Ok(true);
                }
                if tag.is_empty() {
                    tag = format!("amq.ctag-{}", self.broker.next_name.fetch_add(1, Ordering::Relaxed));
                }
                if let Some(state) = self.channels.get_mut(&channel) {
                    state.consumers.push(Consumer { tag: tag.clone(), queue, no_ack });
                }
                if !no_wait {
                    let mut out = Out::method(60, 21);
                    out.shortstr(&tag);
                    self.send(channel, out);
                }
            }
            (Stage::Open, 60, 30) => {
                let tag = args.shortstr()?;
                let no_wait = args.bit()?;
                if let Some(state) = self.channels.get_mut(&channel) {
                    state.consumers.retain(|consumer| consumer.tag != tag);
                }
                if !no_wait {
                    let mut out = Out::method(60, 31);
                    out.shortstr(&tag);
                    self.send(channel, out);
                }
            }
            (Stage::Open, 60, 40) => {
                let _reserved = args.short()?;
                let exchange = args.shortstr()?;
                let routing_key = args.shortstr()?;
                if let Some(state) = self.channels.get_mut(&channel) {
                    state.publishing = Some((exchange, routing_key));
                    state.header = None;
                    state.body.clear();
                }
            }
            (Stage::Open, 60, 70) => {
                let _reserved = args.short()?;
                let queue = args.shortstr()?;
                let no_ack = args.bit()?;
                let messages = match self.broker.queues.receive(&queue, 1, LEASE_MS, now_ms()) {
                    Ok(messages) => messages,
                    Err(_) => {
                        self.close_channel(channel, NOT_FOUND, &format!("NOT_FOUND - no queue '{queue}'"), 60, 70);
                        return Ok(true);
                    }
                };
                match messages.into_iter().next() {
                    None => {
                        let mut out = Out::method(60, 72);
                        out.shortstr("");
                        self.send(channel, out);
                    }
                    Some(message) => {
                        let remaining = self.broker.queues.depth(&queue, now_ms()).map_or(0, |depth| depth.ready);
                        let Some(state) = self.channels.get_mut(&channel) else {
                            return Ok(true);
                        };
                        state.next_tag += 1;
                        let delivery_tag = state.next_tag;
                        if no_ack {
                            let _ = self.broker.queues.ack(&queue, message.id);
                        } else {
                            state.unacked.insert(delivery_tag, (queue.clone(), message.id));
                        }
                        let (exchange, routing_key, _, _) = decode_message(&message.body)?;
                        let mut out = Out::method(60, 71);
                        out.longlong(delivery_tag).octet(u8::from(message.receive_count > 1)).shortstr(&exchange).shortstr(&routing_key).long(remaining as u32);
                        self.deliver_message(channel, out, &message.body)?;
                    }
                }
            }
            (Stage::Open, 60, 80) => {
                let tag = args.longlong()?;
                let multiple = args.bit()?;
                self.acknowledge(channel, tag, multiple, None);
            }
            (Stage::Open, 60, 90) => {
                let tag = args.longlong()?;
                let requeue = args.bit()?;
                self.acknowledge(channel, tag, false, Some(requeue));
            }
            (Stage::Open, 60, 120) => {
                let tag = args.longlong()?;
                let multiple = args.bit()?;
                let requeue = args.bit()?;
                self.acknowledge(channel, tag, multiple, Some(requeue));
            }
            (Stage::Open, 60, 110) => {
                self.acknowledge(channel, 0, true, Some(true));
                self.send(channel, Out::method(60, 111));
            }
            (Stage::Open, 85, 10) => {
                let no_wait = args.bit()?;
                if let Some(state) = self.channels.get_mut(&channel) {
                    state.confirms.get_or_insert(0);
                }
                if !no_wait {
                    self.send(channel, Out::method(85, 11));
                }
            }
            (Stage::Open, 90, _) => {
                self.close_channel(channel, NOT_IMPLEMENTED, "NOT_IMPLEMENTED - transactions are not supported yet", class, method_id);
            }
            _ => {
                self.close_connection(COMMAND_INVALID, &format!("COMMAND_INVALID - unexpected method {class}.{method_id}"), class, method_id);
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn start_frame() -> Vec<u8> {
    let mut out = Out::method(10, 10);
    out.octet(0).octet(9).string_table(&[("product", "EggDB"), ("platform", "Rust"), ("information", "compatible with AMQP 0-9-1")]).longstr(b"PLAIN").longstr(b"en_US");
    method(&out, 0)
}

pub(crate) fn serve_stream(broker: &Broker<'_>, config: &ServerConfig, mut reader: impl Read, writer: &mut impl Write, stop: &(dyn Fn() -> bool + Sync)) -> io::Result<()> {
    let mut header = [0u8; 8];
    let mut filled = 0;
    while filled < 8 {
        match reader.read(&mut header[filled..]) {
            Ok(0) => return Ok(()),
            Ok(read) => filled += read,
            Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                if stop() {
                    return Ok(());
                }
            }
            Err(error) => return Err(error),
        }
    }
    if &header != PROTOCOL_HEADER {
        writer.write_all(PROTOCOL_HEADER)?;
        return Ok(());
    }
    writer.write_all(&start_frame())?;
    let mut connection = Connection { broker, config, stage: Stage::StartSent, channels: BTreeMap::new(), outgoing: VecDeque::new() };
    let mut buffer = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let outcome = loop {
        if stop() {
            break Ok(());
        }
        match reader.read(&mut chunk) {
            Ok(0) => break Ok(()),
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(error) => break Err(error),
        }
        let mut keep_going = true;
        loop {
            match take_frame(&mut buffer) {
                Ok(Some(incoming)) => match connection.handle(incoming) {
                    Ok(true) => {}
                    Ok(false) => {
                        keep_going = false;
                        break;
                    }
                    Err(message) => {
                        connection.close_connection(FRAME_ERROR, &format!("FRAME_ERROR - {message}"), 0, 0);
                        keep_going = false;
                        break;
                    }
                },
                Ok(None) => break,
                Err(message) => {
                    connection.close_connection(FRAME_ERROR, &format!("FRAME_ERROR - {message}"), 0, 0);
                    keep_going = false;
                    break;
                }
            }
        }
        if keep_going && matches!(connection.stage, Stage::Open) {
            let _ = connection.pump();
        }
        while let Some(bytes) = connection.outgoing.pop_front() {
            writer.write_all(&bytes)?;
        }
        if !keep_going {
            break Ok(());
        }
    };
    let channels: Vec<u16> = connection.channels.keys().copied().collect();
    for channel in channels {
        connection.release_channel(channel);
    }
    outcome
}

struct Polled {
    stream: TcpStream,
}

impl Read for Polled {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buffer)
    }
}

/// Serves AMQP 0-9-1 clients on `listener` from the queues in `keyspace` until `stop` turns
/// true.
pub fn serve(keyspace: &(dyn Keyspace + Sync), listener: &TcpListener, config: &ServerConfig, stop: &(dyn Fn() -> bool + Sync)) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let broker = Broker::new(keyspace);
    let active = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        while !stop() {
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(_) => {
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
            };
            if stream.set_nonblocking(false).is_err() || stream.set_read_timeout(Some(READ_POLL)).is_err() || stream.set_nodelay(true).is_err() {
                continue;
            }
            if active.fetch_add(1, Ordering::AcqRel) >= config.max_connections {
                active.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let (broker, active) = (&broker, &active);
            let spawned = std::thread::Builder::new().name("eggdb-amqp".to_owned()).spawn_scoped(scope, move || {
                if let Ok(mut writer) = stream.try_clone() {
                    let _ = serve_stream(broker, config, Polled { stream }, &mut writer, stop);
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
