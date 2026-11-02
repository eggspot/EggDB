//! The operators and functions of PostgreSQL's `hstore` extension over EggDB's built-in
//! `hstore` type (D259): `->`, `?`, `?|`, `?&`, `@>`, `<@`, `||` and `-`, and
//! `akeys`, `avals`, `skeys`, `svals`, `each`, `exist`, `defined`, `delete`, `slice`, `hstore`,
//! `hstore_to_json`, `hstore_to_jsonb` and `hstore_to_array`.

use crate::arrays::{ArrayElement, ArrayValue};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::hstore::{HstoreValue, hstore_of};
use crate::json_functions::RecordSink;
use crate::set_returning::RowSink;
use crate::types::{DefinedType, SqlType, Value};

pub(crate) const EACH: &str = "each";
pub(crate) const EACH_COLUMNS: [&str; 2] = ["key", "value"];

const HSTORE: Option<SqlType> = Some(SqlType::Defined(DefinedType::HSTORE));
const TEXT: Option<SqlType> = Some(SqlType::Text);
const TEXT_ARRAY: Option<SqlType> = Some(SqlType::Array(ArrayElement::Text));

pub(crate) fn is_hstore_type(sql_type: Option<SqlType>) -> bool {
    sql_type == HSTORE
}

pub(crate) fn argument_types(name: &str) -> Option<&'static [Option<SqlType>]> {
    Some(match name {
        "akeys" | "avals" | "skeys" | "svals" | "hstore_to_json" | "hstore_to_jsonb"
        | "hstore_to_array" => &[HSTORE],
        "exist" | "defined" => &[HSTORE, TEXT],
        "slice" => &[HSTORE, TEXT_ARRAY],
        "delete" => &[HSTORE, None],
        _ => return None,
    })
}

pub(crate) fn result_type(name: &str, argument_count: usize) -> Option<SqlType> {
    match (name, argument_count) {
        ("akeys" | "avals" | "hstore_to_array", 1) => TEXT_ARRAY,
        ("exist" | "defined", 2) => Some(SqlType::Boolean),
        ("delete" | "slice", 2) | ("hstore", 1 | 2) => HSTORE,
        ("hstore_to_json", 1) => Some(SqlType::Json),
        ("hstore_to_jsonb", 1) => Some(SqlType::Jsonb),
        _ => None,
    }
}

pub(crate) fn fetch_type(selector: Option<SqlType>) -> SqlType {
    match selector {
        Some(SqlType::Array(_)) => SqlType::Array(ArrayElement::Text),
        _ => SqlType::Text,
    }
}

fn no_function(name: &str, values: &[Value]) -> SqlError {
    let types: Vec<String> = values.iter().map(Value::type_name).collect();
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("function {name}({}) does not exist", types.join(", ")),
    )
    .with_hint(
        "No function matches the given name and argument types. You might need to add explicit type casts.",
    )
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

fn null_key() -> SqlError {
    SqlError::new(
        SqlState::NULL_VALUE_NOT_ALLOWED,
        "null value not allowed for hstore key",
    )
}

fn hstore_argument(value: &Value) -> Option<SqlResult<HstoreValue>> {
    if let Some(hstore) = hstore_of(value) {
        return Some(Ok(hstore.clone()));
    }
    value.character_text().map(crate::hstore::parse_text)
}

fn text_items(value: &Value) -> Option<SqlResult<Vec<Option<String>>>> {
    match value {
        Value::Array(array) => Some(Ok(array.items.iter().map(Value::to_text).collect())),
        other => other.character_text().map(|text| {
            crate::arrays::parse_text(text, ArrayElement::Text)
                .map(|array| array.items.iter().map(Value::to_text).collect())
        }),
    }
}

fn keys_of(value: &Value) -> Option<SqlResult<Vec<String>>> {
    text_items(value).map(|items| items.map(|items| items.into_iter().flatten().collect()))
}

fn text_array(items: impl IntoIterator<Item = Option<String>>) -> SqlResult<Value> {
    let items: Vec<Value> = items
        .into_iter()
        .map(|item| item.map_or(Value::Null, Value::Text))
        .collect();
    let array = ArrayValue::new(ArrayElement::Text, items);
    crate::memory::check_value_bytes(crate::arrays::held_bytes(&array))?;
    Ok(Value::Array(Box::new(array)))
}

fn hstore_result(pairs: Vec<(String, Option<String>)>) -> SqlResult<Value> {
    let hstore = crate::hstore::checked(pairs)?;
    crate::memory::check_value_bytes(hstore.held_bytes())?;
    Ok(crate::hstore::value(hstore))
}

fn owned_pairs(hstore: &HstoreValue) -> impl Iterator<Item = (String, Option<String>)> + '_ {
    hstore.pairs().iter().cloned()
}

pub(crate) fn fetch(document: &Value, selector: &Value) -> SqlResult<Value> {
    let Some(hstore) = hstore_of(document) else {
        return Err(no_operator("->", document, selector));
    };
    if selector.is_null() {
        return Ok(Value::Null);
    }
    if let Value::Array(array) = selector {
        return text_array(array.items.iter().map(|key| {
            key.to_text()
                .and_then(|key| hstore.get(&key).flatten().map(str::to_owned))
        }));
    }
    let Some(key) = selector.character_text() else {
        return Err(no_operator("->", document, selector));
    };
    Ok(hstore
        .get(key)
        .flatten()
        .map_or(Value::Null, |found| Value::Text(found.to_owned())))
}

pub(crate) fn has_key(symbol: &str, document: &Value, keys: &Value) -> SqlResult<Value> {
    let Some(hstore) = hstore_of(document) else {
        return Err(no_operator(symbol, document, keys));
    };
    if keys.is_null() {
        return Ok(Value::Null);
    }
    if symbol == "?" {
        let key = keys
            .character_text()
            .ok_or_else(|| no_operator(symbol, document, keys))?;
        return Ok(Value::Boolean(hstore.get(key).is_some()));
    }
    let wanted = keys_of(keys).ok_or_else(|| no_operator(symbol, document, keys))??;
    let present = |key: &String| hstore.get(key).is_some();
    Ok(Value::Boolean(if symbol == "?|" {
        wanted.iter().any(present)
    } else {
        wanted.iter().all(present)
    }))
}

fn contains(container: &HstoreValue, contained: &HstoreValue) -> bool {
    contained
        .pairs()
        .iter()
        .all(|(key, value)| container.get(key) == Some(value.as_deref()))
}

pub(crate) fn containment(
    contains_operator: bool,
    left: &Value,
    right: &Value,
) -> Option<SqlResult<Value>> {
    if hstore_of(left).is_none() && hstore_of(right).is_none() {
        return None;
    }
    if left.is_null() || right.is_null() {
        return Some(Ok(Value::Null));
    }
    let symbol = if contains_operator { "@>" } else { "<@" };
    let operands = hstore_argument(left).zip(hstore_argument(right));
    let Some((left_hstore, right_hstore)) = operands else {
        return Some(Err(no_operator(symbol, left, right)));
    };
    Some(left_hstore.and_then(|left_hstore| {
        let right_hstore = right_hstore?;
        Ok(Value::Boolean(if contains_operator {
            contains(&left_hstore, &right_hstore)
        } else {
            contains(&right_hstore, &left_hstore)
        }))
    }))
}

fn concatenation_operand(value: &Value, unknown: bool) -> Option<Option<&Value>> {
    match value {
        Value::Null => Some(None),
        Value::Text(_) if unknown => Some(Some(value)),
        other => hstore_of(other).map(|_| Some(other)),
    }
}

pub(crate) fn concatenated(
    (left, left_unknown): (&Value, bool),
    (right, right_unknown): (&Value, bool),
) -> Option<SqlResult<Value>> {
    if hstore_of(left).is_none() && hstore_of(right).is_none() {
        return None;
    }
    let left = concatenation_operand(left, left_unknown)?;
    let right = concatenation_operand(right, right_unknown)?;
    let (Some(left), Some(right)) = (left, right) else {
        return Some(Ok(Value::Null));
    };
    Some(concatenate(left, right))
}

fn concatenate(left: &Value, right: &Value) -> SqlResult<Value> {
    let parsed = |value: &Value| {
        hstore_argument(value).unwrap_or_else(|| Err(no_operator("||", left, right)))
    };
    let (left_hstore, right_hstore) = (parsed(left)?, parsed(right)?);
    let pairs = owned_pairs(&right_hstore)
        .chain(owned_pairs(&left_hstore))
        .collect();
    hstore_result(pairs)
}

fn without_keys(hstore: &HstoreValue, keys: &[String]) -> SqlResult<Value> {
    hstore_result(
        owned_pairs(hstore)
            .filter(|(key, _)| !keys.contains(key))
            .collect(),
    )
}

fn deleted(name: &str, document: &Value, removed: &Value) -> SqlResult<Value> {
    let Some(hstore) = hstore_of(document) else {
        return Err(no_operator(name, document, removed));
    };
    if removed.is_null() {
        return Ok(Value::Null);
    }
    if let Some(other) = hstore_of(removed) {
        return hstore_result(
            owned_pairs(hstore)
                .filter(|(key, value)| other.get(key) != Some(value.as_deref()))
                .collect(),
        );
    }
    if let Some(key) = removed.character_text() {
        return without_keys(hstore, &[key.to_owned()]);
    }
    if matches!(removed, Value::Array(_)) {
        let keys = keys_of(removed).ok_or_else(|| no_operator(name, document, removed))??;
        return without_keys(hstore, &keys);
    }
    Err(no_operator(name, document, removed))
}

pub(crate) fn subtract(document: &Value, removed: &Value) -> SqlResult<Value> {
    deleted("-", document, removed)
}

fn sliced(hstore: &HstoreValue, keys: &Value) -> SqlResult<Value> {
    let keys = keys_of(keys).ok_or_else(|| no_function("slice", std::slice::from_ref(keys)))??;
    hstore_result(
        owned_pairs(hstore)
            .filter(|(key, _)| keys.contains(key))
            .collect(),
    )
}

fn from_key_and_value(key: &Value, value: &Value) -> Option<SqlResult<Value>> {
    let value = match value {
        Value::Null => None,
        other => Some(other.character_text()?.to_owned()),
    };
    match key {
        Value::Null => Some(Ok(Value::Null)),
        other => {
            let key = other.character_text()?.to_owned();
            Some(hstore_result(vec![(key, value)]))
        }
    }
}

fn from_arrays(keys: &Value, values: &Value) -> SqlResult<Value> {
    let Some(keys) = text_items(keys).transpose()? else {
        return Err(no_function("hstore", &[keys.clone(), values.clone()]));
    };
    let values = match values {
        Value::Null => vec![None; keys.len()],
        other => text_items(other)
            .ok_or_else(|| no_function("hstore", std::slice::from_ref(other)))??,
    };
    if values.len() != keys.len() {
        return Err(SqlError::new(
            SqlState::ARRAY_SUBSCRIPT_ERROR,
            "arrays must have same bounds",
        ));
    }
    let mut pairs = Vec::with_capacity(keys.len());
    for (key, value) in keys.into_iter().zip(values) {
        pairs.push((key.ok_or_else(null_key)?, value));
    }
    hstore_result(pairs)
}

fn from_alternating(items: Vec<Option<String>>) -> SqlResult<Value> {
    if !items.len().is_multiple_of(2) {
        return Err(SqlError::new(
            SqlState::ARRAY_SUBSCRIPT_ERROR,
            "array must have even number of elements",
        ));
    }
    let mut pairs = Vec::with_capacity(items.len() / 2);
    let mut items = items.into_iter();
    while let (Some(key), Some(value)) = (items.next(), items.next()) {
        pairs.push((key.ok_or_else(null_key)?, value));
    }
    hstore_result(pairs)
}

fn constructed(values: &[Value]) -> Option<SqlResult<Value>> {
    match values {
        [Value::Null] | [Value::Null, Value::Null] => Some(Ok(Value::Null)),
        [Value::Array(array)] => Some(from_alternating(
            array.items.iter().map(Value::to_text).collect(),
        )),
        [single] => hstore_argument(single).map(|parsed| parsed.map(crate::hstore::value)),
        [keys @ Value::Array(_), paired] => Some(from_arrays(keys, paired)),
        [key, value] => from_key_and_value(key, value),
        _ => None,
    }
}

fn first_hstore(values: &[Value]) -> Option<&HstoreValue> {
    values.first().and_then(hstore_of)
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if name == "hstore" {
        return constructed(values);
    }
    argument_types(name)?;
    if values.first().is_some_and(Value::is_null) || values.get(1).is_some_and(Value::is_null) {
        return result_type(name, values.len()).map(|_| Ok(Value::Null));
    }
    let hstore = first_hstore(values)?;
    Some(match (name, values) {
        ("akeys", [_]) => text_array(hstore.pairs().iter().map(|(key, _)| Some(key.clone()))),
        ("avals", [_]) => text_array(hstore.pairs().iter().map(|(_, value)| value.clone())),
        ("hstore_to_array", [_]) => text_array(
            hstore
                .pairs()
                .iter()
                .flat_map(|(key, value)| [Some(key.clone()), value.clone()]),
        ),
        ("hstore_to_json", [_]) => Ok(Value::Json(crate::hstore::to_json(hstore))),
        ("hstore_to_jsonb", [_]) => {
            crate::json::normalize(&crate::hstore::to_json(hstore)).map(Value::Jsonb)
        }
        ("exist", [_, key]) => key
            .character_text()
            .map(|key| Value::Boolean(hstore.get(key).is_some()))
            .ok_or_else(|| no_function(name, values)),
        ("defined", [_, key]) => key
            .character_text()
            .map(|key| Value::Boolean(hstore.get(key).flatten().is_some()))
            .ok_or_else(|| no_function(name, values)),
        ("delete", [document, removed]) => deleted(name, document, removed),
        ("slice", [_, keys]) => sliced(hstore, keys),
        _ => Err(no_function(name, values)),
    })
}

fn set_argument(name: &str, values: &[Value]) -> SqlResult<Option<HstoreValue>> {
    match values {
        [Value::Null] => Ok(None),
        [single] => hstore_argument(single)
            .ok_or_else(|| no_function(name, values))?
            .map(Some),
        _ => Err(no_function(name, values)),
    }
}

pub(crate) fn set_result_type(name: &str, types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    match types {
        [None] => Ok(SqlType::Text),
        [Some(sql_type)] if is_hstore_type(Some(*sql_type)) || sql_type.is_character() => {
            Ok(SqlType::Text)
        }
        _ => Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("function {name}() does not exist for these argument types"),
        )),
    }
}

pub(crate) fn key_rows(values: &[Value], sink: &mut RowSink<'_>) -> SqlResult<()> {
    let Some(hstore) = set_argument("skeys", values)? else {
        return Ok(());
    };
    for (key, _) in hstore.pairs() {
        sink(Value::Text(key.clone()))?;
    }
    Ok(())
}

pub(crate) fn value_rows(values: &[Value], sink: &mut RowSink<'_>) -> SqlResult<()> {
    let Some(hstore) = set_argument("svals", values)? else {
        return Ok(());
    };
    for (_, value) in hstore.pairs() {
        sink(value.clone().map_or(Value::Null, Value::Text))?;
    }
    Ok(())
}

pub(crate) fn each_records(values: &[Value], sink: &mut RecordSink<'_>) -> SqlResult<()> {
    let Some(hstore) = set_argument(EACH, values)? else {
        return Ok(());
    };
    for (key, value) in hstore.pairs() {
        sink(vec![
            Value::Text(key.clone()),
            value.clone().map_or(Value::Null, Value::Text),
        ])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hstore(text: &str) -> Value {
        crate::hstore::value(crate::hstore::parse_text(text).unwrap())
    }

    fn text(value: &str) -> Value {
        Value::Text(value.to_owned())
    }

    fn texts(items: &[Option<&str>]) -> Value {
        Value::Array(Box::new(ArrayValue::new(
            ArrayElement::Text,
            items
                .iter()
                .map(|item| item.map_or(Value::Null, |item| Value::Text(item.to_owned())))
                .collect(),
        )))
    }

    fn shown(value: SqlResult<Value>) -> String {
        value
            .unwrap()
            .to_text()
            .unwrap_or_else(|| "NULL".to_owned())
    }

    #[test]
    fn fetching_a_key_or_keys_returns_their_values() {
        let h = hstore("a=>1, b=>NULL");
        assert_eq!(shown(fetch(&h, &text("a"))), "1");
        assert_eq!(shown(fetch(&h, &text("b"))), "NULL");
        assert_eq!(shown(fetch(&h, &text("z"))), "NULL");
        assert_eq!(
            shown(fetch(&h, &texts(&[Some("b"), Some("a"), None]))),
            "{NULL,1,NULL}"
        );
        assert_eq!(
            fetch(&text("x"), &text("a")).unwrap_err().state().code(),
            "42883"
        );
    }

    #[test]
    fn key_tests_follow_postgresql() {
        let h = hstore("a=>1, b=>NULL");
        assert_eq!(shown(has_key("?", &h, &text("b"))), "t");
        assert_eq!(shown(has_key("?", &h, &text("c"))), "f");
        assert_eq!(shown(has_key("?|", &h, &text("{c,a}"))), "t");
        assert_eq!(
            shown(has_key("?&", &h, &texts(&[Some("a"), Some("c")]))),
            "f"
        );
        assert_eq!(
            shown(has_key("?&", &h, &texts(&[Some("a"), Some("b")]))),
            "t"
        );
        assert_eq!(shown(has_key("?", &h, &Value::Null)), "NULL");
    }

    #[test]
    fn containment_compares_pairs_and_nulls() {
        let h = hstore("a=>1, b=>NULL, c=>3");
        let check = |contains: bool, left: &Value, right: &Value| {
            shown(containment(contains, left, right).unwrap())
        };
        assert_eq!(check(true, &h, &hstore("a=>1, b=>NULL")), "t");
        assert_eq!(check(true, &h, &hstore("a=>2")), "f");
        assert_eq!(check(true, &h, &text("c=>3")), "t");
        assert_eq!(check(false, &hstore("c=>3"), &h), "t");
        assert_eq!(check(false, &h, &hstore("c=>3")), "f");
        assert!(containment(true, &text("a"), &text("b")).is_none());
    }

    #[test]
    fn concatenation_lets_the_right_side_win_and_subtraction_removes() {
        let left = hstore("a=>1, b=>2");
        let joined = concatenated((&left, false), (&hstore("b=>3, c=>NULL"), false)).unwrap();
        assert_eq!(shown(joined), "\"a\"=>\"1\", \"b\"=>\"3\", \"c\"=>NULL");
        let literal = concatenated((&left, false), (&text("z=>9"), true)).unwrap();
        assert_eq!(shown(literal), "\"a\"=>\"1\", \"b\"=>\"2\", \"z\"=>\"9\"");
        assert!(concatenated((&left, false), (&text("z"), false)).is_none());
        assert_eq!(shown(subtract(&left, &text("a"))), "\"b\"=>\"2\"");
        assert_eq!(shown(subtract(&left, &texts(&[Some("a"), Some("b")]))), "");
        assert_eq!(
            shown(subtract(&left, &hstore("a=>1, b=>9"))),
            "\"b\"=>\"2\""
        );
        assert_eq!(
            subtract(&left, &Value::Integer(1))
                .unwrap_err()
                .state()
                .code(),
            "42883"
        );
    }

    #[test]
    fn functions_list_test_cut_and_build() {
        let h = hstore("a=>1, b=>NULL");
        let call = |name: &str, values: &[Value]| shown(function(name, values).unwrap());
        assert_eq!(call("akeys", std::slice::from_ref(&h)), "{a,b}");
        assert_eq!(call("avals", std::slice::from_ref(&h)), "{1,NULL}");
        assert_eq!(
            call("hstore_to_array", std::slice::from_ref(&h)),
            "{a,1,b,NULL}"
        );
        assert_eq!(
            call("hstore_to_json", std::slice::from_ref(&h)),
            "{\"a\": \"1\", \"b\": null}"
        );
        assert_eq!(call("exist", &[h.clone(), text("b")]), "t");
        assert_eq!(call("defined", &[h.clone(), text("b")]), "f");
        assert_eq!(call("delete", &[h.clone(), text("a")]), "\"b\"=>NULL");
        assert_eq!(
            call("slice", &[h.clone(), texts(&[Some("a"), Some("x")])]),
            "\"a\"=>\"1\""
        );
        assert_eq!(call("hstore", &[text("k"), text("v")]), "\"k\"=>\"v\"");
        assert_eq!(call("hstore", &[text("k"), Value::Null]), "\"k\"=>NULL");
        assert_eq!(call("hstore", &[Value::Null, text("v")]), "NULL");
        assert_eq!(
            call(
                "hstore",
                &[texts(&[Some("a"), Some("b")]), texts(&[Some("1"), None])]
            ),
            "\"a\"=>\"1\", \"b\"=>NULL"
        );
        assert_eq!(
            call("hstore", &[texts(&[Some("a"), Some("1")])]),
            "\"a\"=>\"1\""
        );
        assert_eq!(call("akeys", &[Value::Null]), "NULL");
        let state = |name: &str, values: &[Value]| {
            function(name, values)
                .unwrap()
                .unwrap_err()
                .state()
                .code()
                .to_owned()
        };
        assert_eq!(state("hstore", &[texts(&[Some("a")]), texts(&[])]), "2202E");
        assert_eq!(state("hstore", &[texts(&[Some("a")])]), "2202E");
        assert_eq!(
            state("hstore", &[texts(&[None]), texts(&[Some("1")])]),
            "22004"
        );
        let long_key = "k".repeat(crate::hstore::MAX_KEY_BYTES + 1);
        assert_eq!(state("hstore", &[text(&long_key), text("v")]), "54000");
        assert!(function("akeys", &[Value::Integer(1)]).is_none());
        assert!(function("not_hstore", &[h]).is_none());
    }

    #[test]
    fn set_returning_functions_yield_keys_values_and_pairs() {
        let h = hstore("a=>1, b=>NULL");
        let mut keys = Vec::new();
        key_rows(std::slice::from_ref(&h), &mut |value| {
            keys.push(value);
            Ok(())
        })
        .unwrap();
        assert_eq!(keys, [text("a"), text("b")]);
        let mut values = Vec::new();
        value_rows(&[text("x=>NULL")], &mut |value| {
            values.push(value);
            Ok(())
        })
        .unwrap();
        assert_eq!(values, [Value::Null]);
        let mut records = Vec::new();
        each_records(&[h], &mut |record| {
            records.push(record);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            records,
            [vec![text("a"), text("1")], vec![text("b"), Value::Null]]
        );
        assert!(each_records(&[Value::Integer(1)], &mut |_| Ok(())).is_err());
        assert!(each_records(&[text("a b")], &mut |_| Ok(())).is_err());
    }
}
