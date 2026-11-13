//! AMQP 1.0 messages in the broker's one stored form, AMQP 0-9-1's (exchange, routing key,
//! basic properties, body), so a message keeps what it carries whichever protocol sends or
//! receives it (D1062). The properties, application properties and message annotations of an
//! AMQP 1.0 message are kept byte for byte in `x-amqp-1.0-*` headers and also mapped to the
//! nearest 0-9-1 properties and headers; an AMQP 1.0 receiver gets the kept sections back as they
//! were sent, and a message from 0-9-1 gets sections built from its properties.

use super::value::{Value, decode_all, decode_first, encode};
use crate::field_table::{BasicProperties, FieldValue};
use eggdb_queues::selector::SelectorValue;

const HEADER_SECTION: u64 = 0x70;
const DELIVERY_ANNOTATIONS: u64 = 0x71;
const MESSAGE_ANNOTATIONS: u64 = 0x72;
const PROPERTIES_SECTION: u64 = 0x73;
const APPLICATION_PROPERTIES: u64 = 0x74;
const DATA_SECTION: u64 = 0x75;
const SEQUENCE_SECTION: u64 = 0x76;
const VALUE_SECTION: u64 = 0x77;
const FOOTER: u64 = 0x78;
const TIMESTAMP: u8 = 0x83;
const DOUBLE: u8 = 0x82;
const FLOAT: u8 = 0x72;
const UUID: u8 = 0x98;

const KEPT_PROPERTIES: &str = "x-amqp-1.0-properties";
const KEPT_APPLICATION_PROPERTIES: &str = "x-amqp-1.0-app-properties";
const KEPT_MESSAGE_ANNOTATIONS: &str = "x-amqp-1.0-message-annotations";
const KEPT_FOOTER: &str = "x-amqp-1.0-footer";
const BODY_KIND: &str = "x-amqp-1.0-body-kind";
const BODY_STRING: &str = "string";
const BODY_SECTIONS: &str = "sections";
const KEPT_PREFIX: &str = "x-amqp-1.0-";

/// Where a JMS client says a message is going, from its `x-opt-jms-dest` annotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DestinationKind {
    Queue,
    Topic,
}

/// An AMQP 1.0 message read for storing.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Inbound {
    pub(crate) properties: Vec<u8>,
    pub(crate) body: Vec<u8>,
    pub(crate) to: Option<String>,
    pub(crate) destination_kind: Option<DestinationKind>,
}

fn section_inner(section: &Value) -> &Value {
    match section {
        Value::Described(_, inner) => inner,
        other => other,
    }
}

fn identifier_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) | Value::Symbol(text) => Some(text.clone()),
        Value::Ulong(number) => Some(number.to_string()),
        Value::Binary(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        Value::Fixed(UUID, bytes) => Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect()),
        _ => None,
    }
}

fn timestamp_ms(value: &Value) -> Option<i64> {
    match value {
        Value::Fixed(TIMESTAMP, bytes) => bytes.as_slice().try_into().ok().map(i64::from_be_bytes),
        _ => None,
    }
}

fn timestamp(ms: i64) -> Value {
    Value::Fixed(TIMESTAMP, ms.to_be_bytes().to_vec())
}

fn field_value(value: &Value) -> Option<FieldValue> {
    Some(match value {
        Value::Null => FieldValue::Void,
        Value::Bool(flag) => FieldValue::Boolean(*flag),
        Value::Ubyte(number) => FieldValue::Integer(i128::from(*number)),
        Value::Ushort(number) => FieldValue::Integer(i128::from(*number)),
        Value::Uint(number) => FieldValue::Integer(i128::from(*number)),
        Value::Ulong(number) => FieldValue::Integer(i128::from(*number)),
        Value::Signed(number) => FieldValue::Integer(i128::from(*number)),
        Value::Fixed(DOUBLE, bytes) => FieldValue::Float(f64::from_be_bytes(bytes.as_slice().try_into().ok()?)),
        Value::Fixed(FLOAT, bytes) => FieldValue::Float(f64::from(f32::from_be_bytes(bytes.as_slice().try_into().ok()?))),
        Value::Fixed(TIMESTAMP, _) => FieldValue::Timestamp(u64::try_from(timestamp_ms(value)? / 1000).ok()?),
        Value::String(text) | Value::Symbol(text) => FieldValue::Bytes(text.as_bytes().to_vec()),
        Value::Binary(bytes) => FieldValue::Bytes(bytes.clone()),
        _ => return None,
    })
}

fn amqp_value(value: &FieldValue) -> Option<Value> {
    Some(match value {
        FieldValue::Void => Value::Null,
        FieldValue::Boolean(flag) => Value::Bool(*flag),
        FieldValue::Integer(number) => Value::Signed(i64::try_from(*number).ok()?),
        FieldValue::Float(number) => Value::Fixed(DOUBLE, number.to_be_bytes().to_vec()),
        FieldValue::Timestamp(seconds) => timestamp(i64::try_from(*seconds).ok()?.saturating_mul(1000)),
        FieldValue::Bytes(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => Value::String(text.to_owned()),
            Err(_) => Value::Binary(bytes.clone()),
        },
        _ => return None,
    })
}

fn map_entries(section: &Value) -> Vec<(String, &Value)> {
    match section_inner(section) {
        Value::Map(pairs) => pairs
            .iter()
            .filter_map(|(key, value)| Some((key.text()?.to_owned(), value)))
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether `address` names a topic by its prefix (`/topic/` or `topic://`).
pub(crate) fn names_topic(address: &str) -> bool {
    address.starts_with("/topic/") || address.starts_with("topic://")
}

/// `address` without the queue or topic prefix clients may write.
pub(crate) fn routable_address(address: &str) -> String {
    address
        .trim_start_matches("/queues/")
        .trim_start_matches("/queue/")
        .trim_start_matches("queue://")
        .trim_start_matches("/topic/")
        .trim_start_matches("topic://")
        .to_owned()
}

/// Reads the sections of an AMQP 1.0 message for storing; `None` for the parts it lacks.
pub(crate) fn inbound(payload: &[u8]) -> Result<Inbound, String> {
    let mut properties = BasicProperties::default();
    let mut headers: Vec<(String, FieldValue)> = Vec::new();
    let mut body = Vec::new();
    let mut body_kind: Option<&str> = None;
    let mut to = None;
    let mut destination_kind = None;
    let mut ttl_ms = None;
    let mut at = 0;
    while at < payload.len() {
        let (section, used) = decode_first(&payload[at..])?;
        let raw = &payload[at..at + used];
        at += used;
        match section.descriptor() {
            Some(HEADER_SECTION) => {
                properties.delivery_mode = Some(if section.field(0).flag().unwrap_or(false) { 2 } else { 1 });
                properties.priority = section.field(1).unsigned().and_then(|priority| u8::try_from(priority).ok());
                ttl_ms = section.field(2).unsigned();
            }
            Some(DELIVERY_ANNOTATIONS) => {}
            Some(MESSAGE_ANNOTATIONS) => {
                for (name, value) in map_entries(&section) {
                    if name == "x-opt-jms-dest" {
                        destination_kind = match value {
                            Value::Signed(1 | 3) | Value::Ubyte(1 | 3) => Some(DestinationKind::Topic),
                            Value::Signed(0 | 2) | Value::Ubyte(0 | 2) => Some(DestinationKind::Queue),
                            _ => None,
                        };
                    }
                    if let Some(converted) = field_value(value) {
                        headers.push((name, converted));
                    }
                }
                headers.push((KEPT_MESSAGE_ANNOTATIONS.to_owned(), FieldValue::Bytes(raw.to_vec())));
            }
            Some(PROPERTIES_SECTION) => {
                properties.message_id = identifier_text(section.field(0));
                properties.user_id = match section.field(1) {
                    Value::Binary(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
                    _ => None,
                };
                to = section.field(2).text().map(str::to_owned);
                properties.message_type = section.field(3).text().map(str::to_owned);
                properties.reply_to = section.field(4).text().map(str::to_owned);
                properties.correlation_id = identifier_text(section.field(5));
                properties.content_type = section.field(6).text().map(str::to_owned);
                properties.content_encoding = section.field(7).text().map(str::to_owned);
                properties.timestamp = timestamp_ms(section.field(9)).and_then(|ms| u64::try_from(ms / 1000).ok());
                headers.push((KEPT_PROPERTIES.to_owned(), FieldValue::Bytes(raw.to_vec())));
            }
            Some(APPLICATION_PROPERTIES) => {
                for (name, value) in map_entries(&section) {
                    if let Some(converted) = field_value(value) {
                        headers.push((name, converted));
                    }
                }
                headers.push((KEPT_APPLICATION_PROPERTIES.to_owned(), FieldValue::Bytes(raw.to_vec())));
            }
            Some(DATA_SECTION) if body_kind.is_none() || body_kind == Some("data") => {
                if let Value::Binary(bytes) = section_inner(&section) {
                    body.extend_from_slice(bytes);
                }
                body_kind = Some("data");
            }
            Some(VALUE_SECTION) if body_kind.is_none() => match section_inner(&section) {
                Value::String(text) => {
                    body.extend_from_slice(text.as_bytes());
                    body_kind = Some(BODY_STRING);
                }
                _ => {
                    body.extend_from_slice(raw);
                    body_kind = Some(BODY_SECTIONS);
                }
            },
            Some(DATA_SECTION | SEQUENCE_SECTION | VALUE_SECTION) => {
                if body_kind == Some(BODY_STRING) {
                    let text = std::mem::take(&mut body);
                    encode(&Value::Described(Box::new(Value::Ulong(VALUE_SECTION)), Box::new(Value::String(String::from_utf8_lossy(&text).into_owned()))), &mut body);
                }
                if body_kind == Some("data") {
                    let bytes = std::mem::take(&mut body);
                    encode(&Value::Described(Box::new(Value::Ulong(DATA_SECTION)), Box::new(Value::Binary(bytes))), &mut body);
                }
                body.extend_from_slice(raw);
                body_kind = Some(BODY_SECTIONS);
            }
            Some(FOOTER) => headers.push((KEPT_FOOTER.to_owned(), FieldValue::Bytes(raw.to_vec()))),
            _ => return Err("the message has a section AMQP 1.0 does not define".to_owned()),
        }
    }
    if let Some(kind) = body_kind.filter(|kind| *kind != "data") {
        headers.push((BODY_KIND.to_owned(), FieldValue::Bytes(kind.as_bytes().to_vec())));
    }
    if let Some(ttl) = ttl_ms {
        properties.expiration = Some(ttl.to_string());
    }
    if !headers.is_empty() {
        properties.headers = Some(headers);
    }
    if destination_kind.is_none() && to.as_deref().is_some_and(names_topic) {
        destination_kind = Some(DestinationKind::Topic);
    }
    Ok(Inbound {
        properties: properties.encode(),
        body,
        to: to.as_deref().map(routable_address),
        destination_kind,
    })
}

fn kept<'h>(headers: &'h [(String, FieldValue)], name: &str) -> Option<&'h [u8]> {
    headers.iter().find_map(|(key, value)| match value {
        FieldValue::Bytes(bytes) if key == name => Some(bytes.as_slice()),
        _ => None,
    })
}

fn described(code: u64, inner: Value) -> Value {
    Value::Described(Box::new(Value::Ulong(code)), Box::new(inner))
}

/// The sections an AMQP 1.0 receiver gets for a stored message: a header with the delivery
/// count, then the sections an AMQP 1.0 sender wrote, or ones built from 0-9-1 properties.
pub(crate) fn transfer_payload(queue: &str, id: u64, receive_count: u32, properties: &[u8], body: &[u8]) -> Vec<u8> {
    let properties = BasicProperties::decode(properties);
    let headers = properties.headers.clone().unwrap_or_default();
    let mut out = Vec::with_capacity(body.len() + 128);
    let mut header = vec![
        Value::Bool(properties.delivery_mode != Some(1)),
        properties.priority.map_or(Value::Null, Value::Ubyte),
        Value::Null,
        Value::Bool(receive_count <= 1),
        Value::Uint(receive_count.saturating_sub(1)),
    ];
    if let Some(ttl) = properties.expiration.as_deref().and_then(|ttl| ttl.trim().parse::<u32>().ok()) {
        header[2] = Value::Uint(ttl);
    }
    encode(&Value::described(HEADER_SECTION, header), &mut out);
    match kept(&headers, KEPT_MESSAGE_ANNOTATIONS) {
        Some(raw) => out.extend_from_slice(raw),
        None => {
            let annotations: Vec<(Value, Value)> = headers
                .iter()
                .filter(|(name, _)| name.starts_with("x-opt-"))
                .filter_map(|(name, value)| Some((Value::Symbol(name.clone()), amqp_value(value)?)))
                .collect();
            if !annotations.is_empty() {
                encode(&described(MESSAGE_ANNOTATIONS, Value::Map(annotations)), &mut out);
            }
        }
    }
    match kept(&headers, KEPT_PROPERTIES) {
        Some(raw) => out.extend_from_slice(raw),
        None => {
            let text = |value: &Option<String>| value.clone().map_or(Value::Null, Value::String);
            let symbol = |value: &Option<String>| value.clone().map_or(Value::Null, Value::Symbol);
            let fields = vec![
                Value::String(properties.message_id.clone().unwrap_or_else(|| format!("{queue}-{id}"))),
                properties.user_id.clone().map_or(Value::Null, |user| Value::Binary(user.into_bytes())),
                Value::Null,
                text(&properties.message_type),
                text(&properties.reply_to),
                text(&properties.correlation_id),
                symbol(&properties.content_type),
                symbol(&properties.content_encoding),
                Value::Null,
                properties
                    .timestamp
                    .and_then(|seconds| i64::try_from(seconds).ok())
                    .map_or(Value::Null, |seconds| timestamp(seconds.saturating_mul(1000))),
            ];
            encode(&Value::described(PROPERTIES_SECTION, fields), &mut out);
        }
    }
    match kept(&headers, KEPT_APPLICATION_PROPERTIES) {
        Some(raw) => out.extend_from_slice(raw),
        None => {
            let application: Vec<(Value, Value)> = headers
                .iter()
                .filter(|(name, _)| !name.starts_with(KEPT_PREFIX) && !name.starts_with("x-opt-"))
                .filter_map(|(name, value)| Some((Value::String(name.clone()), amqp_value(value)?)))
                .collect();
            if !application.is_empty() {
                encode(&described(APPLICATION_PROPERTIES, Value::Map(application)), &mut out);
            }
        }
    }
    match kept(&headers, BODY_KIND) {
        Some(kind) if kind == BODY_STRING.as_bytes() => {
            encode(&described(VALUE_SECTION, Value::String(String::from_utf8_lossy(body).into_owned())), &mut out);
        }
        Some(kind) if kind == BODY_SECTIONS.as_bytes() && decode_all(body).is_ok() => out.extend_from_slice(body),
        _ => encode(&described(DATA_SECTION, Value::Binary(body.to_vec())), &mut out),
    }
    if let Some(raw) = kept(&headers, KEPT_FOOTER) {
        out.extend_from_slice(raw);
    }
    out
}

fn selector_value(value: &FieldValue) -> SelectorValue {
    match value {
        FieldValue::Boolean(flag) => SelectorValue::Boolean(*flag),
        FieldValue::Integer(number) => SelectorValue::Number(*number as f64),
        FieldValue::Float(number) => SelectorValue::Number(*number),
        FieldValue::Timestamp(seconds) => SelectorValue::Number(*seconds as f64 * 1000.0),
        FieldValue::Bytes(bytes) => SelectorValue::Text(String::from_utf8_lossy(bytes).into_owned()),
        _ => SelectorValue::Null,
    }
}

/// What a JMS selector reads of a stored message: the JMS header fields it may name
/// (`JMSDeliveryMode`, `JMSPriority`, `JMSMessageID`, `JMSTimestamp`, `JMSCorrelationID`,
/// `JMSType`) and the message's properties by name.
pub(crate) fn selector_field(properties: &BasicProperties, name: &str) -> SelectorValue {
    let text = |value: &Option<String>| value.clone().map_or(SelectorValue::Null, SelectorValue::Text);
    match name {
        "JMSDeliveryMode" => SelectorValue::Text(if properties.delivery_mode == Some(1) { "NON_PERSISTENT" } else { "PERSISTENT" }.to_owned()),
        "JMSPriority" => SelectorValue::Number(f64::from(properties.priority.unwrap_or(4))),
        "JMSMessageID" => text(&properties.message_id),
        "JMSTimestamp" => properties.timestamp.map_or(SelectorValue::Null, |seconds| SelectorValue::Number(seconds as f64 * 1000.0)),
        "JMSCorrelationID" => text(&properties.correlation_id),
        "JMSType" => text(&properties.message_type),
        _ => properties
            .headers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find(|(key, _)| key == name && !key.starts_with(KEPT_PREFIX))
            .map_or(SelectorValue::Null, |(_, value)| selector_value(value)),
    }
}
