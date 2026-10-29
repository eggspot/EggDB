use crate::datetime::{self, MICROS_PER_DAY, MICROS_PER_SECOND};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{SqlType, Value};
use std::cell::RefCell;
use std::rc::Rc;

pub(crate) const MAX_MODEL_BYTES: usize = 1000;
const MAX_INPUT_BYTES: usize = 4000;
const MAX_ELEMENT_BYTES: usize = 9;
const CACHED_MODELS: usize = 8;
const NAME_WIDTH: usize = 9;
const JULIAN_DAY_OF_EPOCH: i64 = 2_451_545;
const NANOS_PER_MICRO: i64 = 1000;

pub(crate) const MONTHS: [&str; 12] = [
    "JANUARY",
    "FEBRUARY",
    "MARCH",
    "APRIL",
    "MAY",
    "JUNE",
    "JULY",
    "AUGUST",
    "SEPTEMBER",
    "OCTOBER",
    "NOVEMBER",
    "DECEMBER",
];

pub(crate) const DAYS: [&str; 7] = [
    "SUNDAY",
    "MONDAY",
    "TUESDAY",
    "WEDNESDAY",
    "THURSDAY",
    "FRIDAY",
    "SATURDAY",
];

const TO_CHAR_ONLY: [&str; 12] = [
    "SYEAR", "SCC", "IYYY", "IYY", "YEAR", "IW", "IY", "CC", "WW", "I", "Q", "W",
];

const NOT_YET: [&str; 19] = [
    "SYYYY", "Y,YYY", "A.D.", "B.C.", "SPTH", "TZH", "TZM", "TZR", "TZD", "AD", "BC", "DS", "DL",
    "RM", "TS", "TH", "SP", "EE", "E",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ModelUse {
    DateInput,
    TimestampInput,
    Output,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Spelling {
    Upper,
    Capitalized,
    Lower,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DatetimeElement {
    Text(String),
    Punctuation(char),
    FillMode,
    ExactMode,
    Meridian { dotted: bool, spelling: Spelling },
    DayOfMonth,
    FractionalSeconds(Option<u8>),
    Hour12,
    Hour24,
    Minute,
    Second,
    MonthNumber,
    MonthAbbreviation(Spelling),
    MonthName(Spelling),
    Year(u8),
    RoundedYear(u8),
    RadixCharacter,
    DayOfWeek,
    DayName(Spelling),
    DayAbbreviation(Spelling),
    DayOfYear,
    JulianDay,
    SecondsPastMidnight,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DatetimeModel {
    elements: Vec<DatetimeElement>,
}

fn date_error(code: u16, message: &str) -> SqlError {
    let state = match code {
        1841 | 1847 | 1839 | 1849 | 1850 | 1851 | 1852 => SqlState::DATETIME_FIELD_OVERFLOW,
        1801 => SqlState::PROGRAM_LIMIT_EXCEEDED,
        3001 => SqlState::FEATURE_NOT_SUPPORTED,
        _ => SqlState::INVALID_DATETIME_FORMAT,
    };
    SqlError::new(state, message).with_oracle_code(code)
}

pub(crate) fn not_recognized() -> SqlError {
    date_error(1821, "date format not recognized")
}

pub(crate) fn year_out_of_range() -> SqlError {
    date_error(
        1841,
        "(full) year must be between -4713 and +9999, and not be 0",
    )
}

fn not_a_month() -> SqlError {
    date_error(1843, "not a valid month")
}

fn not_numeric() -> SqlError {
    date_error(
        1858,
        "a non-numeric character was found where a numeric was expected",
    )
}

fn literal_mismatch() -> SqlError {
    date_error(1861, "literal does not match format string")
}

fn input_too_short() -> SqlError {
    date_error(1840, "input value not long enough for date format")
}

fn input_left_over() -> SqlError {
    date_error(
        1830,
        "date format picture ends before converting entire input string",
    )
}

fn spelling_of(text: &[char]) -> Spelling {
    let mut letters = text
        .iter()
        .filter(|character| character.is_ascii_alphabetic());
    match (letters.next(), letters.next()) {
        (Some(first), _) if first.is_ascii_lowercase() => Spelling::Lower,
        (Some(_), Some(second)) if second.is_ascii_lowercase() => Spelling::Capitalized,
        _ => Spelling::Upper,
    }
}

fn starts_with(upper: &[char], word: &str) -> bool {
    word.chars().count() <= upper.len() && word.chars().zip(upper).all(|(a, b)| a == *b)
}

enum Scanned {
    Element(DatetimeElement, usize),
    ToCharOnly,
}

fn element_at(chars: &[char], upper: &[char]) -> SqlResult<Scanned> {
    use DatetimeElement as E;
    let spelling = |length: usize| spelling_of(&chars[..length.min(chars.len())]);
    let first = upper[0];
    if first == '"' {
        let Some(end) = chars[1..].iter().position(|character| *character == '"') else {
            return Err(not_recognized());
        };
        return Ok(Scanned::Element(
            E::Text(chars[1..=end].iter().collect()),
            end + 2,
        ));
    }
    if matches!(first, '-' | '/' | ',' | '.' | ';' | ':' | ' ') {
        return Ok(Scanned::Element(E::Punctuation(chars[0]), 1));
    }
    if NOT_YET.iter().any(|word| starts_with(upper, word)) {
        return Err(not_recognized());
    }
    if TO_CHAR_ONLY.iter().any(|word| starts_with(upper, word)) {
        return Ok(Scanned::ToCharOnly);
    }
    if starts_with(upper, "FF") {
        if let Some(digit) = upper.get(2).and_then(|digit| digit.to_digit(10))
            && digit >= 1
        {
            return Ok(Scanned::Element(E::FractionalSeconds(Some(digit as u8)), 3));
        }
        return Ok(Scanned::Element(E::FractionalSeconds(None), 2));
    }
    let meridian = |dotted: bool| move |spelling: Spelling| E::Meridian { dotted, spelling };
    let table: [(&str, &dyn Fn(Spelling) -> DatetimeElement); 28] = [
        ("A.M.", &meridian(true)),
        ("P.M.", &meridian(true)),
        ("AM", &meridian(false)),
        ("PM", &meridian(false)),
        ("FM", &|_| E::FillMode),
        ("FX", &|_| E::ExactMode),
        ("HH24", &|_| E::Hour24),
        ("HH12", &|_| E::Hour12),
        ("HH", &|_| E::Hour12),
        ("MONTH", &E::MonthName),
        ("MON", &E::MonthAbbreviation),
        ("MM", &|_| E::MonthNumber),
        ("MI", &|_| E::Minute),
        ("SSSSS", &|_| E::SecondsPastMidnight),
        ("SS", &|_| E::Second),
        ("DDD", &|_| E::DayOfYear),
        ("DD", &|_| E::DayOfMonth),
        ("DAY", &E::DayName),
        ("DY", &E::DayAbbreviation),
        ("D", &|_| E::DayOfWeek),
        ("J", &|_| E::JulianDay),
        ("RRRR", &|_| E::RoundedYear(4)),
        ("RR", &|_| E::RoundedYear(2)),
        ("X", &|_| E::RadixCharacter),
        ("YYYY", &|_| E::Year(4)),
        ("YYY", &|_| E::Year(3)),
        ("YY", &|_| E::Year(2)),
        ("Y", &|_| E::Year(1)),
    ];
    for (word, make) in table {
        if starts_with(upper, word) {
            let length = word.chars().count();
            return Ok(Scanned::Element(make(spelling(length)), length));
        }
    }
    Err(not_recognized())
}

fn input_only_refusal(element: &DatetimeElement) -> Option<&'static str> {
    use DatetimeElement as E;
    Some(match element {
        E::DayOfWeek => "D",
        E::DayName(_) => "DAY",
        E::DayAbbreviation(_) => "DY",
        E::DayOfYear => "DDD",
        E::JulianDay => "J",
        E::SecondsPastMidnight => "SSSSS",
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    Fraction,
    Meridian,
}

fn field_of(element: &DatetimeElement) -> Option<Field> {
    use DatetimeElement as E;
    Some(match element {
        E::Year(_) | E::RoundedYear(_) => Field::Year,
        E::MonthNumber | E::MonthAbbreviation(_) | E::MonthName(_) => Field::Month,
        E::DayOfMonth => Field::Day,
        E::Hour12 | E::Hour24 => Field::Hour,
        E::Minute => Field::Minute,
        E::Second => Field::Second,
        E::FractionalSeconds(_) => Field::Fraction,
        E::Meridian { .. } => Field::Meridian,
        _ => return None,
    })
}

impl DatetimeModel {
    pub(crate) fn parse(text: &str, purpose: ModelUse) -> SqlResult<Self> {
        if text.len() > MAX_MODEL_BYTES {
            return Err(date_error(
                1801,
                "date format is too long for internal buffer",
            ));
        }
        let chars: Vec<char> = text.chars().collect();
        let upper: Vec<char> = chars.iter().map(char::to_ascii_uppercase).collect();
        let mut elements = Vec::new();
        let mut seen: Vec<Field> = Vec::new();
        let mut at = 0;
        while at < chars.len() {
            let (element, length) = match element_at(&chars[at..], &upper[at..])? {
                Scanned::Element(element, length) => (element, length),
                Scanned::ToCharOnly if purpose == ModelUse::Output => {
                    return Err(not_recognized());
                }
                Scanned::ToCharOnly => {
                    return Err(date_error(
                        1820,
                        "format code cannot appear in date input format",
                    ));
                }
            };
            at += length;
            if purpose != ModelUse::Output {
                if let Some(name) = input_only_refusal(&element) {
                    return Err(date_error(
                        3001,
                        &format!(
                            "unimplemented feature: format element {name} in TO_DATE and TO_TIMESTAMP"
                        ),
                    ));
                }
                if purpose == ModelUse::DateInput
                    && matches!(element, DatetimeElement::FractionalSeconds(_))
                {
                    return Err(not_recognized());
                }
                if let Some(field) = field_of(&element) {
                    if seen.contains(&field) {
                        return Err(date_error(1810, "format code appears twice"));
                    }
                    seen.push(field);
                }
            }
            elements.push(element);
        }
        Ok(Self { elements })
    }

    pub(crate) fn has_fraction(&self) -> bool {
        self.elements
            .iter()
            .any(|element| matches!(element, DatetimeElement::FractionalSeconds(_)))
    }

    fn output_bound(&self) -> usize {
        self.elements
            .iter()
            .map(|element| match element {
                DatetimeElement::Text(text) => text.len(),
                DatetimeElement::Punctuation(character) => character.len_utf8(),
                _ => MAX_ELEMENT_BYTES,
            })
            .fold(0usize, usize::saturating_add)
    }
}

thread_local! {
    static MODELS: RefCell<Vec<(String, ModelUse, Rc<DatetimeModel>)>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn cached_model(text: &str, purpose: ModelUse) -> SqlResult<Rc<DatetimeModel>> {
    let found = MODELS.with(|models| {
        models
            .borrow()
            .iter()
            .find(|(cached, cached_purpose, _)| *cached_purpose == purpose && cached == text)
            .map(|(_, _, model)| Rc::clone(model))
    });
    if let Some(model) = found {
        return Ok(model);
    }
    let model = Rc::new(DatetimeModel::parse(text, purpose)?);
    MODELS.with(|models| {
        let mut models = models.borrow_mut();
        if models.len() >= CACHED_MODELS {
            models.remove(0);
        }
        models.push((text.to_owned(), purpose, Rc::clone(&model)));
    });
    Ok(model)
}

pub(crate) fn clear_model_cache() {
    MODELS.with(|models| models.borrow_mut().clear());
}

#[cfg(test)]
fn cached_models() -> usize {
    MODELS.with(|models| models.borrow().len())
}

fn spelled(name: &str, spelling: Spelling) -> String {
    match spelling {
        Spelling::Upper => name.to_owned(),
        Spelling::Lower => name.to_ascii_lowercase(),
        Spelling::Capitalized => {
            let mut out = String::with_capacity(name.len());
            for (index, character) in name.chars().enumerate() {
                out.push(if index == 0 {
                    character
                } else {
                    character.to_ascii_lowercase()
                });
            }
            out
        }
    }
}

fn push_number(out: &mut String, value: i64, width: usize, fill: bool) {
    if fill {
        out.push_str(&value.to_string());
    } else {
        out.push_str(&format!("{value:0width$}"));
    }
}

fn push_name(out: &mut String, name: &str, spelling: Spelling, pad: bool) {
    let shown = spelled(name, spelling);
    let width = shown.chars().count();
    out.push_str(&shown);
    if pad && width < NAME_WIDTH {
        out.extend(std::iter::repeat_n(' ', NAME_WIDTH - width));
    }
}

pub(crate) fn format_datetime(
    local: i64,
    model: &DatetimeModel,
    precision: Option<u8>,
) -> SqlResult<String> {
    use DatetimeElement as E;
    crate::oracle_functions::within_limit(model.output_bound())?;
    let days = local.div_euclid(MICROS_PER_DAY);
    let of_day = local.rem_euclid(MICROS_PER_DAY);
    let (year, month, day) = datetime::civil_from_days(days);
    if !(1..=9999).contains(&year) {
        return Err(year_out_of_range());
    }
    let hour = of_day / (3600 * MICROS_PER_SECOND);
    let minute = of_day / (60 * MICROS_PER_SECOND) % 60;
    let second = of_day / MICROS_PER_SECOND % 60;
    let micros = of_day % MICROS_PER_SECOND;
    let weekday = (days + 6).rem_euclid(7) as usize;
    let mut out = String::new();
    let mut fill = false;
    for element in &model.elements {
        match element {
            E::Text(text) => out.push_str(text),
            E::Punctuation(character) => out.push(*character),
            E::FillMode => fill = !fill,
            E::ExactMode => {}
            E::Meridian { dotted, spelling } => {
                let text = match (hour < 12, dotted) {
                    (true, false) => "AM",
                    (false, false) => "PM",
                    (true, true) => "A.M.",
                    (false, true) => "P.M.",
                };
                out.push_str(&if *spelling == Spelling::Lower {
                    text.to_ascii_lowercase()
                } else {
                    text.to_owned()
                });
            }
            E::DayOfMonth => push_number(&mut out, day, 2, fill),
            E::FractionalSeconds(digits) => {
                let digits = match (digits, precision) {
                    (Some(digits), _) => usize::from(*digits),
                    (None, Some(0)) => return Err(not_recognized()),
                    (None, Some(precision)) => usize::from(precision),
                    (None, None) => 6,
                };
                let six = format!("{micros:06}");
                if digits <= 6 {
                    out.push_str(&six[..digits]);
                } else {
                    out.push_str(&six);
                    out.extend(std::iter::repeat_n('0', digits - 6));
                }
            }
            E::Hour12 => push_number(
                &mut out,
                if hour % 12 == 0 { 12 } else { hour % 12 },
                2,
                fill,
            ),
            E::Hour24 => push_number(&mut out, hour, 2, fill),
            E::Minute => push_number(&mut out, minute, 2, fill),
            E::Second => push_number(&mut out, second, 2, fill),
            E::MonthNumber => push_number(&mut out, month, 2, fill),
            E::MonthAbbreviation(spelling) => {
                push_name(&mut out, &MONTHS[month as usize - 1][..3], *spelling, false);
            }
            E::MonthName(spelling) => {
                push_name(&mut out, MONTHS[month as usize - 1], *spelling, !fill);
            }
            E::Year(width) | E::RoundedYear(width) => {
                let width = usize::from(*width);
                let shown = year % 10i64.pow(width as u32);
                push_number(&mut out, shown, width, fill);
            }
            E::RadixCharacter => out.push('.'),
            E::DayOfWeek => push_number(&mut out, weekday as i64 + 1, 1, fill),
            E::DayName(spelling) => push_name(&mut out, DAYS[weekday], *spelling, !fill),
            E::DayAbbreviation(spelling) => {
                push_name(&mut out, &DAYS[weekday][..3], *spelling, false);
            }
            E::DayOfYear => {
                let first = datetime::days_from_civil(year, 1, 1);
                push_number(&mut out, days - first + 1, 3, fill);
            }
            E::JulianDay => push_number(&mut out, days + JULIAN_DAY_OF_EPOCH, 7, fill),
            E::SecondsPastMidnight => {
                push_number(&mut out, of_day / MICROS_PER_SECOND, 5, fill);
            }
        }
    }
    Ok(out)
}

#[derive(Default)]
struct Fields {
    year: Option<i64>,
    month: Option<i64>,
    day: Option<i64>,
    hour: Option<i64>,
    minute: Option<i64>,
    second: Option<i64>,
    nanos: i64,
    afternoon: Option<bool>,
}

struct Reader<'a> {
    input: &'a [char],
    at: usize,
}

impl Reader<'_> {
    fn at_end(&self) -> bool {
        self.at >= self.input.len()
    }

    fn peek(&self) -> Option<char> {
        self.input.get(self.at).copied()
    }

    fn skip_blanks(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.at += 1;
        }
    }

    fn digit_run(&self) -> usize {
        self.input[self.at..]
            .iter()
            .take_while(|character| character.is_ascii_digit())
            .count()
    }

    fn digits(&mut self, most: usize, exact: Option<usize>) -> SqlResult<Option<(i64, usize)>> {
        let count = self.digit_run().min(most);
        if count == 0 {
            return Ok(None);
        }
        if let Some(wanted) = exact
            && count != wanted
        {
            return Err(date_error(
                1862,
                "the numeric value does not match the length of the format item",
            ));
        }
        let mut value = 0i64;
        for character in &self.input[self.at..self.at + count] {
            value = value * 10 + i64::from(character.to_digit(10).unwrap_or(0));
        }
        self.at += count;
        Ok(Some((value, count)))
    }

    fn word(&mut self, word: &str) -> bool {
        let length = word.chars().count();
        let matches = self.input.len() >= self.at + length
            && word
                .chars()
                .zip(&self.input[self.at..])
                .all(|(wanted, found)| found.to_ascii_uppercase() == wanted);
        if matches {
            self.at += length;
        }
        matches
    }

    fn month_name(&mut self) -> Option<i64> {
        let mut try_names = |full: bool| {
            MONTHS.iter().position(|name| {
                let name = if full { name } else { &name[..3] };
                self.word(name)
            })
        };
        try_names(true)
            .or_else(|| try_names(false))
            .map(|index| index as i64 + 1)
    }
}

fn is_time_element(element: &DatetimeElement) -> bool {
    use DatetimeElement as E;
    matches!(
        element,
        E::Hour12
            | E::Hour24
            | E::Minute
            | E::Second
            | E::FractionalSeconds(_)
            | E::Meridian { .. }
            | E::Punctuation(_)
            | E::RadixCharacter
            | E::FillMode
            | E::ExactMode
            | E::Text(_)
    )
}

fn round_year(two_digits: i64, now_year: i64) -> i64 {
    let century = now_year.div_euclid(100) * 100;
    let current = now_year.rem_euclid(100);
    match (two_digits < 50, current < 50) {
        (true, false) => century + 100 + two_digits,
        (false, true) => century - 100 + two_digits,
        _ => century + two_digits,
    }
}

fn checked(
    value: i64,
    range: std::ops::RangeInclusive<i64>,
    code: u16,
    message: &str,
) -> SqlResult<i64> {
    if range.contains(&value) {
        Ok(value)
    } else {
        Err(date_error(code, message))
    }
}

fn read_number(
    reader: &mut Reader<'_>,
    most: usize,
    fill: bool,
    exact: bool,
) -> SqlResult<Option<(i64, usize)>> {
    let wanted = (exact && !fill).then_some(most);
    reader.digits(most, wanted)
}

fn read_element(
    element: &DatetimeElement,
    reader: &mut Reader<'_>,
    fields: &mut Fields,
    modes: (bool, bool),
    now_year: i64,
) -> SqlResult<()> {
    use DatetimeElement as E;
    let (fill, exact) = modes;
    let number = |reader: &mut Reader<'_>, most: usize| -> SqlResult<i64> {
        match read_number(reader, most, fill, exact)? {
            Some((value, _)) => Ok(value),
            None => Err(not_numeric()),
        }
    };
    match element {
        E::FillMode | E::ExactMode => {}
        E::Punctuation(character) => {
            if exact {
                if reader.peek() != Some(*character) {
                    return Err(literal_mismatch());
                }
                reader.at += 1;
            } else if *character != ' '
                && reader
                    .peek()
                    .is_some_and(|found| !found.is_alphanumeric() && !found.is_whitespace())
            {
                reader.at += 1;
            }
        }
        E::RadixCharacter => {
            if reader.peek() == Some('.') {
                reader.at += 1;
            } else if exact {
                return Err(literal_mismatch());
            }
        }
        E::Text(text) => {
            if !reader.word(&text.to_uppercase()) {
                return Err(literal_mismatch());
            }
        }
        E::Meridian { .. } => {
            let afternoon = if reader.word("A.M.") || reader.word("AM") {
                false
            } else if reader.word("P.M.") || reader.word("PM") {
                true
            } else {
                return Err(date_error(1855, "AM/A.M. or PM/P.M. required"));
            };
            fields.afternoon = Some(afternoon);
        }
        E::DayOfMonth => {
            let day = number(reader, 2)?;
            fields.day = Some(checked(
                day,
                1..=31,
                1847,
                "day of month must be between 1 and last day of month",
            )?);
        }
        E::MonthNumber => {
            let month = match read_number(reader, 2, fill, exact)? {
                Some((month, _)) => month,
                None if !exact => reader.month_name().ok_or_else(not_a_month)?,
                None => return Err(not_numeric()),
            };
            fields.month = Some(checked(month, 1..=12, 1843, "not a valid month")?);
        }
        E::MonthAbbreviation(_) | E::MonthName(_) => {
            let full_first = matches!(element, E::MonthName(_));
            let month = if exact {
                let found = MONTHS
                    .iter()
                    .position(|name| reader.word(if full_first { name } else { &name[..3] }));
                found.map(|index| index as i64 + 1)
            } else {
                reader.month_name()
            };
            fields.month = Some(month.ok_or_else(not_a_month)?);
        }
        E::Hour12 => {
            let hour = number(reader, 2)?;
            fields.hour = Some(checked(
                hour,
                1..=12,
                1849,
                "hour must be between 1 and 12",
            )?);
        }
        E::Hour24 => {
            let hour = number(reader, 2)?;
            fields.hour = Some(checked(
                hour,
                0..=23,
                1850,
                "hour must be between 0 and 23",
            )?);
        }
        E::Minute => {
            let minute = number(reader, 2)?;
            fields.minute = Some(checked(
                minute,
                0..=59,
                1851,
                "minutes must be between 0 and 59",
            )?);
        }
        E::Second => {
            let second = number(reader, 2)?;
            fields.second = Some(checked(
                second,
                0..=59,
                1852,
                "seconds must be between 0 and 59",
            )?);
        }
        E::FractionalSeconds(digits) => {
            let most = usize::from(digits.unwrap_or(9));
            let Some((value, count)) = reader.digits(most, None)? else {
                return Err(not_numeric());
            };
            fields.nanos = value * 10i64.pow(9 - count as u32);
        }
        E::Year(width) | E::RoundedYear(width) => {
            let rounded = matches!(element, E::RoundedYear(_));
            let width = usize::from(*width);
            let run = reader.digit_run();
            let full = !exact && (width == 2 && run > 2);
            let most = if full { 4 } else { width };
            let Some((value, count)) = read_number(reader, most, fill, exact && !full)? else {
                return Err(not_numeric());
            };
            let year = if rounded && count <= 2 {
                round_year(value, now_year)
            } else if rounded || full || width == 4 {
                value
            } else {
                let unit = 10i64.pow(width as u32);
                now_year.div_euclid(unit) * unit + value
            };
            if !(1..=9999).contains(&year) {
                return Err(year_out_of_range());
            }
            fields.year = Some(year);
        }
        E::DayOfWeek
        | E::DayName(_)
        | E::DayAbbreviation(_)
        | E::DayOfYear
        | E::JulianDay
        | E::SecondsPastMidnight => return Err(not_recognized()),
    }
    Ok(())
}

pub(crate) fn parse_datetime(text: &str, model: &DatetimeModel, now: i64) -> SqlResult<i64> {
    if text.len() > MAX_INPUT_BYTES {
        return Err(input_left_over());
    }
    let input: Vec<char> = text.chars().collect();
    let mut reader = Reader {
        input: &input,
        at: 0,
    };
    let (now_year, now_month, _) = datetime::civil_from_days(now.div_euclid(MICROS_PER_DAY));
    let mut fields = Fields::default();
    let mut fill = false;
    let mut exact = false;
    for (index, element) in model.elements.iter().enumerate() {
        match element {
            DatetimeElement::FillMode => {
                fill = !fill;
                continue;
            }
            DatetimeElement::ExactMode => {
                exact = !exact;
                continue;
            }
            _ => {}
        }
        if !exact {
            reader.skip_blanks();
        }
        if reader.at_end() {
            if model.elements[index..].iter().all(is_time_element) {
                break;
            }
            return Err(input_too_short());
        }
        read_element(element, &mut reader, &mut fields, (fill, exact), now_year)?;
    }
    reader.skip_blanks();
    if !reader.at_end() {
        return Err(input_left_over());
    }
    assemble(&fields, now_year, now_month)
}

fn assemble(fields: &Fields, now_year: i64, now_month: i64) -> SqlResult<i64> {
    let year = fields.year.unwrap_or(now_year);
    let month = fields.month.unwrap_or(now_month);
    let day = fields.day.unwrap_or(1);
    if !(1..=9999).contains(&year) {
        return Err(year_out_of_range());
    }
    if day > datetime::days_in_month(year, month) {
        return Err(date_error(1839, "date not valid for month specified"));
    }
    let mut hour = fields.hour.unwrap_or(0);
    if let Some(afternoon) = fields.afternoon {
        hour = hour % 12 + if afternoon { 12 } else { 0 };
    }
    let seconds = datetime::days_from_civil(year, month, day) * 86_400
        + hour * 3600
        + fields.minute.unwrap_or(0) * 60
        + fields.second.unwrap_or(0);
    let micros = i128::from(seconds) * i128::from(MICROS_PER_SECOND)
        + i128::from((fields.nanos + NANOS_PER_MICRO / 2) / NANOS_PER_MICRO);
    datetime::checked_timestamp(micros)
}

fn local_now() -> SqlResult<i64> {
    datetime::utc_to_local(datetime::statement_now())
}

pub(crate) fn read_by_model(text: &str, format: &str, purpose: ModelUse) -> SqlResult<i64> {
    let model = cached_model(format, purpose)?;
    parse_datetime(text, &model, local_now()?)
}

pub(crate) fn strict_iso(text: &str, with_time: bool) -> Option<i64> {
    let bytes = text.trim().as_bytes();
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = bytes.get(range)?;
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        part.iter().try_fold(0i64, |value, digit| {
            Some(value * 10 + i64::from(digit - b'0'))
        })
    };
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    if !(1..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || day < 1
        || day > datetime::days_in_month(year, month)
    {
        return None;
    }
    let mut micros = datetime::days_from_civil(year, month, day) * MICROS_PER_DAY;
    if bytes.len() == 10 {
        return Some(micros);
    }
    if !with_time || bytes.len() < 19 || !matches!(bytes[10], b' ' | b'T') {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    micros += (hour * 3600 + minute * 60 + second) * MICROS_PER_SECOND;
    if bytes.len() == 19 {
        return Some(micros);
    }
    if bytes[19] != b'.' || bytes.len() == 20 || bytes.len() > 29 {
        return None;
    }
    let digits = &bytes[20..];
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut nanos = 0i64;
    for index in 0..9 {
        nanos = nanos * 10 + digits.get(index).map_or(0, |digit| i64::from(digit - b'0'));
    }
    Some(micros + (nanos + NANOS_PER_MICRO / 2) / NANOS_PER_MICRO)
}

fn split_offset(text: &str) -> (&str, Option<i32>) {
    let trimmed = text.trim();
    let Some(at) = trimmed.rfind([' ']) else {
        return (trimmed, None);
    };
    let (body, zone) = (trimmed[..at].trim_end(), trimmed[at + 1..].trim());
    let Some(sign) = zone
        .chars()
        .next()
        .filter(|sign| *sign == '+' || *sign == '-')
    else {
        return (trimmed, None);
    };
    let (hours, minutes) = zone[1..].split_once(':').unwrap_or((&zone[1..], "0"));
    let (Ok(hours), Ok(minutes)) = (hours.parse::<i32>(), minutes.parse::<i32>()) else {
        return (trimmed, None);
    };
    if !(0..=14).contains(&hours) || !(0..=59).contains(&minutes) || body.len() < 10 {
        return (trimmed, None);
    }
    let seconds = (hours * 3600 + minutes * 60) * if sign == '-' { -1 } else { 1 };
    (body, Some(seconds))
}

pub(crate) fn date_literal(text: &str) -> SqlResult<Value> {
    strict_iso(text, false)
        .map(Value::Timestamp)
        .ok_or_else(literal_mismatch)
}

pub(crate) fn timestamp_literal(text: &str) -> SqlResult<Value> {
    let (body, offset) = split_offset(text);
    let local = strict_iso(body, true).ok_or_else(literal_mismatch)?;
    match offset {
        None => Ok(Value::Timestamp(local)),
        Some(offset) => datetime::checked_timestamp(
            i128::from(local) - i128::from(offset) * i128::from(MICROS_PER_SECOND),
        )
        .map(Value::TimestampTz),
    }
}

pub(crate) fn literal_has_offset(text: &str) -> bool {
    split_offset(text).1.is_some()
}

fn check_date_language(nlsparam: Option<&Value>) -> SqlResult<()> {
    let Some(value) = nlsparam else {
        return Ok(());
    };
    let Value::Text(text) = value else {
        return Ok(());
    };
    let (name, setting) = crate::oracle_nls::parameter_assignment(text)?;
    if name != "nls_date_language" {
        return Err(crate::oracle_nls::invalid_parameter_string());
    }
    crate::oracle_nls::require_american(&setting)
}

fn source_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Text(text) | Value::Bpchar(text) => Some(text.clone()),
        other => crate::oracle_functions::text_of(other),
    }
}

fn to_datetime(values: &[Value], purpose: ModelUse) -> SqlResult<Value> {
    let truncated = |micros: i64| {
        if purpose == ModelUse::DateInput {
            micros - micros.rem_euclid(MICROS_PER_SECOND)
        } else {
            micros
        }
    };
    match values.first() {
        Some(Value::Timestamp(micros)) if values.len() == 1 => {
            return Ok(Value::Timestamp(truncated(*micros)));
        }
        Some(Value::TimestampTz(micros)) if values.len() == 1 => {
            return datetime::utc_to_local(*micros).map(|local| Value::Timestamp(truncated(local)));
        }
        _ => {}
    }
    let Some(text) = values.first().and_then(source_text) else {
        return Ok(Value::Null);
    };
    if values.get(1).is_some_and(Value::is_null) || values.get(2).is_some_and(Value::is_null) {
        return Ok(Value::Null);
    }
    check_date_language(values.get(2))?;
    let format = match values.get(1).and_then(source_text) {
        Some(format) => format,
        None => {
            let nls = crate::oracle_nls::current();
            if purpose == ModelUse::DateInput {
                nls.date_format().to_owned()
            } else {
                nls.timestamp_format().to_owned()
            }
        }
    };
    read_by_model(&text, &format, purpose).map(|micros| Value::Timestamp(truncated(micros)))
}

pub(crate) fn to_date(values: &[Value]) -> SqlResult<Value> {
    to_datetime(values, ModelUse::DateInput)
}

pub(crate) fn to_timestamp(values: &[Value]) -> SqlResult<Value> {
    to_datetime(values, ModelUse::TimestampInput)
}

fn nls_output(declared: Option<SqlType>, value: &Value) -> (String, Option<u8>) {
    let nls = crate::oracle_nls::current();
    match (value, declared) {
        (Value::TimestampTz(_), Some(SqlType::TimestampTz(precision))) => {
            (nls.timestamp_tz_format().to_owned(), precision.or(Some(6)))
        }
        (Value::TimestampTz(_), _) => (nls.timestamp_tz_format().to_owned(), None),
        (Value::Date(_), _) | (_, Some(SqlType::Timestamp(Some(0)) | SqlType::Date)) => {
            (nls.date_format().to_owned(), Some(0))
        }
        (_, Some(SqlType::Timestamp(Some(precision)))) => {
            (nls.timestamp_format().to_owned(), Some(precision))
        }
        (_, Some(SqlType::Timestamp(None))) => (nls.timestamp_format().to_owned(), None),
        _ => (nls.date_format().to_owned(), None),
    }
}

fn local_of(value: &Value) -> SqlResult<Option<i64>> {
    Ok(Some(match value {
        Value::Timestamp(micros) => *micros,
        Value::TimestampTz(micros) => datetime::utc_to_local(*micros)?,
        Value::Date(days) => datetime::date_to_timestamp(*days)?,
        _ => return Ok(None),
    }))
}

pub(crate) fn nls_text(value: &Value, declared: Option<SqlType>) -> SqlResult<Option<String>> {
    let Some(local) = local_of(value)? else {
        return Ok(None);
    };
    let (format, precision) = nls_output(declared, value);
    let model = cached_model(&format, ModelUse::Output)?;
    format_datetime(local, &model, precision).map(Some)
}

pub(crate) fn to_char_datetime(values: &[Value], declared: Option<SqlType>) -> SqlResult<Value> {
    if values.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    check_date_language(values.get(2))?;
    let Some(local) = local_of(&values[0])? else {
        return Ok(Value::Null);
    };
    let (default_format, precision) = nls_output(declared, &values[0]);
    let format = match values.get(1).and_then(source_text) {
        Some(format) => format,
        None => default_format,
    };
    let model = cached_model(&format, ModelUse::Output)?;
    let text = format_datetime(local, &model, precision)?;
    Ok(if text.is_empty() {
        Value::Null
    } else {
        Value::Text(text)
    })
}

fn date_of(value: &Value) -> SqlResult<Option<i64>> {
    match value {
        Value::Text(text) | Value::Bpchar(text) => {
            let format = crate::oracle_nls::current().date_format().to_owned();
            read_by_model(text, &format, ModelUse::DateInput).map(Some)
        }
        other => local_of(other),
    }
}

pub(crate) fn add_months(date: &Value, months: Option<i64>) -> SqlResult<Value> {
    let (Some(local), Some(months)) = (date_of(date)?, months) else {
        return Ok(Value::Null);
    };
    let days = local.div_euclid(MICROS_PER_DAY);
    let of_day = local.rem_euclid(MICROS_PER_DAY);
    let (year, month, day) = datetime::civil_from_days(days);
    let total = (year * 12 + month - 1)
        .checked_add(months)
        .ok_or_else(year_out_of_range)?;
    let (new_year, new_month) = (total.div_euclid(12), total.rem_euclid(12) + 1);
    if !(1..=9999).contains(&new_year) {
        return Err(year_out_of_range());
    }
    let last = datetime::days_in_month(new_year, new_month);
    let new_day = if day == datetime::days_in_month(year, month) {
        last
    } else {
        day.min(last)
    };
    let micros = datetime::days_from_civil(new_year, new_month, new_day) * MICROS_PER_DAY + of_day;
    Ok(Value::Timestamp(
        micros - micros.rem_euclid(MICROS_PER_SECOND),
    ))
}

pub(crate) fn last_day(date: &Value) -> SqlResult<Value> {
    let Some(local) = date_of(date)? else {
        return Ok(Value::Null);
    };
    let days = local.div_euclid(MICROS_PER_DAY);
    let of_day = local.rem_euclid(MICROS_PER_DAY);
    let (year, month, _) = datetime::civil_from_days(days);
    let last = datetime::days_from_civil(year, month, datetime::days_in_month(year, month));
    let micros = last * MICROS_PER_DAY + of_day;
    Ok(Value::Timestamp(
        micros - micros.rem_euclid(MICROS_PER_SECOND),
    ))
}

pub(crate) fn truncated_date(date: &Value, unit: Option<&str>) -> SqlResult<Value> {
    let Some(local) = local_of(date)? else {
        return Ok(Value::Null);
    };
    let days = local.div_euclid(MICROS_PER_DAY);
    let of_day = local.rem_euclid(MICROS_PER_DAY);
    let (year, month, _) = datetime::civil_from_days(days);
    let unit = unit.unwrap_or("DD").trim().to_ascii_uppercase();
    let micros = match unit.as_str() {
        "YYYY" | "YEAR" | "YYY" | "YY" | "Y" | "SYYYY" | "SYEAR" => {
            datetime::days_from_civil(year, 1, 1) * MICROS_PER_DAY
        }
        "MM" | "MON" | "MONTH" => datetime::days_from_civil(year, month, 1) * MICROS_PER_DAY,
        "DD" | "DDD" | "J" => days * MICROS_PER_DAY,
        "HH" | "HH12" | "HH24" => {
            days * MICROS_PER_DAY + of_day - of_day % (3600 * MICROS_PER_SECOND)
        }
        "MI" => days * MICROS_PER_DAY + of_day - of_day % (60 * MICROS_PER_SECOND),
        _ => return Err(not_recognized()),
    };
    Ok(Value::Timestamp(micros))
}

#[cfg(test)]
mod tests {
    use super::{
        DatetimeModel, ModelUse, cached_model, cached_models, clear_model_cache, format_datetime,
        parse_datetime, round_year, strict_iso,
    };
    use crate::datetime::{MICROS_PER_DAY, MICROS_PER_SECOND, days_from_civil};

    fn at(year: i64, month: i64, day: i64, hour: i64, minute: i64, second: i64) -> i64 {
        days_from_civil(year, month, day) * MICROS_PER_DAY
            + (hour * 3600 + minute * 60 + second) * MICROS_PER_SECOND
    }

    fn now() -> i64 {
        at(2026, 10, 29, 9, 30, 0)
    }

    fn read(text: &str, format: &str) -> Result<i64, u16> {
        let model = DatetimeModel::parse(format, ModelUse::TimestampInput)
            .map_err(|error| error.vendor_code().unwrap_or(0))?;
        parse_datetime(text, &model, now()).map_err(|error| error.vendor_code().unwrap_or(0))
    }

    fn write(micros: i64, format: &str, precision: Option<u8>) -> Result<String, u16> {
        let model = DatetimeModel::parse(format, ModelUse::Output)
            .map_err(|error| error.vendor_code().unwrap_or(0))?;
        format_datetime(micros, &model, precision).map_err(|error| error.vendor_code().unwrap_or(0))
    }

    #[test]
    fn every_element_writes_as_the_format_models_page_describes() {
        let stamp = at(1999, 12, 1, 22, 5, 9) + 123_456;
        assert_eq!(
            write(stamp, "DD-MON-YYYY HH24:MI:SSxFF", Some(6)).as_deref(),
            Ok("01-DEC-1999 22:05:09.123456")
        );
        assert_eq!(
            write(stamp, "DD-MON-RR HH.MI.SSXFF AM", None).as_deref(),
            Ok("01-DEC-99 10.05.09.123456 PM")
        );
        assert_eq!(write(stamp, "FF3", None).as_deref(), Ok("123"));
        assert_eq!(write(stamp, "FF9", None).as_deref(), Ok("123456000"));
        assert_eq!(
            write(stamp, "Day day DAY", None).as_deref(),
            Ok("Wednesday wednesday WEDNESDAY")
        );
        assert_eq!(write(stamp, "Month-", None).as_deref(), Ok("December -"));
        assert_eq!(
            write(stamp, "fmMonth DD, YYYY", None).as_deref(),
            Ok("December 1, 1999")
        );
        assert_eq!(
            write(stamp, "Mon mon DY dy", None).as_deref(),
            Ok("Dec dec WED wed")
        );
        assert_eq!(
            write(stamp, "D DDD J SSSSS", None).as_deref(),
            Ok("4 335 2451514 79509")
        );
        assert_eq!(
            write(stamp, "Y YY YYY a.m. P.M.", None).as_deref(),
            Ok("9 99 999 p.m. P.M.")
        );
        assert_eq!(write(stamp, "\"day \"DD", None).as_deref(), Ok("day 01"));
        assert_eq!(write(stamp, "FMDD FMDD", None).as_deref(), Ok("1 01"));
        assert_eq!(write(stamp, "HH24:MI:SS.FF", Some(0)), Err(1821));
        assert_eq!(write(stamp, "FF", Some(3)).as_deref(), Ok("123"));
    }

    #[test]
    fn unknown_and_unsupported_elements_are_refused() {
        assert_eq!(write(0, "QQ", None), Err(1821));
        assert_eq!(write(0, "DS", None), Err(1821));
        assert_eq!(write(0, "IW", None), Err(1821));
        assert_eq!(read("1", "IW"), Err(1820));
        assert_eq!(read("1", "J"), Err(3001));
        assert_eq!(read("1", "DDD"), Err(3001));
        assert_eq!(read("01 01", "DD DD"), Err(1810));
        assert!(DatetimeModel::parse("HH24:MI:SS.FF", ModelUse::DateInput).is_err());
        assert_eq!(
            DatetimeModel::parse(&"DD".repeat(501), ModelUse::Output)
                .map_err(|error| error.vendor_code()),
            Err(Some(1801))
        );
        assert_eq!(write(0, "\"open", None), Err(1821));
    }

    #[test]
    fn the_fx_table_matches_and_fails_as_documented() {
        let expected = at(1998, 1, 15, 0, 0, 0);
        assert_eq!(read("15/ JAN /1998", "DD-MON-YYYY"), Ok(expected));
        assert!(read(" 15! JAN % /1998", "DD-MON-YYYY").is_err());
        assert!(read("15/JAN/1998", "FXDD-MON-YYYY").is_err());
        assert_eq!(read("15-JAN-1998", "FXDD-MON-YYYY"), Ok(expected));
        assert!(read("1-JAN-1998", "FXDD-MON-YYYY").is_err());
        assert_eq!(
            read("01-JAN-1998", "FXDD-MON-YYYY"),
            Ok(at(1998, 1, 1, 0, 0, 0))
        );
        assert_eq!(
            read("1-JAN-1998", "FXFMDD-MON-YYYY"),
            Ok(at(1998, 1, 1, 0, 0, 0))
        );
    }

    #[test]
    fn rr_and_yy_follow_the_century_rules() {
        assert_eq!(round_year(98, 2026), 1998);
        assert_eq!(round_year(17, 2026), 2017);
        assert_eq!(round_year(17, 1998), 2017);
        assert_eq!(round_year(98, 1998), 1998);
        assert_eq!(
            read("27-OCT-98", "DD-MON-RR"),
            Ok(at(1998, 10, 27, 0, 0, 0))
        );
        assert_eq!(
            read("27-OCT-98", "DD-MON-YY"),
            Ok(at(2098, 10, 27, 0, 0, 0))
        );
        assert_eq!(
            read("27-OCT-1998", "DD-MON-RR"),
            Ok(at(1998, 10, 27, 0, 0, 0))
        );
        assert_eq!(
            read("27-OCT-1998", "DD-MON-YY"),
            Ok(at(1998, 10, 27, 0, 0, 0))
        );
    }

    #[test]
    fn alternatives_defaults_and_left_out_fields_follow_the_conversion_rules() {
        assert_eq!(
            read("15-JANUARY-1998", "DD-MM-YYYY"),
            Ok(at(1998, 1, 15, 0, 0, 0))
        );
        assert_eq!(
            read("15-January-1998", "DD-MON-YYYY"),
            Ok(at(1998, 1, 15, 0, 0, 0))
        );
        assert_eq!(
            read("15-JAN-1998", "DD-MONTH-YYYY"),
            Ok(at(1998, 1, 15, 0, 0, 0))
        );
        assert_eq!(read("15011998", "DD-MM-YYYY"), Ok(at(1998, 1, 15, 0, 0, 0)));
        assert_eq!(
            read("15-JAN-1998", "DD-MON-YYYY HH24:MI:SS"),
            Ok(at(1998, 1, 15, 0, 0, 0))
        );
        assert_eq!(read("1998", "YYYY"), Ok(at(1998, 10, 1, 0, 0, 0)));
        assert_eq!(read("15", "DD"), Ok(at(2026, 10, 15, 0, 0, 0)));
        assert_eq!(read("15-JAN", "DD-MON-YYYY"), Err(1840));
        assert_eq!(read("15-JAN-1998 x", "DD-MON-YYYY"), Err(1830));
        assert_eq!(
            read("17-06-2013", "dd-MM-yyyy"),
            Ok(at(2013, 6, 17, 0, 0, 0))
        );
        assert_eq!(
            read("January 15, 1989, 11:00 A.M.", "Month dd, YYYY, HH:MI A.M."),
            Ok(at(1989, 1, 15, 11, 0, 0))
        );
        assert_eq!(read("12:30 AM", "HH:MI AM"), Ok(at(2026, 10, 1, 0, 30, 0)));
        assert_eq!(read("12:30 PM", "HH:MI AM"), Ok(at(2026, 10, 1, 12, 30, 0)));
    }

    #[test]
    fn field_ranges_leap_days_and_years_are_checked() {
        assert_eq!(
            read("29-FEB-2024", "DD-MON-YYYY"),
            Ok(at(2024, 2, 29, 0, 0, 0))
        );
        assert_eq!(read("29-FEB-2023", "DD-MON-YYYY"), Err(1839));
        assert_eq!(read("32-JAN-2023", "DD-MON-YYYY"), Err(1847));
        assert_eq!(read("15-13-2023", "DD-MM-YYYY"), Err(1843));
        assert_eq!(read("15-XYZ-2023", "DD-MON-YYYY"), Err(1843));
        assert_eq!(read("24:00", "HH24:MI"), Err(1850));
        assert_eq!(read("13:00", "HH:MI"), Err(1849));
        assert_eq!(read("10:60", "HH24:MI"), Err(1851));
        assert_eq!(read("10:10:60", "HH24:MI:SS"), Err(1852));
        assert_eq!(read("10:10 XM", "HH:MI AM"), Err(1855));
        assert_eq!(read("AB", "DD"), Err(1858));
        assert_eq!(read("01-01-0000", "DD-MM-YYYY"), Err(1841));
        assert_eq!(read("01-01-0001", "DD-MM-YYYY"), Ok(at(1, 1, 1, 0, 0, 0)));
        assert_eq!(
            read("31-12-9999", "DD-MM-YYYY"),
            Ok(at(9999, 12, 31, 0, 0, 0))
        );
        assert_eq!(
            write(at(9999, 12, 31, 0, 0, 0), "YYYY", None).as_deref(),
            Ok("9999")
        );
        assert_eq!(write(at(10000, 1, 1, 0, 0, 0), "YYYY", None), Err(1841));
        assert_eq!(read("15-JAN-1998 \"x\"", "DD-MON-YYYY \"x\""), Err(1861));
    }

    #[test]
    fn nine_fraction_digits_round_to_microseconds() {
        assert_eq!(
            read(
                "17-JUN-03 10.15.30.123456789 PM",
                "DD-MON-RR HH.MI.SSXFF AM"
            ),
            Ok(at(2003, 6, 17, 22, 15, 30) + 123_457)
        );
        assert_eq!(
            read("01.5", "SS.FF"),
            Ok(at(2026, 10, 1, 0, 0, 1) + 500_000)
        );
    }

    #[test]
    fn strict_iso_accepts_only_iso_8601() {
        assert_eq!(
            strict_iso("2024-02-03", false),
            Some(at(2024, 2, 3, 0, 0, 0))
        );
        assert_eq!(
            strict_iso("2024-02-03 10:11:12.5", true),
            Some(at(2024, 2, 3, 10, 11, 12) + 500_000)
        );
        assert_eq!(
            strict_iso("2024-02-03T10:11:12", true),
            Some(at(2024, 2, 3, 10, 11, 12))
        );
        assert_eq!(strict_iso("now", true), None);
        assert_eq!(strict_iso("epoch", true), None);
        assert_eq!(strict_iso("2024-02-30", true), None);
        assert_eq!(strict_iso("2024-02-03 10:11:12", false), None);
        assert_eq!(strict_iso("0000-01-01", false), None);
    }

    #[test]
    fn models_are_cached_per_statement_and_cleared() {
        clear_model_cache();
        let first = cached_model("DD-MON-RR", ModelUse::Output).expect("model");
        let again = cached_model("DD-MON-RR", ModelUse::Output).expect("model");
        assert!(std::rc::Rc::ptr_eq(&first, &again));
        assert_eq!(cached_models(), 1);
        for index in 0..20 {
            let format = format!("DD{}", "-".repeat(index));
            cached_model(&format, ModelUse::Output).expect("model");
        }
        assert_eq!(cached_models(), 8);
        clear_model_cache();
        assert_eq!(cached_models(), 0);
    }

    #[test]
    fn seeded_random_models_and_inputs_never_panic() {
        let cases: usize = std::env::var("EGGDB_FUZZ_CASES")
            .ok()
            .and_then(|cases| cases.parse().ok())
            .unwrap_or(3000);
        let mut state: u64 = std::env::var("EGGDB_FUZZ_SEED")
            .ok()
            .and_then(|seed| seed.parse().ok())
            .unwrap_or(0x005e_ed42);
        let pieces = [
            "DD", "MM", "MON", "MONTH", "YYYY", "YY", "RR", "RRRR", "HH", "HH24", "MI", "SS", "FF",
            "FF9", "AM", "A.M.", "FM", "FX", "-", "/", " ", ".", "X", "\"t\"", "J", "DAY", "Q",
            "\"", "9", "é", "DDD", "SSSSS", "Y,YYY", "TZH",
        ];
        let inputs = [
            "15",
            "-",
            "JAN",
            "January",
            "1998",
            "98",
            " ",
            "12",
            "59",
            "AM",
            "P.M.",
            ".",
            "9999",
            "0",
            "x",
            "é",
            "/",
            "123456789",
        ];
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..cases {
            let mut model = String::new();
            for _ in 0..next() % 8 {
                model.push_str(pieces[next() as usize % pieces.len()]);
            }
            let mut input = String::new();
            for _ in 0..next() % 8 {
                input.push_str(inputs[next() as usize % inputs.len()]);
            }
            let micros = (next() % 3_700_000) as i64 * MICROS_PER_DAY
                + (next() % MICROS_PER_DAY as u64) as i64
                - 719_162 * MICROS_PER_DAY;
            for purpose in [
                ModelUse::DateInput,
                ModelUse::TimestampInput,
                ModelUse::Output,
            ] {
                if let Ok(parsed) = DatetimeModel::parse(&model, purpose) {
                    if purpose == ModelUse::Output {
                        if let Ok(text) = format_datetime(micros, &parsed, None) {
                            assert!(text.len() <= parsed.output_bound(), "{model}");
                        }
                    } else {
                        let _ = parse_datetime(&input, &parsed, now());
                    }
                }
            }
        }
    }
}
