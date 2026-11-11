use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{SqlType, Value};

const MAX_RAW_BYTES: usize = 32_767;
const DEFAULT_LOB_AMOUNT: i64 = 32_767;

/// The internal Oracle function for a function of a package Oracle supplies (`UTL_RAW`,
/// `DBMS_LOB`), with its argument counts; `None` for another name.
pub(crate) fn internal_name(package: &str, function: &str) -> Option<(&'static str, usize, usize)> {
    Some(match (package, function) {
        ("utl_raw", "cast_to_raw") => ("oracle_utl_raw_cast_to_raw", 1, 1),
        ("utl_raw", "cast_to_varchar2") => ("oracle_utl_raw_cast_to_varchar2", 1, 1),
        ("utl_raw", "length") => ("oracle_utl_raw_length", 1, 1),
        ("utl_raw", "concat") => ("oracle_utl_raw_concat", 1, 12),
        ("utl_raw", "substr") => ("oracle_utl_raw_substr", 2, 3),
        ("utl_raw", "compare") => ("oracle_utl_raw_compare", 2, 3),
        ("utl_raw", "bit_and") => ("oracle_utl_raw_bit_and", 2, 2),
        ("utl_raw", "bit_or") => ("oracle_utl_raw_bit_or", 2, 2),
        ("utl_raw", "bit_xor") => ("oracle_utl_raw_bit_xor", 2, 2),
        ("utl_raw", "bit_complement") => ("oracle_utl_raw_bit_complement", 1, 1),
        ("utl_raw", "reverse") => ("oracle_utl_raw_reverse", 1, 1),
        ("utl_raw", "copies") => ("oracle_utl_raw_copies", 2, 2),
        ("dbms_lob", "getlength") => ("oracle_dbms_lob_getlength", 1, 1),
        ("dbms_lob", "substr") => ("oracle_dbms_lob_substr", 1, 3),
        ("dbms_lob", "instr") => ("oracle_dbms_lob_instr", 2, 4),
        ("dbms_lob", "compare") => ("oracle_dbms_lob_compare", 2, 5),
        _ => return None,
    })
}

pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    Some(match name {
        "oracle_utl_raw_cast_to_raw"
        | "oracle_utl_raw_concat"
        | "oracle_utl_raw_substr"
        | "oracle_utl_raw_bit_and"
        | "oracle_utl_raw_bit_or"
        | "oracle_utl_raw_bit_xor"
        | "oracle_utl_raw_bit_complement"
        | "oracle_utl_raw_reverse"
        | "oracle_utl_raw_copies" => SqlType::Bytea,
        "oracle_utl_raw_cast_to_varchar2" => SqlType::Text,
        "oracle_utl_raw_length"
        | "oracle_utl_raw_compare"
        | "oracle_dbms_lob_getlength"
        | "oracle_dbms_lob_instr"
        | "oracle_dbms_lob_compare" => SqlType::Numeric(None),
        _ => return None,
    })
}

fn numeric_error(message: &str, code: u16) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message.to_owned()).with_oracle_code(code)
}

fn raw_of(value: &Value) -> SqlResult<Option<Vec<u8>>> {
    Ok(match value {
        Value::Null => None,
        Value::Bytea(bytes) => Some(bytes.clone()),
        Value::Text(text) => Some(hex_bytes(text)?),
        other => Some(hex_bytes(&other.to_text().unwrap_or_default())?),
    })
}

fn hex_bytes(text: &str) -> SqlResult<Vec<u8>> {
    let digits: Vec<u8> = text.bytes().collect();
    let padded = if digits.len() % 2 == 1 {
        let mut padded = vec![b'0'];
        padded.extend_from_slice(&digits);
        padded
    } else {
        digits
    };
    padded
        .chunks(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect::<Option<Vec<u8>>>()
        .ok_or_else(|| numeric_error("hex to raw conversion error", 1465))
}

fn whole(value: &Value) -> Option<i64> {
    value.as_integer().or_else(|| value.as_float().map(|number| number.trunc() as i64))
}

fn number(value: i64) -> Value {
    Value::Numeric(crate::numeric::Numeric::from_i64(value))
}

fn raw_result(bytes: Vec<u8>) -> SqlResult<Value> {
    if bytes.len() > MAX_RAW_BYTES {
        return Err(numeric_error("resulting RAW is longer than 32767 bytes", 6502));
    }
    Ok(if bytes.is_empty() {
        Value::Null
    } else {
        Value::Bytea(bytes)
    })
}

fn bitwise(left: &Value, right: &Value, operation: fn(u8, u8) -> u8) -> SqlResult<Value> {
    let (Some(left), Some(right)) = (raw_of(left)?, raw_of(right)?) else {
        return Ok(Value::Null);
    };
    let (short, long) = if left.len() <= right.len() {
        (left, right)
    } else {
        (right, left)
    };
    let mut out: Vec<u8> = short
        .iter()
        .zip(&long)
        .map(|(a, b)| operation(*a, *b))
        .collect();
    out.extend_from_slice(&long[short.len()..]);
    raw_result(out)
}

fn lob_units(value: &Value) -> Option<Vec<Vec<u8>>> {
    match value {
        Value::Null => None,
        Value::Bytea(bytes) => Some(bytes.iter().map(|byte| vec![*byte]).collect()),
        Value::Text(text) => Some(
            text.chars()
                .map(|character| character.to_string().into_bytes())
                .collect(),
        ),
        other => other
            .to_text()
            .ok()
            .map(|text| text.chars().map(|c| c.to_string().into_bytes()).collect()),
    }
}

fn lob_value(like: &Value, units: &[Vec<u8>]) -> Value {
    if units.is_empty() {
        return Value::Null;
    }
    let bytes: Vec<u8> = units.concat();
    match like {
        Value::Bytea(_) => Value::Bytea(bytes),
        _ => Value::Text(String::from_utf8_lossy(&bytes).into_owned()),
    }
}

fn substring_units(units: &[Vec<u8>], offset: i64, amount: i64) -> Option<&[Vec<u8>]> {
    if offset < 1 || amount < 1 {
        return None;
    }
    let start = usize::try_from(offset - 1).ok()?;
    if start >= units.len() {
        return None;
    }
    let end = start.saturating_add(usize::try_from(amount).ok()?).min(units.len());
    Some(&units[start..end])
}

/// Evaluates a function of `UTL_RAW` or `DBMS_LOB`; `None` for another name.
pub(crate) fn call(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !name.starts_with("oracle_utl_raw_") && !name.starts_with("oracle_dbms_lob_") {
        return None;
    }
    Some(evaluate(name, values))
}

fn evaluate(name: &str, values: &[Value]) -> SqlResult<Value> {
    match (name, values) {
        ("oracle_utl_raw_cast_to_raw", [text]) => match text {
            Value::Null => Ok(Value::Null),
            Value::Bytea(bytes) => raw_result(bytes.clone()),
            other => raw_result(other.to_text().unwrap_or_default().into_bytes()),
        },
        ("oracle_utl_raw_cast_to_varchar2", [raw]) => Ok(match raw_of(raw)? {
            None => Value::Null,
            Some(bytes) if bytes.is_empty() => Value::Null,
            Some(bytes) => Value::Text(String::from_utf8_lossy(&bytes).into_owned()),
        }),
        ("oracle_utl_raw_length", [raw]) => {
            Ok(raw_of(raw)?.map_or(Value::Null, |bytes| number(bytes.len() as i64)))
        }
        ("oracle_utl_raw_concat", parts) => {
            let mut out = Vec::new();
            for part in parts {
                if let Some(bytes) = raw_of(part)? {
                    out.extend_from_slice(&bytes);
                }
            }
            raw_result(out)
        }
        ("oracle_utl_raw_substr", [raw, position, rest @ ..]) => {
            let Some(bytes) = raw_of(raw)? else {
                return Ok(Value::Null);
            };
            let Some(position) = whole(position) else {
                return Ok(Value::Null);
            };
            let length = bytes.len() as i64;
            let start = if position < 0 { length + position } else { position - 1 };
            if position == 0 || start < 0 || start >= length {
                return Err(numeric_error("numeric or value error", 6502));
            }
            let count = match rest.first() {
                Some(count) => match whole(count) {
                    Some(count) if count >= 1 && start + count <= length => count,
                    Some(_) => return Err(numeric_error("numeric or value error", 6502)),
                    None => return Ok(Value::Null),
                },
                None => length - start,
            };
            raw_result(bytes[start as usize..(start + count) as usize].to_vec())
        }
        ("oracle_utl_raw_compare", [left, right, rest @ ..]) => {
            let left = raw_of(left)?.unwrap_or_default();
            let right = raw_of(right)?.unwrap_or_default();
            let pad = match rest.first() {
                Some(pad) => raw_of(pad)?.and_then(|bytes| bytes.first().copied()).unwrap_or(0),
                None => 0,
            };
            let length = left.len().max(right.len());
            for at in 0..length {
                let a = left.get(at).copied().unwrap_or(pad);
                let b = right.get(at).copied().unwrap_or(pad);
                if a != b {
                    return Ok(number(at as i64 + 1));
                }
            }
            Ok(number(0))
        }
        ("oracle_utl_raw_bit_and", [left, right]) => bitwise(left, right, |a, b| a & b),
        ("oracle_utl_raw_bit_or", [left, right]) => bitwise(left, right, |a, b| a | b),
        ("oracle_utl_raw_bit_xor", [left, right]) => bitwise(left, right, |a, b| a ^ b),
        ("oracle_utl_raw_bit_complement", [raw]) => match raw_of(raw)? {
            None => Ok(Value::Null),
            Some(bytes) => raw_result(bytes.iter().map(|byte| !byte).collect()),
        },
        ("oracle_utl_raw_reverse", [raw]) => match raw_of(raw)? {
            None => Ok(Value::Null),
            Some(mut bytes) => {
                bytes.reverse();
                raw_result(bytes)
            }
        },
        ("oracle_utl_raw_copies", [raw, count]) => {
            let (Some(bytes), Some(count)) = (raw_of(raw)?, whole(count)) else {
                return Ok(Value::Null);
            };
            if count < 1 || bytes.is_empty() {
                return Err(numeric_error("numeric or value error", 6502));
            }
            let total = bytes.len().saturating_mul(usize::try_from(count).unwrap_or(usize::MAX));
            if total > MAX_RAW_BYTES {
                return Err(numeric_error("resulting RAW is longer than 32767 bytes", 6502));
            }
            raw_result(bytes.repeat(count as usize))
        }
        ("oracle_dbms_lob_getlength", [lob]) => {
            Ok(lob_units(lob).map_or(Value::Null, |units| number(units.len() as i64)))
        }
        ("oracle_dbms_lob_substr", [lob, rest @ ..]) => {
            let Some(units) = lob_units(lob) else {
                return Ok(Value::Null);
            };
            let amount = rest.first().map_or(Some(DEFAULT_LOB_AMOUNT), whole);
            let offset = rest.get(1).map_or(Some(1), whole);
            let (Some(amount), Some(offset)) = (amount, offset) else {
                return Ok(Value::Null);
            };
            Ok(substring_units(&units, offset, amount)
                .map_or(Value::Null, |part| lob_value(lob, part)))
        }
        ("oracle_dbms_lob_instr", [lob, pattern, rest @ ..]) => {
            let (Some(units), Some(wanted)) = (lob_units(lob), lob_units(pattern)) else {
                return Ok(Value::Null);
            };
            let offset = rest.first().map_or(Some(1), whole);
            let nth = rest.get(1).map_or(Some(1), whole);
            let (Some(offset), Some(nth)) = (offset, nth) else {
                return Ok(Value::Null);
            };
            if offset < 1 || nth < 1 || wanted.is_empty() {
                return Ok(Value::Null);
            }
            let mut seen = 0;
            let mut at = (offset - 1) as usize;
            while at + wanted.len() <= units.len() {
                if units[at..at + wanted.len()] == wanted[..] {
                    seen += 1;
                    if seen == nth {
                        return Ok(number(at as i64 + 1));
                    }
                }
                at += 1;
            }
            Ok(number(0))
        }
        ("oracle_dbms_lob_compare", [left, right, rest @ ..]) => {
            let (Some(left), Some(right)) = (lob_units(left), lob_units(right)) else {
                return Ok(Value::Null);
            };
            let amount = rest.first().and_then(whole).unwrap_or(i64::MAX);
            let left_offset = rest.get(1).and_then(whole).unwrap_or(1);
            let right_offset = rest.get(2).and_then(whole).unwrap_or(1);
            if amount < 1 || left_offset < 1 || right_offset < 1 {
                return Ok(Value::Null);
            }
            let take = |units: &[Vec<u8>], offset: i64| -> Vec<Vec<u8>> {
                units
                    .iter()
                    .skip((offset - 1) as usize)
                    .take(usize::try_from(amount).unwrap_or(usize::MAX))
                    .cloned()
                    .collect()
            };
            let (left, right) = (take(&left, left_offset), take(&right, right_offset));
            Ok(number(match left.cmp(&right) {
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Greater => 1,
            }))
        }
        _ => Ok(Value::Null),
    }
}
