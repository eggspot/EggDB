use crate::packet::{
    CONNACK, CONNECT, DISCONNECT, Fields, PINGREQ, PINGRESP, PUBACK, PUBCOMP, PUBLISH, PUBREC,
    PUBREL, Parsed, SUBACK, SUBSCRIBE, UNSUBACK, UNSUBSCRIBE, encode, filter_matches,
    pending_packet_bytes, put_string, take_packet, valid_filter,
};
use eggdb_keyspace::Keyspace;
use eggdb_platform::connection_limits::{MemoryBudget, Reservation};
use std::collections::{BTreeMap, BTreeSet};
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
type RetainedMessage = (String, u8, Vec<u8>);

const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

struct Subscriber {
    filters: Vec<(String, u8)>,
    writer: SharedWriter,
    version: u8,
    next_id: u16,
}

/// What every connection shares: subscriptions and the retained messages.
pub(crate) struct Broker<'k> {
    keyspace: &'k (dyn Keyspace + Sync),
    subscribers: Mutex<BTreeMap<u64, Subscriber>>,
}

fn publish_packet(
    version: u8,
    topic: &str,
    payload: &[u8],
    qos: u8,
    retain: bool,
    id: u16,
) -> Vec<u8> {
    let mut body = Vec::new();
    put_string(&mut body, topic.as_bytes());
    if qos > 0 {
        body.extend_from_slice(&id.to_be_bytes());
    }
    if version >= 5 {
        body.push(0);
    }
    body.extend_from_slice(payload);
    encode(PUBLISH, (qos << 1) | u8::from(retain), &body)
}

impl<'k> Broker<'k> {
    pub(crate) fn new(keyspace: &'k (dyn Keyspace + Sync)) -> Self {
        Self {
            keyspace,
            subscribers: Mutex::new(BTreeMap::new()),
        }
    }

    fn deliver(&self, topic: &str, payload: &[u8], qos: u8, budget: &MemoryBudget) {
        let mut deliveries: Vec<(SharedWriter, u8, u8, u16)> = Vec::new();
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for subscriber in subscribers.values_mut() {
            let Some(granted) = subscriber
                .filters
                .iter()
                .filter(|(filter, _)| filter_matches(filter, topic))
                .map(|(_, granted)| *granted)
                .max()
            else {
                continue;
            };
            let qos = qos.min(granted);
            if qos > 0 {
                subscriber.next_id = subscriber.next_id.wrapping_add(1).max(1);
            }
            deliveries.push((
                Arc::clone(&subscriber.writer),
                subscriber.version,
                qos,
                subscriber.next_id,
            ));
        }
        drop(subscribers);
        let packet_bytes = payload
            .len()
            .saturating_add(topic.len())
            .saturating_add(PUBLISH_HEADER_BYTES);
        let mut held = budget.empty_reservation();
        for (writer, version, qos, id) in deliveries {
            if !held.hold_if_large(packet_bytes) {
                continue;
            }
            let packet = publish_packet(version, topic, payload, qos, false, id);
            let mut writer = writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = writer.write_all(&packet);
        }
    }

    fn retain(&self, topic: &str, payload: &[u8], qos: u8) -> Result<(), String> {
        self.keyspace.write(0, &mut |keys| {
            if payload.is_empty() {
                keys.delete(topic.as_bytes())?;
            } else {
                let mut value = vec![qos];
                value.extend_from_slice(payload);
                keys.put(topic.as_bytes(), &value)?;
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
        self.keyspace.read(0, &mut |keys| {
            for (key, value) in keys.scan(b"")? {
                let Ok(topic) = String::from_utf8(key) else {
                    continue;
                };
                if filter_matches(filter, &topic)
                    && let Some((&qos, payload)) = value.split_first()
                {
                    found_bytes = found_bytes
                        .saturating_add(payload.len())
                        .saturating_add(topic.len())
                        .saturating_add(PUBLISH_HEADER_BYTES);
                    if !held.hold_if_large(found_bytes) {
                        return Err(OUT_OF_MEMORY_BUDGET.to_owned());
                    }
                    found.push((topic, qos, payload.to_vec()));
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
        budget: &MemoryBudget,
    ) -> Result<(), String> {
        if retain {
            self.retain(topic, payload, qos)?;
        }
        self.deliver(topic, payload, qos, budget);
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
    version: u8,
    writer: SharedWriter,
    will: Option<Will>,
    incoming_qos2: BTreeSet<u16>,
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
                let topic = fields.string()?;
                if topic.is_empty() || topic.contains(['+', '#', '\0']) {
                    return Err("a PUBLISH names an invalid topic".to_owned());
                }
                let packet_id = if qos > 0 { fields.short()? } else { 0 };
                if self.version >= 5 {
                    fields.skip_properties()?;
                }
                let payload = fields.rest().to_vec();
                let duplicate = qos == 2 && !self.incoming_qos2.insert(packet_id);
                if !duplicate {
                    self.broker
                        .publish(&topic, &payload, qos, flags & 1 == 1, self.budget)?;
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
                if kind == PUBREC {
                    let packet_id = fields.short()?;
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
                    if valid_filter(&filter) && requested < 3 {
                        let granted = requested.min(1);
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
                    let (retained, _held) = self.broker.retained(&filter, self.budget)?;
                    for (topic, qos, payload) in retained {
                        let qos = qos.min(granted);
                        self.send(&publish_packet(
                            self.version,
                            &topic,
                            &payload,
                            qos,
                            true,
                            1,
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

fn connect(config: &ServerConfig, body: &[u8]) -> Parsed<(u8, u16, Option<Will>, bool)> {
    let mut fields = Fields::new(body);
    let name = fields.string()?;
    let version = fields.byte()?;
    if !matches!((name.as_str(), version), ("MQTT", 4 | 5) | ("MQIsdp", 3)) {
        return Err("unsupported protocol version".to_owned());
    }
    let flags = fields.byte()?;
    let keep_alive = fields.short()?;
    if version >= 5 {
        fields.skip_properties()?;
    }
    let _client = fields.string()?;
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
    Ok((version, keep_alive, will, allowed))
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
                            let Ok((version, seconds, will, allowed)) =
                                connect(config, &packet.body)
                            else {
                                let _ = writer
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .write_all(&encode(CONNACK, 0, &[0, 1]));
                                finished = Some(true);
                                break;
                            };
                            let refused = if version >= 5 { 0x87 } else { 5 };
                            let mut ack = vec![0, if allowed { 0 } else { refused }];
                            if version >= 5 {
                                ack.push(0);
                            }
                            let _ = writer
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .write_all(&encode(CONNACK, 0, &ack));
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
                                version,
                                writer: Arc::clone(&writer),
                                will,
                                incoming_qos2: BTreeSet::new(),
                            });
                        }
                        Some(active) => {
                            match active.handle(packet.kind, packet.flags, &packet.body) {
                                Ok(true) => {}
                                Ok(false) => {
                                    finished = Some(true);
                                    break;
                                }
                                Err(_) => {
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
        broker
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        if !clean_end && let Some(will) = active.will {
            let _ = broker.publish(&will.topic, &will.payload, will.qos, will.retain, budget);
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
            let stream = match listener.accept() {
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
