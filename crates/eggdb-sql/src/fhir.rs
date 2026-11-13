//! Structural validation of FHIR resources in JSON (D834), written from the FHIR specification's
//! JSON format rules: the shape every resource must have, the rules of primitive values and
//! extensions, references, date and time formats, and the required elements and code values of
//! common core resources. It is not a full profile validator.

mod profile;

use crate::json::{Json, Node, parse_document};

pub(crate) use profile::validate as validate_against_profile;

const MOST_DEPTH: usize = 64;
const MOST_ISSUES: usize = 1000;

struct Required {
    resource: &'static str,
    elements: &'static [&'static str],
}

const REQUIRED: [Required; 11] = [
    Required { resource: "Observation", elements: &["status", "code"] },
    Required { resource: "Encounter", elements: &["status"] },
    Required { resource: "Condition", elements: &["subject"] },
    Required { resource: "Procedure", elements: &["status", "subject"] },
    Required { resource: "MedicationRequest", elements: &["status", "intent", "subject", "medication[x]"] },
    Required { resource: "DiagnosticReport", elements: &["status", "code"] },
    Required { resource: "AllergyIntolerance", elements: &["patient"] },
    Required { resource: "Immunization", elements: &["status", "vaccineCode", "patient", "occurrence[x]"] },
    Required { resource: "Bundle", elements: &["type"] },
    Required { resource: "OperationOutcome", elements: &["issue"] },
    Required { resource: "Consent", elements: &["status"] },
];

struct Codes {
    path: &'static str,
    allowed: &'static [&'static str],
}

const CODES: [Codes; 6] = [
    Codes {
        path: "Observation.status",
        allowed: &["registered", "preliminary", "final", "amended", "corrected", "cancelled", "entered-in-error", "unknown"],
    },
    Codes { path: "Patient.gender", allowed: &["male", "female", "other", "unknown"] },
    Codes {
        path: "Bundle.type",
        allowed: &[
            "document",
            "message",
            "transaction",
            "transaction-response",
            "batch",
            "batch-response",
            "history",
            "searchset",
            "collection",
            "subscription-notification",
        ],
    },
    Codes {
        path: "DiagnosticReport.status",
        allowed: &["registered", "partial", "preliminary", "modified", "final", "amended", "corrected", "appended", "cancelled", "entered-in-error", "unknown"],
    },
    Codes {
        path: "Encounter.status",
        allowed: &["planned", "arrived", "triaged", "in-progress", "onleave", "finished", "cancelled", "entered-in-error", "unknown", "on-hold", "discharged", "completed", "discontinued"],
    },
    Codes { path: "Narrative.status", allowed: &["generated", "extensions", "additional", "empty"] },
];

struct Issue {
    severity: &'static str,
    code: &'static str,
    diagnostics: String,
    expression: String,
}

struct Validator {
    issues: Vec<Issue>,
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn digits(text: &str, count: usize) -> bool {
    text.len() == count && text.bytes().all(|byte| byte.is_ascii_digit())
}

fn valid_date(text: &str) -> bool {
    let parts: Vec<&str> = text.split('-').collect();
    match parts.as_slice() {
        [year] => digits(year, 4),
        [year, month] => digits(year, 4) && digits(month, 2) && (1..=12).contains(&month.parse::<u8>().unwrap_or(0)),
        [year, month, day] => {
            digits(year, 4)
                && digits(month, 2)
                && digits(day, 2)
                && (1..=12).contains(&month.parse::<u8>().unwrap_or(0))
                && (1..=31).contains(&day.parse::<u8>().unwrap_or(0))
        }
        _ => false,
    }
}

fn valid_time_with_zone(text: &str) -> bool {
    let (time, zone) = if let Some(time) = text.strip_suffix('Z') {
        (time, "")
    } else if let Some(at) = text.rfind(['+', '-']) {
        (&text[..at], &text[at + 1..])
    } else {
        return false;
    };
    let zone_ok = zone.is_empty() || matches!(zone.split_once(':'), Some((hours, minutes)) if digits(hours, 2) && digits(minutes, 2));
    let (clock, fraction) = time.split_once('.').map_or((time, None), |(clock, fraction)| (clock, Some(fraction)));
    let clock_ok = matches!(clock.split(':').collect::<Vec<_>>().as_slice(), [hours, minutes, seconds] if digits(hours, 2) && digits(minutes, 2) && digits(seconds, 2) && hours.parse::<u8>().unwrap_or(99) < 24 && minutes.parse::<u8>().unwrap_or(99) < 60 && seconds.parse::<u8>().unwrap_or(99) < 61);
    let fraction_ok = fraction.is_none_or(|fraction| !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit()));
    zone_ok && clock_ok && fraction_ok
}

fn valid_date_time(text: &str) -> bool {
    match text.split_once('T') {
        None => valid_date(text),
        Some((date, time)) => date.len() == 10 && valid_date(date) && valid_time_with_zone(time),
    }
}

fn valid_instant(text: &str) -> bool {
    matches!(text.split_once('T'), Some((date, _)) if date.len() == 10) && valid_date_time(text)
}

fn valid_id(text: &str) -> bool {
    (1..=64).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.')
}

fn valid_reference(text: &str) -> bool {
    if text.starts_with('#') || text.starts_with("urn:uuid:") || text.starts_with("urn:oid:") || text.contains("://") {
        return true;
    }
    let parts: Vec<&str> = text.split('/').collect();
    match parts.as_slice() {
        [kind, id] => kind.starts_with(|first: char| first.is_ascii_uppercase()) && valid_id(id),
        [kind, id, "_history", version] => {
            kind.starts_with(|first: char| first.is_ascii_uppercase()) && valid_id(id) && valid_id(version)
        }
        _ => false,
    }
}

fn member<'n>(members: &'n [(String, Node)], name: &str) -> Option<&'n Node> {
    members.iter().find(|(key, _)| key == name).map(|(_, node)| node)
}

impl Validator {
    fn issue(&mut self, severity: &'static str, code: &'static str, path: &str, diagnostics: impl Into<String>) {
        if self.issues.len() < MOST_ISSUES {
            self.issues.push(Issue {
                severity,
                code,
                diagnostics: diagnostics.into(),
                expression: path.to_owned(),
            });
        }
    }

    fn resource(&mut self, node: &Node, path: &str, depth: usize) {
        let Json::Object(members) = &node.value else {
            self.issue("error", "structure", path, "a resource must be a JSON object");
            return;
        };
        let kind = match member(members, "resourceType").map(|node| &node.value) {
            Some(Json::String(kind)) if kind.starts_with(|first: char| first.is_ascii_uppercase()) && kind.bytes().all(|byte| byte.is_ascii_alphanumeric()) => kind.clone(),
            Some(_) => {
                self.issue("error", "structure", path, "resourceType must be the name of a resource type");
                return;
            }
            None => {
                self.issue("error", "required", path, "a resource needs resourceType");
                return;
            }
        };
        let here = if path.is_empty() { kind.clone() } else { path.to_owned() };
        if let Some(id) = member(members, "id") {
            match &id.value {
                Json::String(text) if valid_id(text) => {}
                _ => self.issue("error", "value", &format!("{here}.id"), "id must be 1 to 64 letters, digits, '-' or '.'"),
            }
        }
        if let Some(required) = REQUIRED.iter().find(|required| required.resource == kind) {
            for element in required.elements {
                let present = match element.strip_suffix("[x]") {
                    Some(stem) => members.iter().any(|(key, _)| key.starts_with(stem) && key.len() > stem.len() && key[stem.len()..].starts_with(|first: char| first.is_ascii_uppercase())),
                    None => member(members, element).is_some(),
                };
                if !present {
                    self.issue("error", "required", &format!("{here}.{element}"), format!("{kind}.{element} is required"));
                }
            }
        }
        self.members(members, &here, &kind, depth);
    }

    fn members(&mut self, members: &[(String, Node)], path: &str, owner: &str, depth: usize) {
        if depth > MOST_DEPTH {
            self.issue("error", "too-costly", path, "the resource is nested too deeply");
            return;
        }
        if members.is_empty() {
            self.issue("error", "structure", path, "an object must not be empty");
        }
        for (index, (key, value)) in members.iter().enumerate() {
            if members[..index].iter().any(|(earlier, _)| earlier == key) {
                self.issue("error", "structure", &format!("{path}.{key}"), format!("{key} appears twice"));
            }
            let child = format!("{path}.{key}");
            if key == "resourceType" {
                continue;
            }
            if let Some(primitive) = key.strip_prefix('_') {
                self.primitive_extension(value, members, primitive, &child, depth);
                continue;
            }
            match key.as_str() {
                "contained" => self.resources(value, &child, depth),
                "resource" if matches!(owner, "entry" | "parameter") => self.resource(value, &child, depth + 1),
                "extension" | "modifierExtension" => self.extensions(value, &child, depth),
                _ => self.element(key, value, &child, owner, depth),
            }
        }
    }

    fn resources(&mut self, value: &Node, path: &str, depth: usize) {
        match &value.value {
            Json::Array(items) if !items.is_empty() => {
                for (at, item) in items.iter().enumerate() {
                    self.resource(item, &format!("{path}[{at}]"), depth + 1);
                }
            }
            _ => self.issue("error", "structure", path, "contained must be a non-empty array of resources"),
        }
    }

    fn primitive_extension(&mut self, value: &Node, members: &[(String, Node)], primitive: &str, path: &str, depth: usize) {
        let check = |validator: &mut Self, node: &Node, at: &str| match &node.value {
            Json::Null => {}
            Json::Object(fields) => validator.members(fields, at, "Element", depth + 1),
            _ => validator.issue("error", "structure", at, "a primitive's extension holder must be an object"),
        };
        match (&value.value, member(members, primitive).map(|node| &node.value)) {
            (Json::Array(holders), Some(Json::Array(values))) => {
                if holders.len() != values.len() {
                    self.issue("error", "structure", path, format!("_{primitive} must have as many items as {primitive}"));
                }
                for (at, holder) in holders.iter().enumerate() {
                    check(self, holder, &format!("{path}[{at}]"));
                }
            }
            (Json::Array(_), _) => self.issue("error", "structure", path, format!("_{primitive} is an array but {primitive} is not")),
            _ => check(self, value, path),
        }
    }

    fn extensions(&mut self, value: &Node, path: &str, depth: usize) {
        let Json::Array(items) = &value.value else {
            self.issue("error", "structure", path, "extensions must be an array");
            return;
        };
        if items.is_empty() {
            self.issue("error", "structure", path, "an array must not be empty");
        }
        for (at, item) in items.iter().enumerate() {
            let here = format!("{path}[{at}]");
            let Json::Object(fields) = &item.value else {
                self.issue("error", "structure", &here, "an extension must be an object");
                continue;
            };
            match member(fields, "url").map(|node| &node.value) {
                Some(Json::String(url)) if !url.is_empty() => {}
                _ => self.issue("error", "required", &format!("{here}.url"), "an extension needs a url"),
            }
            let values = fields.iter().filter(|(key, _)| key.starts_with("value")).count();
            let nested = member(fields, "extension").is_some();
            if values > 1 || (values == 1 && nested) || (values == 0 && !nested) {
                self.issue("error", "structure", &here, "an extension has one value[x] or nested extensions, not both or neither");
            }
            self.members(fields, &here, "Extension", depth + 1);
        }
    }

    fn element(&mut self, key: &str, value: &Node, path: &str, owner: &str, depth: usize) {
        match &value.value {
            Json::Null => self.issue("error", "structure", path, "null is only allowed inside arrays of primitives"),
            Json::String(text) if text.is_empty() => self.issue("error", "value", path, "a string must not be empty"),
            Json::String(text) => self.primitive(key, text, path, owner),
            Json::Array(items) => {
                if items.is_empty() {
                    self.issue("error", "structure", path, "an array must not be empty");
                }
                for (at, item) in items.iter().enumerate() {
                    let here = format!("{path}[{at}]");
                    match &item.value {
                        Json::Null => {}
                        Json::Object(fields) => self.members(fields, &here, key, depth + 1),
                        Json::String(text) => self.primitive(key, text, &here, owner),
                        Json::Array(_) => self.issue("error", "structure", &here, "arrays do not nest in FHIR JSON"),
                        _ => {}
                    }
                }
            }
            Json::Object(fields) => {
                if key == "text" {
                    self.narrative(fields, path);
                }
                if let Some(Json::String(reference)) = member(fields, "reference").map(|node| &node.value)
                    && !valid_reference(reference)
                {
                    self.issue("warning", "value", &format!("{path}.reference"), format!("{reference} is not a resource reference"));
                }
                let element_owner = if key == "meta" { "Meta" } else { key };
                self.members(fields, path, element_owner, depth + 1);
            }
            _ => {}
        }
    }

    fn narrative(&mut self, fields: &[(String, Node)], path: &str) {
        if let Some(Json::String(div)) = member(fields, "div").map(|node| &node.value)
            && !(div.starts_with("<div") && div.contains("http://www.w3.org/1999/xhtml"))
        {
            self.issue("error", "value", &format!("{path}.div"), "the narrative is a <div> in the XHTML namespace");
        }
    }

    fn primitive(&mut self, key: &str, text: &str, path: &str, owner: &str) {
        let checked = match key {
            "birthDate" => Some(("date", valid_date(text))),
            "lastUpdated" | "issued" | "instant" => Some(("instant", valid_instant(text))),
            key if key.ends_with("DateTime") || key == "recorded" || key == "authoredOn" => Some(("dateTime", valid_date_time(text))),
            "versionId" => Some(("id", valid_id(text))),
            _ => None,
        };
        if let Some((kind, false)) = checked {
            self.issue("error", "value", path, format!("{text} is not a valid {kind}"));
        }
        let qualified = format!("{}.{key}", if key == "status" && owner == "text" { "Narrative" } else { owner });
        if let Some(codes) = CODES.iter().find(|codes| codes.path == qualified)
            && !codes.allowed.contains(&text)
        {
            self.issue("error", "code-invalid", path, format!("{text} is not one of the codes {qualified} takes"));
        }
    }
}

/// An `OperationOutcome` (JSON) for `resource`: one issue per problem found, with its severity,
/// FHIR issue code, a diagnostic and the element's path, or one `informational` issue when there
/// is none, as FHIR's `$validate` answers.
pub(crate) fn validate(resource: &str) -> String {
    let mut validator = Validator { issues: Vec::new() };
    match parse_document(resource) {
        Ok(document) => validator.resource(&document, "", 0),
        Err(error) => validator.issue("fatal", "structure", "", format!("the resource is not JSON: {}", error.message)),
    }
    outcome(&validator.issues)
}

fn outcome(issues: &[Issue]) -> String {
    if issues.is_empty() {
        return "{\"resourceType\":\"OperationOutcome\",\"issue\":[{\"severity\":\"information\",\"code\":\"informational\",\"diagnostics\":\"All OK\"}]}".to_owned();
    }
    let issues: Vec<String> = issues
        .iter()
        .map(|issue| {
            let expression = if issue.expression.is_empty() { String::new() } else { format!(",\"expression\":[{}]", json_string(&issue.expression)) };
            format!(
                "{{\"severity\":\"{}\",\"code\":\"{}\",\"diagnostics\":{}{expression}}}",
                issue.severity,
                issue.code,
                json_string(&issue.diagnostics)
            )
        })
        .collect();
    format!("{{\"resourceType\":\"OperationOutcome\",\"issue\":[{}]}}", issues.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_valid_patient_is_all_ok() {
        let patient = r#"{"resourceType":"Patient","id":"p-1","meta":{"lastUpdated":"2024-05-01T10:00:00Z"},"gender":"female","birthDate":"1980-02-29","name":[{"family":"Ng","given":["Ada"]}],"extension":[{"url":"http://example.org/eye","valueString":"blue"}]}"#;
        assert!(validate(patient).contains("All OK"), "{}", validate(patient));
    }

    #[test]
    fn structure_formats_codes_and_required_elements_are_reported() {
        let outcome = validate(r#"{"resourceType":"Observation","id":"bad id!","status":"done","effectiveDateTime":"2024-13-01","subject":{"reference":"not a ref"},"note":[],"text":{"status":"generated","div":"<p>x</p>"},"extension":[{"url":"u"}]}"#);
        for expected in [
            "Observation.id",
            "Observation.code is required",
            "done is not one of the codes Observation.status takes",
            "2024-13-01 is not a valid dateTime",
            "not a ref is not a resource reference",
            "an array must not be empty",
            "the narrative is a <div>",
            "one value[x] or nested extensions",
        ] {
            assert!(outcome.contains(expected), "{expected}: {outcome}");
        }
        assert!(validate("[1]").contains("must be a JSON object"));
        assert!(validate("{\"id\":\"x\"}").contains("needs resourceType"));
        assert!(validate("{").contains("\"fatal\""));
    }

    #[test]
    fn bundles_and_contained_resources_are_validated_inside() {
        let outcome = validate(r#"{"resourceType":"Bundle","type":"collection","entry":[{"resource":{"resourceType":"Observation","status":"final"}}]}"#);
        assert!(outcome.contains("Observation.code is required"), "{outcome}");
        assert!(valid_instant("2024-05-01T10:00:00.123+02:00"));
        assert!(!valid_instant("2024-05-01"));
        assert!(valid_date_time("2024"));
        assert!(!valid_date_time("2024-05-01T10:00"));
    }

    #[test]
    fn profiles_check_cardinality_types_fixed_values_patterns_and_required_bindings() {
        let profile = r#"{"resourceType":"StructureDefinition","url":"http://example.org/vitals","type":"Observation","snapshot":{"element":[
            {"path":"Observation"},
            {"path":"Observation.status","min":1,"max":"1","type":[{"code":"code"}],"fixedCode":"final"},
            {"path":"Observation.code","min":1,"max":"1","type":[{"code":"CodeableConcept"}],"binding":{"strength":"required","valueSet":"http://example.org/vs/vitals|1.0"}},
            {"path":"Observation.category","min":1,"max":"*","type":[{"code":"CodeableConcept"}],"patternCodeableConcept":{"coding":[{"system":"http://terminology.hl7.org/CodeSystem/observation-category","code":"vital-signs"}]}},
            {"path":"Observation.value[x]","min":1,"max":"1","type":[{"code":"Quantity"}]},
            {"path":"Observation.note","max":"0"},
            {"path":"Observation.component","sliceName":"systolic"}
        ]}}"#;
        let value_sets = r#"{"resourceType":"ValueSet","url":"http://example.org/vs/vitals","compose":{"include":[{"system":"http://loinc.org","concept":[{"code":"8867-4"},{"code":"8310-5"}]}]}}"#;
        let good = r#"{"resourceType":"Observation","status":"final","code":{"coding":[{"system":"http://loinc.org","code":"8867-4"}]},"category":[{"coding":[{"system":"http://terminology.hl7.org/CodeSystem/observation-category","code":"vital-signs"}],"text":"Vital"}],"valueQuantity":{"value":72,"unit":"/min"}}"#;
        let outcome = validate_against_profile(good, profile, Some(value_sets));
        assert!(!outcome.contains(""error""), "{outcome}");
        assert!(outcome.contains("slice of Observation.component is not checked"), "{outcome}");
        let bad = r#"{"resourceType":"Observation","status":"preliminary","code":{"coding":[{"system":"http://loinc.org","code":"1234-5"}]},"category":[{"coding":[{"system":"http://example.org","code":"lab"}]}],"valueString":"fast","note":[{"text":"x"}]}"#;
        let outcome = validate_against_profile(bad, profile, Some(value_sets));
        for expected in [
            "Observation.status must equal the profile's fixed value",
            "Observation.code has no code from the required ValueSet",
            "Observation.category must contain the profile's pattern",
            "Observation.value[x] must be of type Quantity",
            "Observation.note allows at most 0, found 1",
        ] {
            assert!(outcome.contains(expected), "{expected}: {outcome}");
        }
        let missing = validate_against_profile(r#"{"resourceType":"Observation","status":"final","code":{"text":"x"}}"#, profile, None);
        assert!(missing.contains("Observation.category needs at least 1, found 0"), "{missing}");
        assert!(missing.contains("was not given"), "{missing}");
        let wrong = validate_against_profile(r#"{"resourceType":"Patient"}"#, profile, None);
        assert!(wrong.contains("constrains Observation, not Patient"), "{wrong}");
    }
}
