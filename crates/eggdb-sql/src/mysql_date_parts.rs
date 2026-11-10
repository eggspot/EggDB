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
    let part =
        match moment_of(unit, value)? {
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
        (
            if short < 70 {
                2000 + short
            } else {
                1900 + short
            },
            2,
        )
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

pub(crate) const WEEK_FUNCTION: &str = "mysql_week";
pub(crate) const YEARWEEK_FUNCTION: &str = "mysql_yearweek";
pub(crate) const DATE_FORMAT_FUNCTION: &str = "mysql_date_format";
pub(crate) const WEEK_OF_YEAR_MODE: i64 = 3;

const MONDAY_FIRST: i64 = 1;
const WEEK_YEAR: i64 = 2;
const FIRST_WEEKDAY: i64 = 4;
const WEEKDAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn behaviour_of(mode: i64) -> i64 {
    let mode = mode.rem_euclid(8);
    if mode & MONDAY_FIRST == 0 {
        mode ^ FIRST_WEEKDAY
    } else {
        mode
    }
}

fn days_in_year(year: i64) -> i64 {
    if datetime::is_leap_year(year) { 366 } else { 365 }
}

fn weekday_of(days: i64, sunday_first: bool) -> i64 {
    if sunday_first {
        (days + 6).rem_euclid(7)
    } else {
        (days + 5).rem_euclid(7)
    }
}

fn calendar_week(days: i64, behaviour: i64) -> (i64, i64) {
    let (mut year, month, day) = datetime::civil_from_days(days);
    let monday_first = behaviour & MONDAY_FIRST != 0;
    let mut week_year = behaviour & WEEK_YEAR != 0;
    let first_weekday = behaviour & FIRST_WEEKDAY != 0;
    let mut first = datetime::days_from_civil(year, 1, 1);
    let mut weekday = weekday_of(first, !monday_first);
    let starts_late = |weekday: i64| (first_weekday && weekday != 0) || (!first_weekday && weekday >= 4);
    if month == 1 && day <= 7 - weekday {
        if !week_year && starts_late(weekday) {
            return (year, 0);
        }
        week_year = true;
        year -= 1;
        let length = days_in_year(year);
        first -= length;
        weekday = (weekday + 53 * 7 - length).rem_euclid(7);
    }
    let elapsed = if starts_late(weekday) {
        days - (first + (7 - weekday))
    } else {
        days - (first - weekday)
    };
    if week_year && elapsed >= 52 * 7 {
        let next = (weekday + days_in_year(year)).rem_euclid(7);
        if (!first_weekday && next < 4) || (first_weekday && next == 0) {
            return (year + 1, 1);
        }
    }
    (year, elapsed / 7 + 1)
}

fn local_of(value: &Value) -> SqlResult<Option<Result<LocalTime, i64>>> {
    Ok(match moment_of("day", value)? {
        None => None,
        Some(Moment::Local(local)) => Some(Ok(local)),
        Some(Moment::ZeroDate { of_day }) => Some(Err(of_day)),
        Some(Moment::Duration(_)) => None,
    })
}

fn mode_of(mode: &Value) -> Option<i64> {
    mode.as_integer()
}

/// MySQL's `WEEK(date, mode)`.
pub(crate) fn week(value: &Value, mode: &Value) -> SqlResult<Value> {
    let (Some(Ok(local)), Some(mode)) = (local_of(value)?, mode_of(mode)) else {
        return Ok(Value::Null);
    };
    Ok(Value::BigInt(calendar_week(local.days, behaviour_of(mode)).1))
}

/// MySQL's `YEARWEEK(date, mode)`: the year and week as `YYYYWW`.
pub(crate) fn year_week(value: &Value, mode: &Value) -> SqlResult<Value> {
    let (Some(Ok(local)), Some(mode)) = (local_of(value)?, mode_of(mode)) else {
        return Ok(Value::Null);
    };
    let (year, week) = calendar_week(local.days, behaviour_of(mode) | WEEK_YEAR);
    Ok(Value::BigInt(year * 100 + week))
}

fn english_suffix(day: i64) -> &'static str {
    match (day % 10, day % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    }
}

/// MySQL's `DATE_FORMAT(date, format)`, with every `%` specifier MySQL documents; a zero date
/// formats its zero fields and gives `NULL` for the specifiers that need a real day.
pub(crate) fn date_format(value: &Value, format: &Value) -> SqlResult<Value> {
    let Value::Text(format) = format else {
        return Ok(if matches!(format, Value::Null) {
            Value::Null
        } else {
            Value::Text(String::new())
        });
    };
    let Some(moment) = local_of(value)? else {
        return Ok(Value::Null);
    };
    let (days, of_day) = match moment {
        Ok(local) => (Some(local.days), local.of_day),
        Err(of_day) => (None, of_day),
    };
    let (year, month, day) = days.map_or((0, 0, 0), datetime::civil_from_days);
    let hour = of_day / MICROS_PER_HOUR;
    let minute = of_day / MICROS_PER_MINUTE % 60;
    let second = of_day / MICROS_PER_SECOND % 60;
    let micro = of_day % MICROS_PER_SECOND;
    let twelve = if hour % 12 == 0 { 12 } else { hour % 12 };
    let meridiem = if hour < 12 { "AM" } else { "PM" };
    let mut out = String::new();
    let mut characters = format.chars();
    while let Some(character) = characters.next() {
        if character != '%' {
            out.push(character);
            continue;
        }
        let Some(specifier) = characters.next() else {
            break;
        };
        let needs_day = matches!(specifier, 'a' | 'W' | 'w' | 'j' | 'U' | 'u' | 'V' | 'v' | 'X' | 'x' | 'D');
        let days = match (needs_day, days) {
            (true, None) => return Ok(Value::Null),
            (_, days) => days.unwrap_or_default(),
        };
        let piece = match specifier {
            'a' => WEEKDAY_NAMES[weekday_of(days, true) as usize][..3].to_owned(),
            'W' => WEEKDAY_NAMES[weekday_of(days, true) as usize].to_owned(),
            'w' => weekday_of(days, true).to_string(),
            'b' => MONTH_NAMES.get((month - 1).max(0) as usize).filter(|_| month > 0).map_or(String::new(), |name| name[..3].to_owned()),
            'M' => MONTH_NAMES.get((month - 1).max(0) as usize).filter(|_| month > 0).map_or(String::new(), |name| (*name).to_owned()),
            'c' => month.to_string(),
            'm' => format!("{month:02}"),
            'D' => format!("{day}{}", english_suffix(day)),
            'd' => format!("{day:02}"),
            'e' => day.to_string(),
            'j' => format!("{:03}", days - datetime::days_from_civil(year, 1, 1) + 1),
            'Y' => format!("{year:04}"),
            'y' => format!("{:02}", year.rem_euclid(100)),
            'f' => format!("{micro:06}"),
            'H' => format!("{hour:02}"),
            'k' => hour.to_string(),
            'h' | 'I' => format!("{twelve:02}"),
            'l' => twelve.to_string(),
            'i' => format!("{minute:02}"),
            'S' | 's' => format!("{second:02}"),
            'p' => meridiem.to_owned(),
            'r' => format!("{twelve:02}:{minute:02}:{second:02} {meridiem}"),
            'T' => format!("{hour:02}:{minute:02}:{second:02}"),
            'U' => format!("{:02}", calendar_week(days, behaviour_of(0)).1),
            'u' => format!("{:02}", calendar_week(days, behaviour_of(1)).1),
            'V' => format!("{:02}", calendar_week(days, behaviour_of(2)).1),
            'v' => format!("{:02}", calendar_week(days, behaviour_of(3)).1),
            'X' => format!("{:04}", calendar_week(days, behaviour_of(2) | WEEK_YEAR).0),
            'x' => format!("{:04}", calendar_week(days, behaviour_of(3) | WEEK_YEAR).0),
            other => other.to_string(),
        };
        out.push_str(&piece);
    }
    Ok(Value::Text(out))
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
        let time = Value::Time(crate::datetime::parse_mysql_time("13:45:01.5").expect("a time"));
        assert_eq!(part("hour", time.clone()), Some(13));
        assert_eq!(part("microsecond", time), Some(500_000));
    }

    #[test]
    fn week_modes_follow_mysqls_table() {
        let week = |date: &str, mode: i64| match super::week(&text(date), &Value::BigInt(mode)) {
            Ok(Value::BigInt(week)) => week,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(week("2008-02-20", 0), 7);
        assert_eq!(week("2008-02-20", 1), 8);
        assert_eq!(week("2008-12-31", 1), 53);
        assert_eq!(week("2000-01-01", 0), 0);
        assert_eq!(week("2000-01-01", 2), 52);
        assert_eq!(week("2008-12-29", 3), 1);
        let year_week = super::year_week(&text("1987-01-01"), &Value::BigInt(0)).expect("a week");
        assert_eq!(year_week, Value::BigInt(198_652));
    }

    #[test]
    fn date_format_spells_every_specifier() {
        let formatted = |date: &str, format: &str| {
            super::date_format(&text(date), &Value::Text(format.to_owned())).expect("a format")
        };
        assert_eq!(
            formatted("2009-10-04 22:23:00", "%W %M %Y"),
            Value::Text("Sunday October 2009".to_owned())
        );
        assert_eq!(
            formatted("2007-10-04 22:23:00", "%H:%i:%s"),
            Value::Text("22:23:00".to_owned())
        );
        assert_eq!(
            formatted("1900-10-04 22:23:00", "%D %y %a %d %m %b %j"),
            Value::Text("4th 00 Thu 04 10 Oct 277".to_owned())
        );
        assert_eq!(
            formatted("1997-10-04 22:23:00", "%H %k %I %r %T %S %w"),
            Value::Text("22 22 10 10:23:00 PM 22:23:00 00 6".to_owned())
        );
        assert_eq!(formatted("1999-01-01", "%X %V"), Value::Text("1998 52".to_owned()));
        assert_eq!(formatted("0000-00-00", "%Y-%m-%d"), Value::Text("0000-00-00".to_owned()));
        assert_eq!(formatted("0000-00-00", "%W"), Value::Null);
    }
}
