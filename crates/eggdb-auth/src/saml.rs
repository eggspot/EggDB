//! SAML 2.0 sign-in for interactive tools (D1188), written from the SAML 2.0 core and bindings
//! specifications, XML Signature 1.1 and Exclusive XML Canonicalization 1.0. EggDB is the
//! service provider: [`SamlConfig::sign_in_url`] sends the browser to the identity provider with
//! an `AuthnRequest` (HTTP-Redirect binding), the provider posts a signed `Response` back
//! (HTTP-POST binding), and [`SamlConfig::accept_response`] checks it: the XML signature against
//! the provider's configured certificate (RSA or ECDSA with SHA-256 over exclusive
//! canonicalization, never the key the message carries), that the signed element is the very
//! assertion read (no signature wrapping), the issuer, audience, recipient and validity times.
//! The user then gets a one-time ticket to use as the password of a `saml` login rule.

use eggdb_tls::signature::{SignatureAlgorithm, SignatureHash, verify_signature};
use eggdb_tls::x509::Certificate;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

const MOST_XML_BYTES: usize = 1 << 20;
const MOST_DEPTH: usize = 64;
const MOST_NODES: usize = 20_000;
const CLOCK_SKEW_SECONDS: i64 = 180;
const TICKET_SECONDS: i64 = 300;
const MOST_TICKETS: usize = 10_000;
const DSIG: &str = "http://www.w3.org/2000/09/xmldsig#";
const SAML_ASSERTION: &str = "urn:oasis:names:tc:SAML:2.0:assertion";
const SAML_PROTOCOL: &str = "urn:oasis:names:tc:SAML:2.0:protocol";
const EXC_C14N: &str = "http://www.w3.org/2001/10/xml-exc-c14n#";
const ENVELOPED: &str = "http://www.w3.org/2000/09/xmldsig#enveloped-signature";
const SUCCESS: &str = "urn:oasis:names:tc:SAML:2.0:status:Success";
const BEARER: &str = "urn:oasis:names:tc:SAML:2.0:cm:bearer";

/// Why a SAML sign-in failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamlError(pub String);

impl std::fmt::Display for SamlError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SamlError {}

fn failed(message: impl Into<String>) -> SamlError {
    SamlError(message.into())
}

#[derive(Debug, Clone)]
enum Content {
    Element(usize),
    Text(String),
}

#[derive(Debug, Clone)]
struct Element {
    prefix: String,
    local: String,
    namespace: String,
    attributes: Vec<Attribute>,
    in_scope: Vec<(String, String)>,
    children: Vec<Content>,
    parent: Option<usize>,
}

#[derive(Debug, Clone)]
struct Attribute {
    prefix: String,
    local: String,
    namespace: String,
    value: String,
}

#[derive(Debug, Clone)]
struct Document {
    elements: Vec<Element>,
}

struct Reader<'t> {
    text: &'t str,
    at: usize,
}

fn decoded(raw: &str, attribute: bool) -> Result<String, SamlError> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find(['&', '\t', '\n', '\r']) {
        out.push_str(&rest[..start]);
        let character = rest[start..].chars().next().unwrap_or(' ');
        if character != '&' {
            out.push(if attribute { ' ' } else { character });
            rest = &rest[start + 1..];
            continue;
        }
        let end = rest[start..].find(';').ok_or_else(|| failed("an entity reference has no ';'"))? + start;
        let name = &rest[start + 1..end];
        let value = match name {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let number = if let Some(hex) = name.strip_prefix("#x") {
                    u32::from_str_radix(hex, 16).ok()
                } else if let Some(decimal) = name.strip_prefix('#') {
                    decimal.parse::<u32>().ok()
                } else {
                    return Err(failed(format!("the entity &{name}; is not one XML predefines")));
                };
                number.and_then(char::from_u32).ok_or_else(|| failed("a character reference is not a character"))?
            }
        };
        out.push(value);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn split_name(name: &str) -> (String, String) {
    match name.split_once(':') {
        Some((prefix, local)) => (prefix.to_owned(), local.to_owned()),
        None => (String::new(), name.to_owned()),
    }
}

fn is_name_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, ':' | '_' | '-' | '.')
}

impl Reader<'_> {
    fn rest(&self) -> &str {
        &self.text[self.at..]
    }

    fn skip_space(&mut self) {
        let trimmed = self.rest().trim_start_matches([' ', '\t', '\r', '\n']);
        self.at = self.text.len() - trimmed.len();
    }

    fn name(&mut self) -> Result<String, SamlError> {
        let length = self.rest().find(|character: char| !is_name_character(character)).unwrap_or(self.rest().len());
        if length == 0 {
            return Err(failed("an XML name is missing"));
        }
        let name = self.rest()[..length].to_owned();
        self.at += length;
        Ok(name)
    }

    fn expect(&mut self, wanted: &str) -> Result<(), SamlError> {
        if !self.rest().starts_with(wanted) {
            return Err(failed(format!("the XML lacks '{wanted}' where it belongs")));
        }
        self.at += wanted.len();
        Ok(())
    }

    fn skip_past(&mut self, end: &str) -> Result<(), SamlError> {
        let found = self.rest().find(end).ok_or_else(|| failed(format!("the XML never closes with '{end}'")))?;
        self.at += found + end.len();
        Ok(())
    }
}

impl Document {
    fn parse(text: &str) -> Result<Self, SamlError> {
        if text.len() > MOST_XML_BYTES {
            return Err(failed("the SAML message is larger than 1 MiB"));
        }
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let mut reader = Reader { text: &normalized, at: 0 };
        let mut document = Document { elements: Vec::new() };
        let mut stack: Vec<usize> = Vec::new();
        let mut root = None;
        loop {
            if reader.at >= normalized.len() {
                break;
            }
            if reader.rest().starts_with("<?") {
                reader.skip_past("?>")?;
            } else if reader.rest().starts_with("<!--") {
                reader.skip_past("-->")?;
            } else if reader.rest().starts_with("<!DOCTYPE") || reader.rest().starts_with("<!ENTITY") {
                return Err(failed("SAML messages may not carry a DOCTYPE"));
            } else if reader.rest().starts_with("<![CDATA[") {
                reader.at += 9;
                let end = reader.rest().find("]]>").ok_or_else(|| failed("a CDATA section never closes"))?;
                let text = reader.rest()[..end].to_owned();
                reader.at += end + 3;
                let parent = *stack.last().ok_or_else(|| failed("text outside the root element"))?;
                document.elements[parent].children.push(Content::Text(text));
            } else if reader.rest().starts_with("</") {
                reader.at += 2;
                let name = reader.name()?;
                reader.skip_space();
                reader.expect(">")?;
                let open = stack.pop().ok_or_else(|| failed("a closing tag closes nothing"))?;
                let element = &document.elements[open];
                let written = if element.prefix.is_empty() { element.local.clone() } else { format!("{}:{}", element.prefix, element.local) };
                if written != name {
                    return Err(failed(format!("</{name}> closes <{written}>")));
                }
            } else if reader.rest().starts_with('<') {
                if root.is_some() && stack.is_empty() {
                    return Err(failed("the XML has more than one root element"));
                }
                if stack.len() >= MOST_DEPTH || document.elements.len() >= MOST_NODES {
                    return Err(failed("the SAML message nests too deeply or has too many elements"));
                }
                reader.at += 1;
                let name = reader.name()?;
                let mut raw_attributes: Vec<(String, String)> = Vec::new();
                loop {
                    reader.skip_space();
                    if reader.rest().starts_with("/>") || reader.rest().starts_with('>') {
                        break;
                    }
                    let attribute = reader.name()?;
                    reader.skip_space();
                    reader.expect("=")?;
                    reader.skip_space();
                    let quote = reader.rest().chars().next().filter(|quote| matches!(quote, '"' | '\'')).ok_or_else(|| failed("an attribute value is not quoted"))?;
                    reader.at += 1;
                    let end = reader.rest().find(quote).ok_or_else(|| failed("an attribute value never closes"))?;
                    let value = decoded(&reader.rest()[..end], true)?;
                    reader.at += end + 1;
                    if raw_attributes.iter().any(|(known, _)| *known == attribute) {
                        return Err(failed(format!("the attribute {attribute} appears twice")));
                    }
                    raw_attributes.push((attribute, value));
                }
                let empty = reader.rest().starts_with("/>");
                reader.at += if empty { 2 } else { 1 };
                let parent = stack.last().copied();
                let mut in_scope = parent.map(|parent| document.elements[parent].in_scope.clone()).unwrap_or_else(|| vec![("xml".to_owned(), "http://www.w3.org/XML/1998/namespace".to_owned())]);
                for (attribute, value) in &raw_attributes {
                    let declared = if attribute == "xmlns" { Some(String::new()) } else { attribute.strip_prefix("xmlns:").map(str::to_owned) };
                    if let Some(prefix) = declared {
                        in_scope.retain(|(known, _)| *known != prefix);
                        in_scope.push((prefix, value.clone()));
                    }
                }
                let resolve = |prefix: &str, default_applies: bool| -> Result<String, SamlError> {
                    if prefix.is_empty() && !default_applies {
                        return Ok(String::new());
                    }
                    match in_scope.iter().find(|(known, _)| known == prefix) {
                        Some((_, uri)) => Ok(uri.clone()),
                        None if prefix.is_empty() => Ok(String::new()),
                        None => Err(failed(format!("the prefix {prefix} is not declared"))),
                    }
                };
                let (prefix, local) = split_name(&name);
                let namespace = resolve(&prefix, true)?;
                let mut attributes = Vec::new();
                for (attribute, value) in raw_attributes {
                    if attribute == "xmlns" || attribute.starts_with("xmlns:") {
                        continue;
                    }
                    let (attribute_prefix, attribute_local) = split_name(&attribute);
                    attributes.push(Attribute {
                        namespace: resolve(&attribute_prefix, false)?,
                        prefix: attribute_prefix,
                        local: attribute_local,
                        value,
                    });
                }
                let id = document.elements.len();
                document.elements.push(Element {
                    prefix,
                    local,
                    namespace,
                    attributes,
                    in_scope,
                    children: Vec::new(),
                    parent,
                });
                match parent {
                    Some(parent) => document.elements[parent].children.push(Content::Element(id)),
                    None => root = Some(id),
                }
                if !empty {
                    stack.push(id);
                }
            } else {
                let end = reader.rest().find('<').unwrap_or(reader.rest().len());
                let raw = &reader.rest()[..end];
                match stack.last() {
                    Some(&parent) => {
                        let text = decoded(raw, false)?;
                        document.elements[parent].children.push(Content::Text(text));
                    }
                    None if raw.trim().is_empty() => {}
                    None => return Err(failed("text outside the root element")),
                }
                reader.at += end;
            }
        }
        if !stack.is_empty() || root != Some(0) {
            return Err(failed("the XML is not one complete element"));
        }
        Ok(document)
    }

    fn children(&self, id: usize) -> impl Iterator<Item = usize> + '_ {
        self.elements[id].children.iter().filter_map(|child| match child {
            Content::Element(child) => Some(*child),
            Content::Text(_) => None,
        })
    }

    fn child(&self, id: usize, namespace: &str, local: &str) -> Option<usize> {
        self.children(id).find(|child| self.elements[*child].namespace == namespace && self.elements[*child].local == local)
    }

    fn text(&self, id: usize) -> String {
        self.elements[id]
            .children
            .iter()
            .map(|child| match child {
                Content::Text(text) => text.clone(),
                Content::Element(child) => self.text(*child),
            })
            .collect()
    }

    fn attribute(&self, id: usize, local: &str) -> Option<&str> {
        self.elements[id]
            .attributes
            .iter()
            .find(|attribute| attribute.namespace.is_empty() && attribute.local == local)
            .map(|attribute| attribute.value.as_str())
    }

    fn with_id(&self, wanted: &str) -> Vec<usize> {
        (0..self.elements.len()).filter(|id| self.attribute(*id, "ID") == Some(wanted)).collect()
    }

    fn descendants(&self, id: usize, out: &mut Vec<usize>) {
        for child in self.children(id).collect::<Vec<_>>() {
            out.push(child);
            self.descendants(child, out);
        }
    }
}

fn escaped_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#xD;"),
            other => out.push(other),
        }
    }
    out
}

fn escaped_attribute(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#x9;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            other => out.push(other),
        }
    }
    out
}

struct Canonical<'d> {
    document: &'d Document,
    left_out: Option<usize>,
    inclusive: &'d [String],
    out: String,
}

impl Canonical<'_> {
    fn element(&mut self, id: usize, rendered: &[(String, String)]) {
        let element = &self.document.elements[id];
        let qualified = if element.prefix.is_empty() { element.local.clone() } else { format!("{}:{}", element.prefix, element.local) };
        let mut used: Vec<String> = vec![element.prefix.clone()];
        for attribute in &element.attributes {
            if !attribute.prefix.is_empty() && !used.contains(&attribute.prefix) {
                used.push(attribute.prefix.clone());
            }
        }
        for prefix in self.inclusive {
            let prefix = if prefix == "#default" { String::new() } else { prefix.clone() };
            if !used.contains(&prefix) && element.in_scope.iter().any(|(known, _)| *known == prefix) {
                used.push(prefix);
            }
        }
        let mut declarations: Vec<(String, String)> = Vec::new();
        for prefix in used {
            if prefix == "xml" {
                continue;
            }
            let uri = element.in_scope.iter().find(|(known, _)| *known == prefix).map(|(_, uri)| uri.clone()).unwrap_or_default();
            let already = rendered.iter().find(|(known, _)| *known == prefix).map(|(_, uri)| uri.as_str());
            let needed = match already {
                Some(previous) => previous != uri,
                None => !(prefix.is_empty() && uri.is_empty()),
            };
            if needed {
                declarations.push((prefix, uri));
            }
        }
        declarations.sort();
        let mut attributes: Vec<&Attribute> = element.attributes.iter().collect();
        attributes.sort_by(|left, right| (left.namespace.as_str(), left.local.as_str()).cmp(&(right.namespace.as_str(), right.local.as_str())));
        self.out.push('<');
        self.out.push_str(&qualified);
        for (prefix, uri) in &declarations {
            if prefix.is_empty() {
                self.out.push_str(&format!(" xmlns=\"{}\"", escaped_attribute(uri)));
            } else {
                self.out.push_str(&format!(" xmlns:{prefix}=\"{}\"", escaped_attribute(uri)));
            }
        }
        for attribute in attributes {
            let name = if attribute.prefix.is_empty() { attribute.local.clone() } else { format!("{}:{}", attribute.prefix, attribute.local) };
            self.out.push_str(&format!(" {name}=\"{}\"", escaped_attribute(&attribute.value)));
        }
        self.out.push('>');
        let mut now_rendered = rendered.to_vec();
        for (prefix, uri) in declarations {
            now_rendered.retain(|(known, _)| *known != prefix);
            now_rendered.push((prefix, uri));
        }
        for child in self.document.elements[id].children.clone() {
            match child {
                Content::Text(text) => self.out.push_str(&escaped_text(&text)),
                Content::Element(child) if Some(child) == self.left_out => {}
                Content::Element(child) => self.element(child, &now_rendered),
            }
        }
        self.out.push_str("</");
        self.out.push_str(&qualified);
        self.out.push('>');
    }
}

fn exclusive_canonical(document: &Document, id: usize, left_out: Option<usize>, inclusive: &[String]) -> String {
    let mut canonical = Canonical {
        document,
        left_out,
        inclusive,
        out: String::new(),
    };
    canonical.element(id, &[]);
    canonical.out
}

fn inclusive_prefixes(document: &Document, transform: usize) -> Vec<String> {
    document
        .children(transform)
        .find(|child| document.elements[*child].namespace == EXC_C14N && document.elements[*child].local == "InclusiveNamespaces")
        .and_then(|list| document.attribute(list, "PrefixList").map(|prefixes| prefixes.split_whitespace().map(str::to_owned).collect()))
        .unwrap_or_default()
}

fn base64(text: &str) -> Result<Vec<u8>, SamlError> {
    let compact: String = text.chars().filter(|character| !character.is_whitespace()).collect();
    eggdb_crypto::base64_decode(compact.as_bytes()).map_err(|_| failed("a base64 value is not valid"))
}

fn algorithm_of(uri: &str) -> Result<SignatureAlgorithm, SamlError> {
    match uri {
        "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256" => Ok(SignatureAlgorithm::RsaPkcs1(SignatureHash::Sha256)),
        "http://www.w3.org/2001/04/xmldsig-more#rsa-sha512" => Ok(SignatureAlgorithm::RsaPkcs1(SignatureHash::Sha512)),
        "http://www.w3.org/2001/04/xmldsig-more#ecdsa-sha256" => Ok(SignatureAlgorithm::Ecdsa(SignatureHash::Sha256)),
        other => Err(failed(format!("the signature method {other} is not accepted (SHA-1 and others are refused)"))),
    }
}

fn ecdsa_der(raw: &[u8]) -> Vec<u8> {
    let integer = |half: &[u8]| {
        let trimmed: Vec<u8> = half.iter().copied().skip_while(|byte| *byte == 0).collect();
        let mut content = if trimmed.first().is_some_and(|byte| byte & 0x80 != 0) { vec![0] } else { Vec::new() };
        content.extend(if trimmed.is_empty() { vec![0] } else { trimmed });
        let mut out = vec![0x02, content.len() as u8];
        out.extend(content);
        out
    };
    let (left, right) = raw.split_at(raw.len() / 2);
    let mut body = integer(left);
    body.extend(integer(right));
    let mut out = vec![0x30, body.len() as u8];
    out.extend(body);
    out
}

fn verified_element(document: &Document, signature: usize, certificate: &Certificate) -> Result<usize, SamlError> {
    let signed_info = document.child(signature, DSIG, "SignedInfo").ok_or_else(|| failed("the signature has no SignedInfo"))?;
    let method = document.child(signed_info, DSIG, "CanonicalizationMethod").and_then(|method| document.attribute(method, "Algorithm")).unwrap_or_default();
    if method != EXC_C14N {
        return Err(failed("SignedInfo is not canonicalized with exclusive C14N"));
    }
    let signature_method = document.child(signed_info, DSIG, "SignatureMethod").and_then(|method| document.attribute(method, "Algorithm")).unwrap_or_default();
    let algorithm = algorithm_of(signature_method)?;
    let references: Vec<usize> = document.children(signed_info).filter(|child| document.elements[*child].namespace == DSIG && document.elements[*child].local == "Reference").collect();
    let [reference] = references.as_slice() else {
        return Err(failed("the signature must have exactly one Reference"));
    };
    let uri = document.attribute(*reference, "URI").unwrap_or_default();
    let target_id = uri.strip_prefix('#').filter(|id| !id.is_empty()).ok_or_else(|| failed("the Reference does not point at an element by ID"))?;
    let matches = document.with_id(target_id);
    let [target] = matches.as_slice() else {
        return Err(failed("the signed ID is not on exactly one element"));
    };
    if document.elements[signature].parent != Some(*target) {
        return Err(failed("the signature is not inside the element it signs"));
    }
    let transforms = document.child(*reference, DSIG, "Transforms").map(|transforms| document.children(transforms).collect::<Vec<_>>()).unwrap_or_default();
    let mut enveloped = false;
    let mut inclusive = Vec::new();
    for transform in &transforms {
        match document.attribute(*transform, "Algorithm").unwrap_or_default() {
            ENVELOPED => enveloped = true,
            EXC_C14N => inclusive = inclusive_prefixes(document, *transform),
            other => return Err(failed(format!("the transform {other} is not accepted"))),
        }
    }
    if !enveloped {
        return Err(failed("the signature does not use the enveloped-signature transform"));
    }
    let digest_method = document.child(*reference, DSIG, "DigestMethod").and_then(|method| document.attribute(method, "Algorithm")).unwrap_or_default();
    let digest_value = base64(&document.child(*reference, DSIG, "DigestValue").map(|value| document.text(value)).unwrap_or_default())?;
    let canonical = exclusive_canonical(document, *target, Some(signature), &inclusive);
    let computed = match digest_method {
        "http://www.w3.org/2001/04/xmlenc#sha256" => eggdb_crypto::sha256(canonical.as_bytes()).to_vec(),
        "http://www.w3.org/2001/04/xmlenc#sha512" => eggdb_crypto::sha512(canonical.as_bytes()).to_vec(),
        other => return Err(failed(format!("the digest method {other} is not accepted"))),
    };
    if !eggdb_crypto::constant_time_eq(&computed, &digest_value) {
        return Err(failed("the signed element's digest does not match"));
    }
    let signed_info_inclusive = document.child(signed_info, DSIG, "CanonicalizationMethod").map(|method| inclusive_prefixes(document, method)).unwrap_or_default();
    let signed_text = exclusive_canonical(document, signed_info, None, &signed_info_inclusive);
    let mut signature_value = base64(&document.child(signature, DSIG, "SignatureValue").map(|value| document.text(value)).unwrap_or_default())?;
    if matches!(algorithm, SignatureAlgorithm::Ecdsa(_)) {
        signature_value = ecdsa_der(&signature_value);
    }
    verify_signature(algorithm, certificate.public_key(), signed_text.as_bytes(), &signature_value)
        .map_err(|error| failed(format!("the SAML signature does not verify with the identity provider's certificate: {error}")))?;
    Ok(*target)
}

fn instant(text: &str) -> Result<i64, SamlError> {
    let text = text.trim();
    let digits = text.strip_suffix('Z').ok_or_else(|| failed("a SAML time is not in UTC"))?;
    let (whole, _) = digits.split_once('.').unwrap_or((digits, ""));
    let bytes = whole.as_bytes();
    if bytes.len() != 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' || bytes[13] != b':' || bytes[16] != b':' {
        return Err(failed(format!("'{text}' is not a SAML time")));
    }
    let number = |from: usize, to: usize| -> Result<i64, SamlError> { whole[from..to].parse::<i64>().map_err(|_| failed(format!("'{text}' is not a SAML time"))) };
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    let (hour, minute, second) = (number(11, 13)?, number(14, 16)?, number(17, 19)?);
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year.rem_euclid(400);
    let month_index = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Ok((era * 146_097 + day_of_era - 719_468) * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn instant_text(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rest / 3_600, rest % 3_600 / 60, rest % 60)
}

/// How EggDB signs users in through one SAML identity provider.
#[derive(Debug, Clone)]
pub struct SamlConfig {
    /// The provider's entity ID, which its responses name as their issuer.
    pub idp_entity: String,
    /// The provider's single sign-on URL (HTTP-Redirect binding).
    pub idp_sso_url: String,
    /// The certificate whose key signs the provider's responses.
    pub idp_certificate: Certificate,
    /// EggDB's entity ID, the audience assertions are for.
    pub sp_entity: String,
    /// EggDB's assertion consumer URL, where the provider posts its response.
    pub acs_url: String,
    /// The attribute naming the role; the subject's `NameID` when `None`.
    pub name_attribute: Option<String>,
    tickets: Arc<Mutex<HashMap<[u8; 32], (String, i64)>>>,
}

/// A signed-in SAML user: the role name the assertion gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamlLogin {
    /// The role name.
    pub name: String,
}

impl SamlConfig {
    /// A configuration for one identity provider.
    pub fn new(idp_entity: String, idp_sso_url: String, idp_certificate: Certificate, sp_entity: String, acs_url: String, name_attribute: Option<String>) -> Self {
        Self {
            idp_entity,
            idp_sso_url,
            idp_certificate,
            sp_entity,
            acs_url,
            name_attribute,
            tickets: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The URL that sends a browser to the provider with a new `AuthnRequest`; `request_id` is
    /// a fresh random identifier and `relay_state` comes back with the response.
    pub fn sign_in_url(&self, request_id: &str, now: i64, relay_state: &str) -> String {
        let request = format!(
            "<samlp:AuthnRequest xmlns:samlp=\"{SAML_PROTOCOL}\" xmlns:saml=\"{SAML_ASSERTION}\" ID=\"{}\" Version=\"2.0\" IssueInstant=\"{}\" Destination=\"{}\" AssertionConsumerServiceURL=\"{}\" ProtocolBinding=\"urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST\"><saml:Issuer>{}</saml:Issuer></samlp:AuthnRequest>",
            escaped_attribute(request_id),
            instant_text(now),
            escaped_attribute(&self.idp_sso_url),
            escaped_attribute(&self.acs_url),
            escaped_text(&self.sp_entity)
        );
        let deflated = eggdb_crypto::base64_encode(&raw_deflate(request.as_bytes()));
        let separator = if self.idp_sso_url.contains('?') { '&' } else { '?' };
        format!(
            "{}{separator}SAMLRequest={}&RelayState={}",
            self.idp_sso_url,
            percent_encoded(&deflated),
            percent_encoded(relay_state)
        )
    }

    /// Checks the base64 `SAMLResponse` a browser posted at `now` (Unix seconds).
    pub fn accept_response(&self, posted: &str, now: i64) -> Result<SamlLogin, SamlError> {
        let xml = String::from_utf8(base64(posted)?).map_err(|_| failed("the SAML response is not UTF-8"))?;
        let document = Document::parse(&xml)?;
        let response = 0;
        if document.elements[response].namespace != SAML_PROTOCOL || document.elements[response].local != "Response" {
            return Err(failed("the message is not a SAML Response"));
        }
        let status = document
            .child(response, SAML_PROTOCOL, "Status")
            .and_then(|status| document.child(status, SAML_PROTOCOL, "StatusCode"))
            .and_then(|code| document.attribute(code, "Value"))
            .unwrap_or_default();
        if status != SUCCESS {
            return Err(failed(format!("the identity provider answered {status}")));
        }
        let response_signed = match document.child(response, DSIG, "Signature") {
            Some(signature) => Some(verified_element(&document, signature, &self.idp_certificate)?),
            None => None,
        };
        let assertions: Vec<usize> = document.children(response).filter(|child| document.elements[*child].namespace == SAML_ASSERTION && document.elements[*child].local == "Assertion").collect();
        let [assertion] = assertions.as_slice() else {
            return Err(failed("the response must hold exactly one unencrypted Assertion"));
        };
        let assertion = *assertion;
        let assertion_signed = match document.child(assertion, DSIG, "Signature") {
            Some(signature) => Some(verified_element(&document, signature, &self.idp_certificate)?),
            None => None,
        };
        if assertion_signed != Some(assertion) && response_signed != Some(response) {
            return Err(failed("neither the assertion nor the response is signed"));
        }
        let issuer = document.child(assertion, SAML_ASSERTION, "Issuer").map(|issuer| document.text(issuer)).unwrap_or_default();
        if issuer.trim() != self.idp_entity {
            return Err(failed(format!("the assertion's issuer is {issuer}, not {}", self.idp_entity)));
        }
        let conditions = document.child(assertion, SAML_ASSERTION, "Conditions").ok_or_else(|| failed("the assertion has no Conditions"))?;
        if let Some(before) = document.attribute(conditions, "NotBefore")
            && now + CLOCK_SKEW_SECONDS < instant(before)?
        {
            return Err(failed("the assertion is not valid yet"));
        }
        let until = document.attribute(conditions, "NotOnOrAfter").ok_or_else(|| failed("the assertion's Conditions have no NotOnOrAfter"))?;
        if now - CLOCK_SKEW_SECONDS >= instant(until)? {
            return Err(failed("the assertion has expired"));
        }
        let mut audiences = Vec::new();
        for restriction in document.children(conditions).filter(|child| document.elements[*child].local == "AudienceRestriction") {
            audiences.extend(document.children(restriction).map(|audience| document.text(audience).trim().to_owned()));
        }
        if !audiences.contains(&self.sp_entity) {
            return Err(failed("the assertion is not meant for this service"));
        }
        let subject = document.child(assertion, SAML_ASSERTION, "Subject").ok_or_else(|| failed("the assertion has no Subject"))?;
        let confirmed = document.children(subject).filter(|child| document.elements[*child].local == "SubjectConfirmation").any(|confirmation| {
            document.attribute(confirmation, "Method") == Some(BEARER)
                && document.child(confirmation, SAML_ASSERTION, "SubjectConfirmationData").is_some_and(|data| {
                    document.attribute(data, "Recipient") == Some(self.acs_url.as_str())
                        && document.attribute(data, "NotOnOrAfter").and_then(|until| instant(until).ok()).is_some_and(|until| now - CLOCK_SKEW_SECONDS < until)
                })
        });
        if !confirmed {
            return Err(failed("the assertion has no bearer confirmation for this service's URL that is still valid"));
        }
        let name = match &self.name_attribute {
            None => document.child(subject, SAML_ASSERTION, "NameID").map(|name| document.text(name)),
            Some(wanted) => document.child(assertion, SAML_ASSERTION, "AttributeStatement").and_then(|statement| {
                document
                    .children(statement)
                    .find(|attribute| document.attribute(*attribute, "Name") == Some(wanted.as_str()))
                    .and_then(|attribute| document.child(attribute, SAML_ASSERTION, "AttributeValue"))
                    .map(|value| document.text(value))
            }),
        }
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty() && name.len() <= 63)
        .ok_or_else(|| failed("the assertion names no user"))?;
        let mut all = Vec::new();
        document.descendants(assertion, &mut all);
        if all.iter().any(|element| document.elements[*element].local == "EncryptedID" || document.elements[*element].local == "EncryptedAttribute") {
            return Err(failed("encrypted assertions are not accepted"));
        }
        Ok(SamlLogin { name })
    }

    /// A one-time ticket for `role`, valid for five minutes, to use as its password with a
    /// `saml` login rule; `random` is 32 fresh random bytes.
    pub fn issue_ticket(&self, role: &str, random: [u8; 32], now: i64) -> String {
        let ticket = format!("eggdb_saml_{}", random.iter().map(|byte| format!("{byte:02x}")).collect::<String>());
        let mut tickets = self.tickets.lock().unwrap_or_else(PoisonError::into_inner);
        tickets.retain(|_, (_, expires)| *expires > now);
        if tickets.len() < MOST_TICKETS {
            tickets.insert(eggdb_crypto::sha256(ticket.as_bytes()), (role.to_owned(), now + TICKET_SECONDS));
        }
        ticket
    }

    /// Whether `ticket` is a live ticket for `role`; a redeemed ticket is gone.
    pub fn redeem_ticket(&self, role: &str, ticket: &str, now: i64) -> bool {
        let key = eggdb_crypto::sha256(ticket.as_bytes());
        let mut tickets = self.tickets.lock().unwrap_or_else(PoisonError::into_inner);
        match tickets.get(&key) {
            Some((owner, expires)) if owner == role && *expires > now => {
                tickets.remove(&key);
                true
            }
            _ => false,
        }
    }
}

fn percent_encoded(text: &str) -> String {
    text.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn raw_deflate(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 65_535 * 5 + 5);
    let mut chunks = bytes.chunks(65_535).peekable();
    if chunks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(chunk) = chunks.next() {
        out.push(u8::from(chunks.peek().is_none()));
        let length = chunk.len() as u16;
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&(!length).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_canonicalization_follows_the_specification() {
        let document = Document::parse(
            "<a:Root xmlns:a=\"urn:a\" xmlns:b=\"urn:b\" z=\"2\" a:y=\"1\"><a:Child  x=\"t&#9;ab\">x &lt; y &amp; z<!-- gone --><b:Used/></a:Child><Empty/></a:Root>",
        )
        .unwrap();
        assert_eq!(
            exclusive_canonical(&document, 1, None, &[]),
            "<a:Child xmlns:a=\"urn:a\" x=\"t&#x9;ab\">x &lt; y &amp; z<b:Used xmlns:b=\"urn:b\"></b:Used></a:Child>"
        );
        assert_eq!(
            exclusive_canonical(&document, 0, None, &["b".to_owned()]),
            "<a:Root xmlns:a=\"urn:a\" xmlns:b=\"urn:b\" z=\"2\" a:y=\"1\"><a:Child x=\"t&#x9;ab\">x &lt; y &amp; z<b:Used></b:Used></a:Child><Empty></Empty></a:Root>"
        );
    }

    #[test]
    fn hostile_xml_is_refused() {
        for bad in [
            "<!DOCTYPE x [<!ENTITY e \"boom\">]><x>&e;</x>",
            "<x>&e;</x>",
            "<x><y></x></y>",
            "<x/><y/>",
            "<x a=\"1\" a=\"2\"/>",
            "<p:x/>",
            "text",
        ] {
            assert!(Document::parse(bad).is_err(), "{bad}");
        }
        let deep = "<x>".repeat(100) + &"</x>".repeat(100);
        assert!(Document::parse(&deep).is_err());
    }

    #[test]
    fn times_and_tickets_behave() {
        assert_eq!(instant("2026-11-15T10:20:30.123Z").unwrap(), instant("2026-11-15T10:20:30Z").unwrap());
        assert_eq!(instant_text(instant("2026-11-15T10:20:30Z").unwrap()), "2026-11-15T10:20:30Z");
        assert!(instant("2026-11-15 10:20:30").is_err());
        let pem = test_certificate();
        let config = SamlConfig::new("idp".into(), "https://idp/sso".into(), pem, "eggdb".into(), "https://db/auth/saml/acs".into(), None);
        let ticket = config.issue_ticket("erin", [3; 32], 100);
        assert!(!config.redeem_ticket("frank", &ticket, 101));
        assert!(config.redeem_ticket("erin", &ticket, 101));
        assert!(!config.redeem_ticket("erin", &ticket, 102));
        let late = config.issue_ticket("erin", [4; 32], 100);
        assert!(!config.redeem_ticket("erin", &late, 100 + TICKET_SECONDS));
        let url = config.sign_in_url("_abc", 100, "/console");
        assert!(url.starts_with("https://idp/sso?SAMLRequest="), "{url}");
        assert!(url.ends_with("&RelayState=%2Fconsole"));
    }

    pub(crate) fn test_certificate() -> Certificate {
        let mut seed = 7u8;
        let mut random = |bytes: &mut [u8]| -> std::io::Result<()> {
            for byte in bytes.iter_mut() {
                seed = seed.wrapping_mul(31).wrapping_add(17);
                *byte = seed;
            }
            Ok(())
        };
        let request = eggdb_tls::self_signed::SelfSignedCertificateRequest {
            common_name: "idp.example".to_owned(),
            subject_alternative_names: Vec::new(),
            not_before_unix_seconds: 0,
            not_after_unix_seconds: 4_000_000_000,
        };
        let (certificate, _) = eggdb_tls::self_signed::generate_self_signed_certificate(&request, &mut random).expect("a test certificate");
        certificate
    }

    fn raw_signature(der: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut at = 2;
        for _ in 0..2 {
            let length = usize::from(der[at + 1]);
            let integer = &der[at + 2..at + 2 + length];
            let trimmed: Vec<u8> = integer.iter().copied().skip_while(|byte| *byte == 0).collect();
            out.extend(std::iter::repeat_n(0u8, 32 - trimmed.len()));
            out.extend(trimmed);
            at += 2 + length;
        }
        out
    }

    fn signed_response(name: &str, now: i64, tamper: bool) -> (String, Certificate) {
        let mut seed = 11u8;
        let mut random = |bytes: &mut [u8]| -> std::io::Result<()> {
            for byte in bytes.iter_mut() {
                seed = seed.wrapping_mul(29).wrapping_add(3);
                *byte = seed;
            }
            Ok(())
        };
        let request = eggdb_tls::self_signed::SelfSignedCertificateRequest {
            common_name: "idp.example".to_owned(),
            subject_alternative_names: Vec::new(),
            not_before_unix_seconds: 0,
            not_after_unix_seconds: 4_000_000_000,
        };
        let (certificate, key) = eggdb_tls::self_signed::generate_self_signed_certificate(&request, &mut random).unwrap();
        let until = instant_text(now + 300);
        let before = instant_text(now - 10);
        let assertion_body = format!(
            "<saml:Issuer>https://idp.example</saml:Issuer>SIGNATURE<saml:Subject><saml:NameID>{name}</saml:NameID><saml:SubjectConfirmation Method=\"{BEARER}\"><saml:SubjectConfirmationData NotOnOrAfter=\"{until}\" Recipient=\"https://db.example/auth/saml/acs\"/></saml:SubjectConfirmation></saml:Subject><saml:Conditions NotBefore=\"{before}\" NotOnOrAfter=\"{until}\"><saml:AudienceRestriction><saml:Audience>eggdb</saml:Audience></saml:AudienceRestriction></saml:Conditions>"
        );
        let open = format!("<saml:Assertion xmlns:saml=\"{SAML_ASSERTION}\" ID=\"_a1\" Version=\"2.0\" IssueInstant=\"{before}\">");
        let unsigned = format!("{open}{}</saml:Assertion>", assertion_body.replace("SIGNATURE", ""));
        let parsed = Document::parse(&unsigned).unwrap();
        let digest = eggdb_crypto::base64_encode(&eggdb_crypto::sha256(exclusive_canonical(&parsed, 0, None, &[]).as_bytes()));
        let signed_info = format!(
            "<ds:SignedInfo xmlns:ds=\"{DSIG}\"><ds:CanonicalizationMethod Algorithm=\"{EXC_C14N}\"/><ds:SignatureMethod Algorithm=\"http://www.w3.org/2001/04/xmldsig-more#ecdsa-sha256\"/><ds:Reference URI=\"#_a1\"><ds:Transforms><ds:Transform Algorithm=\"{ENVELOPED}\"/><ds:Transform Algorithm=\"{EXC_C14N}\"/></ds:Transforms><ds:DigestMethod Algorithm=\"http://www.w3.org/2001/04/xmlenc#sha256\"/><ds:DigestValue>{digest}</ds:DigestValue></ds:Reference></ds:SignedInfo>"
        );
        let canonical_info = exclusive_canonical(&Document::parse(&signed_info).unwrap(), 0, None, &[]);
        let der = key.sign(SignatureAlgorithm::Ecdsa(SignatureHash::Sha256), canonical_info.as_bytes(), &mut random).unwrap();
        let signature = format!(
            "<ds:Signature xmlns:ds=\"{DSIG}\">{}<ds:SignatureValue>{}</ds:SignatureValue></ds:Signature>",
            signed_info.replace(&format!(" xmlns:ds=\"{DSIG}\""), ""),
            eggdb_crypto::base64_encode(&raw_signature(&der))
        );
        let mut assertion = format!("{open}{}</saml:Assertion>", assertion_body.replace("SIGNATURE", &signature));
        if tamper {
            assertion = assertion.replace(&format!("<saml:NameID>{name}</saml:NameID>"), "<saml:NameID>root</saml:NameID>");
        }
        let response = format!(
            "<samlp:Response xmlns:samlp=\"{SAML_PROTOCOL}\" ID=\"_r1\" Version=\"2.0\"><samlp:Status><samlp:StatusCode Value=\"{SUCCESS}\"/></samlp:Status>{assertion}</samlp:Response>"
        );
        (eggdb_crypto::base64_encode(response.as_bytes()), certificate)
    }

    fn configured(certificate: Certificate) -> SamlConfig {
        SamlConfig::new(
            "https://idp.example".to_owned(),
            "https://idp.example/sso".to_owned(),
            certificate,
            "eggdb".to_owned(),
            "https://db.example/auth/saml/acs".to_owned(),
            None,
        )
    }

    #[test]
    fn a_signed_assertion_signs_its_subject_in_and_tampering_or_lateness_fails() {
        let now = 1_900_000_000;
        let (posted, certificate) = signed_response("erin", now, false);
        let config = configured(certificate.clone());
        assert_eq!(config.accept_response(&posted, now).unwrap(), SamlLogin { name: "erin".to_owned() });
        assert!(config.accept_response(&posted, now + 900).unwrap_err().0.contains("expired"));
        let (tampered, tampered_certificate) = signed_response("erin", now, true);
        assert!(configured(tampered_certificate).accept_response(&tampered, now).unwrap_err().0.contains("digest"));
        let other = configured(test_certificate());
        assert!(other.accept_response(&posted, now).unwrap_err().0.contains("does not verify"));
    }
}
