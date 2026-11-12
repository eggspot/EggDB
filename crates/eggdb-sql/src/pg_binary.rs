use crate::arrays::{ArrayElement, ArrayValue};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::interval::Interval;
use crate::pg_catalog::CatalogType;
use crate::time_zones::TimeZone;
use crate::types::{DefinedKind, SqlType, Value};

const BOOL_OID: u32 = 16;
const BYTEA_OID: u32 = 17;
const NAME_OID: u32 = 19;
const INT8_OID: u32 = 20;
const INT2_OID: u32 = 21;
const INT4_OID: u32 = 23;
const TEXT_OID: u32 = 25;
const JSON_OID: u32 = 114;
const FLOAT4_OID: u32 = 700;
const FLOAT8_OID: u32 = 701;
const UNKNOWN_OID: u32 = 705;
const BPCHAR_OID: u32 = 1042;
const VARCHAR_OID: u32 = 1043;
const DATE_OID: u32 = 1082;
const TIME_OID: u32 = 1083;
const TIMESTAMP_OID: u32 = 1114;
const TIMESTAMPTZ_OID: u32 = 1184;
const INTERVAL_OID: u32 = 1186;
const TIMETZ_OID: u32 = 1266;
const NUMERIC_OID: u32 = 1700;
const UUID_OID: u32 = 2950;
const JSONB_OID: u32 = 3802;
const CITEXT_OID: u32 = crate::citext::CITEXT_OID;
const RANGE_EMPTY: u8 = 0x01;
const RANGE_LOWER_INCLUSIVE: u8 = 0x02;
const RANGE_UPPER_INCLUSIVE: u8 = 0x04;
const RANGE_LOWER_INFINITE: u8 = 0x08;
const RANGE_UPPER_INFINITE: u8 = 0x10;
const HSTORE_OID: u32 = crate::hstore::HSTORE_OID;
const VECTOR_OID: u32 = crate::vector_type::VECTOR_OID;
const GEOMETRY_OID: u32 = crate::geometry_type::GEOMETRY_OID;

const IDENTIFIER_OIDS: [u32; 7] = [26, 28, 24, 2205, 2206, 4089, 4096];

const ARRAY_OIDS: [u32; 16] = [
    1000, 1005, 1007, 1009, 1014, 1015, 1016, 1021, 1022, 1231, 1028, 1003, 1002, 1034, 22, 30,
];

const TEXT_INPUT_OIDS: [u32; 8] = [
    0,
    JSON_OID,
    TEXT_OID,
    VARCHAR_OID,
    NAME_OID,
    BPCHAR_OID,
    UNKNOWN_OID,
    CITEXT_OID,
];

const MICROS_PER_DAY: i64 = 86_400_000_000;
const MAX_OFFSET: i32 = 15 * 3600 + 59 * 60 + 59;
const NUMERIC_POSITIVE: u16 = 0x0000;
const NUMERIC_NEGATIVE: u16 = 0x4000;
const NUMERIC_NAN: u16 = 0xC000;
const NUMERIC_POSITIVE_INFINITY: u16 = 0xD000;
const NUMERIC_NEGATIVE_INFINITY: u16 = 0xF000;
const MAX_NUMERIC_GROUPS: usize = 1000;
const JSONB_VERSION: u8 = 1;
const MAX_BYTEA_BYTES: usize = 1_000_000_000;
const MAX_ARRAY_ITEMS: usize = 134_217_727;

fn cannot_send(value: &Value, oid: u32) -> SqlError {
    SqlError::new(
        SqlState::INTERNAL_ERROR,
        format!(
            "cannot send a {} value as type OID {oid}",
            value.sql_type().map_or("NULL".to_owned(), SqlType::name)
        ),
    )
}

fn incorrect_binary() -> SqlError {
    SqlError::new(
        SqlState::INVALID_BINARY_REPRESENTATION,
        "incorrect binary data format",
    )
}

fn invalid_utf8() -> SqlError {
    SqlError::new(
        SqlState::CHARACTER_NOT_IN_REPERTOIRE,
        "invalid byte sequence for encoding \"UTF8\"",
    )
}

fn no_binary_form(oid: u32) -> SqlError {
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("the binary format of type OID {oid} is not supported yet"),
    )
}

/// The bytes of `value` in PostgreSQL's binary form of the type with OID `oid`, as a
/// `DataRow` in binary format and `COPY ... (FORMAT binary)` carry it, with a type that has
/// no binary form of its own sent as its text in `zone`; `None` for NULL.
pub fn encode_pg_binary(value: &Value, oid: u32, zone: TimeZone) -> SqlResult<Option<Vec<u8>>> {
    if let Value::Defined(defined) = value {
        match defined.as_ref() {
            crate::types::DefinedValue::Record(record) => return record_binary(record, zone).map(Some),
            crate::types::DefinedValue::Range(range) => return range_binary(range, zone).map(Some),
            _ => {}
        }
    }
    if ARRAY_OIDS.contains(&oid) || matches!(value, Value::Array(_)) {
        return match value {
            Value::Null => Ok(None),
            Value::Array(array) => array_binary(array, zone).map(Some),
            _ => Err(cannot_send(value, oid)),
        };
    }
    let Some(text) = value.to_text_in(zone) else {
        return Ok(None);
    };
    let bytes = match oid {
        oid if IDENTIFIER_OIDS.contains(&oid) => match value {
            Value::Oid(number) => number.to_be_bytes().to_vec(),
            Value::Reg(reg) => reg.oid.to_be_bytes().to_vec(),
            _ => return Err(cannot_send(value, oid)),
        },
        BOOL_OID => match value {
            Value::Boolean(flag) => vec![u8::from(*flag)],
            _ => return Err(cannot_send(value, oid)),
        },
        INT2_OID | INT4_OID | INT8_OID => {
            let number = match value {
                Value::SmallInt(number) => i64::from(*number),
                Value::Integer(number) => i64::from(*number),
                Value::BigInt(number) => *number,
                _ => return Err(cannot_send(value, oid)),
            };
            match oid {
                INT2_OID => i16::try_from(number)
                    .map_err(|_| cannot_send(value, oid))?
                    .to_be_bytes()
                    .to_vec(),
                INT4_OID => i32::try_from(number)
                    .map_err(|_| cannot_send(value, oid))?
                    .to_be_bytes()
                    .to_vec(),
                _ => number.to_be_bytes().to_vec(),
            }
        }
        FLOAT4_OID => match value {
            Value::Real(number) => number.to_bits().to_be_bytes().to_vec(),
            _ => return Err(cannot_send(value, oid)),
        },
        FLOAT8_OID => match value {
            Value::Real(number) => f64::from(*number).to_bits().to_be_bytes().to_vec(),
            Value::Double(number) => number.to_bits().to_be_bytes().to_vec(),
            _ => return Err(cannot_send(value, oid)),
        },
        NUMERIC_OID => match value {
            Value::Numeric(_) => numeric_binary(&text),
            _ => return Err(cannot_send(value, oid)),
        },
        DATE_OID => match value {
            Value::Date(days) => days.to_be_bytes().to_vec(),
            _ => return Err(cannot_send(value, oid)),
        },
        TIMESTAMP_OID | TIMESTAMPTZ_OID => match value {
            Value::Timestamp(micros) | Value::TimestampTz(micros) => micros.to_be_bytes().to_vec(),
            _ => return Err(cannot_send(value, oid)),
        },
        TIME_OID => match value {
            Value::Time(micros) => micros.to_be_bytes().to_vec(),
            _ => return Err(cannot_send(value, oid)),
        },
        TIMETZ_OID => match value {
            Value::TimeTz {
                micros,
                offset_seconds,
            } => {
                let mut bytes = micros.to_be_bytes().to_vec();
                bytes.extend_from_slice(&offset_seconds.wrapping_neg().to_be_bytes());
                bytes
            }
            _ => return Err(cannot_send(value, oid)),
        },
        INTERVAL_OID => match value {
            Value::Interval(interval) => {
                let mut bytes = interval.micros.to_be_bytes().to_vec();
                bytes.extend_from_slice(&interval.days.to_be_bytes());
                bytes.extend_from_slice(&interval.months.to_be_bytes());
                bytes
            }
            _ => return Err(cannot_send(value, oid)),
        },
        BYTEA_OID => match value {
            Value::Bytea(bytes) => bytes.clone(),
            _ => return Err(cannot_send(value, oid)),
        },
        UUID_OID => match value {
            Value::Uuid(bytes) => bytes.to_vec(),
            _ => return Err(cannot_send(value, oid)),
        },
        JSONB_OID => {
            let mut bytes = Vec::with_capacity(text.len() + 1);
            bytes.push(JSONB_VERSION);
            bytes.extend_from_slice(text.as_bytes());
            bytes
        }
        HSTORE_OID => match crate::hstore::hstore_of(value) {
            Some(hstore) => hstore_binary(hstore).ok_or_else(|| cannot_send(value, oid))?,
            None => return Err(cannot_send(value, oid)),
        },
        GEOMETRY_OID => match crate::geometry_type::geometry_of(value) {
            Some(ewkb) => ewkb.to_vec(),
            None => return Err(cannot_send(value, oid)),
        },
        VECTOR_OID => match crate::vector_type::vector_of(value) {
            Some(values) => {
                crate::vector_type::binary(values).ok_or_else(|| cannot_send(value, oid))?
            }
            None => return Err(cannot_send(value, oid)),
        },
        _ => text.into_bytes(),
    };
    Ok(Some(bytes))
}

fn field_oid(value: &Value) -> u32 {
    value.sql_type().map_or(TEXT_OID, crate::pg_catalog::type_oid)
}

fn put_field(out: &mut Vec<u8>, bytes: Option<Vec<u8>>) -> SqlResult<()> {
    match bytes {
        Some(bytes) => {
            let length = i32::try_from(bytes.len()).map_err(|_| incorrect_binary())?;
            out.extend_from_slice(&length.to_be_bytes());
            out.extend_from_slice(&bytes);
        }
        None => out.extend_from_slice(&(-1i32).to_be_bytes()),
    }
    Ok(())
}

fn record_binary(record: &crate::records::RecordValue, zone: TimeZone) -> SqlResult<Vec<u8>> {
    let count = i32::try_from(record.fields.len()).map_err(|_| incorrect_binary())?;
    let mut out = count.to_be_bytes().to_vec();
    for field in &record.fields {
        let oid = field_oid(field);
        out.extend_from_slice(&oid.to_be_bytes());
        put_field(&mut out, encode_pg_binary(field, oid, zone)?)?;
    }
    Ok(out)
}

fn range_binary(range: &crate::ranges::RangeValue, zone: TimeZone) -> SqlResult<Vec<u8>> {
    use crate::ranges::RangeBound;
    let Some(bounds) = &range.bounds else {
        return Ok(vec![RANGE_EMPTY]);
    };
    let subtype = crate::ranges::builtin(range.type_oid).map(|builtin| crate::pg_catalog::type_oid(builtin.subtype));
    let mut flags = 0u8;
    let mut out = vec![0u8];
    for (bound, inclusive, infinite) in [
        (&bounds.lower, RANGE_LOWER_INCLUSIVE, RANGE_LOWER_INFINITE),
        (&bounds.upper, RANGE_UPPER_INCLUSIVE, RANGE_UPPER_INFINITE),
    ] {
        let value = match bound {
            RangeBound::Unbounded => {
                flags |= infinite;
                continue;
            }
            RangeBound::Inclusive(value) => {
                flags |= inclusive;
                value
            }
            RangeBound::Exclusive(value) => value,
        };
        let oid = subtype.unwrap_or_else(|| field_oid(value));
        put_field(&mut out, encode_pg_binary(value, oid, zone)?)?;
    }
    out[0] = flags;
    Ok(out)
}

fn range_text(bytes: &[u8], subtype: u32) -> SqlResult<String> {
    let (&flags, mut rest) = bytes.split_first().ok_or_else(incorrect_binary)?;
    if flags & RANGE_EMPTY != 0 {
        return if rest.is_empty() { Ok("empty".to_owned()) } else { Err(incorrect_binary()) };
    }
    let mut bound = |infinite: u8| -> SqlResult<String> {
        if flags & infinite != 0 {
            return Ok(String::new());
        }
        let length = rest.get(..4).map(|word| i32::from_be_bytes(four(word))).ok_or_else(incorrect_binary)?;
        let length = usize::try_from(length).map_err(|_| incorrect_binary())?;
        let value = rest.get(4..4 + length).ok_or_else(incorrect_binary)?;
        rest = &rest[4 + length..];
        let text = decode_pg_binary_text(value, subtype)?;
        Ok(format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\"")))
    };
    let lower = bound(RANGE_LOWER_INFINITE)?;
    let upper = bound(RANGE_UPPER_INFINITE)?;
    if !rest.is_empty() {
        return Err(incorrect_binary());
    }
    Ok(format!(
        "{}{lower},{upper}{}",
        if flags & RANGE_LOWER_INCLUSIVE != 0 { '[' } else { '(' },
        if flags & RANGE_UPPER_INCLUSIVE != 0 { ']' } else { ')' }
    ))
}

fn put_length(out: &mut Vec<u8>, length: usize) -> Option<()> {
    out.extend_from_slice(&i32::try_from(length).ok()?.to_be_bytes());
    Some(())
}

fn hstore_binary(hstore: &crate::hstore::HstoreValue) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    put_length(&mut out, hstore.pairs().len())?;
    for (key, value) in hstore.pairs() {
        put_length(&mut out, key.len())?;
        out.extend_from_slice(key.as_bytes());
        match value {
            Some(value) => {
                put_length(&mut out, value.len())?;
                out.extend_from_slice(value.as_bytes());
            }
            None => out.extend_from_slice(&(-1i32).to_be_bytes()),
        }
    }
    Some(out)
}

fn hstore_length(bytes: &[u8], at: &mut usize) -> SqlResult<i32> {
    let word: [u8; 4] = bytes
        .get(*at..at.saturating_add(4))
        .and_then(|word| word.try_into().ok())
        .ok_or_else(incorrect_binary)?;
    *at += 4;
    Ok(i32::from_be_bytes(word))
}

fn hstore_text_at(bytes: &[u8], at: &mut usize, length: i32) -> SqlResult<String> {
    let length = usize::try_from(length).map_err(|_| incorrect_binary())?;
    let end = at.checked_add(length).ok_or_else(incorrect_binary)?;
    let slice = bytes.get(*at..end).ok_or_else(incorrect_binary)?;
    *at = end;
    text_of(slice)
}

fn hstore_from_binary(bytes: &[u8]) -> SqlResult<crate::hstore::HstoreValue> {
    let mut at = 0;
    let count = usize::try_from(hstore_length(bytes, &mut at)?).map_err(|_| incorrect_binary())?;
    if count > bytes.len() / 8 {
        return Err(incorrect_binary());
    }
    let mut pairs = Vec::with_capacity(count);
    for _ in 0..count {
        let key_length = hstore_length(bytes, &mut at)?;
        if key_length < 0 {
            return Err(SqlError::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                "null value not allowed for hstore key",
            ));
        }
        let key = hstore_text_at(bytes, &mut at, key_length)?;
        let value_length = hstore_length(bytes, &mut at)?;
        let value = if value_length == -1 {
            None
        } else {
            Some(hstore_text_at(bytes, &mut at, value_length)?)
        };
        pairs.push((key, value));
    }
    if at != bytes.len() {
        return Err(incorrect_binary());
    }
    crate::hstore::checked(pairs)
}

fn array_binary(array: &ArrayValue, zone: TimeZone) -> SqlResult<Vec<u8>> {
    let element_oid = crate::pg_catalog::type_oid(array.element.sql_type());
    let too_long = || {
        cannot_send(
            &Value::Array(Box::new(array.clone())),
            crate::pg_catalog::type_oid(SqlType::Array(array.element)),
        )
    };
    let length = i32::try_from(array.items.len()).map_err(|_| too_long())?;
    let mut out = Vec::with_capacity(20 + array.items.len() * 8);
    let dimensions: i32 = if array.items.is_empty() { 0 } else { 1 };
    out.extend_from_slice(&dimensions.to_be_bytes());
    out.extend_from_slice(&i32::from(array.items.iter().any(Value::is_null)).to_be_bytes());
    out.extend_from_slice(&element_oid.to_be_bytes());
    if dimensions == 1 {
        out.extend_from_slice(&length.to_be_bytes());
        out.extend_from_slice(&array.lower_bound.to_be_bytes());
    }
    for item in &array.items {
        match encode_pg_binary(item, element_oid, zone)? {
            None => out.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(bytes) => {
                let size = i32::try_from(bytes.len()).map_err(|_| too_long())?;
                out.extend_from_slice(&size.to_be_bytes());
                out.extend_from_slice(&bytes);
            }
        }
    }
    Ok(out)
}

fn special_numeric_binary(sign: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(&0i16.to_be_bytes());
    out.extend_from_slice(&0i16.to_be_bytes());
    out.extend_from_slice(&sign.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out
}

fn numeric_binary(text: &str) -> Vec<u8> {
    match text {
        "NaN" => return special_numeric_binary(NUMERIC_NAN),
        "Infinity" => return special_numeric_binary(NUMERIC_POSITIVE_INFINITY),
        "-Infinity" => return special_numeric_binary(NUMERIC_NEGATIVE_INFINITY),
        _ => {}
    }
    let (sign, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (NUMERIC_NEGATIVE, rest),
        None => (NUMERIC_POSITIVE, text),
    };
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let digit = |byte: u8| i16::from(byte - b'0');
    let group = |chunk: &[u8]| {
        chunk
            .iter()
            .fold(0i16, |total, &byte| total * 10 + digit(byte))
    };
    let whole = whole.trim_start_matches('0').as_bytes();
    let lead = whole.len() % 4;
    let mut groups: Vec<i16> = Vec::new();
    if lead > 0 {
        groups.push(group(&whole[..lead]));
    }
    groups.extend(whole[lead..].chunks(4).map(group));
    let mut weight = groups.len() as i32 - 1;
    for chunk in fraction.as_bytes().chunks(4) {
        let mut value = group(chunk);
        for _ in chunk.len()..4 {
            value *= 10;
        }
        groups.push(value);
    }
    let leading_zeros = groups.iter().take_while(|&&group| group == 0).count();
    groups.drain(..leading_zeros);
    weight -= leading_zeros as i32;
    while groups.last() == Some(&0) {
        groups.pop();
    }
    if groups.is_empty() {
        weight = 0;
    }
    let scale = fraction.len() as u16;
    let mut out = Vec::with_capacity(8 + groups.len() * 2);
    out.extend_from_slice(&(groups.len() as i16).to_be_bytes());
    out.extend_from_slice(&(weight as i16).to_be_bytes());
    out.extend_from_slice(
        &(if groups.is_empty() {
            NUMERIC_POSITIVE
        } else {
            sign
        })
        .to_be_bytes(),
    );
    out.extend_from_slice(&scale.to_be_bytes());
    for group in groups {
        out.extend_from_slice(&group.to_be_bytes());
    }
    out
}

fn numeric_text(bytes: &[u8]) -> Option<String> {
    let header = bytes.get(..8)?;
    let count = usize::try_from(i16::from_be_bytes([header[0], header[1]])).ok()?;
    let weight = i32::from(i16::from_be_bytes([header[2], header[3]]));
    let sign = u16::from_be_bytes([header[4], header[5]]);
    let scale = usize::from(u16::from_be_bytes([header[6], header[7]]));
    if count > MAX_NUMERIC_GROUPS || scale > 0x3FFF || bytes.len() != 8 + count * 2 {
        return None;
    }
    if sign == NUMERIC_NAN {
        return (count == 0).then(|| "NaN".to_owned());
    }
    if sign == NUMERIC_POSITIVE_INFINITY || sign == NUMERIC_NEGATIVE_INFINITY {
        let text = if sign == NUMERIC_POSITIVE_INFINITY {
            "Infinity"
        } else {
            "-Infinity"
        };
        return (count == 0).then(|| text.to_owned());
    }
    if sign != NUMERIC_POSITIVE && sign != NUMERIC_NEGATIVE {
        return None;
    }
    let mut groups = Vec::with_capacity(count);
    for pair in bytes[8..].as_chunks::<2>().0 {
        let group = i16::from_be_bytes(*pair);
        if !(0..10_000).contains(&group) {
            return None;
        }
        groups.push(group);
    }
    if weight.unsigned_abs() as usize > MAX_NUMERIC_GROUPS + count {
        return None;
    }
    let mut whole = String::new();
    let mut fraction = String::new();
    let lowest = weight - count as i32 + 1;
    for position in (lowest.min(0)..=weight.max(0)).rev() {
        let index = weight - position;
        let value = usize::try_from(index)
            .ok()
            .and_then(|index| groups.get(index))
            .copied()
            .unwrap_or(0);
        let written = format!("{value:04}");
        if position >= 0 {
            whole.push_str(&written);
        } else {
            fraction.push_str(&written);
        }
    }
    let whole = whole.trim_start_matches('0');
    let whole = if whole.is_empty() { "0" } else { whole };
    fraction.truncate(scale.min(fraction.len()));
    while fraction.len() < scale {
        fraction.push('0');
    }
    let negative = sign == NUMERIC_NEGATIVE && groups.iter().any(|&group| group != 0);
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(whole);
    if scale > 0 {
        out.push('.');
        out.push_str(&fraction);
    }
    Some(out)
}

fn bytea_text(bytes: &[u8]) -> SqlResult<String> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    if bytes.len() > MAX_BYTEA_BYTES {
        return Err(SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("a bytea value cannot exceed {MAX_BYTEA_BYTES} bytes"),
        ));
    }
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("\\x");
    for &byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0F)]));
    }
    Ok(out)
}

fn text_of(bytes: &[u8]) -> SqlResult<String> {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| invalid_utf8())
}

fn fixed(bytes: &[u8], length: usize) -> SqlResult<&[u8]> {
    if bytes.len() == length {
        Ok(bytes)
    } else {
        Err(incorrect_binary())
    }
}

fn eight(bytes: &[u8]) -> [u8; 8] {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[..8]);
    word
}

fn four(bytes: &[u8]) -> [u8; 4] {
    [bytes[0], bytes[1], bytes[2], bytes[3]]
}

/// The text of a value sent as `bytes` in PostgreSQL's binary form of the type with OID
/// `oid`, as the SQL layer reads a constant of that type: `22P03` when the bytes do not fit
/// the form, `0A000` for a type whose binary form EggDB does not read.
pub fn decode_pg_binary_text(bytes: &[u8], oid: u32) -> SqlResult<String> {
    decode_text(bytes, oid, 0)
}

fn record_text(bytes: &[u8], depth: usize) -> SqlResult<String> {
    if depth >= MOST_RECORD_DEPTH {
        return Err(incorrect_binary());
    }
    let count = usize::try_from(header_word(bytes, 0)?).map_err(|_| incorrect_binary())?;
    let mut at = 4usize;
    let mut fields = Vec::with_capacity(count.min(MOST_RECORD_FIELDS));
    for _ in 0..count {
        let oid = bytes
            .get(at..at + 4)
            .map(|word| u32::from_be_bytes(four(word)))
            .ok_or_else(incorrect_binary)?;
        let length = header_word(bytes, at + 4)?;
        at += 8;
        if length < 0 {
            fields.push(String::new());
            continue;
        }
        let length = usize::try_from(length).map_err(|_| incorrect_binary())?;
        let value = bytes.get(at..at + length).ok_or_else(incorrect_binary)?;
        at += length;
        let text = decode_text(value, oid, depth + 1)?;
        fields.push(format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\"")));
    }
    if at != bytes.len() {
        return Err(incorrect_binary());
    }
    Ok(format!("({})", fields.join(",")))
}

fn decode_text(bytes: &[u8], oid: u32, depth: usize) -> SqlResult<String> {
    let value = match oid {
        BOOL_OID => match fixed(bytes, 1)?[0] {
            0 => Value::Boolean(false),
            1 => Value::Boolean(true),
            _ => return Err(incorrect_binary()),
        },
        INT2_OID => {
            let bytes = fixed(bytes, 2)?;
            Value::SmallInt(i16::from_be_bytes([bytes[0], bytes[1]]))
        }
        INT4_OID => Value::Integer(i32::from_be_bytes(four(fixed(bytes, 4)?))),
        INT8_OID => Value::BigInt(i64::from_be_bytes(eight(fixed(bytes, 8)?))),
        FLOAT4_OID => Value::Real(f32::from_bits(u32::from_be_bytes(four(fixed(bytes, 4)?)))),
        FLOAT8_OID => Value::Double(f64::from_bits(u64::from_be_bytes(eight(fixed(bytes, 8)?)))),
        NUMERIC_OID => return numeric_text(bytes).ok_or_else(incorrect_binary),
        DATE_OID => Value::Date(i32::from_be_bytes(four(fixed(bytes, 4)?))),
        TIMESTAMP_OID => Value::Timestamp(i64::from_be_bytes(eight(fixed(bytes, 8)?))),
        TIMESTAMPTZ_OID => Value::TimestampTz(i64::from_be_bytes(eight(fixed(bytes, 8)?))),
        TIME_OID => {
            let micros = i64::from_be_bytes(eight(fixed(bytes, 8)?));
            if !(0..=MICROS_PER_DAY).contains(&micros) {
                return Err(incorrect_binary());
            }
            Value::Time(micros)
        }
        TIMETZ_OID => {
            let bytes = fixed(bytes, 12)?;
            let micros = i64::from_be_bytes(eight(bytes));
            let west = i32::from_be_bytes(four(&bytes[8..]));
            if !(0..=MICROS_PER_DAY).contains(&micros)
                || !(-MAX_OFFSET..=MAX_OFFSET).contains(&west)
            {
                return Err(incorrect_binary());
            }
            Value::TimeTz {
                micros,
                offset_seconds: -west,
            }
        }
        INTERVAL_OID => {
            let bytes = fixed(bytes, 16)?;
            Value::Interval(Interval {
                micros: i64::from_be_bytes(eight(bytes)),
                days: i32::from_be_bytes(four(&bytes[8..])),
                months: i32::from_be_bytes(four(&bytes[12..])),
            })
        }
        BYTEA_OID => return bytea_text(bytes),
        UUID_OID => {
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(fixed(bytes, 16)?);
            Value::Uuid(uuid)
        }
        JSONB_OID => match bytes.split_first() {
            Some((&JSONB_VERSION, rest)) => return text_of(rest),
            _ => return Err(incorrect_binary()),
        },
        HSTORE_OID => {
            return hstore_from_binary(bytes).map(|hstore| crate::hstore::to_text(&hstore));
        }
        GEOMETRY_OID => {
            return crate::geometry_type::from_binary(bytes).map(|ewkb| crate::geometry_type::to_text(&ewkb));
        }
        VECTOR_OID => {
            return crate::vector_type::from_binary(bytes)
                .map(|values| crate::vector_type::to_text(&values));
        }
        oid if TEXT_INPUT_OIDS.contains(&oid) => return text_of(bytes),
        oid if crate::ranges::builtin(oid).is_some() => {
            let subtype = crate::ranges::builtin(oid).map_or(TEXT_OID, |builtin| crate::pg_catalog::type_oid(builtin.subtype));
            return range_text(bytes, subtype);
        }
        oid if crate::user_types::range_subtype(oid).is_some() => {
            let subtype = crate::user_types::range_subtype(oid).map_or(TEXT_OID, crate::pg_catalog::type_oid);
            return range_text(bytes, subtype);
        }
        oid if crate::user_types::composite_attributes(oid).is_some() => return record_text(bytes, depth),
        oid => match ArrayElement::of_array_oid(oid) {
            Some(element) => Value::Array(Box::new(decode_array(bytes, element)?)),
            None => return Err(no_binary_form(oid)),
        },
    };
    Ok(value.to_text_in(TimeZone::UTC).unwrap_or_default())
}

pub(crate) fn writes_binary(sql_type: SqlType) -> bool {
    match sql_type {
        SqlType::Defined(defined) => matches!(
            defined.kind,
            DefinedKind::Enum | DefinedKind::Base | DefinedKind::Composite | DefinedKind::Range
        ),
        SqlType::Catalog(kind) => !matches!(
            kind,
            CatalogType::NodeTree
                | CatalogType::AclItem
                | CatalogType::AnyArray
                | CatalogType::Pseudo(_)
        ),
        SqlType::Array(element) => element != ArrayElement::AclItem,
        _ => true,
    }
}

pub(crate) fn reads_binary(sql_type: SqlType) -> bool {
    match sql_type {
        SqlType::Defined(defined) if defined.kind == DefinedKind::Range => {
            crate::user_types::range_subtype(defined.oid).is_some()
        }
        SqlType::Defined(defined) => {
            matches!(defined.kind, DefinedKind::Enum | DefinedKind::Base | DefinedKind::Composite)
        }
        SqlType::Array(element) => {
            !matches!(element.sql_type(), SqlType::Array(_)) && reads_binary(element.sql_type())
        }
        SqlType::Catalog(CatalogType::Name | CatalogType::RefCursor) | SqlType::Dynamic(_) => true,
        SqlType::Catalog(_) => false,
        _ => true,
    }
}

const MOST_RECORD_DEPTH: usize = 32;
const MOST_RECORD_FIELDS: usize = 1600;

fn header_word(bytes: &[u8], at: usize) -> SqlResult<i32> {
    bytes
        .get(at..at + 4)
        .map(|word| i32::from_be_bytes(four(word)))
        .ok_or_else(incorrect_binary)
}

pub(crate) fn decode_array(bytes: &[u8], element: ArrayElement) -> SqlResult<ArrayValue> {
    let (lower_bound, texts) = decode_array_texts(bytes, element)?;
    let items = texts
        .iter()
        .map(|text| match text {
            None => Ok(Value::Null),
            Some(text) => crate::eval::string_constant(text, Some(element.sql_type())),
        })
        .collect::<SqlResult<Vec<_>>>()?;
    let mut array = ArrayValue::new(element, items);
    array.lower_bound = lower_bound;
    Ok(array)
}

pub(crate) fn decode_array_texts(
    bytes: &[u8],
    element: ArrayElement,
) -> SqlResult<(i32, Vec<Option<String>>)> {
    let dimensions = header_word(bytes, 0)?;
    let flags = header_word(bytes, 4)?;
    let element_oid = u32::from_be_bytes(four(bytes.get(8..12).ok_or_else(incorrect_binary)?));
    let expected_oid = crate::pg_catalog::type_oid(element.sql_type());
    if !(0..=1).contains(&flags) {
        return Err(SqlError::new(
            SqlState::INVALID_BINARY_REPRESENTATION,
            "invalid array flags",
        ));
    }
    if element_oid != expected_oid {
        return Err(SqlError::new(
            SqlState::DATATYPE_MISMATCH,
            format!(
                "binary data has array element type {element_oid} instead of expected {expected_oid}"
            ),
        ));
    }
    match dimensions {
        0 => {
            return if bytes.len() == 12 {
                Ok((1, Vec::new()))
            } else {
                Err(incorrect_binary())
            };
        }
        1 => {}
        more if more > 1 => {
            return Err(SqlError::not_supported(
                "multidimensional arrays are not supported yet",
            ));
        }
        _ => {
            return Err(SqlError::new(
                SqlState::INVALID_BINARY_REPRESENTATION,
                format!("invalid number of dimensions: {dimensions}"),
            ));
        }
    }
    let length = header_word(bytes, 12)?;
    let lower_bound = header_word(bytes, 16)?;
    let count = usize::try_from(length).map_err(|_| incorrect_binary())?;
    if count > MAX_ARRAY_ITEMS
        || i64::from(lower_bound) + i64::from(length) - 1 > i64::from(i32::MAX)
    {
        return Err(SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            "array size exceeds the maximum allowed",
        ));
    }
    let mut at = 20;
    let mut texts = Vec::with_capacity(count.min(bytes.len() / 4));
    for _ in 0..count {
        let size = header_word(bytes, at)?;
        at += 4;
        if size == -1 {
            texts.push(None);
            continue;
        }
        let size = usize::try_from(size).map_err(|_| incorrect_binary())?;
        let end = at.checked_add(size).ok_or_else(incorrect_binary)?;
        let item = bytes.get(at..end).ok_or_else(incorrect_binary)?;
        at = end;
        texts.push(Some(decode_pg_binary_text(item, expected_oid)?));
    }
    if at != bytes.len() {
        return Err(incorrect_binary());
    }
    Ok((lower_bound, texts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_infinities_take_postgresql_14_special_signs() {
        for (value, sign) in [("Infinity", [0xD0, 0x00]), ("-Infinity", [0xF0, 0x00])] {
            let number =
                Value::Numeric(crate::numeric::Numeric::parse_with_infinity(value).unwrap());
            let bytes = encode_pg_binary(&number, NUMERIC_OID, TimeZone::UTC)
                .unwrap()
                .unwrap();
            assert_eq!(bytes, [0, 0, 0, 0, sign[0], sign[1], 0, 0]);
            assert_eq!(decode_pg_binary_text(&bytes, NUMERIC_OID).unwrap(), value);
        }
        assert!(decode_pg_binary_text(&[0, 1, 0, 0, 0xD0, 0, 0, 0, 0, 1], NUMERIC_OID).is_err());
    }

    #[test]
    fn arrays_read_back_what_they_write() {
        let array = ArrayValue {
            element: ArrayElement::Integer,
            items: vec![Value::Integer(7), Value::Null, Value::Integer(-1)],
            lower_bound: 1,
        };
        let bytes = encode_pg_binary(
            &Value::Array(Box::new(array)),
            crate::pg_catalog::type_oid(SqlType::Array(ArrayElement::Integer)),
            TimeZone::UTC,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            decode_array_texts(&bytes, ArrayElement::Integer).unwrap(),
            (1, vec![Some("7".into()), None, Some("-1".into())])
        );
        assert_eq!(
            decode_array_texts(&bytes, ArrayElement::Text)
                .unwrap_err()
                .state()
                .code(),
            "42804"
        );
        let empty = [0u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 23];
        assert_eq!(
            decode_array_texts(&empty, ArrayElement::Integer).unwrap(),
            (1, Vec::new())
        );
    }

    #[test]
    fn hostile_array_bytes_are_errors() {
        let mut seed = 0x5eed_a11a_u64;
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let length = (seed % 40) as usize;
            let mut bytes: Vec<u8> = (0..length)
                .map(|shift| (seed >> (shift % 8 * 8)) as u8)
                .collect();
            if bytes.len() >= 12 {
                bytes[..12].copy_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 23]);
            }
            let _ = decode_array_texts(&bytes, ArrayElement::Integer);
        }
        let huge = [
            0u8, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 23, 0x7f, 0xff, 0xff, 0xff, 0, 0, 0, 1,
        ];
        assert!(decode_array_texts(&huge, ArrayElement::Integer).is_err());
    }

    #[test]
    fn citext_travels_as_its_text_and_hstore_in_postgresql_binary_layout() {
        let citext = crate::citext::value("MiXed".into());
        assert_eq!(
            encode_pg_binary(&citext, CITEXT_OID, TimeZone::UTC).unwrap(),
            Some(b"MiXed".to_vec())
        );
        assert_eq!(
            decode_pg_binary_text(b"MiXed", CITEXT_OID).unwrap(),
            "MiXed"
        );
        let hstore = crate::hstore::value(crate::hstore::HstoreValue::from_pairs([
            ("b".to_owned(), None),
            ("a".to_owned(), Some("1".to_owned())),
        ]));
        let bytes = encode_pg_binary(&hstore, HSTORE_OID, TimeZone::UTC)
            .unwrap()
            .unwrap();
        let expected: Vec<u8> = [
            &2i32.to_be_bytes()[..],
            &1i32.to_be_bytes(),
            b"a",
            &1i32.to_be_bytes(),
            b"1",
            &1i32.to_be_bytes(),
            b"b",
            &(-1i32).to_be_bytes(),
        ]
        .concat();
        assert_eq!(bytes, expected);
        assert_eq!(
            decode_pg_binary_text(&bytes, HSTORE_OID).unwrap(),
            "\"a\"=>\"1\", \"b\"=>NULL"
        );
        assert!(writes_binary(SqlType::Defined(
            crate::types::DefinedType::HSTORE
        )));
        assert!(reads_binary(SqlType::Defined(
            crate::types::DefinedType::CITEXT
        )));
    }

    #[test]
    fn ranges_and_records_have_their_binary_forms() {
        use crate::ranges::{RangeBound, RangeBounds, RangeValue};
        let range = Value::Defined(Box::new(crate::types::DefinedValue::Range(RangeValue {
            type_oid: 3904,
            bounds: Some(RangeBounds {
                lower: RangeBound::Inclusive(Value::Integer(1)),
                upper: RangeBound::Exclusive(Value::Integer(5)),
            }),
        })));
        let bytes = encode_pg_binary(&range, 3904, TimeZone::UTC).unwrap().unwrap();
        let expected = [
            &[RANGE_LOWER_INCLUSIVE][..],
            &4i32.to_be_bytes(),
            &1i32.to_be_bytes(),
            &4i32.to_be_bytes(),
            &5i32.to_be_bytes(),
        ]
        .concat();
        assert_eq!(bytes, expected);
        assert_eq!(decode_pg_binary_text(&bytes, 3904).unwrap(), "[\"1\",\"5\")");
        assert_eq!(decode_pg_binary_text(&[RANGE_EMPTY], 3904).unwrap(), "empty");
        assert!(decode_pg_binary_text(&bytes[..6], 3904).is_err());
        let record = crate::records::record_value(2249, vec![Value::Integer(7), Value::Null], vec![]);
        let bytes = encode_pg_binary(&record, 2249, TimeZone::UTC).unwrap().unwrap();
        let expected = [
            &2i32.to_be_bytes()[..],
            &INT4_OID.to_be_bytes(),
            &4i32.to_be_bytes(),
            &7i32.to_be_bytes(),
            &TEXT_OID.to_be_bytes(),
            &(-1i32).to_be_bytes(),
        ]
        .concat();
        assert_eq!(bytes, expected);
    }

    #[test]
    fn hostile_hstore_bytes_are_errors_never_panics() {
        let null_key = [&1i32.to_be_bytes()[..], &(-1i32).to_be_bytes()].concat();
        assert_eq!(
            decode_pg_binary_text(&null_key, HSTORE_OID)
                .unwrap_err()
                .state()
                .code(),
            "22004"
        );
        let huge_count = i32::MAX.to_be_bytes();
        assert!(decode_pg_binary_text(&huge_count, HSTORE_OID).is_err());
        let trailing = [&0i32.to_be_bytes()[..], &[0]].concat();
        assert!(decode_pg_binary_text(&trailing, HSTORE_OID).is_err());
        let bad_utf8 = [
            &1i32.to_be_bytes()[..],
            &1i32.to_be_bytes(),
            &[0xFF],
            &(-1i32).to_be_bytes(),
        ]
        .concat();
        assert_eq!(
            decode_pg_binary_text(&bad_utf8, HSTORE_OID)
                .unwrap_err()
                .state()
                .code(),
            "22021"
        );
        let mut seed = 0x4853_544f_5245_u64;
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let length = (seed % 32) as usize;
            let mut bytes: Vec<u8> = (0..length)
                .map(|shift| match (seed >> (shift % 8 * 8)) as u8 % 4 {
                    0 => 0,
                    1 => 0xFF,
                    2 => 1,
                    _ => b'k',
                })
                .collect();
            if bytes.len() >= 4 {
                bytes[..4].copy_from_slice(&((seed % 3) as i32).to_be_bytes());
            }
            if let Ok(text) = decode_pg_binary_text(&bytes, HSTORE_OID) {
                let parsed = crate::hstore::parse_text(&text).unwrap();
                let again = hstore_binary(&parsed).unwrap();
                assert_eq!(decode_pg_binary_text(&again, HSTORE_OID).unwrap(), text);
            }
        }
    }

    #[test]
    fn types_without_a_binary_form_are_named() {
        assert!(reads_binary(SqlType::Integer));
        assert!(reads_binary(SqlType::Array(ArrayElement::Text)));
        assert!(!reads_binary(SqlType::Catalog(CatalogType::Oid)));
        assert_eq!(
            decode_pg_binary_text(&[0, 0, 0, 1], 26)
                .unwrap_err()
                .state()
                .code(),
            "0A000"
        );
        assert_eq!(
            decode_pg_binary_text(&[0, 1], INT4_OID)
                .unwrap_err()
                .state()
                .code(),
            "22P03"
        );
    }

    #[test]
    fn records_read_back_with_each_field_quoted() {
        let mut bytes = 3i32.to_be_bytes().to_vec();
        bytes.extend_from_slice(&INT4_OID.to_be_bytes());
        bytes.extend_from_slice(&4i32.to_be_bytes());
        bytes.extend_from_slice(&7i32.to_be_bytes());
        bytes.extend_from_slice(&TEXT_OID.to_be_bytes());
        bytes.extend_from_slice(&5i32.to_be_bytes());
        bytes.extend_from_slice(b"a\"b,c");
        bytes.extend_from_slice(&TEXT_OID.to_be_bytes());
        bytes.extend_from_slice(&(-1i32).to_be_bytes());
        assert_eq!(record_text(&bytes, 0).unwrap(), "(\"7\",\"a\\\"b,c\",)");
        assert!(record_text(&bytes[..bytes.len() - 1], 0).is_err());
        let mut long = bytes.clone();
        long.push(0);
        assert!(record_text(&long, 0).is_err());
        assert!(record_text(&i32::MAX.to_be_bytes(), 0).is_err());
        assert!(record_text(&bytes, MOST_RECORD_DEPTH).is_err());
    }
}
