use crate::error::{SqlError, SqlResult, SqlState};
use crate::sqlite_functions::MAX_TEXT_BYTES;
use crate::types::Value;
use std::cmp::Ordering;

const MAX_UTF8_CHAR_BYTES: usize = 4;

fn too_long() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        "result of string concatenation is too long",
    )
    .with_oracle_code(1489)
}

fn within_limit(bytes: usize) -> SqlResult<()> {
    if bytes > MAX_TEXT_BYTES {
        Err(too_long())
    } else {
        Ok(())
    }
}

fn invalid_number() -> SqlError {
    SqlError::new(SqlState::INVALID_TEXT_REPRESENTATION, "invalid number").with_oracle_code(1722)
}

fn out_of_range(argument: &str) -> SqlError {
    SqlError::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        format!("argument '{argument}' is out of range"),
    )
    .with_oracle_code(1428)
}

fn divisor_is_zero() -> SqlError {
    SqlError::new(SqlState::DIVISION_BY_ZERO, "divisor is equal to zero").with_oracle_code(1476)
}

fn numeric_overflow() -> SqlError {
    SqlError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "numeric overflow").with_oracle_code(1426)
}

fn invalid_format() -> SqlError {
    SqlError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        "invalid number format model",
    )
    .with_oracle_code(1481)
}

/// `number` as Oracle converts a number to text: no trailing zeros, no leading zero before
/// the decimal point (`.5`), and `Nan`, `Inf` and `-Inf` for the special values.
pub(crate) fn number_text(number: f64) -> String {
    if number.is_nan() {
        return "Nan".to_owned();
    }
    if number.is_infinite() {
        return if number > 0.0 { "Inf" } else { "-Inf" }.to_owned();
    }
    if number == 0.0 {
        return "0".to_owned();
    }
    let text = format!("{number}");
    if let Some(rest) = text.strip_prefix("0.") {
        return format!(".{rest}");
    }
    if let Some(rest) = text.strip_prefix("-0.") {
        return format!("-.{rest}");
    }
    text
}

fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Text(text) => Some(text.clone()),
        Value::Real(number) => Some(number_text(f64::from(*number))),
        Value::Double(number) => Some(number_text(*number)),
        Value::Boolean(flag) => Some(if *flag { "TRUE" } else { "FALSE" }.to_owned()),
        other => other.to_text(),
    }
}

fn text_or_null(text: String) -> Value {
    if text.is_empty() {
        Value::Null
    } else {
        Value::Text(text)
    }
}

enum Number {
    Integer(i64),
    Float(f64),
}

impl Number {
    fn float(&self) -> f64 {
        match self {
            Self::Integer(number) => *number as f64,
            Self::Float(number) => *number,
        }
    }

    fn value(self) -> Value {
        match self {
            Self::Integer(number) => Value::BigInt(number),
            Self::Float(number) => Value::Double(number),
        }
    }
}

fn parse_number(text: &str) -> SqlResult<Number> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(invalid_number());
    }
    if let Ok(number) = trimmed.parse::<i64>() {
        return Ok(Number::Integer(number));
    }
    let numeric = trimmed
        .bytes()
        .all(|byte| byte.is_ascii_digit() || b"+-.eE".contains(&byte))
        && trimmed.bytes().any(|byte| byte.is_ascii_digit());
    match trimmed.parse::<f64>() {
        Ok(number) if numeric && number.is_finite() => Ok(Number::Float(number)),
        _ => Err(invalid_number()),
    }
}

fn number_of(value: &Value) -> SqlResult<Option<Number>> {
    Ok(Some(match value {
        Value::Null => return Ok(None),
        Value::Boolean(flag) => Number::Integer(i64::from(*flag)),
        Value::SmallInt(number) => Number::Integer(i64::from(*number)),
        Value::Integer(number) => Number::Integer(i64::from(*number)),
        Value::BigInt(number) => Number::Integer(*number),
        Value::Real(number) => Number::Float(f64::from(*number)),
        Value::Double(number) => Number::Float(*number),
        Value::Text(text) => parse_number(text)?,
    }))
}

fn whole_of(value: &Value) -> SqlResult<Option<i64>> {
    Ok(number_of(value)?.map(|number| match number {
        Number::Integer(number) => number,
        Number::Float(number) => number.trunc() as i64,
    }))
}

fn concatenated(values: &[Value]) -> SqlResult<Value> {
    let parts: Vec<String> = values.iter().filter_map(text_of).collect();
    within_limit(parts.iter().map(String::len).sum())?;
    Ok(text_or_null(parts.concat()))
}

fn divided(left: &Value, right: &Value) -> SqlResult<Value> {
    let (Some(left), Some(right)) = (number_of(left)?, number_of(right)?) else {
        return Ok(Value::Null);
    };
    if let (Number::Integer(a), Number::Integer(b)) = (&left, &right) {
        if *b == 0 {
            return Err(divisor_is_zero());
        }
        if a.checked_rem(*b) == Some(0) {
            return a
                .checked_div(*b)
                .map(Value::BigInt)
                .ok_or_else(numeric_overflow);
        }
    }
    let divisor = right.float();
    if divisor == 0.0 {
        return Err(divisor_is_zero());
    }
    Ok(Value::Double(left.float() / divisor))
}

fn chars_of(value: &Value) -> Option<Vec<char>> {
    text_of(value).map(|text| text.chars().collect())
}

fn substring(values: &[Value]) -> SqlResult<Value> {
    let Some(characters) = chars_of(&values[0]) else {
        return Ok(Value::Null);
    };
    let Some(start) = whole_of(&values[1])? else {
        return Ok(Value::Null);
    };
    let length = match values.get(2) {
        Some(length) => match whole_of(length)? {
            Some(length) if length < 1 => return Ok(Value::Null),
            Some(length) => Some(length),
            None => return Ok(Value::Null),
        },
        None => None,
    };
    let count = characters.len() as i64;
    let first = match start {
        0 => 0,
        start if start > 0 => start - 1,
        start => count + start,
    };
    if first < 0 || first >= count {
        return Ok(Value::Null);
    }
    let end = match length {
        Some(length) => first.saturating_add(length).min(count),
        None => count,
    };
    Ok(text_or_null(
        characters[first as usize..end as usize].iter().collect(),
    ))
}

fn position_of(values: &[Value]) -> SqlResult<Value> {
    let (Some(haystack), Some(needle)) = (chars_of(&values[0]), chars_of(&values[1])) else {
        return Ok(Value::Null);
    };
    let start = match values.get(2) {
        Some(value) => match whole_of(value)? {
            Some(start) => start,
            None => return Ok(Value::Null),
        },
        None => 1,
    };
    let occurrence = match values.get(3) {
        Some(value) => match whole_of(value)? {
            Some(occurrence) if occurrence < 1 => {
                return Err(out_of_range(&occurrence.to_string()));
            }
            Some(occurrence) => occurrence,
            None => return Ok(Value::Null),
        },
        None => 1,
    };
    let count = haystack.len() as i64;
    let width = needle.len() as i64;
    if start == 0 || width > count {
        return Ok(Value::BigInt(0));
    }
    let matches_at = |at: i64| haystack[at as usize..(at + width) as usize] == needle[..];
    let mut seen = 0;
    if start > 0 {
        let mut at = start - 1;
        while at + width <= count {
            if matches_at(at) {
                seen += 1;
                if seen == occurrence {
                    return Ok(Value::BigInt(at + 1));
                }
            }
            at += 1;
        }
    } else {
        let mut at = (count + start).min(count - width);
        while at >= 0 {
            if matches_at(at) {
                seen += 1;
                if seen == occurrence {
                    return Ok(Value::BigInt(at + 1));
                }
            }
            at -= 1;
        }
    }
    Ok(Value::BigInt(0))
}

fn initial_capitals(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_word = false;
    for character in text.chars() {
        if character.is_alphanumeric() {
            if in_word {
                out.extend(character.to_lowercase());
            } else {
                out.extend(character.to_uppercase());
            }
            in_word = true;
        } else {
            out.push(character);
            in_word = false;
        }
    }
    out
}

fn trimmed(values: &[Value], left: bool, right: bool) -> SqlResult<Value> {
    let Some(text) = text_of(&values[0]) else {
        return Ok(Value::Null);
    };
    let set: Vec<char> = match values.get(1) {
        Some(set) => match chars_of(set) {
            Some(set) => set,
            None => return Ok(Value::Null),
        },
        None => vec![' '],
    };
    let mut rest = text.as_str();
    if left {
        rest = rest.trim_start_matches(|character| set.contains(&character));
    }
    if right {
        rest = rest.trim_end_matches(|character| set.contains(&character));
    }
    Ok(text_or_null(rest.to_owned()))
}

fn trim_one(values: &[Value], left: bool, right: bool) -> SqlResult<Value> {
    if let Some(set) = chars_of(&values[1])
        && set.len() != 1
    {
        return Err(SqlError::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            "trim set should have only one character",
        )
        .with_oracle_code(30001));
    }
    trimmed(values, left, right)
}

fn padded(values: &[Value], left: bool) -> SqlResult<Value> {
    let Some(text) = chars_of(&values[0]) else {
        return Ok(Value::Null);
    };
    let Some(width) = whole_of(&values[1])? else {
        return Ok(Value::Null);
    };
    let fill: Vec<char> = match values.get(2) {
        Some(fill) => match chars_of(fill) {
            Some(fill) => fill,
            None => return Ok(Value::Null),
        },
        None => vec![' '],
    };
    if width < 1 || fill.is_empty() {
        return Ok(Value::Null);
    }
    let width = usize::try_from(width).map_err(|_| too_long())?;
    within_limit(width.saturating_mul(MAX_UTF8_CHAR_BYTES))?;
    if text.len() >= width {
        return Ok(text_or_null(text[..width].iter().collect()));
    }
    let padding: String = fill.iter().cycle().take(width - text.len()).collect();
    let body: String = text.iter().collect();
    Ok(Value::Text(if left {
        padding + &body
    } else {
        body + &padding
    }))
}

fn replaced(values: &[Value]) -> SqlResult<Value> {
    let Some(text) = text_of(&values[0]) else {
        return Ok(Value::Null);
    };
    let Some(search) = text_of(&values[1]).filter(|search| !search.is_empty()) else {
        return Ok(text_or_null(text));
    };
    let replacement = values.get(2).and_then(text_of).unwrap_or_default();
    if replacement.len() > search.len() {
        let count = text.matches(search.as_str()).count();
        within_limit(
            text.len()
                .saturating_add(count.saturating_mul(replacement.len() - search.len())),
        )?;
    }
    Ok(text_or_null(text.replace(&search, &replacement)))
}

fn character(value: &Value) -> SqlResult<Value> {
    let Some(code) = whole_of(value)? else {
        return Ok(Value::Null);
    };
    let character = u32::try_from(code)
        .ok()
        .and_then(char::from_u32)
        .ok_or_else(invalid_number)?;
    Ok(Value::Text(character.to_string()))
}

fn character_code(value: &Value) -> Value {
    let Some(first) = text_of(value).and_then(|text| text.chars().next()) else {
        return Value::Null;
    };
    let mut bytes = [0; 4];
    let code = first
        .encode_utf8(&mut bytes)
        .bytes()
        .fold(0i64, |code, byte| code * 256 + i64::from(byte));
    Value::BigInt(code)
}

fn scaled(value: &Value, digits: Option<&Value>, rounding: bool) -> SqlResult<Value> {
    let Some(number) = number_of(value)? else {
        return Ok(Value::Null);
    };
    let digits = match digits {
        Some(digits) => match whole_of(digits)? {
            Some(digits) => digits.clamp(-400, 400),
            None => return Ok(Value::Null),
        },
        None => 0,
    };
    match number {
        Number::Integer(number) if digits >= 0 => Ok(Value::BigInt(number)),
        Number::Integer(number) => {
            let Some(unit) = u32::try_from(-digits)
                .ok()
                .and_then(|exponent| 10i64.checked_pow(exponent))
            else {
                return Ok(Value::BigInt(0));
            };
            let remainder = number % unit;
            let mut result = number - remainder;
            if rounding && remainder.abs() * 2 >= unit {
                result = result
                    .checked_add(unit * number.signum())
                    .ok_or_else(numeric_overflow)?;
            }
            Ok(Value::BigInt(result))
        }
        Number::Float(number) => {
            let factor = 10f64.powi(digits as i32);
            let shifted = number * factor;
            if !shifted.is_finite() {
                return Ok(Value::Double(number));
            }
            let moved = if rounding {
                shifted.round()
            } else {
                shifted.trunc()
            };
            Ok(Value::Double(moved / factor))
        }
    }
}

fn remainder(left: &Value, right: &Value) -> SqlResult<Value> {
    let (Some(left), Some(right)) = (number_of(left)?, number_of(right)?) else {
        return Ok(Value::Null);
    };
    Ok(match (left, right) {
        (left, Number::Integer(0)) => left.value(),
        (Number::Integer(a), Number::Integer(b)) => Value::BigInt(a.checked_rem(b).unwrap_or(0)),
        (left, right) if right.float() == 0.0 => left.value(),
        (left, right) => Value::Double(left.float() % right.float()),
    })
}

fn rounded_toward(value: &Value, up: bool) -> SqlResult<Value> {
    Ok(match number_of(value)? {
        None => Value::Null,
        Some(Number::Integer(number)) => Value::BigInt(number),
        Some(Number::Float(number)) => Value::Double(if up { number.ceil() } else { number.floor() }),
    })
}

fn power(base: &Value, exponent: &Value) -> SqlResult<Value> {
    let (Some(base), Some(exponent)) = (number_of(base)?, number_of(exponent)?) else {
        return Ok(Value::Null);
    };
    if let (Number::Integer(base), Number::Integer(exponent)) = (&base, &exponent)
        && let Ok(exponent) = u32::try_from(*exponent)
        && let Some(result) = base.checked_pow(exponent)
    {
        return Ok(Value::BigInt(result));
    }
    let (base, exponent) = (base.float(), exponent.float());
    if base < 0.0 && exponent.fract() != 0.0 {
        return Err(out_of_range(&number_text(base)));
    }
    if base == 0.0 && exponent < 0.0 {
        return Err(divisor_is_zero());
    }
    let result = base.powf(exponent);
    if result.is_finite() {
        Ok(Value::Double(result))
    } else {
        Err(numeric_overflow())
    }
}

fn square_root(value: &Value) -> SqlResult<Value> {
    let Some(number) = number_of(value)? else {
        return Ok(Value::Null);
    };
    let number = number.float();
    if number < 0.0 {
        return Err(out_of_range(&number_text(number)));
    }
    Ok(Value::Double(number.sqrt()))
}

fn sign(value: &Value) -> SqlResult<Value> {
    Ok(match number_of(value)? {
        None => Value::Null,
        Some(number) => {
            let number = number.float();
            Value::BigInt(if number > 0.0 {
                1
            } else if number < 0.0 {
                -1
            } else {
                0
            })
        }
    })
}

fn absolute(value: &Value) -> SqlResult<Value> {
    Ok(match number_of(value)? {
        None => Value::Null,
        Some(Number::Integer(number)) => {
            Value::BigInt(number.checked_abs().ok_or_else(numeric_overflow)?)
        }
        Some(Number::Float(number)) => Value::Double(number.abs()),
    })
}

fn extreme(values: &[Value], greatest: bool) -> SqlResult<Value> {
    if values.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    let wanted = if greatest {
        Ordering::Greater
    } else {
        Ordering::Less
    };
    if matches!(values[0], Value::Text(_)) {
        let mut best = text_of(&values[0]).unwrap_or_default();
        for value in &values[1..] {
            let text = text_of(value).unwrap_or_default();
            if text.cmp(&best) == wanted {
                best = text;
            }
        }
        return Ok(text_or_null(best));
    }
    let mut numbers = Vec::with_capacity(values.len());
    for value in values {
        if let Some(number) = number_of(value)? {
            numbers.push(number);
        }
    }
    let all_integers = numbers
        .iter()
        .all(|number| matches!(number, Number::Integer(_)));
    let mut best = 0;
    for (index, number) in numbers.iter().enumerate() {
        let order = match (number, &numbers[best]) {
            (Number::Integer(a), Number::Integer(b)) => a.cmp(b),
            (a, b) => a.float().total_cmp(&b.float()),
        };
        if order == wanted {
            best = index;
        }
    }
    let chosen = numbers.swap_remove(best);
    Ok(if all_integers {
        chosen.value()
    } else {
        Value::Double(chosen.float())
    })
}

struct Model {
    fill_mode: bool,
    dollar: bool,
    leading_sign: bool,
    trailing_minus: bool,
    integer: Vec<char>,
    fraction: Vec<char>,
    point: bool,
    width: usize,
}

fn model_of(format: &str) -> SqlResult<Model> {
    let upper = format.to_ascii_uppercase();
    let mut rest = upper.as_str();
    let mut model = Model {
        fill_mode: false,
        dollar: false,
        leading_sign: false,
        trailing_minus: false,
        integer: Vec::new(),
        fraction: Vec::new(),
        point: false,
        width: format.len(),
    };
    if let Some(after) = rest.strip_prefix("FM") {
        model.fill_mode = true;
        model.width -= 2;
        rest = after;
    }
    if let Some(after) = rest.strip_prefix('S') {
        model.leading_sign = true;
        rest = after;
    }
    if let Some(after) = rest.strip_suffix("MI") {
        model.trailing_minus = true;
        rest = after;
    }
    for element in rest.chars() {
        match element {
            '$' if !model.dollar => model.dollar = true,
            '9' | '0' if model.point => model.fraction.push(element),
            '9' | '0' => model.integer.push(element),
            ',' | 'G' if !model.point => model.integer.push(','),
            '.' | 'D' if !model.point => model.point = true,
            _ => return Err(invalid_format()),
        }
    }
    if model.integer.is_empty() && model.fraction.is_empty() {
        return Err(invalid_format());
    }
    Ok(model)
}

fn formatted(number: f64, format: &str) -> SqlResult<String> {
    let model = model_of(format)?;
    let negative = number < 0.0;
    let digits = format!("{:.*}", model.fraction.len(), number.abs());
    let (whole, fraction) = digits.split_once('.').unwrap_or((&digits, ""));
    let slots = model.integer.iter().filter(|slot| **slot != ',').count();
    let whole: Vec<char> = if whole == "0" && !model.fraction.is_empty() {
        Vec::new()
    } else {
        whole.chars().collect()
    };
    if whole.len() > slots {
        return Ok("#".repeat(model.width + 1));
    }
    let first_zero = model.integer.iter().position(|slot| *slot == '0');
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
    let mut prefix = String::new();
    if model.leading_sign {
        prefix.push(if negative { '-' } else { '+' });
    } else if !model.trailing_minus {
        prefix.push(if negative { '-' } else { ' ' });
    }
    if model.dollar {
        prefix.push('$');
    }
    text.insert_str(first_printed, &prefix);
    if model.point {
        text.push('.');
        text.push_str(fraction);
    }
    if model.trailing_minus {
        text.push(if negative { '-' } else { ' ' });
    }
    if model.fill_mode {
        let mut trimmed = text.trim().to_owned();
        if model.fraction.iter().all(|slot| *slot == '9') && model.point {
            while trimmed.ends_with('0') {
                trimmed.pop();
            }
        }
        return Ok(trimmed);
    }
    Ok(text)
}

fn to_char(values: &[Value]) -> SqlResult<Value> {
    let format = match values.get(1) {
        Some(format) => match text_of(format) {
            Some(format) => Some(format),
            None => return Ok(Value::Null),
        },
        None => None,
    };
    match (&values[0], format) {
        (Value::Null, _) => Ok(Value::Null),
        (Value::Text(text), None) => Ok(text_or_null(text.clone())),
        (value, None) => Ok(text_of(value).map_or(Value::Null, text_or_null)),
        (value, Some(format)) => {
            let Some(number) = number_of(value)? else {
                return Ok(Value::Null);
            };
            formatted(number.float(), &format).map(text_or_null)
        }
    }
}

fn to_number(values: &[Value]) -> SqlResult<Value> {
    let Some(text) = text_of(&values[0]) else {
        return Ok(Value::Null);
    };
    let cleaned = match values.get(1) {
        Some(format) => {
            let Some(format) = text_of(format) else {
                return Ok(Value::Null);
            };
            model_of(&format)?;
            text.replace([',', '$'], "")
        }
        None => text,
    };
    Ok(parse_number(&cleaned)?.value())
}

fn row_count(value: &Value) -> SqlResult<Value> {
    Ok(Value::BigInt(match number_of(value)? {
        None => 0,
        Some(Number::Integer(number)) => number.max(0),
        Some(Number::Float(number)) => number.max(0.0).trunc() as i64,
    }))
}

pub(crate) fn call(name: &str, values: &[Value]) -> SqlResult<Value> {
    let unary_text = |change: fn(&str) -> String| -> Value {
        text_of(&values[0]).map_or(Value::Null, |text| text_or_null(change(&text)))
    };
    match (name, values) {
        ("oracle_concat", _) => concatenated(values),
        ("oracle_divide", [left, right]) => divided(left, right),
        ("oracle_row_count", [value]) => row_count(value),
        ("oracle_length", [value]) => Ok(text_of(value)
            .filter(|text| !text.is_empty())
            .map_or(Value::Null, |text| {
                Value::BigInt(text.chars().count() as i64)
            })),
        ("oracle_substr", _) => substring(values),
        ("oracle_instr", _) => position_of(values),
        ("oracle_upper", [_]) => Ok(unary_text(str::to_uppercase)),
        ("oracle_lower", [_]) => Ok(unary_text(str::to_lowercase)),
        ("oracle_initcap", [_]) => Ok(unary_text(initial_capitals)),
        ("oracle_ltrim", _) => trimmed(values, true, false),
        ("oracle_rtrim", _) => trimmed(values, false, true),
        ("oracle_trim_leading", [_, _]) => trim_one(values, true, false),
        ("oracle_trim_trailing", [_, _]) => trim_one(values, false, true),
        ("oracle_trim_both", [_, _]) => trim_one(values, true, true),
        ("oracle_lpad", _) => padded(values, true),
        ("oracle_rpad", _) => padded(values, false),
        ("oracle_replace", _) => replaced(values),
        ("oracle_chr", [value]) => character(value),
        ("oracle_ascii", [value]) => Ok(character_code(value)),
        ("oracle_abs", [value]) => absolute(value),
        ("oracle_round", [value, rest @ ..]) => scaled(value, rest.first(), true),
        ("oracle_trunc", [value, rest @ ..]) => scaled(value, rest.first(), false),
        ("oracle_mod", [left, right]) => remainder(left, right),
        ("oracle_ceil", [value]) => rounded_toward(value, true),
        ("oracle_floor", [value]) => rounded_toward(value, false),
        ("oracle_power", [base, exponent]) => power(base, exponent),
        ("oracle_sqrt", [value]) => square_root(value),
        ("oracle_sign", [value]) => sign(value),
        ("oracle_greatest", [_, ..]) => extreme(values, true),
        ("oracle_least", [_, ..]) => extreme(values, false),
        ("oracle_to_char", [_, ..]) => to_char(values),
        ("oracle_to_number", [_, ..]) => to_number(values),
        _ => Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("function {name}() does not exist"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{call, formatted, number_text};
    use crate::types::Value;

    fn text(value: &str) -> Value {
        Value::Text(value.into())
    }

    #[test]
    fn numbers_print_without_a_leading_zero_or_trailing_zeros() {
        assert_eq!(number_text(0.5), ".5");
        assert_eq!(number_text(-0.25), "-.25");
        assert_eq!(number_text(3.0), "3");
        assert_eq!(number_text(1234.5), "1234.5");
    }

    #[test]
    fn number_format_models_place_digits_signs_and_separators() {
        assert_eq!(formatted(1234.5, "9,999.99").as_deref(), Ok(" 1,234.50"));
        assert_eq!(formatted(-12.0, "999").as_deref(), Ok(" -12"));
        assert_eq!(formatted(7.0, "000").as_deref(), Ok(" 007"));
        assert_eq!(formatted(0.5, "9.99").as_deref(), Ok("  .50"));
        assert_eq!(formatted(0.0, "999").as_deref(), Ok("   0"));
        assert_eq!(formatted(1.5, "FM9.99").as_deref(), Ok("1.5"));
        assert_eq!(formatted(12345.0, "999").as_deref(), Ok("####"));
        assert_eq!(formatted(12.0, "$99").as_deref(), Ok(" $12"));
        assert_eq!(formatted(12.0, "S99").as_deref(), Ok("+12"));
        assert_eq!(formatted(-12.0, "99MI").as_deref(), Ok("12-"));
        assert!(formatted(1.0, "X9").is_err());
    }

    #[test]
    fn padding_past_the_length_limit_is_refused_before_it_is_built() {
        let error = call(
            "oracle_lpad",
            &[text("x"), Value::BigInt(2_000_000_000), text("ab")],
        )
        .expect_err("8 GB");
        assert_eq!(error.vendor_code(), Some(1489));
        let error = call(
            "oracle_rpad",
            &[text("x"), Value::BigInt(i64::MAX), text("ab")],
        )
        .expect_err("too long");
        assert_eq!(error.vendor_code(), Some(1489));
        assert_eq!(
            call("oracle_lpad", &[text("7"), Value::BigInt(3), text("0")]),
            Ok(text("007"))
        );
        let subject = text(&" ".repeat(1000));
        let error = call(
            "oracle_replace",
            &[subject, text(" "), text(&"x".repeat(2_000_000))],
        )
        .expect_err("2 GB");
        assert_eq!(error.vendor_code(), Some(1489));
    }

    #[test]
    fn substr_and_instr_count_characters_from_either_end() {
        assert_eq!(
            call("oracle_substr", &[text("ABCDEFG"), Value::BigInt(-3), Value::BigInt(2)]),
            Ok(text("EF"))
        );
        assert_eq!(
            call("oracle_substr", &[text("ABC"), Value::BigInt(5)]),
            Ok(Value::Null)
        );
        assert_eq!(
            call(
                "oracle_instr",
                &[text("CORPORATE FLOOR"), text("OR"), Value::BigInt(-3), Value::BigInt(2)]
            ),
            Ok(Value::BigInt(2))
        );
        assert_eq!(
            call(
                "oracle_instr",
                &[text("CORPORATE FLOOR"), text("OR"), Value::BigInt(3), Value::BigInt(2)]
            ),
            Ok(Value::BigInt(14))
        );
    }
}
