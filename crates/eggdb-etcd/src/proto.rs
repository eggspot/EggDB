//! The protocol buffers wire format as etcd's messages use it: varints, length-delimited fields
//! and nested messages, read with every length checked.

const WIRE_VARINT: u64 = 0;
const WIRE_FIXED64: u64 = 1;
const WIRE_LENGTH: u64 = 2;
const WIRE_FIXED32: u64 = 5;

/// One field as read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field<'b> {
    Varint(u64),
    Bytes(&'b [u8]),
    Fixed,
}

fn varint(bytes: &[u8], at: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at).ok_or("a varint ends early")?;
        *at += 1;
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("a varint longer than 10 bytes".to_owned())
}

/// The fields of a message, in order.
pub(crate) fn fields(bytes: &[u8]) -> Result<Vec<(u32, Field<'_>)>, String> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let key = varint(bytes, &mut at)?;
        let number = u32::try_from(key >> 3).ok().filter(|number| *number > 0).ok_or("a field number out of range")?;
        let value = match key & 7 {
            WIRE_VARINT => Field::Varint(varint(bytes, &mut at)?),
            WIRE_FIXED64 => {
                at = at.checked_add(8).filter(|end| *end <= bytes.len()).ok_or("a fixed64 ends early")?;
                Field::Fixed
            }
            WIRE_FIXED32 => {
                at = at.checked_add(4).filter(|end| *end <= bytes.len()).ok_or("a fixed32 ends early")?;
                Field::Fixed
            }
            WIRE_LENGTH => {
                let length = usize::try_from(varint(bytes, &mut at)?).map_err(|_| "a length too large")?;
                let end = at.checked_add(length).filter(|end| *end <= bytes.len()).ok_or("a field runs past its message")?;
                let value = &bytes[at..end];
                at = end;
                Field::Bytes(value)
            }
            other => return Err(format!("wire type {other} is not supported")),
        };
        out.push((number, value));
    }
    Ok(out)
}

/// A message's fields by number, for reading.
pub(crate) struct Read<'b> {
    fields: Vec<(u32, Field<'b>)>,
}

impl<'b> Read<'b> {
    pub(crate) fn new(bytes: &'b [u8]) -> Result<Self, String> {
        Ok(Self { fields: fields(bytes)? })
    }

    pub(crate) fn bytes(&self, number: u32) -> &'b [u8] {
        self.fields
            .iter()
            .rev()
            .find_map(|(field, value)| match value {
                Field::Bytes(bytes) if *field == number => Some(*bytes),
                _ => None,
            })
            .unwrap_or_default()
    }

    pub(crate) fn has(&self, number: u32) -> bool {
        self.fields.iter().any(|(field, _)| *field == number)
    }

    pub(crate) fn int(&self, number: u32) -> i64 {
        self.fields
            .iter()
            .rev()
            .find_map(|(field, value)| match value {
                Field::Varint(value) if *field == number => Some(*value as i64),
                _ => None,
            })
            .unwrap_or(0)
    }

    pub(crate) fn flag(&self, number: u32) -> bool {
        self.int(number) != 0
    }

    pub(crate) fn repeated(&self, number: u32) -> Vec<&'b [u8]> {
        self.fields
            .iter()
            .filter_map(|(field, value)| match value {
                Field::Bytes(bytes) if *field == number => Some(*bytes),
                _ => None,
            })
            .collect()
    }
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7F) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Builds a message.
#[derive(Debug, Default, Clone)]
pub(crate) struct Write {
    bytes: Vec<u8>,
}

impl Write {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn int(mut self, number: u32, value: i64) -> Self {
        if value != 0 {
            put_varint(&mut self.bytes, u64::from(number) << 3 | WIRE_VARINT);
            put_varint(&mut self.bytes, value as u64);
        }
        self
    }

    pub(crate) fn flag(self, number: u32, value: bool) -> Self {
        self.int(number, i64::from(value))
    }

    pub(crate) fn bytes(mut self, number: u32, value: &[u8]) -> Self {
        put_varint(&mut self.bytes, u64::from(number) << 3 | WIRE_LENGTH);
        put_varint(&mut self.bytes, value.len() as u64);
        self.bytes.extend_from_slice(value);
        self
    }

    pub(crate) fn bytes_if_any(self, number: u32, value: &[u8]) -> Self {
        if value.is_empty() { self } else { self.bytes(number, value) }
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }
}
