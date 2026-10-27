use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{SqlType, Value, integer_value, without_trailing_spaces};

pub(crate) const MAX_BYTES: usize = 1_000_000_000;
const BASE64_LINE: usize = 76;
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn too_large() -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("a bytea value cannot exceed {MAX_BYTES} bytes"),
    )
}

pub(crate) fn check_size(length: usize) -> SqlResult<()> {
    if length > MAX_BYTES {
        Err(too_large())
    } else {
        Ok(())
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn invalid_hex_digit(byte: u8) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("invalid hexadecimal digit: \"{}\"", char::from(byte)),
    )
}

fn odd_hex() -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        "invalid hexadecimal data: odd number of digits",
    )
}

fn hex_decode(text: &[u8]) -> SqlResult<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 2);
    let mut at = 0;
    while at < text.len() {
        let byte = text[at];
        if byte.is_ascii_whitespace() {
            at += 1;
            continue;
        }
        let high = hex_value(byte).ok_or_else(|| invalid_hex_digit(byte))?;
        let low_byte = *text.get(at + 1).ok_or_else(odd_hex)?;
        let low = hex_value(low_byte).ok_or_else(|| {
            if low_byte.is_ascii_whitespace() {
                odd_hex()
            } else {
                invalid_hex_digit(low_byte)
            }
        })?;
        out.push(high << 4 | low);
        at += 2;
    }
    Ok(out)
}

fn escape_decode(text: &[u8], type_name: &str) -> SqlResult<Vec<u8>> {
    let invalid = || {
        SqlError::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!("invalid input syntax for type {type_name}"),
        )
    };
    let mut out = Vec::with_capacity(text.len());
    let mut at = 0;
    while at < text.len() {
        let byte = text[at];
        if byte != b'\\' {
            out.push(byte);
            at += 1;
            continue;
        }
        match text.get(at + 1..at + 4) {
            Some(
                [
                    first @ b'0'..=b'3',
                    second @ b'0'..=b'7',
                    third @ b'0'..=b'7',
                ],
            ) => {
                out.push((first - b'0') << 6 | (second - b'0') << 3 | (third - b'0'));
                at += 4;
            }
            _ if text.get(at + 1) == Some(&b'\\') => {
                out.push(b'\\');
                at += 2;
            }
            _ => return Err(invalid()),
        }
    }
    Ok(out)
}

pub(crate) fn parse(text: &str) -> SqlResult<Vec<u8>> {
    let bytes = match text.as_bytes() {
        [b'\\', b'x' | b'X', rest @ ..] => hex_decode(rest)?,
        bytes => escape_decode(bytes, "bytea")?,
    };
    check_size(bytes.len())?;
    Ok(bytes)
}

pub(crate) fn format_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("\\x");
    for &byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0F)]));
    }
    out
}

fn unknown_format(name: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("unrecognized encoding: \"{name}\""),
    )
}

pub(crate) fn encode(bytes: &[u8], format: &str) -> SqlResult<String> {
    match format.to_ascii_lowercase().as_str() {
        "hex" => {
            check_size(bytes.len().saturating_mul(2))?;
            Ok(format_hex(bytes).split_off(2))
        }
        "base64" => {
            let length = bytes.len().div_ceil(3).saturating_mul(4);
            check_size(length.saturating_add(length / BASE64_LINE))?;
            Ok(base64_encode(bytes))
        }
        "escape" => {
            check_size(bytes.len().saturating_mul(4))?;
            let mut out = String::with_capacity(bytes.len());
            for &byte in bytes {
                match byte {
                    0 | 0x80..=0xFF => out.push_str(&format!("\\{byte:03o}")),
                    b'\\' => out.push_str("\\\\"),
                    _ => out.push(char::from(byte)),
                }
            }
            Ok(out)
        }
        _ => Err(unknown_format(format)),
    }
}

pub(crate) fn decode(text: &str, format: &str) -> SqlResult<Vec<u8>> {
    match format.to_ascii_lowercase().as_str() {
        "hex" => hex_decode(text.as_bytes()),
        "base64" => base64_decode(text.as_bytes()),
        "escape" => escape_decode(text.as_bytes(), "bytea"),
        _ => Err(unknown_format(format)),
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut line = 0;
    for chunk in bytes.chunks(3) {
        if line == BASE64_LINE {
            out.push('\n');
            line = 0;
        }
        let word = u32::from(chunk[0]) << 16
            | u32::from(chunk.get(1).copied().unwrap_or(0)) << 8
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        for index in 0..4 {
            if index <= chunk.len() {
                let sextet = (word >> (18 - 6 * index)) & 0x3F;
                out.push(char::from(BASE64_ALPHABET[sextet as usize]));
            } else {
                out.push('=');
            }
        }
        line += 4;
    }
    out
}

fn base64_decode(text: &[u8]) -> SqlResult<Vec<u8>> {
    let invalid_symbol = |byte: u8| {
        SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!(
                "invalid symbol \"{}\" found while decoding base64 sequence",
                char::from(byte)
            ),
        )
    };
    let bad_end = || {
        SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "invalid base64 end sequence",
        )
        .with_hint("Input data is missing padding, is truncated, or is otherwise corrupted.")
    };
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut word = 0u32;
    let mut count = 0;
    let mut padding = 0;
    for &byte in text {
        if byte.is_ascii_whitespace() {
            continue;
        }
        let value = if byte == b'=' {
            padding += 1;
            0
        } else {
            if padding > 0 {
                return Err(invalid_symbol(byte));
            }
            BASE64_ALPHABET
                .iter()
                .position(|&symbol| symbol == byte)
                .ok_or_else(|| invalid_symbol(byte))? as u32
        };
        word = word << 6 | value;
        count += 1;
        if count == 4 {
            if padding > 2 {
                return Err(bad_end());
            }
            let bytes = word.to_be_bytes();
            out.extend_from_slice(&bytes[1..4 - padding]);
            word = 0;
            count = 0;
        }
    }
    if count != 0 {
        return Err(bad_end());
    }
    Ok(out)
}

pub(crate) fn substring(
    length: usize,
    start: i64,
    count: Option<i64>,
) -> SqlResult<(usize, usize)> {
    let end = match count {
        Some(count) if count < 0 => {
            return Err(SqlError::new(
                SqlState::SUBSTRING_ERROR,
                "negative substring length not allowed",
            ));
        }
        Some(count) => start.saturating_add(count),
        None => i64::MAX,
    };
    let first = start.max(1);
    if end <= first {
        return Ok((0, 0));
    }
    let clamp =
        |position: i64| usize::try_from(position - 1).map_or(length, |index| index.min(length));
    Ok((clamp(first), clamp(end)))
}

pub(crate) fn parse_uuid(text: &str) -> SqlResult<[u8; 16]> {
    let invalid = || {
        SqlError::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!("invalid input syntax for type uuid: \"{text}\""),
        )
    };
    let mut bytes = text.as_bytes();
    let braced = bytes.first() == Some(&b'{');
    if braced {
        bytes = &bytes[1..];
    }
    let mut out = [0u8; 16];
    for (index, slot) in out.iter_mut().enumerate() {
        let (Some(&high), Some(&low)) = (bytes.first(), bytes.get(1)) else {
            return Err(invalid());
        };
        let (Some(high), Some(low)) = (hex_value(high), hex_value(low)) else {
            return Err(invalid());
        };
        *slot = high << 4 | low;
        bytes = &bytes[2..];
        if bytes.first() == Some(&b'-') && index % 2 == 1 && index < 15 {
            bytes = &bytes[1..];
        }
    }
    if braced {
        match bytes.split_first() {
            Some((b'}', rest)) => bytes = rest,
            _ => return Err(invalid()),
        }
    }
    if bytes.is_empty() {
        Ok(out)
    } else {
        Err(invalid())
    }
}

pub(crate) fn format_uuid(bytes: &[u8; 16]) -> String {
    let hex = format_hex(bytes);
    let digits = &hex[2..];
    format!(
        "{}-{}-{}-{}-{}",
        &digits[..8],
        &digits[8..12],
        &digits[12..16],
        &digits[16..20],
        &digits[20..]
    )
}

pub(crate) fn random_uuid(mut bytes: [u8; 16]) -> [u8; 16] {
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    bytes
}

fn character(value: &Value) -> Option<&str> {
    match value {
        Value::Text(text) => Some(text.as_str()),
        Value::Bpchar(text) => Some(without_trailing_spaces(text)),
        _ => None,
    }
}

fn count_argument(value: &Value) -> Option<i64> {
    value.as_integer()
}

fn sliced_text(text: &str, start: i64, count: Option<i64>) -> SqlResult<Value> {
    let (first, end) = substring(text.chars().count(), start, count)?;
    Ok(Value::Text(
        text.chars().skip(first).take(end - first).collect(),
    ))
}

fn sliced_bytes(bytes: &[u8], start: i64, count: Option<i64>) -> SqlResult<Value> {
    let (first, end) = substring(bytes.len(), start, count)?;
    Ok(Value::Bytea(bytes[first..end].to_vec()))
}

pub(crate) fn argument_types(name: &str) -> Option<&'static [Option<SqlType>]> {
    Some(match name {
        "encode" => &[Some(SqlType::Bytea), Some(SqlType::Text)],
        "decode" => &[Some(SqlType::Text), Some(SqlType::Text)],
        "get_byte" => &[Some(SqlType::Bytea), Some(SqlType::Integer)],
        "substring" => &[None, Some(SqlType::Integer), Some(SqlType::Integer)],
        _ => return None,
    })
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    Some(match (name, values) {
        ("length" | "char_length" | "character_length", [Value::Bpchar(text)]) => integer_value(
            without_trailing_spaces(text).chars().count() as i64,
            SqlType::Integer,
        ),
        ("octet_length", [Value::Bpchar(text)]) => {
            integer_value(text.len() as i64, SqlType::Integer)
        }
        ("length" | "octet_length", [Value::Bytea(bytes)]) => {
            integer_value(bytes.len() as i64, SqlType::Integer)
        }
        ("lower", [Value::Bpchar(text)]) => {
            Ok(Value::Text(without_trailing_spaces(text).to_lowercase()))
        }
        ("upper", [Value::Bpchar(text)]) => {
            Ok(Value::Text(without_trailing_spaces(text).to_uppercase()))
        }
        ("encode" | "decode" | "get_byte", [Value::Null, _] | [_, Value::Null]) => Ok(Value::Null),
        ("encode", [Value::Bytea(bytes), format]) => {
            encode(bytes, character(format)?).map(Value::Text)
        }
        ("decode", [text, format]) => {
            decode(character(text)?, character(format)?).map(Value::Bytea)
        }
        ("get_byte", [Value::Bytea(bytes), index]) => {
            let index = count_argument(index)?;
            match usize::try_from(index).ok().and_then(|at| bytes.get(at)) {
                Some(&byte) => Ok(Value::Integer(i32::from(byte))),
                None => Err(SqlError::new(
                    SqlState::ARRAY_SUBSCRIPT_ERROR,
                    format!(
                        "index {index} out of valid range, 0..{}",
                        bytes.len() as i64 - 1
                    ),
                )),
            }
        }
        ("substring", [Value::Null, ..] | [_, Value::Null, ..] | [_, _, Value::Null]) => {
            Ok(Value::Null)
        }
        ("substring", [value, start, rest @ ..]) if rest.len() <= 1 => {
            let start = count_argument(start)?;
            let count = match rest.first() {
                Some(count) => Some(count_argument(count)?),
                None => None,
            };
            match value {
                Value::Bytea(bytes) => sliced_bytes(bytes, start, count),
                other => sliced_text(character(other)?, start, count),
            }
        }
        ("gen_random_uuid", []) => {
            let mut bytes = [0u8; 16];
            crate::random::fill(&mut bytes).map(|()| Value::Uuid(random_uuid(bytes)))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_and_escape_input_read_as_postgresql_reads_them() {
        assert_eq!(parse("\\x0aFf").unwrap(), vec![0x0A, 0xFF]);
        assert_eq!(parse("\\x 01 02").unwrap(), vec![1, 2]);
        assert_eq!(parse("\\x").unwrap(), Vec::<u8>::new());
        assert_eq!(
            parse("abc\\\\d\\001\\377").unwrap(),
            b"abc\\d\x01\xff".to_vec()
        );
        assert_eq!(
            parse("\\x0").unwrap_err().message(),
            "invalid hexadecimal data: odd number of digits"
        );
        assert_eq!(
            parse("\\x0g").unwrap_err().message(),
            "invalid hexadecimal digit: \"g\""
        );
        assert_eq!(
            parse("\\x 0 1").unwrap_err().message(),
            "invalid hexadecimal data: odd number of digits"
        );
        assert_eq!(parse("a\\b").unwrap_err().state().code(), "22P02");
        assert_eq!(parse("a\\").unwrap_err().state().code(), "22P02");
        assert_eq!(parse("\\400").unwrap_err().state().code(), "22P02");
        assert_eq!(format_hex(&[0, 0xAB, 0x10]), "\\x00ab10");
    }

    #[test]
    fn encode_and_decode_round_trip_in_every_format() {
        let mut state = 0xB17E_5EED_0000_0031u64;
        for length in 0..300 {
            let bytes: Vec<u8> = (0..length)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect();
            for format in ["hex", "base64", "escape", "HEX"] {
                let text = encode(&bytes, format).unwrap();
                assert_eq!(decode(&text, format).unwrap(), bytes, "{format} {text}");
            }
        }
        assert_eq!(encode(b"hello", "base64").unwrap(), "aGVsbG8=");
        assert_eq!(encode(b"\x00\\a\xff", "escape").unwrap(), "\\000\\\\a\\377");
        assert_eq!(
            encode(&[0u8; 60], "base64").unwrap().matches('\n').count(),
            1
        );
        assert!(decode("aGVsbG8", "base64").is_err());
        assert!(decode("aG=sbG8=", "base64").is_err());
        assert!(decode("====", "base64").is_err());
        assert!(decode("a!", "base64").is_err());
        assert!(decode("abc", "hex").is_err());
        assert!(encode(b"x", "rot13").is_err());
    }

    #[test]
    fn substring_follows_sql_positions() {
        assert_eq!(substring(5, 2, Some(2)).unwrap(), (1, 3));
        assert_eq!(substring(5, 0, Some(2)).unwrap(), (0, 1));
        assert_eq!(substring(5, -3, Some(2)).unwrap(), (0, 0));
        assert_eq!(substring(5, 4, None).unwrap(), (3, 5));
        assert_eq!(substring(5, 9, None).unwrap(), (5, 5));
        assert_eq!(substring(5, i64::MAX, Some(i64::MAX)).unwrap(), (0, 0));
        assert_eq!(substring(5, i64::MIN, Some(i64::MAX)).unwrap(), (0, 0));
        assert!(substring(5, 1, Some(-1)).is_err());
    }

    #[test]
    fn uuids_read_every_accepted_form_and_print_lower_case() {
        let canonical = "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11";
        for text in [
            "A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11",
            "{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}",
            "a0eebc999c0b4ef8bb6d6bb9bd380a11",
            "a0ee-bc99-9c0b-4ef8-bb6d-6bb9-bd38-0a11",
            "{a0eebc99-9c0b4ef8-bb6d6bb9-bd380a11}",
        ] {
            assert_eq!(format_uuid(&parse_uuid(text).unwrap()), canonical, "{text}");
        }
        for text in [
            "",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a1",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a111",
            "{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}",
            "a0eebc9-99c0b-4ef8-bb6d-6bb9bd380a11",
            "a0eebc99--9c0b-4ef8-bb6d-6bb9bd380a11",
            " a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            "g0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11-",
            "é0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
        ] {
            assert_eq!(
                parse_uuid(text).unwrap_err().state().code(),
                "22P02",
                "{text}"
            );
        }
        let random = random_uuid([0xFF; 16]);
        assert_eq!(format_uuid(&random), "ffffffff-ffff-4fff-bfff-ffffffffffff");
    }
}
