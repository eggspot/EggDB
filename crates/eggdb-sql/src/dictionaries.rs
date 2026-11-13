//! Full FIX and UN/EDIFACT dictionaries, loaded from the standards' own machine-readable
//! publications rather than shipped (D1069): a FIX data dictionary in the XML layout FIX engines
//! exchange (`<fix>` with `<fields>`, `<messages>`, `<components>` and `<header>`), and the
//! UN/EDIFACT segment directory (EDSD) and code lists (UNCL) as UNECE publishes them as text.
//! Each is turned into JSON, which the `eggdb_dictionaries` extension keeps per version or
//! directory and uses to describe messages: names for every tag, segment, element and code, and
//! FIX repeating groups nested by the message's layout.

use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{Json, Node, parse_document};
use crate::types::{SqlType, Value};
use crate::xml_path::Document;

pub(crate) const FUNCTIONS: [&str; 4] = ["fix_dictionary", "fix_annotate", "edifact_directory", "edifact_annotate"];

const MOST_LAYOUT_DEPTH: usize = 32;
const MOST_INPUT_BYTES: usize = 64 << 20;
const SOH: char = '\u{1}';

/// The extension that keeps loaded dictionaries and describes messages with them.
pub(crate) const DICTIONARIES_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_dictionaries;
CREATE TABLE eggdb_dictionaries.fix_dictionaries (version text PRIMARY KEY, dictionary jsonb NOT NULL, loaded_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE eggdb_dictionaries.edifact_directories (directory text PRIMARY KEY, segments jsonb NOT NULL, codes jsonb NOT NULL DEFAULT '[]'::jsonb, loaded_at timestamptz NOT NULL DEFAULT now());
CREATE FUNCTION eggdb_dictionaries.load_fix(dictionary_xml text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    parsed jsonb := fix_dictionary(dictionary_xml);
BEGIN
    INSERT INTO eggdb_dictionaries.fix_dictionaries (version, dictionary) VALUES (parsed ->> 'version', parsed)
        ON CONFLICT (version) DO UPDATE SET dictionary = EXCLUDED.dictionary, loaded_at = now();
    RETURN parsed ->> 'version';
END
$$;
CREATE FUNCTION eggdb_dictionaries.load_edifact(directory_name text, segment_directory text, code_lists text DEFAULT NULL) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    segments jsonb := edifact_directory('segments', segment_directory);
    codes jsonb := CASE WHEN code_lists IS NULL THEN '[]'::jsonb ELSE edifact_directory('codes', code_lists) END;
BEGIN
    INSERT INTO eggdb_dictionaries.edifact_directories (directory, segments, codes) VALUES (upper(directory_name), segments, codes)
        ON CONFLICT (directory) DO UPDATE SET segments = EXCLUDED.segments, codes = EXCLUDED.codes, loaded_at = now();
    RETURN jsonb_array_length(segments);
END
$$;
CREATE FUNCTION eggdb_dictionaries.fix_describe(message text, dictionary_version text DEFAULT NULL) RETURNS jsonb LANGUAGE plpgsql AS $$
DECLARE
    wanted text := coalesce(dictionary_version, fix_field(message, 8));
    found jsonb;
BEGIN
    SELECT d.dictionary INTO found FROM eggdb_dictionaries.fix_dictionaries d WHERE d.version = wanted;
    IF found IS NULL THEN
        RAISE EXCEPTION 'no FIX dictionary % is loaded', wanted USING ERRCODE = '42704';
    END IF;
    RETURN fix_annotate(message, found);
END
$$;
CREATE FUNCTION eggdb_dictionaries.edifact_describe(message text, directory_name text) RETURNS jsonb LANGUAGE plpgsql AS $$
DECLARE
    found record;
BEGIN
    SELECT d.segments, d.codes INTO found FROM eggdb_dictionaries.edifact_directories d WHERE d.directory = upper(directory_name);
    IF NOT FOUND THEN
        RAISE EXCEPTION 'no EDIFACT directory % is loaded', directory_name USING ERRCODE = '42704';
    END IF;
    RETURN edifact_annotate(message, found.segments, found.codes);
END
$$;
"#;

fn invalid(what: &str, message: impl std::fmt::Display) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, format!("the {what} cannot be read: {message}"))
}

fn quoted(text: &str) -> String {
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

fn jsonb(text: String) -> SqlResult<Value> {
    parse_document(&text).map_err(|error| invalid("result", error.message))?;
    Ok(Value::Jsonb(text))
}

struct FixDictionary<'d> {
    document: &'d Document,
    components: Vec<(String, usize)>,
    tags: Vec<(String, u32)>,
}

impl FixDictionary<'_> {
    fn layout(&self, parent: usize, depth: usize) -> SqlResult<Vec<String>> {
        if depth > MOST_LAYOUT_DEPTH {
            return Err(invalid("FIX dictionary", "its components nest too deeply"));
        }
        let mut out = Vec::new();
        for child in self.document.child_elements(parent) {
            let name = self.document.attribute(child, "name").unwrap_or_default();
            let required = self.document.attribute(child, "required").is_some_and(|flag| flag.eq_ignore_ascii_case("y"));
            let tag = self.tags.iter().find(|(known, _)| known == name).map_or(0, |(_, tag)| *tag);
            match self.document.element_name(child) {
                Some("field") => out.push(format!("{{\"field\":{},\"tag\":{tag},\"required\":{required}}}", quoted(name))),
                Some("group") => out.push(format!(
                    "{{\"group\":{},\"tag\":{tag},\"required\":{required},\"layout\":[{}]}}",
                    quoted(name),
                    self.layout(child, depth + 1)?.join(",")
                )),
                Some("component") => {
                    let Some((_, definition)) = self.components.iter().find(|(known, _)| known == name) else {
                        return Err(invalid("FIX dictionary", format!("the component {name} is not defined")));
                    };
                    out.extend(self.layout(*definition, depth + 1)?);
                }
                _ => {}
            }
        }
        Ok(out)
    }
}

/// A FIX data dictionary's XML as JSON: its version, every field with its tag, name, type and
/// enumerated values, and every message's layout (header, body and trailer, components inlined,
/// repeating groups nested).
fn fix_dictionary(xml: &str) -> SqlResult<String> {
    let document = Document::parse(xml, true)?;
    let root = document.child_elements(0).into_iter().next().ok_or_else(|| invalid("FIX dictionary", "it has no root element"))?;
    if document.element_name(root) != Some("fix") {
        return Err(invalid("FIX dictionary", "its root element is not <fix>"));
    }
    let numbered = |name: &str| document.attribute(root, name).unwrap_or_default().to_owned();
    let kind = document.attribute(root, "type").unwrap_or("FIX");
    let service_pack = document.attribute(root, "servicepack").filter(|pack| *pack != "0").map(|pack| format!("SP{pack}")).unwrap_or_default();
    let version = format!("{kind}.{}.{}{service_pack}", numbered("major"), numbered("minor"));
    let section = |name: &str| document.child_elements(root).into_iter().find(|child| document.element_name(*child) == Some(name));
    let mut fields = Vec::new();
    let mut tags = Vec::new();
    for field in section("fields").map(|fields| document.child_elements(fields)).unwrap_or_default() {
        let name = document.attribute(field, "name").unwrap_or_default();
        let tag: u32 = document
            .attribute(field, "number")
            .and_then(|number| number.parse().ok())
            .ok_or_else(|| invalid("FIX dictionary", format!("the field {name} has no number")))?;
        tags.push((name.to_owned(), tag));
        let values: Vec<String> = document
            .child_elements(field)
            .into_iter()
            .filter(|value| document.element_name(*value) == Some("value"))
            .map(|value| {
                format!(
                    "{{\"value\":{},\"description\":{}}}",
                    quoted(document.attribute(value, "enum").unwrap_or_default()),
                    quoted(document.attribute(value, "description").unwrap_or_default())
                )
            })
            .collect();
        fields.push(format!(
            "{{\"tag\":{tag},\"name\":{},\"type\":{},\"values\":[{}]}}",
            quoted(name),
            quoted(document.attribute(field, "type").unwrap_or_default()),
            values.join(",")
        ));
    }
    let components = section("components")
        .map(|components| document.child_elements(components))
        .unwrap_or_default()
        .into_iter()
        .map(|component| (document.attribute(component, "name").unwrap_or_default().to_owned(), component))
        .collect();
    let dictionary = FixDictionary { document: &document, components, tags };
    let header = section("header").map(|header| dictionary.layout(header, 0)).transpose()?.unwrap_or_default();
    let trailer = section("trailer").map(|trailer| dictionary.layout(trailer, 0)).transpose()?.unwrap_or_default();
    let mut messages = Vec::new();
    for message in section("messages").map(|messages| document.child_elements(messages)).unwrap_or_default() {
        messages.push(format!(
            "{{\"name\":{},\"type\":{},\"category\":{},\"layout\":[{}]}}",
            quoted(document.attribute(message, "name").unwrap_or_default()),
            quoted(document.attribute(message, "msgtype").unwrap_or_default()),
            quoted(document.attribute(message, "msgcat").unwrap_or_default()),
            dictionary.layout(message, 0)?.join(",")
        ));
    }
    Ok(format!(
        "{{\"version\":{},\"header\":[{}],\"trailer\":[{}],\"fields\":[{}],\"messages\":[{}]}}",
        quoted(&version),
        header.join(","),
        trailer.join(","),
        fields.join(","),
        messages.join(",")
    ))
}

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, value)| value),
        _ => None,
    }
}

fn text_of<'n>(node: &'n Node, name: &str) -> Option<&'n str> {
    match &member(node, name)?.value {
        Json::String(text) => Some(text),
        _ => None,
    }
}

fn number_of(node: &Node, name: &str) -> Option<u32> {
    match &member(node, name)?.value {
        Json::Number(text) => text.parse().ok(),
        _ => None,
    }
}

fn items(node: Option<&Node>) -> &[Node] {
    match node.map(|node| &node.value) {
        Some(Json::Array(items)) => items,
        _ => &[],
    }
}

/// One FIX layout entry: a field, or a repeating group with its own layout.
enum Entry<'n> {
    Field(u32),
    Group(u32, &'n [Node]),
}

fn entry(node: &Node) -> Option<Entry<'_>> {
    let tag = number_of(node, "tag")?;
    match member(node, "layout") {
        Some(layout) if member(node, "group").is_some() => Some(Entry::Group(tag, items(Some(layout)))),
        _ => Some(Entry::Field(tag)),
    }
}

struct FixAnnotator<'d> {
    fields: Vec<(u32, &'d Node)>,
}

impl FixAnnotator<'_> {
    fn field(&self, tag: u32, value: &str) -> String {
        let definition = self.fields.iter().find(|(known, _)| *known == tag).map(|(_, node)| *node);
        let name = definition.and_then(|node| text_of(node, "name"));
        let description = definition.and_then(|node| {
            items(member(node, "values")).iter().find(|candidate| text_of(candidate, "value") == Some(value)).and_then(|candidate| text_of(candidate, "description"))
        });
        let mut out = format!("{{\"tag\":{tag},\"value\":{}", quoted(value));
        if let Some(name) = name {
            out.push_str(&format!(",\"name\":{}", quoted(name)));
        }
        if let Some(description) = description {
            out.push_str(&format!(",\"meaning\":{}", quoted(description)));
        }
        out.push('}');
        out
    }

    fn group_tags(layout: &[Node], depth: usize) -> Vec<u32> {
        if depth > MOST_LAYOUT_DEPTH {
            return Vec::new();
        }
        let mut tags = Vec::new();
        for node in layout {
            match entry(node) {
                Some(Entry::Field(tag)) => tags.push(tag),
                Some(Entry::Group(tag, inner)) => {
                    tags.push(tag);
                    tags.extend(Self::group_tags(inner, depth + 1));
                }
                None => {}
            }
        }
        tags
    }

    fn groups_of(layout: &[Node]) -> Vec<(u32, &[Node])> {
        layout
            .iter()
            .filter_map(entry)
            .filter_map(|entry| match entry {
                Entry::Group(tag, inner) => Some((tag, inner)),
                Entry::Field(_) => None,
            })
            .collect()
    }

    fn walk(&self, fields: &[(u32, &str)], at: &mut usize, layout: &[Node], out: &mut Vec<String>) {
        let groups = Self::groups_of(layout);
        while let Some(&(tag, value)) = fields.get(*at) {
            *at += 1;
            match groups.iter().find(|(group, _)| *group == tag) {
                Some((_, inner)) => out.push(self.group(fields, at, (tag, value), inner, 1)),
                None => out.push(self.field(tag, value)),
            }
        }
    }

    fn group(&self, fields: &[(u32, &str)], at: &mut usize, counter: (u32, &str), inner: &[Node], depth: usize) -> String {
        let delimiter = inner.iter().find_map(|node| {
            entry(node).map(|entry| match entry {
                Entry::Field(tag) | Entry::Group(tag, _) => tag,
            })
        });
        let nested_groups = Self::groups_of(inner);
        let known = Self::group_tags(inner, depth);
        let count: usize = counter.1.parse().unwrap_or(0);
        let mut instances = Vec::new();
        for _ in 0..count {
            if fields.get(*at).map(|(tag, _)| *tag) != delimiter {
                break;
            }
            let mut instance = Vec::new();
            let mut first = true;
            while let Some(&(tag, value)) = fields.get(*at) {
                if !first && (Some(tag) == delimiter || !known.contains(&tag)) {
                    break;
                }
                first = false;
                *at += 1;
                match nested_groups.iter().find(|(group, _)| *group == tag).filter(|_| depth < MOST_LAYOUT_DEPTH) {
                    Some((_, nested)) => instance.push(self.group(fields, at, (tag, value), nested, depth + 1)),
                    None => instance.push(self.field(tag, value)),
                }
            }
            instances.push(format!("[{}]", instance.join(",")));
        }
        let mut described = self.field(counter.0, counter.1);
        described.pop();
        described.push_str(&format!(",\"instances\":[{}]}}", instances.join(",")));
        described
    }
}

fn fix_message_fields(message: &str) -> SqlResult<Vec<(u32, &str)>> {
    let delimiter = if message.contains(SOH) { SOH } else { '|' };
    let mut fields = Vec::new();
    for part in message.trim_end_matches(['\r', '\n']).split(delimiter).filter(|part| !part.is_empty()) {
        let (tag, value) = part.split_once('=').ok_or_else(|| invalid("FIX message", format!("the field {part:?} has no =")))?;
        let tag = tag.parse().map_err(|_| invalid("FIX message", format!("the tag {tag:?} is not a number")))?;
        fields.push((tag, value));
    }
    Ok(fields)
}

/// A FIX message's fields with the dictionary's names and value meanings, repeating groups
/// nested as `instances` by the layout of the message type in tag 35.
fn fix_annotate(message: &str, dictionary: &str) -> SqlResult<String> {
    let dictionary = parse_document(dictionary).map_err(|error| invalid("FIX dictionary", error.message))?;
    let fields = fix_message_fields(message)?;
    let annotator = FixAnnotator {
        fields: items(member(&dictionary, "fields")).iter().filter_map(|node| Some((number_of(node, "tag")?, node))).collect(),
    };
    let message_type = fields.iter().find(|(tag, _)| *tag == 35).map(|(_, value)| *value);
    let definition = items(member(&dictionary, "messages")).iter().find(|node| text_of(node, "type") == message_type);
    let mut layout: Vec<Node> = items(member(&dictionary, "header")).to_vec();
    layout.extend(definition.map(|node| items(member(node, "layout")).to_vec()).unwrap_or_default());
    layout.extend(items(member(&dictionary, "trailer")).iter().cloned());
    let mut out = Vec::new();
    let mut at = 0;
    annotator.walk(&fields, &mut at, &layout, &mut out);
    let name = definition.and_then(|node| text_of(node, "name")).map_or_else(|| "null".to_owned(), quoted);
    Ok(format!("{{\"message\":{name},\"fields\":[{}]}}", out.join(",")))
}

fn spaced_columns(line: &str) -> Vec<&str> {
    line.split("  ").map(str::trim).filter(|part| !part.is_empty()).collect()
}

fn without_change_mark(line: &str) -> &str {
    line.strip_prefix(['+', '*', '#', '|', 'X']).unwrap_or(line).trim_start()
}

fn indent_of(line: &str) -> usize {
    let unmarked = line.strip_prefix(['+', '*', '#', '|', 'X']).map_or(line, |rest| rest);
    line.len() - unmarked.trim_start().len()
}

fn name_after<'c>(columns: &[&'c str], code: &str) -> &'c str {
    match columns {
        [first, second, ..] if *first == code => second,
        [first, ..] => first.strip_prefix(code).unwrap_or(first).trim(),
        [] => "",
    }
}

fn is_segment_tag(word: &str) -> bool {
    word.len() == 3 && word.bytes().all(|byte| byte.is_ascii_uppercase())
}

fn is_element_code(word: &str) -> bool {
    let bytes = word.as_bytes();
    bytes.len() == 4 && (bytes[0].is_ascii_digit() || bytes[0].is_ascii_uppercase()) && bytes[1..].iter().all(u8::is_ascii_digit)
}

/// UNECE's segment directory (EDSD) text as JSON: each segment's tag, name and elements in
/// position order, each simple or composite with its status (`M`/`C`), repeat count and, for a
/// composite, its components with their formats.
fn edifact_segments(text: &str) -> String {
    let mut segments: Vec<(String, String, Vec<String>)> = Vec::new();
    let mut element: Option<(String, Vec<String>)> = None;
    let flush = |segments: &mut Vec<(String, String, Vec<String>)>, element: &mut Option<(String, Vec<String>)>| {
        if let (Some((head, components)), Some(segment)) = (element.take(), segments.last_mut()) {
            segment.2.push(format!("{head},\"components\":[{}]}}", components.join(",")));
        }
    };
    for line in text.lines() {
        let trimmed = without_change_mark(line);
        let words: Vec<&str> = trimmed.split_whitespace().collect();
        let Some(first) = words.first() else { continue };
        if is_segment_tag(first) && indent_of(line) >= 3 && words.len() >= 2 {
            flush(&mut segments, &mut element);
            segments.push(((*first).to_owned(), words[1..].join(" "), Vec::new()));
            continue;
        }
        let columns = spaced_columns(trimmed);
        if first.len() == 3 && first.bytes().all(|byte| byte.is_ascii_digit()) && words.len() >= 3 && is_element_code(words[1]) {
            flush(&mut segments, &mut element);
            let status_at = columns.iter().skip(2).position(|part| matches!(*part, "M" | "C")).map(|found| found + 2);
            let flag = status_at.map_or("C", |found| columns[found]);
            let repeat = status_at.and_then(|found| columns.get(found + 1)).and_then(|count| count.parse::<u32>().ok()).unwrap_or(1);
            let name = name_after(&columns[1..], words[1]);
            element = Some((
                format!("{{\"position\":{},\"code\":{},\"name\":{},\"status\":{},\"repeat\":{repeat}", quoted(first), quoted(words[1]), quoted(name), quoted(flag)),
                Vec::new(),
            ));
            continue;
        }
        if is_element_code(first) && words.len() >= 3
            && let Some((_, components)) = element.as_mut()
        {
            let format = words.last().copied().unwrap_or_default();
            let status = words.iter().rev().nth(1).copied().unwrap_or("C");
            let name = name_after(&columns, first);
            components.push(format!("{{\"code\":{},\"name\":{},\"status\":{},\"format\":{}}}", quoted(first), quoted(name), quoted(status), quoted(format)));
        }
    }
    flush(&mut segments, &mut element);
    let list: Vec<String> = segments.into_iter().map(|(tag, name, elements)| format!("{{\"tag\":{},\"name\":{},\"elements\":[{}]}}", quoted(&tag), quoted(&name), elements.join(","))).collect();
    format!("[{}]", list.join(","))
}

/// UNECE's code lists (UNCL) text as JSON: each coded data element's number and name, with
/// every code and its name.
fn edifact_codes(text: &str) -> String {
    let mut elements: Vec<(String, String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        let trimmed = without_change_mark(line);
        let words: Vec<&str> = trimmed.split_whitespace().collect();
        let Some(first) = words.first() else { continue };
        let indent = indent_of(line);
        if first.len() == 4 && first.bytes().all(|byte| byte.is_ascii_digit()) && indent < 8 && words.len() >= 2 && line.trim_end().ends_with(']') {
            let name = words[1..].iter().take_while(|word| !word.starts_with('[')).copied().collect::<Vec<_>>().join(" ");
            elements.push(((*first).to_owned(), name, Vec::new()));
            continue;
        }
        if indent < 8
            && words.len() >= 2
            && !first.ends_with(':')
            && first.len() <= 17
            && first.bytes().all(|byte| byte.is_ascii_alphanumeric())
            && let Some(element) = elements.last_mut()
        {
            element.2.push(format!("{{\"code\":{},\"name\":{}}}", quoted(first), quoted(&words[1..].join(" "))));
        }
    }
    let list: Vec<String> = elements.into_iter().map(|(code, name, codes)| format!("{{\"element\":{},\"name\":{},\"codes\":[{}]}}", quoted(&code), quoted(&name), codes.join(","))).collect();
    format!("[{}]", list.join(","))
}

fn edifact_values(element: &Node) -> Vec<String> {
    match &element.value {
        Json::String(text) => vec![text.clone()],
        Json::Array(components) => components.iter().map(|component| match &component.value {
            Json::String(text) => text.clone(),
            _ => String::new(),
        }).collect(),
        _ => Vec::new(),
    }
}

fn code_meaning<'n>(codes: &'n Node, element: &str, value: &str) -> Option<&'n str> {
    let list = items(Some(codes)).iter().find(|node| text_of(node, "element") == Some(element))?;
    items(member(list, "codes")).iter().find(|code| text_of(code, "code") == Some(value)).and_then(|code| text_of(code, "name"))
}

fn described_value(code: &str, name: &str, value: &str, codes: &Node) -> String {
    let mut out = format!("{{\"code\":{},\"name\":{},\"value\":{}", quoted(code), quoted(name), quoted(value));
    if let Some(meaning) = code_meaning(codes, code, value) {
        out.push_str(&format!(",\"meaning\":{}", quoted(meaning)));
    }
    out.push('}');
    out
}

/// An EDIFACT message (as `edifact_parse` reads it) with every segment, element, component and
/// code named from a loaded segment directory and code lists.
fn edifact_annotate(parsed: &str, segments: &str, codes: &str) -> SqlResult<String> {
    let parsed = parse_document(parsed).map_err(|error| invalid("EDIFACT message", error.message))?;
    let segments = parse_document(segments).map_err(|error| invalid("EDIFACT directory", error.message))?;
    let codes = parse_document(codes).map_err(|error| invalid("EDIFACT code lists", error.message))?;
    let mut out = Vec::new();
    for segment in items(Some(&parsed)) {
        let tag = text_of(segment, "tag").unwrap_or_default();
        let definition = items(Some(&segments)).iter().find(|node| text_of(node, "tag") == Some(tag));
        let layout = definition.map(|node| items(member(node, "elements"))).unwrap_or_default();
        let mut elements = Vec::new();
        for (at, element) in items(member(segment, "elements")).iter().enumerate() {
            let values = edifact_values(element);
            let Some(position) = layout.get(at) else {
                elements.push(format!("{{\"values\":[{}]}}", values.iter().map(|value| quoted(value)).collect::<Vec<_>>().join(",")));
                continue;
            };
            let code = text_of(position, "code").unwrap_or_default();
            let name = text_of(position, "name").unwrap_or_default();
            let components = items(member(position, "components"));
            if components.is_empty() {
                elements.push(described_value(code, name, values.first().map(String::as_str).unwrap_or_default(), &codes));
                continue;
            }
            let described: Vec<String> = values
                .iter()
                .enumerate()
                .filter(|(_, value)| !value.is_empty())
                .map(|(index, value)| match components.get(index) {
                    Some(component) => described_value(text_of(component, "code").unwrap_or_default(), text_of(component, "name").unwrap_or_default(), value, &codes),
                    None => format!("{{\"value\":{}}}", quoted(value)),
                })
                .collect();
            elements.push(format!("{{\"code\":{},\"name\":{},\"components\":[{}]}}", quoted(code), quoted(name), described.join(",")));
        }
        let name = definition.and_then(|node| text_of(node, "name")).map_or_else(|| "null".to_owned(), quoted);
        out.push(format!("{{\"tag\":{},\"name\":{name},\"elements\":[{}]}}", quoted(tag), elements.join(",")));
    }
    Ok(format!("[{}]", out.join(",")))
}

pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    FUNCTIONS.contains(&name).then_some(SqlType::Jsonb)
}

fn text_argument(value: &Value, what: &str) -> SqlResult<String> {
    let text = value.to_text().ok_or_else(|| invalid(what, "it must be text or jsonb"))?;
    if text.len() > MOST_INPUT_BYTES {
        return Err(SqlError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, format!("the {what} is larger than 64 MiB")));
    }
    Ok(text)
}

/// `fix_dictionary(xml)`, `fix_annotate(message, dictionary)`,
/// `edifact_directory('segments' | 'codes', text)` and
/// `edifact_annotate(message, segments, codes)`.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    if values.iter().any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    Some((|| match (name, values) {
        ("fix_dictionary", [xml]) => jsonb(fix_dictionary(&text_argument(xml, "FIX dictionary")?)?),
        ("fix_annotate", [message, dictionary]) => jsonb(fix_annotate(&text_argument(message, "FIX message")?, &text_argument(dictionary, "FIX dictionary")?)?),
        ("edifact_directory", [kind, content]) => {
            let content = text_argument(content, "EDIFACT directory")?;
            match text_argument(kind, "directory kind")?.to_ascii_lowercase().as_str() {
                "segments" | "edsd" => jsonb(edifact_segments(&content)),
                "codes" | "uncl" => jsonb(edifact_codes(&content)),
                other => Err(invalid("directory kind", format!("{other} is neither segments nor codes"))),
            }
        }
        ("edifact_annotate", [message, segments, codes]) => {
            let message = text_argument(message, "EDIFACT message")?;
            let parsed = match crate::standards::function("edifact_parse", &[Value::Text(message)]) {
                Some(Ok(Value::Jsonb(parsed))) => parsed,
                Some(Err(error)) => return Err(error),
                _ => return Err(invalid("EDIFACT message", "it could not be parsed")),
            };
            jsonb(edifact_annotate(&parsed, &text_argument(segments, "EDIFACT directory")?, &text_argument(codes, "EDIFACT code lists")?)?)
        }
        _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, format!("{name} does not take {} arguments", values.len()))),
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DICTIONARY: &str = r#"<fix type="FIX" major="4" minor="4" servicepack="0">
      <header><field name="BeginString" required="Y"/><field name="MsgType" required="Y"/></header>
      <trailer><field name="CheckSum" required="Y"/></trailer>
      <messages>
        <message name="NewOrderSingle" msgtype="D" msgcat="app">
          <field name="ClOrdID" required="Y"/>
          <component name="Parties" required="N"/>
          <field name="Side" required="Y"/>
        </message>
      </messages>
      <components>
        <component name="Parties">
          <group name="NoPartyIDs" required="N"><field name="PartyID" required="N"/><field name="PartyRole" required="N"/></group>
        </component>
      </components>
      <fields>
        <field number="8" name="BeginString" type="STRING"/>
        <field number="10" name="CheckSum" type="STRING"/>
        <field number="11" name="ClOrdID" type="STRING"/>
        <field number="35" name="MsgType" type="STRING"><value enum="D" description="ORDER_SINGLE"/></field>
        <field number="54" name="Side" type="CHAR"><value enum="1" description="BUY"/><value enum="2" description="SELL"/></field>
        <field number="448" name="PartyID" type="STRING"/>
        <field number="452" name="PartyRole" type="INT"><value enum="1" description="EXECUTING_FIRM"/></field>
        <field number="453" name="NoPartyIDs" type="NUMINGROUP"/>
      </fields>
    </fix>"#;

    #[test]
    fn fix_dictionaries_name_fields_and_nest_repeating_groups() {
        let dictionary = fix_dictionary(DICTIONARY).unwrap();
        assert!(dictionary.starts_with("{\"version\":\"FIX.4.4\""), "{dictionary}");
        assert!(dictionary.contains("{\"group\":\"NoPartyIDs\",\"tag\":453,\"required\":false,\"layout\":[{\"field\":\"PartyID\",\"tag\":448"), "{dictionary}");
        let described = fix_annotate("8=FIX.4.4|35=D|11=A1|453=2|448=EGG|452=1|448=HEN|54=2|10=000|", &dictionary).unwrap();
        assert!(described.starts_with("{\"message\":\"NewOrderSingle\""), "{described}");
        assert!(described.contains("{\"tag\":35,\"value\":\"D\",\"name\":\"MsgType\",\"meaning\":\"ORDER_SINGLE\"}"), "{described}");
        assert!(described.contains("\"instances\":[[{\"tag\":448,\"value\":\"EGG\",\"name\":\"PartyID\"},{\"tag\":452,\"value\":\"1\",\"name\":\"PartyRole\",\"meaning\":\"EXECUTING_FIRM\"}],[{\"tag\":448,\"value\":\"HEN\",\"name\":\"PartyID\"}]]"), "{described}");
        assert!(described.contains("{\"tag\":54,\"value\":\"2\",\"name\":\"Side\",\"meaning\":\"SELL\"}"), "{described}");
        assert!(fix_dictionary("<fix><messages><message name=\"X\"><component name=\"Missing\"/></message></messages></fix>").is_err());
    }

    #[test]
    fn edifact_directories_are_read_from_unece_text() {
        let segments = edifact_segments(
            "       BGM  BEGINNING OF MESSAGE\n\n       Function: To indicate the type and function of a message.\n\n010    C002 DOCUMENT/MESSAGE NAME                      C    1\n       1001  Document name code                        C      an..3\n       1131  Code list identification code             C      an..17\n\n020    1225 MESSAGE FUNCTION CODE                      C    1      an..3\n",
        );
        assert!(segments.contains("\"tag\":\"BGM\",\"name\":\"BEGINNING OF MESSAGE\""), "{segments}");
        assert!(segments.contains("{\"code\":\"1001\",\"name\":\"Document name code\",\"status\":\"C\",\"format\":\"an..3\"}"), "{segments}");
        assert!(segments.contains("\"position\":\"020\",\"code\":\"1225\""), "{segments}");
        let codes = edifact_codes("     1001  Document name code                                      [C]\n\n     Desc: Code specifying the document name.\n\n     Repr: an..3\n\n     220   Order\n              Document by means of which a buyer initiates a transaction.\n     380   Commercial invoice\n");
        assert!(codes.contains("{\"code\":\"220\",\"name\":\"Order\"}"), "{codes}");
        assert!(!codes.contains("Desc"), "{codes}");
        let parsed = r#"[{"tag":"BGM","elements":[["220","","9"],"9"]}]"#;
        let described = edifact_annotate(parsed, &segments, &codes).unwrap();
        assert!(described.contains("{\"code\":\"1001\",\"name\":\"Document name code\",\"value\":\"220\",\"meaning\":\"Order\"}"), "{described}");
        assert!(described.contains("{\"code\":\"1225\",\"name\":\"MESSAGE FUNCTION CODE\",\"value\":\"9\"}"), "{described}");
    }

    #[test]
    fn the_extension_keeps_dictionaries_and_describes_messages() {
        use crate::memory_store::MemoryStore;
        use crate::tests::render;
        use crate::Session;
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        let mut run = |sql: &str| {
            let mut outputs = Vec::new();
            session.execute(sql, &mut |result| outputs.push(render(&result))).unwrap_or_else(|error| panic!("{sql}: {error}"));
            outputs.pop().unwrap_or_default()
        };
        run("CREATE EXTENSION eggdb_dictionaries");
        assert!(run(&format!("SELECT eggdb_dictionaries.load_fix('{}')", DICTIONARY.replace('\'', "''"))).contains("FIX.4.4"));
        assert!(run("SELECT eggdb_dictionaries.fix_describe('8=FIX.4.4|35=D|11=A1|54=1|10=000|') ->> 'message'").contains("NewOrderSingle"));
    }
}
