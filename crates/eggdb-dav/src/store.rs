//! WebDAV resources on a key-value database (D895): each path is a collection (plain, a
//! calendar or an address book) or a file with its content type and body, and each keeps its
//! dead properties, last-modified time and entity tag.

use eggdb_keyspace::{KeysRead, KeysWrite};

const RESOURCE: u8 = b'R';
const COUNTER: &[u8] = b"\x00etag";

/// What a resource is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A file.
    File,
    /// A plain collection.
    Collection,
    /// A CalDAV calendar collection.
    Calendar,
    /// A CardDAV address book collection.
    AddressBook,
}

impl Kind {
    fn code(self) -> u8 {
        match self {
            Self::File => 0,
            Self::Collection => 1,
            Self::Calendar => 2,
            Self::AddressBook => 3,
        }
    }

    fn of_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::File,
            1 => Self::Collection,
            2 => Self::Calendar,
            3 => Self::AddressBook,
            _ => return None,
        })
    }

    /// Whether the resource holds others.
    pub fn is_collection(self) -> bool {
        self != Self::File
    }
}

/// A stored resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    /// Its path, `/` or `/a/b` without a trailing slash.
    pub path: String,
    /// What it is.
    pub kind: Kind,
    /// Its content type, for files.
    pub content_type: String,
    /// When it last changed, in seconds since 1970.
    pub modified: u64,
    /// Its entity tag's number, new at every change.
    pub version: u64,
    /// Its dead properties: namespace, name and XML text.
    pub properties: Vec<(String, String, String)>,
    /// Its body, for files.
    pub body: Vec<u8>,
}

impl Resource {
    /// The quoted entity tag.
    pub fn etag(&self) -> String {
        format!("\"{}-{}\"", self.modified, self.version)
    }
}

/// `path` with `.` and `..` segments, repeated and trailing slashes removed; `None` when it
/// climbs above the root or holds a NUL.
pub fn normalized(path: &str) -> Option<String> {
    if path.contains('\0') {
        return None;
    }
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    Some(format!("/{}", segments.join("/")))
}

/// The parent of `path`, `None` for the root.
pub fn parent(path: &str) -> Option<String> {
    if path == "/" {
        return None;
    }
    let (parent, _) = path.rsplit_once('/')?;
    Some(if parent.is_empty() { "/".to_owned() } else { parent.to_owned() })
}

fn key(path: &str) -> Vec<u8> {
    let mut key = vec![RESOURCE];
    key.extend_from_slice(path.as_bytes());
    key
}

fn put_field(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn take_field<'b>(bytes: &'b [u8], at: &mut usize) -> Option<&'b [u8]> {
    let length = u32::from_be_bytes(bytes.get(*at..*at + 4)?.try_into().ok()?) as usize;
    *at += 4;
    let field = bytes.get(*at..*at + length)?;
    *at += length;
    Some(field)
}

fn encoded(resource: &Resource) -> Vec<u8> {
    let mut out = vec![resource.kind.code()];
    out.extend_from_slice(&resource.modified.to_be_bytes());
    out.extend_from_slice(&resource.version.to_be_bytes());
    put_field(&mut out, resource.content_type.as_bytes());
    out.extend_from_slice(&(resource.properties.len() as u32).to_be_bytes());
    for (namespace, name, value) in &resource.properties {
        put_field(&mut out, namespace.as_bytes());
        put_field(&mut out, name.as_bytes());
        put_field(&mut out, value.as_bytes());
    }
    out.extend_from_slice(&resource.body);
    out
}

fn decoded(path: &str, bytes: &[u8]) -> Option<Resource> {
    let kind = Kind::of_code(*bytes.first()?)?;
    let modified = u64::from_be_bytes(bytes.get(1..9)?.try_into().ok()?);
    let version = u64::from_be_bytes(bytes.get(9..17)?.try_into().ok()?);
    let mut at = 17;
    let text = |field: &[u8]| String::from_utf8(field.to_vec()).ok();
    let content_type = text(take_field(bytes, &mut at)?)?;
    let count = u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize;
    at += 4;
    let mut properties = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let namespace = text(take_field(bytes, &mut at)?)?;
        let name = text(take_field(bytes, &mut at)?)?;
        let value = text(take_field(bytes, &mut at)?)?;
        properties.push((namespace, name, value));
    }
    Some(Resource {
        path: path.to_owned(),
        kind,
        content_type,
        modified,
        version,
        properties,
        body: bytes.get(at..)?.to_vec(),
    })
}

/// The resource at `path`; the root is always a collection.
pub fn get(keys: &dyn KeysRead, path: &str) -> Result<Option<Resource>, String> {
    if let Some(bytes) = keys.get(&key(path))? {
        return Ok(decoded(path, &bytes));
    }
    Ok((path == "/").then(|| Resource {
        path: "/".to_owned(),
        kind: Kind::Collection,
        content_type: String::new(),
        modified: 0,
        version: 0,
        properties: Vec::new(),
        body: Vec::new(),
    }))
}

/// The resources directly inside collection `path`.
pub fn children(keys: &dyn KeysRead, path: &str) -> Result<Vec<Resource>, String> {
    let prefix = if path == "/" { "/".to_owned() } else { format!("{path}/") };
    let mut found = Vec::new();
    for (stored, value) in keys.scan(&key(&prefix))? {
        let Some(child) = stored.get(1..).and_then(|bytes| std::str::from_utf8(bytes).ok()) else {
            continue;
        };
        let Some(name) = child.strip_prefix(&prefix) else {
            continue;
        };
        if name.is_empty() || name.contains('/') {
            continue;
        }
        if let Some(resource) = decoded(child, &value) {
            found.push(resource);
        }
    }
    Ok(found)
}

/// Every resource at or under `path`.
pub fn subtree(keys: &dyn KeysRead, path: &str) -> Result<Vec<Resource>, String> {
    let mut found = Vec::new();
    if let Some(resource) = get(keys, path)? {
        found.push(resource);
    }
    let prefix = if path == "/" { "/".to_owned() } else { format!("{path}/") };
    for (stored, value) in keys.scan(&key(&prefix))? {
        let Some(child) = stored.get(1..).and_then(|bytes| std::str::from_utf8(bytes).ok()) else {
            continue;
        };
        if child == path {
            continue;
        }
        if let Some(resource) = decoded(child, &value) {
            found.push(resource);
        }
    }
    Ok(found)
}

fn next_version(keys: &mut dyn KeysWrite) -> Result<u64, String> {
    let current = keys.get(COUNTER)?.and_then(|bytes| bytes.try_into().ok()).map_or(0, u64::from_be_bytes);
    let next = current + 1;
    keys.put(COUNTER, &next.to_be_bytes())?;
    Ok(next)
}

/// Stores `resource` with a new version and `modified` time.
pub fn put(keys: &mut dyn KeysWrite, mut resource: Resource, modified: u64) -> Result<Resource, String> {
    resource.version = next_version(keys)?;
    resource.modified = modified;
    keys.put(&key(&resource.path), &encoded(&resource))?;
    if let Some(parent_path) = parent(&resource.path)
        && let Some(mut container) = get(&*keys, &parent_path)?
        && parent_path != "/"
    {
        container.version = next_version(keys)?;
        keys.put(&key(&parent_path), &encoded(&container))?;
    }
    Ok(resource)
}

/// Removes the resource at `path` and everything under it; returns how many.
pub fn remove(keys: &mut dyn KeysWrite, path: &str) -> Result<usize, String> {
    let gone = subtree(&*keys, path)?;
    for resource in &gone {
        keys.delete(&key(&resource.path))?;
    }
    if let Some(parent_path) = parent(path)
        && parent_path != "/"
        && let Some(mut container) = get(&*keys, &parent_path)?
    {
        container.version = next_version(keys)?;
        keys.put(&key(&parent_path), &encoded(&container))?;
    }
    Ok(gone.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_keyspace::{Keyspace, MemoryKeyspace};

    #[test]
    fn paths_normalize_and_resources_nest() {
        assert_eq!(normalized("/a//b/./c/../d/").as_deref(), Some("/a/b/d"));
        assert_eq!(normalized("/../x"), None);
        assert_eq!(parent("/a/b").as_deref(), Some("/a"));
        assert_eq!(parent("/a").as_deref(), Some("/"));
        let keyspace = MemoryKeyspace::default();
        keyspace
            .write(0, &mut |keys| {
                let resource = |path: &str, kind: Kind| Resource {
                    path: path.to_owned(),
                    kind,
                    content_type: "text/plain".to_owned(),
                    modified: 0,
                    version: 0,
                    properties: vec![("DAV:".to_owned(), "displayname".to_owned(), "Docs".to_owned())],
                    body: b"hello".to_vec(),
                };
                put(keys, resource("/docs", Kind::Collection), 1)?;
                put(keys, resource("/docs/a.txt", Kind::File), 2)?;
                put(keys, resource("/docs/sub", Kind::Collection), 3)?;
                put(keys, resource("/docs/sub/b.txt", Kind::File), 4)?;
                assert_eq!(children(&*keys, "/docs")?.len(), 2);
                assert_eq!(children(&*keys, "/")?.len(), 1);
                let file = get(&*keys, "/docs/a.txt")?.unwrap_or_else(|| panic!("stored"));
                assert_eq!(file.body, b"hello");
                assert_eq!(file.properties[0].2, "Docs");
                assert_eq!(remove(keys, "/docs/sub")?, 2);
                assert_eq!(subtree(&*keys, "/docs")?.len(), 2);
                Ok(())
            })
            .unwrap();
    }
}
