use crate::store::{SCHEMA, identifier, literal, run};
use eggdb_sql::{Session, Value};
use std::collections::BTreeMap;

const MOST_ROWS: usize = 1_000_000;
const MOST_WINDOWS: i64 = 100_000;
const UNIX_EPOCH_FROM_2000_MICROS: i64 = 946_684_800_000_000;
const LEADING_COLUMNS: [&str; 4] = ["_start", "_stop", "_time", "_value"];

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Name(String),
    Text(String),
    Number(String),
    Duration(i64),
    Time(String),
    Regex(String),
    Symbol(&'static str),
}

const SYMBOLS: [&str; 25] = [
    "|>", "=>", "==", "!=", "<=", ">=", "=~", "!~", "<", ">", "(", ")", "[", "]", "{", "}", ",", ":", ".", "=", "-", "+",
    "*", "/", "%",
];

fn duration_micros(text: &str) -> Option<i64> {
    let characters: Vec<char> = text.chars().collect();
    let mut total: i64 = 0;
    let mut at = 0;
    while at < characters.len() {
        let start = at;
        while at < characters.len() && characters[at].is_ascii_digit() {
            at += 1;
        }
        let amount: i64 = characters[start..at].iter().collect::<String>().parse().ok()?;
        let unit_start = at;
        while at < characters.len() && characters[at].is_alphabetic() {
            at += 1;
        }
        let unit: String = characters[unit_start..at].iter().collect();
        let micros_per_unit: i64 = match unit.as_str() {
            "ns" => {
                total = total.checked_add(amount / 1000)?;
                continue;
            }
            "us" | "µs" => 1,
            "ms" => 1_000,
            "s" => 1_000_000,
            "m" => 60_000_000,
            "h" => 3_600_000_000,
            "d" => 86_400_000_000,
            "w" => 604_800_000_000,
            _ => return None,
        };
        total = total.checked_add(amount.checked_mul(micros_per_unit)?)?;
    }
    Some(total)
}

fn tokens(text: &str) -> Result<Vec<Token>, String> {
    let characters: Vec<char> = text.chars().collect();
    let mut found: Vec<Token> = Vec::new();
    let mut at = 0;
    while at < characters.len() {
        let character = characters[at];
        if character.is_whitespace() {
            at += 1;
            continue;
        }
        if character == '/' && characters.get(at + 1) == Some(&'/') {
            while at < characters.len() && characters[at] != '\n' {
                at += 1;
            }
            continue;
        }
        if character == '"' {
            let mut out = String::new();
            at += 1;
            loop {
                match characters.get(at) {
                    None => return Err("a string is not closed".to_owned()),
                    Some('"') => break,
                    Some('\\') => {
                        match characters.get(at + 1) {
                            Some('n') => out.push('\n'),
                            Some('t') => out.push('\t'),
                            Some(other) => out.push(*other),
                            None => return Err("a string is not closed".to_owned()),
                        }
                        at += 2;
                    }
                    Some(other) => {
                        out.push(*other);
                        at += 1;
                    }
                }
            }
            at += 1;
            found.push(Token::Text(out));
            continue;
        }
        if character == '/' && matches!(found.last(), Some(Token::Symbol("=~" | "!~"))) {
            let mut out = String::new();
            at += 1;
            loop {
                match characters.get(at) {
                    None => return Err("a regular expression is not closed".to_owned()),
                    Some('/') => break,
                    Some('\\') if characters.get(at + 1) == Some(&'/') => {
                        out.push('/');
                        at += 2;
                    }
                    Some(other) => {
                        out.push(*other);
                        at += 1;
                    }
                }
            }
            at += 1;
            found.push(Token::Regex(out));
            continue;
        }
        if character.is_ascii_digit() {
            let start = at;
            while at < characters.len() && (characters[at].is_ascii_digit() || characters[at] == '.') {
                at += 1;
            }
            let digits = at - start;
            if characters.get(at) == Some(&'-') && digits == 4 {
                while at < characters.len() && !characters[at].is_whitespace() && !matches!(characters[at], ',' | ')' | ']') {
                    at += 1;
                }
                found.push(Token::Time(characters[start..at].iter().collect()));
            } else if characters.get(at).is_some_and(|next| next.is_alphabetic()) {
                while at < characters.len() && (characters[at].is_alphanumeric()) {
                    at += 1;
                }
                let word: String = characters[start..at].iter().collect();
                found.push(Token::Duration(duration_micros(&word).ok_or_else(|| format!("invalid duration {word}"))?));
            } else {
                found.push(Token::Number(characters[start..at].iter().collect()));
            }
            continue;
        }
        if character.is_alphabetic() || character == '_' {
            let start = at;
            while at < characters.len() && (characters[at].is_alphanumeric() || characters[at] == '_') {
                at += 1;
            }
            found.push(Token::Name(characters[start..at].iter().collect()));
            continue;
        }
        let rest: String = characters[at..(at + 2).min(characters.len())].iter().collect();
        let Some(symbol) = SYMBOLS.iter().find(|symbol| rest.starts_with(**symbol)) else {
            return Err(format!("unexpected {character}"));
        };
        found.push(Token::Symbol(*symbol));
        at += symbol.chars().count();
    }
    Ok(found)
}

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Column(String),
    Text(String),
    Number(f64),
    Boolean(bool),
    Regex(String),
    Null,
    Not(Box<Expr>),
    Exists(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Compare(Box<Expr>, &'static str, Box<Expr>),
    Arithmetic(Box<Expr>, &'static str, Box<Expr>),
    Record { extends_row: bool, fields: Vec<(String, Expr)> },
    Function(String, Vec<Expr>),
    Conditional(Box<Expr>, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
enum Argument {
    Text(String),
    Number(f64),
    Duration(i64),
    Time(String),
    Boolean(bool),
    Name(String),
    Now,
    List(Vec<Argument>),
    Record(Vec<(String, Argument)>),
    Predicate { parameter: String, body: Expr },
}

#[derive(Debug, Clone, PartialEq)]
struct Call {
    name: String,
    arguments: Vec<(String, Argument)>,
}

impl Call {
    fn argument(&self, name: &str) -> Option<&Argument> {
        self.arguments.iter().find(|(key, _)| key == name).map(|(_, value)| value)
    }
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
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
        if matches!(self.peek(), Some(Token::Symbol(found)) if *found == symbol) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, symbol: &str) -> Result<(), String> {
        if self.eat(symbol) {
            Ok(())
        } else {
            Err(format!("expected {symbol}, found {:?}", self.peek()))
        }
    }

    fn name(&mut self) -> Result<String, String> {
        match self.next() {
            Some(Token::Name(name)) => Ok(name),
            other => Err(format!("expected a name, found {other:?}")),
        }
    }

    #[cfg(test)]
    fn pipeline(&mut self) -> Result<Vec<Call>, String> {
        let mut calls = vec![self.call()?];
        while self.eat("|>") {
            calls.push(self.call()?);
        }
        Ok(calls)
    }

    fn statement(&mut self) -> Result<Option<Statement>, String> {
        while matches!(self.peek(), Some(Token::Name(word)) if word == "import")
            && matches!(self.tokens.get(self.at + 1), Some(Token::Text(_)))
        {
            self.at += 2;
        }
        if self.peek().is_none() {
            return Ok(None);
        }
        let assigns = matches!(self.peek(), Some(Token::Name(_))) && matches!(self.tokens.get(self.at + 1), Some(Token::Symbol("=")));
        let assign = if assigns {
            let name = self.name()?;
            self.expect("=")?;
            Some(name)
        } else {
            None
        };
        let names_stream = matches!(self.peek(), Some(Token::Name(_)))
            && !matches!(self.tokens.get(self.at + 1), Some(Token::Symbol("(" | ".")));
        let (head, mut calls) = if names_stream {
            (Head::Variable(self.name()?), Vec::new())
        } else {
            let first = self.call()?;
            if first.name == "join" { (Head::Join(first), Vec::new()) } else { (Head::From, vec![first]) }
        };
        while self.eat("|>") {
            calls.push(self.call()?);
        }
        Ok(Some(Statement { assign, head, calls }))
    }

    fn call(&mut self) -> Result<Call, String> {
        let mut name = self.name()?;
        while self.eat(".") {
            name = format!("{name}.{}", self.name()?);
        }
        self.expect("(")?;
        let mut arguments = Vec::new();
        while !self.eat(")") {
            let key = self.name()?;
            self.expect(":")?;
            arguments.push((key, self.argument()?));
            if !self.eat(",") {
                self.expect(")")?;
                break;
            }
        }
        Ok(Call { name, arguments })
    }

    fn argument(&mut self) -> Result<Argument, String> {
        if self.eat("-") {
            return match self.next() {
                Some(Token::Duration(micros)) => Ok(Argument::Duration(-micros)),
                Some(Token::Number(number)) => {
                    Ok(Argument::Number(-number.parse::<f64>().map_err(|_| format!("invalid number {number}"))?))
                }
                other => Err(format!("found {other:?} after -")),
            };
        }
        if self.eat("{") {
            let mut fields = Vec::new();
            while !self.eat("}") {
                let key = self.name()?;
                self.expect(":")?;
                fields.push((key, self.argument()?));
                if !self.eat(",") {
                    self.expect("}")?;
                    break;
                }
            }
            return Ok(Argument::Record(fields));
        }
        if self.eat("[") {
            let mut items = Vec::new();
            while !self.eat("]") {
                items.push(self.argument()?);
                if !self.eat(",") {
                    self.expect("]")?;
                    break;
                }
            }
            return Ok(Argument::List(items));
        }
        if self.eat("(") {
            let parameter = self.name()?;
            self.expect(")")?;
            self.expect("=>")?;
            let body = self.or()?;
            return Ok(Argument::Predicate { parameter, body });
        }
        match self.next() {
            Some(Token::Text(text)) => Ok(Argument::Text(text)),
            Some(Token::Number(number)) => Ok(Argument::Number(number.parse().map_err(|_| format!("invalid number {number}"))?)),
            Some(Token::Duration(micros)) => Ok(Argument::Duration(micros)),
            Some(Token::Time(text)) => Ok(Argument::Time(text)),
            Some(Token::Name(name)) if name == "true" || name == "false" => Ok(Argument::Boolean(name == "true")),
            Some(Token::Name(name)) if name == "now" => {
                self.expect("(")?;
                self.expect(")")?;
                Ok(Argument::Now)
            }
            Some(Token::Name(name)) => {
                let mut name = name;
                while self.eat(".") {
                    name = format!("{name}.{}", self.name()?);
                }
                Ok(Argument::Name(name))
            }
            other => Err(format!("expected an argument value, found {other:?}")),
        }
    }

    fn or(&mut self) -> Result<Expr, String> {
        let mut left = self.and()?;
        while matches!(self.peek(), Some(Token::Name(word)) if word == "or") {
            self.at += 1;
            left = Expr::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, String> {
        let mut left = self.unary()?;
        while matches!(self.peek(), Some(Token::Name(word)) if word == "and") {
            self.at += 1;
            left = Expr::And(Box::new(left), Box::new(self.unary()?));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Some(Token::Name(word)) if word == "not" => {
                self.at += 1;
                Ok(Expr::Not(Box::new(self.unary()?)))
            }
            Some(Token::Name(word)) if word == "exists" => {
                self.at += 1;
                Ok(Expr::Exists(Box::new(self.primary()?)))
            }
            _ => self.comparison(),
        }
    }

    fn comparison(&mut self) -> Result<Expr, String> {
        let left = self.additive()?;
        let operator = match self.peek() {
            Some(Token::Symbol(symbol)) if ["==", "!=", "<", "<=", ">", ">=", "=~", "!~"].contains(symbol) => *symbol,
            _ => return Ok(left),
        };
        self.at += 1;
        let right = self.additive()?;
        Ok(Expr::Compare(Box::new(left), operator, Box::new(right)))
    }

    fn additive(&mut self) -> Result<Expr, String> {
        let mut left = self.multiplicative()?;
        while let Some(Token::Symbol(symbol @ ("+" | "-"))) = self.peek().cloned() {
            self.at += 1;
            left = Expr::Arithmetic(Box::new(left), symbol, Box::new(self.multiplicative()?));
        }
        Ok(left)
    }

    fn multiplicative(&mut self) -> Result<Expr, String> {
        let mut left = self.primary()?;
        while let Some(Token::Symbol(symbol @ ("*" | "/" | "%"))) = self.peek().cloned() {
            self.at += 1;
            left = Expr::Arithmetic(Box::new(left), symbol, Box::new(self.primary()?));
        }
        Ok(left)
    }

    fn record(&mut self) -> Result<Expr, String> {
        let extends_row = matches!(self.peek(), Some(Token::Name(_)))
            && matches!(self.tokens.get(self.at + 1), Some(Token::Name(word)) if word == "with");
        if extends_row {
            self.at += 2;
        }
        let mut fields = Vec::new();
        while !self.eat("}") {
            let key = match self.next() {
                Some(Token::Name(name) | Token::Text(name)) => name,
                other => return Err(format!("expected a field name, found {other:?}")),
            };
            self.expect(":")?;
            fields.push((key, self.or()?));
            if !self.eat(",") {
                self.expect("}")?;
                break;
            }
        }
        Ok(Expr::Record { extends_row, fields })
    }

    fn primary(&mut self) -> Result<Expr, String> {
        if self.eat("(") {
            let inner = self.or()?;
            self.expect(")")?;
            return Ok(inner);
        }
        if self.eat("{") {
            return self.record();
        }
        if matches!(self.peek(), Some(Token::Name(word)) if word == "if") {
            self.at += 1;
            let condition = self.or()?;
            if !matches!(self.next(), Some(Token::Name(word)) if word == "then") {
                return Err("expected then".to_owned());
            }
            let then = self.or()?;
            if !matches!(self.next(), Some(Token::Name(word)) if word == "else") {
                return Err("expected else".to_owned());
            }
            let otherwise = self.or()?;
            return Ok(Expr::Conditional(Box::new(condition), Box::new(then), Box::new(otherwise)));
        }
        if let (Some(Token::Name(function)), Some(Token::Symbol("("))) = (self.peek().cloned(), self.tokens.get(self.at + 1)) {
            self.at += 2;
            let mut arguments = Vec::new();
            while !self.eat(")") {
                self.name()?;
                self.expect(":")?;
                arguments.push(self.or()?);
                if !self.eat(",") {
                    self.expect(")")?;
                    break;
                }
            }
            return Ok(Expr::Function(function, arguments));
        }
        if self.eat("-") {
            return match self.next() {
                Some(Token::Number(number)) => Ok(Expr::Number(-number.parse::<f64>().map_err(|_| format!("invalid number {number}"))?)),
                other => Err(format!("found {other:?} after -")),
            };
        }
        match self.next() {
            Some(Token::Text(text)) => Ok(Expr::Text(text)),
            Some(Token::Number(number)) => Ok(Expr::Number(number.parse().map_err(|_| format!("invalid number {number}"))?)),
            Some(Token::Regex(pattern)) => Ok(Expr::Regex(pattern)),
            Some(Token::Name(word)) if word == "true" || word == "false" => Ok(Expr::Boolean(word == "true")),
            Some(Token::Name(_)) => {
                if self.eat(".") {
                    return Ok(Expr::Column(self.name()?));
                }
                self.expect("[")?;
                let Some(Token::Text(column)) = self.next() else {
                    return Err("expected a quoted column name in r[...]".to_owned());
                };
                self.expect("]")?;
                Ok(Expr::Column(column))
            }
            other => Err(format!("expected a value, found {other:?}")),
        }
    }
}

enum Partial {
    Known(Expr),
    Sql(String),
}

struct Candidate<'c> {
    measurement: &'c str,
    field: &'c str,
    tags: Vec<&'c str>,
}

fn literal_sql(expr: &Expr) -> String {
    match expr {
        Expr::Text(text) | Expr::Regex(text) => literal(text),
        Expr::Number(number) => format!("{number:?}"),
        Expr::Boolean(flag) => if *flag { "TRUE" } else { "FALSE" }.to_owned(),
        _ => "NULL".to_owned(),
    }
}

fn compare_known(left: &Expr, operator: &str, right: &Expr) -> Option<bool> {
    let ordering = match (left, right) {
        (Expr::Null, _) | (_, Expr::Null) => return Some(false),
        (Expr::Text(left), Expr::Text(right)) => left.cmp(right),
        (Expr::Number(left), Expr::Number(right)) => left.partial_cmp(right)?,
        (Expr::Boolean(left), Expr::Boolean(right)) => left.cmp(right),
        _ => return None,
    };
    Some(match operator {
        "==" => ordering.is_eq(),
        "!=" => !ordering.is_eq(),
        "<" => ordering.is_lt(),
        "<=" => ordering.is_le(),
        ">" => ordering.is_gt(),
        ">=" => ordering.is_ge(),
        _ => return None,
    })
}

fn partial(expr: &Expr, candidate: &Candidate<'_>) -> Partial {
    let column_sql = |name: &str| identifier(name);
    match expr {
        Expr::Column(name) if name == "_measurement" => Partial::Known(Expr::Text(candidate.measurement.to_owned())),
        Expr::Column(name) if name == "_field" => Partial::Known(Expr::Text(candidate.field.to_owned())),
        Expr::Column(name) if name == "_value" => Partial::Sql(column_sql(candidate.field)),
        Expr::Column(name) if name == "_time" => Partial::Sql("\"time\"".to_owned()),
        Expr::Column(name) if candidate.tags.contains(&name.as_str()) => {
            Partial::Sql(format!("NULLIF({}, '')", column_sql(name)))
        }
        Expr::Column(_) => Partial::Known(Expr::Null),
        Expr::Not(inner) => match partial(inner, candidate) {
            Partial::Known(Expr::Boolean(flag)) => Partial::Known(Expr::Boolean(!flag)),
            Partial::Known(other) => Partial::Sql(format!("NOT ({})", literal_sql(&other))),
            Partial::Sql(sql) => Partial::Sql(format!("NOT ({sql})")),
        },
        Expr::Exists(inner) => match partial(inner, candidate) {
            Partial::Known(Expr::Null) => Partial::Known(Expr::Boolean(false)),
            Partial::Known(_) => Partial::Known(Expr::Boolean(true)),
            Partial::Sql(sql) => Partial::Sql(format!("({sql}) IS NOT NULL")),
        },
        Expr::And(left, right) | Expr::Or(left, right) => {
            let is_and = matches!(expr, Expr::And(..));
            let (left, right) = (partial(left, candidate), partial(right, candidate));
            match (left, right) {
                (Partial::Known(Expr::Boolean(flag)), other) | (other, Partial::Known(Expr::Boolean(flag))) => {
                    if flag == is_and {
                        other
                    } else {
                        Partial::Known(Expr::Boolean(flag))
                    }
                }
                (left, right) => {
                    let sql = |side: Partial| match side {
                        Partial::Known(known) => literal_sql(&known),
                        Partial::Sql(sql) => sql,
                    };
                    Partial::Sql(format!("({} {} {})", sql(left), if is_and { "AND" } else { "OR" }, sql(right)))
                }
            }
        }
        Expr::Compare(left, operator, right) => {
            let (left, right) = (partial(left, candidate), partial(right, candidate));
            if let (Partial::Known(left), Partial::Known(right)) = (&left, &right)
                && let Some(answer) = compare_known(left, operator, right)
            {
                return Partial::Known(Expr::Boolean(answer));
            }
            let sql = |side: Partial| match side {
                Partial::Known(known) => literal_sql(&known),
                Partial::Sql(sql) => sql,
            };
            let operator_sql = match *operator {
                "==" => "=",
                "!=" => "<>",
                "=~" => "~",
                other => other,
            };
            Partial::Sql(format!("({} {operator_sql} {})", sql(left), sql(right)))
        }
        Expr::Arithmetic(left, operator, right) => {
            let sql = |side: Partial| match side {
                Partial::Known(known) => literal_sql(&known),
                Partial::Sql(sql) => sql,
            };
            Partial::Sql(format!("({} {operator} {})", sql(partial(left, candidate)), sql(partial(right, candidate))))
        }
        known => Partial::Known(known.clone()),
    }
}

fn pushable(expr: &Expr) -> bool {
    match expr {
        Expr::Record { .. } | Expr::Function(..) | Expr::Conditional(..) => false,
        Expr::Not(inner) | Expr::Exists(inner) => pushable(inner),
        Expr::And(left, right) | Expr::Or(left, right) | Expr::Compare(left, _, right) | Expr::Arithmetic(left, _, right) => {
            pushable(left) && pushable(right)
        }
        _ => true,
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Flux {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    Text(String),
    Time(i64),
}

fn flux_of(value: &Value) -> Flux {
    match value {
        Value::Null => Flux::Null,
        Value::Boolean(flag) => Flux::Boolean(*flag),
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) => value.as_integer().map_or(Flux::Null, Flux::Integer),
        Value::TimestampTz(micros) | Value::Timestamp(micros) => Flux::Time(*micros),
        Value::Text(text) => Flux::Text(text.clone()),
        other => other.as_float().map_or_else(|| other.to_text().map_or(Flux::Null, Flux::Text), Flux::Float),
    }
}

fn value_of(flux: Flux) -> Value {
    match flux {
        Flux::Null => Value::Null,
        Flux::Boolean(flag) => Value::Boolean(flag),
        Flux::Integer(whole) => Value::BigInt(whole),
        Flux::Float(number) => Value::Double(number),
        Flux::Text(text) => Value::Text(text),
        Flux::Time(micros) => Value::TimestampTz(micros),
    }
}

fn as_float(flux: &Flux) -> Option<f64> {
    match flux {
        Flux::Integer(whole) => Some(*whole as f64),
        Flux::Float(number) => Some(*number),
        Flux::Time(micros) => Some(*micros as f64),
        _ => None,
    }
}

fn evaluate(expr: &Expr, row: &Row) -> Result<Flux, String> {
    Ok(match expr {
        Expr::Column(name) => row.get(name).map_or(Flux::Null, flux_of),
        Expr::Text(text) => Flux::Text(text.clone()),
        Expr::Number(number) => Flux::Float(*number),
        Expr::Boolean(flag) => Flux::Boolean(*flag),
        Expr::Null => Flux::Null,
        Expr::Regex(_) => return Err("regular expressions after a transformation are not supported yet".to_owned()),
        Expr::Not(inner) => match evaluate(inner, row)? {
            Flux::Boolean(flag) => Flux::Boolean(!flag),
            _ => Flux::Null,
        },
        Expr::Exists(inner) => Flux::Boolean(evaluate(inner, row)? != Flux::Null),
        Expr::And(left, right) => Flux::Boolean(
            evaluate(left, row)? == Flux::Boolean(true) && evaluate(right, row)? == Flux::Boolean(true),
        ),
        Expr::Or(left, right) => Flux::Boolean(
            evaluate(left, row)? == Flux::Boolean(true) || evaluate(right, row)? == Flux::Boolean(true),
        ),
        Expr::Compare(left, operator, right) => {
            let (left, right) = (evaluate(left, row)?, evaluate(right, row)?);
            let ordering = match (&left, &right) {
                (Flux::Null, _) | (_, Flux::Null) => return Ok(Flux::Boolean(false)),
                (Flux::Text(left), Flux::Text(right)) => Some(left.cmp(right)),
                (Flux::Boolean(left), Flux::Boolean(right)) => Some(left.cmp(right)),
                _ => match (as_float(&left), as_float(&right)) {
                    (Some(left), Some(right)) => left.partial_cmp(&right),
                    _ => None,
                },
            };
            let Some(ordering) = ordering else {
                return Ok(Flux::Boolean(false));
            };
            Flux::Boolean(match *operator {
                "==" => ordering.is_eq(),
                "!=" => !ordering.is_eq(),
                "<" => ordering.is_lt(),
                "<=" => ordering.is_le(),
                ">" => ordering.is_gt(),
                ">=" => ordering.is_ge(),
                _ => return Err("regular expressions after a transformation are not supported yet".to_owned()),
            })
        }
        Expr::Arithmetic(left, operator, right) => {
            let (left, right) = (evaluate(left, row)?, evaluate(right, row)?);
            match (&left, &right, *operator) {
                (Flux::Null, _, _) | (_, Flux::Null, _) => Flux::Null,
                (Flux::Text(left), Flux::Text(right), "+") => Flux::Text(format!("{left}{right}")),
                (Flux::Integer(left), Flux::Integer(right), operator) => match operator {
                    "+" => Flux::Integer(left.wrapping_add(*right)),
                    "-" => Flux::Integer(left.wrapping_sub(*right)),
                    "*" => Flux::Integer(left.wrapping_mul(*right)),
                    "/" if *right != 0 => Flux::Integer(left / right),
                    "%" if *right != 0 => Flux::Integer(left % right),
                    _ => return Err("division by zero".to_owned()),
                },
                _ => match (as_float(&left), as_float(&right)) {
                    (Some(left), Some(right)) => Flux::Float(match *operator {
                        "+" => left + right,
                        "-" => left - right,
                        "*" => left * right,
                        "/" => left / right,
                        _ => left % right,
                    }),
                    _ => return Err(format!("cannot apply {operator} to {left:?} and {right:?}")),
                },
            }
        }
        Expr::Conditional(condition, then, otherwise) => {
            if evaluate(condition, row)? == Flux::Boolean(true) {
                evaluate(then, row)?
            } else {
                evaluate(otherwise, row)?
            }
        }
        Expr::Function(name, arguments) => {
            let argument = match arguments.first() {
                Some(argument) => evaluate(argument, row)?,
                None => Flux::Null,
            };
            match (name.as_str(), argument) {
                (_, Flux::Null) => Flux::Null,
                ("float", value) => as_float(&value)
                    .or_else(|| if let Flux::Text(text) = &value { text.trim().parse().ok() } else { None })
                    .map_or(Flux::Null, Flux::Float),
                ("int", value) => as_float(&value)
                    .map(|number| number.trunc() as i64)
                    .or_else(|| if let Flux::Text(text) = &value { text.trim().parse().ok() } else { None })
                    .map_or(Flux::Null, Flux::Integer),
                ("string", value) => Flux::Text(match value {
                    Flux::Text(text) => text,
                    Flux::Boolean(flag) => flag.to_string(),
                    Flux::Integer(whole) => whole.to_string(),
                    Flux::Float(number) => number.to_string(),
                    Flux::Time(micros) => micros.to_string(),
                    Flux::Null => String::new(),
                }),
                ("bool", Flux::Text(text)) => Flux::Boolean(text == "true"),
                ("bool", value) => Flux::Boolean(as_float(&value).is_some_and(|number| number != 0.0)),
                (other, _) => return Err(format!("function {other}() is not supported yet")),
            }
        }
        Expr::Record { .. } => return Err("a record is not a value here".to_owned()),
    })
}

type Row = BTreeMap<String, Value>;

#[derive(Debug, Clone)]
struct Table {
    key: Vec<String>,
    rows: Vec<Row>,
}

impl Table {
    fn key_values(&self) -> Vec<String> {
        let first = self.rows.first();
        self.key
            .iter()
            .map(|column| first.and_then(|row| row.get(column)).and_then(Value::to_text).unwrap_or_default())
            .collect()
    }
}

fn time_bound(session: &mut Session<'_>, argument: Option<&Argument>) -> Result<String, String> {
    Ok(match argument {
        None | Some(Argument::Now) => "now()".to_owned(),
        Some(Argument::Duration(micros)) => format!("now() + '{micros} microseconds'::interval"),
        Some(Argument::Time(text)) => format!("{}::timestamptz", literal(text)),
        Some(Argument::Number(seconds)) => format!("to_timestamp({seconds:?})"),
        Some(other) => return Err(format!("range() cannot take {other:?}")),
    })
    .and_then(|sql| {
        let result = run(session, &format!("SELECT ({sql})::timestamptz"))?
            .into_iter()
            .rev()
            .find(|result| result.columns.is_some())
            .ok_or("the range could not be read")?;
        match result.rows.first().and_then(|row| row.first()) {
            Some(Value::TimestampTz(micros)) => Ok(micros.to_string()),
            _ => Err("the range could not be read".to_owned()),
        }
    })
}

fn micros_of(text: &str) -> i64 {
    text.parse().unwrap_or(0)
}

fn timestamp_sql(micros: i64) -> String {
    let text = Value::TimestampTz(micros).to_text().unwrap_or_default();
    format!("{}::timestamptz", literal(&text))
}

fn source_tables(
    session: &mut Session<'_>,
    start: i64,
    stop: i64,
    predicates: &[Expr],
) -> Result<Vec<Table>, String> {
    let catalog = crate::store::catalog(session)?;
    let mut tables: Vec<Table> = Vec::new();
    let mut total = 0usize;
    for (measurement, columns) in &catalog {
        let tags: Vec<&str> = columns
            .iter()
            .filter(|(name, kind)| kind == "text" && name != "time")
            .map(|(name, _)| name.as_str())
            .collect();
        for (field, _) in columns.iter().filter(|(name, kind)| kind != "text" && name != "time") {
            let candidate = Candidate {
                measurement,
                field,
                tags: tags.clone(),
            };
            let mut conditions = vec![
                format!("\"time\" >= {}", timestamp_sql(start)),
                format!("\"time\" < {}", timestamp_sql(stop)),
                format!("{} IS NOT NULL", identifier(field)),
            ];
            let mut excluded = false;
            for predicate in predicates {
                match partial(predicate, &candidate) {
                    Partial::Known(Expr::Boolean(true)) => {}
                    Partial::Known(Expr::Boolean(false) | Expr::Null) => excluded = true,
                    Partial::Known(other) => conditions.push(literal_sql(&other)),
                    Partial::Sql(sql) => conditions.push(sql),
                }
            }
            if excluded {
                continue;
            }
            let selected: Vec<String> = tags
                .iter()
                .map(|tag| identifier(tag))
                .chain(["\"time\"".to_owned(), identifier(field)])
                .collect();
            let order: Vec<String> = (1..=tags.len() + 1).map(|at| at.to_string()).collect();
            let sql = format!(
                "SELECT {} FROM {SCHEMA}.{} WHERE {} ORDER BY {} LIMIT {}",
                selected.join(", "),
                identifier(measurement),
                conditions.join(" AND "),
                order.join(", "),
                MOST_ROWS + 1 - total.min(MOST_ROWS)
            );
            let result = run(session, &sql)?
                .into_iter()
                .rev()
                .find(|result| result.columns.is_some())
                .ok_or("the query returned no rows")?;
            total += result.rows.len();
            if total > MOST_ROWS {
                return Err(format!("the query reads more than {MOST_ROWS} rows; narrow its range or filter"));
            }
            let mut by_series: BTreeMap<Vec<String>, Vec<Row>> = BTreeMap::new();
            for values in result.rows {
                let mut row = Row::new();
                row.insert("_start".to_owned(), Value::TimestampTz(start));
                row.insert("_stop".to_owned(), Value::TimestampTz(stop));
                row.insert("_measurement".to_owned(), Value::Text(measurement.clone()));
                row.insert("_field".to_owned(), Value::Text(field.clone()));
                let mut series = Vec::new();
                for (tag, value) in tags.iter().zip(&values) {
                    let text = value.to_text().unwrap_or_default();
                    series.push(text.clone());
                    if !text.is_empty() {
                        row.insert((*tag).to_owned(), Value::Text(text));
                    }
                }
                row.insert("_time".to_owned(), values.get(tags.len()).cloned().unwrap_or(Value::Null));
                row.insert("_value".to_owned(), values.get(tags.len() + 1).cloned().unwrap_or(Value::Null));
                by_series.entry(series).or_default().push(row);
            }
            for (_, rows) in by_series {
                let mut key: Vec<String> = vec!["_start".to_owned(), "_stop".to_owned(), "_field".to_owned(), "_measurement".to_owned()];
                key.extend(
                    rows.first()
                        .map(|row| row.keys().filter(|column| tags.contains(&column.as_str())).cloned().collect::<Vec<_>>())
                        .unwrap_or_default(),
                );
                tables.push(Table { key, rows });
            }
        }
    }
    Ok(tables)
}

fn number(value: &Value) -> Option<f64> {
    value.as_float()
}

fn aggregate(function: &str, rows: &[Row]) -> Result<Option<Row>, String> {
    let values: Vec<&Value> = rows
        .iter()
        .filter_map(|row| row.get("_value"))
        .filter(|value| !matches!(value, Value::Null))
        .collect();
    let numbers: Vec<f64> = values.iter().filter_map(|value| number(value)).collect();
    let selector = |pick: Option<&Row>| -> Result<Option<Row>, String> { Ok(pick.cloned()) };
    let mut out = rows.first().cloned().unwrap_or_default();
    out.remove("_time");
    let value = match function {
        "count" => Value::BigInt(values.len() as i64),
        "sum" if values.iter().all(|value| value.as_integer().is_some()) && !values.is_empty() => {
            Value::BigInt(values.iter().filter_map(|value| value.as_integer()).fold(0i64, i64::wrapping_add))
        }
        "sum" if numbers.is_empty() => Value::Null,
        "sum" => Value::Double(numbers.iter().sum()),
        "mean" if numbers.is_empty() => Value::Null,
        "mean" => Value::Double(numbers.iter().sum::<f64>() / numbers.len() as f64),
        "median" if numbers.is_empty() => Value::Null,
        "median" => {
            let mut sorted = numbers.clone();
            sorted.sort_by(f64::total_cmp);
            let middle = sorted.len() / 2;
            Value::Double(if sorted.len() % 2 == 0 { (sorted[middle - 1] + sorted[middle]) / 2.0 } else { sorted[middle] })
        }
        "spread" if numbers.is_empty() => Value::Null,
        "spread" => Value::Double(numbers.iter().copied().fold(f64::MIN, f64::max) - numbers.iter().copied().fold(f64::MAX, f64::min)),
        "stddev" if numbers.len() < 2 => Value::Null,
        "stddev" => {
            let mean = numbers.iter().sum::<f64>() / numbers.len() as f64;
            let variance = numbers.iter().map(|number| (number - mean).powi(2)).sum::<f64>() / (numbers.len() - 1) as f64;
            Value::Double(variance.sqrt())
        }
        "first" => return selector(rows.iter().find(|row| row.get("_value").is_some_and(|value| !matches!(value, Value::Null)))),
        "last" => return selector(rows.iter().rev().find(|row| row.get("_value").is_some_and(|value| !matches!(value, Value::Null)))),
        "min" | "max" => {
            let mut best: Option<(&Row, f64)> = None;
            for row in rows {
                let Some(value) = row.get("_value").and_then(number) else { continue };
                let better = match best {
                    None => true,
                    Some((_, held)) => if function == "min" { value < held } else { value > held },
                };
                if better {
                    best = Some((row, value));
                }
            }
            return selector(best.map(|(row, _)| row));
        }
        other => return Err(format!("unsupported aggregate {other}")),
    };
    out.insert("_value".to_owned(), value);
    Ok(Some(out))
}

fn function_name(argument: Option<&Argument>) -> Result<String, String> {
    match argument {
        Some(Argument::Name(name)) => Ok(name.clone()),
        other => Err(format!("aggregateWindow needs fn: an aggregate, found {other:?}")),
    }
}

fn aggregate_window(tables: Vec<Table>, call: &Call, start: i64, stop: i64) -> Result<Vec<Table>, String> {
    let Some(Argument::Duration(every)) = call.argument("every") else {
        return Err("aggregateWindow needs every: a duration".to_owned());
    };
    let every = *every;
    if every <= 0 {
        return Err("aggregateWindow's every must be positive".to_owned());
    }
    let function = function_name(call.argument("fn"))?;
    let create_empty = !matches!(call.argument("createEmpty"), Some(Argument::Boolean(false)));
    let origin = -UNIX_EPOCH_FROM_2000_MICROS;
    let floor = |at: i64| origin + (at - origin).div_euclid(every) * every;
    if (stop - start) / every > MOST_WINDOWS {
        return Err(format!("aggregateWindow would make more than {MOST_WINDOWS} windows"));
    }
    let mut out = Vec::with_capacity(tables.len());
    for table in tables {
        let template = table.rows.first().cloned().unwrap_or_default();
        let mut rows = Vec::new();
        let mut window = floor(start);
        let mut at = 0;
        while window < stop {
            let end = window + every;
            let begin = at;
            while at < table.rows.len()
                && table.rows[at].get("_time").is_some_and(|time| matches!(time, Value::TimestampTz(micros) if *micros < end))
            {
                at += 1;
            }
            let members = &table.rows[begin..at];
            let reduced = if members.is_empty() {
                if !create_empty {
                    window = end;
                    continue;
                }
                let mut empty = template.clone();
                empty.insert("_value".to_owned(), if function == "count" { Value::BigInt(0) } else { Value::Null });
                Some(empty)
            } else {
                aggregate(&function, members)?
            };
            if let Some(mut row) = reduced {
                row.insert("_time".to_owned(), Value::TimestampTz(end.min(stop)));
                rows.push(row);
            }
            window = end;
        }
        out.push(Table { key: table.key, rows });
    }
    Ok(out)
}

fn names(argument: Option<&Argument>) -> Result<Vec<String>, String> {
    match argument {
        None => Ok(Vec::new()),
        Some(Argument::List(items)) => items
            .iter()
            .map(|item| match item {
                Argument::Text(text) => Ok(text.clone()),
                other => Err(format!("expected column names, found {other:?}")),
            })
            .collect(),
        Some(other) => Err(format!("expected a list of column names, found {other:?}")),
    }
}

fn regroup(tables: Vec<Table>, columns: Vec<String>) -> Vec<Table> {
    let mut groups: BTreeMap<Vec<String>, Vec<Row>> = BTreeMap::new();
    for table in tables {
        for row in table.rows {
            let key: Vec<String> = columns
                .iter()
                .map(|column| row.get(column).and_then(Value::to_text).unwrap_or_default())
                .collect();
            groups.entry(key).or_default().push(row);
        }
    }
    groups
        .into_values()
        .map(|rows| Table {
            key: columns.clone(),
            rows,
        })
        .collect()
}

fn compare_values(left: Option<&Value>, right: Option<&Value>) -> std::cmp::Ordering {
    match (left.and_then(number), right.and_then(number)) {
        (Some(left), Some(right)) => left.total_cmp(&right),
        _ => left.and_then(Value::to_text).cmp(&right.and_then(Value::to_text)),
    }
}

fn whole_number(argument: Option<&Argument>, default: usize) -> usize {
    match argument {
        Some(Argument::Number(number)) if *number >= 0.0 => *number as usize,
        _ => default,
    }
}

struct Outcome {
    result: String,
    tables: Vec<Table>,
}

#[derive(Debug, Clone)]
struct Stream {
    tables: Vec<Table>,
    start: i64,
    stop: i64,
}

struct Applied {
    stream: Stream,
    result: String,
    yielded: bool,
}

fn execute(session: &mut Session<'_>, calls: &[Call]) -> Result<Applied, String> {
    if calls.first().is_none_or(|call| call.name != "from") {
        return Err("a query starts with from()".to_owned());
    }
    let Some(range) = calls.iter().position(|call| call.name == "range") else {
        return Err("a query needs range() after from()".to_owned());
    };
    let start = micros_of(&time_bound(session, calls[range].argument("start"))?);
    let stop = micros_of(&time_bound(session, calls[range].argument("stop"))?);
    let mut pushed = Vec::new();
    let mut next = range + 1;
    while let Some(call) = calls.get(next).filter(|call| call.name == "filter") {
        match call.argument("fn") {
            Some(Argument::Predicate { body, .. }) if pushable(body) => pushed.push(body.clone()),
            Some(Argument::Predicate { .. }) => break,
            _ => return Err("filter needs fn: (r) => ...".to_owned()),
        }
        next += 1;
    }
    let tables = source_tables(session, start, stop, &pushed)?;
    let rest: Vec<&Call> = calls
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(index, _)| *index != range && !(*index > range && *index < next))
        .map(|(_, call)| call)
        .collect();
    apply(Stream { tables, start, stop }, &rest)
}

fn apply(stream: Stream, calls: &[&Call]) -> Result<Applied, String> {
    let Stream { mut tables, start, stop } = stream;
    let mut result = "_result".to_owned();
    let mut yielded = false;
    for call in calls {
        tables = match call.name.as_str() {
            "filter" => {
                let Some(Argument::Predicate { body, .. }) = call.argument("fn") else {
                    return Err("filter needs fn: (r) => ...".to_owned());
                };
                let mut kept = Vec::with_capacity(tables.len());
                for mut table in tables {
                    let mut rows = Vec::with_capacity(table.rows.len());
                    for row in table.rows {
                        if evaluate(body, &row)? == Flux::Boolean(true) {
                            rows.push(row);
                        }
                    }
                    table.rows = rows;
                    kept.push(table);
                }
                kept
            }
            "map" => {
                let Some(Argument::Predicate { body: Expr::Record { extends_row, fields }, .. }) = call.argument("fn") else {
                    return Err("map needs fn: (r) => ({...})".to_owned());
                };
                let mut mapped = Vec::with_capacity(tables.len());
                for mut table in tables {
                    let mut rows = Vec::with_capacity(table.rows.len());
                    for row in table.rows {
                        let mut out = if *extends_row {
                            row.clone()
                        } else {
                            table.key.iter().filter_map(|column| row.get(column).map(|value| (column.clone(), value.clone()))).collect()
                        };
                        for (name, expr) in fields {
                            out.insert(name.clone(), value_of(evaluate(expr, &row)?));
                        }
                        rows.push(out);
                    }
                    table.rows = rows;
                    mapped.push(table);
                }
                mapped
            }
            "rename" | "duplicate" => {
                let pairs: Vec<(String, String)> = if call.name == "rename" {
                    match call.argument("columns") {
                        Some(Argument::Record(fields)) => fields
                            .iter()
                            .map(|(from, to)| match to {
                                Argument::Text(to) => Ok((from.clone(), to.clone())),
                                other => Err(format!("rename's columns map names to strings, found {other:?}")),
                            })
                            .collect::<Result<_, String>>()?,
                        _ => return Err("rename needs columns: {old: \"new\"}".to_owned()),
                    }
                } else {
                    match (call.argument("column"), call.argument("as")) {
                        (Some(Argument::Text(from)), Some(Argument::Text(to))) => vec![(from.clone(), to.clone())],
                        _ => return Err("duplicate needs column: and as:".to_owned()),
                    }
                };
                let keep = call.name == "duplicate";
                tables
                    .into_iter()
                    .map(|mut table| {
                        for row in &mut table.rows {
                            for (from, to) in &pairs {
                                let value = if keep { row.get(from).cloned() } else { row.remove(from) };
                                if let Some(value) = value {
                                    row.insert(to.clone(), value);
                                }
                            }
                        }
                        if !keep {
                            for column in &mut table.key {
                                if let Some((_, to)) = pairs.iter().find(|(from, _)| from == column) {
                                    column.clone_from(to);
                                }
                            }
                        }
                        table
                    })
                    .collect()
            }
            "pivot" => {
                let row_key = names(call.argument("rowKey"))?;
                let column_key = names(call.argument("columnKey"))?;
                let Some(Argument::Text(value_column)) = call.argument("valueColumn") else {
                    return Err("pivot needs valueColumn:".to_owned());
                };
                tables
                    .into_iter()
                    .map(|table| {
                        let key: Vec<String> = table.key.iter().filter(|column| !column_key.contains(column)).cloned().collect();
                        let mut rows: Vec<(Vec<String>, Row)> = Vec::new();
                        for row in table.rows {
                            let identity: Vec<String> = row_key
                                .iter()
                                .map(|column| row.get(column).and_then(Value::to_text).unwrap_or_default())
                                .collect();
                            let name: Vec<String> = column_key
                                .iter()
                                .map(|column| row.get(column).and_then(Value::to_text).unwrap_or_default())
                                .collect();
                            let value = row.get(value_column).cloned().unwrap_or(Value::Null);
                            let at = match rows.iter().position(|(known, _)| *known == identity) {
                                Some(at) => at,
                                None => {
                                    let mut base = Row::new();
                                    for (column, cell) in &row {
                                        if key.contains(column) || row_key.contains(column) {
                                            base.insert(column.clone(), cell.clone());
                                        }
                                    }
                                    rows.push((identity, base));
                                    rows.len() - 1
                                }
                            };
                            rows[at].1.insert(name.join("_"), value);
                        }
                        Table { key, rows: rows.into_iter().map(|(_, row)| row).collect() }
                    })
                    .collect()
            }
            "aggregateWindow" => aggregate_window(tables, call, start, stop)?,
            "mean" | "sum" | "count" | "median" | "spread" | "stddev" | "first" | "last" | "min" | "max" => tables
                .into_iter()
                .map(|table| {
                    let rows = aggregate(&call.name, &table.rows)?.into_iter().collect();
                    Ok(Table { key: table.key, rows })
                })
                .collect::<Result<_, String>>()?,
            "group" => {
                let columns = names(call.argument("columns"))?;
                if matches!(call.argument("mode"), Some(Argument::Text(mode)) if mode != "by") {
                    return Err("group() supports mode: \"by\" only".to_owned());
                }
                regroup(tables, columns)
            }
            "sort" => {
                let columns = {
                    let given = names(call.argument("columns"))?;
                    if given.is_empty() { vec!["_value".to_owned()] } else { given }
                };
                let descending = matches!(call.argument("desc"), Some(Argument::Boolean(true)));
                tables
                    .into_iter()
                    .map(|mut table| {
                        table.rows.sort_by(|left, right| {
                            let ordering = columns
                                .iter()
                                .map(|column| compare_values(left.get(column), right.get(column)))
                                .find(|ordering| !ordering.is_eq())
                                .unwrap_or(std::cmp::Ordering::Equal);
                            if descending { ordering.reverse() } else { ordering }
                        });
                        table
                    })
                    .collect()
            }
            "limit" | "tail" => {
                let count = whole_number(call.argument("n"), usize::MAX);
                let offset = whole_number(call.argument("offset"), 0);
                tables
                    .into_iter()
                    .map(|mut table| {
                        if call.name == "tail" {
                            let keep = table.rows.len().saturating_sub(offset);
                            table.rows.truncate(keep);
                            let skip = table.rows.len().saturating_sub(count);
                            table.rows.drain(..skip);
                        } else {
                            table.rows = table.rows.into_iter().skip(offset).take(count).collect();
                        }
                        table
                    })
                    .collect()
            }
            "keep" | "drop" => {
                let columns = names(call.argument("columns"))?;
                let keeping = call.name == "keep";
                tables
                    .into_iter()
                    .map(|mut table| {
                        let wanted = |column: &String| columns.contains(column) == keeping;
                        table.key.retain(|column| wanted(column));
                        for row in &mut table.rows {
                            row.retain(|column, _| wanted(column));
                        }
                        table
                    })
                    .collect()
            }
            "yield" => {
                if let Some(Argument::Text(name)) = call.argument("name") {
                    result = name.clone();
                }
                yielded = true;
                tables
            }
            other => return Err(format!("function {other}() is not supported yet")),
        };
    }
    tables.retain(|table| !table.rows.is_empty());
    Ok(Applied {
        stream: Stream { tables, start, stop },
        result,
        yielded,
    })
}

fn on_values(row: &Row, on: &[String]) -> Option<Vec<String>> {
    on.iter().map(|column| row.get(column).and_then(Value::to_text)).collect()
}

fn joined(call: &Call, streams: &BTreeMap<String, Stream>) -> Result<Stream, String> {
    let Some(Argument::Record(pairs)) = call.argument("tables") else {
        return Err("join needs tables: {a: first, b: second}".to_owned());
    };
    let [(left_name, Argument::Name(left_stream)), (right_name, Argument::Name(right_stream))] = pairs.as_slice() else {
        return Err("join takes exactly two streams named by variables".to_owned());
    };
    if matches!(call.argument("method"), Some(Argument::Text(method)) if method != "inner") {
        return Err("join supports method: \"inner\" only".to_owned());
    }
    let on = names(call.argument("on"))?;
    if on.is_empty() {
        return Err("join needs on: [columns]".to_owned());
    }
    let stream_named = |name: &str| streams.get(name).ok_or_else(|| format!("undefined identifier {name}"));
    let (left, right) = (stream_named(left_stream)?, stream_named(right_stream)?);
    let columns_of = |stream: &Stream| -> Vec<String> {
        let mut columns: Vec<String> = stream.tables.iter().flat_map(|table| table.rows.iter()).flat_map(|row| row.keys().cloned()).collect();
        columns.sort();
        columns.dedup();
        columns
    };
    let (left_columns, right_columns) = (columns_of(left), columns_of(right));
    let mut by_key: BTreeMap<Vec<String>, Vec<&Row>> = BTreeMap::new();
    for row in right.tables.iter().flat_map(|table| table.rows.iter()) {
        if let Some(key) = on_values(row, &on) {
            by_key.entry(key).or_default().push(row);
        }
    }
    let renamed = |column: &str, other_side: &[String], side: &str| {
        if other_side.iter().any(|other| other == column) {
            format!("{column}_{side}")
        } else {
            column.to_owned()
        }
    };
    let mut rows = Vec::new();
    for left_row in left.tables.iter().flat_map(|table| table.rows.iter()) {
        let Some(key) = on_values(left_row, &on) else {
            continue;
        };
        for right_row in by_key.get(&key).map(Vec::as_slice).unwrap_or_default() {
            if rows.len() >= MOST_ROWS {
                return Err(format!("join makes more than {MOST_ROWS} rows"));
            }
            let mut row = Row::new();
            for (column, value) in left_row.iter() {
                if on.contains(column) {
                    row.insert(column.clone(), value.clone());
                } else {
                    row.insert(renamed(column, &right_columns, left_name), value.clone());
                }
            }
            for (column, value) in right_row.iter().filter(|(column, _)| !on.contains(column)) {
                row.insert(renamed(column, &left_columns, right_name), value.clone());
            }
            rows.push(row);
        }
    }
    let key_of = |stream: &Stream| stream.tables.first().map(|table| table.key.clone()).unwrap_or_default();
    let (left_key, right_key) = (key_of(left), key_of(right));
    let key: Vec<String> = on
        .iter()
        .filter(|column| left_key.contains(column) && right_key.contains(column))
        .cloned()
        .collect();
    let mut tables = regroup(vec![Table { key: Vec::new(), rows }], key);
    tables.retain(|table| !table.rows.is_empty());
    Ok(Stream {
        tables,
        start: left.start.min(right.start),
        stop: left.stop.max(right.stop),
    })
}

#[derive(Debug, Clone)]
enum Head {
    From,
    Variable(String),
    Join(Call),
}

#[derive(Debug, Clone)]
struct Statement {
    assign: Option<String>,
    head: Head,
    calls: Vec<Call>,
}

fn run_statement(session: &mut Session<'_>, statement: &Statement, streams: &BTreeMap<String, Stream>) -> Result<Applied, String> {
    match &statement.head {
        Head::From => execute(session, &statement.calls),
        Head::Variable(name) => {
            let stream = streams.get(name).cloned().ok_or_else(|| format!("undefined identifier {name}"))?;
            apply(stream, &statement.calls.iter().collect::<Vec<_>>())
        }
        Head::Join(call) => apply(joined(call, streams)?, &statement.calls.iter().collect::<Vec<_>>()),
    }
}

fn datatype(values: &[&Value]) -> &'static str {
    let found = values.iter().find(|value| !matches!(value, Value::Null));
    match found {
        Some(Value::TimestampTz(_) | Value::Timestamp(_)) => "dateTime:RFC3339",
        Some(Value::Double(_) | Value::Real(_)) => "double",
        Some(Value::BigInt(_) | Value::Integer(_) | Value::SmallInt(_)) => "long",
        Some(Value::Numeric(_)) => "unsignedLong",
        Some(Value::Boolean(_)) => "boolean",
        _ => "string",
    }
}

fn cell(value: Option<&Value>) -> String {
    let text = match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::TimestampTz(micros) | Value::Timestamp(micros)) => {
            crate::influxql::time_json(*micros, None, None).trim_matches('"').to_owned()
        }
        Some(Value::Double(number)) => format!("{number}"),
        Some(other) => other.to_text().unwrap_or_default(),
    };
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text
    }
}

fn annotated_csv(outcome: &Outcome) -> String {
    let mut out = String::new();
    let mut previous_schema: Option<(Vec<String>, Vec<&'static str>, Vec<bool>)> = None;
    for (index, table) in outcome.tables.iter().enumerate() {
        let mut columns: Vec<String> = LEADING_COLUMNS
            .iter()
            .filter(|column| table.rows.iter().any(|row| row.contains_key(**column)))
            .map(|column| (*column).to_owned())
            .collect();
        let mut rest: Vec<String> = table
            .rows
            .iter()
            .flat_map(|row| row.keys().cloned())
            .filter(|column| !LEADING_COLUMNS.contains(&column.as_str()))
            .collect();
        rest.sort();
        rest.dedup();
        columns.extend(rest);
        let types: Vec<&'static str> = columns
            .iter()
            .map(|column| {
                let values: Vec<&Value> = table.rows.iter().filter_map(|row| row.get(column)).collect();
                datatype(&values)
            })
            .collect();
        let grouped: Vec<bool> = columns.iter().map(|column| table.key.contains(column)).collect();
        let schema = (columns.clone(), types.clone(), grouped.clone());
        if previous_schema.as_ref() != Some(&schema) {
            if previous_schema.is_some() {
                out.push_str("\r\n");
            }
            out.push_str(&format!("#datatype,string,long,{}\r\n", types.join(",")));
            let groups: Vec<&str> = grouped.iter().map(|flag| if *flag { "true" } else { "false" }).collect();
            out.push_str(&format!("#group,false,false,{}\r\n", groups.join(",")));
            out.push_str(&format!("#default,{},,{}\r\n", cell(Some(&Value::Text(outcome.result.clone()))), ",".repeat(columns.len().saturating_sub(1))));
            out.push_str(&format!(",result,table,{}\r\n", columns.join(",")));
            previous_schema = Some(schema);
        }
        for row in &table.rows {
            let cells: Vec<String> = columns.iter().map(|column| cell(row.get(column))).collect();
            out.push_str(&format!(",,{index},{}\r\n", cells.join(",")));
        }
    }
    out.push_str("\r\n");
    out
}

/// The query text of a `/api/v2/query` body: the `query` member of a JSON body, or the whole
/// body when it is sent as `application/vnd.flux`.
pub(crate) fn query_text(body: &[u8], content_type: Option<&str>) -> Result<String, String> {
    let text = std::str::from_utf8(body).map_err(|_| "the body is not UTF-8".to_owned())?;
    if content_type.is_some_and(|kind| kind.starts_with("application/vnd.flux")) {
        return Ok(text.to_owned());
    }
    let document = eggdb_sql::json::parse_document(text).map_err(|error| error.message)?;
    match &document.value {
        eggdb_sql::json::Json::Object(members) => members
            .iter()
            .find(|(key, _)| key == "query")
            .and_then(|(_, value)| match &value.value {
                eggdb_sql::json::Json::String(query) => Some(query.clone()),
                _ => None,
            })
            .ok_or_else(|| "the body has no query".to_owned()),
        _ => Err("the body is not a JSON object".to_owned()),
    }
}

/// Runs a Flux query (a `from() |> range() |> ...` pipeline over the metrics tables) and
/// returns InfluxDB 2's annotated CSV.
pub(crate) fn answer(session: &mut Session<'_>, query: &str) -> Result<String, String> {
    let mut parser = Parser { tokens: tokens(query)?, at: 0 };
    let mut streams: BTreeMap<String, Stream> = BTreeMap::new();
    let mut outcomes = Vec::new();
    while let Some(statement) = parser.statement()? {
        let applied = run_statement(session, &statement, &streams)?;
        let shown = statement.assign.is_none() || applied.yielded;
        if shown {
            outcomes.push(Outcome {
                result: applied.result,
                tables: applied.stream.tables.clone(),
            });
        }
        if let Some(name) = statement.assign {
            streams.insert(name, applied.stream);
        }
    }
    if outcomes.is_empty() {
        return Err("the query yields no result".to_owned());
    }
    Ok(outcomes.iter().map(annotated_csv).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipelines_parse_and_filters_fold() {
        let mut parser = Parser {
            tokens: tokens(
                r#"from(bucket: "b") |> range(start: -1h) |> filter(fn: (r) => r._measurement == "cpu" and r["host"] =~ /^a/) |> aggregateWindow(every: 1m, fn: mean)"#,
            )
            .unwrap(),
            at: 0,
        };
        let calls = parser.pipeline().unwrap();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[1].argument("start"), Some(&Argument::Duration(-3_600_000_000)));
        let Some(Argument::Predicate { body, .. }) = calls[2].argument("fn") else {
            panic!("a predicate");
        };
        let cpu = Candidate { measurement: "cpu", field: "usage", tags: vec!["host"] };
        let Partial::Sql(sql) = partial(body, &cpu) else {
            panic!("depends on the row");
        };
        assert_eq!(sql, "(NULLIF(\"host\", '') ~ '^a')");
        let memory = Candidate { measurement: "mem", field: "used", tags: vec!["host"] };
        assert!(matches!(partial(body, &memory), Partial::Known(Expr::Boolean(false))));
        assert_eq!(duration_micros("1h30m"), Some(5_400_000_000));
    }

    #[test]
    fn windows_aggregate_and_fill_empty_ones() {
        let minute = 60_000_000;
        let start = -UNIX_EPOCH_FROM_2000_MICROS;
        let row = |at: i64, value: f64| {
            let mut row = Row::new();
            row.insert("_time".to_owned(), Value::TimestampTz(start + at));
            row.insert("_value".to_owned(), Value::Double(value));
            row
        };
        let table = Table {
            key: Vec::new(),
            rows: vec![row(0, 1.0), row(10_000_000, 3.0), row(2 * minute, 5.0)],
        };
        let call = Call {
            name: "aggregateWindow".to_owned(),
            arguments: vec![
                ("every".to_owned(), Argument::Duration(minute)),
                ("fn".to_owned(), Argument::Name("mean".to_owned())),
            ],
        };
        let out = aggregate_window(vec![table], &call, start, start + 3 * minute).unwrap();
        let values: Vec<Value> = out[0].rows.iter().map(|row| row["_value"].clone()).collect();
        assert_eq!(values, vec![Value::Double(2.0), Value::Null, Value::Double(5.0)]);
        assert_eq!(out[0].rows[0]["_time"], Value::TimestampTz(start + minute));
    }

    #[test]
    fn row_expressions_compute_and_records_extend_rows() {
        let mut parser = Parser {
            tokens: tokens(r#"map(fn: (r) => ({r with _value: r._value * 2.0 + 1.0, label: if r._value > 3 then "high" else "low"}))"#).unwrap(),
            at: 0,
        };
        let call = parser.call().unwrap();
        let Some(Argument::Predicate { body: Expr::Record { extends_row: true, fields }, .. }) = call.argument("fn") else {
            panic!("a record");
        };
        let mut row = Row::new();
        row.insert("_value".to_owned(), Value::Double(5.0));
        assert_eq!(evaluate(&fields[0].1, &row).unwrap(), Flux::Float(11.0));
        assert_eq!(evaluate(&fields[1].1, &row).unwrap(), Flux::Text("high".to_owned()));
    }

    #[test]
    fn programs_assign_streams_and_join_them() {
        let mut parser = Parser {
            tokens: tokens(
                r#"import "join"
                cpu = from(bucket: "b") |> range(start: -1h) |> filter(fn: (r) => r._measurement == "cpu")
                mem = from(bucket: "b") |> range(start: -1h)
                join(tables: {cpu: cpu, mem: mem}, on: ["_time", "host"]) |> yield(name: "both")"#,
            )
            .unwrap(),
            at: 0,
        };
        let first = parser.statement().unwrap().unwrap();
        assert_eq!(first.assign.as_deref(), Some("cpu"));
        assert!(matches!(first.head, Head::From));
        assert_eq!(first.calls.len(), 3);
        let second = parser.statement().unwrap().unwrap();
        assert_eq!(second.assign.as_deref(), Some("mem"));
        let third = parser.statement().unwrap().unwrap();
        let Head::Join(join) = &third.head else {
            panic!("a join");
        };
        assert!(parser.statement().unwrap().is_none());
        let row = |host: &str, value: f64| {
            let mut row = Row::new();
            row.insert("_time".to_owned(), Value::BigInt(1));
            row.insert("host".to_owned(), Value::Text(host.to_owned()));
            row.insert("_value".to_owned(), Value::Double(value));
            row
        };
        let stream = |rows: Vec<Row>| Stream {
            tables: vec![Table { key: vec!["host".to_owned()], rows }],
            start: 0,
            stop: 10,
        };
        let streams = BTreeMap::from([
            ("cpu".to_owned(), stream(vec![row("a", 0.5), row("b", 0.25)])),
            ("mem".to_owned(), stream(vec![row("a", 512.0)])),
        ]);
        let joined = joined(join, &streams).unwrap();
        assert_eq!(joined.tables.len(), 1);
        let only = &joined.tables[0].rows[0];
        assert_eq!(only["_value_cpu"], Value::Double(0.5));
        assert_eq!(only["_value_mem"], Value::Double(512.0));
        assert_eq!(joined.tables[0].key, vec!["host".to_owned()]);
        let applied = apply(joined, &third.calls.iter().collect::<Vec<_>>()).unwrap();
        assert!(applied.yielded);
        assert_eq!(applied.result, "both");
    }
}
