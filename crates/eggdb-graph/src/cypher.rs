use crate::value::Value;
use crate::{GraphError, GraphResult};

const MAX_DEPTH: usize = 64;
const MAX_HOPS: u32 = 15;

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    Quoted(String),
    Text(String),
    Integer(i64),
    Float(f64),
    Parameter(String),
    Symbol(&'static str),
}

const SYMBOLS: [&str; 26] = [
    "..", "<=", ">=", "<>", "=~", "+=", "(", ")", "[", "]", "{", "}", ",", ":", ".", "|", "*", "+", "-", "/", "%", "^", "=", "<", ">", ";",
];

fn syntax(message: impl Into<String>) -> GraphError {
    GraphError::new("Neo.ClientError.Statement.SyntaxError", message)
}

fn tokenize(text: &str) -> GraphResult<Vec<(Token, usize)>> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        if byte.is_ascii_whitespace() {
            at += 1;
            continue;
        }
        if text[at..].starts_with("//") {
            at = text[at..].find('\n').map_or(bytes.len(), |end| at + end);
            continue;
        }
        if text[at..].starts_with("/*") {
            at = text[at + 2..].find("*/").map(|end| at + 2 + end + 2).ok_or_else(|| syntax("an unterminated comment"))?;
            continue;
        }
        let start = at;
        if byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80 {
            let rest = &text[at..];
            let length = rest.find(|character: char| !(character.is_alphanumeric() || character == '_')).unwrap_or(rest.len());
            if length == 0 {
                return Err(syntax(format!("unexpected character at position {at}")));
            }
            tokens.push((Token::Word(rest[..length].to_owned()), start));
            at += length;
            continue;
        }
        if byte.is_ascii_digit() {
            let rest = &text[at..];
            let mut length = rest.find(|character: char| !character.is_ascii_digit()).unwrap_or(rest.len());
            let mut float = false;
            if rest[length..].starts_with('.') && rest[length + 1..].starts_with(|character: char| character.is_ascii_digit()) {
                float = true;
                length += 1 + rest[length + 1..].find(|character: char| !character.is_ascii_digit()).unwrap_or(rest.len() - length - 1);
            }
            if rest[length..].starts_with(['e', 'E']) {
                let exponent = &rest[length + 1..];
                let sign = usize::from(exponent.starts_with(['+', '-']));
                let digits = exponent[sign..].find(|character: char| !character.is_ascii_digit()).unwrap_or(exponent.len() - sign);
                if digits > 0 {
                    float = true;
                    length += 1 + sign + digits;
                }
            }
            let number = &rest[..length];
            let token = if float {
                Token::Float(number.parse().map_err(|_| syntax(format!("invalid number {number}")))?)
            } else {
                Token::Integer(number.parse().map_err(|_| syntax(format!("integer is too large: {number}")))?)
            };
            tokens.push((token, start));
            at += length;
            continue;
        }
        if byte == b'\'' || byte == b'"' || byte == b'`' {
            let mut out = String::new();
            let mut chars = text[at + 1..].char_indices();
            let mut closed = None;
            while let Some((offset, character)) = chars.next() {
                if character == char::from(byte) {
                    if byte == b'`' && text[at + 1 + offset + 1..].starts_with('`') {
                        chars.next();
                        out.push('`');
                        continue;
                    }
                    closed = Some(offset);
                    break;
                }
                if character == '\\' && byte != b'`' {
                    let Some((_, escaped)) = chars.next() else {
                        break;
                    };
                    out.push(match escaped {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        other => other,
                    });
                    continue;
                }
                out.push(character);
            }
            let Some(offset) = closed else {
                return Err(syntax("an unterminated string or name"));
            };
            tokens.push((if byte == b'`' { Token::Quoted(out) } else { Token::Text(out) }, start));
            at += offset + 2;
            continue;
        }
        if byte == b'$' {
            let rest = &text[at + 1..];
            let length = rest.find(|character: char| !(character.is_alphanumeric() || character == '_')).unwrap_or(rest.len());
            if length == 0 {
                return Err(syntax("a parameter needs a name"));
            }
            tokens.push((Token::Parameter(rest[..length].to_owned()), start));
            at += 1 + length;
            continue;
        }
        let Some(symbol) = SYMBOLS.iter().find(|symbol| text[at..].starts_with(**symbol)) else {
            return Err(syntax(format!("unexpected character at position {at}")));
        };
        tokens.push((Token::Symbol(symbol), start));
        at += symbol.len();
    }
    Ok(tokens)
}

/// A binary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operator {
    Or,
    Xor,
    And,
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Power,
    In,
    StartsWith,
    EndsWith,
    Contains,
}

/// An expression.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Literal(Value),
    Parameter(String),
    Variable(String),
    Property(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    List(Vec<Expr>),
    Map(Vec<(String, Expr)>),
    Binary(Operator, Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Negate(Box<Expr>),
    IsNull(Box<Expr>, bool),
    Call { name: String, distinct: bool, arguments: Vec<Expr>, star: bool },
    Case { subject: Option<Box<Expr>>, arms: Vec<(Expr, Expr)>, otherwise: Option<Box<Expr>> },
}

/// The direction a relationship pattern follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Outgoing,
    Incoming,
    Either,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NodePattern {
    pub(crate) variable: Option<String>,
    pub(crate) labels: Vec<String>,
    pub(crate) properties: Vec<(String, Expr)>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RelationshipPattern {
    pub(crate) variable: Option<String>,
    pub(crate) kinds: Vec<String>,
    pub(crate) properties: Vec<(String, Expr)>,
    pub(crate) direction: Direction,
    pub(crate) hops: Option<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Pattern {
    pub(crate) start: NodePattern,
    pub(crate) steps: Vec<(RelationshipPattern, NodePattern)>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Projection {
    pub(crate) expr: Expr,
    pub(crate) name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Projecting {
    pub(crate) distinct: bool,
    pub(crate) star: bool,
    pub(crate) items: Vec<Projection>,
    pub(crate) order: Vec<(Expr, bool)>,
    pub(crate) skip: Option<Expr>,
    pub(crate) limit: Option<Expr>,
    pub(crate) filter: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SetItem {
    Property(Expr, String, Expr),
    Replace(String, Expr, bool),
    Labels(String, Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Clause {
    Match { optional: bool, patterns: Vec<Pattern>, filter: Option<Expr> },
    Create(Vec<Pattern>),
    Merge(Pattern),
    Set(Vec<SetItem>),
    Remove(Vec<SetItem>),
    Delete { detach: bool, targets: Vec<Expr> },
    Unwind(Expr, String),
    With(Projecting),
    Return(Projecting),
}

impl Clause {
    pub(crate) fn writes(&self) -> bool {
        matches!(self, Clause::Create(_) | Clause::Merge(_) | Clause::Set(_) | Clause::Remove(_) | Clause::Delete { .. })
    }
}

struct Parser<'t> {
    text: &'t str,
    tokens: Vec<(Token, usize)>,
    at: usize,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at).map(|(token, _)| token)
    }

    fn peek_at(&self, ahead: usize) -> Option<&Token> {
        self.tokens.get(self.at + ahead).map(|(token, _)| token)
    }

    fn offset(&self) -> usize {
        self.tokens.get(self.at).map_or(self.text.len(), |(_, offset)| *offset)
    }

    fn unexpected(&self) -> GraphError {
        match self.peek() {
            Some(token) => syntax(format!("Invalid input {token:?} at position {}", self.offset())),
            None => syntax("Unexpected end of input"),
        }
    }

    fn is_keyword(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Word(found)) if found.eq_ignore_ascii_case(word))
    }

    fn is_keyword_at(&self, ahead: usize, word: &str) -> bool {
        matches!(self.peek_at(ahead), Some(Token::Word(found)) if found.eq_ignore_ascii_case(word))
    }

    fn take_keyword(&mut self, word: &str) -> bool {
        let found = self.is_keyword(word);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect_keyword(&mut self, word: &str) -> GraphResult<()> {
        if self.take_keyword(word) { Ok(()) } else { Err(self.unexpected()) }
    }

    fn is_symbol(&self, symbol: &str) -> bool {
        matches!(self.peek(), Some(Token::Symbol(found)) if *found == symbol)
    }

    fn take_symbol(&mut self, symbol: &str) -> bool {
        let found = self.is_symbol(symbol);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect_symbol(&mut self, symbol: &str) -> GraphResult<()> {
        if self.take_symbol(symbol) { Ok(()) } else { Err(self.unexpected()) }
    }

    fn name(&mut self) -> GraphResult<String> {
        match self.peek().cloned() {
            Some(Token::Word(word) | Token::Quoted(word)) => {
                self.at += 1;
                Ok(word)
            }
            _ => Err(self.unexpected()),
        }
    }

    fn integer(&mut self) -> GraphResult<u32> {
        match self.peek() {
            Some(Token::Integer(number)) if (0..=i64::from(u32::MAX)).contains(number) => {
                let number = *number as u32;
                self.at += 1;
                Ok(number)
            }
            _ => Err(self.unexpected()),
        }
    }

    fn map_literal(&mut self) -> GraphResult<Vec<(String, Expr)>> {
        self.expect_symbol("{")?;
        let mut entries = Vec::new();
        if !self.take_symbol("}") {
            loop {
                let key = self.name()?;
                self.expect_symbol(":")?;
                entries.push((key, self.expression()?));
                if !self.take_symbol(",") {
                    break;
                }
            }
            self.expect_symbol("}")?;
        }
        Ok(entries)
    }

    fn node_pattern(&mut self) -> GraphResult<NodePattern> {
        self.expect_symbol("(")?;
        let variable = match self.peek() {
            Some(Token::Word(_) | Token::Quoted(_)) => Some(self.name()?),
            _ => None,
        };
        let mut labels = Vec::new();
        while self.take_symbol(":") {
            labels.push(self.name()?);
        }
        let properties = if self.is_symbol("{") { self.map_literal()? } else { Vec::new() };
        self.expect_symbol(")")?;
        Ok(NodePattern { variable, labels, properties })
    }

    fn relationship_pattern(&mut self) -> GraphResult<Option<RelationshipPattern>> {
        let incoming = if self.is_symbol("<") && matches!(self.peek_at(1), Some(Token::Symbol("-"))) {
            self.at += 2;
            true
        } else if self.take_symbol("-") {
            false
        } else {
            return Ok(None);
        };
        let mut pattern = RelationshipPattern { variable: None, kinds: Vec::new(), properties: Vec::new(), direction: Direction::Either, hops: None };
        if self.take_symbol("[") {
            if matches!(self.peek(), Some(Token::Word(_) | Token::Quoted(_))) {
                pattern.variable = Some(self.name()?);
            }
            if self.take_symbol(":") {
                pattern.kinds.push(self.name()?);
                while self.take_symbol("|") {
                    self.take_symbol(":");
                    pattern.kinds.push(self.name()?);
                }
            }
            if self.take_symbol("*") {
                let low = if matches!(self.peek(), Some(Token::Integer(_))) { Some(self.integer()?) } else { None };
                let (min, max) = if self.take_symbol("..") {
                    let high = if matches!(self.peek(), Some(Token::Integer(_))) { Some(self.integer()?) } else { None };
                    (low.unwrap_or(1), high.unwrap_or(MAX_HOPS))
                } else {
                    match low {
                        Some(exact) => (exact, exact),
                        None => (1, MAX_HOPS),
                    }
                };
                if max > MAX_HOPS || min > max {
                    return Err(syntax(format!("variable-length relationships go at most {MAX_HOPS} hops")));
                }
                pattern.hops = Some((min, max));
            }
            if self.is_symbol("{") {
                pattern.properties = self.map_literal()?;
            }
            self.expect_symbol("]")?;
        }
        self.expect_symbol("-")?;
        let outgoing = self.take_symbol(">");
        pattern.direction = match (incoming, outgoing) {
            (true, true) => return Err(syntax("a relationship cannot point both ways")),
            (true, false) => Direction::Incoming,
            (false, true) => Direction::Outgoing,
            (false, false) => Direction::Either,
        };
        Ok(Some(pattern))
    }

    fn pattern(&mut self) -> GraphResult<Pattern> {
        if matches!(self.peek(), Some(Token::Word(_))) && matches!(self.peek_at(1), Some(Token::Symbol("="))) {
            return Err(GraphError::not_supported("named paths"));
        }
        let start = self.node_pattern()?;
        let mut steps = Vec::new();
        while let Some(relationship) = self.relationship_pattern()? {
            steps.push((relationship, self.node_pattern()?));
        }
        Ok(Pattern { start, steps })
    }

    fn patterns(&mut self) -> GraphResult<Vec<Pattern>> {
        let mut patterns = vec![self.pattern()?];
        while self.take_symbol(",") {
            patterns.push(self.pattern()?);
        }
        Ok(patterns)
    }

    fn expression(&mut self) -> GraphResult<Expr> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(syntax("the expression is nested too deeply"));
        }
        let parsed = self.or();
        self.depth -= 1;
        parsed
    }

    fn binary(&mut self, operator: Operator, left: Expr, next: fn(&mut Self) -> GraphResult<Expr>) -> GraphResult<Expr> {
        Ok(Expr::Binary(operator, Box::new(left), Box::new(next(self)?)))
    }

    fn or(&mut self) -> GraphResult<Expr> {
        let mut left = self.xor()?;
        while self.take_keyword("or") {
            left = self.binary(Operator::Or, left, Self::xor)?;
        }
        Ok(left)
    }

    fn xor(&mut self) -> GraphResult<Expr> {
        let mut left = self.and()?;
        while self.take_keyword("xor") {
            left = self.binary(Operator::Xor, left, Self::and)?;
        }
        Ok(left)
    }

    fn and(&mut self) -> GraphResult<Expr> {
        let mut left = self.not()?;
        while self.take_keyword("and") {
            left = self.binary(Operator::And, left, Self::not)?;
        }
        Ok(left)
    }

    fn not(&mut self) -> GraphResult<Expr> {
        if self.take_keyword("not") {
            self.depth += 1;
            if self.depth > MAX_DEPTH {
                return Err(syntax("the expression is nested too deeply"));
            }
            let inner = self.not();
            self.depth -= 1;
            return Ok(Expr::Not(Box::new(inner?)));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> GraphResult<Expr> {
        let mut left = self.additive()?;
        loop {
            let operator = match self.peek() {
                Some(Token::Symbol("=")) => Operator::Equal,
                Some(Token::Symbol("<>")) => Operator::NotEqual,
                Some(Token::Symbol("<")) => Operator::Less,
                Some(Token::Symbol("<=")) => Operator::LessOrEqual,
                Some(Token::Symbol(">")) => Operator::Greater,
                Some(Token::Symbol(">=")) => Operator::GreaterOrEqual,
                Some(Token::Word(word)) if word.eq_ignore_ascii_case("in") => Operator::In,
                Some(Token::Word(word)) if word.eq_ignore_ascii_case("contains") => Operator::Contains,
                Some(Token::Word(word)) if word.eq_ignore_ascii_case("starts") && self.is_keyword_at(1, "with") => Operator::StartsWith,
                Some(Token::Word(word)) if word.eq_ignore_ascii_case("ends") && self.is_keyword_at(1, "with") => Operator::EndsWith,
                Some(Token::Word(word)) if word.eq_ignore_ascii_case("is") => {
                    self.at += 1;
                    let negated = self.take_keyword("not");
                    self.expect_keyword("null")?;
                    left = Expr::IsNull(Box::new(left), negated);
                    continue;
                }
                _ => return Ok(left),
            };
            self.at += if matches!(operator, Operator::StartsWith | Operator::EndsWith) { 2 } else { 1 };
            left = self.binary(operator, left, Self::additive)?;
        }
    }

    fn additive(&mut self) -> GraphResult<Expr> {
        let mut left = self.multiplicative()?;
        loop {
            let operator = if self.take_symbol("+") {
                Operator::Add
            } else if self.is_symbol("-") {
                self.at += 1;
                Operator::Subtract
            } else {
                return Ok(left);
            };
            left = self.binary(operator, left, Self::multiplicative)?;
        }
    }

    fn multiplicative(&mut self) -> GraphResult<Expr> {
        let mut left = self.power()?;
        loop {
            let operator = if self.take_symbol("*") {
                Operator::Multiply
            } else if self.take_symbol("/") {
                Operator::Divide
            } else if self.take_symbol("%") {
                Operator::Modulo
            } else {
                return Ok(left);
            };
            left = self.binary(operator, left, Self::power)?;
        }
    }

    fn power(&mut self) -> GraphResult<Expr> {
        let left = self.unary()?;
        if self.take_symbol("^") {
            return self.binary(Operator::Power, left, Self::unary);
        }
        Ok(left)
    }

    fn unary(&mut self) -> GraphResult<Expr> {
        if self.take_symbol("-") {
            self.depth += 1;
            if self.depth > MAX_DEPTH {
                return Err(syntax("the expression is nested too deeply"));
            }
            let inner = self.unary();
            self.depth -= 1;
            return Ok(Expr::Negate(Box::new(inner?)));
        }
        self.take_symbol("+");
        self.postfix()
    }

    fn postfix(&mut self) -> GraphResult<Expr> {
        let mut expr = self.primary()?;
        loop {
            if self.take_symbol(".") {
                expr = Expr::Property(Box::new(expr), self.name()?);
            } else if self.take_symbol("[") {
                let index = self.expression()?;
                self.expect_symbol("]")?;
                expr = Expr::Index(Box::new(expr), Box::new(index));
            } else {
                return Ok(expr);
            }
        }
    }

    fn primary(&mut self) -> GraphResult<Expr> {
        let Some(token) = self.peek().cloned() else {
            return Err(self.unexpected());
        };
        match token {
            Token::Integer(number) => {
                self.at += 1;
                Ok(Expr::Literal(Value::Integer(number)))
            }
            Token::Float(number) => {
                self.at += 1;
                Ok(Expr::Literal(Value::Float(number)))
            }
            Token::Text(text) => {
                self.at += 1;
                Ok(Expr::Literal(Value::String(text)))
            }
            Token::Parameter(name) => {
                self.at += 1;
                Ok(Expr::Parameter(name))
            }
            Token::Symbol("(") => {
                self.at += 1;
                let inner = self.expression()?;
                self.expect_symbol(")")?;
                Ok(inner)
            }
            Token::Symbol("[") => {
                self.at += 1;
                let mut items = Vec::new();
                if !self.take_symbol("]") {
                    loop {
                        items.push(self.expression()?);
                        if !self.take_symbol(",") {
                            break;
                        }
                    }
                    self.expect_symbol("]")?;
                }
                Ok(Expr::List(items))
            }
            Token::Symbol("{") => Ok(Expr::Map(self.map_literal()?)),
            Token::Quoted(name) => {
                self.at += 1;
                Ok(Expr::Variable(name))
            }
            Token::Word(word) => {
                let lower = word.to_ascii_lowercase();
                match lower.as_str() {
                    "true" | "false" => {
                        self.at += 1;
                        Ok(Expr::Literal(Value::Boolean(lower == "true")))
                    }
                    "null" => {
                        self.at += 1;
                        Ok(Expr::Literal(Value::Null))
                    }
                    "case" => self.case(),
                    _ if matches!(self.peek_at(1), Some(Token::Symbol("("))) => self.call(lower),
                    _ => {
                        self.at += 1;
                        Ok(Expr::Variable(word))
                    }
                }
            }
            Token::Symbol(_) => Err(self.unexpected()),
        }
    }

    fn case(&mut self) -> GraphResult<Expr> {
        self.expect_keyword("case")?;
        let subject = if self.is_keyword("when") { None } else { Some(Box::new(self.expression()?)) };
        let mut arms = Vec::new();
        while self.take_keyword("when") {
            let when = self.expression()?;
            self.expect_keyword("then")?;
            arms.push((when, self.expression()?));
        }
        let otherwise = if self.take_keyword("else") { Some(Box::new(self.expression()?)) } else { None };
        self.expect_keyword("end")?;
        Ok(Expr::Case { subject, arms, otherwise })
    }

    fn call(&mut self, mut name: String) -> GraphResult<Expr> {
        self.at += 2;
        if self.take_symbol("*") {
            self.expect_symbol(")")?;
            return Ok(Expr::Call { name, distinct: false, arguments: Vec::new(), star: true });
        }
        let distinct = self.take_keyword("distinct");
        let mut arguments = Vec::new();
        if !self.take_symbol(")") {
            loop {
                arguments.push(self.expression()?);
                if !self.take_symbol(",") {
                    break;
                }
            }
            self.expect_symbol(")")?;
        }
        name.make_ascii_lowercase();
        Ok(Expr::Call { name, distinct, arguments, star: false })
    }

    fn projecting(&mut self, returning: bool) -> GraphResult<Projecting> {
        let distinct = self.take_keyword("distinct");
        let mut star = false;
        let mut items = Vec::new();
        if self.take_symbol("*") {
            star = true;
        } else {
            loop {
                let start = self.offset();
                let expr = self.expression()?;
                let end = self.tokens.get(self.at).map_or(self.text.len(), |(_, offset)| *offset);
                let name = if self.take_keyword("as") {
                    self.name()?
                } else {
                    match &expr {
                        Expr::Variable(name) => name.clone(),
                        _ if !returning => return Err(syntax("Expression in WITH must be aliased (use AS)")),
                        _ => self.text[start..end].trim().to_owned(),
                    }
                };
                items.push(Projection { expr, name });
                if !self.take_symbol(",") {
                    break;
                }
            }
        }
        let mut order = Vec::new();
        if self.is_keyword("order") && self.is_keyword_at(1, "by") {
            self.at += 2;
            loop {
                let expr = self.expression()?;
                let descending = self.take_keyword("desc") || self.take_keyword("descending");
                if !descending {
                    let _ = self.take_keyword("asc") || self.take_keyword("ascending");
                }
                order.push((expr, descending));
                if !self.take_symbol(",") {
                    break;
                }
            }
        }
        let skip = if self.take_keyword("skip") { Some(self.expression()?) } else { None };
        let limit = if self.take_keyword("limit") { Some(self.expression()?) } else { None };
        let filter = if !returning && self.take_keyword("where") { Some(self.expression()?) } else { None };
        Ok(Projecting { distinct, star, items, order, skip, limit, filter })
    }

    fn set_items(&mut self, removing: bool) -> GraphResult<Vec<SetItem>> {
        let mut items = Vec::new();
        loop {
            let variable = self.name()?;
            if self.is_symbol(":") {
                let mut labels = Vec::new();
                while self.take_symbol(":") {
                    labels.push(self.name()?);
                }
                items.push(SetItem::Labels(variable, labels));
            } else if !removing && (self.is_symbol("=") || self.is_symbol("+=")) {
                let merge = self.take_symbol("+=");
                if !merge {
                    self.expect_symbol("=")?;
                }
                items.push(SetItem::Replace(variable, self.expression()?, merge));
            } else {
                self.expect_symbol(".")?;
                let property = self.name()?;
                let value = if removing {
                    Expr::Literal(Value::Null)
                } else {
                    self.expect_symbol("=")?;
                    self.expression()?
                };
                items.push(SetItem::Property(Expr::Variable(variable), property, value));
            }
            if !self.take_symbol(",") {
                return Ok(items);
            }
        }
    }

    fn clause(&mut self) -> GraphResult<Clause> {
        if self.take_keyword("optional") {
            self.expect_keyword("match")?;
            return self.match_rest(true);
        }
        if self.take_keyword("match") {
            return self.match_rest(false);
        }
        if self.take_keyword("create") {
            return Ok(Clause::Create(self.patterns()?));
        }
        if self.take_keyword("merge") {
            let pattern = self.pattern()?;
            if self.is_keyword("on") {
                return Err(GraphError::not_supported("MERGE ... ON CREATE and ON MATCH"));
            }
            return Ok(Clause::Merge(pattern));
        }
        if self.take_keyword("set") {
            return Ok(Clause::Set(self.set_items(false)?));
        }
        if self.take_keyword("remove") {
            return Ok(Clause::Remove(self.set_items(true)?));
        }
        let detach = self.take_keyword("detach");
        if self.take_keyword("delete") {
            let mut targets = vec![self.expression()?];
            while self.take_symbol(",") {
                targets.push(self.expression()?);
            }
            return Ok(Clause::Delete { detach, targets });
        }
        if detach {
            return Err(self.unexpected());
        }
        if self.take_keyword("unwind") {
            let list = self.expression()?;
            self.expect_keyword("as")?;
            return Ok(Clause::Unwind(list, self.name()?));
        }
        if self.take_keyword("with") {
            return Ok(Clause::With(self.projecting(false)?));
        }
        if self.take_keyword("return") {
            return Ok(Clause::Return(self.projecting(true)?));
        }
        if self.is_keyword("call") || self.is_keyword("foreach") || self.is_keyword("load") || self.is_keyword("union") {
            return Err(GraphError::not_supported(&format!("{:?}", self.peek())));
        }
        Err(self.unexpected())
    }

    fn match_rest(&mut self, optional: bool) -> GraphResult<Clause> {
        let patterns = self.patterns()?;
        let filter = if self.take_keyword("where") { Some(self.expression()?) } else { None };
        Ok(Clause::Match { optional, patterns, filter })
    }
}

/// Parses a query into its clauses.
pub(crate) fn parse(text: &str) -> GraphResult<Vec<Clause>> {
    let tokens = tokenize(text)?;
    let mut parser = Parser { text, tokens, at: 0, depth: 0 };
    let mut clauses = Vec::new();
    while parser.peek().is_some() && !parser.is_symbol(";") {
        clauses.push(parser.clause()?);
    }
    parser.take_symbol(";");
    if parser.peek().is_some() {
        return Err(parser.unexpected());
    }
    if clauses.is_empty() {
        return Err(syntax("an empty query"));
    }
    let last = clauses.last();
    if !matches!(last, Some(Clause::Return(_))) && !clauses.iter().any(Clause::writes) {
        return Err(syntax("Query cannot conclude with MATCH, WITH or UNWIND (must be a RETURN clause or an update clause)"));
    }
    if clauses.iter().rev().skip(1).any(|clause| matches!(clause, Clause::Return(_))) {
        return Err(syntax("RETURN can only be used at the end of the query"));
    }
    Ok(clauses)
}
