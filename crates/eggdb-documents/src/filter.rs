use crate::error::{DocumentError, DocumentResult};
use crate::path::{Reached, reached};
use eggdb_bson::{Bson, Document, compare, type_rank};
use std::cmp::Ordering;

const MAX_DEPTH: usize = 100;

/// A parsed query filter, as `find`, `update`, `delete` and `$match` take it.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    /// Every one holds.
    And(Vec<Filter>),
    /// At least one holds.
    Or(Vec<Filter>),
    /// None holds.
    Nor(Vec<Filter>),
    /// The values at a dotted path meet a condition.
    Field {
        /// The dotted path.
        path: String,
        /// The condition.
        condition: Condition,
    },
}

/// A condition on the values at one path.
#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    /// Equal to the value (`$eq`, or a plain value).
    Eq(Bson),
    /// Not equal (`$ne`).
    Ne(Bson),
    /// Greater than (`$gt`).
    Gt(Bson),
    /// Greater than or equal (`$gte`).
    Gte(Bson),
    /// Less than (`$lt`).
    Lt(Bson),
    /// Less than or equal (`$lte`).
    Lte(Bson),
    /// Equal to one of the values (`$in`).
    In(Vec<Bson>),
    /// Equal to none of the values (`$nin`).
    Nin(Vec<Bson>),
    /// The path holds a value, or does not (`$exists`).
    Exists(bool),
    /// The value has one of the types, by number (`$type`); `number` is `0`.
    Type(Vec<i32>),
    /// A string matching the regular expression (`$regex` and `$options`).
    Regex {
        /// The pattern.
        pattern: String,
        /// The option letters.
        options: String,
    },
    /// An array of this length (`$size`).
    Size(i64),
    /// An array holding all of these (`$all`).
    All(Vec<Bson>),
    /// An array with an element meeting every condition (`$elemMatch` with operators).
    ElemMatchValue(Vec<Condition>),
    /// An array with a document element matching the filter (`$elemMatch` with fields).
    ElemMatchDocument(Box<Filter>),
    /// A number whose remainder by the divisor is the remainder (`$mod`).
    Mod(i64, i64),
    /// The condition does not hold (`$not`).
    Not(Box<Condition>),
    /// Every condition holds (several operators on one path).
    Every(Vec<Condition>),
}

fn is_operator_document(value: &Bson) -> bool {
    value.as_document().and_then(Document::first).is_some_and(|(name, _)| name.starts_with('$'))
}

fn filters_of(value: &Bson, operator: &str, depth: usize) -> DocumentResult<Vec<Filter>> {
    let Bson::Array(items) = value else {
        return Err(DocumentError::bad_value(format!("{operator} must be an array")));
    };
    if items.is_empty() {
        return Err(DocumentError::bad_value(format!("{operator} argument must be a non-empty array")));
    }
    items
        .iter()
        .map(|item| match item {
            Bson::Document(document) => parse_at(document, depth + 1),
            _ => Err(DocumentError::bad_value(format!("{operator} argument's entries must be objects"))),
        })
        .collect()
}

/// Reads a query filter document.
pub fn parse_filter(filter: &Document) -> DocumentResult<Filter> {
    parse_at(filter, 0)
}

fn parse_at(filter: &Document, depth: usize) -> DocumentResult<Filter> {
    if depth > MAX_DEPTH {
        return Err(DocumentError::bad_value("the filter is nested too deeply"));
    }
    let mut all = Vec::new();
    for (name, value) in filter.iter() {
        match name.as_str() {
            "$and" => all.push(Filter::And(filters_of(value, "$and", depth)?)),
            "$or" => all.push(Filter::Or(filters_of(value, "$or", depth)?)),
            "$nor" => all.push(Filter::Nor(filters_of(value, "$nor", depth)?)),
            "$comment" => {}
            "$where" | "$expr" | "$text" | "$jsonSchema" => {
                return Err(DocumentError::not_implemented(format!("{name} is not supported yet")));
            }
            other if other.starts_with('$') => {
                return Err(DocumentError::bad_value(format!("unknown top level operator: {other}")));
            }
            path => all.push(Filter::Field { path: path.to_owned(), condition: parse_condition(value, depth)? }),
        }
    }
    Ok(if all.len() == 1 { all.remove(0) } else { Filter::And(all) })
}

fn number_of(value: &Bson, operator: &str) -> DocumentResult<i64> {
    match value {
        Bson::Double(number) if number.fract() != 0.0 => {
            Err(DocumentError::bad_value(format!("{operator} must be a whole number")))
        }
        other => other.as_i64().ok_or_else(|| DocumentError::bad_value(format!("{operator} needs a number"))),
    }
}

fn type_code(value: &Bson) -> DocumentResult<i32> {
    if let Some(number) = value.as_i64() {
        return Ok(number as i32);
    }
    let code = match value.as_str() {
        Some("double") => 1,
        Some("string") => 2,
        Some("object") => 3,
        Some("array") => 4,
        Some("binData") => 5,
        Some("undefined") => 6,
        Some("objectId") => 7,
        Some("bool") => 8,
        Some("date") => 9,
        Some("null") => 10,
        Some("regex") => 11,
        Some("dbPointer") => 12,
        Some("javascript") => 13,
        Some("symbol") => 14,
        Some("javascriptWithScope") => 15,
        Some("int") => 16,
        Some("timestamp") => 17,
        Some("long") => 18,
        Some("decimal") => 19,
        Some("minKey") => -1,
        Some("maxKey") => 127,
        Some("number") => 0,
        _ => return Err(DocumentError::bad_value("unknown type name alias in $type")),
    };
    Ok(code)
}

fn array_of(value: &Bson, operator: &str) -> DocumentResult<Vec<Bson>> {
    match value {
        Bson::Array(items) => Ok(items.clone()),
        _ => Err(DocumentError::bad_value(format!("{operator} needs an array"))),
    }
}

fn parse_condition(value: &Bson, depth: usize) -> DocumentResult<Condition> {
    if depth > MAX_DEPTH {
        return Err(DocumentError::bad_value("the filter is nested too deeply"));
    }
    let Some(operators) = value.as_document().filter(|_| is_operator_document(value)) else {
        return Ok(match value {
            Bson::Regex { pattern, options } => Condition::Regex { pattern: pattern.clone(), options: options.clone() },
            other => Condition::Eq(other.clone()),
        });
    };
    let mut conditions = Vec::new();
    let regex_options = operators.get_str("$options").map(str::to_owned);
    for (name, argument) in operators.iter() {
        let condition = match name.as_str() {
            "$eq" => Condition::Eq(argument.clone()),
            "$ne" => Condition::Ne(argument.clone()),
            "$gt" => Condition::Gt(argument.clone()),
            "$gte" => Condition::Gte(argument.clone()),
            "$lt" => Condition::Lt(argument.clone()),
            "$lte" => Condition::Lte(argument.clone()),
            "$in" => Condition::In(array_of(argument, "$in")?),
            "$nin" => Condition::Nin(array_of(argument, "$nin")?),
            "$exists" => Condition::Exists(argument.is_truthy()),
            "$type" => Condition::Type(match argument {
                Bson::Array(items) => items.iter().map(type_code).collect::<DocumentResult<_>>()?,
                single => vec![type_code(single)?],
            }),
            "$regex" => match argument {
                Bson::String(pattern) => Condition::Regex {
                    pattern: pattern.clone(),
                    options: regex_options.clone().unwrap_or_default(),
                },
                Bson::Regex { pattern, options } => Condition::Regex {
                    pattern: pattern.clone(),
                    options: regex_options.clone().unwrap_or_else(|| options.clone()),
                },
                _ => return Err(DocumentError::bad_value("$regex has to be a string")),
            },
            "$options" => {
                if !operators.contains_key("$regex") {
                    return Err(DocumentError::bad_value("$options needs a $regex"));
                }
                continue;
            }
            "$size" => Condition::Size(number_of(argument, "$size")?),
            "$all" => Condition::All(array_of(argument, "$all")?),
            "$elemMatch" => {
                let Bson::Document(inner) = argument else {
                    return Err(DocumentError::bad_value("$elemMatch needs an Object"));
                };
                if is_operator_document(argument)
                    && !matches!(inner.first(), Some((name, _)) if matches!(name.as_str(), "$and" | "$or" | "$nor"))
                {
                    match parse_condition(argument, depth + 1)? {
                        Condition::Every(all) => Condition::ElemMatchValue(all),
                        single => Condition::ElemMatchValue(vec![single]),
                    }
                } else {
                    Condition::ElemMatchDocument(Box::new(parse_at(inner, depth + 1)?))
                }
            }
            "$mod" => {
                let items = array_of(argument, "$mod")?;
                let [divisor, remainder] = items.as_slice() else {
                    return Err(DocumentError::bad_value("malformed mod, not enough elements"));
                };
                let divisor = number_of(divisor, "$mod")?;
                if divisor == 0 {
                    return Err(DocumentError::bad_value("divisor cannot be 0"));
                }
                Condition::Mod(divisor, number_of(remainder, "$mod")?)
            }
            "$not" => match argument {
                Bson::Regex { pattern, options } => {
                    Condition::Not(Box::new(Condition::Regex { pattern: pattern.clone(), options: options.clone() }))
                }
                Bson::Document(inner) if is_operator_document(argument) && !inner.is_empty() => {
                    Condition::Not(Box::new(parse_condition(argument, depth + 1)?))
                }
                _ => return Err(DocumentError::bad_value("$not needs a regex or a document")),
            },
            other => return Err(DocumentError::bad_value(format!("unknown operator: {other}"))),
        };
        conditions.push(condition);
    }
    Ok(if conditions.len() == 1 { conditions.remove(0) } else { Condition::Every(conditions) })
}

fn equal(value: &Bson, wanted: &Bson) -> bool {
    match wanted {
        Bson::Regex { pattern, options } if !matches!(value, Bson::Regex { .. }) => regex_matches(value, pattern, options),
        _ => compare(value, wanted) == Ordering::Equal,
    }
}

fn regex_matches(value: &Bson, pattern: &str, options: &str) -> bool {
    match value {
        Bson::String(text) | Bson::Symbol(text) => {
            eggdb_sql::regex_matches_with_flags(text, pattern, options).unwrap_or(false)
        }
        Bson::Regex { pattern: stored, options: stored_options } => stored == pattern && stored_options == options,
        _ => false,
    }
}

fn ordered(value: &Bson, bound: &Bson, accept: fn(Ordering) -> bool) -> bool {
    let same_kind = type_rank(value) == type_rank(bound)
        || matches!((value, bound), (Bson::MinKey | Bson::MaxKey, _) | (_, Bson::MinKey | Bson::MaxKey));
    same_kind && accept(compare(value, bound))
}

fn leaf_values<'d>(reached: &[Reached<'d>]) -> Vec<Option<&'d Bson>> {
    let mut out = Vec::new();
    for item in reached {
        match item {
            Reached::Value(value) => {
                out.push(Some(*value));
                if let Bson::Array(items) = value {
                    out.extend(items.iter().map(Some));
                }
            }
            Reached::Missing => out.push(None),
        }
    }
    out
}

fn any_leaf(reached: &[Reached<'_>], test: impl Fn(Option<&Bson>) -> bool) -> bool {
    leaf_values(reached).into_iter().any(test)
}

fn condition_holds(condition: &Condition, reached: &[Reached<'_>]) -> bool {
    match condition {
        Condition::Eq(Bson::Null) => any_leaf(reached, |value| matches!(value, None | Some(Bson::Null | Bson::Undefined))),
        Condition::Eq(wanted) => any_leaf(reached, |value| value.is_some_and(|value| equal(value, wanted))),
        Condition::Ne(wanted) => !condition_holds(&Condition::Eq(wanted.clone()), reached),
        Condition::Gt(bound) => any_leaf(reached, |value| value.is_some_and(|value| ordered(value, bound, Ordering::is_gt))),
        Condition::Gte(bound) => {
            (matches!(bound, Bson::Null) && condition_holds(&Condition::Eq(Bson::Null), reached))
                || any_leaf(reached, |value| value.is_some_and(|value| ordered(value, bound, Ordering::is_ge)))
        }
        Condition::Lt(bound) => any_leaf(reached, |value| value.is_some_and(|value| ordered(value, bound, Ordering::is_lt))),
        Condition::Lte(bound) => {
            (matches!(bound, Bson::Null) && condition_holds(&Condition::Eq(Bson::Null), reached))
                || any_leaf(reached, |value| value.is_some_and(|value| ordered(value, bound, Ordering::is_le)))
        }
        Condition::In(values) => values.iter().any(|wanted| condition_holds(&Condition::Eq(wanted.clone()), reached)),
        Condition::Nin(values) => !condition_holds(&Condition::In(values.clone()), reached),
        Condition::Exists(wanted) => reached.iter().any(|item| matches!(item, Reached::Value(_))) == *wanted,
        Condition::Type(codes) => any_leaf(reached, |value| {
            value.is_some_and(|value| {
                codes.iter().any(|code| *code == value.type_number() || (*code == 0 && value.is_number()))
            })
        }) || reached.iter().any(|item| matches!(item, Reached::Value(Bson::Array(_))) && codes.contains(&4)),
        Condition::Regex { pattern, options } => {
            any_leaf(reached, |value| value.is_some_and(|value| regex_matches(value, pattern, options)))
        }
        Condition::Size(length) => arrays(reached).any(|items| items.len() as i64 == *length),
        Condition::All(values) => {
            !values.is_empty()
                && values.iter().all(|wanted| match wanted {
                    Bson::Document(inner) if inner.get("$elemMatch").is_some() => {
                        parse_condition(wanted, 1).is_ok_and(|condition| condition_holds(&condition, reached))
                    }
                    _ => condition_holds(&Condition::Eq(wanted.clone()), reached),
                })
        }
        Condition::ElemMatchValue(conditions) => arrays(reached).any(|items| {
            items.iter().any(|item| {
                let one = [Reached::Value(item)];
                conditions.iter().all(|condition| condition_holds(condition, &one))
            })
        }),
        Condition::ElemMatchDocument(filter) => arrays(reached).any(|items| {
            items.iter().any(|item| match item {
                Bson::Document(inner) => matches(filter, inner),
                _ => false,
            })
        }),
        Condition::Mod(divisor, remainder) => any_leaf(reached, |value| {
            value.and_then(|value| value.as_f64()).is_some_and(|number| {
                number.is_finite() && (number.trunc() as i64).wrapping_rem(*divisor) == *remainder
            })
        }),
        Condition::Not(inner) => !condition_holds(inner, reached),
        Condition::Every(conditions) => conditions.iter().all(|condition| condition_holds(condition, reached)),
    }
}

fn arrays<'r, 'd>(reached: &'r [Reached<'d>]) -> impl Iterator<Item = &'d Vec<Bson>> + 'r {
    reached.iter().filter_map(|item| match item {
        Reached::Value(Bson::Array(items)) => Some(items),
        _ => None,
    })
}

/// Whether `document` matches `filter`.
pub fn matches(filter: &Filter, document: &Document) -> bool {
    match filter {
        Filter::And(all) => all.iter().all(|inner| matches(inner, document)),
        Filter::Or(any) => any.iter().any(|inner| matches(inner, document)),
        Filter::Nor(none) => !none.iter().any(|inner| matches(inner, document)),
        Filter::Field { path, condition } => condition_holds(condition, &reached(document, path)),
    }
}

/// The `_id` an equality filter names, when the filter is only that, so a store can read the
/// one document instead of scanning.
pub fn only_id(filter: &Filter) -> Option<&Bson> {
    match filter {
        Filter::Field { path, condition: Condition::Eq(value) } if path == "_id" && !matches!(value, Bson::Regex { .. } | Bson::Document(_) | Bson::Array(_) | Bson::Null) => Some(value),
        _ => None,
    }
}
