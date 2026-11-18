//! Scalar functions one dialect has under its own name (D1292, D1295, D1296, D1299): SQL
//! Server's `PATINDEX`, `QUOTENAME`, `ISNUMERIC`, the `...FROMPARTS` functions, `HASHBYTES`,
//! `STRING_ESCAPE` and `APP_NAME`; Oracle's `BITAND`, `REMAINDER`, `NANVL`, `SYS_GUID`, hashes,
//! LOB constructors and Unicode escapes; and PostgreSQL's `unistr`. Each is answered in its own
//! dialect only.

use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{SqlType, Value};

const SQLSERVER_FUNCTIONS: [&str; 12] = [
    "formatmessage",
    "hashbytes",
    "string_escape",
    "app_name",
    "patindex",
    "quotename",
    "isnumeric",
    "datefromparts",
    "datetimefromparts",
    "datetime2fromparts",
    "smalldatetimefromparts",
    "timefromparts",
];
const ORACLE_FUNCTIONS: [&str; 19] = [
    "to_multi_byte",
    "to_single_byte",
    "bitand",
    "remainder",
    "nanvl",
    "sys_guid",
    "standard_hash",
    "ora_hash",
    "nls_upper",
    "nls_lower",
    "nls_initcap",
    "to_clob",
    "to_blob",
    "empty_clob",
    "empty_blob",
    "vsize",
    "lnnvl",
    "asciistr",
    "unistr",
];
const ORA_HASH_MOST: i64 = 4_294_967_295;
const MOST_QUOTED_CHARACTERS: usize = 128;

fn dialect_has(name: &str) -> bool {
    match crate::constraints::current_dialect() {
        Dialect::SqlServer => SQLSERVER_FUNCTIONS.contains(&name),
        Dialect::Oracle => ORACLE_FUNCTIONS.contains(&name),
        Dialect::PostgreSql => name == "unistr",
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq)]
enum PatternPart {
    Any,
    One,
    Class { negated: bool, members: Vec<(char, char)> },
    Literal(char),
}

fn pattern_parts(pattern: &str) -> Vec<PatternPart> {
    let characters: Vec<char> = pattern.chars().flat_map(char::to_lowercase).collect();
    let mut parts = Vec::new();
    let mut at = 0;
    while at < characters.len() {
        match characters[at] {
            '%' => parts.push(PatternPart::Any),
            '_' => parts.push(PatternPart::One),
            '[' => match characters[at + 1..].iter().position(|character| *character == ']') {
                Some(length) if length > 0 => {
                    let inside = &characters[at + 1..at + 1 + length];
                    let (negated, inside) = match inside.split_first() {
                        Some(('^', rest)) => (true, rest),
                        _ => (false, inside),
                    };
                    let mut members = Vec::new();
                    let mut index = 0;
                    while index < inside.len() {
                        if index + 2 < inside.len() && inside[index + 1] == '-' {
                            members.push((inside[index], inside[index + 2]));
                            index += 3;
                        } else {
                            members.push((inside[index], inside[index]));
                            index += 1;
                        }
                    }
                    parts.push(PatternPart::Class { negated, members });
                    at += length + 1;
                }
                _ => parts.push(PatternPart::Literal('[')),
            },
            other => parts.push(PatternPart::Literal(other)),
        }
        at += 1;
    }
    parts
}

fn part_matches(part: &PatternPart, character: char) -> bool {
    match part {
        PatternPart::Any | PatternPart::One => true,
        PatternPart::Literal(literal) => *literal == character,
        PatternPart::Class { negated, members } => {
            members.iter().any(|(low, high)| (*low..=*high).contains(&character)) != *negated
        }
    }
}

fn like(parts: &[PatternPart], text: &[char]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut resume: Option<(usize, usize)> = None;
    while t < text.len() {
        match parts.get(p) {
            Some(PatternPart::Any) => {
                resume = Some((p, t));
                p += 1;
            }
            Some(part) if part_matches(part, text[t]) => {
                p += 1;
                t += 1;
            }
            _ => match resume {
                Some((star, from)) => {
                    p = star + 1;
                    t = from + 1;
                    resume = Some((star, from + 1));
                }
                None => return false,
            },
        }
    }
    parts[p.min(parts.len())..].iter().all(|part| *part == PatternPart::Any)
}

fn pattern_index(pattern: &str, text: &str) -> i64 {
    let parts = pattern_parts(pattern);
    let characters: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    match parts.split_first() {
        Some((PatternPart::Any, rest)) => (0..=characters.len())
            .find(|start| like(rest, &characters[*start..]))
            .map_or(0, |start| i64::try_from(start + 1).unwrap_or(0)),
        _ if like(&parts, &characters) => 1,
        _ => 0,
    }
}

fn quote_name(name: &str, quote: char) -> Option<String> {
    if name.chars().count() > MOST_QUOTED_CHARACTERS {
        return None;
    }
    let (open, close) = match quote {
        '[' | ']' => ('[', ']'),
        '"' => ('"', '"'),
        '\'' => ('\'', '\''),
        '(' | ')' => ('(', ')'),
        '<' | '>' => ('<', '>'),
        '{' | '}' => ('{', '}'),
        '`' => ('`', '`'),
        _ => return None,
    };
    let doubled = name.replace(close, &format!("{close}{close}"));
    Some(format!("{open}{doubled}{close}"))
}

fn looks_numeric(text: &str) -> bool {
    let trimmed = text.trim();
    let unsigned = trimmed.trim_start_matches(['+', '-']).trim_start_matches(['$', '€', '£', '¥']).trim_start_matches(['+', '-']);
    if unsigned.is_empty() {
        return !trimmed.is_empty() && trimmed.chars().all(|character| "+-$.".contains(character)) && trimmed.chars().count() == 1;
    }
    if unsigned == "." {
        return true;
    }
    let without_groups: String = unsigned.chars().filter(|character| *character != ',').collect();
    without_groups.parse::<f64>().is_ok_and(f64::is_finite) && !without_groups.contains(['i', 'I', 'n', 'N'])
}

fn parts_error(type_name: &str) -> SqlError {
    SqlError::new(
        SqlState::DATETIME_FIELD_OVERFLOW,
        format!("Cannot construct data type {type_name}, some of the arguments have values which are not valid."),
    )
}

fn timestamp_from_parts(values: &[Value], seconds: f64) -> SqlResult<Value> {
    let mut parts: Vec<Value> = values.iter().take(5).cloned().collect();
    parts.push(Value::Double(seconds));
    match crate::temporal::made_local(&parts) {
        Some(Ok(micros)) => Ok(Value::Timestamp(micros)),
        _ => Err(parts_error("datetime")),
    }
}

fn hashed_bytes(value: &Value) -> Vec<u8> {
    match value {
        Value::Bytea(bytes) => bytes.clone(),
        other => other.to_text().unwrap_or_default().into_bytes(),
    }
}

fn ascii_escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\005C"),
            ' '..='~' => out.push(character),
            other => {
                let mut units = [0u16; 2];
                for unit in other.encode_utf16(&mut units) {
                    out.push_str(&format!("\\{unit:04X}"));
                }
            }
        }
    }
    out
}

fn unicode_unescaped(text: &str) -> SqlResult<String> {
    let invalid = || SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "invalid Unicode escape value").with_oracle_code(30186);
    let mut units: Vec<u16> = Vec::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\\' {
            let mut buffer = [0u16; 2];
            units.extend_from_slice(character.encode_utf16(&mut buffer));
            continue;
        }
        if characters.peek() == Some(&'\\') {
            characters.next();
            units.push(u16::from(b'\\'));
            continue;
        }
        let width = match characters.peek() {
            Some('+') => 6,
            Some('u') => 4,
            Some('U') => 8,
            _ => 4,
        };
        if width != 4 || characters.peek() == Some(&'u') {
            characters.next();
        }
        let digits: String = characters.by_ref().take(width).collect();
        if digits.chars().count() != width {
            return Err(invalid());
        }
        if width == 4 {
            units.push(u16::from_str_radix(&digits, 16).map_err(|_| invalid())?);
        } else {
            let point = u32::from_str_radix(&digits, 16).map_err(|_| invalid())?;
            let character = char::from_u32(point).ok_or_else(invalid)?;
            let mut buffer = [0u16; 2];
            units.extend_from_slice(character.encode_utf16(&mut buffer));
        }
    }
    String::from_utf16(&units).map_err(|_| invalid())
}

const FULL_WIDTH_OFFSET: u32 = 0xFEE0;

fn full_width(character: char) -> char {
    match character {
        ' ' => '\u{3000}',
        '!'..='~' => char::from_u32(u32::from(character) + FULL_WIDTH_OFFSET).unwrap_or(character),
        other => other,
    }
}

fn half_width(character: char) -> char {
    match character {
        '\u{3000}' => ' ',
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(u32::from(character) - FULL_WIDTH_OFFSET).unwrap_or(character),
        other => other,
    }
}

fn formatted_message(format: &str, arguments: &[Value]) -> String {
    let mut out = String::with_capacity(format.len());
    let mut characters = format.chars().peekable();
    let mut next = arguments.iter();
    while let Some(character) = characters.next() {
        if character != '%' {
            out.push(character);
            continue;
        }
        let mut flags = String::new();
        while let Some(&flag) = characters.peek() {
            if flag.is_ascii_digit() || matches!(flag, '-' | '+' | ' ' | '#' | '.') {
                flags.push(flag);
                characters.next();
            } else {
                break;
            }
        }
        let Some(kind) = characters.next() else {
            out.push('%');
            break;
        };
        let mut argument = || next.next().filter(|value| !value.is_null());
        match kind {
            '%' => out.push('%'),
            's' | 'S' => out.push_str(&argument().map_or_else(|| "(null)".to_owned(), text_of)),
            'd' | 'i' => out.push_str(&argument().and_then(Value::as_integer).map_or_else(|| "(null)".to_owned(), |number| number.to_string())),
            'u' => out.push_str(&argument().and_then(Value::as_integer).map_or_else(|| "(null)".to_owned(), |number| (number as u64).to_string())),
            'x' => out.push_str(&argument().and_then(Value::as_integer).map_or_else(|| "(null)".to_owned(), |number| format!("{number:x}"))),
            'X' => out.push_str(&argument().and_then(Value::as_integer).map_or_else(|| "(null)".to_owned(), |number| format!("{number:X}"))),
            'o' => out.push_str(&argument().and_then(Value::as_integer).map_or_else(|| "(null)".to_owned(), |number| format!("{number:o}"))),
            other => {
                out.push('%');
                out.push_str(&flags);
                out.push(other);
            }
        }
    }
    out
}

fn text_of(value: &Value) -> String {
    value.to_text().unwrap_or_default()
}

fn json_escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '/' => out.push_str("\\/"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if u32::from(control) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(control))),
            other => out.push(other),
        }
    }
    out
}

fn rounded_away(value: f64) -> f64 {
    value.round()
}

/// Runs one of these functions in its dialect; `None` elsewhere or for other names.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !dialect_has(name) {
        return None;
    }
    if !matches!(name, "nanvl" | "lnnvl" | "formatmessage") && values.iter().any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    let text = |at: usize| values.get(at).and_then(Value::to_text);
    let integer = |at: usize| values.get(at).and_then(Value::as_integer);
    Some(match (name, values) {
        ("patindex", [_, _]) => Ok(Value::BigInt(pattern_index(&text(0)?, &text(1)?))),
        ("quotename", [_]) => Ok(quote_name(&text(0)?, '[').map_or(Value::Null, Value::Text)),
        ("quotename", [_, _]) => Ok(text(1)
            .and_then(|quote| quote.chars().next())
            .and_then(|quote| quote_name(&text(0)?, quote))
            .map_or(Value::Null, Value::Text)),
        ("isnumeric", [value]) => Ok(Value::Integer(i32::from(match value {
            Value::Text(text) | Value::Bpchar(text) => looks_numeric(text),
            other => other.as_float().is_some(),
        }))),
        ("datefromparts", [_, _, _]) => crate::temporal::made_date(integer(0)?, integer(1)?, integer(2)?)
            .and_then(crate::datetime::checked_date)
            .map(Value::Date)
            .map_err(|_| SqlError::new(SqlState::DATETIME_FIELD_OVERFLOW, "Cannot construct data type date, some of the arguments have values which are not valid.")),
        ("datetimefromparts", [_, _, _, _, _, _, _]) => {
            let seconds = integer(5)? as f64 + integer(6)? as f64 / 1_000.0;
            timestamp_from_parts(values, seconds)
        }
        ("datetime2fromparts", [_, _, _, _, _, _, _, _]) => {
            let precision = integer(7)?;
            if !(0..=7).contains(&precision) {
                return Some(Err(parts_error("datetime2")));
            }
            let seconds = integer(5)? as f64 + integer(6)? as f64 / 10f64.powi(i32::try_from(precision).unwrap_or(0));
            timestamp_from_parts(values, seconds)
        }
        ("smalldatetimefromparts", [_, _, _, _, _]) => timestamp_from_parts(values, 0.0),
        ("timefromparts", [_, _, _, _, _]) => {
            let precision = integer(4)?;
            if !(0..=7).contains(&precision) {
                return Some(Err(parts_error("time")));
            }
            let seconds = integer(2)? as f64 + integer(3)? as f64 / 10f64.powi(i32::try_from(precision).unwrap_or(0));
            crate::temporal::made_time(integer(0)?, integer(1)?, seconds).map(Value::Time).map_err(|_| parts_error("time"))
        }
        ("hashbytes", [algorithm, input]) => {
            let bytes = match input {
                Value::Bytea(bytes) => bytes.clone(),
                other => other.to_text().unwrap_or_default().into_bytes(),
            };
            Ok(match text_of(algorithm).to_ascii_uppercase().as_str() {
                "MD5" => Value::Bytea(eggdb_crypto::md5_checksum(&bytes).to_vec()),
                "SHA" | "SHA1" => Value::Bytea(eggdb_crypto::sha1(&bytes).to_vec()),
                "SHA2_256" => Value::Bytea(eggdb_crypto::sha256(&bytes).to_vec()),
                "SHA2_512" => Value::Bytea(eggdb_crypto::sha512(&bytes).to_vec()),
                _ => Value::Null,
            })
        }
        ("string_escape", [value, kind]) => {
            if !text_of(kind).eq_ignore_ascii_case("json") {
                return Some(Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "An invalid value was specified for argument 2: STRING_ESCAPE supports the type 'json'.",
                )));
            }
            Ok(Value::Text(json_escaped(&text_of(value))))
        }
        ("app_name", []) => Ok(Value::Text(String::new())),
        ("formatmessage", [format, arguments @ ..]) => Ok(match format {
            Value::Null => Value::Null,
            format => Value::Text(formatted_message(&text_of(format), arguments)),
        }),
        ("sys_guid", []) => {
            let mut bytes = [0u8; 16];
            crate::random::fill(&mut bytes).map(|()| Value::Bytea(bytes.to_vec()))
        }
        ("standard_hash", [input, rest @ ..]) => {
            let bytes = hashed_bytes(input);
            let algorithm = rest.first().map_or_else(|| "SHA1".to_owned(), text_of).to_ascii_uppercase();
            match algorithm.as_str() {
                "SHA1" => Ok(Value::Bytea(eggdb_crypto::sha1(&bytes).to_vec())),
                "SHA256" => Ok(Value::Bytea(eggdb_crypto::sha256(&bytes).to_vec())),
                "SHA384" => Ok(Value::Bytea(eggdb_crypto::sha384(&bytes).to_vec())),
                "SHA512" => Ok(Value::Bytea(eggdb_crypto::sha512(&bytes).to_vec())),
                "MD5" => Ok(Value::Bytea(eggdb_crypto::md5_checksum(&bytes).to_vec())),
                _ => Err(SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "invalid algorithm name").with_oracle_code(932)),
            }
        }
        ("ora_hash", [input, rest @ ..]) => {
            let most = rest.first().and_then(Value::as_integer).unwrap_or(ORA_HASH_MOST);
            let seed = rest.get(1).and_then(Value::as_integer).unwrap_or(0);
            if !(0..=ORA_HASH_MOST).contains(&most) || !(0..=ORA_HASH_MOST).contains(&seed) {
                return Some(Err(SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "argument is out of range").with_oracle_code(1428)));
            }
            let digest = eggdb_crypto::sha256(&[seed.to_be_bytes().as_slice(), &hashed_bytes(input)].concat());
            let word = u64::from_be_bytes(digest[..8].try_into().unwrap_or([0; 8]));
            Ok(Value::BigInt(i64::try_from(word % (most as u64 + 1)).unwrap_or(0)))
        }
        ("nls_upper" | "nls_lower", [text, ..]) => crate::string_functions::case_mapped(&text_of(text), name == "nls_upper"),
        ("nls_initcap", [text, ..]) => crate::string_functions::function("initcap", std::slice::from_ref(text))?,
        ("to_clob", [value]) => Ok(Value::Text(text_of(value))),
        ("to_blob", [value]) => Ok(Value::Bytea(hashed_bytes(value))),
        ("empty_clob", []) => Ok(Value::Text(String::new())),
        ("empty_blob", []) => Ok(Value::Bytea(Vec::new())),
        ("vsize", [value]) => Ok(Value::BigInt(i64::try_from(hashed_bytes(value).len()).unwrap_or(i64::MAX))),
        ("lnnvl", [condition]) => Ok(Value::Boolean(!matches!(condition, Value::Boolean(true)))),
        ("asciistr", [text]) => Ok(Value::Text(ascii_escaped(&text_of(text)))),
        ("to_multi_byte", [text]) => Ok(Value::Text(text_of(text).chars().map(full_width).collect())),
        ("to_single_byte", [text]) => Ok(Value::Text(text_of(text).chars().map(half_width).collect())),
        ("unistr", [text]) => unicode_unescaped(&text_of(text)).map(Value::Text),
        ("bitand", [_, _]) => Ok(Value::BigInt(integer(0)? & integer(1)?)),
        ("remainder", [dividend, divisor]) => {
            let (n2, n1) = (dividend.as_float()?, divisor.as_float()?);
            if n1 == 0.0 {
                Err(SqlError::new(SqlState::DIVISION_BY_ZERO, "divisor is equal to zero").with_oracle_code(1476))
            } else {
                Ok(Value::Double(n2 - n1 * rounded_away(n2 / n1)))
            }
        }
        ("nanvl", [value, replacement]) => Ok(match value {
            Value::Double(number) if number.is_nan() => replacement.clone(),
            Value::Real(number) if number.is_nan() => replacement.clone(),
            other => other.clone(),
        }),
        _ => return None,
    })
}

/// The result type of one of these functions in its dialect.
pub(crate) fn result_type(name: &str, first: Option<SqlType>) -> Option<SqlType> {
    if !dialect_has(name) {
        return None;
    }
    Some(match name {
        "patindex" | "bitand" => SqlType::BigInt,
        "hashbytes" | "sys_guid" | "standard_hash" | "to_blob" | "empty_blob" => SqlType::Bytea,
        "ora_hash" | "vsize" => SqlType::BigInt,
        "lnnvl" => SqlType::Boolean,
        "nls_upper" | "nls_lower" | "nls_initcap" | "to_clob" | "empty_clob" | "asciistr" | "unistr" | "to_multi_byte"
        | "to_single_byte" => SqlType::Text,
        "string_escape" | "app_name" | "formatmessage" => SqlType::Text,
        "isnumeric" => SqlType::Integer,
        "quotename" => SqlType::Text,
        "datefromparts" => SqlType::Date,
        "datetimefromparts" | "datetime2fromparts" | "smalldatetimefromparts" => SqlType::Timestamp(None),
        "timefromparts" => SqlType::Time(None),
        "nanvl" => first.unwrap_or(SqlType::Double),
        _ => SqlType::Double,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_names_and_numbers_as_sql_server_reads_them() {
        assert_eq!(pattern_index("%ter%", "interesting data"), 3);
        assert_eq!(pattern_index("%en_%", "interesting"), 0);
        assert_eq!(pattern_index("%[0-9]%", "abc4d"), 4);
        assert_eq!(pattern_index("%[^a-z]%", "abc4d"), 4);
        assert_eq!(pattern_index("abc%", "ABCdef"), 1);
        assert_eq!(pattern_index("%xyz%", "abc"), 0);
        assert_eq!(quote_name("abc[]def", '[').as_deref(), Some("[abc[]]def]"));
        assert_eq!(quote_name("it's", '\'').as_deref(), Some("'it''s'"));
        assert_eq!(quote_name("x", '!'), None);
        assert!(looks_numeric(" 12.5 "));
        assert!(looks_numeric("$1,000"));
        assert!(looks_numeric("1e5"));
        assert!(looks_numeric("-"));
        assert!(!looks_numeric("12a"));
        assert!(!looks_numeric(""));
        assert_eq!(
            formatted_message("%s has %d eggs, 100%%, %x", &[Value::Text("Ann".to_owned()), Value::Integer(12), Value::Integer(255)]),
            "Ann has 12 eggs, 100%, ff"
        );
        assert_eq!(formatted_message("%s and %s", &[Value::Text("a".to_owned())]), "a and (null)");
        assert_eq!(json_escaped("a\"b\\c/d\n"), "a\\\"b\\\\c\\/d\\n");
        assert_eq!(ascii_escaped("ABÄC\\"), "AB\\00C4C\\005C");
        assert_eq!(unicode_unescaped("AB\\00C4C\\\\").unwrap(), "ABÄC\\");
        assert!(unicode_unescaped("\\12").is_err());
        assert_eq!("Ab 1".chars().map(full_width).collect::<String>(), "\u{FF21}\u{FF42}\u{3000}\u{FF11}");
        assert_eq!("\u{FF21}\u{FF42}\u{3000}\u{FF11}".chars().map(half_width).collect::<String>(), "Ab 1");
        assert_eq!(unicode_unescaped("d\\0061t\\+000061 \\u0062 \\U0001F600").unwrap(), "data b \u{1F600}");
    }
}
