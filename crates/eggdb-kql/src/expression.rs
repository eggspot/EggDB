use crate::{KqlError, error};

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Name(String),
    Number(String),
    Timespan(String),
    Text(String),
    Raw(String),
    Symbol(String),
}

pub(crate) struct Tokens {
    items: Vec<Token>,
    at: usize,
}

const SYMBOLS: [&str; 17] = [
    "==", "!=", "<>", "<=", ">=", "=~", "!~", "..", "<", ">", "+", "-", "*", "/", "%", "(", ")",
];

fn timespan_unit(unit: &str) -> Option<&'static str> {
    Some(match unit {
        "d" => "days",
        "h" => "hours",
        "m" => "minutes",
        "s" => "seconds",
        "ms" => "milliseconds",
        "microsecond" | "microseconds" => "microseconds",
        _ => return None,
    })
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

impl Tokens {
    pub(crate) fn new(text: &str) -> Result<Self, KqlError> {
        let characters: Vec<char> = text.chars().collect();
        let mut items = Vec::new();
        let mut at = 0;
        while at < characters.len() {
            let character = characters[at];
            if character.is_whitespace() {
                at += 1;
            } else if character == ',' {
                items.push(Token::Symbol(",".to_owned()));
                at += 1;
            } else if character == '\'' || character == '"' {
                let mut value = String::new();
                at += 1;
                loop {
                    let next = *characters
                        .get(at)
                        .ok_or_else(|| error("a string is not closed"))?;
                    at += 1;
                    if next == character {
                        break;
                    }
                    if next == '\\' {
                        let escaped = *characters
                            .get(at)
                            .ok_or_else(|| error("a string is not closed"))?;
                        at += 1;
                        value.push(match escaped {
                            'n' => '\n',
                            't' => '\t',
                            other => other,
                        });
                    } else {
                        value.push(next);
                    }
                }
                items.push(Token::Text(value));
            } else if character.is_ascii_digit() {
                let start = at;
                while at < characters.len()
                    && (characters[at].is_ascii_digit()
                        || (characters[at] == '.'
                            && characters.get(at + 1).is_some_and(char::is_ascii_digit)))
                {
                    at += 1;
                }
                let number: String = characters[start..at].iter().collect();
                let unit_start = at;
                while at < characters.len() && characters[at].is_ascii_alphabetic() {
                    at += 1;
                }
                let unit: String = characters[unit_start..at].iter().collect();
                if unit.is_empty() {
                    items.push(Token::Number(number));
                } else {
                    let name = timespan_unit(&unit).ok_or_else(|| {
                        error(format!("{number}{unit} is not a number or timespan"))
                    })?;
                    items.push(Token::Timespan(format!("{number} {name}")));
                }
            } else if character.is_alphabetic() || character == '_' {
                let start = at;
                while at < characters.len()
                    && (characters[at].is_alphanumeric() || characters[at] == '_')
                {
                    at += 1;
                }
                let name: String = characters[start..at].iter().collect();
                if matches!(
                    name.to_ascii_lowercase().as_str(),
                    "datetime" | "todatetime"
                ) && characters.get(at) == Some(&'(')
                {
                    let end = characters[at..]
                        .iter()
                        .position(|character| *character == ')')
                        .map(|offset| at + offset)
                        .ok_or_else(|| error("datetime( is not closed"))?;
                    let inner: String = characters[at + 1..end].iter().collect();
                    items.push(Token::Raw(format!(
                        "{}::timestamptz",
                        literal(inner.trim().trim_matches('"').trim_matches('\''))
                    )));
                    at = end + 1;
                } else {
                    items.push(Token::Name(name));
                }
            } else if character == '!'
                && characters
                    .get(at + 1)
                    .is_some_and(|next| next.is_alphabetic())
            {
                let start = at + 1;
                at = start;
                while at < characters.len() && characters[at].is_alphabetic() {
                    at += 1;
                }
                let word: String = characters[start..at].iter().collect();
                items.push(Token::Name(format!("!{word}")));
            } else {
                let rest: String = characters[at..characters.len().min(at + 2)]
                    .iter()
                    .collect();
                let symbol = SYMBOLS
                    .iter()
                    .find(|symbol| rest.starts_with(**symbol))
                    .ok_or_else(|| error(format!("unexpected character {character}")))?;
                items.push(Token::Symbol((*symbol).to_owned()));
                at += symbol.chars().count();
            }
        }
        Ok(Self { items, at: 0 })
    }

    fn peek(&self) -> Option<&Token> {
        self.items.get(self.at)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.items.get(self.at).cloned();
        self.at += 1;
        token
    }

    fn word_is(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Name(name)) if name.eq_ignore_ascii_case(word))
    }

    fn symbol_is(&self, symbol: &str) -> bool {
        matches!(self.peek(), Some(Token::Symbol(found)) if found == symbol)
    }

    fn expect(&mut self, symbol: &str) -> Result<(), KqlError> {
        if self.symbol_is(symbol) {
            self.at += 1;
            Ok(())
        } else {
            Err(error(format!("expected {symbol}")))
        }
    }
}

fn or(tokens: &mut Tokens, depth: usize) -> Result<String, KqlError> {
    if depth > 64 {
        return Err(error("the expression is nested too deeply"));
    }
    let mut left = and(tokens, depth)?;
    while tokens.word_is("or") {
        tokens.at += 1;
        left = format!("({left} OR {})", and(tokens, depth)?);
    }
    Ok(left)
}

fn and(tokens: &mut Tokens, depth: usize) -> Result<String, KqlError> {
    let mut left = comparison(tokens, depth)?;
    while tokens.word_is("and") {
        tokens.at += 1;
        left = format!("({left} AND {})", comparison(tokens, depth)?);
    }
    Ok(left)
}

fn pattern(tokens: &mut Tokens, depth: usize) -> Result<String, KqlError> {
    match tokens.next() {
        Some(Token::Text(text)) => Ok(text
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")),
        _ => {
            tokens.at -= 1;
            let _ = additive(tokens, depth)?;
            Err(error(
                "contains, startswith, endswith and has take a string",
            ))
        }
    }
}

fn list(tokens: &mut Tokens, depth: usize) -> Result<Vec<String>, KqlError> {
    tokens.expect("(")?;
    let mut items = Vec::new();
    while !tokens.symbol_is(")") {
        items.push(additive(tokens, depth + 1)?);
        if tokens.symbol_is(",") {
            tokens.at += 1;
        } else {
            break;
        }
    }
    tokens.expect(")")?;
    Ok(items)
}

fn comparison(tokens: &mut Tokens, depth: usize) -> Result<String, KqlError> {
    let left = additive(tokens, depth)?;
    let operator = match tokens.peek() {
        Some(Token::Symbol(symbol))
            if matches!(
                symbol.as_str(),
                "==" | "!=" | "<>" | "<" | "<=" | ">" | ">=" | "=~" | "!~"
            ) =>
        {
            symbol.clone()
        }
        Some(Token::Name(name)) => name.to_ascii_lowercase(),
        _ => return Ok(left),
    };
    let like = |negated: bool, value: String| {
        format!(
            "{left} {}ILIKE {}",
            if negated { "NOT " } else { "" },
            literal(&value)
        )
    };
    Ok(match operator.as_str() {
        "==" => {
            tokens.at += 1;
            format!("{left} = {}", additive(tokens, depth)?)
        }
        "!=" | "<>" => {
            tokens.at += 1;
            format!("{left} <> {}", additive(tokens, depth)?)
        }
        "<" | "<=" | ">" | ">=" => {
            tokens.at += 1;
            format!("{left} {operator} {}", additive(tokens, depth)?)
        }
        "=~" | "!~" => {
            tokens.at += 1;
            format!(
                "lower({left}) {} lower({})",
                if operator == "=~" { "=" } else { "<>" },
                additive(tokens, depth)?
            )
        }
        "contains" | "!contains" | "has" | "!has" => {
            tokens.at += 1;
            let value = pattern(tokens, depth)?;
            like(operator.starts_with('!'), format!("%{value}%"))
        }
        "startswith" | "!startswith" => {
            tokens.at += 1;
            let value = pattern(tokens, depth)?;
            like(operator.starts_with('!'), format!("{value}%"))
        }
        "endswith" | "!endswith" => {
            tokens.at += 1;
            let value = pattern(tokens, depth)?;
            like(operator.starts_with('!'), format!("%{value}"))
        }
        "in" | "!in" => {
            tokens.at += 1;
            let items = list(tokens, depth)?;
            if items.is_empty() {
                return Ok(if operator == "in" {
                    "FALSE".to_owned()
                } else {
                    "TRUE".to_owned()
                });
            }
            format!(
                "{left} {}IN ({})",
                if operator == "!in" { "NOT " } else { "" },
                items.join(", ")
            )
        }
        "between" | "!between" => {
            tokens.at += 1;
            tokens.expect("(")?;
            let low = additive(tokens, depth)?;
            tokens.expect("..")?;
            let high = additive(tokens, depth)?;
            tokens.expect(")")?;
            format!(
                "{left} {}BETWEEN {low} AND {high}",
                if operator == "!between" { "NOT " } else { "" }
            )
        }
        _ => left,
    })
}

fn additive(tokens: &mut Tokens, depth: usize) -> Result<String, KqlError> {
    let mut left = multiplicative(tokens, depth)?;
    while let Some(Token::Symbol(symbol)) = tokens.peek() {
        if symbol != "+" && symbol != "-" {
            break;
        }
        let symbol = symbol.clone();
        tokens.at += 1;
        left = format!("({left} {symbol} {})", multiplicative(tokens, depth)?);
    }
    Ok(left)
}

fn multiplicative(tokens: &mut Tokens, depth: usize) -> Result<String, KqlError> {
    let mut left = primary(tokens, depth)?;
    while let Some(Token::Symbol(symbol)) = tokens.peek() {
        if !matches!(symbol.as_str(), "*" | "/" | "%") {
            break;
        }
        let symbol = symbol.clone();
        tokens.at += 1;
        left = format!("({left} {symbol} {})", primary(tokens, depth)?);
    }
    Ok(left)
}

fn arguments(tokens: &mut Tokens, depth: usize) -> Result<Vec<String>, KqlError> {
    tokens.expect("(")?;
    let mut items = Vec::new();
    while !tokens.symbol_is(")") {
        items.push(or(tokens, depth + 1)?);
        if tokens.symbol_is(",") {
            tokens.at += 1;
        } else {
            break;
        }
    }
    tokens.expect(")")?;
    Ok(items)
}

fn call(name: &str, arguments: &[String]) -> Result<String, KqlError> {
    let one = || {
        arguments
            .first()
            .cloned()
            .ok_or_else(|| error(format!("{name} takes an argument")))
    };
    Ok(
        match (name.to_ascii_lowercase().as_str(), arguments.len()) {
            ("now", 0) => "now()".to_owned(),
            ("ago", 1) => format!("(now() - {})", one()?),
            ("bin" | "floor", 2) => {
                if arguments[1].starts_with("interval ") {
                    format!("time_bucket({}, {})", arguments[1], arguments[0])
                } else {
                    format!(
                        "(floor({} / {}) * {})",
                        arguments[0], arguments[1], arguments[1]
                    )
                }
            }
            ("isnull", 1) => format!("({} IS NULL)", one()?),
            ("isnotnull", 1) => format!("({} IS NOT NULL)", one()?),
            ("isempty", 1) => format!("({0} IS NULL OR {0} = '')", one()?),
            ("isnotempty", 1) => format!("({0} IS NOT NULL AND {0} <> '')", one()?),
            ("not", 1) => format!("(NOT {})", one()?),
            ("strlen", 1) => format!("length({})", one()?),
            ("tolower", 1) => format!("lower({})", one()?),
            ("toupper", 1) => format!("upper({})", one()?),
            ("tostring", 1) => format!("CAST({} AS text)", one()?),
            ("toint", 1) => format!("CAST({} AS integer)", one()?),
            ("tolong", 1) => format!("CAST({} AS bigint)", one()?),
            ("todouble" | "toreal", 1) => format!("CAST({} AS double precision)", one()?),
            ("iff" | "iif", 3) => format!(
                "(CASE WHEN {} THEN {} ELSE {} END)",
                arguments[0], arguments[1], arguments[2]
            ),
            ("coalesce", count) if count > 0 => format!("coalesce({})", arguments.join(", ")),
            ("strcat", count) if count > 0 => format!("concat({})", arguments.join(", ")),
            ("substring", 2) => format!("substring({} FROM ({}) + 1)", arguments[0], arguments[1]),
            ("substring", 3) => format!(
                "substring({} FROM ({}) + 1 FOR {})",
                arguments[0], arguments[1], arguments[2]
            ),
            ("abs" | "round" | "sqrt" | "exp" | "log10", _) => {
                format!("{}({})", name.to_ascii_lowercase(), arguments.join(", "))
            }
            (other, count) => {
                return Err(error(format!(
                    "the function {other} with {count} arguments is not supported yet"
                )));
            }
        },
    )
}

fn primary(tokens: &mut Tokens, depth: usize) -> Result<String, KqlError> {
    if depth > 64 {
        return Err(error("the expression is nested too deeply"));
    }
    match tokens.next() {
        Some(Token::Number(number)) => Ok(number),
        Some(Token::Timespan(span)) => Ok(format!("interval {}", literal(&span))),
        Some(Token::Text(text)) => Ok(literal(&text)),
        Some(Token::Raw(raw)) => Ok(raw),
        Some(Token::Symbol(symbol)) if symbol == "-" => {
            Ok(format!("(-{})", primary(tokens, depth + 1)?))
        }
        Some(Token::Symbol(symbol)) if symbol == "(" => {
            let inner = or(tokens, depth + 1)?;
            tokens.expect(")")?;
            Ok(format!("({inner})"))
        }
        Some(Token::Name(name)) => {
            match name.to_ascii_lowercase().as_str() {
                "true" => return Ok("TRUE".to_owned()),
                "false" => return Ok("FALSE".to_owned()),
                "null" => return Ok("NULL".to_owned()),
                _ => {}
            }
            if tokens.symbol_is("(") {
                let arguments = arguments(tokens, depth)?;
                return call(&name, &arguments);
            }
            if name.starts_with('!') {
                return Err(error(format!("{name} is not a value")));
            }
            Ok(format!("\"{name}\""))
        }
        _ => Err(error(
            "an expression ends too early or has an unexpected symbol",
        )),
    }
}

/// The SQL of one KQL expression; every token must be used.
pub(crate) fn translate_expression(tokens: &mut Tokens) -> Result<String, KqlError> {
    let sql = or(tokens, 0)?;
    if tokens.at < tokens.items.len() {
        return Err(error("unexpected text after the expression"));
    }
    Ok(sql)
}
