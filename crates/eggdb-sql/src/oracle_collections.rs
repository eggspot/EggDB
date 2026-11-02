//! Oracle's collection operators in SQL (`CARDINALITY`, `MEMBER OF`, `SUBMULTISET OF` and the
//! `MULTISET` set operators) over PL/SQL nested tables and varrays.

use crate::error::{SqlError, SqlResult, SqlState};
use crate::plpgsql::{MAX_COLLECTION_ELEMENTS, too_many_elements};
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

const CANCEL_CHECK_EVERY: usize = 1024;

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
        && record.names.iter().all(|name| name.parse::<u32>().is_ok())
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
    match name {
        MEMBER_OF => member_of(left, right),
        SUBMULTISET_OF => submultiset_of(left, right),
        _ => multiset(name, left, right, MAX_COLLECTION_ELEMENTS),
    }
}

fn multiset(name: &str, left: &Value, right: &Value, most: usize) -> SqlResult<Value> {
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    let (Some(left), Some(right)) = (elements(left), elements(right)) else {
        return Err(not_a_collection(name));
    };
    let kept: Vec<&Value> = match name {
        MULTISET_UNION_ALL => {
            at_most(left.len().saturating_add(right.len()), most)?;
            left.iter().chain(right).collect()
        }
        MULTISET_UNION_DISTINCT => distinct(left.iter().chain(right).collect())?,
        MULTISET_INTERSECT_ALL => intersection(left, right)?,
        MULTISET_INTERSECT_DISTINCT => distinct(intersection(left, right)?)?,
        MULTISET_EXCEPT_ALL => difference(left, right)?,
        MULTISET_EXCEPT_DISTINCT => {
            let excluded = Counted::of(right)?;
            let mut kept = Vec::new();
            for (step, element) in distinct(left.iter().collect())?.into_iter().enumerate() {
                pause_point(step)?;
                if !excluded.contains(element) {
                    kept.push(element);
                }
            }
            kept
        }
        _ => return Err(not_a_collection(name)),
    };
    at_most(kept.len(), most)?;
    Ok(collection(kept.into_iter().cloned().collect()))
}

fn at_most(count: usize, most: usize) -> SqlResult<()> {
    if count > most {
        Err(too_many_elements())
    } else {
        Ok(())
    }
}

fn pause_point(step: usize) -> SqlResult<()> {
    if step.is_multiple_of(CANCEL_CHECK_EVERY) {
        crate::cancel::check()?;
    }
    Ok(())
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
    let mut saw_null = false;
    for (step, candidate) in found.iter().enumerate() {
        pause_point(step)?;
        if candidate.is_null() {
            saw_null = true;
        } else if compare_values(candidate, element) == Ordering::Equal {
            return Ok(Value::Boolean(true));
        }
    }
    Ok(if saw_null {
        Value::Null
    } else {
        Value::Boolean(false)
    })
}

fn submultiset_of(left: &Value, right: &Value) -> SqlResult<Value> {
    if left.is_null() {
        return Ok(Value::Null);
    }
    let Some(left) = elements(left) else {
        return Err(not_a_collection(SUBMULTISET_OF));
    };
    let right = match right {
        Value::Null => None,
        other => Some(elements(other).ok_or_else(|| not_a_collection(SUBMULTISET_OF))?),
    };
    if left.is_empty() {
        return Ok(Value::Boolean(true));
    }
    let Some(right) = right else {
        return Ok(Value::Null);
    };
    let mut remaining = Counted::of(right)?;
    let mut left_nulls = 0usize;
    let mut unmatched = 0usize;
    for (step, element) in left.iter().enumerate() {
        pause_point(step)?;
        if element.is_null() {
            left_nulls += 1;
        } else if !remaining.take(element) {
            unmatched += 1;
        }
    }
    let right_nulls = right.iter().filter(|element| element.is_null()).count();
    Ok(if left_nulls == 0 && unmatched == 0 {
        Value::Boolean(true)
    } else if unmatched <= right_nulls && left.len() <= right.len() {
        Value::Null
    } else {
        Value::Boolean(false)
    })
}

fn multiset_order(left: &Value, right: &Value) -> Ordering {
    match (left.is_null(), right.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => compare_values(left, right),
    }
}

struct Counted<'v> {
    runs: Vec<(&'v Value, usize)>,
}

impl<'v> Counted<'v> {
    fn of(values: &'v [Value]) -> SqlResult<Self> {
        let mut sorted: Vec<&Value> = values.iter().collect();
        crate::cancel::check()?;
        sorted.sort_by(|left, right| multiset_order(left, right));
        crate::cancel::check()?;
        let mut runs: Vec<(&Value, usize)> = Vec::new();
        for value in sorted {
            match runs.last_mut() {
                Some((last, count)) if multiset_order(last, value) == Ordering::Equal => {
                    *count += 1;
                }
                _ => runs.push((value, 1)),
            }
        }
        Ok(Self { runs })
    }

    fn run(&self, value: &Value) -> Option<usize> {
        self.runs
            .binary_search_by(|(run, _)| multiset_order(run, value))
            .ok()
    }

    fn contains(&self, value: &Value) -> bool {
        self.run(value).is_some()
    }

    fn take(&mut self, value: &Value) -> bool {
        let Some((_, count)) = self.run(value).and_then(|at| self.runs.get_mut(at)) else {
            return false;
        };
        if *count == 0 {
            return false;
        }
        *count -= 1;
        true
    }
}

fn intersection<'v>(left: &'v [Value], right: &'v [Value]) -> SqlResult<Vec<&'v Value>> {
    let mut remaining = Counted::of(right)?;
    let mut kept = Vec::new();
    for (step, element) in left.iter().enumerate() {
        pause_point(step)?;
        if remaining.take(element) {
            kept.push(element);
        }
    }
    Ok(kept)
}

fn difference<'v>(left: &'v [Value], right: &'v [Value]) -> SqlResult<Vec<&'v Value>> {
    let mut remaining = Counted::of(right)?;
    let mut kept = Vec::new();
    for (step, element) in left.iter().enumerate() {
        pause_point(step)?;
        if !remaining.take(element) {
            kept.push(element);
        }
    }
    Ok(kept)
}

fn distinct(values: Vec<&Value>) -> SqlResult<Vec<&Value>> {
    let mut keyed: Vec<(usize, &Value)> = values.iter().copied().enumerate().collect();
    crate::cancel::check()?;
    keyed.sort_by(|(left_at, left), (right_at, right)| {
        multiset_order(left, right).then(left_at.cmp(right_at))
    });
    crate::cancel::check()?;
    let mut first = vec![false; values.len()];
    let mut previous: Option<&Value> = None;
    for (at, value) in keyed {
        if previous.is_none_or(|seen| multiset_order(seen, value) != Ordering::Equal)
            && let Some(slot) = first.get_mut(at)
        {
            *slot = true;
        }
        previous = Some(value);
    }
    Ok(values
        .into_iter()
        .zip(first)
        .filter_map(|(value, kept)| kept.then_some(value))
        .collect())
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

#[cfg(test)]
mod tests {
    use super::{
        MULTISET_EXCEPT_ALL, MULTISET_EXCEPT_DISTINCT, MULTISET_INTERSECT_ALL,
        MULTISET_INTERSECT_DISTINCT, MULTISET_UNION_ALL, MULTISET_UNION_DISTINCT, collection,
        elements, multiset,
    };
    use crate::types::Value;

    fn numbers(values: impl IntoIterator<Item = i64>) -> Value {
        collection(values.into_iter().map(Value::BigInt).collect())
    }

    fn listed(value: &Value) -> Vec<i64> {
        elements(value)
            .unwrap_or_default()
            .iter()
            .map(|element| element.as_integer().unwrap_or(-1))
            .collect()
    }

    #[test]
    fn multiset_results_past_the_collection_limit_are_refused() {
        let left = numbers([1, 2]);
        let right = numbers([2, 3]);
        let error = multiset(MULTISET_UNION_ALL, &left, &right, 3).unwrap_err();
        assert_eq!(error.state().code(), "54000");
        assert_eq!(
            listed(&multiset(MULTISET_UNION_DISTINCT, &left, &right, 3).unwrap()),
            [1, 2, 3]
        );
        assert!(multiset(MULTISET_UNION_DISTINCT, &left, &numbers([4, 5]), 3).is_err());
    }

    #[test]
    fn large_multisets_are_matched_by_sorting_in_order_of_the_left_operand() {
        let count = 60_000;
        let left = numbers((0..count).rev().chain(0..10));
        let right = numbers((0..count).step_by(2));
        let most = usize::MAX;
        let intersect = multiset(MULTISET_INTERSECT_ALL, &left, &right, most).unwrap();
        assert_eq!(listed(&intersect).len(), 30_000);
        assert_eq!(listed(&intersect)[..2], [59_998, 59_996]);
        let except = multiset(MULTISET_EXCEPT_ALL, &left, &right, most).unwrap();
        assert_eq!(listed(&except).len(), 30_010);
        let except_distinct = multiset(MULTISET_EXCEPT_DISTINCT, &left, &right, most).unwrap();
        assert_eq!(listed(&except_distinct).len(), 30_000);
        let union = multiset(MULTISET_UNION_DISTINCT, &left, &right, most).unwrap();
        assert_eq!(listed(&union).len(), 60_000);
        assert_eq!(listed(&union)[..2], [59_999, 59_998]);
        let intersect_distinct = multiset(MULTISET_INTERSECT_DISTINCT, &left, &left, most).unwrap();
        assert_eq!(listed(&intersect_distinct).len(), 60_000);
    }

    #[test]
    fn null_elements_match_each_other_in_the_set_operators() {
        let left = collection(vec![Value::Null, Value::BigInt(1), Value::Null]);
        let right = collection(vec![Value::Null]);
        let except = multiset(MULTISET_EXCEPT_ALL, &left, &right, usize::MAX).unwrap();
        assert_eq!(elements(&except).unwrap(), [Value::BigInt(1), Value::Null]);
        let distinct = multiset(MULTISET_UNION_DISTINCT, &left, &right, usize::MAX).unwrap();
        assert_eq!(
            elements(&distinct).unwrap(),
            [Value::Null, Value::BigInt(1)]
        );
    }
}
