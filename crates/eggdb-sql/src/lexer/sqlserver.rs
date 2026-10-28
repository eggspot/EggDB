use super::{Lexer, MAX_IDENTIFIER_BYTES, TokenKind, unterminated};
use crate::error::{SqlError, SqlResult, SqlState};
use std::borrow::Cow;

const SQLSERVER_OPERATORS: [(&str, &str); 14] = [
    ("<>", "<>"),
    ("!=", "<>"),
    ("!<", ">="),
    ("!>", "<="),
    ("<=", "<="),
    (">=", ">="),
    ("=", "="),
    ("<", "<"),
    (">", ">"),
    ("+", "+"),
    ("-", "-"),
    ("*", "*"),
    ("/", "/"),
    ("%", "%"),
];

fn is_sqlserver_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b'#' || byte >= 0x80
}

fn is_sqlserver_identifier_part(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'@' | b'#' | b'$') || byte >= 0x80
}

fn identifier_too_long(name: &str, position: usize) -> SqlError {
    let start: String = name.chars().take(30).collect();
    SqlError::new(
        SqlState::NAME_TOO_LONG,
        format!(
            "The identifier that starts with '{start}' is too long. Maximum length is {MAX_IDENTIFIER_BYTES} bytes in EggDB."
        ),
    )
    .at(position)
}

fn incorrect_syntax(near: &str, position: usize) -> SqlError {
    SqlError::syntax(format!("Incorrect syntax near '{near}'."), position)
}

impl<'t> Lexer<'t> {
    pub(super) fn next_sqlserver_kind(&mut self, byte: u8) -> SqlResult<TokenKind<'t>> {
        match byte {
            b'.' if self.peek(1).is_some_and(|next| next.is_ascii_digit()) => self.number(),
            b'.' => {
                self.at += 1;
                Ok(TokenKind::Dot)
            }
            b'0' if matches!(self.peek(1), Some(b'x' | b'X')) => Err(SqlError::not_supported(
                "binary literals (0x...) are not supported in this version of EggDB",
            )
            .at(self.at)),
            b'0'..=b'9' => self.number(),
            b'\'' => self.string(false),
            b'n' | b'N' if self.peek(1) == Some(b'\'') => {
                self.at += 1;
                self.string(false)
            }
            b'"' => self.delimited_sqlserver_identifier(b'"'),
            b'[' => self.delimited_sqlserver_identifier(b']'),
            b'@' => self.sqlserver_variable(),
            b'#' => {
                let start = self.at;
                let _ = self.sqlserver_word();
                Err(SqlError::not_supported(
                    "temporary tables (#name) are not supported in this version of EggDB",
                )
                .at(start))
            }
            _ if is_sqlserver_identifier_start(byte) => self.sqlserver_word(),
            _ => {
                let rest = &self.text[self.at..];
                if let Some((spelled, meaning)) = SQLSERVER_OPERATORS
                    .iter()
                    .find(|(spelled, _)| rest.starts_with(*spelled))
                {
                    self.at += spelled.len();
                    return Ok(TokenKind::Operator(meaning));
                }
                let character = rest.chars().next().unwrap_or('?');
                Err(incorrect_syntax(&character.to_string(), self.at))
            }
        }
    }

    fn sqlserver_word(&mut self) -> SqlResult<TokenKind<'t>> {
        let start = self.at;
        while self.peek(0).is_some_and(is_sqlserver_identifier_part) {
            self.at += 1;
        }
        let text = &self.text[start..self.at];
        if text.len() > MAX_IDENTIFIER_BYTES {
            return Err(identifier_too_long(text, start));
        }
        if !text.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return Ok(TokenKind::Word(Cow::Borrowed(text)));
        }
        Ok(TokenKind::Word(Cow::Owned(text.to_lowercase())))
    }

    fn sqlserver_variable(&mut self) -> SqlResult<TokenKind<'t>> {
        let start = self.at;
        self.at += 1;
        if self.peek(0) == Some(b'@') {
            self.at += 1;
        }
        let name_start = self.at;
        while self.peek(0).is_some_and(is_sqlserver_identifier_part) {
            self.at += 1;
        }
        if self.at == name_start {
            return Err(incorrect_syntax(&self.text[start..self.at], start));
        }
        if self.at - start > MAX_IDENTIFIER_BYTES {
            return Err(identifier_too_long(&self.text[start..self.at], start));
        }
        Ok(TokenKind::Variable(Cow::Owned(
            self.text[start..self.at].to_lowercase(),
        )))
    }

    fn delimited_sqlserver_identifier(&mut self, closing: u8) -> SqlResult<TokenKind<'t>> {
        let start = self.at;
        self.at += 1;
        let mut name = String::new();
        loop {
            let Some(character) = self
                .text
                .get(self.at..)
                .and_then(|rest| rest.chars().next())
            else {
                return Err(unterminated(self.text, start, "quoted identifier"));
            };
            self.at += character.len_utf8();
            if character == char::from(closing) {
                if self.peek(0) == Some(closing) {
                    self.at += 1;
                    name.push(character);
                } else {
                    break;
                }
            } else {
                name.push(character);
            }
        }
        if name.is_empty() {
            let shown = if closing == b']' { "[]" } else { "\"\"" };
            return Err(SqlError::syntax(
                format!("An object or column name is missing or empty near '{shown}'."),
                start,
            ));
        }
        if name.len() > MAX_IDENTIFIER_BYTES {
            return Err(identifier_too_long(&name, start));
        }
        Ok(TokenKind::QuotedIdentifier(name.to_lowercase()))
    }
}

#[cfg(test)]
mod tests {
    use crate::dialect::Dialect;
    use crate::lexer::{TokenKind, tokenize_in};

    fn kinds(text: &str) -> Vec<TokenKind<'_>> {
        tokenize_in(text, Dialect::SqlServer)
            .expect("tokenizes")
            .into_iter()
            .map(|token| token.kind)
            .collect()
    }

    #[test]
    fn brackets_and_double_quotes_name_case_insensitively_and_strings_take_n_prefixes() {
        assert_eq!(
            kinds("SELECT [Order Details].[Unit]]Price], \"Qty\", N'caf\u{e9}' FROM t"),
            vec![
                TokenKind::Word("select".into()),
                TokenKind::QuotedIdentifier("order details".into()),
                TokenKind::Dot,
                TokenKind::QuotedIdentifier("unit]price".into()),
                TokenKind::Comma,
                TokenKind::QuotedIdentifier("qty".into()),
                TokenKind::Comma,
                TokenKind::String("caf\u{e9}".into()),
                TokenKind::Word("from".into()),
                TokenKind::Word("t".into()),
                TokenKind::End,
            ]
        );
    }

    #[test]
    fn variables_comments_and_the_allowed_operators_lex() {
        assert_eq!(
            kinds("@@VERSION /* a /* nested */ comment */ -- line\n a !< b != c"),
            vec![
                TokenKind::Variable("@@version".into()),
                TokenKind::Word("a".into()),
                TokenKind::Operator(">="),
                TokenKind::Word("b".into()),
                TokenKind::Operator("<>"),
                TokenKind::Word("c".into()),
                TokenKind::End,
            ]
        );
    }

    #[test]
    fn operators_outside_transact_sql_are_syntax_errors() {
        for text in [
            "SELECT 2 ^ 3",
            "SELECT 'a' || 'b'",
            "SELECT 1::int",
            "SELECT ~1",
            "SELECT $1",
            "SELECT 1 & 2",
        ] {
            let error = tokenize_in(text, Dialect::SqlServer).expect_err(text);
            assert_eq!(error.state().code(), "42601", "{text}");
            assert!(error.message().starts_with("Incorrect syntax near"), "{text}");
        }
    }

    #[test]
    fn temporary_tables_binary_literals_and_long_names_are_refused() {
        for text in ["SELECT * FROM #t", "SELECT 0x0102"] {
            let error = tokenize_in(text, Dialect::SqlServer).expect_err(text);
            assert_eq!(error.state().code(), "0A000", "{text}");
        }
        let long = format!("SELECT [{}]", "x".repeat(64));
        let error = tokenize_in(&long, Dialect::SqlServer).expect_err("long");
        assert_eq!(error.state().code(), "42622");
        assert!(tokenize_in("SELECT [abc", Dialect::SqlServer).is_err());
        assert!(tokenize_in("SELECT []", Dialect::SqlServer).is_err());
    }
}
