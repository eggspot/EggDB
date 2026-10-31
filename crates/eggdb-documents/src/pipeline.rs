use crate::error::{DocumentError, DocumentResult};
use crate::expression::{accumulate_values, evaluate, field_value};
use crate::filter::{Filter, matches, parse_filter};
use crate::path::slot;
use crate::sort::{SortSpec, compare_by, parse_sort};
use eggdb_bson::{Bson, Document, compare};
use std::cmp::Ordering;

/// One stage of an aggregation pipeline.
#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    /// `$match`.
    Match(Filter),
    /// `$project`, as its specification.
    Project(Document),
    /// `$addFields` or `$set`.
    AddFields(Document),
    /// `$unset`, the paths to remove.
    Unset(Vec<String>),
    /// `$sort`.
    Sort(SortSpec),
    /// `$skip`.
    Skip(u64),
    /// `$limit`.
    Limit(u64),
    /// `$count`, the output field.
    Count(String),
    /// `$group`: the key expression and the accumulators by output field.
    Group(Bson, Vec<(String, String, Bson)>),
    /// `$unwind`.
    Unwind {
        /// The array's path, without `$`.
        path: String,
        /// Where to put each element's index.
        index_field: Option<String>,
        /// Keep documents whose array is missing, null or empty.
        preserve_empty: bool,
    },
    /// `$lookup` with equality fields.
    Lookup {
        /// The other collection.
        from: String,
        /// The path in these documents.
        local_field: String,
        /// The path in the other collection's documents.
        foreign_field: String,
        /// The output array field.
        as_field: String,
    },
    /// `$replaceRoot` or `$replaceWith`, the new document's expression.
    ReplaceRoot(Bson),
}

const ACCUMULATORS: [&str; 9] = ["$sum", "$avg", "$min", "$max", "$first", "$last", "$push", "$addToSet", "$count"];

fn not_supported(name: &str) -> DocumentError {
    DocumentError::not_implemented(format!("the {name} stage is not supported yet"))
}

fn whole_number(value: &Bson, stage: &str) -> DocumentResult<u64> {
    match value.as_i64() {
        Some(number) if number >= 0 => Ok(number as u64),
        _ => Err(DocumentError::bad_value(format!("invalid argument to {stage} stage: needs a non-negative whole number"))),
    }
}

fn path_of(value: &Bson, stage: &str) -> DocumentResult<String> {
    match value.as_str().and_then(|text| text.strip_prefix('$')) {
        Some(path) if !path.is_empty() && !path.starts_with('$') => Ok(path.to_owned()),
        _ => Err(DocumentError::bad_value(format!("{stage} needs a field path starting with '$'"))),
    }
}

fn parse_stage(stage: &Document) -> DocumentResult<Stage> {
    let Some((name, argument)) = stage.first().filter(|_| stage.len() == 1) else {
        return Err(DocumentError::bad_value("A pipeline stage specification object must contain exactly one field."));
    };
    let document_argument = || match argument {
        Bson::Document(inner) => Ok(inner.clone()),
        _ => Err(DocumentError::bad_value(format!("the {name} stage's specification must be an object"))),
    };
    Ok(match name.as_str() {
        "$match" => Stage::Match(parse_filter(&document_argument()?)?),
        "$project" => Stage::Project(document_argument()?),
        "$addFields" | "$set" => Stage::AddFields(document_argument()?),
        "$unset" => Stage::Unset(match argument {
            Bson::String(path) => vec![path.clone()],
            Bson::Array(items) => items
                .iter()
                .map(|item| item.as_str().map(str::to_owned).ok_or_else(|| DocumentError::bad_value("$unset specification must be a string or an array of strings")))
                .collect::<DocumentResult<_>>()?,
            _ => return Err(DocumentError::bad_value("$unset specification must be a string or an array of strings")),
        }),
        "$sort" => Stage::Sort(parse_sort(&document_argument()?)?),
        "$skip" => Stage::Skip(whole_number(argument, "$skip")?),
        "$limit" => match whole_number(argument, "$limit")? {
            0 => return Err(DocumentError::bad_value("the limit must be positive")),
            limit => Stage::Limit(limit),
        },
        "$count" => match argument.as_str() {
            Some(field) if !field.is_empty() && !field.starts_with('$') && !field.contains('.') => Stage::Count(field.to_owned()),
            _ => return Err(DocumentError::bad_value("the count field must be a non-empty string without '$' or '.'")),
        },
        "$group" => {
            let spec = document_argument()?;
            let Some(key) = spec.get("_id").cloned() else {
                return Err(DocumentError::failed_to_parse("a group specification must include an _id"));
            };
            let mut accumulators = Vec::new();
            for (field, accumulator) in spec.iter().filter(|(field, _)| *field != "_id") {
                let Some((operator, expression)) = accumulator.as_document().and_then(Document::first) else {
                    return Err(DocumentError::bad_value(format!("The field '{field}' must be an accumulator object")));
                };
                if !ACCUMULATORS.contains(&operator.as_str()) {
                    return Err(DocumentError::bad_value(format!("unknown group operator '{operator}'")));
                }
                accumulators.push((field.clone(), operator.clone(), expression.clone()));
            }
            Stage::Group(key, accumulators)
        }
        "$unwind" => match argument {
            Bson::String(_) => Stage::Unwind { path: path_of(argument, "$unwind")?, index_field: None, preserve_empty: false },
            Bson::Document(spec) => Stage::Unwind {
                path: path_of(spec.get("path").unwrap_or(&Bson::Null), "$unwind")?,
                index_field: spec.get_str("includeArrayIndex").map(str::to_owned),
                preserve_empty: spec.get_bool("preserveNullAndEmptyArrays") == Some(true),
            },
            _ => return Err(DocumentError::bad_value("expected either a string or an object as specification for $unwind stage")),
        },
        "$lookup" => {
            let spec = document_argument()?;
            if spec.contains_key("pipeline") {
                return Err(DocumentError::not_implemented("$lookup with a pipeline is not supported yet"));
            }
            let field = |name: &str| {
                spec.get_str(name).map(str::to_owned).ok_or_else(|| DocumentError::failed_to_parse(format!("$lookup needs the string field '{name}'")))
            };
            Stage::Lookup { from: field("from")?, local_field: field("localField")?, foreign_field: field("foreignField")?, as_field: field("as")? }
        }
        "$replaceRoot" => match document_argument()?.get("newRoot") {
            Some(root) => Stage::ReplaceRoot(root.clone()),
            None => return Err(DocumentError::failed_to_parse("no newRoot specified for the $replaceRoot stage")),
        },
        "$replaceWith" => Stage::ReplaceRoot(argument.clone()),
        other => return Err(not_supported(other)),
    })
}

/// Reads a pipeline (the `pipeline` array of `aggregate`), expanding `$sortByCount` into the
/// `$group` and `$sort` it stands for.
pub fn parse_pipeline(stages: &[Bson]) -> DocumentResult<Vec<Stage>> {
    let mut out = Vec::new();
    for stage in stages {
        let Bson::Document(stage) = stage else {
            return Err(DocumentError::type_mismatch("Each element of the 'pipeline' array must be an object"));
        };
        if let Some(expression) = stage.get("$sortByCount").filter(|_| stage.len() == 1) {
            out.push(Stage::Group(expression.clone(), vec![("count".to_owned(), "$sum".to_owned(), Bson::Int32(1))]));
            out.push(Stage::Sort(parse_sort(&Document::from_entries(vec![("count".to_owned(), Bson::Int32(-1))]))?));
            continue;
        }
        out.push(parse_stage(stage)?);
    }
    Ok(out)
}

fn set_path(document: &mut Document, path: &str, value: Bson) -> DocumentResult<()> {
    if let Some(target) = slot(document, path, true).map_err(DocumentError::bad_value)? {
        target.set(value);
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProjectedField {
    Include,
    Exclude,
    Computed,
}

fn projected_field(value: &Bson) -> ProjectedField {
    match value {
        Bson::Boolean(_) | Bson::Int32(_) | Bson::Int64(_) | Bson::Double(_) | Bson::Decimal128(_) => {
            if value.is_truthy() { ProjectedField::Include } else { ProjectedField::Exclude }
        }
        _ => ProjectedField::Computed,
    }
}

fn project(document: &Document, spec: &Document) -> DocumentResult<Document> {
    let fields: Vec<(&String, &Bson, ProjectedField)> =
        spec.iter().filter(|(name, _)| *name != "_id").map(|(name, value)| (name, value, projected_field(value))).collect();
    let id_kind = spec.get("_id").map(projected_field);
    let excluding = fields.iter().any(|(_, _, kind)| *kind == ProjectedField::Exclude)
        || (fields.is_empty() && id_kind == Some(ProjectedField::Exclude));
    if excluding {
        if let Some((name, _, _)) = fields.iter().find(|(_, _, kind)| *kind != ProjectedField::Exclude) {
            return Err(DocumentError::bad_value(format!("Invalid $project :: caused by :: Cannot do inclusion on field {name} in exclusion projection")));
        }
        let mut out = document.clone();
        let dropped = fields.iter().map(|(name, _, _)| name.as_str()).chain((id_kind == Some(ProjectedField::Exclude)).then_some("_id"));
        for path in dropped {
            if let Ok(Some(target)) = slot(&mut out, path, false) {
                target.remove();
            }
        }
        return Ok(out);
    }
    let mut out = Document::new();
    match (id_kind, spec.get("_id")) {
        (Some(ProjectedField::Computed), Some(expression)) => {
            if let Some(value) = evaluate(expression, document)? {
                out.insert("_id", value);
            }
        }
        (Some(ProjectedField::Exclude), _) => {}
        _ => {
            if let Some(id) = document.get("_id") {
                out.insert("_id", id.clone());
            }
        }
    }
    for (path, value, kind) in fields {
        let result = if kind == ProjectedField::Include { field_value(document, path) } else { evaluate(value, document)? };
        if let Some(result) = result {
            set_path(&mut out, path, result)?;
        }
    }
    Ok(out)
}

fn group_key(value: Option<Bson>) -> Bson {
    value.unwrap_or(Bson::Null)
}

struct Group {
    key: Bson,
    inputs: Vec<Vec<Option<Bson>>>,
}

fn run_group(documents: Vec<Document>, key: &Bson, accumulators: &[(String, String, Bson)]) -> DocumentResult<Vec<Document>> {
    let mut groups: Vec<Group> = Vec::new();
    for document in &documents {
        let value = group_key(evaluate(key, document)?);
        let at = match groups.iter().position(|group| compare(&group.key, &value) == Ordering::Equal) {
            Some(at) => at,
            None => {
                groups.push(Group { key: value, inputs: vec![Vec::new(); accumulators.len()] });
                groups.len() - 1
            }
        };
        for (slot, (_, operator, expression)) in groups[at].inputs.iter_mut().zip(accumulators) {
            let input = if operator == "$count" { Some(Bson::Int32(1)) } else { evaluate(expression, document)? };
            slot.push(input);
        }
    }
    let mut out = Vec::with_capacity(groups.len());
    for group in groups {
        let mut result = Document::from_entries(vec![("_id".to_owned(), group.key)]);
        for ((field, operator, _), inputs) in accumulators.iter().zip(group.inputs) {
            let present: Vec<Bson> = inputs.iter().flatten().cloned().collect();
            let value = match operator.as_str() {
                "$sum" | "$count" => accumulate_values("$sum", &present),
                "$avg" | "$min" | "$max" => accumulate_values(operator, &present),
                "$first" => inputs.first().cloned().flatten().unwrap_or(Bson::Null),
                "$last" => inputs.last().cloned().flatten().unwrap_or(Bson::Null),
                "$push" => Bson::Array(present),
                _ => {
                    let mut unique: Vec<Bson> = Vec::new();
                    for value in present {
                        if !unique.iter().any(|held| compare(held, &value) == Ordering::Equal) {
                            unique.push(value);
                        }
                    }
                    Bson::Array(unique)
                }
            };
            result.insert(field.clone(), value);
        }
        out.push(result);
    }
    Ok(out)
}

fn unwind(documents: Vec<Document>, path: &str, index_field: Option<&str>, preserve_empty: bool) -> DocumentResult<Vec<Document>> {
    let mut out = Vec::new();
    for document in documents {
        match crate::path::get_exact(&document, path).cloned() {
            Some(Bson::Array(items)) if !items.is_empty() => {
                for (at, item) in items.into_iter().enumerate() {
                    let mut copy = document.clone();
                    set_path(&mut copy, path, item)?;
                    if let Some(field) = index_field {
                        set_path(&mut copy, field, Bson::Int64(at as i64))?;
                    }
                    out.push(copy);
                }
            }
            Some(Bson::Array(_)) | Some(Bson::Null) | None => {
                if preserve_empty {
                    let mut copy = document.clone();
                    if matches!(crate::path::get_exact(&copy, path), Some(Bson::Array(_)))
                        && let Ok(Some(target)) = slot(&mut copy, path, false)
                    {
                        target.remove();
                    }
                    if let Some(field) = index_field {
                        set_path(&mut copy, field, Bson::Null)?;
                    }
                    out.push(copy);
                }
            }
            Some(_) => {
                let mut copy = document;
                if let Some(field) = index_field {
                    set_path(&mut copy, field, Bson::Null)?;
                }
                out.push(copy);
            }
        }
    }
    Ok(out)
}

fn lookup_values(document: &Document, path: &str) -> Vec<Bson> {
    match field_value(document, path) {
        Some(Bson::Array(items)) => items,
        Some(value) => vec![value],
        None => vec![Bson::Null],
    }
}

/// Runs `stages` over `input`; `load` reads another collection of the same database for
/// `$lookup`.
pub fn run_pipeline(stages: &[Stage], input: Vec<Document>, load: &dyn Fn(&str) -> DocumentResult<Vec<Document>>) -> DocumentResult<Vec<Document>> {
    let mut documents = input;
    for stage in stages {
        documents = match stage {
            Stage::Match(filter) => documents.into_iter().filter(|document| matches(filter, document)).collect(),
            Stage::Project(spec) => documents.iter().map(|document| project(document, spec)).collect::<DocumentResult<_>>()?,
            Stage::AddFields(spec) => documents
                .into_iter()
                .map(|mut document| {
                    for (path, expression) in spec.iter() {
                        match evaluate(expression, &document)? {
                            Some(value) => set_path(&mut document, path, value)?,
                            None => {
                                if let Ok(Some(target)) = slot(&mut document, path, false) {
                                    target.remove();
                                }
                            }
                        }
                    }
                    Ok(document)
                })
                .collect::<DocumentResult<_>>()?,
            Stage::Unset(paths) => documents
                .into_iter()
                .map(|mut document| {
                    for path in paths {
                        if let Ok(Some(target)) = slot(&mut document, path, false) {
                            target.remove();
                        }
                    }
                    document
                })
                .collect(),
            Stage::Sort(spec) => {
                documents.sort_by(|left, right| compare_by(spec, left, right));
                documents
            }
            Stage::Skip(count) => documents.into_iter().skip(usize::try_from(*count).unwrap_or(usize::MAX)).collect(),
            Stage::Limit(count) => documents.into_iter().take(usize::try_from(*count).unwrap_or(usize::MAX)).collect(),
            Stage::Count(field) => {
                if documents.is_empty() {
                    Vec::new()
                } else {
                    vec![Document::from_entries(vec![(field.clone(), Bson::Int32(documents.len() as i32))])]
                }
            }
            Stage::Group(key, accumulators) => run_group(documents, key, accumulators)?,
            Stage::Unwind { path, index_field, preserve_empty } => unwind(documents, path, index_field.as_deref(), *preserve_empty)?,
            Stage::Lookup { from, local_field, foreign_field, as_field } => {
                let foreign = load(from)?;
                documents
                    .into_iter()
                    .map(|mut document| {
                        let wanted = lookup_values(&document, local_field);
                        let joined: Vec<Bson> = foreign
                            .iter()
                            .filter(|other| {
                                let have = lookup_values(other, foreign_field);
                                have.iter().any(|value| wanted.iter().any(|want| compare(value, want) == Ordering::Equal))
                            })
                            .map(|other| Bson::Document(other.clone()))
                            .collect();
                        set_path(&mut document, as_field, Bson::Array(joined))?;
                        Ok(document)
                    })
                    .collect::<DocumentResult<_>>()?
            }
            Stage::ReplaceRoot(expression) => documents
                .iter()
                .map(|document| match evaluate(expression, document)? {
                    Some(Bson::Document(root)) => Ok(root),
                    other => Err(DocumentError::bad_value(format!(
                        "'newRoot' expression must evaluate to an object, but resulting value was of type {}",
                        other.map_or("missing", |value| value.type_name())
                    ))),
                })
                .collect::<DocumentResult<_>>()?,
        };
    }
    Ok(documents)
}

impl Stage {
    /// Whether the stage is a `$match` whose filter can run where the documents are read.
    pub fn as_match(&self) -> Option<&Filter> {
        match self {
            Stage::Match(filter) => Some(filter),
            _ => None,
        }
    }
}
