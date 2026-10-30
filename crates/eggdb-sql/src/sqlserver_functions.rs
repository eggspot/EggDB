use crate::ast::AggregateFunction;
use crate::datetime::{self, MICROS_PER_DAY, MICROS_PER_SECOND};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::numeric::Numeric;
use crate::sqlite_functions::MAX_TEXT_BYTES;
use crate::sqlserver_conversion::{DateOrder, Temporal, parse_datetime, text_of};
use crate::sqlserver_numbers::NumberFunction;
use crate::sqlserver_strings::{TextFunction, TextOrigin};
use crate::sqlserver_types::SqlServerType;
use crate::types::{NumericModifier, SqlType, Value};
use std::cell::Cell;

const MICROS_PER_MILLISECOND: i64 = 1000;
const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SECOND;
const MICROS_PER_HOUR: i64 = 60 * MICROS_PER_MINUTE;
const NANOS_PER_MICRO: i128 = 1000;
const SCOPE_IDENTITY_PRECISION: u16 = 38;
const DATE_NAME_LENGTH: u32 = 30;
const SECONDS_PER_MINUTE: i32 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DatePart {
    Year,
    Quarter,
    Month,
    DayOfYear,
    Day,
    Week,
    Weekday,
    Hour,
    Minute,
    Second,
    Millisecond,
    Microsecond,
    Nanosecond,
    IsoWeek,
    TzOffset,
}

impl DatePart {
    pub(crate) fn named(word: &str) -> Option<Self> {
        Some(match word {
            "year" | "yy" | "yyyy" => Self::Year,
            "quarter" | "qq" | "q" => Self::Quarter,
            "month" | "mm" | "m" => Self::Month,
            "dayofyear" | "dy" | "y" => Self::DayOfYear,
            "day" | "dd" | "d" => Self::Day,
            "week" | "wk" | "ww" => Self::Week,
            "weekday" | "dw" | "w" => Self::Weekday,
            "hour" | "hh" => Self::Hour,
            "minute" | "mi" | "n" => Self::Minute,
            "second" | "ss" | "s" => Self::Second,
            "millisecond" | "ms" => Self::Millisecond,
            "microsecond" | "mcs" => Self::Microsecond,
            "nanosecond" | "ns" => Self::Nanosecond,
            "iso_week" | "isowk" | "isoww" => Self::IsoWeek,
            "tzoffset" | "tz" => Self::TzOffset,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Year => "year",
            Self::Quarter => "quarter",
            Self::Month => "month",
            Self::DayOfYear => "dayofyear",
            Self::Day => "day",
            Self::Week => "week",
            Self::Weekday => "weekday",
            Self::Hour => "hour",
            Self::Minute => "minute",
            Self::Second => "second",
            Self::Millisecond => "millisecond",
            Self::Microsecond => "microsecond",
            Self::Nanosecond => "nanosecond",
            Self::IsoWeek => "iso_week",
            Self::TzOffset => "tzoffset",
        }
    }

    pub(crate) fn counts_between_dates(self) -> bool {
        !matches!(self, Self::IsoWeek | Self::TzOffset)
    }

    fn is_time_of_day(self) -> bool {
        matches!(
            self,
            Self::Hour
                | Self::Minute
                | Self::Second
                | Self::Millisecond
                | Self::Microsecond
                | Self::Nanosecond
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Function {
    Convert {
        target: SqlServerType,
        style: Option<u16>,
    },
    TryConvert {
        target: SqlServerType,
        style: Option<u16>,
    },
    DateAdd(DatePart),
    DateDiff(DatePart),
    DateDiffBig(DatePart),
    DatePart(DatePart),
    DateName(DatePart),
    EndOfMonth,
    IsNull,
    Len,
    CharIndex,
    NewId,
    RowCount,
    ScopeIdentity,
    AggregateResult {
        function: AggregateFunction,
        input: Option<SqlType>,
    },
    Metadata(crate::sqlserver_catalog::MetadataFunction),
    Text {
        function: TextFunction,
        national: bool,
    },
    Number(NumberFunction),
    Format,
}

impl Function {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Convert { .. } => "convert",
            Self::TryConvert { .. } => "try_convert",
            Self::DateAdd(_) => "dateadd",
            Self::DateDiff(_) => "datediff",
            Self::DateDiffBig(_) => "datediff_big",
            Self::DatePart(_) => "datepart",
            Self::DateName(_) => "datename",
            Self::EndOfMonth => "eomonth",
            Self::IsNull => "isnull",
            Self::Len => "len",
            Self::CharIndex => "charindex",
            Self::NewId => "newid",
            Self::RowCount => "@@rowcount",
            Self::ScopeIdentity => "scope_identity",
            Self::AggregateResult { function, .. } => function.name(),
            Self::Metadata(function) => function.name(),
            Self::Text { function, .. } => function.name(),
            Self::Number(function) => function.name(),
            Self::Format => "format",
        }
    }

    pub(crate) fn needs_declared_type(self) -> bool {
        match self {
            Self::Text { function, .. } => function.needs_declared_type(),
            Self::Number(_) => true,
            _ => false,
        }
    }

    pub(crate) fn result_type(self, first_argument: Option<SqlType>) -> Option<SqlType> {
        match self {
            Self::Convert { target, .. } | Self::TryConvert { target, .. } => {
                Some(target.engine_type())
            }
            Self::DateAdd(_) => match first_argument {
                Some(sql_type)
                    if sql_type.is_datetime() || matches!(sql_type, SqlType::Time(_)) =>
                {
                    Some(sql_type)
                }
                _ => Some(SqlType::Timestamp(Some(3))),
            },
            Self::DateDiff(_)
            | Self::DatePart(_)
            | Self::Len
            | Self::CharIndex
            | Self::RowCount
            | Self::AggregateResult {
                function: AggregateFunction::Count,
                ..
            } => Some(SqlType::Integer),
            Self::DateDiffBig(_) => Some(SqlType::BigInt),
            Self::DateName(_) => Some(SqlType::Varchar(Some(DATE_NAME_LENGTH))),
            Self::EndOfMonth => Some(SqlType::Date),
            Self::IsNull => first_argument,
            Self::NewId => Some(SqlType::Uuid),
            Self::ScopeIdentity => Some(SqlType::Numeric(Some(NumericModifier {
                precision: SCOPE_IDENTITY_PRECISION,
                scale: 0,
            }))),
            Self::AggregateResult { function, input } => {
                crate::sqlserver_numbers::aggregate_type(function, input)
            }
            Self::Metadata(function) => function.result_type(),
            Self::Text { function, .. } => function.result_type(first_argument),
            Self::Number(function) => function.result_type(first_argument),
            Self::Format => Some(SqlType::Text),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SessionStatus {
    pub(crate) row_count: i64,
    pub(crate) identity: Option<i64>,
}

thread_local! {
    static STATUS: Cell<SessionStatus> = const {
        Cell::new(SessionStatus {
            row_count: 0,
            identity: None,
        })
    };
}

pub(crate) struct StatusGuard(SessionStatus);

impl Drop for StatusGuard {
    fn drop(&mut self) {
        STATUS.with(|status| status.set(self.0));
    }
}

pub(crate) fn enter_status(status: SessionStatus) -> StatusGuard {
    StatusGuard(STATUS.with(|current| current.replace(status)))
}

fn status() -> SessionStatus {
    STATUS.with(Cell::get)
}

fn lob_too_large() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("Attempting to grow LOB beyond maximum allowed size of {MAX_TEXT_BYTES} bytes."),
    )
    .with_sqlserver_code(7119)
}

pub(crate) fn within_limit(bytes: usize) -> SqlResult<()> {
    if bytes > MAX_TEXT_BYTES {
        Err(lob_too_large())
    } else {
        crate::memory::check_value_bytes(bytes)
    }
}

pub(crate) fn date_add_overflow(target: &str) -> SqlError {
    SqlError::new(
        SqlState::DATETIME_FIELD_OVERFLOW,
        format!("Adding a value to a '{target}' column caused an overflow."),
    )
    .with_sqlserver_code(517)
}

fn date_diff_overflow() -> SqlError {
    SqlError::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        "The datediff function resulted in an overflow. The number of dateparts separating two date/time instances is too large. Try to use datediff with a less precise datepart.",
    )
    .with_sqlserver_code(535)
}

fn part_not_supported(part: DatePart, function: &str, type_name: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!(
            "The datepart {} is not supported by date function {function} for data type {type_name}.",
            part.name()
        ),
    )
    .with_sqlserver_code(9810)
}

fn argument_type_invalid(value: &Value, argument: usize, function: &str) -> SqlError {
    SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        format!(
            "Argument data type {} is invalid for argument {argument} of {function} function.",
            crate::sqlserver_types::value_type_name(value)
        ),
    )
    .with_sqlserver_code(8116)
}

fn count_overflow() -> SqlError {
    SqlError::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        "Arithmetic overflow error converting expression to data type int.",
    )
    .with_sqlserver_code(8115)
}

fn civil_micros(year: i64, month: i64, day: i64) -> i64 {
    datetime::days_from_civil(year, month, day) * MICROS_PER_DAY
}

fn first_moment() -> i64 {
    civil_micros(1, 1, 1)
}

fn end_moment() -> i64 {
    civil_micros(10000, 1, 1)
}

enum DateArgument {
    Date(i64),
    Time(i64),
    Local(i64),
    Utc(i64),
}

fn date_argument(
    value: &Value,
    argument: usize,
    function: &str,
) -> SqlResult<Option<DateArgument>> {
    Ok(Some(match value {
        Value::Null => return Ok(None),
        Value::Date(days) => DateArgument::Date(i64::from(*days) * MICROS_PER_DAY),
        Value::Time(micros) => DateArgument::Time(*micros),
        Value::Timestamp(micros) => DateArgument::Local(*micros),
        Value::TimestampTz(micros) => DateArgument::Utc(*micros),
        Value::Text(text) | Value::Bpchar(text) => {
            match parse_datetime(text, Temporal::DateTime2, 7, DateOrder::MonthDayYear)? {
                Value::Timestamp(micros) => DateArgument::Local(micros),
                _ => return Ok(None),
            }
        }
        other => return Err(argument_type_invalid(other, argument, function)),
    }))
}

impl DateArgument {
    fn micros(&self) -> i64 {
        match self {
            Self::Date(micros) | Self::Local(micros) | Self::Utc(micros) => *micros,
            Self::Time(micros) => civil_micros(1900, 1, 1) + micros,
        }
    }

    fn type_name(&self) -> &'static str {
        match self {
            Self::Date(_) => "date",
            Self::Time(_) => "time",
            Self::Local(_) => "datetime2",
            Self::Utc(_) => "datetimeoffset",
        }
    }
}

fn add_months(micros: i64, months: i64) -> Option<i64> {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let of_day = micros.rem_euclid(MICROS_PER_DAY);
    let (year, month, day) = datetime::civil_from_days(days);
    let total = (year * 12 + month - 1).checked_add(months)?;
    let (year, month) = (total.div_euclid(12), total.rem_euclid(12) + 1);
    if !(1..=9999).contains(&year) {
        return None;
    }
    let day = day.min(datetime::days_in_month(year, month));
    Some(datetime::days_from_civil(year, month, day) * MICROS_PER_DAY + of_day)
}

fn units_of(part: DatePart) -> Option<i128> {
    Some(match part {
        DatePart::DayOfYear | DatePart::Day | DatePart::Weekday => i128::from(MICROS_PER_DAY),
        DatePart::Week => 7 * i128::from(MICROS_PER_DAY),
        DatePart::Hour => i128::from(MICROS_PER_HOUR),
        DatePart::Minute => i128::from(MICROS_PER_MINUTE),
        DatePart::Second => i128::from(MICROS_PER_SECOND),
        DatePart::Millisecond => i128::from(MICROS_PER_MILLISECOND),
        DatePart::Microsecond => 1,
        DatePart::Year
        | DatePart::Quarter
        | DatePart::Month
        | DatePart::Nanosecond
        | DatePart::IsoWeek
        | DatePart::TzOffset => {
            return None;
        }
    })
}

fn date_add(part: DatePart, number: &Value, date: &Value) -> SqlResult<Value> {
    if number.is_null() || date.is_null() {
        return Ok(Value::Null);
    }
    let count = match number {
        Value::Text(_) | Value::Bpchar(_) => {
            crate::sqlserver_conversion::convert(number.clone(), SqlServerType::BigInt, None, None)?
                .as_integer()
                .unwrap_or(0)
        }
        Value::Boolean(flag) => i64::from(*flag),
        Value::Numeric(value) => value
            .truncated(0)
            .ok()
            .and_then(|whole| whole.integral_value())
            .and_then(|whole| i64::try_from(whole).ok())
            .ok_or_else(|| date_add_overflow("datetime2"))?,
        other => match other.as_integer() {
            Some(count) => count,
            None => {
                let float = other.as_float().unwrap_or(f64::NAN).trunc();
                if !float.is_finite() || float.abs() > 9e18 {
                    return Err(date_add_overflow("datetime2"));
                }
                float as i64
            }
        },
    };
    let Some(argument_date) = date_argument(date, 3, "dateadd")? else {
        return Ok(Value::Null);
    };
    let type_name = argument_date.type_name();
    if matches!(argument_date, DateArgument::Date(_)) && part.is_time_of_day() {
        return Err(part_not_supported(part, "dateadd", "date"));
    }
    if matches!(argument_date, DateArgument::Time(_)) && !part.is_time_of_day() {
        return Err(part_not_supported(part, "dateadd", "time"));
    }
    let overflow = || date_add_overflow(type_name);
    let start = argument_date.micros();
    let shifted = match part {
        DatePart::Year => add_months(start, count.checked_mul(12).ok_or_else(overflow)?),
        DatePart::Quarter => add_months(start, count.checked_mul(3).ok_or_else(overflow)?),
        DatePart::Month => add_months(start, count),
        DatePart::Nanosecond => start.checked_add((i128::from(count) / NANOS_PER_MICRO) as i64),
        part => {
            let units = units_of(part).unwrap_or(1);
            let moved = i128::from(start) + i128::from(count) * units;
            i64::try_from(moved).ok()
        }
    }
    .ok_or_else(overflow)?;
    Ok(match argument_date {
        DateArgument::Time(_) => Value::Time(shifted.rem_euclid(MICROS_PER_DAY)),
        _ if shifted < first_moment() || shifted >= end_moment() => return Err(overflow()),
        DateArgument::Date(_) => Value::Date(shifted.div_euclid(MICROS_PER_DAY) as i32),
        DateArgument::Local(_) => Value::Timestamp(shifted),
        DateArgument::Utc(_) => Value::TimestampTz(shifted),
    })
}

fn boundaries(part: DatePart, micros: i64) -> i128 {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let (year, month, _) = datetime::civil_from_days(days);
    match part {
        DatePart::Year => i128::from(year),
        DatePart::Quarter => i128::from(year * 4 + (month - 1) / 3),
        DatePart::Month => i128::from(year * 12 + month - 1),
        DatePart::DayOfYear | DatePart::Day | DatePart::Weekday => i128::from(days),
        DatePart::Week => i128::from((days - 1).div_euclid(7)),
        DatePart::Nanosecond => i128::from(micros) * NANOS_PER_MICRO,
        part => i128::from(micros).div_euclid(units_of(part).unwrap_or(1)),
    }
}

fn date_diff(part: DatePart, start: &Value, end: &Value, big: bool) -> SqlResult<Value> {
    let function = if big { "datediff_big" } else { "datediff" };
    let (Some(start), Some(end)) = (
        date_argument(start, 2, function)?,
        date_argument(end, 3, function)?,
    ) else {
        return Ok(Value::Null);
    };
    let difference = boundaries(part, end.micros()) - boundaries(part, start.micros());
    if big {
        i64::try_from(difference)
            .map(Value::BigInt)
            .map_err(|_| date_diff_big_overflow())
    } else {
        i32::try_from(difference)
            .map(Value::Integer)
            .map_err(|_| date_diff_overflow())
    }
}

fn date_diff_big_overflow() -> SqlError {
    SqlError::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        "The datediff_big function resulted in an overflow. The number of dateparts separating two date/time instances is too large. Try to use datediff_big with a less precise datepart.",
    )
    .with_sqlserver_code(535)
}

struct LocalDate {
    days: i64,
    of_day: i64,
    offset_seconds: Option<i32>,
}

fn local_date(value: &Value, part: DatePart, function: &str) -> SqlResult<Option<LocalDate>> {
    let Some(argument) = date_argument(value, 2, function)? else {
        return Ok(None);
    };
    let type_name = argument.type_name();
    let refused = match argument {
        DateArgument::Date(_) => part.is_time_of_day(),
        DateArgument::Time(_) => !part.is_time_of_day() && part != DatePart::TzOffset,
        _ => false,
    };
    if refused {
        return Err(part_not_supported(part, function, type_name));
    }
    let (local, offset_seconds) = match argument {
        DateArgument::Utc(micros) => (datetime::utc_to_local(micros)?, Some(datetime::session_offset())),
        other => (other.micros(), None),
    };
    Ok(Some(LocalDate {
        days: local.div_euclid(MICROS_PER_DAY),
        of_day: local.rem_euclid(MICROS_PER_DAY),
        offset_seconds,
    }))
}

fn sunday_based_weekday(days: i64) -> i64 {
    (days + 6).rem_euclid(7) + 1
}

fn part_number(part: DatePart, date: &LocalDate) -> i64 {
    let (year, month, day) = datetime::civil_from_days(date.days);
    let day_of_year = date.days - datetime::days_from_civil(year, 1, 1) + 1;
    let first_weekday = sunday_based_weekday(datetime::days_from_civil(year, 1, 1));
    let of_second = date.of_day % MICROS_PER_SECOND;
    match part {
        DatePart::Year => year,
        DatePart::Quarter => (month - 1) / 3 + 1,
        DatePart::Month => month,
        DatePart::DayOfYear => day_of_year,
        DatePart::Day => day,
        DatePart::Week => (day_of_year - 1 + first_weekday - 1) / 7 + 1,
        DatePart::Weekday => sunday_based_weekday(date.days),
        DatePart::Hour => date.of_day / MICROS_PER_HOUR,
        DatePart::Minute => date.of_day / MICROS_PER_MINUTE % 60,
        DatePart::Second => date.of_day / MICROS_PER_SECOND % 60,
        DatePart::Millisecond => of_second / MICROS_PER_MILLISECOND,
        DatePart::Microsecond => of_second,
        DatePart::Nanosecond => of_second * NANOS_PER_MICRO as i64,
        DatePart::IsoWeek => datetime::iso_week(date.days).1,
        DatePart::TzOffset => i64::from(date.offset_seconds.unwrap_or(0) / SECONDS_PER_MINUTE),
    }
}

fn date_part(part: DatePart, value: &Value) -> SqlResult<Value> {
    Ok(match local_date(value, part, "datepart")? {
        None => Value::Null,
        Some(date) => Value::Integer(i32::try_from(part_number(part, &date)).unwrap_or(i32::MAX)),
    })
}

fn offset_name(offset_seconds: i32) -> String {
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let minutes = offset_seconds.unsigned_abs() / 60;
    format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

fn date_name(part: DatePart, value: &Value) -> SqlResult<Value> {
    let Some(date) = local_date(value, part, "datename")? else {
        return Ok(Value::Null);
    };
    let number = part_number(part, &date);
    Ok(Value::Text(match part {
        DatePart::Month => crate::sqlserver_format::MONTH_NAMES
            .get(usize::try_from(number - 1).unwrap_or(0))
            .copied()
            .unwrap_or_default()
            .to_owned(),
        DatePart::Weekday => crate::sqlserver_format::DAY_NAMES
            .get(usize::try_from(number - 1).unwrap_or(0))
            .copied()
            .unwrap_or_default()
            .to_owned(),
        DatePart::TzOffset => offset_name(date.offset_seconds.unwrap_or(0)),
        _ => number.to_string(),
    }))
}

fn end_of_month(arguments: &[Value]) -> SqlResult<Value> {
    let start = arguments.first().unwrap_or(&Value::Null);
    let months = arguments.get(1).unwrap_or(&Value::Null);
    let Some(argument) = date_argument(start, 1, "eomonth")? else {
        return Ok(Value::Null);
    };
    if matches!(argument, DateArgument::Time(_)) {
        return Err(argument_type_invalid(start, 1, "eomonth"));
    }
    let added = match months {
        Value::Null if arguments.len() > 1 => return Ok(Value::Null),
        Value::Null => 0,
        Value::Text(_) | Value::Bpchar(_) => {
            crate::sqlserver_conversion::convert(months.clone(), SqlServerType::Int, None, None)?
                .as_integer()
                .unwrap_or(0)
        }
        Value::Numeric(number) => number
            .truncated(0)
            .ok()
            .and_then(|whole| whole.integral_value())
            .and_then(|whole| i64::try_from(whole).ok())
            .ok_or_else(|| date_add_overflow("date"))?,
        other => other
            .as_integer()
            .ok_or_else(|| argument_type_invalid(other, 2, "eomonth"))?,
    };
    let local = match argument {
        DateArgument::Utc(micros) => datetime::utc_to_local(micros)?,
        other => other.micros(),
    };
    let day = local.div_euclid(MICROS_PER_DAY) * MICROS_PER_DAY;
    let shifted = add_months(day, added).ok_or_else(|| date_add_overflow("date"))?;
    let (year, month, _) = datetime::civil_from_days(shifted.div_euclid(MICROS_PER_DAY));
    let last = datetime::days_from_civil(year, month, datetime::days_in_month(year, month));
    i32::try_from(last)
        .map(Value::Date)
        .map_err(|_| date_add_overflow("date"))
}

fn character(value: &Value) -> SqlResult<Option<String>> {
    Ok(match value {
        Value::Null => None,
        Value::Text(text) | Value::Bpchar(text) => Some(text.clone()),
        other => Some(text_of(other, None, None)?),
    })
}

fn length(value: &Value) -> SqlResult<Value> {
    Ok(match character(value)? {
        None => Value::Null,
        Some(text) => {
            let counted = text.trim_end_matches(' ').chars().count();
            Value::Integer(i32::try_from(counted).unwrap_or(i32::MAX))
        }
    })
}

fn char_index(arguments: &[Value]) -> SqlResult<Value> {
    let (Some(find), Some(within)) = (
        character(arguments.first().unwrap_or(&Value::Null))?,
        character(arguments.get(1).unwrap_or(&Value::Null))?,
    ) else {
        return Ok(Value::Null);
    };
    let start = match arguments.get(2) {
        None => 1,
        Some(Value::Null) => return Ok(Value::Null),
        Some(value) => value.as_integer().unwrap_or(1).max(1),
    };
    if find.is_empty() {
        return Ok(Value::Integer(0));
    }
    let skipped = usize::try_from(start - 1).unwrap_or(usize::MAX);
    let Some((offset, _)) = within.char_indices().nth(skipped) else {
        return Ok(Value::Integer(0));
    };
    let found = within[offset..]
        .find(&find)
        .map_or(0, |at| within[..offset + at].chars().count() + 1);
    Ok(Value::Integer(i32::try_from(found).unwrap_or(i32::MAX)))
}

fn new_id() -> SqlResult<Value> {
    let mut bytes = [0u8; 16];
    crate::random::fill(&mut bytes)?;
    Ok(Value::Uuid(crate::bytea::random_uuid(bytes)))
}

fn aggregate_result(
    function: AggregateFunction,
    input: Option<SqlType>,
    value: &Value,
) -> SqlResult<Value> {
    match (function, value) {
        (_, Value::Null) => Ok(Value::Null),
        (AggregateFunction::Count, other) => other
            .as_integer()
            .and_then(|count| i32::try_from(count).ok())
            .map(Value::Integer)
            .ok_or_else(count_overflow),
        (AggregateFunction::Sum | AggregateFunction::Avg, other) => {
            crate::sqlserver_numbers::aggregate_value(function, input, other.clone())
        }
        (_, other) => Ok(other.clone()),
    }
}

pub(crate) fn call(function: Function, arguments: &[Value]) -> SqlResult<Value> {
    call_declared(function, arguments, None)
}

pub(crate) fn call_declared(
    function: Function,
    arguments: &[Value],
    declared: Option<SqlType>,
) -> SqlResult<Value> {
    let argument = |index: usize| arguments.get(index).unwrap_or(&Value::Null);
    match function {
        Function::Convert { target, style } => {
            crate::sqlserver_conversion::convert(argument(0).clone(), target, style, None)
        }
        Function::TryConvert { target, style } => {
            crate::sqlserver_conversion::try_convert(argument(0).clone(), target, style, None)
        }
        Function::DateAdd(part) => date_add(part, argument(0), argument(1)),
        Function::DateDiff(part) => date_diff(part, argument(0), argument(1), false),
        Function::DateDiffBig(part) => date_diff(part, argument(0), argument(1), true),
        Function::DatePart(part) => date_part(part, argument(0)),
        Function::DateName(part) => date_name(part, argument(0)),
        Function::EndOfMonth => end_of_month(arguments),
        Function::IsNull => Ok(if argument(0).is_null() {
            argument(1).clone()
        } else {
            argument(0).clone()
        }),
        Function::Len => length(argument(0)),
        Function::CharIndex => char_index(arguments),
        Function::NewId => new_id(),
        Function::RowCount => Ok(Value::Integer(
            i32::try_from(status().row_count).unwrap_or(i32::MAX),
        )),
        Function::ScopeIdentity => Ok(match status().identity {
            Some(identity) => Value::Numeric(Numeric::from_i64(identity)),
            None => Value::Null,
        }),
        Function::AggregateResult { function, input } => {
            aggregate_result(function, input, argument(0))
        }
        Function::Metadata(function) => {
            crate::sqlserver_catalog::call_metadata(function, arguments)
        }
        Function::Text { function, national } => {
            crate::sqlserver_strings::call(function, arguments, TextOrigin { national, declared })
        }
        Function::Number(function) => crate::sqlserver_numbers::call(function, arguments, declared),
        Function::Format => crate::sqlserver_format::format(arguments),
    }
}

#[cfg(test)]
mod tests {
    use super::{DatePart, Function, SessionStatus, call, enter_status, within_limit};
    use crate::types::Value;

    fn text(value: &str) -> Value {
        Value::Text(value.to_owned())
    }

    fn diff(part: &str) -> Value {
        let part = DatePart::named(part).unwrap_or(DatePart::Year);
        call(
            Function::DateDiff(part),
            &[
                text("2005-12-31 23:59:59.9999999"),
                text("2006-01-01 00:00:00.0000000"),
            ],
        )
        .unwrap_or(Value::Null)
    }

    #[test]
    fn datediff_counts_one_boundary_for_every_part_across_new_year() {
        for part in [
            "year",
            "quarter",
            "month",
            "dayofyear",
            "day",
            "week",
            "hour",
            "minute",
            "second",
            "millisecond",
            "microsecond",
        ] {
            assert_eq!(diff(part), Value::Integer(1), "{part}");
        }
    }

    #[test]
    fn dateadd_clamps_to_the_month_end_and_refuses_time_parts_on_dates() {
        let added = call(
            Function::DateAdd(DatePart::Month),
            &[Value::Integer(1), text("20060831")],
        );
        let expected = crate::sqlserver_conversion::parse_datetime(
            "2006-09-30",
            crate::sqlserver_conversion::Temporal::DateTime2,
            7,
            crate::sqlserver_conversion::DateOrder::MonthDayYear,
        );
        assert_eq!(added.ok(), expected.ok());
        let on_date = call(
            Function::DateAdd(DatePart::Hour),
            &[Value::Integer(1), Value::Date(0)],
        );
        assert_eq!(on_date.err().and_then(|e| e.vendor_code()), Some(9810));
        let overflow = call(
            Function::DateAdd(DatePart::Year),
            &[Value::Integer(i32::MAX), Value::Date(0)],
        );
        assert_eq!(overflow.err().and_then(|e| e.vendor_code()), Some(517));
    }

    #[test]
    fn len_and_charindex_count_characters() {
        assert_eq!(
            call(Function::Len, &[text("abc  ")]).ok(),
            Some(Value::Integer(3))
        );
        assert_eq!(
            call(Function::Len, &[text("  é😀")]).ok(),
            Some(Value::Integer(4))
        );
        let found = call(
            Function::CharIndex,
            &[
                text("bicycle"),
                text("Reflectors are vital safety components of your bicycle."),
            ],
        );
        assert_eq!(found.ok(), Some(Value::Integer(48)));
        let after = call(
            Function::CharIndex,
            &[text("b"), text("😀ab b"), Value::Integer(4)],
        );
        assert_eq!(after.ok(), Some(Value::Integer(5)));
        let missing = call(Function::CharIndex, &[text("z"), text("abc")]);
        assert_eq!(missing.ok(), Some(Value::Integer(0)));
    }

    #[test]
    fn session_values_are_read_when_the_statement_runs() {
        let _status = enter_status(SessionStatus {
            row_count: 7,
            identity: Some(42),
        });
        assert_eq!(call(Function::RowCount, &[]).ok(), Some(Value::Integer(7)));
        assert!(matches!(
            call(Function::ScopeIdentity, &[]),
            Ok(Value::Numeric(_))
        ));
    }

    #[test]
    fn values_over_a_gigabyte_are_refused_before_they_are_built() {
        let error = within_limit(1_000_000_001).err();
        assert_eq!(error.and_then(|e| e.vendor_code()), Some(7119));
    }

    fn part_of(part: &str, value: Value) -> Value {
        let part = DatePart::named(part).unwrap_or(DatePart::Year);
        call(Function::DatePart(part), &[value]).unwrap_or(Value::Null)
    }

    #[test]
    fn datepart_reads_every_part_with_sunday_as_the_first_day() {
        let stamp = || text("2020-07-04 13:05:09.1234567");
        let cases = [
            ("year", 2020),
            ("quarter", 3),
            ("month", 7),
            ("dayofyear", 186),
            ("day", 4),
            ("week", 27),
            ("weekday", 7),
            ("hour", 13),
            ("minute", 5),
            ("second", 9),
            ("millisecond", 123),
            ("microsecond", 123_456),
            ("nanosecond", 123_456_000),
            ("iso_week", 27),
            ("tzoffset", 0),
        ];
        for (part, expected) in cases {
            assert_eq!(part_of(part, stamp()), Value::Integer(expected), "{part}");
        }
        assert_eq!(part_of("week", text("2020-01-04")), Value::Integer(1));
        assert_eq!(part_of("week", text("2020-01-05")), Value::Integer(2));
        let on_time = call(Function::DatePart(DatePart::Year), &[Value::Time(0)]);
        assert_eq!(on_time.err().and_then(|e| e.vendor_code()), Some(9810));
    }

    #[test]
    fn datename_names_months_and_weekdays() {
        let named = |part| {
            call(Function::DateName(part), &[text("2020-07-04")]).unwrap_or(Value::Null)
        };
        assert_eq!(named(DatePart::Month), text("July"));
        assert_eq!(named(DatePart::Weekday), text("Saturday"));
        assert_eq!(named(DatePart::Day), text("4"));
    }

    #[test]
    fn eomonth_moves_by_months_and_lands_on_the_last_day() {
        let end = |arguments: &[Value]| call(Function::EndOfMonth, arguments);
        let leap = crate::datetime::days_from_civil(2024, 2, 29) as i32;
        assert_eq!(end(&[text("2024-02-10")]).ok(), Some(Value::Date(leap)));
        let later = crate::datetime::days_from_civil(2024, 4, 30) as i32;
        assert_eq!(
            end(&[text("2024-01-31"), Value::Integer(3)]).ok(),
            Some(Value::Date(later))
        );
        let overflow = end(&[text("9999-12-01"), Value::Integer(1)]);
        assert_eq!(overflow.err().and_then(|e| e.vendor_code()), Some(517));
        let huge = end(&[text("2020-01-01"), Value::Integer(i32::MAX)]);
        assert_eq!(huge.err().and_then(|e| e.vendor_code()), Some(517));
    }

    #[test]
    fn datediff_big_counts_past_an_int() {
        let counted = call(
            Function::DateDiffBig(DatePart::Millisecond),
            &[text("0001-01-01"), text("9999-12-31")],
        );
        assert_eq!(counted.ok(), Some(Value::BigInt(315_537_811_200_000)));
    }
}
