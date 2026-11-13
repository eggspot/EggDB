//! OPC UA's binary encoding (IEC 62541-6) read into JSON for `opcua_decode` (D833): a Variant or a
//! DataValue, with every built-in type but DiagnosticInfo.

use crate::error::{SqlError, SqlResult, SqlState};

const MOST_DEPTH: usize = 16;
const MOST_ARRAY_ITEMS: usize = 1_000_000;
const TICKS_PER_MICROSECOND: i64 = 10;
const MICROSECONDS_FROM_1601_TO_1970: i64 = 11_644_473_600_000_000;

const TYPE_NAMES: [&str; 26] = [
    "Null",
    "Boolean",
    "SByte",
    "Byte",
    "Int16",
    "UInt16",
    "Int32",
    "UInt32",
    "Int64",
    "UInt64",
    "Float",
    "Double",
    "String",
    "DateTime",
    "Guid",
    "ByteString",
    "XmlElement",
    "NodeId",
    "ExpandedNodeId",
    "StatusCode",
    "QualifiedName",
    "LocalizedText",
    "ExtensionObject",
    "DataValue",
    "Variant",
    "DiagnosticInfo",
];

fn invalid(message: impl std::fmt::Display) -> SqlError {
    SqlError::new(SqlState::INVALID_TEXT_REPRESENTATION, format!("invalid OPC UA binary: {message}"))
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn finite(number: f64) -> String {
    if number.is_finite() {
        format!("{number}")
    } else {
        json_string(&number.to_string())
    }
}

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    fn take(&mut self, count: usize) -> SqlResult<&'b [u8]> {
        let end = self
            .at
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| invalid("a value runs past the end"))?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn array<const N: usize>(&mut self) -> SqlResult<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn u8(&mut self) -> SqlResult<u8> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> SqlResult<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn i32(&mut self) -> SqlResult<i32> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> SqlResult<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn i64(&mut self) -> SqlResult<i64> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    fn bytes(&mut self) -> SqlResult<Option<&'b [u8]>> {
        let length = self.i32()?;
        if length < 0 {
            return Ok(None);
        }
        let length = usize::try_from(length).map_err(|_| invalid("a length is too large"))?;
        self.take(length).map(Some)
    }

    fn string(&mut self) -> SqlResult<String> {
        match self.bytes()? {
            None => Ok("null".to_owned()),
            Some(bytes) => std::str::from_utf8(bytes).map(json_string).map_err(|_| invalid("a string is not UTF-8")),
        }
    }

    fn date_time(&mut self) -> SqlResult<String> {
        let ticks = self.i64()?;
        if ticks <= 0 || ticks == i64::MAX {
            return Ok("null".to_owned());
        }
        let micros = ticks / TICKS_PER_MICROSECOND - MICROSECONDS_FROM_1601_TO_1970;
        Ok(crate::types::Value::TimestampTz(micros)
            .to_text_in(crate::time_zones::TimeZone::UTC)
            .map_or_else(|| "null".to_owned(), |text| json_string(&text)))
    }

    fn guid(&mut self) -> SqlResult<String> {
        let first = u32::from_le_bytes(self.array()?);
        let second = u16::from_le_bytes(self.array()?);
        let third = u16::from_le_bytes(self.array()?);
        let rest: [u8; 8] = self.array()?;
        Ok(json_string(&format!(
            "{first:08x}-{second:04x}-{third:04x}-{}-{}",
            hex(&rest[..2]),
            hex(&rest[2..])
        )))
    }

    fn node_id(&mut self, expanded: bool) -> SqlResult<String> {
        let encoding = self.u8()?;
        let (namespace, identifier) = match encoding & 0x0f {
            0 => (0, self.u8()?.to_string()),
            1 => (u16::from(self.u8()?), self.u16()?.to_string()),
            2 => (self.u16()?, self.u32()?.to_string()),
            3 => (self.u16()?, self.string()?),
            4 => (self.u16()?, self.guid()?),
            5 => (self.u16()?, self.bytes()?.map_or_else(|| "null".to_owned(), |bytes| json_string(&hex(bytes)))),
            other => return Err(invalid(format!("node id encoding {other}"))),
        };
        let mut out = format!("{{\"namespace\":{namespace},\"id\":{identifier}");
        if expanded && encoding & 0x80 != 0 {
            out.push_str(&format!(",\"namespace_uri\":{}", self.string()?));
        }
        if expanded && encoding & 0x40 != 0 {
            out.push_str(&format!(",\"server_index\":{}", self.u32()?));
        }
        out.push('}');
        Ok(out)
    }

    fn scalar(&mut self, kind: u8, depth: usize) -> SqlResult<String> {
        Ok(match kind {
            1 => (self.u8()? != 0).to_string(),
            2 => (self.u8()? as i8).to_string(),
            3 => self.u8()?.to_string(),
            4 => i16::from_le_bytes(self.array()?).to_string(),
            5 => self.u16()?.to_string(),
            6 => self.i32()?.to_string(),
            7 => self.u32()?.to_string(),
            8 => self.i64()?.to_string(),
            9 => u64::from_le_bytes(self.array()?).to_string(),
            10 => finite(f64::from(f32::from_le_bytes(self.array()?))),
            11 => finite(f64::from_le_bytes(self.array()?)),
            12 | 16 => self.string()?,
            13 => self.date_time()?,
            14 => self.guid()?,
            15 => self.bytes()?.map_or_else(|| "null".to_owned(), |bytes| json_string(&hex(bytes))),
            17 => self.node_id(false)?,
            18 => self.node_id(true)?,
            19 => self.u32()?.to_string(),
            20 => format!("{{\"namespace\":{},\"name\":{}}}", self.u16()?, self.string()?),
            21 => {
                let mask = self.u8()?;
                let locale = if mask & 0x01 != 0 { self.string()? } else { "null".to_owned() };
                let text = if mask & 0x02 != 0 { self.string()? } else { "null".to_owned() };
                format!("{{\"locale\":{locale},\"text\":{text}}}")
            }
            22 => {
                let type_id = self.node_id(false)?;
                let body = match self.u8()? {
                    0 => "null".to_owned(),
                    1 => self.bytes()?.map_or_else(|| "null".to_owned(), |bytes| json_string(&hex(bytes))),
                    2 => self.string()?,
                    other => return Err(invalid(format!("extension object encoding {other}"))),
                };
                format!("{{\"type_id\":{type_id},\"body\":{body}}}")
            }
            23 => self.data_value(depth + 1)?,
            24 => self.variant(depth + 1)?,
            25 => return Err(SqlError::not_supported("OPC UA DiagnosticInfo values are not supported yet")),
            other => return Err(invalid(format!("built-in type {other}"))),
        })
    }

    fn variant(&mut self, depth: usize) -> SqlResult<String> {
        if depth > MOST_DEPTH {
            return Err(invalid("values are nested too deeply"));
        }
        let mask = self.u8()?;
        let kind = mask & 0x3f;
        if kind == 0 {
            return Ok("{\"type\":\"Null\",\"value\":null}".to_owned());
        }
        let name = TYPE_NAMES.get(usize::from(kind)).ok_or_else(|| invalid(format!("built-in type {kind}")))?;
        if mask & 0x80 == 0 {
            return Ok(format!("{{\"type\":\"{name}\",\"value\":{}}}", self.scalar(kind, depth)?));
        }
        let length = self.i32()?;
        let mut items = Vec::new();
        if length > 0 {
            let length = usize::try_from(length).map_err(|_| invalid("an array length is too large"))?;
            if length > MOST_ARRAY_ITEMS || length > self.bytes.len().saturating_sub(self.at) {
                return Err(invalid("an array is longer than its bytes"));
            }
            for _ in 0..length {
                items.push(self.scalar(kind, depth)?);
            }
        }
        let value = if length < 0 { "null".to_owned() } else { format!("[{}]", items.join(",")) };
        let mut out = format!("{{\"type\":\"{name}\",\"value\":{value}");
        if mask & 0x40 != 0 {
            let count = self.i32()?;
            let count = usize::try_from(count.max(0)).map_err(|_| invalid("a dimension count is too large"))?;
            if count > MOST_DEPTH * 4 {
                return Err(invalid("too many array dimensions"));
            }
            let mut dimensions = Vec::with_capacity(count);
            for _ in 0..count {
                dimensions.push(self.i32()?.to_string());
            }
            out.push_str(&format!(",\"dimensions\":[{}]", dimensions.join(",")));
        }
        out.push('}');
        Ok(out)
    }

    fn data_value(&mut self, depth: usize) -> SqlResult<String> {
        if depth > MOST_DEPTH {
            return Err(invalid("values are nested too deeply"));
        }
        let mask = self.u8()?;
        let mut members = Vec::new();
        if mask & 0x01 != 0 {
            members.push(format!("\"value\":{}", self.variant(depth + 1)?));
        }
        if mask & 0x02 != 0 {
            members.push(format!("\"status\":{}", self.u32()?));
        }
        if mask & 0x04 != 0 {
            members.push(format!("\"source_timestamp\":{}", self.date_time()?));
        }
        if mask & 0x10 != 0 {
            members.push(format!("\"source_picoseconds\":{}", self.u16()?));
        }
        if mask & 0x08 != 0 {
            members.push(format!("\"server_timestamp\":{}", self.date_time()?));
        }
        if mask & 0x20 != 0 {
            members.push(format!("\"server_picoseconds\":{}", self.u16()?));
        }
        Ok(format!("{{{}}}", members.join(",")))
    }
}

/// What `opcua_decode` reads: a Variant or a DataValue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoded {
    Variant,
    DataValue,
}

/// `bytes`, one OPC UA binary Variant or DataValue, as JSON: a Variant as `{"type", "value"}`
/// (arrays as JSON arrays, with `dimensions` for a matrix), a DataValue with its `value`,
/// `status` code and timestamps. Integers stay exact, DateTimes become timestamps, ByteStrings
/// hex, Guids text. Bytes left over are an error.
pub(crate) fn decode(bytes: &[u8], encoded: Encoded) -> SqlResult<String> {
    let mut reader = Reader { bytes, at: 0 };
    let json = match encoded {
        Encoded::Variant => reader.variant(0)?,
        Encoded::DataValue => reader.data_value(0)?,
    };
    if reader.at != bytes.len() {
        return Err(invalid("bytes are left after the value"));
    }
    Ok(json)
}

/// The OPC UA binary DataValue at the start of `bytes` as JSON (the shape `opcua_decode` gives),
/// with the number of bytes it took, for readers of messages that carry DataValues among other
/// fields (D1071).
pub fn opcua_data_value_prefix(bytes: &[u8]) -> Result<(String, usize), String> {
    let mut reader = Reader { bytes, at: 0 };
    let json = reader.data_value(0).map_err(|error| error.message().to_owned())?;
    Ok((json, reader.at))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_of_scalars_arrays_and_strings_decode() {
        assert_eq!(decode(&[11, 0, 0, 0, 0, 0, 0, 0xf8, 0x3f], Encoded::Variant).unwrap(), "{\"type\":\"Double\",\"value\":1.5}");
        let array = [0x86, 2, 0, 0, 0, 7, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];
        assert_eq!(decode(&array, Encoded::Variant).unwrap(), "{\"type\":\"Int32\",\"value\":[7,-1]}");
        let text = [12, 2, 0, 0, 0, b'o', b'k'];
        assert_eq!(decode(&text, Encoded::Variant).unwrap(), "{\"type\":\"String\",\"value\":\"ok\"}");
        let node = [17, 2, 3, 0, 0x39, 0x05, 0, 0];
        assert_eq!(decode(&node, Encoded::Variant).unwrap(), "{\"type\":\"NodeId\",\"value\":{\"namespace\":3,\"id\":1337}}");
        assert!(decode(&[11, 0, 0], Encoded::Variant).is_err());
        assert!(decode(&[1, 1, 9], Encoded::Variant).is_err());
        assert!(decode(&[0x86, 0xff, 0xff, 0xff, 0x7f], Encoded::Variant).is_err());
    }

    #[test]
    fn a_data_value_carries_its_status_and_timestamps() {
        let mut bytes = vec![0x07, 1, 1, 0, 0, 0, 0];
        let ticks: i64 = (1_700_000_000 * 1_000_000 + MICROSECONDS_FROM_1601_TO_1970) * TICKS_PER_MICROSECOND;
        bytes.extend_from_slice(&ticks.to_le_bytes());
        let json = decode(&bytes, Encoded::DataValue).unwrap();
        assert!(json.starts_with("{\"value\":{\"type\":\"Boolean\",\"value\":true},\"status\":0,\"source_timestamp\":\"2023-11-14"), "{json}");
    }
}
