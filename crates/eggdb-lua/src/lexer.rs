use crate::LuaError;

const MOST_LONG_BRACKET_LEVEL: usize = 64;

/// One token of Lua source.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Token {
    Name(String),
    Number(f64),
    String(Vec<u8>),
    Keyword(&'static str),
    Symbol(&'static str),
    End,
}

/// A token with the line it starts on.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Located {
    pub(crate) token: Token,
    pub(crate) line: u32,
}

const KEYWORDS: [&str; 21] = [
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "if", "in", "local",
    "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];
const SYMBOLS: [&str; 26] = [
    "...", "..", "==", "~=", "<=", ">=", "+", "-", "*", "/", "%", "^", "#", "<", ">", "=", "(",
    ")", "{", "}", "[", "]", ";", ":", ",", ".",
];

struct Lexer<'s> {
    bytes: &'s [u8],
    at: usize,
    line: u32,
}

impl Lexer<'_> {
    fn peek(&self, ahead: usize) -> Option<u8> {
        self.bytes.get(self.at + ahead).copied()
    }

    fn error(&self, message: &str) -> LuaError {
        LuaError::syntax(self.line, message)
    }

    fn long_bracket_level(&self) -> Option<usize> {
        if self.peek(0) != Some(b'[') {
            return None;
        }
        let mut level = 0;
        while self.peek(1 + level) == Some(b'=') {
            level += 1;
            if level > MOST_LONG_BRACKET_LEVEL {
                return None;
            }
        }
        (self.peek(1 + level) == Some(b'[')).then_some(level)
    }

    fn long_string(&mut self, level: usize) -> Result<Vec<u8>, LuaError> {
        self.at += level + 2;
        if self.peek(0) == Some(b'\r') {
            self.at += 1;
        }
        if self.peek(0) == Some(b'\n') {
            self.at += 1;
            self.line += 1;
        }
        let mut out = Vec::new();
        loop {
            match self.peek(0) {
                None => return Err(self.error("unfinished long string")),
                Some(b']') => {
                    let closes = (1..=level).all(|offset| self.peek(offset) == Some(b'='))
                        && self.peek(level + 1) == Some(b']');
                    if closes {
                        self.at += level + 2;
                        return Ok(out);
                    }
                    out.push(b']');
                    self.at += 1;
                }
                Some(byte) => {
                    if byte == b'\n' {
                        self.line += 1;
                    }
                    out.push(byte);
                    self.at += 1;
                }
            }
        }
    }

    fn skip_space_and_comments(&mut self) -> Result<(), LuaError> {
        loop {
            match self.peek(0) {
                Some(b'\n') => {
                    self.line += 1;
                    self.at += 1;
                }
                Some(byte) if byte.is_ascii_whitespace() => self.at += 1,
                Some(b'-') if self.peek(1) == Some(b'-') => {
                    self.at += 2;
                    if let Some(level) = self.long_bracket_level() {
                        self.long_string(level)?;
                    } else {
                        while self.peek(0).is_some_and(|byte| byte != b'\n') {
                            self.at += 1;
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn number(&mut self) -> Result<Token, LuaError> {
        let start = self.at;
        if self.peek(0) == Some(b'0') && matches!(self.peek(1), Some(b'x' | b'X')) {
            self.at += 2;
            let digits_start = self.at;
            while self.peek(0).is_some_and(|byte| byte.is_ascii_hexdigit()) {
                self.at += 1;
            }
            let digits = std::str::from_utf8(&self.bytes[digits_start..self.at]).unwrap_or("");
            return u64::from_str_radix(digits, 16)
                .map(|value| Token::Number(value as f64))
                .map_err(|_| self.error("malformed number"));
        }
        while self
            .peek(0)
            .is_some_and(|byte| byte.is_ascii_digit() || byte == b'.')
        {
            self.at += 1;
        }
        if matches!(self.peek(0), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(0), Some(b'+' | b'-')) {
                self.at += 1;
            }
            while self.peek(0).is_some_and(|byte| byte.is_ascii_digit()) {
                self.at += 1;
            }
        }
        if self.peek(0).is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_') {
            return Err(self.error("malformed number"));
        }
        std::str::from_utf8(&self.bytes[start..self.at])
            .ok()
            .and_then(|text| text.parse::<f64>().ok())
            .map(Token::Number)
            .ok_or_else(|| self.error("malformed number"))
    }

    fn quoted(&mut self, quote: u8) -> Result<Vec<u8>, LuaError> {
        self.at += 1;
        let mut out = Vec::new();
        loop {
            let Some(byte) = self.peek(0) else {
                return Err(self.error("unfinished string"));
            };
            self.at += 1;
            match byte {
                b'\n' => return Err(self.error("unfinished string")),
                b'\\' => {
                    let Some(escaped) = self.peek(0) else {
                        return Err(self.error("unfinished string"));
                    };
                    self.at += 1;
                    match escaped {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'a' => out.push(7),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'v' => out.push(11),
                        b'\n' => {
                            self.line += 1;
                            out.push(b'\n');
                        }
                        digit if digit.is_ascii_digit() => {
                            let mut value = u32::from(digit - b'0');
                            for _ in 0..2 {
                                match self.peek(0) {
                                    Some(next) if next.is_ascii_digit() => {
                                        value = value * 10 + u32::from(next - b'0');
                                        self.at += 1;
                                    }
                                    _ => break,
                                }
                            }
                            let byte = u8::try_from(value).map_err(|_| self.error("escape sequence too large"))?;
                            out.push(byte);
                        }
                        other => out.push(other),
                    }
                }
                closing if closing == quote => return Ok(out),
                other => out.push(other),
            }
        }
    }

    fn next(&mut self) -> Result<Located, LuaError> {
        self.skip_space_and_comments()?;
        let line = self.line;
        let Some(byte) = self.peek(0) else {
            return Ok(Located {
                token: Token::End,
                line,
            });
        };
        let token = if byte.is_ascii_alphabetic() || byte == b'_' {
            let start = self.at;
            while self
                .peek(0)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                self.at += 1;
            }
            let word = String::from_utf8_lossy(&self.bytes[start..self.at]).into_owned();
            match KEYWORDS.iter().find(|keyword| **keyword == word) {
                Some(keyword) => Token::Keyword(keyword),
                None => Token::Name(word),
            }
        } else if byte.is_ascii_digit()
            || (byte == b'.' && self.peek(1).is_some_and(|next| next.is_ascii_digit()))
        {
            self.number()?
        } else if byte == b'"' || byte == b'\'' {
            Token::String(self.quoted(byte)?)
        } else if let Some(level) = self.long_bracket_level() {
            Token::String(self.long_string(level)?)
        } else {
            let rest = &self.bytes[self.at..];
            let symbol = SYMBOLS
                .iter()
                .find(|symbol| rest.starts_with(symbol.as_bytes()))
                .ok_or_else(|| self.error(&format!("unexpected symbol near '{}'", byte as char)))?;
            self.at += symbol.len();
            Token::Symbol(symbol)
        };
        Ok(Located { token, line })
    }
}

/// The tokens of `source`, ending with [`Token::End`].
pub(crate) fn tokens(source: &[u8]) -> Result<Vec<Located>, LuaError> {
    let mut lexer = Lexer {
        bytes: source,
        at: 0,
        line: 1,
    };
    let mut out = Vec::new();
    loop {
        let located = lexer.next()?;
        let done = located.token == Token::End;
        out.push(located);
        if done {
            return Ok(out);
        }
    }
}
