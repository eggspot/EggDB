//! A reader for SPARQL 1.1 queries and updates, written from the W3C recommendations' grammars
//! (SPARQL 1.1 Query Language, section 19; SPARQL 1.1 Update, section 3), translating group
//! graph patterns into the algebra as section 18.2 describes.

use crate::sparql_ast::{
    AggregateKind, Arithmetic, Comparison, Dataset, Expr, GraphPattern, GraphTarget, Node, OrderKey,
    Path, Projection, QuadPattern, Query, QueryForm, TransferKind, TriplePattern, UpdateOperation, Verb,
};
use crate::syntax::resolve;
use crate::term::{
    Literal, RDF_FIRST, RDF_NIL, RDF_REST, RDF_TYPE, Term, XSD_BOOLEAN, XSD_DECIMAL, XSD_DOUBLE, XSD_INTEGER,
};
use crate::{RdfError, RdfResult};
use std::collections::HashMap;

const MAX_DEPTH: usize = 64;
const MAX_TEXT_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Iri(String),
    Prefixed(String, String),
    Blank(String),
    Variable(String),
    Text(String),
    Language(String),
    Integer(String),
    Decimal(String),
    Double(String),
    Word(String),
    Symbol(&'static str),
    End,
}

const SYMBOLS: [&str; 26] = [
    "^^", "&&", "||", "!=", "<=", ">=", "{", "}", "(", ")", "[", "]", ".", ",", ";", "=", "<", ">", "!", "+", "-",
    "*", "/", "^", "|", "?",
];

fn syntax(at: usize, message: &str) -> RdfError {
    RdfError::syntax(format!("{message} at byte {at}"))
}

fn is_name_char(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '-' | '.' | ':' | '%' | '\\')
}

fn escape(text: &str, at: &mut usize, start: usize) -> RdfResult<char> {
    let Some(kind) = text.get(*at..).and_then(|rest| rest.chars().next()) else {
        return Err(syntax(start, "an escape at the end"));
    };
    *at += kind.len_utf8();
    let digits = match kind {
        't' => return Ok('\t'),
        'b' => return Ok('\u{8}'),
        'n' => return Ok('\n'),
        'r' => return Ok('\r'),
        'f' => return Ok('\u{c}'),
        '"' => return Ok('"'),
        '\'' => return Ok('\''),
        '\\' => return Ok('\\'),
        'u' => 4,
        'U' => 8,
        _ => return Err(syntax(start, "an unknown escape")),
    };
    let hex = text.get(*at..*at + digits).ok_or_else(|| syntax(start, "a short \\u escape"))?;
    let code = u32::from_str_radix(hex, 16).map_err(|_| syntax(start, "a \\u escape that is not hexadecimal"))?;
    *at += digits;
    char::from_u32(code).ok_or_else(|| syntax(start, "a \\u escape that is not a character"))
}

fn tokens(text: &str) -> RdfResult<Vec<(Token, usize)>> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(RdfError::invalid("the request is longer than 1 MiB"));
    }
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let Some(character) = text.get(at..).and_then(|rest| rest.chars().next()) else {
            break;
        };
        if character.is_whitespace() {
            at += character.len_utf8();
            continue;
        }
        if character == '#' {
            at += text[at..].find(['\n', '\r']).unwrap_or(text.len() - at);
            continue;
        }
        let start = at;
        let token = match character {
            '<' => {
                let rest = &text[at + 1..];
                let end = rest.find(|found: char| matches!(found, '>' | '<' | '"' | '{' | '}' | '|' | '^' | '`') || found <= ' ');
                if let Some(end) = end
                    && rest.as_bytes().get(end) == Some(&b'>')
                {
                    let mut iri = String::new();
                    let mut inner = 0;
                    let raw = &rest[..end];
                    while inner < raw.len() {
                        let Some(next) = raw[inner..].chars().next() else {
                            break;
                        };
                        inner += next.len_utf8();
                        if next == '\\' {
                            iri.push(escape(raw, &mut inner, start)?);
                        } else {
                            iri.push(next);
                        }
                    }
                    at += end + 2;
                    Token::Iri(iri)
                } else if rest.starts_with('=') {
                    at += 2;
                    Token::Symbol("<=")
                } else {
                    at += 1;
                    Token::Symbol("<")
                }
            }
            '?' | '$' => {
                let rest = &text[at + 1..];
                let length = rest.find(|found: char| !(found.is_alphanumeric() || found == '_')).unwrap_or(rest.len());
                if length == 0 {
                    if character == '$' {
                        return Err(syntax(start, "a variable without a name"));
                    }
                    at += 1;
                    Token::Symbol("?")
                } else {
                    at += 1 + length;
                    Token::Variable(rest[..length].to_owned())
                }
            }
            '"' | '\'' => {
                let quote = if text[at..].starts_with("\"\"\"") {
                    "\"\"\""
                } else if text[at..].starts_with("'''") {
                    "'''"
                } else if character == '"' {
                    "\""
                } else {
                    "'"
                };
                at += quote.len();
                let mut value = String::new();
                loop {
                    if text[at..].starts_with(quote) {
                        at += quote.len();
                        break;
                    }
                    let Some(next) = text.get(at..).and_then(|rest| rest.chars().next()) else {
                        return Err(syntax(start, "an unterminated string"));
                    };
                    at += next.len_utf8();
                    match next {
                        '\\' => value.push(escape(text, &mut at, start)?),
                        '\n' | '\r' if quote.len() == 1 => return Err(syntax(start, "a line break in a short string")),
                        other => value.push(other),
                    }
                }
                Token::Text(value)
            }
            '@' => {
                let rest = &text[at + 1..];
                let length = rest.find(|found: char| !(found.is_ascii_alphanumeric() || found == '-')).unwrap_or(rest.len());
                if length == 0 {
                    return Err(syntax(start, "an empty language tag"));
                }
                at += 1 + length;
                Token::Language(rest[..length].to_ascii_lowercase())
            }
            '_' if text[at..].starts_with("_:") => {
                let rest = &text[at + 2..];
                let mut length = rest.find(|found: char| !(found.is_alphanumeric() || matches!(found, '_' | '-' | '.'))).unwrap_or(rest.len());
                while length > 0 && rest[..length].ends_with('.') {
                    length -= 1;
                }
                if length == 0 {
                    return Err(syntax(start, "a blank node without a label"));
                }
                at += 2 + length;
                Token::Blank(rest[..length].to_owned())
            }
            '0'..='9' | '.' if character != '.' || text[at + 1..].starts_with(|next: char| next.is_ascii_digit()) => {
                let rest = &text[at..];
                let mut length = rest.find(|found: char| !found.is_ascii_digit()).unwrap_or(rest.len());
                let mut kind = 0;
                if rest[length..].starts_with('.') && rest[length + 1..].starts_with(|next: char| next.is_ascii_digit()) {
                    length += 1;
                    length += rest[length..].find(|found: char| !found.is_ascii_digit()).unwrap_or(rest.len() - length);
                    kind = 1;
                }
                if rest[length..].starts_with(['e', 'E']) {
                    let mut exponent = length + 1;
                    if rest[exponent..].starts_with(['+', '-']) {
                        exponent += 1;
                    }
                    let digits = rest[exponent..].find(|found: char| !found.is_ascii_digit()).unwrap_or(rest.len() - exponent);
                    if digits > 0 {
                        length = exponent + digits;
                        kind = 2;
                    }
                }
                at += length;
                let number = rest[..length].to_owned();
                match kind {
                    0 => Token::Integer(number),
                    1 => Token::Decimal(number),
                    _ => Token::Double(number),
                }
            }
            _ if character.is_alphabetic() || character == ':' => {
                let rest = &text[at..];
                let mut length = 0;
                let mut chars = rest.char_indices();
                while let Some((offset, next)) = chars.next() {
                    if next == '\\' {
                        chars.next();
                        continue;
                    }
                    if !is_name_char(next) {
                        length = offset;
                        break;
                    }
                    length = offset + next.len_utf8();
                }
                while length > 0 && rest[..length].ends_with('.') {
                    length -= 1;
                }
                let name = &rest[..length];
                at += length;
                match name.split_once(':') {
                    Some((prefix, local)) => {
                        let mut unescaped = String::new();
                        let mut escaped = false;
                        for next in local.chars() {
                            if escaped || next != '\\' {
                                unescaped.push(next);
                                escaped = false;
                            } else {
                                escaped = true;
                            }
                        }
                        Token::Prefixed(prefix.to_owned(), unescaped)
                    }
                    None => Token::Word(name.to_owned()),
                }
            }
            _ => {
                let rest = &text[at..];
                let Some(symbol) = SYMBOLS.iter().find(|symbol| rest.starts_with(**symbol)) else {
                    return Err(syntax(start, "an unexpected character"));
                };
                at += symbol.len();
                Token::Symbol(symbol)
            }
        };
        found.push((token, start));
    }
    found.push((Token::End, text.len()));
    Ok(found)
}

struct Parser {
    tokens: Vec<(Token, usize)>,
    at: usize,
    base: String,
    prefixes: HashMap<String, String>,
    depth: usize,
    fresh: usize,
    allow_blanks_as_variables: bool,
}

impl Parser {
    fn peek(&self) -> &Token {
        self.tokens.get(self.at).map_or(&Token::End, |(token, _)| token)
    }

    fn peek_at(&self, ahead: usize) -> &Token {
        self.tokens.get(self.at + ahead).map_or(&Token::End, |(token, _)| token)
    }

    fn position(&self) -> usize {
        self.tokens.get(self.at).map_or(0, |(_, at)| *at)
    }

    fn advance(&mut self) -> Token {
        let token = self.peek().clone();
        self.at += 1;
        token
    }

    fn unexpected(&self) -> RdfError {
        syntax(self.position(), &format!("unexpected {:?}", self.peek()))
    }

    fn is_word(&self, word: &str) -> bool {
        matches!(self.peek(), Token::Word(found) if found.eq_ignore_ascii_case(word))
    }

    fn is_word_at(&self, ahead: usize, word: &str) -> bool {
        matches!(self.peek_at(ahead), Token::Word(found) if found.eq_ignore_ascii_case(word))
    }

    fn take_word(&mut self, word: &str) -> bool {
        let found = self.is_word(word);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect_word(&mut self, word: &str) -> RdfResult<()> {
        if self.take_word(word) {
            return Ok(());
        }
        Err(syntax(self.position(), &format!("expected {}", word.to_ascii_uppercase())))
    }

    fn is(&self, symbol: &str) -> bool {
        matches!(self.peek(), Token::Symbol(found) if *found == symbol)
    }

    fn take(&mut self, symbol: &str) -> bool {
        let found = self.is(symbol);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect(&mut self, symbol: &str) -> RdfResult<()> {
        if self.take(symbol) {
            return Ok(());
        }
        Err(syntax(self.position(), &format!("expected '{symbol}'")))
    }

    fn deeper(&mut self) -> RdfResult<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(syntax(self.position(), "the request nests too deeply"));
        }
        Ok(())
    }

    fn shallower(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn fresh_variable(&mut self) -> String {
        self.fresh += 1;
        format!("_:{}", self.fresh)
    }

    fn prologue(&mut self) -> RdfResult<()> {
        loop {
            if self.take_word("base") {
                let Token::Iri(iri) = self.advance() else {
                    return Err(syntax(self.position(), "BASE needs an IRI"));
                };
                self.base = resolve(&self.base, &iri);
            } else if self.take_word("prefix") {
                let Token::Prefixed(prefix, local) = self.advance() else {
                    return Err(syntax(self.position(), "PREFIX needs a name ending in ':'"));
                };
                if !local.is_empty() {
                    return Err(syntax(self.position(), "PREFIX needs a name ending in ':'"));
                }
                let Token::Iri(iri) = self.advance() else {
                    return Err(syntax(self.position(), "PREFIX needs an IRI"));
                };
                let namespace = resolve(&self.base, &iri);
                self.prefixes.insert(prefix, namespace);
            } else {
                return Ok(());
            }
        }
    }

    fn iri_of(&self, token: &Token) -> RdfResult<Option<String>> {
        Ok(match token {
            Token::Iri(iri) => Some(resolve(&self.base, iri)),
            Token::Prefixed(prefix, local) => {
                let namespace = self
                    .prefixes
                    .get(prefix)
                    .ok_or_else(|| syntax(self.position(), &format!("the prefix '{prefix}:' is not declared")))?;
                Some(format!("{namespace}{local}"))
            }
            _ => None,
        })
    }

    fn iri(&mut self) -> RdfResult<String> {
        let token = self.peek().clone();
        match self.iri_of(&token)? {
            Some(iri) => {
                self.at += 1;
                Ok(iri)
            }
            None => Err(syntax(self.position(), "expected an IRI")),
        }
    }

    fn literal(&mut self, lexical: String) -> RdfResult<Term> {
        if let Token::Language(language) = self.peek().clone() {
            self.at += 1;
            return Ok(Term::Literal(Literal::tagged(&lexical, &language)));
        }
        if self.take("^^") {
            let datatype = self.iri()?;
            return Ok(Term::Literal(Literal::typed(&lexical, &datatype)));
        }
        Ok(Term::Literal(Literal::string(&lexical)))
    }

    fn number(&mut self, negative: bool) -> RdfResult<Option<Term>> {
        let sign = if negative { "-" } else { "" };
        let term = match self.peek().clone() {
            Token::Integer(text) => Term::Literal(Literal::typed(&format!("{sign}{text}"), XSD_INTEGER)),
            Token::Decimal(text) => Term::Literal(Literal::typed(&format!("{sign}{text}"), XSD_DECIMAL)),
            Token::Double(text) => Term::Literal(Literal::typed(&format!("{sign}{text}"), XSD_DOUBLE)),
            _ => return Ok(None),
        };
        self.at += 1;
        Ok(Some(term))
    }

    fn constant(&mut self) -> RdfResult<Option<Term>> {
        if self.is("-") || self.is("+") {
            let negative = self.is("-");
            if matches!(self.peek_at(1), Token::Integer(_) | Token::Decimal(_) | Token::Double(_)) {
                self.at += 1;
                return self.number(negative);
            }
            return Ok(None);
        }
        if let Some(number) = self.number(false)? {
            return Ok(Some(number));
        }
        let token = self.peek().clone();
        if let Some(iri) = self.iri_of(&token)? {
            self.at += 1;
            return Ok(Some(Term::Iri(iri)));
        }
        match token {
            Token::Text(text) => {
                self.at += 1;
                self.literal(text).map(Some)
            }
            Token::Word(word) if word.eq_ignore_ascii_case("true") || word.eq_ignore_ascii_case("false") => {
                self.at += 1;
                Ok(Some(Term::Literal(Literal::typed(&word.to_ascii_lowercase(), XSD_BOOLEAN))))
            }
            _ => Ok(None),
        }
    }

    fn node(&mut self, triples: &mut Vec<TriplePattern>) -> RdfResult<Node> {
        match self.peek().clone() {
            Token::Variable(name) => {
                self.at += 1;
                Ok(Node::Variable(name))
            }
            Token::Blank(label) => {
                self.at += 1;
                if self.allow_blanks_as_variables {
                    Ok(Node::Variable(format!("_:{label}")))
                } else {
                    Ok(Node::Term(Term::Blank(label)))
                }
            }
            Token::Symbol("[") => self.blank_property_list(triples),
            Token::Symbol("(") => self.collection(triples),
            _ => match self.constant()? {
                Some(term) => Ok(Node::Term(term)),
                None => Err(self.unexpected()),
            },
        }
    }

    fn blank_node(&mut self) -> Node {
        let name = self.fresh_variable();
        if self.allow_blanks_as_variables {
            Node::Variable(name)
        } else {
            Node::Term(Term::Blank(name.trim_start_matches("_:").to_owned()))
        }
    }

    fn blank_property_list(&mut self, triples: &mut Vec<TriplePattern>) -> RdfResult<Node> {
        self.deeper()?;
        self.expect("[")?;
        let node = self.blank_node();
        if !self.take("]") {
            self.property_list(&node, triples)?;
            self.expect("]")?;
        }
        self.shallower();
        Ok(node)
    }

    fn collection(&mut self, triples: &mut Vec<TriplePattern>) -> RdfResult<Node> {
        self.deeper()?;
        self.expect("(")?;
        let mut items = Vec::new();
        while !self.take(")") {
            if *self.peek() == Token::End {
                return Err(syntax(self.position(), "an unterminated collection"));
            }
            items.push(self.node(triples)?);
        }
        self.shallower();
        let mut list = Node::Term(Term::iri(RDF_NIL));
        for item in items.into_iter().rev() {
            let cell = self.blank_node();
            triples.push(TriplePattern {
                subject: cell.clone(),
                verb: Verb::Path(Path::Iri(RDF_FIRST.to_owned())),
                object: item,
            });
            triples.push(TriplePattern {
                subject: cell.clone(),
                verb: Verb::Path(Path::Iri(RDF_REST.to_owned())),
                object: list,
            });
            list = cell;
        }
        Ok(list)
    }

    fn path_primary(&mut self) -> RdfResult<Path> {
        if self.take_word("a") {
            return Ok(Path::Iri(RDF_TYPE.to_owned()));
        }
        if self.take("!") {
            let mut negated = Vec::new();
            let mut one = |parser: &mut Self| -> RdfResult<(String, bool)> {
                let inverse = parser.take("^");
                if parser.take_word("a") {
                    return Ok((RDF_TYPE.to_owned(), inverse));
                }
                Ok((parser.iri()?, inverse))
            };
            if self.take("(") {
                if !self.take(")") {
                    loop {
                        negated.push(one(self)?);
                        if self.take(")") {
                            break;
                        }
                        self.expect("|")?;
                    }
                }
            } else {
                negated.push(one(self)?);
            }
            return Ok(Path::Negated(negated));
        }
        if self.take("(") {
            self.deeper()?;
            let path = self.path()?;
            self.shallower();
            self.expect(")")?;
            return Ok(path);
        }
        Ok(Path::Iri(self.iri()?))
    }

    fn path_element(&mut self) -> RdfResult<Path> {
        let inverse = self.take("^");
        let primary = self.path_primary()?;
        let modified = if self.take("*") {
            Path::ZeroOrMore(Box::new(primary))
        } else if self.take("+") {
            Path::OneOrMore(Box::new(primary))
        } else if self.take("?") {
            Path::ZeroOrOne(Box::new(primary))
        } else {
            primary
        };
        Ok(if inverse { Path::Inverse(Box::new(modified)) } else { modified })
    }

    fn path(&mut self) -> RdfResult<Path> {
        let mut alternatives = Vec::new();
        loop {
            let mut sequence = vec![self.path_element()?];
            while self.take("/") {
                sequence.push(self.path_element()?);
            }
            alternatives.push(if sequence.len() == 1 { sequence.remove(0) } else { Path::Sequence(sequence) });
            if !self.take("|") {
                break;
            }
        }
        Ok(if alternatives.len() == 1 { alternatives.remove(0) } else { Path::Alternative(alternatives) })
    }

    fn verb(&mut self) -> RdfResult<Verb> {
        if let Token::Variable(name) = self.peek().clone() {
            self.at += 1;
            return Ok(Verb::Variable(name));
        }
        Ok(Verb::Path(self.path()?))
    }

    fn property_list(&mut self, subject: &Node, triples: &mut Vec<TriplePattern>) -> RdfResult<()> {
        loop {
            let verb = self.verb()?;
            loop {
                let object = self.node(triples)?;
                triples.push(TriplePattern {
                    subject: subject.clone(),
                    verb: verb.clone(),
                    object,
                });
                if !self.take(",") {
                    break;
                }
            }
            if !self.take(";") {
                return Ok(());
            }
            while self.take(";") {}
            if self.is(".") || self.is("]") || self.is("}") || *self.peek() == Token::End || self.at_pattern_keyword() {
                return Ok(());
            }
        }
    }

    fn at_pattern_keyword(&self) -> bool {
        ["optional", "minus", "filter", "bind", "values", "graph", "service"]
            .iter()
            .any(|word| self.is_word(word))
            || self.is("{")
    }

    fn at_triples_start(&self) -> bool {
        match self.peek() {
            Token::Variable(_)
            | Token::Iri(_)
            | Token::Prefixed(..)
            | Token::Blank(_)
            | Token::Text(_)
            | Token::Integer(_)
            | Token::Decimal(_)
            | Token::Double(_) => true,
            Token::Symbol("[" | "(" | "-" | "+") => true,
            Token::Word(word) => word.eq_ignore_ascii_case("true") || word.eq_ignore_ascii_case("false"),
            _ => false,
        }
    }

    fn triples_block(&mut self, triples: &mut Vec<TriplePattern>) -> RdfResult<()> {
        loop {
            if !self.at_triples_start() {
                return Ok(());
            }
            let empty_list_start = self.is("[") && matches!(self.peek_at(1), Token::Symbol("]"));
            let node_start = self.is("[") || self.is("(");
            let subject = self.node(triples)?;
            let ends = self.is(".") || self.is("}") || self.at_pattern_keyword();
            if !(node_start && !empty_list_start && ends) {
                self.property_list(&subject, triples)?;
            }
            if !self.take(".") {
                return Ok(());
            }
        }
    }

    fn group_graph_pattern(&mut self) -> RdfResult<GraphPattern> {
        self.deeper()?;
        self.expect("{")?;
        let pattern = if self.is_word("select") {
            let query = self.select_query(true)?;
            GraphPattern::Select(Box::new(query))
        } else {
            self.group_body()?
        };
        self.expect("}")?;
        self.shallower();
        Ok(pattern)
    }

    fn join(left: GraphPattern, right: GraphPattern) -> GraphPattern {
        match (&left, &right) {
            (GraphPattern::Triples(empty), _) if empty.is_empty() => right,
            (_, GraphPattern::Triples(empty)) if empty.is_empty() => left,
            (GraphPattern::Triples(first), GraphPattern::Triples(second)) => {
                let mut both = first.clone();
                both.extend(second.iter().cloned());
                GraphPattern::Triples(both)
            }
            _ => GraphPattern::Join(Box::new(left), Box::new(right)),
        }
    }

    fn group_body(&mut self) -> RdfResult<GraphPattern> {
        let mut pattern = GraphPattern::empty();
        let mut filters: Vec<Expr> = Vec::new();
        loop {
            let mut triples = Vec::new();
            self.triples_block(&mut triples)?;
            if !triples.is_empty() {
                pattern = Self::join(pattern, GraphPattern::Triples(triples));
            }
            if self.take_word("optional") {
                let inner = self.group_graph_pattern()?;
                pattern = match inner {
                    GraphPattern::Filter(condition, inner) => GraphPattern::Optional(Box::new(pattern), inner, Some(condition)),
                    other => GraphPattern::Optional(Box::new(pattern), Box::new(other), None),
                };
            } else if self.take_word("minus") {
                let inner = self.group_graph_pattern()?;
                pattern = GraphPattern::Minus(Box::new(pattern), Box::new(inner));
            } else if self.take_word("graph") {
                let name = match self.peek().clone() {
                    Token::Variable(name) => {
                        self.at += 1;
                        Node::Variable(name)
                    }
                    _ => Node::Term(Term::Iri(self.iri()?)),
                };
                let inner = self.group_graph_pattern()?;
                pattern = Self::join(pattern, GraphPattern::Graph(name, Box::new(inner)));
            } else if self.take_word("service") {
                return Err(RdfError::not_supported("SERVICE"));
            } else if self.take_word("filter") {
                filters.push(self.constraint()?);
            } else if self.take_word("bind") {
                self.expect("(")?;
                let expr = self.expression()?;
                self.expect_word("as")?;
                let Token::Variable(name) = self.advance() else {
                    return Err(syntax(self.position(), "BIND needs AS ?variable"));
                };
                self.expect(")")?;
                pattern = GraphPattern::Bind(Box::new(pattern), name, expr);
            } else if self.take_word("values") {
                let values = self.values_block()?;
                pattern = Self::join(pattern, values);
            } else if self.is("{") {
                let mut combined = self.group_graph_pattern()?;
                while self.take_word("union") {
                    let next = self.group_graph_pattern()?;
                    combined = GraphPattern::Union(Box::new(combined), Box::new(next));
                }
                pattern = Self::join(pattern, combined);
            } else {
                break;
            }
            self.take(".");
        }
        let mut conditions = filters.into_iter();
        if let Some(first) = conditions.next() {
            let condition = conditions.fold(first, |left, right| Expr::And(Box::new(left), Box::new(right)));
            pattern = GraphPattern::Filter(condition, Box::new(pattern));
        }
        Ok(pattern)
    }

    fn values_block(&mut self) -> RdfResult<GraphPattern> {
        let mut variables = Vec::new();
        let single = if let Token::Variable(name) = self.peek().clone() {
            self.at += 1;
            variables.push(name);
            true
        } else {
            self.expect("(")?;
            while let Token::Variable(name) = self.peek().clone() {
                self.at += 1;
                variables.push(name);
            }
            self.expect(")")?;
            false
        };
        self.expect("{")?;
        let mut rows = Vec::new();
        while !self.take("}") {
            let mut row = Vec::new();
            if single {
                row.push(self.data_value()?);
            } else {
                self.expect("(")?;
                while !self.take(")") {
                    row.push(self.data_value()?);
                }
            }
            if row.len() != variables.len() {
                return Err(syntax(self.position(), "a VALUES row has the wrong number of values"));
            }
            rows.push(row);
        }
        Ok(GraphPattern::Values(variables, rows))
    }

    fn data_value(&mut self) -> RdfResult<Option<Term>> {
        if self.take_word("undef") {
            return Ok(None);
        }
        match self.constant()? {
            Some(term) => Ok(Some(term)),
            None => Err(self.unexpected()),
        }
    }

    fn constraint(&mut self) -> RdfResult<Expr> {
        if self.is("(") {
            self.at += 1;
            let expr = self.expression()?;
            self.expect(")")?;
            return Ok(expr);
        }
        self.primary()
    }

    fn expression(&mut self) -> RdfResult<Expr> {
        self.deeper()?;
        let mut left = self.and_expression()?;
        while self.take("||") {
            let right = self.and_expression()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        self.shallower();
        Ok(left)
    }

    fn and_expression(&mut self) -> RdfResult<Expr> {
        let mut left = self.relational()?;
        while self.take("&&") {
            let right = self.relational()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn relational(&mut self) -> RdfResult<Expr> {
        let left = self.additive()?;
        let comparison = [
            ("=", Comparison::Equal),
            ("!=", Comparison::NotEqual),
            ("<=", Comparison::LessOrEqual),
            (">=", Comparison::GreaterOrEqual),
            ("<", Comparison::Less),
            (">", Comparison::Greater),
        ]
        .into_iter()
        .find(|(symbol, _)| self.is(symbol));
        if let Some((_, comparison)) = comparison {
            self.at += 1;
            let right = self.additive()?;
            return Ok(Expr::Compare(comparison, Box::new(left), Box::new(right)));
        }
        let negated = self.is_word("not") && self.is_word_at(1, "in");
        if negated {
            self.at += 1;
        }
        if self.take_word("in") {
            let list = self.argument_list()?;
            return Ok(Expr::In(Box::new(left), list, negated));
        }
        Ok(left)
    }

    fn additive(&mut self) -> RdfResult<Expr> {
        let mut left = self.multiplicative()?;
        loop {
            let operator = if self.take("+") {
                Arithmetic::Add
            } else if self.take("-") {
                Arithmetic::Subtract
            } else {
                return Ok(left);
            };
            let right = self.multiplicative()?;
            left = Expr::Arithmetic(operator, Box::new(left), Box::new(right));
        }
    }

    fn multiplicative(&mut self) -> RdfResult<Expr> {
        let mut left = self.unary()?;
        loop {
            let operator = if self.take("*") {
                Arithmetic::Multiply
            } else if self.take("/") {
                Arithmetic::Divide
            } else {
                return Ok(left);
            };
            let right = self.unary()?;
            left = Expr::Arithmetic(operator, Box::new(left), Box::new(right));
        }
    }

    fn unary(&mut self) -> RdfResult<Expr> {
        if self.take("!") {
            self.deeper()?;
            let inner = self.unary()?;
            self.shallower();
            return Ok(Expr::Not(Box::new(inner)));
        }
        if self.take("-") {
            self.deeper()?;
            let inner = self.unary()?;
            self.shallower();
            return Ok(Expr::Negate(Box::new(inner)));
        }
        if self.take("+") {
            return self.unary();
        }
        self.primary()
    }

    fn argument_list(&mut self) -> RdfResult<Vec<Expr>> {
        self.expect("(")?;
        let mut arguments = Vec::new();
        if self.take(")") {
            return Ok(arguments);
        }
        loop {
            arguments.push(self.expression()?);
            if self.take(")") {
                return Ok(arguments);
            }
            self.expect(",")?;
        }
    }

    fn aggregate(&mut self, kind: AggregateKind) -> RdfResult<Expr> {
        self.expect("(")?;
        let distinct = self.take_word("distinct");
        let argument = if kind == AggregateKind::Count && self.take("*") {
            None
        } else {
            Some(Box::new(self.expression()?))
        };
        let mut separator = None;
        if kind == AggregateKind::GroupConcat && self.take(";") {
            self.expect_word("separator")?;
            self.expect("=")?;
            let Token::Text(text) = self.advance() else {
                return Err(syntax(self.position(), "SEPARATOR needs a string"));
            };
            separator = Some(text);
        }
        self.expect(")")?;
        Ok(Expr::Aggregate {
            kind,
            distinct,
            argument,
            separator,
        })
    }

    fn primary(&mut self) -> RdfResult<Expr> {
        match self.peek().clone() {
            Token::Symbol("(") => {
                self.at += 1;
                let inner = self.expression()?;
                self.expect(")")?;
                Ok(inner)
            }
            Token::Variable(name) => {
                self.at += 1;
                Ok(Expr::Variable(name))
            }
            Token::Word(word) => {
                let lowered = word.to_ascii_lowercase();
                if lowered == "not" && self.is_word_at(1, "exists") {
                    self.at += 2;
                    let pattern = self.group_graph_pattern()?;
                    return Ok(Expr::Exists(Box::new(pattern), true));
                }
                if lowered == "exists" {
                    self.at += 1;
                    let pattern = self.group_graph_pattern()?;
                    return Ok(Expr::Exists(Box::new(pattern), false));
                }
                let aggregate = match lowered.as_str() {
                    "count" => Some(AggregateKind::Count),
                    "sum" => Some(AggregateKind::Sum),
                    "min" => Some(AggregateKind::Min),
                    "max" => Some(AggregateKind::Max),
                    "avg" => Some(AggregateKind::Avg),
                    "sample" => Some(AggregateKind::Sample),
                    "group_concat" => Some(AggregateKind::GroupConcat),
                    _ => None,
                };
                if let Some(kind) = aggregate
                    && self.peek_at(1) == &Token::Symbol("(")
                {
                    self.at += 1;
                    return self.aggregate(kind);
                }
                if lowered == "true" || lowered == "false" {
                    self.at += 1;
                    return Ok(Expr::Constant(Term::Literal(Literal::typed(&lowered, XSD_BOOLEAN))));
                }
                if self.peek_at(1) == &Token::Symbol("(") {
                    self.at += 1;
                    if lowered == "bound" {
                        self.expect("(")?;
                        let Token::Variable(name) = self.advance() else {
                            return Err(syntax(self.position(), "BOUND needs a variable"));
                        };
                        self.expect(")")?;
                        return Ok(Expr::Call("bound".to_owned(), vec![Expr::Variable(name)]));
                    }
                    let arguments = self.argument_list()?;
                    return Ok(Expr::Call(lowered, arguments));
                }
                if matches!(lowered.as_str(), "now" | "rand" | "uuid" | "struuid" | "bnode") {
                    self.at += 1;
                    return Ok(Expr::Call(lowered, Vec::new()));
                }
                Err(self.unexpected())
            }
            Token::Iri(_) | Token::Prefixed(..) if self.peek_at(1) == &Token::Symbol("(") => {
                let iri = self.iri()?;
                let arguments = self.argument_list()?;
                Ok(Expr::Call(iri, arguments))
            }
            _ => match self.constant()? {
                Some(term) => Ok(Expr::Constant(term)),
                None => Err(self.unexpected()),
            },
        }
    }

    fn dataset(&mut self) -> RdfResult<Option<Dataset>> {
        let mut dataset = Dataset::default();
        let mut any = false;
        while self.take_word("from") {
            any = true;
            if self.take_word("named") {
                dataset.named.push(self.iri()?);
            } else {
                dataset.default.push(self.iri()?);
            }
        }
        Ok(any.then_some(dataset))
    }

    fn solution_modifiers(&mut self, query: &mut Query) -> RdfResult<()> {
        if self.is_word("group") && self.is_word_at(1, "by") {
            self.at += 2;
            loop {
                let (expr, name) = match self.peek().clone() {
                    Token::Variable(name) => {
                        self.at += 1;
                        (Expr::Variable(name.clone()), name)
                    }
                    Token::Symbol("(") => {
                        self.at += 1;
                        let expr = self.expression()?;
                        let name = if self.take_word("as") {
                            let Token::Variable(name) = self.advance() else {
                                return Err(syntax(self.position(), "AS needs a variable"));
                            };
                            name
                        } else {
                            self.fresh_variable()
                        };
                        self.expect(")")?;
                        (expr, name)
                    }
                    Token::Word(_) | Token::Iri(_) | Token::Prefixed(..) if self.peek_at(1) == &Token::Symbol("(") => {
                        (self.primary()?, self.fresh_variable())
                    }
                    _ => break,
                };
                query.group_by.push(Projection { expr, name });
            }
            if query.group_by.is_empty() {
                return Err(syntax(self.position(), "GROUP BY needs a condition"));
            }
        }
        if self.take_word("having") {
            while self.is("(")
                || (matches!(self.peek(), Token::Word(_) | Token::Iri(_) | Token::Prefixed(..))
                    && self.peek_at(1) == &Token::Symbol("("))
            {
                query.having.push(self.constraint()?);
            }
        }
        if self.is_word("order") && self.is_word_at(1, "by") {
            self.at += 2;
            loop {
                let descending = if self.take_word("desc") {
                    true
                } else {
                    self.take_word("asc");
                    false
                };
                let expr = match self.peek().clone() {
                    Token::Variable(name) => {
                        self.at += 1;
                        Expr::Variable(name)
                    }
                    Token::Symbol("(") => self.constraint()?,
                    Token::Word(_) | Token::Iri(_) | Token::Prefixed(..) if self.peek_at(1) == &Token::Symbol("(") => {
                        self.constraint()?
                    }
                    _ => break,
                };
                query.order_by.push(OrderKey { expr, descending });
            }
        }
        loop {
            if self.take_word("limit") {
                let Token::Integer(text) = self.advance() else {
                    return Err(syntax(self.position(), "LIMIT needs a whole number"));
                };
                query.limit = Some(text.parse().map_err(|_| syntax(self.position(), "LIMIT is too large"))?);
            } else if self.take_word("offset") {
                let Token::Integer(text) = self.advance() else {
                    return Err(syntax(self.position(), "OFFSET needs a whole number"));
                };
                query.offset = text.parse().map_err(|_| syntax(self.position(), "OFFSET is too large"))?;
            } else {
                return Ok(());
            }
        }
    }

    fn select_query(&mut self, nested: bool) -> RdfResult<Query> {
        self.expect_word("select")?;
        let distinct = self.take_word("distinct") || self.take_word("reduced");
        let projection = if self.take("*") {
            None
        } else {
            let mut columns = Vec::new();
            loop {
                match self.peek().clone() {
                    Token::Variable(name) => {
                        self.at += 1;
                        columns.push(Projection {
                            expr: Expr::Variable(name.clone()),
                            name,
                        });
                    }
                    Token::Symbol("(") => {
                        self.at += 1;
                        let expr = self.expression()?;
                        self.expect_word("as")?;
                        let Token::Variable(name) = self.advance() else {
                            return Err(syntax(self.position(), "AS needs a variable"));
                        };
                        self.expect(")")?;
                        columns.push(Projection { expr, name });
                    }
                    _ => break,
                }
            }
            if columns.is_empty() {
                return Err(syntax(self.position(), "SELECT needs * or variables"));
            }
            Some(columns)
        };
        let dataset = if nested { None } else { self.dataset()? };
        self.take_word("where");
        let pattern = self.group_graph_pattern()?;
        let mut query = Query {
            form: QueryForm::Select { distinct, projection },
            dataset,
            pattern,
            group_by: Vec::new(),
            having: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: 0,
        };
        self.solution_modifiers(&mut query)?;
        if nested && self.take_word("values") {
            let values = self.values_block()?;
            query.pattern = Self::join(query.pattern, values);
        }
        Ok(query)
    }

    fn template(&mut self) -> RdfResult<Vec<TriplePattern>> {
        self.expect("{")?;
        let mut triples = Vec::new();
        self.triples_block(&mut triples)?;
        self.expect("}")?;
        Ok(triples)
    }

    fn query(&mut self) -> RdfResult<Query> {
        self.prologue()?;
        let mut query = if self.is_word("select") {
            self.select_query(false)?
        } else if self.take_word("ask") {
            let dataset = self.dataset()?;
            self.take_word("where");
            let pattern = self.group_graph_pattern()?;
            let mut query = Query {
                form: QueryForm::Ask,
                dataset,
                pattern,
                group_by: Vec::new(),
                having: Vec::new(),
                order_by: Vec::new(),
                limit: None,
                offset: 0,
            };
            self.solution_modifiers(&mut query)?;
            query
        } else if self.take_word("construct") {
            let (template, dataset, pattern) = if self.is("{") {
                let template = self.template()?;
                let dataset = self.dataset()?;
                self.take_word("where");
                (template, dataset, self.group_graph_pattern()?)
            } else {
                let dataset = self.dataset()?;
                self.expect_word("where")?;
                let template = self.template()?;
                (template.clone(), dataset, GraphPattern::Triples(template))
            };
            let mut query = Query {
                form: QueryForm::Construct(template),
                dataset,
                pattern,
                group_by: Vec::new(),
                having: Vec::new(),
                order_by: Vec::new(),
                limit: None,
                offset: 0,
            };
            self.solution_modifiers(&mut query)?;
            query
        } else if self.take_word("describe") {
            let mut described = Vec::new();
            if !self.take("*") {
                loop {
                    match self.peek().clone() {
                        Token::Variable(name) => {
                            self.at += 1;
                            described.push(Node::Variable(name));
                        }
                        Token::Iri(_) | Token::Prefixed(..) => described.push(Node::Term(Term::Iri(self.iri()?))),
                        _ => break,
                    }
                }
            }
            let dataset = self.dataset()?;
            let pattern = if self.take_word("where") || self.is("{") {
                self.group_graph_pattern()?
            } else {
                GraphPattern::empty()
            };
            let mut query = Query {
                form: QueryForm::Describe(described),
                dataset,
                pattern,
                group_by: Vec::new(),
                having: Vec::new(),
                order_by: Vec::new(),
                limit: None,
                offset: 0,
            };
            self.solution_modifiers(&mut query)?;
            query
        } else {
            return Err(syntax(self.position(), "expected SELECT, ASK, CONSTRUCT or DESCRIBE"));
        };
        if self.take_word("values") {
            let values = self.values_block()?;
            query.pattern = Self::join(query.pattern, values);
        }
        if *self.peek() != Token::End {
            return Err(self.unexpected());
        }
        Ok(query)
    }

    fn quad_patterns(&mut self) -> RdfResult<Vec<QuadPattern>> {
        self.expect("{")?;
        let mut quads = Vec::new();
        loop {
            let mut triples = Vec::new();
            self.triples_block(&mut triples)?;
            quads.extend(Self::quads_of(triples, None)?);
            if self.take_word("graph") {
                let graph = match self.peek().clone() {
                    Token::Variable(name) => {
                        self.at += 1;
                        Node::Variable(name)
                    }
                    _ => Node::Term(Term::Iri(self.iri()?)),
                };
                self.expect("{")?;
                let mut inner = Vec::new();
                self.triples_block(&mut inner)?;
                self.expect("}")?;
                quads.extend(Self::quads_of(inner, Some(graph))?);
                self.take(".");
                continue;
            }
            break;
        }
        self.expect("}")?;
        Ok(quads)
    }

    fn quads_of(triples: Vec<TriplePattern>, graph: Option<Node>) -> RdfResult<Vec<QuadPattern>> {
        triples
            .into_iter()
            .map(|triple| {
                let predicate = match triple.verb {
                    Verb::Variable(name) => Node::Variable(name),
                    Verb::Path(Path::Iri(iri)) => Node::Term(Term::Iri(iri)),
                    Verb::Path(_) => return Err(RdfError::syntax("property paths are not allowed in update templates")),
                };
                Ok(QuadPattern {
                    subject: triple.subject,
                    predicate,
                    object: triple.object,
                    graph: graph.clone(),
                })
            })
            .collect()
    }

    fn graph_target(&mut self, allow_all: bool) -> RdfResult<GraphTarget> {
        if self.take_word("default") {
            return Ok(GraphTarget::Default);
        }
        if allow_all && self.take_word("named") {
            return Ok(GraphTarget::AllNamed);
        }
        if allow_all && self.take_word("all") {
            return Ok(GraphTarget::All);
        }
        self.take_word("graph");
        Ok(GraphTarget::Named(Term::Iri(self.iri()?)))
    }

    fn update_operation(&mut self) -> RdfResult<UpdateOperation> {
        if self.take_word("load") {
            return Err(RdfError::not_supported("LOAD (fetching a document by IRI)"));
        }
        if self.take_word("clear") || self.take_word("drop") {
            let silent = self.take_word("silent");
            return Ok(UpdateOperation::Clear(self.graph_target(true)?, silent));
        }
        if self.take_word("create") {
            let silent = self.take_word("silent");
            self.expect_word("graph")?;
            return Ok(UpdateOperation::Create(Term::Iri(self.iri()?), silent));
        }
        let transfer = if self.take_word("add") {
            Some(TransferKind::Add)
        } else if self.take_word("move") {
            Some(TransferKind::Move)
        } else if self.take_word("copy") {
            Some(TransferKind::Copy)
        } else {
            None
        };
        if let Some(kind) = transfer {
            let silent = self.take_word("silent");
            let from = self.graph_target(false)?;
            self.expect_word("to")?;
            let to = self.graph_target(false)?;
            return Ok(UpdateOperation::Transfer { kind, from, to, silent });
        }
        if self.is_word("insert") && self.is_word_at(1, "data") {
            self.at += 2;
            self.allow_blanks_as_variables = false;
            let quads = self.quad_patterns();
            self.allow_blanks_as_variables = true;
            return Ok(UpdateOperation::InsertData(quads?));
        }
        if self.is_word("delete") && self.is_word_at(1, "data") {
            self.at += 2;
            self.allow_blanks_as_variables = false;
            let quads = self.quad_patterns();
            self.allow_blanks_as_variables = true;
            return Ok(UpdateOperation::DeleteData(quads?));
        }
        if self.is_word("delete") && self.is_word_at(1, "where") {
            self.at += 2;
            let quads = self.quad_patterns()?;
            let pattern = Self::pattern_of_quads(&quads);
            return Ok(UpdateOperation::Modify {
                with: None,
                delete: quads,
                insert: Vec::new(),
                using: None,
                pattern,
            });
        }
        let with = if self.take_word("with") { Some(Term::Iri(self.iri()?)) } else { None };
        let delete = if self.take_word("delete") { self.quad_patterns()? } else { Vec::new() };
        let insert = if self.take_word("insert") {
            self.allow_blanks_as_variables = false;
            let quads = self.quad_patterns();
            self.allow_blanks_as_variables = true;
            quads?
        } else {
            Vec::new()
        };
        if delete.is_empty() && insert.is_empty() && with.is_none() {
            return Err(syntax(self.position(), "expected an update operation"));
        }
        let mut using = Dataset::default();
        let mut any = false;
        while self.take_word("using") {
            any = true;
            if self.take_word("named") {
                using.named.push(self.iri()?);
            } else {
                using.default.push(self.iri()?);
            }
        }
        self.expect_word("where")?;
        let pattern = self.group_graph_pattern()?;
        Ok(UpdateOperation::Modify {
            with,
            delete,
            insert,
            using: any.then_some(using),
            pattern,
        })
    }

    fn pattern_of_quads(quads: &[QuadPattern]) -> GraphPattern {
        let mut pattern = GraphPattern::empty();
        for quad in quads {
            let triple = TriplePattern {
                subject: quad.subject.clone(),
                verb: match &quad.predicate {
                    Node::Variable(name) => Verb::Variable(name.clone()),
                    Node::Term(Term::Iri(iri)) => Verb::Path(Path::Iri(iri.clone())),
                    Node::Term(_) => Verb::Path(Path::Iri(String::new())),
                },
                object: quad.object.clone(),
            };
            let single = GraphPattern::Triples(vec![triple]);
            let placed = match &quad.graph {
                Some(graph) => GraphPattern::Graph(graph.clone(), Box::new(single)),
                None => single,
            };
            pattern = Self::join(pattern, placed);
        }
        pattern
    }

    fn update(&mut self) -> RdfResult<Vec<UpdateOperation>> {
        let mut operations = Vec::new();
        loop {
            self.prologue()?;
            if *self.peek() == Token::End {
                return Ok(operations);
            }
            operations.push(self.update_operation()?);
            if !self.take(";") {
                break;
            }
        }
        if *self.peek() != Token::End {
            return Err(self.unexpected());
        }
        Ok(operations)
    }
}

fn parser(text: &str, base: &str) -> RdfResult<Parser> {
    Ok(Parser {
        tokens: tokens(text)?,
        at: 0,
        base: base.to_owned(),
        prefixes: HashMap::new(),
        depth: 0,
        fresh: 0,
        allow_blanks_as_variables: true,
    })
}

/// Reads a SPARQL 1.1 query.
pub(crate) fn parse_query(text: &str, base: &str) -> RdfResult<Query> {
    parser(text, base)?.query()
}

/// Reads a SPARQL 1.1 update request.
pub(crate) fn parse_update(text: &str, base: &str) -> RdfResult<Vec<UpdateOperation>> {
    parser(text, base)?.update()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_queries_translate_to_the_algebra() {
        let query = parse_query(
            r#"PREFIX foaf: <http://xmlns.com/foaf/0.1/>
               SELECT DISTINCT ?name (COUNT(?friend) AS ?friends)
               WHERE {
                 ?person a foaf:Person ; foaf:name ?name .
                 OPTIONAL { ?person foaf:knows ?friend FILTER(?friend != ?person) }
                 FILTER (lang(?name) = "en" && ?age >= 18)
               }
               GROUP BY ?name ORDER BY DESC(?friends) LIMIT 10 OFFSET 5"#,
            "",
        )
        .unwrap();
        let QueryForm::Select { distinct, projection: Some(columns) } = &query.form else {
            panic!("a select");
        };
        assert!(*distinct);
        assert_eq!(columns.len(), 2);
        assert!(matches!(&columns[1].expr, Expr::Aggregate { kind: AggregateKind::Count, .. }));
        let GraphPattern::Filter(_, inner) = &query.pattern else {
            panic!("a filter over the group");
        };
        assert!(matches!(inner.as_ref(), GraphPattern::Optional(_, _, Some(_))));
        assert_eq!(query.group_by.len(), 1);
        assert!(query.order_by[0].descending);
        assert_eq!((query.limit, query.offset), (Some(10), 5));
    }

    #[test]
    fn paths_unions_values_and_subqueries_parse() {
        let query = parse_query(
            "PREFIX ex: <http://ex/> SELECT * WHERE { ?a ex:p/ex:q* | ^ex:r ?b . { ?a ex:s 1 } UNION { ?a ex:s -2.5 } VALUES ?b { ex:x UNDEF } { SELECT ?a WHERE { ?a ex:t ?c } LIMIT 1 } BIND(?b AS ?c) MINUS { ?a ex:u [] } }",
            "",
        )
        .unwrap();
        let text = format!("{query:?}");
        assert!(text.contains("Alternative"), "{text}");
        assert!(text.contains("ZeroOrMore"), "{text}");
        assert!(text.contains("Union"), "{text}");
        assert!(text.contains("Values"), "{text}");
        assert!(text.contains("Select"), "{text}");
        assert!(text.contains("Minus"), "{text}");
        assert!(text.contains("-2.5"), "{text}");
        assert!(parse_query("ASK { <http://a> <http://b> ?c }", "").is_ok());
        assert!(parse_query("CONSTRUCT { ?s <http://b> ?o } WHERE { ?s <http://a> ?o }", "").is_ok());
        assert!(parse_query("CONSTRUCT WHERE { ?s <http://a> ?o }", "").is_ok());
        assert!(parse_query("DESCRIBE <http://a>", "").is_ok());
    }

    #[test]
    fn updates_parse_into_operations() {
        let operations = parse_update(
            "PREFIX ex: <http://ex/> INSERT DATA { ex:a ex:b \"c\"@en . GRAPH ex:g { ex:x ex:y _:z } } ; DELETE { ?s ex:b ?o } INSERT { ?s ex:c ?o } WHERE { ?s ex:b ?o } ; DELETE WHERE { ?s ex:gone ?o } ; CLEAR SILENT GRAPH ex:g ; COPY DEFAULT TO ex:h",
            "",
        )
        .unwrap();
        assert_eq!(operations.len(), 5);
        let UpdateOperation::InsertData(quads) = &operations[0] else {
            panic!("insert data");
        };
        assert_eq!(quads.len(), 2);
        assert!(matches!(quads[1].object, Node::Term(Term::Blank(_))));
        assert!(matches!(operations[4], UpdateOperation::Transfer { kind: TransferKind::Copy, .. }));
    }

    #[test]
    fn hostile_requests_fail_without_panicking() {
        for text in [
            "",
            "SELECT",
            "SELECT * WHERE {",
            "SELECT * WHERE { ?a ex:b ?c }",
            "SELECT * WHERE { ?a <http://b> \"open }",
            "SELECT ?x WHERE { } LIMIT x",
            "SELECT * WHERE { ?a <http://b> ?c } garbage",
            "SELECT * WHERE { SERVICE <http://x> { } }",
        ] {
            assert!(parse_query(text, "").is_err(), "{text}");
        }
        let deep = format!("SELECT * WHERE {}{}", "{ ".repeat(200), "}".repeat(200));
        assert!(parse_query(&deep, "").is_err());
        let deep_expression = format!("ASK {{ FILTER({}1{}) }}", "(".repeat(200), ")".repeat(200));
        assert!(parse_query(&deep_expression, "").is_err());
        assert!(parse_update("LOAD <http://x>", "").is_err());
    }
}
