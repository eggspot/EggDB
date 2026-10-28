#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Token {
    Word(String),
    Quoted(String),
    Symbol(u8),
    Literal,
}

impl Token {
    pub(crate) fn is_word(&self, word: &str) -> bool {
        matches!(self, Self::Word(found) if found == word)
    }

    pub(crate) fn is_symbol(&self, symbol: u8) -> bool {
        matches!(self, Self::Symbol(found) if *found == symbol)
    }

    pub(crate) fn name_part(&self) -> Option<String> {
        match self {
            Self::Word(word) => Some(word.clone()),
            Self::Quoted(name) => Some(format!("\"{}\"", name.replace('"', "\"\""))),
            Self::Symbol(_) | Self::Literal => None,
        }
    }
}

pub(crate) struct Words<'t> {
    bytes: &'t [u8],
    at: usize,
}

impl<'t> Words<'t> {
    pub(crate) fn new(text: &'t str) -> Self {
        Self {
            bytes: text.as_bytes(),
            at: 0,
        }
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.at + offset).copied()
    }

    fn skip_space_and_comments(&mut self) {
        loop {
            match self.peek(0) {
                Some(byte) if byte.is_ascii_whitespace() => self.at += 1,
                Some(b'-') if self.peek(1) == Some(b'-') => {
                    while let Some(byte) = self.peek(0) {
                        self.at += 1;
                        if byte == b'\n' {
                            break;
                        }
                    }
                }
                Some(b'/') if self.peek(1) == Some(b'*') => {
                    self.at += 2;
                    let mut depth = 1usize;
                    while depth > 0 {
                        match (self.peek(0), self.peek(1)) {
                            (None, _) => return,
                            (Some(b'/'), Some(b'*')) => {
                                depth += 1;
                                self.at += 2;
                            }
                            (Some(b'*'), Some(b'/')) => {
                                depth -= 1;
                                self.at += 2;
                            }
                            _ => self.at += 1,
                        }
                    }
                }
                _ => return,
            }
        }
    }

    fn quoted(&mut self, quote: u8, backslash_escapes: bool) -> Vec<u8> {
        let mut content = Vec::new();
        self.at += 1;
        while let Some(byte) = self.peek(0) {
            self.at += 1;
            if backslash_escapes && byte == b'\\' {
                if let Some(next) = self.peek(0) {
                    content.push(next);
                    self.at += 1;
                }
                continue;
            }
            if byte == quote {
                if self.peek(0) == Some(quote) {
                    content.push(quote);
                    self.at += 1;
                    continue;
                }
                return content;
            }
            content.push(byte);
        }
        content
    }

    pub(crate) fn literal_text(&mut self) -> Option<String> {
        self.skip_space_and_comments();
        if self.peek(0) != Some(b'\'') {
            return None;
        }
        let content = self.quoted(b'\'', false);
        let text = String::from_utf8_lossy(&content).into_owned();
        Some(if text.contains('"') {
            text
        } else {
            text.to_ascii_lowercase()
        })
    }

    fn dollar_tag_length(&self) -> Option<usize> {
        let mut length = 1;
        loop {
            match self.peek(length)? {
                b'$' => return Some(length + 1),
                byte if is_word_byte(byte) && !(length == 1 && byte.is_ascii_digit()) => {
                    length += 1;
                }
                _ => return None,
            }
        }
    }

    fn dollar_quoted(&mut self, tag_length: usize) {
        let tag = &self.bytes[self.at..self.at + tag_length];
        self.at += tag_length;
        while self.at < self.bytes.len() {
            if self.bytes[self.at..].starts_with(tag) {
                self.at += tag_length;
                return;
            }
            self.at += 1;
        }
    }
}

impl Iterator for Words<'_> {
    type Item = Token;

    fn next(&mut self) -> Option<Token> {
        self.skip_space_and_comments();
        let byte = self.peek(0)?;
        if byte == b'\'' {
            self.quoted(b'\'', false);
            return Some(Token::Literal);
        }
        if byte == b'"' {
            let name = self.quoted(b'"', false);
            return Some(Token::Quoted(String::from_utf8_lossy(&name).into_owned()));
        }
        if byte == b'$' {
            if let Some(length) = self.dollar_tag_length() {
                self.dollar_quoted(length);
                return Some(Token::Literal);
            }
            self.at += 1;
            while self.peek(0).is_some_and(|byte| byte.is_ascii_digit()) {
                self.at += 1;
            }
            return Some(Token::Literal);
        }
        if byte.is_ascii_digit() {
            while self
                .peek(0)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'_')
            {
                self.at += 1;
            }
            return Some(Token::Literal);
        }
        if is_word_byte(byte) {
            let start = self.at;
            while self.peek(0).is_some_and(is_word_byte) {
                self.at += 1;
            }
            let word = String::from_utf8_lossy(&self.bytes[start..self.at]).to_ascii_lowercase();
            if self.peek(0) == Some(b'\'') && matches!(word.as_str(), "e" | "x" | "b" | "n") {
                self.quoted(b'\'', word == "e");
                return Some(Token::Literal);
            }
            if word == "u" && self.peek(0) == Some(b'&') && self.peek(1) == Some(b'\'') {
                self.at += 1;
                self.quoted(b'\'', false);
                return Some(Token::Literal);
            }
            return Some(Token::Word(word));
        }
        self.at += 1;
        Some(Token::Symbol(byte))
    }
}

pub(crate) fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 0x80
}

pub(crate) fn qualified_name(tokens: &[Token], at: &mut usize) -> Option<String> {
    let mut name = tokens.get(*at)?.name_part()?;
    *at += 1;
    while tokens.get(*at).is_some_and(|token| token.is_symbol(b'.')) {
        let Some(part) = tokens.get(*at + 1).and_then(Token::name_part) else {
            break;
        };
        name.push('.');
        name.push_str(&part);
        *at += 2;
    }
    Some(name)
}
