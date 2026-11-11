use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;

pub(crate) const CONTAINS_FUNCTION: &str = "sqlserver_contains";
pub(crate) const FREETEXT_FUNCTION: &str = "sqlserver_freetext";

const NOISE_WORDS: [&str; 20] = [
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "to", "with",
];

fn words(text: &str) -> Vec<String> {
    text.split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
enum Condition {
    Term { words: Vec<String>, prefix: bool },
    And(Box<Condition>, Box<Condition>),
    AndNot(Box<Condition>, Box<Condition>),
    Or(Box<Condition>, Box<Condition>),
    All(Vec<Condition>),
    Any(Vec<Condition>),
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Quoted(String),
    Word(String),
    Open,
    Close,
    Comma,
    And,
    Or,
    Not,
    Near,
}

fn tokens(text: &str) -> Result<Vec<Token>, String> {
    let mut found = Vec::new();
    let mut characters = text.chars().peekable();
    while let Some(&character) = characters.peek() {
        match character {
            ' ' | '\t' | '\r' | '\n' => {
                characters.next();
            }
            '"' => {
                characters.next();
                let mut quoted = String::new();
                loop {
                    match characters.next() {
                        Some('"') => break,
                        Some(inner) => quoted.push(inner),
                        None => return Err("a quoted term is not closed".to_owned()),
                    }
                }
                found.push(Token::Quoted(quoted));
            }
            '(' | ')' | ',' | '&' | '|' | '!' | '~' => {
                characters.next();
                found.push(match character {
                    '(' => Token::Open,
                    ')' => Token::Close,
                    ',' => Token::Comma,
                    '&' => Token::And,
                    '|' => Token::Or,
                    '!' => Token::Not,
                    _ => Token::Near,
                });
            }
            _ => {
                let mut word = String::new();
                while let Some(&next) = characters.peek() {
                    if next.is_whitespace() || matches!(next, '"' | '(' | ')' | ',' | '&' | '|' | '!' | '~') {
                        break;
                    }
                    word.push(next);
                    characters.next();
                }
                found.push(match word.to_ascii_uppercase().as_str() {
                    "AND" => Token::And,
                    "OR" => Token::Or,
                    "NOT" => Token::Not,
                    "NEAR" => Token::Near,
                    _ => Token::Word(word),
                });
            }
        }
    }
    Ok(found)
}

struct Reader {
    tokens: Vec<Token>,
    at: usize,
}

impl Reader {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at).cloned();
        self.at += 1;
        token
    }

    fn expect(&mut self, wanted: &Token) -> Result<(), String> {
        match self.next() {
            Some(token) if token == *wanted => Ok(()),
            other => Err(format!("expected {wanted:?}, found {other:?}")),
        }
    }

    fn any(&mut self) -> Result<Condition, String> {
        let mut left = self.all()?;
        while self.peek() == Some(&Token::Or) {
            self.at += 1;
            left = Condition::Or(Box::new(left), Box::new(self.all()?));
        }
        Ok(left)
    }

    fn all(&mut self) -> Result<Condition, String> {
        let mut left = self.unary()?;
        loop {
            match self.peek().cloned() {
                Some(Token::And) => {
                    self.at += 1;
                    if self.peek() == Some(&Token::Not) {
                        self.at += 1;
                        left = Condition::AndNot(Box::new(left), Box::new(self.unary()?));
                    } else {
                        left = Condition::And(Box::new(left), Box::new(self.unary()?));
                    }
                }
                Some(Token::Near) => {
                    self.at += 1;
                    left = Condition::And(Box::new(left), Box::new(self.unary()?));
                }
                _ => return Ok(left),
            }
        }
    }

    fn list(&mut self) -> Result<Vec<Condition>, String> {
        self.expect(&Token::Open)?;
        let mut items = Vec::new();
        loop {
            match self.peek().cloned() {
                Some(Token::Close) => {
                    self.at += 1;
                    return Ok(items);
                }
                Some(Token::Comma) => self.at += 1,
                Some(Token::Open) => items.push(Condition::All(self.list()?)),
                Some(Token::Word(word)) if word.chars().all(|character| character.is_ascii_digit()) => self.at += 1,
                Some(Token::Word(word)) if matches!(word.to_ascii_uppercase().as_str(), "TRUE" | "FALSE" | "MAX" | "INFLECTIONAL" | "THESAURUS") => {
                    self.at += 1;
                }
                Some(_) => items.push(self.unary()?),
                None => return Err("a list is not closed".to_owned()),
            }
        }
    }

    fn unary(&mut self) -> Result<Condition, String> {
        match self.next() {
            Some(Token::Open) => {
                let inner = self.any()?;
                self.expect(&Token::Close)?;
                Ok(inner)
            }
            Some(Token::Near) => {
                let items = self.list()?;
                Ok(Condition::All(items.into_iter().flat_map(|item| match item {
                    Condition::All(inner) => inner,
                    other => vec![other],
                }).collect()))
            }
            Some(Token::Quoted(text)) => {
                let prefix = text.trim_end().ends_with('*');
                Ok(Condition::Term {
                    words: words(text.trim_end().trim_end_matches('*')),
                    prefix,
                })
            }
            Some(Token::Word(word)) => match word.to_ascii_uppercase().as_str() {
                "FORMSOF" => Ok(Condition::Any(self.list()?)),
                "ISABOUT" => Ok(Condition::Any(self.list()?)),
                "WEIGHT" => {
                    self.list()?;
                    self.unary()
                }
                _ => Ok(Condition::Term {
                    prefix: word.ends_with('*'),
                    words: words(word.trim_end_matches('*')),
                }),
            },
            other => Err(format!("unexpected {other:?}")),
        }
    }
}

fn found(document: &[String], phrase: &[String], prefix: bool) -> bool {
    if phrase.is_empty() || document.len() < phrase.len() {
        return false;
    }
    document.windows(phrase.len()).any(|window| {
        window.iter().zip(phrase).enumerate().all(|(index, (word, wanted))| {
            if prefix && index + 1 == phrase.len() {
                word.starts_with(wanted.as_str())
            } else {
                word == wanted
            }
        })
    })
}

fn holds(document: &[String], condition: &Condition) -> bool {
    match condition {
        Condition::Term { words, prefix } => found(document, words, *prefix),
        Condition::And(left, right) => holds(document, left) && holds(document, right),
        Condition::AndNot(left, right) => holds(document, left) && !holds(document, right),
        Condition::Or(left, right) => holds(document, left) || holds(document, right),
        Condition::All(items) => items.iter().all(|item| holds(document, item)),
        Condition::Any(items) => items.iter().any(|item| holds(document, item)),
    }
}

fn condition_of(text: &str) -> SqlResult<Condition> {
    let refused = |near: &str| {
        SqlError::new(
            SqlState::SYNTAX_ERROR,
            format!("Syntax error near '{near}' in the full-text search condition '{text}'."),
        )
        .with_vendor_code(7630)
    };
    let mut reader = Reader {
        tokens: tokens(text).map_err(|message| refused(&message))?,
        at: 0,
    };
    if reader.tokens.is_empty() {
        return Err(SqlError::new(
            SqlState::SYNTAX_ERROR,
            "Null or empty full-text predicate.",
        )
        .with_vendor_code(7645));
    }
    let condition = reader.any().map_err(|message| refused(&message))?;
    if reader.at < reader.tokens.len() {
        return Err(refused(&format!("{:?}", reader.tokens[reader.at])));
    }
    Ok(condition)
}

fn document_of(columns: &[Value]) -> Vec<String> {
    columns
        .iter()
        .filter_map(Value::to_text)
        .flat_map(|text| words(&text))
        .collect()
}

/// Transact-SQL's `CONTAINS (columns, condition)`: whether the columns' text satisfies the
/// search condition (words, `"phrases"`, `"prefix*"`, `AND`, `OR`, `AND NOT`, `&`, `|`, `&!`,
/// `NEAR`, `FORMSOF` and `ISABOUT` read as the words they list), compared case-insensitively.
pub(crate) fn contains(values: &[Value]) -> SqlResult<Value> {
    let [query, columns @ ..] = values else {
        return Err(SqlError::new(SqlState::SYNTAX_ERROR, "CONTAINS needs columns and a search condition"));
    };
    let Some(query) = query.to_text() else {
        return Err(SqlError::new(SqlState::SYNTAX_ERROR, "Null or empty full-text predicate.").with_vendor_code(7645));
    };
    let condition = condition_of(&query)?;
    Ok(Value::Boolean(holds(&document_of(columns), &condition)))
}

/// Transact-SQL's `FREETEXT (columns, text)`: whether the columns hold any of the text's words
/// other than noise words.
pub(crate) fn freetext(values: &[Value]) -> SqlResult<Value> {
    let [query, columns @ ..] = values else {
        return Err(SqlError::new(SqlState::SYNTAX_ERROR, "FREETEXT needs columns and a search text"));
    };
    let Some(query) = query.to_text() else {
        return Ok(Value::Boolean(false));
    };
    let document = document_of(columns);
    Ok(Value::Boolean(
        words(&query)
            .iter()
            .filter(|word| !NOISE_WORDS.contains(&word.as_str()))
            .any(|word| document.contains(word)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(condition: &str, text: &str) -> bool {
        matches!(contains(&[Value::Text(condition.into()), Value::Text(text.into())]), Ok(Value::Boolean(true)))
    }

    #[test]
    fn search_conditions_follow_transact_sql() {
        assert!(check("\"mountain bike\"", "A Mountain Bike for trails"));
        assert!(!check("\"bike mountain\"", "A Mountain Bike for trails"));
        assert!(check("\"chain*\"", "chainring and gears"));
        assert!(check("bike AND NOT road", "mountain bike"));
        assert!(!check("bike &! mountain", "mountain bike"));
        assert!(check("tire OR bike", "bike"));
        assert!(check("NEAR((mountain, bike), 5)", "bike for the mountain"));
        assert!(check("FORMSOF(INFLECTIONAL, ride, rides)", "she rides"));
        assert!(contains(&[Value::Text("\"open".into()), Value::Text("x".into())]).is_err());
        assert!(matches!(
            freetext(&[Value::Text("the safety of riders".into()), Value::Text("Riders first".into())]),
            Ok(Value::Boolean(true))
        ));
    }
}
