//! SQL Server's and Oracle's scalar functions with no PostgreSQL twin (D1292): SQL Server's
//! `PATINDEX`, `QUOTENAME`, `ISNUMERIC` and `DATEFROMPARTS`, and Oracle's `BITAND`,
//! `REMAINDER` and `NANVL`, each answered in its own dialect.

use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{SqlType, Value};

const SQLSERVER_FUNCTIONS: [&str; 8] = [
    "patindex",
    "quotename",
    "isnumeric",
    "datefromparts",
    "datetimefromparts",
    "datetime2fromparts",
    "smalldatetimefromparts",
    "timefromparts",
];
const ORACLE_FUNCTIONS: [&str; 3] = ["bitand", "remainder", "nanvl"];
const MOST_QUOTED_CHARACTERS: usize = 128;

fn dialect_has(name: &str) -> bool {
    match crate::constraints::current_dialect() {
        Dialect::SqlServer => SQLSERVER_FUNCTIONS.contains(&name),
        Dialect::Oracle => ORACLE_FUNCTIONS.contains(&name),
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

/// `PATINDEX(pattern, text)`: the position (from 1) where `pattern` first matches, its leading
/// `%` letting the match start anywhere; 0 when it never does. Case-insensitive, as SQL Server's
/// default collation is.
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

fn rounded_away(value: f64) -> f64 {
    value.round()
}

/// Runs one of these functions in its dialect; `None` elsewhere or for other names.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !dialect_has(name) {
        return None;
    }
    if name != "nanvl" && values.iter().any(Value::is_null) {
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
    }
}
