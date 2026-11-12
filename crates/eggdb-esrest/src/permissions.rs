use eggdb_sql::json::{Json, Node, parse_document};
use std::cell::RefCell;

/// A user of the search API who may only read, and only the documents whose permission field
/// names the user, one of the user's groups, or `*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchUser {
    /// The name the user signs in with.
    pub name: String,
    /// The SHA-256 of the user's password.
    pub password_sha256: [u8; 32],
    /// The groups the user belongs to.
    pub groups: Vec<String>,
}

fn hex_digest(text: &str) -> Option<[u8; 32]> {
    let text = text.trim();
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let mut digest = [0u8; 32];
    for (at, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(at * 2..at * 2 + 2)?, 16).ok()?;
    }
    Some(digest)
}

/// The users of a `--es-users` file: one per line as `name:sha256-of-password:group,group`
/// (the groups may be left out); empty lines and lines starting with `#` are skipped.
pub fn parse_search_users(text: &str) -> Result<Vec<SearchUser>, String> {
    let mut users = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.splitn(3, ':');
        let (Some(name), Some(digest)) = (fields.next(), fields.next()) else {
            return Err(format!("line {}: write name:sha256-of-password:groups", number + 1));
        };
        if name.is_empty() {
            return Err(format!("line {}: the user has no name", number + 1));
        }
        let password_sha256 = hex_digest(digest)
            .ok_or_else(|| format!("line {}: the password digest is not 64 hex digits", number + 1))?;
        let groups = fields
            .next()
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|group| !group.is_empty())
            .map(str::to_owned)
            .collect();
        users.push(SearchUser {
            name: name.to_owned(),
            password_sha256,
            groups,
        });
    }
    Ok(users)
}

/// Who a request comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Principal {
    Administrator,
    Reader { name: String, names: Vec<String> },
}

impl Principal {
    pub(crate) fn reader(user: &SearchUser) -> Self {
        let mut names = vec![user.name.clone(), "*".to_owned()];
        names.extend(user.groups.iter().cloned());
        Self::Reader {
            name: user.name.clone(),
            names,
        }
    }
}

struct Restriction {
    names: Vec<String>,
    field: String,
}

thread_local! {
    static CURRENT: RefCell<Option<Restriction>> = const { RefCell::new(None) };
}

/// Keeps a request's restriction in force until it is dropped.
pub(crate) struct Entered {
    previous: Option<Restriction>,
}

impl Drop for Entered {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CURRENT.with(|current| {
            if let Ok(mut current) = current.try_borrow_mut() {
                *current = previous;
            }
        });
    }
}

/// Restricts what the rest of the request reads to what `principal` may see.
pub(crate) fn enter(principal: &Principal, field: &str) -> Entered {
    let restriction = match principal {
        Principal::Administrator => None,
        Principal::Reader { names, .. } => Some(Restriction {
            names: names.clone(),
            field: field.to_owned(),
        }),
    };
    let previous = CURRENT.with(|current| {
        current
            .try_borrow_mut()
            .map(|mut current| std::mem::replace(&mut *current, restriction))
            .unwrap_or_default()
    });
    Entered { previous }
}

fn permitted(value: &Node, names: &[String]) -> bool {
    match &value.value {
        Json::String(text) => names.iter().any(|name| name == text),
        Json::Array(items) => items.iter().any(|item| permitted(item, names)),
        _ => false,
    }
}

fn field_of<'n>(source: &'n Node, path: &str) -> Option<&'n Node> {
    path.split('.').try_fold(source, |node, part| match &node.value {
        Json::Object(members) => members.iter().find(|(name, _)| name == part).map(|(_, value)| value),
        _ => None,
    })
}

/// Whether the request's user may see a document with this parsed `_source`.
pub(crate) fn visible(source: &Node) -> bool {
    CURRENT.with(|current| {
        let Ok(current) = current.try_borrow() else {
            return false;
        };
        match current.as_ref() {
            None => true,
            Some(restriction) => {
                field_of(source, &restriction.field).is_some_and(|value| permitted(value, &restriction.names))
            }
        }
    })
}

/// Whether the request's user may see a document with this `_source` text.
pub(crate) fn source_visible(source: &str) -> bool {
    let restricted = CURRENT.with(|current| current.try_borrow().map_or(true, |current| current.is_some()));
    !restricted || parse_document(source).is_ok_and(|parsed| visible(&parsed))
}

/// Whether a request only reads: what a reader may send.
pub(crate) fn only_reads(method: &str, parts: &[&str]) -> bool {
    match method {
        "GET" | "HEAD" => true,
        "POST" => matches!(
            parts.last().copied(),
            Some("_search" | "_count" | "_msearch" | "_mget" | "_field_caps" | "_analyze" | "scroll" | "template")
        ) || matches!(parts, ["_search", "scroll", _]),
        "DELETE" => matches!(parts, ["_search", "scroll", ..] | ["_pit"]),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readers_see_only_documents_naming_them_or_their_groups() {
        let users = parse_search_users(&format!("# readers\nada:{}:engineering, all\n", "ab".repeat(32))).unwrap();
        assert_eq!(users[0].groups, ["engineering", "all"]);
        let reader = Principal::reader(&users[0]);
        let open = parse_document("{\"readers\":[\"engineering\"]}").unwrap();
        let closed = parse_document("{\"readers\":\"finance\"}").unwrap();
        let unmarked = parse_document("{\"title\":\"x\"}").unwrap();
        let everyone = parse_document("{\"acl\":{\"readers\":\"*\"}}").unwrap();
        {
            let _entered = enter(&reader, "readers");
            assert!(visible(&open));
            assert!(!visible(&closed));
            assert!(!visible(&unmarked));
        }
        {
            let _entered = enter(&reader, "acl.readers");
            assert!(visible(&everyone));
        }
        assert!(visible(&closed));
        assert!(only_reads("POST", &["docs", "_search"]));
        assert!(!only_reads("POST", &["docs", "_doc"]));
        assert!(parse_search_users("bob:short").is_err());
    }
}
