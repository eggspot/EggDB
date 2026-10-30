use crate::datetime::{self, MICROS_PER_DAY, MICROS_PER_SECOND};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::interval::Interval;
use crate::numeric::Numeric;
use crate::oracle_datetime::{DAYS, MONTHS};
use crate::types::Value;

const JULIAN_DAY_OF_EPOCH: i64 = 2_451_545;
const UNIX_EPOCH_SECONDS: f64 = 946_684_800.0;
const NAME_WIDTH: usize = 9;
const ROMAN_WIDTH: usize = 15;
const MAX_TEMPLATE_CHARS: usize = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Spelling {
    Upper,
    Capitalized,
    Lower,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Hour12,
    Hour24,
    Minute,
    Second,
    Millisecond,
    Microsecond,
    Fraction(u8),
    SecondsOfDay,
    Meridian { dotted: bool, lower: bool },
    YearWithComma,
    Year(u8),
    IsoYear(u8),
    Era { dotted: bool, lower: bool },
    MonthName(Spelling),
    MonthAbbreviation(Spelling),
    MonthNumber,
    DayName(Spelling),
    DayAbbreviation(Spelling),
    DayOfYear,
    IsoDayOfYear,
    DayOfMonth,
    DayOfWeek,
    IsoDayOfWeek,
    WeekOfMonth,
    WeekOfYear,
    IsoWeek,
    Century,
    JulianDay,
    Quarter,
    RomanMonth { lower: bool },
    ZoneHours,
    ZoneMinutes,
    ZoneName { lower: bool },
    Offset,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Piece {
    Text(String),
    Field {
        field: Field,
        fill: bool,
        ordinal: Option<bool>,
    },
}

const PATTERNS: [&str; 52] = [
    "A.D.", "A.M.", "B.C.", "P.M.", "Y,YYY", "SSSSS", "MONTH", "HH24", "HH12", "IYYY", "IDDD",
    "SSSS", "YYYY", "FF1", "FF2", "FF3", "FF4", "FF5", "FF6", "IYY", "YYY", "DDD", "DAY", "MON",
    "TZH", "TZM", "AD", "AM", "BC", "PM", "HH", "MI", "MS", "US", "SS", "YY", "IY", "MM", "DY",
    "DD", "ID", "IW", "WW", "CC", "RM", "TZ", "OF", "Y", "I", "D", "J", "Q",
];

fn spelling_of(text: &[char]) -> Spelling {
    let mut letters = text.iter().filter(|character| character.is_ascii_alphabetic());
    match (letters.next(), letters.next()) {
        (Some(first), _) if first.is_ascii_lowercase() => Spelling::Lower,
        (Some(_), Some(second)) if second.is_ascii_lowercase() => Spelling::Capitalized,
        _ => Spelling::Upper,
    }
}

fn field_named(pattern: &str, written: &[char]) -> Field {
    let spelling = spelling_of(written);
    let lower = spelling == Spelling::Lower;
    match pattern {
        "A.D." | "B.C." => Field::Era { dotted: true, lower },
        "AD" | "BC" => Field::Era { dotted: false, lower },
        "A.M." | "P.M." => Field::Meridian { dotted: true, lower },
        "AM" | "PM" => Field::Meridian { dotted: false, lower },
        "Y,YYY" => Field::YearWithComma,
        "SSSSS" | "SSSS" => Field::SecondsOfDay,
        "MONTH" => Field::MonthName(spelling),
        "MON" => Field::MonthAbbreviation(spelling),
        "DAY" => Field::DayName(spelling),
        "DY" => Field::DayAbbreviation(spelling),
        "HH24" => Field::Hour24,
        "HH12" | "HH" => Field::Hour12,
        "IYYY" => Field::IsoYear(4),
        "IYY" => Field::IsoYear(3),
        "IY" => Field::IsoYear(2),
        "I" => Field::IsoYear(1),
        "IDDD" => Field::IsoDayOfYear,
        "YYYY" => Field::Year(4),
        "YYY" => Field::Year(3),
        "YY" => Field::Year(2),
        "Y" => Field::Year(1),
        "DDD" => Field::DayOfYear,
        "DD" => Field::DayOfMonth,
        "D" => Field::DayOfWeek,
        "ID" => Field::IsoDayOfWeek,
        "TZH" => Field::ZoneHours,
        "TZM" => Field::ZoneMinutes,
        "TZ" => Field::ZoneName { lower },
        "OF" => Field::Offset,
        "MI" => Field::Minute,
        "MS" => Field::Millisecond,
        "US" => Field::Microsecond,
        "SS" => Field::Second,
        "MM" => Field::MonthNumber,
        "IW" => Field::IsoWeek,
        "WW" => Field::WeekOfYear,
        "W" => Field::WeekOfMonth,
        "CC" => Field::Century,
        "RM" => Field::RomanMonth { lower },
        "J" => Field::JulianDay,
        "Q" => Field::Quarter,
        other => {
            let digits = other.strip_prefix("FF").and_then(|digit| digit.parse::<u8>().ok());
            Field::Fraction(digits.unwrap_or(6))
        }
    }
}

fn starts_with_upper(upper: &[char], word: &str) -> bool {
    word.chars().count() <= upper.len() && word.chars().zip(upper).all(|(a, b)| a == *b)
}

fn is_numeric_field(field: Field) -> bool {
    !matches!(
        field,
        Field::Meridian { .. }
            | Field::Era { .. }
            | Field::MonthName(_)
            | Field::MonthAbbreviation(_)
            | Field::DayName(_)
            | Field::DayAbbreviation(_)
            | Field::RomanMonth { .. }
            | Field::ZoneName { .. }
            | Field::Offset
    )
}

fn push_text(pieces: &mut Vec<Piece>, character: char) {
    if let Some(Piece::Text(text)) = pieces.last_mut() {
        text.push(character);
    } else {
        pieces.push(Piece::Text(character.to_string()));
    }
}

fn too_long_template() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        "the format template is too long",
    )
}

fn datetime_template(template: &str) -> SqlResult<Vec<Piece>> {
    let chars: Vec<char> = template.chars().collect();
    if chars.len() > MAX_TEMPLATE_CHARS {
        return Err(too_long_template());
    }
    let upper: Vec<char> = chars.iter().map(char::to_ascii_uppercase).collect();
    let mut pieces = Vec::new();
    let mut at = 0;
    let mut fill = false;
    while at < chars.len() {
        let character = chars[at];
        if character == '"' {
            at += 1;
            while at < chars.len() && chars[at] != '"' {
                if chars[at] == '\\' && chars.get(at + 1) == Some(&'"') {
                    at += 1;
                }
                push_text(&mut pieces, chars[at]);
                at += 1;
            }
            at += 1;
            continue;
        }
        if character == '\\' && chars.get(at + 1) == Some(&'"') {
            push_text(&mut pieces, '"');
            at += 2;
            continue;
        }
        let rest = &upper[at..];
        if starts_with_upper(rest, "FM") {
            fill = true;
            at += 2;
            continue;
        }
        if starts_with_upper(rest, "FX") || starts_with_upper(rest, "TM") {
            at += 2;
            continue;
        }
        let Some(pattern) = PATTERNS
            .iter()
            .find(|pattern| starts_with_upper(rest, pattern))
        else {
            push_text(&mut pieces, character);
            fill = false;
            at += 1;
            continue;
        };
        let length = pattern.chars().count();
        let field = field_named(pattern, &chars[at..at + length]);
        at += length;
        let ordinal = if is_numeric_field(field) && starts_with_upper(&upper[at..], "TH") {
            let upper_case = chars[at] == 'T';
            at += 2;
            Some(upper_case)
        } else {
            None
        };
        pieces.push(Piece::Field {
            field,
            fill,
            ordinal,
        });
        fill = false;
    }
    Ok(pieces)
}

struct Moment {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    micros: i64,
    days: Option<i64>,
    offset: Option<i32>,
    abbreviation: Option<String>,
}

impl Moment {
    fn of_local(local: i64, offset: Option<i32>) -> Self {
        let days = local.div_euclid(MICROS_PER_DAY);
        let of_day = local.rem_euclid(MICROS_PER_DAY);
        let (year, month, day) = datetime::civil_from_days(days);
        Self {
            year,
            month,
            day,
            hour: of_day / (3600 * MICROS_PER_SECOND),
            minute: of_day / (60 * MICROS_PER_SECOND) % 60,
            second: of_day / MICROS_PER_SECOND % 60,
            micros: of_day % MICROS_PER_SECOND,
            days: Some(days),
            offset,
            abbreviation: None,
        }
    }

    fn of_interval(interval: Interval) -> Self {
        let micros = interval.micros;
        Self {
            year: i64::from(interval.months / 12),
            month: i64::from(interval.months % 12),
            day: i64::from(interval.days),
            hour: micros / (3600 * MICROS_PER_SECOND),
            minute: micros / (60 * MICROS_PER_SECOND) % 60,
            second: micros / MICROS_PER_SECOND % 60,
            micros: micros % MICROS_PER_SECOND,
            days: None,
            offset: None,
            abbreviation: None,
        }
    }

    fn shown_year(&self) -> (i64, bool) {
        if self.days.is_some() && self.year <= 0 {
            (1 - self.year, true)
        } else {
            (self.year, false)
        }
    }

    fn day_of_year(&self) -> i64 {
        match self.days {
            Some(days) => days - datetime::days_from_civil(self.year, 1, 1) + 1,
            None => self.day,
        }
    }

    fn weekday(&self) -> usize {
        self.days.map_or(0, |days| (days + 6).rem_euclid(7) as usize)
    }

    fn iso(&self) -> (i64, i64) {
        self.days.map_or((self.year, 1), datetime::iso_week)
    }
}

fn spelled(name: &str, spelling: Spelling) -> String {
    match spelling {
        Spelling::Upper => name.to_ascii_uppercase(),
        Spelling::Lower => name.to_ascii_lowercase(),
        Spelling::Capitalized => {
            let lower = name.to_ascii_lowercase();
            let mut characters = lower.chars();
            match characters.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + characters.as_str(),
                None => String::new(),
            }
        }
    }
}

fn ordinal_suffix(number: i64, upper: bool) -> &'static str {
    let last_two = number.unsigned_abs() % 100;
    let suffix = if (11..=13).contains(&last_two) {
        "th"
    } else {
        match last_two % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        }
    };
    match (upper, suffix) {
        (false, suffix) => suffix,
        (true, "st") => "ST",
        (true, "nd") => "ND",
        (true, "rd") => "RD",
        (true, _) => "TH",
    }
}

fn roman(number: i64) -> Option<String> {
    if !(1..=3999).contains(&number) {
        return None;
    }
    const NUMERALS: [(i64, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut left = number;
    let mut out = String::new();
    for (value, numeral) in NUMERALS {
        while left >= value {
            out.push_str(numeral);
            left -= value;
        }
    }
    Some(out)
}

fn number_text(value: i64, width: usize, fill: bool) -> String {
    if fill {
        value.to_string()
    } else if value < 0 {
        format!("-{:0width$}", value.unsigned_abs(), width = width.saturating_sub(1))
    } else {
        format!("{value:0width$}")
    }
}

fn padded_name(name: String, fill: bool) -> String {
    let width = name.chars().count();
    if fill || width >= NAME_WIDTH {
        name
    } else {
        name + &" ".repeat(NAME_WIDTH - width)
    }
}

fn zone_text(moment: &Moment, lower: bool) -> String {
    let Some(offset) = moment.offset else {
        return String::new();
    };
    let text = if let Some(abbreviation) = &moment.abbreviation {
        abbreviation.clone()
    } else if offset == 0 {
        "UTC".to_owned()
    } else {
        datetime::format_offset(offset)
    };
    if lower { text.to_ascii_lowercase() } else { text }
}

fn field_text(moment: &Moment, field: Field, fill: bool) -> (String, Option<i64>) {
    let number = |value: i64, width: usize| (number_text(value, width, fill), Some(value));
    let (year, before_christ) = moment.shown_year();
    match field {
        Field::Hour12 => {
            let hour = moment.hour % 12;
            number(if hour == 0 { 12 } else { hour }, 2)
        }
        Field::Hour24 => number(moment.hour, 2),
        Field::Minute => number(moment.minute, 2),
        Field::Second => number(moment.second, 2),
        Field::Millisecond => number(moment.micros / 1000, 3),
        Field::Microsecond => number(moment.micros, 6),
        Field::Fraction(digits) => {
            let six = format!("{:06}", moment.micros);
            let shown = six.get(..usize::from(digits).min(6)).unwrap_or("").to_owned();
            (shown, None)
        }
        Field::SecondsOfDay => {
            let seconds = moment.hour * 3600 + moment.minute * 60 + moment.second;
            (seconds.to_string(), Some(seconds))
        }
        Field::Meridian { dotted, lower } => {
            let text = match (moment.hour % 24 < 12, dotted) {
                (true, false) => "AM",
                (false, false) => "PM",
                (true, true) => "A.M.",
                (false, true) => "P.M.",
            };
            let text = if lower {
                text.to_ascii_lowercase()
            } else {
                text.to_owned()
            };
            (text, None)
        }
        Field::YearWithComma => (
            format!("{},{:03}", year / 1000, year % 1000),
            Some(year),
        ),
        Field::Year(width) => {
            let width = usize::from(width);
            let shown = if width >= 4 {
                year
            } else {
                year % 10i64.pow(width as u32)
            };
            number(shown, width)
        }
        Field::IsoYear(width) => {
            let width = usize::from(width);
            let (iso_year, _) = moment.iso();
            let shown = if width >= 4 {
                iso_year
            } else {
                iso_year % 10i64.pow(width as u32)
            };
            number(shown, width)
        }
        Field::Era { dotted, lower } => {
            let text = match (before_christ, dotted) {
                (false, false) => "AD",
                (true, false) => "BC",
                (false, true) => "A.D.",
                (true, true) => "B.C.",
            };
            let text = if lower {
                text.to_ascii_lowercase()
            } else {
                text.to_owned()
            };
            (text, None)
        }
        Field::MonthName(spelling) => {
            let name = MONTHS
                .get(usize::try_from(moment.month - 1).unwrap_or(0))
                .copied()
                .unwrap_or("");
            (padded_name(spelled(name, spelling), fill), None)
        }
        Field::MonthAbbreviation(spelling) => {
            let name = MONTHS
                .get(usize::try_from(moment.month - 1).unwrap_or(0))
                .copied()
                .unwrap_or("");
            (spelled(name.get(..3).unwrap_or(name), spelling), None)
        }
        Field::MonthNumber => number(moment.month, 2),
        Field::DayName(spelling) => {
            let name = DAYS.get(moment.weekday()).copied().unwrap_or("");
            (padded_name(spelled(name, spelling), fill), None)
        }
        Field::DayAbbreviation(spelling) => {
            let name = DAYS.get(moment.weekday()).copied().unwrap_or("");
            (spelled(name.get(..3).unwrap_or(name), spelling), None)
        }
        Field::DayOfYear => number(moment.day_of_year(), 3),
        Field::IsoDayOfYear => {
            let (_, week) = moment.iso();
            let weekday = moment.weekday() as i64;
            let iso_weekday = if weekday == 0 { 7 } else { weekday };
            number((week - 1) * 7 + iso_weekday, 3)
        }
        Field::DayOfMonth => number(moment.day, 2),
        Field::DayOfWeek => number(moment.weekday() as i64 + 1, 1),
        Field::IsoDayOfWeek => {
            let weekday = moment.weekday() as i64;
            number(if weekday == 0 { 7 } else { weekday }, 1)
        }
        Field::WeekOfMonth => number((moment.day - 1) / 7 + 1, 1),
        Field::WeekOfYear => number((moment.day_of_year() - 1) / 7 + 1, 2),
        Field::IsoWeek => number(moment.iso().1, 2),
        Field::Century => {
            let century = (year + 99) / 100;
            number(if before_christ { -century } else { century }, 2)
        }
        Field::JulianDay => {
            let julian = moment.days.unwrap_or(0) + JULIAN_DAY_OF_EPOCH;
            (julian.to_string(), Some(julian))
        }
        Field::Quarter => number((moment.month - 1) / 3 + 1, 1),
        Field::RomanMonth { lower } => {
            let numeral = roman(moment.month).unwrap_or_default();
            let numeral = if lower {
                numeral.to_ascii_lowercase()
            } else {
                numeral
            };
            let shown = if fill {
                numeral
            } else {
                format!("{numeral:<4}")
            };
            (shown, None)
        }
        Field::ZoneHours => {
            let offset = moment.offset.unwrap_or(0);
            let sign = if offset < 0 { '-' } else { '+' };
            (format!("{sign}{:02}", offset.unsigned_abs() / 3600), None)
        }
        Field::ZoneMinutes => {
            let offset = moment.offset.unwrap_or(0);
            number(i64::from(offset.unsigned_abs() / 60 % 60), 2)
        }
        Field::ZoneName { lower } => (zone_text(moment, lower), None),
        Field::Offset => (
            moment.offset.map(datetime::format_offset).unwrap_or_default(),
            None,
        ),
    }
}

fn format_moment(moment: &Moment, template: &str) -> SqlResult<String> {
    let mut out = String::new();
    for piece in datetime_template(template)? {
        match piece {
            Piece::Text(text) => out.push_str(&text),
            Piece::Field {
                field,
                fill,
                ordinal,
            } => {
                let (text, number) = field_text(moment, field, fill);
                out.push_str(&text);
                if let (Some(upper), Some(number)) = (ordinal, number) {
                    out.push_str(ordinal_suffix(number, upper));
                }
            }
        }
        crate::memory::check_value_bytes(out.len())?;
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SignKind {
    Anchored,
    Minus,
    Plus,
    Both,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum NumberPiece {
    Digit { zero: bool },
    Point,
    Group,
    Sign(SignKind),
    Brackets,
    Currency,
    Roman { lower: bool },
    Ordinal { upper: bool },
    Shift,
    Exponent,
    Text(String),
}

struct NumberTemplate {
    pieces: Vec<NumberPiece>,
    fill: bool,
    integer_digits: usize,
    fraction_digits: usize,
    shift: u32,
}

fn number_template(template: &str) -> SqlResult<NumberTemplate> {
    let chars: Vec<char> = template.chars().collect();
    if chars.len() > MAX_TEMPLATE_CHARS {
        return Err(too_long_template());
    }
    let upper: Vec<char> = chars.iter().map(char::to_ascii_uppercase).collect();
    let mut pieces = Vec::new();
    let mut fill = false;
    let mut at = 0;
    let mut after_point = false;
    let mut after_shift = false;
    let mut integer_digits = 0;
    let mut fraction_digits = 0;
    let mut shift = 0u32;
    while at < chars.len() {
        let rest = &upper[at..];
        let (piece, length) = match upper[at] {
            '9' | '0' => {
                if after_shift {
                    shift = shift.saturating_add(1);
                }
                if after_point && !after_shift {
                    fraction_digits += 1;
                } else {
                    integer_digits += 1;
                }
                (
                    NumberPiece::Digit {
                        zero: upper[at] == '0',
                    },
                    1,
                )
            }
            '.' | 'D' if !starts_with_upper(rest, "DAY") => {
                if after_point {
                    return Err(SqlError::syntax("multiple decimal points", 0));
                }
                after_point = true;
                (NumberPiece::Point, 1)
            }
            ',' | 'G' => (NumberPiece::Group, 1),
            'S' if starts_with_upper(rest, "SG") => (NumberPiece::Sign(SignKind::Both), 2),
            'S' => (NumberPiece::Sign(SignKind::Anchored), 1),
            'M' if starts_with_upper(rest, "MI") => (NumberPiece::Sign(SignKind::Minus), 2),
            'P' if starts_with_upper(rest, "PL") => (NumberPiece::Sign(SignKind::Plus), 2),
            'P' if starts_with_upper(rest, "PR") => (NumberPiece::Brackets, 2),
            'L' => (NumberPiece::Currency, 1),
            'R' if starts_with_upper(rest, "RN") => (
                NumberPiece::Roman {
                    lower: chars[at] == 'r',
                },
                2,
            ),
            'T' if starts_with_upper(rest, "TH") => (
                NumberPiece::Ordinal {
                    upper: chars[at] == 'T',
                },
                2,
            ),
            'V' => {
                after_shift = true;
                (NumberPiece::Shift, 1)
            }
            'E' if starts_with_upper(rest, "EEEE") => (NumberPiece::Exponent, 4),
            'F' if starts_with_upper(rest, "FM") => {
                fill = true;
                at += 2;
                continue;
            }
            '"' => {
                let mut text = String::new();
                let mut end = at + 1;
                while end < chars.len() && chars[end] != '"' {
                    text.push(chars[end]);
                    end += 1;
                }
                (NumberPiece::Text(text), end + 1 - at)
            }
            _ => (NumberPiece::Text(chars[at].to_string()), 1),
        };
        pieces.push(piece);
        at += length;
    }
    Ok(NumberTemplate {
        pieces,
        fill,
        integer_digits,
        fraction_digits,
        shift,
    })
}

fn numeric_of(value: &Value) -> Option<Numeric> {
    match value {
        Value::Numeric(number) => Some(number.clone()),
        Value::Real(_) | Value::Double(_) => {
            let number = value.as_float()?;
            if !number.is_finite() {
                return Some(Numeric::nan());
            }
            Numeric::parse(&format!("{number}")).ok()
        }
        other => other.as_integer().map(Numeric::from_i64),
    }
}

fn digits_of(number: &Numeric) -> (bool, String, String) {
    let text = number.to_string();
    let (negative, magnitude) = match text.strip_prefix('-') {
        Some(rest) => (true, rest.to_owned()),
        None => (false, text),
    };
    let (whole, fraction) = match magnitude.split_once('.') {
        Some((whole, fraction)) => (whole.to_owned(), fraction.to_owned()),
        None => (magnitude.clone(), String::new()),
    };
    let whole = whole.trim_start_matches('0').to_owned();
    let is_zero = whole.is_empty() && fraction.bytes().all(|digit| digit == b'0');
    (negative && !is_zero, whole, fraction)
}

fn power_of_ten(exponent: u32) -> SqlResult<Numeric> {
    Numeric::parse(&format!("1{}", "0".repeat(exponent as usize)))
}

fn roman_text(number: &Numeric, lower: bool, fill: bool) -> String {
    let rounded = number.to_i128_rounded().and_then(|whole| i64::try_from(whole).ok());
    let numeral = match rounded.and_then(roman) {
        Some(numeral) if lower => numeral.to_ascii_lowercase(),
        Some(numeral) => numeral,
        None => return "#".repeat(ROMAN_WIDTH),
    };
    if fill {
        numeral
    } else {
        format!("{numeral:>width$}", width = ROMAN_WIDTH)
    }
}

fn exponent_text(number: &Numeric, template: &NumberTemplate) -> SqlResult<String> {
    let (negative, whole, fraction) = digits_of(number);
    let all = format!("{whole}{fraction}");
    let first = all.bytes().position(|digit| digit != b'0');
    let exponent = match first {
        None => 0i64,
        Some(at) => whole.len() as i64 - at as i64 - 1,
    };
    let scaled = if exponent >= 0 {
        number.divide(&power_of_ten(exponent as u32)?)?
    } else {
        number.multiply(&power_of_ten(exponent.unsigned_abs() as u32)?)?
    };
    let mut mantissa = scaled.absolute().rounded(template.fraction_digits as i64)?;
    let mut exponent = exponent;
    if mantissa.compare(&Numeric::from_i64(10)) != std::cmp::Ordering::Less {
        mantissa = mantissa
            .divide(&Numeric::from_i64(10))?
            .rounded(template.fraction_digits as i64)?;
        exponent += 1;
    }
    let sign = if negative {
        "-"
    } else if template.fill {
        ""
    } else {
        " "
    };
    let exponent_sign = if exponent < 0 { '-' } else { '+' };
    Ok(format!(
        "{sign}{mantissa}e{exponent_sign}{:02}",
        exponent.unsigned_abs()
    ))
}

struct IntegerArea {
    text: String,
    started: bool,
    leading_blanks: usize,
    open: bool,
    closed: bool,
}

impl IntegerArea {
    fn push_blank(&mut self) {
        self.text.push(' ');
        if !self.started {
            self.leading_blanks = self.text.len();
        }
    }

    fn push_shown(&mut self, character: char) {
        self.started = true;
        self.text.push(character);
    }
}

struct NumberShape {
    negative: bool,
    fill: bool,
    explicit_sign: bool,
    anchored_leading: bool,
}

impl NumberShape {
    fn sign_of(&self, kind: SignKind) -> Option<char> {
        match kind {
            SignKind::Minus if !self.negative => (!self.fill).then_some(' '),
            SignKind::Minus => Some('-'),
            SignKind::Anchored | SignKind::Both | SignKind::Plus => {
                Some(if self.negative { '-' } else { '+' })
            }
        }
    }

    fn leading_sign(&self) -> Option<char> {
        if self.anchored_leading {
            Some(if self.negative { '-' } else { '+' })
        } else if self.explicit_sign {
            None
        } else if self.negative {
            Some('-')
        } else if self.fill {
            None
        } else {
            Some(' ')
        }
    }

    fn close(&self, out: &mut String, area: &mut IntegerArea) {
        if area.closed {
            return;
        }
        area.closed = true;
        let blanks = area.leading_blanks.min(area.text.len());
        let (lead, body) = area.text.split_at(blanks);
        if !self.fill {
            out.push_str(lead);
        }
        if let Some(sign) = self.leading_sign() {
            out.push(sign);
        }
        out.push_str(body);
    }
}

fn first_zero_position(pieces: &[NumberPiece]) -> Option<usize> {
    let mut position = 0;
    for piece in pieces {
        match piece {
            NumberPiece::Digit { zero } => {
                if *zero {
                    return Some(position);
                }
                position += 1;
            }
            NumberPiece::Point => return None,
            _ => {}
        }
    }
    None
}

fn format_number(value: &Value, template_text: &str) -> SqlResult<String> {
    let template = number_template(template_text)?;
    let Some(number) = numeric_of(value) else {
        return Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("function to_char({}, unknown) does not exist", value.type_name()),
        ));
    };
    if let Some(NumberPiece::Roman { lower }) = template
        .pieces
        .iter()
        .find(|piece| matches!(piece, NumberPiece::Roman { .. }))
    {
        return Ok(roman_text(&number, *lower, template.fill));
    }
    if template.pieces.contains(&NumberPiece::Exponent) {
        return exponent_text(&number, &template);
    }
    let shifted = if template.shift > 0 && !number.is_nan() {
        number.multiply(&power_of_ten(template.shift)?)?
    } else {
        number.clone()
    };
    let rounded = if number.is_nan() {
        shifted
    } else {
        shifted.rounded(template.fraction_digits as i64)?
    };
    let (negative, whole, fraction) = digits_of(&rounded);
    let overflow = number.is_nan() || whole.len() > template.integer_digits;
    let whole_digits: Vec<char> = whole.chars().collect();
    let fraction_digits: Vec<char> = fraction.chars().collect();
    let first_zero = first_zero_position(&template.pieces);
    let shape = NumberShape {
        negative,
        fill: template.fill,
        explicit_sign: template
            .pieces
            .iter()
            .any(|piece| matches!(piece, NumberPiece::Sign(_) | NumberPiece::Brackets)),
        anchored_leading: template
            .pieces
            .iter()
            .take_while(|piece| !matches!(piece, NumberPiece::Digit { .. } | NumberPiece::Point))
            .any(|piece| matches!(piece, NumberPiece::Sign(SignKind::Anchored))),
    };
    let missing = template.integer_digits.saturating_sub(whole_digits.len());
    let blank_integer = whole_digits.is_empty() && template.fraction_digits == 0;
    let mut area = IntegerArea {
        text: String::new(),
        started: false,
        leading_blanks: 0,
        open: false,
        closed: false,
    };
    let mut out = String::new();
    let mut digit_index = 0usize;
    let mut fraction_index = 0usize;
    let mut after_point = false;
    let mut fraction_text = String::new();
    let mut fraction_keep = 0usize;
    for piece in &template.pieces {
        if after_point && !matches!(piece, NumberPiece::Digit { .. }) && !fraction_text.is_empty() {
            out.push_str(&fraction_text[..fraction_keep]);
            fraction_text.clear();
            fraction_keep = 0;
        }
        match piece {
            NumberPiece::Digit { zero } if !after_point => {
                area.open = true;
                if overflow {
                    area.push_shown('#');
                } else if digit_index >= missing {
                    area.push_shown(whole_digits.get(digit_index - missing).copied().unwrap_or('0'));
                } else if *zero
                    || first_zero.is_some_and(|first| digit_index >= first)
                    || (blank_integer && digit_index + 1 == template.integer_digits)
                {
                    area.push_shown('0');
                } else {
                    area.push_blank();
                }
                digit_index += 1;
            }
            NumberPiece::Digit { zero } => {
                let shown = if overflow {
                    '#'
                } else {
                    fraction_digits.get(fraction_index).copied().unwrap_or('0')
                };
                fraction_index += 1;
                fraction_text.push(shown);
                if *zero || shown != '0' || !template.fill {
                    fraction_keep = fraction_text.len();
                }
            }
            NumberPiece::Group if !after_point && area.open => {
                if area.started {
                    area.text.push(',');
                } else if !template.fill {
                    area.push_blank();
                }
            }
            NumberPiece::Point => {
                shape.close(&mut out, &mut area);
                after_point = true;
                out.push('.');
            }
            NumberPiece::Sign(kind) => {
                if *kind == SignKind::Anchored && shape.anchored_leading {
                    continue;
                }
                let Some(sign) = shape.sign_of(*kind) else {
                    continue;
                };
                if area.open && !area.closed {
                    area.text.push(sign);
                } else {
                    out.push(sign);
                }
            }
            NumberPiece::Text(text) => {
                if area.open && !area.closed {
                    if text.chars().all(|character| character == ' ') && !area.started {
                        for _ in text.chars() {
                            area.push_blank();
                        }
                    } else {
                        area.text.push_str(text);
                    }
                } else {
                    out.push_str(text);
                }
            }
            NumberPiece::Brackets => {
                shape.close(&mut out, &mut area);
                if negative {
                    out.insert(0, '<');
                    out.push('>');
                } else if !template.fill {
                    out.insert(0, ' ');
                    out.push(' ');
                }
            }
            NumberPiece::Ordinal { upper } => {
                shape.close(&mut out, &mut area);
                if !negative {
                    let whole_number = whole.parse::<i64>().unwrap_or(0);
                    out.push_str(ordinal_suffix(whole_number, *upper));
                }
            }
            NumberPiece::Currency => {
                if area.open && !area.closed {
                    area.text.push(' ');
                } else {
                    out.push(' ');
                }
            }
            NumberPiece::Group
            | NumberPiece::Roman { .. }
            | NumberPiece::Shift
            | NumberPiece::Exponent => {}
        }
        crate::memory::check_value_bytes(out.len().saturating_add(area.text.len()))?;
    }
    if !fraction_text.is_empty() {
        out.push_str(&fraction_text[..fraction_keep]);
    }
    if area.open {
        shape.close(&mut out, &mut area);
    }
    Ok(out)
}

pub(crate) fn to_char(values: &[Value]) -> Option<SqlResult<Value>> {
    let [value, template] = values else {
        return None;
    };
    if value.is_null() || template.is_null() {
        return Some(Ok(Value::Null));
    }
    let template = template.character_text()?;
    let formatted = match value {
        Value::Date(days) => format_moment(
            &Moment::of_local(i64::from(*days) * MICROS_PER_DAY, None),
            template,
        ),
        Value::Timestamp(micros) => format_moment(&Moment::of_local(*micros, None), template),
        Value::TimestampTz(micros) => {
            let zone = datetime::session_zone();
            let offset = zone.offset_at(*micros);
            let mut moment = Moment::of_local(datetime::to_local(*micros, offset), Some(offset));
            moment.abbreviation = zone.abbreviation_at(*micros);
            format_moment(&moment, template)
        }
        Value::Interval(interval) => format_moment(&Moment::of_interval(*interval), template),
        Value::SmallInt(_)
        | Value::Integer(_)
        | Value::BigInt(_)
        | Value::Numeric(_)
        | Value::Real(_)
        | Value::Double(_) => format_number(value, template),
        _ => return None,
    };
    Some(formatted.map(Value::Text))
}

fn invalid_value(found: &str, pattern: &str, detail: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_DATETIME_FORMAT,
        format!("invalid value \"{found}\" for \"{pattern}\""),
    )
    .with_detail(detail.to_owned())
}

fn out_of_range(text: &str) -> SqlError {
    SqlError::new(
        SqlState::DATETIME_FIELD_OVERFLOW,
        format!("date/time field value out of range: \"{text}\""),
    )
}

#[derive(Default)]
struct Parts {
    year: Option<i64>,
    year_digits: usize,
    before_christ: bool,
    century: Option<i64>,
    month: Option<i64>,
    day: Option<i64>,
    day_of_year: Option<i64>,
    julian: Option<i64>,
    hour: Option<i64>,
    twelve_hour: bool,
    afternoon: Option<bool>,
    minute: Option<i64>,
    second: Option<i64>,
    micros: Option<i64>,
    offset: Option<i64>,
    zone_minutes: Option<i64>,
    zone_negative: bool,
    region: Option<crate::time_zones::TimeZone>,
}

struct Reader<'a> {
    chars: &'a [char],
    at: usize,
}

impl Reader<'_> {
    fn skip_spaces(&mut self) {
        while self.chars.get(self.at).is_some_and(|character| character.is_whitespace()) {
            self.at += 1;
        }
    }

    fn digits(&mut self, most: usize) -> String {
        let mut taken = String::new();
        while taken.len() < most {
            match self.chars.get(self.at) {
                Some(character) if character.is_ascii_digit() => {
                    taken.push(*character);
                    self.at += 1;
                }
                _ => break,
            }
        }
        taken
    }

    fn word(&mut self, options: &[&str]) -> Option<usize> {
        let rest: String = self.chars[self.at.min(self.chars.len())..]
            .iter()
            .take(12)
            .collect::<String>()
            .to_ascii_uppercase();
        let found = options
            .iter()
            .enumerate()
            .filter(|(_, option)| rest.starts_with(&option.to_ascii_uppercase()))
            .max_by_key(|(_, option)| option.len())?;
        self.at += found.1.chars().count();
        Some(found.0)
    }

    fn shown(&self) -> String {
        self.chars[self.at.min(self.chars.len())..]
            .iter()
            .take_while(|character| !character.is_whitespace())
            .take(20)
            .collect()
    }
}

fn pattern_name(field: Field) -> &'static str {
    match field {
        Field::Hour12 => "HH12",
        Field::Hour24 => "HH24",
        Field::Minute => "MI",
        Field::Second => "SS",
        Field::Millisecond => "MS",
        Field::Microsecond => "US",
        Field::Fraction(_) => "FF",
        Field::Year(4) => "YYYY",
        Field::Year(_) => "YY",
        Field::MonthNumber => "MM",
        Field::DayOfMonth => "DD",
        Field::DayOfYear => "DDD",
        Field::MonthName(_) => "Month",
        Field::MonthAbbreviation(_) => "Mon",
        _ => "pattern",
    }
}

fn widest(field: Field) -> usize {
    match field {
        Field::Year(4) | Field::IsoYear(4) | Field::YearWithComma => 4,
        Field::Year(width) | Field::IsoYear(width) => usize::from(width),
        Field::DayOfYear | Field::IsoDayOfYear | Field::Millisecond => 3,
        Field::Microsecond => 6,
        Field::Fraction(digits) => usize::from(digits),
        Field::SecondsOfDay => 5,
        Field::JulianDay => 9,
        Field::DayOfWeek | Field::IsoDayOfWeek | Field::Quarter | Field::WeekOfMonth => 1,
        _ => 2,
    }
}

fn reads_digits(piece: Option<&Piece>) -> bool {
    matches!(piece, Some(Piece::Field { field, .. }) if is_numeric_field(*field))
}

fn read_number(
    reader: &mut Reader<'_>,
    field: Field,
    next: Option<&Piece>,
) -> SqlResult<(i64, usize)> {
    reader.skip_spaces();
    let negative = matches!(field, Field::Year(_)) && reader.chars.get(reader.at) == Some(&'-');
    if negative {
        reader.at += 1;
    }
    let most = if reads_digits(next) || matches!(field, Field::Fraction(_) | Field::Millisecond | Field::Microsecond) {
        widest(field)
    } else {
        widest(field).max(9)
    };
    let digits = reader.digits(most);
    if digits.is_empty() {
        return Err(invalid_value(
            &reader.shown(),
            pattern_name(field),
            "Value must be an integer.",
        ));
    }
    let value: i64 = digits.parse().map_err(|_| {
        invalid_value(&digits, pattern_name(field), "Value must be an integer.")
    })?;
    Ok((if negative { -value } else { value }, digits.len()))
}

fn fill_parts(text: &str, template: &str) -> SqlResult<Parts> {
    let pieces = datetime_template(template)?;
    let chars: Vec<char> = text.chars().collect();
    let mut reader = Reader {
        chars: &chars,
        at: 0,
    };
    let mut parts = Parts::default();
    for (index, piece) in pieces.iter().enumerate() {
        match piece {
            Piece::Text(literal) => {
                for character in literal.chars() {
                    if character.is_whitespace() {
                        reader.skip_spaces();
                    } else if !character.is_alphanumeric() {
                        reader.skip_spaces();
                        if reader
                            .chars
                            .get(reader.at)
                            .is_some_and(|found| !found.is_alphanumeric())
                        {
                            reader.at += 1;
                        }
                    } else if reader.at < reader.chars.len() {
                        reader.at += 1;
                    }
                }
            }
            Piece::Field { field, .. } => {
                let next = pieces.get(index + 1);
                read_field(&mut reader, *field, next, &mut parts)?;
            }
        }
    }
    Ok(parts)
}

fn read_field(
    reader: &mut Reader<'_>,
    field: Field,
    next: Option<&Piece>,
    parts: &mut Parts,
) -> SqlResult<()> {
    match field {
        Field::Meridian { .. } => {
            reader.skip_spaces();
            let found = reader
                .word(&["AM", "PM", "A.M.", "P.M."])
                .ok_or_else(|| {
                    invalid_value(
                        &reader.shown(),
                        "AM",
                        "The given value did not match any of the allowed values for this field.",
                    )
                })?;
            parts.afternoon = Some(found % 2 == 1);
        }
        Field::Era { .. } => {
            reader.skip_spaces();
            let found = reader.word(&["AD", "BC", "A.D.", "B.C."]).ok_or_else(|| {
                invalid_value(
                    &reader.shown(),
                    "BC",
                    "The given value did not match any of the allowed values for this field.",
                )
            })?;
            parts.before_christ = found % 2 == 1;
        }
        Field::MonthName(_) | Field::MonthAbbreviation(_) => {
            reader.skip_spaces();
            let full: Vec<String> = MONTHS.iter().map(|name| (*name).to_owned()).collect();
            let short: Vec<String> = MONTHS
                .iter()
                .map(|name| name.get(..3).unwrap_or(name).to_owned())
                .collect();
            let names: Vec<&str> = if matches!(field, Field::MonthName(_)) {
                full.iter().map(String::as_str).chain(short.iter().map(String::as_str)).collect()
            } else {
                short.iter().map(String::as_str).collect()
            };
            let found = reader.word(&names).ok_or_else(|| {
                invalid_value(
                    &reader.shown(),
                    pattern_name(field),
                    "The given value did not match any of the allowed values for this field.",
                )
            })?;
            parts.month = Some(found as i64 % 12 + 1);
        }
        Field::DayName(_) | Field::DayAbbreviation(_) => {
            reader.skip_spaces();
            let full: Vec<&str> = DAYS.to_vec();
            let short: Vec<&str> = DAYS.iter().map(|name| name.get(..3).unwrap_or(name)).collect();
            let names: Vec<&str> = full.into_iter().chain(short).collect();
            reader.word(&names).ok_or_else(|| {
                invalid_value(
                    &reader.shown(),
                    "Day",
                    "The given value did not match any of the allowed values for this field.",
                )
            })?;
        }
        Field::ZoneHours => {
            reader.skip_spaces();
            let negative = reader.chars.get(reader.at) == Some(&'-');
            if matches!(reader.chars.get(reader.at), Some('-' | '+')) {
                reader.at += 1;
            }
            let (hours, _) = read_number(reader, field, next)?;
            parts.zone_negative = negative;
            parts.offset = Some(hours * 3600);
        }
        Field::ZoneMinutes => {
            let (minutes, _) = read_number(reader, field, next)?;
            parts.zone_minutes = Some(minutes);
        }
        Field::Offset | Field::ZoneName { .. } => {
            reader.skip_spaces();
            let zone: String = reader.chars[reader.at.min(reader.chars.len())..]
                .iter()
                .take_while(|character| !character.is_whitespace())
                .collect();
            reader.at += zone.chars().count();
            if datetime::time_zone_offset(&zone).is_none() {
                if let Some(region) = crate::time_zones::TimeZone::region(&zone) {
                    parts.region = Some(region);
                    return Ok(());
                }
            }
            let east = datetime::time_zone_offset(&zone)
                .map(i64::from)
                .ok_or_else(|| {
                    SqlError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("time zone \"{zone}\" not recognized"),
                    )
                })?;
            parts.offset = Some(if matches!(field, Field::Offset) {
                -east
            } else {
                east
            });
        }
        Field::RomanMonth { .. } => {
            reader.skip_spaces();
            let numerals = [
                "I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX", "X", "XI", "XII",
            ];
            let found = reader.word(&numerals).ok_or_else(|| {
                invalid_value(
                    &reader.shown(),
                    "RM",
                    "The given value did not match any of the allowed values for this field.",
                )
            })?;
            parts.month = Some(found as i64 + 1);
        }
        Field::YearWithComma => {
            let (thousands, _) = read_number(reader, Field::Year(1), next)?;
            if reader.chars.get(reader.at) == Some(&',') {
                reader.at += 1;
            }
            let (rest, _) = read_number(reader, Field::Year(3), next)?;
            parts.year = Some(thousands * 1000 + rest);
            parts.year_digits = 4;
        }
        Field::Fraction(_) | Field::Millisecond | Field::Microsecond => {
            let (value, digits) = read_number(reader, field, next)?;
            let scale = match field {
                Field::Millisecond => 1000,
                Field::Microsecond => 1,
                _ => 10i64.pow(6u32.saturating_sub(digits as u32)),
            };
            let micros = if matches!(field, Field::Millisecond) {
                value * 1000
            } else {
                value * scale
            };
            parts.micros = Some(parts.micros.unwrap_or(0) + micros);
        }
        other => {
            let (value, digits) = read_number(reader, other, next)?;
            match other {
                Field::Year(_) | Field::IsoYear(_) => {
                    parts.year = Some(value);
                    parts.year_digits = digits;
                }
                Field::Century => parts.century = Some(value),
                Field::MonthNumber => parts.month = Some(value),
                Field::DayOfMonth => parts.day = Some(value),
                Field::DayOfYear | Field::IsoDayOfYear => parts.day_of_year = Some(value),
                Field::JulianDay => parts.julian = Some(value),
                Field::Hour12 => {
                    parts.hour = Some(value);
                    parts.twelve_hour = true;
                }
                Field::Hour24 => parts.hour = Some(value),
                Field::Minute => parts.minute = Some(value),
                Field::Second => parts.second = Some(value),
                Field::SecondsOfDay => {
                    parts.hour = Some(value / 3600);
                    parts.minute = Some(value / 60 % 60);
                    parts.second = Some(value % 60);
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn adjusted_year(year: i64, digits: usize) -> i64 {
    match digits {
        1 => year + 2020,
        2 if year < 70 => year + 2000,
        2 => year + 1900,
        3 if year < 520 => year + 2000,
        3 => year + 1000,
        _ => year,
    }
}

fn assembled(parts: &Parts, text: &str) -> SqlResult<(i64, Option<i64>)> {
    let mut year = match (parts.year, parts.century) {
        (Some(year), _) => adjusted_year(year, parts.year_digits),
        (None, Some(century)) => (century - 1) * 100 + 1,
        (None, None) => 0,
    };
    if parts.before_christ && parts.year.is_some() {
        year = 1 - year;
    }
    let days = if let Some(julian) = parts.julian {
        julian - JULIAN_DAY_OF_EPOCH
    } else if let (Some(day_of_year), None) = (parts.day_of_year, parts.month) {
        let length = if datetime::is_leap_year(year) { 366 } else { 365 };
        if !(1..=length).contains(&day_of_year) {
            return Err(out_of_range(text));
        }
        datetime::days_from_civil(year, 1, 1) + day_of_year - 1
    } else {
        let month = parts.month.unwrap_or(1);
        let day = parts.day.unwrap_or(1);
        if !(1..=12).contains(&month) || day < 1 || day > datetime::days_in_month(year, month) {
            return Err(out_of_range(text));
        }
        datetime::days_from_civil(year, month, day)
    };
    let mut hour = parts.hour.unwrap_or(0);
    if parts.twelve_hour || parts.afternoon.is_some() {
        if !(1..=12).contains(&hour) && parts.hour.is_some() {
            return Err(SqlError::new(
                SqlState::INVALID_DATETIME_FORMAT,
                format!("hour \"{hour}\" is invalid for the 12-hour clock"),
            )
            .with_hint("Use the 24-hour clock, or give an hour between 1 and 12."));
        }
        match parts.afternoon {
            Some(true) if hour < 12 => hour += 12,
            Some(false) if hour == 12 => hour = 0,
            _ => {}
        }
    }
    let minute = parts.minute.unwrap_or(0);
    let second = parts.second.unwrap_or(0);
    if !(0..=24).contains(&hour) || !(0..=59).contains(&minute) || !(0..=60).contains(&second) {
        return Err(out_of_range(text));
    }
    let micros = i128::from(days) * i128::from(MICROS_PER_DAY)
        + i128::from(hour * 3600 + minute * 60 + second) * i128::from(MICROS_PER_SECOND)
        + i128::from(parts.micros.unwrap_or(0));
    let local = datetime::checked_timestamp(micros)?;
    let offset = parts.offset.map(|seconds| {
        let minutes = parts.zone_minutes.unwrap_or(0) * 60;
        let total = seconds + minutes;
        if parts.zone_negative { -total } else { total }
    });
    let offset = offset
        .or(parts.zone_minutes.map(|minutes| minutes * 60))
        .or_else(|| parts.region.map(|zone| i64::from(zone.offset_of_local(local))));
    Ok((local, offset))
}

pub(crate) fn to_date(values: &[Value]) -> Option<SqlResult<Value>> {
    let [text, template] = values else {
        return None;
    };
    if text.is_null() || template.is_null() {
        return Some(Ok(Value::Null));
    }
    let (text, template) = (text.character_text()?, template.character_text()?);
    Some(
        fill_parts(text, template)
            .and_then(|parts| assembled(&parts, text))
            .and_then(|(local, _)| datetime::checked_date(local.div_euclid(MICROS_PER_DAY)))
            .map(Value::Date),
    )
}

fn from_epoch(seconds: f64) -> SqlResult<Value> {
    if seconds.is_nan() {
        return Err(SqlError::new(
            SqlState::DATETIME_FIELD_OVERFLOW,
            "timestamp cannot be NaN",
        ));
    }
    let micros = ((seconds - UNIX_EPOCH_SECONDS) * 1_000_000.0).round();
    if !micros.is_finite() || micros.abs() > 9.2e18 {
        return Err(datetime::timestamp_out_of_range());
    }
    datetime::checked_timestamp(micros as i128).map(Value::TimestampTz)
}

pub(crate) fn to_timestamp(values: &[Value]) -> Option<SqlResult<Value>> {
    match values {
        [Value::Null] | [Value::Null, _] | [_, Value::Null] => Some(Ok(Value::Null)),
        [seconds] => Some(from_epoch(seconds.as_float()?)),
        [text, template] => {
            let (text, template) = (text.character_text()?, template.character_text()?);
            Some(
                fill_parts(text, template)
                    .and_then(|parts| assembled(&parts, text))
                    .and_then(|(local, offset)| match offset {
                        Some(offset) => datetime::checked_timestamp(
                            i128::from(local) - i128::from(offset) * i128::from(MICROS_PER_SECOND),
                        ),
                        None => datetime::local_to_utc(local),
                    })
                    .map(Value::TimestampTz),
            )
        }
        _ => None,
    }
}

fn read_number_text(text: &str, template: &NumberTemplate) -> SqlResult<Numeric> {
    let has_point = template.pieces.contains(&NumberPiece::Point);
    let mut whole = String::new();
    let mut fraction = String::new();
    let mut negative = false;
    let mut in_fraction = false;
    for character in text.chars() {
        match character {
            '0'..='9' if in_fraction => {
                if fraction.len() < template.fraction_digits {
                    fraction.push(character);
                }
            }
            '0'..='9' => {
                if whole.len() < template.integer_digits.max(1) {
                    whole.push(character);
                }
            }
            '.' if has_point && !in_fraction => in_fraction = true,
            '-' | '<' => negative = true,
            _ => {}
        }
    }
    if whole.is_empty() && fraction.is_empty() {
        return Err(SqlError::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!("invalid input syntax for type numeric: \"{text}\""),
        ));
    }
    let written = format!(
        "{}{}.{}",
        if negative { "-" } else { "" },
        if whole.is_empty() { "0" } else { &whole },
        if fraction.is_empty() { "0" } else { &fraction }
    );
    let mut number = Numeric::parse(&written)?.rounded(fraction.len() as i64)?;
    if template.shift > 0 {
        number = number.divide(&power_of_ten(template.shift)?)?;
    }
    Ok(number)
}

pub(crate) fn to_number(values: &[Value]) -> Option<SqlResult<Value>> {
    let [text, template] = values else {
        return None;
    };
    if text.is_null() || template.is_null() {
        return Some(Ok(Value::Null));
    }
    let (text, template) = (text.character_text()?, template.character_text()?);
    Some(
        number_template(template)
            .and_then(|template| read_number_text(text, &template))
            .map(Value::Numeric),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn number(value: &str, template: &str) -> String {
        let value = Value::Numeric(Numeric::parse(value).unwrap());
        format_number(&value, template).unwrap()
    }

    #[test]
    fn numbers_format_as_the_manuals_examples_show() {
        let cases = [
            ("-0.1", "99.99", "  -.10"),
            ("-0.1", "FM9.99", "-.1"),
            ("-0.1", "FM90.99", "-0.1"),
            ("0.1", "0.9", " 0.1"),
            ("12", "9990999.9", "    0012.0"),
            ("12", "FM9990999.9", "0012."),
            ("485", "999", " 485"),
            ("-485", "999", "-485"),
            ("485", "9 9 9", " 4 8 5"),
            ("1485", "9,999", " 1,485"),
            ("1485", "9G999", " 1,485"),
            ("148.5", "999.999", " 148.500"),
            ("148.5", "FM999.999", "148.5"),
            ("148.5", "FM999.990", "148.500"),
            ("148.5", "999D999", " 148.500"),
            ("3148.5", "9G999D999", " 3,148.500"),
            ("-485", "999S", "485-"),
            ("-485", "999MI", "485-"),
            ("485", "999MI", "485 "),
            ("485", "FM999MI", "485"),
            ("485", "PL999", "+485"),
            ("485", "SG999", "+485"),
            ("-485", "SG999", "-485"),
            ("-485", "9SG99", "4-85"),
            ("-485", "999PR", "<485>"),
            ("485", "RN", "        CDLXXXV"),
            ("485", "FMRN", "CDLXXXV"),
            ("5.2", "FMRN", "V"),
            ("482", "999th", " 482nd"),
            ("485", "\"Good number:\"999", "Good number: 485"),
            ("485.8", "\"Pre:\"999\" Post:\" .999", "Pre: 485 Post: .800"),
            ("12", "99V999", " 12000"),
            ("12.4", "99V999", " 12400"),
            ("12.45", "99V9", " 125"),
            ("0.0004859", "9.99EEEE", " 4.86e-04"),
            ("1234", "99", " ##"),
        ];
        for (value, template, expected) in cases {
            assert_eq!(number(value, template), expected, "to_char({value}, {template})");
        }
    }

    fn moment(text: &str, template: &str) -> String {
        let micros = datetime::parse_timestamp(text, false).unwrap();
        format_moment(&Moment::of_local(micros, None), template).unwrap()
    }

    #[test]
    fn dates_and_times_format_with_postgresql_patterns() {
        let cases = [
            ("2002-04-20 17:31:12.66", "HH12:MI:SS", "05:31:12"),
            ("2002-04-20 17:31:12.66", "HH24:MI:SS.MS", "17:31:12.660"),
            ("2002-04-20 17:31:12.66", "Day, DD  HH12:MI:SS", "Saturday , 20  05:31:12"),
            ("2002-04-20 17:31:12.66", "FMDay, FMDD  HH12:MI:SS", "Saturday, 20  05:31:12"),
            ("2002-04-20 17:31:12.66", "YYYY-MM-DD\"T\"HH24:MI:SS", "2002-04-20T17:31:12"),
            ("2002-04-20 17:31:12.66", "Month MON mon Mon", "April     APR apr Apr"),
            ("2002-04-20 17:31:12.66", "DDDth Q CC J IW WW D ID", "110th 2 21 2452385 16 16 7 6"),
            ("2002-04-01 08:00:00", "FMDDth FMMonth YYYY am", "1st April 2002 am"),
            ("2002-04-01 08:00:00", "RM rm Y,YYY yyyy", "IV   iv   2,002 2002"),
        ];
        for (text, template, expected) in cases {
            assert_eq!(moment(text, template), expected, "to_char({text}, {template})");
        }
    }

    #[test]
    fn text_reads_back_by_the_template() {
        let date = |text: &str, template: &str| {
            to_date(&[Value::Text(text.into()), Value::Text(template.into())])
                .unwrap()
                .map(|value| value.to_text().unwrap_or_default())
        };
        assert_eq!(date("05 Dec 2000", "DD Mon YYYY").unwrap(), "2000-12-05");
        assert_eq!(date("2000-12-05", "YYYY-MM-DD").unwrap(), "2000-12-05");
        assert_eq!(date("20001205", "YYYYMMDD").unwrap(), "2000-12-05");
        assert_eq!(date("5 12 95", "DD MM YY").unwrap(), "1995-12-05");
        assert_eq!(
            date("2009-02-30", "YYYY-MM-DD").unwrap_err().state().code(),
            "22008"
        );
        assert_eq!(date("2000-xx-01", "YYYY-MM-DD").unwrap_err().state().code(), "22007");
        let number = |text: &str, template: &str| {
            to_number(&[Value::Text(text.into()), Value::Text(template.into())])
                .unwrap()
                .map(|value| value.to_text().unwrap_or_default())
        };
        assert_eq!(number("12,454.8-", "99G999D9S").unwrap(), "-12454.8");
        assert_eq!(number("1,234", "9G999").unwrap(), "1234");
    }

    #[test]
    fn hostile_templates_never_panic() {
        let mut seed = 0x70_C4A2_u64;
        let alphabet: Vec<char> = "9.0,SMIPLRNTHVEDGFYMHH24\"x \\".chars().collect();
        for _ in 0..5_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let length = (seed % 24) as usize;
            let template: String = (0..length)
                .map(|at| alphabet[((seed >> (at % 48)) as usize + at * 7) % alphabet.len()])
                .collect();
            let _ = format_number(&Value::Integer(-1234), &template);
            let _ = format_number(&Value::Double(0.5), &template);
            let _ = format_moment(&Moment::of_local(0, Some(3600)), &template);
            let _ = fill_parts("2024-02-29 13:45:01", &template);
            let _ = number_template(&template).and_then(|parsed| read_number_text("-1,2.5", &parsed));
        }
    }
}
