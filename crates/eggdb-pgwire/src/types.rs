//! PostgreSQL's type OIDs for EggDB's SQL types, and values in their text and binary forms.

use crate::messages::{ColumnDescription, Format};
use eggdb_sql::{OutputColumn, SqlError, SqlState, SqlType, Value};

/// `bool`.
pub const BOOL_OID: u32 = 16;
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
        TEXT_OID | NAME_OID | BPCHAR_OID => SqlType::Text,
        VARCHAR_OID => SqlType::Varchar(None),
        _ => return None,
    })
}

fn type_size(oid: u32) -> i16 {
    match oid {
        BOOL_OID => 1,
        INT2_OID => 2,
        INT4_OID | FLOAT4_OID => 4,
        INT8_OID | FLOAT8_OID => 8,
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

/// The bytes of `value` in a `DataRow`, for a column described with `oid` in `format`; `None`
/// for NULL.
pub fn encode_value(value: &Value, oid: u32, format: Format) -> Result<Option<Vec<u8>>, SqlError> {
    let Some(text) = value.to_text() else {
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
        _ => text.into_bytes(),
    };
    Ok(Some(bytes))
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
        0 | TEXT_OID | VARCHAR_OID | NAME_OID | BPCHAR_OID | UNKNOWN_OID => {
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
    Ok(value.to_text().unwrap_or_default())
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
            state(decode_parameter(&[0; 8], 1114, Format::Binary, 1)),
            "0A000"
        );
    }

    #[test]
    fn values_encode_in_text_and_binary() {
        assert_eq!(
            encode_value(&Value::Null, INT4_OID, Format::Binary).unwrap(),
            None
        );
        assert_eq!(
            encode_value(&Value::Integer(7), INT4_OID, Format::Binary).unwrap(),
            Some(7i32.to_be_bytes().to_vec())
        );
        assert_eq!(
            encode_value(&Value::Integer(7), INT8_OID, Format::Binary).unwrap(),
            Some(7i64.to_be_bytes().to_vec())
        );
        assert_eq!(
            encode_value(&Value::Boolean(true), BOOL_OID, Format::Text).unwrap(),
            Some(b"t".to_vec())
        );
        assert_eq!(
            encode_value(&Value::Double(2.5), FLOAT8_OID, Format::Binary).unwrap(),
            Some(2.5f64.to_bits().to_be_bytes().to_vec())
        );
        assert_eq!(
            encode_value(&Value::BigInt(1 << 40), INT4_OID, Format::Binary)
                .unwrap_err()
                .state()
                .code(),
            "XX000"
        );
        assert_eq!(
            encode_value(&Value::Integer(3), TEXT_OID, Format::Binary).unwrap(),
            Some(b"3".to_vec())
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
