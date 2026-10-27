use crate::error::{SqlError, SqlResult, SqlState};
use std::cmp::Ordering;

/// A column type of the PostgreSQL dialect that this release stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlType {
    /// `smallint` (`int2`): a 16-bit signed integer.
    SmallInt,
    /// `integer` (`int`, `int4`): a 32-bit signed integer.
    Integer,
    /// `bigint` (`int8`): a 64-bit signed integer.
    BigInt,
    /// `real` (`float4`): an IEEE 754 single-precision number.
    Real,
    /// `double precision` (`float8`, `float`): an IEEE 754 double-precision number.
    Double,
    /// `boolean` (`bool`).
    Boolean,
    /// `text`: a string of any length.
    Text,
    /// `character varying(n)` (`varchar(n)`): a string of at most `n` characters, or of any
    /// length without `n`.
    Varchar(Option<u32>),
}

impl SqlType {
    /// The name PostgreSQL gives the type in messages, such as `double precision`.
    pub fn name(self) -> String {
        match self {
            Self::SmallInt => "smallint".into(),
            Self::Integer => "integer".into(),
            Self::BigInt => "bigint".into(),
            Self::Real => "real".into(),
            Self::Double => "double precision".into(),
            Self::Boolean => "boolean".into(),
            Self::Text => "text".into(),
            Self::Varchar(None) => "character varying".into(),
            Self::Varchar(Some(length)) => format!("character varying({length})"),
        }
    }

    pub(crate) fn internal_name(self) -> &'static str {
        match self {
            Self::SmallInt => "int2",
            Self::Integer => "int4",
            Self::BigInt => "int8",
            Self::Real => "float4",
            Self::Double => "float8",
            Self::Boolean => "bool",
            Self::Text => "text",
            Self::Varchar(_) => "varchar",
        }
    }

    pub(crate) fn is_numeric(self) -> bool {
        matches!(
            self,
            Self::SmallInt | Self::Integer | Self::BigInt | Self::Real | Self::Double
        )
    }

    pub(crate) fn is_float(self) -> bool {
        matches!(self, Self::Real | Self::Double)
    }

    pub(crate) fn is_string(self) -> bool {
        matches!(self, Self::Text | Self::Varchar(_))
    }

    fn integer_range(self) -> Option<(i64, i64)> {
        match self {
            Self::SmallInt => Some((i16::MIN.into(), i16::MAX.into())),
            Self::Integer => Some((i32::MIN.into(), i32::MAX.into())),
            Self::BigInt => Some((i64::MIN, i64::MAX)),
            _ => None,
        }
    }
}

/// A value of the PostgreSQL dialect: SQL's NULL or a value of one [`SqlType`].
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL's NULL.
    Null,
    /// A `boolean`.
    Boolean(bool),
    /// A `smallint`.
    SmallInt(i16),
    /// An `integer`.
    Integer(i32),
    /// A `bigint`.
    BigInt(i64),
    /// A `real`.
    Real(f32),
    /// A `double precision`.
    Double(f64),
    /// A `text` or `character varying`.
    Text(String),
}

impl Value {
    /// The value's type, or `None` for NULL.
    pub fn sql_type(&self) -> Option<SqlType> {
        Some(match self {
            Self::Null => return None,
            Self::Boolean(_) => SqlType::Boolean,
            Self::SmallInt(_) => SqlType::SmallInt,
            Self::Integer(_) => SqlType::Integer,
            Self::BigInt(_) => SqlType::BigInt,
            Self::Real(_) => SqlType::Real,
            Self::Double(_) => SqlType::Double,
            Self::Text(_) => SqlType::Text,
        })
    }

    /// Whether the value is NULL.
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub(crate) fn type_name(&self) -> String {
        self.sql_type()
            .map_or_else(|| "unknown".to_owned(), SqlType::name)
    }

    pub(crate) fn as_integer(&self) -> Option<i64> {
        match *self {
            Self::SmallInt(value) => Some(value.into()),
            Self::Integer(value) => Some(value.into()),
            Self::BigInt(value) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn as_float(&self) -> Option<f64> {
        match *self {
            Self::Real(value) => Some(value.into()),
            Self::Double(value) => Some(value),
            _ => self.as_integer().map(|value| value as f64),
        }
    }

    /// The value as PostgreSQL prints it in text form: `t` and `f` for booleans, floating
    /// point in the shortest form that reads back exactly, and `None` for NULL.
    pub fn to_text(&self) -> Option<String> {
        Some(match self {
            Self::Null => return None,
            Self::Boolean(true) => "t".into(),
            Self::Boolean(false) => "f".into(),
            Self::SmallInt(value) => value.to_string(),
            Self::Integer(value) => value.to_string(),
            Self::BigInt(value) => value.to_string(),
            Self::Real(value) => format_float(f64::from(*value), format!("{value:e}"), 6),
            Self::Double(value) => format_float(*value, format!("{value:e}"), 15),
            Self::Text(text) => text.clone(),
        })
    }
}

fn format_float(value: f64, scientific: String, fixed_below_exponent: i32) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let Some((mantissa, exponent)) = scientific.split_once('e') else {
        return scientific;
    };
    let exponent: i32 = exponent.parse().unwrap_or(0);
    if exponent < -4 || exponent >= fixed_below_exponent {
        let sign = if exponent < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exponent.unsigned_abs());
    }
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if exponent < 0 {
        out.push_str("0.");
        for _ in 0..(-exponent - 1) {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        let whole = exponent as usize + 1;
        if digits.len() <= whole {
            out.push_str(&digits);
            for _ in digits.len()..whole {
                out.push('0');
            }
        } else {
            out.push_str(&digits[..whole]);
            out.push('.');
            out.push_str(&digits[whole..]);
        }
    }
    out
}

pub(crate) fn out_of_range(sql_type: SqlType) -> SqlError {
    SqlError::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        format!("{} out of range", sql_type.name()),
    )
}

pub(crate) fn float_overflow() -> SqlError {
    SqlError::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        "value out of range: overflow",
    )
}

pub(crate) fn integer_value(value: i64, sql_type: SqlType) -> SqlResult<Value> {
    Ok(match sql_type {
        SqlType::SmallInt => {
            Value::SmallInt(i16::try_from(value).map_err(|_| out_of_range(sql_type))?)
        }
        SqlType::Integer => {
            Value::Integer(i32::try_from(value).map_err(|_| out_of_range(sql_type))?)
        }
        _ => Value::BigInt(value),
    })
}

fn rounded_to_integer(value: f64, sql_type: SqlType) -> SqlResult<Value> {
    let rounded = value.round_ties_even();
    let low = sql_type.integer_range().map_or(i64::MIN, |(low, _)| low) as f64;
    if !rounded.is_finite() || rounded < low || rounded >= -low {
        return Err(out_of_range(sql_type));
    }
    integer_value(rounded as i64, sql_type)
}

fn real_value(value: f64) -> SqlResult<Value> {
    let narrowed = value as f32;
    if narrowed.is_infinite() && value.is_finite() {
        return Err(float_overflow());
    }
    if narrowed == 0.0 && value != 0.0 {
        return Err(SqlError::new(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            "value out of range: underflow",
        ));
    }
    Ok(Value::Real(narrowed))
}

pub(crate) fn parse_input(text: &str, sql_type: SqlType) -> SqlResult<Value> {
    let invalid = || {
        SqlError::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!(
                "invalid input syntax for type {}: \"{text}\"",
                sql_type.name()
            ),
        )
    };
    let trimmed = text.trim_matches(|c: char| c.is_ascii_whitespace());
    match sql_type {
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt => {
            let digits = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid());
            }
            let parsed: i64 = trimmed
                .parse()
                .map_err(|_| input_out_of_range(text, sql_type))?;
            integer_value(parsed, sql_type).map_err(|_| input_out_of_range(text, sql_type))
        }
        SqlType::Real | SqlType::Double => {
            let unsigned = trimmed.trim_start_matches(['+', '-']);
            let special = ["nan", "inf", "infinity"]
                .iter()
                .any(|word| unsigned.eq_ignore_ascii_case(word));
            let looks_numeric = !unsigned.is_empty()
                && unsigned
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || b".eE+-".contains(&byte));
            if !special && !looks_numeric {
                return Err(invalid());
            }
            let parsed: f64 = trimmed.parse().map_err(|_| invalid())?;
            if parsed.is_infinite() && !special {
                return Err(input_out_of_range(text, sql_type));
            }
            if sql_type == SqlType::Real {
                real_value(parsed).map_err(|_| input_out_of_range(text, sql_type))
            } else {
                Ok(Value::Double(parsed))
            }
        }
        SqlType::Boolean => {
            let lower = trimmed.to_ascii_lowercase();
            let is_prefix_of = |word: &str| !lower.is_empty() && word.starts_with(lower.as_str());
            if lower == "1" || lower == "on" || is_prefix_of("true") || is_prefix_of("yes") {
                Ok(Value::Boolean(true))
            } else if lower == "0" || lower == "off" || is_prefix_of("false") || is_prefix_of("no")
            {
                Ok(Value::Boolean(false))
            } else {
                Err(invalid())
            }
        }
        SqlType::Text | SqlType::Varchar(_) => {
            assign_string(text.to_owned(), sql_type, Conversion::Assignment)
        }
    }
}

fn input_out_of_range(text: &str, sql_type: SqlType) -> SqlError {
    SqlError::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        format!(
            "value \"{text}\" is out of range for type {}",
            sql_type.name()
        ),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Conversion {
    Explicit,
    Assignment,
}

fn assign_string(text: String, sql_type: SqlType, conversion: Conversion) -> SqlResult<Value> {
    let SqlType::Varchar(Some(limit)) = sql_type else {
        return Ok(Value::Text(text));
    };
    let limit = limit as usize;
    let Some((cut, _)) = text.char_indices().nth(limit) else {
        return Ok(Value::Text(text));
    };
    if conversion == Conversion::Explicit || text[cut..].chars().all(|c| c == ' ') {
        let mut text = text;
        text.truncate(cut);
        return Ok(Value::Text(text));
    }
    Err(SqlError::new(
        SqlState::STRING_DATA_RIGHT_TRUNCATION,
        format!("value too long for type {}", sql_type.name()),
    ))
}

pub(crate) fn convert(value: Value, target: SqlType, conversion: Conversion) -> SqlResult<Value> {
    let Some(source) = value.sql_type() else {
        return Ok(Value::Null);
    };
    if target.is_string() {
        let text = match value {
            Value::Text(text) => text,
            Value::Boolean(flag) => flag.to_string(),
            other => other.to_text().unwrap_or_default(),
        };
        return assign_string(text, target, conversion);
    }
    if source.is_string() {
        return match (conversion, value) {
            (Conversion::Explicit, Value::Text(text)) => parse_input(&text, target),
            _ => Err(mismatch(source, target)),
        };
    }
    match (target, &value) {
        (SqlType::Boolean, Value::Boolean(_)) => Ok(value),
        (SqlType::Boolean, Value::Integer(number)) if conversion == Conversion::Explicit => {
            Ok(Value::Boolean(*number != 0))
        }
        (SqlType::Integer, Value::Boolean(flag)) if conversion == Conversion::Explicit => {
            Ok(Value::Integer(i32::from(*flag)))
        }
        (SqlType::SmallInt | SqlType::Integer | SqlType::BigInt, _) if source.is_numeric() => {
            match value.as_integer() {
                Some(number) => integer_value(number, target),
                None => rounded_to_integer(value.as_float().unwrap_or(f64::NAN), target),
            }
        }
        (SqlType::Real, _) if source.is_numeric() => match value {
            Value::Real(_) => Ok(value),
            _ => real_value(value.as_float().unwrap_or(f64::NAN)),
        },
        (SqlType::Double, _) if source.is_numeric() => {
            Ok(Value::Double(value.as_float().unwrap_or(f64::NAN)))
        }
        _ => Err(mismatch(source, target)),
    }
}

fn mismatch(source: SqlType, target: SqlType) -> SqlError {
    SqlError::new(
        SqlState::CANNOT_COERCE,
        format!("cannot cast type {} to {}", source.name(), target.name()),
    )
}

pub(crate) fn compare_floats(left: f64, right: f64) -> Ordering {
    match (left.is_nan(), right.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => left.partial_cmp(&right).unwrap_or(Ordering::Equal),
    }
}

pub(crate) fn comparable(left: SqlType, right: SqlType) -> bool {
    (left.is_numeric() && right.is_numeric())
        || (left.is_string() && right.is_string())
        || left == right
}

pub(crate) fn compare_values(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Boolean(a), Value::Boolean(b)) => a.cmp(b),
        (Value::Text(a), Value::Text(b)) => a.as_bytes().cmp(b.as_bytes()),
        _ => match (left.as_integer(), right.as_integer()) {
            (Some(a), Some(b)) => a.cmp(&b),
            _ => compare_floats(
                left.as_float().unwrap_or(f64::NAN),
                right.as_float().unwrap_or(f64::NAN),
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{Conversion, SqlType, Value, convert, parse_input};

    #[test]
    fn floats_print_like_postgresql() {
        let cases = [
            (Value::Double(1.5), "1.5"),
            (Value::Double(1e15), "1e+15"),
            (Value::Double(123456789012345.0), "123456789012345"),
            (Value::Double(0.0001), "0.0001"),
            (Value::Double(0.00001), "1e-05"),
            (Value::Double(-2.5e-10), "-2.5e-10"),
            (Value::Double(100.0), "100"),
            (Value::Double(f64::NAN), "NaN"),
            (Value::Double(f64::NEG_INFINITY), "-Infinity"),
            (Value::Double(-0.0), "-0"),
            (Value::Double(0.1 + 0.2), "0.30000000000000004"),
            (Value::Real(1e6), "1e+06"),
            (Value::Real(123456.0), "123456"),
            (Value::Real(0.1), "0.1"),
        ];
        for (value, text) in cases {
            assert_eq!(value.to_text().as_deref(), Some(text), "{value:?}");
        }
    }

    #[test]
    fn input_follows_postgresql_rules() {
        assert_eq!(
            parse_input(" 42 ", SqlType::Integer),
            Ok(Value::Integer(42))
        );
        assert_eq!(
            parse_input("40000", SqlType::SmallInt)
                .expect_err("range")
                .message(),
            "value \"40000\" is out of range for type smallint"
        );
        assert_eq!(
            parse_input("4x", SqlType::Integer)
                .expect_err("syntax")
                .message(),
            "invalid input syntax for type integer: \"4x\""
        );
        assert_eq!(
            parse_input("YES", SqlType::Boolean),
            Ok(Value::Boolean(true))
        );
        assert_eq!(
            parse_input("f", SqlType::Boolean),
            Ok(Value::Boolean(false))
        );
        assert_eq!(
            parse_input("-Infinity", SqlType::Double),
            Ok(Value::Double(f64::NEG_INFINITY))
        );
        assert!(parse_input("1e400", SqlType::Double).is_err());
    }

    #[test]
    fn conversions_round_and_check_ranges() {
        assert_eq!(
            convert(Value::Double(2.5), SqlType::Integer, Conversion::Assignment),
            Ok(Value::Integer(2))
        );
        assert_eq!(
            convert(Value::Double(3.5), SqlType::Integer, Conversion::Assignment),
            Ok(Value::Integer(4))
        );
        assert_eq!(
            convert(
                Value::BigInt(1 << 40),
                SqlType::Integer,
                Conversion::Assignment
            )
            .expect_err("range")
            .message(),
            "integer out of range"
        );
        assert_eq!(
            convert(
                Value::Text("abcdef".into()),
                SqlType::Varchar(Some(3)),
                Conversion::Explicit
            ),
            Ok(Value::Text("abc".into()))
        );
        assert_eq!(
            convert(
                Value::Text("ab   ".into()),
                SqlType::Varchar(Some(3)),
                Conversion::Assignment
            ),
            Ok(Value::Text("ab ".into()))
        );
        assert!(
            convert(
                Value::Text("abcd".into()),
                SqlType::Varchar(Some(3)),
                Conversion::Assignment
            )
            .is_err()
        );
        assert!(
            convert(
                Value::Double(f64::NAN),
                SqlType::BigInt,
                Conversion::Explicit
            )
            .is_err()
        );
    }
}
