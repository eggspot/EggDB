mod oracle;
mod sqlite;

pub(crate) use oracle::swap_uniform_case;

use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use std::borrow::Cow;

pub(crate) const MAX_IDENTIFIER_BYTES: usize = 63;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TokenKind<'t> {
    Word(Cow<'t, str>),
    QuotedIdentifier(String),
    Integer(&'t str),
    Decimal(&'t str),
    String(Cow<'t, str>),
    Parameter(&'t str),
    Placeholder,
    NumberedParameter(usize),
    Variable(Cow<'t, str>),
    Operator(&'t str),
    LeftParen,
    RightParen,
    LeftBracket,
    RightBracket,
    Comma,
    Semicolon,
    Dot,
    Colon,
    DoubleColon,
    End,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Token<'t> {
    pub(crate) kind: TokenKind<'t>,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

pub(crate) fn tokenize(text: &str) -> SqlResult<Vec<Token<'_>>> {
    tokenize_in(text, Dialect::PostgreSql)
}

pub(crate) fn tokenize_in(text: &str, dialect: Dialect) -> SqlResult<Vec<Token<'_>>> {
    let mut lexer = Lexer {
        text,
        bytes: text.as_bytes(),
        at: 0,
        dialect,
        open_executable_comments: 0,
        named_parameters: Vec::new(),
        highest_parameter: 0,
    };
    let mut tokens = Vec::with_capacity(text.len() / 4 + 2);
    loop {
        lexer.skip_space_and_comments()?;
        let start = lexer.at;
        let kind = lexer.next_kind()?;
        let done = kind == TokenKind::End;
        tokens.push(Token {
            kind,
            start,
            end: lexer.at,
        });
        if done {
            return Ok(tokens);
        }
    }
}

struct Lexer<'t> {
    text: &'t str,
    bytes: &'t [u8],
    at: usize,
    dialect: Dialect,
    open_executable_comments: usize,
    named_parameters: Vec<(&'t str, usize)>,
    highest_parameter: usize,
}

const MYSQL_OPERATORS: [&str; 22] = [
    "<=>", "<=", ">=", "<>", "!=", "<<", ">>", "&&", "||", ":=", "->", "=", "<", ">", "+", "-",
    "*", "/", "%", "!", "&", "|",
];

fn starts_mysql_comment_after_dashes(next: Option<u8>) -> bool {
    next.is_none_or(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80
}

fn is_identifier_part(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit() || byte == b'$'
}

fn is_operator_char(byte: u8) -> bool {
    b"+-*/<>=~!@#%^&|`?".contains(&byte)
}

fn truncated_identifier(mut name: String) -> String {
    if name.len() > MAX_IDENTIFIER_BYTES {
        let mut cut = MAX_IDENTIFIER_BYTES;
        while !name.is_char_boundary(cut) {
            cut -= 1;
        }
        name.truncate(cut);
    }
    name
}

impl<'t> Lexer<'t> {
    fn peek(&self, ahead: usize) -> Option<u8> {
        self.bytes.get(self.at + ahead).copied()
    }

    fn skip_space_and_comments(&mut self) -> SqlResult<()> {
        if matches!(self.dialect, Dialect::Sqlite | Dialect::Oracle) {
            self.skip_sqlite_space_and_comments();
            return Ok(());
        }
        let mysql = self.dialect == Dialect::MySql;
        loop {
            match (self.peek(0), self.peek(1)) {
                (Some(byte), _) if byte.is_ascii_whitespace() => self.at += 1,
                (Some(b'*'), Some(b'/')) if self.open_executable_comments > 0 => {
                    self.open_executable_comments -= 1;
                    self.at += 2;
                }
                (Some(b'/'), Some(b'*')) if mysql && self.peek(2) == Some(b'!') => {
                    self.at += 3;
                    self.digits();
                    self.open_executable_comments += 1;
                }
                (Some(b'-'), Some(b'-'))
                    if mysql && !starts_mysql_comment_after_dashes(self.peek(2)) =>
                {
                    return Ok(());
                }
                (Some(b'#'), _) if mysql => self.skip_line(),
                (Some(b'-'), Some(b'-')) => {
                    while let Some(byte) = self.peek(0) {
                        if byte == b'\n' {
                            break;
                        }
                        self.at += 1;
                    }
                }
                (Some(b'/'), Some(b'*')) => self.skip_block_comment()?,
                _ => return Ok(()),
            }
        }
    }

    fn skip_line(&mut self) {
        while let Some(byte) = self.peek(0) {
            if byte == b'\n' {
                break;
            }
            self.at += 1;
        }
    }

    fn skip_block_comment(&mut self) -> SqlResult<()> {
        let start = self.at;
        let mut depth = 0usize;
        loop {
            match (self.peek(0), self.peek(1)) {
                (Some(b'/'), Some(b'*')) => {
                    depth += 1;
                    self.at += 2;
                }
                (Some(b'*'), Some(b'/')) => {
                    depth -= 1;
                    self.at += 2;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                (Some(_), _) => self.at += 1,
                (None, _) => {
                    return Err(SqlError::syntax(
                        "unterminated /* comment at or near \"/*\"",
                        start,
                    ));
                }
            }
        }
    }

    fn next_kind(&mut self) -> SqlResult<TokenKind<'t>> {
        let Some(byte) = self.peek(0) else {
            return Ok(TokenKind::End);
        };
        if self.dialect == Dialect::Sqlite && byte == b'[' {
            return self.bracket_identifier();
        }
        let single = match byte {
            b'(' => Some(TokenKind::LeftParen),
            b')' => Some(TokenKind::RightParen),
            b'[' => Some(TokenKind::LeftBracket),
            b']' => Some(TokenKind::RightBracket),
            b',' => Some(TokenKind::Comma),
            b';' => Some(TokenKind::Semicolon),
            _ => None,
        };
        if let Some(kind) = single {
            self.at += 1;
            return Ok(kind);
        }
        if self.dialect == Dialect::MySql {
            return self.next_mysql_kind(byte);
        }
        if self.dialect == Dialect::Sqlite {
            return self.next_sqlite_kind(byte);
        }
        if self.dialect == Dialect::Oracle {
            return self.next_oracle_kind(byte);
        }

        match byte {
            b':' if self.peek(1) == Some(b':') => {
                self.at += 2;
                Ok(TokenKind::DoubleColon)
            }
            b':' => {
                self.at += 1;
                Ok(TokenKind::Colon)
            }
            b'.' if self.peek(1).is_some_and(|next| next.is_ascii_digit()) => self.number(),
            b'.' => {
                self.at += 1;
                Ok(TokenKind::Dot)
            }
            b'0'..=b'9' => self.number(),
            b'\'' => self.string(false),
            b'"' => self.quoted_identifier(),
            b'$' => self.dollar(),
            b'e' | b'E' if self.peek(1) == Some(b'\'') => {
                self.at += 1;
                self.string(true)
            }
            _ if is_identifier_start(byte) => Ok(self.word()),
            _ if is_operator_char(byte) => Ok(self.operator()),
            _ => {
                let character = self.text[self.at..].chars().next().unwrap_or('?');
                Err(SqlError::syntax(
                    format!("syntax error at or near \"{character}\""),
                    self.at,
                ))
            }
        }
    }

    fn next_mysql_kind(&mut self, byte: u8) -> SqlResult<TokenKind<'t>> {
        match byte {
            b'.' if self.peek(1).is_some_and(|next| next.is_ascii_digit()) => self.number(),
            b'.' => {
                self.at += 1;
                Ok(TokenKind::Dot)
            }
            b'0'..=b'9' => self.number(),
            b'\'' | b'"' => self.mysql_string(byte),
            b'`' => self.backtick_identifier(),
            b'?' => {
                self.at += 1;
                Ok(TokenKind::Placeholder)
            }
            b'@' => self.variable(),
            b':' if self.peek(1) != Some(b'=') => {
                self.at += 1;
                Ok(TokenKind::Colon)
            }
            _ if is_identifier_start(byte) => Ok(self.word()),
            _ => {
                let rest = &self.text[self.at..];
                if let Some(operator) = MYSQL_OPERATORS
                    .iter()
                    .find(|operator| rest.starts_with(**operator))
                {
                    let operator = &self.text[self.at..self.at + operator.len()];
                    self.at += operator.len();
                    return Ok(TokenKind::Operator(operator));
                }
                let character = rest.chars().next().unwrap_or('?');
                Err(SqlError::syntax(
                    format!("syntax error at or near \"{character}\""),
                    self.at,
                ))
            }
        }
    }

    fn mysql_string(&mut self, quote: u8) -> SqlResult<TokenKind<'t>> {
        let start = self.at;
        self.at += 1;
        if let Some(length) = self.bytes[self.at..]
            .iter()
            .position(|&byte| byte == quote || byte == b'\\')
            && self.bytes[self.at + length] == quote
            && self.bytes.get(self.at + length + 1) != Some(&quote)
        {
            let value = &self.text[self.at..self.at + length];
            self.at += length + 1;
            return Ok(TokenKind::String(Cow::Borrowed(value)));
        }
        let mut value = String::new();
        loop {
            let Some(character) = self
                .text
                .get(self.at..)
                .and_then(|rest| rest.chars().next())
            else {
                return Err(unterminated(self.text, start, "quoted string"));
            };
            self.at += character.len_utf8();
            if character == char::from(quote) {
                if self.peek(0) == Some(quote) {
                    self.at += 1;
                    value.push(character);
                    continue;
                }
                return Ok(TokenKind::String(Cow::Owned(value)));
            }
            if character != '\\' {
                value.push(character);
                continue;
            }
            let Some(escaped) = self
                .text
                .get(self.at..)
                .and_then(|rest| rest.chars().next())
            else {
                return Err(unterminated(self.text, start, "quoted string"));
            };
            self.at += escaped.len_utf8();
            match escaped {
                '0' => value.push('\0'),
                'b' => value.push('\u{8}'),
                'n' => value.push('\n'),
                'r' => value.push('\r'),
                't' => value.push('\t'),
                'Z' => value.push('\u{1a}'),
                '%' | '_' => {
                    value.push('\\');
                    value.push(escaped);
                }
                other => value.push(other),
            }
        }
    }

    fn backtick_identifier(&mut self) -> SqlResult<TokenKind<'t>> {
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
            if character == '`' {
                if self.peek(0) == Some(b'`') {
                    self.at += 1;
                    name.push('`');
                } else {
                    break;
                }
            } else {
                name.push(character);
            }
        }
        if name.is_empty() {
            return Err(SqlError::syntax(
                "zero-length delimited identifier at or near \"``\"",
                start,
            ));
        }
        Ok(TokenKind::QuotedIdentifier(truncated_identifier(
            name.to_lowercase(),
        )))
    }

    fn variable(&mut self) -> SqlResult<TokenKind<'t>> {
        let start = self.at;
        self.at += 1;
        if self.peek(0) == Some(b'@') {
            self.at += 1;
        }
        let name_start = self.at;
        while self.peek(0).is_some_and(is_identifier_part) {
            self.at += 1;
        }
        if self.at == name_start {
            return Err(SqlError::syntax(
                format!("syntax error at or near \"{}\"", &self.text[start..self.at]),
                start,
            ));
        }
        Ok(TokenKind::Variable(Cow::Owned(
            self.text[start..self.at].to_ascii_lowercase(),
        )))
    }

    fn word(&mut self) -> TokenKind<'t> {
        let start = self.at;
        while self.peek(0).is_some_and(is_identifier_part) {
            self.at += 1;
        }
        let text = &self.text[start..self.at];
        if text.len() <= MAX_IDENTIFIER_BYTES && !text.bytes().any(|byte| byte.is_ascii_uppercase())
        {
            return TokenKind::Word(Cow::Borrowed(text));
        }
        TokenKind::Word(Cow::Owned(truncated_identifier(text.to_ascii_lowercase())))
    }

    fn number(&mut self) -> SqlResult<TokenKind<'t>> {
        let start = self.at;
        let mut decimal = false;
        self.digits();
        if self.peek(0) == Some(b'.') && self.peek(1) != Some(b'.') {
            decimal = true;
            self.at += 1;
            self.digits();
        }
        if matches!(self.peek(0), Some(b'e' | b'E')) {
            let sign = usize::from(matches!(self.peek(1), Some(b'+' | b'-')));
            if self
                .peek(1 + sign)
                .is_some_and(|byte| byte.is_ascii_digit())
            {
                decimal = true;
                self.at += 1 + sign;
                self.digits();
            }
        }
        if self.peek(0).is_some_and(is_identifier_part) {
            while self.peek(0).is_some_and(is_identifier_part) {
                self.at += 1;
            }
            return Err(SqlError::syntax(
                format!(
                    "trailing junk after numeric literal at or near \"{}\"",
                    &self.text[start..self.at]
                ),
                start,
            ));
        }
        let text = &self.text[start..self.at];
        Ok(if decimal {
            TokenKind::Decimal(text)
        } else {
            TokenKind::Integer(text)
        })
    }

    fn digits(&mut self) {
        while self.peek(0).is_some_and(|byte| byte.is_ascii_digit()) {
            self.at += 1;
        }
    }

    fn string(&mut self, escapes: bool) -> SqlResult<TokenKind<'t>> {
        let start = self.at;
        self.at += 1;
        if let Some(length) = self.bytes[self.at..]
            .iter()
            .position(|&byte| byte == b'\'' || escapes && byte == b'\\')
            && self.bytes[self.at + length] == b'\''
            && self.bytes.get(self.at + length + 1) != Some(&b'\'')
        {
            let value = &self.text[self.at..self.at + length];
            self.at += length + 1;
            return Ok(TokenKind::String(Cow::Borrowed(value)));
        }
        let mut value = String::new();
        loop {
            let Some(rest) = self.text.get(self.at..) else {
                return Err(unterminated(self.text, start, "quoted string"));
            };
            let mut chars = rest.chars();
            let Some(character) = chars.next() else {
                return Err(unterminated(self.text, start, "quoted string"));
            };
            self.at += character.len_utf8();
            match character {
                '\'' if self.peek(0) == Some(b'\'') => {
                    self.at += 1;
                    value.push('\'');
                }
                '\'' => return Ok(TokenKind::String(Cow::Owned(value))),
                '\\' if escapes => self.escape(&mut value, start)?,
                _ => value.push(character),
            }
        }
    }

    fn escape(&mut self, value: &mut String, start: usize) -> SqlResult<()> {
        let Some(byte) = self.peek(0) else {
            return Err(unterminated(self.text, start, "quoted string"));
        };
        self.at += 1;
        let character = match byte {
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'x' => {
                let mut code = 0u32;
                let mut count = 0;
                while count < 2 {
                    let Some(digit) = self.peek(0).and_then(|byte| (byte as char).to_digit(16))
                    else {
                        break;
                    };
                    code = code * 16 + digit;
                    count += 1;
                    self.at += 1;
                }
                if count == 0 {
                    'x'
                } else {
                    escaped_byte(code, start)?
                }
            }
            b'0'..=b'7' | b'u' | b'U' => {
                return Err(SqlError::not_supported(
                    "octal and Unicode escapes in E'' strings are not supported yet",
                )
                .at(start));
            }
            _ => {
                self.at -= 1;
                let Some(character) = self.text[self.at..].chars().next() else {
                    return Err(unterminated(self.text, start, "quoted string"));
                };
                self.at += character.len_utf8();
                character
            }
        };
        value.push(character);
        Ok(())
    }

    fn quoted_identifier(&mut self) -> SqlResult<TokenKind<'t>> {
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
            if character == '"' {
                if self.peek(0) == Some(b'"') {
                    self.at += 1;
                    name.push('"');
                } else {
                    break;
                }
            } else {
                name.push(character);
            }
        }
        if name.is_empty() {
            return Err(SqlError::syntax(
                "zero-length delimited identifier at or near \"\"\"\"",
                start,
            ));
        }
        Ok(TokenKind::QuotedIdentifier(truncated_identifier(name)))
    }

    fn dollar(&mut self) -> SqlResult<TokenKind<'t>> {
        let start = self.at;
        if self.peek(1).is_some_and(|byte| byte.is_ascii_digit()) {
            self.at += 1;
            let digits_start = self.at;
            self.digits();
            return Ok(TokenKind::Parameter(&self.text[digits_start..self.at]));
        }
        let mut end_of_tag = self.at + 1;
        while self
            .bytes
            .get(end_of_tag)
            .is_some_and(|&byte| is_identifier_part(byte) && byte != b'$')
        {
            end_of_tag += 1;
        }
        if self.bytes.get(end_of_tag) != Some(&b'$') {
            return Err(SqlError::syntax("syntax error at or near \"$\"", start));
        }
        let tag = &self.text[start..=end_of_tag];
        let body_start = end_of_tag + 1;
        let Some(length) = self.text[body_start..].find(tag) else {
            return Err(unterminated(self.text, start, "dollar-quoted string"));
        };
        let value = &self.text[body_start..body_start + length];
        self.at = body_start + length + tag.len();
        Ok(TokenKind::String(Cow::Borrowed(value)))
    }

    fn operator(&mut self) -> TokenKind<'t> {
        let start = self.at;
        let mut end = start;
        while let Some(&byte) = self.bytes.get(end) {
            if !is_operator_char(byte) {
                break;
            }
            let next = self.bytes.get(end + 1).copied();
            if end > start
                && (byte == b'-' && next == Some(b'-') || byte == b'/' && next == Some(b'*'))
            {
                break;
            }
            end += 1;
        }
        let mut text = &self.text[start..end];
        let special = text.bytes().any(|byte| b"~!@#%^&|`?".contains(&byte));
        while text.len() > 1 && !special && (text.ends_with('+') || text.ends_with('-')) {
            text = &text[..text.len() - 1];
        }
        self.at = start + text.len();
        TokenKind::Operator(text)
    }
}

fn escaped_byte(code: u32, start: usize) -> SqlResult<char> {
    match char::from_u32(code) {
        Some(character) if code != 0 && code < 0x80 => Ok(character),
        _ => Err(SqlError::new(
            SqlState::INVALID_ESCAPE_SEQUENCE,
            format!("invalid byte sequence for encoding \"UTF8\": 0x{code:02x}"),
        )
        .at(start)),
    }
}

fn unterminated(text: &str, start: usize, what: &str) -> SqlError {
    let rest: String = text[start..].chars().take(40).collect();
    SqlError::syntax(format!("unterminated {what} at or near \"{rest}\""), start)
}

#[cfg(test)]
mod tests {
    use super::{TokenKind, tokenize};

    fn kinds(text: &str) -> Vec<TokenKind<'_>> {
        tokenize(text)
            .expect("tokenizes")
            .into_iter()
            .map(|token| token.kind)
            .collect()
    }

    #[test]
    fn words_fold_to_lower_case_and_quoted_identifiers_keep_their_case() {
        assert_eq!(
            kinds("SELECT \"MiXed\"\"Q\" FROM Users"),
            vec![
                TokenKind::Word("select".into()),
                TokenKind::QuotedIdentifier("MiXed\"Q".into()),
                TokenKind::Word("from".into()),
                TokenKind::Word("users".into()),
                TokenKind::End,
            ]
        );
    }

    #[test]
    fn numbers_split_into_integers_and_decimals() {
        assert_eq!(
            kinds("1 1.5 .5 2e3 7."),
            vec![
                TokenKind::Integer("1"),
                TokenKind::Decimal("1.5"),
                TokenKind::Decimal(".5"),
                TokenKind::Decimal("2e3"),
                TokenKind::Decimal("7."),
                TokenKind::End,
            ]
        );
    }

    #[test]
    fn strings_double_their_quotes_and_e_strings_take_escapes() {
        assert_eq!(
            kinds("'it''s' E'a\\nb\\x41' $$x'y$$ $t$q$t$"),
            vec![
                TokenKind::String("it's".into()),
                TokenKind::String("a\nbA".into()),
                TokenKind::String("x'y".into()),
                TokenKind::String("q".into()),
                TokenKind::End,
            ]
        );
    }

    #[test]
    fn comments_nest_and_operators_stop_before_a_trailing_minus() {
        assert_eq!(
            kinds("a /* x /* y */ z */ <>-- note\n=-1 ::"),
            vec![
                TokenKind::Word("a".into()),
                TokenKind::Operator("<>"),
                TokenKind::Operator("="),
                TokenKind::Operator("-"),
                TokenKind::Integer("1"),
                TokenKind::DoubleColon,
                TokenKind::End,
            ]
        );
    }

    #[test]
    fn unterminated_inputs_are_syntax_errors_at_their_start() {
        for (text, position) in [("x 'abc", 2), ("\"abc", 0), ("/* x", 0), ("$a$ b", 0)] {
            let error = tokenize(text).expect_err(text);
            assert_eq!(error.state().code(), "42601", "{text}");
            assert_eq!(error.position(), Some(position), "{text}");
        }
        assert_eq!(
            tokenize("12abc").expect_err("junk").message(),
            "trailing junk after numeric literal at or near \"12abc\""
        );
    }
}
