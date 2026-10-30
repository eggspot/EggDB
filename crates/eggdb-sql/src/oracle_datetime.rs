use crate::datetime::{self, MICROS_PER_DAY, MICROS_PER_SECOND};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::oracle_numerals;
use crate::types::{SqlType, Value};
use std::cell::RefCell;
use std::rc::Rc;

pub(crate) const MAX_MODEL_BYTES: usize = 1000;
const MAX_INPUT_BYTES: usize = 4000;
const MAX_ELEMENT_BYTES: usize = 9;
const CACHED_MODELS: usize = 8;
const NAME_WIDTH: usize = 9;
const ROMAN_MONTH_WIDTH: usize = 4;
pub(crate) const JULIAN_DAY_OF_EPOCH: i64 = 2_451_545;
const MAX_JULIAN_DAY: i64 = 5_373_484;
const NANOS_PER_MICRO: i64 = 1000;
const FIRST_YEAR: i64 = -4713;
const LAST_YEAR: i64 = 9999;
const MAX_ZONE_HOURS: i64 = 15;
const SHORT_DATE: &str = "MM/DD/RRRR";
const LONG_DATE: &str = "fmDay, Month dd, yyyyfm";
const SHORT_TIME: &str = "HH:MI:SS AM";
const ROMAN_MONTHS: [&str; 12] = [
    "XII", "XI", "X", "IX", "VIII", "VII", "VI", "V", "IV", "III", "II", "I",
];
const UTC_NAMES: [&str; 13] = [
    "UTC",
    "GMT",
    "Z",
    "ZULU",
    "UCT",
    "UNIVERSAL",
    "ETC/UTC",
    "ETC/GMT",
    "ETC/UCT",
    "ETC/UNIVERSAL",
    "ETC/ZULU",
    "GMT0",
    "GREENWICH",
];

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ModelUse {
    DateInput,
    TimestampInput,
    ZonedInput,
    Output,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Spelling {
    Upper,
    Capitalized,
    Lower,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NumberSuffix {
    Ordinal,
    Spelled,
    SpelledOrdinal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DatetimeElement {
    Text(String),
    Punctuation(char),
    FillMode,
    ExactMode,
    Meridian {
        dotted: bool,
        spelling: Spelling,
    },
    Era {
        dotted: bool,
        spelling: Spelling,
    },
    DayOfMonth,
    FractionalSeconds(Option<u8>),
    Hour12,
    Hour24,
    Minute,
    Second,
    MonthNumber,
    MonthAbbreviation(Spelling),
    MonthName(Spelling),
    RomanMonth(Spelling),
    Year(u8),
    RoundedYear(u8),
    SignedYear,
    GroupedYear,
    SpelledYear {
        signed: bool,
        spelling: Spelling,
    },
    Century {
        signed: bool,
    },
    IsoYear(u8),
    IsoWeek,
    WeekOfYear,
    WeekOfMonth,
    Quarter,
    RadixCharacter,
    DayOfWeek,
    DayName(Spelling),
    DayAbbreviation(Spelling),
    DayOfYear,
    JulianDay,
    SecondsPastMidnight,
    ZoneHour,
    ZoneMinute,
    ZoneRegion,
    ZoneAbbreviation,
    Suffixed {
        number: Box<DatetimeElement>,
        suffix: NumberSuffix,
        spelling: Spelling,
        suffix_spelling: Spelling,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DatetimeModel {
    elements: Vec<DatetimeElement>,
}

fn date_error(code: u16, message: &str) -> SqlError {
    let state = match code {
        1841 | 1847 | 1839 | 1848 | 1849 | 1850 | 1851 | 1852 | 1853 | 1854 | 1874 | 1875 => {
            SqlState::DATETIME_FIELD_OVERFLOW
        }
        1801 => SqlState::PROGRAM_LIMIT_EXCEEDED,
        3001 => SqlState::FEATURE_NOT_SUPPORTED,
        1857 | 1882 => SqlState::INVALID_PARAMETER_VALUE,
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

pub(crate) fn year_in_range(year: i64) -> bool {
    (FIRST_YEAR..=LAST_YEAR).contains(&year)
}

pub(crate) fn not_a_day_of_the_week() -> SqlError {
    date_error(1846, "not a valid day of the week")
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

fn output_only_element() -> SqlError {
    date_error(1820, "format code cannot appear in date input format")
}

pub(crate) fn zone_region_not_found() -> SqlError {
    date_error(1882, "timezone region not found")
}

fn not_a_zone() -> SqlError {
    date_error(1857, "not a valid time zone")
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
    ShortForm(&'static str, usize),
}

type Maker = fn(Spelling) -> DatetimeElement;

const ELEMENTS: &[(&str, Maker)] = &[
    ("A.D.", |spelling| DatetimeElement::Era {
        dotted: true,
        spelling,
    }),
    ("A.M.", |spelling| DatetimeElement::Meridian {
        dotted: true,
        spelling,
    }),
    ("B.C.", |spelling| DatetimeElement::Era {
        dotted: true,
        spelling,
    }),
    ("P.M.", |spelling| DatetimeElement::Meridian {
        dotted: true,
        spelling,
    }),
    ("AD", |spelling| DatetimeElement::Era {
        dotted: false,
        spelling,
    }),
    ("AM", |spelling| DatetimeElement::Meridian {
        dotted: false,
        spelling,
    }),
    ("BC", |spelling| DatetimeElement::Era {
        dotted: false,
        spelling,
    }),
    ("PM", |spelling| DatetimeElement::Meridian {
        dotted: false,
        spelling,
    }),
    ("CC", |_| DatetimeElement::Century { signed: false }),
    ("SCC", |_| DatetimeElement::Century { signed: true }),
    ("DAY", DatetimeElement::DayName),
    ("DDD", |_| DatetimeElement::DayOfYear),
    ("DD", |_| DatetimeElement::DayOfMonth),
    ("DY", DatetimeElement::DayAbbreviation),
    ("D", |_| DatetimeElement::DayOfWeek),
    ("FM", |_| DatetimeElement::FillMode),
    ("FX", |_| DatetimeElement::ExactMode),
    ("HH24", |_| DatetimeElement::Hour24),
    ("HH12", |_| DatetimeElement::Hour12),
    ("HH", |_| DatetimeElement::Hour12),
    ("IYYY", |_| DatetimeElement::IsoYear(4)),
    ("IYY", |_| DatetimeElement::IsoYear(3)),
    ("IY", |_| DatetimeElement::IsoYear(2)),
    ("IW", |_| DatetimeElement::IsoWeek),
    ("I", |_| DatetimeElement::IsoYear(1)),
    ("J", |_| DatetimeElement::JulianDay),
    ("MI", |_| DatetimeElement::Minute),
    ("MM", |_| DatetimeElement::MonthNumber),
    ("MONTH", DatetimeElement::MonthName),
    ("MON", DatetimeElement::MonthAbbreviation),
    ("Q", |_| DatetimeElement::Quarter),
    ("RM", DatetimeElement::RomanMonth),
    ("RRRR", |_| DatetimeElement::RoundedYear(4)),
    ("RR", |_| DatetimeElement::RoundedYear(2)),
    ("SSSSS", |_| DatetimeElement::SecondsPastMidnight),
    ("SS", |_| DatetimeElement::Second),
    ("SYYYY", |_| DatetimeElement::SignedYear),
    ("SYEAR", |spelling| DatetimeElement::SpelledYear {
        signed: true,
        spelling,
    }),
    ("TZD", |_| DatetimeElement::ZoneAbbreviation),
    ("TZH", |_| DatetimeElement::ZoneHour),
    ("TZM", |_| DatetimeElement::ZoneMinute),
    ("TZR", |_| DatetimeElement::ZoneRegion),
    ("WW", |_| DatetimeElement::WeekOfYear),
    ("W", |_| DatetimeElement::WeekOfMonth),
    ("X", |_| DatetimeElement::RadixCharacter),
    ("Y,YYY", |_| DatetimeElement::GroupedYear),
    ("YEAR", |spelling| DatetimeElement::SpelledYear {
        signed: false,
        spelling,
    }),
    ("YYYY", |_| DatetimeElement::Year(4)),
    ("YYY", |_| DatetimeElement::Year(3)),
    ("YY", |_| DatetimeElement::Year(2)),
    ("Y", |_| DatetimeElement::Year(1)),
];

const SHORT_FORMS: [(&str, &str); 3] = [("DL", LONG_DATE), ("DS", SHORT_DATE), ("TS", SHORT_TIME)];

const SUFFIXES: [(&str, NumberSuffix); 4] = [
    ("SPTH", NumberSuffix::SpelledOrdinal),
    ("THSP", NumberSuffix::SpelledOrdinal),
    ("SP", NumberSuffix::Spelled),
    ("TH", NumberSuffix::Ordinal),
];

fn takes_suffix(element: &DatetimeElement) -> bool {
    use DatetimeElement as E;
    matches!(
        element,
        E::DayOfMonth
            | E::DayOfYear
            | E::DayOfWeek
            | E::MonthNumber
            | E::Year(_)
            | E::RoundedYear(_)
            | E::Hour12
            | E::Hour24
            | E::Minute
            | E::Second
            | E::JulianDay
            | E::SecondsPastMidnight
            | E::Century { signed: false }
            | E::IsoYear(_)
            | E::IsoWeek
            | E::WeekOfYear
            | E::WeekOfMonth
            | E::Quarter
    )
}

fn with_suffix(
    element: DatetimeElement,
    length: usize,
    chars: &[char],
    upper: &[char],
) -> (DatetimeElement, usize) {
    if !takes_suffix(&element) {
        return (element, length);
    }
    let rest = &upper[length..];
    for (word, suffix) in SUFFIXES {
        if starts_with(rest, word) {
            let end = length + word.len();
            let spelling = spelling_of(&chars[..end]);
            let suffix_spelling = spelling_of(&chars[length..end]);
            return (
                DatetimeElement::Suffixed {
                    number: Box::new(element),
                    suffix,
                    spelling,
                    suffix_spelling,
                },
                end,
            );
        }
    }
    (element, length)
}

fn element_at(chars: &[char], upper: &[char]) -> SqlResult<Scanned> {
    use DatetimeElement as E;
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
    if starts_with(upper, "FF") {
        if let Some(digit) = upper.get(2).and_then(|digit| digit.to_digit(10))
            && digit >= 1
        {
            return Ok(Scanned::Element(E::FractionalSeconds(Some(digit as u8)), 3));
        }
        return Ok(Scanned::Element(E::FractionalSeconds(None), 2));
    }
    for (word, expansion) in SHORT_FORMS {
        if starts_with(upper, word) {
            return Ok(Scanned::ShortForm(expansion, word.len()));
        }
    }
    for (word, make) in ELEMENTS {
        if starts_with(upper, word) {
            let length = word.chars().count();
            let element = make(spelling_of(&chars[..length.min(chars.len())]));
            let (element, length) = with_suffix(element, length, chars, upper);
            return Ok(Scanned::Element(element, length));
        }
    }
    Err(not_recognized())
}

fn is_output_only(element: &DatetimeElement) -> bool {
    use DatetimeElement as E;
    matches!(
        element,
        E::SpelledYear { .. }
            | E::Century { .. }
            | E::IsoYear(_)
            | E::IsoWeek
            | E::WeekOfYear
            | E::WeekOfMonth
            | E::Quarter
            | E::Suffixed { .. }
    )
}

fn is_zone_element(element: &DatetimeElement) -> bool {
    use DatetimeElement as E;
    matches!(
        element,
        E::ZoneHour | E::ZoneMinute | E::ZoneRegion | E::ZoneAbbreviation
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Year,
    Era,
    Month,
    Day,
    DayOfYear,
    Julian,
    Weekday,
    Hour,
    Meridian,
    Minute,
    Second,
    SecondsOfDay,
    Fraction,
    ZoneHour,
    ZoneMinute,
    ZoneRegion,
    ZoneAbbreviation,
}

fn field_of(element: &DatetimeElement) -> Option<Field> {
    use DatetimeElement as E;
    Some(match element {
        E::Year(_) | E::RoundedYear(_) | E::SignedYear | E::GroupedYear => Field::Year,
        E::Era { .. } => Field::Era,
        E::MonthNumber | E::MonthAbbreviation(_) | E::MonthName(_) | E::RomanMonth(_) => {
            Field::Month
        }
        E::DayOfMonth => Field::Day,
        E::DayOfYear => Field::DayOfYear,
        E::JulianDay => Field::Julian,
        E::DayOfWeek | E::DayName(_) | E::DayAbbreviation(_) => Field::Weekday,
        E::Hour12 | E::Hour24 => Field::Hour,
        E::Meridian { .. } => Field::Meridian,
        E::Minute => Field::Minute,
        E::Second => Field::Second,
        E::SecondsPastMidnight => Field::SecondsOfDay,
        E::FractionalSeconds(_) => Field::Fraction,
        E::ZoneHour => Field::ZoneHour,
        E::ZoneMinute => Field::ZoneMinute,
        E::ZoneRegion => Field::ZoneRegion,
        E::ZoneAbbreviation => Field::ZoneAbbreviation,
        _ => return None,
    })
}

fn repeated_field(field: Field) -> SqlError {
    match field {
        Field::Year => date_error(1812, "year may only be specified once"),
        Field::Hour => date_error(1813, "hour may only be specified once"),
        Field::Meridian => date_error(1814, "AM/PM conflicts with use of A.M./P.M."),
        Field::Era => date_error(1815, "BC/AD conflicts with use of B.C./A.D."),
        Field::Month => date_error(1816, "month may only be specified once"),
        Field::Weekday => date_error(1817, "day of week may only be specified once"),
        _ => date_error(1810, "format code appears twice"),
    }
}

fn check_input_elements(elements: &[DatetimeElement]) -> SqlResult<()> {
    let mut seen: Vec<(Field, &DatetimeElement)> = Vec::new();
    for element in elements {
        let Some(field) = field_of(element) else {
            continue;
        };
        if let Some((_, earlier)) = seen.iter().find(|(known, _)| *known == field) {
            return Err(if *earlier == element {
                date_error(1810, "format code appears twice")
            } else {
                repeated_field(field)
            });
        }
        seen.push((field, element));
    }
    let has = |wanted: Field| seen.iter().any(|(field, _)| *field == wanted);
    if has(Field::Julian) && has(Field::DayOfYear) {
        return Err(date_error(1811, "Julian date precludes use of day of year"));
    }
    if has(Field::Meridian)
        && elements
            .iter()
            .any(|element| *element == DatetimeElement::Hour24)
    {
        return Err(date_error(
            1818,
            "'HH24' precludes use of meridian indicator",
        ));
    }
    if has(Field::Era)
        && elements
            .iter()
            .any(|element| *element == DatetimeElement::SignedYear)
    {
        return Err(date_error(1819, "signed year precludes use of BC/AD"));
    }
    Ok(())
}

fn is_blank(element: &DatetimeElement) -> bool {
    matches!(
        element,
        DatetimeElement::Punctuation(' ') | DatetimeElement::FillMode | DatetimeElement::ExactMode
    )
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
        let mut short_forms = 0usize;
        let mut written = 0usize;
        let mut at = 0;
        while at < chars.len() {
            match element_at(&chars[at..], &upper[at..])? {
                Scanned::Element(element, length) => {
                    at += length;
                    if !is_blank(&element) {
                        written += 1;
                    }
                    elements.push(element);
                }
                Scanned::ShortForm(expansion, length) => {
                    at += length;
                    short_forms += 1;
                    elements.extend(Self::parse(expansion, ModelUse::Output)?.elements);
                }
            }
        }
        if short_forms > 0 && written > 0 {
            return Err(not_recognized());
        }
        if purpose != ModelUse::Output {
            Self::check_input(&elements, purpose)?;
        }
        Ok(Self { elements })
    }

    fn check_input(elements: &[DatetimeElement], purpose: ModelUse) -> SqlResult<()> {
        for element in elements {
            if is_output_only(element) {
                return Err(output_only_element());
            }
            if purpose != ModelUse::ZonedInput && is_zone_element(element) {
                return Err(not_recognized());
            }
            if purpose == ModelUse::DateInput
                && matches!(element, DatetimeElement::FractionalSeconds(_))
            {
                return Err(not_recognized());
            }
        }
        check_input_elements(elements)
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
                DatetimeElement::SpelledYear { .. }
                | DatetimeElement::Suffixed {
                    suffix: NumberSuffix::Spelled | NumberSuffix::SpelledOrdinal,
                    ..
                } => oracle_numerals::MAX_SPELLED_BYTES,
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
            let mut word_start = true;
            for character in name.chars() {
                out.push(if word_start {
                    character
                } else {
                    character.to_ascii_lowercase()
                });
                word_start = matches!(character, ' ' | '-');
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
    push_padded(out, &spelled(name, spelling), NAME_WIDTH, pad);
}

fn push_padded(out: &mut String, shown: &str, width: usize, pad: bool) {
    let length = shown.chars().count();
    out.push_str(shown);
    if pad && length < width {
        out.extend(std::iter::repeat_n(' ', width - length));
    }
}

fn push_sign(out: &mut String, negative: bool, fill: bool) {
    if negative {
        out.push('-');
    } else if !fill {
        out.push(' ');
    }
}

pub(crate) fn zone_text(offset: i32) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let magnitude = offset.unsigned_abs();
    format!("{sign}{:02}:{:02}", magnitude / 3600, magnitude / 60 % 60)
}

struct Parts {
    days: i64,
    of_day: i64,
    year: i64,
    month: i64,
    day: i64,
    offset: Option<i32>,
}

impl Parts {
    fn of(local: i64, offset: Option<i32>) -> SqlResult<Self> {
        let days = local.div_euclid(MICROS_PER_DAY);
        let (year, month, day) = datetime::civil_from_days(days);
        if !year_in_range(year) {
            return Err(year_out_of_range());
        }
        Ok(Self {
            days,
            of_day: local.rem_euclid(MICROS_PER_DAY),
            year,
            month,
            day,
            offset,
        })
    }

    fn before_christ(&self) -> bool {
        self.year <= 0
    }

    fn era_year(&self) -> i64 {
        if self.year <= 0 {
            1 - self.year
        } else {
            self.year
        }
    }

    fn hour(&self) -> i64 {
        self.of_day / (3600 * MICROS_PER_SECOND)
    }

    fn weekday(&self) -> i64 {
        (self.days + 6).rem_euclid(7)
    }

    fn day_of_year(&self) -> i64 {
        self.days - datetime::days_from_civil(self.year, 1, 1) + 1
    }

    fn number(&self, element: &DatetimeElement) -> Option<(i64, usize)> {
        use DatetimeElement as E;
        let last_digits = |value: i64, width: u8| {
            let width = usize::from(width);
            (value % 10i64.pow(width as u32), width)
        };
        Some(match element {
            E::DayOfMonth => (self.day, 2),
            E::DayOfYear => (self.day_of_year(), 3),
            E::DayOfWeek => (self.weekday() + 1, 1),
            E::MonthNumber => (self.month, 2),
            E::Year(width) | E::RoundedYear(width) => last_digits(self.era_year(), *width),
            E::Hour12 => {
                let hour = self.hour() % 12;
                (if hour == 0 { 12 } else { hour }, 2)
            }
            E::Hour24 => (self.hour(), 2),
            E::Minute => (self.of_day / (60 * MICROS_PER_SECOND) % 60, 2),
            E::Second => (self.of_day / MICROS_PER_SECOND % 60, 2),
            E::JulianDay => (self.days + JULIAN_DAY_OF_EPOCH, 7),
            E::SecondsPastMidnight => (self.of_day / MICROS_PER_SECOND, 5),
            E::Century { signed: false } => ((self.era_year() + 99) / 100, 2),
            E::IsoYear(width) => last_digits(datetime::iso_week(self.days).0.abs(), *width),
            E::IsoWeek => (datetime::iso_week(self.days).1, 2),
            E::WeekOfYear => ((self.day_of_year() - 1) / 7 + 1, 2),
            E::WeekOfMonth => ((self.day - 1) / 7 + 1, 1),
            E::Quarter => ((self.month - 1) / 3 + 1, 1),
            _ => return None,
        })
    }
}

fn push_suffixed(
    out: &mut String,
    value: i64,
    width: usize,
    fill: bool,
    shape: (NumberSuffix, Spelling, Spelling),
) {
    let (suffix, spelling, suffix_spelling) = shape;
    let magnitude = value.unsigned_abs();
    match suffix {
        NumberSuffix::Ordinal => {
            push_number(out, value, width, fill);
            out.push_str(&spelled(
                oracle_numerals::ordinal_suffix(magnitude),
                suffix_spelling,
            ));
        }
        NumberSuffix::Spelled => out.push_str(&spelled(
            &oracle_numerals::spelled(magnitude),
            spelling,
        )),
        NumberSuffix::SpelledOrdinal => out.push_str(&spelled(
            &oracle_numerals::spelled_ordinal(magnitude),
            spelling,
        )),
    }
}

fn letters(text: &str, spelling: Spelling) -> String {
    if spelling == Spelling::Lower {
        text.to_ascii_lowercase()
    } else {
        text.to_owned()
    }
}

pub(crate) fn format_zoned(
    local: i64,
    model: &DatetimeModel,
    precision: Option<u8>,
    offset: Option<i32>,
) -> SqlResult<String> {
    use DatetimeElement as E;
    crate::oracle_functions::within_limit(model.output_bound())?;
    let parts = Parts::of(local, offset)?;
    let micros = parts.of_day % MICROS_PER_SECOND;
    let mut out = String::new();
    let mut fill = false;
    for element in &model.elements {
        if let Some((value, width)) = parts.number(element) {
            push_number(&mut out, value, width, fill);
            continue;
        }
        match element {
            E::Text(text) => out.push_str(text),
            E::Punctuation(character) => out.push(*character),
            E::FillMode => fill = !fill,
            E::ExactMode => {}
            E::Meridian { dotted, spelling } => {
                let text = match (parts.hour() < 12, dotted) {
                    (true, false) => "AM",
                    (false, false) => "PM",
                    (true, true) => "A.M.",
                    (false, true) => "P.M.",
                };
                out.push_str(&letters(text, *spelling));
            }
            E::Era { dotted, spelling } => {
                let text = match (parts.before_christ(), dotted) {
                    (false, false) => "AD",
                    (true, false) => "BC",
                    (false, true) => "A.D.",
                    (true, true) => "B.C.",
                };
                out.push_str(&letters(text, *spelling));
            }
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
            E::MonthAbbreviation(spelling) => {
                push_name(
                    &mut out,
                    &MONTHS[parts.month as usize - 1][..3],
                    *spelling,
                    false,
                );
            }
            E::MonthName(spelling) => {
                push_name(&mut out, MONTHS[parts.month as usize - 1], *spelling, !fill);
            }
            E::RomanMonth(spelling) => {
                let numeral = oracle_numerals::roman(parts.month as u32).unwrap_or_default();
                push_padded(
                    &mut out,
                    &letters(&numeral, *spelling),
                    ROMAN_MONTH_WIDTH,
                    !fill,
                );
            }
            E::SignedYear => {
                push_sign(&mut out, parts.before_christ(), fill);
                push_number(&mut out, parts.era_year(), 4, fill);
            }
            E::GroupedYear => {
                let year = parts.era_year();
                out.push_str(&format!("{},{:03}", year / 1000, year % 1000));
            }
            E::SpelledYear { signed, spelling } => {
                if *signed {
                    push_sign(&mut out, parts.before_christ(), fill);
                }
                out.push_str(&spelled(
                    &oracle_numerals::spelled_year(parts.era_year().unsigned_abs()),
                    *spelling,
                ));
            }
            E::Century { signed: true } => {
                push_sign(&mut out, parts.before_christ(), fill);
                push_number(&mut out, (parts.era_year() + 99) / 100, 2, fill);
            }
            E::RadixCharacter => out.push('.'),
            E::DayName(spelling) => push_name(&mut out, DAYS[parts.weekday() as usize], *spelling, !fill),
            E::DayAbbreviation(spelling) => {
                push_name(
                    &mut out,
                    &DAYS[parts.weekday() as usize][..3],
                    *spelling,
                    false,
                );
            }
            E::ZoneHour => {
                let offset = parts.offset.ok_or_else(not_recognized)?;
                let sign = if offset < 0 { '-' } else { '+' };
                out.push(sign);
                out.push_str(&format!("{:02}", offset.unsigned_abs() / 3600));
            }
            E::ZoneMinute => {
                let offset = parts.offset.ok_or_else(not_recognized)?;
                out.push_str(&format!("{:02}", offset.unsigned_abs() / 60 % 60));
            }
            E::ZoneRegion => {
                let offset = parts.offset.ok_or_else(not_recognized)?;
                out.push_str(&zone_text(offset));
            }
            E::ZoneAbbreviation => {
                parts.offset.ok_or_else(not_recognized)?;
            }
            E::Suffixed {
                number,
                suffix,
                spelling,
                suffix_spelling,
            } => {
                let Some((value, width)) = parts.number(number) else {
                    return Err(not_recognized());
                };
                push_suffixed(
                    &mut out,
                    value,
                    width,
                    fill,
                    (*suffix, *spelling, *suffix_spelling),
                );
            }
            _ => return Err(not_recognized()),
        }
    }
    Ok(out)
}

#[derive(Default)]
struct Fields {
    year: Option<i64>,
    before_christ: Option<bool>,
    month: Option<i64>,
    day: Option<i64>,
    day_of_year: Option<i64>,
    julian: Option<i64>,
    weekday: Option<i64>,
    hour: Option<i64>,
    minute: Option<i64>,
    second: Option<i64>,
    seconds_of_day: Option<i64>,
    nanos: i64,
    afternoon: Option<bool>,
    zone_hour: Option<(bool, i64)>,
    zone_minute: Option<i64>,
    zone: Option<i32>,
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

    fn sign(&mut self) -> bool {
        match self.peek() {
            Some('-') => {
                self.at += 1;
                true
            }
            Some('+') => {
                self.at += 1;
                false
            }
            _ => false,
        }
    }

    fn name_among(&mut self, names: &[&str], full: Option<bool>) -> Option<i64> {
        let mut try_names = |full: bool| {
            names.iter().position(|name| {
                let name: &str = if full { name } else { &name[..3] };
                self.word(name)
            })
        };
        let found = match full {
            Some(full) => try_names(full),
            None => try_names(true).or_else(|| try_names(false)),
        };
        found.map(|index| index as i64)
    }

    fn month_name(&mut self) -> Option<i64> {
        self.name_among(&MONTHS, None).map(|index| index + 1)
    }

    fn roman_month(&mut self) -> Option<i64> {
        let found = ROMAN_MONTHS.iter().position(|numeral| self.word(numeral))?;
        Some(12 - found as i64)
    }

    fn zone_name(&mut self) -> String {
        let start = self.at;
        while self
            .peek()
            .is_some_and(|character| character.is_alphanumeric() || matches!(character, '/' | '_'))
        {
            self.at += 1;
        }
        self.input[start..self.at].iter().collect()
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
            | E::SecondsPastMidnight
            | E::FractionalSeconds(_)
            | E::Meridian { .. }
            | E::Punctuation(_)
            | E::RadixCharacter
            | E::FillMode
            | E::ExactMode
            | E::Text(_)
            | E::ZoneHour
            | E::ZoneMinute
            | E::ZoneRegion
            | E::ZoneAbbreviation
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

fn read_year(
    element: &DatetimeElement,
    reader: &mut Reader<'_>,
    modes: (bool, bool),
    now_year: i64,
) -> SqlResult<i64> {
    use DatetimeElement as E;
    let (fill, exact) = modes;
    let (rounded, width) = match element {
        E::RoundedYear(width) => (true, usize::from(*width)),
        E::Year(width) => (false, usize::from(*width)),
        _ => (false, 4),
    };
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
    Ok(year)
}

fn read_grouped_year(reader: &mut Reader<'_>) -> SqlResult<i64> {
    let mut value = 0i64;
    let mut count = 0;
    let mut comma = false;
    while let Some(character) = reader.peek() {
        if let Some(digit) = character.to_digit(10)
            && count < 4
        {
            value = value * 10 + i64::from(digit);
            count += 1;
        } else if character == ',' && !comma && count == 1 {
            comma = true;
        } else {
            break;
        }
        reader.at += 1;
    }
    if count == 0 {
        return Err(not_numeric());
    }
    if value == 0 {
        return Err(year_out_of_range());
    }
    Ok(value)
}

fn read_zone_region(reader: &mut Reader<'_>, fields: &mut Fields) -> SqlResult<()> {
    if reader
        .peek()
        .is_some_and(|character| matches!(character, '+' | '-') || character.is_ascii_digit())
    {
        let negative = reader.sign();
        let Some((hours, _)) = reader.digits(2, None)? else {
            return Err(not_a_zone());
        };
        let minutes = if reader.peek() == Some(':') {
            reader.at += 1;
            reader.digits(2, None)?.map_or(0, |(minutes, _)| minutes)
        } else {
            0
        };
        fields.zone = Some(offset_of(negative, hours, minutes)?);
        return Ok(());
    }
    let name = reader.zone_name();
    fields.zone = Some(named_zone(&name)?);
    Ok(())
}

fn offset_of(negative: bool, hours: i64, minutes: i64) -> SqlResult<i32> {
    let hours = checked(
        hours,
        0..=MAX_ZONE_HOURS,
        1874,
        "time zone hour must be between -15 and 15",
    )?;
    let minutes = checked(
        minutes,
        0..=59,
        1875,
        "time zone minute must be between -59 and 59",
    )?;
    let seconds = (hours * 3600 + minutes * 60) as i32;
    Ok(if negative { -seconds } else { seconds })
}

fn named_zone(name: &str) -> SqlResult<i32> {
    let upper = name.trim().to_ascii_uppercase();
    if UTC_NAMES.contains(&upper.as_str()) {
        Ok(0)
    } else {
        Err(zone_region_not_found())
    }
}

pub(crate) fn zone_offset(text: &str) -> SqlResult<i32> {
    let trimmed = text.trim();
    if trimmed
        .chars()
        .next()
        .is_some_and(|character| matches!(character, '+' | '-') || character.is_ascii_digit())
    {
        let (negative, body) = match trimmed.strip_prefix('-') {
            Some(body) => (true, body),
            None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
        };
        let (hours, minutes) = body.split_once(':').unwrap_or((body, "0"));
        let number = |part: &str| -> SqlResult<i64> {
            let part = part.trim();
            if part.is_empty() || part.len() > 2 || !part.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(not_a_zone());
            }
            part.parse::<i64>().map_err(|_| not_a_zone())
        };
        return offset_of(negative, number(hours)?, number(minutes)?);
    }
    named_zone(trimmed)
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
        E::Era { .. } => {
            let before_christ = if reader.word("A.D.") || reader.word("AD") {
                false
            } else if reader.word("B.C.") || reader.word("BC") {
                true
            } else {
                return Err(date_error(1856, "BC/B.C. or AD/A.D. required"));
            };
            fields.before_christ = Some(before_christ);
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
            let full = matches!(element, E::MonthName(_));
            let month = if exact {
                reader.name_among(&MONTHS, Some(full)).map(|index| index + 1)
            } else {
                reader.month_name()
            };
            fields.month = Some(month.ok_or_else(not_a_month)?);
        }
        E::RomanMonth(_) => fields.month = Some(reader.roman_month().ok_or_else(not_a_month)?),
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
        E::Year(_) | E::RoundedYear(_) => {
            fields.year = Some(read_year(element, reader, modes, now_year)?);
        }
        E::SignedYear => {
            let negative = reader.sign();
            let year = number(reader, 4)?;
            if !(1..=9999).contains(&year) {
                return Err(year_out_of_range());
            }
            fields.year = Some(if negative { -year } else { year });
        }
        E::GroupedYear => fields.year = Some(read_grouped_year(reader)?),
        E::DayOfWeek => {
            let weekday = number(reader, 1)?;
            if !(1..=7).contains(&weekday) {
                return Err(not_a_day_of_the_week());
            }
            fields.weekday = Some(weekday - 1);
        }
        E::DayName(_) | E::DayAbbreviation(_) => {
            let full = exact.then_some(matches!(element, E::DayName(_)));
            fields.weekday = Some(
                reader
                    .name_among(&DAYS, full)
                    .ok_or_else(not_a_day_of_the_week)?,
            );
        }
        E::DayOfYear => {
            let day = number(reader, 3)?;
            fields.day_of_year = Some(checked(
                day,
                1..=366,
                1848,
                "day of year must be between 1 and 365 (366 for leap year)",
            )?);
        }
        E::JulianDay => {
            let julian = number(reader, 7)?;
            fields.julian = Some(checked(
                julian,
                1..=MAX_JULIAN_DAY,
                1854,
                "julian date must be between 1 and 5373484",
            )?);
        }
        E::SecondsPastMidnight => {
            let seconds = number(reader, 5)?;
            fields.seconds_of_day = Some(checked(
                seconds,
                0..=86_399,
                1853,
                "seconds in day must be between 0 and 86399",
            )?);
        }
        E::ZoneHour => {
            let negative = reader.sign();
            let Some((hours, _)) = reader.digits(2, None)? else {
                return Err(not_numeric());
            };
            checked(
                hours,
                0..=MAX_ZONE_HOURS,
                1874,
                "time zone hour must be between -15 and 15",
            )?;
            fields.zone_hour = Some((negative, hours));
        }
        E::ZoneMinute => {
            let minutes = number(reader, 2)?;
            fields.zone_minute = Some(checked(
                minutes,
                0..=59,
                1875,
                "time zone minute must be between -59 and 59",
            )?);
        }
        E::ZoneRegion => read_zone_region(reader, fields)?,
        E::ZoneAbbreviation => {
            reader.zone_name();
        }
        E::SpelledYear { .. }
        | E::Century { .. }
        | E::IsoYear(_)
        | E::IsoWeek
        | E::WeekOfYear
        | E::WeekOfMonth
        | E::Quarter
        | E::Suffixed { .. } => return Err(output_only_element()),
    }
    Ok(())
}

pub(crate) fn parse_zoned(
    text: &str,
    model: &DatetimeModel,
    now: i64,
) -> SqlResult<(i64, Option<i32>)> {
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

fn astronomical_year(written: i64, before_christ: Option<bool>) -> SqlResult<i64> {
    if written == 0 {
        return Err(year_out_of_range());
    }
    Ok(match (written < 0, before_christ) {
        (true, _) => written + 1,
        (false, Some(true)) => 1 - written,
        _ => written,
    })
}

fn assemble_date(fields: &Fields, now_year: i64, now_month: i64) -> SqlResult<(i64, i64, i64)> {
    if let Some(julian) = fields.julian {
        let (year, month, day) = datetime::civil_from_days(julian - JULIAN_DAY_OF_EPOCH);
        if let Some(written) = fields.year
            && astronomical_year(written, fields.before_christ)? != year
        {
            return Err(date_error(1831, "year conflicts with Julian date"));
        }
        if fields.month.is_some_and(|written| written != month) {
            return Err(date_error(1833, "month conflicts with Julian date"));
        }
        if fields.day.is_some_and(|written| written != day) {
            return Err(date_error(1834, "day of month conflicts with Julian date"));
        }
        return Ok((year, month, day));
    }
    let year = astronomical_year(fields.year.unwrap_or(now_year), fields.before_christ)?;
    if !year_in_range(year) {
        return Err(year_out_of_range());
    }
    if let Some(day_of_year) = fields.day_of_year {
        let length = if datetime::is_leap_year(year) { 366 } else { 365 };
        if day_of_year > length {
            return Err(date_error(
                1848,
                "day of year must be between 1 and 365 (366 for leap year)",
            ));
        }
        let days = datetime::days_from_civil(year, 1, 1) + day_of_year - 1;
        let (_, month, day) = datetime::civil_from_days(days);
        if fields.month.is_some_and(|written| written != month)
            || fields.day.is_some_and(|written| written != day)
        {
            return Err(date_error(1839, "date not valid for month specified"));
        }
        return Ok((year, month, day));
    }
    let month = fields.month.unwrap_or(now_month);
    let day = fields.day.unwrap_or(1);
    if day > datetime::days_in_month(year, month) {
        return Err(date_error(1839, "date not valid for month specified"));
    }
    Ok((year, month, day))
}

fn assemble_time(fields: &Fields) -> SqlResult<(i64, i64, i64)> {
    let mut hour = fields.hour.unwrap_or(0);
    if let Some(afternoon) = fields.afternoon {
        hour = hour % 12 + if afternoon { 12 } else { 0 };
    }
    let minute = fields.minute.unwrap_or(0);
    let second = fields.second.unwrap_or(0);
    let Some(total) = fields.seconds_of_day else {
        return Ok((hour, minute, second));
    };
    let (from_hour, from_minute, from_second) = (total / 3600, total / 60 % 60, total % 60);
    if fields.hour.is_some() && hour != from_hour {
        return Err(date_error(1836, "hour conflicts with seconds in day"));
    }
    if fields.minute.is_some() && minute != from_minute {
        return Err(date_error(1837, "minutes of hour conflicts with seconds in day"));
    }
    if fields.second.is_some() && second != from_second {
        return Err(date_error(1838, "seconds of minute conflicts with seconds in day"));
    }
    Ok((from_hour, from_minute, from_second))
}

fn assemble_zone(fields: &Fields) -> SqlResult<Option<i32>> {
    if let Some(zone) = fields.zone {
        return Ok(Some(zone));
    }
    match (fields.zone_hour, fields.zone_minute) {
        (Some((negative, hours)), minutes) => {
            offset_of(negative, hours, minutes.unwrap_or(0)).map(Some)
        }
        (None, Some(minutes)) => offset_of(false, 0, minutes).map(Some),
        (None, None) => Ok(None),
    }
}

fn assemble(fields: &Fields, now_year: i64, now_month: i64) -> SqlResult<(i64, Option<i32>)> {
    let (year, month, day) = assemble_date(fields, now_year, now_month)?;
    if !year_in_range(year) {
        return Err(year_out_of_range());
    }
    let days = datetime::days_from_civil(year, month, day);
    if fields
        .weekday
        .is_some_and(|weekday| weekday != (days + 6).rem_euclid(7))
    {
        return Err(date_error(1835, "day of week conflicts with Julian date"));
    }
    let (hour, minute, second) = assemble_time(fields)?;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second;
    let micros = i128::from(seconds) * i128::from(MICROS_PER_SECOND)
        + i128::from((fields.nanos + NANOS_PER_MICRO / 2) / NANOS_PER_MICRO);
    Ok((datetime::checked_timestamp(micros)?, assemble_zone(fields)?))
}

fn local_now() -> SqlResult<i64> {
    datetime::utc_to_local(datetime::statement_now())
}

pub(crate) fn read_by_model(text: &str, format: &str, purpose: ModelUse) -> SqlResult<i64> {
    read_zoned(text, format, purpose).map(|(local, _)| local)
}

pub(crate) fn read_zoned(
    text: &str,
    format: &str,
    purpose: ModelUse,
) -> SqlResult<(i64, Option<i32>)> {
    let model = cached_model(format, purpose)?;
    parse_zoned(text, &model, local_now()?)
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

fn default_format(purpose: ModelUse) -> String {
    let nls = crate::oracle_nls::current();
    match purpose {
        ModelUse::DateInput => nls.date_format().to_owned(),
        ModelUse::ZonedInput => nls.timestamp_tz_format().to_owned(),
        _ => nls.timestamp_format().to_owned(),
    }
}

fn read_source(values: &[Value], purpose: ModelUse) -> SqlResult<Option<(i64, Option<i32>)>> {
    let Some(text) = values.first().and_then(source_text) else {
        return Ok(None);
    };
    if values.get(1).is_some_and(Value::is_null) || values.get(2).is_some_and(Value::is_null) {
        return Ok(None);
    }
    check_date_language(values.get(2))?;
    let format = match values.get(1).and_then(source_text) {
        Some(format) => format,
        None => default_format(purpose),
    };
    read_zoned(&text, &format, purpose).map(Some)
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
    Ok(match read_source(values, purpose)? {
        Some((local, _)) => Value::Timestamp(truncated(local)),
        None => Value::Null,
    })
}

pub(crate) fn to_date(values: &[Value]) -> SqlResult<Value> {
    to_datetime(values, ModelUse::DateInput)
}

pub(crate) fn to_timestamp(values: &[Value]) -> SqlResult<Value> {
    to_datetime(values, ModelUse::TimestampInput)
}

pub(crate) fn utc_of(local: i64, offset: Option<i32>) -> SqlResult<i64> {
    match offset {
        Some(offset) => datetime::checked_timestamp(
            i128::from(local) - i128::from(offset) * i128::from(MICROS_PER_SECOND),
        ),
        None => datetime::local_to_utc(local),
    }
}

pub(crate) fn to_timestamp_tz(values: &[Value]) -> SqlResult<Value> {
    match values.first() {
        Some(Value::TimestampTz(micros)) if values.len() == 1 => {
            return Ok(Value::TimestampTz(*micros));
        }
        Some(Value::Timestamp(micros)) if values.len() == 1 => {
            return datetime::local_to_utc(*micros).map(Value::TimestampTz);
        }
        _ => {}
    }
    Ok(match read_source(values, ModelUse::ZonedInput)? {
        Some((local, offset)) => Value::TimestampTz(utc_of(local, offset)?),
        None => Value::Null,
    })
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

pub(crate) fn local_of(value: &Value) -> SqlResult<Option<i64>> {
    Ok(Some(match value {
        Value::Timestamp(micros) => *micros,
        Value::TimestampTz(micros) => datetime::utc_to_local(*micros)?,
        Value::Date(days) => datetime::date_to_timestamp(*days)?,
        _ => return Ok(None),
    }))
}

fn offset_of_value(value: &Value) -> Option<i32> {
    matches!(value, Value::TimestampTz(_)).then(datetime::session_offset)
}

pub(crate) fn nls_text(value: &Value, declared: Option<SqlType>) -> SqlResult<Option<String>> {
    let Some(local) = local_of(value)? else {
        return Ok(None);
    };
    let (format, precision) = nls_output(declared, value);
    let model = cached_model(&format, ModelUse::Output)?;
    format_zoned(local, &model, precision, offset_of_value(value)).map(Some)
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
    let text = format_zoned(local, &model, precision, offset_of_value(&values[0]))?;
    Ok(if text.is_empty() {
        Value::Null
    } else {
        Value::Text(text)
    })
}

pub(crate) fn date_of(value: &Value) -> SqlResult<Option<i64>> {
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
    if !year_in_range(new_year) {
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

#[cfg(test)]
mod tests {
    use super::{
        DatetimeModel, ModelUse, cached_model, cached_models, clear_model_cache, format_zoned,
        parse_zoned, round_year, strict_iso, zone_offset,
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
        parse_zoned(text, &model, now())
            .map(|(local, _)| local)
            .map_err(|error| error.vendor_code().unwrap_or(0))
    }

    fn read_zone(text: &str, format: &str) -> Result<(i64, Option<i32>), u16> {
        let model = DatetimeModel::parse(format, ModelUse::ZonedInput)
            .map_err(|error| error.vendor_code().unwrap_or(0))?;
        parse_zoned(text, &model, now()).map_err(|error| error.vendor_code().unwrap_or(0))
    }

    fn write(micros: i64, format: &str, precision: Option<u8>) -> Result<String, u16> {
        let model = DatetimeModel::parse(format, ModelUse::Output)
            .map_err(|error| error.vendor_code().unwrap_or(0))?;
        format_zoned(micros, &model, precision, None)
            .map_err(|error| error.vendor_code().unwrap_or(0))
    }

    fn write_zone(micros: i64, format: &str, offset: i32) -> Result<String, u16> {
        let model = DatetimeModel::parse(format, ModelUse::Output)
            .map_err(|error| error.vendor_code().unwrap_or(0))?;
        format_zoned(micros, &model, None, Some(offset))
            .map_err(|error| error.vendor_code().unwrap_or(0))
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
    fn calendar_elements_write_centuries_weeks_quarters_and_iso_years() {
        let stamp = at(1999, 12, 1, 22, 5, 9);
        assert_eq!(
            write(stamp, "CC SCC Q WW W IW IYYY IY I", None).as_deref(),
            Ok("20  20 4 48 1 48 1999 99 9")
        );
        assert_eq!(write(at(2000, 1, 1, 0, 0, 0), "CC", None).as_deref(), Ok("20"));
        assert_eq!(write(at(2001, 1, 1, 0, 0, 0), "CC", None).as_deref(), Ok("21"));
        assert_eq!(
            write(at(2021, 1, 1, 0, 0, 0), "IYYY-IW", None).as_deref(),
            Ok("2020-53")
        );
        assert_eq!(
            write(stamp, "Y,YYY SYYYY AD a.d.", None).as_deref(),
            Ok("1,999  1999 AD a.d.")
        );
        assert_eq!(
            write(stamp, "RM rm FMRM", None).as_deref(),
            Ok("XII  xii  XII")
        );
        assert_eq!(
            write(stamp, "YEAR Year", None).as_deref(),
            Ok("NINETEEN NINETY-NINE Nineteen Ninety-Nine")
        );
        let before = at(-43, 3, 15, 0, 0, 0);
        assert_eq!(
            write(before, "YYYY BC SYYYY B.C.", None).as_deref(),
            Ok("0044 BC -0044 B.C.")
        );
    }

    #[test]
    fn suffixes_write_ordinals_and_spelled_numbers() {
        let stamp = at(1999, 12, 1, 22, 5, 9);
        assert_eq!(write(stamp, "DDTH", None).as_deref(), Ok("01ST"));
        assert_eq!(write(stamp, "fmDDth", None).as_deref(), Ok("1st"));
        assert_eq!(write(stamp, "DDSP", None).as_deref(), Ok("ONE"));
        assert_eq!(write(stamp, "Ddspth", None).as_deref(), Ok("First"));
        assert_eq!(
            write(stamp, "MMSPTH", None).as_deref(),
            Ok("TWELFTH")
        );
        assert_eq!(
            write(stamp, "Jsp", None).as_deref(),
            Ok("Two Million Four Hundred Fifty-One Thousand Five Hundred Fourteen")
        );
        assert_eq!(write(stamp, "MONTHTH", None), Err(1821));
    }

    #[test]
    fn short_forms_expand_as_the_america_territory_defines_them() {
        let stamp = at(1999, 12, 1, 22, 5, 9);
        assert_eq!(write(stamp, "DS", None).as_deref(), Ok("12/01/1999"));
        assert_eq!(
            write(stamp, "DL", None).as_deref(),
            Ok("Wednesday, December 1, 1999")
        );
        assert_eq!(
            write(stamp, "DS TS", None).as_deref(),
            Ok("12/01/1999 10:05:09 PM")
        );
        assert_eq!(write(stamp, "DS YYYY", None), Err(1821));
        assert_eq!(read("12/01/1999", "DS"), Ok(at(1999, 12, 1, 0, 0, 0)));
        assert_eq!(
            read("Wednesday, December 1, 1999", "DL"),
            Ok(at(1999, 12, 1, 0, 0, 0))
        );
    }

    #[test]
    fn zones_write_from_the_offset_and_need_one() {
        let stamp = at(1999, 12, 1, 10, 0, 0);
        assert_eq!(
            write_zone(stamp, "HH24:MI TZH:TZM TZR", -8 * 3600).as_deref(),
            Ok("10:00 -08:00 -08:00")
        );
        assert_eq!(
            write_zone(stamp, "TZH TZD\"|\"", 5 * 3600 + 1800).as_deref(),
            Ok("+05 |")
        );
        assert_eq!(write(stamp, "TZH", None), Err(1821));
    }

    #[test]
    fn unknown_and_unsupported_elements_are_refused() {
        assert_eq!(write(0, "QQ", None).as_deref(), Ok("11"));
        assert_eq!(write(0, "EE", None), Err(1821));
        assert_eq!(write(0, "E", None), Err(1821));
        assert_eq!(read("1", "IW"), Err(1820));
        assert_eq!(read("1", "Q"), Err(1820));
        assert_eq!(read("1", "DDTH"), Err(1820));
        assert_eq!(read("01 01", "DD DD"), Err(1810));
        assert_eq!(read("01 01", "MM MON"), Err(1816));
        assert_eq!(read("1998 98", "YYYY RR"), Err(1812));
        assert_eq!(read("10 10", "HH HH24"), Err(1813));
        assert_eq!(read("2451514 001", "J DDD"), Err(1811));
        assert_eq!(read("10 AM", "HH24 AM"), Err(1818));
        assert_eq!(read("-1998 BC", "SYYYY BC"), Err(1819));
        assert_eq!(read("1 Monday", "D DAY"), Err(1817));
        assert_eq!(read("+08", "TZH"), Err(1821));
        assert!(DatetimeModel::parse("HH24:MI:SS.FF", ModelUse::DateInput).is_err());
        assert_eq!(
            DatetimeModel::parse(&"DD".repeat(501), ModelUse::Output)
                .map_err(|error| error.vendor_code()),
            Err(Some(1801))
        );
        assert_eq!(write(0, "\"open", None), Err(1821));
    }

    #[test]
    fn julian_days_days_of_year_week_days_and_seconds_read_with_their_conflicts() {
        assert_eq!(read("2451514", "J"), Ok(at(1999, 12, 1, 0, 0, 0)));
        assert_eq!(read("1999 335", "YYYY DDD"), Ok(at(1999, 12, 1, 0, 0, 0)));
        assert_eq!(read("2024 366", "YYYY DDD"), Ok(at(2024, 12, 31, 0, 0, 0)));
        assert_eq!(read("2023 366", "YYYY DDD"), Err(1848));
        assert_eq!(read("2023 367", "YYYY DDD"), Err(1848));
        assert_eq!(read("1998 2451514", "YYYY J"), Err(1831));
        assert_eq!(read("11 2451514", "MM J"), Err(1833));
        assert_eq!(read("02 2451514", "DD J"), Err(1834));
        assert_eq!(read("0", "J"), Err(1854));
        assert_eq!(
            read("Wednesday 01-12-1999", "DAY DD-MM-YYYY"),
            Ok(at(1999, 12, 1, 0, 0, 0))
        );
        assert_eq!(
            read("wed 01-12-1999", "DY DD-MM-YYYY"),
            Ok(at(1999, 12, 1, 0, 0, 0))
        );
        assert_eq!(read("Monday 01-12-1999", "DAY DD-MM-YYYY"), Err(1835));
        assert_eq!(read("4 01-12-1999", "D DD-MM-YYYY"), Ok(at(1999, 12, 1, 0, 0, 0)));
        assert_eq!(read("8 01-12-1999", "D DD-MM-YYYY"), Err(1846));
        assert_eq!(read("Someday", "DAY"), Err(1846));
        assert_eq!(
            read("01-12-1999 79509", "DD-MM-YYYY SSSSS"),
            Ok(at(1999, 12, 1, 22, 5, 9))
        );
        assert_eq!(
            read("01-12-1999 22 79509", "DD-MM-YYYY HH24 SSSSS"),
            Ok(at(1999, 12, 1, 22, 5, 9))
        );
        assert_eq!(read("01-12-1999 21 79509", "DD-MM-YYYY HH24 SSSSS"), Err(1836));
        assert_eq!(read("86400", "SSSSS"), Err(1853));
        assert_eq!(read("XII-1999", "RM-YYYY"), Ok(at(1999, 12, 1, 0, 0, 0)));
        assert_eq!(read("iv-1999", "RM-YYYY"), Ok(at(1999, 4, 1, 0, 0, 0)));
    }

    #[test]
    fn eras_signed_years_and_grouped_years_read_before_christ_too() {
        assert_eq!(read("15-03-0044 BC", "DD-MM-YYYY BC"), Ok(at(-43, 3, 15, 0, 0, 0)));
        assert_eq!(read("15-03-0044 A.D.", "DD-MM-YYYY BC"), Ok(at(44, 3, 15, 0, 0, 0)));
        assert_eq!(read("15-03-0044 XX", "DD-MM-YYYY BC"), Err(1856));
        assert_eq!(read("-0044", "SYYYY"), Ok(at(-43, 10, 1, 0, 0, 0)));
        assert_eq!(read("2,024", "Y,YYY"), Ok(at(2024, 10, 1, 0, 0, 0)));
        assert_eq!(read("2024", "Y,YYY"), Ok(at(2024, 10, 1, 0, 0, 0)));
        assert_eq!(read("01-01-4713 BC", "DD-MM-YYYY BC"), Ok(at(-4712, 1, 1, 0, 0, 0)));
        assert_eq!(read("01-01-4715 BC", "DD-MM-YYYY BC"), Err(1841));
        assert_eq!(read("01-01-4712 BC", "DD-MM-YYYY BC"), Ok(at(-4711, 1, 1, 0, 0, 0)));
    }

    #[test]
    fn zones_read_as_hours_minutes_or_a_region() {
        assert_eq!(
            read_zone("1999-12-01 10:00 -08:00", "YYYY-MM-DD HH24:MI TZH:TZM"),
            Ok((at(1999, 12, 1, 10, 0, 0), Some(-8 * 3600)))
        );
        assert_eq!(
            read_zone("1999-12-01 10:00 +05:30", "YYYY-MM-DD HH24:MI TZR"),
            Ok((at(1999, 12, 1, 10, 0, 0), Some(5 * 3600 + 1800)))
        );
        assert_eq!(
            read_zone("1999-12-01 10:00 UTC", "YYYY-MM-DD HH24:MI TZR"),
            Ok((at(1999, 12, 1, 10, 0, 0), Some(0)))
        );
        assert_eq!(
            read_zone("1999-12-01 10:00", "YYYY-MM-DD HH24:MI TZR"),
            Ok((at(1999, 12, 1, 10, 0, 0), None))
        );
        assert_eq!(
            read_zone("1999-12-01 10:00 US/Pacific", "YYYY-MM-DD HH24:MI TZR"),
            Err(1882)
        );
        assert_eq!(read_zone("+16", "TZH"), Err(1874));
        assert_eq!(zone_offset("-8:00").ok(), Some(-8 * 3600));
        assert_eq!(zone_offset(" +05:30 ").ok(), Some(5 * 3600 + 1800));
        assert_eq!(zone_offset("utc").ok(), Some(0));
        assert_eq!(
            zone_offset("+5:x").err().and_then(|error| error.vendor_code()),
            Some(1857)
        );
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
            "\"", "9", "é", "DDD", "SSSSS", "Y,YYY", "TZH", "TZM", "TZR", "TZD", "BC", "SYYYY",
            "RM", "DS", "DL", "TS", "SP", "TH", "SPTH", "CC", "YEAR", "IW", "D", "DY",
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
            "BC",
            "+08:00",
            "UTC",
            "XII",
            "Monday",
            "2451514",
            ",",
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
                - 2_440_000 * MICROS_PER_DAY;
            let offset = (next() % 50_000) as i32 - 25_000;
            for purpose in [
                ModelUse::DateInput,
                ModelUse::TimestampInput,
                ModelUse::ZonedInput,
                ModelUse::Output,
            ] {
                if let Ok(parsed) = DatetimeModel::parse(&model, purpose) {
                    if purpose == ModelUse::Output {
                        if let Ok(text) = format_zoned(micros, &parsed, None, Some(offset)) {
                            assert!(text.len() <= parsed.output_bound(), "{model}");
                        }
                    } else {
                        let _ = parse_zoned(&input, &parsed, now());
                    }
                }
            }
        }
    }
}
