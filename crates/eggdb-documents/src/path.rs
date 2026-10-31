use eggdb_bson::{Bson, Document};

/// A value a path reaches in a document, or the note that it reaches nothing.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Reached<'d> {
    Value(&'d Bson),
    Missing,
}

fn index_of(part: &str) -> Option<usize> {
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

/// Where a path to change ends: the container and the last part.
pub(crate) enum Slot<'d> {
    Field(&'d mut Document, String),
    Element(&'d mut Vec<Bson>, usize),
}

/// The slot `path` names, creating embedded documents on the way when `create` is set.
/// Fails with a message when a part goes through a value that is neither a document nor an
/// array.
pub(crate) fn slot<'d>(document: &'d mut Document, path: &str, create: bool) -> Result<Option<Slot<'d>>, String> {
    let parts: Vec<&str> = path.split('.').collect();
    if parts.iter().any(|part| part.is_empty()) {
        return Err(format!("The update path '{path}' contains an empty field name, which is not allowed."));
    }
    let (last, middle) = parts.split_last().ok_or_else(|| "an empty path".to_owned())?;
    let mut container = ContainerMut::Document(document);
    for part in middle {
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
                        return Err(format!("Cannot create field '{part}' in element {{{part}: {}}}", other.type_name()));
                    }
                    _ => return Ok(None),
                }
            }
            ContainerMut::Array(items) => {
                let Some(at) = index_of(part) else {
                    return Err(format!("Cannot create field '{part}' in an array"));
                };
                if at >= items.len() {
                    if !create {
                        return Ok(None);
                    }
                    if at > items.len() + 1_000_000 {
                        return Err("the array index is too large".to_owned());
                    }
                    items.resize(at, Bson::Null);
                    items.push(Bson::Document(Document::new()));
                }
                if create && matches!(items[at], Bson::Null) {
                    items[at] = Bson::Document(Document::new());
                }
                match &mut items[at] {
                    Bson::Document(next) => ContainerMut::Document(next),
                    Bson::Array(next) => ContainerMut::Array(next),
                    other if create => {
                        return Err(format!("Cannot create field '{part}' in element {{{part}: {}}}", other.type_name()));
                    }
                    _ => return Ok(None),
                }
            }
        };
    }
    Ok(Some(match container {
        ContainerMut::Document(inner) => Slot::Field(inner, (*last).to_owned()),
        ContainerMut::Array(items) => {
            let Some(at) = index_of(last) else {
                return Err(format!("Cannot create field '{last}' in an array"));
            };
            if at > items.len() + 1_000_000 {
                return Err("the array index is too large".to_owned());
            }
            Slot::Element(items, at)
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
            Slot::Element(items, at) => items.get_mut(at).map(|item| std::mem::replace(item, Bson::Null)),
        }
    }
}
