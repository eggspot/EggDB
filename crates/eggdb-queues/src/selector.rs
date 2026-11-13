//! JMS message selectors, shared by every messaging protocol that filters by them: STOMP's
//! `selector` header and AMQP 1.0's `jms-selector` source filter.

const MAX_SELECTOR_BYTES: usize = 4096;
const MAX_DEPTH: usize = 32;

/// A value a selector reads: a literal, a message property or a message header.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectorValue {
    /// Absent, or SQL's `NULL`.
    Null,
    /// `TRUE` or `FALSE`.
    Boolean(bool),
    /// A number; exact and approximate numerics are compared by value.
    Number(f64),
    /// A string.
    Text(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Operand {
    Literal(SelectorValue),
    Header(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Comparison {
    Equal,
    NotEqual,
    Less,
    Greater,
    LessOrEqual,
    GreaterOrEqual,
}

#[derive(Debug, Clone, PartialEq)]
enum Condition {
    And(Box<Condition>, Box<Condition>),
    Or(Box<Condition>, Box<Condition>),
    Not(Box<Condition>),
    Compare(Operand, Comparison, Operand),
    Like {
        operand: Operand,
        pattern: Vec<char>,
        escape: Option<char>,
        negated: bool,
    },
    In {
        operand: Operand,
        listed: Vec<String>,
        negated: bool,
    },
    IsNull {
        operand: Operand,
        negated: bool,
    },
    Between {
        operand: Operand,
        low: Operand,
        high: Operand,
        negated: bool,
    },
    Value(Operand),
}

/// A JMS message selector: the subset of SQL-92 that JMS defines over a message's headers and
/// properties, as STOMP's `selector` header and AMQP 1.0's `jms-selector` filter carry it.
#[derive(Debug, Clone, PartialEq)]
pub struct Selector {
    condition: Condition,
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    Text(String),
    Number(f64),
    Symbol(&'static str),
    Open,
    Close,
    Comma,
}

fn tokens(text: &str) -> Result<Vec<Token>, String> {
    let characters: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut at = 0;
    while at < characters.len() {
        let character = characters[at];
        if character.is_whitespace() {
            at += 1;
        } else if character == '\'' {
            let mut value = String::new();
            at += 1;
            loop {
                match characters.get(at) {
                    None => return Err("a string in the selector is not closed".to_owned()),
                    Some('\'') if characters.get(at + 1) == Some(&'\'') => {
                        value.push('\'');
                        at += 2;
                    }
                    Some('\'') => {
                        at += 1;
                        break;
                    }
                    Some(other) => {
                        value.push(*other);
                        at += 1;
                    }
                }
            }
            out.push(Token::Text(value));
        } else if character.is_ascii_digit()
            || (character == '.' && characters.get(at + 1).is_some_and(char::is_ascii_digit))
            || (character == '-'
                && characters.get(at + 1).is_some_and(char::is_ascii_digit)
                && !matches!(out.last(), Some(Token::Word(_) | Token::Number(_) | Token::Text(_) | Token::Close)))
        {
            let start = at;
            at += 1;
            while characters
                .get(at)
                .is_some_and(|next| next.is_ascii_alphanumeric() || *next == '.')
            {
                at += 1;
            }
            let written: String = characters[start..at].iter().collect();
            let trimmed = written.trim_end_matches(['L', 'l', 'F', 'f', 'D', 'd']);
            out.push(Token::Number(
                trimmed
                    .parse()
                    .map_err(|_| format!("{written} is not a number"))?,
            ));
        } else if character.is_alphabetic() || character == '_' || character == '$' {
            let start = at;
            while characters.get(at).is_some_and(|next| {
                next.is_alphanumeric() || matches!(next, '_' | '$' | '-' | '.' | ':')
            }) {
                at += 1;
            }
            out.push(Token::Word(characters[start..at].iter().collect()));
        } else {
            let pair: String = characters[at..(at + 2).min(characters.len())].iter().collect();
            let (token, width) = match (character, pair.as_str()) {
                (_, "<>") => (Token::Symbol("<>"), 2),
                (_, "<=") => (Token::Symbol("<="), 2),
                (_, ">=") => (Token::Symbol(">="), 2),
                ('=', _) => (Token::Symbol("="), 1),
                ('<', _) => (Token::Symbol("<"), 1),
                ('>', _) => (Token::Symbol(">"), 1),
                ('(', _) => (Token::Open, 1),
                (')', _) => (Token::Close, 1),
                (',', _) => (Token::Comma, 1),
                (other, _) => return Err(format!("the selector has an unexpected {other}")),
            };
            out.push(token);
            at += width;
        }
    }
    Ok(out)
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

    fn keyword(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Word(found)) if found.eq_ignore_ascii_case(word))
    }

    fn keyword_at(&self, offset: usize, word: &str) -> bool {
        matches!(self.tokens.get(self.at + offset), Some(Token::Word(found)) if found.eq_ignore_ascii_case(word))
    }

    fn take_keyword(&mut self, word: &str) -> bool {
        let found = self.keyword(word);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect_keyword(&mut self, word: &str) -> Result<(), String> {
        if self.take_keyword(word) {
            Ok(())
        } else {
            Err(format!("the selector needs {word} here"))
        }
    }

    fn deeper(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("the selector nests too deeply".to_owned());
        }
        Ok(())
    }

    fn or(&mut self) -> Result<Condition, String> {
        let mut left = self.and()?;
        while self.take_keyword("or") {
            left = Condition::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Condition, String> {
        let mut left = self.not()?;
        while self.take_keyword("and") {
            left = Condition::And(Box::new(left), Box::new(self.not()?));
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Condition, String> {
        if self.take_keyword("not") {
            self.deeper()?;
            let inner = self.not()?;
            self.depth -= 1;
            return Ok(Condition::Not(Box::new(inner)));
        }
        self.comparison()
    }

    fn operand(&mut self) -> Result<Operand, String> {
        let token = self.peek().cloned().ok_or("the selector ends too soon")?;
        self.at += 1;
        Ok(match token {
            Token::Text(text) => Operand::Literal(SelectorValue::Text(text)),
            Token::Number(number) => Operand::Literal(SelectorValue::Number(number)),
            Token::Word(word) if word.eq_ignore_ascii_case("true") => {
                Operand::Literal(SelectorValue::Boolean(true))
            }
            Token::Word(word) if word.eq_ignore_ascii_case("false") => {
                Operand::Literal(SelectorValue::Boolean(false))
            }
            Token::Word(word) if word.eq_ignore_ascii_case("null") => Operand::Literal(SelectorValue::Null),
            Token::Word(word) => Operand::Header(word),
            other => return Err(format!("the selector has an unexpected {other:?}")),
        })
    }

    fn text(&mut self) -> Result<String, String> {
        match self.peek().cloned() {
            Some(Token::Text(text)) => {
                self.at += 1;
                Ok(text)
            }
            _ => Err("the selector needs a string here".to_owned()),
        }
    }

    fn comparison(&mut self) -> Result<Condition, String> {
        if self.peek() == Some(&Token::Open) {
            self.at += 1;
            self.deeper()?;
            let inner = self.or()?;
            self.depth -= 1;
            if self.peek() != Some(&Token::Close) {
                return Err("the selector does not close a parenthesis".to_owned());
            }
            self.at += 1;
            return Ok(inner);
        }
        let operand = self.operand()?;
        let negated = self.keyword("not")
            && (self.keyword_at(1, "like") || self.keyword_at(1, "in") || self.keyword_at(1, "between"));
        if negated {
            self.at += 1;
        }
        if self.take_keyword("like") {
            let pattern = self.text()?.chars().collect();
            let escape = if self.take_keyword("escape") {
                let written = self.text()?;
                let mut characters = written.chars();
                match (characters.next(), characters.next()) {
                    (Some(escape), None) => Some(escape),
                    _ => return Err("ESCAPE takes one character".to_owned()),
                }
            } else {
                None
            };
            return Ok(Condition::Like {
                operand,
                pattern,
                escape,
                negated,
            });
        }
        if self.take_keyword("in") {
            if self.peek() != Some(&Token::Open) {
                return Err("IN needs a parenthesized list".to_owned());
            }
            self.at += 1;
            let mut listed = vec![self.text()?];
            while self.peek() == Some(&Token::Comma) {
                self.at += 1;
                listed.push(self.text()?);
            }
            if self.peek() != Some(&Token::Close) {
                return Err("IN's list is not closed".to_owned());
            }
            self.at += 1;
            return Ok(Condition::In {
                operand,
                listed,
                negated,
            });
        }
        if self.take_keyword("between") {
            let low = self.operand()?;
            self.expect_keyword("and")?;
            let high = self.operand()?;
            return Ok(Condition::Between {
                operand,
                low,
                high,
                negated,
            });
        }
        if self.take_keyword("is") {
            let negated = self.take_keyword("not");
            self.expect_keyword("null")?;
            return Ok(Condition::IsNull { operand, negated });
        }
        let comparison = match self.peek() {
            Some(Token::Symbol("=")) => Comparison::Equal,
            Some(Token::Symbol("<>")) => Comparison::NotEqual,
            Some(Token::Symbol("<")) => Comparison::Less,
            Some(Token::Symbol(">")) => Comparison::Greater,
            Some(Token::Symbol("<=")) => Comparison::LessOrEqual,
            Some(Token::Symbol(">=")) => Comparison::GreaterOrEqual,
            _ => return Ok(Condition::Value(operand)),
        };
        self.at += 1;
        Ok(Condition::Compare(operand, comparison, self.operand()?))
    }
}

type Lookup<'l> = &'l dyn Fn(&str) -> SelectorValue;

fn value_of(operand: &Operand, headers: Lookup<'_>) -> SelectorValue {
    match operand {
        Operand::Literal(literal) => literal.clone(),
        Operand::Header(name) => headers(name),
    }
}

fn number_of(literal: &SelectorValue) -> Option<f64> {
    match literal {
        SelectorValue::Number(number) => Some(*number),
        SelectorValue::Text(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn text_of(literal: &SelectorValue) -> Option<String> {
    match literal {
        SelectorValue::Text(text) => Some(text.clone()),
        SelectorValue::Boolean(flag) => Some(flag.to_string()),
        SelectorValue::Number(number) => Some(number.to_string()),
        SelectorValue::Null => None,
    }
}

fn compared(left: &SelectorValue, right: &SelectorValue) -> Option<std::cmp::Ordering> {
    if matches!(left, SelectorValue::Null) || matches!(right, SelectorValue::Null) {
        return None;
    }
    let numeric = matches!(left, SelectorValue::Number(_)) || matches!(right, SelectorValue::Number(_));
    if numeric {
        return number_of(left)?.partial_cmp(&number_of(right)?);
    }
    match (left, right) {
        (SelectorValue::Boolean(a), SelectorValue::Boolean(b)) => Some(a.cmp(b)),
        (SelectorValue::Boolean(flag), SelectorValue::Text(text)) | (SelectorValue::Text(text), SelectorValue::Boolean(flag)) => {
            text.eq_ignore_ascii_case(&flag.to_string()).then_some(std::cmp::Ordering::Equal)
        }
        _ => Some(text_of(left)?.cmp(&text_of(right)?)),
    }
}

fn like(text: &[char], pattern: &[char], escape: Option<char>) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((&first, rest)) if Some(first) == escape => match rest.split_first() {
            Some((&literal, after)) => {
                text.first() == Some(&literal) && like(&text[1..], after, escape)
            }
            None => false,
        },
        Some(('%', rest)) => (0..=text.len()).any(|skip| like(&text[skip..], rest, escape)),
        Some(('_', rest)) => !text.is_empty() && like(&text[1..], rest, escape),
        Some((&literal, rest)) => text.first() == Some(&literal) && like(&text[1..], rest, escape),
    }
}

fn negate(value: Option<bool>, negated: bool) -> Option<bool> {
    value.map(|value| value != negated)
}

fn evaluate(condition: &Condition, headers: Lookup<'_>) -> Option<bool> {
    match condition {
        Condition::And(left, right) => match (evaluate(left, headers), evaluate(right, headers)) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        },
        Condition::Or(left, right) => match (evaluate(left, headers), evaluate(right, headers)) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        },
        Condition::Not(inner) => evaluate(inner, headers).map(|value| !value),
        Condition::Compare(left, comparison, right) => {
            let ordering = compared(&value_of(left, headers), &value_of(right, headers));
            let equal_kind = matches!(comparison, Comparison::Equal | Comparison::NotEqual);
            let ordering = match ordering {
                None if equal_kind
                    && !matches!(value_of(left, headers), SelectorValue::Null)
                    && !matches!(value_of(right, headers), SelectorValue::Null) =>
                {
                    return Some(*comparison == Comparison::NotEqual);
                }
                other => other?,
            };
            Some(match comparison {
                Comparison::Equal => ordering.is_eq(),
                Comparison::NotEqual => !ordering.is_eq(),
                Comparison::Less => ordering.is_lt(),
                Comparison::Greater => ordering.is_gt(),
                Comparison::LessOrEqual => ordering.is_le(),
                Comparison::GreaterOrEqual => ordering.is_ge(),
            })
        }
        Condition::Like {
            operand,
            pattern,
            escape,
            negated,
        } => {
            let text: Vec<char> = text_of(&value_of(operand, headers))?.chars().collect();
            negate(Some(like(&text, pattern, *escape)), *negated)
        }
        Condition::In {
            operand,
            listed,
            negated,
        } => {
            let text = text_of(&value_of(operand, headers))?;
            negate(Some(listed.contains(&text)), *negated)
        }
        Condition::IsNull { operand, negated } => Some(
            matches!(value_of(operand, headers), SelectorValue::Null) != *negated,
        ),
        Condition::Between {
            operand,
            low,
            high,
            negated,
        } => {
            let value = value_of(operand, headers);
            let above = compared(&value, &value_of(low, headers))?.is_ge();
            let below = compared(&value, &value_of(high, headers))?.is_le();
            negate(Some(above && below), *negated)
        }
        Condition::Value(operand) => match value_of(operand, headers) {
            SelectorValue::Boolean(flag) => Some(flag),
            SelectorValue::Text(text) if text.eq_ignore_ascii_case("true") => Some(true),
            SelectorValue::Text(text) if text.eq_ignore_ascii_case("false") => Some(false),
            _ => None,
        },
    }
}

impl Selector {
    /// The selector written as `text`, or why it cannot be read.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_SELECTOR_BYTES {
            return Err(format!("a selector takes at most {MAX_SELECTOR_BYTES} bytes"));
        }
        let mut parser = Parser {
            tokens: tokens(text)?,
            at: 0,
            depth: 0,
        };
        let condition = parser.or()?;
        if parser.at != parser.tokens.len() {
            return Err("the selector has more after its condition".to_owned());
        }
        Ok(Self { condition })
    }

    /// Whether a message is chosen, `value` giving each header or property it names
    /// ([`SelectorValue::Null`] when it has none): only when the condition is true, not false or
    /// unknown.
    pub fn chooses(&self, value: &dyn Fn(&str) -> SelectorValue) -> bool {
        evaluate(&self.condition, value) == Some(true)
    }

    /// Whether a message with the text `headers` is chosen, each header's value read as text.
    pub fn chooses_headers(&self, headers: &[(String, String)]) -> bool {
        self.chooses(&|name| {
            headers
                .iter()
                .find(|(header, _)| header == name)
                .map_or(SelectorValue::Null, |(_, value)| SelectorValue::Text(value.clone()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn selectors_follow_sql92_with_unknown_as_not_chosen() {
        let order = headers(&[("type", "order"), ("amount", "250"), ("region", "eu-west")]);
        let chosen = |text: &str| Selector::parse(text).unwrap().chooses_headers(&order);
        assert!(chosen("type = 'order' AND amount > 100"));
        assert!(!chosen("amount BETWEEN 1 AND 100"));
        assert!(chosen("region LIKE 'eu-%'"));
        assert!(chosen("type IN ('order', 'refund') AND missing IS NULL"));
        assert!(!chosen("missing = 'x'"));
        assert!(!chosen("NOT (missing = 'x')"));
        assert!(chosen("missing = 'x' OR amount <> 1"));
        assert!(Selector::parse("amount >").is_err());
        assert!(Selector::parse("type = 'open").is_err());
    }

    #[test]
    fn typed_values_compare_as_numbers_and_booleans() {
        let selector = Selector::parse("JMSPriority > 4 AND urgent AND price BETWEEN 9.5 AND 10").unwrap();
        let typed = |name: &str| match name {
            "JMSPriority" => SelectorValue::Number(7.0),
            "urgent" => SelectorValue::Boolean(true),
            "price" => SelectorValue::Number(9.75),
            _ => SelectorValue::Null,
        };
        assert!(selector.chooses(&typed));
        assert!(!selector.chooses(&|_| SelectorValue::Null));
    }
}
