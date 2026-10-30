use crate::ast::{Expr, ExprKind, FromClause, SelectItem};
use crate::datetime;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::Evaluator;
use crate::json::{self, Json, Node};
use crate::types::{SqlType, Value};

pub(crate) const FUNCTIONS: [&str; 16] = [
    "to_json",
    "to_jsonb",
    "row_to_json",
    "json_build_object",
    "jsonb_build_object",
    "json_build_array",
    "jsonb_build_array",
    "jsonb_set",
    "jsonb_insert",
    "jsonb_strip_nulls",
    "json_strip_nulls",
    "jsonb_pretty",
    "json_extract_path",
    "jsonb_extract_path",
    "json_extract_path_text",
    "jsonb_extract_path_text",
];

pub(crate) const SET_RETURNING: [&str; 10] = [
    "json_each",
    "jsonb_each",
    "json_each_text",
    "jsonb_each_text",
    "json_array_elements",
    "jsonb_array_elements",
    "json_array_elements_text",
    "jsonb_array_elements_text",
    "json_object_keys",
    "jsonb_object_keys",
];

pub(crate) const WHOLE_ROW: &str = "whole_row";
pub(crate) const ROW_OBJECT: &str = "json_row";
pub(crate) const OPENJSON_VALUE: &str = "*openjson value*";
pub(crate) const OPENJSON_JSON: &str = "*openjson json*";

const WHOLE_ROW_CALLERS: [&str; 7] = [
    "row_to_json",
    "to_json",
    "to_jsonb",
    "json_agg",
    "jsonb_agg",
    "json_build_array",
    "jsonb_build_array",
];

fn invalid(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

fn null_key() -> SqlError {
    SqlError::new(
        SqlState::NULL_VALUE_NOT_ALLOWED,
        "null value not allowed for object key",
    )
}

fn leaf(value: Json) -> Node {
    Node {
        value,
        start: 0,
        end: 0,
    }
}

fn iso_local(micros: i64) -> String {
    datetime::format_timestamp(micros).replacen(' ', "T", 1)
}

fn iso_offset(offset: i32) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let magnitude = offset.unsigned_abs();
    let (hours, minutes, seconds) = (magnitude / 3600, magnitude / 60 % 60, magnitude % 60);
    if seconds == 0 {
        format!("{sign}{hours:02}:{minutes:02}")
    } else {
        format!("{sign}{hours:02}:{minutes:02}:{seconds:02}")
    }
}

fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    json::write_string(text, &mut out);
    out
}

pub(crate) fn to_json_text(value: &Value) -> SqlResult<String> {
    let text = match value {
        Value::Null => "null".to_owned(),
        Value::Boolean(flag) => flag.to_string(),
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) | Value::Oid(_) => {
            value.to_text().unwrap_or_default()
        }
        Value::Numeric(number) if number.is_nan() => quoted("NaN"),
        Value::Numeric(number) => number.to_string(),
        Value::Real(_) | Value::Double(_) => {
            let number = value.as_float().unwrap_or(f64::NAN);
            let shown = value.to_text().unwrap_or_default();
            if number.is_finite() {
                shown
            } else {
                quoted(&shown)
            }
        }
        Value::Json(text) | Value::Jsonb(text) => text.clone(),
        Value::Text(text) | Value::Bpchar(text) => quoted(text),
        Value::Date(days) => quoted(&datetime::format_date(*days)),
        Value::Timestamp(micros) => quoted(&iso_local(*micros)),
        Value::TimestampTz(micros) => {
            let offset = datetime::session_offset_at(*micros);
            quoted(&format!(
                "{}{}",
                iso_local(datetime::to_local(*micros, offset)),
                iso_offset(offset)
            ))
        }
        Value::Array(array) => {
            let mut out = String::from("[");
            for (at, item) in array.items.iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                out.push_str(&to_json_text(item)?);
                crate::memory::check_value_bytes(out.len())?;
            }
            out.push(']');
            out
        }
        other => quoted(&other.to_text().unwrap_or_default()),
    };
    crate::memory::check_value_bytes(text.len())?;
    Ok(text)
}

fn to_jsonb_text(value: &Value) -> SqlResult<String> {
    match value {
        Value::Jsonb(text) => Ok(text.clone()),
        other => json::normalize(&to_json_text(other)?),
    }
}

fn is_structured(value: &Value) -> bool {
    matches!(value, Value::Array(_))
}

pub(crate) fn json_element(value: &Value, binary: bool) -> SqlResult<String> {
    if binary {
        to_jsonb_text(value)
    } else {
        to_json_text(value)
    }
}

pub(crate) fn aggregate_separator(value: &Value, from_whole_row: bool) -> &'static str {
    if from_whole_row || is_structured(value) {
        ", \n "
    } else {
        ", "
    }
}

pub(crate) fn object_key(value: &Value) -> SqlResult<String> {
    match value {
        Value::Null => Err(null_key()),
        Value::Json(_) | Value::Jsonb(_) | Value::Array(_) => Err(invalid(
            "key value must be scalar, not array, composite, or json",
        )),
        other => Ok(other.to_text().unwrap_or_default()),
    }
}

pub(crate) fn finish_list(items: &str, binary: bool) -> SqlResult<Value> {
    let text = format!("[{items}]");
    if binary {
        json::normalize(&text).map(Value::Jsonb)
    } else {
        Ok(Value::Json(text))
    }
}

pub(crate) fn finish_object(members: &str, binary: bool) -> SqlResult<Value> {
    if binary {
        json::normalize(&format!("{{{members}}}")).map(Value::Jsonb)
    } else {
        Ok(Value::Json(format!("{{ {members} }}")))
    }
}

pub(crate) fn object_member(key: &str, value: &str, binary: bool) -> String {
    if binary {
        format!("{}: {value}", quoted(key))
    } else {
        format!("{} : {value}", quoted(key))
    }
}

fn build_object(values: &[Value], binary: bool, name: &str) -> SqlResult<Value> {
    if !values.len().is_multiple_of(2) {
        return Err(invalid("argument list must have even number of elements").with_hint(
            format!(
                "The arguments of {name}() must consist of alternating keys and values."
            ),
        ));
    }
    let mut out = String::from("{");
    for (at, pair) in values.chunks_exact(2).enumerate() {
        let key = object_key(&pair[0]).map_err(|error| {
            if pair[0].is_null() {
                SqlError::new(
                    SqlState::NULL_VALUE_NOT_ALLOWED,
                    format!("argument {} cannot be null", at * 2 + 1),
                )
                .with_hint("Object keys should be text.")
            } else {
                error
            }
        })?;
        if at > 0 {
            out.push_str(", ");
        }
        out.push_str(&quoted(&key));
        out.push_str(" : ");
        out.push_str(&to_json_text(&pair[1])?);
        crate::memory::check_value_bytes(out.len())?;
    }
    out.push('}');
    if binary {
        json::normalize(&out).map(Value::Jsonb)
    } else {
        Ok(Value::Json(out))
    }
}

fn build_array(values: &[Value], binary: bool) -> SqlResult<Value> {
    let mut out = String::from("[");
    for (at, value) in values.iter().enumerate() {
        if at > 0 {
            out.push_str(", ");
        }
        out.push_str(&to_json_text(value)?);
        crate::memory::check_value_bytes(out.len())?;
    }
    out.push(']');
    if binary {
        json::normalize(&out).map(Value::Jsonb)
    } else {
        Ok(Value::Json(out))
    }
}

struct Document {
    text: String,
    root: Node,
    binary: bool,
}

fn document(value: &Value, binary: bool) -> Option<SqlResult<Document>> {
    let (text, binary) = match value {
        Value::Json(text) => (text, false),
        Value::Jsonb(text) => (text, true),
        Value::Text(text) => (text, binary),
        _ => return None,
    };
    Some(json::parse(text, binary).map(|root| Document {
        text: text.clone(),
        root,
        binary,
    }))
}

fn jsonb_tree(value: &Value) -> Option<SqlResult<Json>> {
    Some(document(value, true)?.map(|document| document.root.value))
}

fn compact(value: &Json, out: &mut String) -> SqlResult<()> {
    match value {
        Json::Null => out.push_str("null"),
        Json::Boolean(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Json::Number(text) => out.push_str(text),
        Json::String(text) => json::write_string(text, out),
        Json::Array(items) => {
            out.push('[');
            for (at, item) in items.iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                compact(&item.value, out)?;
            }
            out.push(']');
        }
        Json::Object(members) => {
            out.push('{');
            for (at, (key, item)) in members.iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                json::write_string(key, out);
                out.push(':');
                compact(&item.value, out)?;
            }
            out.push('}');
        }
    }
    crate::memory::check_value_bytes(out.len())
}

fn path_of(value: &Value) -> SqlResult<Vec<String>> {
    match value {
        Value::Array(array) => array
            .items
            .iter()
            .enumerate()
            .map(|(at, item)| match item {
                Value::Null => Err(SqlError::new(
                    SqlState::NULL_VALUE_NOT_ALLOWED,
                    format!("path element at position {} is null", at + 1),
                )),
                other => Ok(other.to_text().unwrap_or_default()),
            })
            .collect(),
        other => json::parse_path(other.character_text().unwrap_or_default()),
    }
}

fn array_position(step: &str, at: usize, length: usize) -> SqlResult<i64> {
    let index: i64 = step.trim().parse().map_err(|_| {
        SqlError::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!("path element at position {at} is not an integer: \"{step}\""),
        )
    })?;
    Ok(if index < 0 {
        index + length as i64
    } else {
        index
    })
}

fn set_at(node: &mut Json, path: &[String], depth: usize, new: &Json, create: bool) -> SqlResult<()> {
    let Some(step) = path.get(depth) else {
        return Ok(());
    };
    let last = depth + 1 == path.len();
    match node {
        Json::Object(members) => {
            match members.iter_mut().find(|(key, _)| key == step) {
                Some((_, child)) if last => child.value = new.clone(),
                Some((_, child)) => set_at(&mut child.value, path, depth + 1, new, create)?,
                None if last && create => members.push((step.clone(), leaf(new.clone()))),
                None => {}
            }
        }
        Json::Array(items) => {
            let index = array_position(step, depth + 1, items.len())?;
            match usize::try_from(index).ok().filter(|index| *index < items.len()) {
                Some(index) if last => items[index].value = new.clone(),
                Some(index) => set_at(&mut items[index].value, path, depth + 1, new, create)?,
                None if last && create => {
                    if index < 0 {
                        items.insert(0, leaf(new.clone()));
                    } else {
                        items.push(leaf(new.clone()));
                    }
                }
                None => {}
            }
        }
        _ if depth == 0 => return Err(invalid("cannot set path in scalar")),
        _ => {}
    }
    Ok(())
}

fn insert_at(node: &mut Json, path: &[String], depth: usize, new: &Json, after: bool) -> SqlResult<()> {
    let Some(step) = path.get(depth) else {
        return Ok(());
    };
    let last = depth + 1 == path.len();
    match node {
        Json::Object(members) => {
            let found = members.iter_mut().find(|(key, _)| key == step);
            match found {
                Some(_) if last => {
                    return Err(invalid("cannot replace existing key")
                        .with_hint("Try using the function jsonb_set to replace key value."));
                }
                Some((_, child)) => insert_at(&mut child.value, path, depth + 1, new, after)?,
                None if last => members.push((step.clone(), leaf(new.clone()))),
                None => {}
            }
        }
        Json::Array(items) => {
            let index = array_position(step, depth + 1, items.len())?;
            if last {
                let length = items.len() as i64;
                let at = if after { index + 1 } else { index }.clamp(0, length);
                items.insert(usize::try_from(at).unwrap_or(0), leaf(new.clone()));
            } else if let Some(child) = usize::try_from(index).ok().and_then(|at| items.get_mut(at)) {
                insert_at(&mut child.value, path, depth + 1, new, after)?;
            }
        }
        _ if depth == 0 => return Err(invalid("cannot set path in scalar")),
        _ => {}
    }
    Ok(())
}

fn delete_at(node: &mut Json, path: &[String], depth: usize) -> SqlResult<()> {
    let Some(step) = path.get(depth) else {
        return Ok(());
    };
    let last = depth + 1 == path.len();
    match node {
        Json::Object(members) => {
            if last {
                members.retain(|(key, _)| key != step);
            } else if let Some((_, child)) = members.iter_mut().find(|(key, _)| key == step) {
                delete_at(&mut child.value, path, depth + 1)?;
            }
        }
        Json::Array(items) => {
            let index = array_position(step, depth + 1, items.len())?;
            if let Some(at) = usize::try_from(index).ok().filter(|at| *at < items.len()) {
                if last {
                    items.remove(at);
                } else {
                    delete_at(&mut items[at].value, path, depth + 1)?;
                }
            }
        }
        _ if depth == 0 => return Err(invalid("cannot delete path in scalar")),
        _ => {}
    }
    Ok(())
}

fn strip_nulls(node: &mut Json) {
    match node {
        Json::Object(members) => {
            members.retain(|(_, child)| child.value != Json::Null);
            for (_, child) in members.iter_mut() {
                strip_nulls(&mut child.value);
            }
        }
        Json::Array(items) => {
            for item in items.iter_mut() {
                strip_nulls(&mut item.value);
            }
        }
        _ => {}
    }
}

fn pretty(value: &Json, depth: usize, out: &mut String) -> SqlResult<()> {
    let indent = |out: &mut String, depth: usize| {
        out.push('\n');
        out.push_str(&"    ".repeat(depth));
    };
    match value {
        Json::Array(items) => {
            out.push('[');
            for (at, item) in items.iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                indent(out, depth + 1);
                pretty(&item.value, depth + 1, out)?;
            }
            indent(out, depth);
            out.push(']');
        }
        Json::Object(members) => {
            out.push('{');
            for (at, (key, item)) in members.iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                indent(out, depth + 1);
                json::write_string(key, out);
                out.push_str(": ");
                pretty(&item.value, depth + 1, out)?;
            }
            indent(out, depth);
            out.push('}');
        }
        other => out.push_str(&json::normalized_value(other)?),
    }
    crate::memory::check_value_bytes(out.len())
}

fn jsonb_value(tree: &Json) -> SqlResult<Value> {
    json::normalized_value(tree).map(Value::Jsonb)
}

fn edited(
    values: &[Value],
    edit: impl FnOnce(&mut Json, &[String]) -> SqlResult<()>,
) -> Option<SqlResult<Value>> {
    let tree = jsonb_tree(values.first()?)?;
    let path = path_of(values.get(1)?);
    Some(tree.and_then(|mut tree| {
        let path = path?;
        edit(&mut tree, &path)?;
        jsonb_value(&tree)
    }))
}

fn flag(values: &[Value], at: usize, default: bool) -> Option<bool> {
    match values.get(at) {
        None => Some(default),
        Some(Value::Boolean(flag)) => Some(*flag),
        Some(_) => None,
    }
}

fn extracted(values: &[Value], binary: bool, as_text: bool) -> Option<SqlResult<Value>> {
    let (first, steps) = values.split_first()?;
    let steps: Option<Vec<String>> = steps
        .iter()
        .map(|step| step.character_text().map(str::to_owned))
        .collect();
    let steps = steps?;
    let document = match document(first, binary)? {
        Ok(document) => document,
        Err(error) => return Some(Err(error)),
    };
    let mut node = &document.root;
    for step in &steps {
        match json::child(node, &json::Step::Key(step.clone()), document.binary) {
            Some(next) => node = next,
            None => return Some(Ok(Value::Null)),
        }
    }
    Some(if as_text {
        json::render_text(&document.text, node, document.binary)
            .map(|text| text.map_or(Value::Null, Value::Text))
    } else {
        json::render(&document.text, node, document.binary).map(|text| {
            if document.binary {
                Value::Jsonb(text)
            } else {
                Value::Json(text)
            }
        })
    })
}

fn row_to_json(values: &[Value]) -> Option<SqlResult<Value>> {
    let (record, pretty_lines) = match values {
        [record] => (record, false),
        [record, Value::Boolean(flag)] => (record, *flag),
        _ => return None,
    };
    let Value::Json(text) = record else {
        return None;
    };
    if !pretty_lines {
        return Some(Ok(Value::Json(text.clone())));
    }
    Some(json::parse(text, false).and_then(|node| match node.value {
        Json::Object(members) => {
            let mut out = String::from("{");
            for (at, (key, item)) in members.iter().enumerate() {
                if at > 0 {
                    out.push_str(",\n ");
                }
                json::write_string(key, &mut out);
                out.push(':');
                compact(&item.value, &mut out)?;
            }
            out.push('}');
            Ok(Value::Json(out))
        }
        _ => Ok(Value::Json(text.clone())),
    }))
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    match name {
        OPENJSON_VALUE => return openjson_extracted(values, false),
        OPENJSON_JSON => return openjson_extracted(values, true),
        _ => {}
    }
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    let builder = matches!(
        name,
        "json_build_object" | "jsonb_build_object" | "json_build_array" | "jsonb_build_array"
    );
    if !builder && values.iter().any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    Some(match name {
        "to_json" => to_json_text(values.first()?).map(Value::Json),
        "to_jsonb" => to_jsonb_text(values.first()?).map(Value::Jsonb),
        "row_to_json" => return row_to_json(values),
        "json_build_object" => build_object(values, false, name),
        "jsonb_build_object" => build_object(values, true, name),
        "json_build_array" => build_array(values, false),
        "jsonb_build_array" => build_array(values, true),
        "jsonb_set" if (3..=4).contains(&values.len()) => {
            let create = flag(values, 3, true)?;
            let new = jsonb_tree(&values[2])?;
            return edited(values, |tree, path| {
                let new = new?;
                set_at(tree, path, 0, &new, create)
            });
        }
        "jsonb_insert" if (3..=4).contains(&values.len()) => {
            let after = flag(values, 3, false)?;
            let new = jsonb_tree(&values[2])?;
            return edited(values, |tree, path| {
                let new = new?;
                insert_at(tree, path, 0, &new, after)
            });
        }
        "jsonb_strip_nulls" | "json_strip_nulls" => {
            let binary = name == "jsonb_strip_nulls";
            let document = document(values.first()?, binary)?;
            document.and_then(|document| {
                let mut tree = document.root.value;
                strip_nulls(&mut tree);
                if binary {
                    jsonb_value(&tree)
                } else {
                    let mut out = String::new();
                    compact(&tree, &mut out)?;
                    Ok(Value::Json(out))
                }
            })
        }
        "jsonb_pretty" => {
            let tree = jsonb_tree(values.first()?)?;
            tree.and_then(|tree| {
                let mut out = String::new();
                pretty(&tree, 0, &mut out)?;
                Ok(Value::Text(out))
            })
        }
        "json_extract_path" => return extracted(values, false, false),
        "jsonb_extract_path" => return extracted(values, true, false),
        "json_extract_path_text" => return extracted(values, false, true),
        "jsonb_extract_path_text" => return extracted(values, true, true),
        _ => return None,
    })
}

fn top_level_contains(tree: &Json, key: &str) -> bool {
    match tree {
        Json::Object(members) => members.iter().any(|(name, _)| name == key),
        Json::Array(items) => items
            .iter()
            .any(|item| matches!(&item.value, Json::String(text) if text == key)),
        Json::String(text) => text == key,
        _ => false,
    }
}

fn no_operator(symbol: &str, left: &Value, right: &Value) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!(
            "operator does not exist: {} {symbol} {}",
            left.type_name(),
            right.type_name()
        ),
    )
    .with_hint(
        "No operator matches the given name and argument types. You might need to add explicit type casts.",
    )
}

fn keys_of(value: &Value) -> SqlResult<Vec<String>> {
    path_of(value)
}

pub(crate) fn has_key(symbol: &str, left: &Value, right: &Value) -> SqlResult<Value> {
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    let Some(tree) = jsonb_tree(left) else {
        return Err(no_operator(symbol, left, right));
    };
    let tree = tree?;
    Ok(Value::Boolean(match symbol {
        "?" => {
            let key = right
                .character_text()
                .ok_or_else(|| no_operator(symbol, left, right))?;
            top_level_contains(&tree, key)
        }
        "?|" => keys_of(right)?
            .iter()
            .any(|key| top_level_contains(&tree, key)),
        _ => keys_of(right)?
            .iter()
            .all(|key| top_level_contains(&tree, key)),
    }))
}

pub(crate) fn delete_path(left: &Value, right: &Value) -> SqlResult<Value> {
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    let Some(tree) = jsonb_tree(left) else {
        return Err(no_operator("#-", left, right));
    };
    let mut tree = tree?;
    let path = path_of(right)?;
    delete_at(&mut tree, &path, 0)?;
    jsonb_value(&tree)
}

pub(crate) fn subtract(left: &Value, right: &Value) -> SqlResult<Value> {
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    let Some(tree) = jsonb_tree(left) else {
        return Err(no_operator("-", left, right));
    };
    let mut tree = tree?;
    if let Some(index) = right.as_integer() {
        match &mut tree {
            Json::Array(items) => {
                let length = items.len() as i64;
                let at = if index < 0 { index + length } else { index };
                if let Some(at) = usize::try_from(at).ok().filter(|at| *at < items.len()) {
                    items.remove(at);
                }
            }
            Json::Object(_) => {
                return Err(invalid("cannot delete from object using integer index"));
            }
            _ => return Err(invalid("cannot delete from scalar")),
        }
        return jsonb_value(&tree);
    }
    let keys = match right {
        Value::Array(_) => path_of(right)?,
        other => vec![
            other
                .character_text()
                .ok_or_else(|| no_operator("-", left, right))?
                .to_owned(),
        ],
    };
    match &mut tree {
        Json::Object(members) => members.retain(|(key, _)| !keys.contains(key)),
        Json::Array(items) => {
            items.retain(|item| !matches!(&item.value, Json::String(text) if keys.contains(text)));
        }
        _ => return Err(invalid("cannot delete from scalar")),
    }
    jsonb_value(&tree)
}

pub(crate) fn concatenated(left: &Value, right: &Value) -> SqlResult<Value> {
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    let (Some(left_tree), Some(right_tree)) = (jsonb_tree(left), jsonb_tree(right)) else {
        return Err(no_operator("||", left, right));
    };
    let (left_tree, right_tree) = (left_tree?, right_tree?);
    let joined = match (left_tree, right_tree) {
        (Json::Object(mut members), Json::Object(more)) => {
            for (key, value) in more {
                members.retain(|(existing, _)| *existing != key);
                members.push((key, value));
            }
            Json::Object(members)
        }
        (Json::Array(mut items), Json::Array(more)) => {
            items.extend(more);
            Json::Array(items)
        }
        (Json::Array(mut items), other) => {
            items.push(leaf(other));
            Json::Array(items)
        }
        (other, Json::Array(mut items)) => {
            items.insert(0, leaf(other));
            Json::Array(items)
        }
        (first, second) => Json::Array(vec![leaf(first), leaf(second)]),
    };
    jsonb_value(&joined)
}

pub(crate) type RecordSink<'s> = dyn FnMut(Vec<Value>) -> SqlResult<()> + 's;

fn call_document(name: &str, values: &[Value]) -> Option<SqlResult<Document>> {
    if values.len() != 1 {
        return None;
    }
    document(values.first()?, name.starts_with("jsonb"))
}

fn rendered(document: &Document, node: &Node, as_text: bool) -> SqlResult<Value> {
    if as_text {
        return Ok(json::render_text(&document.text, node, document.binary)?
            .map_or(Value::Null, Value::Text));
    }
    let text = json::render(&document.text, node, document.binary)?;
    Ok(if document.binary {
        Value::Jsonb(text)
    } else {
        Value::Json(text)
    })
}

fn type_word(value: &Json) -> &'static str {
    match value {
        Json::Object(_) => "an object",
        Json::Array(_) => "an array",
        _ => "a scalar",
    }
}

pub(crate) fn record_rows(name: &str, values: &[Value], sink: &mut RecordSink<'_>) -> SqlResult<()> {
    if values.iter().any(Value::is_null) {
        return Ok(());
    }
    let document = match call_document(name, values) {
        Some(document) => document?,
        None => return Ok(()),
    };
    let as_text = name.ends_with("_text");
    match name {
        "json_each" | "jsonb_each" | "json_each_text" | "jsonb_each_text" => {
            let Json::Object(members) = &document.root.value else {
                return Err(invalid(format!(
                    "cannot call {} on a non-object",
                    name.trim_end_matches("_text")
                )));
            };
            for (key, node) in members {
                sink(vec![Value::Text(key.clone()), rendered(&document, node, as_text)?])?;
            }
        }
        "json_array_elements"
        | "jsonb_array_elements"
        | "json_array_elements_text"
        | "jsonb_array_elements_text" => {
            let Json::Array(items) = &document.root.value else {
                return Err(invalid(format!(
                    "cannot extract elements from {}",
                    type_word(&document.root.value)
                )));
            };
            for node in items {
                sink(vec![rendered(&document, node, as_text)?])?;
            }
        }
        _ => {
            let Json::Object(members) = &document.root.value else {
                return Err(invalid(format!(
                    "cannot call {name} on {}",
                    type_word(&document.root.value)
                )));
            };
            for (key, _) in members {
                sink(vec![Value::Text(key.clone())])?;
            }
        }
    }
    Ok(())
}

fn not_sqlserver_json() -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        "JSON text is not properly formatted.",
    )
    .with_sqlserver_code(13609)
}

/// The rows SQL Server's `OPENJSON` with a `WITH` clause reads from `values[0]`: each element
/// of a JSON array, or a JSON object as one row; NULL gives no rows.
pub(crate) fn openjson_rows(values: &[Value], sink: &mut RecordSink<'_>) -> SqlResult<()> {
    let Some(value) = values.first() else {
        return Ok(());
    };
    if value.is_null() {
        return Ok(());
    }
    let document = match document(value, false) {
        Some(Ok(document)) => document,
        Some(Err(_)) | None => return Err(not_sqlserver_json()),
    };
    match &document.root.value {
        Json::Array(items) => {
            for node in items {
                sink(vec![rendered(&document, node, false)?])?;
            }
        }
        Json::Object(_) => sink(vec![rendered(&document, &document.root, false)?])?,
        _ => return Err(not_sqlserver_json()),
    }
    Ok(())
}

fn openjson_extracted(values: &[Value], as_json: bool) -> Option<SqlResult<Value>> {
    let (first, steps) = values.split_first()?;
    if first.is_null() {
        return Some(Ok(Value::Null));
    }
    let steps: Option<Vec<String>> = steps
        .iter()
        .map(|step| step.character_text().map(str::to_owned))
        .collect();
    let steps = steps?;
    let document = match document(first, false)? {
        Ok(document) => document,
        Err(error) => return Some(Err(error)),
    };
    let mut node = &document.root;
    for step in &steps {
        match json::child(node, &json::Step::Key(step.clone()), false) {
            Some(next) => node = next,
            None => return Some(Ok(Value::Null)),
        }
    }
    let composite = matches!(node.value, Json::Object(_) | Json::Array(_));
    if composite != as_json {
        return Some(Ok(Value::Null));
    }
    Some(
        json::render_text(&document.text, node, false)
            .map(|text| text.map_or(Value::Null, Value::Text)),
    )
}

fn bad_sqlserver_path(path: &str, at: usize) -> SqlError {
    let found = path[at..].chars().next().map_or_else(String::new, String::from);
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!(
            "JSON path is not properly formatted. Unexpected character '{found}' is found at position {at}."
        ),
    )
    .with_sqlserver_code(13607)
}

/// The steps of a SQL Server JSON path (`$`, `$.key`, `$."quoted key"`, `$[2]`, with an
/// optional `lax` mode), as the keys and array indexes `json_extract_path` takes. A `strict`
/// path is refused.
pub(crate) fn sqlserver_path_steps(path: &str) -> SqlResult<Vec<String>> {
    let trimmed = path.trim_start();
    let offset = path.len() - trimmed.len();
    let (rest, offset) = if let Some(rest) = trimmed.strip_prefix("lax ") {
        (rest.trim_start(), path.len() - rest.trim_start().len())
    } else if trimmed.starts_with("strict ") {
        return Err(SqlError::not_supported(
            "OPENJSON with a strict JSON path is not supported yet",
        ));
    } else {
        (trimmed, offset)
    };
    let Some(mut rest) = rest.strip_prefix('$') else {
        return Err(bad_sqlserver_path(path, offset));
    };
    let mut steps = Vec::new();
    loop {
        let at = path.len() - rest.len();
        if rest.is_empty() {
            return Ok(steps);
        }
        if let Some(after) = rest.strip_prefix(".\"") {
            let Some(end) = after.find('"') else {
                return Err(bad_sqlserver_path(path, at));
            };
            steps.push(after[..end].to_owned());
            rest = &after[end + 1..];
        } else if let Some(after) = rest.strip_prefix('.') {
            let end = after
                .find(|character: char| character == '.' || character == '[')
                .unwrap_or(after.len());
            if end == 0 {
                return Err(bad_sqlserver_path(path, at + 1));
            }
            steps.push(after[..end].to_owned());
            rest = &after[end..];
        } else if let Some(after) = rest.strip_prefix('[') {
            let Some(end) = after.find(']') else {
                return Err(bad_sqlserver_path(path, at));
            };
            let index = after[..end].trim();
            if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(bad_sqlserver_path(path, at + 1));
            }
            steps.push(index.to_owned());
            rest = &after[end + 1..];
        } else {
            return Err(bad_sqlserver_path(path, at));
        }
    }
}

pub(crate) fn record_types(name: &str) -> Vec<SqlType> {
    let binary = name.starts_with("jsonb");
    let document_type = if name.ends_with("_text") {
        SqlType::Text
    } else if binary {
        SqlType::Jsonb
    } else {
        SqlType::Json
    };
    if name.contains("_each") {
        vec![SqlType::Text, document_type]
    } else if name.contains("_array_elements") {
        vec![document_type]
    } else {
        vec![SqlType::Text]
    }
}

pub(crate) fn record_names(name: &str) -> &'static [&'static str] {
    if name.contains("_each") {
        &["key", "value"]
    } else if name.contains("_array_elements") {
        &["value"]
    } else {
        &[]
    }
}

fn record_field(value: &Value) -> String {
    let Some(text) = value.to_text() else {
        return String::new();
    };
    let plain = !text.is_empty()
        && !text
            .chars()
            .any(|character| matches!(character, '"' | '\\' | '(' | ')' | ',') || character.is_whitespace());
    if plain {
        return text;
    }
    let mut out = String::from("\"");
    for character in text.chars() {
        if character == '"' || character == '\\' {
            out.push(character);
        }
        out.push(character);
    }
    out.push('"');
    out
}

pub(crate) fn record_text(fields: &[Value]) -> Value {
    match fields {
        [only] => only.clone(),
        _ => Value::Text(format!(
            "({})",
            fields.iter().map(record_field).collect::<Vec<_>>().join(",")
        )),
    }
}

pub(crate) fn whole_row(evaluator: &Evaluator<'_>, arguments: &[Expr]) -> SqlResult<Value> {
    let Some(Expr {
        kind: ExprKind::Column {
            qualifier: Some(label),
            ..
        },
        position,
    }) = arguments.first()
    else {
        return Err(SqlError::new(
            SqlState::INTERNAL_ERROR,
            "a whole-row reference is missing its table",
        ));
    };
    if let Ok(index) = evaluator.scope.column(None, label, *position) {
        return Ok(evaluator.row.get(index).cloned().unwrap_or(Value::Null));
    }
    let Some(source) = evaluator
        .scope
        .sources()
        .iter()
        .find(|source| source.label == label.as_str())
    else {
        return Err(SqlError::new(
            SqlState::UNDEFINED_COLUMN,
            format!("column \"{label}\" does not exist"),
        )
        .at(*position));
    };
    let mut out = String::from("{");
    for (index, column) in source.table.columns.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        json::write_string(crate::temp_tables::display_name(&column.name), &mut out);
        out.push(':');
        let value = evaluator
            .row
            .get(source.offset + index)
            .cloned()
            .unwrap_or(Value::Null);
        out.push_str(&to_json_text(&value)?);
        crate::memory::check_value_bytes(out.len())?;
    }
    out.push('}');
    Ok(Value::Json(out))
}

pub(crate) fn row_object(evaluator: &Evaluator<'_>, arguments: &[Expr]) -> SqlResult<Value> {
    let mut out = String::from("{");
    for (index, argument) in arguments.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let value = evaluator.eval(argument, None)?;
        json::write_string(&format!("f{}", index + 1), &mut out);
        out.push(':');
        out.push_str(&to_json_text(&value)?);
        crate::memory::check_value_bytes(out.len())?;
    }
    out.push('}');
    Ok(Value::Json(out))
}

pub(crate) fn is_whole_row(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Function { name, .. } if name == WHOLE_ROW || name == ROW_OBJECT)
}

fn labels(from: &FromClause) -> Vec<String> {
    std::iter::once(&from.first)
        .chain(from.joins.iter().map(|join| &join.table))
        .map(|reference| {
            reference.alias.clone().unwrap_or_else(|| {
                let name = &reference.name.text;
                name.rsplit('.').next().unwrap_or(name).to_owned()
            })
        })
        .collect()
}

fn marked(expr: &mut Expr, labels: &[String]) {
    let caller = match &expr.kind {
        ExprKind::Function { name, .. } => WHOLE_ROW_CALLERS.contains(&name.as_str()),
        ExprKind::Aggregate { function, .. } => {
            matches!(function.name(), "json_agg" | "jsonb_agg")
        }
        _ => false,
    };
    if caller {
        let arguments: Vec<&mut Expr> = match &mut expr.kind {
            ExprKind::Function { arguments, .. } => arguments.iter_mut().collect(),
            ExprKind::Aggregate {
                argument: Some(argument),
                ..
            } => vec![argument.as_mut()],
            _ => Vec::new(),
        };
        for argument in arguments {
            let label = match &argument.kind {
                ExprKind::Column {
                    qualifier: None,
                    name,
                } if labels.contains(name) => name.clone(),
                _ => continue,
            };
            let position = argument.position;
            *argument = Expr {
                kind: ExprKind::Function {
                    name: WHOLE_ROW.to_owned(),
                    arguments: vec![Expr {
                        kind: ExprKind::Column {
                            qualifier: Some(label),
                            name: "*".to_owned(),
                        },
                        position,
                    }],
                },
                position,
            };
        }
    }
    for child in crate::eval::children_mut(expr) {
        marked(child, labels);
    }
}

pub(crate) fn with_whole_rows(mut items: Vec<SelectItem>, from: Option<&FromClause>) -> Vec<SelectItem> {
    let Some(from) = from else {
        return items;
    };
    let labels = labels(from);
    for item in &mut items {
        if let SelectItem::Expr { expr, .. } = item {
            marked(expr, &labels);
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jsonb(text: &str) -> Value {
        Value::Jsonb(json::normalize(text).unwrap())
    }

    fn shown(result: SqlResult<Value>) -> String {
        match result {
            Ok(value) => value.to_text().unwrap_or_else(|| "NULL".to_owned()),
            Err(error) => format!("error {}", error.state().code()),
        }
    }

    #[test]
    fn builders_write_postgresqls_text() {
        let values = [
            Value::Text("a".into()),
            Value::Integer(1),
            Value::Text("b".into()),
            Value::Text("x\"y".into()),
        ];
        assert_eq!(
            shown(function("json_build_object", &values).unwrap()),
            "{\"a\" : 1, \"b\" : \"x\\\"y\"}"
        );
        assert_eq!(
            shown(function("jsonb_build_object", &values).unwrap()),
            "{\"a\": 1, \"b\": \"x\\\"y\"}"
        );
        assert_eq!(
            shown(function("json_build_array", &[Value::Integer(1), Value::Null, Value::Boolean(true)]).unwrap()),
            "[1, null, true]"
        );
        assert_eq!(shown(function("json_build_object", &[Value::Integer(1)]).unwrap()), "error 22023");
        assert_eq!(
            shown(function("to_json", &[Value::Timestamp(0)]).unwrap()),
            "\"2000-01-01T00:00:00\""
        );
    }

    #[test]
    fn processors_follow_the_manuals_examples() {
        let set = function(
            "jsonb_set",
            &[
                jsonb("[{\"f1\":1,\"f2\":null},2,null,3]"),
                Value::Text("{0,f1}".into()),
                jsonb("[2,3,4]"),
                Value::Boolean(false),
            ],
        )
        .unwrap();
        assert_eq!(shown(set), "[{\"f1\": [2, 3, 4], \"f2\": null}, 2, null, 3]");
        let set = function(
            "jsonb_set",
            &[
                jsonb("[{\"f1\":1,\"f2\":null},2]"),
                Value::Text("{0,f3}".into()),
                jsonb("[2,3,4]"),
            ],
        )
        .unwrap();
        assert_eq!(shown(set), "[{\"f1\": 1, \"f2\": null, \"f3\": [2, 3, 4]}, 2]");
        let inserted = function(
            "jsonb_insert",
            &[
                jsonb("{\"a\": [0,1,2]}"),
                Value::Text("{a, 1}".into()),
                jsonb("\"new_value\""),
            ],
        )
        .unwrap();
        assert_eq!(shown(inserted), "{\"a\": [0, \"new_value\", 1, 2]}");
        let inserted = function(
            "jsonb_insert",
            &[
                jsonb("{\"a\": [0,1,2]}"),
                Value::Text("{a, 1}".into()),
                jsonb("\"new_value\""),
                Value::Boolean(true),
            ],
        )
        .unwrap();
        assert_eq!(shown(inserted), "{\"a\": [0, 1, \"new_value\", 2]}");
        let stripped = function(
            "json_strip_nulls",
            &[Value::Json("[{\"f1\":1, \"f2\":null}, 2, null, 3]".into())],
        )
        .unwrap();
        assert_eq!(shown(stripped), "[{\"f1\":1},2,null,3]");
        let pretty_text = function("jsonb_pretty", &[jsonb("[{\"f1\":1,\"f2\":null}, 2]")]).unwrap();
        assert_eq!(
            shown(pretty_text),
            "[\n    {\n        \"f1\": 1,\n        \"f2\": null\n    },\n    2\n]"
        );
    }

    #[test]
    fn operators_follow_the_manuals_examples() {
        assert_eq!(
            shown(concatenated(&jsonb("[\"a\", \"b\"]"), &jsonb("[\"a\", \"d\"]"))),
            "[\"a\", \"b\", \"a\", \"d\"]"
        );
        assert_eq!(
            shown(concatenated(&jsonb("{\"a\": \"b\"}"), &jsonb("{\"c\": \"d\"}"))),
            "{\"a\": \"b\", \"c\": \"d\"}"
        );
        assert_eq!(shown(concatenated(&jsonb("[1, 2]"), &jsonb("3"))), "[1, 2, 3]");
        assert_eq!(shown(concatenated(&jsonb("1"), &jsonb("[2, 3]"))), "[1, 2, 3]");
        assert_eq!(shown(concatenated(&jsonb("{\"a\": 1}"), &jsonb("2"))), "[{\"a\": 1}, 2]");
        assert_eq!(
            shown(subtract(&jsonb("{\"a\": \"b\", \"c\": \"d\"}"), &Value::Text("a".into()))),
            "{\"c\": \"d\"}"
        );
        assert_eq!(
            shown(subtract(&jsonb("[\"a\", \"b\", \"c\", \"b\"]"), &Value::Text("b".into()))),
            "[\"a\", \"c\"]"
        );
        assert_eq!(shown(subtract(&jsonb("[\"a\", \"b\"]"), &Value::Integer(1))), "[\"a\"]");
        assert_eq!(shown(subtract(&jsonb("1"), &Value::Text("a".into()))), "error 22023");
        assert_eq!(
            shown(delete_path(&jsonb("[\"a\", {\"b\":1}]"), &Value::Text("{1,b}".into()))),
            "[\"a\", {}]"
        );
        assert_eq!(
            shown(has_key("?", &jsonb("{\"a\":1, \"b\":2}"), &Value::Text("b".into()))),
            "t"
        );
        assert_eq!(
            shown(has_key("?", &jsonb("[\"a\", \"b\", \"c\"]"), &Value::Text("b".into()))),
            "t"
        );
        assert_eq!(
            shown(has_key("?|", &jsonb("{\"a\":1, \"b\":2, \"c\":3}"), &Value::Text("{b,d}".into()))),
            "t"
        );
        assert_eq!(
            shown(has_key("?&", &jsonb("[\"a\", \"b\", \"c\"]"), &Value::Text("{a,d}".into()))),
            "f"
        );
    }

    #[test]
    fn set_returning_functions_split_documents() {
        let mut rows = Vec::new();
        record_rows(
            "json_each",
            &[Value::Json("{\"a\":\"foo\", \"b\":\"bar\"}".into())],
            &mut |record| {
                rows.push(record_text(&record).to_text().unwrap_or_default());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(rows, ["(a,\"\"\"foo\"\"\")", "(b,\"\"\"bar\"\"\")"]);
        let mut texts = Vec::new();
        record_rows(
            "jsonb_array_elements_text",
            &[jsonb("[\"foo\", \"bar\", null]")],
            &mut |record| {
                texts.push(record_text(&record).to_text().unwrap_or_else(|| "NULL".into()));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(texts, ["foo", "bar", "NULL"]);
        let refused = record_rows("json_each", &[Value::Json("[1]".into())], &mut |_| Ok(()));
        assert_eq!(refused.unwrap_err().state().code(), "22023");
    }

    #[test]
    fn seeded_hostile_documents_and_paths_never_panic() {
        let alphabet: Vec<char> = "{}[]\":,0123-abnulltrue ".chars().collect();
        let mut seed = 0x0050_4A50_u64;
        let mut next = |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound
        };
        for _ in 0..3_000 {
            let document: String = (0..next(24))
                .map(|_| alphabet[next(alphabet.len() as u64) as usize])
                .collect();
            let path: String = (0..next(8))
                .map(|_| alphabet[next(alphabet.len() as u64) as usize])
                .collect();
            let document = Value::Text(document);
            let path = Value::Text(format!("{{{path}}}"));
            let _ = function("jsonb_set", &[document.clone(), path.clone(), Value::Text("1".into())]);
            let _ = function("jsonb_insert", &[document.clone(), path.clone(), Value::Text("1".into())]);
            let _ = function("jsonb_pretty", &[document.clone()]);
            let _ = delete_path(&document, &path);
            let _ = subtract(&document, &Value::Integer(-1));
            let _ = concatenated(&document, &document);
            let _ = has_key("?|", &document, &path);
            let _ = record_rows("json_each", &[document.clone()], &mut |_| Ok(()));
            let _ = record_rows("jsonb_array_elements_text", &[document], &mut |_| Ok(()));
        }
    }
}
