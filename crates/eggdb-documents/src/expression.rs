use crate::error::{DocumentError, DocumentResult};
use eggdb_bson::{Bson, Document, compare};
use std::cmp::Ordering;

const MAX_DEPTH: usize = 100;

fn descend(value: Bson, parts: &[&str]) -> Option<Bson> {
    let Some((first, rest)) = parts.split_first() else {
        return Some(value);
    };
    match value {
        Bson::Document(mut inner) => descend(inner.remove(first)?, rest),
        Bson::Array(items) => Some(Bson::Array(
            items
                .into_iter()
                .filter(|item| matches!(item, Bson::Document(_) | Bson::Array(_)))
                .filter_map(|item| descend(item, parts))
                .filter(|item| !matches!(item, Bson::Array(inner) if inner.is_empty()))
                .collect(),
        )),
        _ => None,
    }
}

/// The value of a dotted field path the way aggregation expressions read it: through embedded
/// documents, and over every element of an array, giving an array of the values found.
pub fn field_value(document: &Document, path: &str) -> Option<Bson> {
    let parts: Vec<&str> = path.split('.').collect();
    let first = document.get(parts[0])?.clone();
    descend(first, &parts[1..])
}

fn arguments(argument: &Bson) -> Vec<&Bson> {
    match argument {
        Bson::Array(items) => items.iter().collect(),
        single => vec![single],
    }
}

fn exact_count<'a>(name: &str, argument: &'a Bson, count: usize) -> DocumentResult<Vec<&'a Bson>> {
    let items = arguments(argument);
    if items.len() != count {
        return Err(DocumentError::bad_value(format!("Expression {name} takes exactly {count} arguments. {} were passed in.", items.len())));
    }
    Ok(items)
}

fn number_result(value: f64, integer: Option<i64>) -> Bson {
    match integer {
        Some(whole) => match i32::try_from(whole) {
            Ok(small) => Bson::Int32(small),
            Err(_) => Bson::Int64(whole),
        },
        None => Bson::Double(value),
    }
}

fn arithmetic(name: &str, values: &[Bson]) -> DocumentResult<Bson> {
    if values.iter().any(|value| matches!(value, Bson::Null)) {
        return Ok(Bson::Null);
    }
    let all_integers = values.iter().all(|value| matches!(value, Bson::Int32(_) | Bson::Int64(_)));
    let any_long = values.iter().any(|value| matches!(value, Bson::Int64(_)));
    let mut date = None;
    let mut numbers = Vec::with_capacity(values.len());
    for value in values {
        match value {
            Bson::DateTime(milliseconds) if name == "$add" && date.is_none() => date = Some(*milliseconds),
            Bson::DateTime(milliseconds) if name == "$subtract" => numbers.push(*milliseconds as f64),
            other if other.is_number() => numbers.push(other.as_f64().unwrap_or(0.0)),
            other => {
                return Err(DocumentError::type_mismatch(format!("{name} only supports numeric types, not {}", other.type_name())));
            }
        }
    }
    let integers: Vec<i64> = values.iter().filter_map(Bson::as_i64).collect();
    let (float, integer) = match name {
        "$add" => (numbers.iter().sum::<f64>(), integers.iter().try_fold(0i64, |sum, value| sum.checked_add(*value))),
        "$multiply" => (numbers.iter().product::<f64>(), integers.iter().try_fold(1i64, |product, value| product.checked_mul(*value))),
        "$subtract" => {
            if numbers.len() != 2 {
                return Err(DocumentError::bad_value("Expression $subtract takes exactly 2 arguments."));
            }
            let difference = numbers[0] - numbers[1];
            if matches!((&values[0], &values[1]), (Bson::DateTime(_), Bson::DateTime(_))) {
                return Ok(Bson::Int64(difference as i64));
            }
            if matches!(values[0], Bson::DateTime(_)) {
                return Ok(Bson::DateTime(difference as i64));
            }
            (difference, integers.first().zip(integers.get(1)).and_then(|(a, b)| a.checked_sub(*b)))
        }
        "$divide" => {
            if numbers.len() != 2 {
                return Err(DocumentError::bad_value("Expression $divide takes exactly 2 arguments."));
            }
            if numbers[1] == 0.0 {
                return Err(DocumentError::bad_value("can't $divide by zero"));
            }
            return Ok(Bson::Double(numbers[0] / numbers[1]));
        }
        "$mod" => {
            if numbers.len() != 2 {
                return Err(DocumentError::bad_value("Expression $mod takes exactly 2 arguments."));
            }
            if numbers[1] == 0.0 {
                return Err(DocumentError::bad_value("can't $mod by zero"));
            }
            (numbers[0] % numbers[1], integers.first().zip(integers.get(1)).and_then(|(a, b)| a.checked_rem(*b)))
        }
        _ => return Err(DocumentError::bad_value(format!("Unrecognized expression '{name}'"))),
    };
    if let Some(milliseconds) = date {
        return Ok(Bson::DateTime(milliseconds + float as i64));
    }
    Ok(if all_integers {
        match integer {
            Some(whole) if any_long => Bson::Int64(whole),
            Some(whole) => number_result(whole as f64, Some(whole)),
            None => Bson::Double(float),
        }
    } else {
        Bson::Double(float)
    })
}

fn string_of(value: &Bson) -> Option<String> {
    Some(match value {
        Bson::String(text) | Bson::Symbol(text) => text.clone(),
        Bson::Int32(number) => number.to_string(),
        Bson::Int64(number) => number.to_string(),
        Bson::Double(number) => number.to_string(),
        Bson::Boolean(flag) => flag.to_string(),
        Bson::ObjectId(id) => id.iter().map(|byte| format!("{byte:02x}")).collect(),
        Bson::DateTime(milliseconds) => milliseconds.to_string(),
        _ => return None,
    })
}

/// Evaluates an aggregation expression against `root`: field paths (`"$a.b"`), `$$ROOT`,
/// literals, object and array expressions, and the operators MongoDB documents for
/// arithmetic, comparison, logic, conditions, strings and arrays. `None` is a missing value.
pub fn evaluate(expression: &Bson, root: &Document) -> DocumentResult<Option<Bson>> {
    evaluate_at(expression, root, 0)
}

fn value_of(expression: &Bson, root: &Document, depth: usize) -> DocumentResult<Bson> {
    Ok(evaluate_at(expression, root, depth)?.unwrap_or(Bson::Null))
}

fn evaluate_at(expression: &Bson, root: &Document, depth: usize) -> DocumentResult<Option<Bson>> {
    if depth > MAX_DEPTH {
        return Err(DocumentError::bad_value("the expression is nested too deeply"));
    }
    match expression {
        Bson::String(text) if text.starts_with("$$") => {
            let (variable, rest) = text[2..].split_once('.').map_or((&text[2..], None), |(name, rest)| (name, Some(rest)));
            match variable {
                "ROOT" | "CURRENT" => Ok(match rest {
                    None => Some(Bson::Document(root.clone())),
                    Some(path) => field_value(root, path),
                }),
                "REMOVE" => Ok(None),
                other => Err(DocumentError::bad_value(format!("Use of undefined variable: {other}"))),
            }
        }
        Bson::String(text) if text.starts_with('$') => {
            if text.len() == 1 {
                return Err(DocumentError::bad_value("'$' by itself is not a valid FieldPath"));
            }
            Ok(field_value(root, &text[1..]))
        }
        Bson::Array(items) => Ok(Some(Bson::Array(
            items.iter().map(|item| value_of(item, root, depth + 1)).collect::<DocumentResult<_>>()?,
        ))),
        Bson::Document(inner) => match inner.first() {
            Some((name, argument)) if name.starts_with('$') => {
                if inner.len() != 1 {
                    return Err(DocumentError::bad_value("an expression specification must contain exactly one field"));
                }
                operator(name, argument, root, depth + 1)
            }
            _ => {
                let mut out = Document::new();
                for (name, value) in inner.iter() {
                    if let Some(result) = evaluate_at(value, root, depth + 1)? {
                        out.insert(name.clone(), result);
                    }
                }
                Ok(Some(Bson::Document(out)))
            }
        },
        literal => Ok(Some(literal.clone())),
    }
}

fn operator(name: &str, argument: &Bson, root: &Document, depth: usize) -> DocumentResult<Option<Bson>> {
    let all = |argument: &Bson| -> DocumentResult<Vec<Bson>> {
        arguments(argument).into_iter().map(|item| value_of(item, root, depth)).collect()
    };
    Ok(Some(match name {
        "$literal" => argument.clone(),
        "$add" | "$subtract" | "$multiply" | "$divide" | "$mod" => arithmetic(name, &all(argument)?)?,
        "$abs" | "$ceil" | "$floor" | "$sqrt" => {
            let value = value_of(exact_count(name, argument, 1)?[0], root, depth)?;
            if matches!(value, Bson::Null) {
                return Ok(Some(Bson::Null));
            }
            let Some(number) = value.as_f64() else {
                return Err(DocumentError::type_mismatch(format!("{name} only supports numeric types, not {}", value.type_name())));
            };
            match (name, &value) {
                ("$abs", Bson::Int32(whole)) => whole.checked_abs().map_or(Bson::Int64(i64::from(*whole).abs()), Bson::Int32),
                ("$abs", Bson::Int64(whole)) => Bson::Int64(whole.checked_abs().ok_or_else(|| DocumentError::bad_value("can't take $abs of long long min"))?),
                ("$abs", _) => Bson::Double(number.abs()),
                (_, Bson::Int32(_) | Bson::Int64(_)) if name != "$sqrt" => value.clone(),
                ("$ceil", _) => Bson::Double(number.ceil()),
                ("$floor", _) => Bson::Double(number.floor()),
                _ => Bson::Double(number.sqrt()),
            }
        }
        "$concat" => {
            let mut out = String::new();
            for value in all(argument)? {
                match value {
                    Bson::String(text) => out.push_str(&text),
                    Bson::Null => return Ok(Some(Bson::Null)),
                    other => return Err(DocumentError::type_mismatch(format!("$concat only supports strings, not {}", other.type_name()))),
                }
            }
            Bson::String(out)
        }
        "$toUpper" | "$toLower" | "$toString" => {
            let value = value_of(exact_count(name, argument, 1)?[0], root, depth)?;
            let text = match &value {
                Bson::Null => return Ok(Some(if name == "$toString" { Bson::Null } else { Bson::String(String::new()) })),
                other => string_of(other).ok_or_else(|| DocumentError::type_mismatch(format!("{name} can't convert from {}", other.type_name())))?,
            };
            Bson::String(match name {
                "$toUpper" => text.to_uppercase(),
                "$toLower" => text.to_lowercase(),
                _ => text,
            })
        }
        "$strLenCP" => match value_of(exact_count(name, argument, 1)?[0], root, depth)? {
            Bson::String(text) => Bson::Int32(text.chars().count() as i32),
            other => return Err(DocumentError::type_mismatch(format!("$strLenCP requires a string argument, found: {}", other.type_name()))),
        },
        "$eq" | "$ne" | "$gt" | "$gte" | "$lt" | "$lte" | "$cmp" => {
            let values = exact_count(name, argument, 2)?;
            let left = value_of(values[0], root, depth)?;
            let right = value_of(values[1], root, depth)?;
            let ordering = compare(&left, &right);
            match name {
                "$eq" => Bson::Boolean(ordering == Ordering::Equal),
                "$ne" => Bson::Boolean(ordering != Ordering::Equal),
                "$gt" => Bson::Boolean(ordering == Ordering::Greater),
                "$gte" => Bson::Boolean(ordering != Ordering::Less),
                "$lt" => Bson::Boolean(ordering == Ordering::Less),
                "$lte" => Bson::Boolean(ordering != Ordering::Greater),
                _ => Bson::Int32(match ordering {
                    Ordering::Less => -1,
                    Ordering::Equal => 0,
                    Ordering::Greater => 1,
                }),
            }
        }
        "$and" => Bson::Boolean(all(argument)?.iter().all(Bson::is_truthy)),
        "$or" => Bson::Boolean(all(argument)?.iter().any(Bson::is_truthy)),
        "$not" => Bson::Boolean(!value_of(exact_count(name, argument, 1)?[0], root, depth)?.is_truthy()),
        "$cond" => {
            let (condition, then, otherwise) = match argument {
                Bson::Array(items) if items.len() == 3 => (&items[0], &items[1], &items[2]),
                Bson::Document(parts) => match (parts.get("if"), parts.get("then"), parts.get("else")) {
                    (Some(condition), Some(then), Some(otherwise)) => (condition, then, otherwise),
                    _ => return Err(DocumentError::bad_value("$cond needs if, then and else")),
                },
                _ => return Err(DocumentError::bad_value("Expression $cond takes exactly 3 arguments.")),
            };
            let chosen = if value_of(condition, root, depth)?.is_truthy() { then } else { otherwise };
            return evaluate_at(chosen, root, depth);
        }
        "$ifNull" => {
            let items = arguments(argument);
            if items.len() < 2 {
                return Err(DocumentError::bad_value("$ifNull needs at least two arguments"));
            }
            for item in &items[..items.len() - 1] {
                match evaluate_at(item, root, depth)? {
                    Some(Bson::Null | Bson::Undefined) | None => {}
                    Some(found) => return Ok(Some(found)),
                }
            }
            return evaluate_at(items[items.len() - 1], root, depth);
        }
        "$size" => match value_of(exact_count(name, argument, 1)?[0], root, depth)? {
            Bson::Array(items) => Bson::Int32(items.len() as i32),
            other => return Err(DocumentError::bad_value(format!("The argument to $size must be an array. Type of argument: {}", other.type_name()))),
        },
        "$arrayElemAt" => {
            let values = exact_count(name, argument, 2)?;
            let array = value_of(values[0], root, depth)?;
            let index = value_of(values[1], root, depth)?;
            match (array, index.as_i64()) {
                (Bson::Null, _) => Bson::Null,
                (Bson::Array(items), Some(at)) => {
                    let at = if at < 0 { items.len() as i64 + at } else { at };
                    match usize::try_from(at).ok().and_then(|at| items.get(at)) {
                        Some(found) => found.clone(),
                        None => return Ok(None),
                    }
                }
                _ => return Err(DocumentError::bad_value("$arrayElemAt's first argument must be an array and its second a number")),
            }
        }
        "$in" => {
            let values = exact_count(name, argument, 2)?;
            let needle = value_of(values[0], root, depth)?;
            match value_of(values[1], root, depth)? {
                Bson::Array(items) => Bson::Boolean(items.iter().any(|item| compare(item, &needle) == Ordering::Equal)),
                _ => return Err(DocumentError::bad_value("$in requires an array as a second argument")),
            }
        }
        "$type" => match evaluate_at(exact_count(name, argument, 1)?[0], root, depth)? {
            None => Bson::String("missing".to_owned()),
            Some(value) => Bson::String(value.type_name().to_owned()),
        },
        "$sum" | "$avg" | "$max" | "$min" => {
            let values = all(argument)?;
            let values: Vec<Bson> = if let [Bson::Array(items)] = values.as_slice() { items.clone() } else { values };
            accumulate_values(name, &values)
        }
        other => return Err(DocumentError::bad_value(format!("Unrecognized expression '{other}'"))),
    }))
}

/// Folds `values` the way the `$sum`, `$avg`, `$min` and `$max` accumulators do, ignoring
/// values that are not numbers for `$sum` and `$avg` and nulls for `$min` and `$max`.
pub(crate) fn accumulate_values(name: &str, values: &[Bson]) -> Bson {
    match name {
        "$sum" | "$avg" => {
            let numbers: Vec<&Bson> = values.iter().filter(|value| value.is_number()).collect();
            if name == "$avg" {
                if numbers.is_empty() {
                    return Bson::Null;
                }
                return Bson::Double(numbers.iter().filter_map(|value| value.as_f64()).sum::<f64>() / numbers.len() as f64);
            }
            let all_integers = numbers.iter().all(|value| matches!(value, Bson::Int32(_) | Bson::Int64(_)));
            let any_long = numbers.iter().any(|value| matches!(value, Bson::Int64(_)));
            let integer = numbers.iter().try_fold(0i64, |sum, value| value.as_i64().and_then(|number| sum.checked_add(number)));
            match (all_integers, integer) {
                (true, Some(total)) if any_long => Bson::Int64(total),
                (true, Some(total)) => number_result(total as f64, Some(total)),
                _ => Bson::Double(numbers.iter().filter_map(|value| value.as_f64()).sum()),
            }
        }
        _ => {
            let present = values.iter().filter(|value| !matches!(value, Bson::Null | Bson::Undefined));
            let chosen = if name == "$max" { present.max_by(|a, b| compare(a, b)) } else { present.min_by(|a, b| compare(a, b)) };
            chosen.cloned().unwrap_or(Bson::Null)
        }
    }
}
