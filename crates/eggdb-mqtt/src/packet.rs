pub(crate) const CONNECT: u8 = 1;
pub(crate) const CONNACK: u8 = 2;
pub(crate) const PUBLISH: u8 = 3;
pub(crate) const PUBACK: u8 = 4;
pub(crate) const PUBREC: u8 = 5;
pub(crate) const PUBREL: u8 = 6;
pub(crate) const PUBCOMP: u8 = 7;
pub(crate) const SUBSCRIBE: u8 = 8;
pub(crate) const SUBACK: u8 = 9;
pub(crate) const UNSUBSCRIBE: u8 = 10;
pub(crate) const UNSUBACK: u8 = 11;
pub(crate) const PINGREQ: u8 = 12;
pub(crate) const PINGRESP: u8 = 13;
pub(crate) const DISCONNECT: u8 = 14;

pub(crate) const MAX_PACKET_BYTES: usize = 16 << 20;

/// A whole packet: its first byte and its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Packet {
    pub(crate) kind: u8,
    pub(crate) flags: u8,
    pub(crate) body: Vec<u8>,
}

/// The bytes the packet at the front of `buffer` takes once whole: what its fixed header's
/// remaining length declares once that has arrived, or what `buffer` already holds.
pub(crate) fn pending_packet_bytes(buffer: &[u8]) -> usize {
    let mut length = 0usize;
    let mut multiplier = 1usize;
    for (at, &byte) in buffer.iter().enumerate().skip(1).take(4) {
        length = length.saturating_add(usize::from(byte & 0x7f).saturating_mul(multiplier));
        if byte & 0x80 == 0 {
            return (at + 1).saturating_add(length).max(buffer.len());
        }
        multiplier = multiplier.saturating_mul(128);
    }
    buffer.len()
}

/// Takes one packet from the front of `buffer`, or `None` when more bytes are needed.
pub(crate) fn take_packet(buffer: &mut Vec<u8>) -> Result<Option<Packet>, String> {
    let Some(&first) = buffer.first() else {
        return Ok(None);
    };
    let mut length = 0usize;
    let mut multiplier = 1usize;
    let mut at = 1;
    loop {
        let Some(&byte) = buffer.get(at) else {
            return Ok(None);
        };
        length += usize::from(byte & 0x7f) * multiplier;
        at += 1;
        if byte & 0x80 == 0 {
            break;
        }
        multiplier *= 128;
        if at > 4 {
            return Err("a remaining length is longer than four bytes".to_owned());
        }
    }
    if length > MAX_PACKET_BYTES {
        return Err("a packet is larger than 16 MiB".to_owned());
    }
    if buffer.len() < at + length {
        return Ok(None);
    }
    let packet = Packet {
        kind: first >> 4,
        flags: first & 0x0f,
        body: buffer[at..at + length].to_vec(),
    };
    buffer.drain(..at + length);
    Ok(Some(packet))
}

pub(crate) fn encode(kind: u8, flags: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![(kind << 4) | flags];
    let mut length = body.len();
    loop {
        let mut byte = (length % 128) as u8;
        length /= 128;
        if length > 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if length == 0 {
            break;
        }
    }
    out.extend_from_slice(body);
    out
}

pub(crate) struct Fields<'b> {
    bytes: &'b [u8],
    pub(crate) at: usize,
}

pub(crate) type Parsed<T> = Result<T, String>;

/// The MQTT 5 properties of a `PUBLISH` the broker keeps and forwards.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PublishProperties {
    pub(crate) payload_format: Option<u8>,
    pub(crate) message_expiry: Option<u32>,
    pub(crate) topic_alias: Option<u16>,
    pub(crate) content_type: Option<String>,
    pub(crate) response_topic: Option<String>,
    pub(crate) correlation_data: Option<Vec<u8>>,
    pub(crate) user_properties: Vec<(String, String)>,
}

const MAX_USER_PROPERTIES: usize = 256;

pub(crate) fn put_variable(out: &mut Vec<u8>, mut value: usize) {
    loop {
        let mut byte = (value % 128) as u8;
        value /= 128;
        if value > 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

impl PublishProperties {
    /// The properties written as a `PUBLISH` to a subscriber carries them (without the topic
    /// alias, which is per connection), with `expiry` as the remaining message expiry.
    pub(crate) fn encoded(&self, expiry: Option<u32>) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(format) = self.payload_format {
            out.extend_from_slice(&[0x01, format]);
        }
        if let Some(seconds) = expiry {
            out.push(0x02);
            out.extend_from_slice(&seconds.to_be_bytes());
        }
        if let Some(content_type) = &self.content_type {
            out.push(0x03);
            put_string(&mut out, content_type.as_bytes());
        }
        if let Some(topic) = &self.response_topic {
            out.push(0x08);
            put_string(&mut out, topic.as_bytes());
        }
        if let Some(data) = &self.correlation_data {
            out.push(0x09);
            put_string(&mut out, data);
        }
        for (name, value) in &self.user_properties {
            out.push(0x26);
            put_string(&mut out, name.as_bytes());
            put_string(&mut out, value.as_bytes());
        }
        out
    }
}

impl<'b> Fields<'b> {
    pub(crate) fn new(bytes: &'b [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    pub(crate) fn byte(&mut self) -> Parsed<u8> {
        let byte = *self.bytes.get(self.at).ok_or("a packet ends early")?;
        self.at += 1;
        Ok(byte)
    }

    pub(crate) fn short(&mut self) -> Parsed<u16> {
        Ok(u16::from(self.byte()?) << 8 | u16::from(self.byte()?))
    }

    pub(crate) fn binary(&mut self) -> Parsed<&'b [u8]> {
        let length = usize::from(self.short()?);
        let slice = self
            .bytes
            .get(self.at..self.at + length)
            .ok_or("a packet ends early")?;
        self.at += length;
        Ok(slice)
    }

    pub(crate) fn string(&mut self) -> Parsed<String> {
        String::from_utf8(self.binary()?.to_vec()).map_err(|_| "a string is not UTF-8".to_owned())
    }

    pub(crate) fn variable(&mut self) -> Parsed<usize> {
        let mut value = 0usize;
        let mut multiplier = 1usize;
        for _ in 0..4 {
            let byte = self.byte()?;
            value += usize::from(byte & 0x7f) * multiplier;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            multiplier *= 128;
        }
        Err("a variable byte integer is longer than four bytes".to_owned())
    }

    fn four(&mut self) -> Parsed<u32> {
        Ok(u32::from(self.short()?) << 16 | u32::from(self.short()?))
    }

    pub(crate) fn publish_properties(&mut self) -> Parsed<PublishProperties> {
        let length = self.variable()?;
        let end = self
            .at
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or("properties run past the packet")?;
        let mut inner = Fields::new(&self.bytes[self.at..end]);
        self.at = end;
        let mut properties = PublishProperties::default();
        while !inner.finished() {
            let id = inner.variable()?;
            match id {
                0x01 => properties.payload_format = Some(inner.byte()?),
                0x02 => properties.message_expiry = Some(inner.four()?),
                0x23 => properties.topic_alias = Some(inner.short()?),
                0x03 => properties.content_type = Some(inner.string()?),
                0x08 => {
                    let topic = inner.string()?;
                    if topic.contains(['+', '#']) {
                        return Err("a response topic may not hold wildcards".to_owned());
                    }
                    properties.response_topic = Some(topic);
                }
                0x09 => properties.correlation_data = Some(inner.binary()?.to_vec()),
                0x26 => {
                    if properties.user_properties.len() >= MAX_USER_PROPERTIES {
                        return Err("a PUBLISH carries too many user properties".to_owned());
                    }
                    let name = inner.string()?;
                    let value = inner.string()?;
                    properties.user_properties.push((name, value));
                }
                0x0B => {
                    return Err("a client may not send a subscription identifier in PUBLISH".to_owned());
                }
                other => return Err(format!("property {other:#04x} is not allowed in PUBLISH")),
            }
        }
        Ok(properties)
    }

    /// Reads a CONNECT's properties, returning its Session Expiry Interval (0x11) if given.
    pub(crate) fn connect_session_expiry(&mut self) -> Parsed<Option<u32>> {
        let length = self.variable()?;
        let end = self
            .at
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or("properties run past the packet")?;
        let mut inner = Fields::new(&self.bytes[self.at..end]);
        self.at = end;
        let mut expiry = None;
        while !inner.finished() {
            match inner.variable()? {
                0x11 => expiry = Some(inner.four()?),
                0x27 => {
                    inner.four()?;
                }
                0x21 | 0x22 => {
                    inner.short()?;
                }
                0x17 | 0x19 => {
                    inner.byte()?;
                }
                0x15 => {
                    inner.string()?;
                }
                0x16 => {
                    inner.binary()?;
                }
                0x26 => {
                    inner.string()?;
                    inner.string()?;
                }
                other => return Err(format!("property {other:#04x} is not allowed in CONNECT")),
            }
        }
        Ok(expiry)
    }

    pub(crate) fn skip_properties(&mut self) -> Parsed<()> {
        let mut length = 0usize;
        let mut multiplier = 1usize;
        for _ in 0..4 {
            let byte = self.byte()?;
            length += usize::from(byte & 0x7f) * multiplier;
            if byte & 0x80 == 0 {
                let end = self
                    .at
                    .checked_add(length)
                    .filter(|end| *end <= self.bytes.len())
                    .ok_or("properties run past the packet")?;
                self.at = end;
                return Ok(());
            }
            multiplier *= 128;
        }
        Err("a property length is longer than four bytes".to_owned())
    }

    pub(crate) fn rest(&self) -> &'b [u8] {
        self.bytes.get(self.at..).unwrap_or_default()
    }

    pub(crate) fn finished(&self) -> bool {
        self.at >= self.bytes.len()
    }
}

pub(crate) fn put_string(out: &mut Vec<u8>, text: &[u8]) {
    out.extend_from_slice(&(text.len().min(65_535) as u16).to_be_bytes());
    out.extend_from_slice(&text[..text.len().min(65_535)]);
}

/// Whether an MQTT topic filter matches a topic name (`+` one level, `#` the rest; topics
/// starting with `$` match only filters that start with the same level).
pub(crate) fn filter_matches(filter: &str, topic: &str) -> bool {
    if topic.starts_with('$') && !filter.starts_with('$') {
        return false;
    }
    let mut filter_levels = filter.split('/');
    let mut topic_levels = topic.split('/');
    loop {
        match (filter_levels.next(), topic_levels.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => {}
            (Some(level), Some(name)) if level == name => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// Whether `filter` is a valid topic filter.
pub(crate) fn valid_filter(filter: &str) -> bool {
    if filter.is_empty() || filter.contains('\0') {
        return false;
    }
    let levels: Vec<&str> = filter.split('/').collect();
    levels.iter().enumerate().all(|(at, level)| match *level {
        "#" => at == levels.len() - 1,
        "+" => true,
        other => !other.contains(['+', '#']),
    })
}
