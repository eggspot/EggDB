//! MySQL's JSON functions (D1297): `JSON_EXTRACT`, `JSON_UNQUOTE`, `JSON_LENGTH`, `JSON_KEYS`,
//! `JSON_CONTAINS`, `JSON_VALID` and `JSON_TYPE`, with MySQL's path language (`$`, `.key`,
//! `."quoted key"`, `[n]`, `.*` and `[*]`) and MySQL's errors, in MySQL's dialect.

use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{self, Json, Node};
use crate::types::{SqlType, Value};

const FUNCTIONS: [&str; 7] = ["json_extract", "json_unquote", "json_length", "json_keys", "json_contains", "json_valid", "json_type"];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Leg {
    Key(String),
    Index(usize),
    AnyMember,
    AnyElement,
}

fn invalid_path(path: &str) -> SqlError {
    SqlError::new(SqlState::SYNTAX_ERROR, format!("Invalid JSON path expression. The error is around character position 1 in '{path}'."))
}

fn legs_of(path: &str) -> SqlResult<Vec<Leg>> {
    let characters: Vec<char> = path.trim().chars().collect();
    if characters.first() != Some(&'$') {
        return Err(invalid_path(path));
    }
    let mut legs = Vec::new();
    let mut at = 1;
    while at < characters.len() {
        match characters[at] {
            ' ' | '\t' => at += 1,
            '.' => {
                at += 1;
                match characters.get(at) {
                    Some('*') => {
                        legs.push(Leg::AnyMember);
                        at += 1;
                    }
                    Some('"') => {
                        let mut key = String::new();
                        at += 1;
                        loop {
                            match characters.get(at) {
                                Some('\\') => {
                                    key.extend(characters.get(at + 1));
                                    at += 2;
                                }
                                Some('"') => {
                                    at += 1;
                                    break;
                                }
                                Some(other) => {
                                    key.push(*other);
                                    at += 1;
                                }
                                None => return Err(invalid_path(path)),
                            }
                        }
                        legs.push(Leg::Key(key));
                    }
                    _ => {
                        let start = at;
                        while at < characters.len() && (characters[at].is_alphanumeric() || characters[at] == '_' || characters[at] == '$') {
                            at += 1;
                        }
                        if at == start {
                            return Err(invalid_path(path));
                        }
                        legs.push(Leg::Key(characters[start..at].iter().collect()));
                    }
                }
            }
            '[' => {
                let close = characters[at..].iter().position(|character| *character == ']').ok_or_else(|| invalid_path(path))? + at;
                let inside: String = characters[at + 1..close].iter().collect::<String>().trim().to_owned();
                legs.push(if inside == "*" {
                    Leg::AnyElement
                } else {
                    Leg::Index(inside.parse().map_err(|_| invalid_path(path))?)
                });
                at = close + 1;
            }
            _ => return Err(invalid_path(path)),
        }
    }
    Ok(legs)
}

fn wildcard(legs: &[Leg]) -> bool {
    legs.iter().any(|leg| matches!(leg, Leg::AnyMember | Leg::AnyElement))
}

fn found<'n>(node: &'n Node, legs: &[Leg], out: &mut Vec<&'n Node>) {
    let Some((leg, rest)) = legs.split_first() else {
        out.push(node);
        return;
    };
    match (leg, &node.value) {
        (Leg::Key(key), Json::Object(members)) => {
            if let Some((_, value)) = members.iter().rev().find(|(name, _)| name == key) {
                found(value, rest, out);
            }
        }
        (Leg::AnyMember, Json::Object(members)) => members.iter().for_each(|(_, value)| found(value, rest, out)),
        (Leg::Index(index), Json::Array(items)) => {
            if let Some(item) = items.get(*index) {
                found(item, rest, out);
            }
        }
        (Leg::Index(0), _) => found(node, rest, out),
        (Leg::AnyElement, Json::Array(items)) => items.iter().for_each(|item| found(item, rest, out)),
        _ => {}
    }
}

fn document(value: &Value, argument: usize) -> SqlResult<Node> {
    let text = value.to_text().unwrap_or_default();
    json::parse(&text, false).map_err(|_| {
        SqlError::new(SqlState::INVALID_PARAMETER_VALUE, format!("Invalid JSON text in argument {argument} to function."))
    })
}

fn written(node: &Node) -> SqlResult<String> {
    json::normalized_value(&node.value)
}

fn target<'n>(node: &'n Node, path: Option<&Value>) -> SqlResult<Option<&'n Node>> {
    let Some(path) = path else {
        return Ok(Some(node));
    };
    let path = path.to_text().unwrap_or_default();
    let legs = legs_of(&path)?;
    if wildcard(&legs) {
        return Err(SqlError::new(SqlState::SYNTAX_ERROR, "In this situation, path expressions may not contain the * and ** tokens."));
    }
    let mut out = Vec::new();
    found(node, &legs, &mut out);
    Ok(out.first().copied())
}

fn mysql_type(value: &Json) -> &'static str {
    match value {
        Json::Null => "NULL",
        Json::Boolean(_) => "BOOLEAN",
        Json::Number(text) if text.contains(['.', 'e', 'E']) => "DOUBLE",
        Json::Number(_) => "INTEGER",
        Json::String(_) => "STRING",
        Json::Array(_) => "ARRAY",
        Json::Object(_) => "OBJECT",
    }
}

fn unquoted(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        if let Ok(node) = json::parse(trimmed, false)
            && let Json::String(decoded) = node.value
        {
            return decoded;
        }
    }
    text.to_owned()
}

fn extracted(values: &[Value]) -> SqlResult<Value> {
    let node = document(&values[0], 1)?;
    let mut matches = Vec::new();
    let mut many = values.len() > 2;
    for path in &values[1..] {
        let legs = legs_of(&path.to_text().unwrap_or_default())?;
        many |= wildcard(&legs);
        found(&node, &legs, &mut matches);
    }
    if matches.is_empty() {
        return Ok(Value::Null);
    }
    if !many {
        return written(matches[0]).map(Value::Json);
    }
    let pieces = matches.iter().map(|node| written(node)).collect::<SqlResult<Vec<_>>>()?;
    Ok(Value::Json(format!("[{}]", pieces.join(", "))))
}

/// Runs one of these functions in MySQL's dialect; `None` elsewhere or for other names.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !FUNCTIONS.contains(&name) || crate::constraints::current_dialect() != Dialect::MySql {
        return None;
    }
    if values.iter().any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    Some(match (name, values.len()) {
        ("json_extract", count) if count >= 2 => extracted(values),
        ("json_unquote", 1) => Ok(Value::Text(unquoted(&values[0].to_text().unwrap_or_default()))),
        ("json_valid", 1) => Ok(Value::BigInt(i64::from(json::parse(&values[0].to_text().unwrap_or_default(), false).is_ok()))),
        ("json_type", 1) => document(&values[0], 1).map(|node| Value::Text(mysql_type(&node.value).to_owned())),
        ("json_length", 1 | 2) => document(&values[0], 1).and_then(|node| {
            Ok(match target(&node, values.get(1))? {
                None => Value::Null,
                Some(found) => Value::BigInt(match &found.value {
                    Json::Object(members) => members.len() as i64,
                    Json::Array(items) => items.len() as i64,
                    _ => 1,
                }),
            })
        }),
        ("json_keys", 1 | 2) => document(&values[0], 1).and_then(|node| {
            Ok(match target(&node, values.get(1))?.map(|found| &found.value) {
                Some(Json::Object(members)) => {
                    let mut keys = Vec::with_capacity(members.len());
                    for (key, _) in members {
                        let mut quoted = String::new();
                        json::write_string(key, &mut quoted);
                        if !keys.contains(&quoted) {
                            keys.push(quoted);
                        }
                    }
                    Value::Json(format!("[{}]", keys.join(", ")))
                }
                _ => Value::Null,
            })
        }),
        ("json_contains", 2 | 3) => {
            let (Ok(whole), Ok(candidate)) = (document(&values[0], 1), document(&values[1], 2)) else {
                return Some(Err(SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "Invalid JSON text in argument 1 to function json_contains.")));
            };
            target(&whole, values.get(2)).map(|found| match found {
                None => Value::Null,
                Some(found) => Value::BigInt(i64::from(json::contains(&found.value, &candidate.value))),
            })
        }
        _ => return None,
    })
}

/// The result type of one of these functions in MySQL's dialect.
pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    if !FUNCTIONS.contains(&name) || crate::constraints::current_dialect() != Dialect::MySql {
        return None;
    }
    Some(match name {
        "json_extract" | "json_keys" => SqlType::Json,
        "json_unquote" | "json_type" => SqlType::Text,
        _ => SqlType::BigInt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mysql_paths_find_members_elements_and_wildcards() {
        let node = json::parse(r#"{"a": {"b": [10, 20]}, "c d": 1}"#, false).unwrap();
        let mut out = Vec::new();
        found(&node, &legs_of("$.a.b[1]").unwrap(), &mut out);
        assert_eq!(written(out[0]).unwrap(), "20");
        let mut out = Vec::new();
        found(&node, &legs_of(r#"$."c d""#).unwrap(), &mut out);
        assert_eq!(written(out[0]).unwrap(), "1");
        let mut out = Vec::new();
        found(&node, &legs_of("$.a.b[*]").unwrap(), &mut out);
        assert_eq!(out.len(), 2);
        assert!(legs_of("a.b").is_err());
        assert!(legs_of("$.a[x]").is_err());
        assert_eq!(unquoted("\"a\\tb\""), "a\tb");
        assert_eq!(unquoted("plain"), "plain");
        assert_eq!(mysql_type(&json::parse("1.5", false).unwrap().value), "DOUBLE");
    }
}
