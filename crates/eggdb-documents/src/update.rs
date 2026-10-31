use crate::error::{DocumentError, DocumentResult};
use crate::filter::{Condition, Filter, matches, parse_filter};
use crate::path::{Slot, slot};
use crate::sort::{SortSpec, parse_sort, sort_documents};
use eggdb_bson::{Bson, Document, compare};
use std::cmp::Ordering;

/// An update operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    /// `$set`.
    Set,
    /// `$setOnInsert`.
    SetOnInsert,
    /// `$unset`.
    Unset,
    /// `$inc`.
    Inc,
    /// `$mul`.
    Mul,
    /// `$min`.
    Min,
    /// `$max`.
    Max,
    /// `$rename`.
    Rename,
    /// `$push`.
    Push,
    /// `$addToSet`.
    AddToSet,
    /// `$pop`.
    Pop,
    /// `$pull`.
    Pull,
    /// `$pullAll`.
    PullAll,
    /// `$currentDate`.
    CurrentDate,
}

impl Operator {
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "$set" => Self::Set,
            "$setOnInsert" => Self::SetOnInsert,
            "$unset" => Self::Unset,
            "$inc" => Self::Inc,
            "$mul" => Self::Mul,
            "$min" => Self::Min,
            "$max" => Self::Max,
            "$rename" => Self::Rename,
            "$push" => Self::Push,
            "$addToSet" => Self::AddToSet,
            "$pop" => Self::Pop,
            "$pull" => Self::Pull,
            "$pullAll" => Self::PullAll,
            "$currentDate" => Self::CurrentDate,
            _ => return None,
        })
    }
}

/// A parsed update: a whole replacement document, or operators with their paths.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// The document's new content; its `_id` stays.
    Replacement(Document),
    /// Operators, each with the dotted path it changes and its argument.
    Operators(Vec<(Operator, String, Bson)>),
}

fn overlaps(left: &str, right: &str) -> bool {
    left == right
        || left.strip_prefix(right).is_some_and(|rest| rest.starts_with('.'))
        || right.strip_prefix(left).is_some_and(|rest| rest.starts_with('.'))
}

/// Reads an update document (`u`): operators when its first name starts with `$`, else a
/// replacement.
pub fn parse_update(update: &Document) -> DocumentResult<Update> {
    let operator_count = update.keys().filter(|name| name.starts_with('$')).count();
    if operator_count == 0 {
        return Ok(Update::Replacement(update.clone()));
    }
    if operator_count != update.len() {
        return Err(DocumentError::failed_to_parse("an update mixes operators and fields"));
    }
    let mut changes: Vec<(Operator, String, Bson)> = Vec::new();
    for (name, argument) in update.iter() {
        let Some(operator) = Operator::named(name) else {
            return Err(DocumentError::failed_to_parse(format!("Unknown modifier: {name}. Expected a valid update modifier or pipeline-style update specified as an array")));
        };
        let Bson::Document(fields) = argument else {
            return Err(DocumentError::failed_to_parse(format!("Modifiers operate on fields but we found type {} instead.", argument.type_name())));
        };
        for (path, value) in fields.iter() {
            if path.is_empty() || path.split('.').any(str::is_empty) {
                return Err(DocumentError::bad_value(format!("The update path '{path}' contains an empty field name, which is not allowed.")));
            }
            if path.split('.').any(|part| part == "$" || part.starts_with("$[")) {
                return Err(DocumentError::not_implemented("positional update paths ($ and $[]) are not supported yet"));
            }
            let mut touched = vec![path.clone()];
            if operator == Operator::Rename {
                let Some(target) = value.as_str() else {
                    return Err(DocumentError::bad_value("The 'to' field for $rename must be a string"));
                };
                if target == path {
                    return Err(DocumentError::bad_value("The source and target field for $rename must differ"));
                }
                touched.push(target.to_owned());
            }
            for new_path in &touched {
                if let Some((_, existing, _)) = changes.iter().find(|(existing_operator, existing, existing_value)| {
                    overlaps(existing, new_path)
                        || (*existing_operator == Operator::Rename
                            && existing_value.as_str().is_some_and(|target| overlaps(target, new_path)))
                }) {
                    return Err(DocumentError::conflicting_update(format!(
                        "Updating the path '{new_path}' would create a conflict at '{existing}'"
                    )));
                }
            }
            changes.push((operator, path.clone(), value.clone()));
        }
    }
    Ok(Update::Operators(changes))
}

fn add(left: &Bson, right: &Bson, operator: &str) -> DocumentResult<Bson> {
    combine(left, right, operator, i32::checked_add, i64::checked_add, |a, b| a + b)
}

fn multiply(left: &Bson, right: &Bson, operator: &str) -> DocumentResult<Bson> {
    combine(left, right, operator, i32::checked_mul, i64::checked_mul, |a, b| a * b)
}

fn combine(
    left: &Bson,
    right: &Bson,
    operator: &str,
    small: fn(i32, i32) -> Option<i32>,
    large: fn(i64, i64) -> Option<i64>,
    float: fn(f64, f64) -> f64,
) -> DocumentResult<Bson> {
    let overflow = || DocumentError::bad_value(format!("Failed to apply {operator} operations to current value: the result overflows a 64-bit integer"));
    Ok(match (left, right) {
        (Bson::Int32(a), Bson::Int32(b)) => match small(*a, *b) {
            Some(value) => Bson::Int32(value),
            None => Bson::Int64(large(i64::from(*a), i64::from(*b)).ok_or_else(overflow)?),
        },
        (Bson::Int32(_) | Bson::Int64(_), Bson::Int32(_) | Bson::Int64(_)) => {
            Bson::Int64(large(left.as_i64().unwrap_or(0), right.as_i64().unwrap_or(0)).ok_or_else(overflow)?)
        }
        (Bson::Decimal128(_), _) | (_, Bson::Decimal128(_)) => {
            return Err(DocumentError::not_implemented(format!("{operator} on decimal values is not supported yet")));
        }
        _ => Bson::Double(float(left.as_f64().unwrap_or(0.0), right.as_f64().unwrap_or(0.0))),
    })
}

fn zero_like(value: &Bson) -> Bson {
    match value {
        Bson::Int32(_) => Bson::Int32(0),
        Bson::Int64(_) => Bson::Int64(0),
        _ => Bson::Double(0.0),
    }
}

fn target<'d>(document: &'d mut Document, path: &str, create: bool) -> DocumentResult<Option<Slot<'d>>> {
    slot(document, path, create).map_err(DocumentError::bad_value)
}

fn each_of(argument: &Bson) -> (Vec<Bson>, Option<&Document>) {
    match argument {
        Bson::Document(modifiers) if modifiers.contains_key("$each") => match modifiers.get("$each") {
            Some(Bson::Array(items)) => (items.clone(), Some(modifiers)),
            _ => (Vec::new(), Some(modifiers)),
        },
        single => (vec![single.clone()], None),
    }
}

fn push(current: Option<&Bson>, argument: &Bson, path: &str) -> DocumentResult<Bson> {
    let mut items = match current {
        None => Vec::new(),
        Some(Bson::Array(items)) => items.clone(),
        Some(other) => {
            return Err(DocumentError::bad_value(format!("The field '{path}' must be an array but is of type {}", other.type_name())));
        }
    };
    let (each, modifiers) = each_of(argument);
    if let Some(modifiers) = modifiers {
        if !matches!(modifiers.get("$each"), Some(Bson::Array(_))) {
            return Err(DocumentError::bad_value("The argument to $each in $push must be an array"));
        }
        for name in modifiers.keys() {
            if !matches!(name.as_str(), "$each" | "$position" | "$slice" | "$sort") {
                return Err(DocumentError::bad_value(format!("Unrecognized clause in $push: {name}")));
            }
        }
    }
    let position = modifiers.and_then(|modifiers| modifiers.get_i64("$position"));
    let at = match position {
        None => items.len(),
        Some(position) if position < 0 => items.len().saturating_sub(position.unsigned_abs() as usize),
        Some(position) => (position as usize).min(items.len()),
    };
    items.splice(at..at, each);
    if let Some(order) = modifiers.and_then(|modifiers| modifiers.get("$sort")) {
        match order {
            Bson::Document(spec) => {
                let spec = parse_sort(spec)?;
                sort_values_by_fields(&mut items, &spec);
            }
            direction => {
                let descending = direction.as_i64() == Some(-1);
                if !descending && direction.as_i64() != Some(1) {
                    return Err(DocumentError::bad_value("The $sort is invalid: use 1/-1 to sort the whole element, or {field:1/-1} to sort embedded fields"));
                }
                items.sort_by(|a, b| if descending { compare(b, a) } else { compare(a, b) });
            }
        }
    }
    if let Some(slice) = modifiers.and_then(|modifiers| modifiers.get_i64("$slice")) {
        if slice >= 0 {
            items.truncate(slice as usize);
        } else {
            let keep = slice.unsigned_abs() as usize;
            if items.len() > keep {
                items.drain(..items.len() - keep);
            }
        }
    }
    Ok(Bson::Array(items))
}

fn sort_values_by_fields(items: &mut [Bson], spec: &SortSpec) {
    let mut documents: Vec<Document> = items
        .iter()
        .map(|item| match item {
            Bson::Document(inner) => inner.clone(),
            other => Document::from_entries(vec![(String::new(), other.clone())]),
        })
        .collect();
    sort_documents(&mut documents, spec);
    for (slot, document) in items.iter_mut().zip(documents) {
        *slot = match document.get("") {
            Some(value) if document.len() == 1 => value.clone(),
            _ => Bson::Document(document),
        };
    }
}

fn pulled(item: &Bson, argument: &Bson) -> DocumentResult<bool> {
    Ok(match argument {
        Bson::Document(inner) if inner.first().is_some_and(|(name, _)| name.starts_with('$')) => {
            let filter = parse_filter(&Document::from_entries(vec![("v".to_owned(), argument.clone())]))?;
            matches(&filter, &Document::from_entries(vec![("v".to_owned(), item.clone())]))
        }
        Bson::Document(inner) => match item {
            Bson::Document(element) => matches(&parse_filter(inner)?, element),
            _ => false,
        },
        other => compare(item, other) == Ordering::Equal,
    })
}

fn apply_one(document: &mut Document, operator: Operator, path: &str, argument: &Bson, inserting: bool, now_ms: i64) -> DocumentResult<()> {
    match operator {
        Operator::SetOnInsert if !inserting => {}
        Operator::Set | Operator::SetOnInsert => {
            if let Some(slot) = target(document, path, true)? {
                slot.set(argument.clone());
            }
        }
        Operator::Unset => {
            if let Some(slot) = target(document, path, false)? {
                slot.remove();
            }
        }
        Operator::Inc | Operator::Mul => {
            let name = if operator == Operator::Inc { "$inc" } else { "$mul" };
            if !argument.is_number() {
                return Err(DocumentError::type_mismatch(format!("Cannot {} with non-numeric argument: {{{path}: {}}}", &name[1..], argument.type_name())));
            }
            let Some(slot) = target(document, path, true)? else {
                return Ok(());
            };
            let current = slot.get().cloned();
            let updated = match &current {
                None => if operator == Operator::Inc { argument.clone() } else { zero_like(argument) },
                Some(value) if value.is_number() => {
                    if operator == Operator::Inc { add(value, argument, name)? } else { multiply(value, argument, name)? }
                }
                Some(value) => {
                    return Err(DocumentError::type_mismatch(format!("Cannot apply {name} to a value of non-numeric type. The field '{path}' is of non-numeric type {}", value.type_name())));
                }
            };
            slot.set(updated);
        }
        Operator::Min | Operator::Max => {
            let Some(slot) = target(document, path, true)? else {
                return Ok(());
            };
            let replace = match slot.get() {
                None => true,
                Some(current) => {
                    let ordering = compare(argument, current);
                    if operator == Operator::Min { ordering == Ordering::Less } else { ordering == Ordering::Greater }
                }
            };
            if replace {
                slot.set(argument.clone());
            }
        }
        Operator::Rename => {
            let destination = argument.as_str().unwrap_or_default().to_owned();
            let Some(value) = target(document, path, false)?.and_then(Slot::remove) else {
                return Ok(());
            };
            if let Some(slot) = target(document, &destination, true)? {
                slot.set(value);
            }
        }
        Operator::Push => {
            let Some(slot) = target(document, path, true)? else {
                return Ok(());
            };
            let updated = push(slot.get(), argument, path)?;
            slot.set(updated);
        }
        Operator::AddToSet => {
            let Some(slot) = target(document, path, true)? else {
                return Ok(());
            };
            let mut items = match slot.get() {
                None => Vec::new(),
                Some(Bson::Array(items)) => items.clone(),
                Some(other) => {
                    return Err(DocumentError::bad_value(format!("Cannot apply $addToSet to non-array field. Field named '{path}' has non-array type {}", other.type_name())));
                }
            };
            for value in each_of(argument).0 {
                if !items.iter().any(|item| compare(item, &value) == Ordering::Equal) {
                    items.push(value);
                }
            }
            slot.set(Bson::Array(items));
        }
        Operator::Pop => {
            let first = match argument.as_i64() {
                Some(1) => false,
                Some(-1) => true,
                _ => return Err(DocumentError::failed_to_parse("$pop expects 1 or -1")),
            };
            let Some(slot) = target(document, path, false)? else {
                return Ok(());
            };
            match slot.get() {
                None => {}
                Some(Bson::Array(items)) => {
                    let mut items = items.clone();
                    if !items.is_empty() {
                        if first { items.remove(0); } else { items.pop(); }
                    }
                    slot.set(Bson::Array(items));
                }
                Some(other) => {
                    return Err(DocumentError::type_mismatch(format!("Path '{path}' contains an element of non-array type '{}'", other.type_name())));
                }
            }
        }
        Operator::Pull | Operator::PullAll => {
            let Some(slot) = target(document, path, false)? else {
                return Ok(());
            };
            let Some(current) = slot.get() else {
                return Ok(());
            };
            let Bson::Array(items) = current else {
                return Err(DocumentError::bad_value("Cannot apply $pull to a non-array value"));
            };
            let mut kept = Vec::with_capacity(items.len());
            for item in items {
                let remove = if operator == Operator::PullAll {
                    let Bson::Array(values) = argument else {
                        return Err(DocumentError::bad_value("$pullAll requires an array argument"));
                    };
                    values.iter().any(|value| compare(item, value) == Ordering::Equal)
                } else {
                    pulled(item, argument)?
                };
                if !remove {
                    kept.push(item.clone());
                }
            }
            slot.set(Bson::Array(kept));
        }
        Operator::CurrentDate => {
            let as_timestamp = match argument {
                Bson::Boolean(_) => false,
                Bson::Document(spec) => match spec.get_str("$type") {
                    Some("date") => false,
                    Some("timestamp") => true,
                    _ => return Err(DocumentError::bad_value("The '$type' string field is required to be 'date' or 'timestamp'")),
                },
                _ => return Err(DocumentError::bad_value("$currentDate needs true or {$type: 'date' | 'timestamp'}")),
            };
            let value = if as_timestamp {
                Bson::Timestamp { time: (now_ms / 1000) as u32, increment: 1 }
            } else {
                Bson::DateTime(now_ms)
            };
            if let Some(slot) = target(document, path, true)? {
                slot.set(value);
            }
        }
    }
    Ok(())
}

/// Applies `update` to `document`; `inserting` when it creates the document of an upsert, so
/// `$setOnInsert` applies. Returns whether the document changed. Changing `_id` fails, and so
/// does a failure partway, leaving `document` as it was.
pub fn apply_update(update: &Update, document: &mut Document, inserting: bool, now_ms: i64) -> DocumentResult<bool> {
    let before = document.clone();
    let mut working = document.clone();
    match update {
        Update::Replacement(replacement) => {
            let mut next = replacement.clone();
            match (before.get("_id"), replacement.get("_id")) {
                (Some(old), Some(new)) if compare(old, new) != Ordering::Equal => {
                    return Err(DocumentError::immutable_field("After applying the update, the (immutable) field '_id' was found to have been altered"));
                }
                (Some(old), _) => next.insert_first("_id", old.clone()),
                _ => {}
            }
            working = next;
        }
        Update::Operators(changes) => {
            for (operator, path, argument) in changes {
                apply_one(&mut working, *operator, path, argument, inserting, now_ms)?;
            }
            if !inserting && before.get("_id").map(|old| working.get("_id").is_some_and(|new| compare(old, new) == Ordering::Equal)) == Some(false) {
                return Err(DocumentError::immutable_field("Performing an update on the path '_id' would modify the immutable field '_id'"));
            }
        }
    }
    let changed = working != before;
    *document = working;
    Ok(changed)
}

fn seed_from(filter: &Filter, document: &mut Document) {
    match filter {
        Filter::And(all) => {
            for inner in all {
                seed_from(inner, document);
            }
        }
        Filter::Field { path, condition: Condition::Eq(value) } if !matches!(value, Bson::Regex { .. }) => {
            if let Ok(Some(slot)) = slot(document, path, true) {
                slot.set(value.clone());
            }
        }
        Filter::Field { path, condition: Condition::Every(conditions) } => {
            for condition in conditions {
                if let Condition::Eq(value) = condition {
                    seed_from(&Filter::Field { path: path.clone(), condition: Condition::Eq(value.clone()) }, document);
                }
            }
        }
        _ => {}
    }
}

/// The document an upsert starts from: the equality fields of its filter, then the update.
pub fn upsert_document(filter: &Filter, update: &Update, now_ms: i64) -> DocumentResult<Document> {
    let mut seed = Document::new();
    seed_from(filter, &mut seed);
    match update {
        Update::Replacement(replacement) => {
            let mut document = replacement.clone();
            if !document.contains_key("_id")
                && let Some(id) = seed.get("_id")
            {
                document.insert_first("_id", id.clone());
            }
            Ok(document)
        }
        Update::Operators(_) => {
            apply_update(update, &mut seed, true, now_ms)?;
            Ok(seed)
        }
    }
}

