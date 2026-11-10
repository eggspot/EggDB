use crate::json_out::escaped;
use eggdb_sql::json::{Json, Node};

const MOST_DEPTH: usize = 32;
const MOST_OUTPUT_BYTES: usize = 16 << 20;

#[derive(Debug)]
enum Part {
    Text(String),
    Variable { name: String, raw: bool },
    Section {
        name: String,
        inverted: bool,
        children: Vec<Part>,
    },
}

fn parse(template: &str) -> Result<Vec<Part>, String> {
    let mut stack: Vec<(String, bool, Vec<Part>)> = vec![(String::new(), false, Vec::new())];
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        if open > 0
            && let Some((_, _, parts)) = stack.last_mut()
        {
            parts.push(Part::Text(rest[..open].to_owned()));
        }
        let after = &rest[open + 2..];
        let (raw, inner, consumed) = if let Some(tripled) = after.strip_prefix('{') {
            let end = tripled
                .find("}}}")
                .ok_or_else(|| "unclosed {{{ tag in the template".to_owned())?;
            (true, &tripled[..end], end + 4)
        } else {
            let end = after
                .find("}}")
                .ok_or_else(|| "unclosed {{ tag in the template".to_owned())?;
            (false, &after[..end], end + 2)
        };
        rest = &after[consumed..];
        let inner = inner.trim();
        if let Some(name) = inner.strip_prefix('#').or_else(|| inner.strip_prefix('^')) {
            if stack.len() > MOST_DEPTH {
                return Err("the template nests too deeply".to_owned());
            }
            stack.push((name.trim().to_owned(), inner.starts_with('^'), Vec::new()));
        } else if let Some(name) = inner.strip_prefix('/') {
            let (opened, inverted, children) = stack
                .pop()
                .filter(|_| !stack.is_empty())
                .ok_or_else(|| format!("{{{{/{name}}}}} closes no section"))?;
            if opened != name.trim() {
                return Err(format!(
                    "{{{{/{}}}}} closes the section {{{{#{opened}}}}}",
                    name.trim()
                ));
            }
            if let Some((_, _, parts)) = stack.last_mut() {
                parts.push(Part::Section {
                    name: opened,
                    inverted,
                    children,
                });
            }
        } else if inner.starts_with('!') {
        } else if let Some(name) = inner.strip_prefix('&') {
            if let Some((_, _, parts)) = stack.last_mut() {
                parts.push(Part::Variable {
                    name: name.trim().to_owned(),
                    raw: true,
                });
            }
        } else if let Some((_, _, parts)) = stack.last_mut() {
            parts.push(Part::Variable {
                name: inner.to_owned(),
                raw,
            });
        }
    }
    if !rest.is_empty()
        && let Some((_, _, parts)) = stack.last_mut()
    {
        parts.push(Part::Text(rest.to_owned()));
    }
    if stack.len() != 1 {
        let open = stack.last().map(|(name, _, _)| name.clone()).unwrap_or_default();
        return Err(format!("the section {{{{#{open}}}}} is never closed"));
    }
    Ok(stack.pop().map(|(_, _, parts)| parts).unwrap_or_default())
}

fn member<'n>(value: &'n Json, name: &str) -> Option<&'n Json> {
    match value {
        Json::Object(members) => members
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, node)| &node.value),
        Json::Array(items) => name
            .parse::<usize>()
            .ok()
            .and_then(|index| items.get(index))
            .map(|node| &node.value),
        _ => None,
    }
}

fn lookup<'n>(stack: &[&'n Json], name: &str) -> Option<&'n Json> {
    if name == "." {
        return stack.last().copied();
    }
    let mut segments = name.split('.');
    let first = segments.next()?;
    let mut found = stack.iter().rev().find_map(|scope| member(scope, first))?;
    for segment in segments {
        found = member(found, segment)?;
    }
    Some(found)
}

fn json_text(value: &Json, out: &mut String) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Boolean(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Json::Number(text) => out.push_str(text),
        Json::String(text) => out.push_str(&escaped(text)),
        Json::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                json_text(&item.value, out);
            }
            out.push(']');
        }
        Json::Object(members) => {
            out.push('{');
            for (index, (key, item)) in members.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&escaped(key));
                out.push(':');
                json_text(&item.value, out);
            }
            out.push('}');
        }
    }
}

fn plain_text(value: &Json) -> String {
    match value {
        Json::Null => String::new(),
        Json::String(text) => text.clone(),
        other => {
            let mut out = String::new();
            json_text(other, &mut out);
            out
        }
    }
}

fn escaped_inside_string(text: &str) -> String {
    let quoted = escaped(text);
    quoted[1..quoted.len() - 1].to_owned()
}

fn truthy(value: Option<&Json>) -> bool {
    match value {
        None | Some(Json::Null) | Some(Json::Boolean(false)) => false,
        Some(Json::Array(items)) => !items.is_empty(),
        Some(Json::String(text)) => !text.is_empty(),
        Some(_) => true,
    }
}

fn inner_name(children: &[Part]) -> String {
    children
        .iter()
        .map(|part| match part {
            Part::Text(text) => text.trim().to_owned(),
            Part::Variable { name, .. } => name.clone(),
            Part::Section { name, .. } => name.clone(),
        })
        .collect::<String>()
}

fn render_parts<'n>(parts: &[Part], stack: &mut Vec<&'n Json>, out: &mut String) -> Result<(), String> {
    for part in parts {
        if out.len() > MOST_OUTPUT_BYTES {
            return Err("the rendered template is too large".to_owned());
        }
        match part {
            Part::Text(text) => out.push_str(text),
            Part::Variable { name, raw } => {
                if let Some(value) = lookup(stack, name) {
                    let text = plain_text(value);
                    if *raw || !matches!(value, Json::String(_)) {
                        out.push_str(&text);
                    } else {
                        out.push_str(&escaped_inside_string(&text));
                    }
                }
            }
            Part::Section {
                name,
                inverted: false,
                children,
            } if name == "toJson" => {
                if let Some(value) = lookup(stack, &inner_name(children)) {
                    json_text(value, out);
                }
            }
            Part::Section {
                name,
                inverted: false,
                children,
            } if name == "join" => {
                if let Some(Json::Array(items)) = lookup(stack, &inner_name(children)) {
                    let joined: Vec<String> = items.iter().map(|item| plain_text(&item.value)).collect();
                    out.push_str(&joined.join(","));
                }
            }
            Part::Section {
                name,
                inverted,
                children,
            } => {
                let value = lookup(stack, name);
                if *inverted {
                    if !truthy(value) {
                        render_parts(children, stack, out)?;
                    }
                    continue;
                }
                match value {
                    Some(Json::Array(items)) => {
                        for item in items {
                            stack.push(&item.value);
                            let rendered = render_parts(children, stack, out);
                            stack.pop();
                            rendered?;
                        }
                    }
                    found if truthy(found) => {
                        if let Some(found) = found {
                            stack.push(found);
                            let rendered = render_parts(children, stack, out);
                            stack.pop();
                            rendered?;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// Renders a search template as Elasticsearch's Mustache does: `{{name}}` (JSON-escaped inside
/// strings), `{{{name}}}` and `{{&name}}` raw, sections, inverted sections, dotted names,
/// `{{#toJson}}name{{/toJson}}` and `{{#join}}name{{/join}}`.
pub(crate) fn render(template: &str, params: &Node) -> Result<String, String> {
    let parts = parse(template)?;
    let mut out = String::with_capacity(template.len());
    let mut stack = vec![&params.value];
    render_parts(&parts, &mut stack, &mut out)?;
    Ok(out)
}

pub(crate) fn json_of(node: &Node) -> String {
    let mut out = String::new();
    json_text(&node.value, &mut out);
    out
}
