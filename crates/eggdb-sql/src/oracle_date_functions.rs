use crate::datetime::{self, MICROS_PER_DAY, MICROS_PER_SECOND};
use crate::error::SqlResult;
use crate::numeric::Numeric;
use crate::oracle_datetime::{
    DAYS, date_of, local_of, not_a_day_of_the_week, not_recognized, year_in_range,
    year_out_of_range, zone_named, zone_text,
};
use crate::time_zones::TimeZone;
use crate::types::Value;

const MICROS_PER_HOUR: i64 = 3600 * MICROS_PER_SECOND;
const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SECOND;
const SECONDS_PER_DAY: i64 = 86_400;
const DAYS_PER_MONTH_IN_MONTHS_BETWEEN: i64 = 31;
const DAYS_PER_WEEK: i64 = 7;
const FIRST_ROUNDED_UP_MONTH: i64 = 7;
const FIRST_ROUNDED_UP_DAY: i64 = 16;
const YEARS_PER_CENTURY: i64 = 100;
const FIRST_ROUNDED_UP_YEAR_OF_CENTURY: i64 = 50;
const ABBREVIATION_LENGTH: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DateUnit {
    Century,
    Year,
    IsoYear,
    Quarter,
    Month,
    WeekOfYear,
    IsoWeek,
    WeekOfMonth,
    Day,
    StartOfWeek,
    Hour,
    Minute,
}

fn date_unit(text: Option<&str>) -> SqlResult<DateUnit> {
    let name = text.unwrap_or("DD").trim().to_ascii_uppercase();
    Ok(match name.as_str() {
        "CC" | "SCC" => DateUnit::Century,
        "SYYYY" | "YYYY" | "YEAR" | "SYEAR" | "YYY" | "YY" | "Y" => DateUnit::Year,
        "IYYY" | "IYY" | "IY" | "I" => DateUnit::IsoYear,
        "Q" => DateUnit::Quarter,
        "MONTH" | "MON" | "MM" | "RM" => DateUnit::Month,
        "WW" => DateUnit::WeekOfYear,
        "IW" => DateUnit::IsoWeek,
        "W" => DateUnit::WeekOfMonth,
        "DDD" | "DD" | "J" => DateUnit::Day,
        "DAY" | "DY" | "D" => DateUnit::StartOfWeek,
        "HH" | "HH12" | "HH24" => DateUnit::Hour,
        "MI" => DateUnit::Minute,
        _ => return Err(not_recognized()),
    })
}

fn whole_seconds(micros: i64) -> i64 {
    micros - micros.rem_euclid(MICROS_PER_SECOND)
}

fn iso_year_start(iso_year: i64) -> i64 {
    let fourth = datetime::days_from_civil(iso_year, 1, 4);
    let weekday_from_monday = (fourth + 5).rem_euclid(DAYS_PER_WEEK);
    fourth - weekday_from_monday
}

struct Period {
    start: i64,
    next: i64,
    rounds_up: bool,
}

fn day_period(local: i64, unit: DateUnit) -> Period {
    let days = local.div_euclid(MICROS_PER_DAY);
    let of_day = local.rem_euclid(MICROS_PER_DAY);
    let (year, month, day) = datetime::civil_from_days(days);
    let on = datetime::days_from_civil;
    let week = |start: i64| Period {
        start,
        next: start + DAYS_PER_WEEK,
        rounds_up: local - start * MICROS_PER_DAY >= DAYS_PER_WEEK * MICROS_PER_DAY / 2,
    };
    match unit {
        DateUnit::Century => {
            let first_year = (year - 1).div_euclid(YEARS_PER_CENTURY) * YEARS_PER_CENTURY + 1;
            Period {
                start: on(first_year, 1, 1),
                next: on(first_year + YEARS_PER_CENTURY, 1, 1),
                rounds_up: year - first_year >= FIRST_ROUNDED_UP_YEAR_OF_CENTURY,
            }
        }
        DateUnit::Year => Period {
            start: on(year, 1, 1),
            next: on(year + 1, 1, 1),
            rounds_up: month >= FIRST_ROUNDED_UP_MONTH,
        },
        DateUnit::IsoYear => {
            let (iso_year, _) = datetime::iso_week(days);
            Period {
                start: iso_year_start(iso_year),
                next: iso_year_start(iso_year + 1),
                rounds_up: days >= on(iso_year, FIRST_ROUNDED_UP_MONTH, 1),
            }
        }
        DateUnit::Quarter => {
            let first_month = (month - 1) / 3 * 3 + 1;
            let into_quarter = month - first_month;
            let (next_year, next_month) = if first_month == 10 {
                (year + 1, 1)
            } else {
                (year, first_month + 3)
            };
            Period {
                start: on(year, first_month, 1),
                next: on(next_year, next_month, 1),
                rounds_up: into_quarter == 2 || (into_quarter == 1 && day >= FIRST_ROUNDED_UP_DAY),
            }
        }
        DateUnit::Month => {
            let (next_year, next_month) = if month == 12 {
                (year + 1, 1)
            } else {
                (year, month + 1)
            };
            Period {
                start: on(year, month, 1),
                next: on(next_year, next_month, 1),
                rounds_up: day >= FIRST_ROUNDED_UP_DAY,
            }
        }
        DateUnit::WeekOfYear => {
            let first = on(year, 1, 1);
            week(first + (days - first) / DAYS_PER_WEEK * DAYS_PER_WEEK)
        }
        DateUnit::IsoWeek => week(days - (days + 5).rem_euclid(DAYS_PER_WEEK)),
        DateUnit::WeekOfMonth => {
            let first = on(year, month, 1);
            week(first + (days - first) / DAYS_PER_WEEK * DAYS_PER_WEEK)
        }
        DateUnit::StartOfWeek => week(days - (days + 6).rem_euclid(DAYS_PER_WEEK)),
        DateUnit::Day | DateUnit::Hour | DateUnit::Minute => Period {
            start: days,
            next: days + 1,
            rounds_up: of_day >= MICROS_PER_DAY / 2,
        },
    }
}

fn time_unit_start(local: i64, unit: i64, rounding: bool) -> i64 {
    let start = local - local.rem_euclid(unit);
    if rounding && local - start >= unit / 2 {
        start + unit
    } else {
        start
    }
}

fn moved(local: i64, unit: DateUnit, rounding: bool) -> SqlResult<i64> {
    let local = whole_seconds(local);
    let micros = match unit {
        DateUnit::Hour => time_unit_start(local, MICROS_PER_HOUR, rounding),
        DateUnit::Minute => time_unit_start(local, MICROS_PER_MINUTE, rounding),
        _ => {
            let period = day_period(local, unit);
            let day = if rounding && period.rounds_up {
                period.next
            } else {
                period.start
            };
            day * MICROS_PER_DAY
        }
    };
    let (year, _, _) = datetime::civil_from_days(micros.div_euclid(MICROS_PER_DAY));
    if !year_in_range(year) {
        return Err(year_out_of_range());
    }
    Ok(micros)
}

pub(crate) fn truncated_date(date: &Value, unit: Option<&str>) -> SqlResult<Value> {
    let Some(local) = local_of(date)? else {
        return Ok(Value::Null);
    };
    moved(local, date_unit(unit)?, false).map(Value::Timestamp)
}

pub(crate) fn rounded_date(date: &Value, unit: Option<&str>) -> SqlResult<Value> {
    let Some(local) = local_of(date)? else {
        return Ok(Value::Null);
    };
    moved(local, date_unit(unit)?, true).map(Value::Timestamp)
}

fn seconds_of_day(local: i64) -> i64 {
    local.rem_euclid(MICROS_PER_DAY) / MICROS_PER_SECOND
}

pub(crate) fn months_between(later: &Value, earlier: &Value) -> SqlResult<Value> {
    let (Some(first), Some(second)) = (date_of(later)?, date_of(earlier)?) else {
        return Ok(Value::Null);
    };
    let (year_one, month_one, day_one) =
        datetime::civil_from_days(first.div_euclid(MICROS_PER_DAY));
    let (year_two, month_two, day_two) =
        datetime::civil_from_days(second.div_euclid(MICROS_PER_DAY));
    let months = (year_one - year_two) * 12 + (month_one - month_two);
    let both_last = day_one == datetime::days_in_month(year_one, month_one)
        && day_two == datetime::days_in_month(year_two, month_two);
    if day_one == day_two || both_last {
        return Ok(Value::BigInt(months));
    }
    let month_seconds = DAYS_PER_MONTH_IN_MONTHS_BETWEEN * SECONDS_PER_DAY;
    let seconds = i128::from(months) * i128::from(month_seconds)
        + i128::from(day_one - day_two) * i128::from(SECONDS_PER_DAY)
        + i128::from(seconds_of_day(first) - seconds_of_day(second));
    crate::oracle_functions::exact_result(
        Numeric::from_i128(seconds).divide(&Numeric::from_i64(month_seconds)),
    )
}

fn weekday_named(text: &str) -> SqlResult<i64> {
    let upper = text.trim().to_ascii_uppercase();
    let abbreviation = upper
        .get(..ABBREVIATION_LENGTH)
        .ok_or_else(not_a_day_of_the_week)?;
    DAYS.iter()
        .position(|day| day.starts_with(abbreviation))
        .map(|index| index as i64)
        .ok_or_else(not_a_day_of_the_week)
}

pub(crate) fn next_day(date: &Value, day: &Value) -> SqlResult<Value> {
    let Some(local) = date_of(date)? else {
        return Ok(Value::Null);
    };
    let Some(name) = crate::oracle_functions::text_of(day) else {
        return Ok(Value::Null);
    };
    let target = weekday_named(&name)?;
    let days = local.div_euclid(MICROS_PER_DAY);
    let weekday = (days + 6).rem_euclid(DAYS_PER_WEEK);
    let ahead = match (target - weekday).rem_euclid(DAYS_PER_WEEK) {
        0 => DAYS_PER_WEEK,
        ahead => ahead,
    };
    let micros = whole_seconds(local) + ahead * MICROS_PER_DAY;
    let (year, _, _) = datetime::civil_from_days(micros.div_euclid(MICROS_PER_DAY));
    if !year_in_range(year) {
        return Err(year_out_of_range());
    }
    Ok(Value::Timestamp(micros))
}

pub(crate) fn from_time_zone(stamp: &Value, zone: &Value) -> SqlResult<Value> {
    let (Some(local), Some(zone)) = (local_of(stamp)?, crate::oracle_functions::text_of(zone))
    else {
        return Ok(Value::Null);
    };
    let zone = zone_named(&zone)?;
    datetime::checked_timestamp(zone.utc_of_local(local)).map(Value::TimestampTz)
}

pub(crate) fn utc_of_stamp(stamp: &Value) -> SqlResult<Value> {
    Ok(match stamp {
        Value::TimestampTz(micros) => Value::Timestamp(*micros),
        Value::Timestamp(local) => Value::Timestamp(datetime::local_to_utc(*local)?),
        Value::Date(days) => {
            Value::Timestamp(datetime::local_to_utc(datetime::date_to_timestamp(*days)?)?)
        }
        _ => Value::Null,
    })
}

pub(crate) fn zone_offset_text(zone: &Value) -> SqlResult<Value> {
    let Some(zone) = crate::oracle_functions::text_of(zone) else {
        return Ok(Value::Null);
    };
    let zone = zone_named(&zone)?;
    Ok(Value::Text(zone_text(zone.offset_at(datetime::statement_now()))))
}

pub(crate) fn session_time_zone(value: &str) -> SqlResult<TimeZone> {
    let trimmed = value.trim();
    let unquoted = trimmed
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .unwrap_or(trimmed);
    if unquoted.eq_ignore_ascii_case("local") || unquoted.eq_ignore_ascii_case("dbtimezone") {
        return Ok(TimeZone::UTC);
    }
    zone_named(unquoted)
}

pub(crate) fn time_zone_setting(zone: TimeZone) -> String {
    if let Some(name) = zone.region_name() {
        return name.to_owned();
    }
    let offset = zone.fixed_offset().unwrap_or(0);
    if offset == 0 {
        return "UTC".to_owned();
    }
    let west = if offset > 0 { '-' } else { '+' };
    let magnitude = offset.unsigned_abs();
    format!("UTC{west}{:02}:{:02}", magnitude / 3600, magnitude / 60 % 60)
}

#[cfg(test)]
mod tests {
    use super::{
        months_between, next_day, rounded_date, session_time_zone, time_zone_setting,
        truncated_date,
    };
    use crate::datetime::{MICROS_PER_DAY, MICROS_PER_SECOND, days_from_civil};
    use crate::time_zones::TimeZone;
    use crate::types::Value;

    fn at(year: i64, month: i64, day: i64, hour: i64, minute: i64) -> Value {
        Value::Timestamp(
            days_from_civil(year, month, day) * MICROS_PER_DAY
                + (hour * 3600 + minute * 60) * MICROS_PER_SECOND,
        )
    }

    fn unit(text: &str) -> Value {
        Value::Text(text.to_owned())
    }

    fn rounded(date: Value, name: &str) -> Value {
        rounded_date(&date, Some(name)).expect("round")
    }

    fn truncated(date: Value, name: &str) -> Value {
        truncated_date(&date, Some(name)).expect("trunc")
    }

    #[test]
    fn round_follows_the_rounding_table_of_the_date_functions() {
        assert_eq!(rounded(at(2024, 6, 30, 23, 0), "YEAR"), at(2024, 1, 1, 0, 0));
        assert_eq!(rounded(at(2024, 7, 1, 0, 0), "YYYY"), at(2025, 1, 1, 0, 0));
        assert_eq!(rounded(at(2024, 5, 15, 0, 0), "Q"), at(2024, 4, 1, 0, 0));
        assert_eq!(rounded(at(2024, 5, 16, 0, 0), "Q"), at(2024, 7, 1, 0, 0));
        assert_eq!(rounded(at(2024, 12, 1, 0, 0), "Q"), at(2025, 1, 1, 0, 0));
        assert_eq!(rounded(at(2024, 1, 15, 23, 0), "MM"), at(2024, 1, 1, 0, 0));
        assert_eq!(rounded(at(2024, 1, 16, 0, 0), "MONTH"), at(2024, 2, 1, 0, 0));
        assert_eq!(rounded(at(2024, 1, 1, 11, 59), "DD"), at(2024, 1, 1, 0, 0));
        assert_eq!(rounded(at(2024, 1, 1, 12, 0), "J"), at(2024, 1, 2, 0, 0));
        assert_eq!(rounded(at(2024, 1, 1, 10, 30), "HH"), at(2024, 1, 1, 11, 0));
        assert_eq!(rounded(at(2050, 3, 1, 0, 0), "CC"), at(2001, 1, 1, 0, 0));
        assert_eq!(rounded(at(2051, 3, 1, 0, 0), "CC"), at(2101, 1, 1, 0, 0));
        assert_eq!(rounded(at(2024, 1, 4, 12, 0), "DAY"), at(2024, 1, 7, 0, 0));
        assert_eq!(rounded(at(2024, 1, 3, 0, 0), "D"), at(2023, 12, 31, 0, 0));
        assert_eq!(rounded(at(2024, 1, 5, 0, 0), "IW"), at(2024, 1, 8, 0, 0));
        assert_eq!(rounded(at(2024, 1, 4, 0, 0), "WW"), at(2024, 1, 1, 0, 0));
        assert_eq!(rounded(at(2024, 1, 11, 0, 0), "W"), at(2024, 1, 8, 0, 0));
        assert!(rounded_date(&at(9999, 12, 31, 0, 0), Some("YYYY")).is_err());
        assert!(rounded_date(&at(2024, 1, 1, 0, 0), Some("XX")).is_err());
        assert_eq!(
            rounded_date(&Value::Null, Some("YYYY")).ok(),
            Some(Value::Null)
        );
    }

    #[test]
    fn trunc_takes_every_unit_of_the_table() {
        assert_eq!(truncated(at(1992, 10, 27, 13, 45), "YEAR"), at(1992, 1, 1, 0, 0));
        assert_eq!(truncated(at(2024, 5, 17, 13, 45), "Q"), at(2024, 4, 1, 0, 0));
        assert_eq!(truncated(at(2024, 5, 17, 13, 45), "HH"), at(2024, 5, 17, 13, 0));
        assert_eq!(truncated(at(2024, 5, 17, 13, 45), "CC"), at(2001, 1, 1, 0, 0));
        assert_eq!(truncated(at(2021, 1, 1, 0, 0), "IYYY"), at(2019, 12, 30, 0, 0));
        assert_eq!(truncated(at(2024, 5, 17, 0, 0), "DAY"), at(2024, 5, 12, 0, 0));
        assert_eq!(truncated(at(2024, 5, 17, 0, 0), "IW"), at(2024, 5, 13, 0, 0));
        assert_eq!(truncated(at(2024, 5, 17, 0, 0), "W"), at(2024, 5, 15, 0, 0));
        assert_eq!(truncated(at(2024, 5, 17, 0, 0), "WW"), at(2024, 5, 13, 0, 0));
    }

    #[test]
    fn months_between_counts_31_day_months_unless_the_days_agree() {
        let between = |later: Value, earlier: Value| {
            months_between(&later, &earlier)
                .ok()
                .and_then(|months| crate::Dialect::Oracle.value_text(&months))
        };
        assert!(
            between(at(1995, 2, 2, 0, 0), at(1995, 1, 1, 0, 0))
                .is_some_and(|months| months.starts_with("1.03225806451612"))
        );
        assert_eq!(
            between(at(2024, 3, 31, 10, 0), at(2024, 2, 29, 0, 0)).as_deref(),
            Some("1")
        );
        assert_eq!(
            between(at(2024, 1, 15, 0, 0), at(2024, 3, 15, 12, 0)).as_deref(),
            Some("-2")
        );
        assert_eq!(
            months_between(&Value::Null, &at(2024, 1, 1, 0, 0)).ok(),
            Some(Value::Null)
        );
    }

    #[test]
    fn next_day_takes_full_names_or_abbreviations() {
        let next = |date: Value, name: &str| next_day(&date, &unit(name));
        assert_eq!(next(at(2009, 10, 15, 9, 0), "TUESDAY").ok(), Some(at(2009, 10, 20, 9, 0)));
        assert_eq!(next(at(2009, 10, 20, 9, 0), "tue").ok(), Some(at(2009, 10, 27, 9, 0)));
        assert_eq!(
            next(at(2009, 10, 15, 0, 0), "Funday")
                .err()
                .and_then(|error| error.vendor_code()),
            Some(1846)
        );
        assert_eq!(
            next(at(2009, 10, 15, 0, 0), "TU")
                .err()
                .and_then(|error| error.vendor_code()),
            Some(1846)
        );
    }

    #[test]
    fn session_time_zones_become_settings_the_clock_reads() {
        let offset = |text: &str| session_time_zone(text).ok().and_then(TimeZone::fixed_offset);
        assert_eq!(offset("'-08:00'"), Some(-8 * 3600));
        assert_eq!(offset("local"), Some(0));
        assert_eq!(offset("'UTC'"), Some(0));
        let paris = session_time_zone("'Europe/Paris'").ok();
        assert_eq!(paris.and_then(TimeZone::region_name), Some("Europe/Paris"));
        assert_eq!(paris.map(time_zone_setting).as_deref(), Some("Europe/Paris"));
        assert_eq!(
            session_time_zone("'Mars/Olympus'")
                .err()
                .and_then(|error| error.vendor_code()),
            Some(1882)
        );
        for offset in [-8 * 3600, 5 * 3600 + 1800, 0] {
            let zone = TimeZone::fixed(offset).unwrap_or_default();
            assert_eq!(
                crate::datetime::time_zone_offset(&time_zone_setting(zone)),
                Some(offset)
            );
        }
    }
}
