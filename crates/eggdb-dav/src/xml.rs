//! A small XML reader for WebDAV request bodies (XML 1.0 elements, attributes, text, CDATA,
//! comments and processing instructions skipped, the five predefined entities and character
//! references), resolving namespace prefixes so `D:prop` and `prop` in `DAV:` read the same,
//! and an escaper for writing.

const MAX_DEPTH: usize = 64;
const MAX_ELEMENTS: usize = 100_000;

/// The `DAV:` namespace.
pub const DAV: &str = "DAV:";
/// CalDAV's namespace.
pub const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
/// CardDAV's namespace.
pub const CARDDAV: &str = "urn:ietf:params:xml:ns:carddav";

/// An element with its namespace, local name, children and text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Element {
    /// The namespace URI, empty when none.
    pub namespace: String,
    /// The local name.
    pub name: String,
    /// The child elements, in order.
    pub children: Vec<Element>,
    /// The text directly inside, entities decoded.
    pub text: String,
    /// The attributes other than namespace declarations, by local name.
    pub attributes: Vec<(String, String)>,
}

impl Element {
    /// The value of attribute `name` (its local name).
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes.iter().find(|(found, _)| found == name).map(|(_, value)| value.as_str())
    }

    /// Whether the element is `name` in `namespace`.
    pub fn is(&self, namespace: &str, name: &str) -> bool {
        self.namespace == namespace && self.name == name
    }

    /// The first child `name` in `namespace`.
    pub fn child(&self, namespace: &str, name: &str) -> Option<&Element> {
        self.children.iter().find(|child| child.is(namespace, name))
    }

    /// Every element under this one, at any depth, named `name` in `namespace`.
    pub fn descendants<'e>(&'e self, namespace: &str, name: &str, out: &mut Vec<&'e Element>) {
        for child in &self.children {
            if child.is(namespace, name) {
                out.push(child);
            }
            child.descendants(namespace, name, out);
        }
    }
}

/// `text` with `&`, `<`, `>` and `"` escaped.
pub fn escaped(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn decoded(text: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let end = rest[at..].find(';')? + at;
        let entity = &rest[at + 1..end];
        match entity {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            numeric => {
                let code = match numeric.strip_prefix("#x").or_else(|| numeric.strip_prefix("#X")) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                    None => numeric.strip_prefix('#')?.parse().ok()?,
                };
                out.push(char::from_u32(code)?);
            }
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

struct Reader<'t> {
    text: &'t str,
    at: usize,
    elements: usize,
}

impl Reader<'_> {
    fn rest(&self) -> &str {
        self.text.get(self.at..).unwrap_or_default()
    }

    fn skip_until(&mut self, end: &str) -> Option<()> {
        let found = self.rest().find(end)?;
        self.at += found + end.len();
        Some(())
    }

    fn skip_misc(&mut self) -> Option<()> {
        loop {
            let trimmed = self.rest().trim_start();
            self.at = self.text.len() - trimmed.len();
            if self.rest().starts_with("<?") {
                self.skip_until("?>")?;
            } else if self.rest().starts_with("<!--") {
                self.skip_until("-->")?;
            } else if self.rest().starts_with("<!DOCTYPE") {
                return None;
            } else {
                return Some(());
            }
        }
    }

    fn name(&mut self) -> Option<String> {
        let rest = self.rest();
        let length = rest.find(|character: char| character.is_whitespace() || matches!(character, '/' | '>' | '='))?;
        if length == 0 {
            return None;
        }
        self.at += length;
        Some(rest[..length].to_owned())
    }

    fn element(&mut self, scopes: &[(String, String)], depth: usize) -> Option<Element> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.elements += 1;
        if self.elements > MAX_ELEMENTS {
            return None;
        }
        if !self.rest().starts_with('<') {
            return None;
        }
        self.at += 1;
        let tag = self.name()?;
        let mut attributes = Vec::new();
        loop {
            let trimmed = self.rest().trim_start();
            self.at = self.text.len() - trimmed.len();
            if self.rest().starts_with("/>") || self.rest().starts_with('>') {
                break;
            }
            let name = self.name()?;
            let trimmed = self.rest().trim_start();
            self.at = self.text.len() - trimmed.len();
            if !self.rest().starts_with('=') {
                return None;
            }
            self.at += 1;
            let trimmed = self.rest().trim_start();
            self.at = self.text.len() - trimmed.len();
            let quote = self.rest().chars().next().filter(|quote| matches!(quote, '"' | '\''))?;
            self.at += 1;
            let end = self.rest().find(quote)?;
            let value = decoded(&self.rest()[..end])?;
            self.at += end + 1;
            attributes.push((name, value));
        }
        let mut scope: Vec<(String, String)> = scopes.to_vec();
        for (name, value) in &attributes {
            if name == "xmlns" {
                scope.push((String::new(), value.clone()));
            } else if let Some(prefix) = name.strip_prefix("xmlns:") {
                scope.push((prefix.to_owned(), value.clone()));
            }
        }
        let (prefix, local) = tag.split_once(':').unwrap_or(("", &tag));
        let namespace = scope.iter().rev().find(|(known, _)| known == prefix).map(|(_, uri)| uri.clone()).unwrap_or_default();
        let plain_attributes = attributes
            .into_iter()
            .filter(|(name, _)| name != "xmlns" && !name.starts_with("xmlns:"))
            .map(|(name, value)| (name.rsplit(':').next().unwrap_or(&name).to_owned(), value))
            .collect();
        let mut element = Element {
            namespace,
            name: local.to_owned(),
            children: Vec::new(),
            text: String::new(),
            attributes: plain_attributes,
        };
        if self.rest().starts_with("/>") {
            self.at += 2;
            return Some(element);
        }
        self.at += 1;
        loop {
            if self.rest().starts_with("</") {
                self.at += 2;
                let closing = self.name()?;
                if closing != tag {
                    return None;
                }
                let trimmed = self.rest().trim_start();
                self.at = self.text.len() - trimmed.len();
                if !self.rest().starts_with('>') {
                    return None;
                }
                self.at += 1;
                return Some(element);
            }
            if self.rest().starts_with("<!--") {
                self.skip_until("-->")?;
            } else if self.rest().starts_with("<![CDATA[") {
                self.at += 9;
                let end = self.rest().find("]]>")?;
                element.text.push_str(&self.rest()[..end]);
                self.at += end + 3;
            } else if self.rest().starts_with("<?") {
                self.skip_until("?>")?;
            } else if self.rest().starts_with('<') {
                let child = self.element(&scope, depth + 1)?;
                element.children.push(child);
            } else {
                let end = self.rest().find('<')?;
                element.text.push_str(&decoded(&self.rest()[..end])?);
                self.at += end;
            }
        }
    }
}

/// The document element of `text`, or `None` when it is not well-formed XML (or uses a
/// document type declaration, which is refused).
pub fn parse(text: &str) -> Option<Element> {
    let mut reader = Reader { text, at: 0, elements: 0 };
    reader.skip_misc()?;
    let root = reader.element(&[("xml".to_owned(), "http://www.w3.org/XML/1998/namespace".to_owned())], 0)?;
    reader.skip_misc()?;
    reader.rest().is_empty().then_some(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_resolve_and_entities_decode() {
        let root = parse(
            "<?xml version=\"1.0\"?><D:propfind xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:prop><D:displayname/><C:calendar-data/><x xmlns=\"urn:other\">a &amp; b</x></D:prop></D:propfind>",
        )
        .unwrap();
        assert!(root.is(DAV, "propfind"));
        let prop = root.child(DAV, "prop").unwrap();
        assert!(prop.children[1].is(CALDAV, "calendar-data"));
        assert_eq!(prop.children[2].namespace, "urn:other");
        assert_eq!(prop.children[2].text, "a & b");
        assert_eq!(parse("<a><b></a>"), None);
        assert_eq!(parse("<!DOCTYPE x [<!ENTITY e \"boom\">]><x/>"), None);
        assert_eq!(parse(&"<a>".repeat(100)), None);
    }
}
