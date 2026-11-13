//! WebDAV access control (RFC 3744) for the server's one user (D1077): every resource is the
//! user's, so the ACL is one protected grant of all privileges to the user's principal, and an
//! `ACL` request may only restate it.

use crate::xml::{self, DAV, Element, escaped};

const PRIVILEGES: [&str; 9] = ["read", "write", "write-properties", "write-content", "unlock", "bind", "unbind", "read-acl", "read-current-user-privilege-set"];

/// `DAV:current-user-privilege-set`'s value: every privilege, or only reading when the server
/// is read-only.
pub(crate) fn current_user_privileges(read_only: bool) -> String {
    PRIVILEGES
        .iter()
        .filter(|privilege| !read_only || matches!(**privilege, "read" | "read-acl" | "read-current-user-privilege-set"))
        .map(|privilege| format!("<D:privilege><D:{privilege}/></D:privilege>"))
        .collect()
}

/// `DAV:acl`'s value: one protected ACE granting `DAV:all` to `principal`.
pub(crate) fn acl(principal: &str) -> String {
    format!(
        "<D:ace><D:principal><D:href>{}/</D:href></D:principal><D:grant><D:privilege><D:all/></D:privilege></D:grant><D:protected/></D:ace>",
        escaped(principal)
    )
}

/// `DAV:supported-privilege-set`'s value: RFC 3744's privilege tree.
pub(crate) fn supported_privileges() -> String {
    let leaf = |name: &str, description: &str| format!("<D:supported-privilege><D:privilege><D:{name}/></D:privilege><D:description xml:lang=\"en\">{description}</D:description></D:supported-privilege>");
    format!(
        "<D:supported-privilege><D:privilege><D:all/></D:privilege><D:abstract/><D:description xml:lang=\"en\">Any operation</D:description>{}<D:supported-privilege><D:privilege><D:write/></D:privilege><D:description xml:lang=\"en\">Write any object</D:description>{}{}{}{}{}</D:supported-privilege>{}</D:supported-privilege>",
        leaf("read", "Read any object"),
        leaf("write-properties", "Write properties"),
        leaf("write-content", "Write resource content"),
        leaf("unlock", "Unlock resources"),
        leaf("bind", "Add new members to collections"),
        leaf("unbind", "Remove members from collections"),
        format!("{}{}", leaf("read-acl", "Read the ACL"), leaf("read-current-user-privilege-set", "Read the current user's privileges")),
    )
}

/// Why an `ACL` request body is refused, as RFC 3744's precondition name, or `None` when every
/// ACE grants to `principal` itself (which changes nothing).
pub(crate) fn refusal(body: &str, principal: &str) -> Option<&'static str> {
    let Some(document) = xml::parse(body) else {
        return Some("no-ace-conflict");
    };
    if !document.is(DAV, "acl") {
        return Some("no-ace-conflict");
    }
    for ace in document.children.iter().filter(|child| child.is(DAV, "ace")) {
        if ace.child(DAV, "deny").is_some() {
            return Some("grant-only");
        }
        let Some(who) = ace.child(DAV, "principal") else {
            return Some("no-ace-conflict");
        };
        if !names(who, principal) {
            return Some("recognized-principal");
        }
    }
    None
}

fn names(who: &Element, principal: &str) -> bool {
    if who.child(DAV, "self").is_some() {
        return true;
    }
    who.child(DAV, "href").is_some_and(|href| href.text.trim().trim_end_matches('/') == principal)
        || who.child(DAV, "property").is_some_and(|property| property.child(DAV, "owner").is_some())
}

/// Whether a `principal-property-search` asks for the user: every `match` text is found, as
/// RFC 3744 §9.4 asks (or any, with `test="anyof"`), in the user's name or address.
pub(crate) fn search_matches(document: &Element, user: &str, address: &str) -> bool {
    let searches: Vec<&Element> = document.children.iter().filter(|child| child.is(DAV, "property-search")).collect();
    if searches.is_empty() {
        return false;
    }
    let found = |search: &&Element| {
        let wanted = search.child(DAV, "match").map(|found| found.text.trim().to_lowercase()).unwrap_or_default();
        [user.to_lowercase(), address.to_lowercase()].iter().any(|candidate| candidate.contains(&wanted))
    };
    if document.attribute("test") == Some("anyof") { searches.iter().any(found) } else { searches.iter().all(found) }
}
