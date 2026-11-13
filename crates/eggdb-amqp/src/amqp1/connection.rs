use super::message::{DestinationKind, inbound, names_topic, routable_address, selector_field, transfer_payload};
use super::value::{Value, decode_all, decode_first, encode};
use crate::field_table::BasicProperties;
use crate::server::{Broker, ServerConfig, StoredMessage, encode_message};
use eggdb_queues::selector::Selector;
use eggdb_platform::connection_limits::{Reservation, UNCOUNTED_BUFFER_BYTES};
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const SASL_HEADER: &[u8; 8] = b"AMQP\x03\x01\x00\x00";
pub(crate) const AMQP_HEADER: &[u8; 8] = b"AMQP\x00\x01\x00\x00";

const FRAME_AMQP: u8 = 0;
const FRAME_SASL: u8 = 1;
const MAX_FRAME: u32 = 64 * 1024;
const MIN_FRAME: u32 = 512;
const MAX_MESSAGE: usize = 16 << 20;
const CHANNEL_MAX: u16 = 255;
const HANDLE_MAX: u32 = 1023;
const WINDOW: u32 = 65_535;
const LINK_CREDIT: u32 = 500;
const MAX_PER_PUMP: u32 = 100;
const MAX_RANGE: u32 = 10_000;
const LEASE_MS: u64 = 30 * 60 * 1000;
const DELIVERY_FRAME_BYTES: usize = 64;
const RESOURCE_LIMIT_EXCEEDED: &str = "amqp:resource-limit-exceeded";
const OUT_OF_MEMORY_BUDGET: &str = "the server's connections already hold their memory budget";

const SASL_MECHANISMS: u64 = 0x40;
const SASL_INIT: u64 = 0x41;
const SASL_OUTCOME: u64 = 0x44;
const OPEN: u64 = 0x10;
const BEGIN: u64 = 0x11;
const ATTACH: u64 = 0x12;
const FLOW: u64 = 0x13;
const TRANSFER: u64 = 0x14;
const DISPOSITION: u64 = 0x15;
const DETACH: u64 = 0x16;
const END: u64 = 0x17;
const CLOSE: u64 = 0x18;
const ERROR: u64 = 0x1d;
const ACCEPTED: u64 = 0x24;
const REJECTED: u64 = 0x25;
const RELEASED: u64 = 0x26;
const MODIFIED: u64 = 0x27;
const VALUE_SECTION: u64 = 0x77;
const SOURCE: u64 = 0x28;
const COORDINATOR: u64 = 0x30;
const DECLARE: u64 = 0x31;
const DISCHARGE: u64 = 0x32;
const DECLARED: u64 = 0x33;
const TRANSACTIONAL_STATE: u64 = 0x34;
const SELECTOR_FILTER: u64 = 0x0000_468c_0000_0004;
const SELECTOR_FILTER_NAME: &str = "apache.org:selector-filter:string";
const TOPIC_EXCHANGE: &str = "amq.topic";
const SELECTOR_SCAN: usize = 1_000;
const MAX_TRANSACTIONS: usize = 64;
const MAX_TRANSACTION_ITEMS: usize = 100_000;
const MAX_TRANSACTION_BYTES: usize = 64 << 20;
const SOURCE_FILTER_FIELD: usize = 7;
const TRANSFER_STATE_FIELD: usize = 7;
const SOURCE_CAPABILITIES_FIELD: usize = 10;
const TARGET_CAPABILITIES_FIELD: usize = 6;

/// What a receiver's disposition does to a message (AMQP 1.0 section 3.4): `accepted` removes
/// it; `rejected`, and `modified` with `undeliverable-here`, send it to the queue's dead letters;
/// `modified` with `delivery-failed` returns it counting the delivery; `released`, and
/// `modified` without either flag, return it as if it had never been sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Accepted,
    DeadLetter,
    Failed,
    Untouched,
}

impl Outcome {
    fn of(state: &Value) -> Self {
        match state.descriptor() {
            Some(REJECTED) => Self::DeadLetter,
            Some(RELEASED) => Self::Untouched,
            Some(MODIFIED) if state.field(1).flag().unwrap_or(false) => Self::DeadLetter,
            Some(MODIFIED) if state.field(0).flag().unwrap_or(false) => Self::Failed,
            Some(MODIFIED) => Self::Untouched,
            _ => Self::Accepted,
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

fn frame(kind: u8, channel: u16, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(
        &u32::try_from(8 + body.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    out.push(2);
    out.push(kind);
    out.extend_from_slice(&channel.to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn performative(code: u64, fields: Vec<Value>) -> Vec<u8> {
    let mut out = Vec::new();
    encode(&Value::described(code, fields), &mut out);
    out
}

fn error(condition: &str, description: &str) -> Value {
    Value::described(
        ERROR,
        vec![
            Value::Symbol(condition.to_owned()),
            Value::String(description.to_owned()),
        ],
    )
}

struct Incoming {
    delivery_id: u32,
    settled: bool,
    bytes: Vec<u8>,
    transaction: Option<u64>,
}

enum Direction {
    FromClient,
    ToClient { presettled: bool, drain: bool },
}

/// Where the messages a client sends on a link go: its queue, a topic (the `amq.topic`
/// exchange with the topic's name as routing key), the queue or topic each message's `to`
/// names (the anonymous relay), or the transaction coordinator.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    Queue,
    Topic(String),
    Anonymous,
    Coordinator,
}

/// A topic subscription's queue, which only its link reads: durable ones outlive the link
/// until the client unsubscribes by detaching with `closed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Subscription {
    durable: bool,
}

struct Link {
    queue: String,
    route: Route,
    direction: Direction,
    credit: u32,
    delivery_count: u32,
    incoming: Option<Incoming>,
    selector: Option<Selector>,
    subscription: Option<Subscription>,
}

#[derive(Default)]
struct Transaction {
    publishes: Vec<(Vec<String>, Vec<u8>)>,
    settles: Vec<(String, u64, Outcome)>,
    bytes: usize,
}

impl Transaction {
    fn full(&self, more_bytes: usize) -> bool {
        self.publishes.len() + self.settles.len() >= MAX_TRANSACTION_ITEMS
            || self.bytes.saturating_add(more_bytes) > MAX_TRANSACTION_BYTES
    }
}

struct Unsettled {
    handle: u32,
    queue: String,
    id: u64,
}

struct Session {
    next_outgoing_id: u32,
    next_incoming_id: u32,
    links: BTreeMap<u32, Link>,
    unsettled: BTreeMap<u32, Unsettled>,
}

#[derive(PartialEq, Eq)]
enum Stage {
    Sasl,
    AwaitHeader,
    AwaitOpen,
    Open,
    Closed,
}

struct Connection<'b, 's> {
    broker: &'b Broker<'s>,
    config: &'b ServerConfig,
    stage: Stage,
    remote_max_frame: u32,
    remote_idle: Option<Duration>,
    sessions: BTreeMap<u16, Session>,
    outgoing: Vec<u8>,
    unfinished_held: Reservation<'b>,
    outgoing_held: Reservation<'b>,
    next_link: usize,
    dynamic_queues: Vec<String>,
    container_id: String,
    transactions: BTreeMap<u64, Transaction>,
    next_transaction: u64,
}

const DYNAMIC_FIELD: usize = 4;
const LOCAL_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_DYNAMIC_QUEUES: usize = 1_000;

fn with_address(terminus: &Value, address: &str) -> Value {
    let Value::Described(descriptor, fields) = terminus else {
        return terminus.clone();
    };
    let mut fields = match fields.as_ref() {
        Value::List(fields) => fields.clone(),
        _ => Vec::new(),
    };
    if fields.is_empty() {
        fields.push(Value::Null);
    }
    fields[0] = Value::String(address.to_owned());
    Value::Described(descriptor.clone(), Box::new(Value::List(fields)))
}

fn delivery_bytes(body_bytes: usize, max_frame: u32) -> usize {
    let frame_room = usize::try_from(max_frame).unwrap_or(usize::MAX).max(1);
    let payload = body_bytes.saturating_add(DELIVERY_FRAME_BYTES);
    payload.saturating_add(
        payload
            .div_ceil(frame_room)
            .saturating_add(1)
            .saturating_mul(DELIVERY_FRAME_BYTES),
    )
}

fn transaction_id(value: &Value) -> Option<u64> {
    match value {
        Value::Binary(bytes) => bytes.as_slice().try_into().ok().map(u64::from_be_bytes),
        _ => None,
    }
}

fn transaction_of(state: &Value) -> Option<(u64, &Value)> {
    (state.descriptor() == Some(TRANSACTIONAL_STATE)).then(|| state.field(0)).and_then(transaction_id).map(|id| (id, state.field(1)))
}

fn symbols(value: &Value) -> Vec<&str> {
    match value {
        Value::Array(items) | Value::List(items) => items.iter().filter_map(Value::text).collect(),
        Value::Symbol(single) => vec![single.as_str()],
        _ => Vec::new(),
    }
}

fn selector_of(filter: &Value) -> Result<Option<Selector>, String> {
    let Value::Map(entries) = filter else {
        return Ok(None);
    };
    for (_, entry) in entries {
        let Value::Described(descriptor, inner) = entry else {
            continue;
        };
        let named = descriptor.unsigned() == Some(SELECTOR_FILTER) || descriptor.text() == Some(SELECTOR_FILTER_NAME);
        if let (true, Some(text)) = (named, inner.text()) {
            return Selector::parse(text).map(Some);
        }
    }
    Ok(None)
}

fn subscription_queue(container_id: &str, link_name: &str, shared: bool) -> String {
    let name = link_name.split('|').next().unwrap_or_default();
    if shared {
        format!("subscription.shared.{name}")
    } else {
        format!("subscription.{container_id}.{name}")
    }
}

impl Connection<'_, '_> {
    fn unfinished_delivery_bytes(&self) -> usize {
        self.sessions
            .values()
            .flat_map(|session| session.links.values())
            .filter_map(|link| link.incoming.as_ref())
            .map(|incoming| incoming.bytes.len())
            .fold(0, usize::saturating_add)
    }

    fn release_finished_deliveries(&mut self) {
        let unfinished = self.unfinished_delivery_bytes();
        if unfinished < self.unfinished_held.bytes() {
            self.unfinished_held.hold_if_large(unfinished);
        }
    }

    fn written(&mut self) {
        self.outgoing.clear();
        self.outgoing.shrink_to(UNCOUNTED_BUFFER_BYTES);
        self.outgoing_held.resize(0);
    }

    fn send(&mut self, kind: u8, channel: u16, body: &[u8]) {
        self.outgoing.extend_from_slice(&frame(kind, channel, body));
    }

    fn close_with(&mut self, condition: &str, description: &str) {
        self.send(
            FRAME_AMQP,
            0,
            &performative(CLOSE, vec![error(condition, description)]),
        );
        self.stage = Stage::Closed;
    }

    fn sasl(&mut self, body: &Value) {
        if body.descriptor() != Some(SASL_INIT) {
            self.stage = Stage::Closed;
            return;
        }
        let mechanism = body.field(0).text().unwrap_or_default();
        let response = match body.field(1) {
            Value::Binary(bytes) => bytes.as_slice(),
            _ => &[],
        };
        let mut parts = response.split(|&byte| byte == 0);
        let (_, user, password) = (
            parts.next(),
            parts.next().unwrap_or_default(),
            parts.next().unwrap_or_default(),
        );
        let authorized = mechanism == "PLAIN"
            && eggdb_crypto::constant_time_eq(user, self.config.user.as_bytes())
            && eggdb_crypto::constant_time_eq(
                &eggdb_crypto::sha256(password),
                &self.config.password_sha256,
            );
        self.send(
            FRAME_SASL,
            0,
            &performative(
                SASL_OUTCOME,
                vec![Value::Ubyte(if authorized { 0 } else { 1 })],
            ),
        );
        self.stage = if authorized {
            Stage::AwaitHeader
        } else {
            Stage::Closed
        };
    }

    fn open(&mut self, body: &Value) {
        if body.descriptor() != Some(OPEN) {
            self.close_with(
                "amqp:connection:framing-error",
                "the first frame must be open",
            );
            return;
        }
        let max_frame = body
            .field(2)
            .unsigned()
            .and_then(|size| u32::try_from(size).ok())
            .unwrap_or(u32::MAX);
        self.remote_max_frame = max_frame.clamp(MIN_FRAME, 1 << 20);
        self.remote_idle = body
            .field(4)
            .unsigned()
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis);
        self.container_id = body.field(0).text().unwrap_or_default().chars().take(128).collect();
        self.send(
            FRAME_AMQP,
            0,
            &performative(
                OPEN,
                vec![
                    Value::String("eggdb".to_owned()),
                    Value::Null,
                    Value::Uint(MAX_FRAME),
                    Value::Ushort(CHANNEL_MAX),
                    Value::Uint(u32::try_from(LOCAL_IDLE_TIMEOUT.as_millis()).unwrap_or(u32::MAX)),
                    Value::Null,
                    Value::Null,
                    Value::Array(vec![
                        Value::Symbol("ANONYMOUS-RELAY".to_owned()),
                        Value::Symbol("SHARED-SUBS".to_owned()),
                    ]),
                ],
            ),
        );
        self.stage = Stage::Open;
    }

    fn handle(&mut self, channel: u16, body: &Value, payload: &[u8]) -> Result<(), String> {
        match body.descriptor() {
            Some(BEGIN) => self.begin(channel, body),
            Some(ATTACH) => self.attach(channel, body),
            Some(FLOW) => self.flow(channel, body),
            Some(TRANSFER) => self.transfer(channel, body, payload),
            Some(DISPOSITION) => self.disposition(channel, body),
            Some(DETACH) => {
                let handle = self.handle_of(body)?;
                let closed = body.field(1).flag().unwrap_or(false);
                self.detach(channel, handle, closed);
                self.send(
                    FRAME_AMQP,
                    channel,
                    &performative(DETACH, vec![Value::Uint(handle), Value::Bool(true)]),
                );
                Ok(())
            }
            Some(END) => {
                if let Some(session) = self.sessions.remove(&channel) {
                    self.release_all(session.unsettled);
                }
                self.send(FRAME_AMQP, channel, &performative(END, Vec::new()));
                Ok(())
            }
            Some(CLOSE) => {
                self.send(FRAME_AMQP, 0, &performative(CLOSE, Vec::new()));
                self.stage = Stage::Closed;
                Ok(())
            }
            _ => Err("the frame is not an AMQP 1.0 performative".to_owned()),
        }
    }

    fn handle_of(&self, body: &Value) -> Result<u32, String> {
        body.field(0)
            .unsigned()
            .and_then(|handle| u32::try_from(handle).ok())
            .ok_or_else(|| "the frame has no link handle".to_owned())
    }

    fn session(&mut self, channel: u16) -> Result<&mut Session, String> {
        self.sessions
            .get_mut(&channel)
            .ok_or_else(|| format!("channel {channel} has no session"))
    }

    fn begin(&mut self, channel: u16, body: &Value) -> Result<(), String> {
        if channel > CHANNEL_MAX || self.sessions.contains_key(&channel) {
            return Err(format!("channel {channel} cannot begin a session"));
        }
        let next_incoming_id = body
            .field(1)
            .unsigned()
            .and_then(|id| u32::try_from(id).ok())
            .unwrap_or(0);
        self.sessions.insert(
            channel,
            Session {
                next_outgoing_id: 0,
                next_incoming_id,
                links: BTreeMap::new(),
                unsettled: BTreeMap::new(),
            },
        );
        self.send(
            FRAME_AMQP,
            channel,
            &performative(
                BEGIN,
                vec![
                    Value::Ushort(channel),
                    Value::Uint(0),
                    Value::Uint(WINDOW),
                    Value::Uint(WINDOW),
                    Value::Uint(HANDLE_MAX),
                ],
            ),
        );
        Ok(())
    }

    fn link_flow(&self, session: &Session, handle: u32, link: &Link, drain: bool) -> Vec<u8> {
        performative(
            FLOW,
            vec![
                Value::Uint(session.next_incoming_id),
                Value::Uint(WINDOW),
                Value::Uint(session.next_outgoing_id),
                Value::Uint(WINDOW),
                Value::Uint(handle),
                Value::Uint(link.delivery_count),
                Value::Uint(link.credit),
                Value::Null,
                Value::Bool(drain),
            ],
        )
    }

    fn attach(&mut self, channel: u16, body: &Value) -> Result<(), String> {
        let handle = body
            .field(1)
            .unsigned()
            .and_then(|handle| u32::try_from(handle).ok())
            .filter(|handle| *handle <= HANDLE_MAX)
            .ok_or("attach has no valid handle")?;
        let client_receives = body.field(2).flag().ok_or("attach has no role")?;
        if self.session(channel)?.links.contains_key(&handle) {
            return Err(format!("handle {handle} is already attached"));
        }
        let terminus = if client_receives {
            body.field(5)
        } else {
            body.field(6)
        };
        let link_name = body.field(0).text().unwrap_or_default().to_owned();
        if !client_receives && terminus.descriptor() == Some(COORDINATOR) {
            return self.attach_coordinator(channel, handle, body);
        }
        let dynamic = terminus.field(DYNAMIC_FIELD).flag() == Some(true);
        let written = terminus.field(0).text().unwrap_or_default();
        let capabilities = symbols(terminus.field(if client_receives {
            SOURCE_CAPABILITIES_FIELD
        } else {
            TARGET_CAPABILITIES_FIELD
        }));
        let topic = names_topic(written)
            || capabilities
                .iter()
                .any(|capability| matches!(*capability, "topic" | "temporary-topic"));
        let shared = capabilities.contains(&"shared");
        let durable = terminus.field(1).unsigned().unwrap_or(0) > 0
            || terminus.field(2).text() == Some("never");
        let mut address = routable_address(written);
        let mut route = Route::Queue;
        let mut subscription = None;
        let mut reply_source = None;
        let selector = if client_receives {
            selector_of(terminus.field(SOURCE_FILTER_FIELD))
        } else {
            Ok(None)
        };
        if dynamic {
            if self.dynamic_queues.len() >= MAX_DYNAMIC_QUEUES {
                return Err("this connection holds too many dynamic queues".to_owned());
            }
            address = format!(
                "amq.dynamic-{}",
                self.broker.next_name.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            if !topic {
                self.dynamic_queues.push(address.clone());
            }
        }
        let declared = match (&selector, client_receives) {
            (Err(description), _) => Err(format!("the selector cannot be read: {description}")),
            (Ok(_), false) if topic => {
                route = Route::Topic(address.clone());
                Ok(())
            }
            (Ok(_), false) if address.is_empty() && !dynamic => {
                route = Route::Anonymous;
                Ok(())
            }
            (Ok(_), true) if topic => {
                let queue = if durable || shared {
                    subscription_queue(&self.container_id, &link_name, shared)
                } else {
                    if self.dynamic_queues.len() >= MAX_DYNAMIC_QUEUES {
                        return Err("this connection holds too many dynamic queues".to_owned());
                    }
                    let queue = format!(
                        "amq.subscription-{}",
                        self.broker.next_name.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    );
                    self.dynamic_queues.push(queue.clone());
                    queue
                };
                subscription = Some(Subscription { durable: durable || shared });
                let topic_name = std::mem::replace(&mut address, queue);
                self.broker
                    .queues
                    .declare(&address)
                    .map_err(|error| error.to_string())
                    .and_then(|_| {
                        self.broker
                            .bind(TOPIC_EXCHANGE, &address, &topic_name)
                            .then_some(())
                            .ok_or_else(|| "the topic exchange is missing".to_owned())
                    })
            }
            (Ok(_), true) if address.is_empty() && !dynamic && !link_name.is_empty() => {
                let queue = subscription_queue(&self.container_id, &link_name, shared);
                if self.broker.queues.exists(&queue).unwrap_or(false) {
                    subscription = Some(Subscription { durable: true });
                    reply_source = Some(Value::described(SOURCE, vec![Value::String(queue.clone())]));
                    address = queue;
                    Ok(())
                } else {
                    Err(format!("there is no subscription named {link_name}"))
                }
            }
            _ if address.is_empty() => Err("the link has no queue address".to_owned()),
            _ => self
                .broker
                .queues
                .declare(&address)
                .map_err(|error| error.to_string())
                .map(|_| ()),
        };
        let selector = selector.unwrap_or(None);
        let snd_settle_mode = body.field(3).unsigned().unwrap_or(2);
        let reply = vec![
            body.field(0).clone(),
            Value::Uint(handle),
            Value::Bool(!client_receives),
            Value::Ubyte(u8::try_from(snd_settle_mode).unwrap_or(2)),
            Value::Ubyte(0),
            match (declared.is_ok(), client_receives) {
                (true, true) if reply_source.is_some() => reply_source.clone().unwrap_or(Value::Null),
                (true, true) if dynamic => with_address(body.field(5), &address),
                (true, _) | (false, true) => body.field(5).clone(),
                (false, false) => Value::Null,
            },
            match (declared.is_ok(), client_receives) {
                (true, false) if dynamic => with_address(body.field(6), &address),
                (true, _) | (false, false) => body.field(6).clone(),
                (false, true) => Value::Null,
            },
            Value::Null,
            Value::Bool(false),
            if client_receives {
                Value::Uint(0)
            } else {
                Value::Null
            },
        ];
        self.send(FRAME_AMQP, channel, &performative(ATTACH, reply));
        if let Err(description) = declared {
            self.send(
                FRAME_AMQP,
                channel,
                &performative(
                    DETACH,
                    vec![
                        Value::Uint(handle),
                        Value::Bool(true),
                        error("amqp:invalid-field", &description),
                    ],
                ),
            );
            return Ok(());
        }
        let link = if client_receives {
            Link {
                queue: address,
                route: Route::Queue,
                direction: Direction::ToClient {
                    presettled: snd_settle_mode == 1,
                    drain: false,
                },
                credit: 0,
                delivery_count: 0,
                incoming: None,
                selector,
                subscription,
            }
        } else {
            let delivery_count = body
                .field(9)
                .unsigned()
                .and_then(|count| u32::try_from(count).ok())
                .unwrap_or(0);
            Link {
                queue: address,
                route,
                direction: Direction::FromClient,
                credit: LINK_CREDIT,
                delivery_count,
                incoming: None,
                selector: None,
                subscription: None,
            }
        };
        let session = self.session(channel)?;
        let granted = matches!(link.direction, Direction::FromClient);
        session.links.insert(handle, link);
        if granted {
            let session = &self.sessions[&channel];
            let flow = self.link_flow(session, handle, &session.links[&handle], false);
            self.send(FRAME_AMQP, channel, &flow);
        }
        Ok(())
    }

    fn flow(&mut self, channel: u16, body: &Value) -> Result<(), String> {
        let echo = body.field(9).flag().unwrap_or(false);
        let session = self.session(channel)?;
        let Some(handle) = body
            .field(4)
            .unsigned()
            .and_then(|handle| u32::try_from(handle).ok())
        else {
            return Ok(());
        };
        let Some(link) = session.links.get_mut(&handle) else {
            return Err(format!("flow names handle {handle}, which is not attached"));
        };
        if let Direction::ToClient { drain, .. } = &mut link.direction {
            let receiver_count = body
                .field(5)
                .unsigned()
                .and_then(|count| u32::try_from(count).ok())
                .unwrap_or(0);
            let credit = body
                .field(6)
                .unsigned()
                .and_then(|credit| u32::try_from(credit).ok())
                .unwrap_or(0);
            link.credit = receiver_count
                .wrapping_add(credit)
                .wrapping_sub(link.delivery_count)
                .min(1 << 20);
            *drain = body.field(8).flag().unwrap_or(false);
        }
        if echo {
            let session = &self.sessions[&channel];
            let flow = self.link_flow(session, handle, &session.links[&handle], false);
            self.send(FRAME_AMQP, channel, &flow);
        }
        Ok(())
    }

    fn transfer(&mut self, channel: u16, body: &Value, payload: &[u8]) -> Result<(), String> {
        let handle = self.handle_of(body)?;
        let unfinished = self
            .unfinished_delivery_bytes()
            .saturating_add(payload.len());
        if !self.unfinished_held.hold_if_large(unfinished) {
            self.close_with(RESOURCE_LIMIT_EXCEEDED, OUT_OF_MEMORY_BUDGET);
            return Ok(());
        }
        let session = self.session(channel)?;
        session.next_incoming_id = session.next_incoming_id.wrapping_add(1);
        let link = session
            .links
            .get_mut(&handle)
            .ok_or_else(|| format!("transfer names handle {handle}, which is not attached"))?;
        if !matches!(link.direction, Direction::FromClient) {
            return Err("the client sent a transfer on a link it receives from".to_owned());
        }
        let incoming = match link.incoming.take() {
            Some(incoming) => incoming,
            None => {
                let delivery_id = body
                    .field(1)
                    .unsigned()
                    .and_then(|id| u32::try_from(id).ok())
                    .ok_or("the first transfer of a delivery has no delivery-id")?;
                link.credit = link.credit.saturating_sub(1);
                link.delivery_count = link.delivery_count.wrapping_add(1);
                Incoming {
                    delivery_id,
                    settled: false,
                    bytes: Vec::new(),
                    transaction: None,
                }
            }
        };
        let mut incoming = incoming;
        incoming.settled |= body.field(4).flag().unwrap_or(false);
        incoming.transaction = incoming
            .transaction
            .or_else(|| transaction_of(body.field(TRANSFER_STATE_FIELD)).map(|(id, _)| id));
        if body.field(9).flag().unwrap_or(false) {
            return Ok(());
        }
        if incoming.bytes.len() + payload.len() > MAX_MESSAGE {
            return Err("the message is larger than 16 MiB".to_owned());
        }
        incoming.bytes.extend_from_slice(payload);
        if body.field(5).flag().unwrap_or(false) {
            link.incoming = Some(incoming);
            return Ok(());
        }
        let queue = link.queue.clone();
        let route = link.route.clone();
        let top_up = link.credit < LINK_CREDIT / 2;
        if top_up {
            link.credit = LINK_CREDIT;
        }
        let state = if route == Route::Coordinator {
            self.coordinate(&incoming.bytes)
        } else {
            match self.store(&route, &queue, &incoming.bytes, incoming.transaction) {
                Ok(()) => {
                    let accepted = Value::described(ACCEPTED, Vec::new());
                    match incoming.transaction {
                        Some(id) => Value::described(
                            TRANSACTIONAL_STATE,
                            vec![Value::Binary(id.to_be_bytes().to_vec()), accepted],
                        ),
                        None => accepted,
                    }
                }
                Err((condition, description)) => {
                    Value::described(REJECTED, vec![error(condition, &description)])
                }
            }
        };
        if !incoming.settled {
            let disposition = performative(
                DISPOSITION,
                vec![
                    Value::Bool(true),
                    Value::Uint(incoming.delivery_id),
                    Value::Null,
                    Value::Bool(true),
                    state,
                ],
            );
            self.send(FRAME_AMQP, channel, &disposition);
        }
        if top_up {
            let session = &self.sessions[&channel];
            let flow = self.link_flow(session, handle, &session.links[&handle], false);
            self.send(FRAME_AMQP, channel, &flow);
        }
        Ok(())
    }

    fn attach_coordinator(&mut self, channel: u16, handle: u32, body: &Value) -> Result<(), String> {
        let reply = vec![
            body.field(0).clone(),
            Value::Uint(handle),
            Value::Bool(true),
            Value::Ubyte(u8::try_from(body.field(3).unsigned().unwrap_or(2)).unwrap_or(2)),
            Value::Ubyte(0),
            body.field(5).clone(),
            body.field(6).clone(),
        ];
        self.send(FRAME_AMQP, channel, &performative(ATTACH, reply));
        let delivery_count = body
            .field(9)
            .unsigned()
            .and_then(|count| u32::try_from(count).ok())
            .unwrap_or(0);
        let session = self.session(channel)?;
        session.links.insert(
            handle,
            Link {
                queue: String::new(),
                route: Route::Coordinator,
                direction: Direction::FromClient,
                credit: LINK_CREDIT,
                delivery_count,
                incoming: None,
                selector: None,
                subscription: None,
            },
        );
        let session = &self.sessions[&channel];
        let flow = self.link_flow(session, handle, &session.links[&handle], false);
        self.send(FRAME_AMQP, channel, &flow);
        Ok(())
    }

    fn store(
        &mut self,
        route: &Route,
        link_queue: &str,
        payload: &[u8],
        transaction: Option<u64>,
    ) -> Result<(), (&'static str, String)> {
        let internal = |description: String| ("amqp:internal-error", description);
        let message = inbound(payload).map_err(|description| ("amqp:decode-error", description))?;
        let (exchange, routing_key) = match route {
            Route::Queue => (String::new(), link_queue.to_owned()),
            Route::Topic(topic) => (TOPIC_EXCHANGE.to_owned(), topic.clone()),
            Route::Anonymous => {
                let to = message
                    .to
                    .clone()
                    .filter(|to| !to.is_empty())
                    .ok_or_else(|| ("amqp:not-found", "a message on the anonymous relay names no to address".to_owned()))?;
                if message.destination_kind == Some(DestinationKind::Topic) {
                    (TOPIC_EXCHANGE.to_owned(), to)
                } else {
                    self.broker.queues.declare(&to).map_err(|error| internal(error.to_string()))?;
                    (String::new(), to)
                }
            }
            Route::Coordinator => return Err(internal("the coordinator takes no messages".to_owned())),
        };
        let queues = self
            .broker
            .route(&exchange, &routing_key, &[])
            .ok_or_else(|| ("amqp:not-found", format!("there is no exchange {exchange}")))?;
        let stored = encode_message(&exchange, &routing_key, &message.properties, &message.body);
        if let Some(id) = transaction {
            let transaction = self
                .transactions
                .get_mut(&id)
                .ok_or_else(|| ("amqp:transaction:unknown-id", format!("transaction {id} is not open")))?;
            if transaction.full(stored.len()) {
                return Err((RESOURCE_LIMIT_EXCEEDED, "the transaction holds too much".to_owned()));
            }
            transaction.bytes = transaction.bytes.saturating_add(stored.len());
            transaction.publishes.push((queues, stored));
            return Ok(());
        }
        let now = now_ms();
        for queue in queues {
            if self.broker.queues.exists(&queue).unwrap_or(false) {
                self.broker
                    .queues
                    .send(&queue, &stored, 0, now)
                    .map_err(|error| internal(error.to_string()))?;
            }
        }
        Ok(())
    }

    fn coordinate(&mut self, payload: &[u8]) -> Value {
        let unknown = |description: &str| {
            Value::described(
                REJECTED,
                vec![error("amqp:transaction:unknown-id", description)],
            )
        };
        let request = decode_all(payload).ok().and_then(|sections| {
            sections.into_iter().find_map(|section| match section {
                Value::Described(descriptor, inner) if descriptor.unsigned() == Some(VALUE_SECTION) => Some(*inner),
                _ => None,
            })
        });
        let Some(request) = request else {
            return unknown("the coordinator expects declare or discharge");
        };
        match request.descriptor() {
            Some(DECLARE) => {
                if self.transactions.len() >= MAX_TRANSACTIONS {
                    return Value::described(
                        REJECTED,
                        vec![error(RESOURCE_LIMIT_EXCEEDED, "this connection holds too many open transactions")],
                    );
                }
                self.next_transaction = self.next_transaction.wrapping_add(1);
                let id = self.next_transaction;
                self.transactions.insert(id, Transaction::default());
                Value::described(DECLARED, vec![Value::Binary(id.to_be_bytes().to_vec())])
            }
            Some(DISCHARGE) => {
                let Some(transaction) = transaction_id(request.field(0)).and_then(|id| self.transactions.remove(&id)) else {
                    return unknown("the transaction is not open");
                };
                let fail = request.field(1).flag().unwrap_or(false);
                self.finish(transaction, !fail);
                Value::described(ACCEPTED, Vec::new())
            }
            _ => unknown("the coordinator expects declare or discharge"),
        }
    }

    fn finish(&self, transaction: Transaction, commit: bool) {
        let now = now_ms();
        let queues = &self.broker.queues;
        if commit {
            for (targets, stored) in &transaction.publishes {
                for queue in targets {
                    if queues.exists(queue).unwrap_or(false) {
                        let _ = queues.send(queue, stored, 0, now);
                    }
                }
            }
        }
        for (queue, id, outcome) in transaction.settles {
            let _ = match (commit, outcome) {
                (false, _) => queues.release(&queue, id, now),
                (true, Outcome::DeadLetter) => queues.dead_letter(&queue, id, now),
                (true, Outcome::Accepted) => queues.ack(&queue, id),
                (true, Outcome::Failed) => queues.release(&queue, id, now),
                (true, Outcome::Untouched) => queues.unlease(&queue, id, now),
            };
        }
    }

    fn disposition(&mut self, channel: u16, body: &Value) -> Result<(), String> {
        if !body.field(0).flag().unwrap_or(false) {
            return Ok(());
        }
        let first = body
            .field(1)
            .unsigned()
            .and_then(|id| u32::try_from(id).ok())
            .ok_or("disposition has no first delivery-id")?;
        let last = body
            .field(2)
            .unsigned()
            .and_then(|id| u32::try_from(id).ok())
            .unwrap_or(first);
        if last.wrapping_sub(first) > MAX_RANGE {
            return Err("disposition covers too many deliveries".to_owned());
        }
        let settled = body.field(3).flag().unwrap_or(false);
        let state = body.field(4).clone();
        let transaction = transaction_of(&state).map(|(id, _)| id);
        let outcome = match transaction_of(&state) {
            Some((_, inner)) => Outcome::of(inner),
            None => Outcome::of(&state),
        };
        let session = self.session(channel)?;
        let mut done = Vec::new();
        let mut id = first;
        loop {
            if let Some(unsettled) = session.unsettled.remove(&id) {
                done.push(unsettled);
            }
            if id == last {
                break;
            }
            id = id.wrapping_add(1);
        }
        if let Some(id) = transaction
            && let Some(open) = self.transactions.get_mut(&id)
        {
            for unsettled in done {
                if open.full(0) {
                    let _ = self.broker.queues.release(&unsettled.queue, unsettled.id, now_ms());
                    continue;
                }
                open.settles.push((unsettled.queue, unsettled.id, outcome));
            }
            done = Vec::new();
        }
        let now = now_ms();
        for unsettled in done {
            let queues = &self.broker.queues;
            let _ = match outcome {
                Outcome::DeadLetter => queues.dead_letter(&unsettled.queue, unsettled.id, now),
                Outcome::Accepted => queues.ack(&unsettled.queue, unsettled.id),
                Outcome::Failed => queues.release(&unsettled.queue, unsettled.id, now),
                Outcome::Untouched => queues.unlease(&unsettled.queue, unsettled.id, now),
            };
        }
        if !settled {
            let reply = performative(
                DISPOSITION,
                vec![
                    Value::Bool(false),
                    Value::Uint(first),
                    Value::Uint(last),
                    Value::Bool(true),
                    state,
                ],
            );
            self.send(FRAME_AMQP, channel, &reply);
        }
        Ok(())
    }

    fn release_all(&self, unsettled: BTreeMap<u32, Unsettled>) {
        let now = now_ms();
        for (_, unsettled) in unsettled {
            let _ = self
                .broker
                .queues
                .release(&unsettled.queue, unsettled.id, now);
        }
    }

    fn detach(&mut self, channel: u16, handle: u32, closed: bool) {
        let Some(session) = self.sessions.get_mut(&channel) else {
            return;
        };
        let removed = session.links.remove(&handle);
        let ids: Vec<u32> = session
            .unsettled
            .iter()
            .filter(|(_, unsettled)| unsettled.handle == handle)
            .map(|(id, _)| *id)
            .collect();
        let released: BTreeMap<u32, Unsettled> = ids
            .into_iter()
            .filter_map(|id| {
                session
                    .unsettled
                    .remove(&id)
                    .map(|unsettled| (id, unsettled))
            })
            .collect();
        self.release_all(released);
        if let Some(Link {
            queue,
            subscription: Some(Subscription { durable }),
            ..
        }) = removed
            && (!durable || closed)
        {
            self.dynamic_queues.retain(|dynamic| *dynamic != queue);
            self.broker.delete_queue(&queue);
        }
    }

    fn send_transfer(
        &mut self,
        channel: u16,
        handle: u32,
        delivery_id: u32,
        tag: u64,
        presettled: bool,
        payload: &[u8],
    ) {
        let chunk = usize::try_from(self.remote_max_frame)
            .unwrap_or(usize::MAX)
            .saturating_sub(256)
            .max(256);
        let pieces: Vec<&[u8]> = if payload.is_empty() {
            vec![payload]
        } else {
            payload.chunks(chunk).collect()
        };
        let count = pieces.len();
        for (index, piece) in pieces.into_iter().enumerate() {
            let mut body = performative(
                TRANSFER,
                vec![
                    Value::Uint(handle),
                    Value::Uint(delivery_id),
                    Value::Binary(tag.to_be_bytes().to_vec()),
                    Value::Uint(0),
                    Value::Bool(presettled),
                    Value::Bool(index + 1 < count),
                ],
            );
            body.extend_from_slice(piece);
            self.send(FRAME_AMQP, channel, &body);
        }
    }

    fn pump(&mut self) {
        let mut wanted = Vec::new();
        for (&channel, session) in &self.sessions {
            for (&handle, link) in &session.links {
                if let Direction::ToClient { presettled, drain } = link.direction
                    && (link.credit > 0 || drain)
                {
                    wanted.push((
                        channel,
                        handle,
                        link.queue.clone(),
                        link.credit.min(MAX_PER_PUMP),
                        presettled,
                        drain,
                        link.selector.clone(),
                        link.subscription.is_some(),
                    ));
                }
            }
        }
        if wanted.is_empty() {
            return;
        }
        let start = self.next_link % wanted.len();
        wanted.rotate_left(start);
        let mut delivered_this_round = false;
        for (position, (channel, handle, queue, room, presettled, drain, selector, owns_queue)) in
            wanted.into_iter().enumerate()
        {
            let scan = if selector.is_some() {
                SELECTOR_SCAN
            } else {
                room as usize
            };
            let mut refused = false;
            let messages = if room > 0 {
                let broker = self.broker;
                let max_frame = self.remote_max_frame;
                let mut held_bytes = self.outgoing.len();
                let held = &mut self.outgoing_held;
                broker
                    .queues
                    .receive_fitting(
                        &queue,
                        scan,
                        LEASE_MS,
                        now_ms(),
                        &mut |body_bytes: usize| {
                            let grown =
                                held_bytes.saturating_add(delivery_bytes(body_bytes, max_frame));
                            if held.hold_if_large(grown) {
                                held_bytes = grown;
                                true
                            } else {
                                refused = true;
                                false
                            }
                        },
                    )
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            if refused && messages.is_empty() && !delivered_this_round {
                self.close_with(RESOURCE_LIMIT_EXCEEDED, OUT_OF_MEMORY_BUDGET);
                return;
            }
            let mut sent = Vec::new();
            let mut passed_over = Vec::new();
            let mut unsent = messages.iter();
            for message in unsent.by_ref() {
                if sent.len() >= room as usize {
                    passed_over.push(message.id);
                    continue;
                }
                let stored = StoredMessage::read(&queue, &message.body);
                if let Some(selector) = &selector {
                    let properties = BasicProperties::decode(stored.properties);
                    if !selector.chooses(&|name| selector_field(&properties, name)) {
                        if owns_queue {
                            let _ = self.broker.queues.ack(&queue, message.id);
                        } else {
                            passed_over.push(message.id);
                        }
                        continue;
                    }
                }
                let Some(session) = self.sessions.get_mut(&channel) else {
                    let _ = self.broker.queues.unlease(&queue, message.id, now_ms());
                    break;
                };
                let delivery_id = session.next_outgoing_id;
                session.next_outgoing_id = session.next_outgoing_id.wrapping_add(1);
                if let Some(link) = session.links.get_mut(&handle) {
                    link.credit = link.credit.saturating_sub(1);
                    link.delivery_count = link.delivery_count.wrapping_add(1);
                }
                if !presettled {
                    session.unsettled.insert(
                        delivery_id,
                        Unsettled {
                            handle,
                            queue: queue.clone(),
                            id: message.id,
                        },
                    );
                }
                let payload = transfer_payload(
                    &queue,
                    message.id,
                    message.receive_count,
                    stored.properties,
                    stored.body,
                );
                self.send_transfer(
                    channel,
                    handle,
                    delivery_id,
                    message.id,
                    presettled,
                    &payload,
                );
                sent.push(message.id);
            }
            let now = now_ms();
            for message in unsent {
                let _ = self.broker.queues.unlease(&queue, message.id, now);
            }
            for id in passed_over {
                let _ = self.broker.queues.unlease(&queue, id, now);
            }
            let delivered_any = !sent.is_empty();
            delivered_this_round |= delivered_any;
            if presettled {
                for id in sent {
                    let _ = self.broker.queues.ack(&queue, id);
                }
            }
            if refused {
                self.next_link = start
                    .saturating_add(position)
                    .saturating_add(usize::from(delivered_any));
                return;
            }
            if drain && sent.len() < room as usize {
                let Some(session) = self.sessions.get_mut(&channel) else {
                    continue;
                };
                let Some(link) = session.links.get_mut(&handle) else {
                    continue;
                };
                link.delivery_count = link.delivery_count.wrapping_add(link.credit);
                link.credit = 0;
                if let Direction::ToClient { drain, .. } = &mut link.direction {
                    *drain = false;
                }
                let session = &self.sessions[&channel];
                let flow = self.link_flow(session, handle, &session.links[&handle], true);
                self.send(FRAME_AMQP, channel, &flow);
            }
        }
    }
}

fn take_frame(buffer: &mut Vec<u8>) -> Result<Option<(u8, u16, Vec<u8>)>, String> {
    let Some(header) = buffer.get(..8) else {
        return Ok(None);
    };
    let size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let offset = usize::from(header[4]) * 4;
    let (kind, channel) = (header[5], u16::from_be_bytes([header[6], header[7]]));
    if !(8..=MAX_FRAME).contains(&size) || offset < 8 || offset > size as usize {
        return Err(format!(
            "a frame of {size} bytes with data offset {offset} is not allowed"
        ));
    }
    let size = size as usize;
    if buffer.len() < size {
        return Ok(None);
    }
    let body = buffer[offset..size].to_vec();
    buffer.drain(..size);
    Ok(Some((kind, channel, body)))
}

pub(crate) fn serve_stream(
    broker: &Broker<'_>,
    config: &ServerConfig,
    mut reader: impl Read,
    writer: &mut impl Write,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    writer.write_all(SASL_HEADER)?;
    writer.write_all(&frame(
        FRAME_SASL,
        0,
        &performative(
            SASL_MECHANISMS,
            vec![Value::Array(vec![Value::Symbol("PLAIN".to_owned())])],
        ),
    ))?;
    let mut connection = Connection {
        broker,
        config,
        stage: Stage::Sasl,
        remote_max_frame: MIN_FRAME,
        remote_idle: None,
        sessions: BTreeMap::new(),
        outgoing: Vec::new(),
        unfinished_held: config.memory_budget.empty_reservation(),
        outgoing_held: config.memory_budget.empty_reservation(),
        next_link: 0,
        dynamic_queues: Vec::new(),
        container_id: String::new(),
        transactions: BTreeMap::new(),
        next_transaction: 0,
    };
    let mut buffer = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut last_sent = Instant::now();
    let mut last_received = Instant::now();
    let outcome = loop {
        if stop() {
            break Ok(());
        }
        if connection.stage == Stage::Open && last_received.elapsed() > LOCAL_IDLE_TIMEOUT {
            connection.close_with("amqp:resource-limit-exceeded", "local-idle-timeout expired");
            if !connection.outgoing.is_empty() {
                let _ = writer.write_all(&connection.outgoing);
            }
            break Ok(());
        }
        match reader.read(&mut chunk) {
            Ok(0) => break Ok(()),
            Ok(read) => {
                last_received = Instant::now();
                buffer.extend_from_slice(&chunk[..read]);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => break Err(error),
        }
        while connection.stage != Stage::Closed {
            if connection.stage == Stage::AwaitHeader {
                let Some(header) = buffer.get(..8) else {
                    break;
                };
                if header != AMQP_HEADER {
                    connection.outgoing.extend_from_slice(AMQP_HEADER);
                    connection.stage = Stage::Closed;
                    break;
                }
                buffer.drain(..8);
                connection.outgoing.extend_from_slice(AMQP_HEADER);
                connection.stage = Stage::AwaitOpen;
                continue;
            }
            let (kind, channel, body) = match take_frame(&mut buffer) {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(description) => {
                    connection.close_with("amqp:connection:framing-error", &description);
                    break;
                }
            };
            if body.is_empty() {
                continue;
            }
            let (performative, used) = match decode_first(&body) {
                Ok(decoded) => decoded,
                Err(description) => {
                    connection.close_with("amqp:decode-error", &description);
                    break;
                }
            };
            match (&connection.stage, kind) {
                (Stage::Sasl, FRAME_SASL) => connection.sasl(&performative),
                (Stage::AwaitOpen, FRAME_AMQP) => connection.open(&performative),
                (Stage::Open, FRAME_AMQP) => {
                    if let Err(description) =
                        connection.handle(channel, &performative, &body[used..])
                    {
                        connection.close_with("amqp:connection:framing-error", &description);
                    }
                    connection.release_finished_deliveries();
                }
                _ => connection.close_with(
                    "amqp:connection:framing-error",
                    "the frame does not fit the connection's stage",
                ),
            }
        }
        if connection.stage == Stage::Open {
            connection.pump();
            if let Some(idle) = connection.remote_idle
                && connection.outgoing.is_empty()
                && last_sent.elapsed() >= idle / 2
            {
                connection.send(FRAME_AMQP, 0, &[]);
            }
        }
        if !connection.outgoing.is_empty() {
            writer.write_all(&connection.outgoing)?;
            connection.written();
            last_sent = Instant::now();
        }
        if connection.stage == Stage::Closed {
            break Ok(());
        }
    };
    let sessions = std::mem::take(&mut connection.sessions);
    for (_, session) in sessions {
        connection.release_all(session.unsettled);
    }
    for (_, transaction) in std::mem::take(&mut connection.transactions) {
        connection.finish(transaction, false);
    }
    for queue in std::mem::take(&mut connection.dynamic_queues) {
        connection.broker.delete_queue(&queue);
    }
    outcome
}

#[cfg(test)]
mod outcome_tests {
    use super::{ACCEPTED, MODIFIED, Outcome, REJECTED, RELEASED};
    use crate::amqp1::value::Value;

    #[test]
    fn each_delivery_state_settles_its_message_as_the_specification_says() {
        let state = |code: u64, fields: Vec<Value>| Outcome::of(&Value::described(code, fields));
        assert_eq!(state(ACCEPTED, Vec::new()), Outcome::Accepted);
        assert_eq!(state(REJECTED, Vec::new()), Outcome::DeadLetter);
        assert_eq!(state(RELEASED, Vec::new()), Outcome::Untouched);
        assert_eq!(state(MODIFIED, Vec::new()), Outcome::Untouched);
        assert_eq!(state(MODIFIED, vec![Value::Bool(true)]), Outcome::Failed);
        assert_eq!(
            state(MODIFIED, vec![Value::Bool(true), Value::Bool(true)]),
            Outcome::DeadLetter
        );
        assert_eq!(Outcome::of(&Value::Null), Outcome::Accepted);
    }
}
