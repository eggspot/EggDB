use crate::codec::{
    Args, FRAME_BODY, FRAME_HEADER, FRAME_HEARTBEAT, FRAME_MAX, FRAME_METHOD, Frame, Out, Parsed,
    frame, take_frame,
};
use crate::field_table::{FieldValue, headers_match, integer_named, message_expiration, text_named};
use eggdb_keyspace::Keyspace;
use eggdb_platform::connection_limits::{MemoryBudget, Reservation};
use eggdb_queues::Queues;
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROTOCOL_HEADER: &[u8; 8] = b"AMQP\x00\x00\x09\x01";
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_POLL: Duration = Duration::from_millis(50);
const LEASE_MS: u64 = 30 * 60 * 1000;
const MAX_CHANNELS: u16 = 2047;
const MAX_BODY_BYTES: u64 = 16 << 20;
const MOST_BUFFERED_BYTES: usize = 64 << 20;
const PROPOSED_HEARTBEAT_SECONDS: u16 = 60;

const NOT_FOUND: u16 = 404;
const ACCESS_REFUSED: u16 = 403;
const PRECONDITION_FAILED: u16 = 406;
const RESOURCE_LOCKED: u16 = 405;
const RESOURCE_ERROR: u16 = 506;
const OUT_OF_MEMORY_BUDGET: &str =
    "RESOURCE_ERROR - the server's connections already hold their memory budget";
const DELIVERY_FRAME_BYTES: usize = 64;
const FRAME_ERROR: u16 = 501;
const COMMAND_INVALID: u16 = 503;
const CHANNEL_ERROR: u16 = 504;
const NOT_IMPLEMENTED: u16 = 540;

/// How the AMQP server runs.
#[derive(Clone)]
pub struct ServerConfig {
    /// The one user clients log in as with PLAIN.
    pub user: String,
    /// The SHA-256 of that user's password.
    pub password_sha256: [u8; 32],
    /// The most connections served at once.
    pub max_connections: usize,
    /// The memory every connection's unread frames, unfinished message bodies and deliveries
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Direct,
    Fanout,
    Topic,
    Headers,
}

#[derive(Debug, Clone)]
struct Binding {
    key: String,
    queue: String,
    arguments: Vec<(String, FieldValue)>,
}

#[derive(Debug, Clone)]
struct Exchange {
    kind: Kind,
    bindings: Vec<Binding>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct QueuePolicy {
    message_ttl_ms: Option<u64>,
    dead_letter_exchange: Option<String>,
    dead_letter_routing_key: Option<String>,
    expires_ms: Option<u64>,
}

impl QueuePolicy {
    fn of(arguments: &[(String, FieldValue)]) -> Self {
        Self {
            message_ttl_ms: integer_named(arguments, "x-message-ttl"),
            dead_letter_exchange: text_named(arguments, "x-dead-letter-exchange"),
            dead_letter_routing_key: text_named(arguments, "x-dead-letter-routing-key"),
            expires_ms: integer_named(arguments, "x-expires").filter(|expires| *expires > 0),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Usage {
    consumers: usize,
    last_used_ms: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct AutoDelete {
    consumers: usize,
    seen: bool,
}

/// What every connection shares: the queues and the exchanges with their bindings.
pub(crate) struct Broker<'s> {
    pub(crate) queues: Queues<'s>,
    exchanges: Mutex<BTreeMap<String, Exchange>>,
    pub(crate) next_name: AtomicU64,
    exclusive_owners: Mutex<BTreeMap<String, u64>>,
    auto_delete: Mutex<BTreeMap<String, AutoDelete>>,
    policies: Mutex<BTreeMap<String, QueuePolicy>>,
    usage: Mutex<BTreeMap<String, Usage>>,
}

impl<'s> Broker<'s> {
    pub(crate) fn new(keyspace: &'s (dyn Keyspace + Sync)) -> Self {
        let mut exchanges = BTreeMap::new();
        for (name, kind) in [
            ("amq.direct", Kind::Direct),
            ("amq.fanout", Kind::Fanout),
            ("amq.topic", Kind::Topic),
            ("amq.headers", Kind::Headers),
            ("amq.match", Kind::Headers),
        ] {
            exchanges.insert(
                name.to_owned(),
                Exchange {
                    kind,
                    bindings: Vec::new(),
                },
            );
        }
        Self {
            queues: Queues::new(keyspace),
            exchanges: Mutex::new(exchanges),
            next_name: AtomicU64::new(1),
            exclusive_owners: Mutex::new(BTreeMap::new()),
            auto_delete: Mutex::new(BTreeMap::new()),
            policies: Mutex::new(BTreeMap::new()),
            usage: Mutex::new(BTreeMap::new()),
        }
    }

    fn used(&self, queue: &str, now: u64) {
        self.usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(queue.to_owned())
            .or_default()
            .last_used_ms = now;
    }

    /// Deletes the queues declared with `x-expires` that have had no consumer, no `basic.get`
    /// and no redeclaration for that long, as RabbitMQ's queue TTL does.
    fn delete_expired_queues(&self, now: u64) {
        let expiring: Vec<(String, u64)> = self
            .policies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(|(queue, policy)| policy.expires_ms.map(|expires| (queue.clone(), expires)))
            .collect();
        if expiring.is_empty() {
            return;
        }
        let gone: Vec<String> = {
            let usage = self.usage.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            expiring
                .into_iter()
                .filter(|(queue, expires)| {
                    usage.get(queue).is_some_and(|used| {
                        used.consumers == 0 && now.saturating_sub(used.last_used_ms) >= *expires
                    })
                })
                .map(|(queue, _)| queue)
                .collect()
        };
        for queue in gone {
            let _ = self.queues.delete(&queue);
            self.forget_queue(&queue);
        }
    }

    fn policy_of(&self, queue: &str) -> QueuePolicy {
        self.policies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(queue)
            .cloned()
            .unwrap_or_default()
    }

    fn set_policy(&self, queue: &str, policy: QueuePolicy) {
        let mut policies = self.policies.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if policy == QueuePolicy::default() {
            policies.remove(queue);
        } else {
            policies.insert(queue.to_owned(), policy);
        }
    }

    fn dead_letter(&self, queue: &str, stored: &[u8], now: u64, reason: &str) {
        let policy = self.policy_of(queue);
        let Some(exchange) = policy.dead_letter_exchange else {
            return;
        };
        let message = StoredMessage::read(queue, stored);
        let routing_key = policy
            .dead_letter_routing_key
            .unwrap_or_else(|| message.routing_key.clone());
        let headers = crate::field_table::with_death(
            crate::field_table::message_headers(message.properties),
            queue,
            reason,
            &message.exchange,
            &message.routing_key,
            now / 1000,
        );
        let properties = crate::field_table::with_headers(message.properties, &headers)
            .unwrap_or_else(|| message.properties.to_vec());
        let Some(targets) = self.route(&exchange, &routing_key, &headers) else {
            return;
        };
        let republished = encode_message(&exchange, &routing_key, &properties, message.body);
        for target in targets.iter().filter(|target| target.as_str() != queue) {
            if self.queues.exists(target).unwrap_or(false) {
                let _ = self.queues.send(target, &republished, 0, now);
            }
        }
    }

    fn expired(&self, queue: &str, message: &eggdb_queues::Message, now: u64) -> bool {
        let stored = StoredMessage::read(queue, &message.body);
        let ttl = match (self.policy_of(queue).message_ttl_ms, message_expiration(stored.properties)) {
            (Some(queue_ttl), Some(message_ttl)) => queue_ttl.min(message_ttl),
            (Some(ttl), None) | (None, Some(ttl)) => ttl,
            (None, None) => return false,
        };
        if now.saturating_sub(message.sent_ms) < ttl {
            return false;
        }
        let _ = self.queues.ack(queue, message.id);
        self.dead_letter(queue, &message.body, now, "expired");
        true
    }

    fn owner_of(&self, queue: &str) -> Option<u64> {
        self.exclusive_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(queue)
            .copied()
    }

    fn consumer_started(&self, queue: &str) {
        {
            let mut usage = self.usage.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = usage.entry(queue.to_owned()).or_default();
            entry.consumers += 1;
            entry.last_used_ms = now_ms();
        }
        let mut auto_delete = self
            .auto_delete
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = auto_delete.get_mut(queue) {
            entry.consumers += 1;
            entry.seen = true;
        }
    }

    fn consumer_ended(&self, queue: &str) {
        if let Some(entry) = self
            .usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(queue)
        {
            entry.consumers = entry.consumers.saturating_sub(1);
            entry.last_used_ms = now_ms();
        }
        let emptied = {
            let mut auto_delete = self
                .auto_delete
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match auto_delete.get_mut(queue) {
                Some(entry) => {
                    entry.consumers = entry.consumers.saturating_sub(1);
                    let emptied = entry.seen && entry.consumers == 0;
                    if emptied {
                        auto_delete.remove(queue);
                    }
                    emptied
                }
                None => false,
            }
        };
        if emptied {
            let _ = self.queues.delete(queue);
            self.forget_queue(queue);
        }
    }

    fn forget_queue(&self, queue: &str) {
        self.policies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(queue);
        self.usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(queue);
        self.exclusive_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(queue);
        let mut exchanges = self
            .exchanges
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for exchange in exchanges.values_mut() {
            exchange.bindings.retain(|binding| binding.queue != queue);
        }
    }

    fn route(
        &self,
        exchange: &str,
        routing_key: &str,
        headers: &[(String, FieldValue)],
    ) -> Option<Vec<String>> {
        if exchange.is_empty() {
            return Some(vec![routing_key.to_owned()]);
        }
        let exchanges = self
            .exchanges
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let found = exchanges.get(exchange)?;
        let mut queues: Vec<String> = found
            .bindings
            .iter()
            .filter(|binding| match found.kind {
                Kind::Direct => binding.key == routing_key,
                Kind::Fanout => true,
                Kind::Topic => topic_matches(&binding.key, routing_key),
                Kind::Headers => headers_match(&binding.arguments, headers),
            })
            .map(|binding| binding.queue.clone())
            .collect();
        queues.sort();
        queues.dedup();
        Some(queues)
    }
}

/// Whether an AMQP topic pattern (`*` one word, `#` zero or more) matches a routing key.
pub(crate) fn topic_matches(pattern: &str, key: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('.').collect();
    let key: Vec<&str> = if key.is_empty() {
        Vec::new()
    } else {
        key.split('.').collect()
    };
    if pattern.len() > 64 {
        return false;
    }
    let mut reachable = vec![false; key.len() + 1];
    reachable[0] = true;
    for part in &pattern {
        let mut next = vec![false; key.len() + 1];
        for at in 0..=key.len() {
            if !reachable[at] {
                continue;
            }
            match *part {
                "#" => next[at..].fill(true),
                "*" => {
                    if at < key.len() {
                        next[at + 1] = true;
                    }
                }
                word => {
                    if key.get(at) == Some(&word) {
                        next[at + 1] = true;
                    }
                }
            }
        }
        reachable = next;
    }
    reachable[key.len()]
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

pub(crate) fn encode_message(
    exchange: &str,
    routing_key: &str,
    properties: &[u8],
    body: &[u8],
) -> Vec<u8> {
    let mut out = Out::default();
    out.shortstr(exchange)
        .shortstr(routing_key)
        .longstr(properties);
    out.bytes.extend_from_slice(body);
    out.bytes
}

struct StoredMessage<'m> {
    exchange: String,
    routing_key: String,
    properties: &'m [u8],
    body: &'m [u8],
}

impl<'m> StoredMessage<'m> {
    fn framed(stored: &'m [u8]) -> Parsed<Self> {
        let mut args = Args::new(stored);
        let exchange = args.shortstr()?;
        let routing_key = args.shortstr()?;
        let properties = args.longstr()?;
        Ok(Self {
            exchange,
            routing_key,
            properties,
            body: args.rest(),
        })
    }

    fn read(queue: &str, stored: &'m [u8]) -> Self {
        Self::framed(stored).unwrap_or_else(|_| Self {
            exchange: String::new(),
            routing_key: queue.to_owned(),
            properties: &[],
            body: stored,
        })
    }
}

struct Consumer {
    tag: String,
    queue: String,
    no_ack: bool,
}

const MAX_STAGED_OPERATIONS: usize = 10_000;
const MAX_STAGED_BYTES: usize = 64 << 20;

#[derive(Default)]
struct Transaction {
    publishes: Vec<(Vec<String>, Vec<u8>)>,
    settlements: Vec<(u64, bool, Option<bool>)>,
    staged_bytes: usize,
}

impl Transaction {
    fn full(&self, more_bytes: usize) -> bool {
        self.publishes.len() + self.settlements.len() >= MAX_STAGED_OPERATIONS
            || self.staged_bytes.saturating_add(more_bytes) > MAX_STAGED_BYTES
    }
}

#[derive(Default)]
struct Channel {
    transaction: Option<Transaction>,
    consumers: Vec<Consumer>,
    prefetch: u16,
    unacked: BTreeMap<u64, (String, u64)>,
    rejectable: BTreeMap<u64, Vec<u8>>,
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
    id: u64,
    owned: Vec<String>,
    broker: &'b Broker<'s>,
    config: &'b ServerConfig,
    stage: Stage,
    channels: BTreeMap<u16, Channel>,
    outgoing: VecDeque<Vec<u8>>,
    bodies_held: Reservation<'b>,
    next_consumer: usize,
    heartbeat: Option<Duration>,
}

fn method(out: &Out, channel: u16) -> Vec<u8> {
    frame(FRAME_METHOD, channel, &out.bytes)
}

impl Connection<'_, '_> {
    fn declared_body_bytes(&self) -> usize {
        self.channels
            .values()
            .filter_map(|state| state.header.as_ref())
            .map(|(size, _)| usize::try_from(*size).unwrap_or(usize::MAX))
            .fold(0, usize::saturating_add)
    }

    fn release_finished_bodies(&mut self) {
        let declared = self.declared_body_bytes();
        if declared < self.bodies_held.bytes() {
            self.bodies_held.hold_if_large(declared);
        }
    }

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
            for consumer in state.consumers {
                self.broker.consumer_ended(&consumer.queue);
            }
        }
    }

    fn locked_by_another(&mut self, channel: u16, queue: &str, class: u16, method_id: u16) -> bool {
        match self.broker.owner_of(queue) {
            Some(owner) if owner != self.id => {
                self.close_channel(
                    channel,
                    RESOURCE_LOCKED,
                    &format!(
                        "RESOURCE_LOCKED - cannot obtain exclusive access to locked queue '{queue}'"
                    ),
                    class,
                    method_id,
                );
                true
            }
            _ => false,
        }
    }

    fn deliver_message(&mut self, channel: u16, out: Out, message: &StoredMessage<'_>) {
        self.send(channel, out);
        let mut header = Out::default();
        header
            .short(60)
            .short(0)
            .longlong(message.body.len() as u64);
        header
            .bytes
            .extend_from_slice(if message.properties.is_empty() {
                &[0, 0]
            } else {
                message.properties
            });
        self.outgoing
            .push_back(frame(FRAME_HEADER, channel, &header.bytes));
        for chunk in message.body.chunks((FRAME_MAX - 8) as usize) {
            self.outgoing.push_back(frame(FRAME_BODY, channel, chunk));
        }
    }

    fn pump(&mut self, held: &mut Reservation<'_>) -> bool {
        let mut consumers: Vec<(u16, String, String, bool)> = self
            .channels
            .iter()
            .flat_map(|(&channel, state)| {
                state.consumers.iter().map(move |consumer| {
                    (
                        channel,
                        consumer.tag.clone(),
                        consumer.queue.clone(),
                        consumer.no_ack,
                    )
                })
            })
            .collect();
        if consumers.is_empty() {
            return true;
        }
        let start = self.next_consumer % consumers.len();
        consumers.rotate_left(start);
        let mut pumped_bytes = 0usize;
        for (position, (channel, tag, queue, no_ack)) in consumers.into_iter().enumerate() {
            let room = match self.channels.get(&channel) {
                Some(state) if state.prefetch > 0 => {
                    usize::from(state.prefetch).saturating_sub(state.unacked.len())
                }
                Some(_) => 64,
                None => 0,
            };
            if room == 0 {
                continue;
            }
            let mut refused = false;
            let leased = self.broker.queues.receive_fitting(
                &queue,
                room,
                LEASE_MS,
                now_ms(),
                &mut |body_bytes: usize| {
                    let grown = pumped_bytes
                        .saturating_add(body_bytes)
                        .saturating_add(DELIVERY_FRAME_BYTES);
                    if held.hold_if_large(grown) {
                        pumped_bytes = grown;
                        true
                    } else {
                        refused = true;
                        false
                    }
                },
            );
            let Ok(messages) = leased else {
                continue;
            };
            if refused && messages.is_empty() && pumped_bytes == 0 {
                self.close_connection(RESOURCE_ERROR, OUT_OF_MEMORY_BUDGET, 0, 0);
                return false;
            }
            let delivered_any = !messages.is_empty();
            self.deliver_batch(channel, &tag, &queue, no_ack, messages);
            if refused {
                self.next_consumer = start
                    .saturating_add(position)
                    .saturating_add(usize::from(delivered_any));
                return true;
            }
        }
        true
    }

    fn deliver_batch(
        &mut self,
        channel: u16,
        tag: &str,
        queue: &str,
        no_ack: bool,
        messages: Vec<eggdb_queues::Message>,
    ) {
        let mut messages = messages.into_iter();
        let dead_lettering = self.broker.policy_of(queue).dead_letter_exchange.is_some();
        while let Some(message) = messages.next() {
            if self.broker.expired(queue, &message, now_ms()) {
                continue;
            }
            let Some(state) = self.channels.get_mut(&channel) else {
                let now = now_ms();
                for unsent in std::iter::once(message).chain(messages) {
                    let _ = self.broker.queues.unlease(queue, unsent.id, now);
                }
                return;
            };
            state.next_tag += 1;
            let delivery_tag = state.next_tag;
            if no_ack {
                let _ = self.broker.queues.ack(queue, message.id);
            } else {
                state
                    .unacked
                    .insert(delivery_tag, (queue.to_owned(), message.id));
                if dead_lettering {
                    state.rejectable.insert(delivery_tag, message.body.clone());
                }
            }
            let stored = StoredMessage::read(queue, &message.body);
            let mut out = Out::method(60, 60);
            out.shortstr(tag)
                .longlong(delivery_tag)
                .octet(u8::from(message.receive_count > 1))
                .shortstr(&stored.exchange)
                .shortstr(&stored.routing_key);
            self.deliver_message(channel, out, &stored);
        }
    }

    fn settle_or_stage(&mut self, channel: u16, tag: u64, multiple: bool, outcome: Option<bool>) {
        let staged = match self
            .channels
            .get_mut(&channel)
            .and_then(|state| state.transaction.as_mut())
        {
            Some(transaction) if !transaction.full(0) => {
                transaction.settlements.push((tag, multiple, outcome));
                true
            }
            Some(_) => {
                self.close_channel(
                    channel,
                    PRECONDITION_FAILED,
                    "PRECONDITION_FAILED - the transaction holds too many acknowledgements",
                    60,
                    80,
                );
                true
            }
            None => false,
        };
        if !staged {
            self.acknowledge(channel, tag, multiple, outcome);
        }
    }

    fn commit_transaction(&mut self, channel: u16) -> Parsed<()> {
        let Some(transaction) = self
            .channels
            .get_mut(&channel)
            .and_then(|state| state.transaction.replace(Transaction::default()))
        else {
            return Ok(());
        };
        let now = now_ms();
        for (queues, stored) in transaction.publishes {
            for queue in queues {
                if self.broker.queues.exists(&queue).unwrap_or(false) {
                    self.broker
                        .queues
                        .send(&queue, &stored, 0, now)
                        .map_err(|error| error.to_string())?;
                }
            }
        }
        for (tag, multiple, outcome) in transaction.settlements {
            self.acknowledge(channel, tag, multiple, outcome);
        }
        Ok(())
    }

    fn acknowledge(&mut self, channel: u16, tag: u64, multiple: bool, outcome: Option<bool>) {
        let Some(state) = self.channels.get_mut(&channel) else {
            return;
        };
        let tags: Vec<u64> = if multiple {
            state
                .unacked
                .range(..=if tag == 0 { u64::MAX } else { tag })
                .map(|(known, _)| *known)
                .collect()
        } else {
            vec![tag]
        };
        let now = now_ms();
        for known in tags {
            let rejected_body = state.rejectable.remove(&known);
            if let Some((queue, id)) = state.unacked.remove(&known) {
                match outcome {
                    Some(true) => {
                        let _ = self.broker.queues.release(&queue, id, now);
                    }
                    Some(false) => {
                        let _ = self.broker.queues.ack(&queue, id);
                        if let Some(body) = rejected_body {
                            self.broker.dead_letter(&queue, &body, now, "rejected");
                        }
                    }
                    None => {
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
        let properties = state
            .header
            .take()
            .map(|(_, properties)| properties)
            .unwrap_or_default();
        let body = std::mem::take(&mut state.body);
        let confirm = state.confirms.as_mut().map(|sequence| {
            *sequence += 1;
            *sequence
        });
        let headers = crate::field_table::message_headers(&properties);
        let Some(queues) = self.broker.route(&exchange, &routing_key, &headers) else {
            self.close_channel(
                channel,
                NOT_FOUND,
                &format!("NOT_FOUND - no exchange '{exchange}'"),
                60,
                40,
            );
            return Ok(());
        };
        let delay_ms = integer_named(&headers, "x-delay").unwrap_or(0);
        let stored = encode_message(&exchange, &routing_key, &properties, &body);
        if let Some(transaction) = self
            .channels
            .get_mut(&channel)
            .and_then(|state| state.transaction.as_mut())
        {
            if transaction.full(stored.len()) {
                self.close_channel(
                    channel,
                    PRECONDITION_FAILED,
                    "PRECONDITION_FAILED - the transaction holds too many messages",
                    60,
                    40,
                );
                return Ok(());
            }
            transaction.staged_bytes = transaction.staged_bytes.saturating_add(stored.len());
            transaction.publishes.push((queues, stored));
            return Ok(());
        }
        let now = now_ms();
        for queue in queues {
            if self.broker.queues.exists(&queue).unwrap_or(false) {
                self.broker
                    .queues
                    .send(&queue, &stored, delay_ms, now)
                    .map_err(|error| error.to_string())?;
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
                    self.close_connection(
                        FRAME_ERROR,
                        "a message body is larger than 16 MiB",
                        60,
                        40,
                    );
                    return Ok(false);
                }
                let properties = args.rest().to_vec();
                let declared = self
                    .declared_body_bytes()
                    .saturating_add(usize::try_from(size).unwrap_or(usize::MAX));
                if !self.bodies_held.hold_if_large(declared) {
                    self.close_connection(RESOURCE_ERROR, OUT_OF_MEMORY_BUDGET, 60, 40);
                    return Ok(false);
                }
                if let Some(state) = self.channels.get_mut(&incoming.channel) {
                    state.header = Some((size, properties));
                    if size == 0 {
                        return self.publish(incoming.channel).map(|()| true);
                    }
                }
                return Ok(true);
            }
            FRAME_BODY => {
                let buffered: usize = self.channels.values().map(|state| state.body.len()).sum();
                if buffered.saturating_add(incoming.payload.len()) > MOST_BUFFERED_BYTES {
                    self.close_connection(
                        FRAME_ERROR,
                        "the connection holds more than 64 MiB of unfinished message bodies",
                        60,
                        40,
                    );
                    return Ok(false);
                }
                let complete = match self.channels.get_mut(&incoming.channel) {
                    Some(state) => {
                        state.body.extend_from_slice(&incoming.payload);
                        let expected = state.header.as_ref().map_or(0, |(size, _)| *size);
                        if state.body.len() as u64 > expected {
                            self.close_connection(
                                FRAME_ERROR,
                                "a message body is longer than its header says",
                                60,
                                40,
                            );
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
                let (_, user, password) = (
                    parts.next(),
                    parts.next().unwrap_or_default(),
                    parts.next().unwrap_or_default(),
                );
                let valid = mechanism == "PLAIN"
                    && user == self.config.user.as_bytes()
                    && eggdb_crypto::constant_time_eq(
                        &eggdb_crypto::sha256(password),
                        &self.config.password_sha256,
                    );
                if !valid {
                    std::thread::sleep(Duration::from_millis(250));
                    self.close_connection(
                        ACCESS_REFUSED,
                        "ACCESS_REFUSED - Login was refused using authentication mechanism PLAIN",
                        10,
                        11,
                    );
                    return Ok(false);
                }
                let mut out = Out::method(10, 30);
                out.short(MAX_CHANNELS)
                    .long(FRAME_MAX)
                    .short(PROPOSED_HEARTBEAT_SECONDS);
                self.send(0, out);
                self.stage = Stage::TuneSent;
            }
            (Stage::TuneSent, 10, 31) => {
                let _channel_max = args.short()?;
                let _frame_max = args.long()?;
                let heartbeat_seconds = args.short()?;
                self.heartbeat = (heartbeat_seconds > 0)
                    .then(|| Duration::from_secs(u64::from(heartbeat_seconds)));
            }
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
                    self.close_connection(
                        CHANNEL_ERROR,
                        "CHANNEL_ERROR - channel already open or invalid",
                        20,
                        10,
                    );
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
                self.close_connection(
                    CHANNEL_ERROR,
                    "CHANNEL_ERROR - expected 'channel.open'",
                    class,
                    method_id,
                );
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
                    "headers" => Kind::Headers,
                    other => {
                        self.close_connection(
                            NOT_IMPLEMENTED,
                            &format!("NOT_IMPLEMENTED - exchange type '{other}'"),
                            40,
                            10,
                        );
                        return Ok(false);
                    }
                };
                let outcome = {
                    let mut exchanges = self
                        .broker
                        .exchanges
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    match exchanges.get(&name) {
                        Some(existing) if existing.kind != kind => Err(PRECONDITION_FAILED),
                        Some(_) => Ok(()),
                        None if passive => Err(NOT_FOUND),
                        None => {
                            exchanges.insert(
                                name.clone(),
                                Exchange {
                                    kind,
                                    bindings: Vec::new(),
                                },
                            );
                            Ok(())
                        }
                    }
                };
                match outcome {
                    Ok(()) if !no_wait => self.send(channel, Out::method(40, 11)),
                    Ok(()) => {}
                    Err(code) => self.close_channel(
                        channel,
                        code,
                        &format!("cannot declare exchange '{name}'"),
                        40,
                        10,
                    ),
                }
            }
            (Stage::Open, 50, 10) => {
                let _reserved = args.short()?;
                let mut name = args.shortstr()?;
                let passive = args.bit()?;
                let _durable = args.bit()?;
                let exclusive = args.bit()?;
                let auto_delete = args.bit()?;
                let no_wait = args.bit()?;
                let arguments = crate::field_table::decode_table(args.table()?)?;
                self.broker.delete_expired_queues(now_ms());
                if arguments.iter().any(|(argument, _)| argument == "x-max-priority") {
                    self.close_channel(
                        channel,
                        NOT_IMPLEMENTED,
                        "NOT_IMPLEMENTED - priority queues (x-max-priority) are not supported yet",
                        50,
                        10,
                    );
                    return Ok(true);
                }
                if name.is_empty() {
                    name = format!(
                        "amq.gen-{}",
                        self.broker.next_name.fetch_add(1, Ordering::Relaxed)
                    );
                }
                if self.locked_by_another(channel, &name, 50, 10) {
                    return Ok(true);
                }
                let exists = self
                    .broker
                    .queues
                    .exists(&name)
                    .map_err(|error| error.to_string())?;
                if passive && !exists {
                    self.close_channel(
                        channel,
                        NOT_FOUND,
                        &format!("NOT_FOUND - no queue '{name}'"),
                        50,
                        10,
                    );
                    return Ok(true);
                }
                if !exists && let Err(error) = self.broker.queues.declare(&name) {
                    self.close_channel(channel, PRECONDITION_FAILED, &error.to_string(), 50, 10);
                    return Ok(true);
                }
                if !passive {
                    self.broker.set_policy(&name, QueuePolicy::of(&arguments));
                }
                self.broker.used(&name, now_ms());
                if !exists && !passive {
                    if exclusive {
                        self.broker
                            .exclusive_owners
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(name.clone(), self.id);
                        self.owned.push(name.clone());
                    }
                    if auto_delete {
                        self.broker
                            .auto_delete
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(name.clone(), AutoDelete::default());
                    }
                }
                if !no_wait {
                    let depth = self
                        .broker
                        .queues
                        .depth(&name, now_ms())
                        .map_err(|error| error.to_string())?;
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
                let arguments = crate::field_table::decode_table(args.table()?)?;
                if self.locked_by_another(channel, &queue, 50, method_id) {
                    return Ok(true);
                }
                let found = {
                    let mut exchanges = self
                        .broker
                        .exchanges
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    match exchanges.get_mut(&exchange) {
                        Some(found) => {
                            found.bindings.retain(|binding| {
                                !(binding.key == routing_key
                                    && binding.queue == queue
                                    && (found.kind != Kind::Headers
                                        || binding.arguments == arguments))
                            });
                            if method_id == 20 {
                                found.bindings.push(Binding {
                                    key: routing_key,
                                    queue,
                                    arguments,
                                });
                            }
                            true
                        }
                        None => false,
                    }
                };
                if !found {
                    self.close_channel(
                        channel,
                        NOT_FOUND,
                        &format!("NOT_FOUND - no exchange '{exchange}'"),
                        50,
                        method_id,
                    );
                } else if !no_wait {
                    self.send(channel, Out::method(50, method_id + 1));
                }
            }
            (Stage::Open, 50, 30) | (Stage::Open, 50, 40) => {
                let _reserved = args.short()?;
                let queue = args.shortstr()?;
                if self.locked_by_another(channel, &queue, 50, method_id) {
                    return Ok(true);
                }
                let outcome = if method_id == 30 {
                    self.broker.queues.purge(&queue)
                } else {
                    let deleted = self.broker.queues.delete(&queue);
                    if deleted.is_ok() {
                        self.broker.forget_queue(&queue);
                        self.broker
                            .auto_delete
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&queue);
                        self.owned.retain(|owned| owned != &queue);
                    }
                    deleted
                };
                match outcome {
                    Ok(count) => {
                        let mut out = Out::method(50, method_id + 1);
                        out.long(count as u32);
                        self.send(channel, out);
                    }
                    Err(_) => self.close_channel(
                        channel,
                        NOT_FOUND,
                        &format!("NOT_FOUND - no queue '{queue}'"),
                        50,
                        method_id,
                    ),
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
                if self.locked_by_another(channel, &queue, 60, 20) {
                    return Ok(true);
                }
                self.broker.delete_expired_queues(now_ms());
                if !self
                    .broker
                    .queues
                    .exists(&queue)
                    .map_err(|error| error.to_string())?
                {
                    self.close_channel(
                        channel,
                        NOT_FOUND,
                        &format!("NOT_FOUND - no queue '{queue}'"),
                        60,
                        20,
                    );
                    return Ok(true);
                }
                if tag.is_empty() {
                    tag = format!(
                        "amq.ctag-{}",
                        self.broker.next_name.fetch_add(1, Ordering::Relaxed)
                    );
                }
                if let Some(state) = self.channels.get_mut(&channel) {
                    self.broker.consumer_started(&queue);
                    state.consumers.push(Consumer {
                        tag: tag.clone(),
                        queue,
                        no_ack,
                    });
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
                let mut ended = Vec::new();
                if let Some(state) = self.channels.get_mut(&channel) {
                    state.consumers.retain(|consumer| {
                        let keep = consumer.tag != tag;
                        if !keep {
                            ended.push(consumer.queue.clone());
                        }
                        keep
                    });
                }
                for queue in ended {
                    self.broker.consumer_ended(&queue);
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
                self.broker.delete_expired_queues(now_ms());
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
                if self.locked_by_another(channel, &queue, 60, 70) {
                    return Ok(true);
                }
                self.broker.delete_expired_queues(now_ms());
                self.broker.used(&queue, now_ms());
                let messages = match self.broker.queues.receive(&queue, 1, LEASE_MS, now_ms()) {
                    Ok(messages) => messages,
                    Err(_) => {
                        self.close_channel(
                            channel,
                            NOT_FOUND,
                            &format!("NOT_FOUND - no queue '{queue}'"),
                            60,
                            70,
                        );
                        return Ok(true);
                    }
                };
                let dead_lettering = self.broker.policy_of(&queue).dead_letter_exchange.is_some();
                match messages
                    .into_iter()
                    .next()
                    .filter(|message| !self.broker.expired(&queue, message, now_ms()))
                {
                    None => {
                        let mut out = Out::method(60, 72);
                        out.shortstr("");
                        self.send(channel, out);
                    }
                    Some(message) => {
                        let remaining = self
                            .broker
                            .queues
                            .depth(&queue, now_ms())
                            .map_or(0, |depth| depth.ready);
                        let Some(state) = self.channels.get_mut(&channel) else {
                            return Ok(true);
                        };
                        state.next_tag += 1;
                        let delivery_tag = state.next_tag;
                        if no_ack {
                            let _ = self.broker.queues.ack(&queue, message.id);
                        } else {
                            state
                                .unacked
                                .insert(delivery_tag, (queue.clone(), message.id));
                            if dead_lettering {
                                state.rejectable.insert(delivery_tag, message.body.clone());
                            }
                        }
                        let stored = StoredMessage::read(&queue, &message.body);
                        let mut out = Out::method(60, 71);
                        out.longlong(delivery_tag)
                            .octet(u8::from(message.receive_count > 1))
                            .shortstr(&stored.exchange)
                            .shortstr(&stored.routing_key)
                            .long(remaining as u32);
                        self.deliver_message(channel, out, &stored);
                    }
                }
            }
            (Stage::Open, 60, 80) => {
                let tag = args.longlong()?;
                let multiple = args.bit()?;
                self.settle_or_stage(channel, tag, multiple, None);
            }
            (Stage::Open, 60, 90) => {
                let tag = args.longlong()?;
                let requeue = args.bit()?;
                self.settle_or_stage(channel, tag, false, Some(requeue));
            }
            (Stage::Open, 60, 120) => {
                let tag = args.longlong()?;
                let multiple = args.bit()?;
                let requeue = args.bit()?;
                self.settle_or_stage(channel, tag, multiple, Some(requeue));
            }
            (Stage::Open, 60, 110) => {
                self.acknowledge(channel, 0, true, Some(true));
                self.send(channel, Out::method(60, 111));
            }
            (Stage::Open, 85, 10) => {
                let no_wait = args.bit()?;
                if self
                    .channels
                    .get(&channel)
                    .is_some_and(|state| state.transaction.is_some())
                {
                    self.close_channel(
                        channel,
                        PRECONDITION_FAILED,
                        "PRECONDITION_FAILED - cannot switch from tx to confirm mode",
                        85,
                        10,
                    );
                    return Ok(true);
                }
                if let Some(state) = self.channels.get_mut(&channel) {
                    state.confirms.get_or_insert(0);
                }
                if !no_wait {
                    self.send(channel, Out::method(85, 11));
                }
            }
            (Stage::Open, 90, 10) => {
                let refused = self
                    .channels
                    .get(&channel)
                    .is_some_and(|state| state.confirms.is_some());
                if refused {
                    self.close_channel(
                        channel,
                        PRECONDITION_FAILED,
                        "PRECONDITION_FAILED - cannot switch from confirm to tx mode",
                        90,
                        10,
                    );
                    return Ok(true);
                }
                if let Some(state) = self.channels.get_mut(&channel) {
                    state.transaction.get_or_insert_with(Transaction::default);
                }
                self.send(channel, Out::method(90, 11));
            }
            (Stage::Open, 90, 20 | 30) => {
                let selected = self
                    .channels
                    .get(&channel)
                    .is_some_and(|state| state.transaction.is_some());
                if !selected {
                    self.close_channel(
                        channel,
                        PRECONDITION_FAILED,
                        "PRECONDITION_FAILED - channel is not transactional",
                        90,
                        method_id,
                    );
                    return Ok(true);
                }
                if method_id == 20 {
                    self.commit_transaction(channel)?;
                } else if let Some(state) = self.channels.get_mut(&channel) {
                    state.transaction = Some(Transaction::default());
                }
                self.send(channel, Out::method(90, method_id + 1));
            }
            _ => {
                self.close_connection(
                    COMMAND_INVALID,
                    &format!("COMMAND_INVALID - unexpected method {class}.{method_id}"),
                    class,
                    method_id,
                );
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn start_frame() -> Vec<u8> {
    let mut out = Out::method(10, 10);
    out.octet(0)
        .octet(9)
        .properties(
            &[
                ("product", "EggDB"),
                ("platform", "Rust"),
                ("information", "compatible with AMQP 0-9-1"),
            ],
            &["publisher_confirms", "basic.nack"],
        )
        .longstr(b"PLAIN")
        .longstr(b"en_US");
    method(&out, 0)
}

pub(crate) fn serve_stream(
    broker: &Broker<'_>,
    config: &ServerConfig,
    mut reader: impl Read,
    writer: &mut impl Write,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    let mut header = [0u8; 8];
    let mut filled = 0;
    while filled < 8 {
        match reader.read(&mut header[filled..]) {
            Ok(0) => return Ok(()),
            Ok(read) => filled += read,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                if stop() {
                    return Ok(());
                }
            }
            Err(error) => return Err(error),
        }
    }
    if &header == crate::amqp1::SASL_HEADER {
        return crate::amqp1::serve_stream(broker, config, reader, writer, stop);
    }
    if &header == crate::amqp1::AMQP_HEADER {
        writer.write_all(crate::amqp1::SASL_HEADER)?;
        return Ok(());
    }
    if &header != PROTOCOL_HEADER {
        writer.write_all(PROTOCOL_HEADER)?;
        return Ok(());
    }
    writer.write_all(&start_frame())?;
    let mut connection = Connection {
        id: broker.next_name.fetch_add(1, Ordering::Relaxed),
        owned: Vec::new(),
        broker,
        config,
        stage: Stage::StartSent,
        channels: BTreeMap::new(),
        outgoing: VecDeque::new(),
        bodies_held: config.memory_budget.empty_reservation(),
        next_consumer: 0,
        heartbeat: None,
    };
    let mut last_received = Instant::now();
    let mut last_sent = Instant::now();
    let mut read_held = config.memory_budget.empty_reservation();
    let mut outgoing_held = config.memory_budget.empty_reservation();
    let mut buffer = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let outcome = loop {
        if stop() {
            break Ok(());
        }
        match reader.read(&mut chunk) {
            Ok(0) => break Ok(()),
            Ok(read) => {
                if !read_held.hold_if_large(buffer.len().saturating_add(read)) {
                    connection.close_connection(RESOURCE_ERROR, OUT_OF_MEMORY_BUDGET, 0, 0);
                    while let Some(bytes) = connection.outgoing.pop_front() {
                        writer.write_all(&bytes)?;
                    }
                    break Ok(());
                }
                buffer.extend_from_slice(&chunk[..read]);
                last_received = Instant::now();
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => break Err(error),
        }
        let mut keep_going = true;
        loop {
            match take_frame(&mut buffer) {
                Ok(Some(incoming)) => match connection.handle(incoming) {
                    Ok(true) => connection.release_finished_bodies(),
                    Ok(false) => {
                        keep_going = false;
                        break;
                    }
                    Err(message) => {
                        connection.close_connection(
                            FRAME_ERROR,
                            &format!("FRAME_ERROR - {message}"),
                            0,
                            0,
                        );
                        keep_going = false;
                        break;
                    }
                },
                Ok(None) => break,
                Err(message) => {
                    connection.close_connection(
                        FRAME_ERROR,
                        &format!("FRAME_ERROR - {message}"),
                        0,
                        0,
                    );
                    keep_going = false;
                    break;
                }
            }
        }
        read_held.hold_if_large(buffer.len());
        if keep_going && matches!(connection.stage, Stage::Open) {
            keep_going = connection.pump(&mut outgoing_held);
        }
        if keep_going && let Some(interval) = connection.heartbeat {
            if last_received.elapsed() >= interval.saturating_mul(2) {
                break Ok(());
            }
            if connection.outgoing.is_empty() && last_sent.elapsed() >= interval / 2 {
                connection
                    .outgoing
                    .push_back(frame(FRAME_HEARTBEAT, 0, &[]));
            }
        }
        if !connection.outgoing.is_empty() {
            last_sent = Instant::now();
        }
        while let Some(bytes) = connection.outgoing.pop_front() {
            writer.write_all(&bytes)?;
        }
        outgoing_held.resize(0);
        if !keep_going {
            break Ok(());
        }
    };
    let channels: Vec<u16> = connection.channels.keys().copied().collect();
    for channel in channels {
        connection.release_channel(channel);
    }
    for queue in std::mem::take(&mut connection.owned) {
        let _ = broker.queues.delete(&queue);
        broker.forget_queue(&queue);
        broker
            .auto_delete
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&queue);
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
pub fn serve(
    keyspace: &(dyn Keyspace + Sync),
    listener: &TcpListener,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let broker = Broker::new(keyspace);
    let active = AtomicUsize::new(0);
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
                || stream.set_nodelay(true).is_err()
            {
                continue;
            }
            if active.fetch_add(1, Ordering::AcqRel) >= config.max_connections {
                active.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let (broker, active) = (&broker, &active);
            let spawned = std::thread::Builder::new()
                .name("eggdb-amqp".to_owned())
                .spawn_scoped(scope, move || {
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

#[cfg(test)]
mod queue_expiry_tests {
    use super::{Broker, QueuePolicy};
    use eggdb_keyspace::MemoryKeyspace;

    #[test]
    fn a_queue_unused_for_its_x_expires_is_deleted_unless_it_has_a_consumer() {
        let keyspace = MemoryKeyspace::default();
        let broker = Broker::new(&keyspace);
        for queue in ["idle", "watched"] {
            broker.queues.declare(queue).unwrap();
            broker.set_policy(
                queue,
                QueuePolicy {
                    expires_ms: Some(10),
                    ..QueuePolicy::default()
                },
            );
            broker.used(queue, 0);
        }
        broker.consumer_started("watched");
        broker.delete_expired_queues(5);
        assert!(broker.queues.exists("idle").unwrap());
        broker.delete_expired_queues(u64::MAX);
        assert!(!broker.queues.exists("idle").unwrap());
        assert!(broker.queues.exists("watched").unwrap());
    }
}
