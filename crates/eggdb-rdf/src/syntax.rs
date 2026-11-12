//! Readers for RDF documents, written from the W3C recommendations: N-Triples, N-Quads, Turtle
//! and TriG (RDF 1.1). Turtle's and TriG's prefixes, base IRIs, `a`, predicate and object
//! lists, blank node property lists, collections, numeric and boolean shorthands and long
//! strings are read; relative IRIs are resolved against the base (RFC 3986, section 5.2).

use crate::term::{
    Literal, Quad, RDF_FIRST, RDF_NIL, RDF_REST, RDF_TYPE, Term, XSD_BOOLEAN, XSD_DECIMAL,
    XSD_DOUBLE, XSD_INTEGER,
};
use crate::{RdfError, RdfResult};
use std::collections::HashMap;

const MAX_DEPTH: usize = 64;

/// The syntax of an RDF document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// N-Triples: one triple per line, full IRIs.
    NTriples,
    /// N-Quads: N-Triples with an optional graph label per line.
    NQuads,
    /// Turtle.
    Turtle,
    /// TriG: Turtle with graph blocks.
    TriG,
}

impl Format {
    /// The format a media type or file extension names, such as `text/turtle` or `nq`.
    pub fn named(name: &str) -> Option<Self> {
        let name = name.trim().to_ascii_lowercase();
        let name = name.split(';').next().unwrap_or_default().trim();
        Some(match name {
            "nt" | "ntriples" | "n-triples" | "application/n-triples" => Self::NTriples,
            "nq" | "nquads" | "n-quads" | "application/n-quads" => Self::NQuads,
            "ttl" | "turtle" | "text/turtle" | "application/x-turtle" => Self::Turtle,
            "trig" | "application/trig" => Self::TriG,
            _ => return None,
        })
    }
}

fn syntax(at: usize, message: &str) -> RdfError {
    RdfError::syntax(format!("{message} at byte {at}"))
}

/// `relative` resolved against `base`, as RFC 3986 section 5.2 does for the forms RDF
/// documents use.
pub(crate) fn resolve(base: &str, relative: &str) -> String {
    let scheme_end = relative.find(':');
    let first_delimiter = relative.find(['/', '?', '#']);
    if let Some(colon) = scheme_end
        && first_delimiter.is_none_or(|delimiter| colon < delimiter)
        && colon > 0
    {
        return relative.to_owned();
    }
    if base.is_empty() {
        return relative.to_owned();
    }
    let without_fragment = base.split('#').next().unwrap_or(base);
    if relative.is_empty() {
        return without_fragment.to_owned();
    }
    if relative.starts_with('#') {
        return format!("{without_fragment}{relative}");
    }
    let scheme = base.find(':').map_or("", |colon| &base[..=colon]);
    if relative.starts_with("//") {
        return format!("{scheme}{relative}");
    }
    let after_scheme = &base[scheme.len()..];
    let authority = if let Some(rest) = after_scheme.strip_prefix("//") {
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        &after_scheme[..end + 2]
    } else {
        ""
    };
    if relative.starts_with('?') {
        let without_query = without_fragment.split('?').next().unwrap_or(without_fragment);
        return format!("{without_query}{relative}");
    }
    let path = if relative.starts_with('/') {
        relative.to_owned()
    } else {
        let base_path = &without_fragment[scheme.len() + authority.len()..];
        let base_path = base_path.split('?').next().unwrap_or(base_path);
        let directory = base_path.rfind('/').map_or("/", |slash| &base_path[..=slash]);
        format!("{directory}{relative}")
    };
    let (path, rest) = match path.find(['?', '#']) {
        Some(at) => (path[..at].to_owned(), path[at..].to_owned()),
        None => (path, String::new()),
    };
    let mut segments: Vec<&str> = Vec::new();
    let parts: Vec<&str> = path.split('/').collect();
    for (index, segment) in parts.iter().enumerate() {
        match *segment {
            "." => {
                if index == parts.len() - 1 {
                    segments.push("");
                }
            }
            ".." => {
                if segments.len() > 1 {
                    segments.pop();
                }
                if index == parts.len() - 1 {
                    segments.push("");
                }
            }
            other => segments.push(other),
        }
    }
    format!("{scheme}{authority}{}{rest}", segments.join("/"))
}

struct Reader<'t> {
    text: &'t str,
    at: usize,
    base: String,
    prefixes: HashMap<String, String>,
    fresh: usize,
    depth: usize,
    quads: Vec<Quad>,
}

impl Reader<'_> {
    fn rest(&self) -> &str {
        self.text.get(self.at..).unwrap_or_default()
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn eat(&mut self, text: &str) -> bool {
        if self.rest().starts_with(text) {
            self.at += text.len();
            return true;
        }
        false
    }

    fn expect(&mut self, text: &str) -> RdfResult<()> {
        if self.eat(text) {
            return Ok(());
        }
        Err(syntax(self.at, &format!("expected '{text}'")))
    }

    fn keyword(&mut self, word: &str) -> bool {
        let rest = self.rest();
        let matched = rest.len() >= word.len()
            && rest.get(..word.len()).is_some_and(|start| start.eq_ignore_ascii_case(word))
            && !rest[word.len()..].chars().next().is_some_and(|next| next.is_alphanumeric() || next == '_' || next == ':');
        if matched {
            self.at += word.len();
        }
        matched
    }

    fn skip_space(&mut self) {
        loop {
            let rest = self.rest();
            let trimmed = rest.trim_start();
            self.at += rest.len() - trimmed.len();
            if self.peek() == Some('#') {
                let line = self.rest().find(['\n', '\r']).unwrap_or(self.rest().len());
                self.at += line;
                continue;
            }
            return;
        }
    }

    fn deeper(&mut self) -> RdfResult<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(syntax(self.at, "the document nests too deeply"));
        }
        Ok(())
    }

    fn fresh_blank(&mut self) -> Term {
        self.fresh += 1;
        Term::Blank(format!("genid{}", self.fresh))
    }

    fn escape(&mut self) -> RdfResult<char> {
        let start = self.at;
        let Some(kind) = self.peek() else {
            return Err(syntax(start, "an escape at the end"));
        };
        self.at += kind.len_utf8();
        let digits = match kind {
            'u' => 4,
            'U' => 8,
            't' => return Ok('\t'),
            'b' => return Ok('\u{8}'),
            'n' => return Ok('\n'),
            'r' => return Ok('\r'),
            'f' => return Ok('\u{c}'),
            '"' => return Ok('"'),
            '\'' => return Ok('\''),
            '\\' => return Ok('\\'),
            _ => return Err(syntax(start, "an unknown escape")),
        };
        let hex = self.rest().get(..digits).ok_or_else(|| syntax(start, "a short \\u escape"))?;
        let code = u32::from_str_radix(hex, 16).map_err(|_| syntax(start, "a \\u escape that is not hexadecimal"))?;
        self.at += digits;
        char::from_u32(code).ok_or_else(|| syntax(start, "a \\u escape that is not a character"))
    }

    fn iri_reference(&mut self) -> RdfResult<String> {
        let start = self.at;
        self.expect("<")?;
        let mut iri = String::new();
        loop {
            let Some(character) = self.peek() else {
                return Err(syntax(start, "an unterminated IRI"));
            };
            self.at += character.len_utf8();
            match character {
                '>' => break,
                '\\' => iri.push(self.escape()?),
                '<' | '"' | '{' | '}' | '|' | '^' | '`' | ' ' | '\n' | '\r' | '\t' => {
                    return Err(syntax(self.at, "a character IRIs may not hold"));
                }
                other => iri.push(other),
            }
        }
        Ok(resolve(&self.base, &iri))
    }

    fn prefix_label(&mut self) -> String {
        let start = self.at;
        while let Some(character) = self.peek() {
            if character.is_alphanumeric() || matches!(character, '_' | '-' | '.') {
                self.at += character.len_utf8();
            } else {
                break;
            }
        }
        let mut label = self.text[start..self.at].to_owned();
        while label.ends_with('.') {
            label.pop();
            self.at -= 1;
        }
        label
    }

    fn prefixed_name(&mut self) -> RdfResult<String> {
        let start = self.at;
        let prefix = self.prefix_label();
        if !self.eat(":") {
            return Err(syntax(start, "expected a prefixed name"));
        }
        let namespace = self
            .prefixes
            .get(&prefix)
            .cloned()
            .ok_or_else(|| syntax(start, &format!("the prefix '{prefix}:' is not declared")))?;
        let mut local = String::new();
        loop {
            let Some(character) = self.peek() else {
                break;
            };
            if character == '\\' {
                self.at += 1;
                let Some(escaped) = self.peek() else {
                    return Err(syntax(self.at, "an escape at the end"));
                };
                self.at += escaped.len_utf8();
                local.push(escaped);
            } else if character == '%' {
                let hex = self.rest().get(..3).unwrap_or_default().to_owned();
                if hex.len() != 3 || !hex[1..].chars().all(|digit| digit.is_ascii_hexdigit()) {
                    return Err(syntax(self.at, "a bad percent escape"));
                }
                local.push_str(&hex);
                self.at += 3;
            } else if character.is_alphanumeric() || matches!(character, '_' | '-' | ':' | '.') {
                local.push(character);
                self.at += character.len_utf8();
            } else {
                break;
            }
        }
        while local.ends_with('.') {
            local.pop();
            self.at -= 1;
        }
        Ok(format!("{namespace}{local}"))
    }

    fn blank_label(&mut self) -> RdfResult<Term> {
        let start = self.at;
        self.expect("_:")?;
        let label = self.prefix_label();
        if label.is_empty() {
            return Err(syntax(start, "a blank node without a label"));
        }
        Ok(Term::Blank(label))
    }

    fn iri(&mut self) -> RdfResult<String> {
        if self.peek() == Some('<') {
            return self.iri_reference();
        }
        self.prefixed_name()
    }

    fn string(&mut self) -> RdfResult<String> {
        let start = self.at;
        let quote = if self.eat("\"\"\"") {
            "\"\"\""
        } else if self.eat("'''") {
            "'''"
        } else if self.eat("\"") {
            "\""
        } else if self.eat("'") {
            "'"
        } else {
            return Err(syntax(start, "expected a string"));
        };
        let long = quote.len() == 3;
        let mut value = String::new();
        loop {
            if self.eat(quote) {
                return Ok(value);
            }
            let Some(character) = self.peek() else {
                return Err(syntax(start, "an unterminated string"));
            };
            self.at += character.len_utf8();
            match character {
                '\\' => value.push(self.escape()?),
                '\n' | '\r' if !long => return Err(syntax(start, "a line break in a short string")),
                other => value.push(other),
            }
        }
    }

    fn literal(&mut self) -> RdfResult<Term> {
        let lexical = self.string()?;
        if self.eat("@") {
            let start = self.at;
            while self.peek().is_some_and(|character| character.is_ascii_alphanumeric() || character == '-') {
                self.at += 1;
            }
            let language = &self.text[start..self.at];
            if language.is_empty() {
                return Err(syntax(start, "an empty language tag"));
            }
            return Ok(Term::Literal(Literal::tagged(&lexical, language)));
        }
        if self.eat("^^") {
            let datatype = self.iri()?;
            return Ok(Term::Literal(Literal::typed(&lexical, &datatype)));
        }
        Ok(Term::Literal(Literal::string(&lexical)))
    }

    fn number(&mut self) -> RdfResult<Term> {
        let start = self.at;
        if matches!(self.peek(), Some('+' | '-')) {
            self.at += 1;
        }
        let digits = |reader: &mut Self| {
            let begin = reader.at;
            while reader.peek().is_some_and(|character| character.is_ascii_digit()) {
                reader.at += 1;
            }
            reader.at - begin
        };
        let whole = digits(self);
        let mut datatype = XSD_INTEGER;
        if self.peek() == Some('.') && self.rest()[1..].chars().next().is_some_and(|next| next.is_ascii_digit()) {
            self.at += 1;
            digits(self);
            datatype = XSD_DECIMAL;
        } else if whole == 0 {
            return Err(syntax(start, "expected a number"));
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            self.at += 1;
            if matches!(self.peek(), Some('+' | '-')) {
                self.at += 1;
            }
            if digits(self) == 0 {
                return Err(syntax(start, "an exponent without digits"));
            }
            datatype = XSD_DOUBLE;
        }
        Ok(Term::Literal(Literal::typed(&self.text[start..self.at], datatype)))
    }

    fn collection(&mut self, graph: &Option<Term>) -> RdfResult<Term> {
        self.deeper()?;
        self.expect("(")?;
        let mut items = Vec::new();
        loop {
            self.skip_space();
            if self.eat(")") {
                break;
            }
            if self.peek().is_none() {
                return Err(syntax(self.at, "an unterminated collection"));
            }
            items.push(self.object(graph)?);
        }
        self.depth -= 1;
        let mut list = Term::iri(RDF_NIL);
        for item in items.into_iter().rev() {
            let node = self.fresh_blank();
            self.emit(node.clone(), Term::iri(RDF_FIRST), item, graph);
            self.emit(node.clone(), Term::iri(RDF_REST), list, graph);
            list = node;
        }
        Ok(list)
    }

    fn blank_property_list(&mut self, graph: &Option<Term>) -> RdfResult<Term> {
        self.deeper()?;
        self.expect("[")?;
        let node = self.fresh_blank();
        self.skip_space();
        if !self.eat("]") {
            self.predicate_objects(&node, graph)?;
            self.skip_space();
            self.expect("]")?;
        }
        self.depth -= 1;
        Ok(node)
    }

    fn emit(&mut self, subject: Term, predicate: Term, object: Term, graph: &Option<Term>) {
        self.quads.push(Quad {
            subject,
            predicate,
            object,
            graph: graph.clone(),
        });
    }

    fn object(&mut self, graph: &Option<Term>) -> RdfResult<Term> {
        self.skip_space();
        match self.peek() {
            Some('<') => Ok(Term::Iri(self.iri_reference()?)),
            Some('_') if self.rest().starts_with("_:") => self.blank_label(),
            Some('[') => self.blank_property_list(graph),
            Some('(') => self.collection(graph),
            Some('"' | '\'') => self.literal(),
            Some('+' | '-' | '.' | '0'..='9') => self.number(),
            _ => {
                if self.keyword("true") {
                    return Ok(Term::Literal(Literal::typed("true", XSD_BOOLEAN)));
                }
                if self.keyword("false") {
                    return Ok(Term::Literal(Literal::typed("false", XSD_BOOLEAN)));
                }
                Ok(Term::Iri(self.prefixed_name()?))
            }
        }
    }

    fn verb(&mut self) -> RdfResult<Term> {
        self.skip_space();
        if self.peek() == Some('a') {
            let after = self.rest()[1..].chars().next();
            if after.is_none_or(|next| next.is_whitespace() || matches!(next, '<' | '"' | '\'' | '[' | '(' | '_')) {
                self.at += 1;
                return Ok(Term::iri(RDF_TYPE));
            }
        }
        Ok(Term::Iri(self.iri()?))
    }

    fn predicate_objects(&mut self, subject: &Term, graph: &Option<Term>) -> RdfResult<()> {
        loop {
            let predicate = self.verb()?;
            loop {
                let object = self.object(graph)?;
                self.emit(subject.clone(), predicate.clone(), object, graph);
                self.skip_space();
                if !self.eat(",") {
                    break;
                }
            }
            self.skip_space();
            if !self.eat(";") {
                return Ok(());
            }
            loop {
                self.skip_space();
                if !self.eat(";") {
                    break;
                }
            }
            self.skip_space();
            if matches!(self.peek(), Some('.' | ']' | '}') | None) {
                return Ok(());
            }
        }
    }

    fn subject(&mut self, graph: &Option<Term>) -> RdfResult<(Term, bool)> {
        self.skip_space();
        match self.peek() {
            Some('[') => Ok((self.blank_property_list(graph)?, true)),
            Some('(') => Ok((self.collection(graph)?, false)),
            Some('_') if self.rest().starts_with("_:") => Ok((self.blank_label()?, false)),
            _ => Ok((Term::Iri(self.iri()?), false)),
        }
    }

    fn triples(&mut self, graph: &Option<Term>) -> RdfResult<()> {
        let (subject, property_list) = self.subject(graph)?;
        self.skip_space();
        if property_list && matches!(self.peek(), Some('.' | '}')) {
            return Ok(());
        }
        self.predicate_objects(&subject, graph)
    }

    fn directive(&mut self) -> RdfResult<bool> {
        let sparql_style = if self.eat("@prefix") {
            Some(false)
        } else if self.keyword("prefix") {
            Some(true)
        } else {
            None
        };
        if let Some(sparql_style) = sparql_style {
            self.skip_space();
            let prefix = self.prefix_label();
            self.expect(":")?;
            self.skip_space();
            let namespace = self.iri_reference()?;
            self.prefixes.insert(prefix, namespace);
            if !sparql_style {
                self.skip_space();
                self.expect(".")?;
            }
            return Ok(true);
        }
        let sparql_style = if self.eat("@base") {
            Some(false)
        } else if self.keyword("base") {
            Some(true)
        } else {
            None
        };
        if let Some(sparql_style) = sparql_style {
            self.skip_space();
            self.base = self.iri_reference()?;
            if !sparql_style {
                self.skip_space();
                self.expect(".")?;
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn graph_block(&mut self, graph: Option<Term>) -> RdfResult<()> {
        self.expect("{")?;
        loop {
            self.skip_space();
            if self.eat("}") {
                return Ok(());
            }
            if self.peek().is_none() {
                return Err(syntax(self.at, "an unterminated graph block"));
            }
            self.triples(&graph)?;
            self.skip_space();
            if !self.eat(".") {
                self.skip_space();
                self.expect("}")?;
                return Ok(());
            }
        }
    }

    fn document(&mut self, trig: bool) -> RdfResult<()> {
        loop {
            self.skip_space();
            if self.peek().is_none() {
                return Ok(());
            }
            if self.directive()? {
                continue;
            }
            if trig {
                if self.peek() == Some('{') {
                    self.graph_block(None)?;
                    continue;
                }
                let named = self.keyword("graph");
                let resume = self.at;
                self.skip_space();
                let label = match self.peek() {
                    Some('_') if self.rest().starts_with("_:") => Some(self.blank_label()?),
                    Some('[') if self.rest()[1..].trim_start().starts_with(']') => {
                        self.at = self.text.len() - self.rest()[1..].trim_start()[1..].len();
                        Some(self.fresh_blank())
                    }
                    Some('<') => Some(Term::Iri(self.iri_reference()?)),
                    Some(character) if character.is_alphabetic() || character == ':' => Some(Term::Iri(self.prefixed_name()?)),
                    _ => None,
                };
                self.skip_space();
                if let Some(label) = label
                    && self.peek() == Some('{')
                {
                    self.graph_block(Some(label))?;
                    continue;
                }
                if named {
                    return Err(syntax(self.at, "GRAPH needs a label and a block"));
                }
                self.at = resume;
            }
            self.triples(&None)?;
            self.skip_space();
            self.expect(".")?;
        }
    }
}

fn line_term(reader: &mut Reader<'_>) -> RdfResult<Term> {
    reader.skip_space();
    match reader.peek() {
        Some('<') => Ok(Term::Iri(reader.iri_reference()?)),
        Some('_') => reader.blank_label(),
        Some('"') => reader.literal(),
        _ => Err(syntax(reader.at, "expected an IRI, a blank node or a literal")),
    }
}

fn quads_of_lines(text: &str, with_graph: bool) -> RdfResult<Vec<Quad>> {
    let mut reader = Reader {
        text,
        at: 0,
        base: String::new(),
        prefixes: HashMap::new(),
        fresh: 0,
        depth: 0,
        quads: Vec::new(),
    };
    loop {
        reader.skip_space();
        if reader.peek().is_none() {
            return Ok(reader.quads);
        }
        let subject = line_term(&mut reader)?;
        let predicate = line_term(&mut reader)?;
        let object = line_term(&mut reader)?;
        reader.skip_space();
        let graph = if with_graph && reader.peek() != Some('.') {
            Some(line_term(&mut reader)?)
        } else {
            None
        };
        reader.skip_space();
        reader.expect(".")?;
        if !matches!(subject, Term::Iri(_) | Term::Blank(_)) || !matches!(predicate, Term::Iri(_)) {
            return Err(syntax(reader.at, "a subject must be an IRI or blank node and a predicate an IRI"));
        }
        reader.quads.push(Quad {
            subject,
            predicate,
            object,
            graph,
        });
    }
}

/// The quads of `text` in `format`, with relative IRIs resolved against `base`; triples
/// outside a graph block are in the default graph. Blank node labels are the document's own.
pub fn parse(text: &str, format: Format, base: &str) -> RdfResult<Vec<Quad>> {
    match format {
        Format::NTriples => quads_of_lines(text, false),
        Format::NQuads => quads_of_lines(text, true),
        Format::Turtle | Format::TriG => {
            let mut reader = Reader {
                text,
                at: 0,
                base: base.to_owned(),
                prefixes: HashMap::new(),
                fresh: 0,
                depth: 0,
                quads: Vec::new(),
            };
            reader.document(format == Format::TriG)?;
            Ok(reader.quads)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turtle_reads_prefixes_lists_and_shorthands() {
        let quads = parse(
            r#"@prefix ex: <http://example.org/> .
               @base <http://example.org/base/> .
               ex:alice a ex:Person ;
                   ex:name "Alice"@en, 'Alicia' ;
                   ex:age 42 ;
                   ex:height 1.68 ;
                   ex:knows [ ex:name """Bob
                   the builder""" ] ;
                   ex:likes ( ex:tea ex:cake ) ;
                   ex:page <doc#top> ;
                   ex:active true .
            "#,
            Format::Turtle,
            "",
        )
        .unwrap();
        let alice = Term::iri("http://example.org/alice");
        assert!(quads.contains(&Quad {
            subject: alice.clone(),
            predicate: Term::iri(RDF_TYPE),
            object: Term::iri("http://example.org/Person"),
            graph: None,
        }));
        assert!(quads.iter().any(|quad| quad.object == Term::Literal(Literal::tagged("Alice", "en"))));
        assert!(quads.iter().any(|quad| quad.object == Term::Literal(Literal::typed("42", XSD_INTEGER))));
        assert!(quads.iter().any(|quad| quad.object == Term::Literal(Literal::typed("1.68", XSD_DECIMAL))));
        assert!(quads.iter().any(|quad| quad.object == Term::iri("http://example.org/base/doc#top")));
        assert_eq!(quads.iter().filter(|quad| quad.predicate == Term::iri(RDF_FIRST)).count(), 2);
        assert_eq!(quads.len(), 14);
    }

    #[test]
    fn trig_and_nquads_read_named_graphs() {
        let trig = parse(
            "PREFIX ex: <http://example.org/>\nex:g1 { ex:a ex:b ex:c . ex:a ex:b ex:d }\nGRAPH ex:g2 { ex:x ex:y ex:z }\n{ ex:p ex:q ex:r }",
            Format::TriG,
            "",
        )
        .unwrap();
        assert_eq!(trig.len(), 4);
        assert_eq!(trig[0].graph, Some(Term::iri("http://example.org/g1")));
        assert_eq!(trig[3].graph, None);
        let lines = parse(
            "<http://a> <http://b> \"c\\n\" .\n# comment\n_:x <http://b> <http://c> <http://g> .\n",
            Format::NQuads,
            "",
        )
        .unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].object, Term::Literal(Literal::string("c\n")));
        assert_eq!(lines[1].graph, Some(Term::iri("http://g")));
    }

    #[test]
    fn relative_iris_resolve_as_rfc_3986_says() {
        let base = "http://a/b/c/d;p?q";
        for (relative, expected) in [
            ("g", "http://a/b/c/g"),
            ("./g", "http://a/b/c/g"),
            ("g/", "http://a/b/c/g/"),
            ("/g", "http://a/g"),
            ("//g", "http://g"),
            ("?y", "http://a/b/c/d;p?y"),
            ("#s", "http://a/b/c/d;p?q#s"),
            ("../g", "http://a/b/g"),
            ("../..", "http://a/"),
            ("mailto:x@y", "mailto:x@y"),
        ] {
            assert_eq!(resolve(base, relative), expected, "{relative}");
        }
    }

    #[test]
    fn broken_documents_fail_without_panicking() {
        for (text, format) in [
            ("ex:a ex:b ex:c .", Format::Turtle),
            ("<a> <b> \"open", Format::Turtle),
            ("<a> <b> .", Format::NTriples),
            ("\"lit\" <b> <c> .", Format::NTriples),
            ("<a> <b> <c>", Format::Turtle),
            ("GRAPH { }", Format::TriG),
            ("<a> <b> \"\\u12\" .", Format::Turtle),
        ] {
            assert!(parse(text, format, "").is_err(), "{text}");
        }
        let deep = format!("<a> <b> {}<c>{} .", "[ <p> ".repeat(100), " ]".repeat(100));
        assert!(parse(&deep, Format::Turtle, "").is_err());
    }
}
