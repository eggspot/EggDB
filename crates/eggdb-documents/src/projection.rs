use crate::error::{DocumentError, DocumentResult};
use eggdb_bson::{Bson, Document};

/// A parsed projection: the paths to keep or to drop, and whether `_id` is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    paths: Vec<String>,
    inclusive: bool,
    keep_id: bool,
}

/// Reads a projection such as `{name: 1, _id: 0}` or `{secret: 0}`.
pub fn parse_projection(spec: &Document) -> DocumentResult<Projection> {
    let mut inclusive = None;
    let mut keep_id = true;
    let mut paths = Vec::new();
    for (path, value) in spec.iter() {
        if matches!(value, Bson::Document(_)) || path.contains('$') {
            return Err(DocumentError::not_implemented("projection operators and expressions are not supported yet"));
        }
        let include = value.is_truthy();
        if path == "_id" {
            keep_id = include;
            continue;
        }
        match inclusive {
            Some(mode) if mode != include => {
                return Err(DocumentError::new_projection_conflict(path, include));
            }
            _ => inclusive = Some(include),
        }
        paths.push(path.clone());
    }
    let only_id_kept = inclusive.is_none() && keep_id && spec.contains_key("_id");
    Ok(Projection { paths, inclusive: inclusive.unwrap_or(only_id_kept), keep_id })
}

impl DocumentError {
    fn new_projection_conflict(path: &str, include: bool) -> Self {
        let mode = if include { "inclusion" } else { "exclusion" };
        let other = if include { "exclusion" } else { "inclusion" };
        Self::bad_value(format!("Cannot do {mode} on field {path} in {other} projection"))
    }
}

fn keep_paths(source: &Document, paths: &[Vec<&str>]) -> Document {
    let mut out = Document::new();
    for (name, value) in source.iter() {
        let wanted: Vec<Vec<&str>> = paths.iter().filter(|parts| parts[0] == name).map(|parts| parts[1..].to_vec()).collect();
        if wanted.is_empty() {
            continue;
        }
        if wanted.iter().any(Vec::is_empty) {
            out.insert(name.clone(), value.clone());
            continue;
        }
        match value {
            Bson::Document(inner) => out.insert(name.clone(), Bson::Document(keep_paths(inner, &wanted))),
            Bson::Array(items) => out.insert(
                name.clone(),
                Bson::Array(
                    items
                        .iter()
                        .filter_map(|item| match item {
                            Bson::Document(inner) => Some(Bson::Document(keep_paths(inner, &wanted))),
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
        let wanted: Vec<Vec<&str>> = paths.iter().filter(|parts| parts[0] == name).map(|parts| parts[1..].to_vec()).collect();
        if wanted.iter().any(Vec::is_empty) {
            continue;
        }
        let kept = match value {
            Bson::Document(inner) if !wanted.is_empty() => Bson::Document(drop_paths(inner, &wanted)),
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
    /// The document as the projection shows it.
    pub fn apply(&self, document: &Document) -> Document {
        let parts: Vec<Vec<&str>> = self.paths.iter().map(|path| path.split('.').collect()).collect();
        let mut out = if self.inclusive { keep_paths(document, &parts) } else { drop_paths(document, &parts) };
        if self.keep_id {
            if let Some(id) = document.get("_id") {
                out.insert_first("_id", id.clone());
            }
        } else {
            out.remove("_id");
        }
        out
    }
}
