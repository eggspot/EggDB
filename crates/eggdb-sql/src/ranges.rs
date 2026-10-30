use crate::ast::{BinaryOperator, Expr};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::Evaluator;
use crate::types::{DefinedKind, DefinedType, DefinedValue, SqlType, Value, compare_values};
use std::cmp::Ordering;

pub(crate) struct BuiltinRange {
    pub(crate) name: &'static str,
    pub(crate) oid: u32,
    pub(crate) array_oid: u32,
    pub(crate) multirange_oid: u32,
    pub(crate) subtype: SqlType,
    pub(crate) discrete: bool,
}

pub(crate) const BUILTIN_RANGES: [BuiltinRange; 6] = [
    BuiltinRange {
        name: "int4range",
        oid: 3904,
        array_oid: 3905,
        multirange_oid: 4451,
        subtype: SqlType::Integer,
        discrete: true,
    },
    BuiltinRange {
        name: "numrange",
        oid: 3906,
        array_oid: 3907,
        multirange_oid: 4532,
        subtype: SqlType::Numeric(None),
        discrete: false,
    },
    BuiltinRange {
        name: "tsrange",
        oid: 3908,
        array_oid: 3909,
        multirange_oid: 4533,
        subtype: SqlType::Timestamp(None),
        discrete: false,
    },
    BuiltinRange {
        name: "tstzrange",
        oid: 3910,
        array_oid: 3911,
        multirange_oid: 4534,
        subtype: SqlType::TimestampTz(None),
        discrete: false,
    },
    BuiltinRange {
        name: "daterange",
        oid: 3912,
        array_oid: 3913,
        multirange_oid: 4535,
        subtype: SqlType::Date,
        discrete: true,
    },
    BuiltinRange {
        name: "int8range",
        oid: 3926,
        array_oid: 3927,
        multirange_oid: 4536,
        subtype: SqlType::BigInt,
        discrete: true,
    },
];

pub(crate) const OVERLAPS: &str = "*range_overlaps*";
pub(crate) const LEFT_OF: &str = "*range_left_of*";
pub(crate) const RIGHT_OF: &str = "*range_right_of*";
pub(crate) const NOT_RIGHT_OF: &str = "*range_not_right_of*";
pub(crate) const NOT_LEFT_OF: &str = "*range_not_left_of*";
pub(crate) const ADJACENT: &str = "*range_adjacent*";

pub(crate) const OPERATORS: [(&str, &str); 6] = [
    ("&&", OVERLAPS),
    ("<<", LEFT_OF),
    (">>", RIGHT_OF),
    ("&<", NOT_RIGHT_OF),
    ("&>", NOT_LEFT_OF),
    ("-|-", ADJACENT),
];

const FUNCTIONS: [&str; 7] = [
    "lower",
    "upper",
    "isempty",
    "lower_inc",
    "upper_inc",
    "lower_inf",
    "upper_inf",
];

/// One end of a range: unbounded, or a value the range includes or stops just short of.
#[derive(Debug, Clone, PartialEq)]
pub enum RangeBound {
    /// No bound: the range goes on forever on this side.
    Unbounded,
    /// The range includes this value.
    Inclusive(Value),
    /// The range stops just short of this value.
    Exclusive(Value),
}

impl RangeBound {
    fn value(&self) -> Option<&Value> {
        match self {
            Self::Unbounded => None,
            Self::Inclusive(value) | Self::Exclusive(value) => Some(value),
        }
    }

    fn is_inclusive(&self) -> bool {
        matches!(self, Self::Inclusive(_))
    }

    fn flipped(&self) -> Self {
        match self {
            Self::Unbounded => Self::Unbounded,
            Self::Inclusive(value) => Self::Exclusive(value.clone()),
            Self::Exclusive(value) => Self::Inclusive(value.clone()),
        }
    }
}

/// The two ends of a range that is not empty.
#[derive(Debug, Clone, PartialEq)]
pub struct RangeBounds {
    /// The lower end.
    pub lower: RangeBound,
    /// The upper end.
    pub upper: RangeBound,
}

/// A range: its type and its ends, or no ends when it is empty.
#[derive(Debug, Clone, PartialEq)]
pub struct RangeValue {
    /// The OID of the range type, such as 3904 for `int4range`.
    pub type_oid: u32,
    /// The ends; `None` for the empty range.
    pub bounds: Option<RangeBounds>,
}

pub(crate) fn builtin(oid: u32) -> Option<&'static BuiltinRange> {
    BUILTIN_RANGES.iter().find(|range| range.oid == oid)
}

pub(crate) fn builtin_named(name: &str) -> Option<&'static BuiltinRange> {
    BUILTIN_RANGES.iter().find(|range| range.name == name)
}

pub(crate) fn is_subtype(sql_type: SqlType) -> bool {
    match sql_type {
        SqlType::SmallInt
        | SqlType::Integer
        | SqlType::BigInt
        | SqlType::Real
        | SqlType::Double
        | SqlType::Numeric(_)
        | SqlType::Text
        | SqlType::Varchar(_)
        | SqlType::Bpchar(_)
        | SqlType::Date
        | SqlType::Timestamp(_)
        | SqlType::TimestampTz(_)
        | SqlType::Time(_)
        | SqlType::Interval { .. }
        | SqlType::Uuid => true,
        SqlType::Defined(defined) => defined.kind == DefinedKind::Enum,
        _ => false,
    }
}

fn is_discrete(type_oid: u32) -> bool {
    builtin(type_oid).is_some_and(|range| range.discrete)
}

fn subtype_of(type_oid: u32) -> SqlResult<SqlType> {
    crate::user_types::range_subtype(type_oid)
        .ok_or_else(|| crate::encoding::corrupt("range type definition"))
}

fn range_value(range: RangeValue) -> Value {
    Value::Defined(Box::new(DefinedValue::Range(range)))
}

fn range_of(value: &Value) -> Option<&RangeValue> {
    match value {
        Value::Defined(defined) => match defined.as_ref() {
            DefinedValue::Range(range) => Some(range),
            _ => None,
        },
        _ => None,
    }
}

fn next_value(value: &Value) -> SqlResult<Value> {
    match value {
        Value::Integer(number) => number
            .checked_add(1)
            .map(Value::Integer)
            .ok_or_else(|| crate::types::out_of_range(SqlType::Integer)),
        Value::BigInt(number) => number
            .checked_add(1)
            .map(Value::BigInt)
            .ok_or_else(|| crate::types::out_of_range(SqlType::BigInt)),
        Value::Date(days) => days
            .checked_add(1)
            .filter(|days| crate::datetime::valid_date(*days))
            .map(Value::Date)
            .ok_or_else(|| {
                SqlError::new(SqlState::DATETIME_FIELD_OVERFLOW, "date out of range")
            }),
        other => Ok(other.clone()),
    }
}

fn canonical(type_oid: u32, bounds: RangeBounds) -> SqlResult<RangeBounds> {
    if !is_discrete(type_oid) {
        return Ok(bounds);
    }
    let lower = match bounds.lower {
        RangeBound::Exclusive(value) => RangeBound::Inclusive(next_value(&value)?),
        other => other,
    };
    let upper = match bounds.upper {
        RangeBound::Inclusive(value) => RangeBound::Exclusive(next_value(&value)?),
        other => other,
    };
    Ok(RangeBounds { lower, upper })
}

pub(crate) fn make(type_oid: u32, lower: RangeBound, upper: RangeBound) -> SqlResult<Value> {
    let lower = match lower {
        RangeBound::Inclusive(Value::Null) | RangeBound::Exclusive(Value::Null) => {
            RangeBound::Unbounded
        }
        other => other,
    };
    let upper = match upper {
        RangeBound::Inclusive(Value::Null) | RangeBound::Exclusive(Value::Null) => {
            RangeBound::Unbounded
        }
        other => other,
    };
    if let (Some(low), Some(high)) = (lower.value(), upper.value()) {
        match compare_values(low, high) {
            Ordering::Greater => {
                return Err(SqlError::new(
                    SqlState::DATA_EXCEPTION,
                    "range lower bound must be less than or equal to range upper bound",
                ));
            }
            Ordering::Equal if !(lower.is_inclusive() && upper.is_inclusive()) => {
                return Ok(range_value(RangeValue {
                    type_oid,
                    bounds: None,
                }));
            }
            _ => {}
        }
    }
    let bounds = canonical(type_oid, RangeBounds { lower, upper })?;
    if let (Some(low), Some(high)) = (bounds.lower.value(), bounds.upper.value())
        && compare_values(low, high) != Ordering::Less
        && !(bounds.lower.is_inclusive() && bounds.upper.is_inclusive())
    {
        return Ok(range_value(RangeValue {
            type_oid,
            bounds: None,
        }));
    }
    Ok(range_value(RangeValue {
        type_oid,
        bounds: Some(bounds),
    }))
}

fn needs_quotes(text: &str) -> bool {
    text.is_empty()
        || text.chars().any(|c| {
            matches!(c, '"' | '\\' | '(' | ')' | '[' | ']' | ',') || c.is_ascii_whitespace()
        })
}

fn push_bound(out: &mut String, value: Option<&Value>) {
    let Some(text) = value.and_then(Value::to_text) else {
        return;
    };
    if !needs_quotes(&text) {
        out.push_str(&text);
        return;
    }
    out.push('"');
    for character in text.chars() {
        if character == '"' || character == '\\' {
            out.push(character);
        }
        out.push(character);
    }
    out.push('"');
}

pub(crate) fn format_text(range: &RangeValue) -> String {
    let Some(bounds) = &range.bounds else {
        return "empty".to_owned();
    };
    let mut out = String::new();
    out.push(if bounds.lower.is_inclusive() { '[' } else { '(' });
    push_bound(&mut out, bounds.lower.value());
    out.push(',');
    push_bound(&mut out, bounds.upper.value());
    out.push(if bounds.upper.is_inclusive() { ']' } else { ')' });
    out
}

fn malformed(text: &str, detail: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("malformed range literal: \"{text}\""),
    )
    .with_detail(detail.to_owned())
}

fn read_bound(
    text: &str,
    characters: &mut std::iter::Peekable<std::str::Chars<'_>>,
    ends: &[char],
) -> SqlResult<Option<String>> {
    let mut bound = String::new();
    let mut quoted = false;
    let mut any_quotes = false;
    loop {
        match characters.peek().copied() {
            None => return Err(malformed(text, "Unexpected end of input.")),
            Some('"') => {
                characters.next();
                any_quotes = true;
                if quoted && characters.peek() == Some(&'"') {
                    characters.next();
                    bound.push('"');
                } else {
                    quoted = !quoted;
                }
            }
            Some('\\') => {
                characters.next();
                match characters.next() {
                    Some(escaped) => bound.push(escaped),
                    None => return Err(malformed(text, "Unexpected end of input.")),
                }
            }
            Some(character) if !quoted && ends.contains(&character) => break,
            Some(character) => {
                characters.next();
                bound.push(character);
            }
        }
    }
    Ok((any_quotes || !bound.is_empty()).then_some(bound))
}

pub(crate) fn parse_text(text: &str, type_oid: u32, subtype: SqlType) -> SqlResult<Value> {
    let trimmed = text.trim_matches(|c: char| c.is_ascii_whitespace());
    if trimmed.eq_ignore_ascii_case("empty") {
        return Ok(range_value(RangeValue {
            type_oid,
            bounds: None,
        }));
    }
    let mut characters = trimmed.chars().peekable();
    let lower_inclusive = match characters.next() {
        Some('[') => true,
        Some('(') => false,
        _ => return Err(malformed(text, "Missing left parenthesis or bracket.")),
    };
    let lower = read_bound(text, &mut characters, &[','])?;
    if characters.next() != Some(',') {
        return Err(malformed(text, "Missing comma after lower bound."));
    }
    let upper = read_bound(text, &mut characters, &[']', ')'])?;
    let upper_inclusive = match characters.next() {
        Some(']') => true,
        Some(')') => false,
        _ => return Err(malformed(text, "Too many commas.")),
    };
    if characters.next().is_some() {
        return Err(malformed(text, "Junk after right parenthesis or bracket."));
    }
    let bound = |found: Option<String>, inclusive: bool| -> SqlResult<RangeBound> {
        Ok(match found {
            None => RangeBound::Unbounded,
            Some(found) => {
                let value = crate::types::parse_input(&found, subtype)?;
                if inclusive {
                    RangeBound::Inclusive(value)
                } else {
                    RangeBound::Exclusive(value)
                }
            }
        })
    };
    make(
        type_oid,
        bound(lower, lower_inclusive)?,
        bound(upper, upper_inclusive)?,
    )
}

#[derive(Clone, Copy)]
struct Side<'b> {
    bound: &'b RangeBound,
    lower: bool,
}

fn compare_sides(first: Side<'_>, second: Side<'_>) -> Ordering {
    match (first.bound.value(), second.bound.value()) {
        (None, None) => match (first.lower, second.lower) {
            (true, true) | (false, false) => Ordering::Equal,
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
        },
        (None, Some(_)) => {
            if first.lower {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Some(_), None) => {
            if second.lower {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Some(a), Some(b)) => {
            let ordering = compare_values(a, b);
            if ordering != Ordering::Equal {
                return ordering;
            }
            let (a_inclusive, b_inclusive) =
                (first.bound.is_inclusive(), second.bound.is_inclusive());
            if !a_inclusive && !b_inclusive {
                if first.lower == second.lower {
                    Ordering::Equal
                } else if first.lower {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            } else if !a_inclusive {
                if first.lower {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            } else if !b_inclusive {
                if second.lower {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            } else {
                Ordering::Equal
            }
        }
    }
}

fn lower_of(bounds: &RangeBounds) -> Side<'_> {
    Side {
        bound: &bounds.lower,
        lower: true,
    }
}

fn upper_of(bounds: &RangeBounds) -> Side<'_> {
    Side {
        bound: &bounds.upper,
        lower: false,
    }
}

pub(crate) fn compare(left: &RangeValue, right: &RangeValue) -> Ordering {
    match (&left.bounds, &right.bounds) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(a), Some(b)) => compare_sides(lower_of(a), lower_of(b))
            .then_with(|| compare_sides(upper_of(a), upper_of(b))),
    }
}

pub(crate) fn equality_key_of(out: &mut Vec<u8>, range: &RangeValue) {
    out.extend_from_slice(&range.type_oid.to_le_bytes());
    let Some(bounds) = &range.bounds else {
        out.push(0);
        return;
    };
    out.push(1);
    for bound in [&bounds.lower, &bounds.upper] {
        match bound {
            RangeBound::Unbounded => out.push(0),
            RangeBound::Inclusive(value) => {
                out.push(1);
                crate::types::equality_key(out, value);
            }
            RangeBound::Exclusive(value) => {
                out.push(2);
                crate::types::equality_key(out, value);
            }
        }
    }
}

fn contains_range(outer: &RangeValue, inner: &RangeValue) -> bool {
    let Some(inner) = &inner.bounds else {
        return true;
    };
    let Some(outer) = &outer.bounds else {
        return false;
    };
    compare_sides(lower_of(outer), lower_of(inner)) != Ordering::Greater
        && compare_sides(upper_of(outer), upper_of(inner)) != Ordering::Less
}

fn contains_element(range: &RangeValue, element: &Value) -> bool {
    let Some(bounds) = &range.bounds else {
        return false;
    };
    let above_lower = match &bounds.lower {
        RangeBound::Unbounded => true,
        RangeBound::Inclusive(low) => compare_values(low, element) != Ordering::Greater,
        RangeBound::Exclusive(low) => compare_values(low, element) == Ordering::Less,
    };
    let below_upper = match &bounds.upper {
        RangeBound::Unbounded => true,
        RangeBound::Inclusive(high) => compare_values(element, high) != Ordering::Greater,
        RangeBound::Exclusive(high) => compare_values(element, high) == Ordering::Less,
    };
    above_lower && below_upper
}

fn overlaps(left: &RangeBounds, right: &RangeBounds) -> bool {
    (compare_sides(lower_of(left), lower_of(right)) != Ordering::Less
        && compare_sides(lower_of(left), upper_of(right)) != Ordering::Greater)
        || (compare_sides(lower_of(right), lower_of(left)) != Ordering::Less
            && compare_sides(lower_of(right), upper_of(left)) != Ordering::Greater)
}

fn touching(upper: &RangeBound, lower: &RangeBound) -> bool {
    match (upper.value(), lower.value()) {
        (Some(a), Some(b)) => {
            compare_values(a, b) == Ordering::Equal && upper.is_inclusive() != lower.is_inclusive()
        }
        _ => false,
    }
}

fn adjacent(left: &RangeBounds, right: &RangeBounds) -> bool {
    touching(&left.upper, &right.lower) || touching(&right.upper, &left.lower)
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

fn same_type(left: &RangeValue, right: &RangeValue, symbol: &str, values: (&Value, &Value)) -> SqlResult<()> {
    if left.type_oid == right.type_oid {
        Ok(())
    } else {
        Err(no_operator(symbol, values.0, values.1))
    }
}

pub(crate) fn containment(contains: bool, left: &Value, right: &Value) -> Option<SqlResult<Value>> {
    let (outer, inner) = if contains { (left, right) } else { (right, left) };
    let symbol = if contains { "@>" } else { "<@" };
    let range = range_of(outer)?;
    if inner.is_null() {
        return Some(Ok(Value::Null));
    }
    Some(match range_of(inner) {
        Some(other) => same_type(range, other, symbol, (left, right))
            .map(|()| Value::Boolean(contains_range(range, other))),
        None => {
            let converted = subtype_of(range.type_oid).and_then(|subtype| {
                crate::types::convert(inner.clone(), subtype, crate::types::Conversion::Assignment)
            });
            match converted {
                Ok(element) => Ok(Value::Boolean(contains_element(range, &element))),
                Err(_) => Err(no_operator(symbol, left, right)),
            }
        }
    })
}

fn side_test(name: &str, left: &RangeValue, right: &RangeValue) -> Option<bool> {
    let (Some(a), Some(b)) = (&left.bounds, &right.bounds) else {
        return Some(false);
    };
    Some(match name {
        OVERLAPS => overlaps(a, b),
        LEFT_OF => compare_sides(upper_of(a), lower_of(b)) == Ordering::Less,
        RIGHT_OF => compare_sides(lower_of(a), upper_of(b)) == Ordering::Greater,
        NOT_RIGHT_OF => compare_sides(upper_of(a), upper_of(b)) != Ordering::Greater,
        NOT_LEFT_OF => compare_sides(lower_of(a), lower_of(b)) != Ordering::Less,
        ADJACENT => adjacent(a, b),
        _ => return None,
    })
}

pub(crate) fn operator(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    let symbol = OPERATORS.iter().find(|(_, internal)| *internal == name)?.0;
    let [left, right] = values else {
        return None;
    };
    if left.is_null() || right.is_null() {
        return Some(Ok(Value::Null));
    }
    if name == OVERLAPS
        && let (Value::Array(a), Value::Array(b)) = (left, right)
    {
        let shared = a.items.iter().any(|item| {
            !item.is_null()
                && b.items
                    .iter()
                    .any(|other| !other.is_null() && compare_values(item, other) == Ordering::Equal)
        });
        return Some(Ok(Value::Boolean(shared)));
    }
    if matches!(name, LEFT_OF | RIGHT_OF)
        && let (Some(a), Some(b)) = (left.as_integer(), right.as_integer())
    {
        let shift = u32::try_from(b).unwrap_or(u32::MAX);
        let shifted = if name == LEFT_OF {
            a.checked_shl(shift).unwrap_or(0)
        } else {
            a.checked_shr(shift).unwrap_or(if a < 0 { -1 } else { 0 })
        };
        let sql_type = left.sql_type().unwrap_or(SqlType::BigInt);
        return Some(crate::types::integer_value(shifted, sql_type).or_else(|_| {
            crate::types::integer_value(shifted as i32 as i64, SqlType::Integer)
        }));
    }
    let (Some(a), Some(b)) = (range_of(left), range_of(right)) else {
        return Some(Err(no_operator(symbol, left, right)));
    };
    if let Err(error) = same_type(a, b, symbol, (left, right)) {
        return Some(Err(error));
    }
    side_test(name, a, b).map(|found| Ok(Value::Boolean(found)))
}

fn bound_extreme<'b>(
    first: Side<'b>,
    second: Side<'b>,
    wanted: Ordering,
) -> &'b RangeBound {
    if compare_sides(first, second) == wanted {
        first.bound
    } else {
        second.bound
    }
}

pub(crate) fn arithmetic(
    operator: BinaryOperator,
    left: &Value,
    right: &Value,
) -> Option<SqlResult<Value>> {
    let (Some(a), Some(b)) = (range_of(left), range_of(right)) else {
        return None;
    };
    let symbol = match operator {
        BinaryOperator::Add => "+",
        BinaryOperator::Multiply => "*",
        BinaryOperator::Subtract => "-",
        _ => return Some(Err(no_operator(operator.symbol(), left, right))),
    };
    if let Err(error) = same_type(a, b, symbol, (left, right)) {
        return Some(Err(error));
    }
    let empty = || {
        Ok(range_value(RangeValue {
            type_oid: a.type_oid,
            bounds: None,
        }))
    };
    let (Some(x), Some(y)) = (&a.bounds, &b.bounds) else {
        return Some(match operator {
            BinaryOperator::Add if a.bounds.is_none() => Ok(right.clone()),
            BinaryOperator::Add | BinaryOperator::Subtract => Ok(left.clone()),
            _ => empty(),
        });
    };
    Some(match operator {
        BinaryOperator::Add => {
            if !overlaps(x, y) && !adjacent(x, y) {
                Err(SqlError::new(
                    SqlState::DATA_EXCEPTION,
                    "result of range union would not be contiguous",
                ))
            } else {
                make(
                    a.type_oid,
                    bound_extreme(lower_of(x), lower_of(y), Ordering::Less).clone(),
                    bound_extreme(upper_of(x), upper_of(y), Ordering::Greater).clone(),
                )
            }
        }
        BinaryOperator::Multiply => {
            if !overlaps(x, y) {
                empty()
            } else {
                make(
                    a.type_oid,
                    bound_extreme(lower_of(x), lower_of(y), Ordering::Greater).clone(),
                    bound_extreme(upper_of(x), upper_of(y), Ordering::Less).clone(),
                )
            }
        }
        _ => difference(a.type_oid, x, y),
    })
}

fn difference(type_oid: u32, x: &RangeBounds, y: &RangeBounds) -> SqlResult<Value> {
    let lower_lower = compare_sides(lower_of(x), lower_of(y));
    let lower_upper = compare_sides(lower_of(x), upper_of(y));
    let upper_lower = compare_sides(upper_of(x), lower_of(y));
    let upper_upper = compare_sides(upper_of(x), upper_of(y));
    if lower_lower == Ordering::Less && upper_upper == Ordering::Greater {
        return Err(SqlError::new(
            SqlState::DATA_EXCEPTION,
            "result of range difference would not be contiguous",
        ));
    }
    if lower_upper == Ordering::Greater || upper_lower == Ordering::Less {
        return make(type_oid, x.lower.clone(), x.upper.clone());
    }
    if lower_lower != Ordering::Less && upper_upper != Ordering::Greater {
        return Ok(range_value(RangeValue {
            type_oid,
            bounds: None,
        }));
    }
    if lower_lower != Ordering::Greater
        && upper_lower != Ordering::Less
        && upper_upper != Ordering::Greater
    {
        return make(type_oid, x.lower.clone(), y.lower.flipped());
    }
    make(type_oid, y.upper.flipped(), x.upper.clone())
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    let [value] = values else {
        return None;
    };
    let range = range_of(value)?;
    let bounds = range.bounds.as_ref();
    Some(Ok(match name {
        "lower" => bounds
            .and_then(|bounds| bounds.lower.value().cloned())
            .unwrap_or(Value::Null),
        "upper" => bounds
            .and_then(|bounds| bounds.upper.value().cloned())
            .unwrap_or(Value::Null),
        "isempty" => Value::Boolean(bounds.is_none()),
        "lower_inc" => Value::Boolean(bounds.is_some_and(|bounds| bounds.lower.is_inclusive())),
        "upper_inc" => Value::Boolean(bounds.is_some_and(|bounds| bounds.upper.is_inclusive())),
        "lower_inf" => Value::Boolean(
            bounds.is_some_and(|bounds| matches!(bounds.lower, RangeBound::Unbounded)),
        ),
        _ => Value::Boolean(
            bounds.is_some_and(|bounds| matches!(bounds.upper, RangeBound::Unbounded)),
        ),
    }))
}

pub(crate) fn constructor(name: &str) -> Option<u32> {
    if let Some(range) = builtin_named(name) {
        return Some(range.oid);
    }
    let registry = crate::user_types::current()?;
    let found = registry.resolve(None, name)?;
    (found.kind() == DefinedKind::Range).then_some(found.oid)
}

pub(crate) fn construct(
    evaluator: &Evaluator<'_>,
    type_oid: u32,
    arguments: &[Expr],
) -> SqlResult<Value> {
    let subtype = subtype_of(type_oid)?;
    let (low, high, flags) = match arguments {
        [low, high] => (low, high, None),
        [low, high, flags] => (low, high, Some(flags)),
        _ => {
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!(
                    "function {}() does not exist with {} arguments",
                    crate::user_types::type_name(DefinedType {
                        kind: DefinedKind::Range,
                        oid: type_oid,
                    }),
                    arguments.len()
                ),
            ));
        }
    };
    let convert = |value: Value| {
        crate::types::convert(value, subtype, crate::types::Conversion::Assignment)
    };
    let low = convert(evaluator.eval(low, Some(subtype))?)?;
    let high = convert(evaluator.eval(high, Some(subtype))?)?;
    let flags = match flags {
        Some(flags) => match evaluator.eval(flags, Some(SqlType::Text))? {
            Value::Null => {
                return Err(SqlError::new(
                    SqlState::NULL_VALUE_NOT_ALLOWED,
                    "range constructor flags argument must not be null",
                ));
            }
            other => other.to_text().unwrap_or_default(),
        },
        None => "[)".to_owned(),
    };
    bounded(type_oid, low, high, &flags)
}

pub(crate) fn construct_from_values(type_oid: u32, values: &[Value]) -> SqlResult<Value> {
    let subtype = subtype_of(type_oid)?;
    let (low, high, flags) = match values {
        [low, high] => (low, high, None),
        [low, high, flags] => (low, high, Some(flags)),
        _ => {
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("a range constructor takes 2 or 3 arguments, not {}", values.len()),
            ));
        }
    };
    let convert = |value: &Value| {
        crate::types::convert(value.clone(), subtype, crate::types::Conversion::Explicit)
    };
    let flags = match flags {
        Some(Value::Null) => {
            return Err(SqlError::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                "range constructor flags argument must not be null",
            ));
        }
        Some(flags) => flags.to_text().unwrap_or_default(),
        None => "[)".to_owned(),
    };
    bounded(type_oid, convert(low)?, convert(high)?, &flags)
}

fn bounded(type_oid: u32, low: Value, high: Value, flags: &str) -> SqlResult<Value> {
    let (lower_inclusive, upper_inclusive) = match flags {
        "[]" => (true, true),
        "[)" => (true, false),
        "(]" => (false, true),
        "()" => (false, false),
        _ => {
            return Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                "invalid range bound flags",
            )
            .with_hint("Valid values are \"[]\", \"[)\", \"(]\", and \"()\"."));
        }
    };
    let bound = |value: Value, inclusive: bool| {
        if value.is_null() {
            RangeBound::Unbounded
        } else if inclusive {
            RangeBound::Inclusive(value)
        } else {
            RangeBound::Exclusive(value)
        }
    };
    make(
        type_oid,
        bound(low, lower_inclusive),
        bound(high, upper_inclusive),
    )
}

pub(crate) fn function_type(name: &str, argument: Option<SqlType>) -> Option<Option<SqlType>> {
    if OPERATORS.iter().any(|(_, internal)| *internal == name) {
        return Some(match (name, argument) {
            (LEFT_OF | RIGHT_OF, Some(sql_type)) if sql_type.is_numeric() => Some(sql_type),
            _ => Some(SqlType::Boolean),
        });
    }
    if let Some(oid) = constructor(name) {
        return Some(Some(DefinedType::range(oid)));
    }
    let range_type = argument
        .and_then(SqlType::defined)
        .filter(|defined| defined.kind == DefinedKind::Range)?;
    Some(match name {
        "lower" | "upper" => crate::user_types::range_subtype(range_type.oid),
        "isempty" | "lower_inc" | "upper_inc" | "lower_inf" | "upper_inf" => {
            Some(SqlType::Boolean)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{RangeBound, RangeValue, compare, format_text, make, parse_text};
    use crate::types::{DefinedValue, SqlType, Value};
    use std::cmp::Ordering;

    fn range(value: Value) -> RangeValue {
        match value {
            Value::Defined(defined) => match *defined {
                DefinedValue::Range(range) => range,
                other => panic!("not a range: {other:?}"),
            },
            other => panic!("not a range: {other:?}"),
        }
    }

    fn int4(text: &str) -> RangeValue {
        range(parse_text(text, 3904, SqlType::Integer).unwrap())
    }

    #[test]
    fn discrete_ranges_take_postgresqls_canonical_form() {
        assert_eq!(format_text(&int4("[1,5]")), "[1,6)");
        assert_eq!(format_text(&int4("(1,5)")), "[2,5)");
        assert_eq!(format_text(&int4("(1,2)")), "empty");
        assert_eq!(format_text(&int4("[3,3)")), "empty");
        assert_eq!(format_text(&int4("[3,3]")), "[3,4)");
        assert_eq!(format_text(&int4("[,5)")), "(,5)");
        assert_eq!(format_text(&int4("(,)")), "(,)");
        assert_eq!(format_text(&int4(" EMPTY ")), "empty");
    }

    #[test]
    fn continuous_ranges_keep_their_bounds() {
        let numeric = range(parse_text("(1.5,\"2.5\"]", 3906, SqlType::Numeric(None)).unwrap());
        assert_eq!(format_text(&numeric), "(1.5,2.5]");
        let reversed = parse_text("[5,1]", 3906, SqlType::Numeric(None));
        assert!(reversed.is_err());
    }

    #[test]
    fn malformed_ranges_are_errors_never_panics() {
        for text in ["", "[", "[1", "[1,", "[1,2", "1,2]", "[1,2]x", "[1,2,3]", "[\"1", "[\\"] {
            assert!(parse_text(text, 3904, SqlType::Integer).is_err(), "{text}");
        }
    }

    #[test]
    fn ranges_order_empty_first_then_by_bounds() {
        assert_eq!(compare(&int4("empty"), &int4("[1,2)")), Ordering::Less);
        assert_eq!(compare(&int4("[1,2)"), &int4("[1,3)")), Ordering::Less);
        assert_eq!(compare(&int4("(,2)"), &int4("[1,3)")), Ordering::Less);
        assert_eq!(compare(&int4("[1,)"), &int4("[1,3)")), Ordering::Greater);
        let made = range(
            make(
                3904,
                RangeBound::Inclusive(Value::Integer(1)),
                RangeBound::Inclusive(Value::Integer(1)),
            )
            .unwrap(),
        );
        assert_eq!(format_text(&made), "[1,2)");
    }
}
