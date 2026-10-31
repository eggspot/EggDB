use crate::vector::{Config, TsVector, quote, quoted, words};
use std::fmt;

const MAX_DEPTH: usize = 64;

/// A text-search query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TsQuery {
    /// A lexeme, or a prefix of lexemes (`'cat':*`).
    Term {
        /// The lexeme.
        lexeme: String,
        /// Whether it matches every lexeme starting with it.
        prefix: bool,
    },
    /// Both.
    And(Box<TsQuery>, Box<TsQuery>),
    /// Either.
    Or(Box<TsQuery>, Box<TsQuery>),
    /// Not.
    Not(Box<TsQuery>),
    /// The left side followed by the right `distance` positions later (`<->` is 1).
    Phrase(Box<TsQuery>, Box<TsQuery>, u32),
    /// Nothing to search for (a query of stop words only), which matches nothing.
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Term(String, bool),
    And,
    Or,
    Not,
    Follows(u32),
    Open,
    Close,
}

fn tokens(text: &str, config: Option<Config>) -> Option<Vec<Token>> {
    let mut out = Vec::new();
    let mut rest = text;
    loop {
        rest = rest.trim_start();
        let Some(first) = rest.chars().next() else {
            return Some(out);
        };
        match first {
            '&' => {
                out.push(Token::And);
                rest = &rest[1..];
            }
            '|' => {
                out.push(Token::Or);
                rest = &rest[1..];
            }
            '!' => {
                out.push(Token::Not);
                rest = &rest[1..];
            }
            '(' => {
                out.push(Token::Open);
                rest = &rest[1..];
            }
            ')' => {
                out.push(Token::Close);
                rest = &rest[1..];
            }
            '<' => {
                let end = rest.find('>')?;
                let inside = &rest[1..end];
                let distance = if inside == "-" { 1 } else { inside.parse().ok()? };
                out.push(Token::Follows(distance));
                rest = &rest[end + 1..];
            }
            '\'' => {
                let (word, after) = quoted(rest)?;
                let (prefix, after) = match after.strip_prefix(":*") {
                    Some(after) => (true, after),
                    None => (false, after),
                };
                let lexeme = match config {
                    Some(config) => config.lexeme(&word),
                    None => Some(word),
                };
                out.push(Token::Term(lexeme.unwrap_or_default(), prefix));
                rest = after;
            }
            _ => {
                let end = rest
                    .find(|character: char| character.is_whitespace() || "&|!()<:'".contains(character))
                    .unwrap_or(rest.len());
                if end == 0 {
                    return None;
                }
                let word = &rest[..end];
                rest = &rest[end..];
                let prefix = match rest.strip_prefix(":*") {
                    Some(after) => {
                        rest = after;
                        true
                    }
                    None => false,
                };
                let lexeme = match config {
                    Some(config) => config.lexeme(word),
                    None => Some(word.to_lowercase()),
                };
                out.push(Token::Term(lexeme.unwrap_or_default(), prefix));
            }
        }
    }
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

    fn or(&mut self) -> Option<TsQuery> {
        let mut left = self.and()?;
        while self.peek() == Some(&Token::Or) {
            self.at += 1;
            left = combine(left, self.and()?, |a, b| TsQuery::Or(Box::new(a), Box::new(b)));
        }
        Some(left)
    }

    fn and(&mut self) -> Option<TsQuery> {
        let mut left = self.phrase()?;
        while self.peek() == Some(&Token::And) {
            self.at += 1;
            left = combine(left, self.phrase()?, |a, b| TsQuery::And(Box::new(a), Box::new(b)));
        }
        Some(left)
    }

    fn phrase(&mut self) -> Option<TsQuery> {
        let mut left = self.unary()?;
        while let Some(Token::Follows(distance)) = self.peek().cloned() {
            self.at += 1;
            left = combine(left, self.unary()?, |a, b| TsQuery::Phrase(Box::new(a), Box::new(b), distance));
        }
        Some(left)
    }

    fn unary(&mut self) -> Option<TsQuery> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return None;
        }
        let result = match self.peek().cloned()? {
            Token::Not => {
                self.at += 1;
                match self.unary()? {
                    TsQuery::Empty => TsQuery::Empty,
                    inner => TsQuery::Not(Box::new(inner)),
                }
            }
            Token::Open => {
                self.at += 1;
                let inner = self.or()?;
                if self.peek() != Some(&Token::Close) {
                    return None;
                }
                self.at += 1;
                inner
            }
            Token::Term(lexeme, prefix) => {
                self.at += 1;
                if lexeme.is_empty() {
                    TsQuery::Empty
                } else {
                    TsQuery::Term { lexeme, prefix }
                }
            }
            _ => return None,
        };
        self.depth -= 1;
        Some(result)
    }
}

fn combine(left: TsQuery, right: TsQuery, join: impl FnOnce(TsQuery, TsQuery) -> TsQuery) -> TsQuery {
    match (left, right) {
        (TsQuery::Empty, other) | (other, TsQuery::Empty) => other,
        (left, right) => join(left, right),
    }
}

impl TsQuery {
    /// A query in `to_tsquery`'s syntax (`fat & (rat | cat:*) & !dog`, `a <-> b`), its words
    /// normalized by `config`; `None` when the syntax is wrong.
    pub fn parse(config: Config, text: &str) -> Option<Self> {
        Self::parse_tokens(tokens(text, Some(config))?)
    }

    /// A query written in its own text form (`'fat' & 'rat'`), lexemes taken as they are.
    pub fn parse_literal(text: &str) -> Option<Self> {
        Self::parse_tokens(tokens(text, None)?)
    }

    fn parse_tokens(tokens: Vec<Token>) -> Option<Self> {
        if tokens.is_empty() {
            return Some(Self::Empty);
        }
        let mut parser = Parser { tokens, at: 0, depth: 0 };
        let query = parser.or()?;
        (parser.at == parser.tokens.len()).then_some(query)
    }

    /// `plainto_tsquery`: every word, all required.
    pub fn plain(config: Config, text: &str) -> Self {
        words(text)
            .into_iter()
            .filter_map(|word| config.lexeme(word))
            .map(|lexeme| Self::Term { lexeme, prefix: false })
            .fold(Self::Empty, |left, right| combine(left, right, |a, b| Self::And(Box::new(a), Box::new(b))))
    }

    /// `phraseto_tsquery`: the words in order, each next to the one before.
    pub fn phrase(config: Config, text: &str) -> Self {
        words(text)
            .into_iter()
            .filter_map(|word| config.lexeme(word))
            .map(|lexeme| Self::Term { lexeme, prefix: false })
            .fold(Self::Empty, |left, right| combine(left, right, |a, b| Self::Phrase(Box::new(a), Box::new(b), 1)))
    }

    /// `websearch_to_tsquery`: words all required, `"quoted phrases"`, `or`, and `-word` to
    /// exclude.
    pub fn web(config: Config, text: &str) -> Self {
        let mut alternatives = vec![Self::Empty];
        let mut rest = text;
        while !rest.trim_start().is_empty() {
            rest = rest.trim_start();
            let (negated, after) = match rest.strip_prefix('-') {
                Some(after) => (true, after),
                None => (false, rest),
            };
            let (part, after) = if let Some(quoted) = after.strip_prefix('"') {
                let end = quoted.find('"').unwrap_or(quoted.len());
                (Self::phrase(config, &quoted[..end]), quoted.get(end + 1..).unwrap_or(""))
            } else {
                let end = after.find(char::is_whitespace).unwrap_or(after.len());
                let word = &after[..end];
                if !negated && word.eq_ignore_ascii_case("or") {
                    alternatives.push(Self::Empty);
                    rest = &after[end..];
                    continue;
                }
                (Self::plain(config, word), &after[end..])
            };
            let part = if negated && part != Self::Empty { Self::Not(Box::new(part)) } else { part };
            if let Some(last) = alternatives.pop() {
                alternatives.push(combine(last, part, |a, b| Self::And(Box::new(a), Box::new(b))));
            }
            rest = after;
        }
        alternatives
            .into_iter()
            .fold(Self::Empty, |left, right| combine(left, right, |a, b| Self::Or(Box::new(a), Box::new(b))))
    }

    /// Whether `vector` matches.
    pub fn matches(&self, vector: &TsVector) -> bool {
        match self {
            Self::Empty => false,
            Self::Term { lexeme, prefix } => vector.contains(lexeme, *prefix),
            Self::And(left, right) => left.matches(vector) && right.matches(vector),
            Self::Or(left, right) => left.matches(vector) || right.matches(vector),
            Self::Not(inner) => !inner.matches(vector),
            Self::Phrase(..) => !self.phrase_positions(vector).is_empty(),
        }
    }

    fn phrase_positions(&self, vector: &TsVector) -> Vec<u32> {
        match self {
            Self::Term { lexeme, prefix } => vector.positions(lexeme, *prefix),
            Self::Phrase(left, right, distance) => {
                let lefts = left.phrase_positions(vector);
                right
                    .phrase_positions(vector)
                    .into_iter()
                    .filter(|end| lefts.iter().any(|start| start + distance == *end))
                    .collect()
            }
            Self::And(left, right) => {
                let lefts = left.phrase_positions(vector);
                let rights = right.phrase_positions(vector);
                if lefts.is_empty() || rights.is_empty() {
                    Vec::new()
                } else {
                    let mut all = [lefts, rights].concat();
                    all.sort_unstable();
                    all.dedup();
                    all
                }
            }
            Self::Or(left, right) => {
                let mut all = [left.phrase_positions(vector), right.phrase_positions(vector)].concat();
                all.sort_unstable();
                all.dedup();
                all
            }
            Self::Not(_) | Self::Empty => Vec::new(),
        }
    }

    /// The lexemes the query asks for (not under `!`).
    pub fn terms(&self) -> Vec<(&str, bool)> {
        match self {
            Self::Term { lexeme, prefix } => vec![(lexeme.as_str(), *prefix)],
            Self::And(left, right) | Self::Or(left, right) | Self::Phrase(left, right, _) => {
                let mut all = left.terms();
                all.extend(right.terms());
                all
            }
            Self::Not(_) | Self::Empty => Vec::new(),
        }
    }
}

fn written(query: &TsQuery, parent: u8, out: &mut String) {
    let (level, text): (u8, Option<&str>) = match query {
        TsQuery::Or(..) => (1, Some(" | ")),
        TsQuery::And(..) => (2, Some(" & ")),
        TsQuery::Phrase(..) => (3, None),
        _ => (4, None),
    };
    let wrap = level < parent;
    if wrap {
        out.push_str("( ");
    }
    match query {
        TsQuery::Empty => {}
        TsQuery::Term { lexeme, prefix } => {
            out.push_str(&quote(lexeme));
            if *prefix {
                out.push_str(":*");
            }
        }
        TsQuery::Not(inner) => {
            out.push('!');
            written(inner, 4, out);
        }
        TsQuery::And(left, right) | TsQuery::Or(left, right) => {
            written(left, level, out);
            out.push_str(text.unwrap_or_default());
            written(right, level + 1, out);
        }
        TsQuery::Phrase(left, right, distance) => {
            written(left, level, out);
            if *distance == 1 {
                out.push_str(" <-> ");
            } else {
                out.push_str(&format!(" <{distance}> "));
            }
            written(right, level + 1, out);
        }
    }
    if wrap {
        out.push_str(" )");
    }
}

impl fmt::Display for TsQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        written(self, 0, &mut out);
        f.write_str(&out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_parse_print_and_match_as_postgresql_documents() {
        let vector = TsVector::from_text(Config::English, "a fat cat sat on a mat and ate a fat rat");
        let query = TsQuery::parse(Config::English, "fat & (rats | dog) & !cow").unwrap();
        assert_eq!(query.to_string(), "'fat' & ( 'rat' | 'dog' ) & !'cow'");
        assert!(query.matches(&vector));
        assert!(TsQuery::parse(Config::English, "fat <-> cat").unwrap().matches(&vector));
        assert!(!TsQuery::parse(Config::English, "cat <-> fat").unwrap().matches(&vector));
        assert!(TsQuery::parse(Config::English, "fat <2> sat").unwrap().matches(&vector));
        assert!(TsQuery::parse(Config::English, "ra:*").unwrap().matches(&vector));
        assert_eq!(TsQuery::plain(Config::English, "The Fat Rats").to_string(), "'fat' & 'rat'");
        assert_eq!(TsQuery::phrase(Config::English, "fat cats").to_string(), "'fat' <-> 'cat'");
        assert_eq!(
            TsQuery::web(Config::English, "\"fat rat\" or cat -dog").to_string(),
            "'fat' <-> 'rat' | 'cat' & !'dog'"
        );
        assert_eq!(TsQuery::parse(Config::English, "the & a").unwrap(), TsQuery::Empty);
        assert_eq!(TsQuery::parse_literal(&query.to_string()), Some(query));
        for broken in ["fat &", "(fat", "fat)", "<x> fat", "&"] {
            assert!(TsQuery::parse(Config::English, broken).is_none(), "{broken}");
        }
        let deep = "(".repeat(100) + "a" + &")".repeat(100);
        assert!(TsQuery::parse(Config::Simple, &deep).is_none());
    }
}
