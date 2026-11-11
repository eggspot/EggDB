use super::{Answer, Api, error, index_glob, json, query_flag, read, write};
use crate::json_out::{Out, written};
use eggdb_http::{Request, Response};
use eggdb_keyspace::KeysRead;
use eggdb_sql::json::{Json, Node, parse_document};

const INDEX_TEMPLATE_RECORD: u8 = 5;
const COMPONENT_TEMPLATE_RECORD: u8 = 6;
const LEGACY_TEMPLATE_RECORD: u8 = 7;
const MAX_TEMPLATE_BYTES: usize = 256 * 1024;
const MAX_TEMPLATES: usize = 1000;
const MAX_TEMPLATE_NAME_BYTES: usize = 255;

/// An index template (`_index_template`), applied to the indices its patterns match, or a
/// component template (`_component_template`) that index templates are composed of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TemplateKind {
    Index,
    Component,
    Legacy,
}

impl TemplateKind {
    fn record(self) -> u8 {
        match self {
            Self::Index => INDEX_TEMPLATE_RECORD,
            Self::Component => COMPONENT_TEMPLATE_RECORD,
            Self::Legacy => LEGACY_TEMPLATE_RECORD,
        }
    }

    fn listed(self) -> &'static str {
        match self {
            Self::Index => "index_templates",
            Self::Component => "component_templates",
            Self::Legacy => "templates",
        }
    }

    fn member(self) -> &'static str {
        match self {
            Self::Index => "index_template",
            Self::Component => "component_template",
            Self::Legacy => "template",
        }
    }

    fn missing(self, name: &str) -> Response {
        let what = match self {
            Self::Index | Self::Legacy => "index template",
            Self::Component => "component template",
        };
        error(404, "resource_not_found_exception", &format!("{what} matching [{name}] not found"))
    }
}

fn template_key(kind: TemplateKind, name: &str) -> Vec<u8> {
    let mut key = vec![kind.record()];
    key.extend_from_slice(name.as_bytes());
    key
}

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, value)| value),
        _ => None,
    }
}

fn plain(value: Json) -> Node {
    Node {
        value,
        start: 0,
        end: 0,
    }
}

fn strings_of(node: &Node) -> Option<Vec<String>> {
    match &node.value {
        Json::String(text) => Some(vec![text.clone()]),
        Json::Array(items) => items
            .iter()
            .map(|item| match &item.value {
                Json::String(text) => Some(text.clone()),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

fn refused(reason: &str) -> Response {
    error(400, "x_content_parse_exception", reason)
}

fn validated(kind: TemplateKind, body: &[u8]) -> Result<String, Response> {
    if body.len() > MAX_TEMPLATE_BYTES {
        return Err(refused("the template is too large"));
    }
    let text = String::from_utf8_lossy(body);
    let parsed = parse_document(&text).map_err(|_| refused("the template is not JSON"))?;
    let Json::Object(members) = &parsed.value else {
        return Err(refused("the template is not an object"));
    };
    for (key, value) in members {
        match (kind, key.as_str()) {
            (TemplateKind::Legacy, "index_patterns" | "template") => {
                if strings_of(value).is_none_or(|patterns| patterns.is_empty()) {
                    return Err(refused("[index_patterns] must be a string or an array of strings"));
                }
            }
            (TemplateKind::Legacy, "mappings" | "settings" | "aliases") => {
                if !matches!(value.value, Json::Object(_)) {
                    return Err(refused(&format!("[{key}] must be an object")));
                }
            }
            (TemplateKind::Legacy, "order") => {
                if !matches!(&value.value, Json::Number(number) if number.parse::<i64>().is_ok()) {
                    return Err(refused("[order] must be a whole number"));
                }
            }
            (TemplateKind::Legacy, "version") => {}
            (TemplateKind::Legacy, other) => return Err(refused(&format!("unknown field [{other}]"))),
            (_, "template") => {
                let Json::Object(parts) = &value.value else {
                    return Err(refused("[template] must be an object"));
                };
                for (part, part_value) in parts {
                    match part.as_str() {
                        "mappings" | "settings" | "aliases" if matches!(part_value.value, Json::Object(_)) => {}
                        "lifecycle" => {}
                        other => return Err(refused(&format!("unknown or malformed field [{other}] in [template]"))),
                    }
                }
            }
            (_, "version" | "_meta" | "deprecated") => {}
            (TemplateKind::Index, "index_patterns") => {
                if strings_of(value).is_none_or(|patterns| patterns.is_empty()) {
                    return Err(refused("[index_patterns] must be a string or an array of strings"));
                }
            }
            (TemplateKind::Index, "composed_of") => {
                if !matches!(value.value, Json::Array(_)) || strings_of(value).is_none() {
                    return Err(refused("[composed_of] must be an array of names"));
                }
            }
            (TemplateKind::Index, "priority") => {
                let valid = matches!(&value.value, Json::Number(number) if number.parse::<u64>().is_ok());
                if !valid {
                    return Err(refused("[priority] must be a non-negative integer"));
                }
            }
            (TemplateKind::Index, "data_stream" | "allow_auto_create" | "ignore_missing_component_templates") => {}
            (_, other) => return Err(refused(&format!("unknown field [{other}]"))),
        }
    }
    match kind {
        TemplateKind::Legacy
            if member(&parsed, "index_patterns").is_none() && member(&parsed, "template").is_none() =>
        {
            Err(error(400, "action_request_validation_exception", "Validation Failed: 1: index patterns are missing;"))
        }
        TemplateKind::Index if member(&parsed, "index_patterns").is_none() => {
            Err(error(400, "action_request_validation_exception", "Validation Failed: 1: index patterns are missing;"))
        }
        TemplateKind::Component if member(&parsed, "template").is_none() => {
            Err(error(400, "action_request_validation_exception", "Validation Failed: 1: template is missing;"))
        }
        _ => Ok(written(&parsed)),
    }
}

/// Merges `over` into `base`: objects member by member, anything else replaced.
pub(super) fn merge_into(base: &mut Json, over: &Json) {
    match (base, over) {
        (Json::Object(base_members), Json::Object(over_members)) => {
            for (key, value) in over_members {
                match base_members.iter_mut().find(|(existing, _)| existing == key) {
                    Some((_, existing)) => merge_into(&mut existing.value, &value.value),
                    None => base_members.push((key.clone(), value.clone())),
                }
            }
        }
        (base, over) => *base = over.clone(),
    }
}

/// What the matching index template gives a new index.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Applied {
    pub(super) mappings: Option<Json>,
    pub(super) aliases: Vec<String>,
}

fn stored(keys: &dyn KeysRead, kind: TemplateKind, name: &str) -> Result<Option<Node>, String> {
    Ok(keys
        .get(&template_key(kind, name))?
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| parse_document(&text).ok()))
}

fn add_template_part(template: &Node, mappings: &mut Json, aliases: &mut Vec<String>) {
    let Some(inner) = member(template, "template") else {
        return;
    };
    if let Some(found) = member(inner, "mappings") {
        merge_into(mappings, &found.value);
    }
    if let Some(Json::Object(named)) = member(inner, "aliases").map(|node| &node.value) {
        for (alias, _) in named {
            if !aliases.contains(alias) {
                aliases.push(alias.clone());
            }
        }
    }
}

/// The mappings and aliases the highest-priority index template matching `index` gives it,
/// its component templates first and its own template last; ties go to the first by name.
pub(super) fn template_for(keys: &dyn KeysRead, index: &str) -> Result<Applied, String> {
    let mut best: Option<(u64, Node)> = None;
    for (_, bytes) in keys.scan(&[INDEX_TEMPLATE_RECORD])? {
        let Some(template) = String::from_utf8(bytes).ok().and_then(|text| parse_document(&text).ok()) else {
            continue;
        };
        let patterns = member(&template, "index_patterns").and_then(strings_of).unwrap_or_default();
        if !patterns.iter().any(|pattern| index_glob(pattern, index)) {
            continue;
        }
        let priority = match member(&template, "priority").map(|node| &node.value) {
            Some(Json::Number(number)) => number.parse().unwrap_or(0),
            _ => 0,
        };
        if best.as_ref().is_none_or(|(highest, _)| priority > *highest) {
            best = Some((priority, template));
        }
    }
    let Some((_, template)) = best else {
        return legacy_templates_for(keys, index);
    };
    let mut mappings = Json::Object(Vec::new());
    let mut aliases = Vec::new();
    for component in member(&template, "composed_of").and_then(strings_of).unwrap_or_default() {
        if let Some(part) = stored(keys, TemplateKind::Component, &component)? {
            add_template_part(&part, &mut mappings, &mut aliases);
        }
    }
    add_template_part(&template, &mut mappings, &mut aliases);
    let mappings = match &mappings {
        Json::Object(members) if members.is_empty() => None,
        _ => Some(mappings),
    };
    Ok(Applied { mappings, aliases })
}

/// What the legacy templates (`_template`) matching `index` give it when no index template
/// does: each one's mappings and aliases merged in `order`, lowest first, so a higher order wins.
fn legacy_templates_for(keys: &dyn KeysRead, index: &str) -> Result<Applied, String> {
    let mut matching: Vec<(i64, String, Node)> = Vec::new();
    for (key, bytes) in keys.scan(&[LEGACY_TEMPLATE_RECORD])? {
        let Some(template) = String::from_utf8(bytes).ok().and_then(|text| parse_document(&text).ok()) else {
            continue;
        };
        let patterns = member(&template, "index_patterns")
            .or_else(|| member(&template, "template"))
            .and_then(strings_of)
            .unwrap_or_default();
        if !patterns.iter().any(|pattern| index_glob(pattern, index)) {
            continue;
        }
        let order = match member(&template, "order").map(|node| &node.value) {
            Some(Json::Number(number)) => number.parse().unwrap_or(0),
            _ => 0,
        };
        matching.push((order, String::from_utf8_lossy(&key[1..]).into_owned(), template));
    }
    matching.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let mut mappings = Json::Object(Vec::new());
    let mut aliases = Vec::new();
    for (_, _, template) in &matching {
        if let Some(found) = member(template, "mappings") {
            merge_into(&mut mappings, &found.value);
        }
        if let Some(Json::Object(named)) = member(template, "aliases").map(|node| &node.value) {
            for (alias, _) in named {
                if !aliases.contains(alias) {
                    aliases.push(alias.clone());
                }
            }
        }
    }
    let mappings = match &mappings {
        Json::Object(members) if members.is_empty() => None,
        _ => Some(mappings),
    };
    Ok(Applied { mappings, aliases })
}

/// The stored mappings text for a new index: the template's mappings with `requested` merged
/// over them.
pub(super) fn mappings_text(applied: &Applied, requested: Option<&Node>) -> String {
    let mut mappings = applied.mappings.clone().unwrap_or(Json::Object(Vec::new()));
    if let Some(requested) = requested {
        merge_into(&mut mappings, &requested.value);
    }
    written(&plain(mappings))
}

impl Api<'_> {
    pub(super) fn put_template(&self, kind: TemplateKind, name: &str, request: &Request) -> Answer {
        if name.is_empty() || name.len() > MAX_TEMPLATE_NAME_BYTES || name.contains(['*', ',', ' ', '"']) {
            return Ok(error(400, "invalid_index_template_exception", &format!("invalid template name [{name}]")));
        }
        let text = match validated(kind, &request.body) {
            Ok(text) => text,
            Err(refusal) => return Ok(refusal),
        };
        let create_only = query_flag(request, "create").is_some_and(|value| value.is_empty() || value == "true");
        let outcome = write(self.keyspace, |keys| {
            let key = template_key(kind, name);
            let exists = keys.get(&key)?.is_some();
            if exists && create_only {
                return Ok(Some(error(
                    400,
                    "illegal_argument_exception",
                    &format!("{} [{name}] already exists", kind.member().replace('_', " ")),
                )));
            }
            if !exists && keys.scan(&[kind.record()])?.len() >= MAX_TEMPLATES {
                return Ok(Some(error(400, "illegal_argument_exception", "too many templates")));
            }
            keys.put(&key, text.as_bytes())?;
            Ok(None)
        })?;
        Ok(outcome.unwrap_or_else(|| json(200, "{\"acknowledged\":true}".to_owned())))
    }

    pub(super) fn get_templates(&self, kind: TemplateKind, names: Option<&str>, head: bool) -> Answer {
        let all = read(self.keyspace, |keys| keys.scan(&[kind.record()]))?;
        let wanted: Option<Vec<&str>> = names.map(|names| names.split(',').collect());
        let mut out = Out::default();
        let legacy = kind == TemplateKind::Legacy;
        out.begin_object();
        if !legacy {
            out.key(kind.listed()).begin_array();
        }
        let mut found = 0;
        for (key, bytes) in all {
            let name = String::from_utf8_lossy(&key[1..]).into_owned();
            if wanted
                .as_ref()
                .is_some_and(|wanted| !wanted.iter().any(|pattern| index_glob(pattern, &name)))
            {
                continue;
            }
            found += 1;
            if legacy {
                out.key(&name).raw(&String::from_utf8_lossy(&bytes));
                continue;
            }
            out.begin_object()
                .key("name")
                .string(&name)
                .key(kind.member())
                .raw(&String::from_utf8_lossy(&bytes))
                .end_object();
        }
        if !legacy {
            out.end_array();
        }
        out.end_object();
        if found == 0 && names.is_some_and(|names| !names.contains('*')) {
            return Ok(if head { json(404, String::new()) } else { kind.missing(names.unwrap_or_default()) });
        }
        Ok(if head { json(200, String::new()) } else { json(200, out.finish()) })
    }

    pub(super) fn delete_template(&self, kind: TemplateKind, name: &str) -> Answer {
        let removed = write(self.keyspace, |keys| keys.delete(&template_key(kind, name)))?;
        Ok(if removed {
            json(200, "{\"acknowledged\":true}".to_owned())
        } else {
            kind.missing(name)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(text: &str) -> Node {
        parse_document(text).unwrap()
    }

    #[test]
    fn merging_keeps_both_sides_properties_and_the_later_value_wins() {
        let mut base = node(r#"{"properties":{"a":{"type":"keyword"},"b":{"type":"long"}},"dynamic":true}"#).value;
        merge_into(&mut base, &node(r#"{"properties":{"b":{"type":"double"},"c":{"type":"text"}},"dynamic":false}"#).value);
        assert_eq!(
            written(&plain(base)),
            r#"{"properties":{"a":{"type":"keyword"},"b":{"type":"double"},"c":{"type":"text"}},"dynamic":false}"#
        );
    }

    #[test]
    fn templates_are_checked_before_they_are_kept() {
        assert!(validated(TemplateKind::Index, br#"{"index_patterns":["logs-*"],"priority":5,"template":{"mappings":{}}}"#).is_ok());
        assert!(validated(TemplateKind::Index, br#"{"template":{}}"#).is_err());
        assert!(validated(TemplateKind::Index, br#"{"index_patterns":[],"template":{}}"#).is_err());
        assert!(validated(TemplateKind::Index, br#"{"index_patterns":"a*","priority":-1}"#).is_err());
        assert!(validated(TemplateKind::Index, br#"{"index_patterns":"a*","bogus":1}"#).is_err());
        assert!(validated(TemplateKind::Component, br#"{"template":{"mappings":{}}}"#).is_ok());
        assert!(validated(TemplateKind::Component, br#"{"index_patterns":"a*"}"#).is_err());
    }
}
