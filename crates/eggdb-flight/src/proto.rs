const WIRE_VARINT: u8 = 0;
const WIRE_FIXED64: u8 = 1;
const WIRE_LENGTH: u8 = 2;
const WIRE_FIXED32: u8 = 5;
const MOST_FIELD_NUMBER: u64 = (1 << 29) - 1;

/// One field of a protocol buffers message as read: its number and its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FieldValue<'b> {
    Varint(u64),
    Bytes(&'b [u8]),
    Fixed,
}

fn varint(bytes: &[u8], at: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at).ok_or("a protocol buffers varint ends early")?;
        *at += 1;
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("a protocol buffers varint longer than 10 bytes".to_owned())
}

/// The fields of a protocol buffers message, in order (proto3 wire format).
pub(crate) fn fields(bytes: &[u8]) -> Result<Vec<(u32, FieldValue<'_>)>, String> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let key = varint(bytes, &mut at)?;
        let number = key >> 3;
        if number == 0 || number > MOST_FIELD_NUMBER {
            return Err("a protocol buffers field number out of range".to_owned());
        }
        let value = match (key & 7) as u8 {
            WIRE_VARINT => FieldValue::Varint(varint(bytes, &mut at)?),
            WIRE_FIXED64 => {
                at = at.checked_add(8).filter(|end| *end <= bytes.len()).ok_or("a fixed64 ends early")?;
                FieldValue::Fixed
            }
            WIRE_FIXED32 => {
                at = at.checked_add(4).filter(|end| *end <= bytes.len()).ok_or("a fixed32 ends early")?;
                FieldValue::Fixed
            }
            WIRE_LENGTH => {
                let length = usize::try_from(varint(bytes, &mut at)?).map_err(|_| "a length too large")?;
                let end = at.checked_add(length).filter(|end| *end <= bytes.len()).ok_or("a field runs past its message")?;
                let value = &bytes[at..end];
                at = end;
                FieldValue::Bytes(value)
            }
            other => return Err(format!("protocol buffers wire type {other} is not supported")),
        };
        out.push((number as u32, value));
    }
    Ok(out)
}

/// The bytes of the last occurrence of field `number`, or empty.
pub(crate) fn bytes_field<'b>(message: &'b [u8], number: u32) -> Result<&'b [u8], String> {
    Ok(fields(message)?
        .into_iter()
        .filter_map(|(field, value)| match value {
            FieldValue::Bytes(bytes) if field == number => Some(bytes),
            _ => None,
        })
        .last()
        .unwrap_or_default())
}

/// The text of field `number`.
pub(crate) fn text_field(message: &[u8], number: u32) -> Result<String, String> {
    String::from_utf8(bytes_field(message, number)?.to_vec()).map_err(|_| "a string field that is not UTF-8".to_owned())
}

/// Every bytes value of a repeated field `number`.
pub(crate) fn repeated_bytes(message: &[u8], number: u32) -> Result<Vec<&[u8]>, String> {
    Ok(fields(message)?
        .into_iter()
        .filter_map(|(field, value)| match value {
            FieldValue::Bytes(bytes) if field == number => Some(bytes),
            _ => None,
        })
        .collect())
}

/// Builds a protocol buffers message.
#[derive(Debug, Default)]
pub(crate) struct Message {
    bytes: Vec<u8>,
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7F) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

impl Message {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn varint(mut self, number: u32, value: u64) -> Self {
        put_varint(&mut self.bytes, u64::from(number) << 3 | u64::from(WIRE_VARINT));
        put_varint(&mut self.bytes, value);
        self
    }

    pub(crate) fn int64(self, number: u32, value: i64) -> Self {
        self.varint(number, value as u64)
    }

    pub(crate) fn bytes(mut self, number: u32, value: &[u8]) -> Self {
        put_varint(&mut self.bytes, u64::from(number) << 3 | u64::from(WIRE_LENGTH));
        put_varint(&mut self.bytes, value.len() as u64);
        self.bytes.extend_from_slice(value);
        self
    }

    pub(crate) fn text(self, number: u32, value: &str) -> Self {
        self.bytes(number, value.as_bytes())
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// A `google.protobuf.Any` holding `value` of the Flight SQL message type `name`.
pub(crate) fn any(name: &str, value: &[u8]) -> Vec<u8> {
    Message::new()
        .text(1, &format!("type.googleapis.com/arrow.flight.protocol.sql.{name}"))
        .bytes(2, value)
        .finish()
}

/// The message type name and value of a `google.protobuf.Any`.
pub(crate) fn unpack_any(bytes: &[u8]) -> Result<(String, Vec<u8>), String> {
    let type_url = text_field(bytes, 1)?;
    let name = type_url.rsplit(['/', '.']).next().unwrap_or_default().to_owned();
    Ok((name, bytes_field(bytes, 2)?.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_through_any() {
        let command = Message::new().text(1, "SELECT 1").finish();
        let packed = any("CommandStatementQuery", &command);
        let (name, value) = unpack_any(&packed).unwrap();
        assert_eq!(name, "CommandStatementQuery");
        assert_eq!(text_field(&value, 1).unwrap(), "SELECT 1");
        let numbers = Message::new().int64(4, -1).varint(1, 300).finish();
        assert_eq!(fields(&numbers).unwrap()[1], (1, FieldValue::Varint(300)));
        assert!(fields(&[0x0A, 0x05, 1]).is_err());
    }
}
