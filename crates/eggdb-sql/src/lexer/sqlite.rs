use super::{Lexer, TokenKind, is_identifier_part, truncated_identifier, unterminated};
use crate::error::{SqlError, SqlResult, SqlState};

const SQLITE_OPERATORS: [&str; 22] = [
    "->>", "||", "<<", ">>", "<=", ">=", "==", "!=", "<>", "->", "=", "<", ">", "+", "-", "*", "/",
    "%", "&", "|", "~", "!",
];

pub(crate) const MAX_SQLITE_PARAMETER: usize = 32766;

impl<'t> Lexer<'t> {
    pub(super) fn skip_sqlite_space_and_comments(&mut self) {
        loop {
            match (self.peek(0), self.peek(1)) {
                (Some(byte), _) if byte.is_ascii_whitespace() => self.at += 1,
                (Some(b'-'), Some(b'-')) => self.skip_line(),
                (Some(b'/'), Some(b'*')) => {
                    self.at += 2;
                    match self.text[self.at..].find("*/") {
                        Some(length) => self.at += length + 2,
                        None => self.at = self.text.len(),
                    }
                }
                _ => return,
            }
        }
    }

    pub(super) fn next_sqlite_kind(&mut self, byte: u8) -> SqlResult<TokenKind<'t>> {
        match byte {
            b'.' if self.peek(1).is_some_and(|next| next.is_ascii_digit()) => self.number(),
            b'.' => {
                self.at += 1;
                Ok(TokenKind::Dot)
            }
            b'0'..=b'9' => self.number(),
            b'\'' => self.string(false),
            b'"' => self.quoted_identifier().map(lowered),
            b'`' => self.backtick_identifier(),
            b'x' | b'X' if self.peek(1) == Some(b'\'') => Err(SqlError::not_supported(
                "BLOB literals (x'...') are not supported yet",
            )
            .at(self.at)),
            b'?' => self.numbered_parameter(),
            b':' | b'@' | b'$' => self.named_parameter(),
            _ if super::is_identifier_start(byte) => Ok(self.word()),
            _ => {
                let rest = &self.text[self.at..];
                if let Some(operator) = SQLITE_OPERATORS
                    .iter()
                    .find(|operator| rest.starts_with(**operator))
                {
                    let operator = &self.text[self.at..self.at + operator.len()];
                    self.at += operator.len();
                    return Ok(TokenKind::Operator(operator));
                }
                let character = rest.chars().next().unwrap_or('?');
                Err(SqlError::syntax(
                    format!("unrecognized token: \"{character}\""),
                    self.at,
                ))
            }
        }
    }

    pub(super) fn bracket_identifier(&mut self) -> SqlResult<TokenKind<'t>> {
        let start = self.at;
        let Some(length) = self.text[start + 1..].find(']') else {
            return Err(unterminated(self.text, start, "quoted identifier"));
        };
        let name = &self.text[start + 1..start + 1 + length];
        self.at = start + length + 2;
        if name.is_empty() {
            return Err(SqlError::syntax(
                "zero-length delimited identifier at or near \"[]\"",
                start,
            ));
        }
        Ok(TokenKind::QuotedIdentifier(truncated_identifier(
            name.to_lowercase(),
        )))
    }

    fn numbered_parameter(&mut self) -> SqlResult<TokenKind<'t>> {
        let start = self.at;
        self.at += 1;
        let digits_start = self.at;
        self.digits();
        if self.at == digits_start {
            self.highest_parameter += 1;
            return self.parameter_numbered(self.highest_parameter, start);
        }
        let number = self.text[digits_start..self.at]
            .parse::<usize>()
            .ok()
            .filter(|number| (1..=MAX_SQLITE_PARAMETER).contains(number))
            .ok_or_else(|| {
                SqlError::new(
                    SqlState::UNDEFINED_PARAMETER,
                    format!("variable number must be between ?1 and ?{MAX_SQLITE_PARAMETER}"),
                )
                .at(start)
            })?;
        self.highest_parameter = self.highest_parameter.max(number);
        Ok(TokenKind::NumberedParameter(number))
    }

    fn named_parameter(&mut self) -> SqlResult<TokenKind<'t>> {
        let start = self.at;
        self.at += 1;
        while self.peek(0).is_some_and(is_identifier_part) {
            self.at += 1;
        }
        if self.at == start + 1 {
            let character = self.text[start..].chars().next().unwrap_or('?');
            return Err(SqlError::syntax(
                format!("unrecognized token: \"{character}\""),
                start,
            ));
        }
        let name = &self.text[start..self.at];
        if let Some(&(_, number)) = self
            .named_parameters
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
        {
            return Ok(TokenKind::NumberedParameter(number));
        }
        self.highest_parameter += 1;
        self.named_parameters.push((name, self.highest_parameter));
        self.parameter_numbered(self.highest_parameter, start)
    }

    fn parameter_numbered(&self, number: usize, start: usize) -> SqlResult<TokenKind<'t>> {
        if number > MAX_SQLITE_PARAMETER {
            return Err(
                SqlError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "too many SQL variables").at(start),
            );
        }
        Ok(TokenKind::NumberedParameter(number))
    }
}

fn lowered(kind: TokenKind<'_>) -> TokenKind<'_> {
    match kind {
        TokenKind::QuotedIdentifier(name) => TokenKind::QuotedIdentifier(name.to_lowercase()),
        other => other,
    }
}
