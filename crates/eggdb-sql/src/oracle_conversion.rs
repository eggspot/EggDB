use crate::datetime::{self, MICROS_PER_DAY};
use crate::encoding::Column;
use crate::error::SqlResult;
use crate::interval::IntervalFields;
use crate::oracle_datetime::{self, ModelUse};
use crate::oracle_intervals::IntervalKind;
use crate::types::{SqlType, Value};

fn nls_models(target: SqlType) -> Option<Vec<(String, ModelUse)>> {
    let nls = crate::oracle_nls::current();
    let date = (nls.date_format().to_owned(), ModelUse::DateInput);
    let timestamp = (nls.timestamp_format().to_owned(), ModelUse::TimestampInput);
    Some(match target {
        SqlType::Date | SqlType::Timestamp(Some(0)) => vec![date],
        SqlType::Timestamp(None) => vec![date, timestamp],
        SqlType::Timestamp(_) => vec![timestamp],
        SqlType::TimestampTz(_) => vec![(
            nls.timestamp_tz_format().to_owned(),
            ModelUse::ZonedInput,
        )],
        _ => return None,
    })
}

fn read_local(text: &str, models: &[(String, ModelUse)]) -> SqlResult<(i64, Option<i32>)> {
    let mut first_error = None;
    for (format, purpose) in models {
        match oracle_datetime::read_zoned(text, format, *purpose) {
            Ok(read) => return Ok(read),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    match (oracle_datetime::strict_iso(text, true), first_error) {
        (Some(local), _) => Ok((local, None)),
        (None, Some(error)) => Err(error),
        (None, None) => Err(oracle_datetime::not_recognized()),
    }
}

fn typed(local: i64, zone: Option<i32>, target: SqlType) -> SqlResult<Value> {
    match target {
        SqlType::Date => Ok(Value::Date(datetime::timestamp_to_date(local))),
        SqlType::TimestampTz(_) => oracle_datetime::utc_of(local, zone).map(Value::TimestampTz),
        _ => Ok(Value::Timestamp(local)),
    }
}

pub(crate) fn datetime_of(text: &str, target: SqlType) -> Option<SqlResult<Value>> {
    if let SqlType::Interval { fields, .. } = target {
        let kind = match fields {
            Some(IntervalFields::Year | IntervalFields::Month | IntervalFields::YearToMonth) => {
                IntervalKind::YearToMonth
            }
            _ => IntervalKind::DayToSecond,
        };
        return Some(
            crate::oracle_intervals::parse_text(text, kind)
                .and_then(|interval| crate::oracle_intervals::conformed(interval, target)),
        );
    }
    let models = nls_models(target)?;
    Some(read_local(text, &models).and_then(|(local, zone)| typed(local, zone, target)))
}

pub(crate) fn literal_of(text: &str, target: SqlType) -> Option<SqlResult<Value>> {
    if let Some(value) = datetime_of(text, target) {
        return Some(value);
    }
    if target.is_numeric() {
        return Some(crate::oracle_functions::parse_number(text).map(|number| number.value()));
    }
    None
}

fn text_for_column(value: &Value) -> Option<String> {
    match value {
        Value::Timestamp(micros) => {
            let declared = if micros.rem_euclid(datetime::MICROS_PER_SECOND) == 0 {
                SqlType::Timestamp(Some(0))
            } else {
                SqlType::Timestamp(Some(6))
            };
            crate::oracle_functions::text_with_type(value, Some(declared))
        }
        Value::TimestampTz(_)
        | Value::Date(_)
        | Value::Numeric(_)
        | Value::Double(_)
        | Value::Real(_) => crate::oracle_functions::text_of(value),
        _ => None,
    }
}

pub(crate) fn assign(value: Value, column: &Column) -> SqlResult<Value> {
    let target = column.sql_type;
    let converted = match &value {
        Value::Text(text) | Value::Bpchar(text) if !target.is_character() => {
            match literal_of(text, target) {
                Some(converted) => converted?,
                None => value,
            }
        }
        Value::Timestamp(micros) if matches!(target, SqlType::Timestamp(Some(0))) => {
            Value::Timestamp(micros - micros.rem_euclid(datetime::MICROS_PER_SECOND))
        }
        Value::Date(days) if matches!(target, SqlType::Timestamp(_)) => {
            Value::Timestamp(i64::from(*days) * MICROS_PER_DAY)
        }
        _ if target.is_character() => match text_for_column(&value) {
            Some(text) => Value::Text(text),
            None => value,
        },
        _ => value,
    };
    crate::execute::assign_to(converted, column)
}

pub(crate) fn converted_for_comparison(
    left: &Value,
    right: &Value,
) -> Option<SqlResult<(Value, Value)>> {
    let convert = |text: &str, other: &Value| -> Option<SqlResult<Value>> {
        let target = match other {
            Value::Timestamp(_) => SqlType::Timestamp(None),
            Value::TimestampTz(_) => SqlType::TimestampTz(Some(6)),
            Value::Date(_) => SqlType::Date,
            Value::Interval(interval) => SqlType::Interval {
                fields: Some(if interval.months != 0 {
                    IntervalFields::YearToMonth
                } else {
                    IntervalFields::DayToSecond
                }),
                precision: None,
            },
            other if other.sql_type().is_some_and(SqlType::is_numeric) => {
                return Some(
                    crate::oracle_functions::parse_number(text).map(|number| number.value()),
                );
            }
            _ => return None,
        };
        datetime_of(text, target)
    };
    match (left, right) {
        (Value::Text(text) | Value::Bpchar(text), other) if !matches!(other, Value::Text(_)) => {
            Some(convert(text, other)?.map(|left| (left, right.clone())))
        }
        (other, Value::Text(text) | Value::Bpchar(text)) if !matches!(other, Value::Text(_)) => {
            Some(convert(text, other)?.map(|right| (left.clone(), right)))
        }
        _ => None,
    }
}

fn whole_seconds(micros: i64) -> i64 {
    micros - micros.rem_euclid(datetime::MICROS_PER_SECOND)
}

fn days_as_micros(days: &Value) -> Option<SqlResult<i64>> {
    let number = match days {
        Value::Text(text) => match crate::oracle_functions::parse_number(text) {
            Ok(number) => number.value(),
            Err(error) => return Some(Err(error)),
        },
        other if other.sql_type().is_some_and(SqlType::is_numeric) => other.clone(),
        _ => return None,
    };
    let seconds = match number {
        Value::Numeric(decimal) => {
            let per_day = crate::numeric::Numeric::from_i64(86_400);
            match decimal
                .multiply(&per_day)
                .and_then(|seconds| seconds.rounded(0))
            {
                Ok(seconds) => seconds.integral_value(),
                Err(error) => return Some(Err(error)),
            }
        }
        Value::Double(float) => {
            let seconds = (float * 86_400.0).round();
            seconds.is_finite().then_some(seconds as i128)
        }
        Value::Real(float) => {
            let seconds = (f64::from(float) * 86_400.0).round();
            seconds.is_finite().then_some(seconds as i128)
        }
        other => other.as_integer().map(|days| i128::from(days) * 86_400),
    };
    Some(
        seconds
            .and_then(|seconds| seconds.checked_mul(i128::from(datetime::MICROS_PER_SECOND)))
            .and_then(|micros| i64::try_from(micros).ok())
            .ok_or_else(oracle_datetime::year_out_of_range),
    )
}

fn plus_days(stamp: i64, micros: i64) -> SqlResult<Value> {
    let moved = i128::from(whole_seconds(stamp)) + i128::from(micros);
    let moved = i64::try_from(moved).map_err(|_| oracle_datetime::year_out_of_range())?;
    let (year, _, _) = datetime::civil_from_days(moved.div_euclid(MICROS_PER_DAY));
    if !oracle_datetime::year_in_range(year) {
        return Err(oracle_datetime::year_out_of_range());
    }
    Ok(Value::Timestamp(moved))
}

fn days_between(left: i64, right: i64) -> SqlResult<Value> {
    let micros = crate::numeric::Numeric::from_i128(i128::from(left) - i128::from(right));
    let per_day = crate::numeric::Numeric::from_i64(MICROS_PER_DAY);
    let days = micros.divide(&per_day)?;
    Ok(match days.integral_value().map(i64::try_from) {
        Some(Ok(whole)) => Value::BigInt(whole),
        _ => Value::Numeric(days),
    })
}

fn local_micros(value: &Value) -> Option<SqlResult<i64>> {
    Some(match value {
        Value::Timestamp(micros) => Ok(*micros),
        Value::TimestampTz(micros) => datetime::utc_to_local(*micros),
        Value::Date(days) => datetime::date_to_timestamp(*days),
        _ => return None,
    })
}

pub(crate) fn arithmetic(
    operator: crate::ast::BinaryOperator,
    left: &Value,
    right: &Value,
) -> Option<SqlResult<Value>> {
    use crate::ast::BinaryOperator as Op;
    if let Some(result) = crate::oracle_intervals::arithmetic(operator, left, right) {
        return Some(result);
    }
    if matches!(left, Value::Interval(_)) || matches!(right, Value::Interval(_)) {
        return None;
    }
    match (operator, local_micros(left), local_micros(right)) {
        (Op::Subtract, Some(left), Some(right)) => {
            return Some(left.and_then(|left| {
                right.and_then(|right| days_between(whole_seconds(left), whole_seconds(right)))
            }));
        }
        (Op::Add | Op::Subtract, Some(stamp), None) => {
            let days = days_as_micros(right)?;
            return Some(stamp.and_then(|stamp| {
                let days = days?;
                plus_days(stamp, if operator == Op::Add { days } else { -days })
            }));
        }
        (Op::Add, None, Some(stamp)) => {
            let days = days_as_micros(left)?;
            return Some(stamp.and_then(|stamp| plus_days(stamp, days?)));
        }
        (_, Some(_), _) | (_, _, Some(_)) => return None,
        _ => {}
    }
    let number = |value: &Value| match value {
        Value::Text(text) | Value::Bpchar(text) => {
            Some(crate::oracle_functions::parse_number(text).map(|number| number.value()))
        }
        _ => None,
    };
    match (number(left), number(right)) {
        (Some(left), None) if right.sql_type().is_some_and(SqlType::is_numeric) => {
            Some(left.map(|left| (left, right.clone())))
        }
        (None, Some(right_value)) if left.sql_type().is_some_and(SqlType::is_numeric) => {
            Some(right_value.map(|right| (left.clone(), right)))
        }
        (Some(left), Some(right)) => Some(left.and_then(|left| right.map(|right| (left, right)))),
        _ => None,
    }
    .map(|pair| pair.and_then(|(left, right)| crate::eval::arithmetic(operator, &left, &right)))
}

#[cfg(test)]
mod tests {
    use super::{arithmetic, datetime_of};
    use crate::ast::BinaryOperator;
    use crate::datetime::{MICROS_PER_DAY, MICROS_PER_SECOND, days_from_civil};
    use crate::types::{SqlType, Value};

    fn at(year: i64, month: i64, day: i64, seconds: i64) -> i64 {
        days_from_civil(year, month, day) * MICROS_PER_DAY + seconds * MICROS_PER_SECOND
    }

    fn decimal(text: &str) -> Value {
        Value::Numeric(crate::numeric::Numeric::parse(text).expect("decimal"))
    }

    #[test]
    fn text_meets_dates_by_the_nls_format_then_strict_iso() {
        assert_eq!(
            datetime_of("17-JUN-03", SqlType::Timestamp(Some(0))).map(Result::ok),
            Some(Some(Value::Timestamp(at(2003, 6, 17, 0))))
        );
        assert_eq!(
            datetime_of("2003-06-17", SqlType::Timestamp(Some(0))).map(Result::ok),
            Some(Some(Value::Timestamp(at(2003, 6, 17, 0))))
        );
        assert!(matches!(
            datetime_of("now", SqlType::Timestamp(Some(0))),
            Some(Err(_))
        ));
        assert_eq!(datetime_of("1", SqlType::Integer).map(Result::ok), None);
    }

    #[test]
    fn date_arithmetic_adds_exact_fractions_of_a_day() {
        let date = Value::Timestamp(at(2024, 1, 31, 0) + 250_000);
        let add = |days: Value| arithmetic(BinaryOperator::Add, &date, &days).map(Result::ok);
        assert_eq!(
            add(Value::BigInt(1)),
            Some(Some(Value::Timestamp(at(2024, 2, 1, 0))))
        );
        let hour = super::super::oracle_functions::call(
            "oracle_divide",
            &[Value::BigInt(1), Value::BigInt(24)],
            &[],
        )
        .expect("1/24");
        assert_eq!(
            add(hour),
            Some(Some(Value::Timestamp(at(2024, 1, 31, 3600))))
        );
        let second = super::super::oracle_functions::call(
            "oracle_divide",
            &[Value::BigInt(1), Value::BigInt(86_400)],
            &[],
        )
        .expect("1/86400");
        assert_eq!(
            add(second),
            Some(Some(Value::Timestamp(at(2024, 1, 31, 1))))
        );
        assert_eq!(
            arithmetic(
                BinaryOperator::Subtract,
                &Value::Timestamp(at(2024, 1, 2, 43_200)),
                &Value::Timestamp(at(2024, 1, 1, 0)),
            )
            .and_then(Result::ok)
            .and_then(|days| crate::Dialect::Oracle.value_text(&days)),
            Some("1.5".to_owned())
        );
        assert_eq!(
            arithmetic(
                BinaryOperator::Add,
                &Value::Timestamp(at(9999, 12, 31, 0)),
                &Value::BigInt(1)
            )
            .and_then(|result| result.err())
            .and_then(|error| error.vendor_code()),
            Some(1841)
        );
        assert!(
            arithmetic(BinaryOperator::Add, &date, &Value::Double(f64::NAN))
                .is_some_and(|result| result.is_err())
        );
        assert_eq!(
            arithmetic(
                BinaryOperator::Add,
                &Value::Text("5.5".into()),
                &Value::Integer(1)
            )
            .map(Result::ok),
            Some(Some(decimal("6.5")))
        );
    }
}
