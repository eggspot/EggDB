use crate::ast::{Expr, ExprKind, Literal};
use crate::datetime::{self, LocalTime, MICROS_PER_DAY, MICROS_PER_SECOND, MYSQL_TIME_LIMIT};
use crate::error::SqlResult;
use crate::mysql_date_parts::{self, MONTH_NAMES, WEEKDAY_NAMES};
use crate::types::{SqlType, Value};

pub(crate) const DATE_ADD: &str = "mysql_date_add";
pub(crate) const DATE_SUB: &str = "mysql_date_sub";
pub(crate) const TIMESTAMPDIFF: &str = "mysql_timestampdiff";
pub(crate) const STR_TO_DATE: &str = "mysql_str_to_date";
pub(crate) const STR_TO_DATETIME: &str = "mysql_str_to_datetime";
pub(crate) const STR_TO_TIME: &str = "mysql_str_to_time";
pub(crate) const INTERVAL_OPERAND: &str = "mysql_interval";
const UNIX_TIMESTAMP: &str = "mysql_unix_timestamp";
const FROM_UNIXTIME: &str = "mysql_from_unixtime";
const FROM_UNIXTIME_FORMATTED: &str = "mysql_from_unixtime_formatted";
const UTC_DATE: &str = "mysql_utc_date";
const UTC_TIME: &str = "mysql_utc_time";
const DATEDIFF: &str = "mysql_datediff";
const TO_DAYS: &str = "mysql_to_days";
const TO_SECONDS: &str = "mysql_to_seconds";
const FROM_DAYS: &str = "mysql_from_days";
const LAST_DAY: &str = "mysql_last_day";
const MAKEDATE: &str = "mysql_makedate";
const MAKETIME: &str = "mysql_maketime";
const MONTHNAME: &str = "mysql_monthname";
const DAYNAME: &str = "mysql_dayname";
const TIME_TO_SEC: &str = "mysql_time_to_sec";
const SEC_TO_TIME: &str = "mysql_sec_to_time";
const PERIOD_ADD: &str = "mysql_period_add";
const PERIOD_DIFF: &str = "mysql_period_diff";

pub(crate) const INTERNAL_NAMES: [&str; 25] = [
    INTERVAL_OPERAND,
    DATE_ADD,
    DATE_SUB,
    TIMESTAMPDIFF,
    STR_TO_DATE,
    STR_TO_DATETIME,
    STR_TO_TIME,
    UNIX_TIMESTAMP,
    FROM_UNIXTIME,
    FROM_UNIXTIME_FORMATTED,
    UTC_DATE,
    UTC_TIME,
    DATEDIFF,
    TO_DAYS,
    TO_SECONDS,
    FROM_DAYS,
    LAST_DAY,
    MAKEDATE,
    MAKETIME,
    MONTHNAME,
    DAYNAME,
    TIME_TO_SEC,
    SEC_TO_TIME,
    PERIOD_ADD,
    PERIOD_DIFF,
];

pub(crate) const SPECIAL_SYNTAX: [&str; 7] = [
    "date_add",
    "date_sub",
    "adddate",
    "subdate",
    "timestampadd",
    "timestampdiff",
    "str_to_date",
];

const DAYS_FROM_YEAR_ZERO_TO_2000: i64 = 730_485;
const LAST_DAY_NUMBER: i64 = 3_652_424;
const UNIX_TIMESTAMP_LIMIT: i64 = 32_536_771_199;
const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SECOND;
const MICROS_PER_HOUR: i64 = 60 * MICROS_PER_MINUTE;
const SIMPLE_UNITS: [&str; 9] = [
    "microsecond",
    "second",
    "minute",
    "hour",
    "day",
    "week",
    "month",
    "quarter",
    "year",
];
const DATE_UNITS: [&str; 5] = ["day", "week", "month", "quarter", "year_month"];
const MONTH_UNITS: [&str; 3] = ["month", "quarter", "year"];

pub(crate) fn internal_name(name: &str, argument_count: usize) -> Option<&'static str> {
    Some(match (name, argument_count) {
        ("unix_timestamp", 0 | 1) => UNIX_TIMESTAMP,
        ("from_unixtime", 1) => FROM_UNIXTIME,
        ("from_unixtime", 2) => FROM_UNIXTIME_FORMATTED,
        ("utc_date", 0) => UTC_DATE,
        ("utc_time", 0) => UTC_TIME,
        ("datediff", 2) => DATEDIFF,
        ("to_days", 1) => TO_DAYS,
        ("to_seconds", 1) => TO_SECONDS,
        ("from_days", 1) => FROM_DAYS,
        ("last_day", 1) => LAST_DAY,
        ("makedate", 2) => MAKEDATE,
        ("maketime", 3) => MAKETIME,
        ("monthname", 1) => MONTHNAME,
        ("dayname", 1) => DAYNAME,
        ("time_to_sec", 1) => TIME_TO_SEC,
        ("sec_to_time", 1) => SEC_TO_TIME,
        ("period_add", 2) => PERIOD_ADD,
        ("period_diff", 2) => PERIOD_DIFF,
        _ => return None,
    })
}

pub(crate) fn is_plain_function(name: &str) -> bool {
    matches!(
        name,
        "unix_timestamp"
            | "from_unixtime"
            | "utc_date"
            | "utc_time"
            | "datediff"
            | "to_days"
            | "to_seconds"
            | "from_days"
            | "last_day"
            | "makedate"
            | "maketime"
            | "monthname"
            | "dayname"
            | "time_to_sec"
            | "sec_to_time"
            | "period_add"
            | "period_diff"
    )
}

pub(crate) fn interval_unit(word: &str) -> Option<&'static str> {
    datetime::MYSQL_UNITS
        .iter()
        .copied()
        .find(|unit| *unit == word)
}

pub(crate) fn timestamp_unit(word: &str) -> Option<&'static str> {
    let word = word.strip_prefix("sql_tsi_").unwrap_or(word);
    let word = if word == "frac_second" { "microsecond" } else { word };
    SIMPLE_UNITS.iter().copied().find(|unit| *unit == word)
}

pub(crate) fn str_to_date_function(format: &str) -> &'static str {
    let mut date = false;
    let mut clock = false;
    let mut characters = format.chars();
    while let Some(character) = characters.next() {
        if character != '%' {
            continue;
        }
        match characters.next() {
            Some('Y' | 'y' | 'm' | 'c' | 'd' | 'e' | 'M' | 'b' | 'j' | 'D' | 'a' | 'W') => {
                date = true;
            }
            Some('H' | 'k' | 'h' | 'I' | 'l' | 'i' | 's' | 'S' | 'f' | 'p' | 'T' | 'r') => {
                clock = true;
            }
            _ => {}
        }
    }
    match (date, clock) {
        (true, false) => STR_TO_DATE,
        (false, true) => STR_TO_TIME,
        _ => STR_TO_DATETIME,
    }
}

fn unit_literal(expr: Option<&Expr>) -> Option<&str> {
    match expr.map(|expr| &expr.kind) {
        Some(ExprKind::Literal(Literal::String(unit))) => Some(unit.as_str()),
        _ => None,
    }
}

/// The static type of one of MySQL's date and time functions, `None` for any other name.
pub(crate) fn result_type(
    name: &str,
    arguments: &[Expr],
    static_type: impl Fn(&Expr) -> Option<SqlType>,
) -> Option<Option<SqlType>> {
    Some(match name {
        DATE_ADD | DATE_SUB => {
            let unit = unit_literal(arguments.get(2)).unwrap_or("second");
            match arguments.first().and_then(&static_type) {
                Some(SqlType::Date) if DATE_UNITS.contains(&unit) || MONTH_UNITS.contains(&unit) => {
                    Some(SqlType::Date)
                }
                Some(SqlType::Date | SqlType::Timestamp(_) | SqlType::TimestampTz(_)) => {
                    Some(SqlType::Timestamp(Some(6)))
                }
                _ => Some(SqlType::Text),
            }
        }
        TIMESTAMPDIFF | UNIX_TIMESTAMP | DATEDIFF | TO_DAYS | TO_SECONDS | TIME_TO_SEC
        | PERIOD_ADD | PERIOD_DIFF => Some(SqlType::BigInt),
        STR_TO_DATE | UTC_DATE | FROM_DAYS | LAST_DAY | MAKEDATE => Some(SqlType::Date),
        STR_TO_DATETIME => Some(SqlType::Timestamp(Some(6))),
        FROM_UNIXTIME => Some(SqlType::Timestamp(Some(6))),
        STR_TO_TIME | UTC_TIME | MAKETIME | SEC_TO_TIME => Some(SqlType::Time(Some(6))),
        FROM_UNIXTIME_FORMATTED | MONTHNAME | DAYNAME => Some(SqlType::Text),
        _ => return None,
    })
}

/// Evaluates one of MySQL's date and time functions; `None` when `name` is not one of them.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !INTERNAL_NAMES.contains(&name) {
        return None;
    }
    if name == INTERVAL_OPERAND {
        return Some(Err(crate::error::SqlError::syntax(
            "You have an error in your SQL syntax near 'INTERVAL': an interval only adds to or subtracts from a date"
                .to_owned(),
            0,
        )));
    }
    if values.iter().any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    Some(evaluate(name, values))
}

fn evaluate(name: &str, values: &[Value]) -> SqlResult<Value> {
    Ok(match (name, values) {
        (DATE_ADD, [date, amount, Value::Text(unit)]) => date_add(date, amount, unit, false)?,
        (DATE_SUB, [date, amount, Value::Text(unit)]) => date_add(date, amount, unit, true)?,
        (TIMESTAMPDIFF, [Value::Text(unit), from, to]) => timestamp_diff(unit, from, to)?,
        (STR_TO_DATE | STR_TO_DATETIME | STR_TO_TIME, [text, format]) => {
            str_to_date(name, text, format)
        }
        (UNIX_TIMESTAMP, []) => Value::BigInt(unix_seconds(datetime::statement_now())),
        (UNIX_TIMESTAMP, [date]) => unix_timestamp(date)?,
        (FROM_UNIXTIME, [seconds]) => from_unixtime(seconds)?,
        (FROM_UNIXTIME_FORMATTED, [seconds, format]) => match from_unixtime(seconds)? {
            Value::Null => Value::Null,
            moment => mysql_date_parts::date_format(&moment, format)?,
        },
        (UTC_DATE, []) => Value::Date(datetime::timestamp_to_date(datetime::statement_now())),
        (UTC_TIME, []) => {
            let of_day = datetime::time_of_timestamp(datetime::statement_now());
            Value::Time(of_day - of_day.rem_euclid(MICROS_PER_SECOND))
        }
        (DATEDIFF, [left, right]) => match (local(left)?, local(right)?) {
            (Some(left), Some(right)) => Value::BigInt(left.days - right.days),
            _ => Value::Null,
        },
        (TO_DAYS, [date]) => local(date)?.map_or(Value::Null, |local| {
            Value::BigInt(local.days + DAYS_FROM_YEAR_ZERO_TO_2000)
        }),
        (TO_SECONDS, [date]) => local(date)?.map_or(Value::Null, |local| {
            Value::BigInt(
                (local.days + DAYS_FROM_YEAR_ZERO_TO_2000) * 86_400 + local.of_day / MICROS_PER_SECOND,
            )
        }),
        (FROM_DAYS, [number]) => from_days(number),
        (LAST_DAY, [date]) => local(date)?.map_or(Value::Null, |local| {
            let (year, month, _) = datetime::civil_from_days(local.days);
            date_value(datetime::days_from_civil(year, month, datetime::days_in_month(year, month)))
        }),
        (MAKEDATE, [year, day]) => make_date(year, day),
        (MAKETIME, [hour, minute, second]) => make_time(hour, minute, second),
        (MONTHNAME, [date]) => local(date)?.map_or(Value::Null, |local| {
            let (_, month, _) = datetime::civil_from_days(local.days);
            name_at(&MONTH_NAMES, month - 1)
        }),
        (DAYNAME, [date]) => local(date)?.map_or(Value::Null, |local| {
            name_at(&WEEKDAY_NAMES, (local.days + 6).rem_euclid(7))
        }),
        (TIME_TO_SEC, [time]) => time_micros(time)?
            .map_or(Value::Null, |micros| Value::BigInt(micros / MICROS_PER_SECOND)),
        (SEC_TO_TIME, [seconds]) => seconds.as_float().map_or(Value::Null, |seconds| {
            let micros = (seconds * MICROS_PER_SECOND as f64).round();
            let limit = MYSQL_TIME_LIMIT as f64;
            Value::Time(micros.clamp(-limit, limit) as i64)
        }),
        (PERIOD_ADD, [period, months]) => match (period.as_integer(), months.as_integer()) {
            (Some(0), Some(_)) => Value::BigInt(0),
            (Some(period), Some(months)) => period_months(period)
                .and_then(|total| total.checked_add(months))
                .filter(|total| *total >= 0)
                .map_or(Value::Null, |total| {
                    Value::BigInt(total.div_euclid(12) * 100 + total.rem_euclid(12) + 1)
                }),
            _ => Value::Null,
        },
        (PERIOD_DIFF, [left, right]) => match (
            left.as_integer().and_then(period_months),
            right.as_integer().and_then(period_months),
        ) {
            (Some(left), Some(right)) => Value::BigInt(left - right),
            _ => Value::Null,
        },
        _ => Value::Null,
    })
}

fn name_at(names: &[&str], index: i64) -> Value {
    usize::try_from(index)
        .ok()
        .and_then(|index| names.get(index))
        .map_or(Value::Null, |name| Value::Text((*name).to_owned()))
}

fn date_value(days: i64) -> Value {
    i32::try_from(days).map_or(Value::Null, Value::Date)
}

fn local(value: &Value) -> SqlResult<Option<LocalTime>> {
    Ok(match mysql_date_parts::local_moment(value)? {
        Some(Ok(local)) => Some(local),
        _ => None,
    })
}

fn in_mysql_range(local: LocalTime) -> bool {
    let (year, _, _) = datetime::civil_from_days(local.days);
    (0..=9999).contains(&year)
}

fn text_has_clock(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.contains(':') || trimmed.len() > 10
}

fn format_local(local: LocalTime, with_clock: bool) -> String {
    let (year, month, day) = datetime::civil_from_days(local.days);
    let date = format!("{year:04}-{month:02}-{day:02}");
    if !with_clock {
        return date;
    }
    let hour = local.of_day / MICROS_PER_HOUR;
    let minute = local.of_day / MICROS_PER_MINUTE % 60;
    let second = local.of_day / MICROS_PER_SECOND % 60;
    let micro = local.of_day % MICROS_PER_SECOND;
    if micro == 0 {
        format!("{date} {hour:02}:{minute:02}:{second:02}")
    } else {
        format!("{date} {hour:02}:{minute:02}:{second:02}.{micro:06}")
    }
}

#[derive(Clone, Copy, Default)]
struct Shift {
    months: i64,
    micros: i64,
}

fn compound_parts(unit: &str) -> Option<&'static [&'static str]> {
    Some(match unit {
        "second_microsecond" => &["second", "microsecond"],
        "minute_microsecond" => &["minute", "second", "microsecond"],
        "minute_second" => &["minute", "second"],
        "hour_microsecond" => &["hour", "minute", "second", "microsecond"],
        "hour_second" => &["hour", "minute", "second"],
        "hour_minute" => &["hour", "minute"],
        "day_microsecond" => &["day", "hour", "minute", "second", "microsecond"],
        "day_second" => &["day", "hour", "minute", "second"],
        "day_minute" => &["day", "hour", "minute"],
        "day_hour" => &["day", "hour"],
        "year_month" => &["year", "month"],
        _ => return None,
    })
}

fn add_part(shift: &mut Shift, part: &str, amount: i64) -> Option<()> {
    let (months, micros) = match part {
        "microsecond" => (0, amount),
        "second" => (0, amount.checked_mul(MICROS_PER_SECOND)?),
        "minute" => (0, amount.checked_mul(MICROS_PER_MINUTE)?),
        "hour" => (0, amount.checked_mul(MICROS_PER_HOUR)?),
        "day" => (0, amount.checked_mul(MICROS_PER_DAY)?),
        "week" => (0, amount.checked_mul(7 * MICROS_PER_DAY)?),
        "month" => (amount, 0),
        "quarter" => (amount.checked_mul(3)?, 0),
        "year" => (amount.checked_mul(12)?, 0),
        _ => return None,
    };
    shift.months = shift.months.checked_add(months)?;
    shift.micros = shift.micros.checked_add(micros)?;
    Some(())
}

fn leading_integer(text: &str) -> Option<i64> {
    let trimmed = text.trim();
    let (negative, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let end = digits
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(digits.len());
    let number: i64 = digits.get(..end).filter(|digits| !digits.is_empty())?.parse().ok()?;
    Some(if negative { -number } else { number })
}

fn simple_shift(unit: &str, amount: &Value) -> Option<Shift> {
    let mut shift = Shift::default();
    if unit == "second" {
        let seconds = match amount {
            Value::Text(text) => text.trim().parse::<f64>().ok()?,
            other => other.as_float()?,
        };
        let micros = (seconds * MICROS_PER_SECOND as f64).round();
        if !micros.is_finite() || micros.abs() >= i64::MAX as f64 {
            return None;
        }
        shift.micros = micros as i64;
        return Some(shift);
    }
    let whole = match amount {
        Value::Text(text) => leading_integer(text)?,
        other => match other.as_integer() {
            Some(number) => number,
            None => {
                let rounded = other.as_float()?.round();
                if !rounded.is_finite() || rounded.abs() >= i64::MAX as f64 {
                    return None;
                }
                rounded as i64
            }
        },
    };
    add_part(&mut shift, unit, whole)?;
    Some(shift)
}

fn compound_shift(parts: &[&str], amount: &Value) -> Option<Shift> {
    let text = match amount {
        Value::Text(text) => text.trim().to_owned(),
        other => other.to_text().ok()?,
    };
    let negative = text.starts_with('-');
    let runs: Vec<&str> = text
        .split(|character: char| !character.is_ascii_digit())
        .filter(|run| !run.is_empty())
        .collect();
    if runs.is_empty() || runs.len() > parts.len() {
        return None;
    }
    let mut shift = Shift::default();
    let skipped = parts.len() - runs.len();
    for (part, run) in parts[skipped..].iter().zip(&runs) {
        let amount = if *part == "microsecond" {
            let digits: String = run.chars().chain(std::iter::repeat('0')).take(6).collect();
            digits.parse::<i64>().ok()?
        } else {
            run.parse::<i64>().ok()?
        };
        add_part(&mut shift, part, amount)?;
    }
    if negative {
        shift.months = shift.months.checked_neg()?;
        shift.micros = shift.micros.checked_neg()?;
    }
    Some(shift)
}

fn shift_of(unit: &str, amount: &Value) -> Option<Shift> {
    match compound_parts(unit) {
        Some(parts) => compound_shift(parts, amount),
        None => simple_shift(unit, amount),
    }
}

fn shifted(local: LocalTime, shift: Shift) -> Option<LocalTime> {
    let mut days = local.days;
    if shift.months != 0 {
        let (year, month, day) = datetime::civil_from_days(days);
        let total = (year.checked_mul(12)?)
            .checked_add(month - 1)?
            .checked_add(shift.months)?;
        let (year, month) = (total.div_euclid(12), total.rem_euclid(12) + 1);
        if !(0..=9999).contains(&year) {
            return None;
        }
        days = datetime::days_from_civil(year, month, day.min(datetime::days_in_month(year, month)));
    }
    let micros = i128::from(days) * i128::from(MICROS_PER_DAY)
        + i128::from(local.of_day)
        + i128::from(shift.micros);
    let micros = i64::try_from(micros).ok()?;
    let moved = LocalTime::of_micros(micros);
    in_mysql_range(moved).then_some(moved)
}

/// Moves `micros` (microseconds since 2000-01-01) by `amount` of MySQL's interval `unit`, as
/// `DATE_ADD` does; `None` when the amount doesn't read or the result leaves MySQL's range.
pub(crate) fn add_interval(micros: i64, amount: &Value, unit: &str) -> Option<i64> {
    let shift = shift_of(unit, amount)?;
    let moved = shifted(LocalTime::of_micros(micros), shift)?;
    moved
        .days
        .checked_mul(MICROS_PER_DAY)?
        .checked_add(moved.of_day)
}

/// Seconds since 1970-01-01 UTC of a MySQL date or time value read in the session's time zone.
pub(crate) fn epoch_seconds_of(value: &Value) -> SqlResult<Option<i64>> {
    let utc = match value {
        Value::TimestampTz(utc) => *utc,
        other => {
            let Some(local) = local(other)? else {
                return Ok(None);
            };
            let micros = local
                .days
                .saturating_mul(MICROS_PER_DAY)
                .saturating_add(local.of_day);
            datetime::local_to_utc(micros)?
        }
    };
    Ok(Some((utc - unix_epoch_micros()).div_euclid(MICROS_PER_SECOND)))
}

/// Microseconds since 2000-01-01 UTC of `seconds` since 1970-01-01 UTC.
pub(crate) fn micros_of_epoch_seconds(seconds: i64) -> Option<i64> {
    seconds
        .checked_mul(MICROS_PER_SECOND)?
        .checked_add(unix_epoch_micros())
}

/// Seconds since 1970-01-01 UTC of `micros` since 2000-01-01 UTC.
pub(crate) fn epoch_seconds_of_micros(micros: i64) -> i64 {
    (micros - unix_epoch_micros()).div_euclid(MICROS_PER_SECOND)
}

fn date_add(date: &Value, amount: &Value, unit: &str, subtract: bool) -> SqlResult<Value> {
    let Some(local) = local(date)? else {
        return Ok(Value::Null);
    };
    let Some(mut shift) = shift_of(unit, amount) else {
        return Ok(Value::Null);
    };
    if subtract {
        let (Some(months), Some(micros)) = (shift.months.checked_neg(), shift.micros.checked_neg())
        else {
            return Ok(Value::Null);
        };
        shift = Shift { months, micros };
    }
    let Some(moved) = shifted(local, shift) else {
        return Ok(Value::Null);
    };
    let date_unit = DATE_UNITS.contains(&unit) || MONTH_UNITS.contains(&unit);
    Ok(match date {
        Value::Date(_) if date_unit => date_value(moved.days),
        Value::Text(text) => Value::Text(format_local(moved, !date_unit || text_has_clock(text))),
        _ => Value::Timestamp(
            moved
                .days
                .saturating_mul(MICROS_PER_DAY)
                .saturating_add(moved.of_day),
        ),
    })
}

fn months_between(from: LocalTime, to: LocalTime) -> i64 {
    let (from_year, from_month, from_day) = datetime::civil_from_days(from.days);
    let (to_year, to_month, to_day) = datetime::civil_from_days(to.days);
    let mut months = (to_year * 12 + to_month) - (from_year * 12 + from_month);
    let to_rest = (to_day, to.of_day);
    let from_rest = (from_day, from.of_day);
    if months > 0 && to_rest < from_rest {
        months -= 1;
    } else if months < 0 && to_rest > from_rest {
        months += 1;
    }
    months
}

fn timestamp_diff(unit: &str, from: &Value, to: &Value) -> SqlResult<Value> {
    let (Some(from), Some(to)) = (local(from)?, local(to)?) else {
        return Ok(Value::Null);
    };
    let elapsed = i128::from(to.days - from.days) * i128::from(MICROS_PER_DAY)
        + i128::from(to.of_day - from.of_day);
    let per = |size: i64| i64::try_from(elapsed / i128::from(size)).ok();
    let difference = match unit {
        "microsecond" => i64::try_from(elapsed).ok(),
        "second" => per(MICROS_PER_SECOND),
        "minute" => per(MICROS_PER_MINUTE),
        "hour" => per(MICROS_PER_HOUR),
        "day" => per(MICROS_PER_DAY),
        "week" => per(7 * MICROS_PER_DAY),
        "month" => Some(months_between(from, to)),
        "quarter" => Some(months_between(from, to) / 3),
        "year" => Some(months_between(from, to) / 12),
        _ => None,
    };
    Ok(difference.map_or(Value::Null, Value::BigInt))
}

fn unix_epoch_micros() -> i64 {
    datetime::days_from_civil(1970, 1, 1) * MICROS_PER_DAY
}

fn unix_seconds(utc: i64) -> i64 {
    let seconds = (utc - unix_epoch_micros()).div_euclid(MICROS_PER_SECOND);
    if (0..=UNIX_TIMESTAMP_LIMIT).contains(&seconds) {
        seconds
    } else {
        0
    }
}

fn unix_timestamp(date: &Value) -> SqlResult<Value> {
    if let Value::TimestampTz(utc) = date {
        return Ok(Value::BigInt(unix_seconds(*utc)));
    }
    let Some(local) = local(date)? else {
        return Ok(Value::Null);
    };
    let micros = local
        .days
        .saturating_mul(MICROS_PER_DAY)
        .saturating_add(local.of_day);
    Ok(Value::BigInt(unix_seconds(datetime::local_to_utc(micros)?)))
}

fn from_unixtime(seconds: &Value) -> SqlResult<Value> {
    let seconds = match seconds {
        Value::Text(text) => text.trim().parse::<f64>().ok(),
        other => other.as_float(),
    };
    let Some(seconds) = seconds.filter(|seconds| {
        seconds.is_finite() && *seconds >= 0.0 && *seconds < (UNIX_TIMESTAMP_LIMIT + 1) as f64
    }) else {
        return Ok(Value::Null);
    };
    let micros = (seconds * MICROS_PER_SECOND as f64).round() as i64;
    let utc = unix_epoch_micros() + micros;
    Ok(Value::Timestamp(datetime::utc_to_local(utc)?))
}

fn from_days(number: &Value) -> Value {
    match number.as_integer() {
        Some(number) if (366..=LAST_DAY_NUMBER).contains(&number) => {
            date_value(number - DAYS_FROM_YEAR_ZERO_TO_2000)
        }
        _ => Value::Null,
    }
}

fn two_digit_year(year: i64) -> i64 {
    match year {
        0..=69 => 2000 + year,
        70..=99 => 1900 + year,
        other => other,
    }
}

fn make_date(year: &Value, day: &Value) -> Value {
    let (Some(year), Some(day)) = (year.as_integer(), day.as_integer()) else {
        return Value::Null;
    };
    if year < 0 || day <= 0 || year > 9999 {
        return Value::Null;
    }
    let year = two_digit_year(year);
    let days = datetime::days_from_civil(year, 1, 1) + day - 1;
    let (reached, _, _) = datetime::civil_from_days(days);
    if reached > 9999 {
        return Value::Null;
    }
    date_value(days)
}

fn make_time(hour: &Value, minute: &Value, second: &Value) -> Value {
    let (Some(hour), Some(minute), Some(second)) =
        (hour.as_integer(), minute.as_integer(), second.as_float())
    else {
        return Value::Null;
    };
    if !(0..60).contains(&minute) || !(0.0..60.0).contains(&second) {
        return Value::Null;
    }
    let magnitude = i128::from(hour.unsigned_abs()) * i128::from(MICROS_PER_HOUR)
        + i128::from(minute * MICROS_PER_MINUTE)
        + (second * MICROS_PER_SECOND as f64).round() as i128;
    let magnitude = magnitude.min(i128::from(MYSQL_TIME_LIMIT)) as i64;
    Value::Time(if hour < 0 { -magnitude } else { magnitude })
}

fn time_micros(value: &Value) -> SqlResult<Option<i64>> {
    Ok(match value {
        Value::Time(micros) => Some(*micros),
        Value::Timestamp(micros) => Some(micros.rem_euclid(MICROS_PER_DAY)),
        Value::Text(text) => match datetime::parse_mysql_time(text) {
            Ok(micros) => Some(micros),
            Err(_) => local(value)?.map(|local| local.of_day),
        },
        other => match other.as_integer() {
            Some(number) => {
                let negative = number < 0;
                let number = number.unsigned_abs();
                let (hours, minutes, seconds) = (number / 10_000, number / 100 % 100, number % 100);
                if minutes > 59 || seconds > 59 {
                    None
                } else {
                    let micros = i64::try_from(hours * 3600 + minutes * 60 + seconds)
                        .ok()
                        .and_then(|seconds| seconds.checked_mul(MICROS_PER_SECOND));
                    micros.map(|micros| if negative { -micros } else { micros })
                }
            }
            None => local(value)?.map(|local| local.of_day),
        },
    })
}

fn period_months(period: i64) -> Option<i64> {
    if period <= 0 {
        return None;
    }
    let year = two_digit_year(period / 100);
    let month = period % 100;
    if !(1..=12).contains(&month) {
        return None;
    }
    Some(year * 12 + month - 1)
}

#[derive(Default)]
struct Fields {
    year: Option<i64>,
    month: Option<i64>,
    day: Option<i64>,
    day_of_year: Option<i64>,
    hour: i64,
    minute: i64,
    second: i64,
    micro: i64,
    meridiem: Option<bool>,
    twelve_hour: bool,
}

struct Input<'a> {
    rest: &'a str,
}

impl<'a> Input<'a> {
    fn skip_spaces(&mut self) {
        self.rest = self.rest.trim_start();
    }

    fn digits(&mut self, most: usize) -> Option<(i64, usize)> {
        let length = self
            .rest
            .bytes()
            .take(most)
            .take_while(u8::is_ascii_digit)
            .count();
        if length == 0 {
            return None;
        }
        let (digits, rest) = self.rest.split_at(length);
        self.rest = rest;
        Some((digits.parse().ok()?, length))
    }

    fn name_of(&mut self, names: &[&str], abbreviated: bool) -> Option<i64> {
        for (index, name) in names.iter().enumerate() {
            let candidate = if abbreviated { &name[..3] } else { name };
            if self
                .rest
                .get(..candidate.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(candidate))
            {
                self.rest = &self.rest[candidate.len()..];
                return Some(index as i64);
            }
        }
        None
    }

    fn literal(&mut self, character: char) -> Option<()> {
        self.rest = self.rest.strip_prefix(character)?;
        Some(())
    }

    fn meridiem(&mut self) -> Option<bool> {
        for (word, after_noon) in [("AM", false), ("PM", true)] {
            if self
                .rest
                .get(..2)
                .is_some_and(|start| start.eq_ignore_ascii_case(word))
            {
                self.rest = &self.rest[2..];
                return Some(after_noon);
            }
        }
        None
    }
}

fn read_clock(input: &mut Input<'_>, fields: &mut Fields, twelve_hour: bool) -> Option<()> {
    fields.hour = input.digits(2)?.0;
    fields.twelve_hour |= twelve_hour;
    input.literal(':')?;
    fields.minute = input.digits(2)?.0;
    input.literal(':')?;
    fields.second = input.digits(2)?.0;
    if twelve_hour {
        input.skip_spaces();
        fields.meridiem = Some(input.meridiem()?);
    }
    Some(())
}

fn read_specifier(input: &mut Input<'_>, fields: &mut Fields, specifier: char) -> Option<()> {
    match specifier {
        'Y' => {
            let (year, length) = input.digits(4)?;
            fields.year = Some(if length <= 2 { two_digit_year(year) } else { year });
        }
        'y' => fields.year = Some(two_digit_year(input.digits(2)?.0)),
        'm' | 'c' => fields.month = Some(input.digits(2)?.0),
        'M' => fields.month = Some(input.name_of(&MONTH_NAMES, false)? + 1),
        'b' => fields.month = Some(input.name_of(&MONTH_NAMES, true)? + 1),
        'd' | 'e' => fields.day = Some(input.digits(2)?.0),
        'D' => {
            fields.day = Some(input.digits(2)?.0);
            let suffix = input.rest.get(..2)?;
            if !suffix.chars().all(|character| character.is_ascii_alphabetic()) {
                return None;
            }
            input.rest = &input.rest[2..];
        }
        'j' => fields.day_of_year = Some(input.digits(3)?.0),
        'a' => {
            input.name_of(&WEEKDAY_NAMES, true)?;
        }
        'W' => {
            input.name_of(&WEEKDAY_NAMES, false)?;
        }
        'H' | 'k' => fields.hour = input.digits(2)?.0,
        'h' | 'I' | 'l' => {
            fields.hour = input.digits(2)?.0;
            fields.twelve_hour = true;
        }
        'i' => fields.minute = input.digits(2)?.0,
        's' | 'S' => fields.second = input.digits(2)?.0,
        'f' => {
            let (digits, length) = input.digits(6)?;
            fields.micro = digits * 10_i64.pow(u32::try_from(6 - length).ok()?);
        }
        'p' => fields.meridiem = Some(input.meridiem()?),
        'T' => read_clock(input, fields, false)?,
        'r' => read_clock(input, fields, true)?,
        '%' => input.literal('%')?,
        _ => return None,
    }
    Some(())
}

fn read_fields(text: &str, format: &str) -> Option<Fields> {
    let mut fields = Fields::default();
    let mut input = Input { rest: text };
    let mut characters = format.chars();
    while let Some(character) = characters.next() {
        if input.rest.is_empty() {
            break;
        }
        if character == '%' {
            let specifier = characters.next()?;
            input.skip_spaces();
            read_specifier(&mut input, &mut fields, specifier)?;
        } else if !character.is_whitespace() {
            input.literal(character)?;
        }
    }
    Some(fields)
}

fn clock_of(fields: &Fields) -> Option<i64> {
    let mut hour = fields.hour;
    if let Some(after_noon) = fields.meridiem {
        if !(1..=12).contains(&hour) {
            return None;
        }
        hour %= 12;
        if after_noon {
            hour += 12;
        }
    } else if fields.twelve_hour && hour > 12 {
        return None;
    }
    if hour > 23 || fields.minute > 59 || fields.second > 59 {
        return None;
    }
    Some(hour * MICROS_PER_HOUR + fields.minute * MICROS_PER_MINUTE + fields.second * MICROS_PER_SECOND + fields.micro)
}

fn days_of(fields: &Fields) -> Option<i64> {
    let year = fields.year?;
    if !(0..=9999).contains(&year) {
        return None;
    }
    if let Some(day_of_year) = fields.day_of_year
        && fields.month.is_none()
        && fields.day.is_none()
    {
        let length = if datetime::is_leap_year(year) { 366 } else { 365 };
        if !(1..=length).contains(&day_of_year) {
            return None;
        }
        return Some(datetime::days_from_civil(year, 1, 1) + day_of_year - 1);
    }
    let (month, day) = (fields.month?, fields.day?);
    if !(1..=12).contains(&month) || !(1..=datetime::days_in_month(year, month)).contains(&day) {
        return None;
    }
    Some(datetime::days_from_civil(year, month, day))
}

fn str_to_date(name: &str, text: &Value, format: &Value) -> Value {
    let (Value::Text(text), Value::Text(format)) = (text, format) else {
        let (Ok(text), Ok(format)) = (text.to_text(), format.to_text()) else {
            return Value::Null;
        };
        return str_to_date(name, &Value::Text(text), &Value::Text(format));
    };
    let Some(fields) = read_fields(text, format) else {
        return Value::Null;
    };
    let Some(clock) = clock_of(&fields) else {
        return Value::Null;
    };
    match name {
        STR_TO_TIME => Value::Time(clock),
        STR_TO_DATE => days_of(&fields).map_or(Value::Null, date_value),
        _ => days_of(&fields).map_or(Value::Null, |days| {
            Value::Timestamp(days.saturating_mul(MICROS_PER_DAY).saturating_add(clock))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Value {
        Value::Text(value.to_owned())
    }

    fn call(name: &str, values: &[Value]) -> Value {
        function(name, values).expect("a MySQL date function").expect("a value")
    }

    #[test]
    fn date_add_and_date_sub_follow_the_mysql_manual() {
        let unit = |unit: &str| text(unit);
        assert_eq!(
            call(DATE_ADD, &[text("2018-05-01"), Value::Integer(1), unit("day")]),
            text("2018-05-02")
        );
        assert_eq!(
            call(DATE_SUB, &[text("2018-05-01"), Value::Integer(1), unit("year")]),
            text("2017-05-01")
        );
        assert_eq!(
            call(DATE_ADD, &[text("2020-12-31 23:59:59"), Value::Integer(1), unit("second")]),
            text("2021-01-01 00:00:00")
        );
        assert_eq!(
            call(DATE_ADD, &[text("2018-12-31 23:59:59"), text("1:1"), unit("minute_second")]),
            text("2019-01-01 00:01:00")
        );
        assert_eq!(
            call(DATE_SUB, &[text("2025-01-01 00:00:00"), text("1 1:1:1"), unit("day_second")]),
            text("2024-12-30 22:58:59")
        );
        assert_eq!(
            call(DATE_ADD, &[text("1900-01-01 00:00:00"), text("-1 10"), unit("day_hour")]),
            text("1899-12-30 14:00:00")
        );
        assert_eq!(
            call(DATE_ADD, &[text("1992-12-31 23:59:59.000002"), text("1.999999"), unit("second_microsecond")]),
            text("1993-01-01 00:00:01.000001")
        );
        assert_eq!(
            call(DATE_ADD, &[text("2024-01-31"), Value::Integer(1), unit("month")]),
            text("2024-02-29")
        );
        assert_eq!(
            call(DATE_ADD, &[text("9999-12-31"), Value::Integer(1), unit("day")]),
            Value::Null
        );
    }

    #[test]
    fn timestampdiff_counts_whole_units() {
        assert_eq!(
            call(TIMESTAMPDIFF, &[text("month"), text("2003-02-01"), text("2003-05-01")]),
            Value::BigInt(3)
        );
        assert_eq!(
            call(TIMESTAMPDIFF, &[text("year"), text("2002-05-01"), text("2001-01-01")]),
            Value::BigInt(-1)
        );
        assert_eq!(
            call(TIMESTAMPDIFF, &[text("minute"), text("2003-02-01"), text("2003-05-01 12:05:55")]),
            Value::BigInt(128_885)
        );
    }

    #[test]
    fn str_to_date_reads_the_format() {
        assert_eq!(
            call(STR_TO_DATETIME, &[text("01,5,2013"), text("%d,%m,%Y")]),
            Value::Timestamp(datetime::days_from_civil(2013, 5, 1) * MICROS_PER_DAY)
        );
        assert_eq!(
            call(STR_TO_DATE, &[text("May 1, 2013"), text("%M %d,%Y")]),
            Value::Date(datetime::days_from_civil(2013, 5, 1) as i32)
        );
        assert_eq!(
            call(STR_TO_TIME, &[text("a09:30:17"), text("a%h:%i:%s")]),
            Value::Time(9 * MICROS_PER_HOUR + 30 * MICROS_PER_MINUTE + 17 * MICROS_PER_SECOND)
        );
        assert_eq!(call(STR_TO_TIME, &[text("09:30:17a"), text("%h:%i:%s")]), Value::Time(
            9 * MICROS_PER_HOUR + 30 * MICROS_PER_MINUTE + 17 * MICROS_PER_SECOND
        ));
        assert_eq!(call(STR_TO_DATE, &[text("a09:30:17"), text("%h:%i:%s")]), Value::Null);
        assert_eq!(str_to_date_function("%Y-%m-%d"), STR_TO_DATE);
        assert_eq!(str_to_date_function("%H:%i"), STR_TO_TIME);
        assert_eq!(str_to_date_function("%Y %H"), STR_TO_DATETIME);
    }

    #[test]
    fn day_numbers_and_periods_follow_the_mysql_manual() {
        assert_eq!(call(TO_DAYS, &[Value::Integer(950_501)]), Value::BigInt(728_779));
        assert_eq!(call(TO_DAYS, &[text("2007-10-07")]), Value::BigInt(733_321));
        assert_eq!(
            call(FROM_DAYS, &[Value::Integer(730_669)]),
            Value::Date(datetime::days_from_civil(2000, 7, 3) as i32)
        );
        assert_eq!(call(TO_SECONDS, &[Value::Integer(950_501)]), Value::BigInt(62_966_505_600));
        assert_eq!(call(DATEDIFF, &[text("2007-12-31 23:59:59"), text("2007-12-30")]), Value::BigInt(1));
        assert_eq!(call(DATEDIFF, &[text("2010-11-30 23:59:59"), text("2010-12-31")]), Value::BigInt(-31));
        assert_eq!(call(PERIOD_ADD, &[Value::Integer(200_801), Value::Integer(2)]), Value::BigInt(200_803));
        assert_eq!(call(PERIOD_DIFF, &[Value::Integer(200_802), Value::Integer(200_703)]), Value::BigInt(11));
        assert_eq!(
            call(LAST_DAY, &[text("2004-02-05")]),
            Value::Date(datetime::days_from_civil(2004, 2, 29) as i32)
        );
        assert_eq!(
            call(MAKEDATE, &[Value::Integer(2011), Value::Integer(32)]),
            Value::Date(datetime::days_from_civil(2011, 2, 1) as i32)
        );
        assert_eq!(call(MAKEDATE, &[Value::Integer(2011), Value::Integer(0)]), Value::Null);
        assert_eq!(call(MONTHNAME, &[text("2008-02-03")]), text("February"));
        assert_eq!(call(DAYNAME, &[text("2007-02-03")]), text("Saturday"));
    }

    #[test]
    fn clock_functions_follow_the_mysql_manual() {
        assert_eq!(call(TIME_TO_SEC, &[text("22:23:00")]), Value::BigInt(80_580));
        assert_eq!(call(TIME_TO_SEC, &[text("00:39:38")]), Value::BigInt(2_378));
        assert_eq!(
            call(SEC_TO_TIME, &[Value::Integer(2_378)]),
            Value::Time(39 * MICROS_PER_MINUTE + 38 * MICROS_PER_SECOND)
        );
        assert_eq!(
            call(MAKETIME, &[Value::Integer(12), Value::Integer(15), Value::Integer(30)]),
            Value::Time(12 * MICROS_PER_HOUR + 15 * MICROS_PER_MINUTE + 30 * MICROS_PER_SECOND)
        );
        assert_eq!(call(MAKETIME, &[Value::Integer(1), Value::Integer(60), Value::Integer(0)]), Value::Null);
        assert_eq!(call(FROM_UNIXTIME, &[Value::Integer(-1)]), Value::Null);
    }
}
