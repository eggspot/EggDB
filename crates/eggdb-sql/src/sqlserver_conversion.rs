use crate::datetime::{self, MICROS_PER_DAY, MICROS_PER_SECOND};
use crate::encoding::Column;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::numeric::Numeric;
use crate::sqlserver_functions::within_limit;
use crate::sqlserver_types::{
    Length, SqlServerType, TypeFamily, engine_type_name, explicit_conversion_refused,
    operand_clash, precedence, value_type_name,
};
use crate::types::{SqlType, Value, numeric_value};

const MAX_DATE_TEXT: usize = 64;
const MAX_MONEY_UNITS: i128 = 9_223_372_036_854_775_807;
const MAX_SMALLMONEY_UNITS: i128 = 2_147_483_647;
const MONEY_SCALE: u16 = 4;
const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SECOND;
const SMALLDATETIME_ROUNDS_UP_AT_MILLIS: i64 = 29_999;

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

const IMPLEMENTED_STYLES: [u16; 8] = [0, 23, 101, 103, 112, 120, 121, 126];

const KNOWN_DATE_STYLES: [u16; 44] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 20, 21, 22, 23, 24, 25, 100, 101, 102, 103,
    104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 120, 121, 126, 127, 130, 131, 140, 141,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DateOrder {
    MonthDayYear,
    DayMonthYear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Temporal {
    Date,
    Time,
    DateTime,
    SmallDateTime,
    DateTime2,
    DateTimeOffset,
}

impl Temporal {
    fn fraction_digits(self) -> usize {
        match self {
            Self::DateTime | Self::SmallDateTime => 3,
            _ => 7,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Date => "date",
            Self::Time => "time",
            Self::DateTime => "datetime",
            Self::SmallDateTime => "smalldatetime",
            Self::DateTime2 => "datetime2",
            Self::DateTimeOffset => "datetimeoffset",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Moment {
    days: i64,
    micros: i64,
    offset_minutes: Option<i32>,
}

fn date_conversion_failed() -> SqlError {
    SqlError::new(
        SqlState::INVALID_DATETIME_FORMAT,
        "Conversion failed when converting date and/or time from character string.",
    )
    .with_sqlserver_code(241)
}

fn out_of_range_date(source: &str, target: &str) -> SqlError {
    SqlError::new(
        SqlState::DATETIME_FIELD_OVERFLOW,
        format!(
            "The conversion of a {source} data type to a {target} data type resulted in an out-of-range value."
        ),
    )
    .with_sqlserver_code(242)
}

fn conversion_failed(value: &Value, text: &str, target: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!(
            "Conversion failed when converting the {} value '{text}' to data type {target}.",
            value_type_name(value)
        ),
    )
    .with_sqlserver_code(245)
}

fn converting_error(source: &str, target: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("Error converting data type {source} to {target}."),
    )
    .with_sqlserver_code(8114)
}

fn overflow(target: &str) -> SqlError {
    SqlError::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        format!("Arithmetic overflow error converting expression to data type {target}."),
    )
    .with_sqlserver_code(8115)
}

fn small_integer_overflow(target: &str, value: impl std::fmt::Display) -> SqlError {
    SqlError::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        format!("Arithmetic overflow error for data type {target}, value = {value}."),
    )
    .with_sqlserver_code(220)
}

fn guid_conversion_failed() -> SqlError {
    SqlError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        "Conversion failed when converting from a character string to uniqueidentifier.",
    )
    .with_sqlserver_code(8169)
}

fn invalid_style(style: u16, source: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("{style} is not a valid style number when converting from {source} to a character string."),
    )
    .with_sqlserver_code(281)
}

fn invalid_style_to_offset(style: u16, source: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("Style {style} is not supported for conversions from {source} to datetimeoffset."),
    )
    .with_sqlserver_code(9809)
}

fn style_not_supported(style: u16) -> SqlError {
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("CONVERT style {style} is not supported in this version of EggDB."),
    )
    .with_sqlserver_code(40517)
}

fn check_date_style(style: Option<u16>, source: &str) -> SqlResult<()> {
    match style {
        None => Ok(()),
        Some(style) if IMPLEMENTED_STYLES.contains(&style) => Ok(()),
        Some(style) if KNOWN_DATE_STYLES.contains(&style) => Err(style_not_supported(style)),
        Some(style) => Err(invalid_style(style, source)),
    }
}

fn character_text(value: &Value) -> Option<&str> {
    match value {
        Value::Text(text) | Value::Bpchar(text) => Some(text),
        _ => None,
    }
}

fn two_digits(text: &str) -> String {
    format!("{text:0>2}")
}

fn year_of(digits: &str) -> Option<i64> {
    let year: i64 = digits.parse().ok()?;
    match digits.len() {
        1 | 2 if year < 50 => Some(2000 + year),
        1 | 2 => Some(1900 + year),
        4 => Some(year),
        _ => None,
    }
}

fn month_named(word: &str) -> Option<i64> {
    let lower = word.to_ascii_lowercase();
    if lower.len() < 3 {
        return None;
    }
    MONTHS
        .iter()
        .position(|month| month.starts_with(&lower))
        .map(|index| index as i64 + 1)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn digits(&mut self) -> &'a str {
        let start = self.at;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.at += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.at]).unwrap_or("")
    }

    fn letters(&mut self) -> &'a str {
        let start = self.at;
        while self.peek().is_some_and(|byte| byte.is_ascii_alphabetic()) {
            self.at += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.at]).unwrap_or("")
    }

    fn spaces(&mut self) -> bool {
        let start = self.at;
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.at += 1;
        }
        self.at > start
    }

    fn take(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn at_end(&self) -> bool {
        self.at >= self.bytes.len()
    }
}

fn civil_days(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || day < 1
        || day > datetime::days_in_month(year, month)
    {
        return None;
    }
    Some(datetime::days_from_civil(year, month, day))
}

fn base_date() -> i64 {
    datetime::days_from_civil(1900, 1, 1)
}

fn read_date(cursor: &mut Cursor<'_>, order: DateOrder) -> Option<(Option<i64>, bool)> {
    match cursor.peek()? {
        byte if byte.is_ascii_digit() => {
            let start = cursor.at;
            let first = cursor.digits();
            match cursor.peek() {
                Some(separator @ (b'-' | b'/' | b'.')) => {
                    cursor.at += 1;
                    let second = cursor.digits();
                    if !cursor.take(separator) {
                        return None;
                    }
                    let third = cursor.digits();
                    if first.is_empty() || second.is_empty() || third.is_empty() {
                        return None;
                    }
                    let iso = first.len() == 4;
                    let (year, month, day) = if iso {
                        (year_of(first)?, second.parse().ok()?, third.parse().ok()?)
                    } else if order == DateOrder::DayMonthYear {
                        (year_of(third)?, second.parse().ok()?, first.parse().ok()?)
                    } else {
                        (year_of(third)?, first.parse().ok()?, second.parse().ok()?)
                    };
                    Some((
                        Some(civil_days(year, month, day)?),
                        iso && separator == b'-',
                    ))
                }
                Some(b':') => {
                    cursor.at = start;
                    Some((None, false))
                }
                _ if first.len() == 8 => {
                    let year = first[..4].parse().ok()?;
                    let month = first[4..6].parse().ok()?;
                    let day = first[6..].parse().ok()?;
                    Some((Some(civil_days(year, month, day)?), false))
                }
                _ if first.len() <= 2 => {
                    let saved = cursor.at;
                    let spaced = cursor.spaces();
                    let word = cursor.letters();
                    match month_named(word) {
                        Some(month) => {
                            cursor.take(b',');
                            cursor.spaces();
                            let year = year_of(cursor.digits())?;
                            Some((Some(civil_days(year, month, first.parse().ok()?)?), false))
                        }
                        None if word.eq_ignore_ascii_case("am")
                            || word.eq_ignore_ascii_case("pm") =>
                        {
                            cursor.at = start;
                            Some((None, false))
                        }
                        None => {
                            let _ = spaced;
                            cursor.at = saved;
                            None
                        }
                    }
                }
                _ => None,
            }
        }
        byte if byte.is_ascii_alphabetic() => {
            let month = month_named(cursor.letters())?;
            cursor.spaces();
            let day: i64 = cursor.digits().parse().ok()?;
            cursor.take(b',');
            cursor.spaces();
            let year = year_of(cursor.digits())?;
            Some((Some(civil_days(year, month, day)?), false))
        }
        _ => None,
    }
}

fn read_time(cursor: &mut Cursor<'_>, fraction_digits: usize) -> Option<i64> {
    let hour_text = cursor.digits();
    if hour_text.is_empty() || hour_text.len() > 2 {
        return None;
    }
    let mut hour: i64 = hour_text.parse().ok()?;
    let mut minute = 0i64;
    let mut second = 0i64;
    let mut fraction = 0i64;
    if cursor.take(b':') {
        let text = cursor.digits();
        if text.is_empty() || text.len() > 2 {
            return None;
        }
        minute = text.parse().ok()?;
        if cursor.take(b':') {
            let text = cursor.digits();
            if text.is_empty() || text.len() > 2 {
                return None;
            }
            second = text.parse().ok()?;
            if cursor.take(b'.') {
                let digits = cursor.digits();
                if digits.len() > fraction_digits {
                    return None;
                }
                let kept: String = digits.chars().take(6).collect();
                fraction = format!("{kept:0<6}").parse().ok()?;
            }
        }
    }
    let saved = cursor.at;
    cursor.spaces();
    let marker = cursor.letters();
    if marker.eq_ignore_ascii_case("am") || marker.eq_ignore_ascii_case("pm") {
        if !(1..=12).contains(&hour) {
            return None;
        }
        hour %= 12;
        if marker.eq_ignore_ascii_case("pm") {
            hour += 12;
        }
    } else {
        cursor.at = saved;
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some(((hour * 60 + minute) * 60 + second) * MICROS_PER_SECOND + fraction)
}

fn read_offset(cursor: &mut Cursor<'_>) -> Option<Option<i32>> {
    let saved = cursor.at;
    cursor.spaces();
    if cursor.take(b'Z') || cursor.take(b'z') {
        return Some(Some(0));
    }
    let sign = match cursor.peek() {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => {
            cursor.at = saved;
            return Some(None);
        }
    };
    cursor.at += 1;
    let hours = cursor.digits();
    if hours.len() != 2 || !cursor.take(b':') {
        return None;
    }
    let minutes = cursor.digits();
    if minutes.len() != 2 {
        return None;
    }
    let total = hours.parse::<i32>().ok()? * 60 + minutes.parse::<i32>().ok()?;
    if total > 14 * 60 || minutes.parse::<i32>().ok()? > 59 {
        return None;
    }
    Some(Some(sign * total))
}

fn parse_moment(text: &str, target: Temporal, order: DateOrder) -> Option<Moment> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_DATE_TEXT {
        return None;
    }
    let mut cursor = Cursor {
        bytes: trimmed.as_bytes(),
        at: 0,
    };
    let (date, iso) = read_date(&mut cursor, order)?;
    let separated_by_t = iso && (cursor.take(b'T') || cursor.take(b't'));
    let spaced = cursor.spaces();
    let micros = if cursor.at_end() {
        if separated_by_t {
            return None;
        }
        0
    } else if date.is_none() || separated_by_t || spaced {
        read_time(&mut cursor, target.fraction_digits())?
    } else {
        return None;
    };
    let offset_minutes = read_offset(&mut cursor)?;
    cursor.spaces();
    if !cursor.at_end() {
        return None;
    }
    Some(Moment {
        days: date.unwrap_or_else(base_date),
        micros,
        offset_minutes,
    })
}

fn rounded_to_datetime(local: i64) -> i64 {
    let millis = (local + 500).div_euclid(1000);
    let adjusted = match millis.rem_euclid(10) {
        0 | 1 => millis - millis.rem_euclid(10),
        2..=4 => millis - millis.rem_euclid(10) + 3,
        5..=8 => millis - millis.rem_euclid(10) + 7,
        _ => millis - millis.rem_euclid(10) + 10,
    };
    adjusted * 1000
}

fn rounded_to_minute(local: i64) -> i64 {
    let millis = rounded_to_datetime(local).div_euclid(1000);
    let minute_start = millis - millis.rem_euclid(60_000);
    let into_minute = millis.rem_euclid(60_000);
    let minutes = if into_minute >= SMALLDATETIME_ROUNDS_UP_AT_MILLIS {
        minute_start + 60_000
    } else {
        minute_start
    };
    minutes * 1000
}

fn civil_micros(year: i64, month: i64, day: i64) -> i64 {
    datetime::days_from_civil(year, month, day) * MICROS_PER_DAY
}

fn within_dates(local: i64, from: i64, until: i64, source: &str, target: &str) -> SqlResult<i64> {
    if local < from || local >= until {
        Err(out_of_range_date(source, target))
    } else {
        Ok(local)
    }
}

fn scaled(micros: i64, scale: u8) -> i64 {
    datetime::round_time(micros, scale.min(6))
}

fn temporal_value(
    local: i64,
    offset_minutes: i32,
    target: Temporal,
    scale: u8,
    source: &str,
) -> SqlResult<Value> {
    let first = civil_micros(1, 1, 1);
    let end = civil_micros(10000, 1, 1);
    Ok(match target {
        Temporal::Date => Value::Date(
            within_dates(local, first, end, source, "date")?.div_euclid(MICROS_PER_DAY) as i32,
        ),
        Temporal::Time => {
            let of_day = scaled(local.rem_euclid(MICROS_PER_DAY), scale);
            Value::Time(of_day % MICROS_PER_DAY)
        }
        Temporal::DateTime => Value::Timestamp(within_dates(
            rounded_to_datetime(local),
            civil_micros(1753, 1, 1),
            end,
            source,
            "datetime",
        )?),
        Temporal::SmallDateTime => Value::Timestamp(within_dates(
            rounded_to_minute(local),
            civil_micros(1900, 1, 1),
            civil_micros(2079, 6, 7),
            source,
            "smalldatetime",
        )?),
        Temporal::DateTime2 => Value::Timestamp(within_dates(
            scaled(local, scale),
            first,
            end,
            source,
            "datetime2",
        )?),
        Temporal::DateTimeOffset => {
            let utc = scaled(local, scale) - i64::from(offset_minutes) * MICROS_PER_MINUTE;
            Value::TimestampTz(within_dates(utc, first, end, source, "datetimeoffset")?)
        }
    })
}

fn temporal_target(target: SqlServerType) -> Option<(Temporal, u8)> {
    Some(match target {
        SqlServerType::Date => (Temporal::Date, 0),
        SqlServerType::Time { scale } => (Temporal::Time, scale),
        SqlServerType::DateTime => (Temporal::DateTime, 3),
        SqlServerType::SmallDateTime => (Temporal::SmallDateTime, 0),
        SqlServerType::DateTime2 { scale } => (Temporal::DateTime2, scale),
        SqlServerType::DateTimeOffset { scale } => (Temporal::DateTimeOffset, scale),
        _ => return None,
    })
}

fn temporal_of_engine(sql_type: SqlType) -> Option<(Temporal, u8)> {
    Some(match sql_type {
        SqlType::Date => (Temporal::Date, 0),
        SqlType::Time(scale) => (Temporal::Time, scale.unwrap_or(6)),
        SqlType::Timestamp(scale) => (Temporal::DateTime2, scale.unwrap_or(6)),
        SqlType::TimestampTz(scale) => (Temporal::DateTimeOffset, scale.unwrap_or(6)),
        _ => return None,
    })
}

pub(crate) fn parse_datetime(
    text: &str,
    target: Temporal,
    scale: u8,
    order: DateOrder,
) -> SqlResult<Value> {
    let moment = parse_moment(text, target, order).ok_or_else(date_conversion_failed)?;
    let local = moment.days * MICROS_PER_DAY + moment.micros;
    let offset = if target == Temporal::DateTimeOffset {
        moment.offset_minutes.unwrap_or(0)
    } else {
        0
    };
    temporal_value(local, offset, target, scale, "varchar")
}

fn order_of(style: Option<u16>) -> DateOrder {
    match style {
        Some(3 | 4 | 5 | 103 | 104 | 105) => DateOrder::DayMonthYear,
        _ => DateOrder::MonthDayYear,
    }
}

fn local_micros(value: &Value) -> SqlResult<Option<i64>> {
    Ok(match value {
        Value::Date(days) => Some(i64::from(*days) * MICROS_PER_DAY),
        Value::Timestamp(micros) => Some(*micros),
        Value::TimestampTz(micros) => Some(datetime::utc_to_local(*micros)?),
        Value::Time(micros) => Some(base_date() * MICROS_PER_DAY + micros),
        _ => None,
    })
}

fn to_temporal(value: &Value, target: Temporal, scale: u8, style: Option<u16>) -> SqlResult<Value> {
    if let Some(text) = character_text(value) {
        if let Some(style) = style.filter(|style| !KNOWN_DATE_STYLES.contains(style)) {
            return Err(invalid_style(style, value_type_name(value)));
        }
        return parse_datetime(text, target, scale, order_of(style));
    }
    let source = value_type_name(value);
    let refused = match (value, target) {
        (Value::Time(_), Temporal::Date) | (Value::Date(_), Temporal::Time) => true,
        (_, _) => !matches!(
            value,
            Value::Date(_) | Value::Time(_) | Value::Timestamp(_) | Value::TimestampTz(_)
        ),
    };
    if refused {
        return Err(explicit_conversion_refused(source, target.name()));
    }
    if target == Temporal::DateTimeOffset
        && let Some(style) = style.filter(|style| *style > 1)
    {
        return Err(invalid_style_to_offset(style, source));
    }
    let local = local_micros(value)?.unwrap_or(0);
    if target == Temporal::DateTimeOffset
        && let Value::TimestampTz(micros) = value
    {
        return Ok(Value::TimestampTz(scaled(*micros, scale)));
    }
    temporal_value(local, 0, target, scale, source)
}

fn clock_text(of_day: i64, digits: usize) -> String {
    let seconds = of_day / MICROS_PER_SECOND;
    let mut out = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    if digits > 0 {
        let fraction = format!("{:06}0", of_day % MICROS_PER_SECOND);
        out.push('.');
        out.push_str(&fraction[..digits.min(7)]);
    }
    out
}

fn trimmed_digits(of_day: i64) -> usize {
    let fraction = of_day % MICROS_PER_SECOND;
    if fraction == 0 {
        0
    } else {
        6 - format!("{fraction:06}")
            .chars()
            .rev()
            .take_while(|c| *c == '0')
            .count()
    }
}

fn date_parts(days: i64) -> (String, String, String) {
    let (year, month, day) = datetime::civil_from_days(days);
    (
        format!("{year:04}"),
        two_digits(&month.to_string()),
        two_digits(&day.to_string()),
    )
}

fn offset_text(offset_seconds: i32) -> String {
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let minutes = offset_seconds.unsigned_abs() / 60;
    format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

fn temporal_text(
    value: &Value,
    style: Option<u16>,
    scale: Option<u8>,
) -> SqlResult<Option<String>> {
    let (local, has_date, has_time, offset) = match value {
        Value::Date(days) => (i64::from(*days) * MICROS_PER_DAY, true, false, None),
        Value::Time(micros) => (*micros, false, true, None),
        Value::Timestamp(micros) => (*micros, true, true, None),
        Value::TimestampTz(micros) => (
            datetime::utc_to_local(*micros)?,
            true,
            true,
            Some(datetime::session_offset()),
        ),
        _ => return Ok(None),
    };
    check_date_style(style, value_type_name(value))?;
    let days = local.div_euclid(MICROS_PER_DAY);
    let of_day = local.rem_euclid(MICROS_PER_DAY);
    let (year, month, day) = date_parts(days);
    let declared = scale.map(|scale| usize::from(scale.min(6)));
    let fraction_digits = declared.unwrap_or_else(|| trimmed_digits(of_day));
    let date = match style {
        Some(101) => format!("{month}/{day}/{year}"),
        Some(103) => format!("{day}/{month}/{year}"),
        Some(112) => format!("{year}{month}{day}"),
        _ => format!("{year}-{month}-{day}"),
    };
    let time = match style {
        Some(120) => clock_text(of_day, 0),
        Some(121) => clock_text(of_day, declared.unwrap_or(3)),
        Some(126) if of_day % MICROS_PER_SECOND == 0 => clock_text(of_day, 0),
        Some(126) => clock_text(of_day, declared.unwrap_or(3)),
        _ => clock_text(of_day, fraction_digits),
    };
    let date_only = matches!(style, Some(23 | 101 | 103 | 112));
    let separator = if style == Some(126) { "T" } else { " " };
    let mut text = match (has_date, has_time) {
        (true, false) => date,
        (false, _) => time,
        (true, true) if date_only => date,
        (true, true) => format!("{date}{separator}{time}"),
    };
    if let Some(offset) = offset
        && !date_only
    {
        text.push(' ');
        text.push_str(&offset_text(offset));
    }
    Ok(Some(text))
}

fn hex_upper(bytes: &[u8]) -> String {
    crate::bytea::format_hex(bytes)[2..].to_ascii_uppercase()
}

pub(crate) fn text_of(value: &Value, style: Option<u16>, scale: Option<u8>) -> SqlResult<String> {
    if let Some(text) = temporal_text(value, style, scale)? {
        return Ok(text);
    }
    Ok(match value {
        Value::Boolean(flag) => if *flag { "1" } else { "0" }.to_owned(),
        Value::Uuid(bytes) => crate::bytea::format_uuid(bytes).to_ascii_uppercase(),
        Value::Bytea(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        Value::Text(text) | Value::Bpchar(text) => text.clone(),
        other => other.to_text().unwrap_or_default(),
    })
}

pub(crate) fn display_text(value: &Value, scale: Option<u8>) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Bytea(bytes) => Some(format!("0x{}", hex_upper(bytes))),
        other => text_of(other, None, scale).ok(),
    }
}

fn integer_text(value: &Value, text: &str, target: &str) -> SqlResult<i64> {
    let trimmed = text.trim();
    let digits = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(conversion_failed(value, text, target));
    }
    trimmed.parse().map_err(|_| overflow(target))
}

fn truncated_integer(number: &Numeric, target: &str) -> SqlResult<i64> {
    if number.is_nan() {
        return Err(overflow(target));
    }
    number
        .truncated(0)
        .ok()
        .and_then(|whole| whole.integral_value())
        .and_then(|whole| i64::try_from(whole).ok())
        .ok_or_else(|| overflow(target))
}

fn to_integer(value: &Value, target: &str) -> SqlResult<i64> {
    match value {
        Value::Boolean(flag) => Ok(i64::from(*flag)),
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) => {
            Ok(value.as_integer().unwrap_or(0))
        }
        Value::Numeric(number) => truncated_integer(number, target),
        Value::Real(_) | Value::Double(_) => {
            let number = value.as_float().unwrap_or(f64::NAN).trunc();
            if number.is_finite() && number >= i64::MIN as f64 && number < i64::MAX as f64 {
                Ok(number as i64)
            } else {
                Err(overflow(target))
            }
        }
        Value::Text(text) | Value::Bpchar(text) => integer_text(value, text, target),
        Value::Bytea(bytes) if bytes.len() <= 8 => Ok(bytes
            .iter()
            .fold(0i64, |sum, byte| (sum << 8) | i64::from(*byte))),
        other => Err(explicit_conversion_refused(value_type_name(other), target)),
    }
}

fn integer_of(number: i64, target: SqlServerType) -> SqlResult<Value> {
    match target {
        SqlServerType::TinyInt if !(0..=255).contains(&number) => {
            Err(small_integer_overflow("tinyint", number))
        }
        SqlServerType::TinyInt => Ok(Value::SmallInt(number as i16)),
        SqlServerType::SmallInt => i16::try_from(number)
            .map(Value::SmallInt)
            .map_err(|_| small_integer_overflow("smallint", number)),
        SqlServerType::Int => i32::try_from(number)
            .map(Value::Integer)
            .map_err(|_| overflow("int")),
        _ => Ok(Value::BigInt(number)),
    }
}

fn to_bit(value: &Value) -> SqlResult<bool> {
    match value {
        Value::Boolean(flag) => Ok(*flag),
        Value::Numeric(number) => Ok(!number.is_zero()),
        Value::Real(_) | Value::Double(_) => Ok(value.as_float().unwrap_or(0.0) != 0.0),
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) => {
            Ok(value.as_integer().unwrap_or(0) != 0)
        }
        Value::Text(text) | Value::Bpchar(text) => {
            let trimmed = text.trim();
            if trimmed.eq_ignore_ascii_case("true") {
                return Ok(true);
            }
            if trimmed.eq_ignore_ascii_case("false") {
                return Ok(false);
            }
            Numeric::parse(trimmed)
                .ok()
                .filter(|number| !number.is_nan() && !trimmed.contains(['e', 'E']))
                .map(|number| !number.is_zero())
                .ok_or_else(|| conversion_failed(value, text, "bit"))
        }
        Value::Bytea(bytes) => Ok(bytes.iter().any(|byte| *byte != 0)),
        other => Err(explicit_conversion_refused(value_type_name(other), "bit")),
    }
}

fn to_numeric(value: &Value, target: &str) -> SqlResult<Numeric> {
    match value {
        Value::Boolean(flag) => Ok(Numeric::from_i64(i64::from(*flag))),
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) => {
            Ok(Numeric::from_i64(value.as_integer().unwrap_or(0)))
        }
        Value::Numeric(number) => Ok(number.clone()),
        Value::Real(number) => Numeric::from_f64(f64::from(*number), 7),
        Value::Double(number) => Numeric::from_f64(*number, 17),
        Value::Text(text) | Value::Bpchar(text) => {
            let trimmed = text.trim();
            if trimmed.contains(['e', 'E']) || trimmed.is_empty() {
                return Err(converting_error(value_type_name(value), target));
            }
            Numeric::parse(trimmed)
                .ok()
                .filter(|number| !number.is_nan())
                .ok_or_else(|| converting_error(value_type_name(value), target))
        }
        other => Err(explicit_conversion_refused(value_type_name(other), target)),
    }
}

fn decimal_of(number: Numeric, target: SqlServerType) -> SqlResult<Value> {
    let name = target.name();
    let modifier = match target.engine_type() {
        SqlType::Numeric(modifier) => modifier,
        _ => None,
    };
    let value = numeric_value(number, modifier).map_err(|_| overflow(name))?;
    let limit = match target {
        SqlServerType::Money => MAX_MONEY_UNITS,
        SqlServerType::SmallMoney => MAX_SMALLMONEY_UNITS,
        _ => return Ok(value),
    };
    let Value::Numeric(number) = &value else {
        return Ok(value);
    };
    let units = number
        .with_modifier(38, i64::from(MONEY_SCALE))
        .ok()
        .and_then(|scaled| scaled.to_string().replace('.', "").parse::<i128>().ok());
    match units {
        Some(units) if units.abs() <= limit => Ok(value),
        _ => Err(overflow(name)),
    }
}

fn to_float(value: &Value, target: &str) -> SqlResult<f64> {
    match value {
        Value::Boolean(flag) => Ok(f64::from(u8::from(*flag))),
        Value::SmallInt(_)
        | Value::Integer(_)
        | Value::BigInt(_)
        | Value::Real(_)
        | Value::Double(_) => Ok(value.as_float().unwrap_or(0.0)),
        Value::Numeric(number) => Ok(number.to_f64()),
        Value::Text(text) | Value::Bpchar(text) => {
            let trimmed = text.trim();
            let looks_numeric = !trimmed.is_empty()
                && trimmed
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || b".eE+-".contains(&byte));
            looks_numeric
                .then(|| trimmed.parse::<f64>().ok())
                .flatten()
                .filter(|number| number.is_finite())
                .ok_or_else(|| converting_error(value_type_name(value), target))
        }
        other => Err(explicit_conversion_refused(value_type_name(other), target)),
    }
}

fn float_of(number: f64, target: SqlServerType) -> SqlResult<Value> {
    if target == SqlServerType::Real {
        let single = number as f32;
        if single.is_infinite() {
            return Err(overflow("real"));
        }
        Ok(Value::Real(single))
    } else {
        Ok(Value::Double(number))
    }
}

fn guid_order(bytes: &[u8; 16]) -> [u8; 16] {
    let mut out = *bytes;
    out[..4].reverse();
    out[4..6].reverse();
    out[6..8].reverse();
    out
}

fn to_guid(value: &Value) -> SqlResult<[u8; 16]> {
    match value {
        Value::Uuid(bytes) => Ok(*bytes),
        Value::Text(text) | Value::Bpchar(text) => {
            let trimmed = text.trim();
            let candidate = match trimmed.strip_prefix('{') {
                Some(rest) => rest.strip_suffix('}').ok_or_else(guid_conversion_failed)?,
                None => trimmed.get(..36.min(trimmed.len())).unwrap_or(trimmed),
            };
            let well_formed = candidate.len() == 36
                && candidate.bytes().enumerate().all(|(index, byte)| {
                    if matches!(index, 8 | 13 | 18 | 23) {
                        byte == b'-'
                    } else {
                        byte.is_ascii_hexdigit()
                    }
                });
            if !well_formed {
                return Err(guid_conversion_failed());
            }
            crate::bytea::parse_uuid(candidate).map_err(|_| guid_conversion_failed())
        }
        Value::Bytea(bytes) => {
            let mut raw = [0u8; 16];
            let length = bytes.len().min(16);
            raw[..length].copy_from_slice(&bytes[..length]);
            Ok(guid_order(&raw))
        }
        other => Err(explicit_conversion_refused(
            value_type_name(other),
            "uniqueidentifier",
        )),
    }
}

fn to_bytes(value: &Value) -> SqlResult<Vec<u8>> {
    Ok(match value {
        Value::Bytea(bytes) => bytes.clone(),
        Value::Text(text) | Value::Bpchar(text) => text.as_bytes().to_vec(),
        Value::Boolean(flag) => vec![u8::from(*flag)],
        Value::SmallInt(number) => number.to_be_bytes().to_vec(),
        Value::Integer(number) => number.to_be_bytes().to_vec(),
        Value::BigInt(number) => number.to_be_bytes().to_vec(),
        Value::Uuid(bytes) => guid_order(bytes).to_vec(),
        other => {
            return Err(explicit_conversion_refused(
                value_type_name(other),
                "varbinary",
            ));
        }
    })
}

fn sized_text(text: String, length: Length, padded: bool) -> SqlResult<Value> {
    let Length::Given(length) = length else {
        return Ok(Value::Text(text));
    };
    let length = length as usize;
    let mut text = match text.char_indices().nth(length) {
        Some((cut, _)) => {
            let mut text = text;
            text.truncate(cut);
            text
        }
        None => text,
    };
    if padded {
        let count = text.chars().count();
        if count < length {
            within_limit(text.len().saturating_add(length - count))?;
            text.extend(std::iter::repeat_n(' ', length - count));
        }
        return Ok(Value::Bpchar(text));
    }
    Ok(Value::Text(text))
}

fn sized_bytes(mut bytes: Vec<u8>, length: Length, padded: bool) -> SqlResult<Value> {
    if let Length::Given(length) = length {
        let length = length as usize;
        bytes.truncate(length);
        if padded && bytes.len() < length {
            within_limit(length)?;
            bytes.resize(length, 0);
        }
    }
    Ok(Value::Bytea(bytes))
}

pub(crate) fn convert(
    value: Value,
    target: SqlServerType,
    style: Option<u16>,
    scale: Option<u8>,
) -> SqlResult<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let source = TypeFamily::of(&value);
    if !source.allows(target.kind()) {
        return Err(explicit_conversion_refused(
            value_type_name(&value),
            target.name(),
        ));
    }
    if let Some((temporal, target_scale)) = temporal_target(target) {
        return to_temporal(&value, temporal, target_scale, style);
    }
    match target {
        SqlServerType::TinyInt
        | SqlServerType::SmallInt
        | SqlServerType::Int
        | SqlServerType::BigInt => integer_of(to_integer(&value, target.name())?, target),
        SqlServerType::Bit => to_bit(&value).map(Value::Boolean),
        SqlServerType::Decimal { .. } | SqlServerType::Money | SqlServerType::SmallMoney => {
            decimal_of(to_numeric(&value, target.name())?, target)
        }
        SqlServerType::Float | SqlServerType::Real => {
            float_of(to_float(&value, target.name())?, target)
        }
        SqlServerType::Char(length) | SqlServerType::NChar(length) => {
            let text = text_of(&value, style, scale)?;
            within_limit(text.len())?;
            sized_text(text, length, true)
        }
        SqlServerType::VarChar(length) | SqlServerType::NVarChar(length) => {
            let text = text_of(&value, style, scale)?;
            within_limit(text.len())?;
            sized_text(text, length, false)
        }
        SqlServerType::Text | SqlServerType::NText => {
            let text = text_of(&value, style, scale)?;
            within_limit(text.len())?;
            Ok(Value::Text(text))
        }
        SqlServerType::Binary(length) => sized_bytes(to_bytes(&value)?, length, true),
        SqlServerType::VarBinary(length) => sized_bytes(to_bytes(&value)?, length, false),
        SqlServerType::Image => to_bytes(&value).map(Value::Bytea),
        SqlServerType::UniqueIdentifier => to_guid(&value).map(Value::Uuid),
        _ => Ok(value),
    }
}

pub(crate) fn implicit(value: Value, target: SqlType) -> SqlResult<Value> {
    if value.is_null() {
        return Ok(value);
    }
    let source = TypeFamily::of(&value);
    let wanted = TypeFamily::of_engine(target);
    if !source.allows(wanted) {
        return Err(operand_clash(
            value_type_name(&value),
            engine_type_name(target),
        ));
    }
    let name = engine_type_name(target);
    match target {
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt => match value {
            Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) => Ok(value),
            other => to_integer(&other, name).map(Value::BigInt),
        },
        SqlType::Boolean => to_bit(&value).map(Value::Boolean),
        SqlType::Numeric(_) => match value {
            Value::Numeric(_) => Ok(value),
            other => to_numeric(&other, name).map(Value::Numeric),
        },
        SqlType::Real | SqlType::Double => match value {
            Value::Real(_) | Value::Double(_) => Ok(value),
            other => to_float(&other, name).map(Value::Double),
        },
        SqlType::Date | SqlType::Time(_) | SqlType::Timestamp(_) | SqlType::TimestampTz(_) => {
            let (temporal, scale) = temporal_of_engine(target).unwrap_or((Temporal::DateTime2, 6));
            if TypeFamily::of(&value) == TypeFamily::of_engine(target)
                && !matches!(
                    (&value, target),
                    (Value::Timestamp(_), SqlType::TimestampTz(_))
                        | (Value::TimestampTz(_), SqlType::Timestamp(_))
                )
            {
                return Ok(value);
            }
            to_temporal(&value, temporal, scale, None)
        }
        SqlType::Text | SqlType::Varchar(_) | SqlType::Bpchar(_) => match value {
            Value::Text(_) | Value::Bpchar(_) => Ok(value),
            other => {
                let text = text_of(&other, None, None)?;
                within_limit(text.len())?;
                Ok(Value::Text(text))
            }
        },
        SqlType::Bytea => to_bytes(&value).map(Value::Bytea),
        SqlType::Uuid => to_guid(&value).map(Value::Uuid),
        _ => Ok(value),
    }
}

pub(crate) fn assign(value: Value, column: &Column) -> SqlResult<Value> {
    let converted = implicit(value, column.sql_type)?;
    crate::execute::assign_to(converted, column)
}

pub(crate) fn by_precedence(left: &Value, right: &Value) -> Option<SqlResult<(Value, Value)>> {
    let (left_rank, right_rank) = (precedence(left)?, precedence(right)?);
    if left_rank == right_rank {
        return None;
    }
    Some(if left_rank < right_rank {
        let target = right.sql_type()?.without_modifier();
        implicit(left.clone(), target).map(|left| (left, right.clone()))
    } else {
        let target = left.sql_type()?.without_modifier();
        implicit(right.clone(), target).map(|right| (left.clone(), right))
    })
}

pub(crate) fn literal_of(text: &str, target: SqlType) -> Option<SqlResult<Value>> {
    let value = Value::Text(text.to_owned());
    match target {
        SqlType::Date | SqlType::Time(_) | SqlType::Timestamp(_) | SqlType::TimestampTz(_) => {
            let (temporal, scale) = temporal_of_engine(target)?;
            Some(parse_datetime(
                text,
                temporal,
                scale,
                DateOrder::MonthDayYear,
            ))
        }
        SqlType::Boolean => Some(to_bit(&value).map(Value::Boolean)),
        SqlType::Uuid => Some(to_guid(&value).map(Value::Uuid)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{DateOrder, Temporal, convert, implicit, parse_datetime, text_of};
    use crate::datetime::{self, MICROS_PER_DAY, MICROS_PER_SECOND};
    use crate::numeric::Numeric;
    use crate::sqlserver_types::{Length, SqlServerType};
    use crate::types::{SqlType, Value};

    fn timestamp(text: &str, target: Temporal) -> String {
        match parse_datetime(text, target, 7, DateOrder::MonthDayYear) {
            Ok(value) => text_of(&value, Some(121), Some(6)).unwrap_or_default(),
            Err(error) => format!("error {}", error.vendor_code().unwrap_or(0)),
        }
    }

    fn number(text: &str) -> Value {
        Value::Numeric(Numeric::parse(text).unwrap_or_else(|_| Numeric::nan()))
    }

    #[test]
    fn date_text_is_read_in_iso_unseparated_numeric_and_named_forms() {
        let cases = [
            ("2020-07-04", "2020-07-04 00:00:00.000000"),
            ("20200704", "2020-07-04 00:00:00.000000"),
            ("07/04/2020", "2020-07-04 00:00:00.000000"),
            ("7/4/20", "2020-07-04 00:00:00.000000"),
            ("01/01/60", "1960-01-01 00:00:00.000000"),
            ("01/01/49", "2049-01-01 00:00:00.000000"),
            ("Jul 4 2020 10:30PM", "2020-07-04 22:30:00.000000"),
            ("4 July 2020", "2020-07-04 00:00:00.000000"),
            ("2020-07-04T10:00:00.1234567", "2020-07-04 10:00:00.123456"),
            ("2005-12-31 23:59:59.9999999", "2005-12-31 23:59:59.999999"),
            ("10:00", "1900-01-01 10:00:00.000000"),
            ("2020-02-30", "error 241"),
            ("2020-07-04T", "error 241"),
            ("junk", "error 241"),
        ];
        for (text, expected) in cases {
            assert_eq!(timestamp(text, Temporal::DateTime2), expected, "{text}");
        }
        assert_eq!(
            timestamp("2020-01-01 10:00:00.1234", Temporal::DateTime),
            "error 241"
        );
        let long = "1".repeat(65);
        assert_eq!(timestamp(&long, Temporal::DateTime2), "error 241");
    }

    #[test]
    fn datetime_rounds_to_its_three_hundredths_and_smalldatetime_to_the_minute() {
        let cases = [
            ("2020-01-01 23:59:59.999", "2020-01-02 00:00:00.000"),
            ("2020-01-01 10:00:00.990", "2020-01-01 10:00:00.990"),
            ("2020-01-01 10:00:00.991", "2020-01-01 10:00:00.990"),
            ("2020-01-01 10:00:00.992", "2020-01-01 10:00:00.993"),
            ("2020-01-01 10:00:00.994", "2020-01-01 10:00:00.993"),
            ("2020-01-01 10:00:00.995", "2020-01-01 10:00:00.997"),
            ("2020-01-01 10:00:00.998", "2020-01-01 10:00:00.997"),
        ];
        for (text, expected) in cases {
            let value = parse_datetime(text, Temporal::DateTime, 3, DateOrder::MonthDayYear);
            let shown = value
                .and_then(|value| text_of(&value, Some(121), Some(3)))
                .unwrap_or_default();
            assert_eq!(shown, expected, "{text}");
        }
        let small = |text| {
            parse_datetime(text, Temporal::SmallDateTime, 0, DateOrder::MonthDayYear)
                .and_then(|value| text_of(&value, Some(120), None))
                .unwrap_or_default()
        };
        assert_eq!(small("2000-05-08 12:35:29.998"), "2000-05-08 12:35:00");
        assert_eq!(small("2000-05-08 12:35:29.999"), "2000-05-08 12:36:00");
        let early = parse_datetime("1752-12-31", Temporal::DateTime, 3, DateOrder::MonthDayYear);
        assert_eq!(early.err().and_then(|error| error.vendor_code()), Some(242));
    }

    #[test]
    fn numbers_truncate_into_integers_and_round_into_money() {
        let int = convert(number("10.6496"), SqlServerType::Int, None, None);
        assert_eq!(int.ok(), Some(Value::Integer(10)));
        let negative = convert(number("-10.6496"), SqlServerType::Int, None, None);
        assert_eq!(negative.ok(), Some(Value::Integer(-10)));
        let money = convert(number("10.3496847"), SqlServerType::Money, None, None);
        assert_eq!(money.ok(), Some(number("10.3497")));
        let tiny = convert(Value::Integer(256), SqlServerType::TinyInt, None, None);
        assert_eq!(tiny.err().and_then(|error| error.vendor_code()), Some(220));
        let small_money = convert(number("214748.3648"), SqlServerType::SmallMoney, None, None);
        assert_eq!(
            small_money.err().and_then(|error| error.vendor_code()),
            Some(8115)
        );
        let text = convert(Value::Text("1.5".into()), SqlServerType::Int, None, None);
        assert_eq!(text.err().and_then(|error| error.vendor_code()), Some(245));
        let exponent = convert(
            Value::Text("1e5".into()),
            SqlServerType::Decimal {
                precision: 18,
                scale: 2,
            },
            None,
            None,
        );
        assert_eq!(
            exponent.err().and_then(|error| error.vendor_code()),
            Some(8114)
        );
    }

    #[test]
    fn bits_guids_text_and_binary_follow_the_conversion_rules() {
        let bit = |text: &str| convert(Value::Text(text.into()), SqlServerType::Bit, None, None);
        assert_eq!(bit("TRUE").ok(), Some(Value::Boolean(true)));
        assert_eq!(bit("false").ok(), Some(Value::Boolean(false)));
        assert_eq!(bit("2").ok(), Some(Value::Boolean(true)));
        assert_eq!(
            bit("yes").err().and_then(|error| error.vendor_code()),
            Some(245)
        );
        let guid = convert(
            Value::Text("6F9619FF-8B86-D011-B42D-00C04FC964FFextra".into()),
            SqlServerType::UniqueIdentifier,
            None,
            None,
        )
        .and_then(|value| text_of(&value, None, None));
        assert_eq!(
            guid.ok().as_deref(),
            Some("6F9619FF-8B86-D011-B42D-00C04FC964FF")
        );
        let padded = convert(
            Value::Text("ab".into()),
            SqlServerType::Char(Length::Given(4)),
            None,
            None,
        );
        assert_eq!(padded.ok(), Some(Value::Bpchar("ab  ".into())));
        let cut = convert(
            Value::Text("abcdef".into()),
            SqlServerType::VarChar(Length::Given(3)),
            None,
            None,
        );
        assert_eq!(cut.ok(), Some(Value::Text("abc".into())));
        let bytes = convert(
            Value::Bytea(vec![1]),
            SqlServerType::Binary(Length::Given(3)),
            None,
            None,
        );
        assert_eq!(bytes.ok(), Some(Value::Bytea(vec![1, 0, 0])));
        let refused = convert(Value::Date(0), SqlServerType::Int, None, None);
        assert_eq!(
            refused.err().and_then(|error| error.vendor_code()),
            Some(529)
        );
    }

    #[test]
    fn styles_write_dates_as_the_style_table_shows() {
        let day = datetime::days_from_civil(2020, 7, 4);
        let at = day * MICROS_PER_DAY + (13 * 3600 + 5 * 60 + 9) * MICROS_PER_SECOND;
        let stamp = Value::Timestamp(at);
        let styled = |style| text_of(&stamp, Some(style), Some(3)).unwrap_or_default();
        assert_eq!(styled(23), "2020-07-04");
        assert_eq!(styled(101), "07/04/2020");
        assert_eq!(styled(103), "04/07/2020");
        assert_eq!(styled(112), "20200704");
        assert_eq!(styled(120), "2020-07-04 13:05:09");
        assert_eq!(styled(121), "2020-07-04 13:05:09.000");
        assert_eq!(styled(126), "2020-07-04T13:05:09");
        let fraction = Value::Timestamp(at + 120_000);
        assert_eq!(
            text_of(&fraction, Some(126), Some(3)).unwrap_or_default(),
            "2020-07-04T13:05:09.120"
        );
        let error = text_of(&stamp, Some(999), None).err();
        assert_eq!(error.and_then(|error| error.vendor_code()), Some(281));
    }

    #[test]
    fn implicit_conversion_truncates_fractions_into_integer_columns_and_refuses_clashes() {
        assert_eq!(
            implicit(number("10.6"), SqlType::Integer).ok(),
            Some(Value::BigInt(10))
        );
        assert_eq!(
            implicit(Value::Integer(1), SqlType::Boolean).ok(),
            Some(Value::Boolean(true))
        );
        let clash = implicit(Value::Date(0), SqlType::Integer);
        assert_eq!(clash.err().and_then(|error| error.vendor_code()), Some(206));
    }
}
