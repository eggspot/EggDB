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
    let packet = Packet { kind: first >> 4, flags: first & 0x0f, body: buffer[at..at + length].to_vec() };
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
        let slice = self.bytes.get(self.at..self.at + length).ok_or("a packet ends early")?;
        self.at += length;
        Ok(slice)
    }

    pub(crate) fn string(&mut self) -> Parsed<String> {
        String::from_utf8(self.binary()?.to_vec()).map_err(|_| "a string is not UTF-8".to_owned())
    }

    pub(crate) fn skip_properties(&mut self) -> Parsed<()> {
        let mut length = 0usize;
        let mut multiplier = 1usize;
        for _ in 0..4 {
            let byte = self.byte()?;
            length += usize::from(byte & 0x7f) * multiplier;
            if byte & 0x80 == 0 {
                let end = self.at.checked_add(length).filter(|end| *end <= self.bytes.len()).ok_or("properties run past the packet")?;
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
