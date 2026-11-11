use crate::packet::{
    CONNACK, CONNECT, DISCONNECT, Fields, PINGREQ, PINGRESP, PUBACK, PUBCOMP, PUBLISH, PUBREC,
    PUBREL, Parsed, PublishProperties, SUBACK, SUBSCRIBE, UNSUBACK, UNSUBSCRIBE, encode,
    filter_matches, pending_packet_bytes, put_string, put_variable, take_packet, valid_filter,
};
use eggdb_keyspace::Keyspace;
use eggdb_platform::connection_limits::{MemoryBudget, Reservation};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_POLL: Duration = Duration::from_millis(200);
const CONNECT_WAIT: Duration = Duration::from_secs(10);
const MAX_SUBSCRIPTIONS: usize = 1_000;
const PUBLISH_HEADER_BYTES: usize = 16;
const QUOTA_EXCEEDED: u8 = 0x97;
const MAX_STORED_SESSIONS: usize = 10_000;
const MAX_QUEUED_MESSAGES: usize = 1_000;
const MAX_INFLIGHT: usize = 1_000;
const SESSIONS_DATABASE: u32 = 1;
const SESSION_FORMAT: u8 = 1;
const MAX_STORED_BYTES: usize = 64 << 20;
const LONGEST_SESSION: Duration = Duration::from_secs(7 * 24 * 3600);
const DUP_FLAG: u8 = 0x08;
const IDENTIFIER_REJECTED_V4: u8 = 0x02;
const IDENTIFIER_REJECTED_V5: u8 = 0x85;
pub(crate) const OUT_OF_MEMORY_BUDGET: &str =
    "the server's connections already hold their memory budget";

/// How the MQTT broker runs.
#[derive(Clone)]
pub struct ServerConfig {
    /// The user clients connect as.
    pub user: String,
    /// The SHA-256 of that user's password.
    pub password_sha256: [u8; 32],
    /// The most connections served at once.
    pub max_connections: usize,
    /// The memory every connection's unread packets, retained replays and deliveries
    /// hold together, shared with the server's other listeners (D284, D307).
    pub memory_budget: Arc<MemoryBudget>,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

type SharedWriter = Arc<Mutex<dyn Write + Send>>;
type RetainedMessage = (String, u8, Vec<u8>, Vec<u8>);

const TOPIC_ALIAS_MAXIMUM: u16 = 16;
const PROTOCOL_ERROR: u8 = 0x82;
const TOPIC_ALIAS_INVALID: u8 = 0x94;
const RETAINED_WITH_PROPERTIES: u8 = 0x80;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

fn retained_value(qos: u8, payload: &[u8], properties: &PublishProperties) -> Vec<u8> {
    let expires_at_ms = properties
        .message_expiry
        .map_or(0, |seconds| now_ms().saturating_add(u64::from(seconds) * 1000));
    let encoded = properties.encoded(None);
    let mut value = vec![RETAINED_WITH_PROPERTIES | qos];
    value.extend_from_slice(&expires_at_ms.to_be_bytes());
    value.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
    value.extend_from_slice(&encoded);
    value.extend_from_slice(payload);
    value
}

fn read_retained(value: &[u8], now: u64) -> Option<(u8, Vec<u8>, Vec<u8>)> {
    let (&first, rest) = value.split_first()?;
    if first & RETAINED_WITH_PROPERTIES == 0 {
        return Some((first, Vec::new(), rest.to_vec()));
    }
    let expires_at_ms = u64::from_be_bytes(rest.get(..8)?.try_into().ok()?);
    let length = u32::from_be_bytes(rest.get(8..12)?.try_into().ok()?) as usize;
    let stored = rest.get(12..12 + length)?;
    let payload = rest.get(12 + length..)?;
    let mut properties = Vec::new();
    if expires_at_ms != 0 {
        if expires_at_ms <= now {
            return None;
        }
        let remaining = (expires_at_ms - now).div_ceil(1000);
        properties.push(0x02);
        properties.extend_from_slice(&u32::try_from(remaining).unwrap_or(u32::MAX).to_be_bytes());
    }
    properties.extend_from_slice(stored);
    Some((first & 3, properties, payload.to_vec()))
}

const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, Clone)]
struct Message {
    topic: String,
    payload: Vec<u8>,
    qos: u8,
    properties: Vec<u8>,
}

struct Subscriber {
    filters: Vec<(String, u8)>,
    writer: SharedWriter,
    version: u8,
    next_id: u16,
    client: String,
    keep_for: Option<Duration>,
    inflight: BTreeMap<u16, (Message, bool)>,
}

impl Subscriber {
    fn next_packet_id(&mut self) -> u16 {
        for _ in 0..=u16::MAX {
            self.next_id = self.next_id.wrapping_add(1).max(1);
            if !self.inflight.contains_key(&self.next_id) {
                break;
            }
        }
        self.next_id
    }
}

/// A persistent session kept while its client is away: its subscriptions, the QoS 1 and 2
/// messages sent and not yet acknowledged (and whether a QoS 2 one was released), and the
/// messages that arrived for it since.
struct StoredSession {
    filters: Vec<(String, u8)>,
    inflight: BTreeMap<u16, (Message, bool)>,
    queued: VecDeque<Message>,
    next_id: u16,
    expires_at: Instant,
}

/// What every connection shares: subscriptions, persistent sessions and the retained messages.
pub(crate) struct Broker<'k> {
    keyspace: &'k (dyn Keyspace + Sync),
    subscribers: Mutex<BTreeMap<u64, Subscriber>>,
    sessions: Mutex<BTreeMap<String, StoredSession>>,
    shared_turn: AtomicUsize,
}

fn direct_grant(filters: &[(String, u8)], topic: &str) -> Option<u8> {
    filters
        .iter()
        .filter(|(filter, _)| !filter.starts_with("$share/") && filter_matches(filter, topic))
        .map(|(_, granted)| *granted)
        .max()
}

fn shared_filter(filter: &str) -> Option<(&str, &str)> {
    let rest = filter.strip_prefix("$share/")?;
    let (group, inner) = rest.split_once('/')?;
    (!group.is_empty() && !group.contains(['+', '#']) && !inner.is_empty()).then_some((group, inner))
}

fn valid_subscription(filter: &str) -> bool {
    match filter.strip_prefix("$share/") {
        Some(_) => shared_filter(filter).is_some_and(|(_, inner)| valid_filter(inner)),
        None => valid_filter(filter),
    }
}

fn publish_packet(
    version: u8,
    topic: &str,
    payload: &[u8],
    qos: u8,
    retain: bool,
    id: u16,
    properties: &[u8],
) -> Vec<u8> {
    let mut body = Vec::new();
    put_string(&mut body, topic.as_bytes());
    if qos > 0 {
        body.extend_from_slice(&id.to_be_bytes());
    }
    if version >= 5 {
        put_variable(&mut body, properties.len());
        body.extend_from_slice(properties);
    }
    body.extend_from_slice(payload);
    encode(PUBLISH, (qos << 1) | u8::from(retain), &body)
}

impl<'k> Broker<'k> {
    pub(crate) fn new(keyspace: &'k (dyn Keyspace + Sync)) -> Self {
        Self {
            keyspace,
            subscribers: Mutex::new(BTreeMap::new()),
            sessions: Mutex::new(load_sessions(keyspace)),
            shared_turn: AtomicUsize::new(0),
        }
    }

    fn save_session(&self, client: &str, session: &StoredSession) {
        let bytes = encode_session(session);
        let _ = self.keyspace.write(SESSIONS_DATABASE, &mut |keys| {
            keys.put(client.as_bytes(), &bytes)
        });
    }

    fn drop_saved_session(&self, client: &str) {
        let _ = self.keyspace.write(SESSIONS_DATABASE, &mut |keys| {
            keys.delete(client.as_bytes()).map(drop)
        });
    }

    fn stored(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, StoredSession>> {
        self.sessions.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn live(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Subscriber>> {
        self.subscribers.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn keep(&self, subscriber: Subscriber) {
        let Some(keep_for) = subscriber.keep_for.filter(|_| !subscriber.client.is_empty()) else {
            return;
        };
        let mut sessions = self.stored();
        let now = Instant::now();
        sessions.retain(|_, session| session.expires_at > now);
        if sessions.len() >= MAX_STORED_SESSIONS && !sessions.contains_key(&subscriber.client) {
            return;
        }
        let session = StoredSession {
            filters: subscriber.filters,
            inflight: subscriber.inflight,
            queued: VecDeque::new(),
            next_id: subscriber.next_id,
            expires_at: now + keep_for,
        };
        self.save_session(&subscriber.client, &session);
        sessions.insert(subscriber.client, session);
    }

    fn end_session(&self, id: u64) {
        let ended = self.live().remove(&id);
        if let Some(subscriber) = ended {
            self.keep(subscriber);
        }
    }

    fn forget_session(&self, client: &str) {
        self.stored().remove(client);
        self.drop_saved_session(client);
    }

    fn take_over(&self, client: &str) {
        if client.is_empty() {
            return;
        }
        let taken: Vec<u64> = self
            .live()
            .iter()
            .filter(|(_, subscriber)| subscriber.client == client)
            .map(|(id, _)| *id)
            .collect();
        for id in taken {
            self.end_session(id);
        }
    }

    fn resume(&self, id: u64, client: &str, writer: &SharedWriter, version: u8, keep_for: Duration) -> Option<Vec<Vec<u8>>> {
        let now = Instant::now();
        let stored = self.stored().remove(client);
        if stored.is_some() {
            self.drop_saved_session(client);
        }
        let stored = stored.filter(|session| session.expires_at > now)?;
        let mut subscriber = Subscriber {
            filters: stored.filters,
            writer: Arc::clone(writer),
            version,
            next_id: stored.next_id,
            client: client.to_owned(),
            keep_for: Some(keep_for),
            inflight: stored.inflight,
        };
        let mut packets = Vec::new();
        for (packet_id, (message, released)) in &subscriber.inflight {
            if *released {
                packets.push(encode(PUBREL, 2, &packet_id.to_be_bytes()));
            } else {
                let mut packet = publish_packet(version, &message.topic, &message.payload, message.qos, false, *packet_id, &message.properties);
                packet[0] |= DUP_FLAG;
                packets.push(packet);
            }
        }
        for message in stored.queued {
            if subscriber.inflight.len() >= MAX_INFLIGHT {
                break;
            }
            let packet_id = subscriber.next_packet_id();
            packets.push(publish_packet(version, &message.topic, &message.payload, message.qos, false, packet_id, &message.properties));
            subscriber.inflight.insert(packet_id, (message, false));
        }
        self.live().insert(id, subscriber);
        Some(packets)
    }

    fn settle(&self, id: u64, kind: u8, packet_id: u16) {
        let mut live = self.live();
        let Some(subscriber) = live.get_mut(&id) else {
            return;
        };
        match kind {
            PUBREC => {
                if let Some((_, released)) = subscriber.inflight.get_mut(&packet_id) {
                    *released = true;
                }
            }
            _ => {
                subscriber.inflight.remove(&packet_id);
            }
        }
    }

    fn queue_for_absent(&self, topic: &str, payload: &[u8], qos: u8, properties: &[u8]) {
        if qos == 0 {
            return;
        }
        let mut sessions = self.stored();
        let now = Instant::now();
        sessions.retain(|_, session| session.expires_at > now);
        let mut changed = Vec::new();
        for (client, session) in sessions.iter_mut() {
            let Some(granted) = direct_grant(&session.filters, topic) else {
                continue;
            };
            let qos = qos.min(granted);
            if qos == 0 {
                continue;
            }
            if session.queued.len() >= MAX_QUEUED_MESSAGES {
                session.queued.pop_front();
            }
            session.queued.push_back(Message {
                topic: topic.to_owned(),
                payload: payload.to_vec(),
                qos,
                properties: properties.to_vec(),
            });
            changed.push(client.clone());
        }
        for client in changed {
            if let Some(session) = sessions.get(&client) {
                self.save_session(&client, session);
            }
        }
    }

    fn deliver(
        &self,
        topic: &str,
        payload: &[u8],
        qos: u8,
        properties: &[u8],
        budget: &MemoryBudget,
    ) {
        let mut deliveries: Vec<(SharedWriter, u8, u8, u16)> = Vec::new();
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut groups: BTreeMap<String, Vec<(u64, u8)>> = BTreeMap::new();
        for (&id, subscriber) in subscribers.iter() {
            for (filter, granted) in &subscriber.filters {
                if let Some((group, inner)) = shared_filter(filter)
                    && filter_matches(inner, topic)
                {
                    groups
                        .entry(format!("{group}/{inner}"))
                        .or_default()
                        .push((id, *granted));
                }
            }
        }
        let turn = self.shared_turn.fetch_add(1, Ordering::Relaxed);
        let mut chosen: BTreeMap<u64, u8> = BTreeMap::new();
        for members in groups.values() {
            let (id, granted) = members[turn % members.len()];
            let held = chosen.entry(id).or_insert(granted);
            *held = (*held).max(granted);
        }
        for (id, subscriber) in subscribers.iter_mut() {
            let direct = direct_grant(&subscriber.filters, topic);
            let Some(granted) = direct.max(chosen.get(id).copied()) else {
                continue;
            };
            let qos = qos.min(granted);
            let packet_id = if qos > 0 {
                if subscriber.inflight.len() >= MAX_INFLIGHT {
                    continue;
                }
                let packet_id = subscriber.next_packet_id();
                subscriber.inflight.insert(
                    packet_id,
                    (
                        Message {
                            topic: topic.to_owned(),
                            payload: payload.to_vec(),
                            qos,
                            properties: properties.to_vec(),
                        },
                        false,
                    ),
                );
                packet_id
            } else {
                0
            };
            deliveries.push((Arc::clone(&subscriber.writer), subscriber.version, qos, packet_id));
        }
        drop(subscribers);
        self.queue_for_absent(topic, payload, qos, properties);
        let packet_bytes = payload
            .len()
            .saturating_add(topic.len())
            .saturating_add(properties.len())
            .saturating_add(PUBLISH_HEADER_BYTES);
        let mut held = budget.empty_reservation();
        for (writer, version, qos, id) in deliveries {
            if !held.hold_if_large(packet_bytes) {
                continue;
            }
            let packet = publish_packet(version, topic, payload, qos, false, id, properties);
            let mut writer = writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = writer.write_all(&packet);
        }
    }

    fn retain(
        &self,
        topic: &str,
        payload: &[u8],
        qos: u8,
        properties: &PublishProperties,
    ) -> Result<(), String> {
        self.keyspace.write(0, &mut |keys| {
            if payload.is_empty() || properties.message_expiry == Some(0) {
                keys.delete(topic.as_bytes())?;
            } else {
                keys.put(topic.as_bytes(), &retained_value(qos, payload, properties))?;
            }
            Ok(())
        })
    }

    fn retained<'b>(
        &self,
        filter: &str,
        budget: &'b MemoryBudget,
    ) -> Result<(Vec<RetainedMessage>, Reservation<'b>), String> {
        let mut found = Vec::new();
        let mut held = budget.empty_reservation();
        let mut found_bytes = 0usize;
        let now = now_ms();
        self.keyspace.read(0, &mut |keys| {
            for (key, value) in keys.scan(b"")? {
                let Ok(topic) = String::from_utf8(key) else {
                    continue;
                };
                if filter_matches(filter, &topic)
                    && let Some((qos, properties, payload)) = read_retained(&value, now)
                {
                    found_bytes = found_bytes
                        .saturating_add(payload.len())
                        .saturating_add(properties.len())
                        .saturating_add(topic.len())
                        .saturating_add(PUBLISH_HEADER_BYTES);
                    if !held.hold_if_large(found_bytes) {
                        return Err(OUT_OF_MEMORY_BUDGET.to_owned());
                    }
                    found.push((topic, qos, properties, payload));
                }
            }
            Ok(())
        })?;
        Ok((found, held))
    }

    fn publish(
        &self,
        topic: &str,
        payload: &[u8],
        qos: u8,
        retain: bool,
        properties: &PublishProperties,
        budget: &MemoryBudget,
    ) -> Result<(), String> {
        if retain {
            self.retain(topic, payload, qos, properties)?;
        }
        let forwarded = properties.encoded(properties.message_expiry);
        self.deliver(topic, payload, qos, &forwarded, budget);
        Ok(())
    }
}

struct Will {
    topic: String,
    payload: Vec<u8>,
    qos: u8,
    retain: bool,
}

struct Session<'b, 'k> {
    broker: &'b Broker<'k>,
    budget: &'b MemoryBudget,
    id: u64,
    client: String,
    keep_for: Option<Duration>,
    version: u8,
    writer: SharedWriter,
    will: Option<Will>,
    incoming_qos2: BTreeSet<u16>,
    topic_aliases: BTreeMap<u16, String>,
}

impl Session<'_, '_> {
    fn send(&self, bytes: &[u8]) -> io::Result<()> {
        self.writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .write_all(bytes)
    }

    fn handle(&mut self, kind: u8, flags: u8, body: &[u8]) -> Parsed<bool> {
        let mut fields = Fields::new(body);
        match kind {
            PUBLISH => {
                let qos = (flags >> 1) & 3;
                if qos == 3 {
                    return Err("a PUBLISH has QoS 3".to_owned());
                }
                let mut topic = fields.string()?;
                let packet_id = if qos > 0 { fields.short()? } else { 0 };
                let properties = if self.version >= 5 {
                    fields.publish_properties()?
                } else {
                    PublishProperties::default()
                };
                if let Some(alias) = properties.topic_alias {
                    if alias == 0 || alias > TOPIC_ALIAS_MAXIMUM {
                        return Err(format!("topic alias {alias} is outside 1 to {TOPIC_ALIAS_MAXIMUM}"));
                    }
                    if topic.is_empty() {
                        topic = self
                            .topic_aliases
                            .get(&alias)
                            .cloned()
                            .ok_or_else(|| format!("topic alias {alias} names no topic yet"))?;
                    } else {
                        self.topic_aliases.insert(alias, topic.clone());
                    }
                }
                if topic.is_empty() || topic.contains(['+', '#', '\0']) {
                    return Err("a PUBLISH names an invalid topic".to_owned());
                }
                let payload = fields.rest().to_vec();
                let duplicate = qos == 2 && !self.incoming_qos2.insert(packet_id);
                if !duplicate {
                    self.broker.publish(
                        &topic,
                        &payload,
                        qos,
                        flags & 1 == 1,
                        &properties,
                        self.budget,
                    )?;
                }
                match qos {
                    1 => self
                        .send(&encode(PUBACK, 0, &packet_id.to_be_bytes()))
                        .map_err(|error| error.to_string())?,
                    2 => self
                        .send(&encode(PUBREC, 0, &packet_id.to_be_bytes()))
                        .map_err(|error| error.to_string())?,
                    _ => {}
                }
            }
            PUBREL => {
                let packet_id = fields.short()?;
                self.incoming_qos2.remove(&packet_id);
                self.send(&encode(PUBCOMP, 0, &packet_id.to_be_bytes()))
                    .map_err(|error| error.to_string())?;
            }
            PUBACK | PUBREC | PUBCOMP => {
                let packet_id = fields.short()?;
                self.broker.settle(self.id, kind, packet_id);
                if kind == PUBREC {
                    self.send(&encode(PUBREL, 2, &packet_id.to_be_bytes()))
                        .map_err(|error| error.to_string())?;
                }
            }
            SUBSCRIBE => {
                if flags != 2 {
                    return Err("SUBSCRIBE flags must be 0010".to_owned());
                }
                let packet_id = fields.short()?;
                if self.version >= 5 {
                    fields.skip_properties()?;
                }
                let mut codes = Vec::new();
                let mut accepted = Vec::new();
                while !fields.finished() {
                    let filter = fields.string()?;
                    let options = fields.byte()?;
                    let requested = options & 3;
                    if valid_subscription(&filter) && requested < 3 {
                        let granted = requested;
                        codes.push(granted);
                        accepted.push((filter, granted));
                    } else {
                        codes.push(0x80);
                    }
                }
                if codes.is_empty() {
                    return Err("a SUBSCRIBE has no filters".to_owned());
                }
                {
                    let mut subscribers = self
                        .broker
                        .subscribers
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let subscriber = subscribers.entry(self.id).or_insert_with(|| Subscriber {
                        filters: Vec::new(),
                        writer: Arc::clone(&self.writer),
                        version: self.version,
                        next_id: 0,
                        client: self.client.clone(),
                        keep_for: self.keep_for,
                        inflight: BTreeMap::new(),
                    });
                    for (filter, granted) in &accepted {
                        subscriber.filters.retain(|(known, _)| known != filter);
                        if subscriber.filters.len() < MAX_SUBSCRIPTIONS {
                            subscriber.filters.push((filter.clone(), *granted));
                        }
                    }
                }
                let mut ack = packet_id.to_be_bytes().to_vec();
                if self.version >= 5 {
                    ack.push(0);
                }
                ack.extend_from_slice(&codes);
                self.send(&encode(SUBACK, 0, &ack))
                    .map_err(|error| error.to_string())?;
                for (filter, granted) in accepted {
                    if filter.starts_with("$share/") {
                        continue;
                    }
                    let (retained, _held) = self.broker.retained(&filter, self.budget)?;
                    for (topic, qos, properties, payload) in retained {
                        let qos = qos.min(granted);
                        self.send(&publish_packet(
                            self.version,
                            &topic,
                            &payload,
                            qos,
                            true,
                            1,
                            &properties,
                        ))
                        .map_err(|error| error.to_string())?;
                    }
                }
            }
            UNSUBSCRIBE => {
                if flags != 2 {
                    return Err("UNSUBSCRIBE flags must be 0010".to_owned());
                }
                let packet_id = fields.short()?;
                if self.version >= 5 {
                    fields.skip_properties()?;
                }
                let mut filters = Vec::new();
                while !fields.finished() {
                    filters.push(fields.string()?);
                }
                {
                    let mut subscribers = self
                        .broker
                        .subscribers
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(subscriber) = subscribers.get_mut(&self.id) {
                        subscriber
                            .filters
                            .retain(|(known, _)| !filters.contains(known));
                    }
                }
                let mut ack = packet_id.to_be_bytes().to_vec();
                if self.version >= 5 {
                    ack.push(0);
                    ack.extend(std::iter::repeat_n(0u8, filters.len()));
                }
                self.send(&encode(UNSUBACK, 0, &ack))
                    .map_err(|error| error.to_string())?;
            }
            PINGREQ => self
                .send(&encode(PINGRESP, 0, &[]))
                .map_err(|error| error.to_string())?,
            DISCONNECT => {
                self.will = None;
                return Ok(false);
            }
            _ => return Err(format!("unexpected packet type {kind}")),
        }
        Ok(true)
    }
}

struct Connected {
    version: u8,
    keep_alive: u16,
    will: Option<Will>,
    allowed: bool,
    client: String,
    clean: bool,
    session_expiry: Option<u32>,
}

fn connect(config: &ServerConfig, body: &[u8]) -> Parsed<Connected> {
    let mut fields = Fields::new(body);
    let name = fields.string()?;
    let version = fields.byte()?;
    if !matches!((name.as_str(), version), ("MQTT", 4 | 5) | ("MQIsdp", 3)) {
        return Err("unsupported protocol version".to_owned());
    }
    let flags = fields.byte()?;
    let keep_alive = fields.short()?;
    let session_expiry = if version >= 5 {
        fields.connect_session_expiry()?
    } else {
        None
    };
    let client = fields.string()?;
    let will = if flags & 0x04 != 0 {
        if version >= 5 {
            fields.skip_properties()?;
        }
        let topic = fields.string()?;
        let payload = fields.binary()?.to_vec();
        Some(Will {
            topic,
            payload,
            qos: ((flags >> 3) & 3).min(1),
            retain: flags & 0x20 != 0,
        })
    } else {
        None
    };
    let user = if flags & 0x80 != 0 {
        Some(fields.binary()?.to_vec())
    } else {
        None
    };
    let password = if flags & 0x40 != 0 {
        Some(fields.binary()?.to_vec())
    } else {
        None
    };
    let allowed = user.as_deref() == Some(config.user.as_bytes())
        && password.is_some_and(|password| {
            eggdb_crypto::constant_time_eq(
                &eggdb_crypto::sha256(&password),
                &config.password_sha256,
            )
        });
    Ok(Connected {
        version,
        keep_alive,
        will,
        allowed,
        client,
        clean: flags & 0x02 != 0,
        session_expiry,
    })
}

fn kept_for(connected: &Connected) -> Option<Duration> {
    if connected.version >= 5 {
        connected
            .session_expiry
            .filter(|seconds| *seconds > 0)
            .map(|seconds| Duration::from_secs(u64::from(seconds)).min(LONGEST_SESSION))
    } else {
        (!connected.clean).then_some(LONGEST_SESSION)
    }
}

fn refuse_over_budget(writer: &SharedWriter, session: Option<&Session<'_, '_>>) {
    if session.is_some_and(|active| active.version >= 5) {
        let _ = writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .write_all(&encode(DISCONNECT, 0, &[QUOTA_EXCEEDED]));
    }
}

/// Serves one client connection until it disconnects or breaks the protocol.
pub(crate) fn serve_stream(
    broker: &Broker<'_>,
    config: &ServerConfig,
    id: u64,
    mut reader: impl Read,
    writer: SharedWriter,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    let budget: &MemoryBudget = &config.memory_budget;
    let mut held = budget.empty_reservation();
    let mut buffer = Vec::new();
    let mut chunk = vec![0u8; 16 * 1024];
    let started = Instant::now();
    let mut last_packet = Instant::now();
    let mut session: Option<Session<'_, '_>> = None;
    let mut keep_alive = Duration::ZERO;
    let clean_end = loop {
        if stop() {
            break true;
        }
        match reader.read(&mut chunk) {
            Ok(0) => break false,
            Ok(read) => {
                let grown = buffer.len().saturating_add(read);
                if !held.hold_if_large(grown.max(pending_packet_bytes(&buffer))) {
                    refuse_over_budget(&writer, session.as_ref());
                    break false;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break false,
        }
        let mut finished = None;
        loop {
            match take_packet(&mut buffer) {
                Ok(Some(packet)) => {
                    last_packet = Instant::now();
                    match &mut session {
                        None => {
                            if packet.kind != CONNECT {
                                finished = Some(false);
                                break;
                            }
                            let Ok(connected) = connect(config, &packet.body) else {
                                let _ = writer
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .write_all(&encode(CONNACK, 0, &[0, 1]));
                                finished = Some(true);
                                break;
                            };
                            let version = connected.version;
                            let seconds = connected.keep_alive;
                            let allowed = connected.allowed;
                            let keep_for = kept_for(&connected);
                            let refused = if version >= 5 { 0x87 } else { 5 };
                            let identifier_refused = connected.client.is_empty() && !connected.clean && version < 5;
                            let code = if !allowed {
                                refused
                            } else if identifier_refused {
                                if version >= 5 { IDENTIFIER_REJECTED_V5 } else { IDENTIFIER_REJECTED_V4 }
                            } else {
                                0
                            };
                            let allowed = code == 0;
                            if allowed {
                                broker.take_over(&connected.client);
                                if connected.clean {
                                    broker.forget_session(&connected.client);
                                }
                            }
                            let resumed = if allowed && !connected.clean {
                                keep_for.and_then(|keep| {
                                    broker.resume(id, &connected.client, &writer, version, keep)
                                })
                            } else {
                                None
                            };
                            let mut ack = vec![u8::from(resumed.is_some()), code];
                            if version >= 5 {
                                ack.extend_from_slice(&[3, 0x22]);
                                ack.extend_from_slice(&TOPIC_ALIAS_MAXIMUM.to_be_bytes());
                            }
                            let _ = writer
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .write_all(&encode(CONNACK, 0, &ack));
                            if let Some(packets) = resumed {
                                let mut held = writer
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                for packet in packets {
                                    if held.write_all(&packet).is_err() {
                                        break;
                                    }
                                }
                            }
                            if !allowed {
                                std::thread::sleep(Duration::from_millis(250));
                                finished = Some(true);
                                break;
                            }
                            keep_alive = Duration::from_secs(u64::from(seconds));
                            session = Some(Session {
                                broker,
                                budget,
                                id,
                                client: connected.client.clone(),
                                keep_for,
                                version,
                                writer: Arc::clone(&writer),
                                will: connected.will,
                                incoming_qos2: BTreeSet::new(),
                                topic_aliases: BTreeMap::new(),
                            });
                        }
                        Some(active) => {
                            match active.handle(packet.kind, packet.flags, &packet.body) {
                                Ok(true) => {}
                                Ok(false) => {
                                    finished = Some(true);
                                    break;
                                }
                                Err(problem) => {
                                    if problem == OUT_OF_MEMORY_BUDGET {
                                        refuse_over_budget(&writer, Some(&*active));
                                    } else if active.version >= 5 {
                                        let code = if problem.starts_with("topic alias") {
                                            TOPIC_ALIAS_INVALID
                                        } else {
                                            PROTOCOL_ERROR
                                        };
                                        let mut properties = vec![0x1F];
                                        put_string(&mut properties, problem.as_bytes());
                                        let mut body = vec![code];
                                        put_variable(&mut body, properties.len());
                                        body.extend_from_slice(&properties);
                                        let _ = active.send(&encode(DISCONNECT, 0, &body));
                                    }
                                    finished = Some(false);
                                    break;
                                }
                            }
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    finished = Some(false);
                    break;
                }
            }
        }
        if let Some(clean) = finished {
            break clean;
        }
        if !held.hold_if_large(pending_packet_bytes(&buffer)) {
            refuse_over_budget(&writer, session.as_ref());
            break false;
        }
        if session.is_none() && started.elapsed() > CONNECT_WAIT {
            break false;
        }
        if !keep_alive.is_zero() && last_packet.elapsed() > keep_alive + keep_alive / 2 {
            break false;
        }
    };
    if let Some(active) = session {
        broker.end_session(id);
        if !clean_end && let Some(will) = active.will {
            let _ = broker.publish(
                &will.topic,
                &will.payload,
                will.qos,
                will.retain,
                &PublishProperties::default(),
                budget,
            );
        }
    }
    Ok(())
}

/// Serves MQTT clients on `listener`, keeping retained messages in `keyspace`, until `stop`
/// turns true.
pub fn serve(
    keyspace: &(dyn Keyspace + Sync),
    listener: &TcpListener,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let broker = Broker::new(keyspace);
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
                || stream.set_write_timeout(Some(WRITE_TIMEOUT)).is_err()
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
                .name("eggdb-mqtt".to_owned())
                .spawn_scoped(scope, move || {
                    if crate::websocket::starts_http(&stream) {
                        let mut stream = stream;
                        if crate::websocket::handshake(&mut stream).is_ok()
                            && let Ok(writer) = stream.try_clone()
                        {
                            let raw = Arc::new(Mutex::new(writer));
                            let shared: SharedWriter =
                                Arc::new(Mutex::new(crate::websocket::FrameWriter(Arc::clone(&raw))));
                            let reader = crate::websocket::FrameReader::new(stream, raw);
                            let _ = serve_stream(broker, config, id, reader, shared, stop);
                        }
                    } else if let Ok(writer) = stream.try_clone() {
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

fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn put_message(out: &mut Vec<u8>, message: &Message) {
    put_bytes(out, message.topic.as_bytes());
    put_bytes(out, &message.payload);
    out.push(message.qos);
    put_bytes(out, &message.properties);
}

/// A kept session as the file stores it: a format byte, when it expires (milliseconds since
/// 1970), the next packet id, the filters with their granted QoS, the unacknowledged messages
/// with their packet ids, and the queued messages.
fn encode_session(session: &StoredSession) -> Vec<u8> {
    let remaining = session.expires_at.saturating_duration_since(Instant::now());
    let expires = epoch_millis().saturating_add(u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX));
    let mut out = vec![SESSION_FORMAT];
    out.extend_from_slice(&expires.to_be_bytes());
    out.extend_from_slice(&session.next_id.to_be_bytes());
    out.extend_from_slice(&u32::try_from(session.filters.len()).unwrap_or(0).to_be_bytes());
    for (filter, granted) in &session.filters {
        put_bytes(&mut out, filter.as_bytes());
        out.push(*granted);
    }
    out.extend_from_slice(&u32::try_from(session.inflight.len()).unwrap_or(0).to_be_bytes());
    for (packet_id, (message, released)) in &session.inflight {
        out.extend_from_slice(&packet_id.to_be_bytes());
        out.push(u8::from(*released));
        put_message(&mut out, message);
    }
    out.extend_from_slice(&u32::try_from(session.queued.len()).unwrap_or(0).to_be_bytes());
    for message in &session.queued {
        put_message(&mut out, message);
    }
    out
}

struct SessionReader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> SessionReader<'b> {
    fn take(&mut self, count: usize) -> Option<&'b [u8]> {
        let taken = self.bytes.get(self.at..self.at.checked_add(count)?)?;
        self.at += count;
        Some(taken)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|bytes| bytes[0])
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<usize> {
        usize::try_from(u32::from_be_bytes(self.take(4)?.try_into().ok()?)).ok()
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn bytes(&mut self) -> Option<Vec<u8>> {
        let length = self.u32()?;
        self.take(length).map(<[u8]>::to_vec)
    }

    fn message(&mut self) -> Option<Message> {
        Some(Message {
            topic: String::from_utf8(self.bytes()?).ok()?,
            payload: self.bytes()?,
            qos: self.u8()?,
            properties: self.bytes()?,
        })
    }
}

fn decode_session(bytes: &[u8]) -> Option<StoredSession> {
    if bytes.len() > MAX_STORED_BYTES {
        return None;
    }
    let mut reader = SessionReader { bytes, at: 0 };
    if reader.u8()? != SESSION_FORMAT {
        return None;
    }
    let expires = reader.u64()?;
    let remaining = Duration::from_millis(expires.checked_sub(epoch_millis())?);
    let next_id = reader.u16()?;
    let filter_count = reader.u32()?;
    let mut filters = Vec::with_capacity(filter_count.min(1024));
    for _ in 0..filter_count {
        filters.push((String::from_utf8(reader.bytes()?).ok()?, reader.u8()?));
    }
    let inflight_count = reader.u32()?;
    let mut inflight = BTreeMap::new();
    for _ in 0..inflight_count {
        let packet_id = reader.u16()?;
        let released = reader.u8()? != 0;
        inflight.insert(packet_id, (reader.message()?, released));
    }
    let queued_count = reader.u32()?;
    let mut queued = VecDeque::new();
    for _ in 0..queued_count {
        queued.push_back(reader.message()?);
    }
    (reader.at == bytes.len()).then(|| StoredSession {
        filters,
        inflight,
        queued,
        next_id,
        expires_at: Instant::now() + remaining,
    })
}

/// The sessions kept in the file by an earlier run, those not yet expired, up to the most a
/// broker keeps; a damaged one is left out.
fn load_sessions(keyspace: &dyn Keyspace) -> BTreeMap<String, StoredSession> {
    let mut sessions = BTreeMap::new();
    let _ = keyspace.read(SESSIONS_DATABASE, &mut |keys| {
        for (client, bytes) in keys.scan(b"")? {
            if sessions.len() >= MAX_STORED_SESSIONS {
                break;
            }
            if let (Ok(client), Some(session)) = (String::from_utf8(client), decode_session(&bytes)) {
                sessions.insert(client, session);
            }
        }
        Ok(())
    });
    sessions
}


#[cfg(test)]
mod session_store_tests {
    use super::*;

    #[test]
    fn a_kept_session_round_trips_through_its_stored_form() {
        let message = |topic: &str| Message {
            topic: topic.to_owned(),
            payload: b"payload".to_vec(),
            qos: 1,
            properties: vec![1, 2],
        };
        let mut inflight = BTreeMap::new();
        inflight.insert(7, (message("a/b"), true));
        let session = StoredSession {
            filters: vec![("a/#".to_owned(), 1), ("c".to_owned(), 2)],
            inflight,
            queued: VecDeque::from([message("a/c")]),
            next_id: 8,
            expires_at: Instant::now() + Duration::from_secs(600),
        };
        let decoded = decode_session(&encode_session(&session)).expect("the session decodes");
        assert_eq!(decoded.filters, session.filters);
        assert_eq!(decoded.next_id, 8);
        assert_eq!(decoded.inflight.len(), 1);
        assert!(decoded.inflight[&7].1);
        assert_eq!(decoded.queued[0].topic, "a/c");
        let bytes = encode_session(&session);
        for cut in 0..bytes.len() {
            assert!(decode_session(&bytes[..cut]).is_none());
        }
        let expired = StoredSession {
            expires_at: Instant::now(),
            ..session
        };
        assert!(decode_session(&encode_session(&expired)).is_none_or(|kept| kept.expires_at <= Instant::now() + Duration::from_secs(1)));
    }
}
