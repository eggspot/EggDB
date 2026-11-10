use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{Json, Node, Step};
use crate::types::Value;

pub(crate) const FUNCTIONS: [(&str, &str, &[usize]); 8] = [
    ("json", "sqlite_json", &[1]),
    ("json_valid", "sqlite_json_valid", &[1]),
    ("json_type", "sqlite_json_type", &[1, 2]),
    ("json_extract", "sqlite_json_extract", &[]),
    ("json_array", "sqlite_json_array", &[]),
    ("json_object", "sqlite_json_object", &[]),
    ("json_array_length", "sqlite_json_array_length", &[1, 2]),
    ("json_quote", "sqlite_json_quote", &[1]),
];

pub(crate) fn named(name: &str) -> Option<(&'static str, &'static [usize])> {
    FUNCTIONS
        .iter()
        .find(|(public, _, _)| *public == name)
        .map(|(_, internal, counts)| (*internal, *counts))
}

pub(crate) fn takes_any_count(internal: &str) -> bool {
    matches!(
        internal,
        "sqlite_json_extract" | "sqlite_json_array" | "sqlite_json_object"
    )
}

fn malformed() -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "malformed JSON").with_sqlite_code(1)
}

fn bad_path(path: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("bad JSON path: '{path}'"),
    )
    .with_sqlite_code(1)
}

fn document(value: &Value) -> SqlResult<Option<(String, Node)>> {
    let text = match value {
        Value::Null => return Ok(None),
        Value::Text(text) | Value::Json(text) | Value::Jsonb(text) => text.clone(),
        other => match other.to_text() {
            Some(text) => text,
            None => return Err(malformed()),
        },
    };
    let node = crate::json::parse(&text, false).map_err(|_| malformed())?;
    Ok(Some((text, node)))
}

fn steps(path: &str) -> SqlResult<Vec<Step>> {
    let mut rest = path.strip_prefix('$').ok_or_else(|| bad_path(path))?;
    let mut found = Vec::new();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('.') {
            if let Some(quoted) = after.strip_prefix('"') {
                let end = quoted.find('"').ok_or_else(|| bad_path(path))?;
                found.push(Step::Key(quoted[..end].to_owned()));
                rest = &quoted[end + 1..];
            } else {
                let end = after.find(['.', '[']).unwrap_or(after.len());
                if end == 0 {
                    return Err(bad_path(path));
                }
                found.push(Step::Key(after[..end].to_owned()));
                rest = &after[end..];
            }
        } else if let Some(after) = rest.strip_prefix('[') {
            let end = after.find(']').ok_or_else(|| bad_path(path))?;
            let inside = after[..end].trim();
            let index = if let Some(back) = inside.strip_prefix('#') {
                let back = back.trim();
                if back.is_empty() {
                    return Err(bad_path(path));
                }
                let count: i64 = back
                    .strip_prefix('-')
                    .and_then(|digits| digits.trim().parse().ok())
                    .ok_or_else(|| bad_path(path))?;
                count.checked_neg().ok_or_else(|| bad_path(path))?
            } else {
                inside.parse::<i64>().ok().filter(|index| *index >= 0).ok_or_else(|| bad_path(path))?
            };
            found.push(Step::Index(index));
            rest = &after[end + 1..];
        } else {
            return Err(bad_path(path));
        }
    }
    Ok(found)
}

fn at<'n>(node: &'n Node, path: &str) -> SqlResult<Option<&'n Node>> {
    let mut current = node;
    for step in steps(path)? {
        match crate::json::child(current, &step, false) {
            Some(next) => current = next,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}

fn write_minified(value: &Json, out: &mut String) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Boolean(true) => out.push_str("true"),
        Json::Boolean(false) => out.push_str("false"),
        Json::Number(text) => out.push_str(text),
        Json::String(text) => crate::json::write_string(text, out),
        Json::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_minified(&item.value, out);
            }
            out.push(']');
        }
        Json::Object(members) => {
            out.push('{');
            for (index, (key, item)) in members.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                crate::json::write_string(key, out);
                out.push(':');
                write_minified(&item.value, out);
            }
            out.push('}');
        }
    }
}

fn minified(value: &Json) -> String {
    let mut out = String::new();
    write_minified(value, &mut out);
    out
}

fn sql_value(node: &Node) -> Value {
    match &node.value {
        Json::Null => Value::Null,
        Json::Boolean(flag) => Value::BigInt(i64::from(*flag)),
        Json::Number(text) => match text.parse::<i64>() {
            Ok(integer) => Value::BigInt(integer),
            Err(_) => text.parse::<f64>().map_or(Value::Null, Value::Double),
        },
        Json::String(text) => Value::Text(text.clone()),
        other => Value::Json(minified(other)),
    }
}

fn type_name(node: &Node) -> &'static str {
    match &node.value {
        Json::Null => "null",
        Json::Boolean(true) => "true",
        Json::Boolean(false) => "false",
        Json::Number(text) if text.parse::<i64>().is_ok() => "integer",
        Json::Number(_) => "real",
        Json::String(_) => "text",
        Json::Array(_) => "array",
        Json::Object(_) => "object",
    }
}

fn write_sql_value(value: &Value, out: &mut String) -> SqlResult<()> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Json(text) | Value::Jsonb(text) => {
            let node = crate::json::parse(text, false).map_err(|_| malformed())?;
            write_minified(&node.value, out);
        }
        Value::Text(text) => crate::json::write_string(text, out),
        Value::Bytea(_) => {
            return Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "JSON cannot hold BLOB values",
            )
            .with_sqlite_code(1));
        }
        Value::Double(number) if !number.is_finite() => out.push_str("null"),
        other => match other.to_text() {
            Some(text) if other.as_float().is_some() => out.push_str(&text),
            Some(text) => crate::json::write_string(&text, out),
            None => out.push_str("null"),
        },
    }
    Ok(())
}

fn path_text(value: &Value) -> SqlResult<Option<String>> {
    match value {
        Value::Null => Ok(None),
        other => other.to_text().map(Some).ok_or_else(|| bad_path("")),
    }
}

pub(crate) fn call(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    let first = values.first().unwrap_or(&Value::Null);
    Some(match name {
        "sqlite_json" => document(first).map(|found| {
            found.map_or(Value::Null, |(_, node)| Value::Json(minified(&node.value)))
        }),
        "sqlite_json_valid" => Ok(Value::BigInt(match first {
            Value::Null => return Some(Ok(Value::Null)),
            other => i64::from(document(other).is_ok_and(|found| found.is_some())),
        })),
        "sqlite_json_type" => (|| {
            let Some((_, node)) = document(first)? else {
                return Ok(Value::Null);
            };
            let target = match values.get(1) {
                Some(path) => match path_text(path)? {
                    Some(path) => at(&node, &path)?,
                    None => return Ok(Value::Null),
                },
                None => Some(&node),
            };
            Ok(target.map_or(Value::Null, |found| Value::Text(type_name(found).to_owned())))
        })(),
        "sqlite_json_array_length" => (|| {
            let Some((_, node)) = document(first)? else {
                return Ok(Value::Null);
            };
            let target = match values.get(1) {
                Some(path) => match path_text(path)? {
                    Some(path) => at(&node, &path)?,
                    None => return Ok(Value::Null),
                },
                None => Some(&node),
            };
            Ok(match target.map(|found| &found.value) {
                Some(Json::Array(items)) => Value::BigInt(items.len() as i64),
                Some(_) => Value::BigInt(0),
                None => Value::Null,
            })
        })(),
        "sqlite_json_extract" => (|| {
            if values.len() < 2 {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_FUNCTION,
                    "wrong number of arguments to function json_extract()",
                )
                .with_sqlite_code(1));
            }
            let Some((_, node)) = document(first)? else {
                return Ok(Value::Null);
            };
            let mut found = Vec::with_capacity(values.len() - 1);
            for path in &values[1..] {
                let Some(path) = path_text(path)? else {
                    return Ok(Value::Null);
                };
                found.push(at(&node, &path)?);
            }
            if found.len() == 1 {
                return Ok(found[0].map_or(Value::Null, sql_value));
            }
            let mut out = String::from("[");
            for (index, item) in found.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                match item {
                    Some(item) => write_minified(&item.value, &mut out),
                    None => out.push_str("null"),
                }
            }
            out.push(']');
            Ok(Value::Json(out))
        })(),
        "sqlite_json_array" => (|| {
            let mut out = String::from("[");
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_sql_value(value, &mut out)?;
            }
            out.push(']');
            Ok(Value::Json(out))
        })(),
        "sqlite_json_object" => (|| {
            if values.len() % 2 != 0 {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_FUNCTION,
                    "json_object() requires an even number of arguments",
                )
                .with_sqlite_code(1));
            }
            let mut out = String::from("{");
            for (index, pair) in values.chunks(2).enumerate() {
                let Some(Value::Text(key)) = pair.first() else {
                    return Err(SqlError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "json_object() labels must be TEXT",
                    )
                    .with_sqlite_code(1));
                };
                if index > 0 {
                    out.push(',');
                }
                crate::json::write_string(key, &mut out);
                out.push(':');
                write_sql_value(pair.get(1).unwrap_or(&Value::Null), &mut out)?;
            }
            out.push('}');
            Ok(Value::Json(out))
        })(),
        "sqlite_json_quote" => (|| {
            let mut out = String::new();
            write_sql_value(first, &mut out)?;
            Ok(Value::Json(out))
        })(),
        _ => return None,
    })
}
