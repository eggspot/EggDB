//! MySQL's JSON functions (D1297): `JSON_EXTRACT`, `JSON_UNQUOTE`, `JSON_LENGTH`, `JSON_KEYS`,
//! `JSON_CONTAINS`, `JSON_VALID` and `JSON_TYPE`, with MySQL's path language (`$`, `.key`,
//! `."quoted key"`, `[n]`, `.*` and `[*]`) and MySQL's errors, in MySQL's dialect.

use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{self, Json, Node};
use crate::types::{SqlType, Value};

const FUNCTIONS: [&str; 14] = [
    "json_search",
    "json_extract",
    "json_unquote",
    "json_length",
    "json_keys",
    "json_contains",
    "json_valid",
    "json_type",
    "json_set",
    "json_insert",
    "json_replace",
    "json_remove",
    "json_merge_patch",
    "json_quote",
];

const EDITING: [(&str, &str); 6] = [
    ("json_set", "sqlite_json_set"),
    ("json_insert", "sqlite_json_insert"),
    ("json_replace", "sqlite_json_replace"),
    ("json_remove", "sqlite_json_remove"),
    ("json_merge_patch", "sqlite_json_patch"),
    ("json_quote", "sqlite_json_quote"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Leg {
    Key(String),
    Index(usize),
    AnyMember,
    AnyElement,
    AnyDepth,
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
            '*' if characters.get(at + 1) == Some(&'*') => {
                if at + 2 >= characters.len() {
                    return Err(invalid_path(path));
                }
                legs.push(Leg::AnyDepth);
                at += 2;
            }
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
    legs.iter().any(|leg| matches!(leg, Leg::AnyMember | Leg::AnyElement | Leg::AnyDepth))
}

fn found<'n>(node: &'n Node, legs: &[Leg], out: &mut Vec<&'n Node>) {
    let Some((leg, rest)) = legs.split_first() else {
        out.push(node);
        return;
    };
    match (leg, &node.value) {
        (Leg::AnyDepth, _) => {
            found(node, rest, out);
            match &node.value {
                Json::Object(members) => members.iter().for_each(|(_, value)| found(value, legs, out)),
                Json::Array(items) => items.iter().for_each(|item| found(item, legs, out)),
                _ => {}
            }
        }
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
    json::parse(&text, true).map_err(|_| {
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

fn like(pattern: &[char], text: &[char], escape: char) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some(('%', rest)) => (0..=text.len()).any(|skip| like(rest, &text[skip..], escape)),
        Some(('_', rest)) => !text.is_empty() && like(rest, &text[1..], escape),
        Some((first, rest)) => {
            let (wanted, rest) = match (*first == escape, rest.split_first()) {
                (true, Some((escaped, after))) => (*escaped, after),
                _ => (*first, rest),
            };
            text.first() == Some(&wanted) && like(rest, &text[1..], escape)
        }
    }
}

fn searched(node: &Node, path: &str, pattern: &[char], escape: char, out: &mut Vec<String>) {
    match &node.value {
        Json::String(text) => {
            let characters: Vec<char> = text.chars().collect();
            if like(pattern, &characters, escape) {
                out.push(path.to_owned());
            }
        }
        Json::Array(items) => {
            for (at, item) in items.iter().enumerate() {
                searched(item, &format!("{path}[{at}]"), pattern, escape, out);
            }
        }
        Json::Object(members) => {
            for (key, value) in members {
                let simple = !key.is_empty() && key.chars().all(|character| character.is_alphanumeric() || character == '_');
                let step = if simple {
                    format!("{path}.{key}")
                } else {
                    let mut quoted = String::new();
                    json::write_string(key, &mut quoted);
                    format!("{path}.{quoted}")
                };
                searched(value, &step, pattern, escape, out);
            }
        }
        _ => {}
    }
}

fn search(values: &[Value]) -> SqlResult<Value> {
    let node = document(&values[0], 1)?;
    let mode = values[1].to_text().unwrap_or_default().to_ascii_lowercase();
    if mode != "one" && mode != "all" {
        return Err(SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "The oneOrAll argument to json_search may take these values: 'one' or 'all'."));
    }
    let pattern: Vec<char> = values[2].to_text().unwrap_or_default().chars().collect();
    let escape = values.get(3).and_then(Value::to_text).and_then(|text| text.chars().next()).unwrap_or('\\');
    let mut found = Vec::new();
    searched(&node, "$", &pattern, escape, &mut found);
    let quoted = |path: &String| {
        let mut out = String::new();
        json::write_string(path, &mut out);
        out
    };
    Ok(match (found.len(), mode.as_str()) {
        (0, _) => Value::Null,
        (_, "one") | (1, _) => Value::Json(quoted(&found[0])),
        _ => Value::Json(format!("[{}]", found.iter().map(quoted).collect::<Vec<_>>().join(", "))),
    })
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
    if values.iter().any(Value::is_null) && name != "json_set" && name != "json_insert" && name != "json_replace" {
        return Some(Ok(Value::Null));
    }
    if let Some((_, internal)) = EDITING.iter().find(|(public, _)| *public == name) {
        if values.first().is_some_and(Value::is_null) {
            return Some(Ok(Value::Null));
        }
        if name != "json_quote" && let Err(error) = document(&values[0], 1) {
            return Some(Err(error));
        }
        return Some(crate::sqlite_json::call(internal, values)?.and_then(|edited| match edited {
            Value::Json(text) | Value::Text(text) if name != "json_quote" => json::normalize(&text).map(Value::Json),
            other => Ok(other),
        }));
    }
    Some(match (name, values.len()) {
        ("json_extract", count) if count >= 2 => extracted(values),
        ("json_search", 3 | 4) => search(values),
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

/// MySQL's `doc->'$.path'` (`JSON_EXTRACT`) and `doc->>'$.path'` (`JSON_UNQUOTE` of it), in
/// MySQL's dialect when the selector is a path; `None` otherwise, for the PostgreSQL operators.
pub(crate) fn arrow(document: &Value, selector: &Value, unquote: bool) -> Option<SqlResult<Value>> {
    if crate::constraints::current_dialect() != Dialect::MySql {
        return None;
    }
    let path = selector.to_text()?;
    if !path.trim_start().starts_with('$') {
        return None;
    }
    if document.is_null() {
        return Some(Ok(Value::Null));
    }
    let extracted = extracted(&[document.clone(), Value::Text(path)]);
    Some(match (extracted, unquote) {
        (Ok(Value::Json(text)), true) => Ok(Value::Text(unquoted(&text))),
        (other, _) => other,
    })
}

/// The result type of one of these functions in MySQL's dialect.
pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    if !FUNCTIONS.contains(&name) || crate::constraints::current_dialect() != Dialect::MySql {
        return None;
    }
    Some(match name {
        "json_extract" | "json_search" | "json_keys" | "json_set" | "json_insert" | "json_replace" | "json_remove" | "json_merge_patch" => SqlType::Json,
        "json_quote" => SqlType::Text,
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
        let mut out = Vec::new();
        found(&node, &legs_of("$**.b").unwrap(), &mut out);
        assert_eq!(out.len(), 1);
        assert!(legs_of("$**").is_err());
        assert!(legs_of("$.a[x]").is_err());
        assert_eq!(unquoted("\"a\\tb\""), "a\tb");
        assert_eq!(unquoted("plain"), "plain");
        assert_eq!(mysql_type(&json::parse("1.5", false).unwrap().value), "DOUBLE");
        let doc = Value::Text(r#"["abc", [{"k": "10"}, "def"], {"x": "abc"}]"#.to_owned());
        assert_eq!(search(&[doc.clone(), Value::Text("one".to_owned()), Value::Text("abc".to_owned())]).unwrap(), Value::Json("\"$[0]\"".to_owned()));
        assert_eq!(
            search(&[doc.clone(), Value::Text("all".to_owned()), Value::Text("abc".to_owned())]).unwrap(),
            Value::Json("[\"$[0]\", \"$[2].x\"]".to_owned())
        );
        assert_eq!(search(&[doc.clone(), Value::Text("one".to_owned()), Value::Text("1%".to_owned())]).unwrap(), Value::Json("\"$[1][0].k\"".to_owned()));
        assert_eq!(search(&[doc, Value::Text("all".to_owned()), Value::Text("zzz".to_owned())]).unwrap(), Value::Null);
    }
}
