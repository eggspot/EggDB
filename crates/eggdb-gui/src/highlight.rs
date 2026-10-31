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
    "ADD", "ALL", "ALTER", "AND", "AS", "ASC", "BEGIN", "BETWEEN", "BY", "CASE", "COMMIT",
    "CREATE", "CROSS", "DEFAULT", "DELETE", "DESC", "DISTINCT", "DROP", "ELSE", "END", "EXISTS",
    "EXPLAIN", "FALSE", "FOREIGN", "FROM", "FULL", "GROUP", "HAVING", "IN", "INDEX", "INNER",
    "INSERT", "INTO", "IS", "JOIN", "KEY", "LEFT", "LIKE", "LIMIT", "NOT", "NULL", "OFFSET", "ON",
    "OR", "ORDER", "OUTER", "PRIMARY", "REFERENCES", "RETURNING", "RIGHT", "ROLLBACK", "SELECT",
    "SET", "TABLE", "THEN", "TRUE", "UNION", "UNIQUE", "UPDATE", "USING", "VALUES", "VIEW",
    "WHEN", "WHERE", "WITH",
];

fn is_keyword(word: &str) -> bool {
    KEYWORDS
        .binary_search(&word.to_ascii_uppercase().as_str())
        .is_ok()
}

/// Splits one line of SQL into runs of the same kind, as byte ranges; `in_comment` says whether
/// a `/* */` comment is open at the line's start, and is updated for the next line.
pub fn tokens(line: &str, in_comment: &mut bool) -> Vec<(std::ops::Range<usize>, TokenKind)> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let start = at;
        let kind = if *in_comment {
            match line[at..].find("*/") {
                Some(end) => {
                    at += end + 2;
                    *in_comment = false;
                }
                None => at = bytes.len(),
            }
            TokenKind::Comment
        } else if line[at..].starts_with("--") {
            at = bytes.len();
            TokenKind::Comment
        } else if line[at..].starts_with("/*") {
            *in_comment = true;
            at += 2;
            match line[at..].find("*/") {
                Some(end) => {
                    at += end + 2;
                    *in_comment = false;
                }
                None => at = bytes.len(),
            }
            TokenKind::Comment
        } else if bytes[at] == b'\'' {
            at += 1;
            while at < bytes.len() {
                if bytes[at] == b'\'' {
                    if bytes.get(at + 1) == Some(&b'\'') {
                        at += 2;
                        continue;
                    }
                    at += 1;
                    break;
                }
                at += 1;
            }
            TokenKind::String
        } else if bytes[at].is_ascii_digit() {
            while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'.') {
                at += 1;
            }
            TokenKind::Number
        } else if bytes[at].is_ascii_alphabetic() || bytes[at] == b'_' {
            while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
                at += 1;
            }
            if is_keyword(&line[start..at]) {
                TokenKind::Keyword
            } else {
                TokenKind::Plain
            }
        } else {
            let width = line[at..].chars().next().map_or(1, char::len_utf8);
            at += width;
            TokenKind::Plain
        };
        match out.last_mut() {
            Some((range, last)) if *last == kind && kind == TokenKind::Plain => range.end = at,
            _ => out.push((start..at, kind)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_strings_numbers_and_comments_are_told_apart() {
        let mut open = false;
        let line = "SELECT 'it''s', 42 FROM t -- note";
        let kinds: Vec<(&str, TokenKind)> = tokens(line, &mut open)
            .into_iter()
            .map(|(range, kind)| (&line[range], kind))
            .collect();
        assert_eq!(kinds[0], ("SELECT", TokenKind::Keyword));
        assert!(kinds.contains(&("'it''s'", TokenKind::String)));
        assert!(kinds.contains(&("42", TokenKind::Number)));
        assert!(kinds.contains(&("FROM", TokenKind::Keyword)));
        assert_eq!(kinds.last(), Some(&("-- note", TokenKind::Comment)));
        assert!(KEYWORDS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn a_block_comment_carries_over_lines() {
        let mut open = false;
        assert_eq!(tokens("a /* b", &mut open).last().map(|(_, kind)| *kind), Some(TokenKind::Comment));
        assert!(open);
        let next = tokens("c */ d", &mut open);
        assert_eq!(next[0].1, TokenKind::Comment);
        assert!(!open);
        let _ = tokens("é'unterminated", &mut open);
    }
}
