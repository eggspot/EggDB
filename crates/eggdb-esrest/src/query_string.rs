use crate::query::{Query, analyze};
use eggdb_sql::json::{Json, Node};

const MAX_GROUP_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    Phrase(String),
    Range { field_bound: (String, String), low_inclusive: bool, high_inclusive: bool },
    Field(String),
    Open,
    Close,
    And,
    Or,
    Not,
    Plus,
    Minus,
}

#[derive(Debug, Clone, PartialEq)]
enum Clause {
    Plain(Query),
    Required(Query),
    Prohibited(Query),
}

/// How a query string is read: the fields a term without `field:` searches, whether
/// juxtaposed terms all have to match, and whether it is `simple_query_string`'s syntax.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Options {
    pub(crate) fields: Vec<String>,
    pub(crate) all_by_default: bool,
    pub(crate) simple: bool,
}

fn tokens(text: &str, simple: bool) -> Result<Vec<Token>, String> {
    let characters: Vec<char> = text.chars().collect();
    let mut found = Vec::new();
    let mut at = 0;
    while at < characters.len() {
        let character = characters[at];
        match character {
            whitespace if whitespace.is_whitespace() => at += 1,
            '(' => {
                found.push(Token::Open);
                at += 1;
            }
            ')' => {
                found.push(Token::Close);
                at += 1;
            }
            '"' => {
                let end = characters[at + 1..]
                    .iter()
                    .position(|next| *next == '"')
                    .map(|offset| at + 1 + offset)
                    .ok_or("a quoted phrase is not closed")?;
                found.push(Token::Phrase(characters[at + 1..end].iter().collect()));
                at = end + 1;
            }
            '+' => {
                found.push(if simple { Token::And } else { Token::Plus });
                at += 1;
            }
            '|' if simple => {
                found.push(Token::Or);
                at += 1;
            }
            '-' | '!' if at + 1 < characters.len() && !characters[at + 1].is_whitespace() || simple => {
                found.push(if character == '!' { Token::Not } else { Token::Minus });
                at += 1;
            }
            '&' if characters.get(at + 1) == Some(&'&') && !simple => {
                found.push(Token::And);
                at += 2;
            }
            '|' if characters.get(at + 1) == Some(&'|') => {
                found.push(Token::Or);
                at += 2;
            }
            '[' | '{' if !simple => {
                let close = characters[at + 1..]
                    .iter()
                    .position(|next| matches!(next, ']' | '}'))
                    .map(|offset| at + 1 + offset)
                    .ok_or("a range is not closed")?;
                let inside: String = characters[at + 1..close].iter().collect();
                let parts: Vec<&str> = inside.split_whitespace().collect();
                let [low, to, high] = parts.as_slice() else {
                    return Err(format!("cannot read the range [{inside}]"));
                };
                if !to.eq_ignore_ascii_case("to") {
                    return Err(format!("cannot read the range [{inside}]"));
                }
                found.push(Token::Range {
                    field_bound: ((*low).to_owned(), (*high).to_owned()),
                    low_inclusive: character == '[',
                    high_inclusive: characters[close] == ']',
                });
                at = close + 1;
            }
            _ => {
                let mut word = String::new();
                while at < characters.len() {
                    let next = characters[at];
                    if next.is_whitespace() || matches!(next, '(' | ')' | '"') || (simple && matches!(next, '|' | '+')) {
                        break;
                    }
                    if next == '\\' && at + 1 < characters.len() {
                        word.push(characters[at + 1]);
                        at += 2;
                        continue;
                    }
                    if next == ':' && !simple && !word.is_empty() {
                        at += 1;
                        break;
                    }
                    word.push(next);
                    at += 1;
                }
                if !simple && characters.get(at.wrapping_sub(1)) == Some(&':') && !word.is_empty() {
                    found.push(Token::Field(word));
                    continue;
                }
                found.push(match word.as_str() {
                    "AND" if !simple => Token::And,
                    "OR" if !simple => Token::Or,
                    "NOT" if !simple => Token::Not,
                    _ => Token::Word(word),
                });
            }
        }
    }
    Ok(found)
}

fn json_bound(text: &str) -> Option<Json> {
    if text == "*" {
        return None;
    }
    Some(if text.parse::<f64>().is_ok() {
        Json::Number(text.to_owned())
    } else {
        Json::String(text.to_owned())
    })
}

fn any_of(mut queries: Vec<Query>) -> Query {
    if queries.len() == 1 {
        return queries.remove(0);
    }
    Query::Bool {
        must: Vec::new(),
        filter: Vec::new(),
        should: queries,
        must_not: Vec::new(),
        minimum_should_match: 1,
    }
}

fn inner(clause: Clause) -> Query {
    match clause {
        Clause::Plain(query) | Clause::Required(query) => query,
        Clause::Prohibited(query) => Query::Bool {
            must: Vec::new(),
            filter: Vec::new(),
            should: Vec::new(),
            must_not: vec![query],
            minimum_should_match: 0,
        },
    }
}

struct Reader<'o> {
    tokens: Vec<Token>,
    at: usize,
    options: &'o Options,
}

impl Reader<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn starts_clause(&self) -> bool {
        matches!(
            self.peek(),
            Some(Token::Word(_) | Token::Phrase(_) | Token::Range { .. } | Token::Field(_) | Token::Open | Token::Not | Token::Plus | Token::Minus)
        )
    }

    fn term(&self, fields: &[String], token: Token) -> Result<Query, String> {
        Ok(match token {
            Token::Phrase(text) => any_of(
                fields
                    .iter()
                    .map(|field| Query::MatchPhrase {
                        field: field.clone(),
                        terms: analyze(&text),
                    })
                    .collect(),
            ),
            Token::Range {
                field_bound: (low, high),
                low_inclusive,
                high_inclusive,
            } => {
                let mut bounds = Vec::new();
                if let Some(low) = json_bound(&low) {
                    bounds.push((if low_inclusive { "gte" } else { "gt" }.to_owned(), low));
                }
                if let Some(high) = json_bound(&high) {
                    bounds.push((if high_inclusive { "lte" } else { "lt" }.to_owned(), high));
                }
                any_of(
                    fields
                        .iter()
                        .map(|field| Query::Range {
                            field: field.clone(),
                            bounds: bounds.clone(),
                        })
                        .collect(),
                )
            }
            Token::Word(word) => {
                let comparison = ["<=", ">=", "<", ">"]
                    .into_iter()
                    .find(|operator| word.starts_with(operator) && word.len() > operator.len());
                if let (Some(operator), false) = (comparison, self.options.simple) {
                    let value = json_bound(&word[operator.len()..]).ok_or("a comparison needs a value")?;
                    let name = match operator {
                        "<=" => "lte",
                        ">=" => "gte",
                        "<" => "lt",
                        _ => "gt",
                    };
                    return Ok(any_of(
                        fields
                            .iter()
                            .map(|field| Query::Range {
                                field: field.clone(),
                                bounds: vec![(name.to_owned(), value.clone())],
                            })
                            .collect(),
                    ));
                }
                if let Some(prefix) = word.strip_suffix('*').filter(|prefix| !prefix.contains('*')) {
                    return Ok(any_of(
                        fields
                            .iter()
                            .map(|field| Query::Prefix {
                                field: field.clone(),
                                prefix: prefix.to_owned(),
                            })
                            .collect(),
                    ));
                }
                let terms = analyze(&word);
                if terms.is_empty() {
                    return Ok(Query::MatchNone);
                }
                Query::Match {
                    fields: fields.to_vec(),
                    terms,
                    all: true,
                }
            }
            other => return Err(format!("unexpected {other:?}")),
        })
    }

    fn unary(&mut self, fields: &[String], depth: usize) -> Result<Clause, String> {
        if depth > MAX_GROUP_DEPTH {
            return Err("the query string nests too deeply".to_owned());
        }
        let token = self.tokens.get(self.at).cloned().ok_or("the query string ends early")?;
        self.at += 1;
        Ok(match token {
            Token::Not | Token::Minus => Clause::Prohibited(inner(self.unary(fields, depth + 1)?)),
            Token::Plus => Clause::Required(inner(self.unary(fields, depth + 1)?)),
            Token::Open => {
                let group = self.any(fields, depth + 1)?;
                if self.peek() == Some(&Token::Close) {
                    self.at += 1;
                } else {
                    return Err("a group is not closed".to_owned());
                }
                Clause::Plain(group)
            }
            Token::Field(field) if field == "_exists_" => {
                let Some(Token::Word(name)) = self.tokens.get(self.at).cloned() else {
                    return Err("_exists_ needs a field".to_owned());
                };
                self.at += 1;
                Clause::Plain(Query::Exists { field: name })
            }
            Token::Field(field) => {
                let scoped = vec![field];
                self.unary(&scoped, depth + 1)?
            }
            other => Clause::Plain(self.term(fields, other)?),
        })
    }

    fn all(&mut self, fields: &[String], depth: usize) -> Result<Vec<Clause>, String> {
        let mut clauses = vec![self.unary(fields, depth)?];
        loop {
            if self.peek() == Some(&Token::And) {
                self.at += 1;
                clauses.push(self.unary(fields, depth)?);
            } else if self.options.all_by_default && self.starts_clause() {
                clauses.push(self.unary(fields, depth)?);
            } else {
                return Ok(clauses);
            }
        }
    }

    fn any(&mut self, fields: &[String], depth: usize) -> Result<Query, String> {
        let mut required = Vec::new();
        let mut prohibited = Vec::new();
        let mut optional = Vec::new();
        loop {
            let group = self.all(fields, depth)?;
            let conjunction = group.len() > 1;
            for clause in group {
                match clause {
                    Clause::Required(query) => required.push(query),
                    Clause::Prohibited(query) => prohibited.push(query),
                    Clause::Plain(query) if conjunction || self.options.all_by_default => required.push(query),
                    Clause::Plain(query) => optional.push(query),
                }
            }
            if self.peek() == Some(&Token::Or) {
                self.at += 1;
                continue;
            }
            if !self.options.all_by_default && self.starts_clause() {
                continue;
            }
            break;
        }
        let minimum = usize::from(required.is_empty() && !optional.is_empty());
        if prohibited.is_empty() && optional.is_empty() && required.len() == 1 {
            return Ok(required.remove(0));
        }
        if prohibited.is_empty() && required.is_empty() && optional.len() == 1 {
            return Ok(optional.remove(0));
        }
        Ok(Query::Bool {
            must: required,
            filter: Vec::new(),
            should: optional,
            must_not: prohibited,
            minimum_should_match: minimum,
        })
    }
}

/// The query a Lucene query string means (`query_string`, or `simple_query_string`'s
/// syntax when `options.simple`).
pub(crate) fn query_of(text: &str, options: &Options) -> Result<Query, String> {
    let parsed = tokens(text, options.simple).and_then(|tokens| {
        if tokens.is_empty() {
            return Ok(Query::MatchNone);
        }
        let mut reader = Reader {
            tokens,
            at: 0,
            options,
        };
        let query = reader.any(&options.fields, 0)?;
        if reader.at < reader.tokens.len() {
            return Err(format!("unexpected {:?}", reader.tokens[reader.at]));
        }
        Ok(query)
    });
    match parsed {
        Ok(query) => Ok(query),
        Err(_) if options.simple => Ok(Query::Match {
            fields: options.fields.clone(),
            terms: analyze(text),
            all: options.all_by_default,
        }),
        Err(reason) => Err(format!("Failed to parse query [{text}]: {reason}")),
    }
}

fn text_member(body: &Node, name: &str) -> Option<String> {
    let Json::Object(members) = &body.value else {
        return None;
    };
    members.iter().find(|(key, _)| key == name).and_then(|(_, node)| match &node.value {
        Json::String(text) => Some(text.clone()),
        Json::Number(text) => Some(text.clone()),
        _ => None,
    })
}

/// Reads a `query_string` or `simple_query_string` query's body.
pub(crate) fn parse(body: &Node, simple: bool) -> Result<Query, String> {
    let kind = if simple { "simple_query_string" } else { "query_string" };
    let text = text_member(body, "query").ok_or_else(|| format!("[{kind}] must be provided with a [query]"))?;
    let mut fields: Vec<String> = match &body.value {
        Json::Object(members) => match members.iter().find(|(key, _)| key == "fields").map(|(_, node)| &node.value) {
            Some(Json::Array(items)) => items
                .iter()
                .filter_map(|item| match &item.value {
                    Json::String(field) => Some(field.split('^').next().unwrap_or_default().to_owned()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    if fields.is_empty() {
        fields.push(text_member(body, "default_field").unwrap_or_else(|| "*".to_owned()));
    }
    let all_by_default = text_member(body, "default_operator").is_some_and(|operator| operator.eq_ignore_ascii_case("and"));
    query_of(
        &text,
        &Options {
            fields,
            all_by_default,
            simple,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str) -> Query {
        query_of(
            text,
            &Options {
                fields: vec!["*".to_owned()],
                all_by_default: false,
                simple: false,
            },
        )
        .unwrap()
    }

    #[test]
    fn lucene_syntax_becomes_the_query_dsl() {
        assert_eq!(
            read("title:rust"),
            Query::Match { fields: vec!["title".to_owned()], terms: vec!["rust".to_owned()], all: true }
        );
        let Query::Bool { must, should, must_not, minimum_should_match, .. } = read("+title:rust -draft \"fast db\" year:[2020 TO *]") else {
            panic!("not a bool query");
        };
        assert_eq!(must.len(), 1);
        assert_eq!(must_not.len(), 1);
        assert_eq!(should.len(), 2);
        assert_eq!(minimum_should_match, 0);
        let Query::Bool { must, .. } = read("a AND b") else {
            panic!("not a bool query");
        };
        assert_eq!(must.len(), 2);
        assert!(query_of("(open", &Options { fields: vec!["*".into()], all_by_default: false, simple: false }).is_err());
        assert!(query_of("(open", &Options { fields: vec!["*".into()], all_by_default: false, simple: true }).is_ok());
    }
}
