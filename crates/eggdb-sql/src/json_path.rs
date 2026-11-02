use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{self, Json, Node};
use crate::numeric::Numeric;
use crate::regex::{CaseSensitivity, Options, StepBudget};
use crate::types::{SqlType, Value};
use std::borrow::Cow;
use std::cell::Cell;
use std::cmp::Ordering;

pub(crate) const FUNCTIONS: [&str; 8] = [
    "jsonb_path_exists",
    "jsonb_path_match",
    "jsonb_path_query_array",
    "jsonb_path_query_first",
    "jsonb_path_exists_tz",
    "jsonb_path_match_tz",
    "jsonb_path_query_array_tz",
    "jsonb_path_query_first_tz",
];

pub(crate) const SET_RETURNING: [&str; 2] = ["jsonb_path_query", "jsonb_path_query_tz"];

const MAX_NODES: usize = 1_000;
const MAX_ITEMS: u64 = 1_000_000;
const MATCH_STEPS: u64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Lax,
    Strict,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JsonPath {
    mode: Mode,
    expr: Expr,
}

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Root,
    Current,
    Last,
    Variable(String),
    Literal(Json),
    Steps(Box<Expr>, Vec<Step>),
    Negate(Box<Expr>),
    Plus(Box<Expr>),
    Arithmetic(Arithmetic, Box<Expr>, Box<Expr>),
    Predicate(Box<Predicate>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arithmetic {
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
}

impl Arithmetic {
    fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Subtract => "-",
            Self::Multiply => "*",
            Self::Divide => "/",
            Self::Modulo => "%",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Step {
    Member(String),
    AnyMember,
    AnyElement,
    Elements(Vec<Subscript>),
    Recursive { from: Level, to: Level },
    Filter(Box<Predicate>),
    Method(Method),
}

#[derive(Debug, Clone, PartialEq)]
struct Subscript {
    from: Expr,
    to: Option<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Number(u32),
    Last,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    Type,
    Size,
    Double,
    Ceiling,
    Floor,
    Abs,
    KeyValue,
    String,
    Number,
    Integer,
    BigInt,
    Boolean,
}

impl Method {
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "type" => Self::Type,
            "size" => Self::Size,
            "double" => Self::Double,
            "ceiling" => Self::Ceiling,
            "floor" => Self::Floor,
            "abs" => Self::Abs,
            "keyvalue" => Self::KeyValue,
            "string" => Self::String,
            "number" => Self::Number,
            "integer" => Self::Integer,
            "bigint" => Self::BigInt,
            "boolean" => Self::Boolean,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Type => "type",
            Self::Size => "size",
            Self::Double => "double",
            Self::Ceiling => "ceiling",
            Self::Floor => "floor",
            Self::Abs => "abs",
            Self::KeyValue => "keyvalue",
            Self::String => "string",
            Self::Number => "number",
            Self::Integer => "integer",
            Self::BigInt => "bigint",
            Self::Boolean => "boolean",
        }
    }

    fn unwraps(self) -> bool {
        !matches!(self, Self::Type | Self::Size)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Comparison {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

impl Comparison {
    fn holds(self, ordering: Ordering) -> bool {
        match self {
            Self::Equal => ordering == Ordering::Equal,
            Self::NotEqual => ordering != Ordering::Equal,
            Self::Less => ordering == Ordering::Less,
            Self::LessOrEqual => ordering != Ordering::Greater,
            Self::Greater => ordering == Ordering::Greater,
            Self::GreaterOrEqual => ordering != Ordering::Less,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Predicate {
    And(Box<Predicate>, Box<Predicate>),
    Or(Box<Predicate>, Box<Predicate>),
    Not(Box<Predicate>),
    IsUnknown(Box<Predicate>),
    Exists(Expr),
    Compare(Comparison, Expr, Expr),
    LikeRegex {
        subject: Expr,
        pattern: String,
        options: Options,
    },
    StartsWith(Expr, Expr),
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Dollar,
    Variable(String),
    At,
    Dot,
    Star,
    StarStar,
    LeftBracket,
    RightBracket,
    LeftParen,
    RightParen,
    LeftBrace,
    RightBrace,
    Comma,
    Question,
    Operator(&'static str),
    Number(String),
    String(String),
    Word(String),
}

fn syntax_error(near: Option<&str>) -> SqlError {
    let message = match near {
        Some(near) => format!("syntax error at or near \"{near}\" of jsonpath input"),
        None => "syntax error at end of jsonpath input".to_owned(),
    };
    SqlError::new(SqlState::SYNTAX_ERROR, message)
}

fn invalid_path(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::SYNTAX_ERROR, message)
}

fn is_word_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || !character.is_ascii()
}

fn hex_value(digits: &str) -> Option<char> {
    u32::from_str_radix(digits, 16).ok().and_then(char::from_u32)
}

struct Lexer<'t> {
    text: &'t str,
    chars: Vec<(usize, char)>,
    at: usize,
}

impl<'t> Lexer<'t> {
    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.at + ahead).map(|(_, character)| *character)
    }

    fn offset(&self) -> usize {
        self.chars
            .get(self.at)
            .map_or(self.text.len(), |(offset, _)| *offset)
    }

    fn near(&self, start: usize) -> String {
        self.text.get(start..).unwrap_or("").chars().take(20).collect()
    }

    fn tokens(mut self) -> SqlResult<Vec<Token>> {
        let mut tokens = Vec::new();
        while let Some(character) = self.peek(0) {
            if character.is_whitespace() {
                self.at += 1;
                continue;
            }
            tokens.push(self.token(character)?);
        }
        Ok(tokens)
    }

    fn word(&mut self) -> String {
        let mut word = String::new();
        while let Some(character) = self.peek(0).filter(|character| is_word_char(*character)) {
            word.push(character);
            self.at += 1;
        }
        word
    }

    fn token(&mut self, character: char) -> SqlResult<Token> {
        let start = self.offset();
        let next = self.peek(1);
        let (token, width) = match character {
            '$' => {
                self.at += 1;
                return Ok(match self.peek(0) {
                    Some('"') => Token::Variable(self.string()?),
                    Some(next) if is_word_char(next) => Token::Variable(self.word()),
                    _ => Token::Dollar,
                });
            }
            '"' => return self.string().map(Token::String),
            '0'..='9' => return Ok(self.number()),
            '@' => (Token::At, 1),
            '.' => (Token::Dot, 1),
            '*' if next == Some('*') => (Token::StarStar, 2),
            '*' => (Token::Star, 1),
            '[' => (Token::LeftBracket, 1),
            ']' => (Token::RightBracket, 1),
            '(' => (Token::LeftParen, 1),
            ')' => (Token::RightParen, 1),
            '{' => (Token::LeftBrace, 1),
            '}' => (Token::RightBrace, 1),
            ',' => (Token::Comma, 1),
            '?' => (Token::Question, 1),
            '=' if next == Some('=') => (Token::Operator("=="), 2),
            '!' if next == Some('=') => (Token::Operator("!="), 2),
            '<' if next == Some('>') => (Token::Operator("!="), 2),
            '<' if next == Some('=') => (Token::Operator("<="), 2),
            '>' if next == Some('=') => (Token::Operator(">="), 2),
            '&' if next == Some('&') => (Token::Operator("&&"), 2),
            '|' if next == Some('|') => (Token::Operator("||"), 2),
            '<' => (Token::Operator("<"), 1),
            '>' => (Token::Operator(">"), 1),
            '!' => (Token::Operator("!"), 1),
            '+' => (Token::Operator("+"), 1),
            '-' => (Token::Operator("-"), 1),
            '/' => (Token::Operator("/"), 1),
            '%' => (Token::Operator("%"), 1),
            _ if is_word_char(character) => return Ok(Token::Word(self.word())),
            _ => return Err(syntax_error(Some(&self.near(start)))),
        };
        self.at += width;
        Ok(token)
    }

    fn digits(&mut self, out: &mut String) {
        while let Some(digit) = self.peek(0).filter(char::is_ascii_digit) {
            out.push(digit);
            self.at += 1;
        }
    }

    fn number(&mut self) -> Token {
        let mut text = String::new();
        self.digits(&mut text);
        if self.peek(0) == Some('.') && self.peek(1).is_some_and(|next| next.is_ascii_digit()) {
            text.push('.');
            self.at += 1;
            self.digits(&mut text);
        }
        if matches!(self.peek(0), Some('e' | 'E')) {
            let signed = matches!(self.peek(1), Some('+' | '-'));
            let digit_at = if signed { 2 } else { 1 };
            if self.peek(digit_at).is_some_and(|next| next.is_ascii_digit()) {
                text.push('e');
                if signed {
                    text.extend(self.peek(1));
                }
                self.at += digit_at;
                self.digits(&mut text);
            }
        }
        Token::Number(text)
    }

    fn hex_digits(&mut self, count: usize, start: usize) -> SqlResult<u32> {
        let mut value = 0u32;
        for _ in 0..count {
            let digit = self
                .peek(0)
                .and_then(|digit| digit.to_digit(16))
                .ok_or_else(|| syntax_error(Some(&self.near(start))))?;
            value = value * 16 + digit;
            self.at += 1;
        }
        Ok(value)
    }

    fn code_point(value: u32) -> SqlResult<char> {
        if value == 0 {
            return Err(SqlError::new(
                SqlState::UNTRANSLATABLE_CHARACTER,
                "unsupported Unicode escape sequence",
            ));
        }
        char::from_u32(value)
            .ok_or_else(|| invalid_path("invalid Unicode escape value in jsonpath input"))
    }

    fn unicode_escape(&mut self, start: usize) -> SqlResult<char> {
        let value = self.hex_digits(4, start)?;
        if (0xDC00..0xE000).contains(&value) {
            return Err(invalid_path(
                "invalid Unicode surrogate pair in jsonpath input",
            ));
        }
        if !(0xD800..0xDC00).contains(&value) {
            return Self::code_point(value);
        }
        if self.peek(0) != Some('\\') || self.peek(1) != Some('u') {
            return Err(invalid_path(
                "invalid Unicode surrogate pair in jsonpath input",
            ));
        }
        self.at += 2;
        let low = self.hex_digits(4, start)?;
        if !(0xDC00..0xE000).contains(&low) {
            return Err(invalid_path(
                "invalid Unicode surrogate pair in jsonpath input",
            ));
        }
        Self::code_point(0x10000 + ((value - 0xD800) << 10) + (low - 0xDC00))
    }

    fn string(&mut self) -> SqlResult<String> {
        let start = self.offset();
        self.at += 1;
        let mut out = String::new();
        loop {
            let Some(character) = self.peek(0) else {
                return Err(invalid_path(
                    "unterminated quoted string in jsonpath input",
                ));
            };
            self.at += 1;
            match character {
                '"' => return Ok(out),
                '\\' => {
                    let Some(escaped) = self.peek(0) else {
                        return Err(syntax_error(None));
                    };
                    self.at += 1;
                    match escaped {
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'v' => out.push('\u{b}'),
                        'x' => {
                            let value = self.hex_digits(2, start)?;
                            out.push(Self::code_point(value)?);
                        }
                        'u' if self.peek(0) == Some('{') => {
                            self.at += 1;
                            let mut digits = String::new();
                            while let Some(digit) = self.peek(0).filter(char::is_ascii_hexdigit) {
                                if digits.len() >= 6 {
                                    return Err(syntax_error(Some(&self.near(start))));
                                }
                                digits.push(digit);
                                self.at += 1;
                            }
                            if self.peek(0) != Some('}') || digits.is_empty() {
                                return Err(syntax_error(Some(&self.near(start))));
                            }
                            self.at += 1;
                            out.push(
                                hex_value(&digits)
                                    .filter(|character| *character != '\0')
                                    .ok_or_else(|| {
                                        invalid_path(
                                            "invalid Unicode escape value in jsonpath input",
                                        )
                                    })?,
                            );
                        }
                        'u' => out.push(self.unicode_escape(start)?),
                        other => out.push(other),
                    }
                }
                other => out.push(other),
            }
        }
    }
}

enum Parsed {
    Value(Expr),
    Truth(Predicate),
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    nodes: usize,
    in_filter: usize,
    in_subscript: usize,
}

fn token_text(token: &Token) -> String {
    match token {
        Token::Dollar => "$".to_owned(),
        Token::Variable(name) => format!("${name}"),
        Token::At => "@".to_owned(),
        Token::Dot => ".".to_owned(),
        Token::Star => "*".to_owned(),
        Token::StarStar => "**".to_owned(),
        Token::LeftBracket => "[".to_owned(),
        Token::RightBracket => "]".to_owned(),
        Token::LeftParen => "(".to_owned(),
        Token::RightParen => ")".to_owned(),
        Token::LeftBrace => "{".to_owned(),
        Token::RightBrace => "}".to_owned(),
        Token::Comma => ",".to_owned(),
        Token::Question => "?".to_owned(),
        Token::Operator(symbol) => (*symbol).to_owned(),
        Token::Number(text) | Token::Word(text) => text.clone(),
        Token::String(text) => format!("\"{text}\""),
    }
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn peek_at(&self, ahead: usize) -> Option<&Token> {
        self.tokens.get(self.at + ahead)
    }

    fn unexpected(&self) -> SqlError {
        syntax_error(self.peek().map(token_text).as_deref())
    }

    fn take(&mut self, token: &Token) -> bool {
        if self.peek() == Some(token) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: &Token) -> SqlResult<()> {
        if self.take(token) {
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    fn is_word(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Word(found)) if found == word)
    }

    fn is_word_at(&self, ahead: usize, word: &str) -> bool {
        matches!(self.peek_at(ahead), Some(Token::Word(found)) if found == word)
    }

    fn take_word(&mut self, word: &str) -> bool {
        if self.is_word(word) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn node(&mut self) -> SqlResult<()> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(SqlError::new(
                SqlState::STATEMENT_TOO_COMPLEX,
                "jsonpath expression is too complex",
            ));
        }
        crate::stack_depth::check()
    }

    fn truth(&self, parsed: Parsed) -> SqlResult<Predicate> {
        match parsed {
            Parsed::Truth(predicate) => Ok(predicate),
            Parsed::Value(_) => Err(self.unexpected()),
        }
    }

    fn value(&self, parsed: Parsed) -> SqlResult<Expr> {
        match parsed {
            Parsed::Value(expr) => Ok(expr),
            Parsed::Truth(_) => Err(self.unexpected()),
        }
    }

    fn or_level(&mut self) -> SqlResult<Parsed> {
        self.node()?;
        let mut left = self.and_level()?;
        while self.take(&Token::Operator("||")) {
            let first = self.truth(left)?;
            let right = self.and_level()?;
            let second = self.truth(right)?;
            self.node()?;
            left = Parsed::Truth(Predicate::Or(Box::new(first), Box::new(second)));
        }
        Ok(left)
    }

    fn and_level(&mut self) -> SqlResult<Parsed> {
        let mut left = self.not_level()?;
        while self.take(&Token::Operator("&&")) {
            let first = self.truth(left)?;
            let right = self.not_level()?;
            let second = self.truth(right)?;
            self.node()?;
            left = Parsed::Truth(Predicate::And(Box::new(first), Box::new(second)));
        }
        Ok(left)
    }

    fn not_level(&mut self) -> SqlResult<Parsed> {
        if self.take(&Token::Operator("!")) {
            self.node()?;
            self.expect(&Token::LeftParen)?;
            let inner = self.or_level()?;
            let inner = self.truth(inner)?;
            self.expect(&Token::RightParen)?;
            return Ok(Parsed::Truth(Predicate::Not(Box::new(inner))));
        }
        self.comparison_level()
    }

    fn comparison_level(&mut self) -> SqlResult<Parsed> {
        if self.is_word("exists") && self.peek_at(1) == Some(&Token::LeftParen) {
            self.at += 2;
            self.node()?;
            let inner = self.or_level()?;
            let inner = self.value(inner)?;
            self.expect(&Token::RightParen)?;
            return Ok(Parsed::Truth(Predicate::Exists(inner)));
        }
        let left = self.additive()?;
        if let Parsed::Truth(predicate) = left {
            if self.is_word("is") && self.is_word_at(1, "unknown") {
                self.at += 2;
                return Ok(Parsed::Truth(Predicate::IsUnknown(Box::new(predicate))));
            }
            return Ok(Parsed::Truth(predicate));
        }
        let Parsed::Value(left) = left else {
            return Err(self.unexpected());
        };
        let comparison = match self.peek() {
            Some(Token::Operator("==")) => Some(Comparison::Equal),
            Some(Token::Operator("!=")) => Some(Comparison::NotEqual),
            Some(Token::Operator("<")) => Some(Comparison::Less),
            Some(Token::Operator("<=")) => Some(Comparison::LessOrEqual),
            Some(Token::Operator(">")) => Some(Comparison::Greater),
            Some(Token::Operator(">=")) => Some(Comparison::GreaterOrEqual),
            _ => None,
        };
        if let Some(comparison) = comparison {
            self.at += 1;
            self.node()?;
            let right = self.additive()?;
            let right = self.value(right)?;
            return Ok(Parsed::Truth(Predicate::Compare(comparison, left, right)));
        }
        if self.take_word("like_regex") {
            self.node()?;
            let Some(Token::String(pattern)) = self.peek().cloned() else {
                return Err(self.unexpected());
            };
            self.at += 1;
            let mut options = Options {
                newline_excluded: true,
                ..Options::default()
            };
            if self.take_word("flag") {
                let Some(Token::String(flags)) = self.peek().cloned() else {
                    return Err(self.unexpected());
                };
                self.at += 1;
                apply_flags(&flags, &mut options)?;
            }
            crate::regex::compiled_with(&pattern, options)?;
            return Ok(Parsed::Truth(Predicate::LikeRegex {
                subject: left,
                pattern,
                options,
            }));
        }
        if self.is_word("starts") && self.is_word_at(1, "with") {
            self.at += 2;
            self.node()?;
            let prefix = match self.peek().cloned() {
                Some(Token::String(text)) => Expr::Literal(Json::String(text)),
                Some(Token::Variable(name)) => Expr::Variable(name),
                _ => return Err(self.unexpected()),
            };
            self.at += 1;
            return Ok(Parsed::Truth(Predicate::StartsWith(left, prefix)));
        }
        Ok(Parsed::Value(left))
    }

    fn additive(&mut self) -> SqlResult<Parsed> {
        let mut left = self.multiplicative()?;
        loop {
            let operator = match self.peek() {
                Some(Token::Operator("+")) => Arithmetic::Add,
                Some(Token::Operator("-")) => Arithmetic::Subtract,
                _ => return Ok(left),
            };
            self.at += 1;
            self.node()?;
            let first = self.value(left)?;
            let right = self.multiplicative()?;
            let second = self.value(right)?;
            left = Parsed::Value(Expr::Arithmetic(operator, Box::new(first), Box::new(second)));
        }
    }

    fn multiplicative(&mut self) -> SqlResult<Parsed> {
        let mut left = self.unary()?;
        loop {
            let operator = match self.peek() {
                Some(Token::Star) => Arithmetic::Multiply,
                Some(Token::Operator("/")) => Arithmetic::Divide,
                Some(Token::Operator("%")) => Arithmetic::Modulo,
                _ => return Ok(left),
            };
            self.at += 1;
            self.node()?;
            let first = self.value(left)?;
            let right = self.unary()?;
            let second = self.value(right)?;
            left = Parsed::Value(Expr::Arithmetic(operator, Box::new(first), Box::new(second)));
        }
    }

    fn unary(&mut self) -> SqlResult<Parsed> {
        let negate = match self.peek() {
            Some(Token::Operator("-")) => true,
            Some(Token::Operator("+")) => false,
            _ => return self.accessor_expr(),
        };
        self.at += 1;
        self.node()?;
        let operand = self.unary()?;
        let operand = Box::new(self.value(operand)?);
        Ok(Parsed::Value(if negate {
            Expr::Negate(operand)
        } else {
            Expr::Plus(operand)
        }))
    }

    fn primary(&mut self) -> SqlResult<Parsed> {
        self.node()?;
        let Some(token) = self.peek().cloned() else {
            return Err(syntax_error(None));
        };
        self.at += 1;
        Ok(Parsed::Value(match token {
            Token::Dollar => Expr::Root,
            Token::At => {
                if self.in_filter == 0 {
                    return Err(invalid_path("@ is not allowed in root expressions"));
                }
                Expr::Current
            }
            Token::Variable(name) => Expr::Variable(name),
            Token::Number(text) => {
                Numeric::parse(&text).map_err(|_| syntax_error(Some(&text)))?;
                Expr::Literal(Json::Number(text))
            }
            Token::String(text) => Expr::Literal(Json::String(text)),
            Token::Word(word) => match word.as_str() {
                "true" => Expr::Literal(Json::Boolean(true)),
                "false" => Expr::Literal(Json::Boolean(false)),
                "null" => Expr::Literal(Json::Null),
                "last" => {
                    if self.in_subscript == 0 {
                        return Err(invalid_path("LAST is allowed only in array subscripts"));
                    }
                    Expr::Last
                }
                _ => {
                    self.at -= 1;
                    return Err(self.unexpected());
                }
            },
            Token::LeftParen => {
                let inner = self.or_level()?;
                self.expect(&Token::RightParen)?;
                return Ok(inner);
            }
            _ => {
                self.at -= 1;
                return Err(self.unexpected());
            }
        }))
    }

    fn accessor_expr(&mut self) -> SqlResult<Parsed> {
        let start = self.primary()?;
        let mut steps = Vec::new();
        loop {
            match self.peek() {
                Some(Token::Dot) => {
                    self.at += 1;
                    self.node()?;
                    steps.push(self.dot_step()?);
                }
                Some(Token::LeftBracket) => {
                    self.at += 1;
                    self.node()?;
                    steps.push(self.bracket_step()?);
                }
                Some(Token::Question) => {
                    self.at += 1;
                    self.node()?;
                    self.expect(&Token::LeftParen)?;
                    self.in_filter += 1;
                    let inner = self.or_level()?;
                    self.in_filter -= 1;
                    let predicate = self.truth(inner)?;
                    self.expect(&Token::RightParen)?;
                    steps.push(Step::Filter(Box::new(predicate)));
                }
                _ => break,
            }
        }
        if steps.is_empty() {
            return Ok(start);
        }
        let start = self.value(start)?;
        Ok(Parsed::Value(Expr::Steps(Box::new(start), steps)))
    }

    fn dot_step(&mut self) -> SqlResult<Step> {
        let Some(token) = self.peek().cloned() else {
            return Err(syntax_error(None));
        };
        self.at += 1;
        match token {
            Token::Star => Ok(Step::AnyMember),
            Token::StarStar => self.recursive_step(),
            Token::String(key) => Ok(Step::Member(key)),
            Token::Word(name) if self.peek() == Some(&Token::LeftParen) => {
                let method = Method::named(&name).ok_or_else(|| self.unexpected())?;
                self.at += 1;
                self.expect(&Token::RightParen)?;
                Ok(Step::Method(method))
            }
            Token::Word(name) => Ok(Step::Member(name)),
            Token::Number(text) if text.bytes().all(|byte| byte.is_ascii_digit()) => {
                Ok(Step::Member(text))
            }
            _ => {
                self.at -= 1;
                Err(self.unexpected())
            }
        }
    }

    fn level(&mut self) -> SqlResult<Level> {
        match self.peek().cloned() {
            Some(Token::Word(word)) if word == "last" => {
                self.at += 1;
                Ok(Level::Last)
            }
            Some(Token::Number(text)) => {
                self.at += 1;
                text.parse::<u32>()
                    .map(Level::Number)
                    .map_err(|_| syntax_error(Some(&text)))
            }
            _ => Err(self.unexpected()),
        }
    }

    fn recursive_step(&mut self) -> SqlResult<Step> {
        if !self.take(&Token::LeftBrace) {
            return Ok(Step::Recursive {
                from: Level::Number(0),
                to: Level::Last,
            });
        }
        let from = self.level()?;
        let to = if self.take_word("to") {
            self.level()?
        } else {
            from
        };
        self.expect(&Token::RightBrace)?;
        Ok(Step::Recursive { from, to })
    }

    fn bracket_step(&mut self) -> SqlResult<Step> {
        if self.take(&Token::Star) {
            self.expect(&Token::RightBracket)?;
            return Ok(Step::AnyElement);
        }
        self.in_subscript += 1;
        let mut subscripts = Vec::new();
        loop {
            let from = self.or_level()?;
            let from = self.value(from)?;
            let to = if self.take_word("to") {
                let to = self.or_level()?;
                Some(self.value(to)?)
            } else {
                None
            };
            subscripts.push(Subscript { from, to });
            if !self.take(&Token::Comma) {
                break;
            }
        }
        self.in_subscript -= 1;
        self.expect(&Token::RightBracket)?;
        Ok(Step::Elements(subscripts))
    }
}

fn apply_flags(flags: &str, options: &mut Options) -> SqlResult<()> {
    for flag in flags.chars() {
        match flag {
            'i' => options.case = CaseSensitivity::Insensitive,
            's' => options.newline_excluded = false,
            'm' => options.line_anchors = true,
            'q' => options.literal = true,
            'x' => {
                return Err(SqlError::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "XQuery \"x\" flag (expanded regular expressions) is not implemented",
                ));
            }
            other => {
                return Err(invalid_path(format!(
                    "Unrecognized flag character \"{other}\" in LIKE_REGEX predicate."
                )));
            }
        }
    }
    Ok(())
}

/// Reads `text` as an SQL/JSON path expression, PostgreSQL's `jsonpath` syntax.
pub(crate) fn parse(text: &str) -> SqlResult<JsonPath> {
    let tokens = Lexer {
        text,
        chars: text.char_indices().collect(),
        at: 0,
    }
    .tokens()?;
    let mut parser = Parser {
        tokens,
        at: 0,
        nodes: 0,
        in_filter: 0,
        in_subscript: 0,
    };
    let mode = if parser.take_word("strict") {
        Mode::Strict
    } else {
        parser.take_word("lax");
        Mode::Lax
    };
    let parsed = parser.or_level()?;
    if parser.peek().is_some() {
        return Err(parser.unexpected());
    }
    let expr = match parsed {
        Parsed::Value(expr) => expr,
        Parsed::Truth(predicate) => Expr::Predicate(Box::new(predicate)),
    };
    Ok(JsonPath { mode, expr })
}

enum Failure {
    Suppressible(SqlError),
    Fatal(SqlError),
}

type Run<T> = Result<T, Failure>;
type Items<'a> = Vec<Cow<'a, Json>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Truth {
    True,
    False,
    Unknown,
}

impl Truth {
    fn of(flag: bool) -> Self {
        if flag { Self::True } else { Self::False }
    }
}

fn suppressible(state: SqlState, message: impl Into<String>) -> Failure {
    Failure::Suppressible(SqlError::new(state, message))
}

fn fatal(error: SqlError) -> Failure {
    Failure::Fatal(error)
}

fn number_of(json: &Json) -> Option<Numeric> {
    match json {
        Json::Number(text) => Numeric::parse(text).ok(),
        _ => None,
    }
}

fn number_json(number: &Numeric) -> Json {
    Json::Number(number.to_string())
}

fn node(value: Json) -> Node {
    Node {
        value,
        start: 0,
        end: 0,
    }
}

fn borrowed_children<'a>(
    item: &Cow<'a, Json>,
    pick: impl for<'b> Fn(&'b Json) -> Vec<&'b Json>,
) -> Items<'a> {
    match item {
        Cow::Borrowed(json) => pick(json).into_iter().map(Cow::Borrowed).collect(),
        Cow::Owned(json) => pick(json)
            .into_iter()
            .map(|child| Cow::Owned(child.clone()))
            .collect(),
    }
}

fn elements(json: &Json) -> Vec<&Json> {
    match json {
        Json::Array(items) => items.iter().map(|item| &item.value).collect(),
        _ => Vec::new(),
    }
}

fn member_values(json: &Json) -> Vec<&Json> {
    match json {
        Json::Object(members) => members.iter().map(|(_, item)| &item.value).collect(),
        _ => Vec::new(),
    }
}

fn member<'j>(json: &'j Json, key: &str) -> Option<&'j Json> {
    match json {
        Json::Object(members) => members
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, item)| &item.value),
        _ => None,
    }
}

fn collect_levels<'j>(
    json: &'j Json,
    level: u32,
    from: u32,
    to: u32,
    out: &mut Vec<&'j Json>,
) {
    if level >= from {
        out.push(json);
    }
    if level >= to {
        return;
    }
    let children = match json {
        Json::Array(_) => elements(json),
        Json::Object(_) => member_values(json),
        _ => return,
    };
    for child in children {
        collect_levels(child, level.saturating_add(1), from, to, out);
    }
}

fn level_number(level: Level) -> u32 {
    match level {
        Level::Number(number) => number,
        Level::Last => u32::MAX,
    }
}

struct Evaluator<'a> {
    root: &'a Json,
    vars: Option<&'a Json>,
    mode: Mode,
    produced: Cell<u64>,
}

impl<'a> Evaluator<'a> {
    fn lax(&self) -> bool {
        self.mode == Mode::Lax
    }

    fn charge(&self, count: usize) -> Run<()> {
        let total = self
            .produced
            .get()
            .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        self.produced.set(total);
        if total > MAX_ITEMS {
            return Err(fatal(SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                "jsonpath evaluation produced too many items",
            )));
        }
        Ok(())
    }

    fn unwrapped(&self, items: Items<'a>) -> Items<'a> {
        if !self.lax() {
            return items;
        }
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            if matches!(item.as_ref(), Json::Array(_)) {
                out.extend(borrowed_children(&item, elements));
            } else {
                out.push(item);
            }
        }
        out
    }

    fn eval(&self, expr: &Expr, current: Option<&Cow<'a, Json>>, last: Option<i64>) -> Run<Items<'a>> {
        crate::stack_depth::check().map_err(fatal)?;
        let items = match expr {
            Expr::Root => vec![Cow::Borrowed(self.root)],
            Expr::Current => current.cloned().into_iter().collect(),
            Expr::Last => match last {
                Some(last) => vec![Cow::Owned(number_json(&Numeric::from_i64(last)))],
                None => {
                    return Err(fatal(invalid_path(
                        "evaluating jsonpath LAST outside of array subscript",
                    )));
                }
            },
            Expr::Variable(name) => match self.vars.and_then(|vars| member(vars, name)) {
                Some(value) => vec![Cow::Borrowed(value)],
                None => {
                    return Err(fatal(SqlError::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("could not find jsonpath variable \"{name}\""),
                    )));
                }
            },
            Expr::Literal(json) => vec![Cow::Owned(json.clone())],
            Expr::Steps(start, steps) => {
                let mut items = self.eval(start, current, last)?;
                for step in steps {
                    let mut next = Vec::new();
                    for item in &items {
                        let before = next.len();
                        self.step(step, item, current, last, &mut next)?;
                        self.charge(next.len() - before)?;
                    }
                    items = next;
                }
                items
            }
            Expr::Negate(operand) | Expr::Plus(operand) => {
                let negate = matches!(expr, Expr::Negate(_));
                let operands = self.eval(operand, current, last)?;
                let mut out = Vec::with_capacity(operands.len());
                for item in self.unwrapped(operands) {
                    let Some(number) = number_of(&item) else {
                        return Err(suppressible(
                            SqlState::SQL_JSON_NUMBER_NOT_FOUND,
                            format!(
                                "operand of unary jsonpath operator {} is not a numeric value",
                                if negate { "-" } else { "+" }
                            ),
                        ));
                    };
                    out.push(Cow::Owned(if negate {
                        number_json(&number.negated())
                    } else {
                        item.into_owned()
                    }));
                }
                out
            }
            Expr::Arithmetic(operator, left, right) => {
                vec![Cow::Owned(self.arithmetic(*operator, left, right, current, last)?)]
            }
            Expr::Predicate(predicate) => vec![Cow::Owned(
                match self.predicate(predicate, current, last)? {
                    Truth::True => Json::Boolean(true),
                    Truth::False => Json::Boolean(false),
                    Truth::Unknown => Json::Null,
                },
            )],
        };
        self.charge(items.len())?;
        Ok(items)
    }

    fn single_number(
        &self,
        expr: &Expr,
        current: Option<&Cow<'a, Json>>,
        last: Option<i64>,
        side: &str,
        symbol: &str,
    ) -> Run<Numeric> {
        let items = self.eval(expr, current, last)?;
        let items = self.unwrapped(items);
        match items.as_slice() {
            [item] => number_of(item),
            _ => None,
        }
        .ok_or_else(|| {
            suppressible(
                SqlState::SINGLETON_SQL_JSON_ITEM_REQUIRED,
                format!("{side} operand of jsonpath operator {symbol} is not a single numeric value"),
            )
        })
    }

    fn arithmetic(
        &self,
        operator: Arithmetic,
        left: &Expr,
        right: &Expr,
        current: Option<&Cow<'a, Json>>,
        last: Option<i64>,
    ) -> Run<Json> {
        let symbol = operator.symbol();
        let left = self.single_number(left, current, last, "left", symbol)?;
        let right = self.single_number(right, current, last, "right", symbol)?;
        if matches!(operator, Arithmetic::Divide | Arithmetic::Modulo) && right.is_zero() {
            return Err(suppressible(SqlState::DIVISION_BY_ZERO, "division by zero"));
        }
        let result = match operator {
            Arithmetic::Add => left.add(&right),
            Arithmetic::Subtract => left.subtract(&right),
            Arithmetic::Multiply => left.multiply(&right),
            Arithmetic::Divide => left.divide(&right),
            Arithmetic::Modulo => left.remainder(&right),
        };
        result.map(|number| number_json(&number)).map_err(Failure::Suppressible)
    }

    #[allow(clippy::ptr_arg)]
    fn step(
        &self,
        step: &Step,
        item: &Cow<'a, Json>,
        current: Option<&Cow<'a, Json>>,
        last: Option<i64>,
        out: &mut Items<'a>,
    ) -> Run<()> {
        match step {
            Step::Member(key) => match item.as_ref() {
                Json::Object(_) => {
                    let found = borrowed_children(item, |json| member(json, key).into_iter().collect());
                    if found.is_empty() && !self.lax() {
                        return Err(suppressible(
                            SqlState::SQL_JSON_MEMBER_NOT_FOUND,
                            format!("JSON object does not contain key \"{key}\""),
                        ));
                    }
                    out.extend(found);
                }
                Json::Array(_) if self.lax() => {
                    for element in borrowed_children(item, elements) {
                        out.extend(borrowed_children(&element, |json| {
                            member(json, key).into_iter().collect()
                        }));
                    }
                }
                _ if self.lax() => {}
                _ => {
                    return Err(suppressible(
                        SqlState::SQL_JSON_MEMBER_NOT_FOUND,
                        "jsonpath member accessor can only be applied to an object",
                    ));
                }
            },
            Step::AnyMember => match item.as_ref() {
                Json::Object(_) => out.extend(borrowed_children(item, member_values)),
                Json::Array(_) if self.lax() => {
                    for element in borrowed_children(item, elements) {
                        out.extend(borrowed_children(&element, member_values));
                    }
                }
                _ if self.lax() => {}
                _ => {
                    return Err(suppressible(
                        SqlState::SQL_JSON_OBJECT_NOT_FOUND,
                        "jsonpath wildcard member accessor can only be applied to an object",
                    ));
                }
            },
            Step::AnyElement => match item.as_ref() {
                Json::Array(_) => out.extend(borrowed_children(item, elements)),
                _ if self.lax() => out.push(item.clone()),
                _ => {
                    return Err(suppressible(
                        SqlState::SQL_JSON_ARRAY_NOT_FOUND,
                        "jsonpath wildcard array accessor can only be applied to an array",
                    ));
                }
            },
            Step::Elements(subscripts) => self.elements(subscripts, item, current, out)?,
            Step::Recursive { from, to } => {
                let (from, to) = (level_number(*from), level_number(*to));
                out.extend(borrowed_children(item, |json| {
                    let mut found = Vec::new();
                    collect_levels(json, 0, from, to, &mut found);
                    found
                }));
            }
            Step::Filter(predicate) => {
                let candidates = if self.lax() && matches!(item.as_ref(), Json::Array(_)) {
                    borrowed_children(item, elements)
                } else {
                    vec![item.clone()]
                };
                for candidate in candidates {
                    if self.predicate(predicate, Some(&candidate), last)? == Truth::True {
                        out.push(candidate);
                    }
                }
            }
            Step::Method(method) => {
                let targets = if method.unwraps() && self.lax() && matches!(item.as_ref(), Json::Array(_)) {
                    borrowed_children(item, elements)
                } else {
                    vec![item.clone()]
                };
                for target in &targets {
                    match self.method(*method, target)? {
                        Json::Array(pairs) if *method == Method::KeyValue => {
                            out.extend(pairs.into_iter().map(|pair| Cow::Owned(pair.value)));
                        }
                        result => out.push(Cow::Owned(result)),
                    }
                }
            }
        }
        Ok(())
    }

    fn subscript(&self, expr: &Expr, current: Option<&Cow<'a, Json>>, last: i64) -> Run<i64> {
        let items = self.eval(expr, current, Some(last))?;
        let number = match items.as_slice() {
            [item] => number_of(item),
            _ => None,
        }
        .ok_or_else(|| {
            suppressible(
                SqlState::INVALID_SQL_JSON_SUBSCRIPT,
                "jsonpath array subscript is not a single numeric value",
            )
        })?;
        number
            .truncated(0)
            .ok()
            .and_then(|whole| whole.integral_value())
            .and_then(|whole| i32::try_from(whole).ok())
            .map(i64::from)
            .ok_or_else(|| {
                suppressible(
                    SqlState::INVALID_SQL_JSON_SUBSCRIPT,
                    "jsonpath array subscript is out of integer range",
                )
            })
    }

    #[allow(clippy::ptr_arg)]
    fn elements(
        &self,
        subscripts: &[Subscript],
        item: &Cow<'a, Json>,
        current: Option<&Cow<'a, Json>>,
        out: &mut Items<'a>,
    ) -> Run<()> {
        let array: Items<'a> = match item.as_ref() {
            Json::Array(_) => borrowed_children(item, elements),
            _ if self.lax() => vec![item.clone()],
            _ => {
                return Err(suppressible(
                    SqlState::SQL_JSON_ARRAY_NOT_FOUND,
                    "jsonpath array accessor can only be applied to an array",
                ));
            }
        };
        let size = i64::try_from(array.len()).unwrap_or(i64::MAX);
        let last = size - 1;
        for subscript in subscripts {
            let mut from = self.subscript(&subscript.from, current, last)?;
            let mut to = match &subscript.to {
                Some(to) => self.subscript(to, current, last)?,
                None => from,
            };
            if from < 0 || from > to || to >= size {
                if !self.lax() {
                    return Err(suppressible(
                        SqlState::INVALID_SQL_JSON_SUBSCRIPT,
                        "jsonpath array subscript is out of bounds",
                    ));
                }
                from = from.max(0);
                to = to.min(last);
            }
            let before = out.len();
            let mut index = from;
            while index <= to {
                if let Some(element) = usize::try_from(index).ok().and_then(|at| array.get(at)) {
                    out.push(element.clone());
                }
                index += 1;
            }
            self.charge(out.len() - before)?;
        }
        Ok(())
    }

    fn method(&self, method: Method, item: &Json) -> Run<Json> {
        let name = method.name();
        let numeric_only = || {
            suppressible(
                SqlState::NON_NUMERIC_SQL_JSON_ITEM,
                format!("jsonpath item method .{name}() can only be applied to a numeric value"),
            )
        };
        let cannot_cast = |message: String| {
            suppressible(SqlState::SQL_JSON_ITEM_CANNOT_BE_CAST_TO_TARGET_TYPE, message)
        };
        Ok(match method {
            Method::Type => Json::String(json::type_of(item).to_owned()),
            Method::Size => match item {
                Json::Array(items) => number_json(&Numeric::from_i64(
                    i64::try_from(items.len()).unwrap_or(i64::MAX),
                )),
                _ if self.lax() => number_json(&Numeric::from_i64(1)),
                _ => {
                    return Err(suppressible(
                        SqlState::SQL_JSON_ARRAY_NOT_FOUND,
                        "jsonpath item method .size() can only be applied to an array",
                    ));
                }
            },
            Method::Ceiling | Method::Floor | Method::Abs => {
                let number = number_of(item).ok_or_else(numeric_only)?;
                let result = match method {
                    Method::Ceiling => number.ceiling(),
                    Method::Floor => number.floor(),
                    _ => Ok(number.absolute()),
                };
                number_json(&result.map_err(Failure::Suppressible)?)
            }
            Method::Double => {
                let invalid = |text: &str| {
                    suppressible(
                        SqlState::NON_NUMERIC_SQL_JSON_ITEM,
                        format!(
                            "argument \"{text}\" of jsonpath item method .double() is invalid for type double precision"
                        ),
                    )
                };
                let not_finite = || {
                    suppressible(
                        SqlState::NON_NUMERIC_SQL_JSON_ITEM,
                        "NaN or Infinity is not allowed for jsonpath item method .double()",
                    )
                };
                match item {
                    Json::Number(text) => {
                        let value: f64 = text.parse().map_err(|_| invalid(text))?;
                        if !value.is_finite() {
                            return Err(not_finite());
                        }
                        item.clone()
                    }
                    Json::String(text) => {
                        let value: f64 = text.trim().parse().map_err(|_| invalid(text))?;
                        if !value.is_finite() {
                            return Err(not_finite());
                        }
                        number_json(&Numeric::from_f64(value, 15).map_err(Failure::Suppressible)?)
                    }
                    _ => {
                        return Err(suppressible(
                            SqlState::NON_NUMERIC_SQL_JSON_ITEM,
                            "jsonpath item method .double() can only be applied to a string or numeric value",
                        ));
                    }
                }
            }
            Method::KeyValue => match item {
                Json::Object(members) => {
                    let mut pairs = Vec::with_capacity(members.len());
                    for (key, value) in members {
                        pairs.push(node(Json::Object(vec![
                            ("key".to_owned(), node(Json::String(key.clone()))),
                            ("value".to_owned(), node(value.value.clone())),
                            ("id".to_owned(), node(Json::Number("0".to_owned()))),
                        ])));
                    }
                    self.charge(pairs.len())?;
                    Json::Array(pairs)
                }
                _ => {
                    return Err(suppressible(
                        SqlState::SQL_JSON_OBJECT_NOT_FOUND,
                        "jsonpath item method .keyvalue() can only be applied to an object",
                    ));
                }
            },
            Method::String => match item {
                Json::String(_) => item.clone(),
                Json::Number(text) => Json::String(text.clone()),
                Json::Boolean(flag) => Json::String(flag.to_string()),
                _ => {
                    return Err(cannot_cast(
                        "jsonpath item method .string() can only be applied to a boolean, string, numeric, or datetime value".to_owned(),
                    ));
                }
            },
            Method::Number | Method::Integer | Method::BigInt => {
                let number = match item {
                    Json::Number(text) => Numeric::parse(text).ok(),
                    Json::String(text) => Numeric::parse(text.trim()).ok().filter(|number| !number.is_nan()),
                    _ => {
                        return Err(cannot_cast(format!(
                            "jsonpath item method .{name}() can only be applied to a string or numeric value"
                        )));
                    }
                };
                let text = match item {
                    Json::String(text) | Json::Number(text) => text.as_str(),
                    _ => "",
                };
                let number = number.ok_or_else(|| {
                    cannot_cast(format!(
                        "argument \"{text}\" of jsonpath item method .{name}() is invalid for type {}",
                        match method {
                            Method::Integer => "integer",
                            Method::BigInt => "bigint",
                            _ => "numeric",
                        }
                    ))
                })?;
                match method {
                    Method::Number => number_json(&number),
                    _ => {
                        let whole = number.to_i128_rounded();
                        let fits = match method {
                            Method::Integer => whole.and_then(|whole| i32::try_from(whole).ok()).map(i128::from),
                            _ => whole.and_then(|whole| i64::try_from(whole).ok()).map(i128::from),
                        };
                        let whole = fits.ok_or_else(|| {
                            cannot_cast(format!(
                                "argument \"{text}\" of jsonpath item method .{name}() is invalid for type {}",
                                if method == Method::Integer { "integer" } else { "bigint" }
                            ))
                        })?;
                        number_json(&Numeric::from_i128(whole))
                    }
                }
            }
            Method::Boolean => match item {
                Json::Boolean(_) => item.clone(),
                Json::Number(text) => {
                    let whole = Numeric::parse(text).ok().and_then(|number| number.integral_value());
                    match whole {
                        Some(whole) => Json::Boolean(whole != 0),
                        None => {
                            return Err(cannot_cast(format!(
                                "argument \"{text}\" of jsonpath item method .boolean() is invalid for type boolean"
                            )));
                        }
                    }
                }
                Json::String(text) => match text.trim().to_ascii_lowercase().as_str() {
                    "t" | "true" | "y" | "yes" | "on" | "1" => Json::Boolean(true),
                    "f" | "false" | "n" | "no" | "off" | "0" => Json::Boolean(false),
                    _ => {
                        return Err(cannot_cast(format!(
                            "argument \"{text}\" of jsonpath item method .boolean() is invalid for type boolean"
                        )));
                    }
                },
                _ => {
                    return Err(cannot_cast(
                        "jsonpath item method .boolean() can only be applied to a boolean, string, or numeric value".to_owned(),
                    ));
                }
            },
        })
    }

    fn operand(&self, expr: &Expr, current: Option<&Cow<'a, Json>>, last: Option<i64>) -> Run<Option<Items<'a>>> {
        match self.eval(expr, current, last) {
            Ok(items) => Ok(Some(self.unwrapped(items))),
            Err(Failure::Suppressible(_)) => Ok(None),
            Err(failure) => Err(failure),
        }
    }

    fn existential(&self, results: impl Iterator<Item = Truth>) -> Truth {
        let mut found = false;
        let mut unknown = false;
        for result in results {
            match result {
                Truth::Unknown if !self.lax() => return Truth::Unknown,
                Truth::Unknown => unknown = true,
                Truth::True if self.lax() => return Truth::True,
                Truth::True => found = true,
                Truth::False => {}
            }
        }
        if found {
            Truth::True
        } else if unknown {
            Truth::Unknown
        } else {
            Truth::False
        }
    }

    fn predicate(&self, predicate: &Predicate, current: Option<&Cow<'a, Json>>, last: Option<i64>) -> Run<Truth> {
        crate::stack_depth::check().map_err(fatal)?;
        Ok(match predicate {
            Predicate::And(left, right) => match self.predicate(left, current, last)? {
                Truth::False => Truth::False,
                first => match (first, self.predicate(right, current, last)?) {
                    (_, Truth::False) => Truth::False,
                    (Truth::True, Truth::True) => Truth::True,
                    _ => Truth::Unknown,
                },
            },
            Predicate::Or(left, right) => match self.predicate(left, current, last)? {
                Truth::True => Truth::True,
                first => match (first, self.predicate(right, current, last)?) {
                    (_, Truth::True) => Truth::True,
                    (Truth::False, Truth::False) => Truth::False,
                    _ => Truth::Unknown,
                },
            },
            Predicate::Not(inner) => match self.predicate(inner, current, last)? {
                Truth::True => Truth::False,
                Truth::False => Truth::True,
                Truth::Unknown => Truth::Unknown,
            },
            Predicate::IsUnknown(inner) => {
                Truth::of(self.predicate(inner, current, last)? == Truth::Unknown)
            }
            Predicate::Exists(expr) => match self.eval(expr, current, last) {
                Ok(items) => Truth::of(!items.is_empty()),
                Err(Failure::Suppressible(_)) => Truth::Unknown,
                Err(failure) => return Err(failure),
            },
            Predicate::Compare(comparison, left, right) => {
                let (Some(left), Some(right)) = (
                    self.operand(left, current, last)?,
                    self.operand(right, current, last)?,
                ) else {
                    return Ok(Truth::Unknown);
                };
                self.charge(left.len().saturating_mul(right.len()))?;
                self.existential(left.iter().flat_map(|first| {
                    right
                        .iter()
                        .map(move |second| compare_items(*comparison, first, second))
                }))
            }
            Predicate::LikeRegex {
                subject,
                pattern,
                options,
            } => {
                let Some(subjects) = self.operand(subject, current, last)? else {
                    return Ok(Truth::Unknown);
                };
                let regex = crate::regex::compiled_with(pattern, *options).map_err(fatal)?;
                let mut results = Vec::with_capacity(subjects.len());
                for item in &subjects {
                    results.push(match item.as_ref() {
                        Json::String(text) => match regex.is_match(text, &mut StepBudget::new(MATCH_STEPS)) {
                            Ok(matched) => Truth::of(matched),
                            Err(_) => Truth::Unknown,
                        },
                        _ => Truth::Unknown,
                    });
                }
                self.existential(results.into_iter())
            }
            Predicate::StartsWith(subject, prefix) => {
                let (Some(subjects), Some(prefixes)) = (
                    self.operand(subject, current, last)?,
                    self.operand(prefix, current, last)?,
                ) else {
                    return Ok(Truth::Unknown);
                };
                let [prefix] = prefixes.as_slice() else {
                    return Ok(Truth::Unknown);
                };
                let Json::String(prefix) = prefix.as_ref() else {
                    return Ok(Truth::Unknown);
                };
                self.existential(subjects.iter().map(|item| match item.as_ref() {
                    Json::String(text) => Truth::of(text.starts_with(prefix.as_str())),
                    _ => Truth::Unknown,
                }))
            }
        })
    }
}

fn compare_items(comparison: Comparison, left: &Json, right: &Json) -> Truth {
    let ordering = match (left, right) {
        (Json::Null, Json::Null) => Ordering::Equal,
        (Json::Null, _) | (_, Json::Null) => {
            return Truth::of(comparison == Comparison::NotEqual);
        }
        (Json::Boolean(first), Json::Boolean(second)) => first.cmp(second),
        (Json::Number(_), Json::Number(_)) => match (number_of(left), number_of(right)) {
            (Some(first), Some(second)) => first.compare(&second),
            _ => return Truth::Unknown,
        },
        (Json::String(first), Json::String(second)) => first.as_str().cmp(second.as_str()),
        _ => return Truth::Unknown,
    };
    Truth::of(comparison.holds(ordering))
}

struct Call {
    target: Json,
    path: JsonPath,
    vars: Option<Json>,
    silent: bool,
}

fn document_tree(value: &Value) -> Option<SqlResult<Json>> {
    let text = match value {
        Value::Jsonb(text) | Value::Json(text) | Value::Text(text) => text,
        _ => return None,
    };
    Some(json::parse(text, true).map(|node| node.value))
}

fn call_of(values: &[Value]) -> Option<SqlResult<Call>> {
    let (target, path, vars, silent) = match values {
        [target, path] => (target, path, None, None),
        [target, path, vars] => (target, path, Some(vars), None),
        [target, path, vars, silent] => (target, path, Some(vars), Some(silent)),
        _ => return None,
    };
    let silent = match silent {
        None => false,
        Some(Value::Boolean(flag)) => *flag,
        Some(_) => return None,
    };
    let path = path.character_text()?;
    let target = document_tree(target)?;
    let vars = match vars {
        Some(vars) => Some(document_tree(vars)?),
        None => None,
    };
    Some((|| {
        let target = target?;
        let path = parse(path)?;
        let vars = match vars {
            Some(vars) => {
                let vars = vars?;
                if !matches!(vars, Json::Object(_)) {
                    return Err(SqlError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "\"vars\" argument is not an object",
                    )
                    .with_detail(
                        "Jsonpath parameters should be encoded as key-value pairs of \"vars\" object.",
                    ));
                }
                Some(vars)
            }
            None => None,
        };
        Ok(Call {
            target,
            path,
            vars,
            silent,
        })
    })())
}

fn run(document: &Json, path: &JsonPath, vars: Option<&Json>) -> Run<Vec<Json>> {
    let evaluator = Evaluator {
        root: document,
        vars,
        mode: path.mode,
        produced: Cell::new(0),
    };
    let items = evaluator.eval(&path.expr, None, None)?;
    Ok(items.into_iter().map(Cow::into_owned).collect())
}

impl Call {
    fn items(&self) -> SqlResult<Option<Vec<Json>>> {
        match run(&self.target, &self.path, self.vars.as_ref()) {
            Ok(items) => Ok(Some(items)),
            Err(Failure::Suppressible(_)) if self.silent => Ok(None),
            Err(Failure::Suppressible(error) | Failure::Fatal(error)) => Err(error),
        }
    }

    fn matched(&self) -> SqlResult<Value> {
        let Some(items) = self.items()? else {
            return Ok(Value::Null);
        };
        match items.as_slice() {
            [Json::Boolean(flag)] => Ok(Value::Boolean(*flag)),
            [Json::Null] => Ok(Value::Null),
            _ if self.silent => Ok(Value::Null),
            _ => Err(SqlError::new(
                SqlState::SINGLETON_SQL_JSON_ITEM_REQUIRED,
                "single boolean result is expected",
            )),
        }
    }
}

fn jsonb(value: &Json) -> SqlResult<Value> {
    json::normalized_value(value).map(Value::Jsonb)
}

fn base_name(name: &str) -> &str {
    name.strip_suffix("_tz").unwrap_or(name)
}

/// Evaluates the scalar SQL/JSON path functions (`jsonb_path_exists`, `jsonb_path_match`,
/// `jsonb_path_query_array`, `jsonb_path_query_first` and their `_tz` forms).
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    if values.iter().any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    let call = match call_of(values)? {
        Ok(call) => call,
        Err(error) => return Some(Err(error)),
    };
    Some(match base_name(name) {
        "jsonb_path_exists" => call
            .items()
            .map(|items| items.map_or(Value::Null, |items| Value::Boolean(!items.is_empty()))),
        "jsonb_path_match" => call.matched(),
        "jsonb_path_query_array" => call.items().and_then(|items| {
            let items = items.unwrap_or_default();
            jsonb(&Json::Array(items.into_iter().map(node).collect()))
        }),
        _ => call.items().and_then(|items| {
            match items.and_then(|items| items.into_iter().next()) {
                Some(first) => jsonb(&first),
                None => Ok(Value::Null),
            }
        }),
    })
}

/// The result type of a scalar SQL/JSON path function.
pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    Some(match base_name(name) {
        "jsonb_path_exists" | "jsonb_path_match" => SqlType::Boolean,
        _ => SqlType::Jsonb,
    })
}

/// The rows of `jsonb_path_query`: each item the path selects, as `jsonb`.
pub(crate) fn query_rows(
    values: &[Value],
    sink: &mut dyn FnMut(Value) -> SqlResult<()>,
) -> SqlResult<()> {
    if values.iter().any(Value::is_null) {
        return Ok(());
    }
    let Some(call) = call_of(values) else {
        return Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            "function jsonb_path_query does not exist",
        ));
    };
    for item in call?.items()?.unwrap_or_default() {
        sink(jsonb(&item)?)?;
    }
    Ok(())
}

/// `jsonb @? jsonpath`: whether the path selects any item, `NULL` when it fails.
pub(crate) fn exists_operator(document: &Value, path: &Value) -> SqlResult<Value> {
    operator_call(document, path)?.map_or(Ok(Value::Null), |call| {
        call.items()
            .map(|items| items.map_or(Value::Null, |items| Value::Boolean(!items.is_empty())))
    })
}

/// `jsonb @@ jsonpath`: the path's single boolean result, `NULL` when it has none.
pub(crate) fn match_operator(document: &Value, path: &Value) -> SqlResult<Value> {
    operator_call(document, path)?.map_or(Ok(Value::Null), |call| call.matched())
}

fn operator_call(document: &Value, path: &Value) -> SqlResult<Option<Call>> {
    if document.is_null() || path.is_null() {
        return Ok(None);
    }
    let values = [document.clone(), path.clone(), Value::Jsonb("{}".to_owned()), Value::Boolean(true)];
    match call_of(&values) {
        Some(call) => call.map(Some),
        None => Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!(
                "operator does not exist: {} @? {}",
                document.type_name(),
                path.type_name()
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn select(document: &Json, path: &JsonPath, vars: Option<&Json>) -> SqlResult<Vec<Json>> {
        run(document, path, vars).map_err(|failure| match failure {
            Failure::Suppressible(error) | Failure::Fatal(error) => error,
        })
    }

    fn query(document: &str, path: &str) -> SqlResult<Vec<String>> {
        let target = json::parse(document, true)?.value;
        let path = parse(path)?;
        select(&target, &path, None)?
            .iter()
            .map(json::normalized_value)
            .collect()
    }

    fn ok(document: &str, path: &str) -> Vec<String> {
        query(document, path).unwrap_or_else(|error| panic!("{path}: {error}"))
    }

    fn state(document: &str, path: &str) -> String {
        match query(document, path) {
            Ok(items) => panic!("{path} gave {items:?}"),
            Err(error) => error.state().code().to_owned(),
        }
    }

    #[test]
    fn accessors_select_members_elements_and_ranges() {
        let doc = r#"{"a":{"b":[1,2,3,4]},"c":"x"}"#;
        assert_eq!(ok(doc, "$.a.b[0]"), ["1"]);
        assert_eq!(ok(doc, "$.a.b[last]"), ["4"]);
        assert_eq!(ok(doc, "$.a.b[1 to 2]"), ["2", "3"]);
        assert_eq!(ok(doc, "$.a.b[0, last - 1]"), ["1", "3"]);
        assert_eq!(ok(doc, "$.a.b[*]").len(), 4);
        assert_eq!(ok(doc, "$.*").len(), 2);
        assert_eq!(ok(doc, r#"$."c""#), ["\"x\""]);
        assert_eq!(ok(doc, "$.missing"), Vec::<String>::new());
        assert_eq!(ok(doc, "$.a.b[10]"), Vec::<String>::new());
        assert_eq!(ok(doc, "$.**{2}"), ["[1, 2, 3, 4]"]);
        assert_eq!(ok(doc, "$.a.b.size()"), ["4"]);
        assert_eq!(ok(doc, "$.c.type()"), ["\"string\""]);
    }

    #[test]
    fn lax_mode_unwraps_and_strict_mode_reports_structural_errors() {
        let doc = r#"{"a":[{"b":1},{"b":2}]}"#;
        assert_eq!(ok(doc, "lax $.a.b"), ["1", "2"]);
        assert_eq!(state(doc, "strict $.a.b"), "2203A");
        assert_eq!(state(doc, "strict $.x"), "2203A");
        assert_eq!(state(r#"{"a":1}"#, "strict $.a[0]"), "22039");
        assert_eq!(state(r#"[1]"#, "strict $[5]"), "22033");
        assert_eq!(ok(r#"{"a":1}"#, "lax $.a[0]"), ["1"]);
    }

    #[test]
    fn filters_compare_match_regex_and_prefixes() {
        let doc = r#"{"items":[{"n":1,"s":"apple"},{"n":5,"s":"Banana"},{"n":null,"s":"cherry"}]}"#;
        assert_eq!(ok(doc, "$.items[*] ? (@.n > 1).s"), ["\"Banana\""]);
        assert_eq!(
            ok(doc, "$.items[*] ? (@.n >= 1 && @.s starts with \"a\").n"),
            ["1"]
        );
        assert_eq!(
            ok(doc, "$.items[*] ? (@.s like_regex \"^b\" flag \"i\").n"),
            ["5"]
        );
        assert_eq!(ok(doc, "$.items[*] ? (@.n == null).s"), ["\"cherry\""]);
        assert_eq!(
            ok(doc, "$.items[*] ? (exists(@.n) && !(@.n < 5)).n"),
            ["5", "null"]
        );
        assert_eq!(ok(doc, "$.items[*] ? ((@.n > \"a\") is unknown).n").len(), 2);
        assert_eq!(ok(doc, "$.items[*].n > 4"), ["true"]);
        assert_eq!(ok(doc, "$.items[*].n > 40"), ["false"]);
    }

    #[test]
    fn arithmetic_and_item_methods_compute_numbers() {
        let doc = r#"{"a":7,"b":"2.5","c":-3.7,"o":{"x":1}}"#;
        assert_eq!(ok(doc, "$.a + 3 * 2"), ["13"]);
        assert_eq!(ok(doc, "$.a % 4"), ["3"]);
        assert_eq!(ok(doc, "-$.a"), ["-7"]);
        assert_eq!(ok(doc, "$.b.double() * 2"), ["5.0"]);
        assert_eq!(ok(doc, "$.c.floor()"), ["-4"]);
        assert_eq!(ok(doc, "$.c.ceiling()"), ["-3"]);
        assert_eq!(ok(doc, "$.c.abs()"), ["3.7"]);
        assert_eq!(ok(doc, "$.a.string()"), ["\"7\""]);
        assert_eq!(ok(doc, "$.b.number()"), ["2.5"]);
        assert_eq!(ok(doc, "$.c.integer()"), ["-4"]);
        assert_eq!(
            ok(doc, "$.o.keyvalue()"),
            [r#"{"id": 0, "key": "x", "value": 1}"#]
        );
        assert_eq!(state(doc, "$.a / 0"), "22012");
        assert_eq!(state(doc, "$.o + 1"), "22038");
        assert_eq!(state(doc, "$.o.floor()"), "22036");
    }

    #[test]
    fn variables_come_from_the_vars_object() {
        let target = json::parse(r#"[1,2,3]"#, true).unwrap().value;
        let vars = json::parse(r#"{"min":2}"#, true).unwrap().value;
        let path = parse("$[*] ? (@ >= $min)").unwrap();
        let found = select(&target, &path, Some(&vars)).unwrap();
        assert_eq!(found.len(), 2);
        let missing = parse("$[*] ? (@ >= $max)").unwrap();
        assert_eq!(
            select(&target, &missing, Some(&vars)).unwrap_err().state().code(),
            "42704"
        );
    }

    #[test]
    fn hostile_paths_return_errors() {
        for path in [
            "", "$.", "$[", "$ ? (@ > 1", "$.a ==", "@.a", "last", "$.\"abc", "$.a(", "$ &&",
            "$.a.nonsense()", "$[1 to]", "$.**{x}", "\"\\u{110000}\"", "\"\\ud800\"", "#",
        ] {
            assert!(parse(path).is_err(), "{path} parsed");
        }
        let deep = format!("{}$ {}", "(".repeat(5_000), ")".repeat(5_000));
        assert_eq!(parse(&deep).unwrap_err().state().code(), "54001");
        let long = format!("${}", ".a".repeat(5_000));
        assert!(parse(&long).is_err());
        let doc = format!("{}1{}", "[".repeat(200), "]".repeat(200));
        let target = json::parse(&doc, true).unwrap().value;
        let path = parse("$.**.**.**.**").unwrap();
        assert_eq!(select(&target, &path, None).unwrap_err().state().code(), "54000");
    }

    #[test]
    fn scalar_functions_follow_postgresql() {
        let doc = Value::Jsonb(r#"{"a":[1,2,3]}"#.to_owned());
        let path = |text: &str| Value::Text(text.to_owned());
        assert_eq!(
            function("jsonb_path_exists", &[doc.clone(), path("$.a[*] ? (@ > 2)")]).unwrap().unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            function("jsonb_path_match", &[doc.clone(), path("exists($.a ? (@ == 9))")]).unwrap().unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            function("jsonb_path_query_array", &[doc.clone(), path("$.a[*] ? (@ > 1)")]).unwrap().unwrap(),
            Value::Jsonb("[2, 3]".to_owned())
        );
        assert_eq!(
            function("jsonb_path_query_first", &[doc.clone(), path("$.a[*]")]).unwrap().unwrap(),
            Value::Jsonb("1".to_owned())
        );
        assert!(function("jsonb_path_match", &[doc.clone(), path("$.a")]).unwrap().is_err());
        assert_eq!(
            function(
                "jsonb_path_query_first",
                &[doc.clone(), path("strict $.x"), Value::Jsonb("{}".to_owned()), Value::Boolean(true)]
            )
            .unwrap()
            .unwrap(),
            Value::Null
        );
        assert!(function("jsonb_path_exists", &[doc.clone(), path("$"), Value::Jsonb("[]".to_owned())]).unwrap().is_err());
        assert_eq!(exists_operator(&doc, &path("$.a[*] ? (@ == 2)")).unwrap(), Value::Boolean(true));
        assert_eq!(match_operator(&doc, &path("$.a[0] == 1")).unwrap(), Value::Boolean(true));
        assert_eq!(match_operator(&doc, &path("$.a")).unwrap(), Value::Null);
    }
}
