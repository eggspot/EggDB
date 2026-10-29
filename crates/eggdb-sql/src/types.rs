use crate::arrays::{ArrayElement, ArrayValue};
use crate::datetime;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::interval::{Interval, IntervalFields};
use crate::numeric::Numeric;
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
    /// `numeric(p, s)` (`decimal`): an exact decimal number, rounded to scale `s` and at most
    /// `p` digits when they are given.
    Numeric(Option<NumericModifier>),
    /// `date`: a day of the proleptic Gregorian calendar, 4713 BC to 5874897 AD.
    Date,
    /// `timestamp(p) without time zone`: a date and time of day to the microsecond, rounded to
    /// `p` fractional digits when `p` is given.
    Timestamp(Option<u8>),
    /// `timestamp(p) with time zone` (`timestamptz`): an instant, stored in UTC and shown in
    /// the session's time zone.
    TimestampTz(Option<u8>),
    /// `json`: JSON text, checked and kept as written.
    Json,
    /// `jsonb`: JSON in its normalized form (keys sorted, the last duplicate kept, whitespace
    /// and number formats canonical).
    Jsonb,
    /// `time(p) without time zone`: a time of day from 00:00:00 to 24:00:00 to the microsecond,
    /// rounded to `p` fractional digits when `p` is given.
    Time(Option<u8>),
    /// `time(p) with time zone` (`timetz`): a time of day and its offset from UTC.
    TimeTz(Option<u8>),
    /// `interval fields(p)`: a span of months, days and microseconds, truncated to `fields`
    /// and rounded to `p` fractional digits of a second when they are given.
    Interval {
        /// The fields kept, as in `interval year to month`.
        fields: Option<IntervalFields>,
        /// The fractional digits of a second kept, 0 to 6.
        precision: Option<u8>,
    },
    /// `bytea`: a string of bytes of any length.
    Bytea,
    /// `uuid`: a 128-bit universally unique identifier (RFC 9562).
    Uuid,
    /// `character(n)` (`char(n)`, `bpchar`): a string padded with spaces to `n` characters,
    /// whose trailing spaces do not count when it is compared; no limit without `n`.
    Bpchar(Option<u32>),
    /// A one-dimensional array of the element type, such as `integer[]`: a value of
    /// expressions and intermediate results, not stored in table columns.
    Array(ArrayElement),
}

/// The precision and scale of a `numeric(p, s)` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumericModifier {
    /// The total number of significant digits, 1 to 1000.
    pub precision: u16,
    /// The number of digits after the decimal point, -1000 to 1000.
    pub scale: i16,
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
            Self::Numeric(None) => "numeric".into(),
            Self::Numeric(Some(modifier)) => {
                format!("numeric({},{})", modifier.precision, modifier.scale)
            }
            Self::Date => "date".into(),
            Self::Timestamp(None) => "timestamp without time zone".into(),
            Self::Timestamp(Some(precision)) => {
                format!("timestamp({precision}) without time zone")
            }
            Self::TimestampTz(None) => "timestamp with time zone".into(),
            Self::TimestampTz(Some(precision)) => {
                format!("timestamp({precision}) with time zone")
            }
            Self::Json => "json".into(),
            Self::Jsonb => "jsonb".into(),
            Self::Time(precision) => {
                format!("time{} without time zone", precision_suffix(precision))
            }
            Self::TimeTz(precision) => {
                format!("time{} with time zone", precision_suffix(precision))
            }
            Self::Interval { fields, precision } => {
                let fields = fields.map_or(String::new(), |fields| format!(" {}", fields.name()));
                format!("interval{fields}{}", precision_suffix(precision))
            }
            Self::Bytea => "bytea".into(),
            Self::Uuid => "uuid".into(),
            Self::Bpchar(None) => "bpchar".into(),
            Self::Bpchar(Some(length)) => format!("character({length})"),
            Self::Array(element) => element.type_name(),
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
            Self::Numeric(_) => "numeric",
            Self::Date => "date",
            Self::Timestamp(_) => "timestamp",
            Self::TimestampTz(_) => "timestamptz",
            Self::Json => "json",
            Self::Jsonb => "jsonb",
            Self::Time(_) => "time",
            Self::TimeTz(_) => "timetz",
            Self::Interval { .. } => "interval",
            Self::Bytea => "bytea",
            Self::Uuid => "uuid",
            Self::Bpchar(_) => "bpchar",
            Self::Array(element) => element.internal_name(),
        }
    }

    pub(crate) fn is_numeric(self) -> bool {
        matches!(
            self,
            Self::SmallInt
                | Self::Integer
                | Self::BigInt
                | Self::Real
                | Self::Double
                | Self::Numeric(_)
        )
    }

    pub(crate) fn without_modifier(self) -> Self {
        match self {
            Self::Numeric(_) => Self::Numeric(None),
            Self::Timestamp(_) => Self::Timestamp(None),
            Self::TimestampTz(_) => Self::TimestampTz(None),
            Self::Time(_) => Self::Time(None),
            Self::TimeTz(_) => Self::TimeTz(None),
            Self::Interval { .. } => Self::Interval {
                fields: None,
                precision: None,
            },
            Self::Bpchar(_) => Self::Bpchar(None),
            other => other,
        }
    }

    pub(crate) fn is_datetime(self) -> bool {
        matches!(self, Self::Date | Self::Timestamp(_) | Self::TimestampTz(_))
    }

    pub(crate) fn is_json(self) -> bool {
        matches!(self, Self::Json | Self::Jsonb)
    }

    pub(crate) fn is_float(self) -> bool {
        matches!(self, Self::Real | Self::Double)
    }

    pub(crate) fn is_string(self) -> bool {
        matches!(self, Self::Text | Self::Varchar(_))
    }

    pub(crate) fn is_character(self) -> bool {
        self.is_string() || matches!(self, Self::Bpchar(_))
    }

    pub(crate) const INTERVAL: Self = Self::Interval {
        fields: None,
        precision: None,
    };

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
    /// A `numeric`.
    Numeric(Numeric),
    /// A `date`: days since 2000-01-01.
    Date(i32),
    /// A `timestamp without time zone`: microseconds since 2000-01-01 00:00:00.
    Timestamp(i64),
    /// A `timestamp with time zone`: microseconds since 2000-01-01 00:00:00 UTC.
    TimestampTz(i64),
    /// A `json` value: its text as written.
    Json(String),
    /// A `jsonb` value: its normalized text.
    Jsonb(String),
    /// A `time without time zone`: microseconds since midnight, 0 to 86,400,000,000
    /// (24:00:00).
    Time(i64),
    /// A `time with time zone`.
    TimeTz {
        /// Microseconds since midnight in the value's own offset, 0 to 86,400,000,000.
        micros: i64,
        /// The offset east of UTC, in seconds.
        offset_seconds: i32,
    },
    /// An `interval`.
    Interval(Interval),
    /// A `bytea`.
    Bytea(Vec<u8>),
    /// A `uuid`, as its 16 bytes.
    Uuid([u8; 16]),
    /// A `character(n)`: its text, padded with spaces to the column's length.
    Bpchar(String),
    /// A one-dimensional array.
    Array(Box<ArrayValue>),
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
            Self::Numeric(_) => SqlType::Numeric(None),
            Self::Date(_) => SqlType::Date,
            Self::Timestamp(_) => SqlType::Timestamp(None),
            Self::TimestampTz(_) => SqlType::TimestampTz(None),
            Self::Json(_) => SqlType::Json,
            Self::Jsonb(_) => SqlType::Jsonb,
            Self::Time(_) => SqlType::Time(None),
            Self::TimeTz { .. } => SqlType::TimeTz(None),
            Self::Interval(_) => SqlType::INTERVAL,
            Self::Bytea(_) => SqlType::Bytea,
            Self::Uuid(_) => SqlType::Uuid,
            Self::Bpchar(_) => SqlType::Bpchar(None),
            Self::Array(array) => SqlType::Array(array.element),
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

    /// The value as a 64-bit integer, when it is a `smallint`, `integer` or `bigint`.
    pub fn as_integer(&self) -> Option<i64> {
        match *self {
            Self::SmallInt(value) => Some(value.into()),
            Self::Integer(value) => Some(value.into()),
            Self::BigInt(value) => Some(value),
            _ => None,
        }
    }

    /// The value as a double-precision number, when it is a number of any type.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Self::Real(value) => Some(f64::from(*value)),
            Self::Double(value) => Some(*value),
            Self::Numeric(value) => Some(value.to_f64()),
            _ => self.as_integer().map(|value| value as f64),
        }
    }

    pub(crate) fn as_numeric(&self) -> Option<Numeric> {
        match self {
            Self::Numeric(value) => Some(value.clone()),
            _ => self.as_integer().map(Numeric::from_i64),
        }
    }

    /// The value as [`Value::to_text`] prints it, with a `timestamp with time zone` shown at
    /// `offset_seconds` east of UTC instead of the running statement's time zone.
    pub fn to_text_at(&self, offset_seconds: i32) -> Option<String> {
        match self {
            Self::TimestampTz(micros) => {
                Some(datetime::format_timestamptz_at(*micros, offset_seconds))
            }
            other => other.to_text(),
        }
    }

    /// The value as PostgreSQL prints it in text form: `t` and `f` for booleans, floating
    /// point in the shortest form that reads back exactly, dates and times in the ISO style
    /// (a `timestamp with time zone` in the time zone of the statement running on this thread,
    /// else UTC), and `None` for NULL.
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
            Self::Text(text) | Self::Json(text) | Self::Jsonb(text) | Self::Bpchar(text) => {
                text.clone()
            }
            Self::Numeric(value) => value.to_string(),
            Self::Date(days) => datetime::format_date(*days),
            Self::Timestamp(micros) => datetime::format_timestamp(*micros),
            Self::TimestampTz(micros) => datetime::format_timestamptz(*micros),
            Self::Time(micros) => datetime::format_time(*micros),
            Self::TimeTz {
                micros,
                offset_seconds,
            } => datetime::format_time_tz(*micros, *offset_seconds),
            Self::Interval(interval) => interval.to_string(),
            Self::Bytea(bytes) => crate::bytea::format_hex(bytes),
            Self::Uuid(bytes) => crate::bytea::format_uuid(bytes),
            Self::Array(array) => crate::arrays::format_text(array),
        })
    }

    pub(crate) fn character_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Bpchar(text) => Some(without_trailing_spaces(text)),
            _ => None,
        }
    }
}

fn precision_suffix(precision: Option<u8>) -> String {
    precision.map_or(String::new(), |precision| format!("({precision})"))
}

pub(crate) fn without_trailing_spaces(text: &str) -> &str {
    text.trim_end_matches(' ')
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
        SqlType::Text | SqlType::Varchar(_) | SqlType::Bpchar(_) => {
            assign_string(text.to_owned(), sql_type, Conversion::Assignment)
        }
        SqlType::Time(precision) => {
            let (micros, _) = datetime::parse_time_of_day(text, false)?;
            Ok(Value::Time(time_value(micros, precision)))
        }
        SqlType::TimeTz(precision) => {
            let (micros, offset_seconds) = datetime::parse_time_of_day(text, true)?;
            Ok(Value::TimeTz {
                micros: time_value(micros, precision),
                offset_seconds,
            })
        }
        SqlType::Interval { fields, precision } => Interval::parse(text, fields)
            .and_then(|interval| interval.with_modifier(fields, precision))
            .map(Value::Interval),
        SqlType::Bytea => crate::bytea::from_text(text).map(Value::Bytea),
        SqlType::Uuid => crate::bytea::parse_uuid(text).map(Value::Uuid),
        SqlType::Numeric(modifier) => numeric_value(Numeric::parse(text)?, modifier),
        SqlType::Date => datetime::parse_date(text).map(Value::Date),
        SqlType::Timestamp(precision) => {
            timestamp_value(datetime::parse_timestamp(text, false)?, precision, false)
        }
        SqlType::TimestampTz(precision) => {
            timestamp_value(datetime::parse_timestamp(text, true)?, precision, true)
        }
        SqlType::Json => {
            crate::json::validate(text)?;
            Ok(Value::Json(text.to_owned()))
        }
        SqlType::Jsonb => crate::json::normalize(text).map(Value::Jsonb),
        SqlType::Array(element) => {
            crate::arrays::parse_text(text, element).map(|array| Value::Array(Box::new(array)))
        }
    }
}

pub(crate) fn numeric_value(value: Numeric, modifier: Option<NumericModifier>) -> SqlResult<Value> {
    match modifier {
        None => Ok(Value::Numeric(value)),
        Some(modifier) => value
            .with_modifier(i64::from(modifier.precision), i64::from(modifier.scale))
            .map(Value::Numeric),
    }
}

fn time_value(micros: i64, precision: Option<u8>) -> i64 {
    precision.map_or(micros, |precision| datetime::round_time(micros, precision))
}

fn convert_time(value: &Value, target: SqlType) -> Option<SqlResult<Value>> {
    let local_time = |micros: i64| datetime::utc_to_local(micros).map(datetime::time_of_timestamp);
    Some(match (target, value) {
        (SqlType::Time(precision), Value::Time(micros)) => {
            Ok(Value::Time(time_value(*micros, precision)))
        }
        (SqlType::Time(precision), Value::TimeTz { micros, .. }) => {
            Ok(Value::Time(time_value(*micros, precision)))
        }
        (SqlType::Time(precision), Value::Timestamp(micros)) => Ok(Value::Time(time_value(
            datetime::time_of_timestamp(*micros),
            precision,
        ))),
        (SqlType::Time(precision), Value::TimestampTz(micros)) => {
            local_time(*micros).map(|micros| Value::Time(time_value(micros, precision)))
        }
        (SqlType::Time(precision), Value::Interval(interval)) => Ok(Value::Time(time_value(
            datetime::wrap_time(i128::from(interval.micros)),
            precision,
        ))),
        (
            SqlType::TimeTz(precision),
            Value::TimeTz {
                micros,
                offset_seconds,
            },
        ) => Ok(Value::TimeTz {
            micros: time_value(*micros, precision),
            offset_seconds: *offset_seconds,
        }),
        (SqlType::TimeTz(precision), Value::Time(micros)) => Ok(Value::TimeTz {
            micros: time_value(*micros, precision),
            offset_seconds: datetime::session_offset(),
        }),
        (SqlType::TimeTz(precision), Value::TimestampTz(micros)) => {
            local_time(*micros).map(|local| Value::TimeTz {
                micros: time_value(local, precision),
                offset_seconds: datetime::session_offset(),
            })
        }
        (SqlType::Interval { fields, precision }, Value::Interval(interval)) => interval
            .with_modifier(fields, precision)
            .map(Value::Interval),
        (SqlType::Interval { fields, precision }, Value::Time(micros)) => Interval {
            months: 0,
            days: 0,
            micros: *micros,
        }
        .with_modifier(fields, precision)
        .map(Value::Interval),
        _ => return None,
    })
}

fn timestamp_value(micros: i64, precision: Option<u8>, zoned: bool) -> SqlResult<Value> {
    let micros = match precision {
        Some(precision) => datetime::round_to_precision(micros, precision)?,
        None => micros,
    };
    Ok(if zoned {
        Value::TimestampTz(micros)
    } else {
        Value::Timestamp(micros)
    })
}

fn convert_datetime(value: &Value, target: SqlType) -> Option<SqlResult<Value>> {
    let local = |micros: i64| datetime::utc_to_local(micros);
    Some(match (target, value) {
        (SqlType::Date, Value::Date(_)) => Ok(value.clone()),
        (SqlType::Date, Value::Timestamp(micros)) => {
            Ok(Value::Date(datetime::timestamp_to_date(*micros)))
        }
        (SqlType::Date, Value::TimestampTz(micros)) => {
            local(*micros).map(|local| Value::Date(datetime::timestamp_to_date(local)))
        }
        (SqlType::Timestamp(precision), Value::Date(days)) => datetime::date_to_timestamp(*days)
            .and_then(|micros| timestamp_value(micros, precision, false)),
        (SqlType::Timestamp(precision), Value::Timestamp(micros)) => {
            timestamp_value(*micros, precision, false)
        }
        (SqlType::Timestamp(precision), Value::TimestampTz(micros)) => {
            local(*micros).and_then(|local| timestamp_value(local, precision, false))
        }
        (SqlType::TimestampTz(precision), Value::Date(days)) => datetime::date_to_timestamp(*days)
            .and_then(datetime::local_to_utc)
            .and_then(|micros| timestamp_value(micros, precision, true)),
        (SqlType::TimestampTz(precision), Value::Timestamp(micros)) => {
            datetime::local_to_utc(*micros)
                .and_then(|micros| timestamp_value(micros, precision, true))
        }
        (SqlType::TimestampTz(precision), Value::TimestampTz(micros)) => {
            timestamp_value(*micros, precision, true)
        }
        _ => return None,
    })
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
    if let SqlType::Bpchar(length) = sql_type {
        return padded(text, length, conversion);
    }
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

fn padded(text: String, length: Option<u32>, conversion: Conversion) -> SqlResult<Value> {
    let Some(length) = length else {
        return Ok(Value::Bpchar(text));
    };
    let length = length as usize;
    match text.char_indices().nth(length) {
        Some((cut, _)) => {
            if conversion == Conversion::Explicit || text[cut..].chars().all(|c| c == ' ') {
                let mut text = text;
                text.truncate(cut);
                Ok(Value::Bpchar(text))
            } else {
                Err(SqlError::new(
                    SqlState::STRING_DATA_RIGHT_TRUNCATION,
                    format!("value too long for type character({length})"),
                ))
            }
        }
        None => {
            let count = text.chars().count();
            let mut text = text;
            text.extend(std::iter::repeat_n(' ', length - count));
            Ok(Value::Bpchar(text))
        }
    }
}

pub(crate) fn convert(value: Value, target: SqlType, conversion: Conversion) -> SqlResult<Value> {
    let Some(source) = value.sql_type() else {
        return Ok(Value::Null);
    };
    if let SqlType::Array(element) = target {
        return crate::arrays::convert_to(value, element, conversion);
    }
    if target.is_character() {
        let text = match value {
            Value::Text(text) => text,
            Value::Bpchar(text) if matches!(target, SqlType::Bpchar(_)) => text,
            Value::Bpchar(mut text) => {
                text.truncate(without_trailing_spaces(&text).len());
                text
            }
            Value::Boolean(flag) => flag.to_string(),
            Value::Bytea(bytes) => crate::bytea::to_text(&bytes)?,
            other => other.to_text().unwrap_or_default(),
        };
        return assign_string(text, target, conversion);
    }
    if source.is_character() {
        return match (conversion, value) {
            (Conversion::Explicit, Value::Text(text)) => parse_input(&text, target),
            (Conversion::Explicit, Value::Bpchar(text)) => {
                parse_input(without_trailing_spaces(&text), target)
            }
            _ => Err(mismatch(source, target)),
        };
    }
    if target.is_datetime()
        && let Some(converted) = convert_datetime(&value, target)
    {
        return converted;
    }
    if let Some(converted) = convert_time(&value, target) {
        return converted;
    }
    match (target, &value) {
        (SqlType::Json, Value::Json(_))
        | (SqlType::Jsonb, Value::Jsonb(_))
        | (SqlType::Bytea, Value::Bytea(_))
        | (SqlType::Uuid, Value::Uuid(_)) => Ok(value),
        (SqlType::Json, Value::Jsonb(text)) => Ok(Value::Json(text.clone())),
        (SqlType::Jsonb, Value::Json(text)) => crate::json::normalize(text).map(Value::Jsonb),
        (SqlType::Numeric(modifier), _) if source.is_numeric() => {
            let exact = match &value {
                Value::Numeric(number) => number.clone(),
                Value::Real(number) => Numeric::from_f64(f64::from(*number), 6)?,
                Value::Double(number) => Numeric::from_f64(*number, 15)?,
                other => Numeric::from_i64(other.as_integer().unwrap_or(0)),
            };
            numeric_value(exact, modifier)
        }
        (SqlType::SmallInt | SqlType::Integer | SqlType::BigInt, Value::Numeric(number)) => {
            if number.is_nan() {
                return Err(SqlError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("cannot convert NaN to {}", target.name()),
                ));
            }
            match number
                .to_i128_rounded()
                .and_then(|value| i64::try_from(value).ok())
            {
                Some(value) => integer_value(value, target),
                None => Err(out_of_range(target)),
            }
        }
        (SqlType::Real | SqlType::Double, Value::Numeric(number)) => {
            let float = number.to_f64();
            if float.is_infinite() {
                return Err(float_overflow());
            }
            if target == SqlType::Real {
                real_value(float)
            } else {
                Ok(Value::Double(float))
            }
        }
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
    if left == SqlType::Json || right == SqlType::Json {
        return false;
    }
    if let (SqlType::Array(left), SqlType::Array(right)) = (left, right) {
        return left.comparable(right);
    }
    (left.is_numeric() && right.is_numeric())
        || (left.is_character() && right.is_character())
        || (left.is_datetime() && right.is_datetime())
        || left.without_modifier() == right.without_modifier()
}

fn time_tz_order(micros: i64, offset: i32) -> (i64, std::cmp::Reverse<i32>) {
    (
        datetime::time_tz_utc(micros, offset),
        std::cmp::Reverse(offset),
    )
}

fn instant(value: &Value, zoned: bool) -> i128 {
    let local = match value {
        Value::Date(days) => i128::from(*days) * i128::from(datetime::MICROS_PER_DAY),
        Value::Timestamp(micros) => i128::from(*micros),
        Value::TimestampTz(micros) => return i128::from(*micros),
        _ => 0,
    };
    if zoned {
        local - i128::from(datetime::session_offset()) * i128::from(datetime::MICROS_PER_SECOND)
    } else {
        local
    }
}

fn is_exact_number(value: &Value) -> bool {
    matches!(
        value,
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) | Value::Numeric(_)
    )
}

pub(crate) fn compare_values(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Boolean(a), Value::Boolean(b)) => a.cmp(b),
        (Value::Text(a), Value::Text(b)) | (Value::Json(a), Value::Json(b)) => {
            a.as_bytes().cmp(b.as_bytes())
        }
        (Value::Bpchar(_), _) | (_, Value::Bpchar(_)) => {
            match (left.character_text(), right.character_text()) {
                (Some(a), Some(b)) => a.as_bytes().cmp(b.as_bytes()),
                _ => Ordering::Equal,
            }
        }
        (Value::Bytea(a), Value::Bytea(b)) => a.cmp(b),
        (Value::Uuid(a), Value::Uuid(b)) => a.cmp(b),
        (Value::Time(a), Value::Time(b)) => a.cmp(b),
        (
            Value::TimeTz {
                micros: a,
                offset_seconds: a_offset,
            },
            Value::TimeTz {
                micros: b,
                offset_seconds: b_offset,
            },
        ) => time_tz_order(*a, *a_offset).cmp(&time_tz_order(*b, *b_offset)),
        (Value::Interval(a), Value::Interval(b)) => a.compare(*b),
        (Value::Array(a), Value::Array(b)) => crate::arrays::compare(a, b),
        (Value::Jsonb(a), Value::Jsonb(b)) => crate::json::comparable(a)
            .as_bytes()
            .cmp(crate::json::comparable(b).as_bytes()),
        (Value::Date(_) | Value::Timestamp(_) | Value::TimestampTz(_), _)
            if right.sql_type().is_some_and(SqlType::is_datetime) =>
        {
            let zoned =
                matches!(left, Value::TimestampTz(_)) || matches!(right, Value::TimestampTz(_));
            instant(left, zoned).cmp(&instant(right, zoned))
        }
        (Value::Numeric(_), _) | (_, Value::Numeric(_))
            if is_exact_number(left) && is_exact_number(right) =>
        {
            match (left.as_numeric(), right.as_numeric()) {
                (Some(a), Some(b)) => a.compare(&b),
                _ => Ordering::Equal,
            }
        }
        _ => match (left.as_integer(), right.as_integer()) {
            (Some(a), Some(b)) => a.cmp(&b),
            _ => compare_floats(
                left.as_float().unwrap_or(f64::NAN),
                right.as_float().unwrap_or(f64::NAN),
            ),
        },
    }
}

pub(crate) fn equality_key(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => out.push(0),
        Value::Boolean(flag) => {
            out.push(1);
            out.push(u8::from(*flag));
        }
        Value::Text(_) | Value::Bpchar(_) => {
            let text = value.character_text().unwrap_or_default();
            out.push(2);
            out.extend_from_slice(&(text.len() as u64).to_le_bytes());
            out.extend_from_slice(text.as_bytes());
        }
        Value::Bytea(bytes) => {
            out.push(10);
            out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        Value::Uuid(bytes) => {
            out.push(11);
            out.extend_from_slice(bytes);
        }
        Value::Time(micros) => {
            out.push(12);
            out.extend_from_slice(&micros.to_le_bytes());
        }
        Value::TimeTz {
            micros,
            offset_seconds,
        } => {
            out.push(13);
            out.extend_from_slice(&datetime::time_tz_utc(*micros, *offset_seconds).to_le_bytes());
            out.extend_from_slice(&offset_seconds.to_le_bytes());
        }
        Value::Interval(interval) => {
            out.push(14);
            out.extend_from_slice(&interval.span().to_le_bytes());
        }
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) => {
            out.push(3);
            out.extend_from_slice(&value.as_integer().unwrap_or(0).to_le_bytes());
        }
        Value::Real(_) | Value::Double(_) => {
            let number = value.as_float().unwrap_or(f64::NAN);
            if number.is_nan() {
                out.push(4);
            } else if number.fract() == 0.0 && number.abs() < 9.0e18 {
                out.push(3);
                out.extend_from_slice(&(number as i64).to_le_bytes());
            } else {
                out.push(5);
                out.extend_from_slice(&number.to_bits().to_le_bytes());
            }
        }
        Value::Numeric(number) => {
            if number.is_nan() {
                out.push(4);
            } else if let Some(whole) = number
                .integral_value()
                .and_then(|whole| i64::try_from(whole).ok())
            {
                out.push(3);
                out.extend_from_slice(&whole.to_le_bytes());
            } else if let Some(double) = number.exact_double() {
                equality_key(out, &Value::Double(double));
            } else {
                let text = number.equality_text();
                out.push(6);
                out.extend_from_slice(&(text.len() as u64).to_le_bytes());
                out.extend_from_slice(text.as_bytes());
            }
        }
        Value::Date(_) | Value::Timestamp(_) => {
            out.push(7);
            out.extend_from_slice(&instant(value, false).to_le_bytes());
        }
        Value::TimestampTz(micros) => {
            out.push(8);
            out.extend_from_slice(&micros.to_le_bytes());
        }
        Value::Json(text) => {
            out.push(9);
            out.extend_from_slice(&(text.len() as u64).to_le_bytes());
            out.extend_from_slice(text.as_bytes());
        }
        Value::Array(array) => crate::arrays::equality_key_of(out, array),
        Value::Jsonb(text) => {
            let comparable = crate::json::comparable(text);
            out.push(9);
            out.extend_from_slice(&(comparable.len() as u64).to_le_bytes());
            out.extend_from_slice(comparable.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Conversion, SqlType, Value, convert, parse_input};

    #[test]
    fn a_value_stays_four_words_with_the_new_types() {
        assert!(std::mem::size_of::<Value>() <= 32);
        assert!(std::mem::size_of::<SqlType>() <= 12);
    }

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
