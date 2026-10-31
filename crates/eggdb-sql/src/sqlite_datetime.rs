use crate::datetime::{civil_from_days, days_from_civil, session_offset_at, statement_now};
use crate::types::Value;

const MS_PER_DAY: i64 = 86_400_000;
const POSTGRES_EPOCH_JD_MS: i64 = 211_813_444_800_000;
const UNIX_EPOCH_JD_MS: i64 = 210_866_760_000_000;
const MAX_JD_MS: i64 = 464_269_060_799_999;
const MAX_FORMAT_BYTES: usize = 1 << 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DateFunction {
    Date,
    Time,
    DateTime,
    JulianDay,
    UnixEpoch,
}

impl DateFunction {
    pub(crate) fn of_internal_name(name: &str) -> Option<Self> {
        Some(match name {
            "sqlite_date" => Self::Date,
            "sqlite_time" => Self::Time,
            "sqlite_datetime" => Self::DateTime,
            "sqlite_julianday" => Self::JulianDay,
            "sqlite_unixepoch" => Self::UnixEpoch,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct Moment {
    jd_ms: i64,
    subsecond: bool,
    pending_number: Option<f64>,
    overflow_days: i64,
}

#[derive(Debug, Clone, Copy)]
struct Civil {
    year: i64,
    month: i64,
    day: i64,
    ms_of_day: i64,
}

impl Moment {
    fn at(jd_ms: i64) -> Self {
        Self {
            jd_ms,
            subsecond: false,
            pending_number: None,
            overflow_days: 0,
        }
    }

    fn civil(&self) -> Civil {
        let shifted = self.jd_ms + MS_PER_DAY / 2;
        let days_since_2000 = shifted.div_euclid(MS_PER_DAY) - 2_451_545;
        let (year, month, day) = civil_from_days(days_since_2000);
        Civil {
            year,
            month,
            day,
            ms_of_day: shifted.rem_euclid(MS_PER_DAY),
        }
    }
}

fn jd_ms_of(year: i64, month: i64, day: i64, ms_of_day: i64) -> Option<i64> {
    let normalized_year = year.checked_add((month - 1).div_euclid(12))?;
    let normalized_month = (month - 1).rem_euclid(12) + 1;
    if !(-10_000..=20_000).contains(&normalized_year) {
        return None;
    }
    let days = days_from_civil(normalized_year, normalized_month, 1) + day - 1;
    (days + 2_451_545)
        .checked_mul(MS_PER_DAY)?
        .checked_sub(MS_PER_DAY / 2)?
        .checked_add(ms_of_day)
}

fn in_range(jd_ms: i64) -> bool {
    (0..=MAX_JD_MS).contains(&jd_ms)
}

fn number_of(value: &Value) -> Option<f64> {
    match value {
        Value::SmallInt(number) => Some(f64::from(*number)),
        Value::Integer(number) => Some(f64::from(*number)),
        Value::BigInt(number) => Some(*number as f64),
        Value::Real(number) => Some(f64::from(*number)),
        Value::Double(number) => Some(*number),
        _ => None,
    }
}

fn text_of(value: &Value) -> Option<String> {
    crate::dialect::Dialect::Sqlite.value_text(value)
}

fn digits(text: &[u8], at: &mut usize, count: usize) -> Option<i64> {
    let slice = text.get(*at..*at + count)?;
    if !slice.iter().all(u8::is_ascii_digit) {
        return None;
    }
    *at += count;
    Some(slice.iter().fold(0, |total, digit| total * 10 + i64::from(digit - b'0')))
}

fn take(text: &[u8], at: &mut usize, byte: u8) -> bool {
    if text.get(*at) == Some(&byte) {
        *at += 1;
        true
    } else {
        false
    }
}

fn time_of_day(text: &[u8], at: &mut usize) -> Option<i64> {
    let hours = digits(text, at, 2)?;
    if !take(text, at, b':') {
        return None;
    }
    let minutes = digits(text, at, 2)?;
    let mut ms = 0;
    if take(text, at, b':') {
        ms = digits(text, at, 2)? * 1000;
        if take(text, at, b'.') {
            let start = *at;
            let mut fraction = 0;
            let mut scale = 1000;
            while let Some(digit) = text.get(*at).filter(|byte| byte.is_ascii_digit()) {
                if scale > 1 {
                    scale /= 10;
                    fraction += i64::from(digit - b'0') * scale;
                }
                *at += 1;
            }
            if *at == start {
                return None;
            }
            ms += fraction;
        }
    }
    if hours > 24 || minutes > 59 || ms >= 60_000 {
        return None;
    }
    Some(hours * 3_600_000 + minutes * 60_000 + ms)
}

fn zone_offset_ms(text: &[u8], at: &mut usize) -> Option<i64> {
    while text.get(*at) == Some(&b' ') {
        *at += 1;
    }
    match text.get(*at) {
        None => Some(0),
        Some(b'Z' | b'z') => {
            *at += 1;
            Some(0)
        }
        Some(sign @ (b'+' | b'-')) => {
            let negative = *sign == b'-';
            *at += 1;
            let hours = digits(text, at, 2)?;
            if !take(text, at, b':') {
                return None;
            }
            let minutes = digits(text, at, 2)?;
            if hours > 14 || minutes > 59 {
                return None;
            }
            let offset = hours * 3_600_000 + minutes * 60_000;
            Some(if negative { -offset } else { offset })
        }
        Some(_) => None,
    }
}

fn parse_text(text: &str) -> Option<i64> {
    let bytes = text.trim().as_bytes();
    let mut at = 0;
    let (year, month, day) = if bytes.get(4) == Some(&b'-') {
        let year = digits(bytes, &mut at, 4)?;
        take(bytes, &mut at, b'-');
        let month = digits(bytes, &mut at, 2)?;
        if !take(bytes, &mut at, b'-') {
            return None;
        }
        let day = digits(bytes, &mut at, 2)?;
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        (year, month, day)
    } else {
        (2000, 1, 1)
    };
    let date_given = at > 0;
    let mut ms_of_day = 0;
    if date_given && matches!(bytes.get(at), Some(b' ' | b'T' | b't')) {
        let mut probe = at + 1;
        while bytes.get(probe) == Some(&b' ') {
            probe += 1;
        }
        if bytes.get(probe).is_some_and(u8::is_ascii_digit) {
            at = probe;
            ms_of_day = time_of_day(bytes, &mut at)?;
        }
    } else if !date_given {
        ms_of_day = time_of_day(bytes, &mut at)?;
    }
    let offset = zone_offset_ms(bytes, &mut at)?;
    if at != bytes.len() {
        return None;
    }
    jd_ms_of(year, month, day, ms_of_day)?.checked_sub(offset)
}

fn jd_ms_of_julian_day(number: f64) -> Option<i64> {
    let ms = (number * MS_PER_DAY as f64).round();
    (ms.is_finite() && ms.abs() < 9.0e15).then_some(ms as i64)
}

fn now_jd_ms() -> i64 {
    POSTGRES_EPOCH_JD_MS + statement_now().div_euclid(1000)
}

fn start(value: &Value) -> Option<Moment> {
    if let Some(number) = number_of(value) {
        let mut moment = Moment::at(jd_ms_of_julian_day(number).unwrap_or(i64::MIN / 2));
        moment.pending_number = Some(number);
        return Some(moment);
    }
    let text = text_of(value)?;
    let trimmed = text.trim();
    if trimmed.eq_ignore_ascii_case("now") {
        return Some(Moment::at(now_jd_ms()));
    }
    if let Ok(number) = trimmed.parse::<f64>() {
        let mut moment = Moment::at(jd_ms_of_julian_day(number).unwrap_or(i64::MIN / 2));
        moment.pending_number = Some(number);
        return Some(moment);
    }
    parse_text(trimmed).map(Moment::at)
}

fn add_months(moment: &mut Moment, months: i64) -> Option<()> {
    let civil = moment.civil();
    let target_month = civil.month.checked_add(months)?;
    let normalized_year = civil.year + (target_month - 1).div_euclid(12);
    let normalized_month = (target_month - 1).rem_euclid(12) + 1;
    let length = crate::datetime::days_in_month(normalized_year, normalized_month);
    moment.overflow_days = (civil.day - length).max(0);
    moment.jd_ms = jd_ms_of(civil.year, target_month, civil.day, civil.ms_of_day)?;
    Some(())
}

fn unit_ms(unit: &str) -> Option<i64> {
    Some(match unit {
        "day" | "days" => MS_PER_DAY,
        "hour" | "hours" => 3_600_000,
        "minute" | "minutes" => 60_000,
        "second" | "seconds" => 1000,
        _ => return None,
    })
}

fn shift_by_amount(moment: &mut Moment, amount: f64, unit: &str) -> Option<()> {
    moment.overflow_days = 0;
    match unit {
        "month" | "months" => {
            let whole = amount.trunc();
            add_months(moment, whole as i64)?;
            let fraction = ((amount - whole) * 30.0 * MS_PER_DAY as f64).round();
            moment.jd_ms = moment.jd_ms.checked_add(fraction as i64)?;
        }
        "year" | "years" => {
            let whole = amount.trunc();
            add_months(moment, (whole as i64).checked_mul(12)?)?;
            let fraction = ((amount - whole) * 365.0 * MS_PER_DAY as f64).round();
            moment.jd_ms = moment.jd_ms.checked_add(fraction as i64)?;
        }
        other => {
            let delta = (amount * unit_ms(other)? as f64).round();
            if !delta.is_finite() || delta.abs() > 9.0e15 {
                return None;
            }
            moment.jd_ms = moment.jd_ms.checked_add(delta as i64)?;
        }
    }
    Some(())
}

fn shift_by_clock(moment: &mut Moment, modifier: &str) -> Option<()> {
    let negative = modifier.starts_with('-');
    let body = modifier.trim_start_matches(['+', '-']);
    let bytes = body.as_bytes();
    let mut at = 0;
    let (years, months, days) = if bytes.get(4) == Some(&b'-') {
        let years = digits(bytes, &mut at, 4)?;
        take(bytes, &mut at, b'-');
        let months = digits(bytes, &mut at, 2)?;
        if !take(bytes, &mut at, b'-') {
            return None;
        }
        let days = digits(bytes, &mut at, 2)?;
        if months > 11 || days > 30 {
            return None;
        }
        while bytes.get(at) == Some(&b' ') {
            at += 1;
        }
        (years, months, days)
    } else {
        (0, 0, 0)
    };
    let clock = if at < bytes.len() {
        time_of_day(bytes, &mut at)?
    } else {
        0
    };
    if at != bytes.len() {
        return None;
    }
    let sign = if negative { -1 } else { 1 };
    moment.overflow_days = 0;
    if years != 0 || months != 0 {
        add_months(moment, sign * (years * 12 + months))?;
    }
    moment.jd_ms = moment
        .jd_ms
        .checked_add(sign * (days * MS_PER_DAY + clock))?;
    Some(())
}

fn local_offset_ms(utc_jd_ms: i64) -> i64 {
    let micros = (utc_jd_ms - POSTGRES_EPOCH_JD_MS).saturating_mul(1000);
    i64::from(session_offset_at(micros)) * 1000
}

fn apply(moment: &mut Moment, modifier: &str) -> Option<()> {
    let lowered = modifier.trim().to_ascii_lowercase();
    let pending = moment.pending_number.take();
    match lowered.as_str() {
        "unixepoch" => {
            let seconds = pending?;
            moment.jd_ms = UNIX_EPOCH_JD_MS.checked_add(jd_ms_of_julian_day(seconds / 86_400.0)?)?;
            return Some(());
        }
        "julianday" => {
            pending?;
            return Some(());
        }
        "auto" => {
            let number = pending?;
            if (0.0..5_373_484.5).contains(&number) {
                return Some(());
            }
            if (-210_866_760_000.0..=253_402_300_799.0).contains(&number) {
                moment.jd_ms =
                    UNIX_EPOCH_JD_MS.checked_add(jd_ms_of_julian_day(number / 86_400.0)?)?;
                return Some(());
            }
            return None;
        }
        "subsec" | "subsecond" => {
            moment.subsecond = true;
            return Some(());
        }
        "ceiling" => {
            moment.overflow_days = 0;
            return Some(());
        }
        "floor" => {
            moment.jd_ms -= moment.overflow_days * MS_PER_DAY;
            moment.overflow_days = 0;
            return Some(());
        }
        "localtime" => {
            moment.jd_ms += local_offset_ms(moment.jd_ms);
            return Some(());
        }
        "utc" => {
            let guess = moment.jd_ms - local_offset_ms(moment.jd_ms);
            moment.jd_ms -= local_offset_ms(guess);
            return Some(());
        }
        "start of day" | "start of month" | "start of year" => {
            let civil = moment.civil();
            let (month, day) = match lowered.as_str() {
                "start of day" => (civil.month, civil.day),
                "start of month" => (civil.month, 1),
                _ => (1, 1),
            };
            moment.jd_ms = jd_ms_of(civil.year, month, day, 0)?;
            moment.overflow_days = 0;
            return Some(());
        }
        _ => {}
    }
    if let Some(weekday) = lowered.strip_prefix("weekday ") {
        let target: f64 = weekday.trim().parse().ok()?;
        if target.fract() != 0.0 || !(0.0..=6.0).contains(&target) {
            return None;
        }
        let current = (moment.jd_ms + MS_PER_DAY / 2 + MS_PER_DAY).div_euclid(MS_PER_DAY) % 7;
        let ahead = (target as i64 - current).rem_euclid(7);
        moment.jd_ms += ahead * MS_PER_DAY;
        return Some(());
    }
    if lowered.starts_with(['+', '-']) && lowered[1..].contains([':', '-']) {
        return shift_by_clock(moment, &lowered);
    }
    let (amount, unit) = lowered.split_once(|c: char| c.is_ascii_whitespace())?;
    let amount: f64 = amount.parse().ok()?;
    shift_by_amount(moment, amount, unit.trim())
}

fn evaluate(values: &[Value]) -> Option<Moment> {
    let mut moment = match values.first() {
        None => Moment::at(now_jd_ms()),
        Some(value) => start(value)?,
    };
    for modifier in values.iter().skip(1) {
        apply(&mut moment, &text_of(modifier)?)?;
    }
    in_range(moment.jd_ms).then_some(moment)
}

fn date_text(civil: &Civil) -> String {
    format!("{:04}-{:02}-{:02}", civil.year, civil.month, civil.day)
}

fn time_text(civil: &Civil, subsecond: bool) -> String {
    let seconds = civil.ms_of_day / 1000;
    let base = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    if subsecond {
        format!("{base}.{:03}", civil.ms_of_day % 1000)
    } else {
        base
    }
}

/// Runs one of SQLite's date and time functions on a time value and its modifiers; an invalid
/// time value or modifier gives NULL, as SQLite does.
pub(crate) fn call(function: DateFunction, values: &[Value]) -> Value {
    if values.iter().any(Value::is_null) {
        return Value::Null;
    }
    let Some(moment) = evaluate(values) else {
        return Value::Null;
    };
    let civil = moment.civil();
    match function {
        DateFunction::Date => Value::Text(date_text(&civil)),
        DateFunction::Time => Value::Text(time_text(&civil, moment.subsecond)),
        DateFunction::DateTime => Value::Text(format!(
            "{} {}",
            date_text(&civil),
            time_text(&civil, moment.subsecond)
        )),
        DateFunction::JulianDay => Value::Double(moment.jd_ms as f64 / MS_PER_DAY as f64),
        DateFunction::UnixEpoch => {
            let ms = moment.jd_ms - UNIX_EPOCH_JD_MS;
            if moment.subsecond {
                Value::Double(ms as f64 / 1000.0)
            } else {
                Value::BigInt(ms.div_euclid(1000))
            }
        }
    }
}

fn day_of_year(civil: &Civil) -> i64 {
    days_from_civil(civil.year, civil.month, civil.day) - days_from_civil(civil.year, 1, 1) + 1
}

fn day_of_week(moment: &Moment) -> i64 {
    (moment.jd_ms + MS_PER_DAY / 2 + MS_PER_DAY).div_euclid(MS_PER_DAY) % 7
}

fn iso_week(civil: &Civil, weekday_sunday_zero: i64) -> (i64, i64) {
    let iso_weekday = if weekday_sunday_zero == 0 { 7 } else { weekday_sunday_zero };
    let thursday = days_from_civil(civil.year, civil.month, civil.day) + (4 - iso_weekday);
    let (year, _, _) = civil_from_days(thursday);
    let week = (thursday - days_from_civil(year, 1, 1)) / 7 + 1;
    (year, week)
}

/// SQLite's `strftime(format, time value, modifiers...)`.
pub(crate) fn strftime(values: &[Value]) -> Value {
    let Some(format) = values.first().and_then(text_of) else {
        return Value::Null;
    };
    if values.iter().any(Value::is_null) {
        return Value::Null;
    }
    let Some(moment) = evaluate(&values[1..]) else {
        return Value::Null;
    };
    let civil = moment.civil();
    let weekday = day_of_week(&moment);
    let seconds = civil.ms_of_day / 1000;
    let hour = seconds / 3600;
    let twelve = if hour % 12 == 0 { 12 } else { hour % 12 };
    let mut out = String::new();
    let mut characters = format.chars();
    while let Some(character) = characters.next() {
        if out.len() > MAX_FORMAT_BYTES {
            return Value::Null;
        }
        if character != '%' {
            out.push(character);
            continue;
        }
        let piece = match characters.next() {
            Some('d') => format!("{:02}", civil.day),
            Some('e') => format!("{:2}", civil.day),
            Some('f') => format!("{:02}.{:03}", seconds % 60, civil.ms_of_day % 1000),
            Some('F') => date_text(&civil),
            Some('H') => format!("{hour:02}"),
            Some('k') => format!("{hour:2}"),
            Some('I') => format!("{twelve:02}"),
            Some('l') => format!("{twelve:2}"),
            Some('j') => format!("{:03}", day_of_year(&civil)),
            Some('J') => crate::sqlite_functions::real_text(moment.jd_ms as f64 / MS_PER_DAY as f64)
                .unwrap_or_default(),
            Some('m') => format!("{:02}", civil.month),
            Some('M') => format!("{:02}", seconds / 60 % 60),
            Some('p') => (if hour < 12 { "AM" } else { "PM" }).to_owned(),
            Some('P') => (if hour < 12 { "am" } else { "pm" }).to_owned(),
            Some('R') => format!("{hour:02}:{:02}", seconds / 60 % 60),
            Some('T') => time_text(&civil, false),
            Some('s') => {
                let ms = moment.jd_ms - UNIX_EPOCH_JD_MS;
                if moment.subsecond {
                    format!("{:.3}", ms as f64 / 1000.0)
                } else {
                    ms.div_euclid(1000).to_string()
                }
            }
            Some('S') => format!("{:02}", seconds % 60),
            Some('u') => (if weekday == 0 { 7 } else { weekday }).to_string(),
            Some('w') => weekday.to_string(),
            Some('U') => format!("{:02}", (day_of_year(&civil) + 6 - weekday) / 7),
            Some('W') => format!(
                "{:02}",
                (day_of_year(&civil) + 6 - (weekday + 6) % 7) / 7
            ),
            Some('V') => format!("{:02}", iso_week(&civil, weekday).1),
            Some('G') => format!("{:04}", iso_week(&civil, weekday).0),
            Some('g') => format!("{:02}", iso_week(&civil, weekday).0 % 100),
            Some('Y') => format!("{:04}", civil.year),
            Some('%') => "%".to_owned(),
            _ => return Value::Null,
        };
        out.push_str(&piece);
    }
    Value::Text(out)
}

/// SQLite's `timediff(a, b)`: the time from `b` to `a` as `(+|-)YYYY-MM-DD HH:MM:SS.SSS`.
pub(crate) fn timediff(values: &[Value]) -> Value {
    let (Some(later), Some(earlier)) = (
        values.first().and_then(|value| evaluate(std::slice::from_ref(value))),
        values.get(1).and_then(|value| evaluate(std::slice::from_ref(value))),
    ) else {
        return Value::Null;
    };
    let (sign, high, low) = if later.jd_ms >= earlier.jd_ms {
        ('+', later, earlier)
    } else {
        ('-', earlier, later)
    };
    let (from, to) = (low.civil(), high.civil());
    let mut months = (to.year - from.year) * 12 + (to.month - from.month);
    let mut probe = low;
    if add_months(&mut probe, months).is_none() {
        return Value::Null;
    }
    if probe.jd_ms > high.jd_ms {
        months -= 1;
        probe = low;
        if add_months(&mut probe, months).is_none() {
            return Value::Null;
        }
    }
    let rest = high.jd_ms - probe.jd_ms;
    let days = rest / MS_PER_DAY;
    let ms = rest % MS_PER_DAY;
    Value::Text(format!(
        "{sign}{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        months / 12,
        months % 12,
        days,
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Value {
        Value::Text(value.to_owned())
    }

    fn run(function: DateFunction, arguments: &[&str]) -> Value {
        let values: Vec<Value> = arguments.iter().map(|argument| text(argument)).collect();
        call(function, &values)
    }

    #[test]
    fn the_documented_examples_give_the_documented_answers() {
        assert_eq!(
            run(DateFunction::Date, &["2023-01-31", "+1 month"]),
            text("2023-03-03")
        );
        assert_eq!(
            run(DateFunction::Date, &["2023-01-31", "+1 month", "floor"]),
            text("2023-02-28")
        );
        assert_eq!(
            run(DateFunction::Date, &["2013-10-07", "start of month", "+1 month", "-1 day"]),
            text("2013-10-31")
        );
        assert_eq!(
            run(DateFunction::Date, &["2024-10-01", "weekday 2"]),
            text("2024-10-01")
        );
        assert_eq!(
            run(DateFunction::Date, &["2024-10-02", "weekday 2"]),
            text("2024-10-08")
        );
        assert_eq!(
            call(DateFunction::DateTime, &[Value::BigInt(1_092_941_466), text("unixepoch")]),
            text("2004-08-19 18:51:06")
        );
        assert_eq!(
            call(DateFunction::DateTime, &[Value::BigInt(1_092_941_466), text("auto")]),
            text("2004-08-19 18:51:06")
        );
        assert_eq!(
            run(DateFunction::JulianDay, &["2000-01-01 12:00:00"]),
            Value::Double(2_451_545.0)
        );
        assert_eq!(
            run(DateFunction::UnixEpoch, &["2004-01-01 02:34:56"]),
            Value::BigInt(1_072_924_496)
        );
        assert_eq!(
            run(DateFunction::Time, &["12:34:56.789", "subsec"]),
            text("12:34:56.789")
        );
        assert_eq!(
            run(DateFunction::DateTime, &["2024-06-30T23:30:00+02:00"]),
            text("2024-06-30 21:30:00")
        );
        assert_eq!(
            run(DateFunction::DateTime, &["2024-01-01", "+0001-02-03 04:05:06"]),
            text("2025-03-04 04:05:06")
        );
    }

    #[test]
    fn invalid_values_and_modifiers_give_null() {
        for arguments in [
            &["2024-13-01"][..],
            &["2024-01-01", "+1 fortnight"],
            &["2024-01-01", "unixepoch"],
            &["2024-01-01", "weekday 7"],
            &["not a date"],
            &["9999-12-31", "+1 day"],
            &["24:61"],
        ] {
            assert_eq!(run(DateFunction::Date, arguments), Value::Null, "{arguments:?}");
        }
    }

    #[test]
    fn strftime_formats_every_documented_field() {
        let values = [
            text("%Y-%m-%d %H:%M:%S %j %w %u %W %U %V %G %s %f %p %I %e"),
            text("2024-03-05 15:04:09.250"),
        ];
        assert_eq!(
            strftime(&values),
            text("2024-03-05 15:04:09 065 2 2 10 09 10 2024 1709651049 09.250 PM 03  5")
        );
        assert_eq!(strftime(&[text("%Q"), text("2024-01-01")]), Value::Null);
    }

    #[test]
    fn timediff_counts_years_months_and_days() {
        assert_eq!(
            timediff(&[text("2023-02-15"), text("2023-01-14 12:00:00")]),
            text("+0000-01-00 12:00:00.000")
        );
        assert_eq!(
            timediff(&[text("2023-01-14 12:00:00"), text("2023-02-15")]),
            text("-0000-01-00 12:00:00.000")
        );
    }

    #[test]
    fn hostile_modifiers_never_panic() {
        for modifier in [
            "+1e308 days",
            "-9999999999999 months",
            "+99999999999999999999 years",
            "nan days",
            "+",
            "-:",
            "weekday",
            "+9999-99-99",
        ] {
            let _ = run(DateFunction::DateTime, &["2024-01-01", modifier]);
        }
    }
}
