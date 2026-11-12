use crate::error::{SqlError, SqlResult, SqlState};

const MOST_METRICS: usize = 100_000;
const MOST_DEPTH: usize = 8;

fn invalid(message: impl std::fmt::Display) -> SqlError {
    SqlError::new(SqlState::INVALID_TEXT_REPRESENTATION, format!("invalid Sparkplug B payload: {message}"))
}

enum Field<'b> {
    Varint(u64),
    Fixed64(u64),
    Fixed32(u32),
    Bytes(&'b [u8]),
}

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    fn varint(&mut self) -> SqlResult<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.bytes.get(self.at).ok_or_else(|| invalid("a varint is cut short"))?;
            self.at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(invalid("a varint is too long"))
    }

    fn take(&mut self, count: usize) -> SqlResult<&'b [u8]> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or_else(|| invalid("a field runs past its message"))?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn next(&mut self) -> SqlResult<Option<(u64, Field<'b>)>> {
        if self.at >= self.bytes.len() {
            return Ok(None);
        }
        let key = self.varint()?;
        let field = match key & 7 {
            0 => Field::Varint(self.varint()?),
            1 => Field::Fixed64(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| invalid("short"))?)),
            2 => {
                let length = usize::try_from(self.varint()?).map_err(|_| invalid("a length does not fit"))?;
                Field::Bytes(self.take(length)?)
            }
            5 => Field::Fixed32(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| invalid("short"))?)),
            other => return Err(invalid(format!("wire type {other} is not supported"))),
        };
        Ok(Some((key >> 3, field)))
    }
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    crate::json::write_string(text, &mut out);
    out
}

const DATATYPES: [&str; 20] = [
    "Unknown", "Int8", "Int16", "Int32", "Int64", "UInt8", "UInt16", "UInt32", "UInt64", "Float", "Double",
    "Boolean", "String", "DateTime", "Text", "UUID", "DataSet", "Bytes", "File", "Template",
];

fn datatype_name(code: u64) -> String {
    usize::try_from(code)
        .ok()
        .and_then(|at| DATATYPES.get(at))
        .map_or_else(|| format!("Type{code}"), |name| (*name).to_owned())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn float_json(number: f64) -> String {
    if number.is_finite() { format!("{number}") } else { "null".to_owned() }
}

fn metric(bytes: &[u8], depth: usize) -> SqlResult<String> {
    let mut reader = Reader { bytes, at: 0 };
    let mut members: Vec<String> = Vec::new();
    let mut datatype = 0u64;
    let mut raw_int: Option<u64> = None;
    let mut value: Option<String> = None;
    let mut is_null = false;
    while let Some((number, field)) = reader.next()? {
        match (number, field) {
            (1, Field::Bytes(name)) => members.push(format!("\"name\":{}", json_string(&String::from_utf8_lossy(name)))),
            (2, Field::Varint(alias)) => members.push(format!("\"alias\":{alias}")),
            (3, Field::Varint(time)) => members.push(format!("\"timestamp\":{time}")),
            (4, Field::Varint(code)) => datatype = code,
            (5, Field::Varint(flag)) => members.push(format!("\"historical\":{}", flag != 0)),
            (6, Field::Varint(flag)) => members.push(format!("\"transient\":{}", flag != 0)),
            (7, Field::Varint(flag)) => is_null = flag != 0,
            (10 | 11, Field::Varint(whole)) => raw_int = Some(whole),
            (12, Field::Fixed32(bits)) => value = Some(float_json(f64::from(f32::from_bits(bits)))),
            (13, Field::Fixed64(bits)) => value = Some(float_json(f64::from_bits(bits))),
            (14, Field::Varint(flag)) => value = Some((flag != 0).to_string()),
            (15, Field::Bytes(text)) => value = Some(json_string(&String::from_utf8_lossy(text))),
            (16, Field::Bytes(raw)) => value = Some(json_string(&hex(raw))),
            (18, Field::Bytes(template)) if depth < MOST_DEPTH => value = Some(template_json(template, depth + 1)?),
            _ => {}
        }
    }
    if let Some(whole) = raw_int {
        value = Some(match datatype {
            1 => (whole as u8 as i8).to_string(),
            2 => (whole as u16 as i16).to_string(),
            3 => (whole as u32 as i32).to_string(),
            4 => (whole as i64).to_string(),
            _ => whole.to_string(),
        });
    }
    members.push(format!("\"datatype\":{}", json_string(&datatype_name(datatype))));
    members.push(format!("\"value\":{}", if is_null { "null".to_owned() } else { value.unwrap_or_else(|| "null".to_owned()) }));
    Ok(format!("{{{}}}", members.join(",")))
}

fn template_json(bytes: &[u8], depth: usize) -> SqlResult<String> {
    let mut reader = Reader { bytes, at: 0 };
    let mut metrics = Vec::new();
    while let Some((number, field)) = reader.next()? {
        if let (2, Field::Bytes(found)) = (number, field) {
            metrics.push(metric(found, depth)?);
        }
    }
    Ok(format!("{{\"metrics\":[{}]}}", metrics.join(",")))
}

/// A Sparkplug B payload (the protobuf `org.eclipse.tahu.protobuf.Payload` an MQTT edge node
/// publishes) as JSON: its `timestamp`, `seq` and `uuid`, and its metrics with their name,
/// alias, timestamp, data type and value, signed integers read by their data type, bytes as hex
/// and templates as nested metrics.
pub(crate) fn decode(payload: &[u8]) -> SqlResult<String> {
    let mut reader = Reader { bytes: payload, at: 0 };
    let mut members = Vec::new();
    let mut metrics = Vec::new();
    while let Some((number, field)) = reader.next()? {
        match (number, field) {
            (1, Field::Varint(time)) => members.push(format!("\"timestamp\":{time}")),
            (2, Field::Bytes(found)) => {
                if metrics.len() >= MOST_METRICS {
                    return Err(invalid(format!("more than {MOST_METRICS} metrics")));
                }
                metrics.push(metric(found, 0)?);
            }
            (3, Field::Varint(sequence)) => members.push(format!("\"seq\":{sequence}")),
            (4, Field::Bytes(uuid)) => members.push(format!("\"uuid\":{}", json_string(&String::from_utf8_lossy(uuid)))),
            (5, Field::Bytes(body)) => members.push(format!("\"body\":{}", json_string(&hex(body)))),
            _ => {}
        }
    }
    members.push(format!("\"metrics\":[{}]", metrics.join(",")));
    Ok(format!("{{{}}}", members.join(",")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(mut value: u64, out: &mut Vec<u8>) {
        while value >= 0x80 {
            out.push((value as u8) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }

    #[test]
    fn payloads_decode_metrics_by_their_data_types() {
        let mut temperature = Vec::new();
        temperature.push(1 << 3 | 2);
        varint(4, &mut temperature);
        temperature.extend_from_slice(b"temp");
        temperature.push(4 << 3);
        varint(10, &mut temperature);
        temperature.push(13 << 3 | 1);
        temperature.extend_from_slice(&21.5f64.to_bits().to_le_bytes());
        let mut offset = Vec::new();
        offset.push(1 << 3 | 2);
        varint(6, &mut offset);
        offset.extend_from_slice(b"offset");
        offset.push(4 << 3);
        varint(3, &mut offset);
        offset.push(10 << 3);
        varint(u64::from((-5i32) as u32), &mut offset);
        let mut payload = Vec::new();
        payload.push(1 << 3);
        varint(1_700_000_000_000, &mut payload);
        for found in [&temperature, &offset] {
            payload.push(2 << 3 | 2);
            varint(found.len() as u64, &mut payload);
            payload.extend_from_slice(found);
        }
        payload.push(3 << 3);
        varint(7, &mut payload);
        let decoded = decode(&payload).unwrap();
        assert!(decoded.contains("\"name\":\"temp\",\"datatype\":\"Double\",\"value\":21.5"), "{decoded}");
        assert!(decoded.contains("\"name\":\"offset\",\"datatype\":\"Int32\",\"value\":-5"), "{decoded}");
        assert!(decoded.contains("\"seq\":7"), "{decoded}");
        assert!(crate::json::normalize(&decoded).is_ok());
        assert!(decode(&payload[..payload.len() - 1]).is_err());
    }
}
