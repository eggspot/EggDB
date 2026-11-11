use crate::promql::{MatchOp, Matcher, anchored};

const MAX_FILTERS: usize = 32;
const MAX_MATCHERS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineOp {
    Contains,
    Lacks,
    Matches,
    Misses,
}

/// A LogQL log query: a stream selector and line filters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LogQuery {
    pub(crate) matchers: Vec<Matcher>,
    pub(crate) filters: Vec<(LineOp, String)>,
}

struct Scanner<'t> {
    text: &'t str,
    at: usize,
}

impl Scanner<'_> {
    fn rest(&self) -> &str {
        &self.text[self.at..]
    }

    fn spaces(&mut self) {
        let rest = self.rest();
        self.at += rest.len() - rest.trim_start().len();
    }

    fn take(&mut self, symbol: &str) -> bool {
        self.spaces();
        if self.rest().starts_with(symbol) {
            self.at += symbol.len();
            true
        } else {
            false
        }
    }

    fn name(&mut self) -> Result<String, String> {
        self.spaces();
        let rest = self.rest();
        let length = rest
            .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .unwrap_or(rest.len());
        if length == 0 || rest.starts_with(|character: char| character.is_ascii_digit()) {
            return Err(format!("parse error at {}: expected a label name", self.at));
        }
        self.at += length;
        Ok(rest[..length].to_owned())
    }

    fn string(&mut self) -> Result<String, String> {
        self.spaces();
        let mut characters = self.rest().char_indices();
        let quote = match characters.next() {
            Some((_, quote @ ('"' | '`'))) => quote,
            _ => return Err(format!("parse error at {}: expected a quoted string", self.at)),
        };
        let mut value = String::new();
        while let Some((offset, character)) = characters.next() {
            match character {
                '\\' if quote == '"' => match characters.next() {
                    Some((_, 'n')) => value.push('\n'),
                    Some((_, 't')) => value.push('\t'),
                    Some((_, other)) => value.push(other),
                    None => break,
                },
                found if found == quote => {
                    self.at += offset + 1;
                    return Ok(value);
                }
                other => value.push(other),
            }
        }
        Err("a string in the query is not closed".to_owned())
    }
}

/// Reads a LogQL log query: `{name="value", other=~"re"} |= "text" != "x" |~ "re" !~ "re"`.
pub(crate) fn parse(text: &str) -> Result<LogQuery, String> {
    let mut scanner = Scanner { text, at: 0 };
    if !scanner.take("{") {
        return Err("parse error: a log query starts with a stream selector {...}".to_owned());
    }
    let mut matchers = Vec::new();
    if !scanner.take("}") {
        loop {
            let name = scanner.name()?;
            let op = if scanner.take("=~") {
                MatchOp::Regex
            } else if scanner.take("!~") {
                MatchOp::NotRegex
            } else if scanner.take("!=") {
                MatchOp::NotEqual
            } else if scanner.take("=") {
                MatchOp::Equal
            } else {
                return Err(format!("parse error at {}: expected a label matcher", scanner.at));
            };
            let value = scanner.string()?;
            if matches!(op, MatchOp::Regex | MatchOp::NotRegex)
                && eggdb_sql::regex_matches_with_flags("", &anchored(&value), "s").is_err()
            {
                return Err(format!("invalid regular expression {value:?}"));
            }
            matchers.push(Matcher { name, op, value });
            if matchers.len() > MAX_MATCHERS {
                return Err("too many label matchers".to_owned());
            }
            if scanner.take("}") {
                break;
            }
            if !scanner.take(",") {
                return Err(format!("parse error at {}: expected , or }}", scanner.at));
            }
        }
    }
    if matchers.iter().all(|matcher| matcher.matches(&[])) {
        return Err(
            "queries require at least one regexp or equality matcher that does not have an empty-compatible value"
                .to_owned(),
        );
    }
    let mut filters = Vec::new();
    loop {
        let op = if scanner.take("|=") {
            LineOp::Contains
        } else if scanner.take("!=") {
            LineOp::Lacks
        } else if scanner.take("|~") {
            LineOp::Matches
        } else if scanner.take("!~") {
            LineOp::Misses
        } else {
            break;
        };
        let value = scanner.string()?;
        if matches!(op, LineOp::Matches | LineOp::Misses)
            && eggdb_sql::regex_matches_with_flags("", &value, "s").is_err()
        {
            return Err(format!("invalid regular expression {value:?}"));
        }
        filters.push((op, value));
        if filters.len() > MAX_FILTERS {
            return Err("too many line filters".to_owned());
        }
    }
    scanner.spaces();
    if !scanner.rest().is_empty() {
        return Err(format!(
            "parse error at {}: only stream selectors and line filters are supported in log queries",
            scanner.at
        ));
    }
    Ok(LogQuery { matchers, filters })
}

impl LogQuery {
    /// Whether a line with `labels` and text `line` is in the result.
    pub(crate) fn keeps(&self, labels: &[(String, String)], line: &str) -> bool {
        self.matchers.iter().all(|matcher| matcher.matches(labels))
            && self.filters.iter().all(|(op, value)| match op {
                LineOp::Contains => line.contains(value.as_str()),
                LineOp::Lacks => !line.contains(value.as_str()),
                LineOp::Matches => {
                    eggdb_sql::regex_matches_with_flags(line, value, "s").unwrap_or(false)
                }
                LineOp::Misses => {
                    !eggdb_sql::regex_matches_with_flags(line, value, "s").unwrap_or(false)
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_and_filters_choose_lines() {
        let query = parse(r#"{job="api", level=~"warn|error"} |= "disk" != "ok""#).unwrap();
        let labels = vec![
            ("job".to_owned(), "api".to_owned()),
            ("level".to_owned(), "warn".to_owned()),
        ];
        assert!(query.keeps(&labels, "disk almost full"));
        assert!(!query.keeps(&labels, "disk ok"));
        assert!(parse(r#"{job=~".*"}"#).is_err());
        assert!(parse(r#"{job="api"} | json"#).is_err());
    }
}
