use crate::protocol::{subject_matches, valid_subject};
use eggdb_keyspace::{KeysRead, KeysWrite, Keyspace};
use eggdb_sql::json::{Json, Node, parse_document};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

const STREAM: u8 = b'S';
const STREAM_STATE: u8 = b'N';
const MESSAGE: u8 = b'M';
const CONSUMER: u8 = b'C';
const CONSUMER_STATE: u8 = b'P';
const DEDUPLICATION: u8 = b'D';
const SUBJECT_INDEX: u8 = b'L';

const API_PREFIX: &str = "$JS.API.";
const ACK_PREFIX: &str = "$JS.ACK.";
const MOST_STREAMS: usize = 1_000;
const MOST_CONSUMERS: usize = 1_000;
const MOST_BATCH: usize = 10_000;
const MOST_NAME_BYTES: usize = 255;
const MOST_WAITING_PULLS: usize = 10_000;
const PUSH_BATCH: usize = 256;
const DEFAULT_ACK_WAIT_NANOS: i64 = 30_000_000_000;
const DEFAULT_DUPLICATE_WINDOW_NANOS: i64 = 120_000_000_000;
const DEFAULT_MAX_ACK_PENDING: i64 = 1_000;
const LONGEST_PULL_WAIT: Duration = Duration::from_secs(3_600);
const DEFAULT_PULL_WAIT: Duration = Duration::from_secs(30);
const NO_MESSAGES: &[u8] = b"NATS/1.0 404 No Messages\r\n\r\n";
const MESSAGE_NOT_FOUND: &[u8] = b"NATS/1.0 404 Message Not Found\r\n\r\n";
const ROLLUP_NOT_ALLOWED: (u16, u32, &str) = (400, 10_003, "rollups are not allowed on this stream");
const ROLLUP_REFUSED: &str = "rollup refused";

const STREAM_NOT_FOUND: (u16, u32, &str) = (404, 10_059, "stream not found");
const CONSUMER_NOT_FOUND: (u16, u32, &str) = (404, 10_014, "consumer not found");
const STREAM_NAME_IN_USE: (u16, u32, &str) = (400, 10_058, "stream name already in use with a different configuration");
const NO_MESSAGE_FOUND: (u16, u32, &str) = (404, 10_037, "no message found");
const SUBJECTS_OVERLAP: (u16, u32, &str) = (400, 10_065, "subjects overlap with an existing stream");
const BAD_REQUEST: (u16, u32, &str) = (400, 10_003, "bad request");
const NO_STREAM_FOR_SUBJECT: (u16, u32, &str) = (503, 10_076, "no stream matches subject");

/// A message the server sends because of a JetStream request, a store or an acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Outgoing {
    pub(crate) subject: String,
    pub(crate) reply: Option<String>,
    pub(crate) headers: Vec<u8>,
    pub(crate) payload: Vec<u8>,
}

impl Outgoing {
    fn json(subject: &str, payload: String) -> Self {
        Self { subject: subject.to_owned(), reply: None, headers: Vec::new(), payload: payload.into_bytes() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retention {
    Limits,
    Interest,
    WorkQueue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamConfig {
    name: String,
    subjects: Vec<String>,
    retention: Retention,
    max_msgs: i64,
    max_bytes: i64,
    max_age: i64,
    max_msg_size: i64,
    max_msgs_per_subject: i64,
    discard_new: bool,
    duplicate_window: i64,
    memory: bool,
    allow_direct: bool,
    allow_rollup: bool,
    deny_delete: bool,
    deny_purge: bool,
    mirror: Option<Origin>,
    sources: Vec<Origin>,
    created: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Origin {
    name: String,
    filter: Option<String>,
    start_seq: u64,
}

impl Origin {
    fn from_node(node: &Node) -> Result<Self, (u16, u32, &'static str)> {
        let name = text(node, "name").filter(|name| valid_name(name)).ok_or(BAD_REQUEST)?;
        let filter = text(node, "filter_subject").map(str::to_owned);
        if filter.as_deref().is_some_and(|filter| !valid_subject(filter, true)) {
            return Err(BAD_REQUEST);
        }
        Ok(Self {
            name: name.to_owned(),
            filter,
            start_seq: integer(node, "opt_start_seq", 0).max(0) as u64,
        })
    }

    fn json(&self) -> String {
        let mut out = format!("{{\"name\":{}", quote(&self.name));
        if let Some(filter) = &self.filter {
            out.push_str(&format!(",\"filter_subject\":{}", quote(filter)));
        }
        if self.start_seq > 0 {
            out.push_str(&format!(",\"opt_start_seq\":{}", self.start_seq));
        }
        out.push('}');
        out
    }

    fn takes(&self, seq: u64, subject: &str) -> bool {
        seq >= self.start_seq && self.filter.as_deref().is_none_or(|filter| subject_matches(filter, subject))
    }
}

const MOST_SOURCES: usize = 32;
const MOST_SOURCE_HOPS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct StreamState {
    first_seq: u64,
    last_seq: u64,
    messages: u64,
    bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliverPolicy {
    All,
    Last,
    New,
    LastPerSubject,
    ByStartSequence(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AckPolicy {
    Explicit,
    None,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConsumerConfig {
    name: String,
    durable: bool,
    deliver_subject: Option<String>,
    deliver_policy: DeliverPolicy,
    ack_policy: AckPolicy,
    ack_wait: i64,
    max_deliver: i64,
    max_ack_pending: i64,
    filter_subject: Option<String>,
    idle_heartbeat: i64,
    flow_control: bool,
    headers_only: bool,
    created: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pending {
    stream_seq: u64,
    consumer_seq: u64,
    deliveries: u32,
    deadline: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ConsumerState {
    delivered_stream_seq: u64,
    consumer_seq: u64,
    ack_floor_stream_seq: u64,
    ack_floor_consumer_seq: u64,
    redelivered: u64,
    pending: Vec<Pending>,
}

struct StoredMessage {
    time: i64,
    subject: String,
    headers: Vec<u8>,
    payload: Vec<u8>,
}

struct WaitingPull {
    stream: String,
    consumer: String,
    reply: String,
    remaining: usize,
    deadline: Instant,
}

fn now_nanos() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_nanos()).unwrap_or(i64::MAX))
}

fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn civil(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

fn timestamp(nanos: i64) -> String {
    let seconds = nanos.div_euclid(1_000_000_000);
    let (year, month, day) = civil(seconds.div_euclid(86_400));
    let of_day = seconds.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:09}Z",
        of_day / 3600,
        of_day / 60 % 60,
        of_day % 60,
        nanos.rem_euclid(1_000_000_000)
    )
}

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Json> {
    match &node.value {
        Json::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, node)| &node.value),
        _ => None,
    }
}

fn member_node<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, node)| node),
        _ => None,
    }
}

fn integer(node: &Node, name: &str, default: i64) -> i64 {
    match member(node, name) {
        Some(Json::Number(number)) => number.parse::<i64>().ok().or_else(|| number.parse::<f64>().ok().map(|value| value as i64)).unwrap_or(default),
        _ => default,
    }
}

fn text<'n>(node: &'n Node, name: &str) -> Option<&'n str> {
    match member(node, name)? {
        Json::String(value) => Some(value).filter(|value| !value.is_empty()),
        _ => None,
    }
}

fn flag(node: &Node, name: &str) -> bool {
    matches!(member(node, name), Some(Json::Boolean(true)))
}

fn strings(node: &Node, name: &str) -> Vec<String> {
    match member(node, name) {
        Some(Json::Array(items)) => items
            .iter()
            .filter_map(|item| match &item.value {
                Json::String(value) => Some(value.clone()),
                _ => None,
            })
            .collect(),
        Some(Json::String(value)) => vec![value.clone()],
        _ => Vec::new(),
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MOST_NAME_BYTES
        && !name.chars().any(|character| matches!(character, '.' | '*' | '>' | '/' | '\\') || character.is_whitespace())
}

fn error_json(kind: &str, (code, err_code, description): (u16, u32, &str)) -> String {
    format!(
        "{{\"type\":\"io.nats.jetstream.api.v1.{kind}\",\"error\":{{\"code\":{code},\"err_code\":{err_code},\"description\":{}}}}}",
        quote(description)
    )
}

fn key(prefix: u8, parts: &[&[u8]]) -> Vec<u8> {
    let mut key = vec![prefix];
    for (at, part) in parts.iter().enumerate() {
        if at > 0 {
            key.push(0);
        }
        key.extend_from_slice(part);
    }
    key
}

fn message_key(stream: &str, seq: u64) -> Vec<u8> {
    let mut key = key(MESSAGE, &[stream.as_bytes()]);
    key.push(0);
    key.extend_from_slice(&seq.to_be_bytes());
    key
}

fn subject_prefix(stream: &str, subject: &str) -> Vec<u8> {
    let mut key = key(SUBJECT_INDEX, &[stream.as_bytes(), subject.as_bytes()]);
    key.push(0);
    key
}

fn subject_key(stream: &str, subject: &str, seq: u64) -> Vec<u8> {
    let mut key = subject_prefix(stream, subject);
    key.extend_from_slice(&seq.to_be_bytes());
    key
}

fn seq_at_end(key: &[u8]) -> u64 {
    key.get(key.len().saturating_sub(8)..).and_then(|raw| raw.try_into().ok()).map_or(0, u64::from_be_bytes)
}

fn subject_seqs(keys: &dyn KeysRead, stream: &str, subject: &str) -> Result<Vec<u64>, String> {
    Ok(keys.scan(&subject_prefix(stream, subject))?.iter().map(|(found, _)| seq_at_end(found)).collect())
}

fn last_per_subject(keys: &dyn KeysRead, stream: &str, filter: Option<&str>) -> Result<Vec<u64>, String> {
    let mut prefix = key(SUBJECT_INDEX, &[stream.as_bytes()]);
    prefix.push(0);
    let mut last: std::collections::BTreeMap<Vec<u8>, u64> = std::collections::BTreeMap::new();
    for (found, _) in keys.scan(&prefix)? {
        let Some(subject) = found.get(prefix.len()..found.len().saturating_sub(9)) else {
            continue;
        };
        let wanted = std::str::from_utf8(subject).is_ok_and(|subject| filter.is_none_or(|filter| subject_matches(filter, subject)));
        if wanted {
            last.insert(subject.to_vec(), seq_at_end(&found));
        }
    }
    let mut seqs: Vec<u64> = last.into_values().collect();
    seqs.sort_unstable();
    Ok(seqs)
}

fn message_prefix(stream: &str) -> Vec<u8> {
    let mut key = key(MESSAGE, &[stream.as_bytes()]);
    key.push(0);
    key
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn take<'b>(bytes: &'b [u8], at: &mut usize, count: usize) -> Option<&'b [u8]> {
    let end = at.checked_add(count).filter(|end| *end <= bytes.len())?;
    let taken = &bytes[*at..end];
    *at = end;
    Some(taken)
}

fn take_u64(bytes: &[u8], at: &mut usize) -> Option<u64> {
    Some(u64::from_be_bytes(take(bytes, at, 8)?.try_into().ok()?))
}

fn take_bytes<'b>(bytes: &'b [u8], at: &mut usize) -> Option<&'b [u8]> {
    let length = u32::from_be_bytes(take(bytes, at, 4)?.try_into().ok()?) as usize;
    take(bytes, at, length)
}

impl StoredMessage {
    fn encode(&self) -> Vec<u8> {
        let mut out = self.time.to_be_bytes().to_vec();
        put_bytes(&mut out, self.subject.as_bytes());
        put_bytes(&mut out, &self.headers);
        out.extend_from_slice(&self.payload);
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let mut at = 0;
        let time = take_u64(bytes, &mut at)? as i64;
        let subject = String::from_utf8(take_bytes(bytes, &mut at)?.to_vec()).ok()?;
        let headers = take_bytes(bytes, &mut at)?.to_vec();
        Some(Self { time, subject, headers, payload: bytes.get(at..)?.to_vec() })
    }

    fn size(&self) -> u64 {
        (self.subject.len() + self.headers.len() + self.payload.len()) as u64
    }
}

impl StreamState {
    fn encode(&self) -> Vec<u8> {
        [self.first_seq, self.last_seq, self.messages, self.bytes].iter().flat_map(|value| value.to_be_bytes()).collect()
    }

    fn decode(bytes: &[u8]) -> Self {
        let mut at = 0;
        let mut next = || take_u64(bytes, &mut at).unwrap_or(0);
        Self { first_seq: next(), last_seq: next(), messages: next(), bytes: next() }
    }
}

impl ConsumerState {
    fn encode(&self) -> Vec<u8> {
        let mut out: Vec<u8> = [
            self.delivered_stream_seq,
            self.consumer_seq,
            self.ack_floor_stream_seq,
            self.ack_floor_consumer_seq,
            self.redelivered,
            self.pending.len() as u64,
        ]
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect();
        for pending in &self.pending {
            out.extend_from_slice(&pending.stream_seq.to_be_bytes());
            out.extend_from_slice(&pending.consumer_seq.to_be_bytes());
            out.extend_from_slice(&pending.deliveries.to_be_bytes());
            out.extend_from_slice(&pending.deadline.to_be_bytes());
        }
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let mut at = 0;
        let mut state = Self {
            delivered_stream_seq: take_u64(bytes, &mut at)?,
            consumer_seq: take_u64(bytes, &mut at)?,
            ack_floor_stream_seq: take_u64(bytes, &mut at)?,
            ack_floor_consumer_seq: take_u64(bytes, &mut at)?,
            redelivered: take_u64(bytes, &mut at)?,
            pending: Vec::new(),
        };
        let count = take_u64(bytes, &mut at)?.min(DEFAULT_MAX_ACK_PENDING as u64 * 100);
        for _ in 0..count {
            state.pending.push(Pending {
                stream_seq: take_u64(bytes, &mut at)?,
                consumer_seq: take_u64(bytes, &mut at)?,
                deliveries: u32::from_be_bytes(take(bytes, &mut at, 4)?.try_into().ok()?),
                deadline: take_u64(bytes, &mut at)? as i64,
            });
        }
        Some(state)
    }

    fn settle_floor(&mut self) {
        match self.pending.iter().min_by_key(|pending| pending.stream_seq) {
            Some(oldest) => {
                self.ack_floor_stream_seq = oldest.stream_seq.saturating_sub(1);
                self.ack_floor_consumer_seq = oldest.consumer_seq.saturating_sub(1);
            }
            None => {
                self.ack_floor_stream_seq = self.delivered_stream_seq;
                self.ack_floor_consumer_seq = self.consumer_seq;
            }
        }
    }
}

impl StreamConfig {
    fn from_request(name: &str, node: &Node, created: i64) -> Result<Self, (u16, u32, &'static str)> {
        if text(node, "name").is_some_and(|given| given != name) || !valid_name(name) {
            return Err(BAD_REQUEST);
        }
        let mirror = member_node(node, "mirror").filter(|mirror| mirror.value != Json::Null).map(Origin::from_node).transpose()?;
        let sources = match member(node, "sources") {
            Some(Json::Array(items)) => items.iter().map(Origin::from_node).collect::<Result<Vec<_>, _>>()?,
            _ => Vec::new(),
        };
        if sources.len() > MOST_SOURCES
            || (mirror.is_some() && !sources.is_empty())
            || mirror.as_ref().is_some_and(|mirror| mirror.name == name)
            || sources.iter().any(|source| source.name == name)
        {
            return Err(BAD_REQUEST);
        }
        let mut subjects = strings(node, "subjects");
        if mirror.is_some() && !subjects.is_empty() {
            return Err(BAD_REQUEST);
        }
        if subjects.is_empty() && mirror.is_none() && sources.is_empty() {
            subjects.push(name.to_owned());
        }
        if subjects.iter().any(|subject| !valid_subject(subject, true) || subject.starts_with("$JS.")) {
            return Err(BAD_REQUEST);
        }
        let retention = match text(node, "retention").unwrap_or("limits") {
            "limits" => Retention::Limits,
            "interest" => Retention::Interest,
            "workqueue" => Retention::WorkQueue,
            _ => return Err(BAD_REQUEST),
        };
        Ok(Self {
            name: name.to_owned(),
            subjects,
            retention,
            max_msgs: integer(node, "max_msgs", -1),
            max_bytes: integer(node, "max_bytes", -1),
            max_age: integer(node, "max_age", 0),
            max_msg_size: integer(node, "max_msg_size", -1),
            max_msgs_per_subject: integer(node, "max_msgs_per_subject", -1),
            allow_direct: flag(node, "allow_direct"),
            allow_rollup: flag(node, "allow_rollup_hdrs"),
            deny_delete: flag(node, "deny_delete"),
            deny_purge: flag(node, "deny_purge"),
            discard_new: text(node, "discard") == Some("new"),
            duplicate_window: match integer(node, "duplicate_window", 0) {
                0 => DEFAULT_DUPLICATE_WINDOW_NANOS,
                window => window,
            },
            memory: text(node, "storage") == Some("memory"),
            mirror,
            sources,
            created,
        })
    }

    fn json(&self) -> String {
        format!(
            "{{\"name\":{},\"subjects\":[{}],\"retention\":\"{}\",\"max_consumers\":-1,\"max_msgs\":{},\"max_bytes\":{},\"max_age\":{},\"max_msgs_per_subject\":{},\"max_msg_size\":{},\"discard\":\"{}\",\"storage\":\"{}\",\"num_replicas\":1,\"duplicate_window\":{},\"allow_direct\":{},\"allow_rollup_hdrs\":{},\"deny_delete\":{},\"deny_purge\":{}}}",
            quote(&self.name),
            self.subjects.iter().map(|subject| quote(subject)).collect::<Vec<_>>().join(","),
            match self.retention {
                Retention::Limits => "limits",
                Retention::Interest => "interest",
                Retention::WorkQueue => "workqueue",
            },
            self.max_msgs,
            self.max_bytes,
            self.max_age,
            self.max_msgs_per_subject,
            self.max_msg_size,
            if self.discard_new { "new" } else { "old" },
            if self.memory { "memory" } else { "file" },
            self.duplicate_window,
            self.allow_direct,
            self.allow_rollup,
            self.deny_delete,
            self.deny_purge
        )
        .trim_end_matches('}')
        .to_owned()
            + &self.origins_json()
            + "}"
    }

    fn origins_json(&self) -> String {
        let mut out = String::new();
        if let Some(mirror) = &self.mirror {
            out.push_str(&format!(",\"mirror\":{}", mirror.json()));
        }
        if !self.sources.is_empty() {
            out.push_str(&format!(",\"sources\":[{}]", self.sources.iter().map(Origin::json).collect::<Vec<_>>().join(",")));
        }
        out
    }

    fn origins(&self) -> impl Iterator<Item = &Origin> {
        self.mirror.iter().chain(&self.sources)
    }

    fn stored(&self) -> String {
        format!("{{\"config\":{},\"created\":{}}}", self.json(), self.created)
    }

    fn from_stored(bytes: &[u8]) -> Option<Self> {
        let document = parse_document(std::str::from_utf8(bytes).ok()?).ok()?;
        let config = member_node(&document, "config")?;
        Self::from_request(text(config, "name")?, config, integer(&document, "created", 0)).ok()
    }
}

impl ConsumerConfig {
    fn from_request(name: Option<&str>, node: &Node, created: i64) -> Result<Self, (u16, u32, &'static str)> {
        let config = member_node(node, "config").unwrap_or(node);
        let durable = text(config, "durable_name");
        let name = match (name, durable, text(config, "name")) {
            (Some(name), _, _) => name.to_owned(),
            (None, Some(durable), _) | (None, None, Some(durable)) => durable.to_owned(),
            (None, None, None) => format!("{:016x}", now_nanos() as u64 ^ 0x9e37_79b9_7f4a_7c15),
        };
        if !valid_name(&name) {
            return Err(BAD_REQUEST);
        }
        let deliver_policy = match text(config, "deliver_policy").unwrap_or("all") {
            "all" => DeliverPolicy::All,
            "last" => DeliverPolicy::Last,
            "new" => DeliverPolicy::New,
            "last_per_subject" => DeliverPolicy::LastPerSubject,
            "by_start_sequence" => DeliverPolicy::ByStartSequence(integer(config, "opt_start_seq", 1).max(1) as u64),
            _ => return Err(BAD_REQUEST),
        };
        let ack_policy = match text(config, "ack_policy").unwrap_or("explicit") {
            "explicit" => AckPolicy::Explicit,
            "none" => AckPolicy::None,
            "all" => AckPolicy::All,
            _ => return Err(BAD_REQUEST),
        };
        let filter_subject = text(config, "filter_subject").map(str::to_owned);
        if filter_subject.as_deref().is_some_and(|filter| !valid_subject(filter, true)) {
            return Err(BAD_REQUEST);
        }
        let deliver_subject = text(config, "deliver_subject").map(str::to_owned);
        if deliver_subject.as_deref().is_some_and(|subject| !valid_subject(subject, false)) {
            return Err(BAD_REQUEST);
        }
        Ok(Self {
            durable: durable.is_some(),
            name,
            deliver_subject,
            deliver_policy,
            ack_policy,
            ack_wait: match integer(config, "ack_wait", 0) {
                wait if wait <= 0 => DEFAULT_ACK_WAIT_NANOS,
                wait => wait,
            },
            max_deliver: integer(config, "max_deliver", -1),
            max_ack_pending: match integer(config, "max_ack_pending", 0) {
                0 => DEFAULT_MAX_ACK_PENDING,
                most => most,
            },
            filter_subject,
            idle_heartbeat: integer(config, "idle_heartbeat", 0).max(0),
            flow_control: flag(config, "flow_control"),
            headers_only: flag(config, "headers_only"),
            created,
        })
    }

    fn json(&self) -> String {
        let mut fields = vec![format!("\"name\":{}", quote(&self.name))];
        if self.durable {
            fields.push(format!("\"durable_name\":{}", quote(&self.name)));
        }
        if let Some(subject) = &self.deliver_subject {
            fields.push(format!("\"deliver_subject\":{}", quote(subject)));
        }
        fields.push(match self.deliver_policy {
            DeliverPolicy::All => "\"deliver_policy\":\"all\"".to_owned(),
            DeliverPolicy::Last => "\"deliver_policy\":\"last\"".to_owned(),
            DeliverPolicy::New => "\"deliver_policy\":\"new\"".to_owned(),
            DeliverPolicy::LastPerSubject => "\"deliver_policy\":\"last_per_subject\"".to_owned(),
            DeliverPolicy::ByStartSequence(seq) => format!("\"deliver_policy\":\"by_start_sequence\",\"opt_start_seq\":{seq}"),
        });
        fields.push(format!(
            "\"ack_policy\":\"{}\"",
            match self.ack_policy {
                AckPolicy::Explicit => "explicit",
                AckPolicy::None => "none",
                AckPolicy::All => "all",
            }
        ));
        fields.push(format!("\"ack_wait\":{}", self.ack_wait));
        fields.push(format!("\"max_deliver\":{}", self.max_deliver));
        fields.push(format!("\"max_ack_pending\":{}", self.max_ack_pending));
        if let Some(filter) = &self.filter_subject {
            fields.push(format!("\"filter_subject\":{}", quote(filter)));
        }
        if self.idle_heartbeat > 0 {
            fields.push(format!("\"idle_heartbeat\":{}", self.idle_heartbeat));
        }
        if self.flow_control {
            fields.push("\"flow_control\":true".to_owned());
        }
        if self.headers_only {
            fields.push("\"headers_only\":true".to_owned());
        }
        fields.push("\"replay_policy\":\"instant\"".to_owned());
        fields.push("\"num_replicas\":0".to_owned());
        format!("{{{}}}", fields.join(","))
    }

    fn stored(&self) -> String {
        format!("{{\"config\":{},\"created\":{},\"durable\":{}}}", self.json(), self.created, self.durable)
    }

    fn from_stored(bytes: &[u8]) -> Option<Self> {
        let document = parse_document(std::str::from_utf8(bytes).ok()?).ok()?;
        let config = member_node(&document, "config")?;
        let mut parsed = Self::from_request(text(config, "name"), config, integer(&document, "created", 0)).ok()?;
        parsed.durable = flag(&document, "durable");
        Some(parsed)
    }

    fn wants(&self, subject: &str) -> bool {
        self.filter_subject.as_deref().is_none_or(|filter| subject_matches(filter, subject))
    }
}

fn read<T>(keyspace: &dyn Keyspace, body: impl FnOnce(&dyn KeysRead) -> Result<T, String>) -> Result<T, String> {
    let mut body = Some(body);
    let mut out = None;
    keyspace.read(0, &mut |keys| {
        if let Some(body) = body.take() {
            out = Some(body(keys)?);
        }
        Ok(())
    })?;
    out.ok_or_else(|| "the read did not run".to_owned())
}

fn write<T>(keyspace: &dyn Keyspace, body: impl FnOnce(&mut dyn KeysWrite) -> Result<T, String>) -> Result<T, String> {
    let mut body = Some(body);
    let mut out = None;
    keyspace.write(0, &mut |keys| {
        if let Some(body) = body.take() {
            out = Some(body(keys)?);
        }
        Ok(())
    })?;
    out.ok_or_else(|| "the write did not run".to_owned())
}

/// Whether some subject matches both `left` and `right`, each of which may hold `*` and `>`.
fn subjects_overlap(left: &str, right: &str) -> bool {
    let mut left_tokens = left.split('.');
    let mut right_tokens = right.split('.');
    loop {
        match (left_tokens.next(), right_tokens.next()) {
            (Some(">"), Some(_)) | (Some(_), Some(">")) => return true,
            (Some(a), Some(b)) => {
                if a != b && a != "*" && b != "*" {
                    return false;
                }
            }
            (None, None) => return true,
            _ => return false,
        }
    }
}

fn stream_config(keys: &dyn KeysRead, name: &str) -> Result<Option<StreamConfig>, String> {
    Ok(keys.get(&key(STREAM, &[name.as_bytes()]))?.and_then(|raw| StreamConfig::from_stored(&raw)))
}

fn stream_state(keys: &dyn KeysRead, name: &str) -> Result<StreamState, String> {
    Ok(keys.get(&key(STREAM_STATE, &[name.as_bytes()]))?.map(|raw| StreamState::decode(&raw)).unwrap_or_default())
}

fn consumer(keys: &dyn KeysRead, stream: &str, name: &str) -> Result<Option<(ConsumerConfig, ConsumerState)>, String> {
    let Some(config) = keys
        .get(&key(CONSUMER, &[stream.as_bytes(), name.as_bytes()]))?
        .and_then(|raw| ConsumerConfig::from_stored(&raw))
    else {
        return Ok(None);
    };
    let state = keys
        .get(&key(CONSUMER_STATE, &[stream.as_bytes(), name.as_bytes()]))?
        .and_then(|raw| ConsumerState::decode(&raw))
        .unwrap_or_default();
    Ok(Some((config, state)))
}

fn consumer_names(keys: &dyn KeysRead, stream: &str) -> Result<Vec<String>, String> {
    let mut prefix = key(CONSUMER, &[stream.as_bytes()]);
    prefix.push(0);
    Ok(keys
        .scan(&prefix)?
        .into_iter()
        .filter_map(|(found, _)| String::from_utf8(found.get(prefix.len()..)?.to_vec()).ok())
        .collect())
}

fn stream_names(keys: &dyn KeysRead) -> Result<Vec<String>, String> {
    Ok(keys
        .scan(&[STREAM])?
        .into_iter()
        .filter_map(|(found, _)| String::from_utf8(found.get(1..)?.to_vec()).ok())
        .collect())
}

fn remove_message(keys: &mut dyn KeysWrite, stream: &str, seq: u64, state: &mut StreamState) -> Result<bool, String> {
    let message_key = message_key(stream, seq);
    let Some(raw) = keys.get(&message_key)? else {
        return Ok(false);
    };
    keys.delete(&message_key)?;
    let decoded = StoredMessage::decode(&raw);
    if let Some(message) = &decoded {
        keys.delete(&subject_key(stream, &message.subject, seq))?;
    }
    let size = decoded.map_or(0, |message| message.size());
    state.messages = state.messages.saturating_sub(1);
    state.bytes = state.bytes.saturating_sub(size);
    if seq == state.first_seq {
        let prefix = message_prefix(stream);
        let mut next = None;
        keys.visit_prefix(&prefix, &mut |found, _| {
            next = found.get(prefix.len()..).and_then(|raw| raw.try_into().ok()).map(u64::from_be_bytes);
            Ok(false)
        })?;
        state.first_seq = next.unwrap_or(state.last_seq + 1);
    }
    Ok(true)
}

fn enforce_limits(keys: &mut dyn KeysWrite, config: &StreamConfig, state: &mut StreamState, now: i64) -> Result<(), String> {
    loop {
        let over_count = config.max_msgs >= 0 && state.messages > config.max_msgs as u64;
        let over_bytes = config.max_bytes >= 0 && state.bytes > config.max_bytes as u64;
        let oldest = if state.messages == 0 {
            None
        } else {
            keys.get(&message_key(&config.name, state.first_seq))?.and_then(|raw| StoredMessage::decode(&raw))
        };
        let too_old = config.max_age > 0 && oldest.as_ref().is_some_and(|message| now - message.time > config.max_age);
        if !(over_count || over_bytes || too_old) || state.messages == 0 {
            return Ok(());
        }
        let first = state.first_seq;
        if !remove_message(keys, &config.name, first, state)? {
            state.first_seq = first + 1;
        }
    }
}

fn acknowledged_by_all(keys: &dyn KeysRead, stream: &str, seq: u64, subject: &str) -> Result<bool, String> {
    for name in consumer_names(keys, stream)? {
        if let Some((config, state)) = consumer(keys, stream, &name)?
            && config.wants(subject)
            && (state.delivered_stream_seq < seq || state.pending.iter().any(|pending| pending.stream_seq == seq))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn interested(keys: &dyn KeysRead, stream: &str, subject: &str) -> Result<bool, String> {
    for name in consumer_names(keys, stream)? {
        if consumer(keys, stream, &name)?.is_some_and(|(config, _)| config.wants(subject)) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn with_header(headers: &[u8], name: &str, value: &str) -> Vec<u8> {
    let text = std::str::from_utf8(headers).unwrap_or_default();
    let mut lines: Vec<&str> = text.split("\r\n").filter(|line| !line.is_empty()).collect();
    if lines.is_empty() {
        lines.push("NATS/1.0");
    }
    let mut out = lines.join("\r\n");
    out.push_str(&format!("\r\n{name}: {value}\r\n\r\n"));
    out.into_bytes()
}

fn header_value<'h>(headers: &'h [u8], name: &str) -> Option<&'h str> {
    std::str::from_utf8(headers).ok()?.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

fn stream_info_json(kind: &str, config: &StreamConfig, state: &StreamState, consumers: usize, keys: &dyn KeysRead) -> Result<String, String> {
    let time_of = |seq: u64| -> Result<String, String> {
        Ok(keys
            .get(&message_key(&config.name, seq))?
            .and_then(|raw| StoredMessage::decode(&raw))
            .map_or_else(|| timestamp(0), |message| timestamp(message.time)))
    };
    let (first_ts, last_ts) = if state.messages == 0 {
        (timestamp(0), timestamp(0))
    } else {
        (time_of(state.first_seq)?, time_of(state.last_seq)?)
    };
    Ok(format!(
        "{{\"type\":\"io.nats.jetstream.api.v1.{kind}\",\"config\":{},\"created\":{},\"state\":{{\"messages\":{},\"bytes\":{},\"first_seq\":{},\"first_ts\":{},\"last_seq\":{},\"last_ts\":{},\"consumer_count\":{consumers}}}}}",
        config.json(),
        quote(&timestamp(config.created)),
        state.messages,
        state.bytes,
        state.first_seq.max(1).min(state.last_seq + 1),
        quote(&first_ts),
        state.last_seq,
        quote(&last_ts)
    ))
}

fn consumer_info_json(kind: &str, stream: &str, config: &ConsumerConfig, state: &ConsumerState, stream_state: &StreamState, waiting: usize) -> String {
    format!(
        "{{\"type\":\"io.nats.jetstream.api.v1.{kind}\",\"stream_name\":{},\"name\":{},\"created\":{},\"config\":{},\"delivered\":{{\"consumer_seq\":{},\"stream_seq\":{}}},\"ack_floor\":{{\"consumer_seq\":{},\"stream_seq\":{}}},\"num_ack_pending\":{},\"num_redelivered\":{},\"num_waiting\":{waiting},\"num_pending\":{}}}",
        quote(stream),
        quote(&config.name),
        quote(&timestamp(config.created)),
        config.json(),
        state.consumer_seq,
        state.delivered_stream_seq,
        state.ack_floor_consumer_seq,
        state.ack_floor_stream_seq,
        state.pending.len(),
        state.redelivered,
        unsent(state, stream_state)
    )
}

fn unsent(state: &ConsumerState, stream_state: &StreamState) -> u64 {
    stream_state.last_seq.saturating_sub(state.delivered_stream_seq)
        + state.pending.iter().filter(|pending| pending.deliveries == 0).count() as u64
}

fn headers_only(headers: Vec<u8>, size: usize) -> Vec<u8> {
    let mut text = String::from_utf8(headers).unwrap_or_default();
    if text.is_empty() {
        text.push_str("NATS/1.0\r\n");
    } else if let Some(stripped) = text.strip_suffix("\r\n\r\n") {
        text = format!("{stripped}\r\n");
    }
    text.push_str(&format!("Nats-Msg-Size: {size}\r\n\r\n"));
    text.into_bytes()
}

fn ack_subject(stream: &str, consumer: &str, pending: &Pending, time: i64, left: u64) -> String {
    format!(
        "{ACK_PREFIX}{stream}.{consumer}.{}.{}.{}.{time}.{left}",
        pending.deliveries, pending.stream_seq, pending.consumer_seq
    )
}

/// JetStream (D216): streams that keep the messages published to their subjects in the
/// keyspace, and durable consumers that deliver them again until they are acknowledged.
pub(crate) struct JetStream<'k> {
    keyspace: &'k (dyn Keyspace + Sync),
    waiting: Mutex<Vec<WaitingPull>>,
    subjects: Mutex<Option<Vec<(String, Vec<String>)>>>,
    pending_push: Mutex<Vec<Outgoing>>,
    last_push: Mutex<std::collections::BTreeMap<(String, String), Instant>>,
}

impl<'k> JetStream<'k> {
    pub(crate) fn new(keyspace: &'k (dyn Keyspace + Sync)) -> Self {
        Self { keyspace, waiting: Mutex::new(Vec::new()), subjects: Mutex::new(None), pending_push: Mutex::new(Vec::new()), last_push: Mutex::new(std::collections::BTreeMap::new()) }
    }

    /// Whether the subject is one JetStream answers instead of the subscribers.
    pub(crate) fn handles(subject: &str) -> bool {
        subject.starts_with(API_PREFIX) || subject.starts_with(ACK_PREFIX)
    }

    fn forget_subjects(&self) {
        *self.subjects.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    fn stream_for(&self, subject: &str) -> Option<String> {
        let mut cached = self.subjects.lock().unwrap_or_else(PoisonError::into_inner);
        if cached.is_none() {
            let loaded = read(self.keyspace, |keys| {
                Ok(keys
                    .scan(&[STREAM])?
                    .into_iter()
                    .filter_map(|(_, raw)| StreamConfig::from_stored(&raw))
                    .map(|config| (config.name, config.subjects))
                    .collect::<Vec<_>>())
            })
            .ok()?;
            *cached = Some(loaded);
        }
        cached
            .as_ref()?
            .iter()
            .find(|(_, subjects)| subjects.iter().any(|filter| subject_matches(filter, subject)))
            .map(|(name, _)| name.clone())
    }

    /// Answers a `$JS.API.` request or a `$JS.ACK.` acknowledgement: the messages to send.
    pub(crate) fn request(&self, subject: &str, reply: Option<&str>, payload: &[u8]) -> Vec<Outgoing> {
        if let Some(ack) = subject.strip_prefix(ACK_PREFIX) {
            return self.acknowledge(ack, reply, payload);
        }
        let Some(reply) = reply else {
            return Vec::new();
        };
        let api = subject.strip_prefix(API_PREFIX).unwrap_or_default();
        let body = std::str::from_utf8(payload).unwrap_or_default().trim();
        let request = if body.is_empty() { parse_document("{}") } else { parse_document(body) };
        let Ok(request) = request else {
            return vec![Outgoing::json(reply, error_json("error_response", (400, 10_025, "invalid JSON")))];
        };
        let tokens: Vec<&str> = api.split('.').collect();
        let answer = match tokens.as_slice() {
            ["INFO"] => self.account_info(),
            ["STREAM", "CREATE", name] => self.create_stream(name, &request, false),
            ["STREAM", "UPDATE", name] => self.create_stream(name, &request, true),
            ["STREAM", "INFO", name] => self.stream_info(name, "stream_info_response"),
            ["STREAM", "DELETE", name] => self.delete_stream(name),
            ["STREAM", "PURGE", name] => self.purge_stream(name, &request),
            ["DIRECT", "GET", stream] => return self.direct_get(stream, None, &request, reply),
            ["DIRECT", "GET", stream, subject @ ..] => return self.direct_get(stream, Some(subject.join(".")), &request, reply),
            ["STREAM", "NAMES"] => self.stream_list(false, &request),
            ["STREAM", "LIST"] => self.stream_list(true, &request),
            ["STREAM", "MSG", "GET", name] => self.get_message(name, &request),
            ["STREAM", "MSG", "DELETE", name] => self.delete_message(name, &request),
            ["CONSUMER", "CREATE", stream] => self.create_consumer(stream, None, &request),
            ["CONSUMER", "CREATE", stream, name] | ["CONSUMER", "CREATE", stream, name, _] | ["CONSUMER", "DURABLE", "CREATE", stream, name] => {
                self.create_consumer(stream, Some(name), &request)
            }
            ["CONSUMER", "INFO", stream, name] => self.consumer_info(stream, name),
            ["CONSUMER", "DELETE", stream, name] => self.delete_consumer(stream, name),
            ["CONSUMER", "NAMES", stream] => self.consumer_list(stream, false),
            ["CONSUMER", "LIST", stream] => self.consumer_list(stream, true),
            ["CONSUMER", "MSG", "NEXT", stream, name] => return self.pull(stream, name, reply, body, &request),
            _ => Ok(error_json("error_response", (400, 10_003, "unknown JetStream API request"))),
        };
        let payload = answer.unwrap_or_else(|problem| error_json("error_response", (500, 10_000, &problem)));
        let mut out = vec![Outgoing::json(reply, payload)];
        out.extend(self.take_pushed());
        out
    }

    fn account_info(&self) -> Result<String, String> {
        read(self.keyspace, |keys| {
            let names = stream_names(keys)?;
            let mut storage = 0u64;
            let mut consumers = 0usize;
            for name in &names {
                storage += stream_state(keys, name)?.bytes;
                consumers += consumer_names(keys, name)?.len();
            }
            Ok(format!(
                "{{\"type\":\"io.nats.jetstream.api.v1.account_info_response\",\"memory\":0,\"storage\":{storage},\"streams\":{},\"consumers\":{consumers},\"limits\":{{\"max_memory\":-1,\"max_storage\":-1,\"max_streams\":{MOST_STREAMS},\"max_consumers\":{MOST_CONSUMERS}}},\"api\":{{\"total\":0,\"errors\":0}}}}",
                names.len()
            ))
        })
    }

    fn create_stream(&self, name: &str, request: &Node, update: bool) -> Result<String, String> {
        let kind = if update { "stream_update_response" } else { "stream_create_response" };
        let config = match StreamConfig::from_request(name, request, now_nanos()) {
            Ok(config) => config,
            Err(problem) => return Ok(error_json(kind, problem)),
        };
        let answer = write(self.keyspace, |keys| {
            let existing = stream_config(&*keys, name)?;
            let names = stream_names(&*keys)?;
            if existing.is_none() && (update || names.len() >= MOST_STREAMS) {
                return Ok(error_json(kind, if update { STREAM_NOT_FOUND } else { BAD_REQUEST }));
            }
            if let Some(existing) = &existing
                && !update
            {
                let same = StreamConfig { created: existing.created, ..config.clone() };
                if &same != existing {
                    return Ok(error_json(kind, STREAM_NAME_IN_USE));
                }
            }
            for other in names.iter().filter(|other| *other != name) {
                if let Some(other) = stream_config(&*keys, other)?
                    && other.subjects.iter().any(|taken| {
                        config.subjects.iter().any(|wanted| subject_matches(taken, wanted) || subject_matches(wanted, taken))
                    })
                {
                    return Ok(error_json(kind, SUBJECTS_OVERLAP));
                }
            }
            let config = StreamConfig { created: existing.as_ref().map_or(config.created, |existing| existing.created), ..config };
            keys.put(&key(STREAM, &[name.as_bytes()]), config.stored().as_bytes())?;
            let mut state = stream_state(&*keys, name)?;
            if state.first_seq == 0 {
                state.first_seq = 1;
            }
            enforce_limits(keys, &config, &mut state, now_nanos())?;
            keys.put(&key(STREAM_STATE, &[name.as_bytes()]), &state.encode())?;
            let consumers = consumer_names(&*keys, name)?.len();
            stream_info_json(kind, &config, &state, consumers, &*keys)
        });
        self.forget_subjects();
        if !update && answer.as_ref().is_ok_and(|text| !text.contains("\"error\"")) {
            self.backfill(name);
        }
        answer
    }

    fn stream_info(&self, name: &str, kind: &str) -> Result<String, String> {
        read(self.keyspace, |keys| match stream_config(keys, name)? {
            Some(config) => stream_info_json(kind, &config, &stream_state(keys, name)?, consumer_names(keys, name)?.len(), keys),
            None => Ok(error_json(kind, STREAM_NOT_FOUND)),
        })
    }

    fn delete_stream(&self, name: &str) -> Result<String, String> {
        let answer = write(self.keyspace, |keys| {
            if stream_config(&*keys, name)?.is_none() {
                return Ok(error_json("stream_delete_response", STREAM_NOT_FOUND));
            }
            let mut doomed: Vec<Vec<u8>> = vec![key(STREAM, &[name.as_bytes()]), key(STREAM_STATE, &[name.as_bytes()])];
            for prefix in [
                message_prefix(name),
                key(CONSUMER, &[name.as_bytes(), b""]),
                key(CONSUMER_STATE, &[name.as_bytes(), b""]),
                key(DEDUPLICATION, &[name.as_bytes(), b""]),
                key(SUBJECT_INDEX, &[name.as_bytes(), b""]),
            ] {
                doomed.extend(keys.scan(&prefix)?.into_iter().map(|(found, _)| found));
            }
            for found in doomed {
                keys.delete(&found)?;
            }
            Ok("{\"type\":\"io.nats.jetstream.api.v1.stream_delete_response\",\"success\":true}".to_owned())
        });
        self.forget_subjects();
        answer
    }

    fn purge_stream(&self, name: &str, request: &Node) -> Result<String, String> {
        write(self.keyspace, |keys| {
            let Some(config) = stream_config(&*keys, name)? else {
                return Ok(error_json("stream_purge_response", STREAM_NOT_FOUND));
            };
            if config.deny_purge {
                return Ok(error_json("stream_purge_response", (400, 10_003, "purge is not allowed on this stream")));
            }
            let filter = text(request, "filter");
            let below = integer(request, "seq", 0).max(0) as u64;
            let keep = integer(request, "keep", 0).max(0) as usize;
            let mut seqs: Vec<u64> = Vec::new();
            keys.visit_prefix(&message_prefix(name), &mut |found, raw| {
                let wanted = filter.is_none_or(|filter| StoredMessage::decode(raw).is_some_and(|message| subject_matches(filter, &message.subject)));
                if wanted {
                    seqs.push(seq_at_end(found));
                }
                Ok(true)
            })?;
            if below > 0 {
                seqs.retain(|seq| *seq < below);
            }
            if keep > 0 {
                seqs.truncate(seqs.len().saturating_sub(keep));
            }
            let mut state = stream_state(&*keys, name)?;
            let mut purged = 0u64;
            for seq in seqs {
                if remove_message(keys, name, seq, &mut state)? {
                    purged += 1;
                }
            }
            if state.messages == 0 {
                state.first_seq = state.last_seq + 1;
            }
            keys.put(&key(STREAM_STATE, &[name.as_bytes()]), &state.encode())?;
            Ok(format!("{{\"type\":\"io.nats.jetstream.api.v1.stream_purge_response\",\"success\":true,\"purged\":{purged}}}"))
        })
    }

    fn stream_list(&self, detailed: bool, request: &Node) -> Result<String, String> {
        let wanted_subject = text(request, "subject");
        let offset = integer(request, "offset", 0).max(0) as usize;
        read(self.keyspace, |keys| {
            let mut names = Vec::new();
            for name in stream_names(keys)? {
                let listed = match &wanted_subject {
                    None => true,
                    Some(wanted) => stream_config(keys, &name)?
                        .is_some_and(|config| config.subjects.iter().any(|subject| subjects_overlap(subject, wanted))),
                };
                if listed {
                    names.push(name);
                }
            }
            let total = names.len();
            let names: Vec<String> = names.into_iter().skip(offset).collect();
            let items = if detailed {
                let mut items = Vec::new();
                for name in &names {
                    if let Some(config) = stream_config(keys, name)? {
                        let info = stream_info_json("stream_info_response", &config, &stream_state(keys, name)?, consumer_names(keys, name)?.len(), keys)?;
                        items.push(info);
                    }
                }
                items
            } else {
                names.iter().map(|name| quote(name)).collect()
            };
            Ok(format!(
                "{{\"type\":\"io.nats.jetstream.api.v1.{}\",\"total\":{total},\"offset\":{offset},\"limit\":{MOST_STREAMS},\"streams\":[{}]}}",
                if detailed { "stream_list_response" } else { "stream_names_response" },
                items.join(",")
            ))
        })
    }

    fn get_message(&self, name: &str, request: &Node) -> Result<String, String> {
        read(self.keyspace, |keys| {
            if stream_config(keys, name)?.is_none() {
                return Ok(error_json("stream_msg_get_response", STREAM_NOT_FOUND));
            }
            let found = if let Some(subject) = text(request, "last_by_subj") {
                let mut last = None;
                keys.visit_prefix(&message_prefix(name), &mut |found, raw| {
                    if let Some(message) = StoredMessage::decode(raw)
                        && subject_matches(subject, &message.subject)
                    {
                        let seq = found.get(found.len().saturating_sub(8)..).and_then(|raw| raw.try_into().ok()).map_or(0, u64::from_be_bytes);
                        last = Some((seq, message));
                    }
                    Ok(true)
                })?;
                last
            } else {
                let seq = integer(request, "seq", 0).max(0) as u64;
                keys.get(&message_key(name, seq))?.and_then(|raw| StoredMessage::decode(&raw)).map(|message| (seq, message))
            };
            let Some((seq, message)) = found else {
                return Ok(error_json("stream_msg_get_response", NO_MESSAGE_FOUND));
            };
            let headers = if message.headers.is_empty() {
                String::new()
            } else {
                format!(",\"hdrs\":{}", quote(&eggdb_crypto::base64_encode(&message.headers)))
            };
            Ok(format!(
                "{{\"type\":\"io.nats.jetstream.api.v1.stream_msg_get_response\",\"message\":{{\"subject\":{},\"seq\":{seq}{headers},\"data\":{},\"time\":{}}}}}",
                quote(&message.subject),
                quote(&eggdb_crypto::base64_encode(&message.payload)),
                quote(&timestamp(message.time))
            ))
        })
    }

    fn direct_get(&self, stream: &str, subject: Option<String>, request: &Node, reply: &str) -> Vec<Outgoing> {
        let found = read(self.keyspace, |keys| {
            let last_of = |subject: &str| -> Result<Option<u64>, String> {
                if subject.contains(['*', '>']) {
                    return Ok(last_per_subject(keys, stream, Some(subject))?.into_iter().max());
                }
                Ok(subject_seqs(keys, stream, subject)?.into_iter().max())
            };
            let seq = match (subject.as_deref(), text(request, "last_by_subj"), text(request, "next_by_subj")) {
                (Some(subject), _, _) | (None, Some(subject), _) => last_of(subject)?,
                (None, None, Some(subject)) => {
                    let from = integer(request, "seq", 1).max(1) as u64;
                    let mut next = None;
                    keys.visit_prefix(&message_prefix(stream), &mut |found, raw| {
                        let seq = seq_at_end(found);
                        if seq >= from && StoredMessage::decode(raw).is_some_and(|message| subject_matches(subject, &message.subject)) {
                            next = Some(seq);
                            return Ok(false);
                        }
                        Ok(true)
                    })?;
                    next
                }
                (None, None, None) => Some(integer(request, "seq", 0).max(0) as u64),
            };
            let Some(seq) = seq else {
                return Ok(None);
            };
            Ok(keys.get(&message_key(stream, seq))?.and_then(|raw| StoredMessage::decode(&raw)).map(|message| (seq, message)))
        });
        let Ok(Some((seq, message))) = found else {
            return vec![Outgoing { subject: reply.to_owned(), reply: None, headers: MESSAGE_NOT_FOUND.to_vec(), payload: Vec::new() }];
        };
        let mut headers = format!(
            "NATS/1.0\r\nNats-Stream: {stream}\r\nNats-Subject: {}\r\nNats-Sequence: {seq}\r\nNats-Time-Stamp: {}\r\n",
            message.subject,
            timestamp(message.time)
        );
        if let Ok(original) = std::str::from_utf8(&message.headers) {
            for line in original.lines().skip(1).filter(|line| !line.trim().is_empty()) {
                headers.push_str(line);
                headers.push_str("\r\n");
            }
        }
        headers.push_str("\r\n");
        vec![Outgoing { subject: reply.to_owned(), reply: None, headers: headers.into_bytes(), payload: message.payload }]
    }

    fn delete_message(&self, name: &str, request: &Node) -> Result<String, String> {
        write(self.keyspace, |keys| {
            let Some(config) = stream_config(&*keys, name)? else {
                return Ok(error_json("stream_msg_delete_response", STREAM_NOT_FOUND));
            };
            if config.deny_delete {
                return Ok(error_json("stream_msg_delete_response", (400, 10_003, "message delete is not allowed on this stream")));
            }
            let mut state = stream_state(&*keys, name)?;
            let removed = remove_message(keys, name, integer(request, "seq", 0).max(0) as u64, &mut state)?;
            keys.put(&key(STREAM_STATE, &[name.as_bytes()]), &state.encode())?;
            Ok(if removed {
                "{\"type\":\"io.nats.jetstream.api.v1.stream_msg_delete_response\",\"success\":true}".to_owned()
            } else {
                error_json("stream_msg_delete_response", NO_MESSAGE_FOUND)
            })
        })
    }

    fn create_consumer(&self, stream: &str, name: Option<&str>, request: &Node) -> Result<String, String> {
        let config = match ConsumerConfig::from_request(name, request, now_nanos()) {
            Ok(config) => config,
            Err(problem) => return Ok(error_json("consumer_create_response", problem)),
        };
        let push = config.deliver_subject.is_some();
        let consumer_name = config.name.clone();
        let answer = write(self.keyspace, |keys| {
            let Some(stream_config) = stream_config(&*keys, stream)? else {
                return Ok(error_json("consumer_create_response", STREAM_NOT_FOUND));
            };
            let names = consumer_names(&*keys, stream)?;
            if !names.contains(&config.name) && names.len() >= MOST_CONSUMERS {
                return Ok(error_json("consumer_create_response", BAD_REQUEST));
            }
            if stream_config.retention == Retention::WorkQueue
                && names.iter().filter(|other| **other != config.name).any(|_| config.filter_subject.is_none())
            {
                return Ok(error_json("consumer_create_response", (400, 10_099, "multiple non-filtered consumers not allowed on workqueue stream")));
            }
            let stream_state = stream_state(&*keys, stream)?;
            let (config, state) = match consumer(&*keys, stream, &config.name)? {
                Some((existing, state)) => (ConsumerConfig { created: existing.created, ..config }, state),
                None => {
                    let start = match config.deliver_policy {
                        DeliverPolicy::All => stream_state.first_seq.saturating_sub(1),
                        DeliverPolicy::Last => stream_state.last_seq.saturating_sub(1),
                        DeliverPolicy::New | DeliverPolicy::LastPerSubject => stream_state.last_seq,
                        DeliverPolicy::ByStartSequence(seq) => seq.saturating_sub(1),
                    };
                    let mut state = ConsumerState {
                        delivered_stream_seq: start,
                        ack_floor_stream_seq: start,
                        ..ConsumerState::default()
                    };
                    if config.deliver_policy == DeliverPolicy::LastPerSubject {
                        for seq in last_per_subject(&*keys, stream, config.filter_subject.as_deref())? {
                            state.consumer_seq += 1;
                            state.pending.push(Pending { stream_seq: seq, consumer_seq: state.consumer_seq, deliveries: 0, deadline: 0 });
                        }
                    }
                    (config, state)
                }
            };
            keys.put(&key(CONSUMER, &[stream.as_bytes(), config.name.as_bytes()]), config.stored().as_bytes())?;
            keys.put(&key(CONSUMER_STATE, &[stream.as_bytes(), config.name.as_bytes()]), &state.encode())?;
            Ok(consumer_info_json("consumer_create_response", stream, &config, &state, &stream_state, 0))
        })?;
        if push && !answer.contains("\"error\"") {
            let delivered = self.deliver_push(stream, &consumer_name);
            self.pending_push.lock().unwrap_or_else(PoisonError::into_inner).extend(delivered);
        }
        Ok(answer)
    }

    fn consumer_info(&self, stream: &str, name: &str) -> Result<String, String> {
        let waiting = self.waiting_for(stream, name);
        read(self.keyspace, |keys| {
            if stream_config(keys, stream)?.is_none() {
                return Ok(error_json("consumer_info_response", STREAM_NOT_FOUND));
            }
            Ok(match consumer(keys, stream, name)? {
                Some((config, state)) => consumer_info_json("consumer_info_response", stream, &config, &state, &stream_state(keys, stream)?, waiting),
                None => error_json("consumer_info_response", CONSUMER_NOT_FOUND),
            })
        })
    }

    fn delete_consumer(&self, stream: &str, name: &str) -> Result<String, String> {
        write(self.keyspace, |keys| {
            if consumer(&*keys, stream, name)?.is_none() {
                return Ok(error_json("consumer_delete_response", CONSUMER_NOT_FOUND));
            }
            keys.delete(&key(CONSUMER, &[stream.as_bytes(), name.as_bytes()]))?;
            keys.delete(&key(CONSUMER_STATE, &[stream.as_bytes(), name.as_bytes()]))?;
            Ok("{\"type\":\"io.nats.jetstream.api.v1.consumer_delete_response\",\"success\":true}".to_owned())
        })
    }

    fn consumer_list(&self, stream: &str, detailed: bool) -> Result<String, String> {
        read(self.keyspace, |keys| {
            if stream_config(keys, stream)?.is_none() {
                return Ok(error_json("consumer_list_response", STREAM_NOT_FOUND));
            }
            let names = consumer_names(keys, stream)?;
            let state = stream_state(keys, stream)?;
            let items: Vec<String> = if detailed {
                let mut items = Vec::new();
                for name in &names {
                    if let Some((config, consumer_state)) = consumer(keys, stream, name)? {
                        items.push(consumer_info_json("consumer_info_response", stream, &config, &consumer_state, &state, 0));
                    }
                }
                items
            } else {
                names.iter().map(|name| quote(name)).collect()
            };
            Ok(format!(
                "{{\"type\":\"io.nats.jetstream.api.v1.{}\",\"total\":{},\"offset\":0,\"limit\":{MOST_CONSUMERS},\"{}\":[{}]}}",
                if detailed { "consumer_list_response" } else { "consumer_names_response" },
                items.len(),
                "consumers",
                items.join(",")
            ))
        })
    }

    fn waiting_for(&self, stream: &str, consumer: &str) -> usize {
        let now = Instant::now();
        let mut waiting = self.waiting.lock().unwrap_or_else(PoisonError::into_inner);
        waiting.retain(|pull| pull.deadline > now);
        waiting.iter().filter(|pull| pull.stream == stream && pull.consumer == consumer).count()
    }

    fn next_messages(&self, stream: &str, name: &str, most: usize, to: &str) -> Result<Vec<Outgoing>, String> {
        write(self.keyspace, |keys| {
            let Some((config, mut state)) = consumer(&*keys, stream, name)? else {
                return Ok(Vec::new());
            };
            let stream_state = stream_state(&*keys, stream)?;
            let now = now_nanos();
            let mut chosen: Vec<(Pending, StoredMessage)> = Vec::new();
            {
                let mut expired: Vec<usize> = (0..state.pending.len())
                    .filter(|at| state.pending[*at].deadline <= now)
                    .filter(|at| config.ack_policy != AckPolicy::None || state.pending[*at].deliveries == 0)
                    .collect();
                expired.sort_by_key(|at| state.pending[*at].stream_seq);
                for at in expired {
                    if chosen.len() >= most {
                        break;
                    }
                    let mut pending = state.pending[at];
                    let message = keys.get(&message_key(stream, pending.stream_seq))?.and_then(|raw| StoredMessage::decode(&raw));
                    let exhausted = config.max_deliver > 0 && i64::from(pending.deliveries) >= config.max_deliver;
                    let Some(message) = message.filter(|_| !exhausted) else {
                        state.pending[at].deadline = i64::MAX;
                        continue;
                    };
                    if pending.deliveries > 0 {
                        state.redelivered += 1;
                    }
                    pending.deliveries += 1;
                    pending.deadline = if config.ack_policy == AckPolicy::None { i64::MAX } else { now.saturating_add(config.ack_wait) };
                    state.pending[at] = pending;
                    chosen.push((pending, message));
                }
                state.pending.retain(|pending| pending.deadline != i64::MAX);
            }
            let room = if config.ack_policy == AckPolicy::None {
                most
            } else {
                most.min((config.max_ack_pending.max(1) as usize).saturating_sub(state.pending.len()))
            };
            let mut fresh: Vec<(u64, StoredMessage)> = Vec::new();
            if chosen.len() < most && room > 0 {
                let prefix = message_prefix(stream);
                let after = state.delivered_stream_seq;
                let wanted = room.min(most - chosen.len());
                let mut last_seen = after;
                keys.visit_prefix(&prefix, &mut |found, raw| {
                    let seq = found.get(prefix.len()..).and_then(|raw| raw.try_into().ok()).map_or(0, u64::from_be_bytes);
                    if seq <= after {
                        return Ok(true);
                    }
                    last_seen = seq;
                    if let Some(message) = StoredMessage::decode(raw)
                        && config.wants(&message.subject)
                    {
                        fresh.push((seq, message));
                    }
                    Ok(fresh.len() < wanted)
                })?;
                state.delivered_stream_seq = if fresh.len() < wanted { stream_state.last_seq.max(last_seen) } else { last_seen };
            }
            for (seq, message) in fresh {
                state.consumer_seq += 1;
                let pending = Pending { stream_seq: seq, consumer_seq: state.consumer_seq, deliveries: 1, deadline: now.saturating_add(config.ack_wait) };
                if config.ack_policy != AckPolicy::None {
                    state.pending.push(pending);
                }
                chosen.push((pending, message));
            }
            state.settle_floor();
            keys.put(&key(CONSUMER_STATE, &[stream.as_bytes(), name.as_bytes()]), &state.encode())?;
            let after = unsent(&state, &stream_state);
            let count = chosen.len() as u64;
            Ok(chosen
                .into_iter()
                .enumerate()
                .map(|(at, (pending, message))| {
                    let left = after + count - 1 - at as u64;
                    let (headers, payload) = if config.headers_only {
                        (headers_only(message.headers, message.payload.len()), Vec::new())
                    } else {
                        (message.headers, message.payload)
                    };
                    Outgoing {
                        subject: to.to_owned(),
                        reply: Some(ack_subject(stream, name, &pending, message.time, left)),
                        headers,
                        payload,
                    }
                })
                .collect())
        })
    }

    fn pull(&self, stream: &str, name: &str, reply: &str, body: &str, request: &Node) -> Vec<Outgoing> {
        let exists = read(self.keyspace, |keys| consumer(keys, stream, name)).ok().flatten();
        if exists.is_none() {
            return vec![Outgoing {
                subject: reply.to_owned(),
                reply: None,
                headers: b"NATS/1.0 404 Consumer Not Found\r\n\r\n".to_vec(),
                payload: Vec::new(),
            }];
        }
        let batch = match body.parse::<usize>() {
            Ok(batch) => batch,
            Err(_) => integer(request, "batch", 1).max(1) as usize,
        }
        .min(MOST_BATCH);
        let no_wait = flag(request, "no_wait");
        let expires = integer(request, "expires", 0);
        let mut delivered = self.next_messages(stream, name, batch, reply).unwrap_or_default();
        let remaining = batch.saturating_sub(delivered.len());
        if remaining > 0 {
            if no_wait {
                if delivered.is_empty() {
                    delivered.push(Outgoing { subject: reply.to_owned(), reply: None, headers: NO_MESSAGES.to_vec(), payload: Vec::new() });
                }
            } else {
                let wait = if expires > 0 { Duration::from_nanos(expires as u64).min(LONGEST_PULL_WAIT) } else { DEFAULT_PULL_WAIT };
                let mut waiting = self.waiting.lock().unwrap_or_else(PoisonError::into_inner);
                let now = Instant::now();
                waiting.retain(|pull| pull.deadline > now);
                if waiting.len() < MOST_WAITING_PULLS {
                    waiting.push(WaitingPull {
                        stream: stream.to_owned(),
                        consumer: name.to_owned(),
                        reply: reply.to_owned(),
                        remaining,
                        deadline: now + wait,
                    });
                }
            }
        }
        delivered
    }

    fn deliver_push(&self, stream: &str, name: &str) -> Vec<Outgoing> {
        let target = read(self.keyspace, |keys| Ok(consumer(keys, stream, name)?.and_then(|(config, _)| config.deliver_subject))).ok().flatten();
        let delivered = match target {
            Some(target) => self.next_messages(stream, name, PUSH_BATCH, &target).unwrap_or_default(),
            None => Vec::new(),
        };
        if !delivered.is_empty() {
            self.last_push
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert((stream.to_owned(), name.to_owned()), Instant::now());
        }
        delivered
    }

    /// What JetStream sends as time passes, called about once a second: a `408` to each pull
    /// whose wait ran out, push deliveries whose acknowledgement wait ran out, and idle
    /// heartbeats to push consumers that asked for them and got nothing in that time.
    pub(crate) fn tick(&self) -> Vec<Outgoing> {
        let now = Instant::now();
        let mut out: Vec<Outgoing> = Vec::new();
        {
            let mut waiting = self.waiting.lock().unwrap_or_else(PoisonError::into_inner);
            for pull in waiting.iter().filter(|pull| pull.deadline <= now) {
                out.push(Outgoing {
                    subject: pull.reply.clone(),
                    reply: None,
                    headers: format!("NATS/1.0 408 Request Timeout\r\nNats-Pending-Messages: {}\r\nNats-Pending-Bytes: 0\r\n\r\n", pull.remaining).into_bytes(),
                    payload: Vec::new(),
                });
            }
            waiting.retain(|pull| pull.deadline > now);
        }
        let push: Vec<(String, ConsumerConfig, ConsumerState)> = read(self.keyspace, |keys| {
            let mut found = Vec::new();
            for stream in stream_names(keys)? {
                for name in consumer_names(keys, &stream)? {
                    if let Some((config, state)) = consumer(keys, &stream, &name)?
                        && config.deliver_subject.is_some()
                    {
                        found.push((stream.clone(), config, state));
                    }
                }
            }
            Ok(found)
        })
        .unwrap_or_default();
        for (stream, config, state) in push {
            let delivered = self.deliver_push(&stream, &config.name);
            if !delivered.is_empty() || config.idle_heartbeat <= 0 {
                out.extend(delivered);
                continue;
            }
            let interval = Duration::from_nanos(config.idle_heartbeat as u64);
            let mut last_push = self.last_push.lock().unwrap_or_else(PoisonError::into_inner);
            let last = last_push.entry((stream.clone(), config.name.clone())).or_insert(now);
            if now.duration_since(*last) >= interval
                && let Some(target) = &config.deliver_subject
            {
                *last = now;
                out.push(Outgoing {
                    subject: target.clone(),
                    reply: None,
                    headers: format!(
                        "NATS/1.0 100 Idle Heartbeat\r\nNats-Last-Consumer: {}\r\nNats-Last-Stream: {}\r\n\r\n",
                        state.consumer_seq, state.delivered_stream_seq
                    )
                    .into_bytes(),
                    payload: Vec::new(),
                });
            }
        }
        out
    }

    fn serve_waiting(&self, stream: &str) -> Vec<Outgoing> {
        let now = Instant::now();
        let ready: Vec<(usize, String, String, usize)> = {
            let mut waiting = self.waiting.lock().unwrap_or_else(PoisonError::into_inner);
            waiting.retain(|pull| pull.deadline > now);
            waiting
                .iter()
                .enumerate()
                .filter(|(_, pull)| pull.stream == stream)
                .map(|(at, pull)| (at, pull.consumer.clone(), pull.reply.clone(), pull.remaining))
                .collect()
        };
        let mut out = Vec::new();
        let mut served: Vec<(String, usize)> = Vec::new();
        for (_, consumer, reply, remaining) in ready {
            let delivered = self.next_messages(stream, &consumer, remaining, &reply).unwrap_or_default();
            served.push((reply, delivered.len()));
            out.extend(delivered);
        }
        let mut waiting = self.waiting.lock().unwrap_or_else(PoisonError::into_inner);
        for (reply, count) in served {
            if let Some(pull) = waiting.iter_mut().find(|pull| pull.reply == reply && pull.stream == stream) {
                pull.remaining = pull.remaining.saturating_sub(count);
            }
        }
        waiting.retain(|pull| pull.remaining > 0);
        out
    }

    fn stream_consumers(&self, stream: &str) -> Vec<(String, bool)> {
        read(self.keyspace, |keys| {
            let mut found = Vec::new();
            for name in consumer_names(keys, stream)? {
                if let Some((config, _)) = consumer(keys, stream, &name)? {
                    found.push((name, config.deliver_subject.is_some()));
                }
            }
            Ok(found)
        })
        .unwrap_or_default()
    }

    /// Stores a message published to a stream's subject: the acknowledgement for `reply` and
    /// the deliveries to push consumers and waiting pulls; nothing when no stream takes it.
    pub(crate) fn store(&self, subject: &str, reply: Option<&str>, headers: &[u8], payload: &[u8]) -> Vec<Outgoing> {
        let Some(stream) = self.stream_for(subject) else {
            return Vec::new();
        };
        self.store_in(stream, subject, reply, headers, payload, 0)
    }

    fn followers(&self, origin: &str) -> Vec<(String, Origin, bool)> {
        read(self.keyspace, |keys| {
            let mut found = Vec::new();
            for name in stream_names(keys)? {
                if let Some(config) = stream_config(keys, &name)? {
                    let mirrored = config.mirror.is_some();
                    for taken in config.origins().filter(|taken| taken.name == origin) {
                        found.push((name.clone(), taken.clone(), mirrored));
                    }
                }
            }
            Ok(found)
        })
        .unwrap_or_default()
    }

    fn copy_to_followers(&self, origin: &str, seq: u64, message: &StoredMessage, hops: usize) -> Vec<Outgoing> {
        if hops >= MOST_SOURCE_HOPS {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (follower, taken, mirrored) in self.followers(origin) {
            if !taken.takes(seq, &message.subject) {
                continue;
            }
            let headers = if mirrored {
                message.headers.clone()
            } else {
                with_header(&message.headers, "Nats-Stream-Source", &format!("{origin} {seq}"))
            };
            out.extend(self.store_in(follower, &message.subject, None, &headers, &message.payload, hops + 1));
        }
        out
    }

    fn backfill(&self, follower: &str) -> Vec<Outgoing> {
        let Ok(Some(config)) = read(self.keyspace, |keys| stream_config(keys, follower)) else {
            return Vec::new();
        };
        let mirrored = config.mirror.is_some();
        let mut out = Vec::new();
        for taken in config.origins() {
            let messages = read(self.keyspace, |keys| {
                Ok(keys
                    .scan(&message_prefix(&taken.name))?
                    .into_iter()
                    .filter_map(|(found, raw)| Some((seq_at_end(&found), StoredMessage::decode(&raw)?)))
                    .collect::<Vec<_>>())
            })
            .unwrap_or_default();
            for (seq, message) in messages {
                if !taken.takes(seq, &message.subject) {
                    continue;
                }
                let headers = if mirrored {
                    message.headers.clone()
                } else {
                    with_header(&message.headers, "Nats-Stream-Source", &format!("{} {seq}", taken.name))
                };
                out.extend(self.store_in(follower.to_owned(), &message.subject, None, &headers, &message.payload, 1));
            }
        }
        out
    }

    fn store_in(&self, stream: String, subject: &str, reply: Option<&str>, headers: &[u8], payload: &[u8], hops: usize) -> Vec<Outgoing> {
        let now = now_nanos();
        let message_id = header_value(headers, "Nats-Msg-Id").map(str::to_owned);
        let expected_stream = header_value(headers, "Nats-Expected-Stream").map(str::to_owned);
        let rollup = header_value(headers, "Nats-Rollup").map(str::to_ascii_lowercase);
        let stored = write(self.keyspace, |keys| {
            let Some(config) = stream_config(&*keys, &stream)? else {
                return Ok(Err(STREAM_NOT_FOUND));
            };
            if expected_stream.as_deref().is_some_and(|expected| expected != stream) {
                return Ok(Err((400, 10_060, "expected stream does not match")));
            }
            let message = StoredMessage { time: now, subject: subject.to_owned(), headers: headers.to_vec(), payload: payload.to_vec() };
            if config.max_msg_size >= 0 && message.payload.len() as i64 > config.max_msg_size {
                return Ok(Err((400, 10_054, "message size exceeds maximum allowed")));
            }
            if let Some(id) = &message_id {
                let dedupe = key(DEDUPLICATION, &[stream.as_bytes(), id.as_bytes()]);
                if let Some(raw) = keys.get(&dedupe)? {
                    let mut at = 0;
                    let (seq, time) = (take_u64(&raw, &mut at).unwrap_or(0), take_u64(&raw, &mut at).unwrap_or(0) as i64);
                    if now - time <= config.duplicate_window {
                        return Ok(Ok((seq, true)));
                    }
                }
            }
            let mut state = stream_state(&*keys, &stream)?;
            let full = (config.max_msgs >= 0 && state.messages >= config.max_msgs as u64)
                || (config.max_bytes >= 0 && state.bytes + message.size() > config.max_bytes as u64);
            if config.discard_new && full {
                return Ok(Err((503, 10_077, "maximum messages exceeded")));
            }
            let seq = state.last_seq + 1;
            if state.messages == 0 {
                state.first_seq = seq;
            }
            state.last_seq = seq;
            state.messages += 1;
            state.bytes += message.size();
            keys.put(&message_key(&stream, seq), &message.encode())?;
            keys.put(&subject_key(&stream, subject, seq), &[])?;
            match rollup.as_deref() {
                None => {}
                Some(_) if !config.allow_rollup => return Err(ROLLUP_REFUSED.to_owned()),
                Some("all") => {
                    let older: Vec<u64> = keys.scan(&message_prefix(&stream))?.iter().map(|(found, _)| seq_at_end(found)).filter(|older| *older < seq).collect();
                    for older in older {
                        remove_message(keys, &stream, older, &mut state)?;
                    }
                }
                Some(_) => {
                    for older in subject_seqs(&*keys, &stream, subject)?.into_iter().filter(|older| *older < seq) {
                        remove_message(keys, &stream, older, &mut state)?;
                    }
                }
            }
            if config.max_msgs_per_subject > 0 {
                let seqs = subject_seqs(&*keys, &stream, subject)?;
                let extra = seqs.len().saturating_sub(config.max_msgs_per_subject as usize);
                for older in seqs.into_iter().take(extra) {
                    remove_message(keys, &stream, older, &mut state)?;
                }
            }
            if let Some(id) = &message_id {
                let mut value = seq.to_be_bytes().to_vec();
                value.extend_from_slice(&now.to_be_bytes());
                keys.put(&key(DEDUPLICATION, &[stream.as_bytes(), id.as_bytes()]), &value)?;
            }
            enforce_limits(keys, &config, &mut state, now)?;
            if config.retention == Retention::Interest && !interested(&*keys, &stream, subject)? {
                remove_message(keys, &stream, seq, &mut state)?;
            }
            keys.put(&key(STREAM_STATE, &[stream.as_bytes()]), &state.encode())?;
            Ok(Ok((seq, false)))
        });
        let mut out = Vec::new();
        if let Some(reply) = reply {
            let answer = match &stored {
                Ok(Ok((seq, duplicate))) => format!(
                    "{{\"stream\":{},\"seq\":{seq}{}}}",
                    quote(&stream),
                    if *duplicate { ",\"duplicate\":true" } else { "" }
                ),
                Ok(Err(problem)) => error_json("pub_ack_response", *problem),
                Err(problem) if problem == ROLLUP_REFUSED => error_json("pub_ack_response", ROLLUP_NOT_ALLOWED),
                Err(_) => error_json("pub_ack_response", NO_STREAM_FOR_SUBJECT),
            };
            out.push(Outgoing::json(reply, answer));
        }
        if let Ok(Ok((seq, false))) = stored {
            for (consumer, push) in self.stream_consumers(&stream) {
                if push {
                    out.extend(self.deliver_push(&stream, &consumer));
                }
            }
            out.extend(self.serve_waiting(&stream));
            let message = StoredMessage { time: now, subject: subject.to_owned(), headers: headers.to_vec(), payload: payload.to_vec() };
            out.extend(self.copy_to_followers(&stream, seq, &message, hops));
        }
        out
    }

    fn acknowledge(&self, tokens: &str, reply: Option<&str>, payload: &[u8]) -> Vec<Outgoing> {
        let parts: Vec<&str> = tokens.split('.').collect();
        let [stream, name, _deliveries, stream_seq, ..] = parts.as_slice() else {
            return Vec::new();
        };
        let Ok(stream_seq) = stream_seq.parse::<u64>() else {
            return Vec::new();
        };
        let body = std::str::from_utf8(payload).unwrap_or_default().trim();
        let (verb, rest) = body.split_once(' ').unwrap_or((body, ""));
        let now = now_nanos();
        let outcome = write(self.keyspace, |keys| {
            let Some((config, mut state)) = consumer(&*keys, stream, name)? else {
                return Ok(false);
            };
            let mut settled: Vec<u64> = Vec::new();
            match verb {
                "" | "+ACK" | "+NXT" | "+TERM" => {
                    let all = config.ack_policy == AckPolicy::All && verb != "+TERM";
                    let covered = |pending: &Pending| if all { pending.stream_seq <= stream_seq } else { pending.stream_seq == stream_seq };
                    settled.extend(state.pending.iter().filter(|pending| covered(pending)).map(|pending| pending.stream_seq));
                    state.pending.retain(|pending| !covered(pending));
                    if verb != "+TERM"
                        && let Some(stream_config) = stream_config(&*keys, stream)?
                        && stream_config.retention == Retention::WorkQueue
                    {
                        let mut stream_state = stream_state(&*keys, stream)?;
                        remove_message(keys, stream, stream_seq, &mut stream_state)?;
                        keys.put(&key(STREAM_STATE, &[stream.as_bytes()]), &stream_state.encode())?;
                    }
                }
                "-NAK" => {
                    let delay = parse_document(rest.trim())
                        .ok()
                        .map_or(0, |options| integer(&options, "delay", 0).max(0));
                    if let Some(pending) = state.pending.iter_mut().find(|pending| pending.stream_seq == stream_seq) {
                        pending.deadline = now.saturating_add(delay);
                    }
                }
                "+WPI" => {
                    if let Some(pending) = state.pending.iter_mut().find(|pending| pending.stream_seq == stream_seq) {
                        pending.deadline = now.saturating_add(config.ack_wait);
                    }
                }
                _ => return Ok(false),
            }
            state.settle_floor();
            keys.put(&key(CONSUMER_STATE, &[stream.as_bytes(), name.as_bytes()]), &state.encode())?;
            if !settled.is_empty()
                && let Some(stream_config) = stream_config(&*keys, stream)?
                && stream_config.retention == Retention::Interest
            {
                let mut stream_state = stream_state(&*keys, stream)?;
                for seq in settled {
                    let subject = keys.get(&message_key(stream, seq))?.and_then(|raw| StoredMessage::decode(&raw)).map(|message| message.subject);
                    if let Some(subject) = subject
                        && acknowledged_by_all(&*keys, stream, seq, &subject)?
                    {
                        remove_message(keys, stream, seq, &mut stream_state)?;
                    }
                }
                keys.put(&key(STREAM_STATE, &[stream.as_bytes()]), &stream_state.encode())?;
            }
            Ok(config.deliver_subject.is_some())
        });
        let mut out = Vec::new();
        if let Some(reply) = reply {
            if verb == "+NXT" {
                let batch = parse_document(rest.trim()).ok().map_or(1, |options| integer(&options, "batch", 1).max(1) as usize);
                out.extend(self.next_messages(stream, name, batch.min(MOST_BATCH), reply).unwrap_or_default());
            } else {
                out.push(Outgoing { subject: reply.to_owned(), reply: None, headers: Vec::new(), payload: Vec::new() });
            }
        }
        if let Ok(true) = outcome {
            out.extend(self.deliver_push(stream, name));
        }
        out
    }

    fn take_pushed(&self) -> Vec<Outgoing> {
        std::mem::take(&mut *self.pending_push.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_keyspace::MemoryKeyspace;

    fn answer(jetstream: &JetStream<'_>, subject: &str, body: &str) -> String {
        let out = jetstream.request(subject, Some("_INBOX.1"), body.as_bytes());
        String::from_utf8(out.first().map(|outgoing| outgoing.payload.clone()).unwrap_or_default()).unwrap_or_default()
    }

    #[test]
    fn streams_store_published_messages_and_pull_consumers_deliver_until_acknowledged() {
        let keyspace = MemoryKeyspace::default();
        let jetstream = JetStream::new(&keyspace);
        let created = answer(&jetstream, "$JS.API.STREAM.CREATE.ORDERS", r#"{"name":"ORDERS","subjects":["orders.>"]}"#);
        assert!(created.contains("stream_create_response") && !created.contains("error"), "{created}");
        let acked = jetstream.store("orders.new", Some("_INBOX.2"), b"", b"first");
        assert_eq!(String::from_utf8_lossy(&acked[0].payload), r#"{"stream":"ORDERS","seq":1}"#);
        jetstream.store("orders.new", None, b"", b"second");
        assert!(jetstream.store("other", Some("_INBOX.3"), b"", b"x").is_empty());
        let consumer = answer(&jetstream, "$JS.API.CONSUMER.DURABLE.CREATE.ORDERS.worker", r#"{"stream_name":"ORDERS","config":{"durable_name":"worker"}}"#);
        assert!(consumer.contains("\"num_pending\":2"), "{consumer}");
        let pulled = jetstream.request("$JS.API.CONSUMER.MSG.NEXT.ORDERS.worker", Some("_INBOX.4"), br#"{"batch":5,"no_wait":true}"#);
        assert_eq!(pulled.len(), 2);
        assert_eq!(pulled[0].payload, b"first");
        let ack = pulled[0].reply.clone().unwrap_or_default();
        assert!(ack.starts_with("$JS.ACK.ORDERS.worker.1.1.1."), "{ack}");
        jetstream.request(&ack, None, b"+ACK");
        let info = answer(&jetstream, "$JS.API.CONSUMER.INFO.ORDERS.worker", "");
        assert!(info.contains("\"num_ack_pending\":1"), "{info}");
        let empty = jetstream.request("$JS.API.CONSUMER.MSG.NEXT.ORDERS.worker", Some("_INBOX.5"), br#"{"batch":1,"no_wait":true}"#);
        assert_eq!(empty[0].headers, NO_MESSAGES);
    }

    #[test]
    fn subjects_overlap_through_wildcards() {
        assert!(subjects_overlap("$O.files.>", "$O.*.M.>"));
        assert!(subjects_overlap("orders.*", "orders.new"));
        assert!(!subjects_overlap("orders.*", "orders.new.eu"));
        assert!(!subjects_overlap("$KV.settings.>", "$O.*.M.>"));
        assert!(subjects_overlap("a.b", "a.b"));
        assert!(!subjects_overlap("a", "a.b"));
    }

    #[test]
    fn an_object_store_bucket_keeps_chunks_and_rolled_up_metadata() {
        let keyspace = MemoryKeyspace::default();
        let jetstream = JetStream::new(&keyspace);
        answer(
            &jetstream,
            "$JS.API.STREAM.CREATE.OBJ_files",
            r#"{"name":"OBJ_files","subjects":["$O.files.C.>","$O.files.M.>"],"discard":"new","allow_rollup_hdrs":true,"allow_direct":true}"#,
        );
        answer(&jetstream, "$JS.API.STREAM.CREATE.ORDERS", r#"{"name":"ORDERS","subjects":["orders.>"]}"#);
        let names = answer(&jetstream, "$JS.API.STREAM.NAMES", r#"{"subject":"$O.*.M.>"}"#);
        assert!(names.contains("\"streams\":[\"OBJ_files\"]") && names.contains("\"total\":1"), "{names}");
        let rollup = b"NATS/1.0\r\nNats-Rollup: sub\r\n\r\n";
        jetstream.store("$O.files.C.first", None, b"", b"old");
        jetstream.store("$O.files.M.cmVwb3J0", None, rollup, br#"{"name":"report","nuid":"first","size":3,"chunks":1}"#);
        jetstream.store("$O.files.C.second", None, b"", b"new-1");
        jetstream.store("$O.files.C.second", None, b"", b"new-2");
        jetstream.store("$O.files.M.cmVwb3J0", None, rollup, br#"{"name":"report","nuid":"second","size":10,"chunks":2}"#);
        answer(&jetstream, "$JS.API.STREAM.PURGE.OBJ_files", r#"{"filter":"$O.files.C.first"}"#);
        let meta = jetstream.request("$JS.API.DIRECT.GET.OBJ_files.$O.files.M.cmVwb3J0", Some("_INBOX.7"), b"");
        assert!(String::from_utf8_lossy(&meta[0].payload).contains("\"nuid\":\"second\""));
        let info = answer(&jetstream, "$JS.API.STREAM.INFO.OBJ_files", "");
        assert!(info.contains("\"messages\":3"), "{info}");
    }

    #[test]
    fn mirrors_and_sources_copy_what_their_origins_store() {
        let keyspace = MemoryKeyspace::default();
        let jetstream = JetStream::new(&keyspace);
        answer(&jetstream, "$JS.API.STREAM.CREATE.ORDERS", r#"{"name":"ORDERS","subjects":["orders.>"]}"#);
        jetstream.store("orders.eu.1", None, b"", b"first");
        jetstream.store("orders.us.1", None, b"", b"second");
        let mirror = answer(&jetstream, "$JS.API.STREAM.CREATE.BACKUP", r#"{"name":"BACKUP","mirror":{"name":"ORDERS"}}"#);
        assert!(mirror.contains("\"mirror\":{\"name\":\"ORDERS\"}"), "{mirror}");
        answer(
            &jetstream,
            "$JS.API.STREAM.CREATE.EU",
            r#"{"name":"EU","sources":[{"name":"ORDERS","filter_subject":"orders.eu.>"}],"allow_direct":true}"#,
        );
        jetstream.store("orders.eu.2", None, b"", b"third");
        let backup = answer(&jetstream, "$JS.API.STREAM.INFO.BACKUP", "");
        assert!(backup.contains("\"messages\":3"), "{backup}");
        let europe = answer(&jetstream, "$JS.API.STREAM.INFO.EU", "");
        assert!(europe.contains("\"messages\":2"), "{europe}");
        let copied = jetstream.request("$JS.API.DIRECT.GET.EU.orders.eu.2", Some("_INBOX.8"), b"");
        assert_eq!(copied[0].payload, b"third");
        assert!(String::from_utf8_lossy(&copied[0].headers).contains("Nats-Stream-Source: ORDERS 3"));
        let refused = answer(&jetstream, "$JS.API.STREAM.CREATE.BAD", r#"{"name":"BAD","subjects":["bad.>"],"mirror":{"name":"ORDERS"}}"#);
        assert!(refused.contains("\"error\""), "{refused}");
        let looped = answer(&jetstream, "$JS.API.STREAM.CREATE.SELF", r#"{"name":"SELF","sources":[{"name":"SELF"}]}"#);
        assert!(looped.contains("\"error\""), "{looped}");
    }

    #[test]
    fn key_value_buckets_keep_one_value_per_key_and_read_it_directly() {
        let keyspace = MemoryKeyspace::default();
        let jetstream = JetStream::new(&keyspace);
        answer(
            &jetstream,
            "$JS.API.STREAM.CREATE.KV_settings",
            r#"{"name":"KV_settings","subjects":["$KV.settings.>"],"max_msgs_per_subject":1,"allow_direct":true,"allow_rollup_hdrs":true}"#,
        );
        jetstream.store("$KV.settings.color", None, b"", b"red");
        jetstream.store("$KV.settings.color", None, b"", b"blue");
        let got = jetstream.request("$JS.API.DIRECT.GET.KV_settings.$KV.settings.color", Some("_INBOX.6"), b"");
        assert_eq!(got[0].payload, b"blue");
        assert!(String::from_utf8_lossy(&got[0].headers).contains("Nats-Sequence: 2"));
        let info = answer(&jetstream, "$JS.API.STREAM.INFO.KV_settings", "");
        assert!(info.contains("\"messages\":1"), "{info}");
    }
}
