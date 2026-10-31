const MAX_ELEMENTS: usize = 10_000;

pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

pub(crate) fn unescape(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let end = rest[at..].find(';').map(|offset| at + offset).ok_or("an XML entity is not closed")?;
        let entity = &rest[at + 1..end];
        let character = match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let code = if let Some(hex) = entity.strip_prefix("#x") {
                    u32::from_str_radix(hex, 16).ok()
                } else {
                    entity.strip_prefix('#').and_then(|decimal| decimal.parse::<u32>().ok())
                };
                code.and_then(char::from_u32).ok_or_else(|| format!("&{entity}; is not an XML entity this endpoint reads"))?
            }
        };
        out.push(character);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The raw contents of every `<name>...</name>` element in `xml`, in order, nested ones
/// included.
pub(crate) fn elements<'x>(xml: &'x str, name: &str) -> Result<Vec<&'x str>, String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let mut found = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        if found.len() >= MAX_ELEMENTS {
            return Err("the XML document has too many elements".to_owned());
        }
        let body_start = start + open.len();
        let end = rest[body_start..].find(&close).map(|offset| body_start + offset).ok_or_else(|| format!("<{name}> is not closed"))?;
        found.push(&rest[body_start..end]);
        rest = &rest[end + close.len()..];
    }
    Ok(found)
}

/// The text of the first `<name>` element in `xml`, unescaped.
pub(crate) fn text(xml: &str, name: &str) -> Result<Option<String>, String> {
    match elements(xml, name)?.first() {
        Some(raw) => Ok(Some(unescape(raw)?)),
        None => Ok(None),
    }
}
