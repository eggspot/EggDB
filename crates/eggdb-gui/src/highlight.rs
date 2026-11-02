use std::ops::Range;

use crate::dialect::QueryDialect;

/// What a run of SQL text is, for its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// A keyword of the dialect.
    Keyword,
    /// A string literal.
    String,
    /// A number.
    Number,
    /// A comment.
    Comment,
    /// Anything else.
    Plain,
}

const KEYWORDS: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "AND",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASE",
    "COMMIT",
    "CREATE",
    "CROSS",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EXISTS",
    "EXPLAIN",
    "FALSE",
    "FOREIGN",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "PRIMARY",
    "REFERENCES",
    "RETURNING",
    "RIGHT",
    "ROLLBACK",
    "SELECT",
    "SET",
    "TABLE",
    "THEN",
    "TRUE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WITH",
];

const POSTGRESQL_KEYWORDS: &[&str] = &["ILIKE", "LATERAL", "RETURNS", "SERIAL"];
const MYSQL_KEYWORDS: &[&str] = &["AUTO_INCREMENT", "DESCRIBE", "ENGINE", "SHOW", "UNSIGNED"];
const SQL_SERVER_KEYWORDS: &[&str] = &["DECLARE", "EXEC", "GO", "IDENTITY", "NVARCHAR", "TOP"];
const ORACLE_KEYWORDS: &[&str] = &["CONNECT", "MINUS", "NUMBER", "ROWNUM", "VARCHAR2"];
const SQLITE_KEYWORDS: &[&str] = &["AUTOINCREMENT", "GLOB", "PRAGMA", "ROWID", "VACUUM"];

fn dialect_keywords(dialect: QueryDialect) -> &'static [&'static str] {
    match dialect {
        QueryDialect::PostgreSql => POSTGRESQL_KEYWORDS,
        QueryDialect::MySql => MYSQL_KEYWORDS,
        QueryDialect::SqlServer => SQL_SERVER_KEYWORDS,
        QueryDialect::Oracle => ORACLE_KEYWORDS,
        QueryDialect::Sqlite => SQLITE_KEYWORDS,
    }
}

/// Whether `word` is one of `dialect`'s keywords, in any case.
pub fn is_keyword(word: &str, dialect: QueryDialect) -> bool {
    let upper = word.to_ascii_uppercase();
    KEYWORDS.binary_search(&upper.as_str()).is_ok()
        || dialect_keywords(dialect).contains(&upper.as_str())
}

/// What is still open where a line ends, so the next line goes on lexing inside it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Carry {
    /// Nothing: the next line starts in plain SQL.
    #[default]
    Code,
    /// A `/* */` comment, `depth` deep where the dialect nests them.
    Comment {
        /// How many comments are open.
        depth: u32,
    },
    /// A literal or quoted name opened by this quote byte (`'`, `"` or a backtick).
    Quoted(u8),
    /// A SQL Server or SQLite `[bracketed]` name.
    Bracketed,
    /// A PostgreSQL dollar-quoted body, closed by this tag (`$$` or `$body$`).
    Dollar(String),
}

/// What one lexed run of SQL is: the highlighter colours them and the statement splitter
/// looks for the semicolons among them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lexeme {
    Keyword,
    Word,
    String,
    QuotedName,
    Number,
    Comment,
    Semicolon,
    Other,
}

fn nests_comments(dialect: QueryDialect) -> bool {
    matches!(dialect, QueryDialect::PostgreSql | QueryDialect::SqlServer)
}

fn double_quote_is_string(dialect: QueryDialect) -> bool {
    dialect == QueryDialect::MySql
}

fn backslash_escapes(dialect: QueryDialect, quote: u8) -> bool {
    dialect == QueryDialect::MySql && quote != b'`'
}

fn quote_lexeme(dialect: QueryDialect, quote: u8) -> Lexeme {
    match quote {
        b'\'' => Lexeme::String,
        b'"' if double_quote_is_string(dialect) => Lexeme::String,
        _ => Lexeme::QuotedName,
    }
}

fn opens_quote(dialect: QueryDialect, byte: u8) -> bool {
    match byte {
        b'\'' | b'"' => true,
        b'`' => matches!(dialect, QueryDialect::MySql | QueryDialect::Sqlite),
        _ => false,
    }
}

fn opens_bracket(dialect: QueryDialect, byte: u8) -> bool {
    byte == b'[' && matches!(dialect, QueryDialect::SqlServer | QueryDialect::Sqlite)
}

fn line_comment_at(text: &str, at: usize, dialect: QueryDialect) -> bool {
    let rest = &text.as_bytes()[at..];
    match rest {
        [b'-', b'-', next, ..] if dialect == QueryDialect::MySql => next.is_ascii_whitespace(),
        [b'-', b'-', ..] => true,
        [b'#', ..] => dialect == QueryDialect::MySql,
        _ => false,
    }
}

fn dollar_tag_at(text: &str, at: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    if bytes.get(at) != Some(&b'$') {
        return None;
    }
    let mut end = at + 1;
    while let Some(&byte) = bytes.get(end) {
        if byte == b'$' {
            return Some(&text[at..=end]);
        }
        let allowed = byte.is_ascii_alphabetic() || byte == b'_' || (end > at + 1 && byte.is_ascii_digit());
        if !allowed {
            return None;
        }
        end += 1;
    }
    None
}

fn next_char_end(text: &str, at: usize) -> usize {
    at + text[at..].chars().next().map_or(1, char::len_utf8)
}

fn word_byte(byte: u8, dialect: QueryDialect) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || (byte == b'$' && dialect != QueryDialect::PostgreSql)
}

fn starts_word(byte: u8, dialect: QueryDialect) -> bool {
    byte.is_ascii_alphabetic()
        || byte == b'_'
        || (dialect == QueryDialect::SqlServer && matches!(byte, b'@' | b'#'))
}

fn continue_carry(text: &str, at: usize, dialect: QueryDialect, carry: &mut Carry) -> (usize, Lexeme) {
    let bytes = text.as_bytes();
    match carry.clone() {
        Carry::Code => (at, Lexeme::Other),
        Carry::Comment { mut depth } => {
            let mut at = at;
            while at < bytes.len() {
                if bytes[at..].starts_with(b"*/") {
                    at += 2;
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        *carry = Carry::Code;
                        return (at, Lexeme::Comment);
                    }
                } else if nests_comments(dialect) && bytes[at..].starts_with(b"/*") {
                    at += 2;
                    depth = depth.saturating_add(1);
                } else {
                    at = next_char_end(text, at);
                }
            }
            *carry = Carry::Comment { depth };
            (at, Lexeme::Comment)
        }
        Carry::Quoted(quote) => {
            let mut at = at;
            while at < bytes.len() {
                let byte = bytes[at];
                if byte == b'\\' && backslash_escapes(dialect, quote) {
                    at = (at + 2).min(bytes.len());
                    if at < bytes.len() && !text.is_char_boundary(at) {
                        at = next_char_end(text, at - 1);
                    }
                    continue;
                }
                if byte == quote {
                    if bytes.get(at + 1) == Some(&quote) {
                        at += 2;
                        continue;
                    }
                    *carry = Carry::Code;
                    return (at + 1, quote_lexeme(dialect, quote));
                }
                at = next_char_end(text, at);
            }
            (at, quote_lexeme(dialect, quote))
        }
        Carry::Bracketed => {
            let mut at = at;
            while at < bytes.len() {
                if bytes[at] == b']' {
                    if bytes.get(at + 1) == Some(&b']') {
                        at += 2;
                        continue;
                    }
                    *carry = Carry::Code;
                    return (at + 1, Lexeme::QuotedName);
                }
                at = next_char_end(text, at);
            }
            (at, Lexeme::QuotedName)
        }
        Carry::Dollar(tag) => match text[at..].find(tag.as_str()) {
            Some(found) => {
                *carry = Carry::Code;
                (at + found + tag.len(), Lexeme::String)
            }
            None => (bytes.len(), Lexeme::String),
        },
    }
}

/// Lexes `text` in `dialect`, starting inside whatever `carry` says is open, and calls `emit`
/// with each run's byte range and kind; `carry` is left saying what is open at the end.
pub(crate) fn lex(
    text: &str,
    dialect: QueryDialect,
    carry: &mut Carry,
    emit: &mut dyn FnMut(Range<usize>, Lexeme),
) {
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let start = at;
        if *carry != Carry::Code {
            let (end, lexeme) = continue_carry(text, at, dialect, carry);
            emit(start..end, lexeme);
            at = end.max(start + 1);
            continue;
        }
        let byte = bytes[at];
        let lexeme = if line_comment_at(text, at, dialect) {
            at = text[at..].find('\n').map_or(bytes.len(), |end| at + end);
            Lexeme::Comment
        } else if bytes[at..].starts_with(b"/*") {
            *carry = Carry::Comment { depth: 1 };
            let (end, lexeme) = continue_carry(text, at + 2, dialect, carry);
            at = end;
            lexeme
        } else if opens_quote(dialect, byte) {
            *carry = Carry::Quoted(byte);
            let (end, lexeme) = continue_carry(text, at + 1, dialect, carry);
            at = end;
            lexeme
        } else if opens_bracket(dialect, byte) {
            *carry = Carry::Bracketed;
            let (end, lexeme) = continue_carry(text, at + 1, dialect, carry);
            at = end;
            lexeme
        } else if let Some(tag) = (dialect == QueryDialect::PostgreSql)
            .then(|| dollar_tag_at(text, at))
            .flatten()
        {
            let tag = tag.to_owned();
            at += tag.len();
            *carry = Carry::Dollar(tag);
            let (end, lexeme) = continue_carry(text, at, dialect, carry);
            at = end;
            lexeme
        } else if byte.is_ascii_digit() {
            while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'.') {
                at += 1;
            }
            Lexeme::Number
        } else if starts_word(byte, dialect) {
            at += 1;
            while at < bytes.len() && word_byte(bytes[at], dialect) {
                at += 1;
            }
            if is_keyword(&text[start..at], dialect) {
                Lexeme::Keyword
            } else {
                Lexeme::Word
            }
        } else if byte == b';' {
            at += 1;
            Lexeme::Semicolon
        } else {
            at = next_char_end(text, at);
            Lexeme::Other
        };
        emit(start..at, lexeme);
    }
}

/// Splits one line of SQL in `dialect` into runs of the same kind, as byte ranges; `carry`
/// says what a previous line left open and is updated for the next line.
pub fn tokens(line: &str, dialect: QueryDialect, carry: &mut Carry) -> Vec<(Range<usize>, TokenKind)> {
    let mut out: Vec<(Range<usize>, TokenKind)> = Vec::new();
    lex(line, dialect, carry, &mut |range, lexeme| {
        let kind = match lexeme {
            Lexeme::Keyword => TokenKind::Keyword,
            Lexeme::String => TokenKind::String,
            Lexeme::Number => TokenKind::Number,
            Lexeme::Comment => TokenKind::Comment,
            Lexeme::Word | Lexeme::QuotedName | Lexeme::Semicolon | Lexeme::Other => TokenKind::Plain,
        };
        match out.last_mut() {
            Some((last, previous)) if *previous == kind && kind == TokenKind::Plain => last.end = range.end,
            _ => out.push((range, kind)),
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(line: &str, dialect: QueryDialect) -> Vec<(&str, TokenKind)> {
        let mut carry = Carry::Code;
        tokens(line, dialect, &mut carry)
            .into_iter()
            .map(|(range, kind)| (&line[range], kind))
            .collect()
    }

    #[test]
    fn keywords_strings_numbers_and_comments_are_told_apart() {
        let found = kinds("SELECT 'it''s', 42 FROM t -- note", QueryDialect::PostgreSql);
        assert_eq!(found[0], ("SELECT", TokenKind::Keyword));
        assert!(found.contains(&("'it''s'", TokenKind::String)));
        assert!(found.contains(&("42", TokenKind::Number)));
        assert!(found.contains(&("FROM", TokenKind::Keyword)));
        assert_eq!(found.last(), Some(&("-- note", TokenKind::Comment)));
        assert!(KEYWORDS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn a_block_comment_carries_over_lines() {
        let mut carry = Carry::Code;
        let dialect = QueryDialect::PostgreSql;
        assert_eq!(
            tokens("a /* b", dialect, &mut carry).last().map(|(_, kind)| *kind),
            Some(TokenKind::Comment)
        );
        assert_eq!(carry, Carry::Comment { depth: 1 });
        let next = tokens("c */ d", dialect, &mut carry);
        assert_eq!(next[0].1, TokenKind::Comment);
        assert_eq!(carry, Carry::Code);
        let _ = tokens("é'unterminated", dialect, &mut carry);
        assert_eq!(carry, Carry::Quoted(b'\''));
    }

    #[test]
    fn each_dialect_lexes_its_own_quotes_and_keywords() {
        let line = "SELECT `order` FROM t LIMIT 1";
        let postgres = kinds(line, QueryDialect::PostgreSql);
        assert!(postgres.contains(&("LIMIT", TokenKind::Keyword)));
        assert!(postgres.contains(&("order", TokenKind::Keyword)));
        let mysql = kinds(line, QueryDialect::MySql);
        assert!(mysql.contains(&("LIMIT", TokenKind::Keyword)));
        assert!(mysql.iter().all(|(text, kind)| !text.contains("order") || *kind == TokenKind::Plain));
        assert!(kinds("SELECT TOP 1 [order]", QueryDialect::SqlServer).contains(&("TOP", TokenKind::Keyword)));
        assert!(!kinds("SELECT TOP 1", QueryDialect::PostgreSql).contains(&("TOP", TokenKind::Keyword)));
        assert_eq!(kinds("# note", QueryDialect::MySql), vec![("# note", TokenKind::Comment)]);
        assert!(kinds("SELECT \"a\"", QueryDialect::MySql).contains(&("\"a\"", TokenKind::String)));
        assert!(!kinds("SELECT \"a\"", QueryDialect::PostgreSql).contains(&("\"a\"", TokenKind::String)));
        assert!(kinds("SELECT $$a;b$$", QueryDialect::PostgreSql).contains(&("$$a;b$$", TokenKind::String)));
        let mut carry = Carry::Code;
        let _ = tokens("/* a /* b */", QueryDialect::PostgreSql, &mut carry);
        assert_eq!(carry, Carry::Comment { depth: 1 });
        let mut carry = Carry::Code;
        let _ = tokens("/* a /* b */", QueryDialect::MySql, &mut carry);
        assert_eq!(carry, Carry::Code);
    }

    #[test]
    fn hostile_bytes_never_stop_the_lexer() {
        for line in ["'\\", "\"é", "[é", "$a", "$$", "/*/", "--", "é$tag$x", "\\'é", "`"] {
            for dialect in QueryDialect::ALL {
                let mut carry = Carry::Code;
                let runs = tokens(line, dialect, &mut carry);
                let covered: usize = runs.iter().map(|(range, _)| range.len()).sum();
                assert_eq!(covered, line.len(), "{line:?} in {dialect:?}");
            }
        }
    }
}
