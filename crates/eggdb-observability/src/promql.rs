use crate::store::Series;
use std::collections::BTreeMap;

pub(crate) const LOOKBACK_MS: i64 = 5 * 60 * 1000;
const MAX_DEPTH: usize = 64;

pub(crate) type Labels = Vec<(String, String)>;

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
    pub(crate) fn matches(&self, labels: &Labels) -> bool {
        let value = labels.iter().find(|(name, _)| *name == self.name).map_or("", |(_, value)| value.as_str());
        match self.op {
            MatchOp::Equal => value == self.value,
            MatchOp::NotEqual => value != self.value,
            MatchOp::Regex => full_match(value, &self.value),
            MatchOp::NotRegex => !full_match(value, &self.value),
        }
    }
}

fn full_match(text: &str, pattern: &str) -> bool {
    eggdb_sql::regex_matches_with_flags(text, &format!("^({pattern})$"), "").unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selector {
    pub(crate) name: String,
    pub(crate) matchers: Vec<Matcher>,
    pub(crate) range_ms: Option<i64>,
    pub(crate) offset_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Grouping {
    By(Vec<String>),
    Without(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Number(f64),
    Selector(Selector),
    Call(String, Vec<Expr>),
    Aggregate(String, Option<Grouping>, Box<Expr>),
    Binary(String, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Name(String),
    Number(f64),
    Text(String),
    Duration(i64),
    Symbol(&'static str),
}

const SYMBOLS: [&str; 19] = ["==", "!=", "=~", "!~", ">=", "<=", "{", "}", "(", ")", "[", "]", ",", "=", "+", "-", "*", "/", ">"];

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
        let (unit, size) = [("ms", 1), ("s", 1000), ("m", 60_000), ("h", 3_600_000), ("d", 86_400_000), ("w", 604_800_000), ("y", 31_536_000_000)]
            .into_iter()
            .find(|(unit, _)| rest.starts_with(unit) && !(unit == &"m" && rest.starts_with("ms")))?;
        total = total.checked_add(amount.checked_mul(size)?)?;
        rest = &rest[unit.len()..];
    }
    Some(total)
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
            at = text[at..].find('\n').map_or(bytes.len(), |offset| at + offset);
        } else if byte == b'[' {
            let end = text[at..].find(']').map(|offset| at + offset).ok_or("a range is not closed")?;
            let inner = text[at + 1..end].trim();
            tokens.push(Token::Duration(duration_ms(inner).filter(|ms| *ms > 0).ok_or_else(|| format!("bad duration {inner}"))?));
            at = end + 1;
        } else if byte.is_ascii_digit() || (byte == b'.' && bytes.get(at + 1).is_some_and(u8::is_ascii_digit)) {
            let end = at + bytes[at..].iter().take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'.').count();
            let word = &text[at..end];
            let value = match word.parse::<f64>() {
                Ok(number) => Token::Number(number),
                Err(_) => Token::Duration(duration_ms(word).ok_or_else(|| format!("bad number {word}"))?),
            };
            tokens.push(value);
            at = end;
        } else if byte.is_ascii_alphabetic() || byte == b'_' || byte == b':' {
            let end = at + bytes[at..].iter().take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_' || **byte == b':').count();
            tokens.push(Token::Name(text[at..end].to_owned()));
            at = end;
        } else if byte == b'"' || byte == b'\'' || byte == b'`' {
            let mut value = String::new();
            let mut index = at + 1;
            loop {
                let character = text[index..].chars().next().ok_or("a string is not closed")?;
                index += character.len_utf8();
                if character == char::from(byte) {
                    break;
                }
                if character == '\\' && byte != b'`' {
                    let escaped = text[index..].chars().next().ok_or("a string is not closed")?;
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
        } else if let Some(symbol) = SYMBOLS.iter().find(|symbol| text[at..].starts_with(**symbol)) {
            tokens.push(Token::Symbol(symbol));
            at += symbol.len();
        } else if byte == b'<' {
            tokens.push(Token::Symbol("<"));
            at += 1;
        } else {
            return Err(format!("unexpected character {:?}", text[at..].chars().next().unwrap_or(' ')));
        }
    }
    Ok(tokens)
}

const AGGREGATIONS: [&str; 5] = ["sum", "avg", "min", "max", "count"];

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    depth: usize,
}

fn precedence(symbol: &str) -> Option<u8> {
    match symbol {
        "==" | "!=" | ">" | "<" | ">=" | "<=" => Some(1),
        "+" | "-" => Some(2),
        "*" | "/" => Some(3),
        _ => None,
    }
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

    fn expect(&mut self, symbol: &str) -> Result<(), String> {
        if self.eat(symbol) { Ok(()) } else { Err(format!("expected {symbol}")) }
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

    fn grouping(&mut self) -> Result<Option<Grouping>, String> {
        match self.peek() {
            Some(Token::Name(word)) if word == "by" => {
                self.at += 1;
                Ok(Some(Grouping::By(self.names()?)))
            }
            Some(Token::Name(word)) if word == "without" => {
                self.at += 1;
                Ok(Some(Grouping::Without(self.names()?)))
            }
            _ => Ok(None),
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
            matchers.push(Matcher { name, op, value });
            if !self.eat(",") {
                self.expect("}")?;
                break;
            }
        }
        Ok(matchers)
    }

    fn selector(&mut self, name: String) -> Result<Expr, String> {
        let mut matchers = if self.eat("{") { self.matchers()? } else { Vec::new() };
        let name = if name.is_empty() {
            let position = matchers.iter().position(|matcher| matcher.name == "__name__" && matcher.op == MatchOp::Equal).ok_or("a selector needs a metric name")?;
            matchers.remove(position).value
        } else {
            name
        };
        let range_ms = match self.peek() {
            Some(Token::Duration(ms)) => {
                let ms = *ms;
                self.at += 1;
                Some(ms)
            }
            _ => None,
        };
        let mut offset_ms = 0;
        if matches!(self.peek(), Some(Token::Name(word)) if word == "offset") {
            self.at += 1;
            offset_ms = match self.next() {
                Some(Token::Duration(ms)) => ms,
                Some(Token::Number(seconds)) if seconds == 0.0 => 0,
                _ => return Err("expected a duration after offset".to_owned()),
            };
        }
        Ok(Expr::Selector(Selector { name, matchers, range_ms, offset_ms }))
    }

    fn primary(&mut self) -> Result<Expr, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("the query is nested too deeply".to_owned());
        }
        let expr = match self.next() {
            Some(Token::Number(number)) => Expr::Number(number),
            Some(Token::Symbol("-")) => Expr::Binary("*".to_owned(), Box::new(Expr::Number(-1.0)), Box::new(self.primary()?)),
            Some(Token::Symbol("(")) => {
                let inner = self.expression(0)?;
                self.expect(")")?;
                inner
            }
            Some(Token::Symbol("{")) => {
                self.at -= 1;
                self.selector(String::new())?
            }
            Some(Token::Name(name)) if AGGREGATIONS.contains(&name.as_str()) => {
                let mut grouping = self.grouping()?;
                self.expect("(")?;
                let inner = self.expression(0)?;
                self.expect(")")?;
                if grouping.is_none() {
                    grouping = self.grouping()?;
                }
                Expr::Aggregate(name, grouping, Box::new(inner))
            }
            Some(Token::Name(name)) if matches!(self.peek(), Some(Token::Symbol("("))) => {
                self.at += 1;
                let mut arguments = Vec::new();
                while !self.eat(")") {
                    arguments.push(self.expression(0)?);
                    if !self.eat(",") {
                        self.expect(")")?;
                        break;
                    }
                }
                Expr::Call(name, arguments)
            }
            Some(Token::Name(name)) => self.selector(name)?,
            _ => return Err("unexpected end of query or token".to_owned()),
        };
        self.depth -= 1;
        Ok(expr)
    }

    fn expression(&mut self, minimum: u8) -> Result<Expr, String> {
        let mut left = self.primary()?;
        loop {
            let Some(Token::Symbol(symbol)) = self.peek() else {
                break;
            };
            let symbol = *symbol;
            let Some(level) = precedence(symbol).filter(|level| *level > minimum) else {
                break;
            };
            self.at += 1;
            let right = self.expression(level)?;
            left = Expr::Binary(symbol.to_owned(), Box::new(left), Box::new(right));
        }
        Ok(left)
    }
}

/// Parses a PromQL expression.
pub(crate) fn parse(text: &str) -> Result<Expr, String> {
    let mut parser = Parser { tokens: tokenize(text)?, at: 0, depth: 0 };
    let expr = parser.expression(0)?;
    if parser.at < parser.tokens.len() {
        return Err("unexpected text after the expression".to_owned());
    }
    Ok(expr)
}

/// Every selector in `expr`, with how far before an evaluation time it reads.
pub(crate) fn selectors(expr: &Expr, out: &mut Vec<(String, i64)>) {
    match expr {
        Expr::Number(_) => {}
        Expr::Selector(selector) => out.push((selector.name.clone(), selector.range_ms.unwrap_or(LOOKBACK_MS) + selector.offset_ms)),
        Expr::Call(_, arguments) => arguments.iter().for_each(|argument| selectors(argument, out)),
        Expr::Aggregate(_, _, inner) => selectors(inner, out),
        Expr::Binary(_, left, right) => {
            selectors(left, out);
            selectors(right, out);
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Data {
    Scalar(f64),
    Vector(Vec<(Labels, f64)>),
    Matrix(Vec<(Labels, Vec<(i64, f64)>)>),
}

fn without_name(labels: &Labels) -> Labels {
    labels.iter().filter(|(name, _)| name != "__name__").cloned().collect()
}

fn window(samples: &[(i64, f64)], start: i64, end: i64) -> &[(i64, f64)] {
    let from = samples.partition_point(|(time, _)| *time <= start);
    let to = samples.partition_point(|(time, _)| *time <= end);
    &samples[from..to.max(from)]
}

fn extrapolated(samples: &[(i64, f64)], start: i64, end: i64, counter: bool, per_second: bool) -> Option<f64> {
    if samples.len() < 2 {
        return None;
    }
    let (first, last) = (samples[0], samples[samples.len() - 1]);
    let mut result = last.1 - first.1;
    if counter {
        for pair in samples.windows(2) {
            if pair[1].1 < pair[0].1 {
                result += pair[0].1;
            }
        }
    }
    let sampled = (last.0 - first.0) as f64 / 1000.0;
    let average = sampled / (samples.len() - 1) as f64;
    let mut to_start = (first.0 - start) as f64 / 1000.0;
    let to_end = (end - last.0) as f64 / 1000.0;
    if counter && result > 0.0 && first.1 >= 0.0 {
        let to_zero = sampled * (first.1 / result);
        if to_zero < to_start {
            to_start = to_zero;
        }
    }
    let threshold = average * 1.1;
    let mut interval = sampled;
    interval += if to_start < threshold { to_start } else { average / 2.0 };
    interval += if to_end < threshold { to_end } else { average / 2.0 };
    let mut factor = interval / sampled;
    if per_second {
        factor /= (end - start) as f64 / 1000.0;
    }
    Some(result * factor)
}

fn range_function(name: &str, samples: &[(i64, f64)], start: i64, end: i64) -> Result<Option<f64>, String> {
    let values = || samples.iter().map(|(_, value)| *value);
    Ok(match name {
        "rate" => extrapolated(samples, start, end, true, true),
        "increase" => extrapolated(samples, start, end, true, false),
        "delta" => extrapolated(samples, start, end, false, false),
        "irate" => match samples {
            [.., before, last] if last.0 > before.0 => {
                let change = if last.1 < before.1 { last.1 } else { last.1 - before.1 };
                Some(change / ((last.0 - before.0) as f64 / 1000.0))
            }
            _ => None,
        },
        "avg_over_time" => (!samples.is_empty()).then(|| values().sum::<f64>() / samples.len() as f64),
        "sum_over_time" => (!samples.is_empty()).then(|| values().sum()),
        "min_over_time" => values().reduce(f64::min),
        "max_over_time" => values().reduce(f64::max),
        "count_over_time" => (!samples.is_empty()).then_some(samples.len() as f64),
        "last_over_time" => samples.last().map(|(_, value)| *value),
        other => return Err(format!("unknown function {other}")),
    })
}

fn scalar_function(name: &str, value: f64) -> Option<f64> {
    Some(match name {
        "abs" => value.abs(),
        "ceil" => value.ceil(),
        "floor" => value.floor(),
        "round" => value.round(),
        "sqrt" => value.sqrt(),
        "exp" => value.exp(),
        "ln" => value.ln(),
        "log2" => value.log2(),
        "log10" => value.log10(),
        _ => return None,
    })
}

fn apply(op: &str, left: f64, right: f64) -> Option<f64> {
    match op {
        "+" => Some(left + right),
        "-" => Some(left - right),
        "*" => Some(left * right),
        "/" => Some(left / right),
        "==" => (left == right).then_some(left),
        "!=" => (left != right).then_some(left),
        ">" => (left > right).then_some(left),
        "<" => (left < right).then_some(left),
        ">=" => (left >= right).then_some(left),
        "<=" => (left <= right).then_some(left),
        _ => None,
    }
}

fn is_comparison(op: &str) -> bool {
    matches!(op, "==" | "!=" | ">" | "<" | ">=" | "<=")
}

/// Evaluates `expr` at `time_ms` over the series fetched for its selectors.
pub(crate) fn evaluate(expr: &Expr, time_ms: i64, data: &BTreeMap<String, Vec<Series>>) -> Result<Data, String> {
    match expr {
        Expr::Number(number) => Ok(Data::Scalar(*number)),
        Expr::Selector(selector) => {
            let at = time_ms - selector.offset_ms;
            let series = data.get(&selector.name).map(Vec::as_slice).unwrap_or_default();
            let chosen = series.iter().filter(|series| selector.matchers.iter().all(|matcher| matcher.matches(&series.labels)));
            match selector.range_ms {
                Some(range) => Ok(Data::Matrix(
                    chosen
                        .map(|series| (series.labels.clone(), window(&series.samples, at - range, at).to_vec()))
                        .filter(|(_, samples)| !samples.is_empty())
                        .collect(),
                )),
                None => Ok(Data::Vector(chosen.filter_map(|series| window(&series.samples, at - LOOKBACK_MS, at).last().map(|(_, value)| (series.labels.clone(), *value))).collect())),
            }
        }
        Expr::Call(name, arguments) => {
            if name == "time" && arguments.is_empty() {
                return Ok(Data::Scalar(time_ms as f64 / 1000.0));
            }
            let [argument] = arguments.as_slice() else {
                return Err(format!("{name} takes one argument"));
            };
            match (evaluate(argument, time_ms, data)?, argument) {
                (Data::Matrix(matrix), Expr::Selector(selector)) => {
                    let range = selector.range_ms.unwrap_or(0);
                    let end = time_ms - selector.offset_ms;
                    let mut out = Vec::new();
                    for (labels, samples) in matrix {
                        if let Some(value) = range_function(name, &samples, end - range, end)? {
                            out.push((without_name(&labels), value));
                        }
                    }
                    Ok(Data::Vector(out))
                }
                (Data::Vector(vector), _) => {
                    let mut out = Vec::with_capacity(vector.len());
                    for (labels, value) in vector {
                        out.push((without_name(&labels), scalar_function(name, value).ok_or_else(|| format!("unknown function {name}"))?));
                    }
                    Ok(Data::Vector(out))
                }
                (Data::Scalar(value), _) => scalar_function(name, value).map(Data::Scalar).ok_or_else(|| format!("{name} does not take a scalar")),
                (Data::Matrix(_), _) => Err(format!("{name} needs a range selector")),
            }
        }
        Expr::Aggregate(op, grouping, inner) => {
            let Data::Vector(vector) = evaluate(inner, time_ms, data)? else {
                return Err(format!("{op} needs an instant vector"));
            };
            let mut groups: BTreeMap<Labels, Vec<f64>> = BTreeMap::new();
            for (labels, value) in vector {
                let key: Labels = match grouping {
                    Some(Grouping::By(names)) => labels.into_iter().filter(|(name, _)| names.contains(name)).collect(),
                    Some(Grouping::Without(names)) => labels.into_iter().filter(|(name, _)| name != "__name__" && !names.contains(name)).collect(),
                    None => Vec::new(),
                };
                groups.entry(key).or_default().push(value);
            }
            let out = groups
                .into_iter()
                .map(|(labels, values)| {
                    let value = match op.as_str() {
                        "sum" => values.iter().sum(),
                        "avg" => values.iter().sum::<f64>() / values.len() as f64,
                        "min" => values.iter().copied().fold(f64::INFINITY, f64::min),
                        "max" => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                        _ => values.len() as f64,
                    };
                    (labels, value)
                })
                .collect();
            Ok(Data::Vector(out))
        }
        Expr::Binary(op, left, right) => {
            let comparison = is_comparison(op);
            match (evaluate(left, time_ms, data)?, evaluate(right, time_ms, data)?) {
                (Data::Scalar(left), Data::Scalar(right)) => {
                    if comparison {
                        return Err("comparisons between scalars need the bool modifier, which is not supported yet".to_owned());
                    }
                    Ok(Data::Scalar(apply(op, left, right).unwrap_or(f64::NAN)))
                }
                (Data::Vector(vector), Data::Scalar(scalar)) => Ok(Data::Vector(
                    vector.into_iter().filter_map(|(labels, value)| apply(op, value, scalar).map(|result| (if comparison { labels } else { without_name(&labels) }, result))).collect(),
                )),
                (Data::Scalar(scalar), Data::Vector(vector)) => Ok(Data::Vector(
                    vector
                        .into_iter()
                        .filter_map(|(labels, value)| apply(op, scalar, value).map(|result| (if comparison { labels } else { without_name(&labels) }, if comparison { value } else { result })))
                        .collect(),
                )),
                (Data::Vector(left), Data::Vector(right)) => {
                    let mut by_labels: BTreeMap<Labels, f64> = BTreeMap::new();
                    for (labels, value) in right {
                        if by_labels.insert(without_name(&labels), value).is_some() {
                            return Err("many-to-many matching is not allowed: the right side has duplicate label sets".to_owned());
                        }
                    }
                    Ok(Data::Vector(
                        left.into_iter()
                            .filter_map(|(labels, value)| {
                                let other = *by_labels.get(&without_name(&labels))?;
                                apply(op, value, other).map(|result| (if comparison { labels } else { without_name(&labels) }, result))
                            })
                            .collect(),
                    ))
                }
                _ => Err("binary operations need scalars or instant vectors".to_owned()),
            }
        }
    }
}
