use crate::datetime::{self, MICROS_PER_DAY, MICROS_PER_SECOND};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::numeric::Numeric;
use crate::sqlserver_conversion::text_of;
use crate::sqlserver_functions::within_limit;
use crate::sqlserver_types::value_type_name;
use crate::types::Value;

pub(crate) const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

pub(crate) const DAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

const LONGEST_PATTERN: usize = 4000;
const DOUBLE_DIGITS: usize = 15;
const REAL_DIGITS: usize = 7;
const MOST_STANDARD_PRECISION: usize = 99;
const MOST_FRACTION_DIGITS: usize = 7;
const TICKS_PER_MICRO: i64 = 10;
const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SECOND;
const MICROS_PER_HOUR: i64 = 60 * MICROS_PER_MINUTE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Culture {
    UnitedStates,
    Invariant,
}

impl Culture {
    fn currency(self) -> &'static str {
        match self {
            Self::UnitedStates => "$",
            Self::Invariant => "\u{a4}",
        }
    }

    fn percent_spacing(self) -> &'static str {
        match self {
            Self::UnitedStates => "",
            Self::Invariant => " ",
        }
    }

    fn date_pattern(self, letter: char) -> Option<&'static str> {
        Some(match (self, letter) {
            (Self::UnitedStates, 'd') => "M/d/yyyy",
            (Self::UnitedStates, 'D') => "dddd, MMMM d, yyyy",
            (Self::UnitedStates, 'f') => "dddd, MMMM d, yyyy h:mm tt",
            (Self::UnitedStates, 'F' | 'U') => "dddd, MMMM d, yyyy h:mm:ss tt",
            (Self::UnitedStates, 'g') => "M/d/yyyy h:mm tt",
            (Self::UnitedStates, 'G') => "M/d/yyyy h:mm:ss tt",
            (Self::UnitedStates, 'm' | 'M') => "MMMM d",
            (Self::UnitedStates, 't') => "h:mm tt",
            (Self::UnitedStates, 'T') => "h:mm:ss tt",
            (Self::UnitedStates, 'y' | 'Y') => "MMMM yyyy",
            (Self::Invariant, 'd') => "MM/dd/yyyy",
            (Self::Invariant, 'D') => "dddd, dd MMMM yyyy",
            (Self::Invariant, 'f') => "dddd, dd MMMM yyyy HH:mm",
            (Self::Invariant, 'F' | 'U') => "dddd, dd MMMM yyyy HH:mm:ss",
            (Self::Invariant, 'g') => "MM/dd/yyyy HH:mm",
            (Self::Invariant, 'G') => "MM/dd/yyyy HH:mm:ss",
            (Self::Invariant, 'm' | 'M') => "MMMM dd",
            (Self::Invariant, 't') => "HH:mm",
            (Self::Invariant, 'T') => "HH:mm:ss",
            (Self::Invariant, 'y' | 'Y') => "yyyy MMMM",
            (_, 'o' | 'O') => "yyyy'-'MM'-'dd'T'HH':'mm':'ss'.'fffffffK",
            (_, 'r' | 'R') => "ddd, dd MMM yyyy HH':'mm':'ss 'GMT'",
            (_, 's') => "yyyy'-'MM'-'dd'T'HH':'mm':'ss",
            (_, 'u') => "yyyy'-'MM'-'dd HH':'mm':'ss'Z'",
            _ => return None,
        })
    }
}

fn unsupported_culture(name: &str) -> SqlError {
    let looks_like_a_culture = !name.is_empty()
        && name.len() <= 20
        && name
            .split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_alphanumeric()));
    if looks_like_a_culture {
        SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("FORMAT with the culture '{name}' is not supported in this version of EggDB."),
        )
        .with_sqlserver_code(40517)
    } else {
        SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("The culture parameter '{name}' provided in the function call is not supported."),
        )
        .with_sqlserver_code(9818)
    }
}

fn culture_named(name: Option<&str>) -> SqlResult<Culture> {
    let Some(name) = name else {
        return Ok(Culture::UnitedStates);
    };
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Ok(Culture::Invariant);
    }
    if trimmed.eq_ignore_ascii_case("en-us") || trimmed.eq_ignore_ascii_case("en") {
        return Ok(Culture::UnitedStates);
    }
    Err(unsupported_culture(trimmed))
}

fn argument_type_invalid(value: &Value) -> SqlError {
    SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        format!(
            "Argument data type {} is invalid for argument 1 of format function.",
            value_type_name(value)
        ),
    )
    .with_sqlserver_code(8116)
}

enum Number {
    Integer { value: i64, bits: u32 },
    Exact(Numeric),
    Float { value: f64, digits: usize },
}

impl Number {
    fn of(value: &Value) -> Option<Self> {
        Some(match value {
            Value::SmallInt(number) => Self::Integer {
                value: i64::from(*number),
                bits: 16,
            },
            Value::Integer(number) => Self::Integer {
                value: i64::from(*number),
                bits: 32,
            },
            Value::BigInt(number) => Self::Integer {
                value: *number,
                bits: 64,
            },
            Value::Numeric(number) => Self::Exact(number.clone()),
            Value::Real(number) => Self::Float {
                value: f64::from(*number),
                digits: REAL_DIGITS,
            },
            Value::Double(number) => Self::Float {
                value: *number,
                digits: DOUBLE_DIGITS,
            },
            _ => return None,
        })
    }

    fn exact(&self) -> Option<Numeric> {
        match self {
            Self::Integer { value, .. } => Some(Numeric::from_i64(*value)),
            Self::Exact(number) => (!number.is_nan()).then(|| number.clone()),
            Self::Float { value, digits } => {
                if value.is_finite() {
                    Numeric::from_f64(*value, *digits).ok()
                } else {
                    None
                }
            }
        }
    }
}

fn is_negative(number: &Numeric) -> bool {
    number.compare(&Numeric::from_i64(0)) == std::cmp::Ordering::Less
}

pub(crate) fn grouped(digits: &str) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn fixed_parts(number: &Numeric, places: usize) -> Option<(bool, String, String)> {
    let rounded = number.rounded(places as i64).ok()?;
    let negative = is_negative(&rounded);
    let shown = rounded.absolute().to_string();
    let (whole, fraction) = shown.split_once('.').unwrap_or((&shown, ""));
    Some((negative, whole.to_owned(), fraction.to_owned()))
}

fn joined_fixed(whole: &str, fraction: &str, group: bool) -> String {
    let whole = if group {
        grouped(whole)
    } else {
        whole.to_owned()
    };
    if fraction.is_empty() {
        whole
    } else {
        format!("{whole}.{fraction}")
    }
}

fn power_of_ten(exponent: i64) -> Option<Numeric> {
    Numeric::parse(&format!("1e{exponent}")).ok()
}

fn significant(number: &Numeric) -> Option<(String, i64)> {
    let shown = number.absolute().to_string();
    let (whole, fraction) = shown.split_once('.').unwrap_or((&shown, ""));
    let all: String = whole.chars().chain(fraction.chars()).collect();
    let leading = all.len() - all.trim_start_matches('0').len();
    let digits = all.trim_start_matches('0').trim_end_matches('0').to_owned();
    if digits.is_empty() {
        return None;
    }
    let exponent = whole.len() as i64 - leading as i64 - 1;
    Some((digits, exponent))
}

fn scientific(number: &Numeric, places: usize, letter: char, exponent_digits: usize) -> Option<String> {
    let negative = is_negative(number);
    let (mantissa, exponent) = match significant(number) {
        None => ("0".to_owned() + &".".repeat(usize::from(places > 0)) + &"0".repeat(places), 0),
        Some((_, exponent)) => {
            let scaled = number.absolute().multiply(&power_of_ten(-exponent)?).ok()?;
            let rounded = scaled.rounded(places as i64).ok()?;
            if rounded.compare(&Numeric::from_i64(10)) != std::cmp::Ordering::Less {
                let again = scaled.multiply(&power_of_ten(-1)?).ok()?.rounded(places as i64).ok()?;
                (again.to_string(), exponent + 1)
            } else {
                (rounded.to_string(), exponent)
            }
        }
    };
    let sign = if exponent < 0 { '-' } else { '+' };
    Some(format!(
        "{}{mantissa}{letter}{sign}{:0>width$}",
        if negative { "-" } else { "" },
        exponent.unsigned_abs(),
        width = exponent_digits
    ))
}

fn general(number: &Number, precision: Option<usize>, letter: char) -> Option<String> {
    if let (Number::Integer { value, .. }, None) = (number, precision) {
        return Some(value.to_string());
    }
    if let (Number::Exact(exact), None) = (number, precision) {
        return Some(exact.to_string());
    }
    let digits = match (number, precision) {
        (_, Some(precision)) if precision > 0 => precision,
        (Number::Float { digits, .. }, _) => *digits,
        (Number::Integer { .. }, _) => 19,
        (Number::Exact(_), _) => 29,
    };
    let exact = number.exact()?;
    let Some((_, exponent)) = significant(&exact) else {
        return Some("0".to_owned());
    };
    let exponent_letter = if letter == 'g' { 'e' } else { 'E' };
    if exponent < -5 || exponent >= digits as i64 {
        let text = scientific(&exact, digits.saturating_sub(1), exponent_letter, 2)?;
        let (mantissa, rest) = text.split_once(exponent_letter)?;
        let mantissa = if mantissa.contains('.') {
            mantissa.trim_end_matches('0').trim_end_matches('.')
        } else {
            mantissa
        };
        return Some(format!("{mantissa}{exponent_letter}{rest}"));
    }
    let places = (digits as i64 - 1 - exponent).max(0) as usize;
    let (negative, whole, fraction) = fixed_parts(&exact, places)?;
    let fraction = fraction.trim_end_matches('0');
    let body = joined_fixed(&whole, fraction, false);
    Some(if negative { format!("-{body}") } else { body })
}

fn hexadecimal(number: &Number, precision: usize, upper: bool) -> Option<String> {
    let Number::Integer { value, bits } = number else {
        return None;
    };
    let mask = if *bits == 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    let raw = (*value as u64) & mask;
    let text = if upper {
        format!("{raw:X}")
    } else {
        format!("{raw:x}")
    };
    Some(format!("{text:0>precision$}"))
}

fn standard_number(number: &Number, letter: char, precision: Option<usize>, culture: Culture) -> Option<String> {
    let places = |default: usize| precision.unwrap_or(default);
    match letter {
        'c' | 'C' => {
            let (negative, whole, fraction) = fixed_parts(&number.exact()?, places(2))?;
            let body = format!("{}{}", culture.currency(), joined_fixed(&whole, &fraction, true));
            Some(if negative { format!("({body})") } else { body })
        }
        'd' | 'D' => {
            let Number::Integer { value, .. } = number else {
                return None;
            };
            let digits = value.unsigned_abs().to_string();
            let width = places(0);
            let body = format!("{digits:0>width$}");
            Some(if *value < 0 { format!("-{body}") } else { body })
        }
        'e' | 'E' => scientific(&number.exact()?, places(6), letter, 3),
        'f' | 'F' | 'n' | 'N' => {
            let (negative, whole, fraction) = fixed_parts(&number.exact()?, places(2))?;
            let body = joined_fixed(&whole, &fraction, matches!(letter, 'n' | 'N'));
            Some(if negative { format!("-{body}") } else { body })
        }
        'g' | 'G' => general(number, precision, letter),
        'p' | 'P' => {
            let hundred = Numeric::from_i64(100);
            let scaled = number.exact()?.multiply(&hundred).ok()?;
            let (negative, whole, fraction) = fixed_parts(&scaled, places(2))?;
            let body = format!(
                "{}{}%",
                joined_fixed(&whole, &fraction, true),
                culture.percent_spacing()
            );
            Some(if negative { format!("-{body}") } else { body })
        }
        'r' | 'R' => match number {
            Number::Float { .. } => general(number, Some(17), 'G'),
            _ => None,
        },
        'x' | 'X' => hexadecimal(number, places(0), letter == 'X'),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NumberToken {
    Zero,
    Hash,
    Point,
    Group,
    Percent,
    PerMille,
    Exponent {
        letter: char,
        always_signed: bool,
        digits: usize,
    },
    Literal(String),
}

fn literal_until(characters: &[char], at: &mut usize, quote: char) -> Option<String> {
    let mut out = String::new();
    loop {
        let c = *characters.get(*at)?;
        *at += 1;
        if c == quote {
            return Some(out);
        }
        out.push(c);
    }
}

fn number_sections(pattern: &str) -> Option<Vec<Vec<NumberToken>>> {
    let characters: Vec<char> = pattern.chars().collect();
    let mut sections = vec![Vec::new()];
    let mut at = 0;
    while let Some(&c) = characters.get(at) {
        at += 1;
        let token = match c {
            '0' => NumberToken::Zero,
            '#' => NumberToken::Hash,
            '.' => NumberToken::Point,
            ',' => NumberToken::Group,
            '%' => NumberToken::Percent,
            '\u{2030}' => NumberToken::PerMille,
            ';' => {
                sections.push(Vec::new());
                continue;
            }
            '\\' => {
                let escaped = *characters.get(at)?;
                at += 1;
                NumberToken::Literal(escaped.to_string())
            }
            '\'' | '"' => NumberToken::Literal(literal_until(&characters, &mut at, c)?),
            'e' | 'E' => {
                let mut look = at;
                let always_signed = match characters.get(look) {
                    Some('+') => {
                        look += 1;
                        true
                    }
                    Some('-') => {
                        look += 1;
                        false
                    }
                    _ => false,
                };
                let zeros = characters[look.min(characters.len())..]
                    .iter()
                    .take_while(|&&next| next == '0')
                    .count();
                if zeros == 0 {
                    NumberToken::Literal(c.to_string())
                } else {
                    at = look + zeros;
                    NumberToken::Exponent {
                        letter: c,
                        always_signed,
                        digits: zeros,
                    }
                }
            }
            other => NumberToken::Literal(other.to_string()),
        };
        if let Some(section) = sections.last_mut() {
            section.push(token);
        }
    }
    if sections.len() > 3 {
        sections.truncate(3);
    }
    Some(sections)
}

fn is_digit(token: &NumberToken) -> bool {
    matches!(token, NumberToken::Zero | NumberToken::Hash)
}

struct Layout {
    integral_places: usize,
    integral_minimum: usize,
    fraction_places: usize,
    fraction_minimum: usize,
    grouping: bool,
    divide_by_thousands: usize,
    multiply_by: usize,
    exponent: Option<(char, bool, usize)>,
}

fn layout_of(tokens: &[NumberToken]) -> Layout {
    let point = tokens.iter().position(|token| *token == NumberToken::Point);
    let exponent_at = tokens
        .iter()
        .position(|token| matches!(token, NumberToken::Exponent { .. }));
    let digits_end = exponent_at.unwrap_or(tokens.len());
    let integral_end = point.unwrap_or(digits_end).min(digits_end);
    let integral: Vec<&NumberToken> = tokens[..integral_end]
        .iter()
        .filter(|token| is_digit(token))
        .collect();
    let integral_minimum = integral
        .iter()
        .position(|token| **token == NumberToken::Zero)
        .map_or(0, |first| integral.len() - first);
    let fraction: Vec<&NumberToken> = match point {
        Some(point) if point < digits_end => tokens[point + 1..digits_end]
            .iter()
            .filter(|token| is_digit(token))
            .collect(),
        _ => Vec::new(),
    };
    let fraction_minimum = fraction
        .iter()
        .rposition(|token| **token == NumberToken::Zero)
        .map_or(0, |last| last + 1);
    let mut grouping = false;
    let mut divide_by_thousands = 0;
    let first_digit = tokens[..integral_end].iter().position(is_digit);
    let last_digit = tokens[..integral_end].iter().rposition(is_digit);
    for (index, token) in tokens[..integral_end].iter().enumerate() {
        if *token != NumberToken::Group {
            continue;
        }
        match (first_digit, last_digit) {
            (Some(first), Some(last)) if index > first && index < last => grouping = true,
            (Some(_), Some(last)) if index > last => divide_by_thousands += 1,
            _ => {}
        }
    }
    let multiply_by = tokens
        .iter()
        .map(|token| match token {
            NumberToken::Percent => 2,
            NumberToken::PerMille => 3,
            _ => 0,
        })
        .sum();
    let exponent = tokens.iter().find_map(|token| match token {
        NumberToken::Exponent {
            letter,
            always_signed,
            digits,
        } => Some((*letter, *always_signed, *digits)),
        _ => None,
    });
    Layout {
        integral_places: integral.len(),
        integral_minimum,
        fraction_places: fraction.len(),
        fraction_minimum,
        grouping,
        divide_by_thousands,
        multiply_by,
        exponent,
    }
}

fn scaled_number(number: &Numeric, layout: &Layout) -> Option<Numeric> {
    let shift = layout.multiply_by as i64 - 3 * layout.divide_by_thousands as i64;
    if shift == 0 {
        Some(number.clone())
    } else {
        number.multiply(&power_of_ten(shift)?).ok()
    }
}

fn custom_number(number: &Numeric, pattern: &str) -> Option<String> {
    let sections = number_sections(pattern)?;
    let negative = is_negative(number);
    let zero = number.is_zero();
    let (tokens, signed) = match sections.as_slice() {
        [_, _, third, ..] if zero => (third, false),
        [_, second, ..] if negative && !second.is_empty() => (second, false),
        [first, ..] => (first, negative),
        [] => return Some(String::new()),
    };
    let layout = layout_of(tokens);
    let value = scaled_number(&number.absolute(), &layout)?;
    let (mut exponent_value, value) = match layout.exponent {
        Some(_) => match significant(&value) {
            None => (0i64, value),
            Some((_, exponent)) => {
                let shift = exponent - (layout.integral_places.max(1) as i64 - 1);
                (shift, value.multiply(&power_of_ten(-shift)?).ok()?)
            }
        },
        None => (0, value),
    };
    let rounded = value.rounded(layout.fraction_places as i64).ok()?;
    let carried = match layout.exponent {
        Some(_) => {
            let limit = power_of_ten(layout.integral_places.max(1) as i64)?;
            rounded.compare(&limit) != std::cmp::Ordering::Less
        }
        None => false,
    };
    let rounded = if carried {
        exponent_value += 1;
        value
            .multiply(&power_of_ten(-1)?)
            .ok()?
            .rounded(layout.fraction_places as i64)
            .ok()?
    } else {
        rounded
    };
    let shown = rounded.to_string();
    let (whole, fraction) = shown.split_once('.').unwrap_or((&shown, ""));
    let mut whole = whole.trim_start_matches('0').to_owned();
    if whole.len() < layout.integral_minimum {
        whole = format!("{whole:0>width$}", width = layout.integral_minimum);
    }
    let mut fraction = fraction.to_owned();
    while fraction.len() > layout.fraction_minimum && fraction.ends_with('0') {
        fraction.pop();
    }
    let whole_digits: Vec<char> = whole.chars().collect();
    let fraction_digits: Vec<char> = fraction.chars().collect();
    let offset = whole_digits.len() as i64 - layout.integral_places as i64;
    let mut out = String::new();
    if signed && !rounded.is_zero() {
        out.push('-');
    }
    let mut integral_seen = 0usize;
    let mut fraction_seen = 0usize;
    let mut after_point = false;
    let push_digit = |out: &mut String, index: usize| {
        out.push(whole_digits[index]);
        let from_right = whole_digits.len() - 1 - index;
        if layout.grouping && from_right > 0 && from_right % 3 == 0 {
            out.push(',');
        }
    };
    for token in tokens {
        match token {
            NumberToken::Zero | NumberToken::Hash if !after_point => {
                let position = integral_seen as i64 + offset;
                let from = if integral_seen == 0 { 0 } else { position.max(0) };
                if position >= 0 {
                    for index in from as usize..=position as usize {
                        if index < whole_digits.len() {
                            push_digit(&mut out, index);
                        }
                    }
                }
                integral_seen += 1;
            }
            NumberToken::Zero | NumberToken::Hash => {
                if let Some(digit) = fraction_digits.get(fraction_seen) {
                    out.push(*digit);
                }
                fraction_seen += 1;
            }
            NumberToken::Point => {
                after_point = true;
                if layout.integral_places == 0 && !whole_digits.is_empty() {
                    for index in 0..whole_digits.len() {
                        push_digit(&mut out, index);
                    }
                }
                if !fraction_digits.is_empty() {
                    out.push('.');
                }
            }
            NumberToken::Group => {}
            NumberToken::Percent => out.push('%'),
            NumberToken::PerMille => out.push('\u{2030}'),
            NumberToken::Exponent {
                letter,
                always_signed,
                digits,
            } => {
                after_point = true;
                out.push(*letter);
                if exponent_value < 0 {
                    out.push('-');
                } else if *always_signed {
                    out.push('+');
                }
                let shown = format!("{:0>width$}", exponent_value.unsigned_abs(), width = *digits);
                out.push_str(&shown);
            }
            NumberToken::Literal(text) => out.push_str(text),
        }
    }
    Some(out)
}

fn number_text(number: &Number, pattern: &str, culture: Culture) -> Option<String> {
    let mut characters = pattern.chars();
    if let Some(letter) = characters.next()
        && letter.is_ascii_alphabetic()
    {
        let rest = characters.as_str();
        if rest.is_empty() {
            return standard_number(number, letter, None, culture);
        }
        if rest.len() <= 2 && rest.bytes().all(|byte| byte.is_ascii_digit()) {
            let precision: usize = rest.parse().ok()?;
            if precision > MOST_STANDARD_PRECISION {
                return None;
            }
            return standard_number(number, letter, Some(precision), culture);
        }
    }
    custom_number(&number.exact()?, pattern)
}

struct Moment {
    days: i64,
    of_day: i64,
    offset_seconds: Option<i32>,
}

impl Moment {
    fn of(value: &Value) -> SqlResult<Option<Self>> {
        Ok(Some(match value {
            Value::Date(days) => Self {
                days: i64::from(*days),
                of_day: 0,
                offset_seconds: None,
            },
            Value::Timestamp(micros) => Self {
                days: micros.div_euclid(MICROS_PER_DAY),
                of_day: micros.rem_euclid(MICROS_PER_DAY),
                offset_seconds: None,
            },
            Value::TimestampTz(micros) => {
                let local = datetime::utc_to_local(*micros)?;
                Self {
                    days: local.div_euclid(MICROS_PER_DAY),
                    of_day: local.rem_euclid(MICROS_PER_DAY),
                    offset_seconds: Some(datetime::session_offset_at(*micros)),
                }
            }
            _ => return Ok(None),
        }))
    }
}

fn run_length(characters: &[char], at: usize) -> usize {
    let first = characters[at];
    characters[at..].iter().take_while(|&&c| c == first).count()
}

fn offset_text(offset_seconds: i32, width: usize) -> String {
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let minutes = offset_seconds.unsigned_abs() / 60;
    let (hours, minutes) = (minutes / 60, minutes % 60);
    match width {
        1 => format!("{sign}{hours}"),
        2 => format!("{sign}{hours:02}"),
        _ => format!("{sign}{hours:02}:{minutes:02}"),
    }
}

fn ticks_text(of_day: i64, count: usize) -> String {
    let ticks = (of_day % MICROS_PER_SECOND) * TICKS_PER_MICRO;
    let shown = format!("{ticks:07}");
    shown[..count.min(MOST_FRACTION_DIGITS)].to_owned()
}

fn date_text(moment: &Moment, pattern: &str) -> Option<String> {
    let characters: Vec<char> = pattern.chars().collect();
    let (year, month, day) = datetime::civil_from_days(moment.days);
    let weekday = (moment.days + 6).rem_euclid(7) as usize;
    let hour = moment.of_day / MICROS_PER_HOUR;
    let minute = moment.of_day / MICROS_PER_MINUTE % 60;
    let second = moment.of_day / MICROS_PER_SECOND % 60;
    let twelve = match hour % 12 {
        0 => 12,
        other => other,
    };
    let mut out = String::new();
    let mut at = 0;
    while at < characters.len() {
        let c = characters[at];
        let run = run_length(&characters, at);
        let mut step = run;
        match c {
            'd' => out.push_str(&match run {
                1 => day.to_string(),
                2 => format!("{day:02}"),
                3 => DAY_NAMES[weekday][..3].to_owned(),
                _ => DAY_NAMES[weekday].to_owned(),
            }),
            'f' | 'F' => {
                if run > MOST_FRACTION_DIGITS {
                    return None;
                }
                let digits = ticks_text(moment.of_day, run);
                if c == 'F' {
                    let trimmed = digits.trim_end_matches('0');
                    if trimmed.is_empty() && out.ends_with('.') {
                        out.pop();
                    }
                    out.push_str(trimmed);
                } else {
                    out.push_str(&digits);
                }
            }
            'g' => out.push_str("A.D."),
            'h' => out.push_str(&if run == 1 {
                twelve.to_string()
            } else {
                format!("{twelve:02}")
            }),
            'H' => out.push_str(&if run == 1 {
                hour.to_string()
            } else {
                format!("{hour:02}")
            }),
            'K' => {
                step = 1;
                if let Some(offset) = moment.offset_seconds {
                    out.push_str(&offset_text(offset, 3));
                }
            }
            'm' => out.push_str(&if run == 1 {
                minute.to_string()
            } else {
                format!("{minute:02}")
            }),
            'M' => out.push_str(&match run {
                1 => month.to_string(),
                2 => format!("{month:02}"),
                3 => MONTH_NAMES[(month - 1) as usize][..3].to_owned(),
                _ => MONTH_NAMES[(month - 1) as usize].to_owned(),
            }),
            's' => out.push_str(&if run == 1 {
                second.to_string()
            } else {
                format!("{second:02}")
            }),
            't' => {
                let marker = if hour < 12 { "AM" } else { "PM" };
                out.push_str(if run == 1 { &marker[..1] } else { marker });
            }
            'y' => out.push_str(&match run {
                1 => (year % 100).to_string(),
                2 => format!("{:02}", year % 100),
                width => format!("{year:0width$}"),
            }),
            'z' => out.push_str(&offset_text(
                moment.offset_seconds.unwrap_or_else(datetime::session_offset_now),
                run,
            )),
            ':' => {
                step = 1;
                out.push(':');
            }
            '/' => {
                step = 1;
                out.push('/');
            }
            '\'' | '"' => {
                let mut next = at + 1;
                out.push_str(&literal_until(&characters, &mut next, c)?);
                step = next - at;
            }
            '\\' => {
                out.push(*characters.get(at + 1)?);
                step = 2;
            }
            '%' => step = 1,
            other => {
                step = 1;
                out.push(other);
            }
        }
        at += step;
    }
    Some(out)
}

fn datetime_text(moment: &Moment, pattern: &str, culture: Culture) -> Option<String> {
    let mut characters = pattern.chars();
    if let (Some(letter), None) = (characters.next(), characters.next()) {
        return date_text(moment, culture.date_pattern(letter)?);
    }
    date_text(moment, pattern)
}

fn time_span_text(of_day: i64, pattern: &str) -> Option<String> {
    let hours = of_day / MICROS_PER_HOUR;
    let minutes = of_day / MICROS_PER_MINUTE % 60;
    let seconds = of_day / MICROS_PER_SECOND % 60;
    let fraction = of_day % MICROS_PER_SECOND;
    match pattern {
        "c" | "t" | "T" => {
            let base = format!("{hours:02}:{minutes:02}:{seconds:02}");
            return Some(if fraction == 0 {
                base
            } else {
                format!("{base}.{}", ticks_text(of_day, MOST_FRACTION_DIGITS))
            });
        }
        "g" => {
            let base = format!("{hours}:{minutes:02}:{seconds:02}");
            let ticks = ticks_text(of_day, MOST_FRACTION_DIGITS);
            let trimmed = ticks.trim_end_matches('0');
            return Some(if trimmed.is_empty() {
                base
            } else {
                format!("{base}.{trimmed}")
            });
        }
        "G" => {
            return Some(format!(
                "0:{hours:02}:{minutes:02}:{seconds:02}.{}",
                ticks_text(of_day, MOST_FRACTION_DIGITS)
            ));
        }
        _ => {}
    }
    let characters: Vec<char> = pattern.chars().collect();
    let mut out = String::new();
    let mut at = 0;
    while at < characters.len() {
        let c = characters[at];
        let run = run_length(&characters, at);
        let mut step = run;
        match c {
            'd' if run <= 8 => out.push_str(&format!("{:0>run$}", 0)),
            'h' if run <= 2 => out.push_str(&format!("{hours:0>run$}")),
            'm' if run <= 2 => out.push_str(&format!("{minutes:0>run$}")),
            's' if run <= 2 => out.push_str(&format!("{seconds:0>run$}")),
            'f' if run <= MOST_FRACTION_DIGITS => out.push_str(&ticks_text(of_day, run)),
            'F' if run <= MOST_FRACTION_DIGITS => {
                out.push_str(ticks_text(of_day, run).trim_end_matches('0'));
            }
            '\'' | '"' => {
                let mut next = at + 1;
                out.push_str(&literal_until(&characters, &mut next, c)?);
                step = next - at;
            }
            '\\' => {
                out.push(*characters.get(at + 1)?);
                step = 2;
            }
            '%' => step = 1,
            _ => return None,
        }
        at += step;
    }
    Some(out)
}

pub(crate) fn format(arguments: &[Value]) -> SqlResult<Value> {
    let value = arguments.first().unwrap_or(&Value::Null);
    let pattern = arguments.get(1).unwrap_or(&Value::Null);
    let culture_name = match arguments.get(2) {
        None | Some(Value::Null) => None,
        Some(culture) => Some(text_of(culture, None, None)?),
    };
    let culture = culture_named(culture_name.as_deref())?;
    if value.is_null() || pattern.is_null() {
        return Ok(Value::Null);
    }
    let pattern = text_of(pattern, None, None)?;
    if pattern.chars().count() > LONGEST_PATTERN || pattern.is_empty() {
        return Ok(Value::Null);
    }
    let shown = if let Some(number) = Number::of(value) {
        number_text(&number, &pattern, culture)
    } else if let Value::Time(of_day) = value {
        time_span_text(*of_day, &pattern)
    } else if let Some(moment) = Moment::of(value)? {
        datetime_text(&moment, &pattern, culture)
    } else {
        return Err(argument_type_invalid(value));
    };
    Ok(match shown {
        Some(text) => {
            within_limit(text.len())?;
            Value::Text(text)
        }
        None => Value::Null,
    })
}

#[cfg(test)]
mod tests {
    use super::format;
    use crate::datetime::{self, MICROS_PER_DAY, MICROS_PER_SECOND};
    use crate::numeric::Numeric;
    use crate::types::Value;

    fn text(value: &str) -> Value {
        Value::Text(value.to_owned())
    }

    fn shown(arguments: &[Value]) -> String {
        match format(arguments) {
            Ok(Value::Text(text)) => text,
            Ok(Value::Null) => "NULL".to_owned(),
            Ok(other) => format!("{other:?}"),
            Err(error) => format!("error {}", error.vendor_code().unwrap_or(0)),
        }
    }

    fn number(value: &str) -> Value {
        Value::Numeric(Numeric::parse(value).unwrap_or_else(|_| Numeric::nan()))
    }

    fn moment() -> Value {
        let day = datetime::days_from_civil(2020, 7, 4);
        Value::Timestamp(day * MICROS_PER_DAY + (13 * 3600 + 5 * 60 + 9) * MICROS_PER_SECOND + 120_000)
    }

    #[test]
    fn standard_number_formats_follow_the_united_states_culture() {
        let cases = [
            (number("1234.567"), "N", "1,234.57"),
            (number("1234.567"), "N0", "1,235"),
            (number("1234.5"), "C", "$1,234.50"),
            (number("-1234.5"), "C", "($1,234.50)"),
            (number("0.1234"), "P", "12.34%"),
            (number("1234.567"), "F1", "1234.6"),
            (Value::Integer(42), "D5", "00042"),
            (Value::Integer(-42), "D", "-42"),
            (Value::Integer(255), "X", "FF"),
            (Value::Integer(-1), "x", "ffffffff"),
            (number("1234.5678"), "E2", "1.23E+003"),
            (Value::Integer(1234), "G", "1234"),
            (Value::Double(0.1 + 0.2), "G", "0.3"),
        ];
        for (value, pattern, expected) in cases {
            assert_eq!(shown(&[value, text(pattern)]), expected, "{pattern}");
        }
        assert_eq!(shown(&[number("1.5"), text("D")]), "NULL");
    }

    #[test]
    fn custom_number_formats_place_digits_groups_and_sections() {
        let cases = [
            (number("1234.5"), "#,##0.00", "1,234.50"),
            (number("0.5"), "#.##", ".5"),
            (number("5"), "0000", "0005"),
            (number("1234567"), "#,##0,", "1,235"),
            (number("0.256"), "0.0%", "25.6%"),
            (number("-12"), "0;(0)", "(12)"),
            (number("0"), "0;(0);'zero'", "zero"),
            (number("123"), "'#'000", "#123"),
            (number("12345"), "0.00E+00", "1.23E+04"),
            (Value::Integer(0), "#", ""),
        ];
        for (value, pattern, expected) in cases {
            assert_eq!(shown(&[value, text(pattern)]), expected, "{pattern}");
        }
    }

    #[test]
    fn dates_take_standard_and_custom_patterns() {
        let cases = [
            ("d", "7/4/2020"),
            ("D", "Saturday, July 4, 2020"),
            ("yyyy-MM-dd", "2020-07-04"),
            ("dd/MM/yyyy HH:mm:ss.fff", "04/07/2020 13:05:09.120"),
            ("MMM d, yy h:mm tt", "Jul 4, 20 1:05 PM"),
            ("ddd", "Sat"),
            ("s", "2020-07-04T13:05:09"),
            ("'Day' d", "Day 4"),
        ];
        for (pattern, expected) in cases {
            assert_eq!(shown(&[moment(), text(pattern)]), expected, "{pattern}");
        }
        assert_eq!(
            shown(&[moment(), text("d"), text("")]),
            "07/04/2020"
        );
        assert_eq!(shown(&[moment(), text("d"), text("de-DE")]), "error 40517");
        assert_eq!(shown(&[moment(), text("d"), text("??")]), "error 9818");
    }

    #[test]
    fn times_are_time_spans_whose_separators_must_be_escaped() {
        let at = Value::Time((7 * 3600 + 35 * 60) * MICROS_PER_SECOND);
        assert_eq!(shown(&[at.clone(), text("hh\\:mm")]), "07:35");
        assert_eq!(shown(&[at.clone(), text("hh:mm")]), "NULL");
        assert_eq!(shown(&[at, text("c")]), "07:35:00");
    }

    #[test]
    fn other_types_and_hostile_patterns_are_refused_or_null() {
        assert_eq!(shown(&[text("abc"), text("N")]), "error 8116");
        assert_eq!(shown(&[Value::Integer(1), Value::Null]), "NULL");
        assert_eq!(shown(&[Value::Integer(1), text("'unclosed")]), "NULL");
        let long = "0".repeat(5000);
        assert_eq!(shown(&[Value::Integer(1), text(&long)]), "NULL");
        let wide = "0".repeat(3000);
        assert_eq!(shown(&[Value::Integer(1), text(&wide)]).len(), 3000);
        assert_eq!(shown(&[number("1"), text("N99")]).len(), 101);
    }
}
