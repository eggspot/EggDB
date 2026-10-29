use crate::ast::BinaryOperator;
use crate::datetime::{self, MICROS_PER_DAY, MICROS_PER_SECOND};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::interval::{self, Interval};
use crate::types::{SqlType, Value};

pub(crate) fn is_time_or_interval(sql_type: SqlType) -> bool {
    matches!(
        sql_type,
        SqlType::Time(_) | SqlType::TimeTz(_) | SqlType::Interval { .. }
    )
}

fn shifted(value: &Value, by: Interval) -> Option<SqlResult<Value>> {
    Some(match value {
        Value::Timestamp(micros) => interval::add_to_timestamp(*micros, by).map(Value::Timestamp),
        Value::Date(days) => datetime::date_to_timestamp(*days)
            .and_then(|micros| interval::add_to_timestamp(micros, by))
            .map(Value::Timestamp),
        Value::TimestampTz(micros) => datetime::utc_to_local(*micros)
            .and_then(|local| interval::add_to_timestamp(local, by))
            .and_then(datetime::local_to_utc)
            .map(Value::TimestampTz),
        Value::Time(micros) => Ok(Value::Time(datetime::wrap_time(
            i128::from(*micros) + i128::from(by.micros),
        ))),
        Value::TimeTz {
            micros,
            offset_seconds,
        } => Ok(Value::TimeTz {
            micros: datetime::wrap_time(i128::from(*micros) + i128::from(by.micros)),
            offset_seconds: *offset_seconds,
        }),
        _ => return None,
    })
}

fn instant(value: &Value, zoned: bool) -> Option<SqlResult<i64>> {
    let local = match value {
        Value::Date(days) => datetime::date_to_timestamp(*days),
        Value::Timestamp(micros) => Ok(*micros),
        Value::TimestampTz(micros) => return Some(Ok(*micros)),
        _ => return None,
    };
    Some(if zoned {
        local.and_then(datetime::local_to_utc)
    } else {
        local
    })
}

fn difference(left: &Value, right: &Value) -> Option<SqlResult<Value>> {
    if matches!((left, right), (Value::Date(_), Value::Date(_))) {
        return None;
    }
    let zoned = matches!(left, Value::TimestampTz(_)) || matches!(right, Value::TimestampTz(_));
    let later = instant(left, zoned)?;
    let earlier = instant(right, zoned)?;
    Some(later.and_then(|later| {
        earlier
            .and_then(|earlier| interval::between_timestamps(later, earlier).map(Value::Interval))
    }))
}

fn factor(value: &Value) -> Option<f64> {
    match value {
        Value::Boolean(_) => None,
        other => other.as_float(),
    }
}

pub(crate) fn arithmetic(
    operator: BinaryOperator,
    left: &Value,
    right: &Value,
) -> Option<SqlResult<Value>> {
    Some(match (operator, left, right) {
        (BinaryOperator::Add, Value::Interval(a), Value::Interval(b)) => {
            a.checked_add(*b).map(Value::Interval)
        }
        (BinaryOperator::Subtract, Value::Interval(a), Value::Interval(b)) => {
            a.checked_sub(*b).map(Value::Interval)
        }
        (BinaryOperator::Multiply, Value::Interval(a), other)
        | (BinaryOperator::Multiply, other, Value::Interval(a)) => {
            a.multiply(factor(other)?).map(Value::Interval)
        }
        (BinaryOperator::Divide, Value::Interval(a), other) => {
            a.divide(factor(other)?).map(Value::Interval)
        }
        (BinaryOperator::Add, Value::Interval(by), other)
        | (BinaryOperator::Add, other, Value::Interval(by)) => shifted(other, *by)?,
        (BinaryOperator::Subtract, other, Value::Interval(by)) => match by.negated() {
            Ok(back) => shifted(other, back)?,
            Err(error) => Err(error),
        },
        (BinaryOperator::Add, Value::Date(days), Value::Time(micros))
        | (BinaryOperator::Add, Value::Time(micros), Value::Date(days)) => {
            datetime::checked_timestamp(
                i128::from(*days) * i128::from(MICROS_PER_DAY) + i128::from(*micros),
            )
            .map(Value::Timestamp)
        }
        (BinaryOperator::Subtract, Value::Time(a), Value::Time(b)) => {
            Ok(Value::Interval(Interval {
                months: 0,
                days: 0,
                micros: a - b,
            }))
        }
        (BinaryOperator::Subtract, _, _) => difference(left, right)?,
        _ => return None,
    })
}

pub(crate) fn arithmetic_type(
    operator: BinaryOperator,
    left: SqlType,
    right: SqlType,
) -> Option<SqlType> {
    let interval = |sql_type| matches!(sql_type, SqlType::Interval { .. });
    let shifted_type = |other: SqlType| match other {
        SqlType::Date | SqlType::Timestamp(_) => Some(SqlType::Timestamp(None)),
        SqlType::TimestampTz(_) => Some(SqlType::TimestampTz(None)),
        SqlType::Time(_) => Some(SqlType::Time(None)),
        SqlType::TimeTz(_) => Some(SqlType::TimeTz(None)),
        _ => None,
    };
    match (operator, left, right) {
        (BinaryOperator::Add | BinaryOperator::Subtract, a, b) if interval(a) && interval(b) => {
            Some(SqlType::INTERVAL)
        }
        (BinaryOperator::Multiply, a, b) if interval(a) && b.is_numeric() => {
            Some(SqlType::INTERVAL)
        }
        (BinaryOperator::Multiply, a, b) if a.is_numeric() && interval(b) => {
            Some(SqlType::INTERVAL)
        }
        (BinaryOperator::Divide, a, b) if interval(a) && b.is_numeric() => Some(SqlType::INTERVAL),
        (BinaryOperator::Add, a, b) if interval(a) => shifted_type(b),
        (BinaryOperator::Add | BinaryOperator::Subtract, a, b) if interval(b) => shifted_type(a),
        (BinaryOperator::Add, SqlType::Date, SqlType::Time(_))
        | (BinaryOperator::Add, SqlType::Time(_), SqlType::Date) => Some(SqlType::Timestamp(None)),
        (BinaryOperator::Subtract, SqlType::Time(_), SqlType::Time(_)) => Some(SqlType::INTERVAL),
        (BinaryOperator::Subtract, SqlType::Date, SqlType::Date) => None,
        (BinaryOperator::Subtract, a, b) if a.is_datetime() && b.is_datetime() => {
            Some(SqlType::INTERVAL)
        }
        _ => None,
    }
}

fn local_now() -> SqlResult<i64> {
    datetime::utc_to_local(datetime::statement_now())
}

fn age_instant(value: &Value) -> Option<SqlResult<i64>> {
    Some(match value {
        Value::Date(days) => datetime::date_to_timestamp(*days),
        Value::Timestamp(micros) => Ok(*micros),
        Value::TimestampTz(micros) => datetime::utc_to_local(*micros),
        _ => return None,
    })
}

pub(crate) const CONSTRUCTORS: [&str; 8] = [
    "make_date",
    "make_time",
    "make_timestamp",
    "make_timestamptz",
    "make_interval",
    "date_bin",
    "isfinite",
    "timezone",
];

fn date_field_error(year: i64, month: i64, day: i64) -> SqlError {
    SqlError::new(
        SqlState::DATETIME_FIELD_OVERFLOW,
        format!("date field value out of range: {year}-{month:02}-{day:02}"),
    )
}

fn time_field_error(hour: i64, minute: i64, seconds: f64) -> SqlError {
    SqlError::new(
        SqlState::DATETIME_FIELD_OVERFLOW,
        format!("time field value out of range: {hour}:{minute:02}:{seconds:02}"),
    )
}

fn made_date(year: i64, month: i64, day: i64) -> SqlResult<i64> {
    let astronomical = match year {
        0 => return Err(date_field_error(year, month, day)),
        year if year < 0 => year + 1,
        year => year,
    };
    if !(1..=12).contains(&month)
        || day < 1
        || day > datetime::days_in_month(astronomical, month)
        || !(-4713..=5_874_897).contains(&year)
    {
        return Err(date_field_error(year, month, day));
    }
    Ok(datetime::days_from_civil(astronomical, month, day))
}

fn made_time(hour: i64, minute: i64, seconds: f64) -> SqlResult<i64> {
    let fits = (0..=23).contains(&hour) && (0..=59).contains(&minute) && (0.0..60.0).contains(&seconds);
    let midnight = hour == 24 && minute == 0 && seconds == 0.0;
    if !(fits || midnight) {
        return Err(time_field_error(hour, minute, seconds));
    }
    let micros = (seconds * 1_000_000.0).round() as i64;
    Ok(hour * 3600 * MICROS_PER_SECOND + minute * 60 * MICROS_PER_SECOND + micros)
}

fn made_local(values: &[Value]) -> Option<SqlResult<i64>> {
    let integer = |at: usize| values.get(at).and_then(Value::as_integer);
    let (year, month, day) = (integer(0)?, integer(1)?, integer(2)?);
    let (hour, minute) = (integer(3)?, integer(4)?);
    let seconds = values.get(5)?.as_float()?;
    Some(made_date(year, month, day).and_then(|days| {
        made_time(hour, minute, seconds).and_then(|of_day| {
            datetime::checked_timestamp(
                i128::from(days) * i128::from(MICROS_PER_DAY) + i128::from(of_day),
            )
        })
    }))
}

fn zone_offset(zone: &Value) -> SqlResult<i32> {
    match zone {
        Value::Interval(interval) => {
            if interval.months != 0 || interval.days != 0 {
                return Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("interval time zone \"{interval}\" must not include months or days"),
                ));
            }
            i32::try_from(interval.micros / MICROS_PER_SECOND)
                .ok()
                .filter(|offset| datetime::valid_offset(*offset))
                .ok_or_else(|| {
                    SqlError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("interval time zone \"{interval}\" is out of range"),
                    )
                })
        }
        other => {
            let name = other.character_text().unwrap_or_default();
            datetime::time_zone_offset(name).ok_or_else(|| {
                SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("time zone \"{name}\" not recognized"),
                )
            })
        }
    }
}

fn at_time_zone(zone: &Value, value: &Value) -> Option<SqlResult<Value>> {
    let offset = match zone_offset(zone) {
        Ok(offset) => offset,
        Err(error) => return Some(Err(error)),
    };
    let east = i128::from(offset) * i128::from(MICROS_PER_SECOND);
    let wrapped = |micros: i64, from_offset: i32| {
        datetime::wrap_time(
            i128::from(micros) - i128::from(from_offset) * i128::from(MICROS_PER_SECOND) + east,
        )
    };
    Some(match value {
        Value::Timestamp(local) => {
            datetime::checked_timestamp(i128::from(*local) - east).map(Value::TimestampTz)
        }
        Value::TimestampTz(utc) => {
            datetime::checked_timestamp(i128::from(*utc) + east).map(Value::Timestamp)
        }
        Value::Date(days) => datetime::date_to_timestamp(*days)
            .and_then(datetime::local_to_utc)
            .and_then(|utc| datetime::checked_timestamp(i128::from(utc) + east))
            .map(Value::Timestamp),
        Value::Text(text) => datetime::parse_timestamp(text, true)
            .and_then(|utc| datetime::checked_timestamp(i128::from(utc) + east))
            .map(Value::Timestamp),
        Value::TimeTz {
            micros,
            offset_seconds,
        } => Ok(Value::TimeTz {
            micros: wrapped(*micros, *offset_seconds),
            offset_seconds: offset,
        }),
        Value::Time(micros) => Ok(Value::TimeTz {
            micros: wrapped(*micros, datetime::session_offset()),
            offset_seconds: offset,
        }),
        _ => return None,
    })
}

fn binned_instant(value: &Value, zoned: Option<bool>) -> Option<SqlResult<(i64, bool)>> {
    Some(match value {
        Value::Timestamp(micros) => Ok((*micros, false)),
        Value::TimestampTz(micros) => Ok((*micros, true)),
        Value::Date(days) => datetime::date_to_timestamp(*days).map(|micros| (micros, false)),
        Value::Text(text) => {
            let zoned = zoned.unwrap_or(false);
            datetime::parse_timestamp(text, zoned).map(|micros| (micros, zoned))
        }
        _ => return None,
    })
}

fn date_bin(stride: &Interval, source: &Value, origin: &Value) -> Option<SqlResult<Value>> {
    if stride.months != 0 {
        return Some(Err(SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "timestamps cannot be binned into intervals containing months or years",
        )));
    }
    let step = i128::from(stride.days) * i128::from(MICROS_PER_DAY) + i128::from(stride.micros);
    if step <= 0 {
        return Some(Err(SqlError::new(
            SqlState::DATETIME_FIELD_OVERFLOW,
            "stride must be greater than zero",
        )));
    }
    let zoned_hint = match (source, origin) {
        (Value::TimestampTz(_), _) | (_, Value::TimestampTz(_)) => Some(true),
        _ => None,
    };
    let source = match binned_instant(source, zoned_hint)? {
        Ok(found) => found,
        Err(error) => return Some(Err(error)),
    };
    let origin = match binned_instant(origin, Some(source.1))? {
        Ok(found) => found,
        Err(error) => return Some(Err(error)),
    };
    let distance = i128::from(source.0) - i128::from(origin.0);
    let binned = i128::from(origin.0) + distance.div_euclid(step) * step;
    Some(datetime::checked_timestamp(binned).map(|micros| {
        if source.1 {
            Value::TimestampTz(micros)
        } else {
            Value::Timestamp(micros)
        }
    }))
}

fn made_interval(values: &[Value]) -> Option<SqlResult<Value>> {
    let integer = |at: usize| values.get(at).map_or(Some(0), Value::as_integer);
    let (years, months, weeks, days) = (integer(0)?, integer(1)?, integer(2)?, integer(3)?);
    let (hours, minutes) = (integer(4)?, integer(5)?);
    let seconds = values.get(6).map_or(Some(0.0), Value::as_float)?;
    let total_months = i128::from(years) * 12 + i128::from(months);
    let total_days = i128::from(weeks) * 7 + i128::from(days);
    let micros = i128::from(hours) * i128::from(3600 * MICROS_PER_SECOND)
        + i128::from(minutes) * i128::from(60 * MICROS_PER_SECOND);
    let fraction = (seconds * 1_000_000.0).round();
    if !fraction.is_finite() || fraction.abs() > 9.2e18 {
        return Some(Err(interval::out_of_range()));
    }
    let micros = micros + fraction as i128;
    Some(
        match (
            i32::try_from(total_months),
            i32::try_from(total_days),
            i64::try_from(micros),
        ) {
            (Ok(months), Ok(days), Ok(micros)) => Ok(Value::Interval(Interval {
                months,
                days,
                micros,
            })),
            _ => Err(interval::out_of_range()),
        },
    )
}

fn constructed(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !CONSTRUCTORS.contains(&name) {
        return None;
    }
    if values.iter().any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    let integer = |at: usize| values.get(at).and_then(Value::as_integer);
    Some(match (name, values) {
        ("make_date", [_, _, _]) => made_date(integer(0)?, integer(1)?, integer(2)?)
            .and_then(datetime::checked_date)
            .map(Value::Date),
        ("make_time", [_, _, seconds]) => {
            made_time(integer(0)?, integer(1)?, seconds.as_float()?).map(Value::Time)
        }
        ("make_timestamp", [_, _, _, _, _, _]) => made_local(values)?.map(Value::Timestamp),
        ("make_timestamptz", [_, _, _, _, _, _]) => made_local(values)?
            .and_then(datetime::local_to_utc)
            .map(Value::TimestampTz),
        ("make_timestamptz", [_, _, _, _, _, _, zone]) => {
            let local = made_local(values)?;
            local.and_then(|local| {
                let offset = zone_offset(zone)?;
                datetime::checked_timestamp(
                    i128::from(local) - i128::from(offset) * i128::from(MICROS_PER_SECOND),
                )
                .map(Value::TimestampTz)
            })
        }
        ("make_interval", _) if values.len() <= 7 => made_interval(values)?,
        ("date_bin", [Value::Interval(stride), source, origin]) => {
            date_bin(stride, source, origin)?
        }
        ("isfinite", [Value::Date(_) | Value::Timestamp(_) | Value::TimestampTz(_)]) => {
            Ok(Value::Boolean(true))
        }
        ("isfinite", [Value::Interval(_)]) => Ok(Value::Boolean(true)),
        ("timezone", [zone, value]) => at_time_zone(zone, value)?,
        _ => return None,
    })
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if let Some(result) = constructed(name, values) {
        return Some(result);
    }
    let justified = |interval: &Interval| match name {
        "justify_days" => interval.justify_days(),
        "justify_hours" => interval.justify_hours(),
        _ => interval.justify_interval(),
    };
    Some(match (name, values) {
        ("justify_days" | "justify_hours" | "justify_interval", [Value::Null]) => Ok(Value::Null),
        ("justify_days" | "justify_hours" | "justify_interval", [Value::Interval(interval)]) => {
            justified(interval).map(Value::Interval)
        }
        ("age", [Value::Null] | [Value::Null, _] | [_, Value::Null]) => Ok(Value::Null),
        ("age", [value]) => {
            let then = age_instant(value)?;
            local_now()
                .map(|now| now - now.rem_euclid(MICROS_PER_DAY))
                .and_then(|midnight| then.and_then(|then| interval::age(midnight, then)))
                .map(Value::Interval)
        }
        ("age", [first, second]) => {
            let first = age_instant(first)?;
            let second = age_instant(second)?;
            first
                .and_then(|first| second.and_then(|second| interval::age(first, second)))
                .map(Value::Interval)
        }
        ("current_time", []) => local_now().map(|local| Value::TimeTz {
            micros: datetime::time_of_timestamp(local),
            offset_seconds: datetime::session_offset(),
        }),
        ("localtime", []) => {
            local_now().map(|local| Value::Time(datetime::time_of_timestamp(local)))
        }
        ("extract" | "date_part", [Value::Text(field), value]) => {
            let extracted = match value {
                Value::Interval(interval) => interval.extract(field),
                Value::Time(micros) => datetime::extract_time(field, *micros, None),
                Value::TimeTz {
                    micros,
                    offset_seconds,
                } => datetime::extract_time(field, *micros, Some(*offset_seconds)),
                _ => return None,
            };
            if name == "extract" {
                extracted.map(Value::Numeric)
            } else {
                extracted.map(|number| Value::Double(number.to_f64()))
            }
        }
        _ => return None,
    })
}
