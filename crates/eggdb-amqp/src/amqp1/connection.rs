use super::value::{Value, decode_all, decode_first, encode};
use crate::server::{Broker, ServerConfig};
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
const HEADER_SECTION: u64 = 0x70;
const PROPERTIES_SECTION: u64 = 0x73;
const DATA_SECTION: u64 = 0x75;
const VALUE_SECTION: u64 = 0x77;

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

fn frame(kind: u8, channel: u16, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&u32::try_from(8 + body.len()).unwrap_or(u32::MAX).to_be_bytes());
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
    Value::described(ERROR, vec![Value::Symbol(condition.to_owned()), Value::String(description.to_owned())])
}

struct Incoming {
    delivery_id: u32,
    settled: bool,
    bytes: Vec<u8>,
}

enum Direction {
    FromClient,
    ToClient { presettled: bool, drain: bool },
}

struct Link {
    queue: String,
    direction: Direction,
    credit: u32,
    delivery_count: u32,
    incoming: Option<Incoming>,
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
}

fn message_body(payload: &[u8]) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    for section in decode_all(payload)? {
        match (section.descriptor(), &section) {
            (Some(DATA_SECTION), Value::Described(_, inner)) => {
                if let Value::Binary(bytes) = inner.as_ref() {
                    body.extend_from_slice(bytes);
                }
            }
            (Some(VALUE_SECTION), Value::Described(_, inner)) => match inner.as_ref() {
                Value::String(text) => body.extend_from_slice(text.as_bytes()),
                Value::Binary(bytes) => body.extend_from_slice(bytes),
                other => encode(other, &mut body),
            },
            _ => {}
        }
    }
    Ok(body)
}

fn outgoing_payload(queue: &str, message: &eggdb_queues::Message) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.body.len() + 64);
    let header = Value::described(
        HEADER_SECTION,
        vec![Value::Bool(true), Value::Null, Value::Null, Value::Bool(message.receive_count <= 1), Value::Uint(message.receive_count.saturating_sub(1))],
    );
    encode(&header, &mut out);
    encode(&Value::described(PROPERTIES_SECTION, vec![Value::String(format!("{queue}-{}", message.id))]), &mut out);
    encode(&Value::Described(Box::new(Value::Ulong(DATA_SECTION)), Box::new(Value::Binary(message.body.clone()))), &mut out);
    out
}

impl Connection<'_, '_> {
    fn send(&mut self, kind: u8, channel: u16, body: &[u8]) {
        self.outgoing.extend_from_slice(&frame(kind, channel, body));
    }

    fn close_with(&mut self, condition: &str, description: &str) {
        self.send(FRAME_AMQP, 0, &performative(CLOSE, vec![error(condition, description)]));
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
        let (_, user, password) = (parts.next(), parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
        let authorized = mechanism == "PLAIN"
            && eggdb_crypto::constant_time_eq(user, self.config.user.as_bytes())
            && eggdb_crypto::constant_time_eq(&eggdb_crypto::sha256(password), &self.config.password_sha256);
        self.send(FRAME_SASL, 0, &performative(SASL_OUTCOME, vec![Value::Ubyte(if authorized { 0 } else { 1 })]));
        self.stage = if authorized { Stage::AwaitHeader } else { Stage::Closed };
    }

    fn open(&mut self, body: &Value) {
        if body.descriptor() != Some(OPEN) {
            self.close_with("amqp:connection:framing-error", "the first frame must be open");
            return;
        }
        let max_frame = body.field(2).unsigned().and_then(|size| u32::try_from(size).ok()).unwrap_or(u32::MAX);
        self.remote_max_frame = max_frame.clamp(MIN_FRAME, 1 << 20);
        self.remote_idle = body.field(4).unsigned().filter(|ms| *ms > 0).map(Duration::from_millis);
        self.send(
            FRAME_AMQP,
            0,
            &performative(OPEN, vec![Value::String("eggdb".to_owned()), Value::Null, Value::Uint(MAX_FRAME), Value::Ushort(CHANNEL_MAX)]),
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
                self.detach(channel, handle);
                self.send(FRAME_AMQP, channel, &performative(DETACH, vec![Value::Uint(handle), Value::Bool(true)]));
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
        body.field(0).unsigned().and_then(|handle| u32::try_from(handle).ok()).ok_or_else(|| "the frame has no link handle".to_owned())
    }

    fn session(&mut self, channel: u16) -> Result<&mut Session, String> {
        self.sessions.get_mut(&channel).ok_or_else(|| format!("channel {channel} has no session"))
    }

    fn begin(&mut self, channel: u16, body: &Value) -> Result<(), String> {
        if channel > CHANNEL_MAX || self.sessions.contains_key(&channel) {
            return Err(format!("channel {channel} cannot begin a session"));
        }
        let next_incoming_id = body.field(1).unsigned().and_then(|id| u32::try_from(id).ok()).unwrap_or(0);
        self.sessions.insert(channel, Session { next_outgoing_id: 0, next_incoming_id, links: BTreeMap::new(), unsettled: BTreeMap::new() });
        self.send(
            FRAME_AMQP,
            channel,
            &performative(BEGIN, vec![Value::Ushort(channel), Value::Uint(0), Value::Uint(WINDOW), Value::Uint(WINDOW), Value::Uint(HANDLE_MAX)]),
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
        let handle = body.field(1).unsigned().and_then(|handle| u32::try_from(handle).ok()).filter(|handle| *handle <= HANDLE_MAX).ok_or("attach has no valid handle")?;
        let client_receives = body.field(2).flag().ok_or("attach has no role")?;
        if self.session(channel)?.links.contains_key(&handle) {
            return Err(format!("handle {handle} is already attached"));
        }
        let terminus = if client_receives { body.field(5) } else { body.field(6) };
        let address = terminus.field(0).text().unwrap_or_default().trim_start_matches("/queues/").trim_start_matches("/queue/").to_owned();
        let declared = if address.is_empty() { Err("the link has no queue address".to_owned()) } else { self.broker.queues.declare(&address).map_err(|error| error.to_string()) };
        let snd_settle_mode = body.field(3).unsigned().unwrap_or(2);
        let reply = vec![
            body.field(0).clone(),
            Value::Uint(handle),
            Value::Bool(!client_receives),
            Value::Ubyte(u8::try_from(snd_settle_mode).unwrap_or(2)),
            Value::Ubyte(0),
            if declared.is_ok() || client_receives { body.field(5).clone() } else { Value::Null },
            if declared.is_ok() || !client_receives { body.field(6).clone() } else { Value::Null },
            Value::Null,
            Value::Bool(false),
            if client_receives { Value::Uint(0) } else { Value::Null },
        ];
        self.send(FRAME_AMQP, channel, &performative(ATTACH, reply));
        if let Err(description) = declared {
            self.send(FRAME_AMQP, channel, &performative(DETACH, vec![Value::Uint(handle), Value::Bool(true), error("amqp:invalid-field", &description)]));
            return Ok(());
        }
        let link = if client_receives {
            Link { queue: address, direction: Direction::ToClient { presettled: snd_settle_mode == 1, drain: false }, credit: 0, delivery_count: 0, incoming: None }
        } else {
            let delivery_count = body.field(9).unsigned().and_then(|count| u32::try_from(count).ok()).unwrap_or(0);
            Link { queue: address, direction: Direction::FromClient, credit: LINK_CREDIT, delivery_count, incoming: None }
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
        let Some(handle) = body.field(4).unsigned().and_then(|handle| u32::try_from(handle).ok()) else {
            return Ok(());
        };
        let Some(link) = session.links.get_mut(&handle) else {
            return Err(format!("flow names handle {handle}, which is not attached"));
        };
        if let Direction::ToClient { drain, .. } = &mut link.direction {
            let receiver_count = body.field(5).unsigned().and_then(|count| u32::try_from(count).ok()).unwrap_or(0);
            let credit = body.field(6).unsigned().and_then(|credit| u32::try_from(credit).ok()).unwrap_or(0);
            link.credit = receiver_count.wrapping_add(credit).wrapping_sub(link.delivery_count).min(1 << 20);
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
        let session = self.session(channel)?;
        session.next_incoming_id = session.next_incoming_id.wrapping_add(1);
        let link = session.links.get_mut(&handle).ok_or_else(|| format!("transfer names handle {handle}, which is not attached"))?;
        if !matches!(link.direction, Direction::FromClient) {
            return Err("the client sent a transfer on a link it receives from".to_owned());
        }
        let incoming = match link.incoming.take() {
            Some(incoming) => incoming,
            None => {
                let delivery_id = body.field(1).unsigned().and_then(|id| u32::try_from(id).ok()).ok_or("the first transfer of a delivery has no delivery-id")?;
                link.credit = link.credit.saturating_sub(1);
                link.delivery_count = link.delivery_count.wrapping_add(1);
                Incoming { delivery_id, settled: false, bytes: Vec::new() }
            }
        };
        let mut incoming = incoming;
        incoming.settled |= body.field(4).flag().unwrap_or(false);
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
        let top_up = link.credit < LINK_CREDIT / 2;
        if top_up {
            link.credit = LINK_CREDIT;
        }
        let stored = message_body(&incoming.bytes).and_then(|body| self.broker.queues.send(&queue, &body, 0, now_ms()).map_err(|error| error.to_string()));
        if !incoming.settled {
            let state = match &stored {
                Ok(_) => Value::described(ACCEPTED, Vec::new()),
                Err(description) => Value::described(REJECTED, vec![error("amqp:internal-error", description)]),
            };
            let disposition = performative(DISPOSITION, vec![Value::Bool(true), Value::Uint(incoming.delivery_id), Value::Null, Value::Bool(true), state]);
            self.send(FRAME_AMQP, channel, &disposition);
        }
        if top_up {
            let session = &self.sessions[&channel];
            let flow = self.link_flow(session, handle, &session.links[&handle], false);
            self.send(FRAME_AMQP, channel, &flow);
        }
        Ok(())
    }

    fn disposition(&mut self, channel: u16, body: &Value) -> Result<(), String> {
        if !body.field(0).flag().unwrap_or(false) {
            return Ok(());
        }
        let first = body.field(1).unsigned().and_then(|id| u32::try_from(id).ok()).ok_or("disposition has no first delivery-id")?;
        let last = body.field(2).unsigned().and_then(|id| u32::try_from(id).ok()).unwrap_or(first);
        if last.wrapping_sub(first) > MAX_RANGE {
            return Err("disposition covers too many deliveries".to_owned());
        }
        let settled = body.field(3).flag().unwrap_or(false);
        let state = body.field(4).clone();
        let accepted = !matches!(state.descriptor(), Some(RELEASED | MODIFIED));
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
        let now = now_ms();
        for unsettled in done {
            let _ = if accepted { self.broker.queues.ack(&unsettled.queue, unsettled.id) } else { self.broker.queues.release(&unsettled.queue, unsettled.id, now) };
        }
        if !settled {
            let reply = performative(DISPOSITION, vec![Value::Bool(false), Value::Uint(first), Value::Uint(last), Value::Bool(true), state]);
            self.send(FRAME_AMQP, channel, &reply);
        }
        Ok(())
    }

    fn release_all(&self, unsettled: BTreeMap<u32, Unsettled>) {
        let now = now_ms();
        for (_, unsettled) in unsettled {
            let _ = self.broker.queues.release(&unsettled.queue, unsettled.id, now);
        }
    }

    fn detach(&mut self, channel: u16, handle: u32) {
        let Some(session) = self.sessions.get_mut(&channel) else {
            return;
        };
        session.links.remove(&handle);
        let ids: Vec<u32> = session.unsettled.iter().filter(|(_, unsettled)| unsettled.handle == handle).map(|(id, _)| *id).collect();
        let released: BTreeMap<u32, Unsettled> = ids.into_iter().filter_map(|id| session.unsettled.remove(&id).map(|unsettled| (id, unsettled))).collect();
        self.release_all(released);
    }

    fn send_transfer(&mut self, channel: u16, handle: u32, delivery_id: u32, tag: u64, presettled: bool, payload: &[u8]) {
        let chunk = usize::try_from(self.remote_max_frame).unwrap_or(usize::MAX).saturating_sub(256).max(256);
        let pieces: Vec<&[u8]> = if payload.is_empty() { vec![payload] } else { payload.chunks(chunk).collect() };
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
                    wanted.push((channel, handle, link.queue.clone(), link.credit.min(MAX_PER_PUMP), presettled, drain));
                }
            }
        }
        for (channel, handle, queue, room, presettled, drain) in wanted {
            let messages = if room > 0 { self.broker.queues.receive(&queue, room as usize, LEASE_MS, now_ms()).unwrap_or_default() } else { Vec::new() };
            let mut sent = Vec::new();
            for message in &messages {
                let Some(session) = self.sessions.get_mut(&channel) else {
                    break;
                };
                let delivery_id = session.next_outgoing_id;
                session.next_outgoing_id = session.next_outgoing_id.wrapping_add(1);
                if let Some(link) = session.links.get_mut(&handle) {
                    link.credit = link.credit.saturating_sub(1);
                    link.delivery_count = link.delivery_count.wrapping_add(1);
                }
                if !presettled {
                    session.unsettled.insert(delivery_id, Unsettled { handle, queue: queue.clone(), id: message.id });
                }
                let payload = outgoing_payload(&queue, message);
                self.send_transfer(channel, handle, delivery_id, message.id, presettled, &payload);
                sent.push(message.id);
            }
            if presettled {
                for id in sent {
                    let _ = self.broker.queues.ack(&queue, id);
                }
            }
            if drain && messages.len() < room as usize {
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
    if size > MAX_FRAME || size < 8 || offset < 8 || offset > size as usize {
        return Err(format!("a frame of {size} bytes with data offset {offset} is not allowed"));
    }
    let size = size as usize;
    if buffer.len() < size {
        return Ok(None);
    }
    let body = buffer[offset..size].to_vec();
    buffer.drain(..size);
    Ok(Some((kind, channel, body)))
}

pub(crate) fn serve_stream(broker: &Broker<'_>, config: &ServerConfig, mut reader: impl Read, writer: &mut impl Write, stop: &(dyn Fn() -> bool + Sync)) -> io::Result<()> {
    writer.write_all(SASL_HEADER)?;
    writer.write_all(&frame(FRAME_SASL, 0, &performative(SASL_MECHANISMS, vec![Value::Array(vec![Value::Symbol("PLAIN".to_owned())])])))?;
    let mut connection = Connection { broker, config, stage: Stage::Sasl, remote_max_frame: MIN_FRAME, remote_idle: None, sessions: BTreeMap::new(), outgoing: Vec::new() };
    let mut buffer = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut last_sent = Instant::now();
    let outcome = loop {
        if stop() {
            break Ok(());
        }
        match reader.read(&mut chunk) {
            Ok(0) => break Ok(()),
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => {}
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
                    if let Err(description) = connection.handle(channel, &performative, &body[used..]) {
                        connection.close_with("amqp:connection:framing-error", &description);
                    }
                }
                _ => connection.close_with("amqp:connection:framing-error", "the frame does not fit the connection's stage"),
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
            connection.outgoing.clear();
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
    outcome
}
