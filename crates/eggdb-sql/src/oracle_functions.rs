use crate::error::{SqlError, SqlResult, SqlState};
use crate::numeric::Numeric;
use crate::sqlite_functions::MAX_TEXT_BYTES;
use crate::types::{SqlType, Value};
use std::cmp::Ordering;

const MAX_UTF8_CHAR_BYTES: usize = 4;

const MAX_CASE_CHANGE_GROWTH: usize = 3;

fn too_long() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        "result of string concatenation is too long",
    )
    .with_oracle_code(1489)
}

pub(crate) fn within_limit(bytes: usize) -> SqlResult<()> {
    if bytes > MAX_TEXT_BYTES {
        Err(too_long())
    } else {
        crate::memory::check_value_bytes(bytes)
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

fn text_with_type(value: &Value, declared: Option<SqlType>) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Text(text) => Some(text.clone()),
        Value::Real(number) => Some(number_text(f64::from(*number))),
        Value::Double(number) => Some(number_text(*number)),
        Value::Numeric(number) => Some(crate::dialect::oracle_numeric_text(number)),
        Value::Boolean(flag) => Some(if *flag { "TRUE" } else { "FALSE" }.to_owned()),
        Value::Bytea(_) => crate::Dialect::Oracle.value_text(value),
        Value::Timestamp(_) | Value::TimestampTz(_) | Value::Date(_) => {
            match crate::oracle_datetime::nls_text(value, declared) {
                Ok(Some(text)) => Some(text),
                _ => value.to_text(),
            }
        }
        other => other.to_text(),
    }
}

fn text_of(value: &Value) -> Option<String> {
    text_with_type(value, None)
}

pub(crate) fn plain_text(value: &Value) -> Option<String> {
    text_of(value)
}

pub(crate) fn text_by_declared_type(value: &Value, declared: Option<SqlType>) -> Option<String> {
    text_with_type(value, declared)
}

fn text_or_null(text: String) -> Value {
    if text.is_empty() {
        Value::Null
    } else {
        Value::Text(text)
    }
}

pub(crate) enum Number {
    Integer(i64),
    Decimal(Numeric),
    Float(f64),
}

impl Number {
    fn float(&self) -> f64 {
        match self {
            Self::Integer(number) => *number as f64,
            Self::Decimal(number) => number.to_f64(),
            Self::Float(number) => *number,
        }
    }

    fn exact(&self) -> Option<Numeric> {
        match self {
            Self::Integer(number) => Some(Numeric::from_i64(*number)),
            Self::Decimal(number) => Some(number.clone()),
            Self::Float(_) => None,
        }
    }

    pub(crate) fn value(self) -> Value {
        match self {
            Self::Integer(number) => Value::BigInt(number),
            Self::Decimal(number) => Value::Numeric(number),
            Self::Float(number) => Value::Double(number),
        }
    }

    fn of_numeric(number: Numeric) -> Self {
        if number.is_nan() {
            return Self::Float(f64::NAN);
        }
        match number.integral_value().map(i64::try_from) {
            Some(Ok(whole)) => Self::Integer(whole),
            _ => Self::Decimal(number),
        }
    }
}

fn exact_result(result: SqlResult<Numeric>) -> SqlResult<Value> {
    result
        .map(|number| Number::of_numeric(number).value())
        .map_err(|error| {
            if error.state() == SqlState::NUMERIC_VALUE_OUT_OF_RANGE {
                numeric_overflow()
            } else {
                error
            }
        })
}

pub(crate) fn parse_number(text: &str) -> SqlResult<Number> {
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
    if !numeric {
        return Err(invalid_number());
    }
    match Numeric::parse(trimmed) {
        Ok(number) => Ok(Number::of_numeric(number)),
        Err(error) if error.state() == SqlState::NUMERIC_VALUE_OUT_OF_RANGE => {
            Err(numeric_overflow())
        }
        Err(_) => Err(invalid_number()),
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
        Value::Text(text) | Value::Json(text) | Value::Jsonb(text) | Value::Bpchar(text) => {
            parse_number(text)?
        }
        Value::Numeric(number) => Number::of_numeric(number.clone()),
        Value::Date(_)
        | Value::Timestamp(_)
        | Value::TimestampTz(_)
        | Value::Time(_)
        | Value::TimeTz { .. }
        | Value::Interval(_)
        | Value::Bytea(_)
        | Value::Uuid(_)
        | Value::Array(_) => {
            return Err(invalid_number());
        }
    }))
}

fn whole_of(value: &Value) -> SqlResult<Option<i64>> {
    Ok(number_of(value)?.map(|number| match number {
        Number::Integer(number) => number,
        Number::Decimal(number) => number.to_f64().trunc() as i64,
        Number::Float(number) => number.trunc() as i64,
    }))
}

fn concatenated(values: &[Value], declared: &[Option<SqlType>]) -> SqlResult<Value> {
    let parts: Vec<String> = values
        .iter()
        .enumerate()
        .filter_map(|(index, value)| text_with_type(value, declared.get(index).copied().flatten()))
        .collect();
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
    if let (Some(a), Some(b)) = (left.exact(), right.exact()) {
        if b.is_zero() {
            return Err(divisor_is_zero());
        }
        return exact_result(a.divide(&b));
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
    if start == 0 || width > count || start > count {
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
        Number::Decimal(number) => {
            let moved = if rounding {
                number.rounded(digits)
            } else {
                number.truncated(digits)
            };
            exact_result(moved)
        }
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
            if factor == 0.0 {
                return Ok(Value::Double(0.0));
            }
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
    if let (Some(a), Some(b)) = (left.exact(), right.exact())
        && matches!(
            (&left, &right),
            (Number::Decimal(_), _) | (_, Number::Decimal(_))
        )
    {
        if b.is_zero() {
            return Ok(left.value());
        }
        return exact_result(a.remainder(&b));
    }
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
        Some(Number::Decimal(number)) => {
            return exact_result(if up { number.ceiling() } else { number.floor() });
        }
        Some(Number::Float(number)) => {
            Value::Double(if up { number.ceil() } else { number.floor() })
        }
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
        Some(Number::Decimal(number)) => {
            Value::BigInt(match number.compare(&Numeric::from_i64(0)) {
                Ordering::Greater => 1,
                Ordering::Less => -1,
                Ordering::Equal => 0,
            })
        }
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
        Some(Number::Decimal(number)) => Value::Numeric(number.absolute()),
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
    let all_exact = numbers
        .iter()
        .all(|number| !matches!(number, Number::Float(_)));
    let mut best = 0;
    for (index, number) in numbers.iter().enumerate() {
        let order = match (number, &numbers[best]) {
            (Number::Integer(a), Number::Integer(b)) => a.cmp(b),
            (a, b) => match (a.exact(), b.exact()) {
                (Some(a), Some(b)) => a.compare(&b),
                _ => a.float().total_cmp(&b.float()),
            },
        };
        if order == wanted {
            best = index;
        }
    }
    let chosen = numbers.swap_remove(best);
    Ok(if all_exact {
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

fn formatted(number: &Number, format: &str) -> SqlResult<String> {
    let model = model_of(format)?;
    within_limit(format.len().saturating_mul(2).saturating_add(2))?;
    let (negative, digits) = fixed_digits(number, model.fraction.len())?;
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

fn is_datetime(value: &Value) -> bool {
    matches!(
        value,
        Value::Timestamp(_) | Value::TimestampTz(_) | Value::Date(_)
    )
}

fn numeric_parameter(values: &[Value]) -> SqlResult<()> {
    match values.get(2) {
        Some(Value::Text(text)) => crate::oracle_nls::check_numeric_parameter(text),
        _ => Ok(()),
    }
}

fn to_char(values: &[Value], declared: Option<SqlType>) -> SqlResult<Value> {
    if is_datetime(&values[0]) {
        return crate::oracle_datetime::to_char_datetime(values, declared);
    }
    if values.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    numeric_parameter(values)?;
    let format = values.get(1).and_then(text_of);
    match (&values[0], format) {
        (Value::Text(text), None) => Ok(text_or_null(text.clone())),
        (value, None) => Ok(text_of(value).map_or(Value::Null, text_or_null)),
        (value, Some(format)) => {
            let Some(number) = number_of(value)? else {
                return Ok(Value::Null);
            };
            formatted(&number, &format).map(text_or_null)
        }
    }
}

fn number_by_model(text: &str, format: &str) -> SqlResult<Number> {
    let model = model_of(format)?;
    let mut rest = text.trim();
    let mut negative = false;
    if model.leading_sign {
        negative = match rest.chars().next() {
            Some('-') => true,
            Some('+') => false,
            _ => return Err(invalid_number()),
        };
        rest = &rest[1..];
    } else if model.trailing_minus {
        if let Some(body) = rest.strip_suffix('-') {
            negative = true;
            rest = body;
        } else if let Some(body) = rest.strip_suffix('+') {
            rest = body;
        }
    } else if let Some(body) = rest.strip_prefix('-') {
        negative = true;
        rest = body;
    }
    if model.dollar {
        rest = rest.strip_prefix('$').unwrap_or(rest);
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
    let slots = model.integer.iter().filter(|slot| **slot != ',').count();
    if !whole_ok
        || !fraction.chars().all(|character| character.is_ascii_digit())
        || digits.len() > slots
        || fraction.len() > model.fraction.len()
        || (digits.is_empty() && fraction.is_empty())
    {
        return Err(invalid_number());
    }
    let sign = if negative { "-" } else { "" };
    let digits = if digits.is_empty() { "0" } else { &digits };
    parse_number(&if fraction.is_empty() {
        format!("{sign}{digits}")
    } else {
        format!("{sign}{digits}.{fraction}")
    })
}

fn to_number(values: &[Value]) -> SqlResult<Value> {
    if values.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    numeric_parameter(values)?;
    let source = &values[0];
    if !matches!(source, Value::Text(_) | Value::Bpchar(_)) {
        return Ok(number_of(source)?.map_or(Value::Null, Number::value));
    }
    let Some(text) = text_of(source) else {
        return Ok(Value::Null);
    };
    match values.get(1).and_then(text_of) {
        Some(format) => Ok(number_by_model(&text, &format)?.value()),
        None => Ok(parse_number(&text)?.value()),
    }
}

fn row_count(value: &Value) -> SqlResult<Value> {
    Ok(Value::BigInt(match number_of(value)? {
        None => 0,
        Some(Number::Integer(number)) => number.max(0),
        Some(Number::Decimal(number)) => number.to_f64().max(0.0).trunc() as i64,
        Some(Number::Float(number)) => number.max(0.0).trunc() as i64,
    }))
}

fn months_of(value: &Value) -> SqlResult<Option<i64>> {
    whole_of(value)
}

fn date_unit(values: &[Value]) -> Option<String> {
    values.get(1).and_then(text_of)
}

pub(crate) fn call(name: &str, values: &[Value], declared: &[Option<SqlType>]) -> SqlResult<Value> {
    let unary_text = |change: fn(&str) -> String| -> SqlResult<Value> {
        let Some(text) = text_of(&values[0]) else {
            return Ok(Value::Null);
        };
        within_limit(text.len().saturating_mul(MAX_CASE_CHANGE_GROWTH))?;
        Ok(text_or_null(change(&text)))
    };
    match (name, values) {
        ("oracle_concat", _) => concatenated(values, declared),
        ("oracle_divide", [left, right]) => divided(left, right),
        ("oracle_row_count", [value]) => row_count(value),
        ("oracle_length", [value]) => Ok(text_of(value)
            .filter(|text| !text.is_empty())
            .map_or(Value::Null, |text| {
                Value::BigInt(text.chars().count() as i64)
            })),
        ("oracle_substr", _) => substring(values),
        ("oracle_instr", _) => position_of(values),
        ("oracle_upper", [_]) => unary_text(str::to_uppercase),
        ("oracle_lower", [_]) => unary_text(str::to_lowercase),
        ("oracle_initcap", [_]) => unary_text(initial_capitals),
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
        ("oracle_round", [value, ..]) if is_datetime(value) => {
            Err(SqlError::not_supported("ROUND of a date"))
        }
        ("oracle_trunc", [value, ..]) if is_datetime(value) => {
            if values.get(1).is_some_and(Value::is_null) {
                return Ok(Value::Null);
            }
            crate::oracle_datetime::truncated_date(value, date_unit(values).as_deref())
        }
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
        ("oracle_to_char", [_, ..]) => to_char(values, declared.first().copied().flatten()),
        ("oracle_to_number", [_, ..]) => to_number(values),
        ("oracle_to_date", [_, ..]) => crate::oracle_datetime::to_date(values),
        ("oracle_to_timestamp", [_, ..]) => crate::oracle_datetime::to_timestamp(values),
        ("oracle_add_months", [date, months]) => {
            crate::oracle_datetime::add_months(date, months_of(months)?)
        }
        ("oracle_last_day", [date]) => crate::oracle_datetime::last_day(date),
        ("oracle_date_literal", [Value::Text(text)]) => crate::oracle_datetime::date_literal(text),
        ("oracle_timestamp_literal", [Value::Text(text)]) => {
            crate::oracle_datetime::timestamp_literal(text)
        }
        _ => Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("function {name}() does not exist"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{formatted, number_text, parse_number};
    use crate::error::SqlResult;
    use crate::types::Value;

    fn call(name: &str, values: &[Value]) -> SqlResult<Value> {
        super::call(name, values, &[])
    }

    fn shaped(number: &str, format: &str) -> SqlResult<String> {
        formatted(&parse_number(number)?, format)
    }

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
        assert!(shaped("1", "X9").is_err());
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
    fn case_changes_past_the_length_limit_are_refused_before_they_are_built() {
        let long = text(&"a".repeat(340_000_000));
        for name in ["oracle_upper", "oracle_lower", "oracle_initcap"] {
            let error = call(name, std::slice::from_ref(&long)).expect_err(name);
            assert_eq!(error.vendor_code(), Some(1489), "{name}");
        }
        assert_eq!(call("oracle_initcap", &[text("ab cd")]), Ok(text("Ab Cd")));
    }

    #[test]
    fn substr_and_instr_count_characters_from_either_end() {
        assert_eq!(
            call(
                "oracle_substr",
                &[text("ABCDEFG"), Value::BigInt(-3), Value::BigInt(2)]
            ),
            Ok(text("EF"))
        );
        assert_eq!(
            call("oracle_substr", &[text("ABC"), Value::BigInt(5)]),
            Ok(Value::Null)
        );
        assert_eq!(
            call(
                "oracle_instr",
                &[
                    text("CORPORATE FLOOR"),
                    text("OR"),
                    Value::BigInt(-3),
                    Value::BigInt(2)
                ]
            ),
            Ok(Value::BigInt(2))
        );
        assert_eq!(
            call(
                "oracle_instr",
                &[
                    text("CORPORATE FLOOR"),
                    text("OR"),
                    Value::BigInt(3),
                    Value::BigInt(2)
                ]
            ),
            Ok(Value::BigInt(14))
        );
        assert_eq!(
            call(
                "oracle_instr",
                &[text("abc"), text("bc"), Value::BigInt(i64::MAX)]
            ),
            Ok(Value::BigInt(0))
        );
        assert_eq!(
            call("oracle_round", &[Value::Double(15.5), Value::BigInt(-400)]),
            Ok(Value::Double(0.0))
        );
    }

    fn decimal(text: &str) -> Value {
        Value::Numeric(crate::numeric::Numeric::parse(text).expect("decimal"))
    }

    #[test]
    fn decimals_stay_exact_through_division_and_the_numeric_functions() {
        assert_eq!(
            call("oracle_divide", &[decimal("0.3"), decimal("0.1")]),
            Ok(Value::BigInt(3))
        );
        assert_eq!(
            call("oracle_round", &[decimal("1.005"), Value::BigInt(2)]),
            Ok(decimal("1.01"))
        );
        assert_eq!(
            call("oracle_trunc", &[decimal("-2.567"), Value::BigInt(1)]),
            Ok(decimal("-2.5"))
        );
        assert_eq!(call("oracle_ceil", &[decimal("2.1")]), Ok(Value::BigInt(3)));
        assert_eq!(
            call("oracle_floor", &[decimal("-2.1")]),
            Ok(Value::BigInt(-3))
        );
        assert_eq!(call("oracle_abs", &[decimal("-0.5")]), Ok(decimal("0.5")));
        assert_eq!(
            call("oracle_sign", &[decimal("-0.5")]),
            Ok(Value::BigInt(-1))
        );
        assert_eq!(
            call("oracle_mod", &[decimal("7.5"), Value::BigInt(2)]),
            Ok(decimal("1.5"))
        );
        assert_eq!(
            call(
                "oracle_greatest",
                &[decimal("0.1"), decimal("0.25"), Value::BigInt(0)]
            ),
            Ok(decimal("0.25"))
        );
        assert_eq!(
            call("oracle_concat", &[decimal("24000.00"), Value::Null]),
            Ok(text("24000"))
        );
        assert_eq!(call("oracle_to_char", &[decimal("0.5")]), Ok(text(".5")));
        assert_eq!(
            call("oracle_to_char", &[decimal("1234.567"), text("9,999.99")]),
            Ok(text(" 1,234.57"))
        );
        assert_eq!(call("oracle_to_number", &[text("0.1")]), Ok(decimal("0.1")));
        let code = |values: &[Value]| {
            call("oracle_to_number", values)
                .err()
                .and_then(|error| error.vendor_code())
        };
        assert_eq!(code(&[text("NaN")]), Some(1722));
        assert_eq!(code(&[text("1e5000")]), Some(1426));
    }

    #[test]
    fn to_number_reads_by_its_format_model() {
        let number = |values: &[Value]| call("oracle_to_number", values);
        assert_eq!(
            number(&[text("123-"), text("999MI")]),
            Ok(Value::BigInt(-123))
        );
        assert_eq!(
            number(&[text("$1,234.50"), text("$9,999.99")]),
            Ok(decimal("1234.50"))
        );
        assert_eq!(
            number(&[text("100.00"), text("9G999D99")]),
            Ok(Value::BigInt(100))
        );
        assert_eq!(number(&[text("+12"), text("S99")]), Ok(Value::BigInt(12)));
        assert_eq!(
            number(&[
                text("12"),
                text("99"),
                text("NLS_NUMERIC_CHARACTERS = '.,'")
            ]),
            Ok(Value::BigInt(12))
        );
        let code = |values: &[Value]| number(values).err().and_then(|error| error.vendor_code());
        assert_eq!(code(&[text("12345"), text("999")]), Some(1722));
        assert_eq!(code(&[text("1.234"), text("9.99")]), Some(1722));
        assert_eq!(code(&[text("1,234"), text("9999")]), Some(1722));
        assert_eq!(code(&[text("12"), text("99X")]), Some(1481));
        assert_eq!(
            code(&[
                text("12"),
                text("99"),
                text("NLS_NUMERIC_CHARACTERS = ',.'")
            ]),
            Some(3001)
        );
        assert_eq!(
            code(&[text("12"), text("99"), text("garbage")]),
            Some(12702)
        );
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
            .unwrap_or(0x0042_5eed);
        let pieces = [
            "9", "0", ".", "D", ",", "G", "$", "S", "MI", "FM", "X", "-", "1", "e", " ",
        ];
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let names = [
            "oracle_to_number",
            "oracle_to_char",
            "oracle_round",
            "oracle_trunc",
            "oracle_divide",
            "oracle_mod",
            "oracle_to_date",
            "oracle_to_timestamp",
            "oracle_add_months",
            "oracle_last_day",
        ];
        for _ in 0..cases {
            let mut first = String::new();
            let mut second = String::new();
            for _ in 0..next() % 7 {
                first.push_str(pieces[next() as usize % pieces.len()]);
                second.push_str(pieces[next() as usize % pieces.len()]);
            }
            let name = names[next() as usize % names.len()];
            let _ = call(name, &[text(&first), text(&second)]);
            let _ = call(name, &[text(&first)]);
        }
    }
}
