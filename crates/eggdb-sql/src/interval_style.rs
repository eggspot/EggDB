use crate::interval::{Interval, MICROS_PER_HOUR, MICROS_PER_MINUTE, MICROS_PER_SECOND};
use std::fmt::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntervalStyle {
    Postgres,
    PostgresVerbose,
    SqlStandard,
    Iso8601,
}

impl IntervalStyle {
    pub(crate) fn named(text: &str) -> Option<Self> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "postgres" => Self::Postgres,
            "postgres_verbose" => Self::PostgresVerbose,
            "sql_standard" => Self::SqlStandard,
            "iso_8601" => Self::Iso8601,
            _ => return None,
        })
    }

    pub(crate) fn current() -> Self {
        crate::pg_catalog::statement_settings()
            .get("IntervalStyle")
            .ok()
            .and_then(|value| Self::named(&value))
            .unwrap_or(Self::Postgres)
    }
}

struct Parts {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    fraction: i64,
}

impl Parts {
    fn of(interval: &Interval) -> Self {
        let micros = interval.micros;
        Self {
            year: i64::from(interval.months / 12),
            month: i64::from(interval.months % 12),
            day: i64::from(interval.days),
            hour: micros / MICROS_PER_HOUR,
            minute: micros / MICROS_PER_MINUTE % 60,
            second: micros / MICROS_PER_SECOND % 60,
            fraction: micros % MICROS_PER_SECOND,
        }
    }
}

fn fraction_digits(fraction: i64) -> String {
    let digits = format!("{:06}", fraction.unsigned_abs());
    digits.trim_end_matches('0').to_owned()
}

fn seconds(second: i64, fraction: i64, padded: bool) -> String {
    let whole = second.unsigned_abs();
    let mut out = if padded {
        format!("{whole:02}")
    } else {
        whole.to_string()
    };
    if fraction != 0 {
        out.push('.');
        out.push_str(&fraction_digits(fraction));
    }
    out
}

fn verbose(interval: &Interval) -> String {
    let parts = Parts::of(interval);
    let mut out = String::from("@");
    let mut is_zero = true;
    let mut is_before = false;
    let mut part = |out: &mut String, value: i64, unit: &str| {
        if value == 0 {
            return;
        }
        let shown = if is_zero {
            is_before = value < 0;
            value.abs()
        } else if is_before {
            -value
        } else {
            value
        };
        is_zero = false;
        let plural = if shown != 1 { "s" } else { "" };
        let _ = write!(out, " {shown} {unit}{plural}");
    };
    part(&mut out, parts.year, "year");
    part(&mut out, parts.month, "mon");
    part(&mut out, parts.day, "day");
    part(&mut out, parts.hour, "hour");
    part(&mut out, parts.minute, "min");
    if parts.second != 0 || parts.fraction != 0 {
        let negative = parts.second < 0 || parts.fraction < 0;
        let sign = if negative {
            if is_zero {
                is_before = true;
                ""
            } else if is_before {
                ""
            } else {
                "-"
            }
        } else if is_before {
            "-"
        } else {
            ""
        };
        let plural = if parts.second.abs() != 1 || parts.fraction != 0 {
            "s"
        } else {
            ""
        };
        let _ = write!(
            out,
            " {sign}{} sec{plural}",
            seconds(parts.second, parts.fraction, false)
        );
        is_zero = false;
    }
    if is_zero {
        out.push_str(" 0");
    }
    if is_before {
        out.push_str(" ago");
    }
    out
}

fn sql_standard(interval: &Interval) -> String {
    let parts = Parts::of(interval);
    let values = [
        parts.year,
        parts.month,
        parts.day,
        parts.hour,
        parts.minute,
        parts.second,
        parts.fraction,
    ];
    let has_negative = values.iter().any(|value| *value < 0);
    let has_positive = values.iter().any(|value| *value > 0);
    let has_year_month = parts.year != 0 || parts.month != 0;
    let has_day_time = values[2..].iter().any(|value| *value != 0);
    let standard = !(has_negative && has_positive) && !(has_year_month && has_day_time);
    if !has_negative && !has_positive {
        return "0".to_owned();
    }
    let time_negative =
        parts.hour < 0 || parts.minute < 0 || parts.second < 0 || parts.fraction < 0;
    if !standard {
        let year_sign = if parts.year < 0 || parts.month < 0 { '-' } else { '+' };
        let day_sign = if parts.day < 0 { '-' } else { '+' };
        let time_sign = if time_negative { '-' } else { '+' };
        return format!(
            "{year_sign}{}-{} {day_sign}{} {time_sign}{}:{:02}:{}",
            parts.year.abs(),
            parts.month.abs(),
            parts.day.abs(),
            parts.hour.abs(),
            parts.minute.abs(),
            seconds(parts.second, parts.fraction, true)
        );
    }
    let sign = if has_negative { "-" } else { "" };
    if has_year_month {
        return format!("{sign}{}-{}", parts.year.abs(), parts.month.abs());
    }
    let time = format!(
        "{}:{:02}:{}",
        parts.hour.abs(),
        parts.minute.abs(),
        seconds(parts.second, parts.fraction, true)
    );
    if parts.day != 0 {
        format!("{sign}{} {time}", parts.day.abs())
    } else {
        format!("{sign}{time}")
    }
}

fn iso_8601(interval: &Interval) -> String {
    let parts = Parts::of(interval);
    let mut out = String::from("P");
    let mut add = |out: &mut String, value: i64, unit: char| {
        if value != 0 {
            let _ = write!(out, "{value}{unit}");
        }
    };
    add(&mut out, parts.year, 'Y');
    add(&mut out, parts.month, 'M');
    add(&mut out, parts.day, 'D');
    let has_time =
        parts.hour != 0 || parts.minute != 0 || parts.second != 0 || parts.fraction != 0;
    if has_time {
        out.push('T');
        add(&mut out, parts.hour, 'H');
        add(&mut out, parts.minute, 'M');
        if parts.second != 0 || parts.fraction != 0 {
            let sign = if parts.second < 0 || parts.fraction < 0 { "-" } else { "" };
            let _ = write!(out, "{sign}{}S", seconds(parts.second, parts.fraction, false));
        }
    }
    if out == "P" {
        return "PT0S".to_owned();
    }
    out
}

pub(crate) fn styled(interval: &Interval, style: IntervalStyle) -> String {
    if !interval.is_finite() {
        return interval.to_string();
    }
    match style {
        IntervalStyle::Postgres => interval.to_string(),
        IntervalStyle::PostgresVerbose => verbose(interval),
        IntervalStyle::SqlStandard => sql_standard(interval),
        IntervalStyle::Iso8601 => iso_8601(interval),
    }
}
