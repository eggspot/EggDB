//! PostgreSQL's multiranges (D1332): an ordered set of ranges that neither overlap nor touch,
//! one multirange type per built-in range type, with their text form, operators, functions
//! and `range_agg`.

use super::{
    RangeBound, RangeBounds, RangeValue, adjacent, bound_extreme, compare, compare_sides, contains_element,
    contains_range, format_text, lower_of, overlaps, range_of, range_value, upper_of,
};
use crate::ast::BinaryOperator;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{DefinedKind, DefinedType, DefinedValue, SqlType, Value};
use std::cmp::Ordering;

/// A built-in multirange type: its name, OID, array OID and the range type it holds.
pub(crate) struct BuiltinMultirange {
    pub(crate) name: &'static str,
    pub(crate) oid: u32,
    pub(crate) array: u32,
    pub(crate) range: u32,
}

pub(crate) const BUILTIN_MULTIRANGES: [BuiltinMultirange; 6] = [
    BuiltinMultirange { name: "int4multirange", oid: 4451, array: 6150, range: 3904 },
    BuiltinMultirange { name: "nummultirange", oid: 4532, array: 6151, range: 3906 },
    BuiltinMultirange { name: "tsmultirange", oid: 4533, array: 6152, range: 3908 },
    BuiltinMultirange { name: "tstzmultirange", oid: 4534, array: 6153, range: 3910 },
    BuiltinMultirange { name: "datemultirange", oid: 4535, array: 6155, range: 3912 },
    BuiltinMultirange { name: "int8multirange", oid: 4536, array: 6157, range: 3926 },
];

/// A multirange: its type and its ranges, in order, none empty, overlapping or adjacent.
#[derive(Debug, Clone, PartialEq)]
pub struct MultirangeValue {
    /// The OID of the multirange type, such as 4451 for `int4multirange`.
    pub type_oid: u32,
    /// The ranges.
    pub ranges: Vec<RangeValue>,
}

pub(crate) fn builtin(oid: u32) -> Option<&'static BuiltinMultirange> {
    BUILTIN_MULTIRANGES.iter().find(|found| found.oid == oid)
}

pub(crate) fn builtin_named(name: &str) -> Option<&'static BuiltinMultirange> {
    BUILTIN_MULTIRANGES.iter().find(|found| found.name == name)
}

fn of_range(range_oid: u32) -> Option<&'static BuiltinMultirange> {
    BUILTIN_MULTIRANGES.iter().find(|found| found.range == range_oid)
}

pub(crate) fn defined_type(oid: u32) -> DefinedType {
    DefinedType { kind: DefinedKind::Base, oid }
}

/// The multirange type that holds ranges of type `range`.
pub(crate) fn type_of_range(range: SqlType) -> Option<SqlType> {
    let SqlType::Defined(defined) = range else {
        return None;
    };
    of_range(defined.oid).map(|found| SqlType::Defined(defined_type(found.oid)))
}

/// The range type a multirange type holds.
pub(crate) fn range_type_of(multirange: SqlType) -> Option<SqlType> {
    let SqlType::Defined(defined) = multirange else {
        return None;
    };
    builtin(defined.oid).map(|found| crate::types::DefinedType::range(found.range))
}

pub(crate) fn is_multirange_type(sql_type: SqlType) -> bool {
    matches!(sql_type, SqlType::Defined(defined) if defined.kind == DefinedKind::Base && builtin(defined.oid).is_some())
}

pub(crate) fn value(multirange: MultirangeValue) -> Value {
    Value::Defined(Box::new(DefinedValue::Multirange(multirange)))
}

pub(crate) fn multirange_of(value: &Value) -> Option<&MultirangeValue> {
    match value {
        Value::Defined(defined) => match defined.as_ref() {
            DefinedValue::Multirange(multirange) => Some(multirange),
            _ => None,
        },
        _ => None,
    }
}

fn union_of(type_oid: u32, x: &RangeBounds, y: &RangeBounds) -> SqlResult<RangeValue> {
    let made = super::make(
        type_oid,
        bound_extreme(lower_of(x), lower_of(y), Ordering::Less).clone(),
        bound_extreme(upper_of(x), upper_of(y), Ordering::Greater).clone(),
    )?;
    range_of(&made).cloned().ok_or_else(|| crate::encoding::corrupt("range"))
}

/// The multirange of type `type_oid` holding `ranges`: sorted, with empty ranges dropped and
/// overlapping or adjacent ranges merged.
pub(crate) fn normalized(type_oid: u32, mut ranges: Vec<RangeValue>) -> SqlResult<MultirangeValue> {
    ranges.retain(|range| range.bounds.is_some());
    ranges.sort_by(compare);
    let mut merged: Vec<RangeValue> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(last) = merged.last_mut()
            && let (Some(x), Some(y)) = (&last.bounds, &range.bounds)
            && (overlaps(x, y) || adjacent(x, y))
        {
            *last = union_of(last.type_oid, x, y)?;
            continue;
        }
        merged.push(range);
    }
    Ok(MultirangeValue { type_oid, ranges: merged })
}

/// The multirange as PostgreSQL prints it: `{[1,3),[5,7)}`, or `{}` when empty.
pub(crate) fn format(multirange: &MultirangeValue) -> String {
    let ranges: Vec<String> = multirange.ranges.iter().map(format_text).collect();
    format!("{{{}}}", ranges.join(","))
}

fn malformed(text: &str, detail: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("malformed multirange literal: \"{text}\""),
    )
    .with_detail(detail.to_owned())
}

/// The multirange of type `type_oid` written as `text`.
pub(crate) fn parse(text: &str, type_oid: u32) -> SqlResult<MultirangeValue> {
    let found = builtin(type_oid).ok_or_else(|| crate::encoding::corrupt("multirange type"))?;
    let subtype = super::subtype_of(found.range)?;
    let trimmed = text.trim();
    let inner = trimmed
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .ok_or_else(|| malformed(text, "Missing left brace."))?;
    let mut ranges = Vec::new();
    let mut piece = String::new();
    let mut depth = 0usize;
    let mut quoted = false;
    for character in inner.chars() {
        match character {
            '"' => quoted = !quoted,
            '[' | '(' if !quoted => depth += 1,
            ']' | ')' if !quoted => depth = depth.saturating_sub(1),
            ',' if !quoted && depth == 0 => {
                ranges.push(std::mem::take(&mut piece));
                continue;
            }
            _ => {}
        }
        piece.push(character);
    }
    if !piece.trim().is_empty() || !ranges.is_empty() {
        ranges.push(piece);
    }
    let mut parsed = Vec::with_capacity(ranges.len());
    for range in ranges {
        if range.trim().is_empty() {
            return Err(malformed(text, "Expected range start."));
        }
        let made = super::parse_text(range.trim(), found.range, subtype)?;
        parsed.push(range_of(&made).cloned().ok_or_else(|| malformed(text, "Expected range start."))?);
    }
    normalized(type_oid, parsed)
}

/// Orders multiranges as PostgreSQL does: range by range, then the shorter first.
pub(crate) fn compare_multiranges(a: &MultirangeValue, b: &MultirangeValue) -> Ordering {
    a.ranges
        .iter()
        .zip(&b.ranges)
        .map(|(x, y)| compare(x, y))
        .find(|order| order.is_ne())
        .unwrap_or_else(|| a.ranges.len().cmp(&b.ranges.len()))
}

pub(crate) fn equality_key_of(out: &mut Vec<u8>, multirange: &MultirangeValue) {
    out.extend_from_slice(&multirange.type_oid.to_le_bytes());
    out.extend_from_slice(&(multirange.ranges.len() as u64).to_le_bytes());
    for range in &multirange.ranges {
        super::equality_key_of(out, range);
    }
}

fn empty(type_oid: u32) -> MultirangeValue {
    MultirangeValue { type_oid, ranges: Vec::new() }
}

fn intersection(a: &MultirangeValue, b: &MultirangeValue) -> SqlResult<MultirangeValue> {
    let mut found = Vec::new();
    for x in &a.ranges {
        for y in &b.ranges {
            let (Some(xb), Some(yb)) = (&x.bounds, &y.bounds) else {
                continue;
            };
            if overlaps(xb, yb) {
                let made = super::make(
                    x.type_oid,
                    bound_extreme(lower_of(xb), lower_of(yb), Ordering::Greater).clone(),
                    bound_extreme(upper_of(xb), upper_of(yb), Ordering::Less).clone(),
                )?;
                if let Some(range) = range_of(&made) {
                    found.push(range.clone());
                }
            }
        }
    }
    normalized(a.type_oid, found)
}

fn without(range: &RangeValue, taken: &RangeValue) -> SqlResult<Vec<RangeValue>> {
    let (Some(x), Some(y)) = (&range.bounds, &taken.bounds) else {
        return Ok(vec![range.clone()]);
    };
    if !overlaps(x, y) {
        return Ok(vec![range.clone()]);
    }
    let mut pieces = Vec::new();
    if compare_sides(lower_of(x), lower_of(y)) == Ordering::Less {
        let made = super::make(range.type_oid, x.lower.clone(), y.lower.flipped())?;
        pieces.extend(range_of(&made).cloned());
    }
    if compare_sides(upper_of(x), upper_of(y)) == Ordering::Greater {
        let made = super::make(range.type_oid, y.upper.flipped(), x.upper.clone())?;
        pieces.extend(range_of(&made).cloned());
    }
    Ok(pieces)
}

fn difference(a: &MultirangeValue, b: &MultirangeValue) -> SqlResult<MultirangeValue> {
    let mut remaining = a.ranges.clone();
    for taken in &b.ranges {
        let mut next = Vec::with_capacity(remaining.len());
        for range in &remaining {
            next.extend(without(range, taken)?);
        }
        remaining = next;
    }
    normalized(a.type_oid, remaining)
}

fn as_multirange(value: &Value, type_oid: u32) -> Option<MultirangeValue> {
    if let Some(multirange) = multirange_of(value) {
        return Some(multirange.clone());
    }
    let range = range_of(value)?;
    Some(MultirangeValue { type_oid, ranges: range.bounds.as_ref().map(|_| vec![range.clone()]).unwrap_or_default() })
}

fn mismatch(symbol: &str, left: &Value, right: &Value) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("operator does not exist: {} {symbol} {}", left.type_name(), right.type_name()),
    )
}

/// `+` (union), `-` (difference) and `*` (intersection) of multiranges.
pub(crate) fn arithmetic(operator: BinaryOperator, left: &Value, right: &Value) -> Option<SqlResult<Value>> {
    let (a, b) = (multirange_of(left)?, multirange_of(right)?);
    if a.type_oid != b.type_oid {
        return Some(Err(mismatch(operator.symbol(), left, right)));
    }
    Some(
        match operator {
            BinaryOperator::Add => normalized(a.type_oid, a.ranges.iter().chain(&b.ranges).cloned().collect()),
            BinaryOperator::Subtract => difference(a, b),
            BinaryOperator::Multiply => intersection(a, b),
            _ => Err(mismatch(operator.symbol(), left, right)),
        }
        .map(value),
    )
}

/// `@>` (`contains` true) and `<@` with a multirange on either side: against an element, a
/// range or another multirange.
pub(crate) fn containment(contains: bool, left: &Value, right: &Value) -> Option<SqlResult<Value>> {
    if multirange_of(left).is_none() && multirange_of(right).is_none() {
        return None;
    }
    let (outer, inner) = if contains { (left, right) } else { (right, left) };
    if inner.is_null() || outer.is_null() {
        return Some(Ok(Value::Null));
    }
    let answer = match (multirange_of(outer), range_of(outer)) {
        (Some(outer), _) => match (multirange_of(inner), range_of(inner)) {
            (Some(inner), _) => inner.ranges.iter().all(|range| outer.ranges.iter().any(|holder| contains_range(holder, range))),
            (None, Some(range)) => {
                range.bounds.is_none() || outer.ranges.iter().any(|holder| contains_range(holder, range))
            }
            (None, None) => outer.ranges.iter().any(|holder| contains_element(holder, inner)),
        },
        (None, Some(range)) => match multirange_of(inner) {
            Some(inner) => inner.ranges.iter().all(|held| contains_range(range, held)),
            None => return None,
        },
        (None, None) => return Some(Err(mismatch(if contains { "@>" } else { "<@" }, left, right))),
    };
    Some(Ok(Value::Boolean(answer)))
}

/// `&&` with a multirange on either side.
pub(crate) fn overlap(values: &[Value]) -> Option<SqlResult<Value>> {
    let [left, right] = values else {
        return None;
    };
    let type_oid = multirange_of(left).or_else(|| multirange_of(right))?.type_oid;
    if left.is_null() || right.is_null() {
        return Some(Ok(Value::Null));
    }
    let (Some(a), Some(b)) = (as_multirange(left, type_oid), as_multirange(right, type_oid)) else {
        return Some(Err(mismatch("&&", left, right)));
    };
    let found = a.ranges.iter().any(|x| {
        b.ranges.iter().any(|y| match (&x.bounds, &y.bounds) {
            (Some(xb), Some(yb)) => overlaps(xb, yb),
            _ => false,
        })
    });
    Some(Ok(Value::Boolean(found)))
}

/// The multirange functions: the constructors named after each type, `multirange`,
/// `range_merge`, and `lower`, `upper`, `isempty`, `lower_inc`, `upper_inc`, `lower_inf` and
/// `upper_inf` of a multirange.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if let Some(found) = builtin_named(name) {
        let mut ranges = Vec::with_capacity(values.len());
        for argument in values {
            match range_of(argument) {
                Some(range) if range.type_oid == found.range => ranges.push(range.clone()),
                _ if argument.is_null() => {
                    return Some(Err(SqlError::new(
                        SqlState::NULL_VALUE_NOT_ALLOWED,
                        "multirange values cannot contain null members",
                    )));
                }
                _ => return None,
            }
        }
        return Some(normalized(found.oid, ranges).map(value));
    }
    if name == "multirange" {
        let [argument] = values else {
            return None;
        };
        if argument.is_null() {
            return Some(Ok(Value::Null));
        }
        let range = range_of(argument)?;
        let found = of_range(range.type_oid)?;
        return Some(normalized(found.oid, vec![range.clone()]).map(value));
    }
    let [argument] = values else {
        return None;
    };
    let multirange = multirange_of(argument)?;
    let first = multirange.ranges.first().and_then(|range| range.bounds.as_ref());
    let last = multirange.ranges.last().and_then(|range| range.bounds.as_ref());
    let bound_value = |bound: Option<&RangeBound>| match bound {
        Some(RangeBound::Inclusive(found) | RangeBound::Exclusive(found)) => found.clone(),
        _ => Value::Null,
    };
    Some(Ok(match name {
        "lower" => bound_value(first.map(|bounds| &bounds.lower)),
        "upper" => bound_value(last.map(|bounds| &bounds.upper)),
        "isempty" => Value::Boolean(multirange.ranges.is_empty()),
        "lower_inc" => Value::Boolean(first.is_some_and(|bounds| matches!(bounds.lower, RangeBound::Inclusive(_)))),
        "upper_inc" => Value::Boolean(last.is_some_and(|bounds| matches!(bounds.upper, RangeBound::Inclusive(_)))),
        "lower_inf" => Value::Boolean(first.is_some_and(|bounds| bounds.lower == RangeBound::Unbounded)),
        "upper_inf" => Value::Boolean(last.is_some_and(|bounds| bounds.upper == RangeBound::Unbounded)),
        "range_merge" => {
            let range_oid = builtin(multirange.type_oid)?.range;
            match (first, last) {
                (Some(first), Some(last)) => {
                    return Some(super::make(range_oid, first.lower.clone(), last.upper.clone()));
                }
                _ => range_value(RangeValue { type_oid: range_oid, bounds: None }),
            }
        }
        _ => return None,
    }))
}

/// The type a multirange function returns, given its first argument's type.
pub(crate) fn function_type(name: &str, first: Option<SqlType>) -> Option<SqlType> {
    if let Some(found) = builtin_named(name) {
        return Some(SqlType::Defined(defined_type(found.oid)));
    }
    if name == "multirange" {
        return first.and_then(type_of_range);
    }
    let first = first.filter(|sql_type| is_multirange_type(*sql_type))?;
    match name {
        "lower" | "upper" => range_type_of(first).and_then(|range| match range {
            SqlType::Defined(defined) => crate::user_types::range_subtype(defined.oid),
            _ => None,
        }),
        "isempty" | "lower_inc" | "upper_inc" | "lower_inf" | "upper_inf" => Some(SqlType::Boolean),
        "range_merge" => range_type_of(first),
        _ => None,
    }
}

/// `range_agg`'s answer over the ranges it collected, or `range_intersect_agg`'s.
pub(crate) fn aggregate(intersect: bool, values: Vec<Value>) -> SqlResult<Value> {
    let Some(first) = values.first() else {
        return Ok(Value::Null);
    };
    if intersect {
        let mut found = first.clone();
        for next in &values[1..] {
            found = if multirange_of(&found).is_some() {
                arithmetic(BinaryOperator::Multiply, &found, next).unwrap_or(Ok(Value::Null))?
            } else {
                super::arithmetic(BinaryOperator::Multiply, &found, next).unwrap_or(Ok(Value::Null))?
            };
        }
        return Ok(found);
    }
    let range_oid = match range_of(first) {
        Some(range) => range.type_oid,
        None => builtin(multirange_of(first).map_or(0, |found| found.type_oid)).map_or(0, |found| found.range),
    };
    let found = of_range(range_oid)
        .ok_or_else(|| SqlError::new(SqlState::UNDEFINED_FUNCTION, "range_agg takes a range or multirange"))?;
    let mut ranges = Vec::new();
    for value in &values {
        match (range_of(value), multirange_of(value)) {
            (Some(range), _) => ranges.push(range.clone()),
            (None, Some(multirange)) => ranges.extend(multirange.ranges.iter().cloned()),
            _ => {}
        }
    }
    normalized(found.oid, ranges).map(self::value)
}

/// The type `range_agg` returns for an argument of type `argument`.
pub(crate) fn aggregate_type(argument: Option<SqlType>) -> Option<SqlType> {
    let argument = argument?;
    if is_multirange_type(argument) {
        return Some(argument);
    }
    type_of_range(argument)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int4(text: &str) -> MultirangeValue {
        parse(text, 4451).unwrap()
    }

    #[test]
    fn multiranges_read_merge_and_print() {
        assert_eq!(format(&int4("{[1,3), [5,7)}")), "{[1,3),[5,7)}");
        assert_eq!(format(&int4("{[5,7), [1,3), [3,4]}")), "{[1,7)}");
        assert_eq!(format(&int4("{}")), "{}");
        assert_eq!(format(&int4("{empty, [1,2]}")), "{[1,3)}");
        assert!(parse("[1,3)", 4451).is_err());
    }

    #[test]
    fn operators_follow_postgresql() {
        let shown = |result: Option<SqlResult<Value>>| result.unwrap().unwrap().to_text().unwrap_or_default();
        let a = value(int4("{[1,5), [8,10)}"));
        let b = value(int4("{[3,9)}"));
        assert_eq!(shown(arithmetic(BinaryOperator::Add, &a, &b)), "{[1,10)}");
        assert_eq!(shown(arithmetic(BinaryOperator::Subtract, &a, &b)), "{[1,3),[9,10)}");
        assert_eq!(shown(arithmetic(BinaryOperator::Multiply, &a, &b)), "{[3,5),[8,9)}");
        assert_eq!(containment(true, &a, &Value::Integer(4)).unwrap().unwrap(), Value::Boolean(true));
        assert_eq!(containment(true, &a, &Value::Integer(6)).unwrap().unwrap(), Value::Boolean(false));
        assert_eq!(overlap(&[a.clone(), b.clone()]).unwrap().unwrap(), Value::Boolean(true));
        assert_eq!(shown(function("range_merge", std::slice::from_ref(&a))), "[1,10)");
        assert_eq!(shown(function("upper", std::slice::from_ref(&a))), "10");
    }

    fn answer(session: &mut crate::Session<'_>, sql: &str) -> String {
        let mut outputs = Vec::new();
        session
            .execute(sql, &mut |result| outputs.push(crate::tests::render(&result)))
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        outputs.pop().unwrap_or_default()
    }

    #[test]
    fn multirange_columns_and_range_agg_work() {
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::new(&store);
        answer(
            &mut session,
            "CREATE TABLE bookings (id integer PRIMARY KEY, slots int4multirange);
             INSERT INTO bookings VALUES (1, '{[1,5), [10,20)}'), (2, int4multirange(int4range(3,7)))",
        );
        assert_eq!(
            answer(&mut session, "SELECT slots, pg_typeof(slots) FROM bookings ORDER BY id"),
            "slots|pg_typeof\n{[1,5),[10,20)}|int4multirange\n{[3,7)}|int4multirange\nSELECT 2"
        );
        assert_eq!(answer(&mut session, "SELECT id FROM bookings WHERE slots @> 4 ORDER BY id"), "id\n1\n2\nSELECT 2");
        assert_eq!(
            answer(
                &mut session,
                "SELECT range_agg(r) FROM (VALUES (int4range(1,3)), (int4range(2,6)), (int4range(8,9))) AS v(r)"
            ),
            "range_agg\n{[1,6),[8,9)}\nSELECT 1"
        );
        assert_eq!(
            answer(&mut session, "SELECT a.slots * b.slots AS both_ FROM bookings a, bookings b WHERE a.id = 1 AND b.id = 2"),
            "both_\n{[3,5)}\nSELECT 1"
        );
        assert_eq!(
            answer(&mut session, "SELECT upper(slots), range_merge(slots) FROM bookings WHERE id = 1"),
            "upper|range_merge\n20|[1,20)\nSELECT 1"
        );
    }
}
