use crate::error::{DocumentError, DocumentResult};
use crate::expression::evaluate;
use crate::filter::{Filter, matched_array_index, matches, parse_filter};
use crate::path::slot;
use eggdb_bson::{Bson, Document};

/// A parsed projection: the paths to keep or to drop, whether `_id` is kept, and the
/// projection operators and expressions.
#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    paths: Vec<String>,
    inclusive: bool,
    keep_id: bool,
    operators: Vec<(String, ProjectionOperator)>,
}

#[derive(Debug, Clone, PartialEq)]
enum ProjectionOperator {
    Slice { skip: Option<i64>, limit: i64 },
    ElemMatch(Filter),
    Positional,
    Expression(Bson),
}

/// Reads a projection such as `{name: 1, _id: 0}`, `{secret: 0}`, `{items: {$slice: 2}}`,
/// `{items: {$elemMatch: {qty: {$gt: 1}}}}`, `{"items.$": 1}` or `{total: {$add: ["$a", 1]}}`.
pub fn parse_projection(spec: &Document) -> DocumentResult<Projection> {
    let mut flat = Vec::new();
    flatten(spec, "", &mut flat, 0)?;
    let mut inclusive = None;
    let mut keep_id = true;
    let mut id_named = false;
    let mut paths = Vec::new();
    let mut operators = Vec::new();
    let set_mode = |path: &str, include: bool, inclusive: &mut Option<bool>| match *inclusive {
        Some(mode) if mode != include => Err(DocumentError::new_projection_conflict(path, include)),
        _ => {
            *inclusive = Some(include);
            Ok(())
        }
    };
    for (path, value) in flat {
        if let Some(array_path) = path.strip_suffix(".$") {
            if array_path.contains('$')
                || operators
                    .iter()
                    .any(|(_, operator)| matches!(operator, ProjectionOperator::Positional))
            {
                return Err(DocumentError::bad_value(
                    "Cannot specify more than one positional proj. per query.",
                ));
            }
            if !value.is_truthy() {
                return Err(DocumentError::bad_value(
                    "positional projection cannot be used with exclusion",
                ));
            }
            set_mode(&path, true, &mut inclusive)?;
            operators.push((array_path.to_owned(), ProjectionOperator::Positional));
            continue;
        }
        if path.split('.').any(|part| part.starts_with('$')) {
            return Err(DocumentError::bad_value(format!(
                "FieldPath field names may not start with '$'. Consider using $getField or $setField. ({path})"
            )));
        }
        match &value {
            Bson::Document(inner) => match inner.first() {
                Some((name, argument)) if name == "$slice" && inner.len() == 1 => {
                    operators.push((path.clone(), slice_of(argument)?));
                }
                Some((name, argument)) if name == "$elemMatch" && inner.len() == 1 => {
                    let Bson::Document(condition) = argument else {
                        return Err(DocumentError::bad_value(
                            "elemMatch: Invalid argument, object required.",
                        ));
                    };
                    if path.contains('.') {
                        return Err(DocumentError::bad_value(
                            "Cannot use $elemMatch projection on a nested field.",
                        ));
                    }
                    if inclusive.is_none() {
                        inclusive = Some(true);
                    }
                    operators.push((
                        path.clone(),
                        ProjectionOperator::ElemMatch(parse_filter(condition)?),
                    ));
                }
                Some((name, _)) if name == "$meta" => {
                    return Err(DocumentError::not_implemented(
                        "$meta in a projection is not supported yet",
                    ));
                }
                _ => {
                    set_mode(&path, true, &mut inclusive)?;
                    if path == "_id" {
                        keep_id = false;
                    }
                    operators.push((path.clone(), ProjectionOperator::Expression(value.clone())));
                }
            },
            Bson::String(_) | Bson::Array(_) | Bson::Null => {
                set_mode(&path, true, &mut inclusive)?;
                if path == "_id" {
                    keep_id = false;
                }
                operators.push((path.clone(), ProjectionOperator::Expression(value.clone())));
            }
            _ => {
                let include = value.is_truthy();
                if path == "_id" {
                    keep_id = include;
                    id_named = true;
                    continue;
                }
                set_mode(&path, include, &mut inclusive)?;
                paths.push(path.clone());
            }
        }
    }
    let only_id_kept = inclusive.is_none() && keep_id && id_named;
    Ok(Projection {
        paths,
        inclusive: inclusive.unwrap_or(only_id_kept),
        keep_id,
        operators,
    })
}

fn flatten(
    spec: &Document,
    prefix: &str,
    out: &mut Vec<(String, Bson)>,
    depth: usize,
) -> DocumentResult<()> {
    if depth > 100 {
        return Err(DocumentError::bad_value(
            "the projection is nested too deeply",
        ));
    }
    for (name, value) in spec.iter() {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        match value {
            Bson::Document(inner)
                if !inner.is_empty()
                    && inner
                        .first()
                        .is_some_and(|(first, _)| !first.starts_with('$')) =>
            {
                flatten(inner, &path, out, depth + 1)?;
            }
            Bson::Document(inner) if inner.is_empty() => {
                return Err(DocumentError::bad_value(format!(
                    "An empty sub-projection is not a valid value. Found empty object at path {path}"
                )));
            }
            _ => out.push((path, value.clone())),
        }
    }
    Ok(())
}

fn slice_of(argument: &Bson) -> DocumentResult<ProjectionOperator> {
    let whole = |value: &Bson| {
        value.as_i64().ok_or_else(|| {
            DocumentError::bad_value("$slice only supports numbers and [skip, limit] arrays")
        })
    };
    match argument {
        Bson::Array(pair) if pair.len() == 2 => {
            let limit = whole(&pair[1])?;
            if limit <= 0 {
                return Err(DocumentError::bad_value("$slice limit must be positive"));
            }
            Ok(ProjectionOperator::Slice {
                skip: Some(whole(&pair[0])?),
                limit,
            })
        }
        Bson::Array(_) => Err(DocumentError::bad_value(
            "$slice array argument must be of form [skip, limit]",
        )),
        other => Ok(ProjectionOperator::Slice {
            skip: None,
            limit: whole(other)?,
        }),
    }
}

fn sliced(items: &[Bson], skip: Option<i64>, limit: i64) -> Vec<Bson> {
    let length = items.len() as i64;
    let (start, count) = match skip {
        None if limit >= 0 => (0, limit),
        None => ((length + limit).max(0), length),
        Some(skip) if skip >= 0 => (skip.min(length), limit),
        Some(skip) => ((length + skip).max(0), limit),
    };
    let start = start as usize;
    let count = usize::try_from(count).unwrap_or(usize::MAX);
    items.iter().skip(start).take(count).cloned().collect()
}

impl DocumentError {
    fn new_projection_conflict(path: &str, include: bool) -> Self {
        let mode = if include { "inclusion" } else { "exclusion" };
        let other = if include { "exclusion" } else { "inclusion" };
        Self::bad_value(format!(
            "Cannot do {mode} on field {path} in {other} projection"
        ))
    }
}

fn keep_paths(source: &Document, paths: &[Vec<&str>]) -> Document {
    let mut out = Document::new();
    for (name, value) in source.iter() {
        let wanted: Vec<Vec<&str>> = paths
            .iter()
            .filter(|parts| parts[0] == name)
            .map(|parts| parts[1..].to_vec())
            .collect();
        if wanted.is_empty() {
            continue;
        }
        if wanted.iter().any(Vec::is_empty) {
            out.insert(name.clone(), value.clone());
            continue;
        }
        match value {
            Bson::Document(inner) => {
                out.insert(name.clone(), Bson::Document(keep_paths(inner, &wanted)))
            }
            Bson::Array(items) => out.insert(
                name.clone(),
                Bson::Array(
                    items
                        .iter()
                        .filter_map(|item| match item {
                            Bson::Document(inner) => {
                                Some(Bson::Document(keep_paths(inner, &wanted)))
                            }
                            _ => None,
                        })
                        .collect(),
                ),
            ),
            _ => {}
        }
    }
    out
}

fn drop_paths(source: &Document, paths: &[Vec<&str>]) -> Document {
    let mut out = Document::new();
    for (name, value) in source.iter() {
        let wanted: Vec<Vec<&str>> = paths
            .iter()
            .filter(|parts| parts[0] == name)
            .map(|parts| parts[1..].to_vec())
            .collect();
        if wanted.iter().any(Vec::is_empty) {
            continue;
        }
        let kept = match value {
            Bson::Document(inner) if !wanted.is_empty() => {
                Bson::Document(drop_paths(inner, &wanted))
            }
            Bson::Array(items) if !wanted.is_empty() => Bson::Array(
                items
                    .iter()
                    .map(|item| match item {
                        Bson::Document(inner) => Bson::Document(drop_paths(inner, &wanted)),
                        other => other.clone(),
                    })
                    .collect(),
            ),
            other => other.clone(),
        };
        out.insert(name.clone(), kept);
    }
    out
}

impl Projection {
    /// The document as the projection shows it; a positional `.$` path shows nothing, since it
    /// needs the query (`apply_matching`).
    pub fn apply(&self, document: &Document) -> Document {
        self.apply_with(document, None)
    }

    /// The document as the projection shows it, with a positional `.$` path showing the first
    /// array element through which `query` matched.
    pub fn apply_matching(&self, document: &Document, query: &Filter) -> Document {
        self.apply_with(document, Some(query))
    }

    fn apply_with(&self, document: &Document, query: Option<&Filter>) -> Document {
        let mut kept: Vec<&str> = self.paths.iter().map(String::as_str).collect();
        if self.inclusive {
            for (path, operator) in &self.operators {
                if matches!(operator, ProjectionOperator::Slice { .. }) {
                    kept.push(path);
                }
            }
        }
        let parts: Vec<Vec<&str>> = kept.iter().map(|path| path.split('.').collect()).collect();
        let mut out = if self.inclusive {
            keep_paths(document, &parts)
        } else {
            drop_paths(document, &parts)
        };
        if self.keep_id {
            if let Some(id) = document.get("_id") {
                out.insert_first("_id", id.clone());
            }
        } else {
            out.remove("_id");
        }
        for (path, operator) in &self.operators {
            match operator {
                ProjectionOperator::Slice { skip, limit } => {
                    if let Ok(Some(target)) = slot(&mut out, path, false) {
                        let items = match target.get() {
                            Some(Bson::Array(items)) => Some(sliced(items, *skip, *limit)),
                            _ => None,
                        };
                        if let Some(items) = items {
                            target.set(Bson::Array(items));
                        }
                    }
                }
                ProjectionOperator::ElemMatch(condition) => {
                    let first = match document.get(path) {
                        Some(Bson::Array(items)) => items.iter().find(|item| match item {
                            Bson::Document(inner) => matches(condition, inner),
                            _ => false,
                        }),
                        _ => None,
                    };
                    match first {
                        Some(first) => out.insert(path.clone(), Bson::Array(vec![first.clone()])),
                        None => {
                            out.remove(path);
                        }
                    }
                }
                ProjectionOperator::Positional => {
                    let element = query
                        .and_then(|query| matched_array_index(query, document))
                        .and_then(|at| match crate::field_value(document, path) {
                            Some(Bson::Array(items)) => items.get(at).cloned(),
                            _ => None,
                        });
                    match element {
                        Some(element) => {
                            if let Ok(Some(target)) = slot(&mut out, path, true) {
                                target.set(Bson::Array(vec![element]));
                            }
                        }
                        None => {
                            if let Ok(Some(target)) = slot(&mut out, path, false) {
                                target.remove();
                            }
                        }
                    }
                }
                ProjectionOperator::Expression(expression) => {
                    let value = evaluate(expression, document).ok().flatten();
                    match value {
                        Some(value) => {
                            if let Ok(Some(target)) = slot(&mut out, path, true) {
                                target.set(value);
                            }
                        }
                        None => {
                            if let Ok(Some(target)) = slot(&mut out, path, false) {
                                target.remove();
                            }
                        }
                    }
                }
            }
        }
        out
    }
}
