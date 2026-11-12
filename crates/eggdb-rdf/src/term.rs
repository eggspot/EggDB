//! RDF terms (RDF 1.1 Concepts): IRIs, blank nodes and literals, and how they are written in
//! N-Triples and stored.

/// `xsd:string`, the datatype of a plain literal.
pub const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
/// `rdf:langString`, the datatype of a literal with a language tag.
pub const RDF_LANG_STRING: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString";
/// `xsd:integer`.
pub const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
/// `xsd:decimal`.
pub const XSD_DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";
/// `xsd:double`.
pub const XSD_DOUBLE: &str = "http://www.w3.org/2001/XMLSchema#double";
/// `xsd:boolean`.
pub const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";
/// `xsd:dateTime`.
pub const XSD_DATE_TIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";
/// `rdf:type`, written `a` in Turtle and SPARQL.
pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
/// `rdf:first`.
pub const RDF_FIRST: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#first";
/// `rdf:rest`.
pub const RDF_REST: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#rest";
/// `rdf:nil`.
pub const RDF_NIL: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#nil";

/// A literal: its lexical form, datatype IRI and, for `rdf:langString`, its language tag in
/// lower case.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Literal {
    /// The lexical form.
    pub lexical: String,
    /// The datatype IRI.
    pub datatype: String,
    /// The language tag, for a language-tagged string.
    pub language: Option<String>,
}

impl Literal {
    /// A plain `xsd:string` literal.
    pub fn string(lexical: &str) -> Self {
        Self {
            lexical: lexical.to_owned(),
            datatype: XSD_STRING.to_owned(),
            language: None,
        }
    }

    /// A literal of `datatype`.
    pub fn typed(lexical: &str, datatype: &str) -> Self {
        Self {
            lexical: lexical.to_owned(),
            datatype: datatype.to_owned(),
            language: None,
        }
    }

    /// A language-tagged string.
    pub fn tagged(lexical: &str, language: &str) -> Self {
        Self {
            lexical: lexical.to_owned(),
            datatype: RDF_LANG_STRING.to_owned(),
            language: Some(language.to_ascii_lowercase()),
        }
    }
}

/// An RDF term.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Term {
    /// An IRI.
    Iri(String),
    /// A blank node, by its label.
    Blank(String),
    /// A literal.
    Literal(Literal),
}

impl Term {
    /// An IRI term.
    pub fn iri(iri: &str) -> Self {
        Self::Iri(iri.to_owned())
    }

    /// The term as stored: a tag byte, then its parts separated by NUL bytes.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Iri(iri) => {
                out.push(b'I');
                out.extend_from_slice(iri.as_bytes());
            }
            Self::Blank(label) => {
                out.push(b'B');
                out.extend_from_slice(label.as_bytes());
            }
            Self::Literal(literal) => {
                out.push(b'L');
                out.extend_from_slice(literal.datatype.as_bytes());
                out.push(0);
                out.extend_from_slice(literal.language.as_deref().unwrap_or_default().as_bytes());
                out.push(0);
                out.extend_from_slice(literal.lexical.as_bytes());
            }
        }
        out
    }

    /// The term stored as `bytes` by [`Term::to_bytes`].
    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let (tag, rest) = bytes.split_first()?;
        let text = |part: &[u8]| String::from_utf8(part.to_vec()).ok();
        Some(match tag {
            b'I' => Self::Iri(text(rest)?),
            b'B' => Self::Blank(text(rest)?),
            b'L' => {
                let mut parts = rest.splitn(3, |byte| *byte == 0);
                let datatype = text(parts.next()?)?;
                let language = text(parts.next()?)?;
                let lexical = text(parts.next()?)?;
                Self::Literal(Literal {
                    lexical,
                    datatype,
                    language: (!language.is_empty()).then_some(language),
                })
            }
            _ => return None,
        })
    }

    /// The term in N-Triples syntax.
    pub fn to_ntriples(&self) -> String {
        match self {
            Self::Iri(iri) => format!("<{}>", escaped_iri(iri)),
            Self::Blank(label) => format!("_:{label}"),
            Self::Literal(literal) => {
                let quoted = format!("\"{}\"", escaped_string(&literal.lexical));
                match &literal.language {
                    Some(language) => format!("{quoted}@{language}"),
                    None if literal.datatype == XSD_STRING => quoted,
                    None => format!("{quoted}^^<{}>", escaped_iri(&literal.datatype)),
                }
            }
        }
    }
}

fn escaped_iri(iri: &str) -> String {
    let mut out = String::with_capacity(iri.len());
    for character in iri.chars() {
        match character {
            '<' | '>' | '"' | '{' | '}' | '|' | '^' | '`' | '\\' => out.push_str(&format!("\\u{:04X}", character as u32)),
            control if (control as u32) <= 0x20 => out.push_str(&format!("\\u{:04X}", control as u32)),
            other => out.push(other),
        }
    }
    out
}

/// A string with N-Triples' escapes for quotes, backslashes and line breaks.
pub(crate) fn escaped_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

/// A triple in a graph: `graph` is `None` for the default graph.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Quad {
    /// The subject: an IRI or a blank node.
    pub subject: Term,
    /// The predicate: an IRI.
    pub predicate: Term,
    /// The object.
    pub object: Term,
    /// The named graph, or `None` for the default graph.
    pub graph: Option<Term>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_round_trip_through_their_stored_bytes() {
        for term in [
            Term::iri("http://example.org/a"),
            Term::Blank("b0".to_owned()),
            Term::Literal(Literal::string("hello")),
            Term::Literal(Literal::tagged("chat", "FR")),
            Term::Literal(Literal::typed("42", XSD_INTEGER)),
        ] {
            assert_eq!(Term::from_bytes(&term.to_bytes()), Some(term));
        }
    }

    #[test]
    fn terms_print_as_ntriples() {
        assert_eq!(Term::Literal(Literal::tagged("chat", "fr")).to_ntriples(), "\"chat\"@fr");
        assert_eq!(
            Term::Literal(Literal::typed("1", XSD_INTEGER)).to_ntriples(),
            "\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>"
        );
        assert_eq!(Term::Literal(Literal::string("a \"b\"\n")).to_ntriples(), "\"a \\\"b\\\"\\n\"");
    }
}
