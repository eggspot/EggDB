use super::{Lexer, MAX_IDENTIFIER_BYTES, TokenKind, unterminated};
use crate::error::{SqlError, SqlResult, SqlState};
use std::borrow::Cow;

const ORACLE_OPERATORS: [&str; 15] = [
    "||", "<=", ">=", "<>", "!=", "^=", "~=", "=>", "=", "<", ">", "+", "-", "*", "/",
];

pub(crate) const MAX_ORACLE_BIND_VARIABLES: usize = 65535;

/// The name EggDB's catalog keeps for an Oracle identifier, and back: a name whose letters
/// are all upper case becomes all lower case and the reverse, and a mixed-case name stays as
/// it is. Unquoted names fold to upper case in Oracle and to lower case in the other dialects,
/// so `T` in Oracle and `t` in PostgreSQL name the same table.
pub(crate) fn swap_uniform_case(name: &str) -> String {
    let upper = name.bytes().any(|byte| byte.is_ascii_uppercase());
    let lower = name.bytes().any(|byte| byte.is_ascii_lowercase());
    match (upper, lower) {
        (true, false) => name.to_ascii_lowercase(),
        (false, true) => name.to_ascii_uppercase(),
        _ => name.to_owned(),
    }
}

fn is_oracle_identifier_part(byte: u8) -> bool {
    super::is_identifier_part(byte) || byte == b'#'
}

fn identifier_too_long(start: usize) -> SqlError {
    SqlError::new(SqlState::NAME_TOO_LONG, "identifier is too long").at(start)
}

impl<'t> Lexer<'t> {
    pub(super) fn next_oracle_kind(&mut self, byte: u8) -> SqlResult<TokenKind<'t>> {
        match byte {
            b'.' if self.peek(1).is_some_and(|next| next.is_ascii_digit()) => self.number(),
            b'.' => {
                self.at += 1;
                Ok(TokenKind::Dot)
            }
            b'0'..=b'9' => self.number(),
            b'\'' => self.string(false),
            b'n' | b'N' if self.peek(1) == Some(b'\'') => {
                self.at += 1;
                self.string(false)
            }
            b'q' | b'Q' if self.peek(1) == Some(b'\'') => self.quote_literal(1),
            b'n' | b'N'
                if matches!(self.peek(1), Some(b'q' | b'Q')) && self.peek(2) == Some(b'\'') =>
            {
                self.quote_literal(2)
            }
            b'"' => self.oracle_quoted_identifier(),
            b':' if self
                .peek(1)
                .is_some_and(|next| next.is_ascii_alphanumeric() || next == b'_') =>
            {
                self.bind_variable()
            }
            b':' => {
                self.at += 1;
                Ok(TokenKind::Colon)
            }
            _ if super::is_identifier_start(byte) => self.oracle_word(),
            _ => {
                let rest = &self.text[self.at..];
                if let Some(operator) = ORACLE_OPERATORS
                    .iter()
                    .find(|operator| rest.starts_with(**operator))
                {
                    let spelled = &self.text[self.at..self.at + operator.len()];
                    self.at += operator.len();
                    return Ok(TokenKind::Operator(match spelled {
                        "^=" | "~=" => "<>",
                        other => other,
                    }));
                }
                let character = rest.chars().next().unwrap_or('?');
                Err(SqlError::syntax(
                    format!("invalid character \"{character}\""),
                    self.at,
                ))
            }
        }
    }

    fn oracle_word(&mut self) -> SqlResult<TokenKind<'t>> {
        let start = self.at;
        while self.peek(0).is_some_and(is_oracle_identifier_part) {
            self.at += 1;
        }
        let text = &self.text[start..self.at];
        if text.len() > MAX_IDENTIFIER_BYTES {
            return Err(identifier_too_long(start));
        }
        if !text.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return Ok(TokenKind::Word(Cow::Borrowed(text)));
        }
        Ok(TokenKind::Word(Cow::Owned(text.to_ascii_lowercase())))
    }

    fn oracle_quoted_identifier(&mut self) -> SqlResult<TokenKind<'t>> {
        let start = self.at;
        let Some(length) = self.text[start + 1..].find('"') else {
            return Err(unterminated(self.text, start, "quoted identifier"));
        };
        let name = &self.text[start + 1..start + 1 + length];
        self.at = start + length + 2;
        if name.is_empty() {
            return Err(SqlError::syntax(
                "zero-length delimited identifier at or near \"\"\"\"",
                start,
            ));
        }
        if name.len() > MAX_IDENTIFIER_BYTES {
            return Err(identifier_too_long(start));
        }
        Ok(TokenKind::QuotedIdentifier(swap_uniform_case(name)))
    }

    fn quote_literal(&mut self, prefix: usize) -> SqlResult<TokenKind<'t>> {
        let start = self.at;
        self.at += prefix + 1;
        let Some(open) = self.text[self.at..].chars().next() else {
            return Err(unterminated(self.text, start, "quoted string"));
        };
        if open.is_whitespace() {
            return Err(unterminated(self.text, start, "quoted string"));
        }
        let close = match open {
            '[' => ']',
            '{' => '}',
            '<' => '>',
            '(' => ')',
            other => other,
        };
        self.at += open.len_utf8();
        let body_start = self.at;
        let mut ending = String::with_capacity(2);
        ending.push(close);
        ending.push('\'');
        let Some(length) = self.text[body_start..].find(&ending) else {
            return Err(unterminated(self.text, start, "quoted string"));
        };
        self.at = body_start + length + ending.len();
        Ok(TokenKind::String(Cow::Borrowed(
            &self.text[body_start..body_start + length],
        )))
    }

    fn bind_variable(&mut self) -> SqlResult<TokenKind<'t>> {
        let start = self.at;
        self.at += 1;
        while self.peek(0).is_some_and(is_oracle_identifier_part) {
            self.at += 1;
        }
        let name = &self.text[start + 1..self.at];
        if let Some(&(_, number)) = self
            .named_parameters
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
        {
            return Ok(TokenKind::NumberedParameter(number));
        }
        self.highest_parameter += 1;
        if self.highest_parameter > MAX_ORACLE_BIND_VARIABLES {
            return Err(
                SqlError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "too many bind variables")
                    .at(start),
            );
        }
        self.named_parameters.push((name, self.highest_parameter));
        Ok(TokenKind::NumberedParameter(self.highest_parameter))
    }
}

#[cfg(test)]
mod tests {
    use super::swap_uniform_case;

    #[test]
    fn uniform_case_names_swap_and_mixed_ones_stay() {
        assert_eq!(swap_uniform_case("EMP"), "emp");
        assert_eq!(swap_uniform_case("emp"), "EMP");
        assert_eq!(swap_uniform_case("Emp"), "Emp");
        assert_eq!(swap_uniform_case("EMP_1#"), "emp_1#");
        assert_eq!(swap_uniform_case("123"), "123");
        for name in ["EMP", "emp", "Emp", "a_B", "Ü"] {
            assert_eq!(swap_uniform_case(&swap_uniform_case(name)), name);
        }
    }
}
