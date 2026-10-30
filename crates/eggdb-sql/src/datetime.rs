use crate::error::{SqlError, SqlResult, SqlState};
use crate::numeric::Numeric;
use crate::time_zones::TimeZone;
use std::cell::Cell;

pub(crate) const MICROS_PER_SECOND: i64 = 1_000_000;
pub(crate) const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SECOND;
const UNIX_EPOCH_DAYS: i64 = 10_957;
const UNIX_DAYS_SHIFT: i64 = 719_468;
pub(crate) const MIN_DATE: i64 = -2_451_545;
pub(crate) const MAX_DATE: i64 = 2_145_031_948;
pub(crate) const MIN_TIMESTAMP: i64 = MIN_DATE * MICROS_PER_DAY;
pub(crate) const END_TIMESTAMP: i64 = 106_751_983 * MICROS_PER_DAY;
const MAX_OFFSET_SECONDS: i64 = 15 * 3600 + 59 * 60 + 59;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatementClock {
    pub(crate) now: Option<i64>,
    pub(crate) zone: TimeZone,
}

thread_local! {
    static CURRENT: Cell<Option<StatementClock>> = const { Cell::new(None) };
}

pub(crate) struct StatementClockGuard(Option<StatementClock>);

impl Drop for StatementClockGuard {
    fn drop(&mut self) {
        CURRENT.with(|current| current.set(self.0));
    }
}

pub(crate) fn enter_statement(zone: TimeZone) -> StatementClockGuard {
    let clock = StatementClock { now: None, zone };
    StatementClockGuard(CURRENT.with(|current| current.replace(Some(clock))))
}

fn system_now() -> i64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_micros()).unwrap_or(i64::MAX)
        });
    since.saturating_sub(UNIX_EPOCH_DAYS * MICROS_PER_DAY)
}

pub(crate) fn statement_now() -> i64 {
    CURRENT.with(|current| match current.get() {
        Some(StatementClock { now: Some(now), .. }) => now,
        Some(clock) => {
            let now = system_now();
            current.set(Some(StatementClock {
                now: Some(now),
                ..clock
            }));
            now
        }
        None => system_now(),
    })
}

pub(crate) fn session_zone() -> TimeZone {
    CURRENT.with(|current| current.get().map_or(TimeZone::UTC, |clock| clock.zone))
}

pub(crate) fn session_offset_at(utc: i64) -> i32 {
    session_zone().offset_at(utc)
}

pub(crate) fn session_offset_now() -> i32 {
    session_offset_at(statement_now())
}

pub(crate) fn time_zone_offset(name: &str) -> Option<i32> {
    let trimmed = name.trim();
    let lower = trimmed.to_ascii_lowercase();
    if [
        "utc",
        "gmt",
        "z",
        "zulu",
        "uct",
        "universal",
        "etc/utc",
        "etc/gmt",
        "etc/uct",
        "etc/universal",
        "etc/zulu",
        "gmt0",
        "greenwich",
    ]
    .contains(&lower.as_str())
    {
        return Some(0);
    }
    let letters = trimmed
        .bytes()
        .take_while(|byte| byte.is_ascii_alphabetic())
        .count();
    if letters != 0 && letters < 3 {
        return None;
    }
    let rest = &trimmed[letters..];
    let (sign, digits) = match rest.as_bytes().first() {
        Some(b'+') => (1, &rest[1..]),
        Some(b'-') => (-1, &rest[1..]),
        Some(byte) if byte.is_ascii_digit() => (1, rest),
        _ => return None,
    };
    let seconds = clock_offset(digits)?;
    i32::try_from(-sign * seconds).ok()
}

fn clock_offset(text: &str) -> Option<i64> {
    let parts: Vec<&str> = text.split(':').collect();
    let number = |part: &str| -> Option<i64> {
        if part.is_empty() || part.len() > 2 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let (hours, minutes, seconds) = match parts.as_slice() {
        [hours] if hours.len() == 4 => (number(hours.get(..2)?)?, number(hours.get(2..)?)?, 0),
        [hours] => (number(hours)?, 0, 0),
        [hours, minutes] => (number(hours)?, number(minutes)?, 0),
        [hours, minutes, seconds] => (number(hours)?, number(minutes)?, number(seconds)?),
        _ => return None,
    };
    if minutes > 59 || seconds > 59 {
        return None;
    }
    let total = hours * 3600 + minutes * 60 + seconds;
    (total <= MAX_OFFSET_SECONDS).then_some(total)
}

pub(crate) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - UNIX_DAYS_SHIFT - UNIX_EPOCH_DAYS
}

pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + UNIX_EPOCH_DAYS + UNIX_DAYS_SHIFT;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

pub(crate) fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

pub(crate) fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DateTimeKind {
    Date,
    Timestamp,
    TimestampTz,
}

impl DateTimeKind {
    fn name(self) -> &'static str {
        match self {
            Self::Date => "date",
            Self::Timestamp => "timestamp without time zone",
            Self::TimestampTz => "timestamp with time zone",
        }
    }

    fn range_name(self) -> &'static str {
        match self {
            Self::Date => "date",
            Self::Timestamp | Self::TimestampTz => "timestamp",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Parsed {
    pub(crate) days: i64,
    pub(crate) micros: i64,
    pub(crate) offset_seconds: Option<i64>,
}

enum Failure {
    Syntax,
    Field,
}

struct Scanner<'t> {
    bytes: &'t [u8],
    at: usize,
}

impl Scanner<'_> {
    fn digits(&mut self, min: usize, max: usize) -> Result<(i64, usize), Failure> {
        let start = self.at;
        while self.at < self.bytes.len() && self.bytes[self.at].is_ascii_digit() {
            self.at += 1;
        }
        let count = self.at - start;
        if count < min || count > max {
            return Err(Failure::Syntax);
        }
        let value = self.bytes[start..self.at]
            .iter()
            .fold(0i64, |total, digit| total * 10 + i64::from(digit - b'0'));
        Ok((value, count))
    }

    fn take(&mut self, byte: u8) -> bool {
        if self.bytes.get(self.at) == Some(&byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn spaces(&mut self) -> bool {
        let start = self.at;
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
        self.at > start
    }

    fn word(&mut self, word: &str) -> bool {
        let end = self.at + word.len();
        let matches = self
            .bytes
            .get(self.at..end)
            .is_some_and(|found| found.eq_ignore_ascii_case(word.as_bytes()));
        if matches {
            self.at = end;
        }
        matches
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn done(&self) -> bool {
        self.at == self.bytes.len()
    }
}

fn parse_parts(text: &str) -> Result<Parsed, Failure> {
    let mut scan = Scanner {
        bytes: text.trim().as_bytes(),
        at: 0,
    };
    let (year, _) = scan.digits(4, 7)?;
    if !scan.take(b'-') {
        return Err(Failure::Syntax);
    }
    let (month, _) = scan.digits(1, 2)?;
    if !scan.take(b'-') {
        return Err(Failure::Syntax);
    }
    let (day, _) = scan.digits(1, 2)?;
    let mut micros = 0i64;
    let mut bc = false;
    let separated = scan.take(b'T') || scan.take(b't') || scan.spaces();
    if separated && scan.peek().is_some_and(|byte| byte.is_ascii_digit()) {
        micros = parse_time(&mut scan)?;
        scan.spaces();
    }
    let zone = parse_zone(&mut scan)?;
    scan.spaces();
    if scan.word("bc") {
        bc = true;
    } else {
        scan.word("ad");
    }
    scan.spaces();
    if !scan.done() {
        return Err(Failure::Syntax);
    }
    if year == 0 || !(1..=12).contains(&month) {
        return Err(Failure::Field);
    }
    let year = if bc { 1 - year } else { year };
    if day < 1 || day > days_in_month(year, month) {
        return Err(Failure::Field);
    }
    let days = days_from_civil(year, month, day);
    let offset_seconds = match zone {
        WrittenZone::Absent => None,
        WrittenZone::Offset(seconds) => Some(seconds),
        WrittenZone::Region(zone) => {
            let local = days.saturating_mul(MICROS_PER_DAY).saturating_add(micros);
            Some(i64::from(zone.offset_of_local(local)))
        }
    };
    Ok(Parsed {
        days,
        micros,
        offset_seconds,
    })
}

enum WrittenZone {
    Absent,
    Offset(i64),
    Region(TimeZone),
}

fn region_at(scan: &mut Scanner<'_>) -> Option<TimeZone> {
    if !scan.peek().is_some_and(|byte| byte.is_ascii_alphabetic()) {
        return None;
    }
    let start = scan.at;
    let mut end = start;
    while scan.bytes.get(end).is_some_and(|&byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'/' | b'-' | b'+')
    }) {
        end += 1;
    }
    let word = std::str::from_utf8(scan.bytes.get(start..end)?).ok()?;
    if word.eq_ignore_ascii_case("bc") || word.eq_ignore_ascii_case("ad") {
        return None;
    }
    let zone = TimeZone::region(word)?;
    scan.at = end;
    Some(zone)
}

fn parse_time(scan: &mut Scanner<'_>) -> Result<i64, Failure> {
    let (hour, _) = scan.digits(1, 2)?;
    if !scan.take(b':') {
        return Err(Failure::Syntax);
    }
    let (minute, _) = scan.digits(2, 2)?;
    let mut second = 0;
    let mut fraction = 0i64;
    if scan.take(b':') {
        second = scan.digits(2, 2)?.0;
        if scan.take(b'.') {
            let start = scan.at;
            while scan.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                scan.at += 1;
            }
            let digits = &scan.bytes[start..scan.at];
            if digits.is_empty() {
                return Err(Failure::Syntax);
            }
            let mut value = 0i64;
            for position in 0..7 {
                let digit = digits
                    .get(position)
                    .map_or(0, |digit| i64::from(digit - b'0'));
                value = value * 10 + digit;
            }
            fraction = (value + 5) / 10;
        }
    }
    if hour > 24
        || minute > 59
        || second > 60
        || (hour == 24 && (minute, second, fraction) != (0, 0, 0))
    {
        return Err(Failure::Field);
    }
    Ok(((hour * 60 + minute) * 60 + second) * MICROS_PER_SECOND + fraction)
}

fn parse_zone(scan: &mut Scanner<'_>) -> Result<WrittenZone, Failure> {
    if let Some(zone) = region_at(scan) {
        return Ok(WrittenZone::Region(zone));
    }
    if scan.word("utc") || scan.word("gmt") || scan.take(b'Z') || scan.take(b'z') {
        return Ok(WrittenZone::Offset(0));
    }
    let sign = match scan.peek() {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return Ok(WrittenZone::Absent),
    };
    scan.at += 1;
    let start = scan.at;
    while scan
        .peek()
        .is_some_and(|byte| byte.is_ascii_digit() || byte == b':')
    {
        scan.at += 1;
    }
    let written = std::str::from_utf8(&scan.bytes[start..scan.at]).map_err(|_| Failure::Syntax)?;
    let seconds = clock_offset(written).ok_or(Failure::Field)?;
    Ok(WrittenZone::Offset(sign * seconds))
}

fn input_error(kind: DateTimeKind, text: &str, failure: Failure) -> SqlError {
    match failure {
        Failure::Syntax => SqlError::new(
            SqlState::INVALID_DATETIME_FORMAT,
            format!("invalid input syntax for type {}: \"{text}\"", kind.name()),
        ),
        Failure::Field => SqlError::new(
            SqlState::DATETIME_FIELD_OVERFLOW,
            format!("date/time field value out of range: \"{text}\""),
        ),
    }
}

fn out_of_range_input(kind: DateTimeKind, text: &str) -> SqlError {
    SqlError::new(
        SqlState::DATETIME_FIELD_OVERFLOW,
        format!("{} out of range: \"{text}\"", kind.range_name()),
    )
}

pub(crate) fn date_out_of_range() -> SqlError {
    SqlError::new(SqlState::DATETIME_FIELD_OVERFLOW, "date out of range")
}

pub(crate) fn timestamp_out_of_range() -> SqlError {
    SqlError::new(SqlState::DATETIME_FIELD_OVERFLOW, "timestamp out of range")
}

pub(crate) fn checked_date(days: i64) -> SqlResult<i32> {
    if (MIN_DATE..=MAX_DATE).contains(&days) {
        i32::try_from(days).map_err(|_| date_out_of_range())
    } else {
        Err(date_out_of_range())
    }
}

pub(crate) fn checked_timestamp(micros: i128) -> SqlResult<i64> {
    if (i128::from(MIN_TIMESTAMP)..i128::from(END_TIMESTAMP)).contains(&micros) {
        Ok(micros as i64)
    } else {
        Err(timestamp_out_of_range())
    }
}

pub(crate) fn valid_timestamp(micros: i64) -> bool {
    (MIN_TIMESTAMP..END_TIMESTAMP).contains(&micros)
}

pub(crate) fn valid_date(days: i32) -> bool {
    (MIN_DATE..=MAX_DATE).contains(&i64::from(days))
}

pub(crate) fn parse_date(text: &str) -> SqlResult<i32> {
    let kind = DateTimeKind::Date;
    let parsed = parse_parts(text).map_err(|failure| input_error(kind, text, failure))?;
    checked_date(parsed.days).map_err(|_| out_of_range_input(kind, text))
}

pub(crate) fn parse_timestamp(text: &str, with_zone: bool) -> SqlResult<i64> {
    let kind = if with_zone {
        DateTimeKind::TimestampTz
    } else {
        DateTimeKind::Timestamp
    };
    let parsed = parse_parts(text).map_err(|failure| input_error(kind, text, failure))?;
    let local = i128::from(parsed.days) * i128::from(MICROS_PER_DAY) + i128::from(parsed.micros);
    let micros = if with_zone {
        match parsed.offset_seconds {
            Some(offset) => local - i128::from(offset) * i128::from(MICROS_PER_SECOND),
            None => {
                let clamped = i64::try_from(local).unwrap_or(if local < 0 { i64::MIN } else { i64::MAX });
                local - i128::from(session_zone().offset_of_local(clamped))
                    * i128::from(MICROS_PER_SECOND)
            }
        }
    } else {
        local
    };
    checked_timestamp(micros).map_err(|_| out_of_range_input(kind, text))
}

pub(crate) const MAX_OFFSET: i32 = MAX_OFFSET_SECONDS as i32;

fn time_input_error(with_zone: bool, text: &str, failure: Failure) -> SqlError {
    let name = if with_zone {
        "time with time zone"
    } else {
        "time without time zone"
    };
    match failure {
        Failure::Syntax => SqlError::new(
            SqlState::INVALID_DATETIME_FORMAT,
            format!("invalid input syntax for type {name}: \"{text}\""),
        ),
        Failure::Field => SqlError::new(
            SqlState::DATETIME_FIELD_OVERFLOW,
            format!("date/time field value out of range: \"{text}\""),
        ),
    }
}

fn clock_parts(text: &str) -> Result<(i64, Option<i64>), Failure> {
    let trimmed = text.trim();
    let year_digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if year_digits >= 4 && trimmed.as_bytes().get(year_digits) == Some(&b'-') {
        let parsed = parse_parts(trimmed)?;
        return Ok((parsed.micros, parsed.offset_seconds));
    }
    let mut scan = Scanner {
        bytes: trimmed.as_bytes(),
        at: 0,
    };
    let mut micros = parse_time(&mut scan)?;
    scan.spaces();
    let hour = micros / (3600 * MICROS_PER_SECOND);
    let half_day = 12 * 3600 * MICROS_PER_SECOND;
    if scan.word("am") || scan.word("a.m.") {
        if hour > 12 {
            return Err(Failure::Field);
        }
        if hour == 12 {
            micros -= half_day;
        }
    } else if scan.word("pm") || scan.word("p.m.") {
        if hour > 12 {
            return Err(Failure::Field);
        }
        if hour != 12 {
            micros += half_day;
        }
    }
    scan.spaces();
    let offset = parse_zone(&mut scan)?;
    scan.spaces();
    if !scan.done() {
        return Err(Failure::Syntax);
    }
    Ok((micros, offset))
}

pub(crate) fn valid_time(micros: i64) -> bool {
    (0..=MICROS_PER_DAY).contains(&micros)
}

pub(crate) fn valid_offset(offset: i32) -> bool {
    (-MAX_OFFSET..=MAX_OFFSET).contains(&offset)
}

pub(crate) fn parse_time_of_day(text: &str, with_zone: bool) -> SqlResult<(i64, i32)> {
    let (micros, offset) =
        clock_parts(text).map_err(|failure| time_input_error(with_zone, text, failure))?;
    if !valid_time(micros) {
        return Err(time_input_error(with_zone, text, Failure::Field));
    }
    let offset = offset.map_or_else(session_offset_now, |offset| offset as i32);
    Ok((micros, offset))
}

fn mysql_time_error(text: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_DATETIME_FORMAT,
        format!("Incorrect TIME value: '{text}'"),
    )
}

fn mysql_fraction(digits: &str) -> Option<i64> {
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mut value = 0i64;
    for position in 0..7 {
        let digit = digits
            .as_bytes()
            .get(position)
            .map_or(0, |digit| i64::from(digit - b'0'));
        value = value * 10 + digit;
    }
    Some((value + 5) / 10)
}

fn small_number(text: &str, longest: usize) -> Option<i64> {
    if text.is_empty() || text.len() > longest || !text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    text.parse().ok()
}

fn mysql_clock(days: i64, clock: &str) -> Option<i64> {
    let (whole, fraction) = match clock.split_once('.') {
        Some((whole, fraction)) => (whole, mysql_fraction(fraction)?),
        None => (clock, 0),
    };
    let (hours, minutes, seconds) = if whole.contains(':') {
        let parts: Vec<&str> = whole.split(':').collect();
        match parts.as_slice() {
            [hours, minutes] => (small_number(hours, 3)?, small_number(minutes, 2)?, 0),
            [hours, minutes, seconds] => (
                small_number(hours, 3)?,
                small_number(minutes, 2)?,
                small_number(seconds, 2)?,
            ),
            _ => return None,
        }
    } else if days != 0 {
        (small_number(whole, 3)?, 0, 0)
    } else {
        let digits = small_number(whole, 7)?;
        (digits / 10_000, digits / 100 % 100, digits % 100)
    };
    if minutes > 59 || seconds > 59 {
        return None;
    }
    let total_seconds = (days * 24 + hours) * 3600 + minutes * 60 + seconds;
    Some(total_seconds * MICROS_PER_SECOND + fraction)
}

pub(crate) fn parse_mysql_time(text: &str) -> SqlResult<i64> {
    let trimmed = text.trim();
    let year_digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if year_digits >= 4 && trimmed.as_bytes().get(year_digits) == Some(&b'-') {
        return parse_time_of_day(text, false).map(|(micros, _)| micros);
    }
    let (negative, body) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, trimmed),
    };
    let (days, clock) = match body.split_once(' ') {
        Some((days, clock)) => (
            small_number(days, 2).ok_or_else(|| mysql_time_error(text))?,
            clock.trim(),
        ),
        None => (0, body),
    };
    let micros = mysql_clock(days, clock).ok_or_else(|| mysql_time_error(text))?;
    if micros > MYSQL_TIME_LIMIT {
        return Err(SqlError::new(
            SqlState::DATETIME_FIELD_OVERFLOW,
            format!("Incorrect TIME value: '{text}'"),
        ));
    }
    Ok(if negative { -micros } else { micros })
}

pub(crate) fn round_time(micros: i64, precision: u8) -> i64 {
    if precision >= 6 {
        return micros;
    }
    if micros < 0 {
        return -round_time(micros.saturating_neg(), precision);
    }
    let unit = 10i64.pow(u32::from(6 - precision));
    (micros + unit / 2) / unit * unit
}

pub(crate) const MYSQL_TIME_LIMIT: i64 = (838 * 3600 + 59 * 60 + 59) * MICROS_PER_SECOND;

pub(crate) fn valid_stored_time(micros: i64) -> bool {
    (-MYSQL_TIME_LIMIT..=MYSQL_TIME_LIMIT).contains(&micros)
}

pub(crate) fn format_time(micros: i64) -> String {
    let sign = if micros < 0 { "-" } else { "" };
    let magnitude = micros.unsigned_abs();
    let micros_per_second = MICROS_PER_SECOND.unsigned_abs();
    let seconds = magnitude / micros_per_second;
    let fraction = magnitude % micros_per_second;
    let mut out = format!(
        "{sign}{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    if fraction != 0 {
        let digits = format!("{fraction:06}");
        out.push('.');
        out.push_str(digits.trim_end_matches('0'));
    }
    out
}

pub(crate) fn format_time_tz(micros: i64, offset: i32) -> String {
    format!("{}{}", format_time(micros), format_offset(offset))
}

pub(crate) fn time_tz_utc(micros: i64, offset: i32) -> i64 {
    micros - i64::from(offset) * MICROS_PER_SECOND
}

pub(crate) fn time_of_timestamp(micros: i64) -> i64 {
    micros.rem_euclid(MICROS_PER_DAY)
}

pub(crate) fn wrap_time(micros: i128) -> i64 {
    micros.rem_euclid(i128::from(MICROS_PER_DAY)) as i64
}

pub(crate) fn extract_time(name: &str, micros: i64, offset: Option<i32>) -> SqlResult<Numeric> {
    let kind_name = if offset.is_some() {
        "time with time zone"
    } else {
        "time without time zone"
    };
    let field = Field::named(name).ok_or_else(|| {
        SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("unit \"{name}\" not recognized for type {kind_name}"),
        )
    })?;
    let units = i128::from(micros);
    let within_minute = units % i128::from(60 * MICROS_PER_SECOND);
    Ok(match (field, offset) {
        (Field::Microseconds, _) => Numeric::from_scaled(within_minute, 0),
        (Field::Milliseconds, _) => Numeric::from_scaled(within_minute, 3),
        (Field::Second, _) => Numeric::from_scaled(within_minute, 6),
        (Field::Minute, _) => {
            Numeric::from_scaled(units / i128::from(60 * MICROS_PER_SECOND) % 60, 0)
        }
        (Field::Hour, _) => Numeric::from_scaled(units / i128::from(3600 * MICROS_PER_SECOND), 0),
        (Field::Epoch, None) => Numeric::from_scaled(units, 6),
        (Field::Epoch, Some(offset)) => Numeric::from_scaled(
            units - i128::from(offset) * i128::from(MICROS_PER_SECOND),
            6,
        ),
        (Field::Timezone, Some(offset)) => Numeric::from_scaled(i128::from(offset), 0),
        (Field::TimezoneHour, Some(offset)) => Numeric::from_scaled(i128::from(offset / 3600), 0),
        (Field::TimezoneMinute, Some(offset)) => {
            Numeric::from_scaled(i128::from(offset / 60 % 60), 0)
        }
        _ => {
            return Err(SqlError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("unit \"{name}\" not supported for type {kind_name}"),
            ));
        }
    })
}

fn year_text(year: i64) -> (String, bool) {
    if year <= 0 {
        (format!("{:04}", 1 - year), true)
    } else {
        (format!("{year:04}"), false)
    }
}

pub(crate) fn format_date(days: i32) -> String {
    let (year, month, day) = civil_from_days(i64::from(days));
    let (year, bc) = year_text(year);
    let era = if bc { " BC" } else { "" };
    format!("{year}-{month:02}-{day:02}{era}")
}

fn format_local(micros: i64, zone: Option<i32>) -> String {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let of_day = micros.rem_euclid(MICROS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let (year, bc) = year_text(year);
    let seconds = of_day / MICROS_PER_SECOND;
    let fraction = of_day % MICROS_PER_SECOND;
    let mut out = format!(
        "{year}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    if fraction != 0 {
        let digits = format!("{fraction:06}");
        out.push('.');
        out.push_str(digits.trim_end_matches('0'));
    }
    if let Some(offset) = zone {
        out.push_str(&format_offset(offset));
    }
    if bc {
        out.push_str(" BC");
    }
    out
}

pub(crate) fn format_offset(offset: i32) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let magnitude = offset.unsigned_abs();
    let (hours, minutes, seconds) = (magnitude / 3600, magnitude / 60 % 60, magnitude % 60);
    match (minutes, seconds) {
        (0, 0) => format!("{sign}{hours:02}"),
        (_, 0) => format!("{sign}{hours:02}:{minutes:02}"),
        _ => format!("{sign}{hours:02}:{minutes:02}:{seconds:02}"),
    }
}

pub(crate) fn format_timestamp(micros: i64) -> String {
    format_local(micros, None)
}

pub(crate) fn format_timestamptz(micros: i64) -> String {
    format_timestamptz_in(micros, session_zone())
}

pub(crate) fn format_timestamptz_in(micros: i64, zone: TimeZone) -> String {
    format_timestamptz_at(micros, zone.offset_at(micros))
}

pub(crate) fn format_timestamptz_at(micros: i64, offset: i32) -> String {
    format_local(to_local(micros, offset), Some(offset))
}

pub(crate) fn to_local(micros: i64, offset: i32) -> i64 {
    micros.saturating_add(i64::from(offset) * MICROS_PER_SECOND)
}

pub(crate) fn local_to_utc(micros: i64) -> SqlResult<i64> {
    checked_timestamp(session_zone().utc_of_local(micros))
}

pub(crate) fn utc_to_local(micros: i64) -> SqlResult<i64> {
    checked_timestamp(session_zone().local_of_utc(micros))
}

pub(crate) fn round_to_precision(micros: i64, precision: u8) -> SqlResult<i64> {
    if precision >= 6 {
        return Ok(micros);
    }
    let unit = 10i64.pow(u32::from(6 - precision));
    let half = unit / 2;
    let rounded = if micros >= 0 {
        (i128::from(micros) + i128::from(half)) / i128::from(unit) * i128::from(unit)
    } else {
        -((-i128::from(micros) + i128::from(half)) / i128::from(unit) * i128::from(unit))
    };
    checked_timestamp(rounded)
}

pub(crate) fn date_to_timestamp(days: i32) -> SqlResult<i64> {
    checked_timestamp(i128::from(days) * i128::from(MICROS_PER_DAY))
}

pub(crate) fn timestamp_to_date(micros: i64) -> i32 {
    micros.div_euclid(MICROS_PER_DAY) as i32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    Microseconds,
    Milliseconds,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
    Decade,
    Century,
    Millennium,
    DayOfWeek,
    IsoDayOfWeek,
    DayOfYear,
    IsoYear,
    Epoch,
    Julian,
    Timezone,
    TimezoneHour,
    TimezoneMinute,
}

impl Field {
    pub(crate) fn named(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "microsecond" | "microseconds" | "us" | "usec" | "usecs" => Self::Microseconds,
            "millisecond" | "milliseconds" | "ms" | "msec" | "msecs" => Self::Milliseconds,
            "second" | "seconds" | "s" | "sec" | "secs" => Self::Second,
            "minute" | "minutes" | "m" | "min" | "mins" => Self::Minute,
            "hour" | "hours" | "h" | "hr" | "hrs" => Self::Hour,
            "day" | "days" | "d" => Self::Day,
            "week" | "weeks" | "w" => Self::Week,
            "month" | "months" | "mon" | "mons" => Self::Month,
            "quarter" | "qtr" => Self::Quarter,
            "year" | "years" | "y" | "yr" | "yrs" => Self::Year,
            "decade" | "decades" => Self::Decade,
            "century" | "centuries" => Self::Century,
            "millennium" | "millennia" | "millenniums" => Self::Millennium,
            "dow" => Self::DayOfWeek,
            "isodow" => Self::IsoDayOfWeek,
            "doy" => Self::DayOfYear,
            "isoyear" => Self::IsoYear,
            "epoch" => Self::Epoch,
            "julian" => Self::Julian,
            "timezone" => Self::Timezone,
            "timezone_hour" => Self::TimezoneHour,
            "timezone_minute" => Self::TimezoneMinute,
            _ => return None,
        })
    }

    fn is_time_of_day(self) -> bool {
        matches!(
            self,
            Self::Microseconds | Self::Milliseconds | Self::Second | Self::Minute | Self::Hour
        )
    }

    fn is_zone(self) -> bool {
        matches!(
            self,
            Self::Timezone | Self::TimezoneHour | Self::TimezoneMinute
        )
    }
}

fn unit_error(name: &str, kind: DateTimeKind) -> SqlError {
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("unit \"{name}\" not supported for type {}", kind.name()),
    )
}

pub(crate) fn unknown_unit(name: &str, kind: DateTimeKind) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("unit \"{name}\" not recognized for type {}", kind.name()),
    )
}

pub(crate) fn iso_week(days: i64) -> (i64, i64) {
    let weekday = (days + 5).rem_euclid(7) + 1;
    let thursday = days - weekday + 4;
    let (iso_year, _, _) = civil_from_days(thursday);
    let first = days_from_civil(iso_year, 1, 1);
    (iso_year, (thursday - first) / 7 + 1)
}

fn numeric_of(units: i128, scale: u16) -> SqlResult<Numeric> {
    Ok(Numeric::from_scaled(units, scale))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalTime {
    pub(crate) days: i64,
    pub(crate) of_day: i64,
}

impl LocalTime {
    pub(crate) fn of_date(days: i32) -> Self {
        Self {
            days: i64::from(days),
            of_day: 0,
        }
    }

    pub(crate) fn of_micros(micros: i64) -> Self {
        Self {
            days: micros.div_euclid(MICROS_PER_DAY),
            of_day: micros.rem_euclid(MICROS_PER_DAY),
        }
    }

    fn micros(self) -> i128 {
        i128::from(self.days) * i128::from(MICROS_PER_DAY) + i128::from(self.of_day)
    }

    fn hour(self) -> i64 {
        self.of_day / (3600 * MICROS_PER_SECOND)
    }

    fn minute(self) -> i64 {
        self.of_day / (60 * MICROS_PER_SECOND) % 60
    }

    fn second(self) -> i64 {
        self.of_day / MICROS_PER_SECOND % 60
    }

    fn microsecond(self) -> i64 {
        self.of_day % MICROS_PER_SECOND
    }
}

fn century_start(year: i64) -> i64 {
    if year > 0 {
        (year + 99) / 100 * 100 - 99
    } else {
        -((99 - (year - 1)) / 100) * 100 + 1
    }
}

fn millennium_start(year: i64) -> i64 {
    if year > 0 {
        (year + 999) / 1000 * 1000 - 999
    } else {
        -((999 - (year - 1)) / 1000) * 1000 + 1
    }
}

pub(crate) fn extract(
    name: &str,
    kind: DateTimeKind,
    local: LocalTime,
    offset: i32,
) -> SqlResult<Numeric> {
    let field = Field::named(name).ok_or_else(|| unknown_unit(name, kind))?;
    if (kind == DateTimeKind::Date && field.is_time_of_day())
        || (kind != DateTimeKind::TimestampTz && field.is_zone())
    {
        return Err(unit_error(name, kind));
    }
    let LocalTime { days, of_day } = local;
    let (year, month, day) = civil_from_days(days);
    let seconds_with_fraction = i128::from(of_day % (60 * MICROS_PER_SECOND));
    let whole = |value: i64| Ok(Numeric::from_i64(value));
    match field {
        Field::Microseconds => whole(of_day % (60 * MICROS_PER_SECOND)),
        Field::Milliseconds => numeric_of(seconds_with_fraction, 3),
        Field::Second => numeric_of(seconds_with_fraction, 6),
        Field::Minute => whole(local.minute()),
        Field::Hour => whole(local.hour()),
        Field::Day => whole(day),
        Field::Month => whole(month),
        Field::Quarter => whole((month - 1) / 3 + 1),
        Field::Year => whole(if year <= 0 { year - 1 } else { year }),
        Field::Decade => whole(year.div_euclid(10)),
        Field::Century => whole(if year > 0 {
            (year + 99) / 100
        } else {
            -((99 - (year - 1)) / 100)
        }),
        Field::Millennium => whole(if year > 0 {
            (year + 999) / 1000
        } else {
            -((999 - (year - 1)) / 1000)
        }),
        Field::DayOfWeek => whole((days + 6).rem_euclid(7)),
        Field::IsoDayOfWeek => whole((days + 5).rem_euclid(7) + 1),
        Field::DayOfYear => whole(days - days_from_civil(year, 1, 1) + 1),
        Field::Week => whole(iso_week(days).1),
        Field::IsoYear => whole(iso_week(days).0),
        Field::Julian => {
            let julian_days = i128::from(days) + 2_451_545;
            if kind == DateTimeKind::Date {
                Ok(Numeric::from_i128(julian_days))
            } else {
                let micros = julian_days * i128::from(MICROS_PER_DAY) + i128::from(of_day);
                Numeric::from_i128(micros).divide(&Numeric::from_i64(MICROS_PER_DAY))
            }
        }
        Field::Epoch => {
            let unix = local.micros() + i128::from(UNIX_EPOCH_DAYS) * i128::from(MICROS_PER_DAY)
                - i128::from(offset) * i128::from(MICROS_PER_SECOND);
            if kind == DateTimeKind::Date {
                Ok(Numeric::from_i128(unix / i128::from(MICROS_PER_SECOND)))
            } else {
                numeric_of(unix, 6)
            }
        }
        Field::Timezone => whole(i64::from(offset)),
        Field::TimezoneHour => whole(i64::from(offset / 3600)),
        Field::TimezoneMinute => whole(i64::from(offset / 60 % 60)),
    }
}

pub(crate) const MYSQL_UNITS: [&str; 20] = [
    "microsecond",
    "second",
    "minute",
    "hour",
    "day",
    "week",
    "month",
    "quarter",
    "year",
    "second_microsecond",
    "minute_microsecond",
    "minute_second",
    "hour_microsecond",
    "hour_second",
    "hour_minute",
    "day_microsecond",
    "day_second",
    "day_minute",
    "day_hour",
    "year_month",
];

fn mysql_week(days: i64) -> i64 {
    let year = civil_from_days(days).0;
    let first = days_from_civil(year, 1, 1);
    let first_weekday = (first + 6).rem_euclid(7);
    let first_sunday = (7 - first_weekday) % 7;
    let day_of_year = days - first;
    if day_of_year < first_sunday {
        0
    } else {
        (day_of_year - first_sunday) / 7 + 1
    }
}

pub(crate) fn mysql_extract(unit: &str, local: LocalTime) -> Option<i64> {
    let (year, month, day) = civil_from_days(local.days);
    let (hour, minute, second, micro) = (
        local.hour(),
        local.minute(),
        local.second(),
        local.microsecond(),
    );
    let clock = (hour * 100 + minute) * 100 + second;
    Some(match unit {
        "microsecond" => micro,
        "second" => second,
        "minute" => minute,
        "hour" => hour,
        "day" => day,
        "week" => mysql_week(local.days),
        "month" => month,
        "quarter" => (month - 1) / 3 + 1,
        "year" => year,
        "second_microsecond" => second * 1_000_000 + micro,
        "minute_microsecond" => (minute * 100 + second) * 1_000_000 + micro,
        "minute_second" => minute * 100 + second,
        "hour_microsecond" => clock * 1_000_000 + micro,
        "hour_second" => clock,
        "hour_minute" => hour * 100 + minute,
        "day_microsecond" => (day * 1_000_000 + clock) * 1_000_000 + micro,
        "day_second" => day * 1_000_000 + clock,
        "day_minute" => (day * 100 + hour) * 100 + minute,
        "day_hour" => day * 100 + hour,
        "year_month" => year * 100 + month,
        _ => return None,
    })
}

pub(crate) const ORACLE_FIELDS: [&str; 8] = [
    "year",
    "month",
    "day",
    "hour",
    "minute",
    "second",
    "timezone_hour",
    "timezone_minute",
];

pub(crate) fn oracle_extract(field: &str, local: LocalTime, offset: i32) -> Option<Numeric> {
    let (year, month, day) = civil_from_days(local.days);
    Some(match field {
        "year" => Numeric::from_i64(if year <= 0 { year - 1 } else { year }),
        "month" => Numeric::from_i64(month),
        "day" => Numeric::from_i64(day),
        "hour" => Numeric::from_i64(local.hour()),
        "minute" => Numeric::from_i64(local.minute()),
        "second" => {
            let written = Numeric::from_scaled(
                i128::from(local.second() * MICROS_PER_SECOND + local.microsecond()),
                6,
            )
            .to_string();
            Numeric::parse(written.trim_end_matches('0').trim_end_matches('.')).ok()?
        }
        "timezone_hour" => Numeric::from_i64(i64::from(offset / 3600)),
        "timezone_minute" => Numeric::from_i64(i64::from(offset / 60 % 60)),
        _ => return None,
    })
}

pub(crate) fn truncate(name: &str, kind: DateTimeKind, local: LocalTime) -> SqlResult<i64> {
    let field = Field::named(name).ok_or_else(|| unknown_unit(name, kind))?;
    let LocalTime { days, of_day } = local;
    let (year, month, _) = civil_from_days(days);
    let at_day = |days: i64| i128::from(days) * i128::from(MICROS_PER_DAY);
    let within_day = |unit: i64| at_day(days) + i128::from(of_day / unit * unit);
    let micros = match field {
        Field::Microseconds => local.micros(),
        Field::Milliseconds => within_day(1000),
        Field::Second => within_day(MICROS_PER_SECOND),
        Field::Minute => within_day(60 * MICROS_PER_SECOND),
        Field::Hour => within_day(3600 * MICROS_PER_SECOND),
        Field::Day => at_day(days),
        Field::Week => at_day(days - (days + 5).rem_euclid(7)),
        Field::Month => at_day(days_from_civil(year, month, 1)),
        Field::Quarter => at_day(days_from_civil(year, (month - 1) / 3 * 3 + 1, 1)),
        Field::Year => at_day(days_from_civil(year, 1, 1)),
        Field::Decade => at_day(days_from_civil(year.div_euclid(10) * 10, 1, 1)),
        Field::Century => at_day(days_from_civil(century_start(year), 1, 1)),
        Field::Millennium => at_day(days_from_civil(millennium_start(year), 1, 1)),
        _ => return Err(unit_error(name, kind)),
    };
    checked_timestamp(micros)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_days_round_trip_across_the_whole_range() {
        assert_eq!(days_from_civil(2000, 1, 1), 0);
        assert_eq!(days_from_civil(1970, 1, 1), -UNIX_EPOCH_DAYS);
        assert_eq!(days_from_civil(-4713, 11, 24), MIN_DATE);
        assert_eq!(days_from_civil(5_874_897, 12, 31), MAX_DATE);
        assert_eq!(
            days_from_civil(294_277, 1, 1),
            END_TIMESTAMP / MICROS_PER_DAY
        );
        let mut days = MIN_DATE;
        while days <= MAX_DATE {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days);
            assert!((1..=days_in_month(year, month)).contains(&day));
            days += 9_973;
        }
    }

    #[test]
    fn dates_parse_and_print_like_postgresql() {
        let cases = [
            ("2024-02-29", "2024-02-29"),
            (" 1999-1-5 ", "1999-01-05"),
            ("0044-03-15 BC", "0044-03-15 BC"),
            ("0001-01-01 BC", "0001-01-01 BC"),
            ("2024-02-29 13:45:01", "2024-02-29"),
            ("4714-11-24 BC", "4714-11-24 BC"),
            ("5874897-12-31", "5874897-12-31"),
        ];
        for (input, output) in cases {
            assert_eq!(format_date(parse_date(input).unwrap()), output, "{input}");
        }
        let code = |text: &str| parse_date(text).unwrap_err().state().code().to_owned();
        assert_eq!(code("2023-02-29"), "22008");
        assert_eq!(code("2024-13-01"), "22008");
        assert_eq!(code("0000-01-01"), "22008");
        assert_eq!(code("4714-11-23 BC"), "22008");
        assert_eq!(code("5874898-01-01"), "22008");
        assert_eq!(code("yesterday-ish"), "22007");
        assert_eq!(code("2024/01/01"), "22007");
        assert_eq!(code("24-01-01"), "22007");
        assert_eq!(code(""), "22007");
    }

    #[test]
    fn timestamps_parse_round_and_print_like_postgresql() {
        let shown = |text: &str| format_timestamp(parse_timestamp(text, false).unwrap());
        assert_eq!(
            shown("2024-02-29 13:45:01.123456"),
            "2024-02-29 13:45:01.123456"
        );
        assert_eq!(shown("2024-02-29T13:45:01.5"), "2024-02-29 13:45:01.5");
        assert_eq!(shown("2024-02-29 13:45"), "2024-02-29 13:45:00");
        assert_eq!(shown("2024-02-29"), "2024-02-29 00:00:00");
        assert_eq!(
            shown("2024-02-29 13:45:01.1234565"),
            "2024-02-29 13:45:01.123457"
        );
        assert_eq!(shown("2024-02-29 23:59:59.9999999"), "2024-03-01 00:00:00");
        assert_eq!(shown("2024-02-29 24:00:00"), "2024-03-01 00:00:00");
        assert_eq!(shown("2024-02-29 13:45:01+05:30"), "2024-02-29 13:45:01");
        assert_eq!(shown("0044-03-15 12:00:00 BC"), "0044-03-15 12:00:00 BC");
        assert_eq!(
            shown("294276-12-31 23:59:59.999999"),
            "294276-12-31 23:59:59.999999"
        );
        assert!(parse_timestamp("294277-01-01", false).is_err());
        assert!(parse_timestamp("2024-02-29 25:00", false).is_err());
        assert!(parse_timestamp("2024-02-29 12:60", false).is_err());
        assert!(parse_timestamp("2024-02-29 12:00:00.", false).is_err());
        assert!(parse_timestamp("2024-02-29 12:00:00+16", true).is_err());
        let tz = |text: &str| format_timestamptz(parse_timestamp(text, true).unwrap());
        assert_eq!(tz("2024-02-29 13:45:01+05:30"), "2024-02-29 08:15:01+00");
        assert_eq!(tz("2024-02-29 13:45:01Z"), "2024-02-29 13:45:01+00");
        assert_eq!(tz("2024-02-29 13:45:01 UTC"), "2024-02-29 13:45:01+00");
        assert_eq!(tz("2024-02-29 13:45:01-0330"), "2024-02-29 17:15:01+00");
    }

    #[test]
    fn the_session_time_zone_reads_and_shows_time_stamps() {
        let _guard = enter_statement(TimeZone::named("UTC-05:30").unwrap_or_default());
        assert_eq!(session_offset_at(0), 5 * 3600 + 1800);
        let micros = parse_timestamp("2024-01-01 10:00:00", true).unwrap();
        assert_eq!(format_timestamptz(micros), "2024-01-01 10:00:00+05:30");
        assert_eq!(
            format_timestamp(micros),
            "2024-01-01 04:30:00",
            "stored as UTC"
        );
        drop(_guard);
        assert_eq!(session_offset_at(0), 0);
        assert_eq!(time_zone_offset("+03"), Some(-3 * 3600));
        assert_eq!(time_zone_offset("EST5"), Some(-5 * 3600));
        assert_eq!(time_zone_offset("Etc/UTC"), Some(0));
        assert_eq!(time_zone_offset("Europe/Rome"), None);
        assert_eq!(time_zone_offset("+99"), None);
    }

    #[test]
    fn extract_and_truncate_follow_the_calendar() {
        let micros = parse_timestamp("2024-02-29 13:45:01.5", false).unwrap();
        let field = |name: &str| {
            extract(
                name,
                DateTimeKind::Timestamp,
                LocalTime::of_micros(micros),
                0,
            )
            .unwrap()
            .to_string()
        };
        assert_eq!(field("year"), "2024");
        assert_eq!(field("month"), "2");
        assert_eq!(field("day"), "29");
        assert_eq!(field("hour"), "13");
        assert_eq!(field("minute"), "45");
        assert_eq!(field("second"), "1.500000");
        assert_eq!(field("milliseconds"), "1500.000");
        assert_eq!(field("dow"), "4");
        assert_eq!(field("isodow"), "4");
        assert_eq!(field("doy"), "60");
        assert_eq!(field("quarter"), "1");
        assert_eq!(field("week"), "9");
        assert_eq!(field("epoch"), "1709214301.500000");
        assert_eq!(field("century"), "21");
        let date = LocalTime::of_date(parse_date("2021-01-03").unwrap());
        let date_field =
            |name: &str| extract(name, DateTimeKind::Date, date, 0).map(|value| value.to_string());
        assert_eq!(date_field("week").unwrap(), "53");
        assert_eq!(date_field("isoyear").unwrap(), "2020");
        assert_eq!(date_field("dow").unwrap(), "0");
        assert_eq!(date_field("epoch").unwrap(), "1609632000");
        assert_eq!(
            date_field("hour").unwrap_err().message(),
            "unit \"hour\" not supported for type date"
        );
        assert!(date_field("fortnight").is_err());
        let truncated = |name: &str| {
            format_timestamp(
                truncate(name, DateTimeKind::Timestamp, LocalTime::of_micros(micros)).unwrap(),
            )
        };
        assert_eq!(truncated("hour"), "2024-02-29 13:00:00");
        assert_eq!(truncated("day"), "2024-02-29 00:00:00");
        assert_eq!(truncated("week"), "2024-02-26 00:00:00");
        assert_eq!(truncated("month"), "2024-02-01 00:00:00");
        assert_eq!(truncated("quarter"), "2024-01-01 00:00:00");
        assert_eq!(truncated("year"), "2024-01-01 00:00:00");
        assert_eq!(truncated("century"), "2001-01-01 00:00:00");
        assert_eq!(truncated("millennium"), "2001-01-01 00:00:00");
        let bc = parse_timestamp("0044-03-15 BC", false).unwrap();
        assert_eq!(
            extract("year", DateTimeKind::Timestamp, LocalTime::of_micros(bc), 0)
                .unwrap()
                .to_string(),
            "-44"
        );
    }

    #[test]
    fn centuries_millennia_and_decades_truncate_before_christ_as_postgresql_does() {
        let truncated = |field: &str, text: &str| {
            let local = LocalTime::of_micros(parse_timestamp(text, false).unwrap());
            format_timestamp(truncate(field, DateTimeKind::Timestamp, local).unwrap())
        };
        assert_eq!(
            truncated("century", "0050-01-01 BC"),
            "0100-01-01 00:00:00 BC"
        );
        assert_eq!(
            truncated("century", "0100-01-01 BC"),
            "0100-01-01 00:00:00 BC"
        );
        assert_eq!(
            truncated("century", "0101-06-01 BC"),
            "0200-01-01 00:00:00 BC"
        );
        assert_eq!(
            truncated("century", "0001-01-01 BC"),
            "0100-01-01 00:00:00 BC"
        );
        assert_eq!(truncated("century", "0001-01-01"), "0001-01-01 00:00:00");
        assert_eq!(truncated("century", "2000-12-31"), "1901-01-01 00:00:00");
        assert_eq!(
            truncated("millennium", "0050-01-01 BC"),
            "1000-01-01 00:00:00 BC"
        );
        assert_eq!(
            truncated("millennium", "1000-01-01 BC"),
            "1000-01-01 00:00:00 BC"
        );
        assert_eq!(
            truncated("millennium", "1001-01-01 BC"),
            "2000-01-01 00:00:00 BC"
        );
        assert_eq!(truncated("millennium", "2000-01-01"), "1001-01-01 00:00:00");
        assert_eq!(
            truncated("decade", "0050-01-01 BC"),
            "0051-01-01 00:00:00 BC"
        );
        assert_eq!(
            truncated("decade", "0001-01-01 BC"),
            "0001-01-01 00:00:00 BC"
        );
        assert_eq!(
            truncated("decade", "0002-01-01 BC"),
            "0011-01-01 00:00:00 BC"
        );
        assert_eq!(truncated("decade", "2024-05-05"), "2020-01-01 00:00:00");
    }

    #[test]
    fn dates_past_the_time_stamp_range_extract_without_overflow() {
        let last = LocalTime::of_date(MAX_DATE as i32);
        let first = LocalTime::of_date(MIN_DATE as i32);
        let field = |name: &str, local| {
            extract(name, DateTimeKind::Date, local, 0)
                .unwrap()
                .to_string()
        };
        assert_eq!(field("year", last), "5874897");
        assert_eq!(field("doy", last), "365");
        assert_eq!(field("epoch", last), "185331706992000");
        assert_eq!(field("julian", last), "2147483493");
        assert_eq!(field("year", first), "-4714");
        assert_eq!(field("epoch", first), "-210866803200");
        assert!(truncate("year", DateTimeKind::TimestampTz, last).is_err());
        assert!(truncate("day", DateTimeKind::Date, first).is_ok());
        assert!(truncate("year", DateTimeKind::Date, first).is_err());
        assert_eq!(mysql_extract("year_month", last), Some(587_489_712));
        assert_eq!(
            oracle_extract("year", first, 0).map(|value| value.to_string()),
            Some("-4714".to_owned())
        );
    }

    #[test]
    fn mysql_units_follow_mysql() {
        let at = |text: &str| LocalTime::of_micros(parse_timestamp(text, false).unwrap());
        assert_eq!(mysql_extract("week", at("2024-01-01")), Some(0));
        assert_eq!(mysql_extract("week", at("2024-01-07")), Some(1));
        assert_eq!(mysql_extract("week", at("2023-01-01")), Some(1));
        assert_eq!(mysql_extract("week", at("2024-12-31")), Some(52));
        let moment = at("2024-02-29 13:45:01.5");
        assert_eq!(mysql_extract("second", moment), Some(1));
        assert_eq!(mysql_extract("microsecond", moment), Some(500_000));
        assert_eq!(mysql_extract("year_month", moment), Some(202_402));
        assert_eq!(mysql_extract("day_minute", moment), Some(291_345));
        assert_eq!(
            mysql_extract("hour_microsecond", moment),
            Some(134_501_500_000)
        );
        assert_eq!(mysql_extract("epoch", moment), None);
    }

    #[test]
    fn precision_rounds_half_away_from_zero() {
        let micros = parse_timestamp("2024-01-01 00:00:00.5", false).unwrap();
        assert_eq!(
            format_timestamp(round_to_precision(micros, 0).unwrap()),
            "2024-01-01 00:00:01"
        );
        let before = parse_timestamp("1999-12-31 23:59:59.5", false).unwrap();
        assert_eq!(
            format_timestamp(round_to_precision(before, 0).unwrap()),
            "1999-12-31 23:59:59"
        );
        let last = END_TIMESTAMP - 1;
        assert!(round_to_precision(last, 0).is_err());
    }

    #[test]
    fn hostile_text_never_panics() {
        let mut seed = 0x1234_5678_9ABC_DEF1u64;
        let alphabet = b"0123456789-:. TZ+bcBCadu\xc3\xa9";
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let length = (seed % 30) as usize;
            let bytes: Vec<u8> = (0..length)
                .map(|at| alphabet[((seed >> (at % 50)) as usize + at * 7) % alphabet.len()])
                .collect();
            let text = String::from_utf8_lossy(&bytes);
            let _ = parse_date(&text);
            let _ = parse_timestamp(&text, true);
            let _ = time_zone_offset(&text);
        }
    }
}
