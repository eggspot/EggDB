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
}
