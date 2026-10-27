//! PostgreSQL's type OIDs for EggDB's SQL types, and values in their text and binary forms.

use crate::messages::{ColumnDescription, Format};
use eggdb_sql::{Interval, OutputColumn, SqlError, SqlState, SqlType, Value};

/// `bool`.
pub const BOOL_OID: u32 = 16;
/// `bytea`.
pub const BYTEA_OID: u32 = 17;
/// `name`.
pub const NAME_OID: u32 = 19;
/// `int8`.
pub const INT8_OID: u32 = 20;
/// `int2`.
pub const INT2_OID: u32 = 21;
/// `int4`.
pub const INT4_OID: u32 = 23;
/// `text`.
pub const TEXT_OID: u32 = 25;
/// `float4`.
pub const FLOAT4_OID: u32 = 700;
/// `float8`.
pub const FLOAT8_OID: u32 = 701;
/// `unknown`.
pub const UNKNOWN_OID: u32 = 705;
/// `bpchar`.
pub const BPCHAR_OID: u32 = 1042;
/// `varchar`.
pub const VARCHAR_OID: u32 = 1043;
/// `json`.
pub const JSON_OID: u32 = 114;
/// `date`.
pub const DATE_OID: u32 = 1082;
/// `timestamp`.
pub const TIMESTAMP_OID: u32 = 1114;
/// `timestamptz`.
pub const TIMESTAMPTZ_OID: u32 = 1184;
/// `time`.
pub const TIME_OID: u32 = 1083;
/// `interval`.
pub const INTERVAL_OID: u32 = 1186;
/// `timetz`.
pub const TIMETZ_OID: u32 = 1266;
/// `uuid`.
pub const UUID_OID: u32 = 2950;
/// `numeric`.
pub const NUMERIC_OID: u32 = 1700;
/// `jsonb`.
pub const JSONB_OID: u32 = 3802;

const MICROS_PER_DAY: i64 = 86_400_000_000;
const MAX_OFFSET: i32 = 15 * 3600 + 59 * 60 + 59;
const NUMERIC_POSITIVE: u16 = 0x0000;
const NUMERIC_NEGATIVE: u16 = 0x4000;
const NUMERIC_NAN: u16 = 0xC000;
const MAX_NUMERIC_GROUPS: usize = 1000;
const JSONB_VERSION: u8 = 1;

/// The type OID PostgreSQL gives `sql_type`.
pub fn oid_of(sql_type: SqlType) -> u32 {
    match sql_type {
        SqlType::SmallInt => INT2_OID,
        SqlType::Integer => INT4_OID,
        SqlType::BigInt => INT8_OID,
        SqlType::Real => FLOAT4_OID,
        SqlType::Double => FLOAT8_OID,
        SqlType::Boolean => BOOL_OID,
        SqlType::Text => TEXT_OID,
        SqlType::Varchar(_) => VARCHAR_OID,
        SqlType::Numeric(_) => NUMERIC_OID,
        SqlType::Date => DATE_OID,
        SqlType::Timestamp(_) => TIMESTAMP_OID,
        SqlType::TimestampTz(_) => TIMESTAMPTZ_OID,
        SqlType::Json => JSON_OID,
        SqlType::Jsonb => JSONB_OID,
        SqlType::Time(_) => TIME_OID,
        SqlType::TimeTz(_) => TIMETZ_OID,
        SqlType::Interval { .. } => INTERVAL_OID,
        SqlType::Bytea => BYTEA_OID,
        SqlType::Uuid => UUID_OID,
        SqlType::Bpchar(_) => BPCHAR_OID,
    }
}

/// The SQL type a parameter declared with `oid` takes, or `None` for an unspecified or
/// unknown type and for types this release does not store, whose values are then read as
/// constants of unknown type.
pub fn sql_type_of(oid: u32) -> Option<SqlType> {
    Some(match oid {
        BOOL_OID => SqlType::Boolean,
        INT2_OID => SqlType::SmallInt,
        INT4_OID => SqlType::Integer,
        INT8_OID => SqlType::BigInt,
        FLOAT4_OID => SqlType::Real,
        FLOAT8_OID => SqlType::Double,
        TEXT_OID | NAME_OID => SqlType::Text,
        BPCHAR_OID => SqlType::Bpchar(None),
        TIME_OID => SqlType::Time(None),
        TIMETZ_OID => SqlType::TimeTz(None),
        INTERVAL_OID => SqlType::Interval {
            fields: None,
            precision: None,
        },
        BYTEA_OID => SqlType::Bytea,
        UUID_OID => SqlType::Uuid,
        VARCHAR_OID => SqlType::Varchar(None),
        NUMERIC_OID => SqlType::Numeric(None),
        DATE_OID => SqlType::Date,
        TIMESTAMP_OID => SqlType::Timestamp(None),
        TIMESTAMPTZ_OID => SqlType::TimestampTz(None),
        JSON_OID => SqlType::Json,
        JSONB_OID => SqlType::Jsonb,
        _ => return None,
    })
}

fn type_size(oid: u32) -> i16 {
    match oid {
        BOOL_OID => 1,
        INT2_OID => 2,
        INT4_OID | FLOAT4_OID | DATE_OID => 4,
        INT8_OID | FLOAT8_OID | TIMESTAMP_OID | TIMESTAMPTZ_OID | TIME_OID => 8,
        TIMETZ_OID => 12,
        INTERVAL_OID | UUID_OID => 16,
        _ => -1,
    }
}

/// The columns of a result as `RowDescription` describes them: each column's type from what
/// SQL knows before reading rows, or else from its first value that is not NULL, or `text`.
pub fn describe_columns(
    columns: &[OutputColumn],
    rows: &[Vec<Value>],
    formats: &[Format],
) -> Vec<ColumnDescription> {
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let sql_type = column.sql_type.or_else(|| {
                rows.iter()
                    .find_map(|row| row.get(index).and_then(Value::sql_type))
            });
            let type_oid = sql_type.map_or(TEXT_OID, oid_of);
            let type_modifier = match sql_type {
                Some(SqlType::Varchar(Some(length))) => {
                    i32::try_from(length).map_or(-1, |length| length.saturating_add(4))
                }
                Some(SqlType::Numeric(Some(modifier))) => {
                    ((i32::from(modifier.precision) << 16) | (i32::from(modifier.scale) & 0xFFFF))
                        + 4
                }
                Some(
                    SqlType::Timestamp(Some(precision))
                    | SqlType::TimestampTz(Some(precision))
                    | SqlType::Time(Some(precision))
                    | SqlType::TimeTz(Some(precision)),
                ) => i32::from(precision),
                Some(SqlType::Bpchar(Some(length))) => {
                    i32::try_from(length).map_or(-1, |length| length.saturating_add(4))
                }
                _ => -1,
            };
            ColumnDescription {
                name: column.name.clone(),
                type_oid,
                type_size: type_size(type_oid),
                type_modifier,
                format: format_at(formats, index),
            }
        })
        .collect()
}

/// The format of column or parameter `index` from a Bind message's list: none means text,
/// one applies to all.
pub fn format_at(formats: &[Format], index: usize) -> Format {
    match formats {
        [] => Format::Text,
        [only] => *only,
        many => many.get(index).copied().unwrap_or(Format::Text),
    }
}

fn cannot_send(value: &Value, oid: u32) -> SqlError {
    SqlError::new(
        SqlState::INTERNAL_ERROR,
        format!(
            "cannot send a {} value as type OID {oid}",
            value.sql_type().map_or("NULL".to_owned(), SqlType::name)
        ),
    )
}

/// The bytes of `value` in a `DataRow`, for a column described with `oid` in `format`, with a
/// `timestamp with time zone` shown `offset_seconds` east of UTC; `None` for NULL.
pub fn encode_value(
    value: &Value,
    oid: u32,
    format: Format,
    offset_seconds: i32,
) -> Result<Option<Vec<u8>>, SqlError> {
    let Some(text) = value.to_text_at(offset_seconds) else {
        return Ok(None);
    };
    if format == Format::Text {
        return Ok(Some(text.into_bytes()));
    }
    let bytes = match oid {
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
        _ => text.into_bytes(),
    };
    Ok(Some(bytes))
}

fn numeric_binary(text: &str) -> Vec<u8> {
    if text == "NaN" {
        let mut out = Vec::with_capacity(8);
        out.extend_from_slice(&0i16.to_be_bytes());
        out.extend_from_slice(&0i16.to_be_bytes());
        out.extend_from_slice(&NUMERIC_NAN.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        return out;
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

fn invalid_binary(number: usize, oid: u32) -> SqlError {
    SqlError::new(
        SqlState::INVALID_BINARY_REPRESENTATION,
        format!("incorrect binary data format in bind parameter {number} (type OID {oid})"),
    )
}

fn invalid_utf8() -> SqlError {
    SqlError::new(
        SqlState::CHARACTER_NOT_IN_REPERTOIRE,
        "invalid byte sequence for encoding \"UTF8\"",
    )
}

/// The text of parameter `number` (1-based) sent as `bytes` in `format` with the declared type
/// `oid`, as the SQL layer reads parameter values.
pub fn decode_parameter(
    bytes: &[u8],
    oid: u32,
    format: Format,
    number: usize,
) -> Result<String, SqlError> {
    let text = |bytes: &[u8]| {
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| invalid_utf8())
    };
    if format == Format::Text {
        return text(bytes);
    }
    let fixed = |length: usize| {
        if bytes.len() == length {
            Ok(bytes)
        } else {
            Err(invalid_binary(number, oid))
        }
    };
    let value = match oid {
        BOOL_OID => match fixed(1)?[0] {
            0 => Value::Boolean(false),
            1 => Value::Boolean(true),
            _ => return Err(invalid_binary(number, oid)),
        },
        INT2_OID => {
            let bytes = fixed(2)?;
            Value::SmallInt(i16::from_be_bytes([bytes[0], bytes[1]]))
        }
        INT4_OID => {
            let bytes = fixed(4)?;
            Value::Integer(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        }
        INT8_OID => {
            let mut word = [0u8; 8];
            word.copy_from_slice(fixed(8)?);
            Value::BigInt(i64::from_be_bytes(word))
        }
        FLOAT4_OID => {
            let bytes = fixed(4)?;
            Value::Real(f32::from_bits(u32::from_be_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3],
            ])))
        }
        FLOAT8_OID => {
            let mut word = [0u8; 8];
            word.copy_from_slice(fixed(8)?);
            Value::Double(f64::from_bits(u64::from_be_bytes(word)))
        }
        NUMERIC_OID => {
            return numeric_text(bytes).ok_or_else(|| invalid_binary(number, oid));
        }
        DATE_OID => {
            let bytes = fixed(4)?;
            Value::Date(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        }
        TIMESTAMP_OID | TIMESTAMPTZ_OID => {
            let mut word = [0u8; 8];
            word.copy_from_slice(fixed(8)?);
            let micros = i64::from_be_bytes(word);
            if oid == TIMESTAMP_OID {
                Value::Timestamp(micros)
            } else {
                Value::TimestampTz(micros)
            }
        }
        TIME_OID => {
            let mut word = [0u8; 8];
            word.copy_from_slice(fixed(8)?);
            let micros = i64::from_be_bytes(word);
            if !(0..=MICROS_PER_DAY).contains(&micros) {
                return Err(invalid_binary(number, oid));
            }
            Value::Time(micros)
        }
        TIMETZ_OID => {
            let bytes = fixed(12)?;
            let mut word = [0u8; 8];
            word.copy_from_slice(&bytes[..8]);
            let micros = i64::from_be_bytes(word);
            let west = i32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
            if !(0..=MICROS_PER_DAY).contains(&micros)
                || !(-MAX_OFFSET..=MAX_OFFSET).contains(&west)
            {
                return Err(invalid_binary(number, oid));
            }
            Value::TimeTz {
                micros,
                offset_seconds: -west,
            }
        }
        INTERVAL_OID => {
            let bytes = fixed(16)?;
            let mut word = [0u8; 8];
            word.copy_from_slice(&bytes[..8]);
            Value::Interval(Interval {
                micros: i64::from_be_bytes(word),
                days: i32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
                months: i32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
            })
        }
        BYTEA_OID => Value::Bytea(bytes.to_vec()),
        UUID_OID => {
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(fixed(16)?);
            Value::Uuid(uuid)
        }
        JSONB_OID => match bytes.split_first() {
            Some((&JSONB_VERSION, rest)) => return text(rest),
            _ => return Err(invalid_binary(number, oid)),
        },
        0 | JSON_OID | TEXT_OID | VARCHAR_OID | NAME_OID | BPCHAR_OID | UNKNOWN_OID => {
            return text(bytes);
        }
        _ => {
            return Err(SqlError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!(
                    "the binary format of type OID {oid} (parameter ${number}) is not supported yet"
                ),
            ));
        }
    };
    Ok(value.to_text_at(0).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_parameters_of_the_common_types_decode_to_their_text() {
        let cases: [(&[u8], u32, &str); 8] = [
            (&[1], BOOL_OID, "t"),
            (&[0], BOOL_OID, "f"),
            (&(-2i16).to_be_bytes(), INT2_OID, "-2"),
            (&70000i32.to_be_bytes(), INT4_OID, "70000"),
            (&i64::MIN.to_be_bytes(), INT8_OID, "-9223372036854775808"),
            (&1.5f32.to_bits().to_be_bytes(), FLOAT4_OID, "1.5"),
            (&0.1f64.to_bits().to_be_bytes(), FLOAT8_OID, "0.1"),
            (b"caf\xc3\xa9", TEXT_OID, "caf\u{e9}"),
        ];
        for (bytes, oid, text) in cases {
            assert_eq!(
                decode_parameter(bytes, oid, Format::Binary, 1).unwrap(),
                text
            );
        }
        assert_eq!(
            decode_parameter(b"42", INT4_OID, Format::Text, 1).unwrap(),
            "42"
        );
    }

    #[test]
    fn wrong_lengths_bad_utf8_and_unknown_binary_types_are_errors() {
        let state =
            |result: Result<String, SqlError>| result.unwrap_err().state().code().to_owned();
        assert_eq!(
            state(decode_parameter(&[0, 1], INT4_OID, Format::Binary, 1)),
            "22P03"
        );
        assert_eq!(
            state(decode_parameter(&[2], BOOL_OID, Format::Binary, 1)),
            "22P03"
        );
        assert_eq!(
            state(decode_parameter(b"\xff", TEXT_OID, Format::Text, 1)),
            "22021"
        );
        assert_eq!(
            state(decode_parameter(b"\xff", 0, Format::Binary, 1)),
            "22021"
        );
        assert_eq!(
            state(decode_parameter(&[0; 8], 1186, Format::Binary, 1)),
            "0A000"
        );
        assert_eq!(
            state(decode_parameter(&[0; 3], DATE_OID, Format::Binary, 1)),
            "22P03"
        );
        assert_eq!(
            state(decode_parameter(&[2, b'{'], JSONB_OID, Format::Binary, 1)),
            "22P03"
        );
    }

    #[test]
    fn values_encode_in_text_and_binary() {
        assert_eq!(
            encode_value(&Value::Null, INT4_OID, Format::Binary, 0).unwrap(),
            None
        );
        assert_eq!(
            encode_value(&Value::Integer(7), INT4_OID, Format::Binary, 0).unwrap(),
            Some(7i32.to_be_bytes().to_vec())
        );
        assert_eq!(
            encode_value(&Value::Integer(7), INT8_OID, Format::Binary, 0).unwrap(),
            Some(7i64.to_be_bytes().to_vec())
        );
        assert_eq!(
            encode_value(&Value::Boolean(true), BOOL_OID, Format::Text, 0).unwrap(),
            Some(b"t".to_vec())
        );
        assert_eq!(
            encode_value(&Value::Double(2.5), FLOAT8_OID, Format::Binary, 0).unwrap(),
            Some(2.5f64.to_bits().to_be_bytes().to_vec())
        );
        assert_eq!(
            encode_value(&Value::BigInt(1 << 40), INT4_OID, Format::Binary, 0)
                .unwrap_err()
                .state()
                .code(),
            "XX000"
        );
        assert_eq!(
            encode_value(&Value::Integer(3), TEXT_OID, Format::Binary, 0).unwrap(),
            Some(b"3".to_vec())
        );
    }

    fn numeric(text: &str) -> Value {
        Value::Numeric(eggdb_sql::Numeric::parse(text).unwrap())
    }

    #[test]
    fn numerics_use_postgresqls_base_10000_binary_form_both_ways() {
        let cases: [(&str, &[i16]); 9] = [
            ("0", &[0, 0, 0, 0]),
            ("0.00", &[0, 0, 0, 2]),
            ("1", &[1, 0, 0, 0, 1]),
            ("-12345.678", &[3, 1, 0x4000, 3, 1, 2345, 6780]),
            ("10000", &[1, 1, 0, 0, 1]),
            ("0.0001", &[1, -1, 0, 4, 1]),
            ("0.00001", &[1, -2, 0, 5, 1000]),
            ("123456789.5", &[4, 2, 0, 1, 1, 2345, 6789, 5000]),
            ("NaN", &[0, 0, -16384, 0]),
        ];
        for (text, words) in cases {
            let expected: Vec<u8> = words.iter().flat_map(|word| word.to_be_bytes()).collect();
            assert_eq!(
                encode_value(&numeric(text), NUMERIC_OID, Format::Binary, 0).unwrap(),
                Some(expected.clone()),
                "{text}"
            );
            assert_eq!(
                decode_parameter(&expected, NUMERIC_OID, Format::Binary, 1).unwrap(),
                text
            );
        }
        for hostile in [
            &[0u8, 1][..],
            &[0, 1, 0, 0, 0, 0, 0, 0],
            &[0, 1, 0, 0, 0, 0, 0, 0, 0x27, 0x10],
            &[0, 0, 0, 0, 0x80, 0, 0, 0],
            &[0xFF, 0xFF, 0, 0, 0, 0, 0, 0],
        ] {
            assert!(decode_parameter(hostile, NUMERIC_OID, Format::Binary, 1).is_err());
        }
        let mut seed = 0x1234_5678u64;
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let length = (seed % 20) as usize;
            let bytes: Vec<u8> = (0..length).map(|at| (seed >> (at % 56)) as u8).collect();
            let _ = decode_parameter(&bytes, NUMERIC_OID, Format::Binary, 1);
        }
    }

    #[test]
    fn dates_times_and_json_round_trip_in_binary() {
        let days = 8_825i32;
        assert_eq!(
            decode_parameter(&days.to_be_bytes(), DATE_OID, Format::Binary, 1).unwrap(),
            "2024-02-29"
        );
        assert_eq!(
            encode_value(&Value::Date(days), DATE_OID, Format::Binary, 0).unwrap(),
            Some(days.to_be_bytes().to_vec())
        );
        let micros = 762_529_501_123_456i64;
        assert_eq!(
            decode_parameter(&micros.to_be_bytes(), TIMESTAMP_OID, Format::Binary, 1).unwrap(),
            "2024-02-29 13:45:01.123456"
        );
        assert_eq!(
            decode_parameter(&micros.to_be_bytes(), TIMESTAMPTZ_OID, Format::Binary, 1).unwrap(),
            "2024-02-29 13:45:01.123456+00"
        );
        assert_eq!(
            encode_value(
                &Value::TimestampTz(micros),
                TIMESTAMPTZ_OID,
                Format::Text,
                3600
            )
            .unwrap(),
            Some(b"2024-02-29 14:45:01.123456+01".to_vec())
        );
        assert_eq!(
            encode_value(
                &Value::Jsonb("{\"a\": 1}".into()),
                JSONB_OID,
                Format::Binary,
                0
            )
            .unwrap(),
            Some(b"\x01{\"a\": 1}".to_vec())
        );
        assert_eq!(
            decode_parameter(b"\x01[1]", JSONB_OID, Format::Binary, 1).unwrap(),
            "[1]"
        );
    }

    #[test]
    fn columns_take_their_static_type_then_their_first_value() {
        let columns = [
            OutputColumn {
                name: "a".into(),
                sql_type: Some(SqlType::Varchar(Some(10))),
            },
            OutputColumn {
                name: "b".into(),
                sql_type: None,
            },
            OutputColumn {
                name: "c".into(),
                sql_type: None,
            },
        ];
        let rows = vec![
            vec![Value::Null, Value::Null, Value::Null],
            vec![Value::Null, Value::BigInt(1), Value::Null],
        ];
        let described = describe_columns(&columns, &rows, &[Format::Binary]);
        assert_eq!(
            described
                .iter()
                .map(|column| (column.type_oid, column.type_size, column.type_modifier))
                .collect::<Vec<_>>(),
            [(VARCHAR_OID, -1, 14), (INT8_OID, 8, -1), (TEXT_OID, -1, -1)]
        );
        assert!(
            described
                .iter()
                .all(|column| column.format == Format::Binary)
        );
    }
}
