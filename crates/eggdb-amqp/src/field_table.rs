const MAX_DEPTH: usize = 8;
const MAX_ENTRIES: usize = 1024;

/// A value in an AMQP 0-9-1 field table, as RabbitMQ writes them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FieldValue {
    Boolean(bool),
    Integer(i128),
    Float(f64),
    Decimal(u8, i32),
    Bytes(Vec<u8>),
    Array(Vec<FieldValue>),
    Timestamp(u64),
    Table(Vec<(String, FieldValue)>),
    Void,
}

impl FieldValue {
    /// Whether two values are equal as a headers exchange compares them: integers of any width
    /// by value, the rest by kind and content.
    pub(crate) fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Float(a), Self::Float(b)) => a == b,
            (Self::Float(a), Self::Integer(b)) | (Self::Integer(b), Self::Float(a)) => {
                *a == *b as f64
            }
            _ => self == other,
        }
    }
}

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    fn take(&mut self, count: usize) -> Result<&'b [u8], String> {
        let end = self
            .at
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or("a field table ends early")?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        self.take(N)?
            .try_into()
            .map_err(|_| "a field table ends early".to_owned())
    }

    fn shortstr(&mut self) -> Result<String, String> {
        let length = usize::from(self.array::<1>()?[0]);
        String::from_utf8(self.take(length)?.to_vec()).map_err(|_| "a field name is not UTF-8".to_owned())
    }

    fn longstr(&mut self) -> Result<&'b [u8], String> {
        let length = u32::from_be_bytes(self.array()?) as usize;
        self.take(length)
    }

    fn value(&mut self, depth: usize) -> Result<FieldValue, String> {
        if depth > MAX_DEPTH {
            return Err("a field table nests too deeply".to_owned());
        }
        let kind = self.array::<1>()?[0];
        Ok(match kind {
            b't' => FieldValue::Boolean(self.array::<1>()?[0] != 0),
            b'b' => FieldValue::Integer(i128::from(i8::from_be_bytes(self.array()?))),
            b'B' => FieldValue::Integer(i128::from(u8::from_be_bytes(self.array()?))),
            b's' => FieldValue::Integer(i128::from(i16::from_be_bytes(self.array()?))),
            b'u' => FieldValue::Integer(i128::from(u16::from_be_bytes(self.array()?))),
            b'I' => FieldValue::Integer(i128::from(i32::from_be_bytes(self.array()?))),
            b'i' => FieldValue::Integer(i128::from(u32::from_be_bytes(self.array()?))),
            b'l' | b'L' => FieldValue::Integer(i128::from(i64::from_be_bytes(self.array()?))),
            b'f' => FieldValue::Float(f64::from(f32::from_be_bytes(self.array()?))),
            b'd' => FieldValue::Float(f64::from_be_bytes(self.array()?)),
            b'D' => {
                let scale = self.array::<1>()?[0];
                FieldValue::Decimal(scale, i32::from_be_bytes(self.array()?))
            }
            b'S' | b'x' => FieldValue::Bytes(self.longstr()?.to_vec()),
            b'A' => {
                let mut inner = Reader {
                    bytes: self.longstr()?,
                    at: 0,
                };
                let mut items = Vec::new();
                while inner.at < inner.bytes.len() {
                    if items.len() >= MAX_ENTRIES {
                        return Err("a field array is too long".to_owned());
                    }
                    items.push(inner.value(depth + 1)?);
                }
                FieldValue::Array(items)
            }
            b'T' => FieldValue::Timestamp(u64::from_be_bytes(self.array()?)),
            b'F' => FieldValue::Table(table_at(self.longstr()?, depth + 1)?),
            b'V' => FieldValue::Void,
            other => return Err(format!("a field table holds an unknown type {}", char::from(other))),
        })
    }
}

fn table_at(bytes: &[u8], depth: usize) -> Result<Vec<(String, FieldValue)>, String> {
    let mut reader = Reader { bytes, at: 0 };
    let mut entries = Vec::new();
    while reader.at < bytes.len() {
        if entries.len() >= MAX_ENTRIES {
            return Err("a field table has too many entries".to_owned());
        }
        let name = reader.shortstr()?;
        let value = reader.value(depth)?;
        entries.push((name, value));
    }
    Ok(entries)
}

/// The entries of a field table's bytes (without its length prefix).
pub(crate) fn decode_table(bytes: &[u8]) -> Result<Vec<(String, FieldValue)>, String> {
    table_at(bytes, 0)
}

/// The `headers` of a content header's properties (the property flags, then the properties
/// in order: content type, content encoding, headers, ...), or none.
pub(crate) fn message_headers(properties: &[u8]) -> Vec<(String, FieldValue)> {
    let mut reader = Reader {
        bytes: properties,
        at: 0,
    };
    let Ok(flags) = reader.array::<2>().map(u16::from_be_bytes) else {
        return Vec::new();
    };
    let has = |bit: u16| flags & (1 << bit) != 0;
    if has(15) && reader.shortstr().is_err() {
        return Vec::new();
    }
    if has(14) && reader.shortstr().is_err() {
        return Vec::new();
    }
    if !has(13) {
        return Vec::new();
    }
    reader
        .longstr()
        .and_then(decode_table)
        .unwrap_or_default()
}

/// A message's `expiration` property in milliseconds, when it has a readable one.
pub(crate) fn message_expiration(properties: &[u8]) -> Option<u64> {
    let mut reader = Reader {
        bytes: properties,
        at: 0,
    };
    let flags = reader.array::<2>().map(u16::from_be_bytes).ok()?;
    let has = |bit: u16| flags & (1 << bit) != 0;
    if !has(8) {
        return None;
    }
    for bit in [15, 14] {
        if has(bit) {
            reader.shortstr().ok()?;
        }
    }
    if has(13) {
        reader.longstr().ok()?;
    }
    for bit in [12, 11] {
        if has(bit) {
            reader.take(1).ok()?;
        }
    }
    for bit in [10, 9] {
        if has(bit) {
            reader.shortstr().ok()?;
        }
    }
    reader.shortstr().ok()?.trim().parse().ok()
}

fn encode_value(value: &FieldValue, out: &mut Vec<u8>) {
    match value {
        FieldValue::Boolean(flag) => {
            out.push(b't');
            out.push(u8::from(*flag));
        }
        FieldValue::Integer(number) => {
            out.push(b'l');
            out.extend_from_slice(&(*number as i64).to_be_bytes());
        }
        FieldValue::Float(number) => {
            out.push(b'd');
            out.extend_from_slice(&number.to_be_bytes());
        }
        FieldValue::Decimal(scale, digits) => {
            out.push(b'D');
            out.push(*scale);
            out.extend_from_slice(&digits.to_be_bytes());
        }
        FieldValue::Bytes(bytes) => {
            out.push(b'S');
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            out.extend_from_slice(bytes);
        }
        FieldValue::Array(items) => {
            let mut inner = Vec::new();
            for item in items {
                encode_value(item, &mut inner);
            }
            out.push(b'A');
            out.extend_from_slice(&(inner.len() as u32).to_be_bytes());
            out.extend_from_slice(&inner);
        }
        FieldValue::Timestamp(seconds) => {
            out.push(b'T');
            out.extend_from_slice(&seconds.to_be_bytes());
        }
        FieldValue::Table(entries) => {
            let inner = encode_table(entries);
            out.push(b'F');
            out.extend_from_slice(&(inner.len() as u32).to_be_bytes());
            out.extend_from_slice(&inner);
        }
        FieldValue::Void => out.push(b'V'),
    }
}

/// A field table's bytes (without its length prefix).
pub(crate) fn encode_table(entries: &[(String, FieldValue)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in entries {
        let name = &name.as_bytes()[..name.len().min(255)];
        out.push(name.len() as u8);
        out.extend_from_slice(name);
        encode_value(value, &mut out);
    }
    out
}

/// The content header's properties with `headers` as their headers table, the other
/// properties kept as they were; `None` when the properties cannot be read.
pub(crate) fn with_headers(properties: &[u8], headers: &[(String, FieldValue)]) -> Option<Vec<u8>> {
    let mut reader = Reader {
        bytes: properties,
        at: 0,
    };
    let flags = reader.array::<2>().map(u16::from_be_bytes).ok()?;
    let has = |bit: u16| flags & (1 << bit) != 0;
    for bit in [15, 14] {
        if has(bit) {
            reader.shortstr().ok()?;
        }
    }
    let before_headers = reader.at;
    if has(13) {
        reader.longstr().ok()?;
    }
    let after_headers = reader.at;
    let table = encode_table(headers);
    let mut out = Vec::with_capacity(properties.len() + table.len() + 4);
    out.extend_from_slice(&(flags | (1 << 13)).to_be_bytes());
    out.extend_from_slice(&properties[2..before_headers]);
    out.extend_from_slice(&(table.len() as u32).to_be_bytes());
    out.extend_from_slice(&table);
    out.extend_from_slice(&properties[after_headers..]);
    Some(out)
}

/// The headers with RabbitMQ's dead-letter record added: the `x-death` entry for this queue and
/// reason counted up and moved first (or a new one with count 1), and the `x-first-death-*` and
/// `x-last-death-*` headers.
pub(crate) fn with_death(
    mut headers: Vec<(String, FieldValue)>,
    queue: &str,
    reason: &str,
    exchange: &str,
    routing_key: &str,
    now_seconds: u64,
) -> Vec<(String, FieldValue)> {
    let text = |value: &str| FieldValue::Bytes(value.as_bytes().to_vec());
    let mut deaths = match headers.iter().position(|(name, _)| name == "x-death") {
        Some(at) => match headers.remove(at).1 {
            FieldValue::Array(items) => items,
            _ => Vec::new(),
        },
        None => Vec::new(),
    };
    let same = |entry: &FieldValue| match entry {
        FieldValue::Table(fields) => {
            text_named(fields, "queue").as_deref() == Some(queue)
                && text_named(fields, "reason").as_deref() == Some(reason)
        }
        _ => false,
    };
    let count = match deaths.iter().position(same) {
        Some(at) => match deaths.remove(at) {
            FieldValue::Table(fields) => integer_named(&fields, "count").unwrap_or(0) + 1,
            _ => 1,
        },
        None => 1,
    };
    deaths.insert(
        0,
        FieldValue::Table(vec![
            ("count".to_owned(), FieldValue::Integer(i128::from(count))),
            ("reason".to_owned(), text(reason)),
            ("queue".to_owned(), text(queue)),
            ("time".to_owned(), FieldValue::Timestamp(now_seconds)),
            ("exchange".to_owned(), text(exchange)),
            ("routing-keys".to_owned(), FieldValue::Array(vec![text(routing_key)])),
        ]),
    );
    headers.insert(0, ("x-death".to_owned(), FieldValue::Array(deaths)));
    for (name, value) in [
        ("x-first-death-reason", reason),
        ("x-first-death-queue", queue),
        ("x-first-death-exchange", exchange),
    ] {
        if !headers.iter().any(|(known, _)| known == name) {
            headers.push((name.to_owned(), text(value)));
        }
    }
    for (name, value) in [
        ("x-last-death-reason", reason),
        ("x-last-death-queue", queue),
        ("x-last-death-exchange", exchange),
    ] {
        headers.retain(|(known, _)| known != name);
        headers.push((name.to_owned(), text(value)));
    }
    headers
}

/// A message's `priority` property, 0 when it has none or it cannot be read.
pub(crate) fn message_priority(properties: &[u8]) -> u8 {
    let mut reader = Reader {
        bytes: properties,
        at: 0,
    };
    let Ok(flags) = reader.array::<2>().map(u16::from_be_bytes) else {
        return 0;
    };
    let has = |bit: u16| flags & (1 << bit) != 0;
    if !has(11) {
        return 0;
    }
    for bit in [15, 14] {
        if has(bit) && reader.shortstr().is_err() {
            return 0;
        }
    }
    if has(13) && reader.longstr().is_err() {
        return 0;
    }
    if has(12) && reader.take(1).is_err() {
        return 0;
    }
    reader.take(1).map_or(0, |priority| priority[0])
}

/// The non-negative integer value of argument or header `name`, if it has one.
pub(crate) fn integer_named(table: &[(String, FieldValue)], name: &str) -> Option<u64> {
    table.iter().find(|(key, _)| key == name).and_then(|(_, value)| match value {
        FieldValue::Integer(number) => u64::try_from(*number).ok(),
        FieldValue::Bytes(text) => std::str::from_utf8(text).ok()?.trim().parse().ok(),
        _ => None,
    })
}

/// The text value of argument or header `name`, if it has one.
pub(crate) fn text_named(table: &[(String, FieldValue)], name: &str) -> Option<String> {
    table.iter().find(|(key, _)| key == name).and_then(|(_, value)| match value {
        FieldValue::Bytes(text) => String::from_utf8(text.clone()).ok(),
        _ => None,
    })
}

/// Whether a headers exchange binding with `arguments` takes a message with `headers`:
/// `x-match` `all` (the default) needs every argument, `any` one of them; `all-with-x` and
/// `any-with-x` also compare arguments starting with `x-`. A `Void` argument asks only that the
/// header is there.
pub(crate) fn headers_match(arguments: &[(String, FieldValue)], headers: &[(String, FieldValue)]) -> bool {
    let mode = arguments
        .iter()
        .find(|(name, _)| name == "x-match")
        .and_then(|(_, value)| match value {
            FieldValue::Bytes(bytes) => std::str::from_utf8(bytes).ok().map(str::to_owned),
            _ => None,
        })
        .unwrap_or_else(|| "all".to_owned());
    let with_x = mode.ends_with("-with-x");
    let any = mode.starts_with("any");
    let wanted: Vec<&(String, FieldValue)> = arguments
        .iter()
        .filter(|(name, _)| name != "x-match" && (with_x || !name.starts_with("x-")))
        .collect();
    let holds = |(name, value): &&(String, FieldValue)| {
        headers.iter().any(|(header, found)| {
            header == name && (matches!(value, FieldValue::Void) || value.same(found))
        })
    };
    if any {
        wanted.iter().any(holds)
    } else {
        wanted.iter().all(holds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: u8, value: &[u8]) -> Vec<u8> {
        let mut out = vec![name.len() as u8];
        out.extend_from_slice(name.as_bytes());
        out.push(kind);
        out.extend_from_slice(value);
        out
    }

    fn text(value: &str) -> Vec<u8> {
        let mut out = (value.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(value.as_bytes());
        out
    }

    #[test]
    fn tables_decode_and_headers_bindings_match() {
        let mut table = entry("format", b'S', &text("pdf"));
        table.extend(entry("pages", b'I', &3i32.to_be_bytes()));
        let headers = decode_table(&table).unwrap();
        assert_eq!(headers[1], ("pages".to_owned(), FieldValue::Integer(3)));
        let all = vec![
            ("x-match".to_owned(), FieldValue::Bytes(b"all".to_vec())),
            ("format".to_owned(), FieldValue::Bytes(b"pdf".to_vec())),
            ("pages".to_owned(), FieldValue::Integer(3)),
        ];
        assert!(headers_match(&all, &headers));
        let any = vec![
            ("x-match".to_owned(), FieldValue::Bytes(b"any".to_vec())),
            ("format".to_owned(), FieldValue::Bytes(b"zip".to_vec())),
            ("pages".to_owned(), FieldValue::Void),
        ];
        assert!(headers_match(&any, &headers));
        let none = vec![("format".to_owned(), FieldValue::Bytes(b"zip".to_vec()))];
        assert!(!headers_match(&none, &headers));
        let mut properties = (1u16 << 13).to_be_bytes().to_vec();
        properties.extend_from_slice(&(table.len() as u32).to_be_bytes());
        properties.extend_from_slice(&table);
        assert_eq!(message_headers(&properties), headers);
    }

    #[test]
    fn expiration_and_named_arguments_are_read() {
        let properties = [0x01, 0x00, 5, b'6', b'0', b'0', b'0', b'0'];
        assert_eq!(message_expiration(&properties), Some(60_000));
        let with_type = [0x81, 0x00, 1, b'x', 2, b'1', b'0'];
        assert_eq!(message_expiration(&with_type), Some(10));
        assert_eq!(message_expiration(&[0x80, 0x00, 1, b'x']), None);
        let arguments = vec![
            ("x-message-ttl".to_owned(), FieldValue::Integer(500)),
            ("x-dead-letter-exchange".to_owned(), FieldValue::Bytes(b"dlx".to_vec())),
        ];
        assert_eq!(integer_named(&arguments, "x-message-ttl"), Some(500));
        assert_eq!(text_named(&arguments, "x-dead-letter-exchange").as_deref(), Some("dlx"));
    }

    #[test]
    fn dead_lettered_messages_carry_x_death_and_keep_their_other_properties() {
        let mut properties = ((1u16 << 15) | (1 << 12)).to_be_bytes().to_vec();
        properties.push(10);
        properties.extend_from_slice(b"text/plain");
        properties.push(2);
        let first = with_death(Vec::new(), "work", "rejected", "jobs", "job.new", 100);
        let rewritten = with_headers(&properties, &first).unwrap();
        let headers = message_headers(&rewritten);
        let Some((_, FieldValue::Array(deaths))) = headers.iter().find(|(name, _)| name == "x-death") else {
            panic!("no x-death in {headers:?}");
        };
        assert_eq!(deaths.len(), 1);
        assert_eq!(text_named(&headers, "x-first-death-queue").as_deref(), Some("work"));
        assert_eq!(rewritten[rewritten.len() - 1], 2, "the delivery mode stays after the headers");
        let again = with_death(headers, "work", "rejected", "jobs", "job.new", 200);
        let Some((_, FieldValue::Array(deaths))) = again.iter().find(|(name, _)| name == "x-death") else {
            panic!("no x-death in {again:?}");
        };
        assert_eq!(deaths.len(), 1);
        let FieldValue::Table(entry) = &deaths[0] else {
            panic!("an x-death entry is a table");
        };
        assert_eq!(integer_named(entry, "count"), Some(2));
        let other = with_death(again, "slow", "expired", "", "slow", 300);
        let Some((_, FieldValue::Array(deaths))) = other.iter().find(|(name, _)| name == "x-death") else {
            panic!("no x-death in {other:?}");
        };
        assert_eq!(deaths.len(), 2);
        assert_eq!(text_named(&other, "x-first-death-reason").as_deref(), Some("rejected"));
        assert_eq!(text_named(&other, "x-last-death-reason").as_deref(), Some("expired"));
    }


    #[test]
    fn the_priority_property_is_read_past_the_earlier_ones() {
        let mut properties = ((1u16 << 15) | (1 << 12) | (1 << 11)).to_be_bytes().to_vec();
        properties.push(4);
        properties.extend_from_slice(b"text");
        properties.push(2);
        properties.push(7);
        assert_eq!(message_priority(&properties), 7);
        assert_eq!(message_priority(&(1u16 << 15).to_be_bytes()), 0);
        assert_eq!(message_priority(&[]), 0);
    }

}
