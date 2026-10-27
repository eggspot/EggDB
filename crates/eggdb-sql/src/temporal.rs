use crate::ast::BinaryOperator;
use crate::datetime::{self, MICROS_PER_DAY};
use crate::error::SqlResult;
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

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
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
