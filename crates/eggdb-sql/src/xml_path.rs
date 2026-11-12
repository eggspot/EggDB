use crate::arrays::{ArrayElement, ArrayValue};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{Conversion, SqlType, Value, convert};

const MOST_DEPTH: usize = 512;
const MOST_NODES: usize = 1_000_000;
const MOST_EXPRESSION_DEPTH: usize = 200;

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Document,
    Element { name: String, attributes: Vec<(String, String)> },
    Text(String),
    Comment(String),
    Instruction { target: String, data: String },
}

#[derive(Debug, Clone, PartialEq)]
struct Node {
    kind: Kind,
    parent: Option<usize>,
    children: Vec<usize>,
    span: (usize, usize),
}

/// A parsed XML document or content fragment, its nodes in document order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Document {
    nodes: Vec<Node>,
}

fn invalid(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::INVALID_XML_DOCUMENT, message.into())
}

fn is_name_start(character: char) -> bool {
    character.is_alphabetic() || character == '_' || character == ':'
}

fn is_name_part(character: char) -> bool {
    is_name_start(character) || character.is_ascii_digit() || matches!(character, '-' | '.') || character.is_alphanumeric()
}

struct Reader<'t> {
    text: &'t str,
    at: usize,
}

impl Reader<'_> {
    fn rest(&self) -> &str {
        &self.text[self.at..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn eat(&mut self, literal: &str) -> bool {
        if self.rest().starts_with(literal) {
            self.at += literal.len();
            true
        } else {
            false
        }
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.at += self.peek().map_or(0, char::len_utf8);
        }
    }

    fn until(&mut self, end: &str) -> SqlResult<String> {
        let Some(found) = self.rest().find(end) else {
            return Err(invalid(format!("expected {end}")));
        };
        let text = self.rest()[..found].to_owned();
        self.at += found + end.len();
        Ok(text)
    }

    fn name(&mut self) -> SqlResult<String> {
        let start = self.at;
        match self.peek() {
            Some(first) if is_name_start(first) => {}
            _ => return Err(invalid("expected a name")),
        }
        while self.peek().is_some_and(is_name_part) {
            self.at += self.peek().map_or(0, char::len_utf8);
        }
        Ok(self.text[start..self.at].to_owned())
    }
}

fn unescaped(text: &str) -> SqlResult<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 1..];
        let end = tail.find(';').ok_or_else(|| invalid("an entity reference is not closed"))?;
        let entity = &tail[..end];
        let character = match entity {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let code = if let Some(hex) = entity.strip_prefix("#x").or_else(|| entity.strip_prefix("#X")) {
                    u32::from_str_radix(hex, 16).ok()
                } else if let Some(decimal) = entity.strip_prefix('#') {
                    decimal.parse().ok()
                } else {
                    None
                };
                code.and_then(char::from_u32)
                    .ok_or_else(|| invalid(format!("unknown entity &{entity};")))?
            }
        };
        out.push(character);
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

impl Document {
    fn add(&mut self, kind: Kind, parent: usize) -> SqlResult<usize> {
        if self.nodes.len() >= MOST_NODES {
            return Err(invalid(format!("an XML value may hold at most {MOST_NODES} nodes")));
        }
        let id = self.nodes.len();
        self.nodes.push(Node {
            kind,
            parent: Some(parent),
            children: Vec::new(),
            span: (0, 0),
        });
        self.nodes[parent].children.push(id);
        Ok(id)
    }

    /// Parses `text` as an XML document (`whole_document`: one root element) or as content.
    pub(crate) fn parse(text: &str, whole_document: bool) -> SqlResult<Self> {
        let mut document = Document {
            nodes: vec![Node {
                kind: Kind::Document,
                parent: None,
                children: Vec::new(),
                span: (0, text.len()),
            }],
        };
        let mut reader = Reader { text, at: 0 };
        let mut open: Vec<(usize, String)> = Vec::new();
        while reader.at < text.len() {
            let parent = open.last().map_or(0, |(id, _)| *id);
            let start = reader.at;
            if reader.peek() != Some('<') {
                let start = reader.at;
                let end = reader.rest().find('<').map_or(text.len(), |found| start + found);
                let content = unescaped(&text[start..end])?;
                reader.at = end;
                if parent == 0 && content.trim().is_empty() {
                    continue;
                }
                document.add(Kind::Text(content), parent)?;
                continue;
            }
            if reader.eat("<?") {
                let target = reader.name()?;
                let data = reader.until("?>")?;
                if !target.eq_ignore_ascii_case("xml") {
                    document.add(Kind::Instruction { target, data: data.trim().to_owned() }, parent)?;
                }
            } else if reader.eat("<!--") {
                let comment = reader.until("-->")?;
                document.add(Kind::Comment(comment), parent)?;
            } else if reader.eat("<![CDATA[") {
                let content = reader.until("]]>")?;
                document.add(Kind::Text(content), parent)?;
            } else if reader.eat("<!") {
                let mut depth = 0usize;
                loop {
                    match reader.peek() {
                        None => return Err(invalid("a declaration is not closed")),
                        Some('[') => depth += 1,
                        Some(']') => depth = depth.saturating_sub(1),
                        Some('>') if depth == 0 => {
                            reader.at += 1;
                            break;
                        }
                        _ => {}
                    }
                    reader.at += reader.peek().map_or(1, char::len_utf8);
                }
            } else if reader.eat("</") {
                let name = reader.name()?;
                reader.skip_space();
                if !reader.eat(">") {
                    return Err(invalid("an end tag is not closed"));
                }
                match open.pop() {
                    Some((id, expected)) if expected == name => document.nodes[id].span.1 = reader.at,
                    Some((_, expected)) => return Err(invalid(format!("expected </{expected}>, found </{name}>"))),
                    None => return Err(invalid(format!("</{name}> closes nothing"))),
                }
            } else {
                reader.at += 1;
                let name = reader.name()?;
                let mut attributes = Vec::new();
                loop {
                    reader.skip_space();
                    if reader.eat("/>") {
                        let id = document.add(Kind::Element { name, attributes }, parent)?;
                        document.nodes[id].span = (start, reader.at);
                        break;
                    }
                    if reader.eat(">") {
                        if open.len() >= MOST_DEPTH {
                            return Err(invalid(format!("XML may nest at most {MOST_DEPTH} elements")));
                        }
                        let id = document.add(Kind::Element { name: name.clone(), attributes }, parent)?;
                        document.nodes[id].span = (start, reader.at);
                        open.push((id, name));
                        break;
                    }
                    let attribute = reader.name()?;
                    reader.skip_space();
                    if !reader.eat("=") {
                        return Err(invalid(format!("attribute {attribute} has no value")));
                    }
                    reader.skip_space();
                    let quote = match reader.peek() {
                        Some(quote @ ('"' | '\'')) => quote,
                        _ => return Err(invalid(format!("attribute {attribute}'s value is not quoted"))),
                    };
                    reader.at += 1;
                    let value = reader.until(&quote.to_string())?;
                    if attributes.iter().any(|(known, _)| *known == attribute) {
                        return Err(invalid(format!("attribute {attribute} is given twice")));
                    }
                    attributes.push((attribute, unescaped(&value)?));
                }
            }
        }
        if let Some((_, name)) = open.last() {
            return Err(invalid(format!("<{name}> is not closed")));
        }
        if whole_document {
            let elements = document.nodes[0]
                .children
                .iter()
                .filter(|child| matches!(document.nodes[**child].kind, Kind::Element { .. }))
                .count();
            let stray_text = document.nodes[0]
                .children
                .iter()
                .any(|child| matches!(&document.nodes[*child].kind, Kind::Text(text) if !text.trim().is_empty()));
            if elements != 1 || stray_text {
                return Err(invalid("an XML document has exactly one root element"));
            }
        }
        Ok(document)
    }

    fn string_value(&self, target: Target) -> String {
        match target {
            Target::Attribute(element, index) => match &self.nodes[element].kind {
                Kind::Element { attributes, .. } => attributes.get(index).map(|(_, value)| value.clone()).unwrap_or_default(),
                _ => String::new(),
            },
            Target::Node(id) => match &self.nodes[id].kind {
                Kind::Text(text) | Kind::Comment(text) => text.clone(),
                Kind::Instruction { data, .. } => data.clone(),
                Kind::Document | Kind::Element { .. } => {
                    let mut out = String::new();
                    self.collect_text(id, &mut out);
                    out
                }
            },
        }
    }

    fn collect_text(&self, id: usize, out: &mut String) {
        for &child in &self.nodes[id].children {
            match &self.nodes[child].kind {
                Kind::Text(text) => out.push_str(text),
                Kind::Element { .. } => self.collect_text(child, out),
                _ => {}
            }
        }
    }

    fn serialize(&self, target: Target, out: &mut String) {
        match target {
            Target::Attribute(..) => out.push_str(&escape(&self.string_value(target), true)),
            Target::Node(id) => match &self.nodes[id].kind {
                Kind::Document => {
                    for &child in &self.nodes[id].children {
                        self.serialize(Target::Node(child), out);
                    }
                }
                Kind::Text(text) => out.push_str(&escape(text, false)),
                Kind::Comment(text) => {
                    out.push_str("<!--");
                    out.push_str(text);
                    out.push_str("-->");
                }
                Kind::Instruction { target, data } => {
                    out.push_str(&format!("<?{target} {data}?>"));
                }
                Kind::Element { name, attributes } => {
                    out.push('<');
                    out.push_str(name);
                    for (key, value) in attributes {
                        out.push_str(&format!(" {key}=\"{}\"", escape(value, true)));
                    }
                    if self.nodes[id].children.is_empty() {
                        out.push_str("/>");
                        return;
                    }
                    out.push('>');
                    for &child in &self.nodes[id].children {
                        self.serialize(Target::Node(child), out);
                    }
                    out.push_str(&format!("</{name}>"));
                }
            },
        }
    }

    fn direct_text(&self, target: Target) -> String {
        match target {
            Target::Node(id) if matches!(self.nodes[id].kind, Kind::Element { .. }) => self.nodes[id]
                .children
                .iter()
                .filter_map(|child| match &self.nodes[*child].kind {
                    Kind::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect(),
            other => self.string_value(other),
        }
    }
}

fn escape(text: &str, attribute: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' if attribute => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Target {
    Node(usize),
    Attribute(usize, usize),
}

impl Target {
    fn order(self) -> (usize, usize) {
        match self {
            Target::Node(id) => (id, 0),
            Target::Attribute(id, index) => (id, index + 1),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Name(String),
    Text(String),
    Number(f64),
    Symbol(&'static str),
    Star,
    Operator(&'static str),
}

const SYMBOLS: [&str; 17] = ["//", "::", "..", "!=", "<=", ">=", "/", "@", ".", "(", ")", "[", "]", ",", "|", "=", "<"];

fn tokens(text: &str) -> SqlResult<Vec<Token>> {
    let characters: Vec<char> = text.chars().collect();
    let mut found: Vec<Token> = Vec::new();
    let mut at = 0;
    let operator_position = |found: &[Token]| match found.last() {
        None => false,
        Some(Token::Symbol("@" | "::" | "(" | "[" | "," | "/" | "//" | "|" | "=" | "!=" | "<" | "<=" | ">=")) => false,
        Some(Token::Operator(_)) => false,
        Some(_) => true,
    };
    while at < characters.len() {
        let character = characters[at];
        if character.is_whitespace() {
            at += 1;
            continue;
        }
        if character == '"' || character == '\'' {
            let end = characters[at + 1..]
                .iter()
                .position(|next| *next == character)
                .ok_or_else(|| invalid_path("a string is not closed"))?;
            found.push(Token::Text(characters[at + 1..at + 1 + end].iter().collect()));
            at += end + 2;
            continue;
        }
        if character.is_ascii_digit() || (character == '.' && characters.get(at + 1).is_some_and(char::is_ascii_digit)) {
            let start = at;
            while at < characters.len() && (characters[at].is_ascii_digit() || characters[at] == '.') {
                at += 1;
            }
            let number: String = characters[start..at].iter().collect();
            found.push(Token::Number(number.parse().map_err(|_| invalid_path("a number is malformed"))?));
            continue;
        }
        if character == '*' {
            found.push(if operator_position(&found) { Token::Operator("*") } else { Token::Star });
            at += 1;
            continue;
        }
        if is_name_start(character) && character != ':' {
            let start = at;
            while at < characters.len()
                && (is_name_part(characters[at]) && !(characters[at] == ':' && characters.get(at + 1) == Some(&':')))
            {
                at += 1;
            }
            let name: String = characters[start..at].iter().collect();
            if operator_position(&found) && matches!(name.as_str(), "and" | "or" | "mod" | "div") {
                found.push(Token::Operator(match name.as_str() {
                    "and" => "and",
                    "or" => "or",
                    "mod" => "mod",
                    _ => "div",
                }));
            } else {
                found.push(Token::Name(name));
            }
            continue;
        }
        if character == '+' || character == '-' {
            found.push(Token::Operator(if character == '+' { "+" } else { "-" }));
            at += 1;
            continue;
        }
        if character == '>' {
            if characters.get(at + 1) == Some(&'=') {
                found.push(Token::Symbol(">="));
                at += 2;
            } else {
                found.push(Token::Operator(">"));
                at += 1;
            }
            continue;
        }
        let rest: String = characters[at..(at + 2).min(characters.len())].iter().collect();
        let Some(symbol) = SYMBOLS.iter().find(|symbol| rest.starts_with(**symbol)) else {
            return Err(invalid_path(format!("unexpected {character}")));
        };
        found.push(Token::Symbol(*symbol));
        at += symbol.chars().count();
    }
    Ok(found)
}

fn invalid_path(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, format!("invalid XPath expression: {}", message.into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Child,
    Descendant,
    DescendantOrSelf,
    SelfAxis,
    Parent,
    Ancestor,
    AncestorOrSelf,
    Attribute,
    FollowingSibling,
    PrecedingSibling,
}

#[derive(Debug, Clone, PartialEq)]
enum Test {
    Name(String),
    Any,
    Text,
    Node,
    Comment,
    Instruction,
}

#[derive(Debug, Clone, PartialEq)]
struct Step {
    axis: Axis,
    test: Test,
    predicates: Vec<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Path { absolute: bool, start: Option<Box<Expr>>, steps: Vec<Step> },
    Filter(Box<Expr>, Vec<Expr>),
    Text(String),
    Number(f64),
    Call(String, Vec<Expr>),
    Negate(Box<Expr>),
    Binary(Box<Expr>, &'static str, Box<Expr>),
    Union(Box<Expr>, Box<Expr>),
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn eat_symbol(&mut self, symbol: &str) -> bool {
        if matches!(self.peek(), Some(Token::Symbol(found)) if *found == symbol) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn eat_operator(&mut self, operator: &str) -> bool {
        if matches!(self.peek(), Some(Token::Operator(found)) if *found == operator) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect_symbol(&mut self, symbol: &str) -> SqlResult<()> {
        if self.eat_symbol(symbol) {
            Ok(())
        } else {
            Err(invalid_path(format!("expected {symbol}")))
        }
    }

    fn expression(&mut self) -> SqlResult<Expr> {
        self.depth += 1;
        if self.depth > MOST_EXPRESSION_DEPTH {
            return Err(invalid_path("the expression nests too deeply"));
        }
        let result = self.or();
        self.depth -= 1;
        result
    }

    fn binary_level(
        &mut self,
        operators: &[&'static str],
        next: fn(&mut Self) -> SqlResult<Expr>,
    ) -> SqlResult<Expr> {
        let mut left = next(self)?;
        loop {
            let found = operators
                .iter()
                .find(|operator| matches!(self.peek(), Some(Token::Operator(found) | Token::Symbol(found)) if found == *operator));
            let Some(operator) = found.copied() else {
                return Ok(left);
            };
            self.at += 1;
            left = Expr::Binary(Box::new(left), operator, Box::new(next(self)?));
        }
    }

    fn or(&mut self) -> SqlResult<Expr> {
        self.binary_level(&["or"], Self::and)
    }

    fn and(&mut self) -> SqlResult<Expr> {
        self.binary_level(&["and"], Self::equality)
    }

    fn equality(&mut self) -> SqlResult<Expr> {
        self.binary_level(&["=", "!="], Self::relational)
    }

    fn relational(&mut self) -> SqlResult<Expr> {
        self.binary_level(&["<", "<=", ">", ">="], Self::additive)
    }

    fn additive(&mut self) -> SqlResult<Expr> {
        self.binary_level(&["+", "-"], Self::multiplicative)
    }

    fn multiplicative(&mut self) -> SqlResult<Expr> {
        self.binary_level(&["*", "div", "mod"], Self::unary)
    }

    fn unary(&mut self) -> SqlResult<Expr> {
        if self.eat_operator("-") {
            return Ok(Expr::Negate(Box::new(self.unary()?)));
        }
        let mut left = self.path()?;
        while self.eat_symbol("|") {
            left = Expr::Union(Box::new(left), Box::new(self.path()?));
        }
        Ok(left)
    }

    fn path(&mut self) -> SqlResult<Expr> {
        if self.eat_symbol("//") {
            let mut steps = vec![descendant_step()];
            steps.extend(self.relative_steps()?);
            return Ok(Expr::Path { absolute: true, start: None, steps });
        }
        if self.eat_symbol("/") {
            let steps = if self.starts_step() { self.relative_steps()? } else { Vec::new() };
            return Ok(Expr::Path { absolute: true, start: None, steps });
        }
        if self.starts_primary() {
            let primary = self.primary()?;
            let mut predicates = Vec::new();
            while self.eat_symbol("[") {
                predicates.push(self.expression()?);
                self.expect_symbol("]")?;
            }
            let filtered = if predicates.is_empty() { primary } else { Expr::Filter(Box::new(primary), predicates) };
            let mut steps = Vec::new();
            loop {
                if self.eat_symbol("//") {
                    steps.push(descendant_step());
                    steps.extend(self.relative_steps()?);
                } else if self.eat_symbol("/") {
                    steps.extend(self.relative_steps()?);
                } else {
                    break;
                }
            }
            if steps.is_empty() {
                return Ok(filtered);
            }
            return Ok(Expr::Path { absolute: false, start: Some(Box::new(filtered)), steps });
        }
        Ok(Expr::Path { absolute: false, start: None, steps: self.relative_steps()? })
    }

    fn starts_step(&self) -> bool {
        matches!(self.peek(), Some(Token::Name(_) | Token::Star | Token::Symbol("@" | "." | "..")))
    }

    fn starts_primary(&self) -> bool {
        match self.peek() {
            Some(Token::Text(_) | Token::Number(_) | Token::Symbol("(")) => true,
            Some(Token::Name(name)) => {
                matches!(self.tokens.get(self.at + 1), Some(Token::Symbol("(")))
                    && !matches!(name.as_str(), "text" | "node" | "comment" | "processing-instruction")
            }
            _ => false,
        }
    }

    fn primary(&mut self) -> SqlResult<Expr> {
        match self.peek().cloned() {
            Some(Token::Text(text)) => {
                self.at += 1;
                Ok(Expr::Text(text))
            }
            Some(Token::Number(number)) => {
                self.at += 1;
                Ok(Expr::Number(number))
            }
            Some(Token::Symbol("(")) => {
                self.at += 1;
                let inner = self.expression()?;
                self.expect_symbol(")")?;
                Ok(inner)
            }
            Some(Token::Name(name)) => {
                self.at += 2;
                let mut arguments = Vec::new();
                if !self.eat_symbol(")") {
                    loop {
                        arguments.push(self.expression()?);
                        if self.eat_symbol(")") {
                            break;
                        }
                        self.expect_symbol(",")?;
                    }
                }
                Ok(Expr::Call(name, arguments))
            }
            other => Err(invalid_path(format!("unexpected {other:?}"))),
        }
    }

    fn relative_steps(&mut self) -> SqlResult<Vec<Step>> {
        let mut steps = vec![self.step()?];
        loop {
            if self.eat_symbol("//") {
                steps.push(descendant_step());
                steps.push(self.step()?);
            } else if self.eat_symbol("/") {
                steps.push(self.step()?);
            } else {
                return Ok(steps);
            }
        }
    }

    fn step(&mut self) -> SqlResult<Step> {
        if self.eat_symbol(".") {
            return Ok(Step { axis: Axis::SelfAxis, test: Test::Node, predicates: Vec::new() });
        }
        if self.eat_symbol("..") {
            return Ok(Step { axis: Axis::Parent, test: Test::Node, predicates: Vec::new() });
        }
        let mut axis = Axis::Child;
        if self.eat_symbol("@") {
            axis = Axis::Attribute;
        } else if let (Some(Token::Name(name)), Some(Token::Symbol("::"))) = (self.peek().cloned(), self.tokens.get(self.at + 1)) {
            axis = match name.as_str() {
                "child" => Axis::Child,
                "descendant" => Axis::Descendant,
                "descendant-or-self" => Axis::DescendantOrSelf,
                "self" => Axis::SelfAxis,
                "parent" => Axis::Parent,
                "ancestor" => Axis::Ancestor,
                "ancestor-or-self" => Axis::AncestorOrSelf,
                "attribute" => Axis::Attribute,
                "following-sibling" => Axis::FollowingSibling,
                "preceding-sibling" => Axis::PrecedingSibling,
                other => return Err(invalid_path(format!("the axis {other} is not supported"))),
            };
            self.at += 2;
        }
        let test = match self.peek().cloned() {
            Some(Token::Star) => {
                self.at += 1;
                Test::Any
            }
            Some(Token::Name(name)) if matches!(self.tokens.get(self.at + 1), Some(Token::Symbol("("))) => {
                self.at += 2;
                if !self.eat_symbol(")") {
                    if name == "processing-instruction" && matches!(self.peek(), Some(Token::Text(_))) {
                        self.at += 1;
                    }
                    self.expect_symbol(")")?;
                }
                match name.as_str() {
                    "text" => Test::Text,
                    "node" => Test::Node,
                    "comment" => Test::Comment,
                    "processing-instruction" => Test::Instruction,
                    other => return Err(invalid_path(format!("{other}() is not a node test"))),
                }
            }
            Some(Token::Name(name)) => {
                self.at += 1;
                Test::Name(name)
            }
            other => return Err(invalid_path(format!("expected a node test, found {other:?}"))),
        };
        let mut predicates = Vec::new();
        while self.eat_symbol("[") {
            predicates.push(self.expression()?);
            self.expect_symbol("]")?;
        }
        Ok(Step { axis, test, predicates })
    }
}

fn descendant_step() -> Step {
    Step { axis: Axis::DescendantOrSelf, test: Test::Node, predicates: Vec::new() }
}

/// A compiled XPath 1.0 expression.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct XPath {
    expr: Expr,
}

impl XPath {
    pub(crate) fn compile(text: &str) -> SqlResult<Self> {
        let mut parser = Parser { tokens: tokens(text)?, at: 0, depth: 0 };
        let expr = parser.expression()?;
        if parser.at < parser.tokens.len() {
            return Err(invalid_path(format!("unexpected {:?}", parser.peek())));
        }
        Ok(Self { expr })
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Nodes(Vec<Target>),
    Text(String),
    Number(f64),
    Boolean(bool),
}

#[derive(Clone, Copy)]
struct Context {
    target: Target,
    position: usize,
    size: usize,
}

fn number_text(number: f64) -> String {
    if number.is_nan() {
        "NaN".to_owned()
    } else if number.is_infinite() {
        if number > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
    } else if number.fract() == 0.0 && number.abs() < 1e15 {
        format!("{}", number as i64)
    } else {
        format!("{number}")
    }
}

fn text_number(text: &str) -> f64 {
    text.trim().parse().unwrap_or(f64::NAN)
}

impl Document {
    fn matches(&self, target: Target, test: &Test, axis: Axis) -> bool {
        match target {
            Target::Attribute(element, index) => {
                axis == Axis::Attribute
                    && match test {
                        Test::Any | Test::Node => true,
                        Test::Name(name) => match &self.nodes[element].kind {
                            Kind::Element { attributes, .. } => attributes.get(index).is_some_and(|(key, _)| key == name),
                            _ => false,
                        },
                        _ => false,
                    }
            }
            Target::Node(id) => match (&self.nodes[id].kind, test) {
                (_, Test::Node) => true,
                (Kind::Element { name, .. }, Test::Name(wanted)) => {
                    name == wanted || (!wanted.contains(':') && name.rsplit(':').next() == Some(wanted.as_str()))
                }
                (Kind::Element { .. }, Test::Any) => true,
                (Kind::Text(_), Test::Text) => true,
                (Kind::Comment(_), Test::Comment) => true,
                (Kind::Instruction { .. }, Test::Instruction) => true,
                _ => false,
            },
        }
    }

    fn descendants(&self, id: usize, out: &mut Vec<Target>) {
        for &child in &self.nodes[id].children {
            out.push(Target::Node(child));
            self.descendants(child, out);
        }
    }

    fn axis(&self, target: Target, axis: Axis) -> Vec<Target> {
        let mut out = Vec::new();
        let (id, is_attribute) = match target {
            Target::Node(id) => (id, false),
            Target::Attribute(element, _) => (element, true),
        };
        match axis {
            Axis::SelfAxis => out.push(target),
            Axis::Child if !is_attribute => out.extend(self.nodes[id].children.iter().map(|child| Target::Node(*child))),
            Axis::Descendant if !is_attribute => self.descendants(id, &mut out),
            Axis::DescendantOrSelf => {
                out.push(target);
                if !is_attribute {
                    self.descendants(id, &mut out);
                }
            }
            Axis::Parent => {
                if is_attribute {
                    out.push(Target::Node(id));
                } else if let Some(parent) = self.nodes[id].parent {
                    out.push(Target::Node(parent));
                }
            }
            Axis::Ancestor | Axis::AncestorOrSelf => {
                if axis == Axis::AncestorOrSelf {
                    out.push(target);
                }
                let mut current = if is_attribute { Some(id) } else { self.nodes[id].parent };
                while let Some(ancestor) = current {
                    out.push(Target::Node(ancestor));
                    current = self.nodes[ancestor].parent;
                }
                out.reverse();
            }
            Axis::Attribute if !is_attribute => {
                if let Kind::Element { attributes, .. } = &self.nodes[id].kind {
                    out.extend((0..attributes.len()).map(|index| Target::Attribute(id, index)));
                }
            }
            Axis::FollowingSibling | Axis::PrecedingSibling if !is_attribute => {
                if let Some(parent) = self.nodes[id].parent {
                    let siblings = &self.nodes[parent].children;
                    let at = siblings.iter().position(|sibling| *sibling == id).unwrap_or(0);
                    if axis == Axis::FollowingSibling {
                        out.extend(siblings[at + 1..].iter().map(|sibling| Target::Node(*sibling)));
                    } else {
                        out.extend(siblings[..at].iter().map(|sibling| Target::Node(*sibling)));
                    }
                }
            }
            _ => {}
        }
        out
    }

    fn string_of(&self, outcome: &Outcome) -> String {
        match outcome {
            Outcome::Nodes(nodes) => nodes.first().map(|node| self.string_value(*node)).unwrap_or_default(),
            Outcome::Text(text) => text.clone(),
            Outcome::Number(number) => number_text(*number),
            Outcome::Boolean(flag) => flag.to_string(),
        }
    }

    fn number_of(&self, outcome: &Outcome) -> f64 {
        match outcome {
            Outcome::Number(number) => *number,
            Outcome::Boolean(flag) => f64::from(u8::from(*flag)),
            other => text_number(&self.string_of(other)),
        }
    }

    fn boolean_of(outcome: &Outcome) -> bool {
        match outcome {
            Outcome::Nodes(nodes) => !nodes.is_empty(),
            Outcome::Text(text) => !text.is_empty(),
            Outcome::Number(number) => *number != 0.0 && !number.is_nan(),
            Outcome::Boolean(flag) => *flag,
        }
    }

    fn compare(&self, left: &Outcome, operator: &str, right: &Outcome) -> bool {
        let numbers = |left: f64, right: f64| match operator {
            "=" => left == right,
            "!=" => left != right,
            "<" => left < right,
            "<=" => left <= right,
            ">" => left > right,
            _ => left >= right,
        };
        match (left, right) {
            (Outcome::Nodes(nodes), other) | (other, Outcome::Nodes(nodes)) if !matches!(other, Outcome::Nodes(_)) => {
                let flipped = matches!(right, Outcome::Nodes(_)) && !matches!(left, Outcome::Nodes(_));
                nodes.iter().any(|node| {
                    let value = Outcome::Text(self.string_value(*node));
                    let (left, right) = if flipped { (other, &value) } else { (&value, other) };
                    self.compare_atoms(left, operator, right, &numbers)
                })
            }
            (Outcome::Nodes(left_nodes), Outcome::Nodes(right_nodes)) => left_nodes.iter().any(|left_node| {
                right_nodes.iter().any(|right_node| {
                    self.compare_atoms(
                        &Outcome::Text(self.string_value(*left_node)),
                        operator,
                        &Outcome::Text(self.string_value(*right_node)),
                        &numbers,
                    )
                })
            }),
            (left, right) => self.compare_atoms(left, operator, right, &numbers),
        }
    }

    fn compare_atoms(&self, left: &Outcome, operator: &str, right: &Outcome, numbers: &dyn Fn(f64, f64) -> bool) -> bool {
        let equality = matches!(operator, "=" | "!=");
        if equality && (matches!(left, Outcome::Boolean(_)) || matches!(right, Outcome::Boolean(_))) {
            let equal = Self::boolean_of(left) == Self::boolean_of(right);
            return if operator == "=" { equal } else { !equal };
        }
        if equality && !matches!(left, Outcome::Number(_)) && !matches!(right, Outcome::Number(_)) {
            let equal = self.string_of(left) == self.string_of(right);
            return if operator == "=" { equal } else { !equal };
        }
        numbers(self.number_of(left), self.number_of(right))
    }

    fn evaluate(&self, expr: &Expr, context: Context) -> SqlResult<Outcome> {
        Ok(match expr {
            Expr::Text(text) => Outcome::Text(text.clone()),
            Expr::Number(number) => Outcome::Number(*number),
            Expr::Negate(inner) => Outcome::Number(-self.number_of(&self.evaluate(inner, context)?)),
            Expr::Union(left, right) => {
                let (Outcome::Nodes(mut left), Outcome::Nodes(right)) = (self.evaluate(left, context)?, self.evaluate(right, context)?) else {
                    return Err(invalid_path("| joins node sets only"));
                };
                left.extend(right);
                Outcome::Nodes(ordered(left))
            }
            Expr::Binary(left, operator, right) => {
                let left_value = self.evaluate(left, context)?;
                match *operator {
                    "or" => Outcome::Boolean(Self::boolean_of(&left_value) || Self::boolean_of(&self.evaluate(right, context)?)),
                    "and" => Outcome::Boolean(Self::boolean_of(&left_value) && Self::boolean_of(&self.evaluate(right, context)?)),
                    "=" | "!=" | "<" | "<=" | ">" | ">=" => {
                        let right_value = self.evaluate(right, context)?;
                        Outcome::Boolean(self.compare(&left_value, operator, &right_value))
                    }
                    arithmetic => {
                        let (left, right) = (self.number_of(&left_value), self.number_of(&self.evaluate(right, context)?));
                        Outcome::Number(match arithmetic {
                            "+" => left + right,
                            "-" => left - right,
                            "*" => left * right,
                            "div" => left / right,
                            _ => left % right,
                        })
                    }
                }
            }
            Expr::Filter(primary, predicates) => {
                let Outcome::Nodes(nodes) = self.evaluate(primary, context)? else {
                    return Err(invalid_path("a predicate filters node sets only"));
                };
                Outcome::Nodes(self.filtered(nodes, predicates)?)
            }
            Expr::Path { absolute, start, steps } => {
                let mut current = if *absolute {
                    vec![Target::Node(0)]
                } else if let Some(start) = start {
                    match self.evaluate(start, context)? {
                        Outcome::Nodes(nodes) => nodes,
                        _ => return Err(invalid_path("a path continues a node set only")),
                    }
                } else {
                    vec![context.target]
                };
                for step in steps {
                    let mut next = Vec::new();
                    for node in &current {
                        let candidates: Vec<Target> = self
                            .axis(*node, step.axis)
                            .into_iter()
                            .filter(|candidate| self.matches(*candidate, &step.test, step.axis))
                            .collect();
                        let reverse = matches!(step.axis, Axis::Ancestor | Axis::AncestorOrSelf | Axis::PrecedingSibling);
                        let candidates = if reverse {
                            let mut reversed = candidates;
                            reversed.reverse();
                            self.filtered(reversed, &step.predicates)?
                        } else {
                            self.filtered(candidates, &step.predicates)?
                        };
                        next.extend(candidates);
                    }
                    current = ordered(next);
                }
                Outcome::Nodes(current)
            }
            Expr::Call(name, arguments) => self.call(name, arguments, context)?,
        })
    }

    fn filtered(&self, nodes: Vec<Target>, predicates: &[Expr]) -> SqlResult<Vec<Target>> {
        let mut current = nodes;
        for predicate in predicates {
            let size = current.len();
            let mut kept = Vec::new();
            for (index, node) in current.iter().enumerate() {
                let context = Context { target: *node, position: index + 1, size };
                let keep = match self.evaluate(predicate, context)? {
                    Outcome::Number(number) => number == (index + 1) as f64,
                    other => Self::boolean_of(&other),
                };
                if keep {
                    kept.push(*node);
                }
            }
            current = kept;
        }
        Ok(current)
    }

    fn call(&self, name: &str, arguments: &[Expr], context: Context) -> SqlResult<Outcome> {
        let values: Vec<Outcome> = arguments.iter().map(|argument| self.evaluate(argument, context)).collect::<SqlResult<_>>()?;
        let text = |at: usize| -> String {
            values.get(at).map_or_else(|| self.string_value(context.target), |value| self.string_of(value))
        };
        let number = |at: usize| values.get(at).map_or(f64::NAN, |value| self.number_of(value));
        Ok(match (name, values.len()) {
            ("last", 0) => Outcome::Number(context.size as f64),
            ("position", 0) => Outcome::Number(context.position as f64),
            ("count", 1) => match &values[0] {
                Outcome::Nodes(nodes) => Outcome::Number(nodes.len() as f64),
                _ => return Err(invalid_path("count() takes a node set")),
            },
            ("name" | "local-name", 0 | 1) => {
                let target = match values.first() {
                    Some(Outcome::Nodes(nodes)) => nodes.first().copied(),
                    Some(_) => return Err(invalid_path(format!("{name}() takes a node set"))),
                    None => Some(context.target),
                };
                let full = target.map(|target| match target {
                    Target::Node(id) => match &self.nodes[id].kind {
                        Kind::Element { name, .. } => name.clone(),
                        Kind::Instruction { target, .. } => target.clone(),
                        _ => String::new(),
                    },
                    Target::Attribute(element, index) => match &self.nodes[element].kind {
                        Kind::Element { attributes, .. } => attributes.get(index).map(|(key, _)| key.clone()).unwrap_or_default(),
                        _ => String::new(),
                    },
                });
                let full = full.unwrap_or_default();
                Outcome::Text(if name == "local-name" { full.rsplit(':').next().unwrap_or_default().to_owned() } else { full })
            }
            ("string", 0 | 1) => Outcome::Text(text(0)),
            ("concat", count) if count >= 2 => Outcome::Text((0..count).map(text).collect()),
            ("starts-with", 2) => Outcome::Boolean(text(0).starts_with(&text(1))),
            ("contains", 2) => Outcome::Boolean(text(0).contains(&text(1))),
            ("substring-before", 2) => {
                let (whole, part) = (text(0), text(1));
                Outcome::Text(whole.find(&part).map(|at| whole[..at].to_owned()).unwrap_or_default())
            }
            ("substring-after", 2) => {
                let (whole, part) = (text(0), text(1));
                Outcome::Text(whole.find(&part).map(|at| whole[at + part.len()..].to_owned()).unwrap_or_default())
            }
            ("substring", 2 | 3) => {
                let characters: Vec<char> = text(0).chars().collect();
                let start = number(1).round();
                let end = if values.len() == 3 { start + number(2).round() } else { f64::INFINITY };
                Outcome::Text(
                    characters
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| {
                            let position = (*index + 1) as f64;
                            position >= start && position < end
                        })
                        .map(|(_, character)| *character)
                        .collect(),
                )
            }
            ("string-length", 0 | 1) => Outcome::Number(text(0).chars().count() as f64),
            ("normalize-space", 0 | 1) => Outcome::Text(text(0).split_whitespace().collect::<Vec<_>>().join(" ")),
            ("translate", 3) => {
                let (from, to): (Vec<char>, Vec<char>) = (text(1).chars().collect(), text(2).chars().collect());
                Outcome::Text(
                    text(0)
                        .chars()
                        .filter_map(|character| match from.iter().position(|known| *known == character) {
                            Some(at) => to.get(at).copied(),
                            None => Some(character),
                        })
                        .collect(),
                )
            }
            ("not", 1) => Outcome::Boolean(!Self::boolean_of(&values[0])),
            ("true", 0) => Outcome::Boolean(true),
            ("false", 0) => Outcome::Boolean(false),
            ("boolean", 1) => Outcome::Boolean(Self::boolean_of(&values[0])),
            ("number", 0 | 1) => Outcome::Number(values.first().map_or_else(|| text_number(&self.string_value(context.target)), |value| self.number_of(value))),
            ("sum", 1) => match &values[0] {
                Outcome::Nodes(nodes) => Outcome::Number(nodes.iter().map(|node| text_number(&self.string_value(*node))).sum()),
                _ => return Err(invalid_path("sum() takes a node set")),
            },
            ("floor", 1) => Outcome::Number(number(0).floor()),
            ("ceiling", 1) => Outcome::Number(number(0).ceil()),
            ("round", 1) => Outcome::Number((number(0) + 0.5).floor()),
            (other, count) => return Err(invalid_path(format!("{other}() with {count} arguments is not supported"))),
        })
    }

    fn run(&self, path: &XPath) -> SqlResult<Outcome> {
        let context = Context { target: Target::Node(0), position: 1, size: 1 };
        self.evaluate(&path.expr, context)
    }
}

fn ordered(mut nodes: Vec<Target>) -> Vec<Target> {
    nodes.sort_by_key(|node| node.order());
    nodes.dedup();
    nodes
}

/// The internal names of the XPath functions.
pub(crate) const FUNCTIONS: [&str; 8] = [
    "updatexml",
    "xpath",
    "xpath_exists",
    "xml_is_well_formed",
    "xml_is_well_formed_document",
    "xml_is_well_formed_content",
    "extractvalue",
    "existsnode",
];

pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    Some(match name {
        "xpath" => SqlType::Array(ArrayElement::Text),
        "xpath_exists" | "xml_is_well_formed" | "xml_is_well_formed_document" | "xml_is_well_formed_content" => SqlType::Boolean,
        "extractvalue" | "updatexml" => SqlType::Text,
        "existsnode" => SqlType::Integer,
        _ => return None,
    })
}

fn text_argument(value: &Value) -> Option<String> {
    value.to_text()
}

fn evaluated(xml: &str, path: &str) -> SqlResult<(Document, Outcome)> {
    let document = Document::parse(xml, false)?;
    let path = XPath::compile(path)?;
    let outcome = document.run(&path)?;
    Ok((document, outcome))
}

/// PostgreSQL's `xpath`, `xpath_exists` and `xml_is_well_formed*`, Oracle's `EXTRACTVALUE` and
/// `EXISTSNODE`, and MySQL's `ExtractValue`; `None` for another name.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    if values.iter().take(2).any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    let texts: Vec<String> = values.iter().filter_map(text_argument).collect();
    Some(match (name, texts.as_slice()) {
        ("xml_is_well_formed" | "xml_is_well_formed_content", [text]) => Ok(Value::Boolean(Document::parse(text, false).is_ok())),
        ("xml_is_well_formed_document", [text]) => Ok(Value::Boolean(Document::parse(text, true).is_ok())),
        ("xpath", [path, xml, ..]) => evaluated(xml, path).and_then(|(document, outcome)| {
            let items: Vec<Value> = match outcome {
                Outcome::Nodes(nodes) => nodes
                    .iter()
                    .map(|node| {
                        let mut out = String::new();
                        document.serialize(*node, &mut out);
                        Value::Text(out)
                    })
                    .collect(),
                other => vec![Value::Text(document.string_of(&other))],
            };
            let array = ArrayValue::new(ArrayElement::Text, items);
            crate::memory::check_value_bytes(crate::arrays::held_bytes(&array))?;
            Ok(Value::Array(Box::new(array)))
        }),
        ("xpath_exists", [path, xml, ..]) => evaluated(xml, path).map(|(_, outcome)| Value::Boolean(Document::boolean_of(&outcome))),
        ("extractvalue", [xml, path, ..]) => evaluated(xml, path).map(|(document, outcome)| match outcome {
            Outcome::Nodes(nodes) if nodes.is_empty() => Value::Null,
            Outcome::Nodes(nodes) => Value::Text(nodes.iter().map(|node| document.direct_text(*node)).collect::<Vec<_>>().join(" ")),
            other => Value::Text(document.string_of(&other)),
        }),
        ("updatexml", [xml, path, replacement]) => evaluated(xml, path).map(|(document, outcome)| match outcome {
            Outcome::Nodes(nodes) => match nodes.as_slice() {
                [Target::Node(id)] if matches!(document.nodes[*id].kind, Kind::Element { .. }) => {
                    let (start, end) = document.nodes[*id].span;
                    Value::Text(format!("{}{replacement}{}", &xml[..start], &xml[end..]))
                }
                _ => Value::Text(xml.clone()),
            },
            _ => Value::Text(xml.clone()),
        }),
        ("existsnode", [xml, path, ..]) => {
            evaluated(xml, path).map(|(_, outcome)| Value::Integer(i32::from(Document::boolean_of(&outcome))))
        }
        _ => Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("function {name} does not exist with these arguments"),
        )),
    })
}

/// The internal table function a parsed `XMLTABLE` becomes.
pub(crate) const TABLE: &str = "eggdb_xml_table";
/// What separates the columns of an `XMLTABLE` plan.
pub(crate) const PLAN_SEPARATOR: &str = "\u{1}";
const TABLE_FIXED_ARGUMENTS: usize = 3;
const MOST_TABLE_ROWS: usize = 1_000_000;

fn damaged_table() -> SqlError {
    SqlError::new(SqlState::INTERNAL_ERROR, "the XMLTABLE plan is damaged")
}

/// Checks `XMLTABLE`'s internal arguments.
pub(crate) fn table_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    if types.len() <= TABLE_FIXED_ARGUMENTS {
        return Err(damaged_table());
    }
    Ok(SqlType::Text)
}

/// `XMLTABLE`'s output columns: the declared names with their placeholders' types.
pub(crate) fn columns(names: &[String], types: &[Option<SqlType>]) -> Vec<crate::session::OutputColumn> {
    names
        .iter()
        .zip(types.iter().skip(TABLE_FIXED_ARGUMENTS))
        .map(|(name, sql_type)| crate::session::OutputColumn {
            name: name.clone(),
            sql_type: *sql_type,
        })
        .collect()
}

enum ColumnPlan {
    Ordinality,
    Serialized,
    Path(XPath),
}

/// The rows of an `XMLTABLE`: one per node the row expression selects, each column read by
/// its path from that node (its string value; the `DEFAULT` when nothing matches) and cast to
/// its declared type.
pub(crate) fn rows(
    values: &[Value],
    names: &[String],
    types: &[Option<SqlType>],
    sink: &mut crate::json_functions::RecordSink<'_>,
) -> SqlResult<()> {
    let [document, row_path, plan, rest @ ..] = values else {
        return Err(damaged_table());
    };
    let width = names.len();
    let (Some(row_path), Some(plan)) = (row_path.to_text(), plan.to_text()) else {
        return Err(damaged_table());
    };
    let plans: Vec<ColumnPlan> = plan
        .split(PLAN_SEPARATOR)
        .map(|column| match column.split_at(column.len().min(1)) {
            ("O", _) => Ok(ColumnPlan::Ordinality),
            ("X", _) => Ok(ColumnPlan::Serialized),
            ("P", path) => XPath::compile(path).map(ColumnPlan::Path),
            _ => Err(damaged_table()),
        })
        .collect::<SqlResult<_>>()?;
    if plans.len() != width || rest.len() < width * 2 {
        return Err(damaged_table());
    }
    if document.is_null() {
        return Ok(());
    }
    let xml = document.to_text().ok_or_else(damaged_table)?;
    let parsed = Document::parse(&xml, false)?;
    let Outcome::Nodes(nodes) = parsed.run(&XPath::compile(&row_path)?)? else {
        return Err(invalid_path("an XMLTABLE row expression must select nodes"));
    };
    if nodes.len() > MOST_TABLE_ROWS {
        return Err(invalid_path(format!("an XMLTABLE may make at most {MOST_TABLE_ROWS} rows")));
    }
    let size = nodes.len();
    for (index, node) in nodes.iter().enumerate() {
        let mut row = Vec::with_capacity(width);
        for (column, plan) in plans.iter().enumerate() {
            let value = match plan {
                ColumnPlan::Ordinality => Value::BigInt(index as i64 + 1),
                ColumnPlan::Serialized => {
                    let mut out = String::new();
                    parsed.serialize(*node, &mut out);
                    Value::Text(out)
                }
                ColumnPlan::Path(path) => {
                    let context = Context { target: *node, position: index + 1, size };
                    match parsed.evaluate(&path.expr, context)? {
                        Outcome::Nodes(found) if found.is_empty() => rest[width + column].clone(),
                        Outcome::Nodes(found) if found.len() == 1 => Value::Text(parsed.string_value(found[0])),
                        Outcome::Nodes(_) => {
                            return Err(SqlError::new(
                                SqlState::CARDINALITY_VIOLATION,
                                format!("more than one value returned by column XPath expression for {}", names[column]),
                            ));
                        }
                        other => Value::Text(parsed.string_of(&other)),
                    }
                }
            };
            let sql_type = types.get(TABLE_FIXED_ARGUMENTS + column).copied().flatten();
            row.push(match sql_type {
                Some(sql_type) if !value.is_null() && !(sql_type.is_string() && matches!(value, Value::Text(_))) => {
                    convert(value, sql_type, Conversion::Explicit)?
                }
                _ => value,
            });
        }
        sink(row)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(xml: &str, path: &str) -> Value {
        function("xpath", &[Value::Text(path.to_owned()), Value::Text(xml.to_owned())])
            .expect("known")
            .expect("runs")
    }

    #[test]
    fn documents_parse_with_entities_cdata_and_attributes() {
        let document = Document::parse("<?xml version=\\"1.0\\"?><a x='1'>t &amp; <![CDATA[<b>]]><!-- c --></a>", true).unwrap();
        assert_eq!(document.string_value(Target::Node(0)), "t & <b>");
        assert!(Document::parse("<a><b></a>", false).is_err());
        assert!(Document::parse("<a/><b/>", true).is_err());
        assert!(Document::parse("<a/><b/>", false).is_ok());
    }

    #[test]
    fn paths_predicates_and_functions_select_nodes() {
        let xml = "<shop><item id='1'><name>egg</name><price>2</price></item><item id='2'><name>milk</name><price>5</price></item></shop>";
        let extract = |path: &str| {
            function("extractvalue", &[Value::Text(xml.to_owned()), Value::Text(path.to_owned())]).unwrap().unwrap()
        };
        assert_eq!(extract("/shop/item[@id='2']/name"), Value::Text("milk".to_owned()));
        assert_eq!(extract("//item[price > 3]/name"), Value::Text("milk".to_owned()));
        assert_eq!(extract("/shop/item[1]/name"), Value::Text("egg".to_owned()));
        assert_eq!(extract("/shop/item[last()]/@id"), Value::Text("2".to_owned()));
        assert_eq!(extract("count(//item)"), Value::Text("2".to_owned()));
        assert_eq!(extract("sum(//price) div 2"), Value::Text("3.5".to_owned()));
        assert_eq!(extract("//name"), Value::Text("egg milk".to_owned()));
        assert_eq!(extract("/shop/missing"), Value::Null);
        let Value::Array(nodes) = run(xml, "//item[name='egg']/price") else {
            panic!("an array");
        };
        assert_eq!(nodes.items, vec![Value::Text("<price>2</price>".to_owned())]);
        let updated = function(
            "updatexml",
            &[
                Value::Text("<a><b>1</b>  <c/></a>".to_owned()),
                Value::Text("/a/c".to_owned()),
                Value::Text("<c>2</c>".to_owned()),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(updated, Value::Text("<a><b>1</b>  <c>2</c></a>".to_owned()));
        let exists = function("xpath_exists", &[Value::Text("//item[@id=3]".to_owned()), Value::Text(xml.to_owned())]).unwrap().unwrap();
        assert_eq!(exists, Value::Boolean(false));
    }
}
