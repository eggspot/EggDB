//! MySQL's string and number functions the other dialects spell differently (D1291):
//! `LOCATE`, `SUBSTRING_INDEX`, `FIND_IN_SET`, `ELT`, `MID`, `STRCMP`, `ORD`, `BIN`, `CONV`,
//! `CRC32`, `UNHEX` and `RAND`, answered in MySQL's dialect only, with MySQL's NULLs for
//! arguments it cannot use.

use crate::dialect::Dialect;
use crate::error::SqlResult;
use crate::types::{SqlType, Value};

/// The functions this module answers in MySQL's dialect.
pub(crate) const FUNCTIONS: [&str; 12] = [
    "locate", "substring_index", "find_in_set", "elt", "mid", "strcmp", "ord", "bin", "conv", "crc32", "unhex", "rand",
];

fn text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Bytea(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        other => other.to_text(),
    }
}

fn folded(text: &str) -> Vec<char> {
    text.chars().map(|character| character.to_lowercase().next().unwrap_or(character)).collect()
}

fn locate(needle: &str, haystack: &str, from: i64) -> i64 {
    let (needle, haystack) = (folded(needle), folded(haystack));
    let Some(start) = usize::try_from(from).ok().and_then(|from| from.checked_sub(1)) else {
        return 0;
    };
    if start > haystack.len() {
        return 0;
    }
    if needle.is_empty() {
        return from;
    }
    (start..=haystack.len().saturating_sub(needle.len()))
        .find(|at| haystack[*at..].starts_with(&needle))
        .map_or(0, |at| i64::try_from(at + 1).unwrap_or(0))
}

fn substring_index(text: &str, delimiter: &str, count: i64) -> String {
    if delimiter.is_empty() || count == 0 {
        return String::new();
    }
    let pieces: Vec<&str> = text.split(delimiter).collect();
    let wanted = usize::try_from(count.unsigned_abs()).unwrap_or(usize::MAX);
    if wanted >= pieces.len() {
        return text.to_owned();
    }
    if count > 0 {
        pieces[..wanted].join(delimiter)
    } else {
        pieces[pieces.len() - wanted..].join(delimiter)
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn digits_in(text: &str, base: u32) -> Option<(bool, u64)> {
    let trimmed = text.trim();
    let (negative, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed),
    };
    let mut value: u64 = 0;
    let mut any = false;
    for character in digits.chars() {
        let Some(digit) = character.to_digit(base) else {
            break;
        };
        value = value.saturating_mul(u64::from(base)).saturating_add(u64::from(digit));
        any = true;
    }
    Some((negative, if any { value } else { 0 }))
}

fn written_in(value: u64, base: u32) -> String {
    if value == 0 {
        return "0".to_owned();
    }
    let mut digits = Vec::new();
    let mut rest = value;
    while rest > 0 {
        let digit = u32::try_from(rest % u64::from(base)).unwrap_or(0);
        digits.push(std::char::from_digit(digit, base).unwrap_or('0').to_ascii_uppercase());
        rest /= u64::from(base);
    }
    digits.iter().rev().collect()
}

/// `CONV(n, from_base, to_base)`: `n` read in `from_base` and written in `to_base` (2 to 36;
/// a negative `to_base` writes a negative number with its sign, else as its unsigned 64-bit
/// form, as MySQL does); NULL for a base out of range.
fn conv(text: &str, from: i64, to: i64) -> Option<String> {
    let (from_base, to_base) = (u32::try_from(from.unsigned_abs()).ok()?, u32::try_from(to.unsigned_abs()).ok()?);
    if !(2..=36).contains(&from_base) || !(2..=36).contains(&to_base) {
        return None;
    }
    let (negative, magnitude) = digits_in(text, from_base)?;
    Some(match (negative, to < 0) {
        (true, true) => format!("-{}", written_in(magnitude, to_base)),
        (true, false) => written_in(magnitude.wrapping_neg(), to_base),
        (false, _) => written_in(magnitude, to_base),
    })
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    let digits: Vec<u32> = text.chars().map(|character| character.to_digit(16)).collect::<Option<_>>()?;
    let padded: Vec<u32> = if digits.len() % 2 == 1 { std::iter::once(0).chain(digits).collect() } else { digits };
    Some(padded.chunks(2).map(|pair| u8::try_from(pair[0] * 16 + pair[1]).unwrap_or(0)).collect())
}

fn ord(text: &str) -> i64 {
    let Some(first) = text.chars().next() else {
        return 0;
    };
    let mut buffer = [0u8; 4];
    first.encode_utf8(&mut buffer).bytes().fold(0i64, |code, byte| code * 256 + i64::from(byte))
}

fn seeded_rand(seed: i64) -> f64 {
    let mut state = (seed as u64).wrapping_add(0x9E37_79B9_7F4A_7C15);
    state = (state ^ (state >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    state = (state ^ (state >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    state ^= state >> 31;
    (state >> 11) as f64 / (1u64 << 53) as f64
}

/// Runs one of [`FUNCTIONS`] in MySQL's dialect; `None` in other dialects or for other names.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !FUNCTIONS.contains(&name) || crate::constraints::current_dialect() != Dialect::MySql {
        return None;
    }
    let integer = |at: usize| values.get(at).and_then(Value::as_integer);
    let string = |at: usize| values.get(at).and_then(text);
    Some(Ok(match (name, values.len()) {
        ("rand", 0) => {
            let mut bytes = [0u8; 8];
            if let Err(error) = crate::random::fill(&mut bytes) {
                return Some(Err(error));
            }
            Value::Double((u64::from_be_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64)
        }
        ("rand", 1) => integer(0).map_or(Value::Null, |seed| Value::Double(seeded_rand(seed))),
        ("elt", count) if count >= 1 => match integer(0) {
            Some(at) if at >= 1 => string(usize::try_from(at).unwrap_or(usize::MAX)).map_or(Value::Null, Value::Text),
            _ => Value::Null,
        },
        _ if values.iter().any(Value::is_null) => Value::Null,
        ("locate", 2 | 3) => match (string(0), string(1)) {
            (Some(needle), Some(haystack)) => Value::BigInt(locate(&needle, &haystack, integer(2).unwrap_or(1))),
            _ => Value::Null,
        },
        ("substring_index", 3) => match (string(0), string(1), integer(2)) {
            (Some(text), Some(delimiter), Some(count)) => Value::Text(substring_index(&text, &delimiter, count)),
            _ => Value::Null,
        },
        ("find_in_set", 2) => match (string(0), string(1)) {
            (Some(wanted), _) if wanted.contains(',') => Value::BigInt(0),
            (Some(wanted), Some(list)) if !list.is_empty() => Value::BigInt(
                list.split(',').position(|item| item.eq_ignore_ascii_case(&wanted)).map_or(0, |at| i64::try_from(at + 1).unwrap_or(0)),
            ),
            (Some(_), Some(_)) => Value::BigInt(0),
            _ => Value::Null,
        },
        ("mid", 2 | 3) => match string(0) {
            Some(text) => {
                let count = text.chars().count() as i64;
                let start = integer(1).unwrap_or(0);
                let from = if start > 0 { start - 1 } else if start < 0 { count + start } else { return Some(Ok(Value::Text(String::new()))) };
                let length = integer(2).unwrap_or(i64::MAX).max(0);
                if from < 0 {
                    Value::Text(String::new())
                } else {
                    Value::Text(text.chars().skip(usize::try_from(from).unwrap_or(0)).take(usize::try_from(length).unwrap_or(usize::MAX)).collect())
                }
            }
            None => Value::Null,
        },
        ("strcmp", 2) => match (string(0), string(1)) {
            (Some(left), Some(right)) => Value::BigInt(match left.to_lowercase().cmp(&right.to_lowercase()) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }),
            _ => Value::Null,
        },
        ("ord", 1) => string(0).map_or(Value::Null, |text| Value::BigInt(ord(&text))),
        ("bin", 1) => string(0).and_then(|text| conv(&text, 10, 2)).map_or(Value::Null, Value::Text),
        ("conv", 3) => match (string(0), integer(1), integer(2)) {
            (Some(text), Some(from), Some(to)) => conv(&text, from, to).map_or(Value::Null, Value::Text),
            _ => Value::Null,
        },
        ("crc32", 1) => string(0).map_or(Value::Null, |text| Value::BigInt(i64::from(crc32(text.as_bytes())))),
        ("unhex", 1) => string(0).and_then(|text| unhex(&text)).map_or(Value::Null, Value::Bytea),
        _ => return None,
    }))
}

/// The result type of one of [`FUNCTIONS`].
pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    Some(match name {
        "locate" | "find_in_set" | "strcmp" | "ord" | "crc32" => SqlType::BigInt,
        "unhex" => SqlType::Bytea,
        "rand" => SqlType::Double,
        _ => SqlType::Text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mysql_string_helpers_follow_mysql() {
        assert_eq!(locate("bar", "foobarbar", 1), 4);
        assert_eq!(locate("bar", "foobarbar", 5), 7);
        assert_eq!(locate("BAR", "foobar", 1), 4);
        assert_eq!(locate("x", "foobar", 1), 0);
        assert_eq!(substring_index("www.mysql.com", ".", 2), "www.mysql");
        assert_eq!(substring_index("www.mysql.com", ".", -2), "mysql.com");
        assert_eq!(conv("a", 16, 2).as_deref(), Some("1010"));
        assert_eq!(conv("6E", 18, 8).as_deref(), Some("172"));
        assert_eq!(conv("-17", 10, -18).as_deref(), Some("-H"));
        assert_eq!(crc32(b"MySQL"), 3_259_397_556);
        assert_eq!(unhex("4D7953514C").as_deref(), Some(&b"MySQL"[..]));
        assert_eq!(unhex("GG"), None);
        assert_eq!(ord("2"), 50);
        assert!((0.0..1.0).contains(&seeded_rand(3)));
        assert_eq!(seeded_rand(3), seeded_rand(3));
    }
}
