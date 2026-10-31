use crate::error::{DocumentError, DocumentResult};
use crate::path::{Reached, reached};
use eggdb_bson::{Bson, Document, compare};
use std::cmp::Ordering;

/// A sort order: paths with their directions, the first deciding first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SortSpec {
    keys: Vec<(String, bool)>,
}

impl SortSpec {
    /// Whether the order has no keys.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Reads a sort document such as `{age: -1, name: 1}`.
pub fn parse_sort(spec: &Document) -> DocumentResult<SortSpec> {
    let mut keys = Vec::new();
    for (path, direction) in spec.iter() {
        let descending = match direction.as_i64() {
            Some(1) => false,
            Some(-1) => true,
            _ if matches!(direction, Bson::Document(inner) if inner.contains_key("$meta")) => {
                return Err(DocumentError::not_implemented("$meta sort keys are not supported yet"));
            }
            _ => return Err(DocumentError::bad_value(format!("$sort key ordering must be 1 (for ascending) or -1 (for descending) for {path}"))),
        };
        if path.is_empty() {
            return Err(DocumentError::bad_value("FieldPath cannot be constructed with empty string"));
        }
        keys.push((path.clone(), descending));
    }
    Ok(SortSpec { keys })
}

fn key_of(document: &Document, path: &str, descending: bool) -> Bson {
    let mut best: Option<Bson> = None;
    for item in reached(document, path) {
        let candidates: Vec<&Bson> = match item {
            Reached::Value(Bson::Array(items)) if items.is_empty() => vec![&Bson::Undefined],
            Reached::Value(Bson::Array(items)) => items.iter().collect(),
            Reached::Value(value) => vec![value],
            Reached::Missing => vec![&Bson::Null],
        };
        for candidate in candidates {
            let better = match &best {
                None => true,
                Some(held) => {
                    let ordering = compare(candidate, held);
                    if descending { ordering == Ordering::Greater } else { ordering == Ordering::Less }
                }
            };
            if better {
                best = Some(candidate.clone());
            }
        }
    }
    best.unwrap_or(Bson::Null)
}

/// Compares two documents by `spec`, arrays sorting by their least element ascending and their
/// greatest descending, as MongoDB documents.
pub fn compare_by(spec: &SortSpec, left: &Document, right: &Document) -> Ordering {
    for (path, descending) in &spec.keys {
        let ordering = compare(&key_of(left, path, *descending), &key_of(right, path, *descending));
        let ordering = if *descending { ordering.reverse() } else { ordering };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// Sorts `documents` by `spec`, keeping the order of equal ones.
pub fn sort_documents(documents: &mut [Document], spec: &SortSpec) {
    if !spec.is_empty() {
        documents.sort_by(|left, right| compare_by(spec, left, right));
    }
}
