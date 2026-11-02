//! Oracle's collection operators in SQL (`CARDINALITY`, `MEMBER OF`, `SUBMULTISET OF` and the
//! `MULTISET` set operators) over PL/SQL nested tables and varrays.

use crate::error::{SqlError, SqlResult, SqlState};
use crate::records::{RECORD_OID, RecordValue, record_value};
use crate::types::{DefinedValue, Value, compare_values};
use std::cmp::Ordering;

pub(crate) const MEMBER_OF: &str = "oracle_member_of";
pub(crate) const SUBMULTISET_OF: &str = "oracle_submultiset_of";
pub(crate) const MULTISET_UNION_ALL: &str = "oracle_multiset_union_all";
pub(crate) const MULTISET_UNION_DISTINCT: &str = "oracle_multiset_union_distinct";
pub(crate) const MULTISET_INTERSECT_ALL: &str = "oracle_multiset_intersect_all";
pub(crate) const MULTISET_INTERSECT_DISTINCT: &str = "oracle_multiset_intersect_distinct";
pub(crate) const MULTISET_EXCEPT_ALL: &str = "oracle_multiset_except_all";
pub(crate) const MULTISET_EXCEPT_DISTINCT: &str = "oracle_multiset_except_distinct";

pub(crate) fn is_operator(name: &str) -> bool {
    [
        MEMBER_OF,
        SUBMULTISET_OF,
        MULTISET_UNION_ALL,
        MULTISET_UNION_DISTINCT,
        MULTISET_INTERSECT_ALL,
        MULTISET_INTERSECT_DISTINCT,
        MULTISET_EXCEPT_ALL,
        MULTISET_EXCEPT_DISTINCT,
    ]
    .contains(&name)
}

pub(crate) fn elements(value: &Value) -> Option<&[Value]> {
    let Value::Defined(defined) = value else {
        return None;
    };
    let DefinedValue::Record(record) = defined.as_ref() else {
        return None;
    };
    is_collection(record).then_some(record.fields.as_slice())
}

fn is_collection(record: &RecordValue) -> bool {
    record.names.len() == record.fields.len()
        && record
            .names
            .iter()
            .all(|name| name.parse::<u32>().is_ok())
}

pub(crate) fn cardinality(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if name != "cardinality" {
        return None;
    }
    let [value] = values else {
        return None;
    };
    let found = elements(value)?;
    let count = i64::try_from(found.len()).unwrap_or(i64::MAX);
    Some(Ok(if found.is_empty() {
        Value::Null
    } else {
        Value::BigInt(count)
    }))
}

pub(crate) fn call(name: &str, left: &Value, right: &Value) -> SqlResult<Value> {
    if name == MEMBER_OF {
        return member_of(left, right);
    }
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    let (Some(left), Some(right)) = (elements(left), elements(right)) else {
        return Err(not_a_collection(name));
    };
    match name {
        SUBMULTISET_OF => Ok(submultiset_of(left, right)),
        MULTISET_UNION_ALL => Ok(collection(left.iter().chain(right).cloned().collect())),
        MULTISET_UNION_DISTINCT => Ok(collection(distinct(
            left.iter().chain(right).cloned().collect(),
        ))),
        MULTISET_INTERSECT_ALL => Ok(collection(intersection(left, right))),
        MULTISET_INTERSECT_DISTINCT => Ok(collection(distinct(intersection(left, right)))),
        MULTISET_EXCEPT_ALL => Ok(collection(difference(left, right))),
        MULTISET_EXCEPT_DISTINCT => Ok(collection(
            distinct(left.to_vec())
                .into_iter()
                .filter(|element| !right.iter().any(|other| equal_or_both_null(other, element)))
                .collect(),
        )),
        _ => Err(not_a_collection(name)),
    }
}

fn member_of(element: &Value, collection: &Value) -> SqlResult<Value> {
    if collection.is_null() {
        return Ok(Value::Null);
    }
    let Some(found) = elements(collection) else {
        return Err(not_a_collection(MEMBER_OF));
    };
    if found.is_empty() {
        return Ok(Value::Boolean(false));
    }
    if element.is_null() {
        return Ok(Value::Null);
    }
    if found.iter().any(|candidate| same(candidate, element)) {
        return Ok(Value::Boolean(true));
    }
    if found.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    Ok(Value::Boolean(false))
}

fn submultiset_of(left: &[Value], right: &[Value]) -> Value {
    if left.is_empty() {
        return Value::Boolean(true);
    }
    if left.iter().chain(right).any(Value::is_null) {
        return Value::Null;
    }
    Value::Boolean(difference(left, right).is_empty())
}

fn same(left: &Value, right: &Value) -> bool {
    !left.is_null() && !right.is_null() && compare_values(left, right) == Ordering::Equal
}

fn intersection(left: &[Value], right: &[Value]) -> Vec<Value> {
    let mut remaining: Vec<&Value> = right.iter().collect();
    let mut kept = Vec::new();
    for element in left {
        if let Some(at) = remaining.iter().position(|other| equal_or_both_null(other, element)) {
            remaining.swap_remove(at);
            kept.push(element.clone());
        }
    }
    kept
}

fn equal_or_both_null(left: &Value, right: &Value) -> bool {
    same(left, right) || (left.is_null() && right.is_null())
}

fn difference(left: &[Value], right: &[Value]) -> Vec<Value> {
    let mut remaining: Vec<&Value> = right.iter().collect();
    let mut kept = Vec::new();
    for element in left {
        match remaining
            .iter()
            .position(|other| equal_or_both_null(other, element))
        {
            Some(at) => {
                remaining.swap_remove(at);
            }
            None => kept.push(element.clone()),
        }
    }
    kept
}

fn distinct(values: Vec<Value>) -> Vec<Value> {
    let mut kept: Vec<Value> = Vec::with_capacity(values.len());
    for value in values {
        if !kept.iter().any(|other| equal_or_both_null(other, &value)) {
            kept.push(value);
        }
    }
    kept
}

fn collection(values: Vec<Value>) -> Value {
    let names = (1..=values.len()).map(|key| key.to_string()).collect();
    record_value(RECORD_OID, values, names)
}

fn not_a_collection(name: &str) -> SqlError {
    let operator = name
        .trim_start_matches("oracle_")
        .replace('_', " ")
        .to_uppercase();
    SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        format!("inconsistent datatypes: {operator} expects a nested table or varray"),
    )
    .with_oracle_code(932)
}
