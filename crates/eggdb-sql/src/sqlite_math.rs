use crate::types::Value;

pub(crate) const FUNCTIONS: [(&str, &str, &[usize]); 31] = [
    ("acos", "sqlite_math_acos", &[1]),
    ("acosh", "sqlite_math_acosh", &[1]),
    ("asin", "sqlite_math_asin", &[1]),
    ("asinh", "sqlite_math_asinh", &[1]),
    ("atan", "sqlite_math_atan", &[1]),
    ("atan2", "sqlite_math_atan2", &[2]),
    ("atanh", "sqlite_math_atanh", &[1]),
    ("ceil", "sqlite_math_ceil", &[1]),
    ("ceiling", "sqlite_math_ceil", &[1]),
    ("cos", "sqlite_math_cos", &[1]),
    ("cosh", "sqlite_math_cosh", &[1]),
    ("degrees", "sqlite_math_degrees", &[1]),
    ("exp", "sqlite_math_exp", &[1]),
    ("floor", "sqlite_math_floor", &[1]),
    ("ln", "sqlite_math_ln", &[1]),
    ("log", "sqlite_math_log", &[1, 2]),
    ("log10", "sqlite_math_log10", &[1]),
    ("log2", "sqlite_math_log2", &[1]),
    ("mod", "sqlite_math_mod", &[2]),
    ("pi", "sqlite_math_pi", &[0]),
    ("pow", "sqlite_math_pow", &[2]),
    ("power", "sqlite_math_pow", &[2]),
    ("radians", "sqlite_math_radians", &[1]),
    ("sin", "sqlite_math_sin", &[1]),
    ("sinh", "sqlite_math_sinh", &[1]),
    ("tan", "sqlite_math_tan", &[1]),
    ("tanh", "sqlite_math_tanh", &[1]),
    ("trunc", "sqlite_math_trunc", &[1]),
    ("unhex", "sqlite_unhex", &[1, 2]),
    ("zeroblob", "sqlite_zeroblob", &[1]),
    ("randomblob", "sqlite_randomblob", &[1]),
];

const MOST_BLOB_BYTES: i64 = 1_000_000_000;

pub(crate) fn named(name: &str) -> Option<(&'static str, &'static [usize])> {
    FUNCTIONS
        .iter()
        .find(|(public, _, _)| *public == name)
        .map(|(_, internal, counts)| (*internal, *counts))
}

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Null | Value::Bytea(_) => None,
        Value::Text(text) => text.trim().parse::<f64>().ok(),
        Value::Boolean(flag) => Some(f64::from(u8::from(*flag))),
        other => other.as_float(),
    }
}

fn real(result: f64) -> Value {
    if result.is_finite() || result.is_infinite() {
        Value::Double(result)
    } else {
        Value::Null
    }
}

fn rounded(value: &Value, round: fn(f64) -> f64) -> Value {
    if let Some(integer) = value.as_integer()
        && !matches!(value, Value::Text(_) | Value::Double(_) | Value::Real(_))
    {
        return Value::BigInt(integer);
    }
    number(value).map_or(Value::Null, |found| real(round(found)))
}

fn unhex(values: &[Value]) -> Value {
    let Some(text) = values.first().and_then(Value::to_text) else {
        return Value::Null;
    };
    let ignored = values.get(1).and_then(Value::to_text).unwrap_or_default();
    let mut bytes = Vec::with_capacity(text.len() / 2);
    let mut high: Option<u8> = None;
    for character in text.chars() {
        let digit = match character.to_digit(16) {
            Some(digit) => digit as u8,
            None if high.is_none() && ignored.contains(character) => continue,
            None => return Value::Null,
        };
        match high.take() {
            Some(first) => bytes.push(first * 16 + digit),
            None => high = Some(digit),
        }
    }
    if high.is_some() {
        return Value::Null;
    }
    Value::Bytea(bytes)
}

fn fill_random(bytes: &mut [u8]) {
    use std::hash::{BuildHasher, Hasher};
    for (number, chunk) in bytes.chunks_mut(8).enumerate() {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_usize(number);
        let drawn = hasher.finish().to_le_bytes();
        chunk.copy_from_slice(&drawn[..chunk.len()]);
    }
}

fn blob_length(value: &Value) -> usize {
    let wanted = value.as_integer().unwrap_or(0).clamp(0, MOST_BLOB_BYTES);
    usize::try_from(wanted).unwrap_or(0)
}

pub(crate) fn call(name: &str, values: &[Value]) -> Option<Value> {
    let first = values.first().unwrap_or(&Value::Null);
    let second = values.get(1).unwrap_or(&Value::Null);
    let unary = |apply: fn(f64) -> f64| number(first).map_or(Value::Null, |x| real(apply(x)));
    let within = |apply: fn(f64) -> f64, low: f64, high: f64| {
        number(first)
            .filter(|x| (low..=high).contains(x))
            .map_or(Value::Null, |x| real(apply(x)))
    };
    Some(match name {
        "sqlite_math_acos" => within(f64::acos, -1.0, 1.0),
        "sqlite_math_asin" => within(f64::asin, -1.0, 1.0),
        "sqlite_math_acosh" => within(f64::acosh, 1.0, f64::INFINITY),
        "sqlite_math_atanh" => number(first)
            .filter(|x| *x > -1.0 && *x < 1.0)
            .map_or(Value::Null, |x| real(x.atanh())),
        "sqlite_math_asinh" => unary(f64::asinh),
        "sqlite_math_atan" => unary(f64::atan),
        "sqlite_math_atan2" => match (number(first), number(second)) {
            (Some(y), Some(x)) => real(y.atan2(x)),
            _ => Value::Null,
        },
        "sqlite_math_cos" => unary(f64::cos),
        "sqlite_math_cosh" => unary(f64::cosh),
        "sqlite_math_sin" => unary(f64::sin),
        "sqlite_math_sinh" => unary(f64::sinh),
        "sqlite_math_tan" => unary(f64::tan),
        "sqlite_math_tanh" => unary(f64::tanh),
        "sqlite_math_degrees" => unary(f64::to_degrees),
        "sqlite_math_radians" => unary(f64::to_radians),
        "sqlite_math_exp" => unary(f64::exp),
        "sqlite_math_ln" => number(first)
            .filter(|x| *x > 0.0)
            .map_or(Value::Null, |x| real(x.ln())),
        "sqlite_math_log10" => number(first)
            .filter(|x| *x > 0.0)
            .map_or(Value::Null, |x| real(x.log10())),
        "sqlite_math_log2" => number(first)
            .filter(|x| *x > 0.0)
            .map_or(Value::Null, |x| real(x.log2())),
        "sqlite_math_log" if values.len() == 2 => match (number(first), number(second)) {
            (Some(base), Some(x)) if base > 0.0 && base != 1.0 && x > 0.0 => real(x.ln() / base.ln()),
            _ => Value::Null,
        },
        "sqlite_math_log" => number(first)
            .filter(|x| *x > 0.0)
            .map_or(Value::Null, |x| real(x.log10())),
        "sqlite_math_mod" => match (number(first), number(second)) {
            (Some(x), Some(y)) if y != 0.0 => real(x % y),
            _ => Value::Null,
        },
        "sqlite_math_pi" => Value::Double(std::f64::consts::PI),
        "sqlite_math_pow" => match (number(first), number(second)) {
            (Some(x), Some(y)) => real(x.powf(y)),
            _ => Value::Null,
        },
        "sqlite_math_ceil" => rounded(first, f64::ceil),
        "sqlite_math_floor" => rounded(first, f64::floor),
        "sqlite_math_trunc" => rounded(first, f64::trunc),
        "sqlite_unhex" => unhex(values),
        "sqlite_zeroblob" => Value::Bytea(vec![0; blob_length(first)]),
        "sqlite_randomblob" => {
            let mut bytes = vec![0; blob_length(first).max(1)];
            fill_random(&mut bytes);
            Value::Bytea(bytes)
        }
        _ => return None,
    })
}
