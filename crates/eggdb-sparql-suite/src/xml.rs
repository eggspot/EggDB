const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Element {
    pub(crate) name: String,
    pub(crate) attributes: Vec<(String, String)>,
    pub(crate) children: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    Element(Element),
    Text(String),
}

impl Element {
    pub(crate) fn local_name(&self) -> &str {
        self.name.rsplit(':').next().unwrap_or(&self.name)
    }

    pub(crate) fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    pub(crate) fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|node| match node {
            Node::Element(element) => Some(element),
            Node::Text(_) => None,
        })
    }

    pub(crate) fn child(&self, name: &str) -> Option<&Element> {
        self.elements().find(|element| element.local_name() == name)
    }

    pub(crate) fn text(&self) -> String {
        self.children
            .iter()
            .map(|node| match node {
                Node::Text(text) => text.clone(),
                Node::Element(element) => element.text(),
            })
            .collect()
    }
}

fn unescaped(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let end = rest[at..].find(';')? + at;
        let entity = &rest[at + 1..end];
        match entity {
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "amp" => out.push('&'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => {
                let code = if let Some(hex) = entity.strip_prefix("#x") {
                    u32::from_str_radix(hex, 16).ok()?
                } else {
                    entity.strip_prefix('#')?.parse().ok()?
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
}

impl Reader<'_> {
    fn rest(&self) -> &str {
        &self.text[self.at..]
    }

    fn skip_until(&mut self, marker: &str) -> Option<()> {
        let found = self.rest().find(marker)?;
        self.at += found + marker.len();
        Some(())
    }

    fn skip_prolog(&mut self) -> Option<()> {
        loop {
            let trimmed = self.rest().trim_start();
            self.at = self.text.len() - trimmed.len();
            if trimmed.starts_with("<?") {
                self.skip_until("?>")?;
            } else if trimmed.starts_with("<!--") {
                self.skip_until("-->")?;
            } else if trimmed.starts_with("<!") {
                self.skip_until(">")?;
            } else {
                return Some(());
            }
        }
    }

    fn name(&mut self) -> String {
        let length = self.rest().find(|character: char| character.is_whitespace() || matches!(character, '>' | '/' | '=')).unwrap_or(0);
        let name = self.rest()[..length].to_owned();
        self.at += length;
        name
    }

    fn element(&mut self, depth: usize) -> Option<Element> {
        if depth > MAX_DEPTH || !self.rest().starts_with('<') {
            return None;
        }
        self.at += 1;
        let name = self.name();
        let mut attributes = Vec::new();
        loop {
            let trimmed = self.rest().trim_start();
            self.at = self.text.len() - trimmed.len();
            if let Some(after) = trimmed.strip_prefix("/>") {
                self.at = self.text.len() - after.len();
                return Some(Element { name, attributes, children: Vec::new() });
            }
            if let Some(after) = trimmed.strip_prefix('>') {
                self.at = self.text.len() - after.len();
                break;
            }
            let key = self.name();
            if key.is_empty() || !self.rest().starts_with('=') {
                return None;
            }
            self.at += 1;
            let quote = self.rest().chars().next().filter(|quote| matches!(quote, '"' | '\''))?;
            self.at += 1;
            let end = self.rest().find(quote)?;
            let value = unescaped(&self.rest()[..end])?;
            self.at += end + 1;
            attributes.push((key, value));
        }
        let mut children = Vec::new();
        loop {
            if self.rest().starts_with("</") {
                self.skip_until(">")?;
                return Some(Element { name, attributes, children });
            }
            if self.rest().starts_with("<!--") {
                self.skip_until("-->")?;
            } else if self.rest().starts_with("<![CDATA[") {
                self.at += 9;
                let end = self.rest().find("]]>")?;
                children.push(Node::Text(self.rest()[..end].to_owned()));
                self.at += end + 3;
            } else if self.rest().starts_with('<') {
                children.push(Node::Element(self.element(depth + 1)?));
            } else {
                let end = self.rest().find('<')?;
                let text = unescaped(&self.rest()[..end])?;
                if !text.trim().is_empty() {
                    children.push(Node::Text(text));
                }
                self.at += end;
            }
        }
    }
}

pub(crate) fn parse(text: &str) -> Option<Element> {
    let mut reader = Reader { text, at: 0 };
    reader.skip_prolog()?;
    reader.element(0)
}
