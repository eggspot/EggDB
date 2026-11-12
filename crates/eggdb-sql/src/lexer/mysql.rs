use super::{Lexer, TokenKind, is_identifier_part};
use crate::error::{SqlError, SqlResult};

fn bits_as_hex(bits: &str) -> String {
    let padding = (8 - bits.len() % 8) % 8;
    let padded: String = "0".repeat(padding) + bits;
    padded
        .as_bytes()
        .chunks(8)
        .map(|byte| {
            let value = byte.iter().fold(0u8, |value, bit| value << 1 | u8::from(*bit == b'1'));
            format!("{value:02x}")
        })
        .collect()
}

impl<'t> Lexer<'t> {
    pub(super) fn mysql_quoted_bits(&mut self) -> SqlResult<TokenKind<'t>> {
        let start = self.at;
        let digits_start = start + 2;
        let mut end = digits_start;
        while matches!(self.bytes.get(end), Some(b'0' | b'1')) {
            end += 1;
        }
        if self.bytes.get(end) != Some(&b'\'') {
            let shown: String = self.text[start..].chars().take(end + 1 - start).collect();
            return Err(SqlError::syntax(
                format!("You have an error in your SQL syntax near '{shown}'"),
                start,
            ));
        }
        self.at = end + 1;
        Ok(TokenKind::Binary(bits_as_hex(&self.text[digits_start..end]).into()))
    }

    pub(super) fn mysql_prefixed_number(&mut self) -> SqlResult<TokenKind<'t>> {
        let start = self.at;
        let hexadecimal = matches!(self.peek(1), Some(b'x' | b'X'));
        let is_digit = |byte: u8| if hexadecimal { byte.is_ascii_hexdigit() } else { matches!(byte, b'0' | b'1') };
        let mut end = start + 2;
        while self.bytes.get(end).copied().is_some_and(is_digit) {
            end += 1;
        }
        if end == start + 2 || self.bytes.get(end).copied().is_some_and(is_identifier_part) {
            return self.number();
        }
        self.at = end;
        let digits = &self.text[start + 2..end];
        Ok(if hexadecimal {
            if digits.len() % 2 == 1 {
                TokenKind::Binary(format!("0{digits}").into())
            } else {
                TokenKind::Binary(digits.into())
            }
        } else {
            TokenKind::Binary(bits_as_hex(digits).into())
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::dialect::Dialect;
    use crate::lexer::{TokenKind, tokenize_in};

    #[test]
    fn bit_and_hexadecimal_literals_become_binary_strings() {
        for (text, digits) in [("b'01000001'", "41"), ("B'1'", "01"), ("b''", ""), ("0b101", "05"), ("0x41", "41"), ("0xABC", "0ABC")] {
            let tokens = tokenize_in(text, Dialect::MySql).expect(text);
            assert_eq!(tokens[0].kind, TokenKind::Binary(digits.into()), "{text}");
        }
        assert!(tokenize_in("b'012'", Dialect::MySql).is_err());
    }
}
