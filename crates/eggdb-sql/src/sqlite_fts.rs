//! SQLite's FTS5 full-text tables, from SQLite's published FTS5 documentation: `CREATE VIRTUAL
//! TABLE ... USING fts5(...)` keeps the columns in an ordinary table, and `MATCH` evaluates FTS5's
//! query syntax (terms, "phrases", prefixes, `AND`/`OR`/`NOT`, parentheses, column filters and
//! `NEAR`) against each row's text, with the unicode61 tokenizer's rules. `bm25()`,
//! `highlight()` and `snippet()` work on the matched row; there is no inverted index yet, so a
//! search reads every row and `bm25()` scores a row by its own term frequencies (D800).

use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::Evaluator;
use crate::ast::{Expr, ExprKind};
use crate::types::Value;
use std::cell::RefCell;

pub(crate) const MATCH_FUNCTION: &str = "fts5_match";
const FUNCTIONS: [&str; 4] = [MATCH_FUNCTION, "bm25", "highlight", "snippet"];
const DEFAULT_NEAR: usize = 10;
const MOST_QUERY_NODES: usize = 10_000;

thread_local! {
    static LAST_QUERY: RefCell<Option<Query>> = const { RefCell::new(None) };
}

pub(crate) fn is_function(name: &str) -> bool {
    FUNCTIONS.contains(&name)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Query {
    Phrase {
        columns: Option<Vec<String>>,
        excluded: bool,
        tokens: Vec<(String, bool)>,
        initial: bool,
    },
    Near(Vec<Query>, usize),
    And(Box<Query>, Box<Query>),
    Or(Box<Query>, Box<Query>),
    Not(Box<Query>, Box<Query>),
}

fn syntax(near: &str) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        format!("fts5: syntax error near \"{near}\""),
    )
    .with_sqlite_code(1)
}

/// The tokens of `text` under the unicode61 tokenizer: runs of letters and digits, folded to
/// lower case.
pub(crate) fn tokens(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
enum Lexeme {
    Word(String),
    Quoted(String),
    Star,
    Caret,
    Colon,
    Minus,
    Open,
    Close,
    OpenBrace,
    CloseBrace,
    Comma,
    Plus,
}

fn lexemes(text: &str) -> SqlResult<Vec<Lexeme>> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&character) = chars.peek() {
        match character {
            ' ' | '\t' | '\n' | '\r' => {
                chars.next();
            }
            '"' => {
                chars.next();
                let mut quoted = String::new();
                loop {
                    match chars.next() {
                        Some('"') if chars.peek() == Some(&'"') => {
                            chars.next();
                            quoted.push('"');
                        }
                        Some('"') => break,
                        Some(other) => quoted.push(other),
                        None => return Err(syntax(text)),
                    }
                }
                out.push(Lexeme::Quoted(quoted));
            }
            '*' | '^' | ':' | '-' | '(' | ')' | '{' | '}' | ',' | '+' => {
                chars.next();
                out.push(match character {
                    '*' => Lexeme::Star,
                    '^' => Lexeme::Caret,
                    ':' => Lexeme::Colon,
                    '-' => Lexeme::Minus,
                    '(' => Lexeme::Open,
                    ')' => Lexeme::Close,
                    '{' => Lexeme::OpenBrace,
                    '}' => Lexeme::CloseBrace,
                    ',' => Lexeme::Comma,
                    _ => Lexeme::Plus,
                });
            }
            _ if character.is_alphanumeric() || character == '_' || !character.is_ascii() => {
                let mut word = String::new();
                while let Some(&next) = chars.peek() {
                    if next.is_alphanumeric() || next == '_' || !next.is_ascii() {
                        word.push(next);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push(Lexeme::Word(word));
            }
            other => return Err(syntax(&other.to_string())),
        }
    }
    Ok(out)
}

struct QueryParser {
    lexemes: Vec<Lexeme>,
    at: usize,
    nodes: usize,
}

impl QueryParser {
    fn peek(&self) -> Option<&Lexeme> {
        self.lexemes.get(self.at)
    }

    fn keyword(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Lexeme::Word(found)) if found == word)
    }

    fn counted(&mut self) -> SqlResult<()> {
        self.nodes += 1;
        if self.nodes > MOST_QUERY_NODES {
            return Err(syntax("a query with too many terms"));
        }
        Ok(())
    }

    fn or(&mut self) -> SqlResult<Query> {
        let mut left = self.and()?;
        while self.keyword("OR") {
            self.at += 1;
            let right = self.and()?;
            left = Query::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> SqlResult<Query> {
        let mut left = self.not()?;
        loop {
            if self.keyword("AND") {
                self.at += 1;
            } else if !self.starts_item() {
                return Ok(left);
            }
            let right = self.not()?;
            left = Query::And(Box::new(left), Box::new(right));
        }
    }

    fn not(&mut self) -> SqlResult<Query> {
        let mut left = self.item()?;
        while self.keyword("NOT") {
            self.at += 1;
            let right = self.item()?;
            left = Query::Not(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn starts_item(&self) -> bool {
        match self.peek() {
            Some(Lexeme::Word(word)) => !matches!(word.as_str(), "AND" | "OR" | "NOT"),
            Some(Lexeme::Quoted(_) | Lexeme::Open | Lexeme::Caret | Lexeme::Minus | Lexeme::OpenBrace) => {
                true
            }
            _ => false,
        }
    }

    fn column_filter(&mut self) -> SqlResult<Option<(Vec<String>, bool)>> {
        let start = self.at;
        let excluded = matches!(self.peek(), Some(Lexeme::Minus));
        if excluded {
            self.at += 1;
        }
        let columns = match self.peek() {
            Some(Lexeme::OpenBrace) => {
                self.at += 1;
                let mut names = Vec::new();
                while let Some(Lexeme::Word(name) | Lexeme::Quoted(name)) = self.peek().cloned() {
                    names.push(name.to_lowercase());
                    self.at += 1;
                }
                if self.peek() != Some(&Lexeme::CloseBrace) {
                    return Err(syntax("{"));
                }
                self.at += 1;
                names
            }
            Some(Lexeme::Word(name) | Lexeme::Quoted(name))
                if self.lexemes.get(self.at + 1) == Some(&Lexeme::Colon) =>
            {
                let name = name.to_lowercase();
                self.at += 1;
                vec![name]
            }
            _ => {
                self.at = start;
                return Ok(None);
            }
        };
        if self.peek() != Some(&Lexeme::Colon) {
            self.at = start;
            return Ok(None);
        }
        self.at += 1;
        Ok(Some((columns, excluded)))
    }

    fn item(&mut self) -> SqlResult<Query> {
        self.counted()?;
        let filter = self.column_filter()?;
        if self.keyword("NEAR") && self.lexemes.get(self.at + 1) == Some(&Lexeme::Open) {
            self.at += 2;
            let mut phrases = Vec::new();
            let mut distance = DEFAULT_NEAR;
            loop {
                match self.peek().cloned() {
                    Some(Lexeme::Close) => {
                        self.at += 1;
                        break;
                    }
                    Some(Lexeme::Comma) => {
                        self.at += 1;
                        let Some(Lexeme::Word(number)) = self.peek().cloned() else {
                            return Err(syntax(","));
                        };
                        distance = number.parse().map_err(|_| syntax(&number))?;
                        self.at += 1;
                    }
                    Some(_) => phrases.push(self.phrase(None)?),
                    None => return Err(syntax("NEAR")),
                }
            }
            if phrases.is_empty() {
                return Err(syntax("NEAR"));
            }
            return Ok(Query::Near(phrases, distance));
        }
        if self.peek() == Some(&Lexeme::Open) {
            self.at += 1;
            let inner = self.or()?;
            if self.peek() != Some(&Lexeme::Close) {
                return Err(syntax("("));
            }
            self.at += 1;
            return Ok(match filter {
                Some((columns, excluded)) => with_columns(inner, &columns, excluded),
                None => inner,
            });
        }
        self.phrase(filter)
    }

    fn phrase(&mut self, filter: Option<(Vec<String>, bool)>) -> SqlResult<Query> {
        let initial = matches!(self.peek(), Some(Lexeme::Caret));
        if initial {
            self.at += 1;
        }
        let mut words = Vec::new();
        loop {
            let text = match self.peek().cloned() {
                Some(Lexeme::Word(word)) if !matches!(word.as_str(), "AND" | "OR" | "NOT") => word,
                Some(Lexeme::Quoted(text)) => text,
                Some(other) if words.is_empty() => return Err(syntax(&format!("{other:?}"))),
                _ => break,
            };
            self.at += 1;
            let prefix = matches!(self.peek(), Some(Lexeme::Star));
            if prefix {
                self.at += 1;
            }
            let found = tokens(&text);
            let count = found.len();
            for (index, token) in found.into_iter().enumerate() {
                words.push((token, prefix && index + 1 == count));
            }
            if self.peek() == Some(&Lexeme::Plus) {
                self.at += 1;
                continue;
            }
            break;
        }
        if words.is_empty() {
            return Err(syntax(""));
        }
        let (columns, excluded) = match filter {
            Some((columns, excluded)) => (Some(columns), excluded),
            None => (None, false),
        };
        Ok(Query::Phrase {
            columns,
            excluded,
            tokens: words,
            initial,
        })
    }
}

fn with_columns(query: Query, columns: &[String], excluded: bool) -> Query {
    match query {
        Query::Phrase {
            columns: None,
            tokens,
            initial,
            ..
        } => Query::Phrase {
            columns: Some(columns.to_vec()),
            excluded,
            tokens,
            initial,
        },
        Query::And(left, right) => Query::And(
            Box::new(with_columns(*left, columns, excluded)),
            Box::new(with_columns(*right, columns, excluded)),
        ),
        Query::Or(left, right) => Query::Or(
            Box::new(with_columns(*left, columns, excluded)),
            Box::new(with_columns(*right, columns, excluded)),
        ),
        Query::Not(left, right) => Query::Not(
            Box::new(with_columns(*left, columns, excluded)),
            Box::new(with_columns(*right, columns, excluded)),
        ),
        other => other,
    }
}

pub(crate) fn parse_query(text: &str) -> SqlResult<Query> {
    let mut parser = QueryParser {
        lexemes: lexemes(text)?,
        at: 0,
        nodes: 0,
    };
    if parser.lexemes.is_empty() {
        return Err(syntax(""));
    }
    let query = parser.or()?;
    if parser.at != parser.lexemes.len() {
        return Err(syntax(text));
    }
    Ok(query)
}

/// One row as FTS5 sees it: each column's name and tokens.
pub(crate) struct Document {
    columns: Vec<(String, Vec<String>)>,
}

impl Document {
    pub(crate) fn of(columns: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            columns: columns
                .into_iter()
                .map(|(name, text)| (name.to_lowercase(), tokens(&text)))
                .collect(),
        }
    }

    fn searched<'d>(
        &'d self,
        columns: &'d Option<Vec<String>>,
        excluded: bool,
    ) -> impl Iterator<Item = &'d Vec<String>> + 'd {
        self.columns.iter().filter_map(move |(name, tokens)| {
            let named = columns.as_ref().is_none_or(|columns| columns.contains(name));
            let wanted = if excluded && columns.is_some() { !named } else { named };
            wanted.then_some(tokens)
        })
    }
}

fn token_matches(token: &str, (wanted, prefix): &(String, bool)) -> bool {
    if *prefix {
        token.starts_with(wanted.as_str())
    } else {
        token == wanted
    }
}

fn phrase_positions(column: &[String], words: &[(String, bool)], initial: bool) -> Vec<usize> {
    if words.is_empty() || column.len() < words.len() {
        return Vec::new();
    }
    (0..=column.len() - words.len())
        .filter(|start| !initial || *start == 0)
        .filter(|start| {
            words
                .iter()
                .enumerate()
                .all(|(offset, word)| token_matches(&column[start + offset], word))
        })
        .collect()
}

fn matches(query: &Query, document: &Document) -> bool {
    match query {
        Query::Phrase {
            columns,
            excluded,
            tokens,
            initial,
        } => document
            .searched(columns, *excluded)
            .any(|column| !phrase_positions(column, tokens, *initial).is_empty()),
        Query::Near(phrases, distance) => document.columns.iter().any(|(_, column)| {
            let spans: Vec<Vec<(usize, usize)>> = phrases
                .iter()
                .map(|phrase| match phrase {
                    Query::Phrase { tokens, initial, .. } => {
                        phrase_positions(column, tokens, *initial)
                            .into_iter()
                            .map(|start| (start, start + tokens.len()))
                            .collect()
                    }
                    _ => Vec::new(),
                })
                .collect();
            if spans.iter().any(Vec::is_empty) {
                return false;
            }
            near_within(&spans, *distance)
        }),
        Query::And(left, right) => matches(left, document) && matches(right, document),
        Query::Or(left, right) => matches(left, document) || matches(right, document),
        Query::Not(left, right) => matches(left, document) && !matches(right, document),
    }
}

fn near_within(spans: &[Vec<(usize, usize)>], distance: usize) -> bool {
    let mut chosen = Vec::with_capacity(spans.len());
    near_choose(spans, distance, &mut chosen)
}

fn near_choose(spans: &[Vec<(usize, usize)>], distance: usize, chosen: &mut Vec<(usize, usize)>) -> bool {
    let Some(options) = spans.get(chosen.len()) else {
        let start = chosen.iter().map(|span| span.0).min().unwrap_or(0);
        let end = chosen.iter().map(|span| span.1).max().unwrap_or(0);
        let covered: usize = chosen.iter().map(|span| span.1 - span.0).sum();
        return end - start <= covered + distance;
    };
    for span in options.iter().take(64) {
        chosen.push(*span);
        if near_choose(spans, distance, chosen) {
            return true;
        }
        chosen.pop();
    }
    false
}

fn terms(query: &Query, out: &mut Vec<(String, bool)>) {
    match query {
        Query::Phrase { tokens, .. } => out.extend(tokens.iter().cloned()),
        Query::Near(phrases, _) => {
            for phrase in phrases {
                terms(phrase, out);
            }
        }
        Query::And(left, right) | Query::Or(left, right) => {
            terms(left, out);
            terms(right, out);
        }
        Query::Not(left, _) => terms(left, out),
    }
}

fn score(query: &Query, document: &Document) -> f64 {
    const K1: f64 = 1.2;
    let mut wanted = Vec::new();
    terms(query, &mut wanted);
    let mut total = 0.0;
    for word in &wanted {
        let frequency = document
            .columns
            .iter()
            .flat_map(|(_, tokens)| tokens.iter())
            .filter(|token| token_matches(token, word))
            .count() as f64;
        total += frequency * (K1 + 1.0) / (frequency + K1);
    }
    -total
}

fn document_of(evaluator: &Evaluator<'_>, argument: &Expr) -> SqlResult<Document> {
    let label = match &argument.kind {
        ExprKind::Column {
            qualifier: None,
            name,
        } => name,
        _ => {
            return Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "the first argument of an FTS5 function is the full-text table",
            )
            .with_sqlite_code(1));
        }
    };
    let Some(source) = evaluator
        .scope
        .sources()
        .iter()
        .find(|source| source.label == label.as_str())
    else {
        return Err(SqlError::new(
            SqlState::UNDEFINED_COLUMN,
            format!("no such column: {label}"),
        )
        .with_sqlite_code(1));
    };
    Ok(Document::of(source.table.columns.iter().enumerate().map(
        |(index, column)| {
            let text = evaluator
                .row
                .get(source.offset + index)
                .and_then(Value::to_text)
                .unwrap_or_default();
            (column.name.clone(), text)
        },
    )))
}

fn column_text(evaluator: &Evaluator<'_>, argument: &Expr, column: i64) -> SqlResult<Option<String>> {
    let ExprKind::Column { name, .. } = &argument.kind else {
        return Ok(None);
    };
    let Some(source) = evaluator
        .scope
        .sources()
        .iter()
        .find(|source| source.label == name.as_str())
    else {
        return Ok(None);
    };
    let Ok(index) = usize::try_from(column) else {
        return Ok(None);
    };
    if index >= source.table.columns.len() {
        return Ok(None);
    }
    Ok(evaluator.row.get(source.offset + index).and_then(Value::to_text))
}

fn highlighted(text: &str, query: &Query, open: &str, close: &str) -> String {
    let mut wanted = Vec::new();
    terms(query, &mut wanted);
    let mut out = String::with_capacity(text.len() + 16);
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if word.is_empty() {
            return;
        }
        let token = word.to_lowercase();
        if wanted.iter().any(|candidate| token_matches(&token, candidate)) {
            out.push_str(open);
            out.push_str(word);
            out.push_str(close);
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    for character in text.chars() {
        if character.is_alphanumeric() {
            word.push(character);
        } else {
            flush(&mut word, &mut out);
            out.push(character);
        }
    }
    flush(&mut word, &mut out);
    out
}

fn snippet_of(text: &str, query: &Query, open: &str, close: &str, ellipsis: &str, most: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut wanted = Vec::new();
    terms(query, &mut wanted);
    let first_hit = words
        .iter()
        .position(|word| {
            tokens(word)
                .iter()
                .any(|token| wanted.iter().any(|candidate| token_matches(token, candidate)))
        })
        .unwrap_or(0);
    let most = most.clamp(1, 64);
    let start = first_hit.saturating_sub(most / 2);
    let end = (start + most).min(words.len());
    let mut out = String::new();
    if start > 0 {
        out.push_str(ellipsis);
    }
    out.push_str(&highlighted(&words[start..end].join(" "), query, open, close));
    if end < words.len() {
        out.push_str(ellipsis);
    }
    out
}

fn text_argument(evaluator: &Evaluator<'_>, argument: Option<&Expr>) -> SqlResult<String> {
    Ok(match argument {
        Some(argument) => evaluator.eval(argument, None)?.to_text().unwrap_or_default(),
        None => String::new(),
    })
}

fn last_query() -> SqlResult<Query> {
    LAST_QUERY.with(|last| last.borrow().clone()).ok_or_else(|| {
        SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "unable to use function in the requested context",
        )
        .with_sqlite_code(1)
    })
}

/// Runs `MATCH` (as [`MATCH_FUNCTION`]), `bm25()`, `highlight()` or `snippet()`.
pub(crate) fn function(evaluator: &Evaluator<'_>, name: &str, arguments: &[Expr]) -> SqlResult<Value> {
    let Some(table) = arguments.first() else {
        return Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("wrong number of arguments to function {name}()"),
        )
        .with_sqlite_code(1));
    };
    match name {
        MATCH_FUNCTION => {
            let Some(text) = arguments.get(1) else {
                return Err(syntax(""));
            };
            let query_text = evaluator.eval(text, None)?;
            let Some(query_text) = query_text.to_text() else {
                return Ok(Value::Null);
            };
            let query = parse_query(&query_text)?;
            let document = document_of(evaluator, table)?;
            let found = matches(&query, &document);
            LAST_QUERY.with(|last| *last.borrow_mut() = Some(query));
            Ok(Value::Integer(i32::from(found)))
        }
        "bm25" => {
            let query = last_query()?;
            Ok(Value::Double(score(&query, &document_of(evaluator, table)?)))
        }
        "highlight" | "snippet" => {
            let query = last_query()?;
            let column = match arguments.get(1) {
                Some(argument) => evaluator.eval(argument, None)?.as_integer().unwrap_or(-1),
                None => 0,
            };
            let open = text_argument(evaluator, arguments.get(2))?;
            let close = text_argument(evaluator, arguments.get(3))?;
            if name == "highlight" {
                let Some(text) = column_text(evaluator, table, column)? else {
                    return Ok(Value::Null);
                };
                return Ok(Value::Text(highlighted(&text, &query, &open, &close)));
            }
            let ellipsis = text_argument(evaluator, arguments.get(4))?;
            let most = match arguments.get(5) {
                Some(argument) => evaluator.eval(argument, None)?.as_integer().unwrap_or(15),
                None => 15,
            };
            let most = usize::try_from(most).unwrap_or(15);
            let text = if column >= 0 {
                column_text(evaluator, table, column)?.unwrap_or_default()
            } else {
                let document = document_of(evaluator, table)?;
                let mut best = String::new();
                for at in 0..document.columns.len() {
                    if let Some(text) = column_text(evaluator, table, at as i64)?
                        && matches(&query, &Document::of([(String::new(), text.clone())]))
                    {
                        best = text;
                        break;
                    }
                }
                best
            };
            Ok(Value::Text(snippet_of(&text, &query, &open, &close, &ellipsis, most)))
        }
        _ => Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("no such function: {name}"),
        )),
    }
}

#[cfg(test)]
#[path = "sqlite_fts_tests.rs"]
mod tests;
