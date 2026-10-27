use crate::datetime;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::numeric::Numeric;
use std::cmp::Ordering;
use std::fmt;

pub(crate) const MICROS_PER_SECOND: i64 = 1_000_000;
pub(crate) const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SECOND;
pub(crate) const MICROS_PER_HOUR: i64 = 60 * MICROS_PER_MINUTE;
pub(crate) const MICROS_PER_DAY: i64 = 24 * MICROS_PER_HOUR;
const DAYS_PER_MONTH: i64 = 30;
const MONTHS_PER_YEAR: i64 = 12;
const MICROS_PER_MONTH: i128 = DAYS_PER_MONTH as i128 * MICROS_PER_DAY as i128;
const MAX_INPUT_BYTES: usize = 4096;
const I64_BOUND: f64 = 9_223_372_036_854_775_808.0;

/// An `interval`: a span of months, days and microseconds, kept apart as PostgreSQL keeps them,
/// because a month's and a day's length depend on the date they are added to.
///
/// Intervals compare and hash as PostgreSQL's do, by their length with a month counted as 30
/// days and a day as 24 hours, so `'1 mon'` equals `'30 days'` and `'720 hours'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Interval {
    /// Whole months (12 to a year).
    pub months: i32,
    /// Whole days.
    pub days: i32,
    /// Microseconds.
    pub micros: i64,
}

/// The fields an `interval` column keeps, as in `interval year to month`: values are truncated
/// to the last field, and a number written without a unit counts in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntervalFields {
    /// `YEAR`.
    Year,
    /// `MONTH`.
    Month,
    /// `DAY`.
    Day,
    /// `HOUR`.
    Hour,
    /// `MINUTE`.
    Minute,
    /// `SECOND`.
    Second,
    /// `YEAR TO MONTH`.
    YearToMonth,
    /// `DAY TO HOUR`.
    DayToHour,
    /// `DAY TO MINUTE`.
    DayToMinute,
    /// `DAY TO SECOND`.
    DayToSecond,
    /// `HOUR TO MINUTE`.
    HourToMinute,
    /// `HOUR TO SECOND`.
    HourToSecond,
    /// `MINUTE TO SECOND`.
    MinuteToSecond,
}

impl IntervalFields {
    pub(crate) const ALL: [Self; 13] = [
        Self::Year,
        Self::Month,
        Self::Day,
        Self::Hour,
        Self::Minute,
        Self::Second,
        Self::YearToMonth,
        Self::DayToHour,
        Self::DayToMinute,
        Self::DayToSecond,
        Self::HourToMinute,
        Self::HourToSecond,
        Self::MinuteToSecond,
    ];

    /// The fields as SQL writes them, such as `year to month`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Year => "year",
            Self::Month => "month",
            Self::Day => "day",
            Self::Hour => "hour",
            Self::Minute => "minute",
            Self::Second => "second",
            Self::YearToMonth => "year to month",
            Self::DayToHour => "day to hour",
            Self::DayToMinute => "day to minute",
            Self::DayToSecond => "day to second",
            Self::HourToMinute => "hour to minute",
            Self::HourToSecond => "hour to second",
            Self::MinuteToSecond => "minute to second",
        }
    }

    pub(crate) fn code(self) -> u8 {
        Self::ALL
            .iter()
            .position(|&fields| fields == self)
            .map_or(0, |index| index as u8 + 1)
    }

    pub(crate) fn from_code(code: u8) -> Option<Self> {
        Self::ALL.get(usize::from(code).checked_sub(1)?).copied()
    }

    pub(crate) fn from_words(first: &str, last: Option<&str>) -> Option<Self> {
        Some(match (first, last) {
            ("year", None) => Self::Year,
            ("month", None) => Self::Month,
            ("day", None) => Self::Day,
            ("hour", None) => Self::Hour,
            ("minute", None) => Self::Minute,
            ("second", None) => Self::Second,
            ("year", Some("month")) => Self::YearToMonth,
            ("day", Some("hour")) => Self::DayToHour,
            ("day", Some("minute")) => Self::DayToMinute,
            ("day", Some("second")) => Self::DayToSecond,
            ("hour", Some("minute")) => Self::HourToMinute,
            ("hour", Some("second")) => Self::HourToSecond,
            ("minute", Some("second")) => Self::MinuteToSecond,
            _ => return None,
        })
    }

    pub(crate) fn ends_in_seconds(self) -> bool {
        matches!(
            self,
            Self::Second | Self::DayToSecond | Self::HourToSecond | Self::MinuteToSecond
        )
    }

    fn bare_number_unit(self) -> Unit {
        match self {
            Self::Year => Unit::Year,
            Self::Month | Self::YearToMonth => Unit::Month,
            Self::Day => Unit::Day,
            Self::Hour | Self::DayToHour => Unit::Hour,
            Self::Minute | Self::DayToMinute | Self::HourToMinute => Unit::Minute,
            _ => Unit::Second,
        }
    }
}

pub(crate) fn out_of_range() -> SqlError {
    SqlError::new(SqlState::DATETIME_FIELD_OVERFLOW, "interval out of range")
}

fn syntax(text: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_DATETIME_FORMAT,
        format!("invalid input syntax for type interval: \"{text}\""),
    )
}

fn field_overflow(text: &str) -> SqlError {
    SqlError::new(
        SqlState::INTERVAL_FIELD_OVERFLOW,
        format!("interval field value out of range: \"{text}\""),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Microsecond,
    Millisecond,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Year,
    Decade,
    Century,
    Millennium,
}

fn unit_named(word: &str) -> Option<Unit> {
    Some(match word.to_ascii_lowercase().as_str() {
        "microsecond" | "microseconds" | "us" | "usec" | "usecs" | "useconds" => Unit::Microsecond,
        "millisecond" | "milliseconds" | "ms" | "msec" | "msecs" | "mseconds" => Unit::Millisecond,
        "second" | "seconds" | "s" | "sec" | "secs" => Unit::Second,
        "minute" | "minutes" | "m" | "min" | "mins" => Unit::Minute,
        "hour" | "hours" | "h" | "hr" | "hrs" => Unit::Hour,
        "day" | "days" | "d" => Unit::Day,
        "week" | "weeks" | "w" => Unit::Week,
        "month" | "months" | "mon" | "mons" => Unit::Month,
        "year" | "years" | "y" | "yr" | "yrs" => Unit::Year,
        "decade" | "decades" | "dec" | "decs" => Unit::Decade,
        "century" | "centuries" | "c" | "cent" => Unit::Century,
        "millennium" | "millennia" | "mil" | "mils" => Unit::Millennium,
        _ => return None,
    })
}

#[derive(Default)]
struct Totals {
    months: i128,
    days: i128,
    micros: i128,
}

const TOTAL_LIMIT: i128 = 1 << 100;
const DIGITS_LIMIT: i128 = 1 << 64;

impl Totals {
    fn add(&mut self, whole: i128, fraction: f64, unit: Unit) -> Option<()> {
        let years = |count: i128, totals: &mut Self| {
            totals.months += whole * count * i128::from(MONTHS_PER_YEAR as i32);
            totals.months +=
                (fraction * count as f64 * MONTHS_PER_YEAR as f64).round_ties_even() as i128;
        };
        match unit {
            Unit::Microsecond => self.micros += whole + fraction.round_ties_even() as i128,
            Unit::Millisecond => {
                self.micros += whole * 1000 + (fraction * 1000.0).round_ties_even() as i128;
            }
            Unit::Second => self.add_micros(whole, fraction, MICROS_PER_SECOND),
            Unit::Minute => self.add_micros(whole, fraction, MICROS_PER_MINUTE),
            Unit::Hour => self.add_micros(whole, fraction, MICROS_PER_HOUR),
            Unit::Day => {
                self.days += whole;
                self.add_fraction_of_day(fraction);
            }
            Unit::Week => {
                self.days += whole * 7;
                self.add_fraction_of_days(fraction * 7.0);
            }
            Unit::Month => {
                self.months += whole;
                self.add_fraction_of_days(fraction * DAYS_PER_MONTH as f64);
            }
            Unit::Year => years(1, self),
            Unit::Decade => years(10, self),
            Unit::Century => years(100, self),
            Unit::Millennium => years(1000, self),
        }
        let fits = |value: i128| (-TOTAL_LIMIT..TOTAL_LIMIT).contains(&value);
        (fits(self.months) && fits(self.days) && fits(self.micros)).then_some(())
    }

    fn add_micros(&mut self, whole: i128, fraction: f64, unit: i64) {
        self.micros +=
            whole * i128::from(unit) + (fraction * unit as f64).round_ties_even() as i128;
    }

    fn add_fraction_of_days(&mut self, days: f64) {
        let whole = days.trunc();
        self.days += whole as i128;
        self.add_fraction_of_day(days - whole);
    }

    fn add_fraction_of_day(&mut self, fraction: f64) {
        self.micros += (fraction * MICROS_PER_DAY as f64).round_ties_even() as i128;
    }

    fn finish(self) -> Option<Interval> {
        Some(Interval {
            months: i32::try_from(self.months).ok()?,
            days: i32::try_from(self.days).ok()?,
            micros: i64::try_from(self.micros).ok()?,
        })
    }
}

struct Number {
    negative: bool,
    whole: i128,
    fraction: f64,
    has_point: bool,
}

struct Cursor<'t> {
    bytes: &'t [u8],
    at: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn peek_at(&self, ahead: usize) -> Option<u8> {
        self.bytes.get(self.at + ahead).copied()
    }

    fn spaces(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.at += 1;
        }
    }

    fn digits(&mut self) -> Option<(i128, usize)> {
        let start = self.at;
        let mut value: i128 = 0;
        while let Some(byte) = self.peek().filter(u8::is_ascii_digit) {
            value = value
                .checked_mul(10)?
                .checked_add(i128::from(byte - b'0'))?;
            if value >= DIGITS_LIMIT {
                return None;
            }
            self.at += 1;
        }
        Some((value, self.at - start))
    }

    fn fraction(&mut self) -> f64 {
        let start = self.at;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.at += 1;
        }
        let digits = &self.bytes[start..self.at];
        let mut value = 0.0;
        let mut scale = 0.1;
        for &digit in digits.iter().take(18) {
            value += f64::from(digit - b'0') * scale;
            scale /= 10.0;
        }
        value
    }

    fn word(&mut self) -> &str {
        let start = self.at;
        while self.peek().is_some_and(|byte| byte.is_ascii_alphabetic()) {
            self.at += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.at]).unwrap_or("")
    }

    fn number(&mut self) -> Result<Number, Failure> {
        let negative = match self.peek() {
            Some(b'-') => {
                self.at += 1;
                true
            }
            Some(b'+') => {
                self.at += 1;
                false
            }
            _ => false,
        };
        let (whole, count) = self.digits().ok_or(Failure::Range)?;
        let has_point = self.peek() == Some(b'.');
        let mut fraction = 0.0;
        if has_point {
            self.at += 1;
            let before = self.at;
            fraction = self.fraction();
            if count == 0 && self.at == before {
                return Err(Failure::Syntax);
            }
        } else if count == 0 {
            return Err(Failure::Syntax);
        }
        Ok(Number {
            negative,
            whole: if negative { -whole } else { whole },
            fraction: if negative { -fraction } else { fraction },
            has_point,
        })
    }
}

enum Failure {
    Syntax,
    Range,
}

fn clock_micros(cursor: &mut Cursor<'_>, hours: &Number) -> Result<i128, Failure> {
    if hours.has_point {
        return Err(Failure::Syntax);
    }
    cursor.at += 1;
    let (minutes, count) = cursor.digits().ok_or(Failure::Range)?;
    if count == 0 {
        return Err(Failure::Syntax);
    }
    let mut seconds = 0i128;
    let mut fraction = 0i128;
    if cursor.peek() == Some(b':') {
        cursor.at += 1;
        let (whole, count) = cursor.digits().ok_or(Failure::Range)?;
        if count == 0 {
            return Err(Failure::Syntax);
        }
        seconds = whole;
        if cursor.peek() == Some(b'.') {
            cursor.at += 1;
            fraction = (cursor.fraction() * MICROS_PER_SECOND as f64).round_ties_even() as i128;
        }
    } else if cursor.peek() == Some(b'.') {
        cursor.at += 1;
        let (minutes_as_seconds, seconds_fraction) = (minutes, cursor.fraction());
        if minutes_as_seconds > 59 {
            return Err(Failure::Range);
        }
        let total = hours.whole.abs() * i128::from(MICROS_PER_MINUTE)
            + minutes_as_seconds * i128::from(MICROS_PER_SECOND)
            + (seconds_fraction * MICROS_PER_SECOND as f64).round_ties_even() as i128;
        return Ok(if hours.negative { -total } else { total });
    }
    if minutes > 59 || seconds > 60 {
        return Err(Failure::Range);
    }
    let total = hours.whole.abs() * i128::from(MICROS_PER_HOUR)
        + minutes * i128::from(MICROS_PER_MINUTE)
        + seconds * i128::from(MICROS_PER_SECOND)
        + fraction;
    Ok(if hours.negative { -total } else { total })
}

fn postgres_style(text: &str, fields: Option<IntervalFields>) -> Result<Totals, Failure> {
    let mut cursor = Cursor {
        bytes: text.as_bytes(),
        at: 0,
    };
    cursor.spaces();
    if cursor.peek() == Some(b'@') {
        cursor.at += 1;
    }
    let mut totals = Totals::default();
    let mut negate = false;
    let mut anything = false;
    let bare_unit = fields.map_or(Unit::Second, IntervalFields::bare_number_unit);
    loop {
        cursor.spaces();
        let Some(byte) = cursor.peek() else {
            break;
        };
        if negate {
            return Err(Failure::Syntax);
        }
        if byte.is_ascii_alphabetic() {
            if cursor.word().eq_ignore_ascii_case("ago") && anything {
                negate = true;
                continue;
            }
            return Err(Failure::Syntax);
        }
        let number = cursor.number()?;
        anything = true;
        if cursor.peek() == Some(b':') {
            let micros = clock_micros(&mut cursor, &number)?;
            totals
                .add(micros, 0.0, Unit::Microsecond)
                .ok_or(Failure::Range)?;
            continue;
        }
        if cursor.peek() == Some(b'-')
            && cursor.peek_at(1).is_some_and(|byte| byte.is_ascii_digit())
            && !number.has_point
        {
            cursor.at += 1;
            let (months, _) = cursor.digits().ok_or(Failure::Range)?;
            if months > 11 {
                return Err(Failure::Range);
            }
            let months = if number.negative { -months } else { months };
            totals
                .add(number.whole, 0.0, Unit::Year)
                .ok_or(Failure::Range)?;
            totals.add(months, 0.0, Unit::Month).ok_or(Failure::Range)?;
            continue;
        }
        cursor.spaces();
        let unit = if cursor.peek().is_some_and(|byte| byte.is_ascii_alphabetic()) {
            let mark = cursor.at;
            let word = cursor.word();
            match unit_named(word) {
                Some(unit) => unit,
                None if word.eq_ignore_ascii_case("ago") => {
                    cursor.at = mark;
                    bare_unit
                }
                None => return Err(Failure::Syntax),
            }
        } else if !number.has_point && starts_clock(&cursor) {
            Unit::Day
        } else {
            bare_unit
        };
        totals
            .add(number.whole, number.fraction, unit)
            .ok_or(Failure::Range)?;
    }
    if !anything {
        return Err(Failure::Syntax);
    }
    if negate {
        totals.months = -totals.months;
        totals.days = -totals.days;
        totals.micros = -totals.micros;
    }
    Ok(totals)
}

fn starts_clock(cursor: &Cursor<'_>) -> bool {
    let rest = &cursor.bytes[cursor.at..];
    let sign = usize::from(matches!(rest.first(), Some(b'+' | b'-')));
    let digits = rest[sign..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    digits > 0 && rest.get(sign + digits) == Some(&b':')
}

fn iso_8601(text: &str) -> Result<Totals, Failure> {
    let mut cursor = Cursor {
        bytes: text.as_bytes(),
        at: 1,
    };
    let mut totals = Totals::default();
    let mut in_time = false;
    let mut anything = false;
    while let Some(byte) = cursor.peek() {
        if byte == b'T' || byte == b't' {
            if in_time {
                return Err(Failure::Syntax);
            }
            in_time = true;
            cursor.at += 1;
            continue;
        }
        let number = cursor.number()?;
        let designator = cursor.peek().ok_or(Failure::Syntax)?.to_ascii_uppercase();
        cursor.at += 1;
        let unit = match (designator, in_time) {
            (b'Y', false) => Unit::Year,
            (b'M', false) => Unit::Month,
            (b'W', false) => Unit::Week,
            (b'D', false) => Unit::Day,
            (b'H', true) => Unit::Hour,
            (b'M', true) => Unit::Minute,
            (b'S', true) => Unit::Second,
            _ => return Err(Failure::Syntax),
        };
        totals
            .add(number.whole, number.fraction, unit)
            .ok_or(Failure::Range)?;
        anything = true;
    }
    if anything {
        Ok(totals)
    } else {
        Err(Failure::Syntax)
    }
}

impl Interval {
    /// The interval's length in microseconds with a month counted as 30 days, the measure
    /// PostgreSQL compares and hashes intervals by.
    pub fn span(self) -> i128 {
        i128::from(self.months) * MICROS_PER_MONTH
            + i128::from(self.days) * i128::from(MICROS_PER_DAY)
            + i128::from(self.micros)
    }

    pub(crate) fn from_span(span: i128) -> Option<Self> {
        let months = (span / MICROS_PER_MONTH).clamp(i128::from(i32::MIN), i128::from(i32::MAX));
        let rest = span - months * MICROS_PER_MONTH;
        let days =
            (rest / i128::from(MICROS_PER_DAY)).clamp(i128::from(i32::MIN), i128::from(i32::MAX));
        let micros = rest - days * i128::from(MICROS_PER_DAY);
        Some(Self {
            months: i32::try_from(months).ok()?,
            days: i32::try_from(days).ok()?,
            micros: i64::try_from(micros).ok()?,
        })
    }

    pub(crate) fn compare(self, other: Self) -> Ordering {
        self.span().cmp(&other.span())
    }

    pub(crate) fn parse(text: &str, fields: Option<IntervalFields>) -> SqlResult<Self> {
        if text.len() > MAX_INPUT_BYTES {
            return Err(syntax(text.get(..64).unwrap_or("")));
        }
        let trimmed = text.trim();
        let iso = trimmed
            .strip_prefix(['P', 'p'])
            .is_some_and(|rest| !rest.is_empty());
        let totals = if iso {
            iso_8601(trimmed)
        } else {
            postgres_style(trimmed, fields)
        };
        let interval = match totals {
            Ok(totals) => totals.finish().ok_or_else(|| field_overflow(text))?,
            Err(Failure::Syntax) => return Err(syntax(text)),
            Err(Failure::Range) => return Err(field_overflow(text)),
        };
        Ok(interval)
    }

    pub(crate) fn with_modifier(
        self,
        fields: Option<IntervalFields>,
        precision: Option<u8>,
    ) -> SqlResult<Self> {
        let mut result = self;
        let truncate_time = |micros: i64, unit: i64| micros / unit * unit;
        match fields {
            None | Some(IntervalFields::Second) => {}
            Some(IntervalFields::Year) => {
                result.months = result.months / 12 * 12;
                result.days = 0;
                result.micros = 0;
            }
            Some(IntervalFields::Month | IntervalFields::YearToMonth) => {
                result.days = 0;
                result.micros = 0;
            }
            Some(IntervalFields::Day) => result.micros = 0,
            Some(IntervalFields::Hour | IntervalFields::DayToHour) => {
                result.micros = truncate_time(result.micros, MICROS_PER_HOUR);
            }
            Some(
                IntervalFields::Minute | IntervalFields::DayToMinute | IntervalFields::HourToMinute,
            ) => result.micros = truncate_time(result.micros, MICROS_PER_MINUTE),
            Some(_) => {}
        }
        if let Some(precision) = precision.filter(|&precision| precision < 6) {
            let unit = 10i128.pow(u32::from(6 - precision));
            let half = unit / 2;
            let micros = i128::from(result.micros);
            let rounded = if micros >= 0 {
                (micros + half) / unit * unit
            } else {
                -((-micros + half) / unit * unit)
            };
            result.micros = i64::try_from(rounded).map_err(|_| out_of_range())?;
        }
        Ok(result)
    }

    pub(crate) fn checked_add(self, other: Self) -> SqlResult<Self> {
        Ok(Self {
            months: self
                .months
                .checked_add(other.months)
                .ok_or_else(out_of_range)?,
            days: self.days.checked_add(other.days).ok_or_else(out_of_range)?,
            micros: self
                .micros
                .checked_add(other.micros)
                .ok_or_else(out_of_range)?,
        })
    }

    pub(crate) fn negated(self) -> SqlResult<Self> {
        Ok(Self {
            months: self.months.checked_neg().ok_or_else(out_of_range)?,
            days: self.days.checked_neg().ok_or_else(out_of_range)?,
            micros: self.micros.checked_neg().ok_or_else(out_of_range)?,
        })
    }

    pub(crate) fn checked_sub(self, other: Self) -> SqlResult<Self> {
        Ok(Self {
            months: self
                .months
                .checked_sub(other.months)
                .ok_or_else(out_of_range)?,
            days: self.days.checked_sub(other.days).ok_or_else(out_of_range)?,
            micros: self
                .micros
                .checked_sub(other.micros)
                .ok_or_else(out_of_range)?,
        })
    }

    pub(crate) fn multiply(self, factor: f64) -> SqlResult<Self> {
        self.scaled(|value| value * factor)
    }

    pub(crate) fn divide(self, divisor: f64) -> SqlResult<Self> {
        if divisor == 0.0 {
            return Err(SqlError::new(
                SqlState::DIVISION_BY_ZERO,
                "division by zero",
            ));
        }
        self.scaled(|value| value / divisor)
    }

    fn scaled(self, apply: impl Fn(f64) -> f64) -> SqlResult<Self> {
        let months_exact = apply(f64::from(self.months));
        let days_exact = apply(f64::from(self.days));
        let months = whole_i32(months_exact)?;
        let mut days = whole_i32(days_exact)?;
        let month_remainder_days =
            round_to_micro((months_exact - f64::from(months)) * DAYS_PER_MONTH as f64);
        let mut second_remainder = round_to_micro(
            (days_exact - f64::from(days) + month_remainder_days - month_remainder_days.trunc())
                * 86_400.0,
        );
        if second_remainder.abs() >= 86_400.0 {
            let whole_days = (second_remainder / 86_400.0).trunc();
            days = days
                .checked_add(whole_i32(whole_days)?)
                .ok_or_else(out_of_range)?;
            second_remainder -= whole_days * 86_400.0;
        }
        days = days
            .checked_add(whole_i32(month_remainder_days.trunc())?)
            .ok_or_else(out_of_range)?;
        let micros = (apply(self.micros as f64) + second_remainder * MICROS_PER_SECOND as f64)
            .round_ties_even();
        if !micros.is_finite() || micros < -I64_BOUND || micros >= I64_BOUND {
            return Err(out_of_range());
        }
        Ok(Self {
            months,
            days,
            micros: micros as i64,
        })
    }

    pub(crate) fn justify_days(self) -> SqlResult<Self> {
        let whole_months = self.days / DAYS_PER_MONTH as i32;
        let mut months = self
            .months
            .checked_add(whole_months)
            .ok_or_else(out_of_range)?;
        let mut days = self.days - whole_months * DAYS_PER_MONTH as i32;
        if months > 0 && days < 0 {
            days += DAYS_PER_MONTH as i32;
            months -= 1;
        } else if months < 0 && days > 0 {
            days -= DAYS_PER_MONTH as i32;
            months += 1;
        }
        Ok(Self {
            months,
            days,
            micros: self.micros,
        })
    }

    pub(crate) fn justify_hours(self) -> SqlResult<Self> {
        let whole_days = self.micros / MICROS_PER_DAY;
        let mut micros = self.micros - whole_days * MICROS_PER_DAY;
        let mut days = self
            .days
            .checked_add(i32::try_from(whole_days).map_err(|_| out_of_range())?)
            .ok_or_else(out_of_range)?;
        if days > 0 && micros < 0 {
            micros += MICROS_PER_DAY;
            days -= 1;
        } else if days < 0 && micros > 0 {
            micros -= MICROS_PER_DAY;
            days += 1;
        }
        Ok(Self {
            months: self.months,
            days,
            micros,
        })
    }

    pub(crate) fn justify_interval(self) -> SqlResult<Self> {
        let per_month = DAYS_PER_MONTH as i32;
        let mut months = self.months;
        let mut days = self.days;
        let mut micros = self.micros;
        if (days > 0 && micros > 0) || (days < 0 && micros < 0) {
            let whole_months = days / per_month;
            days -= whole_months * per_month;
            months = months.checked_add(whole_months).ok_or_else(out_of_range)?;
        }
        let whole_days = micros / MICROS_PER_DAY;
        micros -= whole_days * MICROS_PER_DAY;
        days = days
            .checked_add(i32::try_from(whole_days).map_err(|_| out_of_range())?)
            .ok_or_else(out_of_range)?;
        let whole_months = days / per_month;
        days -= whole_months * per_month;
        months = months.checked_add(whole_months).ok_or_else(out_of_range)?;
        if months > 0 && (days < 0 || (days == 0 && micros < 0)) {
            days += per_month;
            months -= 1;
        } else if months < 0 && (days > 0 || (days == 0 && micros > 0)) {
            days -= per_month;
            months += 1;
        }
        if days > 0 && micros < 0 {
            micros += MICROS_PER_DAY;
            days -= 1;
        } else if days < 0 && micros > 0 {
            micros -= MICROS_PER_DAY;
            days += 1;
        }
        Ok(Self {
            months,
            days,
            micros,
        })
    }

    pub(crate) fn extract(self, name: &str) -> SqlResult<Numeric> {
        let field = datetime::Field::named(name).ok_or_else(|| {
            SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("unit \"{name}\" not recognized for type interval"),
            )
        })?;
        let years = i128::from(self.months / 12);
        let months = i128::from(self.months % 12);
        let micros = i128::from(self.micros);
        let within_minute = micros % i128::from(MICROS_PER_MINUTE);
        let whole = |value: i128| Numeric::from_scaled(value, 0);
        Ok(match field {
            datetime::Field::Microseconds => whole(within_minute),
            datetime::Field::Milliseconds => Numeric::from_scaled(within_minute, 3),
            datetime::Field::Second => Numeric::from_scaled(within_minute, 6),
            datetime::Field::Minute => whole(micros / i128::from(MICROS_PER_MINUTE) % 60),
            datetime::Field::Hour => whole(micros / i128::from(MICROS_PER_HOUR)),
            datetime::Field::Day => whole(i128::from(self.days)),
            datetime::Field::Month => whole(months),
            datetime::Field::Quarter => whole(months / 3 + 1),
            datetime::Field::Year => whole(years),
            datetime::Field::Decade => whole(years / 10),
            datetime::Field::Century => whole(years / 100),
            datetime::Field::Millennium => whole(years / 1000),
            datetime::Field::Epoch => {
                let quarter_days = 1461 * years + 120 * months + 4 * i128::from(self.days);
                Numeric::from_scaled(
                    quarter_days * 21_600 * i128::from(MICROS_PER_SECOND) + micros,
                    6,
                )
            }
            _ => {
                return Err(SqlError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("unit \"{name}\" not supported for type interval"),
                ));
            }
        })
    }
}

fn whole_i32(value: f64) -> SqlResult<i32> {
    if value.is_nan() || value < -2_147_483_648.0 || value >= 2_147_483_648.0 {
        return Err(out_of_range());
    }
    Ok(value as i32)
}

fn round_to_micro(value: f64) -> f64 {
    (value * 1_000_000.0).round_ties_even() / 1_000_000.0
}

impl fmt::Display for Interval {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let years = i64::from(self.months / 12);
        let months = i64::from(self.months % 12);
        let mut written = false;
        let mut before_negative = false;
        let mut part = |out: &mut fmt::Formatter<'_>, value: i64, unit: &str| -> fmt::Result {
            if value == 0 {
                return Ok(());
            }
            let space = if written { " " } else { "" };
            let plus = if before_negative && value > 0 {
                "+"
            } else {
                ""
            };
            let plural = if value != 1 { "s" } else { "" };
            write!(out, "{space}{plus}{value} {unit}{plural}")?;
            before_negative = value < 0;
            written = true;
            Ok(())
        };
        part(out, years, "year")?;
        part(out, months, "mon")?;
        part(out, i64::from(self.days), "day")?;
        if !written || self.micros != 0 {
            let micros = self.micros;
            let hours = (micros / MICROS_PER_HOUR).unsigned_abs();
            let minutes = (micros / MICROS_PER_MINUTE % 60).unsigned_abs();
            let seconds = (micros / MICROS_PER_SECOND % 60).unsigned_abs();
            let fraction = (micros % MICROS_PER_SECOND).unsigned_abs();
            let space = if written { " " } else { "" };
            let sign = if micros < 0 {
                "-"
            } else if before_negative {
                "+"
            } else {
                ""
            };
            write!(out, "{space}{sign}{hours:02}:{minutes:02}:{seconds:02}")?;
            if fraction != 0 {
                let digits = format!("{fraction:06}");
                write!(out, ".{}", digits.trim_end_matches('0'))?;
            }
        }
        Ok(())
    }
}

pub(crate) fn add_to_timestamp(local: i64, interval: Interval) -> SqlResult<i64> {
    let mut micros = i128::from(local);
    if interval.months != 0 {
        let day = local.div_euclid(MICROS_PER_DAY);
        let of_day = local.rem_euclid(MICROS_PER_DAY);
        let (year, month, day_of_month) = datetime::civil_from_days(day);
        let total = year * 12 + (month - 1) + i64::from(interval.months);
        let (year, month) = (total.div_euclid(12), total.rem_euclid(12) + 1);
        if !(-10_000_000..=10_000_000).contains(&year) {
            return Err(datetime::timestamp_out_of_range());
        }
        let day_of_month = day_of_month.min(datetime::days_in_month(year, month));
        micros = i128::from(datetime::days_from_civil(year, month, day_of_month))
            * i128::from(MICROS_PER_DAY)
            + i128::from(of_day);
        datetime::checked_timestamp(micros)?;
    }
    if interval.days != 0 {
        micros += i128::from(interval.days) * i128::from(MICROS_PER_DAY);
        datetime::checked_timestamp(micros)?;
    }
    datetime::checked_timestamp(micros + i128::from(interval.micros))
}

pub(crate) fn between_timestamps(later: i64, earlier: i64) -> SqlResult<Interval> {
    let difference = later.checked_sub(earlier).ok_or_else(out_of_range)?;
    Ok(Interval {
        months: 0,
        days: i32::try_from(difference / MICROS_PER_DAY).map_err(|_| out_of_range())?,
        micros: difference % MICROS_PER_DAY,
    })
}

pub(crate) fn age(first: i64, second: i64) -> SqlResult<Interval> {
    let parts = |micros: i64| {
        let (year, month, day) = datetime::civil_from_days(micros.div_euclid(MICROS_PER_DAY));
        let of_day = micros.rem_euclid(MICROS_PER_DAY);
        [
            year,
            month,
            day,
            of_day / MICROS_PER_HOUR,
            of_day / MICROS_PER_MINUTE % 60,
            of_day / MICROS_PER_SECOND % 60,
            of_day % MICROS_PER_SECOND,
        ]
    };
    let one = parts(first);
    let two = parts(second);
    let mut fields: [i64; 7] = std::array::from_fn(|index| one[index] - two[index]);
    let backwards = first < second;
    if backwards {
        fields = fields.map(|field| -field);
    }
    let [
        mut year,
        mut month,
        mut day,
        mut hour,
        mut minute,
        mut second,
        mut fraction,
    ] = fields;
    while fraction < 0 {
        fraction += MICROS_PER_SECOND;
        second -= 1;
    }
    while second < 0 {
        second += 60;
        minute -= 1;
    }
    while minute < 0 {
        minute += 60;
        hour -= 1;
    }
    while hour < 0 {
        hour += 24;
        day -= 1;
    }
    while day < 0 {
        let (borrow_year, borrow_month) = if backwards {
            (one[0], one[1])
        } else {
            (two[0], two[1])
        };
        day += datetime::days_in_month(borrow_year, borrow_month);
        month -= 1;
    }
    while month < 0 {
        month += 12;
        year -= 1;
    }
    let sign = if backwards { -1 } else { 1 };
    let months = (year * 12 + month) * sign;
    let micros = (((hour * 60 + minute) * 60 + second) * MICROS_PER_SECOND + fraction) * sign;
    Ok(Interval {
        months: i32::try_from(months).map_err(|_| out_of_range())?,
        days: i32::try_from(day * sign).map_err(|_| out_of_range())?,
        micros,
    })
}

#[cfg(test)]
mod tests {
    use super::{Interval, IntervalFields, add_to_timestamp, age, between_timestamps};
    use crate::datetime::{days_from_civil, parse_timestamp};

    fn interval(text: &str) -> Interval {
        Interval::parse(text, None).unwrap_or_else(|error| panic!("{text}: {error}"))
    }

    fn shown(text: &str) -> String {
        interval(text).to_string()
    }

    #[test]
    fn input_forms_read_as_postgresql_reads_them() {
        let cases = [
            ("1 day 02:03:04", "1 day 02:03:04"),
            ("1 year 2 mons", "1 year 2 mons"),
            ("P1Y2M3DT4H5M6S", "1 year 2 mons 3 days 04:05:06"),
            ("p2w", "14 days"),
            ("1.5 years", "1 year 6 mons"),
            ("1.5 months", "1 mon 15 days"),
            ("1.5 days", "1 day 12:00:00"),
            ("-1 day +02:00:00", "-1 days +02:00:00"),
            ("1 day ago", "-1 days"),
            ("@ 3 hours", "03:00:00"),
            ("90", "00:01:30"),
            ("1-2", "1 year 2 mons"),
            ("3 4:05:06", "3 days 04:05:06"),
            ("-04:05", "-04:05:00"),
            ("0", "00:00:00"),
            ("1 millennium 2 centuries 3 decades", "1230 years"),
            ("1.25 seconds", "00:00:01.25"),
            ("2 weeks 1 day", "15 days"),
            ("-1 year -2 mons +3 days", "-1 years -2 mons +3 days"),
            ("1 mon -1 day", "1 mon -1 days"),
            ("100 hours", "100:00:00"),
            ("1 day -00:00:01", "1 day -00:00:01"),
        ];
        for (text, expected) in cases {
            assert_eq!(shown(text), expected, "{text}");
        }
    }

    #[test]
    fn bad_input_is_a_syntax_or_range_error() {
        for text in [
            "",
            "ago",
            "1 fortnight",
            "1 day day",
            "P",
            "PT",
            "P1H",
            "1 day ago ago",
            "--1",
            "1:",
            "x",
        ] {
            let error = Interval::parse(text, None).expect_err(text);
            assert_eq!(error.state().code(), "22007", "{text}: {error}");
        }
        for text in [
            "2147483648 days",
            "178956971 years",
            "9999999999999999999 hours",
            "1:60",
            "18446744073709551616 days",
            "P99999999999999999999D",
        ] {
            let error = Interval::parse(text, None).expect_err(text);
            assert_eq!(error.state().code(), "22015", "{text}: {error}");
        }
        assert!(Interval::parse(&"1".repeat(5000), None).is_err());
    }

    #[test]
    fn fields_truncate_and_give_bare_numbers_their_unit() {
        let with = |text: &str, fields| {
            Interval::parse(text, Some(fields))
                .and_then(|value| value.with_modifier(Some(fields), None))
                .unwrap()
                .to_string()
        };
        assert_eq!(
            with("1 year 2 mons 3 days", IntervalFields::YearToMonth),
            "1 year 2 mons"
        );
        assert_eq!(with("5", IntervalFields::Day), "5 days");
        assert_eq!(with("5", IntervalFields::Year), "5 years");
        assert_eq!(
            with("1 day 04:05:06", IntervalFields::DayToHour),
            "1 day 04:00:00"
        );
        assert_eq!(with("1 year 11 mons", IntervalFields::Year), "1 year");
        let rounded = interval("00:00:01.555555")
            .with_modifier(None, Some(2))
            .unwrap();
        assert_eq!(rounded.to_string(), "00:00:01.56");
    }

    #[test]
    fn equality_counts_a_month_as_thirty_days() {
        assert_eq!(interval("1 mon").span(), interval("30 days").span());
        assert_eq!(interval("1 day").span(), interval("24 hours").span());
        assert!(interval("1 mon 1 day").span() > interval("1 mon").span());
        for span in [
            0i128,
            1,
            -1,
            i128::from(i64::MAX) * 40,
            -(i128::from(i64::MAX) * 40),
        ] {
            let back = Interval::from_span(span).unwrap();
            assert_eq!(back.span(), span);
        }
        let largest = Interval {
            months: i32::MAX,
            days: i32::MAX,
            micros: i64::MAX,
        };
        assert_eq!(
            Interval::from_span(largest.span()).unwrap().span(),
            largest.span()
        );
        let smallest = Interval {
            months: i32::MIN,
            days: i32::MIN,
            micros: i64::MIN,
        };
        assert_eq!(
            Interval::from_span(smallest.span()).unwrap().span(),
            smallest.span()
        );
    }

    #[test]
    fn multiplication_and_division_spill_fractions_down_as_postgresql_does() {
        assert_eq!(
            interval("1 mon").multiply(1.5).unwrap().to_string(),
            "1 mon 15 days"
        );
        assert_eq!(
            interval("1 day").multiply(0.5).unwrap().to_string(),
            "12:00:00"
        );
        assert_eq!(
            interval("1 mon").divide(4.0).unwrap().to_string(),
            "7 days 12:00:00"
        );
        assert_eq!(
            interval("3 days").divide(2.0).unwrap().to_string(),
            "1 day 12:00:00"
        );
        assert!(interval("1 day").multiply(f64::NAN).is_err());
        assert!(interval("1 day").multiply(f64::INFINITY).is_err());
        assert!(interval("1 day").multiply(1e300).is_err());
        assert!(interval("1 day").divide(0.0).is_err());
        assert!(interval("1 day").divide(1e-300).is_err());
        let huge = Interval {
            months: i32::MAX,
            days: i32::MAX,
            micros: i64::MAX,
        };
        assert!(huge.multiply(2.0).is_err());
        assert!(huge.checked_add(huge).is_err());
        assert!(
            Interval {
                months: i32::MIN,
                days: 0,
                micros: 0
            }
            .negated()
            .is_err()
        );
    }

    #[test]
    fn justify_moves_whole_days_and_months_up() {
        assert_eq!(
            interval("35 days").justify_days().unwrap().to_string(),
            "1 mon 5 days"
        );
        assert_eq!(
            interval("27 hours").justify_hours().unwrap().to_string(),
            "1 day 03:00:00"
        );
        assert_eq!(
            interval("1 mon -1 hour")
                .justify_interval()
                .unwrap()
                .to_string(),
            "29 days 23:00:00"
        );
        assert_eq!(
            interval("-35 days").justify_days().unwrap().to_string(),
            "-1 mons -5 days"
        );
    }

    #[test]
    fn adding_months_clamps_to_the_end_of_the_month() {
        let at = |text: &str| parse_timestamp(text, false).unwrap();
        let shown = |micros| crate::datetime::format_timestamp(micros);
        assert_eq!(
            shown(add_to_timestamp(at("2024-01-31"), interval("1 mon")).unwrap()),
            "2024-02-29 00:00:00"
        );
        assert_eq!(
            shown(add_to_timestamp(at("2023-01-31"), interval("1 mon")).unwrap()),
            "2023-02-28 00:00:00"
        );
        assert_eq!(
            shown(add_to_timestamp(at("2024-03-31 10:00"), interval("-1 mon 1 day")).unwrap()),
            "2024-03-01 10:00:00"
        );
        assert!(add_to_timestamp(at("294276-12-31"), interval("1 year")).is_err());
        let extreme = Interval {
            months: i32::MAX,
            days: i32::MAX,
            micros: i64::MAX,
        };
        assert!(add_to_timestamp(0, extreme).is_err());
        assert!(add_to_timestamp(0, extreme.negated().unwrap()).is_err());
        let days = days_from_civil(2000, 1, 1);
        assert_eq!(days, 0);
    }

    #[test]
    fn differences_and_ages_match_postgresql() {
        let at = |text: &str| parse_timestamp(text, false).unwrap();
        assert_eq!(
            between_timestamps(at("2024-03-02 12:00"), at("2024-03-01"))
                .unwrap()
                .to_string(),
            "1 day 12:00:00"
        );
        assert_eq!(
            age(at("2024-03-01"), at("2023-01-31")).unwrap().to_string(),
            "1 year 1 mon 1 day"
        );
        assert_eq!(
            age(at("2023-01-31"), at("2024-03-01")).unwrap().to_string(),
            "-1 years -1 mons -1 days"
        );
        assert!(
            between_timestamps(
                crate::datetime::END_TIMESTAMP - 1,
                crate::datetime::MIN_TIMESTAMP
            )
            .is_err()
        );
    }

    #[test]
    fn extract_reads_each_field() {
        let value = interval("1 year 14 mons 3 days 25:30:15.5");
        let field = |name: &str| value.extract(name).unwrap().to_string();
        assert_eq!(field("year"), "2");
        assert_eq!(field("month"), "2");
        assert_eq!(field("day"), "3");
        assert_eq!(field("hour"), "25");
        assert_eq!(field("minute"), "30");
        assert_eq!(field("second"), "15.500000");
        assert_eq!(
            interval("1 day").extract("epoch").unwrap().to_string(),
            "86400.000000"
        );
        assert_eq!(
            interval("1 year").extract("epoch").unwrap().to_string(),
            "31557600.000000"
        );
        assert!(value.extract("dow").is_err());
        assert!(value.extract("nonsense").is_err());
    }

    #[test]
    fn arithmetic_matches_a_plain_reference_on_random_values() {
        let mut state = 0x5EED_1234_ABCD_0031u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..5_000 {
            let a = Interval {
                months: (next() % 2001) as i32 - 1000,
                days: (next() % 20001) as i32 - 10000,
                micros: (next() % 2_000_000_000_000) as i64 - 1_000_000_000_000,
            };
            let b = Interval {
                months: (next() % 2001) as i32 - 1000,
                days: (next() % 20001) as i32 - 10000,
                micros: (next() % 2_000_000_000_000) as i64 - 1_000_000_000_000,
            };
            let sum = a.checked_add(b).unwrap();
            assert_eq!(sum.span(), a.span() + b.span());
            assert_eq!(a.checked_sub(b).unwrap().span(), a.span() - b.span());
            assert_eq!(a.negated().unwrap().span(), -a.span());
            assert_eq!(a.compare(b), a.span().cmp(&b.span()));
            let doubled = a.multiply(2.0).unwrap();
            assert_eq!(doubled.span(), 2 * a.span());
            for justified in [a.justify_days(), a.justify_hours(), a.justify_interval()] {
                assert_eq!(justified.unwrap().span(), a.span());
            }
            let text = a.to_string();
            assert_eq!(Interval::parse(&text, None).unwrap(), a, "{text}");
        }
    }

    #[test]
    fn extreme_values_print_and_read_back() {
        for value in [
            Interval {
                months: i32::MAX,
                days: i32::MAX,
                micros: i64::MAX,
            },
            Interval {
                months: i32::MIN,
                days: i32::MIN,
                micros: i64::MIN,
            },
            Interval {
                months: -1,
                days: 1,
                micros: -1,
            },
        ] {
            let text = value.to_string();
            assert_eq!(Interval::parse(&text, None).unwrap(), value, "{text}");
        }
    }
}
