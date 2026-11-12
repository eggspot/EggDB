use crate::error::{DocumentError, DocumentResult};
use eggdb_bson::{Bson, Document};
use std::cmp::Ordering;

const MOST_SCRIPT_CHARS: usize = 10_000;
const MOST_DEPTH: usize = 100;
const JS_INTERPRETER_FAILURE: i32 = 139;

fn failure(message: impl Into<String>) -> DocumentError {
    DocumentError::new(JS_INTERPRETER_FAILURE, "JSInterpreterFailure", message)
}

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Undefined,
    Null,
    Boolean(bool),
    Number(f64),
    Text(String),
    Other(Bson),
}

impl Value {
    fn of(value: &Bson) -> Self {
        match value {
            Bson::Double(number) => Self::Number(*number),
            Bson::Int32(number) => Self::Number(f64::from(*number)),
            Bson::Int64(number) => Self::Number(*number as f64),
            Bson::String(text) | Bson::Symbol(text) => Self::Text(text.clone()),
            Bson::Boolean(flag) => Self::Boolean(*flag),
            Bson::Null => Self::Null,
            Bson::Undefined => Self::Undefined,
            other => Self::Other(other.clone()),
        }
    }

    fn truthy(&self) -> bool {
        match self {
            Self::Undefined | Self::Null => false,
            Self::Boolean(flag) => *flag,
            Self::Number(number) => *number != 0.0 && !number.is_nan(),
            Self::Text(text) => !text.is_empty(),
            Self::Other(_) => true,
        }
    }

    fn number(&self) -> f64 {
        match self {
            Self::Undefined => f64::NAN,
            Self::Null => 0.0,
            Self::Boolean(flag) => f64::from(u8::from(*flag)),
            Self::Number(number) => *number,
            Self::Text(text) if text.trim().is_empty() => 0.0,
            Self::Text(text) => text.trim().parse().unwrap_or(f64::NAN),
            Self::Other(Bson::DateTime(millis)) => *millis as f64,
            Self::Other(_) => f64::NAN,
        }
    }

    fn text(&self) -> String {
        match self {
            Self::Undefined => "undefined".to_owned(),
            Self::Null => "null".to_owned(),
            Self::Boolean(flag) => flag.to_string(),
            Self::Number(number) => number_text(*number),
            Self::Text(text) => text.clone(),
            Self::Other(Bson::Array(items)) => items
                .iter()
                .map(|item| match Value::of(item) {
                    Value::Null | Value::Undefined => String::new(),
                    other => other.text(),
                })
                .collect::<Vec<_>>()
                .join(","),
            Self::Other(_) => "[object Object]".to_owned(),
        }
    }

    fn strictly_equal(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Number(left), Self::Number(right)) => left == right,
            (Self::Other(left), Self::Other(right)) => eggdb_bson::compare(left, right) == Ordering::Equal,
            (left, right) => left == right,
        }
    }

    fn loosely_equal(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null | Self::Undefined, Self::Null | Self::Undefined) => true,
            (Self::Null | Self::Undefined, _) | (_, Self::Null | Self::Undefined) => false,
            (Self::Number(_) | Self::Boolean(_), Self::Text(_) | Self::Boolean(_) | Self::Number(_))
            | (Self::Text(_), Self::Number(_) | Self::Boolean(_)) => self.number() == other.number(),
            _ => self.strictly_equal(other),
        }
    }
}

fn number_text(number: f64) -> String {
    if number.is_nan() {
        "NaN".to_owned()
    } else if number.is_infinite() {
        if number > 0.0 { "Infinity".to_owned() } else { "-Infinity".to_owned() }
    } else if number.fract() == 0.0 && number.abs() < 1e21 {
        format!("{number:.0}")
    } else {
        number.to_string()
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(f64),
    Text(String),
    Name(String),
    Symbol(&'static str),
}

const SYMBOLS: [&str; 26] = [
    "===", "!==", "==", "!=", "<=", ">=", "&&", "||", "<", ">", "+", "-", "*", "/", "%", "!", "(", ")", "[", "]",
    "{", "}", ".", ";", "?", ":",
];

fn tokens(script: &str) -> DocumentResult<Vec<Token>> {
    let characters: Vec<char> = script.chars().collect();
    let mut out = Vec::new();
    let mut at = 0;
    while at < characters.len() {
        let character = characters[at];
        if character.is_whitespace() {
            at += 1;
        } else if character.is_ascii_digit() || (character == '.' && characters.get(at + 1).is_some_and(char::is_ascii_digit)) {
            let start = at;
            while at < characters.len() && (characters[at].is_ascii_digit() || characters[at] == '.') {
                at += 1;
            }
            let text: String = characters[start..at].iter().collect();
            out.push(Token::Number(text.parse().map_err(|_| failure(format!("SyntaxError: bad number {text}")))?));
        } else if character == '\'' || character == '"' {
            let mut text = String::new();
            at += 1;
            loop {
                let Some(&next) = characters.get(at) else {
                    return Err(failure("SyntaxError: unterminated string literal"));
                };
                at += 1;
                match next {
                    '\\' => {
                        let escaped = characters.get(at).copied().unwrap_or('\\');
                        at += 1;
                        text.push(match escaped {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            other => other,
                        });
                    }
                    quote if quote == character => break,
                    other => text.push(other),
                }
            }
            out.push(Token::Text(text));
        } else if character.is_alphabetic() || character == '_' || character == '$' {
            let start = at;
            while at < characters.len() && (characters[at].is_alphanumeric() || characters[at] == '_' || characters[at] == '$') {
                at += 1;
            }
            out.push(Token::Name(characters[start..at].iter().collect()));
        } else {
            let rest: String = characters[at..characters.len().min(at + 3)].iter().collect();
            let Some(symbol) = SYMBOLS.iter().find(|symbol| rest.starts_with(**symbol)) else {
                return Err(failure(format!("SyntaxError: unexpected character {character}")));
            };
            at += symbol.len();
            out.push(Token::Symbol(symbol));
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Operator {
    StrictEqual,
    StrictNotEqual,
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
    Remainder,
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Literal(Value),
    This,
    Name(String),
    Member(Box<Node>, Box<Node>),
    Not(Box<Node>),
    Negate(Box<Node>),
    Plus(Box<Node>),
    Binary(Operator, Box<Node>, Box<Node>),
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    Conditional(Box<Node>, Box<Node>, Box<Node>),
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

    fn take_symbol(&mut self, symbol: &str) -> bool {
        if matches!(self.peek(), Some(Token::Symbol(found)) if *found == symbol) {
            self.at += 1;
            return true;
        }
        false
    }

    fn take_name(&mut self, name: &str) -> bool {
        if matches!(self.peek(), Some(Token::Name(found)) if found == name) {
            self.at += 1;
            return true;
        }
        false
    }

    fn expect_symbol(&mut self, symbol: &str) -> DocumentResult<()> {
        if self.take_symbol(symbol) {
            Ok(())
        } else {
            Err(failure(format!("SyntaxError: expected {symbol}")))
        }
    }

    fn deeper(&mut self) -> DocumentResult<()> {
        self.depth += 1;
        if self.depth > MOST_DEPTH {
            return Err(failure("SyntaxError: the script is nested too deeply"));
        }
        Ok(())
    }

    fn expression(&mut self) -> DocumentResult<Node> {
        self.deeper()?;
        let condition = self.or()?;
        let node = if self.take_symbol("?") {
            let when = self.expression()?;
            self.expect_symbol(":")?;
            let otherwise = self.expression()?;
            Node::Conditional(Box::new(condition), Box::new(when), Box::new(otherwise))
        } else {
            condition
        };
        self.depth -= 1;
        Ok(node)
    }

    fn or(&mut self) -> DocumentResult<Node> {
        let mut left = self.and()?;
        while self.take_symbol("||") {
            left = Node::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> DocumentResult<Node> {
        let mut left = self.binary(0)?;
        while self.take_symbol("&&") {
            left = Node::And(Box::new(left), Box::new(self.binary(0)?));
        }
        Ok(left)
    }

    fn binary(&mut self, level: usize) -> DocumentResult<Node> {
        const LEVELS: [&[(&str, Operator)]; 4] = [
            &[
                ("===", Operator::StrictEqual),
                ("!==", Operator::StrictNotEqual),
                ("==", Operator::Equal),
                ("!=", Operator::NotEqual),
            ],
            &[
                ("<=", Operator::LessOrEqual),
                (">=", Operator::GreaterOrEqual),
                ("<", Operator::Less),
                (">", Operator::Greater),
            ],
            &[("+", Operator::Add), ("-", Operator::Subtract)],
            &[("*", Operator::Multiply), ("/", Operator::Divide), ("%", Operator::Remainder)],
        ];
        let Some(operators) = LEVELS.get(level) else {
            return self.unary();
        };
        let mut left = self.binary(level + 1)?;
        'more: loop {
            for (symbol, operator) in *operators {
                if self.take_symbol(symbol) {
                    left = Node::Binary(*operator, Box::new(left), Box::new(self.binary(level + 1)?));
                    continue 'more;
                }
            }
            return Ok(left);
        }
    }

    fn unary(&mut self) -> DocumentResult<Node> {
        self.deeper()?;
        let node = if self.take_symbol("!") {
            Node::Not(Box::new(self.unary()?))
        } else if self.take_symbol("-") {
            Node::Negate(Box::new(self.unary()?))
        } else if self.take_symbol("+") {
            Node::Plus(Box::new(self.unary()?))
        } else {
            self.postfix()?
        };
        self.depth -= 1;
        Ok(node)
    }

    fn postfix(&mut self) -> DocumentResult<Node> {
        let mut node = self.primary()?;
        loop {
            if self.take_symbol(".") {
                let Some(Token::Name(name)) = self.peek().cloned() else {
                    return Err(failure("SyntaxError: expected a property name"));
                };
                self.at += 1;
                node = Node::Member(Box::new(node), Box::new(Node::Literal(Value::Text(name))));
            } else if self.take_symbol("[") {
                let key = self.expression()?;
                self.expect_symbol("]")?;
                node = Node::Member(Box::new(node), Box::new(key));
            } else {
                return Ok(node);
            }
        }
    }

    fn primary(&mut self) -> DocumentResult<Node> {
        let token = self.peek().cloned().ok_or_else(|| failure("SyntaxError: unexpected end of input"))?;
        self.at += 1;
        Ok(match token {
            Token::Number(number) => Node::Literal(Value::Number(number)),
            Token::Text(text) => Node::Literal(Value::Text(text)),
            Token::Name(name) => match name.as_str() {
                "true" => Node::Literal(Value::Boolean(true)),
                "false" => Node::Literal(Value::Boolean(false)),
                "null" => Node::Literal(Value::Null),
                "undefined" => Node::Literal(Value::Undefined),
                "NaN" => Node::Literal(Value::Number(f64::NAN)),
                "Infinity" => Node::Literal(Value::Number(f64::INFINITY)),
                "this" | "obj" => Node::This,
                _ => Node::Name(name),
            },
            Token::Symbol("(") => {
                let inner = self.expression()?;
                self.expect_symbol(")")?;
                inner
            }
            Token::Symbol(symbol) => return Err(failure(format!("SyntaxError: unexpected {symbol}"))),
        })
    }

    fn script(&mut self) -> DocumentResult<Node> {
        let node = if self.take_name("function") {
            if matches!(self.peek(), Some(Token::Name(_))) {
                self.at += 1;
            }
            self.expect_symbol("(")?;
            self.expect_symbol(")")?;
            self.expect_symbol("{")?;
            if !self.take_name("return") {
                return Err(failure("SyntaxError: a $where function must be a single return statement"));
            }
            let node = self.expression()?;
            self.take_symbol(";");
            self.expect_symbol("}")?;
            node
        } else {
            self.take_name("return");
            self.expression()?
        };
        self.take_symbol(";");
        if self.at != self.tokens.len() {
            return Err(failure("SyntaxError: unexpected text after the expression"));
        }
        Ok(node)
    }
}

/// A `$where` predicate: one JavaScript expression, or a function whose body is one `return`,
/// over `this` (or `obj`) and the variables of a code-with-scope value.
#[derive(Debug, Clone, PartialEq)]
pub struct WhereScript {
    body: Node,
    scope: Document,
}

impl WhereScript {
    /// Reads `$where`'s value: a string, JavaScript code or code with a scope.
    pub fn parse(value: &Bson) -> DocumentResult<Self> {
        let (code, scope) = match value {
            Bson::String(code) | Bson::JavaScript(code) => (code.as_str(), Document::new()),
            Bson::JavaScriptWithScope { code, scope } => (code.as_str(), scope.clone()),
            _ => return Err(DocumentError::bad_value("$where got bad type")),
        };
        if code.chars().count() > MOST_SCRIPT_CHARS {
            return Err(failure(format!("a $where script may hold at most {MOST_SCRIPT_CHARS} characters")));
        }
        let mut parser = Parser { tokens: tokens(code)?, at: 0, depth: 0 };
        Ok(Self { body: parser.script()?, scope })
    }

    /// Whether the script's value is truthy for `document`; a script that fails, such as one
    /// reading a property of `undefined`, does not match.
    pub fn matches(&self, document: &Document) -> bool {
        self.evaluate(&self.body, document, 0).is_some_and(|value| value.truthy())
    }

    fn evaluate(&self, node: &Node, document: &Document, depth: usize) -> Option<Value> {
        if depth > MOST_DEPTH {
            return None;
        }
        let evaluate = |node: &Node| self.evaluate(node, document, depth + 1);
        Some(match node {
            Node::Literal(value) => value.clone(),
            Node::This => Value::Other(Bson::Document(document.clone())),
            Node::Name(name) => self.scope.get(name).map_or(Value::Undefined, Value::of),
            Node::Member(object, key) => member(evaluate(object)?, &evaluate(key)?)?,
            Node::Not(inner) => Value::Boolean(!evaluate(inner)?.truthy()),
            Node::Negate(inner) => Value::Number(-evaluate(inner)?.number()),
            Node::Plus(inner) => Value::Number(evaluate(inner)?.number()),
            Node::And(left, right) => {
                let left = evaluate(left)?;
                if left.truthy() { evaluate(right)? } else { left }
            }
            Node::Or(left, right) => {
                let left = evaluate(left)?;
                if left.truthy() { left } else { evaluate(right)? }
            }
            Node::Conditional(condition, when, otherwise) => {
                if evaluate(condition)?.truthy() { evaluate(when)? } else { evaluate(otherwise)? }
            }
            Node::Binary(operator, left, right) => binary(*operator, &evaluate(left)?, &evaluate(right)?),
        })
    }
}

fn member(object: Value, key: &Value) -> Option<Value> {
    let name = key.text();
    match object {
        Value::Undefined | Value::Null => None,
        Value::Text(text) if name == "length" => Some(Value::Number(text.chars().count() as f64)),
        Value::Text(text) => Some(
            name.parse::<usize>()
                .ok()
                .and_then(|index| text.chars().nth(index))
                .map_or(Value::Undefined, |character| Value::Text(character.to_string())),
        ),
        Value::Other(Bson::Document(fields)) => Some(fields.get(&name).map_or(Value::Undefined, Value::of)),
        Value::Other(Bson::Array(items)) if name == "length" => Some(Value::Number(items.len() as f64)),
        Value::Other(Bson::Array(items)) => {
            Some(name.parse::<usize>().ok().and_then(|index| items.get(index)).map_or(Value::Undefined, Value::of))
        }
        _ => Some(Value::Undefined),
    }
}

fn binary(operator: Operator, left: &Value, right: &Value) -> Value {
    let compared = |test: fn(Ordering) -> bool| {
        let order = match (left, right) {
            (Value::Text(left), Value::Text(right)) => Some(left.cmp(right)),
            _ => left.number().partial_cmp(&right.number()),
        };
        Value::Boolean(order.is_some_and(test))
    };
    match operator {
        Operator::StrictEqual => Value::Boolean(left.strictly_equal(right)),
        Operator::StrictNotEqual => Value::Boolean(!left.strictly_equal(right)),
        Operator::Equal => Value::Boolean(left.loosely_equal(right)),
        Operator::NotEqual => Value::Boolean(!left.loosely_equal(right)),
        Operator::Less => compared(Ordering::is_lt),
        Operator::LessOrEqual => compared(Ordering::is_le),
        Operator::Greater => compared(Ordering::is_gt),
        Operator::GreaterOrEqual => compared(Ordering::is_ge),
        Operator::Add => match (left, right) {
            (Value::Text(_) | Value::Other(_), _) | (_, Value::Text(_) | Value::Other(_)) => {
                Value::Text(left.text() + &right.text())
            }
            _ => Value::Number(left.number() + right.number()),
        },
        Operator::Subtract => Value::Number(left.number() - right.number()),
        Operator::Multiply => Value::Number(left.number() * right.number()),
        Operator::Divide => Value::Number(left.number() / right.number()),
        Operator::Remainder => Value::Number(left.number() % right.number()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> Document {
        let mut document = Document::new();
        document.insert("a", Bson::Int32(3));
        document.insert("b", Bson::Double(2.5));
        document.insert("name", Bson::String("egg".to_owned()));
        document.insert("tags", Bson::Array(vec![Bson::String("x".to_owned()), Bson::String("y".to_owned())]));
        document
    }

    #[test]
    fn expressions_and_functions_read_the_document() {
        let holds = |code: &str| WhereScript::parse(&Bson::String(code.to_owned())).unwrap().matches(&document());
        assert!(holds("this.a > this.b"));
        assert!(holds("function() { return obj.name == 'egg' && this.tags.length === 2; }"));
        assert!(holds("this.a + 1 == '4'"));
        assert!(!holds("this.a === '3'"));
        assert!(holds("this.tags[1] == 'y' ? true : false"));
        assert!(!holds("this.missing.field == 1"));
        assert!(WhereScript::parse(&Bson::String("this.a >".to_owned())).is_err());
    }
}
