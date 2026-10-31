use crate::document::{Bson, Document};
use std::cmp::Ordering;

/// Where a value's type sorts among the others, as MongoDB documents the order: MinKey, null,
/// numbers, strings and symbols, documents, arrays, binary data, object ids, booleans, dates,
/// timestamps, regular expressions, then MaxKey.
pub fn type_rank(value: &Bson) -> u8 {
    match value {
        Bson::MinKey => 0,
        Bson::Null | Bson::Undefined => 1,
        Bson::Double(_) | Bson::Int32(_) | Bson::Int64(_) | Bson::Decimal128(_) => 2,
        Bson::String(_) | Bson::Symbol(_) => 3,
        Bson::Document(_) => 4,
        Bson::Array(_) => 5,
        Bson::Binary { .. } => 6,
        Bson::ObjectId(_) => 7,
        Bson::Boolean(_) => 8,
        Bson::DateTime(_) => 9,
        Bson::Timestamp { .. } => 10,
        Bson::Regex { .. } => 11,
        Bson::DbPointer { .. } => 12,
        Bson::JavaScript(_) => 13,
        Bson::JavaScriptWithScope { .. } => 14,
        Bson::MaxKey => 15,
    }
}

fn compare_numbers(left: &Bson, right: &Bson) -> Ordering {
    if let (Some(a), Some(b)) = (exact_integer(left), exact_integer(right)) {
        return a.cmp(&b);
    }
    let a = left.as_f64().unwrap_or(f64::NAN);
    let b = right.as_f64().unwrap_or(f64::NAN);
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => match (exact_integer(left), exact_integer(right)) {
            (Some(integer), None) => compare_integer_with_double(integer, b),
            (None, Some(integer)) => compare_integer_with_double(integer, a).reverse(),
            _ => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
        },
    }
}

fn exact_integer(value: &Bson) -> Option<i64> {
    match value {
        Bson::Int32(number) => Some(i64::from(*number)),
        Bson::Int64(number) => Some(*number),
        _ => None,
    }
}

fn compare_integer_with_double(integer: i64, double: f64) -> Ordering {
    if double >= 9.223_372_036_854_775_807e18 {
        return Ordering::Less;
    }
    if double < -9.223_372_036_854_775_808e18 {
        return Ordering::Greater;
    }
    let whole = double.floor();
    match integer.cmp(&(whole as i64)) {
        Ordering::Equal if double > whole => Ordering::Less,
        other => other,
    }
}

fn compare_documents(left: &Document, right: &Document) -> Ordering {
    let mut right_entries = right.iter();
    for (name, value) in left.iter() {
        let Some((other_name, other_value)) = right_entries.next() else {
            return Ordering::Greater;
        };
        let ordering = type_rank(value)
            .cmp(&type_rank(other_value))
            .then_with(|| name.cmp(other_name))
            .then_with(|| compare(value, other_value));
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    if right_entries.next().is_some() { Ordering::Less } else { Ordering::Equal }
}

/// Compares two values in MongoDB's documented order: by [`type_rank`] first, numbers of any
/// type by their value, strings by their bytes, documents and arrays element by element.
pub fn compare(left: &Bson, right: &Bson) -> Ordering {
    let ranks = type_rank(left).cmp(&type_rank(right));
    if ranks != Ordering::Equal {
        return ranks;
    }
    match (left, right) {
        (a, b) if a.is_number() => compare_numbers(a, b),
        (Bson::String(a) | Bson::Symbol(a), Bson::String(b) | Bson::Symbol(b)) => a.as_bytes().cmp(b.as_bytes()),
        (Bson::Document(a), Bson::Document(b)) => compare_documents(a, b),
        (Bson::Array(a), Bson::Array(b)) => {
            for (x, y) in a.iter().zip(b) {
                let ordering = compare(x, y);
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            a.len().cmp(&b.len())
        }
        (Bson::Binary { subtype: s1, bytes: b1 }, Bson::Binary { subtype: s2, bytes: b2 }) => {
            b1.len().cmp(&b2.len()).then(s1.cmp(s2)).then_with(|| b1.cmp(b2))
        }
        (Bson::ObjectId(a), Bson::ObjectId(b)) => a.cmp(b),
        (Bson::Boolean(a), Bson::Boolean(b)) => a.cmp(b),
        (Bson::DateTime(a), Bson::DateTime(b)) => a.cmp(b),
        (Bson::Timestamp { time: t1, increment: i1 }, Bson::Timestamp { time: t2, increment: i2 }) => {
            t1.cmp(t2).then(i1.cmp(i2))
        }
        (Bson::Regex { pattern: p1, options: o1 }, Bson::Regex { pattern: p2, options: o2 }) => {
            p1.cmp(p2).then_with(|| o1.cmp(o2))
        }
        (Bson::DbPointer { namespace: n1, id: i1 }, Bson::DbPointer { namespace: n2, id: i2 }) => {
            n1.cmp(n2).then(i1.cmp(i2))
        }
        (Bson::JavaScript(a), Bson::JavaScript(b)) => a.cmp(b),
        (Bson::JavaScriptWithScope { code: c1, scope: s1 }, Bson::JavaScriptWithScope { code: c2, scope: s2 }) => {
            c1.cmp(c2).then_with(|| compare_documents(s1, s2))
        }
        _ => Ordering::Equal,
    }
}
