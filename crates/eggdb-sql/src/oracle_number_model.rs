use crate::error::{SqlError, SqlResult, SqlState};
use crate::numeric::Numeric;
use crate::oracle_functions::{Number, invalid_number, parse_number, within_limit};
use std::cmp::Ordering;

const DOLLAR: &str = "$";
const LOCAL_CURRENCY: &str = "$";
const ISO_CURRENCY: &str = "USD";
const DUAL_CURRENCY: &str = "$";
const CURRENCY_FIELD_WIDTH: usize = 12;
const ROMAN_WIDTH: usize = 15;
const MAX_TEXT_MINIMUM_CHARACTERS: usize = 64;
const MIN_EXPONENT_DIGITS: usize = 2;
const MAX_MULTIPLIER_DIGITS: usize = 400;

pub(crate) fn invalid_format() -> SqlError {
    SqlError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        "invalid number format model",
    )
    .with_oracle_code(1481)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sign {
    Default,
    Leading,
    Trailing,
    TrailingMinus,
    Brackets,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Currency {
    Dollar,
    Local,
    Iso,
    Dual,
}

impl Currency {
    fn symbol(self) -> &'static str {
        match self {
            Self::Dollar => DOLLAR,
            Self::Local => LOCAL_CURRENCY,
            Self::Iso => ISO_CURRENCY,
            Self::Dual => DUAL_CURRENCY,
        }
    }

    fn padded(self) -> bool {
        self != Self::Dollar
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    Fixed,
    Scientific,
    Roman { lower: bool },
    Hexadecimal { lower: bool, zeros: bool, digits: usize },
    TextMinimum { scientific: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NumberModel {
    fill_mode: bool,
    sign: Sign,
    currency: Option<(Currency, bool)>,
    blank_when_zero: bool,
    integer: Vec<char>,
    fraction: Vec<char>,
    point: bool,
    multiplier_digits: usize,
    shape: Shape,
    width: usize,
}

impl NumberModel {
    fn new(width: usize, fill_mode: bool, shape: Shape) -> Self {
        Self {
            fill_mode,
            sign: Sign::Default,
            currency: None,
            blank_when_zero: false,
            integer: Vec::new(),
            fraction: Vec::new(),
            point: false,
            multiplier_digits: 0,
            shape,
            width,
        }
    }

    fn digit_slots(&self) -> usize {
        self.integer.iter().filter(|slot| **slot != ',').count()
    }
}

fn special_shape(rest: &str) -> Option<SqlResult<Shape>> {
    let upper = rest.to_ascii_uppercase();
    if upper.starts_with("TM") {
        return Some(match upper.as_str() {
            "TM" | "TM9" => Ok(Shape::TextMinimum { scientific: false }),
            "TME" => Ok(Shape::TextMinimum { scientific: true }),
            _ => Err(invalid_format()),
        });
    }
    if upper == "RN" {
        return Some(Ok(Shape::Roman {
            lower: rest.starts_with('r'),
        }));
    }
    let zeros = rest.chars().take_while(|character| *character == '0').count();
    let hex = &rest[zeros..];
    if !hex.is_empty() && hex.chars().all(|character| matches!(character, 'X' | 'x')) {
        return Some(Ok(Shape::Hexadecimal {
            lower: hex.starts_with('x'),
            zeros: zeros > 0,
            digits: rest.len(),
        }));
    }
    None
}

fn model_of(format: &str) -> SqlResult<NumberModel> {
    let (fill_mode, rest) = match format.get(..2) {
        Some(prefix) if prefix.eq_ignore_ascii_case("FM") => (true, &format[2..]),
        _ => (false, format),
    };
    let width = rest.len();
    if let Some(shape) = special_shape(rest) {
        return Ok(NumberModel::new(width, fill_mode, shape?));
    }
    let upper: Vec<char> = rest.chars().map(|character| character.to_ascii_uppercase()).collect();
    let mut model = NumberModel::new(width, fill_mode, Shape::Fixed);
    let mut multiplier = false;
    let mut at = 0;
    let digits_seen = |model: &NumberModel| !model.integer.is_empty() || model.point;
    while at < upper.len() {
        let next = |word: &str| {
            word.chars()
                .enumerate()
                .all(|(offset, wanted)| upper.get(at + offset) == Some(&wanted))
        };
        let at_end = |length: usize| at + length == upper.len();
        if model.shape == Shape::Scientific && !(next("MI") || next("PR") || next("S")) {
            return Err(invalid_format());
        }
        if next("EEEE") {
            if !digits_seen(&model) || model.integer.contains(&',') || multiplier {
                return Err(invalid_format());
            }
            model.shape = Shape::Scientific;
            at += 4;
            continue;
        }
        if next("MI") || next("PR") {
            if !at_end(2) || model.sign != Sign::Default {
                return Err(invalid_format());
            }
            model.sign = if next("MI") {
                Sign::TrailingMinus
            } else {
                Sign::Brackets
            };
            at += 2;
            continue;
        }
        let character = upper[at];
        at += 1;
        match character {
            'S' if model.sign != Sign::Default => return Err(invalid_format()),
            'S' if at == 1 => model.sign = Sign::Leading,
            'S' if at == upper.len() => model.sign = Sign::Trailing,
            '$' | 'L' | 'C' | 'U' if model.currency.is_some() => return Err(invalid_format()),
            '$' => model.currency = Some((Currency::Dollar, false)),
            'L' | 'C' | 'U' => {
                let currency = match character {
                    'L' => Currency::Local,
                    'C' => Currency::Iso,
                    _ => Currency::Dual,
                };
                model.currency = Some((currency, digits_seen(&model)));
            }
            'B' if !model.blank_when_zero => model.blank_when_zero = true,
            '9' | '0' if multiplier => model.multiplier_digits += 1,
            '9' | '0' if model.point => model.fraction.push(character),
            '9' | '0' => model.integer.push(character),
            ',' | 'G' if !model.point && !multiplier && !model.integer.is_empty() => {
                model.integer.push(',');
            }
            '.' | 'D' if !model.point && !multiplier => model.point = true,
            'V' if !model.point && !multiplier => multiplier = true,
            _ => return Err(invalid_format()),
        }
        if model
            .currency
            .is_some_and(|(_, trailing)| trailing)
            && matches!(character, '9' | '0' | ',' | 'G' | '.' | 'D' | 'V')
        {
            return Err(invalid_format());
        }
    }
    if model.integer.is_empty() && model.fraction.is_empty() && model.multiplier_digits == 0 {
        return Err(invalid_format());
    }
    if model.multiplier_digits > MAX_MULTIPLIER_DIGITS {
        return Err(invalid_format());
    }
    Ok(model)
}

fn fixed_digits(number: &Number, places: usize) -> SqlResult<(bool, String)> {
    Ok(match number {
        Number::Integer(whole) => {
            let mut text = whole.unsigned_abs().to_string();
            if places > 0 {
                text.push('.');
                text.extend(std::iter::repeat_n('0', places));
            }
            (*whole < 0, text)
        }
        Number::Decimal(decimal) => {
            let rounded = decimal.rounded(places as i64)?;
            let negative = rounded.compare(&Numeric::from_i64(0)) == Ordering::Less;
            (negative, rounded.absolute().to_string())
        }
        Number::Float(float) => (*float < 0.0, format!("{:.*}", places, float.abs())),
    })
}

fn whole_number(number: &Number) -> SqlResult<Option<i128>> {
    Ok(match number {
        Number::Integer(whole) => Some(i128::from(*whole)),
        Number::Decimal(decimal) => decimal.rounded(0)?.integral_value(),
        Number::Float(float) => {
            let rounded = float.round();
            (rounded.is_finite() && rounded.abs() < 1e30).then_some(rounded as i128)
        }
    })
}

fn overflowed(width: usize) -> String {
    "#".repeat(width + 1)
}

fn scaled_by_multiplier(number: &Number, digits: usize) -> SqlResult<Number> {
    if digits == 0 {
        return Ok(match number {
            Number::Integer(whole) => Number::Integer(*whole),
            Number::Decimal(decimal) => Number::Decimal(decimal.clone()),
            Number::Float(float) => Number::Float(*float),
        });
    }
    let factor = Numeric::parse(&format!("1{}", "0".repeat(digits)))?;
    let exact = match number.exact() {
        Some(exact) => exact,
        None => Numeric::from_f64(number.float(), 17)?,
    };
    Ok(Number::Decimal(exact.multiply(&factor)?.rounded(0)?))
}

fn without_trailing_zeros(fraction: &str, slots: &[char]) -> String {
    let optional = slots.iter().rev().take_while(|slot| **slot == '9').count();
    let zeros = fraction
        .chars()
        .rev()
        .take(optional)
        .take_while(|digit| *digit == '0')
        .count();
    fraction[..fraction.len() - zeros].to_owned()
}

fn leading_currency(model: &NumberModel) -> Option<Currency> {
    match model.currency {
        Some((Currency::Dollar, _)) => Some(Currency::Dollar),
        Some((currency, false)) => Some(currency),
        _ => None,
    }
}

fn fixed(number: &Number, model: &NumberModel) -> SqlResult<String> {
    let scaled;
    let (number, slots_model) = if model.multiplier_digits > 0 {
        scaled = scaled_by_multiplier(number, model.multiplier_digits)?;
        let mut widened = model.clone();
        widened
            .integer
            .extend(std::iter::repeat_n('9', model.multiplier_digits));
        widened.fraction.clear();
        widened.point = false;
        (&scaled, widened)
    } else {
        (number, model.clone())
    };
    let model = &slots_model;
    let (negative, digits) = fixed_digits(number, model.fraction.len())?;
    let (whole, fraction) = digits.split_once('.').unwrap_or((&digits, ""));
    let slots = model.digit_slots();
    let zero_integer = whole == "0";
    let whole: Vec<char> = if zero_integer && (!model.fraction.is_empty() || model.blank_when_zero) {
        Vec::new()
    } else {
        whole.chars().collect()
    };
    if whole.len() > slots {
        return Ok(overflowed(model.width));
    }
    let first_zero = if model.blank_when_zero && zero_integer {
        None
    } else {
        model.integer.iter().position(|slot| *slot == '0')
    };
    let blanks = slots - whole.len();
    let mut body = Vec::with_capacity(model.integer.len());
    let mut digit_slot = 0;
    let mut printing = false;
    for (index, slot) in model.integer.iter().enumerate() {
        if *slot == ',' {
            body.push(if printing { ',' } else { ' ' });
            continue;
        }
        if digit_slot >= blanks {
            body.push(whole[digit_slot - blanks]);
            printing = true;
        } else if first_zero.is_some_and(|zero| index >= zero) {
            body.push('0');
            printing = true;
        } else {
            body.push(' ');
        }
        digit_slot += 1;
    }
    let mut text: String = body.iter().collect();
    let first_printed = text.find(|c: char| c != ' ').unwrap_or(text.len());
    let currency = leading_currency(model);
    let mut prefix = String::new();
    if let Some(currency) = currency
        && currency.padded()
        && !model.fill_mode
    {
        let padding = CURRENCY_FIELD_WIDTH.saturating_sub(currency.symbol().len());
        prefix.extend(std::iter::repeat_n(' ', padding));
    }
    match model.sign {
        Sign::Leading => prefix.push(if negative { '-' } else { '+' }),
        Sign::Default => prefix.push(if negative { '-' } else { ' ' }),
        Sign::Brackets => prefix.push(if negative { '<' } else { ' ' }),
        Sign::Trailing | Sign::TrailingMinus => {}
    }
    if let Some(currency) = currency {
        prefix.push_str(currency.symbol());
    }
    text.insert_str(first_printed, &prefix);
    if model.point {
        text.push('.');
        if model.fill_mode {
            text.push_str(&without_trailing_zeros(fraction, &model.fraction));
        } else {
            text.push_str(fraction);
        }
    }
    if let Some((currency, true)) = model.currency
        && currency != Currency::Dollar
    {
        text.push_str(currency.symbol());
    }
    match model.sign {
        Sign::Trailing => text.push(if negative { '-' } else { '+' }),
        Sign::TrailingMinus => text.push(if negative { '-' } else { ' ' }),
        Sign::Brackets => text.push(if negative { '>' } else { ' ' }),
        Sign::Leading | Sign::Default => {}
    }
    if model.fill_mode {
        return Ok(text.trim().to_owned());
    }
    Ok(text)
}

fn significant_digits(number: &Number) -> (Vec<u8>, i64) {
    let text = match number {
        Number::Float(float) => {
            let written = format!("{:e}", float.abs());
            let (mantissa, exponent) = written.split_once('e').unwrap_or((&written, "0"));
            let digits: Vec<u8> = mantissa.bytes().filter(u8::is_ascii_digit).map(|digit| digit - b'0').collect();
            let exponent = exponent.parse::<i64>().unwrap_or(0);
            return if digits.iter().all(|digit| *digit == 0) {
                (vec![0], 0)
            } else {
                (digits, exponent)
            };
        }
        Number::Integer(whole) => whole.unsigned_abs().to_string(),
        Number::Decimal(decimal) => decimal.absolute().to_string(),
    };
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    let all: Vec<u8> = whole
        .bytes()
        .chain(fraction.bytes())
        .filter(u8::is_ascii_digit)
        .map(|digit| digit - b'0')
        .collect();
    let Some(first) = all.iter().position(|digit| *digit != 0) else {
        return (vec![0], 0);
    };
    let exponent = whole.len() as i64 - 1 - first as i64;
    let mut digits = all[first..].to_vec();
    while digits.len() > 1 && digits.last() == Some(&0) {
        digits.pop();
    }
    (digits, exponent)
}

fn rounded_digits(mut digits: Vec<u8>, mut exponent: i64, kept: usize) -> (Vec<u8>, i64) {
    if digits.len() > kept {
        let round_up = digits[kept] >= 5;
        digits.truncate(kept);
        if round_up {
            let mut index = kept;
            loop {
                if index == 0 {
                    digits.insert(0, 1);
                    digits.truncate(kept);
                    exponent += 1;
                    break;
                }
                index -= 1;
                if digits[index] == 9 {
                    digits[index] = 0;
                } else {
                    digits[index] += 1;
                    break;
                }
            }
        }
    }
    digits.resize(kept, 0);
    (digits, exponent)
}

fn negative_number(number: &Number) -> bool {
    match number {
        Number::Integer(whole) => *whole < 0,
        Number::Decimal(decimal) => decimal.compare(&Numeric::from_i64(0)) == Ordering::Less,
        Number::Float(float) => *float < 0.0,
    }
}

fn scientific_text(number: &Number, fraction_digits: Option<usize>) -> String {
    let (digits, exponent) = significant_digits(number);
    let kept = fraction_digits.map_or(digits.len(), |fraction| fraction + 1);
    let zero = digits == [0];
    let (digits, exponent) = rounded_digits(digits, if zero { 0 } else { exponent }, kept);
    let mut text = String::new();
    text.push(char::from(b'0' + digits[0]));
    if digits.len() > 1 {
        text.push('.');
        text.extend(digits[1..].iter().map(|digit| char::from(b'0' + digit)));
    }
    text.push('E');
    text.push(if exponent < 0 { '-' } else { '+' });
    text.push_str(&format!(
        "{:0width$}",
        exponent.unsigned_abs(),
        width = MIN_EXPONENT_DIGITS
    ));
    text
}

fn scientific(number: &Number, model: &NumberModel) -> String {
    let negative = negative_number(number);
    let body = scientific_text(number, Some(model.fraction.len()));
    let mut text = String::new();
    match model.sign {
        Sign::Default if model.fill_mode => {
            if negative {
                text.push('-');
            }
        }
        Sign::Default => {
            text.push(' ');
            text.push(if negative { '-' } else { ' ' });
        }
        Sign::Leading => text.push(if negative { '-' } else { '+' }),
        Sign::Brackets => text.push(if negative { '<' } else { ' ' }),
        Sign::Trailing | Sign::TrailingMinus => {}
    }
    text.push_str(&body);
    match model.sign {
        Sign::Trailing => text.push(if negative { '-' } else { '+' }),
        Sign::TrailingMinus => text.push(if negative { '-' } else { ' ' }),
        Sign::Brackets => text.push(if negative { '>' } else { ' ' }),
        Sign::Leading | Sign::Default => {}
    }
    text
}

fn roman(number: &Number, lower: bool, fill_mode: bool) -> SqlResult<String> {
    let numeral = whole_number(number)?
        .and_then(|whole| u32::try_from(whole).ok())
        .and_then(crate::oracle_numerals::roman);
    let Some(numeral) = numeral else {
        return Ok("#".repeat(ROMAN_WIDTH));
    };
    let numeral = if lower {
        numeral.to_ascii_lowercase()
    } else {
        numeral
    };
    Ok(if fill_mode {
        numeral
    } else {
        format!("{numeral:>width$}", width = ROMAN_WIDTH)
    })
}

fn hexadecimal(number: &Number, shape: (bool, bool, usize), fill_mode: bool) -> SqlResult<String> {
    let (lower, zeros, digits) = shape;
    let Some(whole) = whole_number(number)?.filter(|whole| *whole >= 0) else {
        return Ok(overflowed(digits));
    };
    let written = if lower {
        format!("{whole:x}")
    } else {
        format!("{whole:X}")
    };
    if written.len() > digits {
        return Ok(overflowed(digits));
    }
    Ok(if zeros {
        format!("{written:0>digits$}")
    } else if fill_mode {
        written
    } else {
        format!(" {written:>digits$}")
    })
}

fn text_minimum(number: &Number, scientific_form: bool) -> String {
    let fixed_text = match number {
        Number::Integer(whole) => whole.to_string(),
        Number::Decimal(decimal) => crate::dialect::oracle_numeric_text(decimal),
        Number::Float(float) => crate::oracle_functions::number_text(*float),
    };
    if !scientific_form && fixed_text.len() <= MAX_TEXT_MINIMUM_CHARACTERS {
        return fixed_text;
    }
    let body = scientific_text(number, None);
    if negative_number(number) {
        format!("-{body}")
    } else {
        body
    }
}

pub(crate) fn formatted(number: &Number, format: &str) -> SqlResult<String> {
    let model = model_of(format)?;
    within_limit(
        format
            .len()
            .saturating_mul(2)
            .saturating_add(CURRENCY_FIELD_WIDTH + ROMAN_WIDTH + MAX_TEXT_MINIMUM_CHARACTERS),
    )?;
    match model.shape {
        Shape::Fixed => fixed(number, &model),
        Shape::Scientific => Ok(scientific(number, &model)),
        Shape::Roman { lower } => roman(number, lower, model.fill_mode),
        Shape::Hexadecimal {
            lower,
            zeros,
            digits,
        } => hexadecimal(number, (lower, zeros, digits), model.fill_mode),
        Shape::TextMinimum { scientific } => Ok(text_minimum(number, scientific)),
    }
}

fn without_currency<'t>(text: &'t str, model: &NumberModel) -> &'t str {
    let Some((currency, trailing)) = model.currency else {
        return text;
    };
    let symbol = currency.symbol();
    let stripped = if trailing && currency != Currency::Dollar {
        text.strip_suffix(symbol)
    } else {
        text.strip_prefix(symbol)
    };
    stripped.unwrap_or(text).trim()
}

fn signed_body<'t>(text: &'t str, model: &NumberModel) -> SqlResult<(bool, &'t str)> {
    let mut rest = text.trim();
    let negative = match model.sign {
        Sign::Leading => {
            let negative = match rest.chars().next() {
                Some('-') => true,
                Some('+') => false,
                _ => return Err(invalid_number()),
            };
            rest = &rest[1..];
            negative
        }
        Sign::Trailing => {
            let negative = match rest.chars().last() {
                Some('-') => true,
                Some('+') => false,
                _ => return Err(invalid_number()),
            };
            rest = &rest[..rest.len() - 1];
            negative
        }
        Sign::TrailingMinus => {
            if let Some(body) = rest.strip_suffix('-') {
                rest = body;
                true
            } else {
                rest = rest.strip_suffix('+').unwrap_or(rest);
                false
            }
        }
        Sign::Brackets => match rest.strip_prefix('<').and_then(|body| body.strip_suffix('>')) {
            Some(body) => {
                rest = body;
                true
            }
            None => false,
        },
        Sign::Default => match rest.strip_prefix('-') {
            Some(body) => {
                rest = body;
                true
            }
            None => false,
        },
    };
    Ok((negative, rest.trim()))
}

fn hexadecimal_value(text: &str, digits: usize) -> SqlResult<Number> {
    let trimmed = text.trim();
    if trimmed.is_empty()
        || trimmed.len() > digits
        || !trimmed.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid_number());
    }
    let value = u128::from_str_radix(trimmed, 16).map_err(|_| invalid_number())?;
    Ok(match i64::try_from(value) {
        Ok(whole) => Number::Integer(whole),
        Err(_) => Number::Decimal(Numeric::parse(&value.to_string())?),
    })
}

pub(crate) fn number_by_model(text: &str, format: &str) -> SqlResult<Number> {
    let model = model_of(format)?;
    match model.shape {
        Shape::TextMinimum { .. } => return parse_number(text),
        Shape::Roman { .. } => {
            return crate::oracle_numerals::roman_value(text.trim())
                .map(|value| Number::Integer(i64::from(value)))
                .ok_or_else(invalid_number);
        }
        Shape::Hexadecimal { digits, .. } => return hexadecimal_value(text, digits),
        Shape::Fixed | Shape::Scientific => {}
    }
    if model.multiplier_digits > 0 {
        return Err(invalid_format());
    }
    let (negative, rest) = signed_body(text, &model)?;
    let rest = without_currency(rest, &model);
    let (negative, rest) = match (negative, model.sign, rest.strip_prefix('-')) {
        (false, Sign::Default, Some(body)) => (true, body),
        _ => (negative, rest),
    };
    let sign = if negative { "-" } else { "" };
    if model.shape == Shape::Scientific {
        if rest.starts_with(['-', '+']) {
            return Err(invalid_number());
        }
        return parse_number(&format!("{sign}{rest}"));
    }
    let (whole, fraction) = match rest.split_once('.') {
        Some(_) if !model.point => return Err(invalid_number()),
        Some((whole, fraction)) => (whole, fraction),
        None => (rest, ""),
    };
    let grouped = model.integer.contains(&',');
    let whole_ok = whole
        .chars()
        .all(|character| character.is_ascii_digit() || (grouped && character == ','));
    let digits: String = whole.chars().filter(char::is_ascii_digit).collect();
    if !whole_ok
        || !fraction.chars().all(|character| character.is_ascii_digit())
        || digits.len() > model.digit_slots()
        || fraction.len() > model.fraction.len()
        || (digits.is_empty() && fraction.is_empty())
    {
        return Err(invalid_number());
    }
    let digits = if digits.is_empty() { "0" } else { &digits };
    parse_number(&if fraction.is_empty() {
        format!("{sign}{digits}")
    } else {
        format!("{sign}{digits}.{fraction}")
    })
}

#[cfg(test)]
mod tests {
    use super::{formatted, number_by_model};
    use crate::error::SqlResult;
    use crate::oracle_functions::parse_number;

    fn shaped(number: &str, format: &str) -> SqlResult<String> {
        formatted(&parse_number(number)?, format)
    }

    fn read(text: &str, format: &str) -> Result<String, u16> {
        number_by_model(text, format)
            .map(|number| match number.value() {
                crate::types::Value::Numeric(decimal) => crate::dialect::oracle_numeric_text(&decimal),
                other => format!("{other:?}"),
            })
            .map_err(|error| error.vendor_code().unwrap_or(0))
    }

    #[test]
    fn fixed_models_place_digits_signs_and_separators() {
        assert_eq!(shaped("1234.5", "9,999.99").as_deref(), Ok(" 1,234.50"));
        assert_eq!(shaped("-12", "999").as_deref(), Ok(" -12"));
        assert_eq!(shaped("7", "000").as_deref(), Ok(" 007"));
        assert_eq!(shaped("0.5", "9.99").as_deref(), Ok("  .50"));
        assert_eq!(shaped("0", "999").as_deref(), Ok("   0"));
        assert_eq!(shaped("1.5", "FM9.99").as_deref(), Ok("1.5"));
        assert_eq!(shaped("12345", "999").as_deref(), Ok("####"));
        assert_eq!(shaped("12", "$99").as_deref(), Ok(" $12"));
        assert_eq!(shaped("12", "S99").as_deref(), Ok("+12"));
        assert_eq!(shaped("-12", "99MI").as_deref(), Ok("12-"));
        assert_eq!(shaped("1234567890", "9999999999S").as_deref(), Ok("1234567890+"));
        assert_eq!(shaped("-12", "99PR").as_deref(), Ok("<12>"));
        assert_eq!(shaped("12", "99PR").as_deref(), Ok(" 12 "));
        assert_eq!(shaped("12.345", "999V99").as_deref(), Ok("  1235"));
        assert_eq!(shaped("0", "B999").as_deref(), Ok("    "));
        assert_eq!(shaped("0.5", "B0.99").as_deref(), Ok("  .50"));
        assert!(shaped("1", "X9").is_err());
        assert!(shaped("1", "9MI9").is_err());
        assert!(shaped("1", ",99").is_err());
    }

    #[test]
    fn the_results_of_number_conversions_table_holds() {
        assert_eq!(shaped("0", "99.99").as_deref(), Ok("   .00"));
        assert_eq!(shaped("0.1", "99.99").as_deref(), Ok("   .10"));
        assert_eq!(shaped("-0.2", "99.99").as_deref(), Ok("  -.20"));
        assert_eq!(shaped("0", "90.99").as_deref(), Ok("  0.00"));
        assert_eq!(shaped("123.456", "999.999").as_deref(), Ok(" 123.456"));
        assert_eq!(shaped("-123.456", "999.999").as_deref(), Ok("-123.456"));
        assert_eq!(shaped("123.456", "9.9EEEE").as_deref(), Ok("  1.2E+02"));
        assert_eq!(shaped("1E+123", "9.9EEEE").as_deref(), Ok("  1.0E+123"));
        assert_eq!(shaped("123.456", "FM9.9EEEE").as_deref(), Ok("1.2E+02"));
        assert_eq!(shaped("123.45", "FM999.009").as_deref(), Ok("123.45"));
        assert_eq!(shaped("123.0", "FM999.009").as_deref(), Ok("123.00"));
        assert_eq!(
            shaped("123.45", "L999.99").as_deref(),
            Ok("            $123.45")
        );
        assert_eq!(shaped("123.45", "FML999.99").as_deref(), Ok("$123.45"));
        assert_eq!(shaped("123.45", "FMC999.99").as_deref(), Ok("USD123.45"));
        assert_eq!(shaped("123.45", "FM999.99L").as_deref(), Ok("123.45$"));
    }

    #[test]
    fn roman_hexadecimal_scientific_and_text_minimum_shapes_write_as_documented() {
        assert_eq!(shaped("14", "RN").as_deref(), Ok("            XIV"));
        assert_eq!(shaped("14", "FMrn").as_deref(), Ok("xiv"));
        assert_eq!(shaped("4000", "RN").as_deref(), Ok("###############"));
        assert_eq!(shaped("255", "XXXX").as_deref(), Ok("   FF"));
        assert_eq!(shaped("255", "FMxx").as_deref(), Ok("ff"));
        assert_eq!(shaped("255", "0XXX").as_deref(), Ok("00FF"));
        assert_eq!(shaped("256", "XX").as_deref(), Ok("###"));
        assert_eq!(shaped("-1", "XX").as_deref(), Ok("###"));
        assert_eq!(shaped("0.000123", "9.99EEEE").as_deref(), Ok("  1.23E-04"));
        assert_eq!(shaped("-9.996", "9.99EEEE").as_deref(), Ok(" -1.00E+01"));
        assert_eq!(shaped("0", "9.9EEEE").as_deref(), Ok("  0.0E+00"));
        assert_eq!(shaped("123.456", "TM").as_deref(), Ok("123.456"));
        assert_eq!(shaped("0.5", "TM9").as_deref(), Ok(".5"));
        assert_eq!(shaped("123.456", "TME").as_deref(), Ok("1.23456E+02"));
        assert!(shaped("1", "TMX").is_err());
        assert!(shaped("1", "9EEEE9").is_err());
    }

    #[test]
    fn to_number_reads_the_new_elements() {
        assert_eq!(read("FF", "XX").as_deref(), Ok("BigInt(255)"));
        assert_eq!(read("ff", "XX").as_deref(), Ok("BigInt(255)"));
        assert_eq!(read("FFF", "XX"), Err(1722));
        assert_eq!(read("1.2E+03", "9.9EEEE").as_deref(), Ok("BigInt(1200)"));
        assert_eq!(read("<12>", "99PR").as_deref(), Ok("BigInt(-12)"));
        assert_eq!(read("12-", "99S").as_deref(), Ok("BigInt(-12)"));
        assert_eq!(read("$12.50", "L99.99").as_deref(), Ok("12.5"));
        assert_eq!(read("USD12", "C99").as_deref(), Ok("BigInt(12)"));
        assert_eq!(read("XIV", "RN").as_deref(), Ok("BigInt(14)"));
        assert_eq!(read("XIIII", "RN"), Err(1722));
        assert_eq!(read("12", "99V9"), Err(1481));
        assert_eq!(read("12.5", "TM").as_deref(), Ok("12.5"));
    }

    #[test]
    fn seeded_random_number_models_never_panic() {
        let cases: usize = std::env::var("EGGDB_FUZZ_CASES")
            .ok()
            .and_then(|cases| cases.parse().ok())
            .unwrap_or(3000);
        let mut state: u64 = std::env::var("EGGDB_FUZZ_SEED")
            .ok()
            .and_then(|seed| seed.parse().ok())
            .unwrap_or(0x0042_5eef);
        let pieces = [
            "9", "0", ".", "D", ",", "G", "$", "S", "MI", "PR", "FM", "X", "x", "EEEE", "RN", "rn",
            "L", "C", "U", "V", "B", "TM", "TME", "-", "1", "e", " ", "<", ">", "é",
        ];
        let numbers = ["0", "-1", "12.345", "1e100", "-0.0001", "255", "99999999999999999999"];
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..cases {
            let mut model = String::new();
            let mut input = String::new();
            for _ in 0..next() % 7 {
                model.push_str(pieces[next() as usize % pieces.len()]);
                input.push_str(pieces[next() as usize % pieces.len()]);
            }
            let number = numbers[next() as usize % numbers.len()];
            let _ = shaped(number, &model);
            let _ = number_by_model(&input, &model);
            let _ = number_by_model(number, &model);
        }
    }
}
