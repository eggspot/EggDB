use crate::datetime::{self, LocalTime, MICROS_PER_DAY, MICROS_PER_SECOND};
use crate::error::{SqlError, SqlResult};
use crate::types::Value;

pub(crate) const EXTRACT_FUNCTION: &str = "mysql_extract";

const DAY_OF_WEEK: &str = "dayofweek";
const DAY_OF_YEAR: &str = "dayofyear";
const WEEKDAY: &str = "weekday";
const ZERO_DATE: &str = "0000-00-00";
const DATE_PARTS_OF_A_ZERO_DATE: [&str; 4] = ["year", "quarter", "month", "day"];
const CLOCK_UNITS: [&str; 4] = ["hour", "minute", "second", "microsecond"];
const MICROS_PER_HOUR: i64 = 3600 * MICROS_PER_SECOND;
const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SECOND;

pub(crate) fn unit_of_function(name: &str) -> Option<&'static str> {
    Some(match name {
        "year" => "year",
        "quarter" => "quarter",
        "month" => "month",
        "day" | "dayofmonth" => "day",
        "hour" => "hour",
        "minute" => "minute",
        "second" => "second",
        "microsecond" => "microsecond",
        "dayofweek" => DAY_OF_WEEK,
        "dayofyear" => DAY_OF_YEAR,
        "weekday" => WEEKDAY,
        _ => return None,
    })
}

enum Moment {
    Local(LocalTime),
    ZeroDate { of_day: i64 },
    Duration(i64),
}

pub(crate) fn extract(unit: &str, value: &Value) -> SqlResult<Value> {
    let part = match moment_of(unit, value)? {
        None => None,
        Some(Moment::Local(local)) => Some(local_part(unit, local).ok_or_else(|| {
            SqlError::syntax(format!("syntax error at or near \"{unit}\""), 0)
        })?),
        Some(Moment::ZeroDate { of_day }) => zero_date_part(unit, of_day),
        Some(Moment::Duration(micros)) => micros
            .checked_abs()
            .and_then(|length| clock_part(unit, length)),
    };
    Ok(part.map_or(Value::Null, Value::BigInt))
}

fn local_part(unit: &str, local: LocalTime) -> Option<i64> {
    let from_sunday = (local.days + 6).rem_euclid(7);
    match unit {
        DAY_OF_WEEK => Some(from_sunday + 1),
        WEEKDAY => Some((from_sunday + 6) % 7),
        DAY_OF_YEAR => {
            let (year, _, _) = datetime::civil_from_days(local.days);
            Some(local.days - datetime::days_from_civil(year, 1, 1) + 1)
        }
        _ => datetime::mysql_extract(unit, local),
    }
}

fn zero_date_part(unit: &str, of_day: i64) -> Option<i64> {
    if DATE_PARTS_OF_A_ZERO_DATE.contains(&unit) {
        return Some(0);
    }
    clock_part(unit, of_day)
}

fn clock_part(unit: &str, micros: i64) -> Option<i64> {
    Some(match unit {
        "hour" => micros / MICROS_PER_HOUR,
        "minute" => micros / MICROS_PER_MINUTE % 60,
        "second" => micros / MICROS_PER_SECOND % 60,
        "microsecond" => micros % MICROS_PER_SECOND,
        _ => return None,
    })
}

fn moment_of(unit: &str, value: &Value) -> SqlResult<Option<Moment>> {
    let clock_unit = CLOCK_UNITS.contains(&unit);
    Ok(match value {
        Value::Date(days) if datetime::is_infinite_date(*days) => None,
        Value::Timestamp(micros) | Value::TimestampTz(micros)
            if datetime::is_infinite_timestamp(*micros) =>
        {
            None
        }
        Value::Date(days) => Some(Moment::Local(LocalTime::of_date(*days))),
        Value::Timestamp(micros) => Some(Moment::Local(LocalTime::of_micros(*micros))),
        Value::TimestampTz(micros) => Some(Moment::Local(LocalTime::of_micros(
            datetime::utc_to_local(*micros)?,
        ))),
        Value::Time(micros) => Some(time_moment(*micros, clock_unit)?),
        Value::Text(text) => text_moment(text, clock_unit),
        other => other.as_integer().and_then(number_moment),
    })
}

fn time_moment(micros: i64, clock_unit: bool) -> SqlResult<Moment> {
    if clock_unit {
        return Ok(Moment::Duration(micros));
    }
    let today = datetime::timestamp_to_date(datetime::utc_to_local(datetime::statement_now())?);
    Ok(Moment::Local(LocalTime::of_micros(
        i64::from(today)
            .saturating_mul(MICROS_PER_DAY)
            .saturating_add(micros),
    )))
}

fn text_moment(text: &str, clock_unit: bool) -> Option<Moment> {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix(ZERO_DATE) {
        let clock = rest.trim();
        if clock.is_empty() {
            return Some(Moment::ZeroDate { of_day: 0 });
        }
        return datetime::parse_mysql_time(clock)
            .ok()
            .filter(|micros| (0..MICROS_PER_DAY).contains(micros))
            .map(|of_day| Moment::ZeroDate { of_day });
    }
    if clock_unit && let Ok(micros) = datetime::parse_mysql_time(trimmed) {
        return Some(Moment::Duration(micros));
    }
    datetime::parse_timestamp(trimmed, false)
        .ok()
        .filter(|micros| !datetime::is_infinite_timestamp(*micros))
        .map(|micros| Moment::Local(LocalTime::of_micros(micros)))
}

fn number_moment(number: i64) -> Option<Moment> {
    if number == 0 {
        return Some(Moment::ZeroDate { of_day: 0 });
    }
    let digits = u64::try_from(number).ok()?.to_string();
    let (date, clock) = match digits.len() {
        6 | 8 => (digits.as_str(), ""),
        12 | 14 => digits.split_at(digits.len() - 6),
        _ => return None,
    };
    let field = |text: &str, from: usize| -> Option<i64> { text.get(from..from + 2)?.parse().ok() };
    let (year, month_at) = if date.len() == 8 {
        (date.get(0..4)?.parse::<i64>().ok()?, 4)
    } else {
        let short = field(date, 0)?;
        (if short < 70 { 2000 + short } else { 1900 + short }, 2)
    };
    let month = field(date, month_at)?;
    let day = field(date, month_at + 2)?;
    if !(1..=12).contains(&month) || !(1..=datetime::days_in_month(year, month)).contains(&day) {
        return None;
    }
    let (hour, minute, second) = if clock.is_empty() {
        (0, 0, 0)
    } else {
        (field(clock, 0)?, field(clock, 2)?, field(clock, 4)?)
    };
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some(Moment::Local(LocalTime {
        days: datetime::days_from_civil(year, month, day),
        of_day: hour * MICROS_PER_HOUR + minute * MICROS_PER_MINUTE + second * MICROS_PER_SECOND,
    }))
}

#[cfg(test)]
mod tests {
    use super::{extract, unit_of_function};
    use crate::types::Value;

    fn part(function: &str, value: Value) -> Option<i64> {
        let unit = unit_of_function(function).expect("a date-part function");
        match extract(unit, &value).expect("a part") {
            Value::BigInt(number) => Some(number),
            Value::Null => None,
            other => panic!("unexpected {other:?}"),
        }
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_owned())
    }

    #[test]
    fn date_parts_follow_the_mysql_manual_examples() {
        assert_eq!(part("dayofweek", text("2007-02-03")), Some(7));
        assert_eq!(part("weekday", text("2008-02-03 22:23:00")), Some(6));
        assert_eq!(part("weekday", text("2007-11-06")), Some(1));
        assert_eq!(part("dayofyear", text("2007-02-03")), Some(34));
        assert_eq!(part("dayofmonth", text("2007-02-03")), Some(3));
        assert_eq!(part("day", text("2007-02-03")), Some(3));
        assert_eq!(part("month", text("2008-02-03")), Some(2));
        assert_eq!(part("quarter", text("2008-04-01")), Some(2));
        assert_eq!(part("year", text("1987-01-01")), Some(1987));
        assert_eq!(part("hour", text("10:05:03")), Some(10));
        assert_eq!(part("hour", text("272:59:59")), Some(272));
        assert_eq!(part("minute", text("2008-02-03 10:05:03")), Some(5));
        assert_eq!(part("second", text("10:05:03")), Some(3));
        assert_eq!(part("microsecond", text("12:00:00.123456")), Some(123_456));
        assert_eq!(
            part("microsecond", text("2019-12-31 23:59:59.000010")),
            Some(10)
        );
    }

    #[test]
    fn zero_dates_invalid_dates_and_numbers_are_read_as_mysql_reads_them() {
        assert_eq!(part("year", text("0000-00-00")), Some(0));
        assert_eq!(part("month", text("0000-00-00")), Some(0));
        assert_eq!(part("dayofmonth", text("0000-00-00 10:11:12")), Some(0));
        assert_eq!(part("hour", text("0000-00-00 10:11:12")), Some(10));
        assert_eq!(part("dayofweek", text("0000-00-00")), None);
        assert_eq!(part("dayofyear", text("0000-00-00")), None);
        assert_eq!(part("weekday", text("0000-00-00")), None);
        assert_eq!(part("year", text("2024-02-30")), None);
        assert_eq!(part("year", text("not a date")), None);
        assert_eq!(part("year", Value::Integer(20_240_203)), Some(2024));
        assert_eq!(part("month", Value::BigInt(20_240_203_101_112)), Some(2));
        assert_eq!(part("hour", Value::BigInt(20_240_203_101_112)), Some(10));
        assert_eq!(part("year", Value::Integer(991_231)), Some(1999));
        assert_eq!(part("year", Value::Integer(123)), None);
        assert_eq!(part("year", Value::Boolean(true)), None);
    }

    #[test]
    fn stored_dates_and_times_give_their_parts() {
        let date = Value::Date(crate::datetime::parse_date("2024-02-29").expect("a date"));
        assert_eq!(part("dayofweek", date.clone()), Some(5));
        assert_eq!(part("weekday", date.clone()), Some(3));
        assert_eq!(part("dayofyear", date.clone()), Some(60));
        assert_eq!(part("quarter", date), Some(1));
        let time = Value::Time(
            crate::datetime::parse_mysql_time("13:45:01.5").expect("a time"),
        );
        assert_eq!(part("hour", time.clone()), Some(13));
        assert_eq!(part("microsecond", time), Some(500_000));
    }
}
