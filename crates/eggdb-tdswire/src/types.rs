use crate::tokens::COLLATION;
use eggdb_sql::{Dialect, OutputColumn, SqlType, Value};

const MAX_FIXED_NVARCHAR_UNITS: usize = 4000;
const MAX_FIXED_VARBINARY_BYTES: usize = 8000;
const MAX_DECIMAL_PRECISION: u16 = 38;
const DAYS_FROM_YEAR_ONE_TO_2000: i64 = 730_119;
const LAST_DAY_FROM_YEAR_ONE: i64 = 3_652_058;
const MICROS_PER_DAY: i64 = 86_400_000_000;
const PLP_MAX: u16 = 0xFFFF;
const NULL_LENGTH: u16 = 0xFFFF;
const PLP_NULL: u64 = u64::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WireType {
    Int(u8),
    Float(u8),
    Bit,
    Decimal { precision: u8, scale: u8 },
    DateTime2 { scale: u8 },
    Date,
    Time { scale: u8 },
    DateTimeOffset { scale: u8 },
    Guid,
    NVarChar { units: u16 },
    NVarCharMax,
    VarBinary { bytes: u16 },
    VarBinaryMax,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WireColumn {
    pub(crate) name: String,
    pub(crate) kind: WireType,
    pub(crate) as_text: bool,
}

fn decimal_bytes(precision: u8) -> u8 {
    match precision {
        0..=9 => 5,
        10..=19 => 9,
        20..=28 => 13,
        _ => 17,
    }
}

fn time_bytes(scale: u8) -> u8 {
    match scale {
        0..=2 => 3,
        3..=4 => 4,
        _ => 5,
    }
}

fn integer_width(value: i64) -> u8 {
    if i16::try_from(value).is_ok() {
        2
    } else if i32::try_from(value).is_ok() {
        4
    } else {
        8
    }
}

impl WireType {
    pub(crate) fn push_type_info(self, out: &mut Vec<u8>) {
        match self {
            Self::Int(width) => out.extend_from_slice(&[0x26, width]),
            Self::Float(width) => out.extend_from_slice(&[0x6D, width]),
            Self::Bit => out.extend_from_slice(&[0x68, 1]),
            Self::Decimal { precision, scale } => {
                out.extend_from_slice(&[0x6A, decimal_bytes(precision), precision, scale]);
            }
            Self::DateTime2 { scale } => out.extend_from_slice(&[0x2A, scale]),
            Self::Date => out.push(0x28),
            Self::Time { scale } => out.extend_from_slice(&[0x29, scale]),
            Self::DateTimeOffset { scale } => out.extend_from_slice(&[0x2B, scale]),
            Self::Guid => out.extend_from_slice(&[0x24, 16]),
            Self::NVarChar { units } => {
                out.push(0xE7);
                out.extend_from_slice(&units.saturating_mul(2).to_le_bytes());
                out.extend_from_slice(&COLLATION);
            }
            Self::NVarCharMax => {
                out.push(0xE7);
                out.extend_from_slice(&PLP_MAX.to_le_bytes());
                out.extend_from_slice(&COLLATION);
            }
            Self::VarBinary { bytes } => {
                out.push(0xA5);
                out.extend_from_slice(&bytes.to_le_bytes());
            }
            Self::VarBinaryMax => {
                out.push(0xA5);
                out.extend_from_slice(&PLP_MAX.to_le_bytes());
            }
        }
    }
}

fn utf16_units(text: &str) -> usize {
    text.encode_utf16().count()
}

fn nvarchar_for(units: usize) -> WireType {
    if units <= MAX_FIXED_NVARCHAR_UNITS {
        WireType::NVarChar {
            units: units.max(1) as u16,
        }
    } else {
        WireType::NVarCharMax
    }
}

fn character_text(value: &Value) -> Option<&str> {
    match value {
        Value::Text(text) | Value::Bpchar(text) => Some(text),
        _ => None,
    }
}

fn scaled_decimal(value: &Value, precision: u8, scale: u8) -> Option<(bool, u128)> {
    let Value::Numeric(number) = value else {
        return None;
    };
    if number.is_nan() {
        return None;
    }
    let text = number.to_string();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.as_str()),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let fraction = fraction.trim_end_matches('0');
    if fraction.len() > usize::from(scale) {
        return None;
    }
    let whole = whole.trim_start_matches('0');
    if whole.len() + usize::from(scale) > usize::from(precision) {
        return None;
    }
    let mut all = String::with_capacity(usize::from(precision));
    all.push_str(whole);
    all.push_str(fraction);
    for _ in fraction.len()..usize::from(scale) {
        all.push('0');
    }
    let units: u128 = if all.is_empty() { 0 } else { all.parse().ok()? };
    Some((negative && units != 0, units))
}

fn datetime2_parts(value: &Value) -> Option<(i64, i64)> {
    let Value::Timestamp(micros) = value else {
        return None;
    };
    let days = micros.div_euclid(MICROS_PER_DAY) + DAYS_FROM_YEAR_ONE_TO_2000;
    (0..=LAST_DAY_FROM_YEAR_ONE)
        .contains(&days)
        .then_some((days, micros.rem_euclid(MICROS_PER_DAY)))
}

fn date_from_year_one(value: &Value) -> Option<i64> {
    let Value::Date(days) = value else {
        return None;
    };
    let days = i64::from(*days) + DAYS_FROM_YEAR_ONE_TO_2000;
    (0..=LAST_DAY_FROM_YEAR_ONE).contains(&days).then_some(days)
}

fn utc_parts(value: &Value) -> Option<(i64, i64)> {
    let Value::TimestampTz(micros) = value else {
        return None;
    };
    datetime2_parts(&Value::Timestamp(*micros))
}

fn time_of_day(value: &Value) -> Option<i64> {
    match value {
        Value::Time(micros) if (0..MICROS_PER_DAY).contains(micros) => Some(*micros),
        _ => None,
    }
}

fn time_units(micros: i64, scale: u8) -> i64 {
    if scale >= 7 {
        micros * 10
    } else {
        micros / 10i64.pow(6 - u32::from(scale))
    }
}

fn push_time(out: &mut Vec<u8>, micros: i64, scale: u8) {
    let length = usize::from(time_bytes(scale));
    out.extend_from_slice(&time_units(micros, scale).to_le_bytes()[..length]);
}

fn guid_bytes(uuid: &[u8; 16]) -> [u8; 16] {
    let mut out = *uuid;
    out[..4].reverse();
    out[4..6].reverse();
    out[6..8].reverse();
    out
}

fn decimal_shape(value: &Value) -> Option<(usize, usize)> {
    let Value::Numeric(number) = value else {
        return None;
    };
    if number.is_nan() {
        return None;
    }
    let text = number.to_string();
    let digits = text.trim_start_matches('-');
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    Some((whole.trim_start_matches('0').len(), fraction.len()))
}

fn unconstrained_decimal(values: &[&Value]) -> Option<WireType> {
    let mut whole = 0;
    let mut scale = 0;
    for value in values {
        if matches!(value, Value::Null) {
            continue;
        }
        let (digits, places) = decimal_shape(value)?;
        whole = whole.max(digits);
        scale = scale.max(places);
    }
    let precision = (whole + scale).max(1);
    (precision <= usize::from(MAX_DECIMAL_PRECISION)).then_some(WireType::Decimal {
        precision: precision as u8,
        scale: scale as u8,
    })
}

fn fits(kind: WireType, value: &Value) -> bool {
    match (kind, value) {
        (_, Value::Null) => true,
        (WireType::Int(width), Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_)) => value
            .as_integer()
            .is_some_and(|number| integer_width(number) <= width),
        (WireType::Float(4), Value::Real(_)) => true,
        (WireType::Float(8), Value::Real(_) | Value::Double(_)) => true,
        (WireType::Bit, Value::Boolean(_)) => true,
        (WireType::Decimal { precision, scale }, _) => {
            scaled_decimal(value, precision, scale).is_some()
        }
        (WireType::DateTime2 { .. }, _) => datetime2_parts(value).is_some(),
        (WireType::Date, _) => date_from_year_one(value).is_some(),
        (WireType::Time { .. }, _) => time_of_day(value).is_some(),
        (WireType::DateTimeOffset { .. }, _) => utc_parts(value).is_some(),
        (WireType::Guid, Value::Uuid(_)) => true,
        (WireType::NVarChar { units }, _) => {
            character_text(value).is_some_and(|text| utf16_units(text) <= usize::from(units))
        }
        (WireType::NVarCharMax, _) => character_text(value).is_some(),
        (WireType::VarBinary { bytes }, Value::Bytea(data)) => data.len() <= usize::from(bytes),
        (WireType::VarBinaryMax, Value::Bytea(_)) => true,
        _ => false,
    }
}

fn candidate(sql_type: SqlType, values: &[&Value]) -> Option<WireType> {
    Some(match sql_type {
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt => {
            let declared = match sql_type {
                SqlType::SmallInt => 2,
                SqlType::Integer => 4,
                _ => 8,
            };
            let needed = values
                .iter()
                .filter_map(|value| value.as_integer())
                .map(integer_width)
                .max()
                .unwrap_or(2);
            WireType::Int(declared.max(needed))
        }
        SqlType::Real => WireType::Float(4),
        SqlType::Double => WireType::Float(8),
        SqlType::Boolean => WireType::Bit,
        SqlType::Numeric(Some(modifier))
            if (1..=MAX_DECIMAL_PRECISION).contains(&modifier.precision)
                && modifier.scale >= 0
                && modifier.scale as u16 <= modifier.precision =>
        {
            WireType::Decimal {
                precision: modifier.precision as u8,
                scale: modifier.scale as u8,
            }
        }
        SqlType::Timestamp(precision) => WireType::DateTime2 {
            scale: precision.unwrap_or(6).min(6),
        },
        SqlType::Date => WireType::Date,
        SqlType::Time(precision) => WireType::Time {
            scale: precision.unwrap_or(6).min(6),
        },
        SqlType::TimestampTz(precision) => WireType::DateTimeOffset {
            scale: precision.unwrap_or(6).min(6),
        },
        SqlType::Uuid => WireType::Guid,
        SqlType::Numeric(None) => return unconstrained_decimal(values),
        SqlType::Text | SqlType::Varchar(_) | SqlType::Bpchar(_) => {
            let declared = match sql_type {
                SqlType::Varchar(Some(length)) | SqlType::Bpchar(Some(length)) => length as usize,
                _ => 0,
            };
            let longest = values
                .iter()
                .filter_map(|value| character_text(value))
                .map(utf16_units)
                .max()
                .unwrap_or(0);
            nvarchar_for(declared.max(longest))
        }
        SqlType::Bytea => {
            let longest = values
                .iter()
                .filter_map(|value| match value {
                    Value::Bytea(data) => Some(data.len()),
                    _ => None,
                })
                .max()
                .unwrap_or(0);
            if longest <= MAX_FIXED_VARBINARY_BYTES {
                WireType::VarBinary {
                    bytes: longest.max(1) as u16,
                }
            } else {
                WireType::VarBinaryMax
            }
        }
        _ => return None,
    })
}

fn text_of(value: &Value) -> Option<String> {
    Dialect::SqlServer.value_text(value)
}

/// How each result column goes to a TDS client: a TDS type that holds every one of its
/// values, or, when no TDS type of this version does (an unconstrained `numeric`, a date
/// outside year 1 to 9999, NaN, a type 23a sends as text), Unicode text.
pub(crate) fn wire_columns(columns: &[OutputColumn], rows: &[Vec<Value>]) -> Vec<WireColumn> {
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let values: Vec<&Value> = rows.iter().filter_map(|row| row.get(index)).collect();
            let sql_type = column.sql_type.or_else(|| {
                values
                    .iter()
                    .find(|value| !matches!(value, Value::Null))
                    .and_then(|value| value.sql_type())
            });
            let typed = match sql_type {
                None => Some(WireType::Int(4)),
                Some(sql_type) => candidate(sql_type, &values),
            }
            .filter(|kind| values.iter().all(|value| fits(*kind, value)));
            match typed {
                Some(kind) => WireColumn {
                    name: column.name.clone(),
                    kind,
                    as_text: false,
                },
                None => {
                    let longest = values
                        .iter()
                        .filter_map(|value| text_of(value))
                        .map(|text| utf16_units(&text))
                        .max()
                        .unwrap_or(0);
                    WireColumn {
                        name: column.name.clone(),
                        kind: nvarchar_for(longest),
                        as_text: true,
                    }
                }
            }
        })
        .collect()
}

fn push_utf16(out: &mut Vec<u8>, text: &str) {
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
}

fn push_plp(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u64).to_le_bytes());
    if !data.is_empty() {
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
    }
    out.extend_from_slice(&0u32.to_le_bytes());
}

fn push_text(out: &mut Vec<u8>, kind: WireType, text: Option<&str>) {
    match (kind, text) {
        (WireType::NVarCharMax, None) => out.extend_from_slice(&PLP_NULL.to_le_bytes()),
        (WireType::NVarCharMax, Some(text)) => {
            let mut data = Vec::with_capacity(text.len() * 2);
            push_utf16(&mut data, text);
            push_plp(out, &data);
        }
        (_, None) => out.extend_from_slice(&NULL_LENGTH.to_le_bytes()),
        (_, Some(text)) => {
            out.extend_from_slice(&((utf16_units(text) * 2) as u16).to_le_bytes());
            push_utf16(out, text);
        }
    }
}

/// Appends `value` as column `column` sends it in a ROW token. The column was made by
/// [`wire_columns`] from the same rows, so every value fits it.
pub(crate) fn push_value(out: &mut Vec<u8>, column: &WireColumn, value: &Value) {
    if column.as_text {
        push_text(out, column.kind, text_of(value).as_deref());
        return;
    }
    match column.kind {
        WireType::Int(width) => match value.as_integer() {
            Some(number) if !matches!(value, Value::Null) => {
                out.push(width);
                out.extend_from_slice(&number.to_le_bytes()[..usize::from(width)]);
            }
            _ => out.push(0),
        },
        WireType::Float(width) => match value {
            Value::Real(number) if width == 4 => {
                out.push(4);
                out.extend_from_slice(&number.to_le_bytes());
            }
            Value::Real(number) => {
                out.push(8);
                out.extend_from_slice(&f64::from(*number).to_le_bytes());
            }
            Value::Double(number) => {
                out.push(8);
                out.extend_from_slice(&number.to_le_bytes());
            }
            _ => out.push(0),
        },
        WireType::Bit => match value {
            Value::Boolean(flag) => out.extend_from_slice(&[1, u8::from(*flag)]),
            _ => out.push(0),
        },
        WireType::Decimal { precision, scale } => match scaled_decimal(value, precision, scale) {
            Some((negative, units)) => {
                let length = decimal_bytes(precision);
                out.push(length);
                out.push(u8::from(!negative));
                out.extend_from_slice(&units.to_le_bytes()[..usize::from(length - 1)]);
            }
            None => out.push(0),
        },
        WireType::DateTime2 { scale } => match datetime2_parts(value) {
            Some((days, micros)) => {
                let time_length = time_bytes(scale);
                let divisor = 10i64.pow(6 - u32::from(scale.min(6)));
                let units = micros / divisor;
                out.push(time_length + 3);
                out.extend_from_slice(&units.to_le_bytes()[..usize::from(time_length)]);
                out.extend_from_slice(&days.to_le_bytes()[..3]);
            }
            None => out.push(0),
        },
        WireType::Date => match date_from_year_one(value) {
            Some(days) => {
                out.push(3);
                out.extend_from_slice(&days.to_le_bytes()[..3]);
            }
            None => out.push(0),
        },
        WireType::Time { scale } => match time_of_day(value) {
            Some(micros) => {
                out.push(time_bytes(scale));
                push_time(out, micros, scale);
            }
            None => out.push(0),
        },
        WireType::DateTimeOffset { scale } => match utc_parts(value) {
            Some((days, micros)) => {
                out.push(time_bytes(scale) + 5);
                push_time(out, micros, scale);
                out.extend_from_slice(&days.to_le_bytes()[..3]);
                out.extend_from_slice(&0i16.to_le_bytes());
            }
            None => out.push(0),
        },
        WireType::Guid => match value {
            Value::Uuid(uuid) => {
                out.push(16);
                out.extend_from_slice(&guid_bytes(uuid));
            }
            _ => out.push(0),
        },
        WireType::NVarChar { .. } | WireType::NVarCharMax => {
            push_text(out, column.kind, character_text(value));
        }
        WireType::VarBinary { .. } => match value {
            Value::Bytea(data) => {
                out.extend_from_slice(&(data.len() as u16).to_le_bytes());
                out.extend_from_slice(data);
            }
            _ => out.extend_from_slice(&NULL_LENGTH.to_le_bytes()),
        },
        WireType::VarBinaryMax => match value {
            Value::Bytea(data) => push_plp(out, data),
            _ => out.extend_from_slice(&PLP_NULL.to_le_bytes()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{WireType, push_value, wire_columns};
    use eggdb_sql::{OutputColumn, SqlType, Value};

    fn column(sql_type: Option<SqlType>) -> Vec<OutputColumn> {
        vec![OutputColumn {
            name: "c".into(),
            sql_type,
        }]
    }

    fn encoded(sql_type: Option<SqlType>, value: Value) -> (WireType, bool, Vec<u8>) {
        let rows = vec![vec![value.clone()]];
        let columns = wire_columns(&column(sql_type), &rows);
        let mut out = Vec::new();
        push_value(&mut out, &columns[0], &value);
        (columns[0].kind, columns[0].as_text, out)
    }

    #[test]
    fn integers_floats_and_bits_take_their_n_types() {
        assert_eq!(
            encoded(Some(SqlType::Integer), Value::Integer(-2)),
            (WireType::Int(4), false, vec![4, 0xFE, 0xFF, 0xFF, 0xFF])
        );
        assert_eq!(
            encoded(Some(SqlType::BigInt), Value::Null),
            (WireType::Int(8), false, vec![0])
        );
        assert_eq!(
            encoded(None, Value::Null),
            (WireType::Int(4), false, vec![0])
        );
        assert_eq!(
            encoded(Some(SqlType::Double), Value::Double(1.5)).2,
            [&[8u8][..], &1.5f64.to_le_bytes()].concat()
        );
        assert_eq!(
            encoded(Some(SqlType::Boolean), Value::Boolean(true)),
            (WireType::Bit, false, vec![1, 1])
        );
    }

    #[test]
    fn text_is_sized_in_utf16_units_and_long_text_goes_as_plp() {
        let (kind, _, bytes) = encoded(Some(SqlType::Text), Value::Text("a\u{1F600}".into()));
        assert_eq!(kind, WireType::NVarChar { units: 3 });
        assert_eq!(bytes, vec![6, 0, b'a', 0, 0x3D, 0xD8, 0x00, 0xDE]);
        assert_eq!(
            encoded(Some(SqlType::Text), Value::Text(String::new())).0,
            WireType::NVarChar { units: 1 }
        );
        let just_fits = "\u{1F600}".repeat(2000);
        assert_eq!(
            encoded(Some(SqlType::Text), Value::Text(just_fits)).0,
            WireType::NVarChar { units: 4000 }
        );
        let too_long = format!("{}x", "\u{1F600}".repeat(2000));
        let (kind, _, bytes) = encoded(Some(SqlType::Varchar(Some(2001))), Value::Text(too_long));
        assert_eq!(kind, WireType::NVarCharMax);
        assert_eq!(&bytes[..8], &8002u64.to_le_bytes());
        assert_eq!(&bytes[bytes.len() - 4..], &[0, 0, 0, 0]);
    }

    #[test]
    fn values_outside_the_tds_type_go_as_text() {
        let modifier = eggdb_sql::NumericModifier {
            precision: 5,
            scale: 2,
        };
        let decimal = encoded(
            Some(SqlType::Numeric(Some(modifier))),
            Value::Numeric(eggdb_sql::Numeric::parse("-12.5").unwrap()),
        );
        assert_eq!(
            decimal,
            (
                WireType::Decimal {
                    precision: 5,
                    scale: 2
                },
                false,
                vec![5, 0, 0xE2, 0x04, 0, 0]
            )
        );
        let unconstrained = encoded(
            Some(SqlType::Numeric(None)),
            Value::Numeric(eggdb_sql::Numeric::parse("1.5").unwrap()),
        );
        assert!(unconstrained.1);
        let early = encoded(
            Some(SqlType::Timestamp(Some(3))),
            Value::Timestamp(i64::MIN / 2),
        );
        assert!(early.1);
        let rows = vec![vec![Value::Integer(1)], vec![Value::Text("x".into())]];
        let mixed = wire_columns(&column(None), &rows);
        assert!(mixed[0].as_text);
    }

    #[test]
    fn datetime2_counts_days_from_year_one_and_units_of_its_scale() {
        let (kind, text, bytes) =
            encoded(Some(SqlType::Timestamp(Some(3))), Value::Timestamp(1_500));
        assert_eq!(kind, WireType::DateTime2 { scale: 3 });
        assert!(!text);
        assert_eq!(bytes[0], 7);
        assert_eq!(&bytes[1..5], &1u32.to_le_bytes());
        assert_eq!(&bytes[5..8], &730_119u32.to_le_bytes()[..3]);
    }
}
