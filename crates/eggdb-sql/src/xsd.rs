//! Validation of an XML document against an XML Schema (D1070), for the part of XSD that
//! ISO 20022 message schemas are written in: global and local elements with `minOccurs` and
//! `maxOccurs`, named and anonymous complex types with `sequence`, `choice` and `all`,
//! `simpleContent` extensions with attributes, `complexContent` extensions, attributes with
//! `use="required"`, and simple types restricted by `enumeration`, `pattern`, `length`,
//! `minLength`, `maxLength`, `totalDigits`, `fractionDigits` and the inclusive and exclusive
//! bounds, over the built-in types' lexical forms. Lists, unions, wildcards' contents,
//! identity constraints and imports are not checked.

use crate::error::{SqlError, SqlResult, SqlState};
use crate::regex::{Options, Regex, StepBudget};
use crate::xml_path::Document;

const MOST_ISSUES: usize = 1_000;
const MOST_DEPTH: usize = 128;
const PATTERN_STEPS: u64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeRef {
    Builtin(&'static str),
    Defined(usize),
}

#[derive(Debug, Clone, Default)]
struct Facets {
    enumeration: Vec<String>,
    patterns: Vec<String>,
    length: Option<usize>,
    min_length: Option<usize>,
    max_length: Option<usize>,
    total_digits: Option<usize>,
    fraction_digits: Option<usize>,
    min_inclusive: Option<f64>,
    max_inclusive: Option<f64>,
    min_exclusive: Option<f64>,
    max_exclusive: Option<f64>,
}

#[derive(Debug, Clone)]
struct ElementDecl {
    name: String,
    kind: Option<TypeRef>,
    min: usize,
    max: Option<usize>,
}

#[derive(Debug, Clone)]
enum Particle {
    Element(ElementDecl),
    Sequence(Vec<Particle>, usize, Option<usize>),
    Choice(Vec<Particle>, usize, Option<usize>),
    All(Vec<Particle>),
    Any(usize, Option<usize>),
}

#[derive(Debug, Clone)]
struct Attribute {
    name: String,
    kind: Option<TypeRef>,
    required: bool,
}

#[derive(Debug, Clone)]
enum TypeDef {
    Simple { base: Option<TypeRef>, facets: Facets },
    Complex { content: Option<Particle>, attributes: Vec<Attribute>, text: Option<TypeRef>, mixed: bool },
}

struct Schema {
    elements: Vec<ElementDecl>,
    types: Vec<(Option<String>, TypeDef)>,
}

const BUILTINS: [&str; 30] = [
    "string", "normalizedString", "token", "language", "Name", "NCName", "ID", "IDREF", "NMTOKEN", "anyURI", "QName",
    "decimal", "integer", "long", "int", "short", "byte", "nonNegativeInteger", "positiveInteger", "nonPositiveInteger",
    "negativeInteger", "unsignedLong", "unsignedInt", "boolean", "date", "dateTime", "time", "gYear", "gYearMonth",
    "base64Binary",
];

fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn occurs(document: &Document, id: usize, name: &str, default: usize) -> Option<usize> {
    match document.attribute(id, name) {
        None => Some(default),
        Some("unbounded") => None,
        Some(written) => written.trim().parse().ok().or(Some(default)),
    }
}

struct SchemaReader<'d> {
    document: &'d Document,
    types: Vec<(Option<String>, TypeDef)>,
    pending: Vec<(usize, String)>,
}

impl SchemaReader<'_> {
    fn named(&mut self, name: &str) -> TypeRef {
        let name = local(name);
        if let Some(builtin) = BUILTINS.iter().find(|builtin| **builtin == name) {
            return TypeRef::Builtin(builtin);
        }
        if name == "anySimpleType" || name == "anyType" {
            return TypeRef::Builtin("string");
        }
        if let Some(at) = self.types.iter().position(|(known, _)| known.as_deref() == Some(name)) {
            return TypeRef::Defined(at);
        }
        self.types.push((Some(name.to_owned()), TypeDef::Simple { base: Some(TypeRef::Builtin("string")), facets: Facets::default() }));
        let at = self.types.len() - 1;
        self.pending.push((at, name.to_owned()));
        TypeRef::Defined(at)
    }

    fn children(&self, id: usize) -> Vec<(usize, &str)> {
        self.document.child_elements(id).into_iter().filter_map(|child| Some((child, self.document.element_name(child)?))).collect()
    }

    fn inline_type(&mut self, id: usize) -> SqlResult<Option<TypeRef>> {
        for (child, name) in self.children(id) {
            match name {
                "complexType" => return Ok(Some(self.complex(child, None)?)),
                "simpleType" => return Ok(Some(self.simple(child, None)?)),
                _ => {}
            }
        }
        Ok(None)
    }

    fn element(&mut self, id: usize, depth: usize) -> SqlResult<ElementDecl> {
        if depth > MOST_DEPTH {
            return Err(invalid("the schema nests too deeply"));
        }
        let name = self.document.attribute(id, "name").or_else(|| self.document.attribute(id, "ref").map(local)).unwrap_or_default().to_owned();
        let kind = match self.document.attribute(id, "type") {
            Some(written) => Some(self.named(written)),
            None => self.inline_type(id)?,
        };
        Ok(ElementDecl { name, kind, min: occurs(self.document, id, "minOccurs", 1).unwrap_or(1), max: occurs(self.document, id, "maxOccurs", 1) })
    }

    fn particle(&mut self, id: usize, name: &str, depth: usize) -> SqlResult<Option<Particle>> {
        if depth > MOST_DEPTH {
            return Err(invalid("the schema nests too deeply"));
        }
        let min = occurs(self.document, id, "minOccurs", 1).unwrap_or(1);
        let max = occurs(self.document, id, "maxOccurs", 1);
        Ok(Some(match name {
            "element" => Particle::Element(self.element(id, depth + 1)?),
            "any" => Particle::Any(min, max),
            "sequence" | "choice" | "all" => {
                let mut items = Vec::new();
                for (child, child_name) in self.children(id) {
                    if let Some(item) = self.particle(child, child_name, depth + 1)? {
                        items.push(item);
                    }
                }
                match name {
                    "sequence" => Particle::Sequence(items, min, max),
                    "choice" => Particle::Choice(items, min, max),
                    _ => Particle::All(items),
                }
            }
            _ => return Ok(None),
        }))
    }

    fn attribute(&mut self, id: usize) -> SqlResult<Attribute> {
        let kind = match self.document.attribute(id, "type") {
            Some(written) => Some(self.named(written)),
            None => self.inline_type(id)?,
        };
        Ok(Attribute {
            name: self.document.attribute(id, "name").or_else(|| self.document.attribute(id, "ref").map(local)).unwrap_or_default().to_owned(),
            kind,
            required: self.document.attribute(id, "use") == Some("required"),
        })
    }

    fn store(&mut self, name: Option<&str>, definition: TypeDef) -> TypeRef {
        if let Some(name) = name
            && let Some(at) = self.types.iter().position(|(known, _)| known.as_deref() == Some(name))
        {
            self.types[at].1 = definition;
            self.pending.retain(|(pending, _)| *pending != at);
            return TypeRef::Defined(at);
        }
        self.types.push((name.map(str::to_owned), definition));
        TypeRef::Defined(self.types.len() - 1)
    }

    fn complex(&mut self, id: usize, name: Option<&str>) -> SqlResult<TypeRef> {
        let mut content = None;
        let mut attributes = Vec::new();
        let mut text = None;
        let mixed = self.document.attribute(id, "mixed") == Some("true");
        for (child, child_name) in self.children(id) {
            match child_name {
                "sequence" | "choice" | "all" => content = self.particle(child, child_name, 0)?,
                "attribute" => attributes.push(self.attribute(child)?),
                "simpleContent" | "complexContent" => {
                    for (derivation, how) in self.children(child) {
                        if !matches!(how, "extension" | "restriction") {
                            continue;
                        }
                        let base = self.document.attribute(derivation, "base").map(|base| self.named(base));
                        if child_name == "simpleContent" {
                            text = base;
                        } else if how == "extension" {
                            content = base.map(|base| Particle::Sequence(vec![Particle::Element(ElementDecl { name: String::new(), kind: Some(base), min: 0, max: Some(0) })], 1, Some(1)));
                        }
                        for (inner, inner_name) in self.children(derivation) {
                            match inner_name {
                                "attribute" => attributes.push(self.attribute(inner)?),
                                "sequence" | "choice" | "all" => {
                                    let added = self.particle(inner, inner_name, 0)?;
                                    content = match (content.take(), added) {
                                        (Some(Particle::Sequence(mut items, 1, Some(1))), Some(added)) => {
                                            items.push(added);
                                            Some(Particle::Sequence(items, 1, Some(1)))
                                        }
                                        (_, added) => added,
                                    };
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(self.store(name, TypeDef::Complex { content, attributes, text, mixed }))
    }

    fn simple(&mut self, id: usize, name: Option<&str>) -> SqlResult<TypeRef> {
        let mut base = Some(TypeRef::Builtin("string"));
        let mut facets = Facets::default();
        for (child, child_name) in self.children(id) {
            if child_name != "restriction" {
                continue;
            }
            base = match self.document.attribute(child, "base") {
                Some(written) => Some(self.named(written)),
                None => self.inline_type(child)?,
            };
            for (facet, facet_name) in self.children(child) {
                let value = self.document.attribute(facet, "value").unwrap_or_default().to_owned();
                let whole = || value.trim().parse::<usize>().ok();
                let number = || value.trim().parse::<f64>().ok();
                match facet_name {
                    "enumeration" => facets.enumeration.push(value.clone()),
                    "pattern" => facets.patterns.push(value.clone()),
                    "length" => facets.length = whole(),
                    "minLength" => facets.min_length = whole(),
                    "maxLength" => facets.max_length = whole(),
                    "totalDigits" => facets.total_digits = whole(),
                    "fractionDigits" => facets.fraction_digits = whole(),
                    "minInclusive" => facets.min_inclusive = number(),
                    "maxInclusive" => facets.max_inclusive = number(),
                    "minExclusive" => facets.min_exclusive = number(),
                    "maxExclusive" => facets.max_exclusive = number(),
                    _ => {}
                }
            }
        }
        Ok(self.store(name, TypeDef::Simple { base, facets }))
    }
}

fn invalid(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, format!("the XML Schema cannot be read: {}", message.into()))
}

fn read_schema(text: &str) -> SqlResult<Schema> {
    let document = Document::parse(text, true)?;
    let root = document.child_elements(0).into_iter().next().ok_or_else(|| invalid("it has no root element"))?;
    if document.element_name(root) != Some("schema") {
        return Err(invalid("its root element is not xs:schema"));
    }
    let mut reader = SchemaReader { document: &document, types: Vec::new(), pending: Vec::new() };
    let mut elements = Vec::new();
    for (child, name) in reader.children(root) {
        let type_name = document.attribute(child, "name");
        match name {
            "element" => elements.push(reader.element(child, 0)?),
            "complexType" => {
                reader.complex(child, type_name)?;
            }
            "simpleType" => {
                reader.simple(child, type_name)?;
            }
            _ => {}
        }
    }
    if let Some((_, missing)) = reader.pending.first() {
        return Err(invalid(format!("the type {missing} is used but not defined")));
    }
    Ok(Schema { elements, types: reader.types })
}

fn digits_of(text: &str) -> (usize, usize) {
    let unsigned = text.trim_start_matches(['+', '-']);
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let fraction = fraction.trim_end_matches('0');
    let whole = whole.trim_start_matches('0');
    (whole.len() + fraction.len(), fraction.len())
}

fn all_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

fn zone_ok(rest: &str) -> bool {
    rest.is_empty() || rest == "Z" || (rest.len() == 6 && matches!(rest.as_bytes()[0], b'+' | b'-') && all_digits(&rest[1..3]) && &rest[3..4] == ":" && all_digits(&rest[4..6]))
}

fn date_ok(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    (bytes.len() >= 10 && all_digits(text.get(..4)?) && bytes[4] == b'-' && all_digits(text.get(5..7)?) && bytes[7] == b'-' && all_digits(text.get(8..10)?))
        .then(|| text.get(10..))
        .flatten()
        .filter(|_| (1..=12).contains(&text[5..7].parse::<u32>().unwrap_or(0)) && (1..=31).contains(&text[8..10].parse::<u32>().unwrap_or(0)))
}

fn time_ok(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    if !(bytes.len() >= 8 && all_digits(text.get(..2)?) && bytes[2] == b':' && all_digits(text.get(3..5)?) && bytes[5] == b':' && all_digits(text.get(6..8)?)) {
        return None;
    }
    let rest = text.get(8..)?;
    match rest.strip_prefix('.') {
        Some(fraction) => {
            let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
            (digits > 0).then(|| fraction.get(digits..)).flatten()
        }
        None => Some(rest),
    }
}

fn lexical_ok(builtin: &str, text: &str) -> bool {
    let decimal = |text: &str| {
        let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
        let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
        (!whole.is_empty() || !fraction.is_empty()) && whole.bytes().all(|byte| byte.is_ascii_digit()) && fraction.bytes().all(|byte| byte.is_ascii_digit())
    };
    let integer = |text: &str| all_digits(text.strip_prefix(['+', '-']).unwrap_or(text));
    match builtin {
        "decimal" => decimal(text),
        "integer" | "long" | "int" | "short" | "byte" => integer(text),
        "nonNegativeInteger" | "unsignedLong" | "unsignedInt" => integer(text) && !text.starts_with('-'),
        "positiveInteger" => integer(text) && !text.starts_with('-') && text.trim_start_matches(['+', '0']).bytes().any(|byte| byte != b'0'),
        "nonPositiveInteger" => integer(text) && (text.starts_with('-') || text.trim_start_matches(['+', '0']).is_empty()),
        "negativeInteger" => integer(text) && text.starts_with('-'),
        "boolean" => matches!(text, "true" | "false" | "1" | "0"),
        "date" => date_ok(text).is_some_and(zone_ok),
        "dateTime" => date_ok(text).and_then(|rest| rest.strip_prefix('T')).and_then(time_ok).is_some_and(zone_ok),
        "time" => time_ok(text).is_some_and(zone_ok),
        "gYear" => text.len() >= 4 && all_digits(&text[..4]) && zone_ok(&text[4..]),
        "gYearMonth" => text.len() >= 7 && all_digits(&text[..4]) && &text[4..5] == "-" && all_digits(&text[5..7]) && zone_ok(&text[7..]),
        "base64Binary" => text.bytes().filter(|byte| !byte.is_ascii_whitespace()).all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=')),
        _ => true,
    }
}

fn is_numeric(builtin: &str) -> bool {
    !matches!(
        builtin,
        "string" | "normalizedString" | "token" | "language" | "Name" | "NCName" | "ID" | "IDREF" | "NMTOKEN" | "anyURI" | "QName" | "boolean" | "date" | "dateTime" | "time" | "gYear" | "gYearMonth" | "base64Binary"
    )
}

struct Validator<'s> {
    schema: &'s Schema,
    document: &'s Document,
    issues: Vec<(String, String)>,
    budget: StepBudget,
}

impl Validator<'_> {
    fn issue(&mut self, path: &str, message: impl Into<String>) {
        if self.issues.len() < MOST_ISSUES {
            self.issues.push((path.to_owned(), message.into()));
        }
    }

    fn simple_value(&mut self, kind: TypeRef, raw: &str, path: &str) -> SqlResult<()> {
        let mut chain = Vec::new();
        let mut current = Some(kind);
        while let Some(TypeRef::Defined(at)) = current {
            if chain.len() > MOST_DEPTH {
                break;
            }
            match &self.schema.types[at].1 {
                TypeDef::Simple { base, facets } => {
                    chain.push((self.schema.types[at].0.clone(), facets.clone()));
                    current = *base;
                }
                TypeDef::Complex { text, .. } => current = *text,
            }
        }
        let builtin = match current {
            Some(TypeRef::Builtin(name)) => name,
            _ => "string",
        };
        let text = if matches!(builtin, "string") { raw } else { raw.trim() };
        if !lexical_ok(builtin, text) {
            self.issue(path, format!("{text:?} is not a valid {builtin}"));
            return Ok(());
        }
        for (name, facets) in chain {
            let shown = name.unwrap_or_else(|| builtin.to_owned());
            if !facets.enumeration.is_empty() && !facets.enumeration.iter().any(|allowed| allowed == text) {
                self.issue(path, format!("{text:?} is not one of the values {shown} allows"));
            }
            for pattern in &facets.patterns {
                let regex = Regex::compile_with(&format!("^(?:{pattern})$"), Options::default())?;
                if !regex.is_match(text, &mut self.budget)? {
                    self.issue(path, format!("{text:?} does not match {shown}'s pattern {pattern}"));
                }
            }
            let length = text.chars().count();
            if facets.length.is_some_and(|wanted| wanted != length)
                || facets.min_length.is_some_and(|least| length < least)
                || facets.max_length.is_some_and(|most| length > most)
            {
                self.issue(path, format!("{text:?} has a length {shown} does not allow"));
            }
            if is_numeric(builtin) {
                let (total, fraction) = digits_of(text);
                if facets.total_digits.is_some_and(|most| total > most) || facets.fraction_digits.is_some_and(|most| fraction > most) {
                    self.issue(path, format!("{text} has more digits than {shown} allows"));
                }
                if let Ok(number) = text.parse::<f64>()
                    && (facets.min_inclusive.is_some_and(|least| number < least)
                        || facets.max_inclusive.is_some_and(|most| number > most)
                        || facets.min_exclusive.is_some_and(|least| number <= least)
                        || facets.max_exclusive.is_some_and(|most| number >= most))
                {
                    self.issue(path, format!("{text} is outside the range {shown} allows"));
                }
            }
        }
        Ok(())
    }

    fn first_names(particle: &Particle, out: &mut Vec<String>, depth: usize) {
        if depth > MOST_DEPTH {
            return;
        }
        match particle {
            Particle::Element(decl) => out.push(decl.name.clone()),
            Particle::Choice(items, ..) | Particle::All(items) => items.iter().for_each(|item| Self::first_names(item, out, depth + 1)),
            Particle::Sequence(items, ..) => {
                for item in items {
                    Self::first_names(item, out, depth + 1);
                    if Self::required(item) {
                        break;
                    }
                }
            }
            Particle::Any(..) => out.push("*".to_owned()),
        }
    }

    fn required(particle: &Particle) -> bool {
        match particle {
            Particle::Element(decl) => decl.min > 0 && !decl.name.is_empty(),
            Particle::Sequence(items, min, _) => *min > 0 && items.iter().any(Self::required),
            Particle::Choice(items, min, _) => *min > 0 && items.iter().all(Self::required),
            Particle::All(items) => items.iter().any(Self::required),
            Particle::Any(min, _) => *min > 0,
        }
    }

    fn starts(&self, particle: &Particle, child: usize) -> bool {
        let mut names = Vec::new();
        Self::first_names(particle, &mut names, 0);
        let name = self.document.element_name(child).unwrap_or_default();
        names.iter().any(|known| known == name || known == "*")
    }

    fn content(&mut self, particle: &Particle, children: &[usize], at: &mut usize, path: &str, depth: usize) -> SqlResult<()> {
        if depth > MOST_DEPTH {
            return Ok(());
        }
        match particle {
            Particle::Element(decl) if decl.name.is_empty() => {
                if let Some(TypeRef::Defined(base)) = decl.kind
                    && let TypeDef::Complex { content: Some(inherited), .. } = &self.schema.types[base].1
                {
                    let inherited = inherited.clone();
                    self.content(&inherited, children, at, path, depth + 1)?;
                }
            }
            Particle::Element(decl) => {
                let mut count = 0;
                while decl.max.is_none_or(|most| count < most)
                    && let Some(&child) = children.get(*at)
                    && self.document.element_name(child) == Some(decl.name.as_str())
                {
                    let child_path = format!("{path}/{}[{}]", decl.name, count + 1);
                    self.element(child, decl.kind, &child_path, depth + 1)?;
                    *at += 1;
                    count += 1;
                }
                if count < decl.min {
                    self.issue(path, format!("{} must appear at least {} time(s), found {count}", decl.name, decl.min));
                }
            }
            Particle::Sequence(items, min, max) => {
                let mut occurrences = 0;
                while max.is_none_or(|most| occurrences < most) {
                    let first_time = occurrences == 0;
                    let begins = children.get(*at).is_some_and(|child| self.starts(particle, *child));
                    if !first_time && !begins {
                        break;
                    }
                    if first_time && *min == 0 && !begins {
                        break;
                    }
                    let before = *at;
                    for item in items {
                        self.content(item, children, at, path, depth + 1)?;
                    }
                    occurrences += 1;
                    if *at == before {
                        break;
                    }
                }
            }
            Particle::Choice(items, min, max) => {
                let mut occurrences = 0;
                while max.is_none_or(|most| occurrences < most) {
                    let Some(&child) = children.get(*at) else { break };
                    let Some(item) = items.iter().find(|item| self.starts(item, child)) else { break };
                    let before = *at;
                    self.content(item, children, at, path, depth + 1)?;
                    occurrences += 1;
                    if *at == before {
                        break;
                    }
                }
                if occurrences < *min && !items.iter().any(|item| !Self::required(item)) {
                    let mut names = Vec::new();
                    items.iter().for_each(|item| Self::first_names(item, &mut names, 0));
                    self.issue(path, format!("one of {} must appear here", names.join(", ")));
                }
            }
            Particle::All(items) => {
                let mut seen = vec![false; items.len()];
                while let Some(&child) = children.get(*at) {
                    let Some(index) = items.iter().position(|item| self.starts(item, child)) else { break };
                    if seen[index] {
                        break;
                    }
                    seen[index] = true;
                    self.content(&items[index], children, at, path, depth + 1)?;
                }
                for (index, item) in items.iter().enumerate() {
                    if !seen[index] && Self::required(item) {
                        let mut names = Vec::new();
                        Self::first_names(item, &mut names, 0);
                        self.issue(path, format!("{} must appear", names.join(", ")));
                    }
                }
            }
            Particle::Any(min, max) => {
                let mut count = 0;
                while max.is_none_or(|most| count < most) && *at < children.len() {
                    *at += 1;
                    count += 1;
                }
                if count < *min {
                    self.issue(path, "more elements must appear here");
                }
            }
        }
        Ok(())
    }

    fn element(&mut self, id: usize, kind: Option<TypeRef>, path: &str, depth: usize) -> SqlResult<()> {
        if depth > MOST_DEPTH {
            self.issue(path, "the document nests too deeply");
            return Ok(());
        }
        let definition = match kind {
            None => return Ok(()),
            Some(TypeRef::Builtin(_)) => None,
            Some(TypeRef::Defined(at)) => Some(self.schema.types[at].1.clone()),
        };
        let children = self.document.child_elements(id);
        match definition {
            None | Some(TypeDef::Simple { .. }) => {
                if !children.is_empty() {
                    self.issue(path, "a simple value cannot hold elements");
                }
                let text = self.document.text_of(id);
                if let Some(kind) = kind {
                    self.simple_value(kind, &text, path)?;
                }
            }
            Some(TypeDef::Complex { content, attributes, text, mixed }) => {
                for attribute in &attributes {
                    let value = self.document.attributes_of(id).iter().find(|(name, _)| local(name) == attribute.name).map(|(_, value)| value.clone());
                    match (value, attribute.kind) {
                        (None, _) if attribute.required => self.issue(path, format!("the attribute {} is required", attribute.name)),
                        (Some(value), Some(kind)) => self.simple_value(kind, &value, &format!("{path}/@{}", attribute.name))?,
                        _ => {}
                    }
                }
                for (name, _) in self.document.attributes_of(id) {
                    let name = local(name);
                    let declared = attributes.iter().any(|attribute| attribute.name == name);
                    if !declared && !name.starts_with("xmlns") && !matches!(name, "schemaLocation" | "noNamespaceSchemaLocation") {
                        self.issue(path, format!("the attribute {name} is not allowed"));
                    }
                }
                if let Some(text_kind) = text {
                    if !children.is_empty() {
                        self.issue(path, "this element holds a value, not elements");
                    }
                    let value = self.document.text_of(id);
                    self.simple_value(text_kind, &value, path)?;
                    return Ok(());
                }
                if !mixed && self.document.has_direct_text(id) {
                    self.issue(path, "text is not allowed directly in this element");
                }
                let mut at = 0;
                if let Some(content) = &content {
                    self.content(content, &children, &mut at, path, depth + 1)?;
                }
                for &extra in children.get(at..).unwrap_or_default() {
                    self.issue(path, format!("the element {} is not allowed here", self.document.element_name(extra).unwrap_or_default()));
                }
            }
        }
        Ok(())
    }
}

/// The problems of the XML document `message` against the XML Schema `schema`, each as
/// (path, message); empty when it is valid.
pub(crate) fn validate(message: &str, schema: &str) -> SqlResult<Vec<(String, String)>> {
    let schema = read_schema(schema)?;
    let document = Document::parse(message, true)?;
    let mut validator = Validator { schema: &schema, document: &document, issues: Vec::new(), budget: StepBudget::new(PATTERN_STEPS) };
    let Some(root) = document.child_elements(0).into_iter().next() else {
        return Ok(vec![(String::new(), "the document has no root element".to_owned())]);
    };
    let name = document.element_name(root).unwrap_or_default().to_owned();
    match schema.elements.iter().find(|decl| decl.name == name) {
        Some(decl) => validator.element(root, decl.kind, &format!("/{name}"), 0)?,
        None => validator.issue(&format!("/{name}"), format!("the schema declares no global element {name}")),
    }
    Ok(validator.issues)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns="urn:iso:std:iso:20022:tech:xsd:pain.001.001.09" targetNamespace="urn:iso:std:iso:20022:tech:xsd:pain.001.001.09" elementFormDefault="qualified">
      <xs:element name="Document" type="Document"/>
      <xs:complexType name="Document"><xs:sequence><xs:element name="GrpHdr" type="GroupHeader"/></xs:sequence></xs:complexType>
      <xs:complexType name="GroupHeader"><xs:sequence>
        <xs:element name="MsgId" type="Max35Text"/>
        <xs:element name="CreDtTm" type="ISODateTime"/>
        <xs:element maxOccurs="2" minOccurs="0" name="Authstn" type="Max128Text"/>
        <xs:element name="NbOfTxs" type="Max15NumericText"/>
        <xs:element minOccurs="0" name="CtrlSum" type="DecimalNumber"/>
        <xs:element name="Amt" type="ActiveCurrencyAndAmount"/>
        <xs:choice><xs:element name="Cd" type="Code"/><xs:element name="Prtry" type="Max35Text"/></xs:choice>
      </xs:sequence></xs:complexType>
      <xs:simpleType name="Max35Text"><xs:restriction base="xs:string"><xs:minLength value="1"/><xs:maxLength value="35"/></xs:restriction></xs:simpleType>
      <xs:simpleType name="Max128Text"><xs:restriction base="xs:string"><xs:minLength value="1"/><xs:maxLength value="128"/></xs:restriction></xs:simpleType>
      <xs:simpleType name="Max15NumericText"><xs:restriction base="xs:string"><xs:pattern value="[0-9]{1,15}"/></xs:restriction></xs:simpleType>
      <xs:simpleType name="ISODateTime"><xs:restriction base="xs:dateTime"/></xs:simpleType>
      <xs:simpleType name="DecimalNumber"><xs:restriction base="xs:decimal"><xs:fractionDigits value="17"/><xs:totalDigits value="18"/></xs:restriction></xs:simpleType>
      <xs:simpleType name="Code"><xs:restriction base="xs:string"><xs:enumeration value="CASH"/><xs:enumeration value="CORT"/></xs:restriction></xs:simpleType>
      <xs:complexType name="ActiveCurrencyAndAmount"><xs:simpleContent><xs:extension base="ActiveCurrencyAndAmount_SimpleType"><xs:attribute name="Ccy" type="ActiveCurrencyCode" use="required"/></xs:extension></xs:simpleContent></xs:complexType>
      <xs:simpleType name="ActiveCurrencyAndAmount_SimpleType"><xs:restriction base="xs:decimal"><xs:fractionDigits value="5"/><xs:totalDigits value="18"/><xs:minInclusive value="0"/></xs:restriction></xs:simpleType>
      <xs:simpleType name="ActiveCurrencyCode"><xs:restriction base="xs:string"><xs:pattern value="[A-Z]{3,3}"/></xs:restriction></xs:simpleType>
    </xs:schema>"#;

    fn document(header: &str) -> String {
        format!(r#"<Document xmlns="urn:iso:std:iso:20022:tech:xsd:pain.001.001.09"><GrpHdr>{header}</GrpHdr></Document>"#)
    }

    #[test]
    fn a_valid_message_has_no_issues() {
        let message = document("<MsgId>M-1</MsgId><CreDtTm>2026-11-13T10:00:00Z</CreDtTm><NbOfTxs>2</NbOfTxs><CtrlSum>10.50</CtrlSum><Amt Ccy=\"EUR\">10.5</Amt><Cd>CASH</Cd>");
        assert_eq!(validate(&message, SCHEMA).unwrap(), Vec::<(String, String)>::new());
    }

    #[test]
    fn content_values_attributes_and_choices_are_checked() {
        let message = document("<MsgId></MsgId><CreDtTm>2026-13-13T10:00:00Z</CreDtTm><Authstn>a</Authstn><NbOfTxs>two</NbOfTxs><Amt Ccy=\"eur\">-1</Amt><Extra/>");
        let issues: Vec<String> = validate(&message, SCHEMA).unwrap().into_iter().map(|(path, message)| format!("{path}: {message}")).collect();
        let all = issues.join("\n");
        for expected in [
            "/Document/GrpHdr/MsgId[1]: \"\" has a length Max35Text does not allow",
            "/Document/GrpHdr/CreDtTm[1]: \"2026-13-13T10:00:00Z\" is not a valid dateTime",
            "/Document/GrpHdr/NbOfTxs[1]: \"two\" does not match Max15NumericText's pattern",
            "/Document/GrpHdr/Amt[1]/@Ccy: \"eur\" does not match ActiveCurrencyCode's pattern",
            "/Document/GrpHdr/Amt[1]: -1 is outside the range",
            "/Document/GrpHdr: one of Cd, Prtry must appear here",
        ] {
            assert!(all.contains(expected), "{expected}\n{all}");
        }
        assert!(all.contains("is not allowed here"), "{all}");
        assert!(read_schema("<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"><xs:element name=\"A\" type=\"Missing\"/></xs:schema>").is_err());
    }

    #[test]
    fn occurrences_past_the_maximum_are_reported() {
        let message = document("<MsgId>M</MsgId><CreDtTm>2026-11-13T10:00:00Z</CreDtTm><Authstn>a</Authstn><Authstn>b</Authstn><Authstn>c</Authstn><NbOfTxs>1</NbOfTxs><Amt Ccy=\"EUR\">1</Amt><Prtry>X</Prtry>");
        let all: Vec<String> = validate(&message, SCHEMA).unwrap().into_iter().map(|(path, message)| format!("{path}: {message}")).collect();
        assert!(all.iter().any(|issue| issue.contains("NbOfTxs must appear at least 1 time(s), found 0")), "{all:?}");
    }
}
