use crate::ast::BinaryOperator;
use crate::datetime::{self, MICROS_PER_DAY, MICROS_PER_SECOND};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::interval::{self, Interval};
use crate::time_zones::TimeZone;
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
        Value::TimestampTz(micros) => shifted_instant(*micros, by).map(Value::TimestampTz),
        Value::Time(_) | Value::TimeTz { .. } if !by.is_finite() => Err(SqlError::new(
            SqlState::DATETIME_FIELD_OVERFLOW,
            "cannot add infinite interval to time",
        )),
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

pub(crate) fn shifted_instant(utc: i64, by: Interval) -> SqlResult<i64> {
    if !by.is_finite() {
        return Err(datetime::timestamp_out_of_range());
    }
    let moved = if by.months == 0 && by.days == 0 {
        utc
    } else {
        let calendar = Interval {
            months: by.months,
            days: by.days,
            micros: 0,
        };
        datetime::utc_to_local(utc)
            .and_then(|local| interval::add_to_timestamp(local, calendar))
            .and_then(datetime::local_to_utc)?
    };
    datetime::checked_timestamp(i128::from(moved) + i128::from(by.micros))
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

pub(crate) fn infinity_sign(value: &Value) -> Option<bool> {
    match value {
        Value::Date(days) if datetime::is_infinite_date(*days) => Some(*days > 0),
        Value::Timestamp(micros) | Value::TimestampTz(micros)
            if datetime::is_infinite_timestamp(*micros) =>
        {
            Some(*micros > 0)
        }
        _ => None,
    }
}

fn infinite_timestamp(positive: bool) -> i64 {
    if positive {
        datetime::INFINITE_TIMESTAMP
    } else {
        datetime::NEGATIVE_INFINITE_TIMESTAMP
    }
}

fn interval_sign(by: Interval) -> Option<bool> {
    if by == Interval::INFINITY {
        Some(true)
    } else if by == Interval::NEG_INFINITY {
        Some(false)
    } else {
        None
    }
}

fn infinite_shift(value: &Value, by: Interval) -> Option<SqlResult<Value>> {
    let zoned = match value {
        Value::Date(_) | Value::Timestamp(_) => false,
        Value::TimestampTz(_) => true,
        _ => return None,
    };
    let positive = match (infinity_sign(value), interval_sign(by)) {
        (None, None) => return None,
        (Some(base), Some(shift)) if base != shift => {
            return Some(Err(datetime::timestamp_out_of_range()));
        }
        (Some(positive), _) | (None, Some(positive)) => positive,
    };
    let micros = infinite_timestamp(positive);
    Some(Ok(if zoned {
        Value::TimestampTz(micros)
    } else {
        Value::Timestamp(micros)
    }))
}

fn infinite_difference(left: &Value, right: &Value) -> Option<SqlResult<Value>> {
    if !matches!(
        left,
        Value::Date(_) | Value::Timestamp(_) | Value::TimestampTz(_)
    ) || !matches!(
        right,
        Value::Date(_) | Value::Timestamp(_) | Value::TimestampTz(_)
    ) {
        return None;
    }
    let signs = (infinity_sign(left), infinity_sign(right));
    if signs == (None, None) {
        return None;
    }
    if matches!((left, right), (Value::Date(_), Value::Date(_))) {
        return Some(Err(infinite_dates_subtracted()));
    }
    infinite_span(signs.0, signs.1)
}

fn infinite_span(later: Option<bool>, earlier: Option<bool>) -> Option<SqlResult<Value>> {
    let positive = match (later, earlier) {
        (Some(later), Some(earlier)) if later == earlier => {
            return Some(Err(SqlError::new(
                SqlState::DATETIME_FIELD_OVERFLOW,
                "interval out of range",
            )));
        }
        (Some(positive), _) => positive,
        (None, Some(positive)) => !positive,
        (None, None) => return None,
    };
    Some(Ok(Value::Interval(if positive {
        Interval::INFINITY
    } else {
        Interval::NEG_INFINITY
    })))
}

fn infinite_dates_subtracted() -> SqlError {
    SqlError::new(
        SqlState::DATETIME_FIELD_OVERFLOW,
        "cannot subtract infinite dates",
    )
}

fn day_count(value: &Value) -> Option<i64> {
    match value {
        Value::Boolean(_) => None,
        other => other.as_integer(),
    }
}

fn infinite_arithmetic(
    operator: BinaryOperator,
    left: &Value,
    right: &Value,
) -> Option<SqlResult<Value>> {
    match (operator, left, right) {
        (BinaryOperator::Add | BinaryOperator::Subtract, Value::Date(days), other)
            if datetime::is_infinite_date(*days) && day_count(other).is_some() =>
        {
            Some(Ok(Value::Date(*days)))
        }
        (BinaryOperator::Add, other, Value::Date(days))
            if datetime::is_infinite_date(*days) && day_count(other).is_some() =>
        {
            Some(Ok(Value::Date(*days)))
        }
        (BinaryOperator::Add, Value::Date(days), Value::Time(_))
        | (BinaryOperator::Add, Value::Time(_), Value::Date(days))
            if datetime::is_infinite_date(*days) =>
        {
            Some(datetime::date_to_timestamp(*days).map(Value::Timestamp))
        }
        (BinaryOperator::Add, Value::Interval(by), other)
        | (BinaryOperator::Add, other, Value::Interval(by)) => infinite_shift(other, *by),
        (BinaryOperator::Subtract, other, Value::Interval(by)) => {
            let back = match by.negated() {
                Ok(back) => back,
                Err(error) => return Some(Err(error)),
            };
            infinite_shift(other, back)
        }
        (BinaryOperator::Subtract, _, _) => infinite_difference(left, right),
        _ => None,
    }
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
    if let Some(result) = infinite_arithmetic(operator, left, right) {
        return Some(result);
    }
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

pub(crate) const CONSTRUCTORS: [&str; 9] = [
    "time_bucket",
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
    let fits =
        (0..=23).contains(&hour) && (0..=59).contains(&minute) && (0.0..60.0).contains(&seconds);
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

fn zone_named(zone: &Value) -> SqlResult<TimeZone> {
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
                .and_then(TimeZone::fixed)
                .ok_or_else(|| {
                    SqlError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("interval time zone \"{interval}\" is out of range"),
                    )
                })
        }
        other => {
            let name = other.character_text().unwrap_or_default();
            TimeZone::abbreviation_or_named(name).ok_or_else(|| {
                SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("time zone \"{name}\" not recognized"),
                )
            })
        }
    }
}

fn at_time_zone(zone: &Value, value: &Value) -> Option<SqlResult<Value>> {
    let zone = match zone_named(zone) {
        Ok(zone) => zone,
        Err(error) => return Some(Err(error)),
    };
    let in_zone = |utc: i64| {
        if datetime::is_infinite_timestamp(utc) {
            return Ok(utc);
        }
        datetime::checked_timestamp(zone.local_of_utc(utc))
    };
    let offset_now = zone.offset_at(datetime::statement_now());
    let wrapped = |micros: i64, from_offset: i32| {
        datetime::wrap_time(
            i128::from(micros)
                + i128::from(offset_now - from_offset) * i128::from(MICROS_PER_SECOND),
        )
    };
    Some(match value {
        Value::Timestamp(local) if datetime::is_infinite_timestamp(*local) => {
            Ok(Value::TimestampTz(*local))
        }
        Value::Timestamp(local) => {
            datetime::checked_timestamp(zone.utc_of_local(*local)).map(Value::TimestampTz)
        }
        Value::TimestampTz(utc) => in_zone(*utc).map(Value::Timestamp),
        Value::Date(days) => datetime::date_to_timestamp(*days)
            .and_then(datetime::local_to_utc)
            .and_then(in_zone)
            .map(Value::Timestamp),
        Value::Text(text) => datetime::parse_timestamp(text, true)
            .and_then(in_zone)
            .map(Value::Timestamp),
        Value::TimeTz {
            micros,
            offset_seconds,
        } => Ok(Value::TimeTz {
            micros: wrapped(*micros, *offset_seconds),
            offset_seconds: offset_now,
        }),
        Value::Time(micros) => Ok(Value::TimeTz {
            micros: wrapped(*micros, datetime::session_offset_now()),
            offset_seconds: offset_now,
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
    if datetime::is_infinite_timestamp(source.0) {
        return Some(Ok(binned_value(source.0, source.1)));
    }
    let origin = match binned_instant(origin, Some(source.1))? {
        Ok(found) => found,
        Err(error) => return Some(Err(error)),
    };
    if datetime::is_infinite_timestamp(origin.0) {
        return Some(Err(SqlError::new(
            SqlState::DATETIME_FIELD_OVERFLOW,
            "origin out of range",
        )));
    }
    let distance = i128::from(source.0) - i128::from(origin.0);
    let binned = i128::from(origin.0) + distance.div_euclid(step) * step;
    Some(datetime::checked_timestamp(binned).map(|micros| binned_value(micros, source.1)))
}

fn binned_value(micros: i64, zoned: bool) -> Value {
    if zoned {
        Value::TimestampTz(micros)
    } else {
        Value::Timestamp(micros)
    }
}

const TIME_BUCKET_ORIGIN_DAYS: i64 = 2;

fn bucket_error(message: &str) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

fn time_bucket(
    width: &Interval,
    source: &Value,
    origin: Option<&Value>,
) -> Option<SqlResult<Value>> {
    if matches!(source, Value::Null) || matches!(origin, Some(Value::Null)) {
        return Some(Ok(Value::Null));
    }
    let date = matches!(source, Value::Date(_));
    let (micros, zoned) = match binned_instant(source, None)? {
        Ok(found) => found,
        Err(error) => return Some(Err(error)),
    };
    if datetime::is_infinite_timestamp(micros) {
        return Some(Ok(source.clone()));
    }
    let (origin_micros, offset_micros) = match origin {
        None => (None, 0i128),
        Some(Value::Interval(offset)) if offset.months == 0 => (
            None,
            i128::from(offset.days) * i128::from(MICROS_PER_DAY) + i128::from(offset.micros),
        ),
        Some(Value::Interval(_)) => {
            return Some(Err(bucket_error(
                "time_bucket offsets cannot contain months or years",
            )));
        }
        Some(other) => match binned_instant(other, Some(zoned))? {
            Ok((found, _)) => (Some(found), 0),
            Err(error) => return Some(Err(error)),
        },
    };
    let bucketed = if width.months != 0 {
        if width.days != 0 || width.micros != 0 || width.months < 0 {
            return Some(Err(bucket_error(
                "month intervals cannot have day or time component",
            )));
        }
        let origin_days = origin_micros.map_or(0, datetime::timestamp_to_date);
        let (origin_year, origin_month, _) = datetime::civil_from_days(i64::from(origin_days));
        let shifted = i128::from(micros) - offset_micros;
        let (year, month, _) =
            datetime::civil_from_days(shifted.div_euclid(i128::from(MICROS_PER_DAY)) as i64);
        let index = (year * 12 + month - 1) - (origin_year * 12 + origin_month - 1);
        let start = index.div_euclid(i64::from(width.months)) * i64::from(width.months)
            + origin_year * 12
            + origin_month
            - 1;
        let days = datetime::days_from_civil(start.div_euclid(12), start.rem_euclid(12) + 1, 1);
        i128::from(days) * i128::from(MICROS_PER_DAY) + offset_micros
    } else {
        let step = i128::from(width.days) * i128::from(MICROS_PER_DAY) + i128::from(width.micros);
        if step <= 0 {
            return Some(Err(bucket_error("interval must be greater than zero")));
        }
        let origin = origin_micros.map_or(
            i128::from(TIME_BUCKET_ORIGIN_DAYS) * i128::from(MICROS_PER_DAY) + offset_micros,
            i128::from,
        );
        origin + (i128::from(micros) - origin).div_euclid(step) * step
    };
    Some(
        datetime::checked_timestamp(bucketed).map(|micros| match (date, zoned) {
            (true, _) => Value::Date(datetime::timestamp_to_date(micros)),
            (false, true) => Value::TimestampTz(micros),
            (false, false) => Value::Timestamp(micros),
        }),
    )
}

fn integer_bucket(width: &Value, value: &Value, offset: &Value) -> Option<SqlResult<Value>> {
    let (Some(width_number), Some(number), Some(offset_number)) =
        (width.as_integer(), value.as_integer(), offset.as_integer())
    else {
        return matches!(
            (width, value, offset),
            (Value::Null, _, _) | (_, Value::Null, _) | (_, _, Value::Null)
        )
        .then_some(Ok(Value::Null));
    };
    if width_number <= 0 {
        return Some(Err(bucket_error("width must be greater than zero")));
    }
    let bucket = (i128::from(number) - i128::from(offset_number))
        .div_euclid(i128::from(width_number))
        * i128::from(width_number)
        + i128::from(offset_number);
    Some(Ok(match value {
        Value::SmallInt(_) => i16::try_from(bucket)
            .map(Value::SmallInt)
            .unwrap_or(Value::Null),
        Value::Integer(_) => i32::try_from(bucket)
            .map(Value::Integer)
            .unwrap_or(Value::Null),
        _ => i64::try_from(bucket)
            .map(Value::BigInt)
            .unwrap_or(Value::Null),
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
                let zone = zone_named(zone)?;
                datetime::checked_timestamp(zone.utc_of_local(local)).map(Value::TimestampTz)
            })
        }
        ("make_interval", _) if values.len() <= 7 => made_interval(values)?,
        ("date_bin", [Value::Interval(stride), source, origin]) => {
            date_bin(stride, source, origin)?
        }
        ("time_bucket", [Value::Interval(width), source]) => time_bucket(width, source, None)?,
        ("time_bucket", [Value::Interval(width), source, origin]) => {
            time_bucket(width, source, Some(origin))?
        }
        ("time_bucket", [Value::Text(width), source, rest @ ..]) if rest.len() <= 1 => {
            match Interval::parse(width, None) {
                Ok(width) => time_bucket(&width, source, rest.first())?,
                Err(error) => Err(error),
            }
        }
        ("time_bucket", [width, value]) => integer_bucket(width, value, &Value::Integer(0))?,
        ("time_bucket", [width, value, offset]) => integer_bucket(width, value, offset)?,
        ("isfinite", [value @ (Value::Date(_) | Value::Timestamp(_) | Value::TimestampTz(_))]) => {
            Ok(Value::Boolean(infinity_sign(value).is_none()))
        }
        ("isfinite", [Value::Interval(interval)]) => Ok(Value::Boolean(interval.is_finite())),
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
        ("age", [value]) if infinity_sign(value).is_some() => {
            infinite_span(None, infinity_sign(value))?
        }
        ("age", [first, second])
            if (infinity_sign(first).is_some() || infinity_sign(second).is_some())
                && age_instant(first).is_some()
                && age_instant(second).is_some() =>
        {
            infinite_span(infinity_sign(first), infinity_sign(second))?
        }
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
            offset_seconds: datetime::session_offset_now(),
        }),
        ("localtime", []) => {
            local_now().map(|local| Value::Time(datetime::time_of_timestamp(local)))
        }
        ("extract" | "date_part", [Value::Text(field), Value::Interval(interval)])
            if !interval.is_finite() =>
        {
            match interval.infinite_extract(field) {
                Err(error) => Err(error),
                Ok(None) => Ok(Value::Null),
                Ok(Some(infinite)) if name == "date_part" => Ok(Value::Double(infinite)),
                Ok(Some(infinite)) => Ok(Value::Numeric(crate::numeric::Numeric::infinity(
                    infinite < 0.0,
                ))),
            }
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

#[cfg(test)]
mod tests {
    use crate::datetime::{self, MICROS_PER_DAY, MICROS_PER_SECOND};
    use crate::memory_store::MemoryStore;
    use crate::{Session, SqlError};

    fn value(session: &mut Session<'_>, sql: &str) -> Result<String, SqlError> {
        let mut outputs = Vec::new();
        session.execute(sql, &mut |result| {
            outputs.push(crate::tests::render(&result))
        })?;
        let output = outputs.pop().unwrap_or_default();
        Ok(output.lines().nth(1).unwrap_or_default().to_owned())
    }

    #[test]
    fn infinite_dates_and_timestamps_give_infinity_or_null_never_made_up_numbers() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for (sql, shown) in [
            (
                "SELECT date_part('epoch', 'infinity'::timestamp)",
                "Infinity",
            ),
            (
                "SELECT date_part('year', '-infinity'::timestamptz)",
                "-Infinity",
            ),
            (
                "SELECT date_part('millennium', 'infinity'::date)",
                "Infinity",
            ),
            ("SELECT date_part('julian', '-infinity'::date)", "-Infinity"),
            (
                "SELECT date_part('isoyear', 'infinity'::timestamp)",
                "Infinity",
            ),
            (
                "SELECT date_part('decade', 'infinity'::timestamp)",
                "Infinity",
            ),
            (
                "SELECT date_part('century', 'infinity'::timestamp)",
                "Infinity",
            ),
            ("SELECT date_part('month', 'infinity'::timestamp)", "NULL"),
            (
                "SELECT date_part('timezone', 'infinity'::timestamptz)",
                "NULL",
            ),
            ("SELECT extract(day FROM 'infinity'::date)", "NULL"),
            ("SELECT extract(hour FROM '-infinity'::timestamp)", "NULL"),
            ("SELECT extract(dow FROM 'infinity'::timestamptz)", "NULL"),
            (
                "SELECT age('infinity'::timestamp, '2024-01-01')::text",
                "infinity",
            ),
            (
                "SELECT age('2024-01-01'::timestamp, 'infinity')::text",
                "-infinity",
            ),
            (
                "SELECT age('-infinity'::date, 'infinity'::date)::text",
                "-infinity",
            ),
            ("SELECT age('infinity'::timestamptz)::text", "-infinity"),
            ("SELECT age('-infinity'::timestamp)::text", "infinity"),
            (
                "SELECT date_trunc('day', 'infinity'::timestamp)::text",
                "infinity",
            ),
            (
                "SELECT date_trunc('year', '-infinity'::timestamptz)::text",
                "-infinity",
            ),
            (
                "SELECT date_trunc('month', 'infinity'::date)::text",
                "infinity",
            ),
            ("SELECT to_char('infinity'::timestamp, 'YYYY')", "NULL"),
            (
                "SELECT date_bin('1 hour', 'infinity'::timestamp, '2000-01-01')::text",
                "infinity",
            ),
            (
                "SELECT time_bucket('1 day', '-infinity'::date)::text",
                "-infinity",
            ),
        ] {
            assert_eq!(value(&mut session, sql).unwrap(), shown, "{sql}");
        }
        for (sql, shown) in [
            ("SELECT extract(epoch FROM 'infinity'::timestamp)", "Infinity"),
            ("SELECT extract(year FROM '-infinity'::timestamptz)", "-Infinity"),
            ("SELECT extract(julian FROM 'infinity'::date)", "Infinity"),
            ("SELECT extract(century FROM '-infinity'::date)", "-Infinity"),
            ("SELECT extract(epoch FROM 'infinity'::interval)", "Infinity"),
            ("SELECT extract(epoch FROM '-infinity'::interval)", "-Infinity"),
            (
                "SELECT pg_typeof(extract(epoch FROM 'infinity'::timestamp))::text",
                "numeric",
            ),
            (
                "SELECT extract(epoch FROM 'infinity'::timestamp) > 1e300",
                "t",
            ),
            (
                "SELECT (extract(year FROM '-infinity'::date) + 1)::text",
                "-Infinity",
            ),
            (
                "SELECT extract(epoch FROM 'infinity'::timestamp)::float8::text",
                "Infinity",
            ),
            ("SELECT 'Infinity'::numeric::text", "Infinity"),
            ("SELECT '-inf'::numeric::text", "-Infinity"),
            (
                "SELECT to_json(extract(epoch FROM 'infinity'::timestamp))::text",
                "\"Infinity\"",
            ),
            ("SELECT 'infinity'::float8::numeric::text", "Infinity"),
            (
                "SELECT date_trunc('hour', '-infinity'::timestamp)::text",
                "-infinity",
            ),
        ] {
            assert_eq!(value(&mut session, sql).unwrap(), shown, "{sql}");
        }
        for (sql, code) in [
            ("SELECT extract(hour FROM 'infinity'::date)", "0A000"),
            (
                "SELECT extract(epoch FROM 'infinity'::timestamp)::integer",
                "0A000",
            ),
            ("SELECT 'Infinity'::numeric(10, 2)", "22003"),
            (
                "SELECT date_part('timezone', 'infinity'::timestamp)",
                "0A000",
            ),
            (
                "SELECT date_part('fortnight', 'infinity'::timestamp)",
                "22023",
            ),
            (
                "SELECT age('infinity'::timestamp, 'infinity'::timestamp)",
                "22008",
            ),
            ("SELECT age('-infinity'::date, '-infinity'::date)", "22008"),
            (
                "SELECT date_bin('1 hour', '2024-01-01'::timestamp, 'infinity'::timestamp)",
                "22008",
            ),
        ] {
            let error = value(&mut session, sql).unwrap_err();
            assert_eq!(error.state().code(), code, "{sql}");
        }
    }

    fn rows(session: &mut Session<'_>, sql: &str) -> Vec<Vec<crate::Value>> {
        let mut last = None;
        session
            .execute(sql, &mut |result| last = Some(result.rows))
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        last.unwrap_or_default()
    }

    fn vendor_code(session: &mut Session<'_>, sql: &str) -> Option<u16> {
        session
            .execute(sql, &mut |_| {})
            .expect_err(sql)
            .vendor_code()
    }

    fn moments(store: &MemoryStore) {
        let mut setup = Session::new(store);
        rows(
            &mut setup,
            "CREATE SCHEMA hr; CREATE TABLE hr.moments (id integer PRIMARY KEY, stamp timestamp, zoned timestamptz, d date)",
        );
        rows(
            &mut setup,
            "INSERT INTO hr.moments VALUES (1, 'infinity', '-infinity', 'infinity'), (2, '2024-01-01 10:00', '2024-01-01 10:00+00', '2024-01-01')",
        );
    }

    fn utc(year: i64, month: i64, day: i64, hour: i64, minute: i64) -> i64 {
        datetime::days_from_civil(year, month, day) * MICROS_PER_DAY
            + (hour * 3600 + minute * 60) * MICROS_PER_SECOND
    }

    #[test]
    fn sql_server_date_functions_refuse_infinite_dates_as_out_of_range() {
        let store = MemoryStore::default();
        moments(&store);
        let mut session = Session::with_dialect(&store, crate::Dialect::SqlServer);
        for sql in [
            "SELECT DATEADD(day, 1, stamp) FROM hr.moments WHERE id = 1",
            "SELECT DATEDIFF(day, d, '2024-01-01') FROM hr.moments WHERE id = 1",
            "SELECT DATEPART(year, zoned) FROM hr.moments WHERE id = 1",
            "SELECT DATENAME(month, d) FROM hr.moments WHERE id = 1",
            "SELECT EOMONTH(d) FROM hr.moments WHERE id = 1",
            "SELECT CONVERT(varchar(30), stamp, 121) FROM hr.moments WHERE id = 1",
            "SELECT CONVERT(date, stamp) FROM hr.moments WHERE id = 1",
            "SELECT CAST(d AS datetime2) FROM hr.moments WHERE id = 1",
            "SELECT stamp AT TIME ZONE 'UTC' FROM hr.moments WHERE id = 1",
        ] {
            assert_eq!(vendor_code(&mut session, sql), Some(242), "{sql}");
        }
        assert_eq!(
            rows(
                &mut session,
                "SELECT DATEPART(year, stamp) FROM hr.moments WHERE id = 2"
            ),
            vec![vec![crate::Value::Integer(2024)]]
        );
    }

    #[test]
    fn oracle_date_functions_refuse_infinite_dates_with_ora_01841() {
        let store = MemoryStore::default();
        moments(&store);
        let mut session = Session::with_dialect(&store, crate::Dialect::Oracle);
        rows(&mut session, "ALTER SESSION SET CURRENT_SCHEMA = HR");
        for sql in [
            "SELECT TO_CHAR(d, 'YYYY-MM-DD') FROM moments WHERE id = 1",
            "SELECT ADD_MONTHS(d, 1) FROM moments WHERE id = 1",
            "SELECT LAST_DAY(stamp) FROM moments WHERE id = 1",
            "SELECT TRUNC(stamp) FROM moments WHERE id = 1",
            "SELECT MONTHS_BETWEEN(d, DATE '2024-01-01') FROM moments WHERE id = 1",
            "SELECT EXTRACT(YEAR FROM stamp) FROM moments WHERE id = 1",
            "SELECT SYS_EXTRACT_UTC(zoned) FROM moments WHERE id = 1",
            "SELECT stamp + 1 FROM moments WHERE id = 1",
            "SELECT d - DATE '2024-01-01' FROM moments WHERE id = 1",
        ] {
            assert_eq!(vendor_code(&mut session, sql), Some(1841), "{sql}");
        }
        assert_eq!(
            rows(&mut session, "SELECT TO_CHAR(d, 'YYYY') FROM moments WHERE id = 2"),
            vec![vec![crate::Value::Text("2024".into())]]
        );
    }

    #[test]
    fn sql_server_at_time_zone_reads_the_windows_zone_names() {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, crate::Dialect::SqlServer);
        for (sql, instant) in [
            (
                "SELECT CAST('2022-03-27T02:01:00' AS datetime2(0)) AT TIME ZONE 'Central European Standard Time'",
                utc(2022, 3, 27, 1, 1),
            ),
            (
                "SELECT CAST('2022-10-30T02:00:00' AS datetime2(0)) AT TIME ZONE 'Central European Standard Time'",
                utc(2022, 10, 30, 0, 0),
            ),
            (
                "SELECT CAST('2022-07-01T12:00:00' AS datetime2) AT TIME ZONE 'pacific standard time'",
                utc(2022, 7, 1, 19, 0),
            ),
            (
                "SELECT CAST('2022-01-15T08:00:00' AS datetime) AT TIME ZONE 'India Standard Time'",
                utc(2022, 1, 15, 2, 30),
            ),
            (
                "SELECT CAST('2022-07-01T12:00:00+02:00' AS datetimeoffset) AT TIME ZONE 'Tokyo Standard Time'",
                utc(2022, 7, 1, 10, 0),
            ),
            (
                "SELECT (CAST('2022-07-01T12:00:00' AS datetime2) AT TIME ZONE 'UTC') AT TIME ZONE 'W. Europe Standard Time'",
                utc(2022, 7, 1, 12, 0),
            ),
        ] {
            assert_eq!(
                rows(&mut session, sql),
                vec![vec![crate::Value::TimestampTz(instant)]],
                "{sql}"
            );
        }
        assert_eq!(
            rows(
                &mut session,
                "SELECT CAST(NULL AS datetime2) AT TIME ZONE 'UTC'"
            ),
            vec![vec![crate::Value::Null]]
        );
        for (sql, code) in [
            (
                "SELECT CAST('2022-01-01' AS datetime2) AT TIME ZONE 'Europe/Paris'",
                9820,
            ),
            (
                "SELECT CAST('2022-01-01' AS datetime2) AT TIME ZONE 'PST'",
                9820,
            ),
            ("SELECT '2022-01-01' AT TIME ZONE 'UTC'", 8116),
            ("SELECT CAST('2022-01-01' AS date) AT TIME ZONE 'UTC'", 8116),
            ("SELECT CAST('2022-01-01' AS datetime2) AT TIME ZONE 5", 8116),
        ] {
            assert_eq!(vendor_code(&mut session, sql), Some(code), "{sql}");
        }
    }

    #[test]
    fn sys_time_zone_info_lists_the_windows_zones_with_their_offsets_now() {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, crate::Dialect::SqlServer);
        assert_eq!(
            rows(&mut session, "SELECT COUNT(*) FROM sys.time_zone_info"),
            vec![vec![crate::Value::Integer(142)]]
        );
        assert_eq!(
            rows(
                &mut session,
                "SELECT current_utc_offset, is_currently_dst FROM sys.time_zone_info WHERE name IN ('UTC', 'India Standard Time', 'Nepal Standard Time', 'Dateline Standard Time') ORDER BY name"
            ),
            vec![
                vec![crate::Value::Text("-12:00".into()), crate::Value::Boolean(false)],
                vec![crate::Value::Text("+05:30".into()), crate::Value::Boolean(false)],
                vec![crate::Value::Text("+05:45".into()), crate::Value::Boolean(false)],
                vec![crate::Value::Text("+00:00".into()), crate::Value::Boolean(false)],
            ]
        );
    }
}
