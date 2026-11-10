use crate::error::{DocumentError, DocumentResult};
use crate::filter::{Condition, Filter, matched_array_index, matches, parse_filter};
use crate::path::{Slot, get_exact, slot};
use crate::pipeline::{Stage, parse_pipeline, run_pipeline};
use crate::size::{ByteBudget, document_size, value_size};
use crate::sort::{SortSpec, parse_sort, sort_documents};
use eggdb_bson::{Bson, Document, compare, relaxed_json};
use std::cmp::Ordering;
use std::collections::BTreeSet;

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

/// An `arrayFilters` entry: the filter the array elements an update path names with
/// `$[<identifier>]` must match.
#[derive(Debug, Clone, PartialEq)]
pub struct ArrayFilter {
    /// The identifier, the filter's one top-level field name.
    pub identifier: String,
    /// The filter, matched against `{<identifier>: <element>}`.
    pub filter: Filter,
}

/// A parsed update: a whole replacement document, operators with their paths, or a pipeline.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// The document's new content; its `_id` stays.
    Replacement(Document),
    /// Operators, each with the dotted path it changes and its argument.
    Operators {
        /// The operators in the order given, each with its path (which may hold `$`, `$[]`
        /// and `$[<identifier>]`) and argument.
        changes: Vec<(Operator, String, Bson)>,
        /// The filters `$[<identifier>]` path parts use.
        array_filters: Vec<ArrayFilter>,
    },
    /// A pipeline-style update: `$addFields`/`$set`, `$project`, `$unset` and
    /// `$replaceRoot`/`$replaceWith` stages run over the document.
    Pipeline(Vec<Stage>),
}

const PIPELINE_UPDATE_STAGES: [&str; 6] = [
    "$addFields",
    "$set",
    "$project",
    "$unset",
    "$replaceRoot",
    "$replaceWith",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathPart<'p> {
    Field(&'p str),
    MatchedElement,
    AllElements,
    FilteredElements(&'p str),
}

fn path_part(part: &str) -> PathPart<'_> {
    if part == "$" {
        return PathPart::MatchedElement;
    }
    match part
        .strip_prefix("$[")
        .and_then(|rest| rest.strip_suffix(']'))
    {
        Some("") => PathPart::AllElements,
        Some(identifier) => PathPart::FilteredElements(identifier),
        None => PathPart::Field(part),
    }
}

fn is_positional(path: &str) -> bool {
    path.split('.')
        .any(|part| !matches!(path_part(part), PathPart::Field(_)))
}

fn overlaps(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|rest| rest.starts_with('.'))
        || right
            .strip_prefix(left)
            .is_some_and(|rest| rest.starts_with('.'))
}

fn collect_top_level_names(spec: &Document, names: &mut Vec<String>) {
    for (name, value) in spec.iter() {
        match name.as_str() {
            "$and" | "$or" | "$nor" => {
                if let Bson::Array(items) = value {
                    for item in items {
                        if let Bson::Document(inner) = item {
                            collect_top_level_names(inner, names);
                        }
                    }
                }
            }
            operator if operator.starts_with('$') => {}
            path => {
                let top = path.split('.').next().unwrap_or(path);
                if !names.iter().any(|known| known == top) {
                    names.push(top.to_owned());
                }
            }
        }
    }
}

fn array_filter_identifier(spec: &Document) -> DocumentResult<String> {
    let mut names = Vec::new();
    collect_top_level_names(spec, &mut names);
    let identifier = match names.as_slice() {
        [] => {
            return Err(DocumentError::failed_to_parse(
                "Cannot use an expression without a top-level field name in arrayFilters",
            ));
        }
        [only] => only.clone(),
        [first, second, ..] => {
            return Err(DocumentError::failed_to_parse(format!(
                "Error parsing array filter :: caused by :: Expected a single top-level field name, found '{first}' and '{second}'"
            )));
        }
    };
    let valid = identifier
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase())
        && identifier
            .chars()
            .all(|character| character.is_ascii_alphanumeric());
    if !valid {
        return Err(DocumentError::bad_value(format!(
            "Error parsing array filter :: caused by :: The top-level field name must be an alphanumeric string beginning with a lowercase letter, found '{identifier}'"
        )));
    }
    Ok(identifier)
}

fn parse_array_filters(array_filters: Option<&Bson>) -> DocumentResult<Vec<ArrayFilter>> {
    let items = match array_filters {
        None => return Ok(Vec::new()),
        Some(Bson::Array(items)) => items,
        Some(other) => {
            return Err(DocumentError::type_mismatch(format!(
                "BSON field 'arrayFilters' is the wrong type '{}', expected type 'array'",
                other.type_name()
            )));
        }
    };
    let mut out: Vec<ArrayFilter> = Vec::with_capacity(items.len());
    for item in items {
        let Bson::Document(spec) = item else {
            return Err(DocumentError::type_mismatch(format!(
                "BSON field 'arrayFilters' has an element of the wrong type '{}', expected type 'object'",
                item.type_name()
            )));
        };
        let filter = parse_filter(spec)?;
        let identifier = array_filter_identifier(spec)?;
        if out.iter().any(|known| known.identifier == identifier) {
            return Err(DocumentError::failed_to_parse(format!(
                "Found multiple array filters with the same top-level field name {identifier}"
            )));
        }
        out.push(ArrayFilter { identifier, filter });
    }
    Ok(out)
}

fn check_positional_path(
    path: &str,
    array_filters: &[ArrayFilter],
    used: &mut Vec<String>,
) -> DocumentResult<()> {
    let mut matched_elements = 0;
    for (at, part) in path.split('.').enumerate() {
        let kind = path_part(part);
        if at == 0 {
            match kind {
                PathPart::Field(_) => {}
                PathPart::MatchedElement => {
                    return Err(DocumentError::bad_value(format!(
                        "Cannot have positional (i.e. '$') element in the first component in path '{path}'"
                    )));
                }
                PathPart::AllElements | PathPart::FilteredElements(_) => {
                    return Err(DocumentError::bad_value(format!(
                        "Cannot have array filter identifier (i.e. '$[<id>]') element in the first position in path '{path}'"
                    )));
                }
            }
            continue;
        }
        match kind {
            PathPart::Field(_) | PathPart::AllElements => {}
            PathPart::MatchedElement => {
                matched_elements += 1;
                if matched_elements > 1 {
                    return Err(DocumentError::bad_value(format!(
                        "Too many positional (i.e. '$') elements found in path '{path}'"
                    )));
                }
            }
            PathPart::FilteredElements(identifier) => {
                if !array_filters
                    .iter()
                    .any(|filter| filter.identifier == identifier)
                {
                    return Err(DocumentError::bad_value(format!(
                        "No array filter found for identifier '{identifier}' in path '{path}'"
                    )));
                }
                if !used.iter().any(|known| known == identifier) {
                    used.push(identifier.to_owned());
                }
            }
        }
    }
    Ok(())
}

fn check_array_filters_used(
    update: &Document,
    array_filters: &[ArrayFilter],
    used: &[String],
) -> DocumentResult<()> {
    match array_filters
        .iter()
        .find(|filter| !used.contains(&filter.identifier))
    {
        Some(unused) => Err(DocumentError::failed_to_parse(format!(
            "The array filter for identifier '{}' was not used in the update {}",
            unused.identifier,
            relaxed_json(update)
        ))),
        None => Ok(()),
    }
}

/// Reads an update document (`u`): operators when its first name starts with `$`, else a
/// replacement.
pub fn parse_update(update: &Document) -> DocumentResult<Update> {
    parse_update_document(update, Vec::new())
}

/// Reads an update statement's `u` (an operator or replacement document, or a pipeline
/// array) with its `arrayFilters`, failing as MongoDB does: `arrayFilters` with a pipeline is
/// `InvalidOptions` (72), an identifier no filter defines is `BadValue` (2), and a filter no
/// path uses is `FailedToParse` (9).
pub fn parse_update_statement(
    update: &Bson,
    array_filters: Option<&Bson>,
) -> DocumentResult<Update> {
    let array_filters = array_filters.filter(|filters| !matches!(filters, Bson::Null));
    match update {
        Bson::Array(stages) => {
            if array_filters.is_some() {
                return Err(DocumentError::invalid_options(
                    "arrayFilters may not be specified for pipeline-style updates",
                ));
            }
            parse_update_pipeline(stages)
        }
        Bson::Document(update) => {
            parse_update_document(update, parse_array_filters(array_filters)?)
        }
        other => Err(DocumentError::failed_to_parse(format!(
            "Update argument must be either an object or an array, not {}",
            other.type_name()
        ))),
    }
}

fn parse_update_pipeline(stages: &[Bson]) -> DocumentResult<Update> {
    for stage in stages {
        if let Bson::Document(stage) = stage
            && let Some((name, _)) = stage.first()
            && !PIPELINE_UPDATE_STAGES.contains(&name.as_str())
        {
            return Err(DocumentError::invalid_options(format!(
                "{name} is not allowed to be used within an update"
            )));
        }
    }
    Ok(Update::Pipeline(parse_pipeline(stages)?))
}

fn parse_update_document(
    update: &Document,
    array_filters: Vec<ArrayFilter>,
) -> DocumentResult<Update> {
    let operator_count = update.keys().filter(|name| name.starts_with('$')).count();
    if operator_count == 0 {
        check_array_filters_used(update, &array_filters, &[])?;
        return Ok(Update::Replacement(update.clone()));
    }
    if operator_count != update.len() {
        return Err(DocumentError::failed_to_parse(
            "an update mixes operators and fields",
        ));
    }
    let mut changes: Vec<(Operator, String, Bson)> = Vec::new();
    let mut used: Vec<String> = Vec::new();
    for (name, argument) in update.iter() {
        let Some(operator) = Operator::named(name) else {
            return Err(DocumentError::failed_to_parse(format!(
                "Unknown modifier: {name}. Expected a valid update modifier or pipeline-style update specified as an array"
            )));
        };
        let Bson::Document(fields) = argument else {
            return Err(DocumentError::failed_to_parse(format!(
                "Modifiers operate on fields but we found type {} instead.",
                argument.type_name()
            )));
        };
        for (path, value) in fields.iter() {
            if path.is_empty() || path.split('.').any(str::is_empty) {
                return Err(DocumentError::bad_value(format!(
                    "The update path '{path}' contains an empty field name, which is not allowed."
                )));
            }
            check_positional_path(path, &array_filters, &mut used)?;
            let mut touched = vec![path.clone()];
            if operator == Operator::Rename {
                if is_positional(path) {
                    return Err(DocumentError::bad_value(format!(
                        "The source field for $rename may not be dynamic: {path}"
                    )));
                }
                let Some(target) = value.as_str() else {
                    return Err(DocumentError::bad_value(
                        "The 'to' field for $rename must be a string",
                    ));
                };
                if is_positional(target) {
                    return Err(DocumentError::bad_value(format!(
                        "The destination field for $rename may not be dynamic: {target}"
                    )));
                }
                if target == path {
                    return Err(DocumentError::bad_value(
                        "The source and target field for $rename must differ",
                    ));
                }
                touched.push(target.to_owned());
            }
            for new_path in &touched {
                if let Some((_, existing, _)) =
                    changes
                        .iter()
                        .find(|(existing_operator, existing, existing_value)| {
                            overlaps(existing, new_path)
                                || (*existing_operator == Operator::Rename
                                    && existing_value
                                        .as_str()
                                        .is_some_and(|target| overlaps(target, new_path)))
                        })
                {
                    return Err(DocumentError::conflicting_update(format!(
                        "Updating the path '{new_path}' would create a conflict at '{existing}'"
                    )));
                }
            }
            changes.push((operator, path.clone(), value.clone()));
        }
    }
    check_array_filters_used(update, &array_filters, &used)?;
    Ok(Update::Operators {
        changes,
        array_filters,
    })
}

fn add(left: &Bson, right: &Bson, operator: &str) -> DocumentResult<Bson> {
    combine(
        left,
        right,
        operator,
        i32::checked_add,
        i64::checked_add,
        |a, b| a + b,
    )
}

fn multiply(left: &Bson, right: &Bson, operator: &str) -> DocumentResult<Bson> {
    combine(
        left,
        right,
        operator,
        i32::checked_mul,
        i64::checked_mul,
        |a, b| a * b,
    )
}

fn combine(
    left: &Bson,
    right: &Bson,
    operator: &str,
    small: fn(i32, i32) -> Option<i32>,
    large: fn(i64, i64) -> Option<i64>,
    float: fn(f64, f64) -> f64,
) -> DocumentResult<Bson> {
    let overflow = || {
        DocumentError::bad_value(format!(
            "Failed to apply {operator} operations to current value: the result overflows a 64-bit integer"
        ))
    };
    Ok(match (left, right) {
        (Bson::Int32(a), Bson::Int32(b)) => match small(*a, *b) {
            Some(value) => Bson::Int32(value),
            None => Bson::Int64(large(i64::from(*a), i64::from(*b)).ok_or_else(overflow)?),
        },
        (Bson::Int32(_) | Bson::Int64(_), Bson::Int32(_) | Bson::Int64(_)) => Bson::Int64(
            large(left.as_i64().unwrap_or(0), right.as_i64().unwrap_or(0)).ok_or_else(overflow)?,
        ),
        (Bson::Decimal128(_), _) | (_, Bson::Decimal128(_)) => {
            return Err(DocumentError::not_implemented(format!(
                "{operator} on decimal values is not supported yet"
            )));
        }
        _ => Bson::Double(float(
            left.as_f64().unwrap_or(0.0),
            right.as_f64().unwrap_or(0.0),
        )),
    })
}

fn zero_like(value: &Bson) -> Bson {
    match value {
        Bson::Int32(_) => Bson::Int32(0),
        Bson::Int64(_) => Bson::Int64(0),
        _ => Bson::Double(0.0),
    }
}

fn target<'d>(
    document: &'d mut Document,
    path: &str,
    create: bool,
) -> DocumentResult<Option<Slot<'d>>> {
    slot(document, path, create)
}

fn each_of(argument: &Bson) -> (Vec<Bson>, Option<&Document>) {
    match argument {
        Bson::Document(modifiers) if modifiers.contains_key("$each") => {
            match modifiers.get("$each") {
                Some(Bson::Array(items)) => (items.clone(), Some(modifiers)),
                _ => (Vec::new(), Some(modifiers)),
            }
        }
        single => (vec![single.clone()], None),
    }
}

fn push(current: Option<&Bson>, argument: &Bson, path: &str) -> DocumentResult<Bson> {
    let mut items = match current {
        None => Vec::new(),
        Some(Bson::Array(items)) => items.clone(),
        Some(other) => {
            return Err(DocumentError::bad_value(format!(
                "The field '{path}' must be an array but is of type {}",
                other.type_name()
            )));
        }
    };
    let (each, modifiers) = each_of(argument);
    if let Some(modifiers) = modifiers {
        if !matches!(modifiers.get("$each"), Some(Bson::Array(_))) {
            return Err(DocumentError::bad_value(
                "The argument to $each in $push must be an array",
            ));
        }
        for name in modifiers.keys() {
            if !matches!(name.as_str(), "$each" | "$position" | "$slice" | "$sort") {
                return Err(DocumentError::bad_value(format!(
                    "Unrecognized clause in $push: {name}"
                )));
            }
        }
    }
    let position = modifiers.and_then(|modifiers| modifiers.get_i64("$position"));
    let at = match position {
        None => items.len(),
        Some(position) if position < 0 => {
            items.len().saturating_sub(position.unsigned_abs() as usize)
        }
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
                    return Err(DocumentError::bad_value(
                        "The $sort is invalid: use 1/-1 to sort the whole element, or {field:1/-1} to sort embedded fields",
                    ));
                }
                items.sort_by(|a, b| {
                    if descending {
                        compare(b, a)
                    } else {
                        compare(a, b)
                    }
                });
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
            let filter = parse_filter(&Document::from_entries(vec![(
                "v".to_owned(),
                argument.clone(),
            )]))?;
            matches(
                &filter,
                &Document::from_entries(vec![("v".to_owned(), item.clone())]),
            )
        }
        Bson::Document(inner) => match item {
            Bson::Document(element) => matches(&parse_filter(inner)?, element),
            _ => false,
        },
        other => compare(item, other) == Ordering::Equal,
    })
}

fn apply_one(
    document: &mut Document,
    operator: Operator,
    path: &str,
    argument: &Bson,
    inserting: bool,
    now_ms: i64,
) -> DocumentResult<()> {
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
            let name = if operator == Operator::Inc {
                "$inc"
            } else {
                "$mul"
            };
            if !argument.is_number() {
                return Err(DocumentError::type_mismatch(format!(
                    "Cannot {} with non-numeric argument: {{{path}: {}}}",
                    &name[1..],
                    argument.type_name()
                )));
            }
            let Some(slot) = target(document, path, true)? else {
                return Ok(());
            };
            let current = slot.get().cloned();
            let updated = match &current {
                None => {
                    if operator == Operator::Inc {
                        argument.clone()
                    } else {
                        zero_like(argument)
                    }
                }
                Some(value) if value.is_number() => {
                    if operator == Operator::Inc {
                        add(value, argument, name)?
                    } else {
                        multiply(value, argument, name)?
                    }
                }
                Some(value) => {
                    return Err(DocumentError::type_mismatch(format!(
                        "Cannot apply {name} to a value of non-numeric type. The field '{path}' is of non-numeric type {}",
                        value.type_name()
                    )));
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
                    if operator == Operator::Min {
                        ordering == Ordering::Less
                    } else {
                        ordering == Ordering::Greater
                    }
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
                    return Err(DocumentError::bad_value(format!(
                        "Cannot apply $addToSet to non-array field. Field named '{path}' has non-array type {}",
                        other.type_name()
                    )));
                }
            };
            for value in each_of(argument).0 {
                if !items
                    .iter()
                    .any(|item| compare(item, &value) == Ordering::Equal)
                {
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
                        if first {
                            items.remove(0);
                        } else {
                            items.pop();
                        }
                    }
                    slot.set(Bson::Array(items));
                }
                Some(other) => {
                    return Err(DocumentError::type_mismatch(format!(
                        "Path '{path}' contains an element of non-array type '{}'",
                        other.type_name()
                    )));
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
                return Err(DocumentError::bad_value(
                    "Cannot apply $pull to a non-array value",
                ));
            };
            let mut kept = Vec::with_capacity(items.len());
            for item in items {
                let remove = if operator == Operator::PullAll {
                    let Bson::Array(values) = argument else {
                        return Err(DocumentError::bad_value(
                            "$pullAll requires an array argument",
                        ));
                    };
                    values
                        .iter()
                        .any(|value| compare(item, value) == Ordering::Equal)
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
                    _ => {
                        return Err(DocumentError::bad_value(
                            "The '$type' string field is required to be 'date' or 'timestamp'",
                        ));
                    }
                },
                _ => {
                    return Err(DocumentError::bad_value(
                        "$currentDate needs true or {$type: 'date' | 'timestamp'}",
                    ));
                }
            };
            let value = if as_timestamp {
                Bson::Timestamp {
                    time: (now_ms / 1000) as u32,
                    increment: 1,
                }
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

fn extend_path(prefix: &mut String, part: &str) {
    if !prefix.is_empty() {
        prefix.push('.');
    }
    prefix.push_str(part);
}

fn element_matches(array_filter: &ArrayFilter, element: &Bson) -> bool {
    matches(
        &array_filter.filter,
        &Document::from_entries(vec![(array_filter.identifier.clone(), element.clone())]),
    )
}

fn last_part(path: &str) -> &str {
    path.rsplit('.').next().unwrap_or(path)
}

fn concrete_paths(
    document: &Document,
    path: &str,
    matched_element: Option<usize>,
    array_filters: &[ArrayFilter],
) -> DocumentResult<Vec<String>> {
    let mut prefixes = vec![String::new()];
    for part in path.split('.') {
        let array_filter = match path_part(part) {
            PathPart::Field(name) => {
                for prefix in &mut prefixes {
                    extend_path(prefix, name);
                }
                continue;
            }
            PathPart::MatchedElement => {
                let Some(at) = matched_element else {
                    return Err(DocumentError::bad_value(
                        "The positional operator did not find the match needed from the query.",
                    ));
                };
                let at = at.to_string();
                for prefix in &mut prefixes {
                    extend_path(prefix, &at);
                }
                continue;
            }
            PathPart::AllElements => None,
            PathPart::FilteredElements(identifier) => Some(
                array_filters
                    .iter()
                    .find(|filter| filter.identifier == identifier)
                    .ok_or_else(|| {
                        DocumentError::bad_value(format!(
                            "No array filter found for identifier '{identifier}' in path '{path}'"
                        ))
                    })?,
            ),
        };
        let mut next = Vec::new();
        for prefix in &prefixes {
            let items = match get_exact(document, prefix) {
                Some(Bson::Array(items)) => items,
                None => {
                    return Err(DocumentError::bad_value(format!(
                        "The path '{prefix}' must exist in the document in order to apply array updates."
                    )));
                }
                Some(other) => {
                    return Err(DocumentError::bad_value(format!(
                        "Cannot apply array updates to non-array element {}",
                        relaxed_json(&Document::from_entries(vec![(
                            last_part(prefix).to_owned(),
                            other.clone()
                        )]))
                    )));
                }
            };
            for (at, element) in items.iter().enumerate() {
                if array_filter.is_none_or(|filter| element_matches(filter, element)) {
                    next.push(format!("{prefix}.{at}"));
                }
            }
        }
        prefixes = next;
    }
    Ok(prefixes)
}

#[derive(Default)]
struct ChangedPaths(BTreeSet<String>);

impl ChangedPaths {
    fn conflict(&self, path: &str) -> Option<&String> {
        let mut end = 0;
        for part in path.split('.') {
            end += part.len();
            if let Some(found) = self.0.get(&path[..end]) {
                return Some(found);
            }
            end += 1;
        }
        let below = format!("{path}.");
        self.0
            .range(below.clone()..)
            .next()
            .filter(|found| found.starts_with(&below))
    }
}

fn apply_operators(
    working: &mut Document,
    changes: &[(Operator, String, Bson)],
    array_filters: &[ArrayFilter],
    matched_element: Option<usize>,
    inserting: bool,
    now_ms: i64,
) -> DocumentResult<()> {
    let mut changed = ChangedPaths::default();
    let mut budget = None;
    for (operator, path, argument) in changes {
        if *operator == Operator::SetOnInsert && !inserting {
            continue;
        }
        let paths = if is_positional(path) {
            concrete_paths(working, path, matched_element, array_filters)?
        } else {
            vec![path.clone()]
        };
        let rename_target = if *operator == Operator::Rename {
            argument.as_str()
        } else {
            None
        };
        for concrete in paths.iter().map(String::as_str).chain(rename_target) {
            if let Some(existing) = changed.conflict(concrete) {
                return Err(DocumentError::conflicting_update(format!(
                    "Updating the path '{concrete}' would create a conflict at '{existing}'"
                )));
            }
        }
        if paths.len() > 1 && budget.is_none() {
            budget = Some(ByteBudget::starting_at(document_size(working))?);
        }
        for concrete in &paths {
            let Some(budget) = budget.as_mut() else {
                apply_one(working, *operator, concrete, argument, inserting, now_ms)?;
                continue;
            };
            let removed = get_exact(working, concrete).map_or(0, value_size);
            apply_one(working, *operator, concrete, argument, inserting, now_ms)?;
            let added = get_exact(working, concrete).map_or(0, value_size);
            budget.replace(removed, added)?;
        }
        changed.0.extend(paths);
        if let Some(target) = rename_target {
            changed.0.insert(target.to_owned());
        }
    }
    Ok(())
}

fn literal(value: Bson) -> Bson {
    Bson::Document(Document::from_entries(vec![("$literal".to_owned(), value)]))
}

fn cluster_time(now: &Bson) -> Bson {
    let seconds = match now {
        Bson::DateTime(millis) => u32::try_from(millis.div_euclid(1000)).unwrap_or(0),
        _ => 0,
    };
    Bson::Timestamp {
        time: seconds,
        increment: 1,
    }
}

fn value_with_now(value: &Bson, now: &Bson) -> Bson {
    match value {
        Bson::String(text) if text == "$$NOW" => literal(now.clone()),
        Bson::String(text) if text == "$$CLUSTER_TIME" => literal(cluster_time(now)),
        Bson::Document(inner) if inner.contains_key("$literal") => value.clone(),
        Bson::Document(inner) => Bson::Document(document_with_now(inner, now)),
        Bson::Array(items) => {
            Bson::Array(items.iter().map(|item| value_with_now(item, now)).collect())
        }
        other => other.clone(),
    }
}

fn document_with_now(spec: &Document, now: &Bson) -> Document {
    Document::from_entries(
        spec.iter()
            .map(|(name, value)| (name.clone(), value_with_now(value, now)))
            .collect(),
    )
}

/// The aggregation pipeline `stages` with `$$NOW` and `$$CLUSTER_TIME` bound to the time
/// `now_ms`, once for the whole command as MongoDB binds them: in every stage's expressions,
/// and in `$match` only inside `$expr`, where a filter holds expressions.
pub fn pipeline_with_time(stages: &[Bson], now_ms: i64) -> Vec<Bson> {
    let now = Bson::DateTime(now_ms);
    stages
        .iter()
        .map(|stage| match stage {
            Bson::Document(spec) => Bson::Document(Document::from_entries(
                spec.iter()
                    .map(|(name, value)| {
                        let bound = match (name.as_str(), value) {
                            ("$match", Bson::Document(filter)) => {
                                Bson::Document(Document::from_entries(
                                    filter
                                        .iter()
                                        .map(|(field, condition)| {
                                            let condition = if field == "$expr" {
                                                value_with_now(condition, &now)
                                            } else {
                                                condition.clone()
                                            };
                                            (field.clone(), condition)
                                        })
                                        .collect(),
                                ))
                            }
                            _ => value_with_now(value, &now),
                        };
                        (name.clone(), bound)
                    })
                    .collect(),
            )),
            other => other.clone(),
        })
        .collect()
}

/// The query filter `filter` with `$$NOW` and `$$CLUSTER_TIME` bound to `now_ms` inside its
/// `$expr` expressions, at the top level and under `$and`, `$or` and `$nor`.
pub fn filter_with_time(filter: &Document, now_ms: i64) -> Document {
    filter_bound_to(filter, &Bson::DateTime(now_ms))
}

fn filter_bound_to(filter: &Document, now: &Bson) -> Document {
    Document::from_entries(
        filter
            .iter()
            .map(|(field, condition)| {
                let condition = match (field.as_str(), condition) {
                    ("$expr", _) => value_with_now(condition, now),
                    ("$and" | "$or" | "$nor", Bson::Array(filters)) => Bson::Array(
                        filters
                            .iter()
                            .map(|inner| match inner {
                                Bson::Document(inner) => Bson::Document(filter_bound_to(inner, now)),
                                other => other.clone(),
                            })
                            .collect(),
                    ),
                    _ => condition.clone(),
                };
                (field.clone(), condition)
            })
            .collect(),
    )
}

fn stage_with_now(stage: &Stage, now: &Bson) -> Stage {
    match stage {
        Stage::AddFields(spec) => Stage::AddFields(document_with_now(spec, now)),
        Stage::Project(spec) => Stage::Project(document_with_now(spec, now)),
        Stage::ReplaceRoot(expression) => Stage::ReplaceRoot(value_with_now(expression, now)),
        other => other.clone(),
    }
}

fn run_update_pipeline(
    stages: &[Stage],
    document: &Document,
    now_ms: i64,
) -> DocumentResult<Document> {
    let now = Bson::DateTime(now_ms);
    let bound: Vec<Stage> = stages
        .iter()
        .map(|stage| stage_with_now(stage, &now))
        .collect();
    let no_other_collections = |_: &str| -> DocumentResult<Vec<Document>> { Ok(Vec::new()) };
    let mut out = run_pipeline(&bound, vec![document.clone()], &no_other_collections)?;
    match (out.pop(), out.is_empty()) {
        (Some(result), true) => Ok(result),
        _ => Err(DocumentError::bad_value(
            "an update pipeline must give exactly one document",
        )),
    }
}

fn keeping_id(mut next: Document, before: &Document) -> DocumentResult<Document> {
    let Some(old) = before.get("_id") else {
        return Ok(next);
    };
    if next
        .get("_id")
        .is_some_and(|new| compare(old, new) != Ordering::Equal)
    {
        return Err(DocumentError::immutable_field(
            "After applying the update, the (immutable) field '_id' was found to have been altered",
        ));
    }
    next.insert_first("_id", old.clone());
    Ok(next)
}

/// Applies `update` to `document`; `query` is the filter that matched it, which gives the
/// positional `$` its element, and `inserting` is set when the update creates the document
/// of an upsert, so `$setOnInsert` applies. Returns whether the document changed. Changing
/// `_id` fails, and so does a failure partway, leaving `document` as it was.
pub fn apply_update(
    update: &Update,
    document: &mut Document,
    query: Option<&Filter>,
    inserting: bool,
    now_ms: i64,
) -> DocumentResult<bool> {
    let before = document.clone();
    let working = match update {
        Update::Replacement(replacement) => keeping_id(replacement.clone(), &before)?,
        Update::Operators {
            changes,
            array_filters,
        } => {
            let mut working = before.clone();
            let uses_matched_element = changes
                .iter()
                .any(|(_, path, _)| path.split('.').any(|part| part == "$"));
            let matched_element = query
                .filter(|_| uses_matched_element)
                .and_then(|query| matched_array_index(query, &before));
            apply_operators(
                &mut working,
                changes,
                array_filters,
                matched_element,
                inserting,
                now_ms,
            )?;
            if !inserting
                && before.get("_id").map(|old| {
                    working
                        .get("_id")
                        .is_some_and(|new| compare(old, new) == Ordering::Equal)
                }) == Some(false)
            {
                return Err(DocumentError::immutable_field(
                    "Performing an update on the path '_id' would modify the immutable field '_id'",
                ));
            }
            working
        }
        Update::Pipeline(stages) => {
            let result = run_update_pipeline(stages, &before, now_ms)?;
            if inserting {
                result
            } else {
                keeping_id(result, &before)?
            }
        }
    };
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
        Filter::Field {
            path,
            condition: Condition::Eq(value),
        } if !matches!(value, Bson::Regex { .. }) => {
            if let Ok(Some(slot)) = slot(document, path, true) {
                slot.set(value.clone());
            }
        }
        Filter::Field {
            path,
            condition: Condition::Every(conditions),
        } => {
            for condition in conditions {
                if let Condition::Eq(value) = condition {
                    seed_from(
                        &Filter::Field {
                            path: path.clone(),
                            condition: Condition::Eq(value.clone()),
                        },
                        document,
                    );
                }
            }
        }
        _ => {}
    }
}

/// The document an upsert starts from: the equality fields of its filter, then the update
/// (operators or a pipeline) applied to them.
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
        Update::Operators { .. } | Update::Pipeline(_) => {
            apply_update(update, &mut seed, None, true, now_ms)?;
            Ok(seed)
        }
    }
}
