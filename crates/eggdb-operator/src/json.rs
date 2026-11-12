use eggdb_sql::json::{Json, Node};

pub(crate) fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

pub(crate) fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    let Json::Object(members) = &node.value else {
        return None;
    };
    members.iter().find(|(key, _)| key == name).map(|(_, value)| value)
}

pub(crate) fn path<'n>(node: &'n Node, names: &[&str]) -> Option<&'n Node> {
    names.iter().try_fold(node, |at, name| member(at, name))
}

pub(crate) fn text<'n>(node: &'n Node, names: &[&str]) -> Option<&'n str> {
    match &path(node, names)?.value {
        Json::String(text) => Some(text),
        _ => None,
    }
}

pub(crate) fn number(node: &Node, names: &[&str]) -> Option<i64> {
    match &path(node, names)?.value {
        Json::Number(text) => text.parse().ok(),
        _ => None,
    }
}

pub(crate) fn items(node: &Node) -> &[Node] {
    match member(node, "items").map(|items| &items.value) {
        Some(Json::Array(items)) => items,
        _ => &[],
    }
}
