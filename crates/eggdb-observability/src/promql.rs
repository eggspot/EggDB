mod evaluate;
mod functions;

use crate::store::Series;
use std::collections::BTreeMap;

pub(crate) use evaluate::{Data, Evaluator, Work, chosen};
#[cfg(test)]
pub(crate) use functions::expand;

pub(crate) const LOOKBACK_MS: i64 = 5 * 60 * 1000;
const MAX_DEPTH: usize = 64;
const MOST_OPERATORS: usize = 256;
const KEYWORDS: &[&str] = &[
    "bool",
    "on",
    "ignoring",
    "group_left",
    "group_right",
    "and",
    "or",
    "unless",
    "offset",
    "by",
    "without",
];

pub(crate) type Labels = Vec<(String, String)>;

/// The series each metric name read for a query holds.
pub(crate) type Fetched = BTreeMap<String, Vec<Series>>;

/// Why a query failed, in the error types of Prometheus's HTTP API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    BadData(String),
    Execution(String),
    Unavailable(String),
}

impl Failure {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::BadData(message) | Self::Execution(message) | Self::Unavailable(message) => {
                message
            }
        }
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Execution(message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MatchOp {
    Equal,
    NotEqual,
    Regex,
    NotRegex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Matcher {
    pub(crate) name: String,
    pub(crate) op: MatchOp,
    pub(crate) value: String,
}

impl Matcher {
    pub(crate) fn matches(&self, labels: &[(String, String)]) -> bool {
        self.matches_value(label_value(labels, &self.name))
    }

    fn matches_value(&self, value: &str) -> bool {
        match self.op {
            MatchOp::Equal => value == self.value,
            MatchOp::NotEqual => value != self.value,
            MatchOp::Regex => full_match(value, &self.value),
            MatchOp::NotRegex => !full_match(value, &self.value),
        }
    }

    fn matches_empty(&self) -> bool {
        self.matches_value("")
    }
}

pub(crate) fn label_value<'l>(labels: &'l [(String, String)], name: &str) -> &'l str {
    labels
        .iter()
        .find(|(label, _)| label == name)
        .map_or("", |(_, value)| value.as_str())
}

pub(crate) fn anchored(pattern: &str) -> String {
    format!("^(?:{pattern})$")
}

fn full_match(text: &str, pattern: &str) -> bool {
    eggdb_sql::regex_matches_with_flags(text, &anchored(pattern), "s").unwrap_or(false)
}

pub(crate) fn is_label_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selector {
    pub(crate) name: Option<String>,
    pub(crate) matchers: Vec<Matcher>,
    pub(crate) range_ms: Option<i64>,
    pub(crate) offset_ms: i64,
}

impl Selector {
    /// Whether a series of metric `metric`, whose table has the tag columns `tags`, can pass
    /// every matcher.
    pub(crate) fn could_match(&self, metric: &str, tags: &[String]) -> bool {
        if self.name.as_deref().is_some_and(|name| name != metric) {
            return false;
        }
        self.matchers.iter().all(|matcher| {
            if matcher.name == "__name__" {
                matcher.matches_value(metric)
            } else {
                tags.contains(&matcher.name) || matcher.matches_empty()
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Grouping {
    By(Vec<String>),
    Without(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Aggregation {
    pub(crate) op: String,
    pub(crate) grouping: Option<Grouping>,
    pub(crate) parameter: Option<Expr>,
    pub(crate) inner: Expr,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum Cardinality {
    #[default]
    OneToOne,
    ManyToOne(Vec<String>),
    OneToMany(Vec<String>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct VectorMatching {
    pub(crate) cardinality: Cardinality,
    pub(crate) on: bool,
    pub(crate) labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Binary {
    pub(crate) op: &'static str,
    pub(crate) left: Expr,
    pub(crate) right: Expr,
    pub(crate) returns_bool: bool,
    pub(crate) matching: Option<VectorMatching>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Subquery {
    pub(crate) inner: Expr,
    pub(crate) range_ms: i64,
    pub(crate) step_ms: Option<i64>,
    pub(crate) offset_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Number(f64),
    Text(String),
    Selector(Selector),
    Call(String, Vec<Expr>),
    Aggregate(Box<Aggregation>),
    Binary(Box<Binary>),
    Subquery(Box<Subquery>),
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Name(String),
    Number(f64),
    Text(String),
    Duration(i64),
    Range(i64),
    Subquery(i64, Option<i64>),
    Symbol(&'static str),
}

const SYMBOLS: &[&str] = &[
    "==", "!=", "=~", "!~", ">=", "<=", "{", "}", "(", ")", ",", "=", "+", "-", "*", "/", "%", "^",
    ">", "<",
];

/// A duration such as `5m`, `1h30m` or `250ms`, in milliseconds.
pub(crate) fn duration_ms(text: &str) -> Option<i64> {
    let mut total = 0i64;
    let mut rest = text;
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let amount: i64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let (unit, size) = [
            ("ms", 1),
            ("s", 1000),
            ("m", 60_000),
            ("h", 3_600_000),
            ("d", 86_400_000),
            ("w", 604_800_000),
            ("y", 31_536_000_000),
        ]
        .into_iter()
        .find(|(unit, _)| rest.starts_with(unit) && !(unit == &"m" && rest.starts_with("ms")))?;
        total = total.checked_add(amount.checked_mul(size)?)?;
        rest = &rest[unit.len()..];
    }
    Some(total)
}

fn positive_duration(text: &str) -> Result<i64, String> {
    let text = text.trim();
    duration_ms(text)
        .filter(|ms| *ms > 0)
        .ok_or_else(|| format!("bad duration {text}"))
}

fn bracket(inner: &str) -> Result<Token, String> {
    match inner.split_once(':') {
        None => Ok(Token::Range(positive_duration(inner)?)),
        Some((range, step)) => {
            let step = step.trim();
            let step = if step.is_empty() {
                None
            } else {
                Some(positive_duration(step)?)
            };
            Ok(Token::Subquery(positive_duration(range)?, step))
        }
    }
}

fn tokenize(text: &str) -> Result<Vec<Token>, String> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        if byte.is_ascii_whitespace() {
            at += 1;
        } else if byte == b'#' {
            at = text[at..]
                .find('\n')
                .map_or(bytes.len(), |offset| at + offset);
        } else if byte == b'[' {
            let end = text[at..]
                .find(']')
                .map(|offset| at + offset)
                .ok_or("a range is not closed")?;
            tokens.push(bracket(&text[at + 1..end])?);
            at = end + 1;
        } else if byte.is_ascii_digit()
            || (byte == b'.' && bytes.get(at + 1).is_some_and(u8::is_ascii_digit))
        {
            let end = at
                + bytes[at..]
                    .iter()
                    .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'.')
                    .count();
            let word = &text[at..end];
            let value = match word.parse::<f64>() {
                Ok(number) => Token::Number(number),
                Err(_) => {
                    Token::Duration(duration_ms(word).ok_or_else(|| format!("bad number {word}"))?)
                }
            };
            tokens.push(value);
            at = end;
        } else if byte.is_ascii_alphabetic() || byte == b'_' || byte == b':' {
            let end = at
                + bytes[at..]
                    .iter()
                    .take_while(|byte| {
                        byte.is_ascii_alphanumeric() || **byte == b'_' || **byte == b':'
                    })
                    .count();
            tokens.push(Token::Name(text[at..end].to_owned()));
            at = end;
        } else if byte == b'"' || byte == b'\'' || byte == b'`' {
            let mut value = String::new();
            let mut index = at + 1;
            loop {
                let character = text[index..]
                    .chars()
                    .next()
                    .ok_or("a string is not closed")?;
                index += character.len_utf8();
                if character == char::from(byte) {
                    break;
                }
                if character == '\\' && byte != b'`' {
                    let escaped = text[index..]
                        .chars()
                        .next()
                        .ok_or("a string is not closed")?;
                    index += escaped.len_utf8();
                    value.push(match escaped {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    });
                } else {
                    value.push(character);
                }
            }
            tokens.push(Token::Text(value));
            at = index;
        } else if let Some(symbol) = SYMBOLS
            .iter()
            .find(|symbol| text[at..].starts_with(**symbol))
        {
            tokens.push(Token::Symbol(symbol));
            at += symbol.len();
        } else {
            return Err(format!(
                "unexpected character {:?}",
                text[at..].chars().next().unwrap_or(' ')
            ));
        }
    }
    Ok(tokens)
}

const AGGREGATIONS: &[&str] = &[
    "sum",
    "avg",
    "min",
    "max",
    "count",
    "group",
    "stddev",
    "stdvar",
    "topk",
    "bottomk",
    "quantile",
    "count_values",
];

fn takes_parameter(aggregation: &str) -> bool {
    matches!(
        aggregation,
        "topk" | "bottomk" | "quantile" | "count_values"
    )
}

const POWER: u8 = 6;

fn precedence(operator: &str) -> Option<u8> {
    match operator {
        "or" => Some(1),
        "and" | "unless" => Some(2),
        "==" | "!=" | ">" | "<" | ">=" | "<=" => Some(3),
        "+" | "-" => Some(4),
        "*" | "/" | "%" => Some(5),
        "^" => Some(POWER),
        _ => None,
    }
}

pub(crate) fn is_comparison(operator: &str) -> bool {
    matches!(operator, "==" | "!=" | ">" | "<" | ">=" | "<=")
}

pub(crate) fn is_set_operator(operator: &str) -> bool {
    matches!(operator, "and" | "or" | "unless")
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    depth: usize,
    operators: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at).cloned();
        self.at += 1;
        token
    }

    fn eat(&mut self, symbol: &str) -> bool {
        let found = matches!(self.peek(), Some(Token::Symbol(found)) if *found == symbol);
        if found {
            self.at += 1;
        }
        found
    }

    fn eat_word(&mut self, word: &str) -> bool {
        let found = matches!(self.peek(), Some(Token::Name(found)) if found == word);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect(&mut self, symbol: &str) -> Result<(), String> {
        if self.eat(symbol) {
            Ok(())
        } else {
            Err(format!("expected {symbol}"))
        }
    }

    fn deeper(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("the query is nested too deeply".to_owned());
        }
        Ok(())
    }

    fn names(&mut self) -> Result<Vec<String>, String> {
        self.expect("(")?;
        let mut names = Vec::new();
        while !self.eat(")") {
            match self.next() {
                Some(Token::Name(name)) => names.push(name),
                _ => return Err("expected a label name".to_owned()),
            }
            if !self.eat(",") {
                self.expect(")")?;
                break;
            }
        }
        Ok(names)
    }

    fn optional_names(&mut self) -> Result<Vec<String>, String> {
        if matches!(self.peek(), Some(Token::Symbol("("))) {
            self.names()
        } else {
            Ok(Vec::new())
        }
    }

    fn grouping(&mut self) -> Result<Option<Grouping>, String> {
        if self.eat_word("by") {
            Ok(Some(Grouping::By(self.names()?)))
        } else if self.eat_word("without") {
            Ok(Some(Grouping::Without(self.names()?)))
        } else {
            Ok(None)
        }
    }

    fn matchers(&mut self) -> Result<Vec<Matcher>, String> {
        let mut matchers = Vec::new();
        while !self.eat("}") {
            let Some(Token::Name(name)) = self.next() else {
                return Err("expected a label name in a selector".to_owned());
            };
            let op = match self.next() {
                Some(Token::Symbol("=")) => MatchOp::Equal,
                Some(Token::Symbol("!=")) => MatchOp::NotEqual,
                Some(Token::Symbol("=~")) => MatchOp::Regex,
                Some(Token::Symbol("!~")) => MatchOp::NotRegex,
                _ => return Err("expected a label matcher".to_owned()),
            };
            let Some(Token::Text(value)) = self.next() else {
                return Err("expected a quoted label value".to_owned());
            };
            if matches!(op, MatchOp::Regex | MatchOp::NotRegex) {
                eggdb_sql::regex_matches_with_flags("", &anchored(&value), "s")
                    .map_err(|error| format!("error parsing regexp {value:?}: {error}"))?;
            }
            matchers.push(Matcher { name, op, value });
            if !self.eat(",") {
                self.expect("}")?;
                break;
            }
        }
        Ok(matchers)
    }

    fn offset(&mut self) -> Result<i64, String> {
        if !self.eat_word("offset") {
            return Ok(0);
        }
        match self.next() {
            Some(Token::Duration(ms)) => Ok(ms),
            Some(Token::Number(number)) if number == 0.0 => Ok(0),
            _ => Err("expected a duration after offset".to_owned()),
        }
    }

    fn selector(&mut self, name: Option<String>) -> Result<Expr, String> {
        let mut matchers = if self.eat("{") {
            self.matchers()?
        } else {
            Vec::new()
        };
        let name = match name {
            Some(name) => {
                if matchers.iter().any(|matcher| matcher.name == "__name__") {
                    return Err(format!("metric name must not be set twice: {name:?}"));
                }
                Some(name)
            }
            None => {
                if matchers.iter().all(Matcher::matches_empty) {
                    return Err(
                        "vector selector must contain at least one non-empty matcher".to_owned(),
                    );
                }
                matchers
                    .iter()
                    .position(|matcher| matcher.name == "__name__" && matcher.op == MatchOp::Equal)
                    .map(|position| matchers.remove(position).value)
            }
        };
        let range_ms = match self.peek() {
            Some(Token::Range(ms)) => {
                let ms = *ms;
                self.at += 1;
                Some(ms)
            }
            _ => None,
        };
        let offset_ms = self.offset()?;
        Ok(Expr::Selector(Selector {
            name,
            matchers,
            range_ms,
            offset_ms,
        }))
    }

    fn starts_aggregation(&self) -> bool {
        match self.peek() {
            Some(Token::Symbol("(")) => true,
            Some(Token::Name(word)) => word == "by" || word == "without",
            _ => false,
        }
    }

    fn aggregation(&mut self, op: String) -> Result<Expr, String> {
        let mut grouping = self.grouping()?;
        self.expect("(")?;
        let first = self.expression(0)?;
        let (parameter, inner) = if takes_parameter(&op) {
            if !self.eat(",") {
                return Err(format!(
                    "wrong number of arguments for aggregate expression provided, expected 2, got 1: {op}"
                ));
            }
            (Some(first), self.expression(0)?)
        } else {
            if matches!(self.peek(), Some(Token::Symbol(","))) {
                return Err(format!(
                    "wrong number of arguments for aggregate expression provided, expected 1, got 2: {op}"
                ));
            }
            (None, first)
        };
        self.expect(")")?;
        if grouping.is_none() {
            grouping = self.grouping()?;
        }
        match (&parameter, op.as_str()) {
            (Some(Expr::Text(_)), "count_values") | (None, _) => {}
            (Some(Expr::Text(_)), _) => {
                return Err(format!(
                    "expected type scalar in aggregation parameter of {op}, got string"
                ));
            }
            (Some(_), "count_values") => {
                return Err(
                    "expected type string in aggregation parameter of count_values".to_owned(),
                );
            }
            (Some(_), _) => {}
        }
        Ok(Expr::Aggregate(Box::new(Aggregation {
            op,
            grouping,
            parameter,
            inner,
        })))
    }

    fn call(&mut self, name: String) -> Result<Expr, String> {
        self.expect("(")?;
        let mut arguments = Vec::new();
        while !self.eat(")") {
            arguments.push(self.expression(0)?);
            if !self.eat(",") {
                self.expect(")")?;
                break;
            }
        }
        Ok(Expr::Call(name, arguments))
    }

    fn operand(&mut self) -> Result<Expr, String> {
        self.deeper()?;
        let expr = match self.next() {
            Some(Token::Number(number)) => Expr::Number(number),
            Some(Token::Text(text)) => Expr::Text(text),
            Some(Token::Symbol("-")) => Expr::Binary(Box::new(Binary {
                op: "*",
                left: Expr::Number(-1.0),
                right: self.expression(POWER - 1)?,
                returns_bool: false,
                matching: None,
            })),
            Some(Token::Symbol("+")) => self.expression(POWER - 1)?,
            Some(Token::Symbol("(")) => {
                let inner = self.expression(0)?;
                self.expect(")")?;
                inner
            }
            Some(Token::Symbol("{")) => {
                self.at -= 1;
                self.selector(None)?
            }
            Some(Token::Name(name)) if KEYWORDS.contains(&name.as_str()) => {
                return Err(format!("unexpected keyword {name:?}"));
            }
            Some(Token::Name(name))
                if AGGREGATIONS.contains(&name.as_str()) && self.starts_aggregation() =>
            {
                self.aggregation(name)?
            }
            Some(Token::Name(name)) if matches!(self.peek(), Some(Token::Symbol("("))) => {
                self.call(name)?
            }
            Some(Token::Name(name)) => self.selector(Some(name))?,
            _ => return Err("unexpected end of query or token".to_owned()),
        };
        let expr = self.subqueries(expr)?;
        self.depth -= 1;
        Ok(expr)
    }

    fn subqueries(&mut self, mut expr: Expr) -> Result<Expr, String> {
        while let Some(Token::Subquery(range_ms, step_ms)) = self.peek().cloned() {
            self.at += 1;
            let ranged = match &expr {
                Expr::Selector(selector) => selector.range_ms.is_some(),
                Expr::Subquery(_) => true,
                _ => false,
            };
            if ranged {
                return Err(
                    "subquery is only allowed on instant vector, got range vector".to_owned(),
                );
            }
            let offset_ms = self.offset()?;
            expr = Expr::Subquery(Box::new(Subquery {
                inner: expr,
                range_ms,
                step_ms,
                offset_ms,
            }));
        }
        Ok(expr)
    }

    fn peek_operator(&self) -> Option<&'static str> {
        match self.peek()? {
            Token::Symbol(symbol) => precedence(symbol).map(|_| *symbol),
            Token::Name(word) => ["and", "or", "unless"]
                .into_iter()
                .find(|operator| *operator == word.as_str()),
            _ => None,
        }
    }

    fn vector_matching(&mut self, operator: &str) -> Result<Option<VectorMatching>, String> {
        let on = if self.eat_word("on") {
            true
        } else if self.eat_word("ignoring") {
            false
        } else {
            return Ok(None);
        };
        let labels = self.names()?;
        let cardinality = if self.eat_word("group_left") {
            Cardinality::ManyToOne(self.optional_names()?)
        } else if self.eat_word("group_right") {
            Cardinality::OneToMany(self.optional_names()?)
        } else {
            Cardinality::OneToOne
        };
        let included = match &cardinality {
            Cardinality::OneToOne => &[][..],
            Cardinality::ManyToOne(names) | Cardinality::OneToMany(names) => {
                if is_set_operator(operator) {
                    return Err(format!("no grouping allowed for \"{operator}\" operation"));
                }
                names.as_slice()
            }
        };
        if on && let Some(label) = included.iter().find(|label| labels.contains(label)) {
            return Err(format!(
                "label \"{label}\" must not occur in ON and GROUP clause at once"
            ));
        }
        Ok(Some(VectorMatching {
            cardinality,
            on,
            labels,
        }))
    }

    fn expression(&mut self, minimum: u8) -> Result<Expr, String> {
        let mut left = self.operand()?;
        while let Some(operator) = self.peek_operator() {
            let Some(level) = precedence(operator).filter(|level| *level > minimum) else {
                break;
            };
            self.at += 1;
            self.operators += 1;
            if self.operators > MOST_OPERATORS {
                return Err("the query has more than 256 operators".to_owned());
            }
            let returns_bool = self.eat_word("bool");
            if returns_bool && !is_comparison(operator) {
                return Err("bool modifier can only be used on comparison operators".to_owned());
            }
            let matching = self.vector_matching(operator)?;
            self.deeper()?;
            let right = self.expression(if operator == "^" { level - 1 } else { level })?;
            self.depth -= 1;
            left = Expr::Binary(Box::new(Binary {
                op: operator,
                left,
                right,
                returns_bool,
                matching,
            }));
        }
        Ok(left)
    }
}

/// Parses a PromQL expression.
pub(crate) fn parse(text: &str) -> Result<Expr, String> {
    let mut parser = Parser {
        tokens: tokenize(text)?,
        at: 0,
        depth: 0,
        operators: 0,
    };
    let expr = parser.expression(0)?;
    if parser.at < parser.tokens.len() {
        return Err("unexpected text after the expression".to_owned());
    }
    Ok(expr)
}

/// Every selector in `expr`, with how far before an evaluation time it reads, counting
/// `earlier_ms` more for the subqueries around it.
pub(crate) fn selectors<'e>(expr: &'e Expr, earlier_ms: i64, out: &mut Vec<(&'e Selector, i64)>) {
    match expr {
        Expr::Number(_) | Expr::Text(_) => {}
        Expr::Selector(selector) => out.push((
            selector,
            earlier_ms
                .saturating_add(selector.range_ms.unwrap_or(LOOKBACK_MS))
                .saturating_add(selector.offset_ms),
        )),
        Expr::Call(_, arguments) => arguments
            .iter()
            .for_each(|argument| selectors(argument, earlier_ms, out)),
        Expr::Aggregate(aggregation) => {
            if let Some(parameter) = &aggregation.parameter {
                selectors(parameter, earlier_ms, out);
            }
            selectors(&aggregation.inner, earlier_ms, out);
        }
        Expr::Binary(binary) => {
            selectors(&binary.left, earlier_ms, out);
            selectors(&binary.right, earlier_ms, out);
        }
        Expr::Subquery(subquery) => selectors(
            &subquery.inner,
            earlier_ms
                .saturating_add(subquery.range_ms)
                .saturating_add(subquery.offset_ms),
            out,
        ),
    }
}
