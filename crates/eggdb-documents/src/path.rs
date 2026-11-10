use crate::error::{DocumentError, DocumentResult};
use eggdb_bson::{Bson, Document, relaxed_json};

/// A value a path reaches in a document, or the note that it reaches nothing.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Reached<'d> {
    Value(&'d Bson),
    Missing,
}

pub(crate) fn index_of(part: &str) -> Option<usize> {
    if part.is_empty() || part.len() > 9 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
}

fn reach<'d>(value: &'d Bson, parts: &[&str], out: &mut Vec<Reached<'d>>) {
    let Some((first, rest)) = parts.split_first() else {
        out.push(Reached::Value(value));
        return;
    };
    match value {
        Bson::Document(document) => match document.get(first) {
            Some(inner) => reach(inner, rest, out),
            None => out.push(Reached::Missing),
        },
        Bson::Array(items) => {
            let before = out.len();
            if let Some(at) = index_of(first)
                && let Some(item) = items.get(at)
            {
                reach(item, rest, out);
            }
            for item in items {
                if matches!(item, Bson::Document(_)) {
                    reach(item, parts, out);
                }
            }
            if out.len() == before {
                out.push(Reached::Missing);
            }
        }
        _ => out.push(Reached::Missing),
    }
}

/// Every value `path` (dotted) reaches in `document` the way query filters read it: through
/// embedded documents, into every document element of an array, and by numeric index.
pub(crate) fn reached<'d>(document: &'d Document, path: &str) -> Vec<Reached<'d>> {
    let parts: Vec<&str> = path.split('.').collect();
    let mut out = Vec::new();
    match document.get(parts[0]) {
        Some(value) => reach(value, &parts[1..], &mut out),
        None => out.push(Reached::Missing),
    }
    out
}

/// Every value the path `parts` reaches from `value`, the way [`reached`] reads a document.
pub(crate) fn reached_in<'d>(value: &'d Bson, parts: &[&str]) -> Vec<Reached<'d>> {
    let mut out = Vec::new();
    reach(value, parts, &mut out);
    out
}

/// The one value `path` names exactly (numeric parts index arrays), as updates and
/// projections read it.
pub(crate) fn get_exact<'d>(document: &'d Document, path: &str) -> Option<&'d Bson> {
    let mut parts = path.split('.');
    let mut value = document.get(parts.next()?)?;
    for part in parts {
        value = match value {
            Bson::Document(inner) => inner.get(part)?,
            Bson::Array(items) => items.get(index_of(part)?)?,
            _ => return None,
        };
    }
    Some(value)
}

const MAX_PATH_PARTS: usize = 100;

/// Where a path to change ends: the container and the last part.
pub(crate) enum Slot<'d> {
    Field(&'d mut Document, String),
    Element(&'d mut Vec<Bson>, usize),
}

fn not_viable(field: &str, container_name: &str, container: Bson) -> DocumentError {
    DocumentError::path_not_viable(format!(
        "Cannot create field '{field}' in element {}",
        relaxed_json(&Document::from_entries(vec![(
            container_name.to_owned(),
            container
        )]))
    ))
}

fn too_large_index() -> DocumentError {
    DocumentError::bad_value("the array index is too large")
}

/// The slot `path` names, creating embedded documents on the way when `create` is set.
/// Fails with `PathNotViable` when a part goes through a value that is neither a document
/// nor an array, or names a field of an array.
pub(crate) fn slot<'d>(
    document: &'d mut Document,
    path: &str,
    create: bool,
) -> DocumentResult<Option<Slot<'d>>> {
    let parts: Vec<&str> = path.split('.').collect();
    if parts.len() > MAX_PATH_PARTS {
        return Err(DocumentError::bad_value(format!(
            "The update path has {} parts; a path may have at most {MAX_PATH_PARTS}.",
            parts.len()
        )));
    }
    if parts.iter().any(|part| part.is_empty()) {
        return Err(DocumentError::bad_value(format!(
            "The update path '{path}' contains an empty field name, which is not allowed."
        )));
    }
    let Some((last, middle)) = parts.split_last() else {
        return Err(DocumentError::bad_value("an empty path"));
    };
    let mut container = ContainerMut::Document(document);
    let mut container_name = "";
    for (at, part) in middle.iter().enumerate() {
        let next_part = parts.get(at + 1).copied().unwrap_or(*last);
        container = match container {
            ContainerMut::Document(inner) => {
                if inner.get(part).is_none() {
                    if !create {
                        return Ok(None);
                    }
                    inner.insert(*part, Bson::Document(Document::new()));
                }
                match inner.get_mut(part) {
                    Some(Bson::Document(next)) => ContainerMut::Document(next),
                    Some(Bson::Array(next)) => ContainerMut::Array(next),
                    Some(other) if create => {
                        return Err(not_viable(next_part, part, other.clone()));
                    }
                    _ => return Ok(None),
                }
            }
            ContainerMut::Array(items) => {
                let Some(index) = index_of(part) else {
                    return Err(not_viable(part, container_name, Bson::Array(items.clone())));
                };
                if index >= items.len() {
                    if !create {
                        return Ok(None);
                    }
                    if index > items.len() + 1_000_000 {
                        return Err(too_large_index());
                    }
                    items.resize(index, Bson::Null);
                    items.push(Bson::Document(Document::new()));
                }
                let Some(element) = items.get_mut(index) else {
                    return Ok(None);
                };
                if create && matches!(element, Bson::Null) {
                    *element = Bson::Document(Document::new());
                }
                match element {
                    Bson::Document(next) => ContainerMut::Document(next),
                    Bson::Array(next) => ContainerMut::Array(next),
                    other if create => {
                        return Err(not_viable(next_part, part, other.clone()));
                    }
                    _ => return Ok(None),
                }
            }
        };
        container_name = *part;
    }
    Ok(Some(match container {
        ContainerMut::Document(inner) => Slot::Field(inner, (*last).to_owned()),
        ContainerMut::Array(items) => {
            let Some(index) = index_of(last) else {
                return Err(not_viable(last, container_name, Bson::Array(items.clone())));
            };
            if index > items.len() + 1_000_000 {
                return Err(too_large_index());
            }
            Slot::Element(items, index)
        }
    }))
}

enum ContainerMut<'d> {
    Document(&'d mut Document),
    Array(&'d mut Vec<Bson>),
}

impl Slot<'_> {
    pub(crate) fn get(&self) -> Option<&Bson> {
        match self {
            Slot::Field(document, name) => document.get(name),
            Slot::Element(items, at) => items.get(*at),
        }
    }

    pub(crate) fn set(self, value: Bson) {
        match self {
            Slot::Field(document, name) => document.insert(name, value),
            Slot::Element(items, at) => {
                if at >= items.len() {
                    items.resize(at, Bson::Null);
                    items.push(value);
                } else {
                    items[at] = value;
                }
            }
        }
    }

    pub(crate) fn remove(self) -> Option<Bson> {
        match self {
            Slot::Field(document, name) => document.remove(&name),
            Slot::Element(items, at) => items
                .get_mut(at)
                .map(|item| std::mem::replace(item, Bson::Null)),
        }
    }
}
