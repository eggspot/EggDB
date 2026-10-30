use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{Value, compare_values};
use std::cmp::Ordering;
use std::hash::{BuildHasher, Hasher};

const SIGNIFICANT_DIGITS: usize = 15;

pub(crate) const MAX_TEXT_BYTES: usize = 1_000_000_000;

fn too_big() -> SqlError {
    SqlError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "string or blob too big").with_sqlite_code(18)
}

fn within_limit(bytes: usize) -> SqlResult<()> {
    if bytes > MAX_TEXT_BYTES {
        Err(too_big())
    } else {
        crate::memory::check_value_bytes(bytes)
    }
}

pub(crate) fn real_text(value: f64) -> Option<String> {
    if value.is_nan() {
        return None;
    }
    if value.is_infinite() {
        return Some(if value > 0.0 { "Inf" } else { "-Inf" }.to_owned());
    }
    let (mantissa, exponent) = scientific_parts(value, SIGNIFICANT_DIGITS - 1);
    let digits = mantissa.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let negative = value.is_sign_negative() && value != 0.0;
    let sign = if negative { "-" } else { "" };
    if !(-4..15).contains(&exponent) {
        let (first, rest) = digits.split_at(1);
        let rest = if rest.is_empty() { "0" } else { rest };
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        return Some(format!(
            "{sign}{first}.{rest}e{exponent_sign}{:02}",
            exponent.unsigned_abs()
        ));
    }
    let mut out = String::from(sign);
    if exponent < 0 {
        out.push_str("0.");
        for _ in 0..(-exponent - 1) {
            out.push('0');
        }
        out.push_str(digits);
        return Some(out);
    }
    let whole = exponent as usize + 1;
    if digits.len() <= whole {
        out.push_str(digits);
        for _ in digits.len()..whole {
            out.push('0');
        }
        out.push_str(".0");
    } else {
        out.push_str(&digits[..whole]);
        out.push('.');
        out.push_str(&digits[whole..]);
    }
    Some(out)
}

fn scientific_parts(value: f64, decimals: usize) -> (String, i32) {
    let text = format!("{:.*e}", decimals, value.abs());
    let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    (digits, exponent.parse().unwrap_or(0))
}

fn text_of(value: &Value) -> Option<String> {
    Dialect::Sqlite.value_text(value)
}

fn numeric_prefix(text: &str) -> f64 {
    let trimmed = text.trim_start();
    let mut end = 0;
    let bytes = trimmed.as_bytes();
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        end = 1;
    }
    let mut seen_digit = false;
    let mut seen_point = false;
    let mut seen_exponent = false;
    let mut valid_end = 0;
    while end < bytes.len() {
        match bytes[end] {
            b'0'..=b'9' => {
                seen_digit = true;
                valid_end = end + 1;
            }
            b'.' if !seen_point && !seen_exponent => seen_point = true,
            b'e' | b'E' if seen_digit && !seen_exponent => {
                seen_exponent = true;
                if matches!(bytes.get(end + 1), Some(b'+' | b'-')) {
                    end += 1;
                }
            }
            _ => break,
        }
        end += 1;
    }
    trimmed[..valid_end].parse().unwrap_or(0.0)
}

fn number_of(value: &Value) -> Option<f64> {
    match value {
        Value::Null => None,
        Value::Boolean(flag) => Some(f64::from(u8::from(*flag))),
        Value::Text(text) => Some(numeric_prefix(text)),
        other => other.as_float(),
    }
}

fn integer_of(value: &Value) -> Option<i64> {
    match value {
        Value::Boolean(flag) => Some(i64::from(*flag)),
        other => other
            .as_integer()
            .or_else(|| number_of(other).map(|number| number as i64)),
    }
}

fn wrong_type(name: &str) -> SqlError {
    SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        format!("{name}() was given a value it cannot take"),
    )
}

fn class(value: &Value) -> u8 {
    match value {
        Value::Null => 0,
        Value::Text(_) => 2,
        _ => 1,
    }
}

fn sqlite_compare(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Boolean(flag), other) => sqlite_compare(&Value::BigInt(i64::from(*flag)), other),
        (other, Value::Boolean(flag)) => sqlite_compare(other, &Value::BigInt(i64::from(*flag))),
        _ if class(left) != class(right) => class(left).cmp(&class(right)),
        _ => compare_values(left, right),
    }
}

fn chars_between(text: &str, start: i64, length: Option<i64>) -> String {
    let count = text.chars().count() as i64;
    let mut first = start;
    let mut take = length.unwrap_or(i64::MAX / 4);
    if first < 0 {
        first += count;
        if first < 0 {
            take = take.saturating_add(first);
            first = 0;
        }
    } else if first > 0 {
        first -= 1;
    } else if take > 0 {
        take -= 1;
    }
    if take < 0 {
        first = first.saturating_add(take);
        take = take.saturating_neg();
        if first < 0 {
            take = take.saturating_add(first);
            first = 0;
        }
    }
    if take <= 0 || first >= count {
        return String::new();
    }
    text.chars()
        .skip(first as usize)
        .take(take.min(count) as usize)
        .collect()
}

fn trimmed(text: &str, characters: &str, left: bool, right: bool) -> String {
    let set: Vec<char> = characters.chars().collect();
    let mut out = text;
    if left {
        out = out.trim_start_matches(|c| set.contains(&c));
    }
    if right {
        out = out.trim_end_matches(|c| set.contains(&c));
    }
    out.to_owned()
}

fn rounded(value: f64, digits: i64) -> f64 {
    let digits = digits.clamp(0, 30);
    if digits > 15 || !value.is_finite() {
        return value;
    }
    let scale = 10f64.powi(digits as i32);
    let scaled = value * scale;
    if !scaled.is_finite() || scaled.abs() >= 9.0e15 {
        return value;
    }
    scaled.round() / scale
}

fn random_integer() -> i64 {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(0x5eed);
    hasher.finish() as i64
}

fn quoted(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Text(text) => format!("'{}'", text.replace('\'', "''")),
        Value::Bytea(bytes) => format!(
            "X'{}'",
            crate::bytea::format_hex(bytes)[2..].to_ascii_uppercase()
        ),
        other => text_of(other).unwrap_or_else(|| "NULL".to_owned()),
    }
}

pub(crate) fn truth(value: &Value) -> Value {
    match value {
        Value::Null => Value::Null,
        Value::Boolean(flag) => Value::Boolean(*flag),
        other => Value::Boolean(number_of(other).is_some_and(|number| number != 0.0)),
    }
}

pub(crate) fn call(name: &str, values: &[Value]) -> SqlResult<Value> {
    let text = |index: usize| values.get(index).and_then(text_of);
    let any_null = values.iter().any(Value::is_null);
    let null = Value::Null;
    let first = values.first().unwrap_or(&null);
    let second = values.get(1).unwrap_or(&null);
    Ok(match name {
        "sqlite_truth" => truth(first),
        "sqlite_typeof" => Value::Text(
            match first {
                Value::Null => "null",
                Value::Text(_) => "text",
                Value::Bytea(_) => "blob",
                Value::Real(_) | Value::Double(_) => "real",
                _ => "integer",
            }
            .to_owned(),
        ),
        "sqlite_random" => Value::BigInt(random_integer()),
        _ if any_null && name == "sqlite_hex" => Value::Text(String::new()),
        "sqlite_quote" => {
            let length = text_of(first).map_or(4, |text| text.len());
            within_limit(length.saturating_mul(2).saturating_add(2))?;
            Value::Text(quoted(first))
        }
        "sqlite_min" | "sqlite_max" => {
            if any_null {
                return Ok(Value::Null);
            }
            let mut best = first;
            for value in values.iter().skip(1) {
                let ordering = sqlite_compare(value, best);
                if (name == "sqlite_min" && ordering == Ordering::Less)
                    || (name == "sqlite_max" && ordering == Ordering::Greater)
                {
                    best = value;
                }
            }
            best.clone()
        }
        "sqlite_printf" => match text(0) {
            None => Value::Null,
            Some(format) => Value::Text(printf(&format, values.get(1..).unwrap_or(&[]))?),
        },
        _ if any_null => Value::Null,
        "sqlite_length" => match first {
            Value::Bytea(bytes) => Value::BigInt(i64::try_from(bytes.len()).unwrap_or(i64::MAX)),
            _ => Value::BigInt(text(0).unwrap_or_default().chars().count() as i64),
        },
        "sqlite_substr" => {
            let start = integer_of(second).unwrap_or(0);
            let length = values.get(2).and_then(integer_of);
            Value::Text(chars_between(&text(0).unwrap_or_default(), start, length))
        }
        "sqlite_upper" => Value::Text(text(0).unwrap_or_default().to_ascii_uppercase()),
        "sqlite_lower" => Value::Text(text(0).unwrap_or_default().to_ascii_lowercase()),
        "sqlite_trim" | "sqlite_ltrim" | "sqlite_rtrim" => {
            let characters = text(1).unwrap_or_else(|| " ".to_owned());
            Value::Text(trimmed(
                &text(0).unwrap_or_default(),
                &characters,
                name != "sqlite_rtrim",
                name != "sqlite_ltrim",
            ))
        }
        "sqlite_replace" => {
            let subject = text(0).unwrap_or_default();
            let pattern = text(1).unwrap_or_default();
            if pattern.is_empty() {
                Value::Text(subject)
            } else {
                let replacement = text(2).unwrap_or_default();
                if replacement.len() > pattern.len() {
                    let found = subject.matches(pattern.as_str()).count();
                    let growth = found.saturating_mul(replacement.len() - pattern.len());
                    within_limit(subject.len().saturating_add(growth))?;
                }
                Value::Text(subject.replace(&pattern, &replacement))
            }
        }
        "sqlite_instr" => {
            let haystack = text(0).unwrap_or_default();
            let needle = text(1).unwrap_or_default();
            Value::BigInt(match haystack.find(&needle) {
                Some(at) => haystack[..at].chars().count() as i64 + 1,
                None => 0,
            })
        }
        "sqlite_round" => {
            let number = number_of(first).ok_or_else(|| wrong_type("round"))?;
            let digits = values.get(1).and_then(integer_of).unwrap_or(0);
            Value::Double(rounded(number, digits))
        }
        "sqlite_hex" => {
            let bytes = text(0).unwrap_or_default();
            within_limit(bytes.len().saturating_mul(2))?;
            Value::Text(bytes.bytes().map(|byte| format!("{byte:02X}")).collect())
        }
        _ => {
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("function {name}() does not exist"),
            ));
        }
    })
}

struct Spec {
    left: bool,
    plus: bool,
    space: bool,
    zero: bool,
    width: usize,
    precision: Option<usize>,
}

fn take_number(characters: &[char], at: &mut usize) -> Option<usize> {
    let start = *at;
    while characters.get(*at).is_some_and(char::is_ascii_digit) {
        *at += 1;
    }
    if *at == start {
        return None;
    }
    characters[start..*at]
        .iter()
        .collect::<String>()
        .parse::<usize>()
        .ok()
        .map(|number| number.min(1000))
}

fn padded(body: String, spec: &Spec, numeric: bool) -> String {
    let length = body.chars().count();
    if length >= spec.width {
        return body;
    }
    let fill = spec.width - length;
    if spec.left {
        return format!("{body}{}", " ".repeat(fill));
    }
    if spec.zero && numeric {
        let (sign, digits) = match body.chars().next() {
            Some(first @ ('-' | '+' | ' ')) => (first.to_string(), body[1..].to_owned()),
            _ => (String::new(), body),
        };
        return format!("{sign}{}{digits}", "0".repeat(fill));
    }
    format!("{}{body}", " ".repeat(fill))
}

fn signed(body: String, negative: bool, spec: &Spec) -> String {
    if negative {
        format!("-{body}")
    } else if spec.plus {
        format!("+{body}")
    } else if spec.space {
        format!(" {body}")
    } else {
        body
    }
}

fn exponent_form(value: f64, precision: usize, upper: bool) -> String {
    let (digits, exponent) = scientific_parts(value, precision);
    let (first, rest) = digits.split_at(1.min(digits.len()));
    let mut out = first.to_owned();
    if precision > 0 {
        out.push('.');
        out.push_str(rest);
    }
    let sign = if exponent < 0 { '-' } else { '+' };
    out.push(if upper { 'E' } else { 'e' });
    out.push(sign);
    out.push_str(&format!("{:02}", exponent.unsigned_abs()));
    out
}

fn general_form(value: f64, precision: usize, upper: bool) -> String {
    let precision = precision.max(1);
    if value == 0.0 {
        return "0".to_owned();
    }
    let (_, exponent) = scientific_parts(value, precision - 1);
    if exponent < -4 || exponent >= precision as i32 {
        let text = exponent_form(value, precision - 1, upper);
        let (mantissa, tail) = text.split_at(text.find(['e', 'E']).unwrap_or(text.len()));
        let mantissa = if mantissa.contains('.') {
            mantissa.trim_end_matches('0').trim_end_matches('.')
        } else {
            mantissa
        };
        format!("{mantissa}{tail}")
    } else {
        let decimals = (precision as i32 - 1 - exponent).max(0) as usize;
        let text = format!("{:.*}", decimals, value.abs());
        if text.contains('.') {
            text.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            text
        }
    }
}

fn printf(format: &str, arguments: &[Value]) -> SqlResult<String> {
    let characters: Vec<char> = format.chars().collect();
    let mut next = arguments.iter();
    let mut out = String::new();
    let mut at = 0;
    while at < characters.len() {
        let character = characters[at];
        at += 1;
        if character != '%' {
            out.push(character);
            continue;
        }
        let mut spec = Spec {
            left: false,
            plus: false,
            space: false,
            zero: false,
            width: 0,
            precision: None,
        };
        while let Some(&flag) = characters.get(at) {
            match flag {
                '-' => spec.left = true,
                '+' => spec.plus = true,
                ' ' => spec.space = true,
                '0' => spec.zero = true,
                '#' | ',' | '!' => {}
                _ => break,
            }
            at += 1;
        }
        if characters.get(at) == Some(&'*') {
            at += 1;
            let width = next.next().and_then(integer_of).unwrap_or(0);
            spec.left |= width < 0;
            spec.width = width.unsigned_abs().min(1000) as usize;
        } else {
            spec.width = take_number(&characters, &mut at).unwrap_or(0);
        }
        if characters.get(at) == Some(&'.') {
            at += 1;
            if characters.get(at) == Some(&'*') {
                at += 1;
                spec.precision =
                    Some(next.next().and_then(integer_of).unwrap_or(0).clamp(0, 1000) as usize);
            } else {
                spec.precision = Some(take_number(&characters, &mut at).unwrap_or(0));
            }
        }
        while characters.get(at).is_some_and(|&c| c == 'l' || c == 'h') {
            at += 1;
        }
        let Some(&kind) = characters.get(at) else {
            break;
        };
        at += 1;
        let null = Value::Null;
        let argument = if kind == '%' {
            &null
        } else {
            next.next().unwrap_or(&null)
        };
        let piece = match kind {
            '%' => "%".to_owned(),
            'd' | 'i' | 'u' => {
                let number = integer_of(argument).unwrap_or(0);
                let body = number.unsigned_abs().to_string();
                padded(signed(body, number < 0, &spec), &spec, true)
            }
            'x' | 'X' | 'o' => {
                let number = integer_of(argument).unwrap_or(0) as u64;
                let body = match kind {
                    'x' => format!("{number:x}"),
                    'X' => format!("{number:X}"),
                    _ => format!("{number:o}"),
                };
                padded(body, &spec, true)
            }
            'f' | 'F' | 'e' | 'E' | 'g' | 'G' => {
                let number = number_of(argument).unwrap_or(0.0);
                let precision = spec.precision.unwrap_or(6).min(350);
                let body = match kind {
                    'f' | 'F' => format!("{:.*}", precision, number.abs()),
                    'e' | 'E' => exponent_form(number, precision, kind == 'E'),
                    _ => general_form(number, precision, kind == 'G'),
                };
                padded(signed(body, number < 0.0, &spec), &spec, true)
            }
            'c' => {
                let first: String = text_of(argument)
                    .unwrap_or_default()
                    .chars()
                    .take(1)
                    .collect();
                padded(first, &spec, false)
            }
            's' | 'z' | 'q' | 'Q' | 'w' => {
                let mut body = match (kind, argument) {
                    ('Q', Value::Null) => "NULL".to_owned(),
                    (_, value) => text_of(value).unwrap_or_default(),
                };
                if let Some(precision) = spec.precision
                    && kind != 'Q'
                {
                    body = body.chars().take(precision).collect();
                }
                body = match (kind, argument) {
                    ('q', _) => body.replace('\'', "''"),
                    ('w', _) => body.replace('"', "\"\""),
                    ('Q', Value::Null) => body,
                    ('Q', _) => format!("'{}'", body.replace('\'', "''")),
                    _ => body,
                };
                padded(body, &spec, false)
            }
            other => {
                out.push('%');
                out.push(other);
                continue;
            }
        };
        within_limit(out.len().saturating_add(piece.len()))?;
        out.push_str(&piece);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{call, chars_between, printf, real_text};
    use crate::types::Value;

    #[test]
    fn reals_print_with_fifteen_digits_and_a_point() {
        let cases = [
            (1.0, "1.0"),
            (0.5, "0.5"),
            (-2.25, "-2.25"),
            (1e20, "1.0e+20"),
            (1.0 / 3.0, "0.333333333333333"),
            (0.1 + 0.2, "0.3"),
            (123456789012345.0, "123456789012345.0"),
            (1e15, "1.0e+15"),
            (0.0001, "0.0001"),
            (0.00001, "1.0e-05"),
            (0.0, "0.0"),
            (f64::INFINITY, "Inf"),
        ];
        for (value, text) in cases {
            assert_eq!(real_text(value).as_deref(), Some(text), "{value}");
        }
        assert_eq!(real_text(f64::NAN), None);
    }

    #[test]
    fn substr_counts_from_one_from_the_end_and_backwards() {
        assert_eq!(chars_between("hello", 2, Some(3)), "ell");
        assert_eq!(chars_between("hello", -3, None), "llo");
        assert_eq!(chars_between("hello", 0, Some(2)), "h");
        assert_eq!(chars_between("hello", 4, Some(-2)), "el");
        assert_eq!(chars_between("hello", 9, Some(2)), "");
        assert_eq!(chars_between("héllo", 2, Some(1)), "é");
    }

    #[test]
    fn printf_formats_numbers_strings_and_quotes() {
        let args = [
            Value::BigInt(42),
            Value::Double(1.23456),
            Value::Text("it's".into()),
            Value::Null,
        ];
        assert_eq!(
            printf("%d|%5.2f|%q|%Q|%-4s|%%", &args).as_deref(),
            Ok("42| 1.23|it''s|NULL|    |%")
        );
        assert_eq!(
            printf(
                "%05d %x %e %g",
                &[
                    Value::BigInt(-42),
                    Value::BigInt(255),
                    Value::Double(1234.5),
                    Value::Double(0.0001)
                ]
            )
            .as_deref(),
            Ok("-0042 ff 1.234500e+03 0.0001")
        );
        assert_eq!(call("sqlite_printf", &[Value::Null]), Ok(Value::Null));
    }

    #[test]
    fn results_past_the_length_limit_are_refused_before_they_are_built() {
        let subject = Value::Text(" ".repeat(1000));
        let replacement = Value::Text("x".repeat(2_000_000));
        let error = call(
            "sqlite_replace",
            &[subject.clone(), Value::Text(" ".into()), replacement],
        )
        .expect_err("2 GB");
        assert_eq!(error.message(), "string or blob too big");
        assert_eq!(error.vendor_code(), Some(18));
        assert_eq!(
            call(
                "sqlite_replace",
                &[subject, Value::Text(" ".into()), Value::Text("ab".into())]
            )
            .map(|value| value.to_text().map(|text| text.len())),
            Ok(Some(2000))
        );
    }
}
