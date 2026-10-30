//! PostgreSQL's type OIDs for EggDB's SQL types, and values in their text and binary forms.

use crate::messages::{ColumnDescription, Format};
use eggdb_sql::{ArrayElement, OutputColumn, SqlError, SqlState, SqlType, TimeZone, Value};

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
/// `bool[]`.
pub const BOOL_ARRAY_OID: u32 = 1000;
/// `int2[]`.
pub const INT2_ARRAY_OID: u32 = 1005;
/// `int4[]`.
pub const INT4_ARRAY_OID: u32 = 1007;
/// `text[]`.
pub const TEXT_ARRAY_OID: u32 = 1009;
/// `bpchar[]`.
pub const BPCHAR_ARRAY_OID: u32 = 1014;
/// `varchar[]`.
pub const VARCHAR_ARRAY_OID: u32 = 1015;
/// `int8[]`.
pub const INT8_ARRAY_OID: u32 = 1016;
/// `float4[]`.
pub const FLOAT4_ARRAY_OID: u32 = 1021;
/// `float8[]`.
pub const FLOAT8_ARRAY_OID: u32 = 1022;
/// `numeric[]`.
pub const NUMERIC_ARRAY_OID: u32 = 1231;
/// `oid`.
pub const OID_OID: u32 = 26;
/// `xid`.
pub const XID_OID: u32 = 28;
/// `"char"`.
pub const CHAR_OID: u32 = 18;
/// `regproc`.
pub const REGPROC_OID: u32 = 24;
/// `regclass`.
pub const REGCLASS_OID: u32 = 2205;
/// `regtype`.
pub const REGTYPE_OID: u32 = 2206;
/// `regnamespace`.
pub const REGNAMESPACE_OID: u32 = 4089;
/// `regrole`.
pub const REGROLE_OID: u32 = 4096;

const IDENTIFIER_OIDS: [u32; 7] = [
    OID_OID,
    XID_OID,
    REGPROC_OID,
    REGCLASS_OID,
    REGTYPE_OID,
    REGNAMESPACE_OID,
    REGROLE_OID,
];

const ARRAY_OIDS: [u32; 16] = [
    BOOL_ARRAY_OID,
    INT2_ARRAY_OID,
    INT4_ARRAY_OID,
    TEXT_ARRAY_OID,
    BPCHAR_ARRAY_OID,
    VARCHAR_ARRAY_OID,
    INT8_ARRAY_OID,
    FLOAT4_ARRAY_OID,
    FLOAT8_ARRAY_OID,
    NUMERIC_ARRAY_OID,
    1028,
    1003,
    1002,
    1034,
    22,
    30,
];

/// The type OID PostgreSQL gives `sql_type`.
pub fn oid_of(sql_type: SqlType) -> u32 {
    eggdb_sql::type_oid(sql_type)
}

/// The type OID PostgreSQL gives an array of `element`, such as 1007 for `int4[]`.
pub fn array_oid(element: ArrayElement) -> u32 {
    eggdb_sql::type_oid(SqlType::Array(element))
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
        CHAR_OID => 1,
        NAME_OID => 64,
        oid if IDENTIFIER_OIDS.contains(&oid) => 4,
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

/// The bytes of `value` in a `DataRow`, for a column described with `oid` in `format`, with a
/// `timestamp with time zone` shown in `zone`; `None` for NULL.
pub fn encode_value(
    value: &Value,
    oid: u32,
    format: Format,
    zone: TimeZone,
) -> Result<Option<Vec<u8>>, SqlError> {
    match format {
        Format::Binary => eggdb_sql::encode_pg_binary(value, oid, zone),
        Format::Text => Ok(value.to_text_in(zone).map(String::into_bytes)),
    }
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
    if ARRAY_OIDS.contains(&oid) {
        return Err(SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("array parameters (type OID {oid}, parameter ${number}) are not supported yet"),
        ));
    }
    if format == Format::Text {
        return std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| invalid_utf8());
    }
    eggdb_sql::decode_pg_binary_text(bytes, oid).map_err(|error| {
        if error.state() == SqlState::INVALID_BINARY_REPRESENTATION {
            invalid_binary(number, oid)
        } else if error.state() == SqlState::FEATURE_NOT_SUPPORTED {
            SqlError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!(
                    "the binary format of type OID {oid} (parameter ${number}) is not supported yet"
                ),
            )
        } else {
            error
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_sql::{ArrayValue, Interval};

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
            state(decode_parameter(&[0; 8], INTERVAL_OID, Format::Binary, 1)),
            "22P03"
        );
        assert_eq!(
            state(decode_parameter(&[0; 8], 790, Format::Binary, 1)),
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
    fn arrays_take_postgresqls_array_oids_and_binary_form() {
        let array = Value::Array(Box::new(ArrayValue {
            element: ArrayElement::Integer,
            lower_bound: 1,
            items: vec![Value::Integer(7), Value::Null],
        }));
        let mut expected = Vec::new();
        for word in [1i32, 1, 23, 2, 1, 4, 7, -1] {
            expected.extend_from_slice(&word.to_be_bytes());
        }
        assert_eq!(
            encode_value(&array, INT4_ARRAY_OID, Format::Binary, TimeZone::UTC).unwrap(),
            Some(expected)
        );
        assert_eq!(
            encode_value(&array, INT4_ARRAY_OID, Format::Text, TimeZone::UTC).unwrap(),
            Some(b"{7,NULL}".to_vec())
        );
        assert_eq!(
            encode_value(&Value::Null, INT4_ARRAY_OID, Format::Binary, TimeZone::UTC).unwrap(),
            None
        );
        assert!(encode_value(&Value::Integer(7), INT4_ARRAY_OID, Format::Binary, TimeZone::UTC).is_err());
        let empty = Value::Array(Box::new(ArrayValue {
            element: ArrayElement::Text,
            lower_bound: 1,
            items: Vec::new(),
        }));
        let mut expected = Vec::new();
        for word in [0i32, 0, 25] {
            expected.extend_from_slice(&word.to_be_bytes());
        }
        assert_eq!(
            encode_value(&empty, TEXT_ARRAY_OID, Format::Binary, TimeZone::UTC).unwrap(),
            Some(expected)
        );
        let texts = Value::Array(Box::new(ArrayValue {
            element: ArrayElement::Varchar,
            lower_bound: 1,
            items: vec![Value::Text("ab".into())],
        }));
        let mut expected = Vec::new();
        for word in [1i32, 0, 1043, 1, 1, 2] {
            expected.extend_from_slice(&word.to_be_bytes());
        }
        expected.extend_from_slice(b"ab");
        assert_eq!(
            encode_value(&texts, VARCHAR_ARRAY_OID, Format::Binary, TimeZone::UTC).unwrap(),
            Some(expected)
        );
        let pairs = [
            (ArrayElement::SmallInt, 1005),
            (ArrayElement::Integer, 1007),
            (ArrayElement::BigInt, 1016),
            (ArrayElement::Real, 1021),
            (ArrayElement::Double, 1022),
            (ArrayElement::Numeric, 1231),
            (ArrayElement::Boolean, 1000),
            (ArrayElement::Text, 1009),
            (ArrayElement::Varchar, 1015),
            (ArrayElement::Bpchar, 1014),
        ];
        for (element, oid) in pairs {
            assert_eq!(oid_of(SqlType::Array(element)), oid);
            assert_eq!(sql_type_of(oid), None);
            assert_eq!(
                decode_parameter(b"{1}", oid, Format::Text, 1)
                    .unwrap_err()
                    .state()
                    .code(),
                "0A000"
            );
        }
    }

    #[test]
    fn values_encode_in_text_and_binary() {
        assert_eq!(
            encode_value(&Value::Null, INT4_OID, Format::Binary, TimeZone::UTC).unwrap(),
            None
        );
        assert_eq!(
            encode_value(&Value::Integer(7), INT4_OID, Format::Binary, TimeZone::UTC).unwrap(),
            Some(7i32.to_be_bytes().to_vec())
        );
        assert_eq!(
            encode_value(&Value::Integer(7), INT8_OID, Format::Binary, TimeZone::UTC).unwrap(),
            Some(7i64.to_be_bytes().to_vec())
        );
        assert_eq!(
            encode_value(&Value::Boolean(true), BOOL_OID, Format::Text, TimeZone::UTC).unwrap(),
            Some(b"t".to_vec())
        );
        assert_eq!(
            encode_value(&Value::Double(2.5), FLOAT8_OID, Format::Binary, TimeZone::UTC).unwrap(),
            Some(2.5f64.to_bits().to_be_bytes().to_vec())
        );
        assert_eq!(
            encode_value(&Value::BigInt(1 << 40), INT4_OID, Format::Binary, TimeZone::UTC)
                .unwrap_err()
                .state()
                .code(),
            "XX000"
        );
        assert_eq!(
            encode_value(&Value::Integer(3), TEXT_OID, Format::Binary, TimeZone::UTC).unwrap(),
            Some(b"3".to_vec())
        );
    }

    #[test]
    fn item_31_values_round_trip_through_their_binary_forms() {
        let values = [
            (Value::Time(0), TIME_OID),
            (Value::Time(86_400_000_000), TIME_OID),
            (
                Value::TimeTz {
                    micros: 1,
                    offset_seconds: -(15 * 3600 + 59 * 60 + 59),
                },
                TIMETZ_OID,
            ),
            (
                Value::Interval(Interval {
                    months: i32::MIN,
                    days: i32::MAX,
                    micros: i64::MIN,
                }),
                INTERVAL_OID,
            ),
            (Value::Bytea(vec![0, 0xFF, b'\\']), BYTEA_OID),
            (Value::Bytea(Vec::new()), BYTEA_OID),
            (Value::Uuid([0xA5; 16]), UUID_OID),
            (Value::Bpchar("ab  ".into()), BPCHAR_OID),
        ];
        for (value, oid) in values {
            let bytes = encode_value(&value, oid, Format::Binary, TimeZone::UTC)
                .unwrap()
                .unwrap();
            let text = decode_parameter(&bytes, oid, Format::Binary, 1).unwrap();
            assert_eq!(Some(text), value.to_text_in(TimeZone::UTC), "{value:?}");
            assert_eq!(oid_of(value.sql_type().unwrap()), oid);
            assert_eq!(
                sql_type_of(oid).map(SqlType::name),
                value.sql_type().map(SqlType::name)
            );
        }
        let mut random = 0x0B1A_5EED_0000_0031u64;
        for _ in 0..20_000 {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let length = (random % 20) as usize;
            let bytes: Vec<u8> = (0..length)
                .map(|shift| (random >> (shift % 8 * 8)) as u8)
                .collect();
            for oid in [TIME_OID, TIMETZ_OID, INTERVAL_OID, BYTEA_OID, UUID_OID] {
                let _ = decode_parameter(&bytes, oid, Format::Binary, 1);
            }
        }
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
                encode_value(&numeric(text), NUMERIC_OID, Format::Binary, TimeZone::UTC).unwrap(),
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
            encode_value(&Value::Date(days), DATE_OID, Format::Binary, TimeZone::UTC).unwrap(),
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
                TimeZone::fixed(3600).unwrap()
            )
            .unwrap(),
            Some(b"2024-02-29 14:45:01.123456+01".to_vec())
        );
        assert_eq!(
            encode_value(
                &Value::Jsonb("{\"a\": 1}".into()),
                JSONB_OID,
                Format::Binary,
                TimeZone::UTC
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
