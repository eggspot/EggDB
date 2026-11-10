use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{Json, Node, Step};
use crate::types::Value;

pub(crate) const FUNCTIONS: [(&str, &str, &[usize]); 13] = [
    ("json", "sqlite_json", &[1]),
    ("json_valid", "sqlite_json_valid", &[1]),
    ("json_type", "sqlite_json_type", &[1, 2]),
    ("json_extract", "sqlite_json_extract", &[]),
    ("json_array", "sqlite_json_array", &[]),
    ("json_object", "sqlite_json_object", &[]),
    ("json_array_length", "sqlite_json_array_length", &[1, 2]),
    ("json_quote", "sqlite_json_quote", &[1]),
    ("json_set", "sqlite_json_set", &[]),
    ("json_insert", "sqlite_json_insert", &[]),
    ("json_replace", "sqlite_json_replace", &[]),
    ("json_remove", "sqlite_json_remove", &[]),
    ("json_patch", "sqlite_json_patch", &[2]),
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
        "sqlite_json_extract"
            | "sqlite_json_array"
            | "sqlite_json_object"
            | "sqlite_json_set"
            | "sqlite_json_insert"
            | "sqlite_json_replace"
            | "sqlite_json_remove"
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum PathStep {
    Key(String),
    Index(i64),
    End,
}

fn reading_steps(path: &str) -> SqlResult<Vec<Step>> {
    path_steps(path)?
        .into_iter()
        .map(|step| match step {
            PathStep::Key(key) => Ok(Step::Key(key)),
            PathStep::Index(index) => Ok(Step::Index(index)),
            PathStep::End => Ok(Step::Index(i64::MAX)),
        })
        .collect()
}

fn path_steps(path: &str) -> SqlResult<Vec<PathStep>> {
    let mut rest = path.strip_prefix('$').ok_or_else(|| bad_path(path))?;
    let mut found = Vec::new();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('.') {
            if let Some(quoted) = after.strip_prefix('"') {
                let end = quoted.find('"').ok_or_else(|| bad_path(path))?;
                found.push(PathStep::Key(quoted[..end].to_owned()));
                rest = &quoted[end + 1..];
            } else {
                let end = after.find(['.', '[']).unwrap_or(after.len());
                if end == 0 {
                    return Err(bad_path(path));
                }
                found.push(PathStep::Key(after[..end].to_owned()));
                rest = &after[end..];
            }
        } else if let Some(after) = rest.strip_prefix('[') {
            let end = after.find(']').ok_or_else(|| bad_path(path))?;
            let inside = after[..end].trim();
            let step = if let Some(back) = inside.strip_prefix('#') {
                let back = back.trim();
                if back.is_empty() {
                    PathStep::End
                } else {
                    let count: i64 = back
                        .strip_prefix('-')
                        .and_then(|digits| digits.trim().parse().ok())
                        .ok_or_else(|| bad_path(path))?;
                    PathStep::Index(count.checked_neg().ok_or_else(|| bad_path(path))?)
                }
            } else {
                PathStep::Index(
                    inside
                        .parse::<i64>()
                        .ok()
                        .filter(|index| *index >= 0)
                        .ok_or_else(|| bad_path(path))?,
                )
            };
            found.push(step);
            rest = &after[end + 1..];
        } else {
            return Err(bad_path(path));
        }
    }
    Ok(found)
}

fn at<'n>(node: &'n Node, path: &str) -> SqlResult<Option<&'n Node>> {
    let mut current = node;
    for step in reading_steps(path)? {
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
        "sqlite_json_arrow" | "sqlite_json_arrow_text" => arrow(name, values),
        "sqlite_json_set" | "sqlite_json_insert" | "sqlite_json_replace" => {
            edited(name, values)
        }
        "sqlite_json_remove" => removed(values),
        "sqlite_json_patch" => patched(values),
        "sqlite_json_quote" => (|| {
            let mut out = String::new();
            write_sql_value(first, &mut out)?;
            Ok(Value::Json(out))
        })(),
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edit {
    Set,
    Insert,
    Replace,
}

fn bare(value: Json) -> Node {
    Node {
        value,
        start: 0,
        end: 0,
    }
}

fn json_of_value(value: &Value) -> SqlResult<Json> {
    let mut text = String::new();
    write_sql_value(value, &mut text)?;
    crate::json::parse(&text, false)
        .map(|node| node.value)
        .map_err(|_| malformed())
}

fn array_position(length: usize, index: i64) -> Option<usize> {
    let position = if index < 0 {
        i64::try_from(length).ok()?.checked_add(index)?
    } else {
        index
    };
    usize::try_from(position).ok()
}

fn place(target: &mut Json, path: &[PathStep], new: Json, edit: Edit) {
    let Some((step, rest)) = path.split_first() else {
        if edit != Edit::Insert {
            *target = new;
        }
        return;
    };
    match (target, step) {
        (Json::Object(members), PathStep::Key(key)) => {
            match members.iter_mut().find(|(name, _)| name == key) {
                Some((_, child)) => place(&mut child.value, rest, new, edit),
                None if edit != Edit::Replace => {
                    let mut created = if rest.is_empty() {
                        new
                    } else if matches!(rest.first(), Some(PathStep::Key(_))) {
                        Json::Object(Vec::new())
                    } else {
                        Json::Array(Vec::new())
                    };
                    if !rest.is_empty() {
                        place(&mut created, rest, new, edit);
                    }
                    members.push((key.clone(), bare(created)));
                }
                None => {}
            }
        }
        (Json::Array(items), PathStep::Index(index)) => {
            if let Some(position) = array_position(items.len(), *index) {
                if let Some(child) = items.get_mut(position) {
                    place(&mut child.value, rest, new, edit);
                } else if position == items.len() && edit != Edit::Replace && rest.is_empty() {
                    items.push(bare(new));
                }
            }
        }
        (Json::Array(items), PathStep::End) if edit != Edit::Replace => {
            if rest.is_empty() {
                items.push(bare(new));
            }
        }
        _ => {}
    }
}

fn edited(name: &str, values: &[Value]) -> SqlResult<Value> {
    let edit = match name {
        "sqlite_json_insert" => Edit::Insert,
        "sqlite_json_replace" => Edit::Replace,
        _ => Edit::Set,
    };
    if values.len() % 2 == 0 {
        return Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            "json_set() needs an odd number of arguments",
        )
        .with_sqlite_code(1));
    }
    let Some((_, node)) = document(values.first().unwrap_or(&Value::Null))? else {
        return Ok(Value::Null);
    };
    let mut root = node.value;
    for pair in values[1..].chunks(2) {
        let Some(path) = path_text(&pair[0])? else {
            return Ok(Value::Null);
        };
        let steps = path_steps(&path)?;
        let new = json_of_value(pair.get(1).unwrap_or(&Value::Null))?;
        place(&mut root, &steps, new, edit);
    }
    Ok(Value::Json(minified(&root)))
}

fn remove_at(target: &mut Json, path: &[PathStep]) {
    let Some((step, rest)) = path.split_first() else {
        return;
    };
    match (target, step) {
        (Json::Object(members), PathStep::Key(key)) => {
            let Some(position) = members.iter().position(|(name, _)| name == key) else {
                return;
            };
            if rest.is_empty() {
                members.remove(position);
            } else if let Some((_, child)) = members.get_mut(position) {
                remove_at(&mut child.value, rest);
            }
        }
        (Json::Array(items), PathStep::Index(index)) => {
            let Some(position) = array_position(items.len(), *index) else {
                return;
            };
            if position >= items.len() {
                return;
            }
            if rest.is_empty() {
                items.remove(position);
            } else if let Some(child) = items.get_mut(position) {
                remove_at(&mut child.value, rest);
            }
        }
        _ => {}
    }
}

fn removed(values: &[Value]) -> SqlResult<Value> {
    let Some((_, node)) = document(values.first().unwrap_or(&Value::Null))? else {
        return Ok(Value::Null);
    };
    let mut root = node.value;
    for path in &values[1..] {
        let Some(path) = path_text(path)? else {
            return Ok(Value::Null);
        };
        let steps = path_steps(&path)?;
        if steps.is_empty() {
            return Ok(Value::Null);
        }
        remove_at(&mut root, &steps);
    }
    Ok(Value::Json(minified(&root)))
}

fn merge_patch(target: Json, patch: Json) -> Json {
    let Json::Object(changes) = patch else {
        return patch;
    };
    let mut members = match target {
        Json::Object(members) => members,
        _ => Vec::new(),
    };
    for (key, change) in changes {
        let position = members.iter().position(|(name, _)| *name == key);
        if matches!(change.value, Json::Null) {
            if let Some(position) = position {
                members.remove(position);
            }
            continue;
        }
        match position {
            Some(position) => {
                let current = std::mem::replace(&mut members[position].1.value, Json::Null);
                members[position].1.value = merge_patch(current, change.value);
            }
            None => members.push((key, bare(merge_patch(Json::Null, change.value)))),
        }
    }
    Json::Object(members)
}

fn patched(values: &[Value]) -> SqlResult<Value> {
    let (Some((_, target)), Some((_, patch))) = (
        document(values.first().unwrap_or(&Value::Null))?,
        document(values.get(1).unwrap_or(&Value::Null))?,
    ) else {
        return Ok(Value::Null);
    };
    Ok(Value::Json(minified(&merge_patch(target.value, patch.value))))
}

fn arrow_path(right: &Value) -> Option<String> {
    match right {
        Value::Null => None,
        Value::Text(text) if text.starts_with('$') => Some(text.clone()),
        Value::Text(text) => Some(format!("$.\"{text}\"")),
        other => match other.as_integer() {
            Some(index) if index >= 0 => Some(format!("$[{index}]")),
            Some(index) => Some(format!("$[#{index}]")),
            None => other.to_text().map(|text| format!("$.\"{text}\"")),
        },
    }
}

fn arrow(name: &str, values: &[Value]) -> SqlResult<Value> {
    let Some((_, node)) = document(values.first().unwrap_or(&Value::Null))? else {
        return Ok(Value::Null);
    };
    let Some(path) = arrow_path(values.get(1).unwrap_or(&Value::Null)) else {
        return Ok(Value::Null);
    };
    let Some(found) = at(&node, &path)? else {
        return Ok(Value::Null);
    };
    Ok(if name == "sqlite_json_arrow" {
        Value::Json(minified(&found.value))
    } else {
        sql_value(found)
    })
}

pub(crate) const EACH: &str = "sqlite_json_each";
pub(crate) const TREE: &str = "sqlite_json_tree";

pub(crate) const EACH_COLUMNS: [(&str, crate::types::SqlType); 8] = [
    ("key", crate::types::SqlType::DYNAMIC),
    ("value", crate::types::SqlType::DYNAMIC),
    ("type", crate::types::SqlType::Text),
    ("atom", crate::types::SqlType::DYNAMIC),
    ("id", crate::types::SqlType::BigInt),
    ("parent", crate::types::SqlType::BigInt),
    ("fullkey", crate::types::SqlType::Text),
    ("path", crate::types::SqlType::Text),
];

pub(crate) fn each_type(types: &[Option<crate::types::SqlType>]) -> SqlResult<crate::types::SqlType> {
    if types.is_empty() || types.len() > 2 {
        return Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            "wrong number of arguments to function json_each()",
        )
        .with_sqlite_code(1));
    }
    Ok(crate::types::SqlType::Defined(crate::types::DefinedType::RECORD))
}

fn child_path(path: &str, key: &Value) -> String {
    match key {
        Value::BigInt(index) => format!("{path}[{index}]"),
        Value::Text(name)
            if !name.is_empty()
                && name.chars().all(|character| character.is_ascii_alphanumeric() || character == '_') =>
        {
            format!("{path}.{name}")
        }
        Value::Text(name) => format!("{path}.\"{name}\""),
        _ => path.to_owned(),
    }
}

fn atom(node: &Node) -> Value {
    match &node.value {
        Json::Array(_) | Json::Object(_) => Value::Null,
        _ => sql_value(node),
    }
}

struct Walk<'s, 'r> {
    sink: &'s mut crate::json_functions::RecordSink<'r>,
    next_id: i64,
}

impl Walk<'_, '_> {
    fn emit(
        &mut self,
        node: &Node,
        key: Value,
        parent: Option<i64>,
        fullkey: &str,
        path: &str,
    ) -> SqlResult<i64> {
        let id = self.next_id;
        self.next_id += 1;
        (self.sink)(vec![
            key,
            sql_value(node),
            Value::Text(type_name(node).to_owned()),
            atom(node),
            Value::BigInt(id),
            parent.map_or(Value::Null, Value::BigInt),
            Value::Text(fullkey.to_owned()),
            Value::Text(path.to_owned()),
        ])?;
        Ok(id)
    }

    fn children(node: &Node) -> Vec<(Value, &Node)> {
        match &node.value {
            Json::Array(items) => items
                .iter()
                .enumerate()
                .map(|(index, item)| (Value::BigInt(index as i64), item))
                .collect(),
            Json::Object(members) => members
                .iter()
                .map(|(name, item)| (Value::Text(name.clone()), item))
                .collect(),
            _ => Vec::new(),
        }
    }

    fn tree(&mut self, node: &Node, key: Value, parent: Option<i64>, fullkey: &str, path: &str) -> SqlResult<()> {
        let id = self.emit(node, key, parent, fullkey, path)?;
        for (child_key, child) in Self::children(node) {
            let child_fullkey = child_path(fullkey, &child_key);
            self.tree(child, child_key, Some(id), &child_fullkey, fullkey)?;
        }
        Ok(())
    }
}

pub(crate) fn each_records(
    name: &str,
    values: &[Value],
    sink: &mut crate::json_functions::RecordSink<'_>,
) -> SqlResult<()> {
    let Some((_, root)) = document(values.first().unwrap_or(&Value::Null))? else {
        return Ok(());
    };
    let start = match values.get(1) {
        Some(path) => match path_text(path)? {
            Some(path) => path,
            None => return Ok(()),
        },
        None => "$".to_owned(),
    };
    let Some(node) = at(&root, &start)? else {
        return Ok(());
    };
    let mut walk = Walk { sink, next_id: 1 };
    if name == TREE {
        return walk.tree(node, Value::Null, None, &start, &start);
    }
    let children = Walk::children(node);
    if children.is_empty() && !matches!(node.value, Json::Array(_) | Json::Object(_)) {
        walk.emit(node, Value::Null, None, &start, &start)?;
        return Ok(());
    }
    for (key, child) in children {
        let fullkey = child_path(&start, &key);
        walk.emit(child, key, None, &fullkey, &start)?;
    }
    Ok(())
}

pub(crate) fn each_rows(
    name: &str,
    values: &[Value],
    sink: &mut crate::set_returning::RowSink<'_>,
) -> SqlResult<()> {
    each_records(name, values, &mut |fields| {
        sink(crate::records::record_value(
            crate::records::RECORD_OID,
            fields,
            EACH_COLUMNS.iter().map(|(column, _)| (*column).to_owned()).collect(),
        ))
    })
}

