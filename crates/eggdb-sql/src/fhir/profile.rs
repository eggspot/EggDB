//! FHIR validation against a StructureDefinition (D1068): each ElementDefinition of the profile's
//! snapshot (or, without one, its differential) is checked on every instance of its parent
//! element, for cardinality (`min`, `max`), the type of each value (the `type` codes, and the
//! type a choice element's name carries), `fixed[x]` (equal) and `pattern[x]` (contained), and
//! `required` bindings against the ValueSets given (their `compose.include` concepts and
//! `expansion.contains`). Slices and FHIRPath constraints are reported as not checked.

use super::{Validator, member};
use crate::json::{Json, Node, parse_document};

const MOST_ELEMENTS: usize = 5_000;
const MOST_INSTANCES: usize = 100_000;

/// One value found at an element's path, with the path to show in issues and, for a choice
/// element, the type its name carries.
struct Found<'n> {
    node: &'n Node,
    path: String,
    choice_type: Option<String>,
}

fn text<'n>(members: &'n [(String, Node)], name: &str) -> Option<&'n str> {
    match member(members, name).map(|node| &node.value) {
        Some(Json::String(text)) => Some(text),
        _ => None,
    }
}

fn number(value: &Json) -> Option<f64> {
    match value {
        Json::Number(text) => text.parse().ok(),
        _ => None,
    }
}

fn equal(left: &Json, right: &Json) -> bool {
    match (left, right) {
        (Json::Number(_), Json::Number(_)) => number(left) == number(right),
        (Json::Array(left), Json::Array(right)) => left.len() == right.len() && left.iter().zip(right).all(|(a, b)| equal(&a.value, &b.value)),
        (Json::Object(left), Json::Object(right)) => {
            left.len() == right.len() && left.iter().all(|(key, value)| member(right, key).is_some_and(|other| equal(&value.value, &other.value)))
        }
        _ => left == right,
    }
}

fn contains(instance: &Json, pattern: &Json) -> bool {
    match (instance, pattern) {
        (Json::Object(instance), Json::Object(pattern)) => pattern
            .iter()
            .all(|(key, wanted)| member(instance, key).is_some_and(|found| contains(&found.value, &wanted.value))),
        (Json::Array(instance), Json::Array(pattern)) => {
            pattern.iter().all(|wanted| instance.iter().any(|found| contains(&found.value, &wanted.value)))
        }
        _ => equal(instance, pattern),
    }
}

fn choice_members<'n>(members: &'n [(String, Node)], stem: &str) -> Vec<(&'n str, &'n Node)> {
    members
        .iter()
        .filter(|(key, _)| key.len() > stem.len() && key.starts_with(stem) && key[stem.len()..].starts_with(|first: char| first.is_ascii_uppercase()))
        .map(|(key, node)| (key.as_str(), node))
        .collect()
}

fn type_name(suffix: &str) -> String {
    let mut characters = suffix.chars();
    match characters.next() {
        Some(first) if matches!(suffix, "Boolean" | "Integer" | "Decimal" | "String" | "Uri" | "Url" | "Canonical" | "Code" | "Id" | "Oid" | "Uuid" | "Markdown" | "Base64Binary" | "Date" | "DateTime" | "Instant" | "Time" | "PositiveInt" | "UnsignedInt" | "Integer64") => {
            first.to_lowercase().chain(characters).collect()
        }
        _ => suffix.to_owned(),
    }
}

fn children<'n>(found: &Found<'n>, segment: &str) -> Vec<Found<'n>> {
    let Json::Object(members) = &found.node.value else {
        return Vec::new();
    };
    let named: Vec<(&str, &Node, Option<String>)> = match segment.strip_suffix("[x]") {
        Some(stem) => choice_members(members, stem).into_iter().map(|(key, node)| (key, node, Some(type_name(&key[stem.len()..])))).collect(),
        None => member(members, segment).map(|node| vec![(segment, node, None)]).unwrap_or_default(),
    };
    let mut out = Vec::new();
    for (key, node, choice_type) in named {
        match &node.value {
            Json::Array(items) => {
                for (at, item) in items.iter().enumerate() {
                    out.push(Found { node: item, path: format!("{}.{key}[{at}]", found.path), choice_type: choice_type.clone() });
                }
            }
            _ => out.push(Found { node, path: format!("{}.{key}", found.path), choice_type }),
        }
    }
    out
}

fn instances<'n>(root: &'n Node, kind: &str, path: &str) -> Vec<Found<'n>> {
    let mut segments = path.split('.');
    if segments.next() != Some(kind) {
        return Vec::new();
    }
    let mut current = vec![Found { node: root, path: kind.to_owned(), choice_type: None }];
    for segment in segments {
        let mut next = Vec::new();
        for found in &current {
            next.extend(children(found, segment));
            if next.len() > MOST_INSTANCES {
                return next;
            }
        }
        current = next;
    }
    current
}

fn kind_fits(code: &str, value: &Json) -> bool {
    match code {
        "boolean" => matches!(value, Json::Boolean(_)),
        "integer" | "positiveInt" | "unsignedInt" => number(value).is_some_and(|number| number.fract() == 0.0)
            && (code != "positiveInt" || number(value).is_some_and(|number| number > 0.0))
            && (code != "unsignedInt" || number(value).is_some_and(|number| number >= 0.0)),
        "decimal" => matches!(value, Json::Number(_)),
        "integer64" => matches!(value, Json::String(text) if text.parse::<i64>().is_ok()),
        "string" | "code" | "id" | "uri" | "url" | "canonical" | "oid" | "uuid" | "markdown" | "base64Binary" | "xhtml" => matches!(value, Json::String(_)),
        "date" => matches!(value, Json::String(text) if super::valid_date(text)),
        "dateTime" => matches!(value, Json::String(text) if super::valid_date_time(text)),
        "instant" => matches!(value, Json::String(text) if super::valid_instant(text)),
        "time" => matches!(value, Json::String(text) if text.len() >= 8 && text.as_bytes()[2] == b':'),
        "Resource" => matches!(value, Json::Object(members) if member(members, "resourceType").is_some()),
        "http://hl7.org/fhirpath/System.String" => matches!(value, Json::String(_)),
        _ => matches!(value, Json::Object(_)),
    }
}

/// The codes a ValueSet allows, as (system, code); an empty system matches any.
fn allowed_codes(value_sets: &[Node], url: &str) -> Option<Vec<(String, String)>> {
    let canonical = url.split('|').next().unwrap_or(url);
    let set = value_sets.iter().find_map(|node| match &node.value {
        Json::Object(members) if text(members, "url") == Some(canonical) => Some(members),
        _ => None,
    })?;
    let mut codes = Vec::new();
    if let Some(Json::Object(compose)) = member(set, "compose").map(|node| &node.value)
        && let Some(Json::Array(includes)) = member(compose, "include").map(|node| &node.value)
    {
        for include in includes {
            let Json::Object(include) = &include.value else { continue };
            let system = text(include, "system").unwrap_or_default().to_owned();
            if let Some(Json::Array(concepts)) = member(include, "concept").map(|node| &node.value) {
                for concept in concepts {
                    if let Json::Object(concept) = &concept.value
                        && let Some(code) = text(concept, "code")
                    {
                        codes.push((system.clone(), code.to_owned()));
                    }
                }
            }
        }
    }
    if let Some(Json::Object(expansion)) = member(set, "expansion").map(|node| &node.value) {
        let mut pending: Vec<&Node> = match member(expansion, "contains").map(|node| &node.value) {
            Some(Json::Array(items)) => items.iter().collect(),
            _ => Vec::new(),
        };
        while let Some(item) = pending.pop() {
            let Json::Object(item) = &item.value else { continue };
            if let Some(code) = text(item, "code") {
                codes.push((text(item, "system").unwrap_or_default().to_owned(), code.to_owned()));
            }
            if let Some(Json::Array(nested)) = member(item, "contains").map(|node| &node.value) {
                pending.extend(nested.iter());
            }
        }
    }
    Some(codes)
}

fn codings(value: &Json) -> Vec<(String, String)> {
    match value {
        Json::String(code) => vec![(String::new(), code.clone())],
        Json::Object(members) => {
            if let Some(Json::Array(list)) = member(members, "coding").map(|node| &node.value) {
                return list.iter().flat_map(|coding| codings(&coding.value)).collect();
            }
            text(members, "code")
                .map(|code| vec![(text(members, "system").unwrap_or_default().to_owned(), code.to_owned())])
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

impl Validator {
    fn element_definition(&mut self, root: &Node, kind: &str, definition: &[(String, Node)], value_sets: &[Node]) {
        let Some(path) = text(definition, "path") else {
            self.issue("error", "structure", "", "an ElementDefinition has no path");
            return;
        };
        if member(definition, "sliceName").is_some() {
            self.issue("information", "not-supported", path, format!("the slice of {path} is not checked"));
            return;
        }
        let Some((parent_path, segment)) = path.rsplit_once('.') else {
            return;
        };
        let minimum = member(definition, "min").and_then(|node| number(&node.value)).unwrap_or(0.0) as usize;
        let maximum = match text(definition, "max") {
            Some("*") | None => usize::MAX,
            Some(written) => written.parse().unwrap_or(usize::MAX),
        };
        let types: Vec<String> = match member(definition, "type").map(|node| &node.value) {
            Some(Json::Array(types)) => types
                .iter()
                .filter_map(|entry| match &entry.value {
                    Json::Object(entry) => text(entry, "code").map(str::to_owned),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let fixed = definition.iter().find(|(key, _)| key.starts_with("fixed")).map(|(_, node)| node);
        let pattern = definition.iter().find(|(key, _)| key.starts_with("pattern")).map(|(_, node)| node);
        let binding = match member(definition, "binding").map(|node| &node.value) {
            Some(Json::Object(binding)) if text(binding, "strength") == Some("required") => text(binding, "valueSet"),
            _ => None,
        };
        let allowed = binding.map(|url| (url, allowed_codes(value_sets, url)));
        if let Some((url, None)) = &allowed {
            self.issue("information", "not-supported", path, format!("the ValueSet {url} was not given, so {path}'s codes are not checked"));
        }
        for parent in instances(root, kind, parent_path) {
            let values = children(&parent, segment);
            if values.len() < minimum {
                self.issue("error", "required", &parent.path, format!("{path} needs at least {minimum}, found {}", values.len()));
            }
            if values.len() > maximum {
                self.issue("error", "structure", &parent.path, format!("{path} allows at most {maximum}, found {}", values.len()));
            }
            for value in &values {
                let fits = match &value.choice_type {
                    Some(carried) => types.is_empty() || types.iter().any(|code| code == carried),
                    None => types.is_empty() || types.iter().any(|code| kind_fits(code, &value.node.value)),
                };
                if !fits {
                    self.issue("error", "structure", &value.path, format!("{path} must be of type {}", types.join(" or ")));
                }
                if let Some(fixed) = fixed
                    && !equal(&value.node.value, &fixed.value)
                {
                    self.issue("error", "value", &value.path, format!("{path} must equal the profile's fixed value"));
                }
                if let Some(pattern) = pattern
                    && !contains(&value.node.value, &pattern.value)
                {
                    self.issue("error", "value", &value.path, format!("{path} must contain the profile's pattern"));
                }
                if let Some((url, Some(codes))) = &allowed {
                    let found = codings(&value.node.value);
                    let bound = found
                        .iter()
                        .any(|(system, code)| codes.iter().any(|(allowed_system, allowed_code)| allowed_code == code && (allowed_system.is_empty() || system.is_empty() || allowed_system == system)));
                    if !bound {
                        self.issue("error", "code-invalid", &value.path, format!("{path} has no code from the required ValueSet {url}"));
                    }
                }
            }
        }
        if member(definition, "constraint").is_some_and(|node| matches!(&node.value, Json::Array(items) if items.iter().any(|item| matches!(&item.value, Json::Object(constraint) if text(constraint, "source").is_none_or(|source| !source.starts_with("http://hl7.org/fhir/StructureDefinition/")))))) {
            self.issue("information", "not-supported", path, format!("the FHIRPath constraints of {path} are not checked"));
        }
    }

    fn profile(&mut self, root: &Node, profile: &Node, value_sets: &[Node]) {
        let Json::Object(definition) = &profile.value else {
            self.issue("fatal", "structure", "", "the profile must be a StructureDefinition object");
            return;
        };
        if text(definition, "resourceType") != Some("StructureDefinition") {
            self.issue("fatal", "structure", "", "the profile must be a StructureDefinition");
            return;
        }
        let Json::Object(resource) = &root.value else {
            return;
        };
        let kind = text(resource, "resourceType").unwrap_or_default().to_owned();
        if let Some(constrained) = text(definition, "type")
            && constrained != kind
        {
            self.issue("error", "invalid", &kind, format!("the profile constrains {constrained}, not {kind}"));
            return;
        }
        let elements = ["snapshot", "differential"].iter().find_map(|part| match member(definition, part).map(|node| &node.value) {
            Some(Json::Object(part)) => match member(part, "element").map(|node| &node.value) {
                Some(Json::Array(elements)) => Some(elements),
                _ => None,
            },
            _ => None,
        });
        let Some(elements) = elements else {
            self.issue("fatal", "structure", "", "the profile has neither snapshot nor differential elements");
            return;
        };
        if member(definition, "snapshot").is_none() {
            self.issue("information", "informational", "", "the profile has no snapshot, so only its differential is checked");
        }
        for element in elements.iter().take(MOST_ELEMENTS) {
            if let Json::Object(element) = &element.value {
                self.element_definition(root, &kind, element, value_sets);
            }
        }
    }
}

/// An `OperationOutcome` for `resource` checked structurally and against `profile`, a
/// StructureDefinition, with `value_sets` (a ValueSet or an array of them, possibly a Bundle's
/// entries) answering its required bindings.
pub(crate) fn validate(resource: &str, profile: &str, value_sets: Option<&str>) -> String {
    let mut validator = Validator { issues: Vec::new() };
    let document = match parse_document(resource) {
        Ok(document) => document,
        Err(error) => {
            validator.issue("fatal", "structure", "", format!("the resource is not JSON: {}", error.message));
            return super::outcome(&validator.issues);
        }
    };
    validator.resource(&document, "", 0);
    let sets: Vec<Node> = match value_sets.map(parse_document) {
        None => Vec::new(),
        Some(Ok(Node { value: Json::Array(items), .. })) => items,
        Some(Ok(Node { value: Json::Object(members), start, end })) => match (text(&members, "resourceType"), member(&members, "entry").map(|node| &node.value)) {
            (Some("Bundle"), Some(Json::Array(entries))) => entries
                .iter()
                .filter_map(|entry| match &entry.value {
                    Json::Object(entry) => member(entry, "resource").cloned(),
                    _ => None,
                })
                .collect(),
            _ => vec![Node { value: Json::Object(members), start, end }],
        },
        Some(Ok(_)) => Vec::new(),
        Some(Err(error)) => {
            validator.issue("fatal", "structure", "", format!("the ValueSets are not JSON: {}", error.message));
            Vec::new()
        }
    };
    match parse_document(profile) {
        Ok(profile) => validator.profile(&document, &profile, &sets),
        Err(error) => validator.issue("fatal", "structure", "", format!("the profile is not JSON: {}", error.message)),
    }
    super::outcome(&validator.issues)
}
