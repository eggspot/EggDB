use crate::value::Value;
use std::cmp::Ordering;

const NANOS_PER_SECOND: i64 = 1_000_000_000;
const SECONDS_PER_DAY: i64 = 86_400;
const NANOS_PER_DAY: i64 = SECONDS_PER_DAY * NANOS_PER_SECOND;
const MOST_OFFSET_SECONDS: i32 = 18 * 3600;
const MOST_YEAR: i64 = 999_999_999;

/// The SRID of 2D Cartesian points.
pub const CARTESIAN: i32 = 7203;
/// The SRID of 3D Cartesian points.
pub const CARTESIAN_3D: i32 = 9157;
/// The SRID of 2D WGS 84 points.
pub const WGS_84: i32 = 4326;
/// The SRID of 3D WGS 84 points.
pub const WGS_84_3D: i32 = 4979;

/// The Cypher functions that build temporal and spatial values.
pub(crate) const FUNCTIONS: [&str; 7] = ["date", "localtime", "time", "localdatetime", "datetime", "duration", "point"];

/// A Cypher temporal value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Temporal {
    /// A date: days since 1970-01-01.
    Date {
        /// Days since the epoch.
        days: i64,
    },
    /// A time of day with no offset.
    LocalTime {
        /// Nanoseconds since midnight.
        nanos: i64,
    },
    /// A time of day at an offset from UTC.
    Time {
        /// Nanoseconds since local midnight.
        nanos: i64,
        /// The offset from UTC, in seconds.
        offset: i32,
    },
    /// A date and time with no offset.
    LocalDateTime {
        /// Seconds since the epoch, read as local time.
        seconds: i64,
        /// Nanoseconds within the second.
        nanos: i32,
    },
    /// An instant at an offset from UTC.
    DateTime {
        /// Seconds since the epoch, in UTC.
        seconds: i64,
        /// Nanoseconds within the second.
        nanos: i32,
        /// The offset from UTC, in seconds.
        offset: i32,
    },
    /// An amount of time in months, days, seconds and nanoseconds, each kept apart.
    Duration {
        /// Months.
        months: i64,
        /// Days.
        days: i64,
        /// Seconds.
        seconds: i64,
        /// Nanoseconds.
        nanos: i32,
    },
}

/// A Cypher point: an SRID and two or three coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    /// The coordinate reference system: 7203, 9157, 4326 or 4979.
    pub srid: i32,
    /// `x`, or the longitude.
    pub x: f64,
    /// `y`, or the latitude.
    pub y: f64,
    /// `z`, or the height, for 3D points.
    pub z: Option<f64>,
}

pub(crate) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn checked_date(year: i64, month: i64, day: i64) -> Option<i64> {
    ((-MOST_YEAR..=MOST_YEAR).contains(&year) && (1..=12).contains(&month) && (1..=days_in_month(year, month)).contains(&day))
        .then(|| days_from_civil(year, month, day))
}

fn checked_time(hour: i64, minute: i64, second: i64, nanos: i64) -> Option<i64> {
    ((0..24).contains(&hour) && (0..60).contains(&minute) && (0..60).contains(&second) && (0..NANOS_PER_SECOND).contains(&nanos))
        .then(|| ((hour * 60 + minute) * 60 + second) * NANOS_PER_SECOND + nanos)
}

fn digits(text: &str) -> Option<i64> {
    (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())).then(|| text.parse().ok()).flatten()
}

fn parse_date(text: &str) -> Option<i64> {
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text.strip_prefix('+').unwrap_or(text)),
    };
    let parts: Vec<&str> = rest.split('-').collect();
    match parts.as_slice() {
        [year, month, day] => checked_date(sign * digits(year)?, digits(month)?, digits(day)?),
        [year, month] => checked_date(sign * digits(year)?, digits(month)?, 1),
        [compact] if compact.len() == 8 => {
            checked_date(sign * digits(&compact[..4])?, digits(&compact[4..6])?, digits(&compact[6..])?)
        }
        [year] if year.len() == 4 => checked_date(sign * digits(year)?, 1, 1),
        _ => None,
    }
}

fn parse_offset(text: &str) -> Option<i32> {
    if text == "Z" || text == "z" {
        return Some(0);
    }
    let sign = match text.chars().next()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    let rest = text[1..].replace(':', "");
    let (hours, minutes) = match rest.len() {
        2 => (digits(&rest)?, 0),
        4 => (digits(&rest[..2])?, digits(&rest[2..])?),
        _ => return None,
    };
    let offset = i32::try_from(sign * (hours * 3600 + minutes * 60)).ok()?;
    (minutes < 60 && offset.abs() <= MOST_OFFSET_SECONDS).then_some(offset)
}

fn split_offset(text: &str) -> (&str, Option<&str>) {
    if let Some(at) = text.find(['Z', 'z']) {
        return (&text[..at], Some(&text[at..]));
    }
    match text.rfind(['+', '-']) {
        Some(at) if at > 0 => (&text[..at], Some(&text[at..])),
        _ => (text, None),
    }
}

fn parse_local_time(text: &str) -> Option<i64> {
    let (whole, fraction) = match text.split_once(['.', ',']) {
        Some((whole, fraction)) if !fraction.is_empty() && fraction.len() <= 9 => (whole, Some(fraction)),
        Some(_) => return None,
        None => (text, None),
    };
    let nanos = match fraction {
        Some(fraction) => digits(fraction)? * 10i64.pow(9 - fraction.len() as u32),
        None => 0,
    };
    let parts: Vec<&str> = if whole.contains(':') {
        whole.split(':').collect()
    } else {
        whole.as_bytes().chunks(2).map(|pair| std::str::from_utf8(pair).unwrap_or("")).collect()
    };
    match parts.as_slice() {
        [hour] => checked_time(digits(hour)?, 0, 0, 0),
        [hour, minute] => checked_time(digits(hour)?, digits(minute)?, 0, 0),
        [hour, minute, second] => checked_time(digits(hour)?, digits(minute)?, digits(second)?, nanos),
        _ => None,
    }
}

fn parse_time(text: &str) -> Option<(i64, Option<i32>)> {
    let (time, offset) = split_offset(text);
    let offset = match offset {
        Some(offset) => Some(parse_offset(offset)?),
        None => None,
    };
    Some((parse_local_time(time)?, offset))
}

fn parse_date_time(text: &str) -> Option<(i64, i32, Option<i32>)> {
    let (date, time) = match text.split_once(['T', 't']) {
        Some((date, time)) => (date, Some(time)),
        None => (text, None),
    };
    let days = parse_date(date)?;
    let (nanos_of_day, offset) = match time {
        Some(time) => parse_time(time)?,
        None => (0, None),
    };
    let seconds = days * SECONDS_PER_DAY + nanos_of_day / NANOS_PER_SECOND;
    Some((seconds, (nanos_of_day % NANOS_PER_SECOND) as i32, offset))
}

fn parse_duration(text: &str) -> Option<Temporal> {
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text),
    };
    let rest = rest.strip_prefix(['P', 'p'])?;
    let (date_part, time_part) = match rest.split_once(['T', 't']) {
        Some((date, time)) => (date, Some(time)),
        None => (rest, None),
    };
    let (mut months, mut days, mut nanos) = (0i64, 0i64, 0i128);
    let mut number = String::new();
    for character in date_part.chars() {
        if character.is_ascii_digit() {
            number.push(character);
            continue;
        }
        let amount = digits(&std::mem::take(&mut number))?;
        match character.to_ascii_uppercase() {
            'Y' => months = months.checked_add(amount.checked_mul(12)?)?,
            'M' => months = months.checked_add(amount)?,
            'W' => days = days.checked_add(amount.checked_mul(7)?)?,
            'D' => days = days.checked_add(amount)?,
            _ => return None,
        }
    }
    if !number.is_empty() {
        return None;
    }
    if let Some(time_part) = time_part {
        for character in time_part.chars() {
            if character.is_ascii_digit() || character == '.' {
                number.push(character);
                continue;
            }
            let value: f64 = std::mem::take(&mut number).parse().ok()?;
            let unit = match character.to_ascii_uppercase() {
                'H' => 3600.0,
                'M' => 60.0,
                'S' => 1.0,
                _ => return None,
            };
            nanos += (value * unit * NANOS_PER_SECOND as f64).round() as i128;
        }
        if !number.is_empty() {
            return None;
        }
    }
    let nanos = nanos * sign as i128;
    Some(Temporal::Duration {
        months: months * sign,
        days: days * sign,
        seconds: i64::try_from(nanos.div_euclid(i128::from(NANOS_PER_SECOND))).ok()?,
        nanos: nanos.rem_euclid(i128::from(NANOS_PER_SECOND)) as i32,
    })
}

fn now() -> (i64, i32) {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (i64::try_from(since.as_secs()).unwrap_or(i64::MAX), since.subsec_nanos() as i32)
}

fn entry<'v>(entries: &'v [(String, Value)], name: &str) -> Option<&'v Value> {
    entries.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value)
}

fn integer(entries: &[(String, Value)], name: &str, default: i64) -> Option<i64> {
    match entry(entries, name) {
        None | Some(Value::Null) => Some(default),
        Some(Value::Integer(number)) => Some(*number),
        Some(Value::Float(number)) if number.fract() == 0.0 && number.is_finite() => Some(*number as i64),
        _ => None,
    }
}

fn number(entries: &[(String, Value)], name: &str) -> Option<Option<f64>> {
    match entry(entries, name) {
        None | Some(Value::Null) => Some(None),
        Some(Value::Integer(number)) => Some(Some(*number as f64)),
        Some(Value::Float(number)) => Some(Some(*number)),
        _ => None,
    }
}

fn map_offset(entries: &[(String, Value)]) -> Option<Option<i32>> {
    match entry(entries, "timezone").or_else(|| entry(entries, "offset")) {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(text)) => Some(Some(parse_offset(text)?)),
        _ => None,
    }
}

fn map_time(entries: &[(String, Value)]) -> Option<i64> {
    let nanos = integer(entries, "nanosecond", 0)?
        + integer(entries, "microsecond", 0)? * 1_000
        + integer(entries, "millisecond", 0)? * 1_000_000;
    checked_time(integer(entries, "hour", 0)?, integer(entries, "minute", 0)?, integer(entries, "second", 0)?, nanos)
}

fn map_date(entries: &[(String, Value)]) -> Option<i64> {
    checked_date(integer(entries, "year", 1970)?, integer(entries, "month", 1)?, integer(entries, "day", 1)?)
}

fn map_duration(entries: &[(String, Value)]) -> Option<Temporal> {
    let months = integer(entries, "years", 0)?.checked_mul(12)?.checked_add(integer(entries, "months", 0)?)?;
    let days = integer(entries, "weeks", 0)?.checked_mul(7)?.checked_add(integer(entries, "days", 0)?)?;
    let nanos = i128::from(integer(entries, "hours", 0)?) * 3600 * i128::from(NANOS_PER_SECOND)
        + i128::from(integer(entries, "minutes", 0)?) * 60 * i128::from(NANOS_PER_SECOND)
        + i128::from(integer(entries, "seconds", 0)?) * i128::from(NANOS_PER_SECOND)
        + i128::from(integer(entries, "milliseconds", 0)?) * 1_000_000
        + i128::from(integer(entries, "microseconds", 0)?) * 1_000
        + i128::from(integer(entries, "nanoseconds", 0)?);
    Some(Temporal::Duration {
        months,
        days,
        seconds: i64::try_from(nanos.div_euclid(i128::from(NANOS_PER_SECOND))).ok()?,
        nanos: nanos.rem_euclid(i128::from(NANOS_PER_SECOND)) as i32,
    })
}

fn map_point(entries: &[(String, Value)]) -> Option<Point> {
    let geographic = entry(entries, "longitude").is_some() || entry(entries, "latitude").is_some();
    let (x, y, z) = if geographic {
        (number(entries, "longitude")??, number(entries, "latitude")??, number(entries, "height")?)
    } else {
        (number(entries, "x")??, number(entries, "y")??, number(entries, "z")?)
    };
    let crs = match entry(entries, "crs") {
        Some(Value::String(name)) => Some(name.to_ascii_lowercase()),
        None | Some(Value::Null) => None,
        _ => return None,
    };
    let srid = match (integer(entries, "srid", 0)?, crs.as_deref(), geographic, z.is_some()) {
        (srid @ (CARTESIAN | CARTESIAN_3D | WGS_84 | WGS_84_3D), None, _, _) => srid,
        (0, Some("cartesian"), _, false) | (0, None, false, false) => CARTESIAN,
        (0, Some("cartesian-3d"), _, true) | (0, None, false, true) => CARTESIAN_3D,
        (0, Some("wgs-84"), _, false) | (0, None, true, false) => WGS_84,
        (0, Some("wgs-84-3d"), _, true) | (0, None, true, true) => WGS_84_3D,
        _ => return None,
    };
    if matches!(srid, CARTESIAN_3D | WGS_84_3D) != z.is_some() {
        return None;
    }
    Some(Point { srid, x, y, z })
}

/// The value of the temporal or spatial function `name` (lower case) for `arguments`, or `None`
/// when they don't fit: no argument for now, an ISO 8601 string or a map of components.
pub(crate) fn call(name: &str, arguments: &[Value]) -> Option<Value> {
    let (seconds_now, nanos_now) = now();
    let temporal = match (name, arguments) {
        ("point", [Value::Map(entries)]) => return Some(Value::Point(map_point(entries)?)),
        ("date", []) => Temporal::Date { days: seconds_now.div_euclid(SECONDS_PER_DAY) },
        ("date", [Value::String(text)]) => Temporal::Date { days: parse_date(text.trim())? },
        ("date", [Value::Map(entries)]) => Temporal::Date { days: map_date(entries)? },
        ("date", [Value::Temporal(other)]) => Temporal::Date { days: other.local_seconds()?.div_euclid(SECONDS_PER_DAY) },
        ("localtime", []) => Temporal::LocalTime {
            nanos: seconds_now.rem_euclid(SECONDS_PER_DAY) * NANOS_PER_SECOND + i64::from(nanos_now),
        },
        ("localtime", [Value::String(text)]) => Temporal::LocalTime { nanos: parse_local_time(text.trim())? },
        ("localtime", [Value::Map(entries)]) => Temporal::LocalTime { nanos: map_time(entries)? },
        ("time", []) => Temporal::Time {
            nanos: seconds_now.rem_euclid(SECONDS_PER_DAY) * NANOS_PER_SECOND + i64::from(nanos_now),
            offset: 0,
        },
        ("time", [Value::String(text)]) => {
            let (nanos, offset) = parse_time(text.trim())?;
            Temporal::Time { nanos, offset: offset.unwrap_or(0) }
        }
        ("time", [Value::Map(entries)]) => Temporal::Time { nanos: map_time(entries)?, offset: map_offset(entries)?.unwrap_or(0) },
        ("localdatetime", []) => Temporal::LocalDateTime { seconds: seconds_now, nanos: nanos_now },
        ("localdatetime", [Value::String(text)]) => {
            let (seconds, nanos, _) = parse_date_time(text.trim())?;
            Temporal::LocalDateTime { seconds, nanos }
        }
        ("localdatetime", [Value::Map(entries)]) => {
            let nanos_of_day = map_time(entries)?;
            Temporal::LocalDateTime {
                seconds: map_date(entries)? * SECONDS_PER_DAY + nanos_of_day / NANOS_PER_SECOND,
                nanos: (nanos_of_day % NANOS_PER_SECOND) as i32,
            }
        }
        ("datetime", []) => Temporal::DateTime { seconds: seconds_now, nanos: nanos_now, offset: 0 },
        ("datetime", [Value::String(text)]) => {
            let (local, nanos, offset) = parse_date_time(text.trim())?;
            let offset = offset.unwrap_or(0);
            Temporal::DateTime { seconds: local - i64::from(offset), nanos, offset }
        }
        ("datetime", [Value::Map(entries)]) => {
            if let Some(epoch) = entry(entries, "epochseconds").or_else(|| entry(entries, "epochmillis")) {
                let Value::Integer(epoch) = epoch else {
                    return None;
                };
                let (seconds, nanos) = if entry(entries, "epochseconds").is_some() {
                    (*epoch, integer(entries, "nanosecond", 0)?)
                } else {
                    (epoch.div_euclid(1000), epoch.rem_euclid(1000) * 1_000_000)
                };
                Temporal::DateTime { seconds, nanos: i32::try_from(nanos).ok()?, offset: map_offset(entries)?.unwrap_or(0) }
            } else {
                let nanos_of_day = map_time(entries)?;
                let offset = map_offset(entries)?.unwrap_or(0);
                Temporal::DateTime {
                    seconds: map_date(entries)? * SECONDS_PER_DAY + nanos_of_day / NANOS_PER_SECOND - i64::from(offset),
                    nanos: (nanos_of_day % NANOS_PER_SECOND) as i32,
                    offset,
                }
            }
        }
        ("duration", [Value::String(text)]) => parse_duration(text.trim())?,
        ("duration", [Value::Map(entries)]) => map_duration(entries)?,
        _ => return None,
    };
    Some(Value::Temporal(temporal))
}

fn fraction(nanos: i64) -> String {
    if nanos == 0 {
        return String::new();
    }
    let text = format!(".{nanos:09}");
    text.trim_end_matches('0').to_owned()
}

fn clock(nanos_of_day: i64) -> String {
    let seconds = nanos_of_day / NANOS_PER_SECOND;
    format!(
        "{:02}:{:02}:{:02}{}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60,
        fraction(nanos_of_day % NANOS_PER_SECOND)
    )
}

fn calendar(days: i64) -> String {
    let (year, month, day) = civil_from_days(days);
    if (0..=9999).contains(&year) {
        format!("{year:04}-{month:02}-{day:02}")
    } else {
        format!("{year:+}-{month:02}-{day:02}")
    }
}

fn offset_text(offset: i32) -> String {
    if offset == 0 {
        return "Z".to_owned();
    }
    let sign = if offset < 0 { '-' } else { '+' };
    let magnitude = offset.unsigned_abs();
    let seconds = magnitude % 60;
    let base = format!("{sign}{:02}:{:02}", magnitude / 3600, magnitude / 60 % 60);
    if seconds == 0 { base } else { format!("{base}:{seconds:02}") }
}

impl Temporal {
    /// The value's local date and time in seconds since the epoch, for the types that have a date.
    pub(crate) fn local_seconds(&self) -> Option<i64> {
        match *self {
            Self::Date { days } => Some(days * SECONDS_PER_DAY),
            Self::LocalDateTime { seconds, .. } => Some(seconds),
            Self::DateTime { seconds, offset, .. } => Some(seconds + i64::from(offset)),
            _ => None,
        }
    }

    /// The type's name, as Cypher's type errors spell it.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Date { .. } => "DATE",
            Self::LocalTime { .. } => "LOCAL TIME",
            Self::Time { .. } => "ZONED TIME",
            Self::LocalDateTime { .. } => "LOCAL DATETIME",
            Self::DateTime { .. } => "ZONED DATETIME",
            Self::Duration { .. } => "DURATION",
        }
    }

    pub(crate) fn rank(&self) -> u8 {
        match self {
            Self::DateTime { .. } => 0,
            Self::LocalDateTime { .. } => 1,
            Self::Date { .. } => 2,
            Self::Time { .. } => 3,
            Self::LocalTime { .. } => 4,
            Self::Duration { .. } => 5,
        }
    }

    fn key(&self) -> (i128, i128) {
        match *self {
            Self::Date { days } => (i128::from(days), 0),
            Self::LocalTime { nanos } => (i128::from(nanos), 0),
            Self::Time { nanos, offset } => (i128::from(nanos) - i128::from(offset) * i128::from(NANOS_PER_SECOND), 0),
            Self::LocalDateTime { seconds, nanos } | Self::DateTime { seconds, nanos, .. } => {
                (i128::from(seconds), i128::from(nanos))
            }
            Self::Duration { months, days, seconds, nanos } => (
                i128::from(months) * 30 * i128::from(NANOS_PER_DAY) + i128::from(days) * i128::from(NANOS_PER_DAY),
                i128::from(seconds) * i128::from(NANOS_PER_SECOND) + i128::from(nanos),
            ),
        }
    }

    /// The order of two values of the same type: by instant, by time of day, or by length.
    pub(crate) fn order(&self, other: &Self) -> Ordering {
        self.rank().cmp(&other.rank()).then_with(|| self.key().cmp(&other.key()))
    }

    /// Whether `<` and `>` compare the value: every temporal type but durations.
    pub(crate) fn is_ordered(&self) -> bool {
        !matches!(self, Self::Duration { .. })
    }

    /// The value as Cypher's `toString` prints it, in ISO 8601.
    pub fn text(&self) -> String {
        match *self {
            Self::Date { days } => calendar(days),
            Self::LocalTime { nanos } => clock(nanos),
            Self::Time { nanos, offset } => clock(nanos) + &offset_text(offset),
            Self::LocalDateTime { seconds, nanos } => {
                format!("{}T{}", calendar(seconds.div_euclid(SECONDS_PER_DAY)), clock(seconds.rem_euclid(SECONDS_PER_DAY) * NANOS_PER_SECOND + i64::from(nanos)))
            }
            Self::DateTime { seconds, nanos, offset } => {
                let local = seconds + i64::from(offset);
                format!(
                    "{}T{}{}",
                    calendar(local.div_euclid(SECONDS_PER_DAY)),
                    clock(local.rem_euclid(SECONDS_PER_DAY) * NANOS_PER_SECOND + i64::from(nanos)),
                    offset_text(offset)
                )
            }
            Self::Duration { months, days, seconds, nanos } => {
                let mut text = "P".to_owned();
                if months / 12 != 0 {
                    text += &format!("{}Y", months / 12);
                }
                if months % 12 != 0 {
                    text += &format!("{}M", months % 12);
                }
                if days != 0 {
                    text += &format!("{days}D");
                }
                let total = i128::from(seconds) * i128::from(NANOS_PER_SECOND) + i128::from(nanos);
                if total != 0 {
                    text.push('T');
                    let sign = if total < 0 { "-" } else { "" };
                    let magnitude = total.unsigned_abs();
                    let whole = magnitude / NANOS_PER_SECOND as u128;
                    if whole / 3600 != 0 {
                        text += &format!("{sign}{}H", whole / 3600);
                    }
                    if whole / 60 % 60 != 0 {
                        text += &format!("{sign}{}M", whole / 60 % 60);
                    }
                    let rest = magnitude % (60 * NANOS_PER_SECOND as u128);
                    if rest != 0 {
                        text += &format!("{sign}{}{}S", rest / NANOS_PER_SECOND as u128, fraction((rest % NANOS_PER_SECOND as u128) as i64));
                    }
                }
                if text == "P" { "PT0S".to_owned() } else { text }
            }
        }
    }

    /// The component `name` (`year`, `hour`, `offsetSeconds`, `epochMillis`, `days`, ...), or
    /// `None` when the type has no such component.
    pub fn field(&self, name: &str) -> Option<Value> {
        let name = name.to_ascii_lowercase();
        if let Self::Duration { months, days, seconds, nanos } = *self {
            return Some(Value::Integer(match name.as_str() {
                "years" => months / 12,
                "quarters" => months / 3,
                "months" => months,
                "monthsofyear" => months % 12,
                "weeks" => days / 7,
                "days" => days,
                "daysofweek" => days % 7,
                "hours" => seconds / 3600,
                "minutes" => seconds / 60,
                "minutesofhour" => seconds / 60 % 60,
                "seconds" => seconds,
                "secondsofminute" => seconds % 60,
                "milliseconds" => seconds * 1000 + i64::from(nanos) / 1_000_000,
                "nanoseconds" => seconds * NANOS_PER_SECOND + i64::from(nanos),
                "nanosecondsofsecond" => i64::from(nanos),
                _ => return None,
            }));
        }
        let local = self.local_seconds();
        let nanos_of_day = match *self {
            Self::LocalTime { nanos } | Self::Time { nanos, .. } => Some(nanos),
            Self::LocalDateTime { nanos, .. } | Self::DateTime { nanos, .. } => {
                local.map(|local| local.rem_euclid(SECONDS_PER_DAY) * NANOS_PER_SECOND + i64::from(nanos))
            }
            _ => None,
        };
        let offset = match *self {
            Self::Time { offset, .. } | Self::DateTime { offset, .. } => Some(offset),
            _ => None,
        };
        let days = local.map(|local| local.div_euclid(SECONDS_PER_DAY));
        let date = days.map(civil_from_days);
        Some(match name.as_str() {
            "year" => Value::Integer(date?.0),
            "quarter" => Value::Integer((date?.1 - 1) / 3 + 1),
            "month" => Value::Integer(date?.1),
            "day" => Value::Integer(date?.2),
            "dayofweek" | "weekday" => Value::Integer((days? + 3).rem_euclid(7) + 1),
            "ordinalday" => Value::Integer(days? - days_from_civil(date?.0, 1, 1) + 1),
            "hour" => Value::Integer(nanos_of_day? / NANOS_PER_SECOND / 3600),
            "minute" => Value::Integer(nanos_of_day? / NANOS_PER_SECOND / 60 % 60),
            "second" => Value::Integer(nanos_of_day? / NANOS_PER_SECOND % 60),
            "millisecond" => Value::Integer(nanos_of_day? % NANOS_PER_SECOND / 1_000_000),
            "microsecond" => Value::Integer(nanos_of_day? % NANOS_PER_SECOND / 1_000),
            "nanosecond" => Value::Integer(nanos_of_day? % NANOS_PER_SECOND),
            "timezone" | "offset" => Value::String(offset_text(offset?)),
            "offsetminutes" => Value::Integer(i64::from(offset?) / 60),
            "offsetseconds" => Value::Integer(i64::from(offset?)),
            "epochseconds" => match *self {
                Self::DateTime { seconds, .. } => Value::Integer(seconds),
                _ => return None,
            },
            "epochmillis" => match *self {
                Self::DateTime { seconds, nanos, .. } => Value::Integer(seconds * 1000 + i64::from(nanos) / 1_000_000),
                _ => return None,
            },
            _ => return None,
        })
    }
}

impl Point {
    /// The component `name` (`x`, `y`, `z`, `longitude`, `latitude`, `height`, `srid`, `crs`).
    pub fn field(&self, name: &str) -> Option<Value> {
        let geographic = matches!(self.srid, WGS_84 | WGS_84_3D);
        Some(match name.to_ascii_lowercase().as_str() {
            "x" => Value::Float(self.x),
            "y" => Value::Float(self.y),
            "z" => Value::Float(self.z?),
            "longitude" if geographic => Value::Float(self.x),
            "latitude" if geographic => Value::Float(self.y),
            "height" if geographic => Value::Float(self.z?),
            "srid" => Value::Integer(i64::from(self.srid)),
            "crs" => Value::String(self.crs().to_owned()),
            _ => return None,
        })
    }

    /// The name of the point's coordinate reference system.
    pub fn crs(&self) -> &'static str {
        match self.srid {
            WGS_84 => "wgs-84",
            WGS_84_3D => "wgs-84-3d",
            CARTESIAN_3D => "cartesian-3d",
            _ => "cartesian",
        }
    }

    /// The point as Cypher's `toString` prints it.
    pub fn text(&self) -> String {
        let number = |value: f64| if value.fract() == 0.0 && value.is_finite() { format!("{value:.1}") } else { value.to_string() };
        match self.z {
            Some(z) => format!("point({{x: {}, y: {}, z: {}, crs: '{}'}})", number(self.x), number(self.y), number(z), self.crs()),
            None => format!("point({{x: {}, y: {}, crs: '{}'}})", number(self.x), number(self.y), self.crs()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_days_round_trip_and_strings_print_back() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        let datetime = call("datetime", &[Value::String("2024-02-29T13:45:30.5+02:00".to_owned())]);
        let Some(Value::Temporal(datetime)) = datetime else {
            panic!("datetime");
        };
        assert_eq!(datetime.text(), "2024-02-29T13:45:30.5+02:00");
        assert_eq!(datetime.field("hour"), Some(Value::Integer(13)));
        assert_eq!(datetime.field("dayOfWeek"), Some(Value::Integer(4)));
        let Some(Value::Temporal(duration)) = call("duration", &[Value::String("P1Y2M3DT4H5M6S".to_owned())]) else {
            panic!("duration");
        };
        assert_eq!(duration.text(), "P1Y2M3DT4H5M6S");
        assert!(call("date", &[Value::String("2023-02-29".to_owned())]).is_none());
    }
}
