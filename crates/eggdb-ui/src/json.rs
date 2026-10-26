const DEEPEST: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Token {
    Punctuation,
    Key,
    String,
    Number,
    Literal,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct JsonLine {
    pub(crate) indent: usize,
    pub(crate) parts: Vec<(Token, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JsonError {
    pub(crate) offset: usize,
    pub(crate) expected: &'static str,
}

impl JsonError {
    pub(crate) fn message(&self) -> String {
        format!(
            "Not JSON: expected {} at byte {}",
            self.expected, self.offset
        )
    }
}

pub(crate) fn pretty(bytes: &[u8]) -> Result<Vec<JsonLine>, JsonError> {
    let text = std::str::from_utf8(bytes).map_err(|error| JsonError {
        offset: error.valid_up_to(),
        expected: "valid UTF-8",
    })?;
    let mut printer = Printer {
        bytes: text.as_bytes(),
        text,
        at: 0,
        lines: Vec::new(),
        current: JsonLine::default(),
    };
    printer.whitespace();
    printer.value(0)?;
    printer.whitespace();
    if printer.at != printer.bytes.len() {
        return Err(printer.error("the end of the value"));
    }
    printer.lines.push(printer.current);
    Ok(printer.lines)
}

struct Printer<'a> {
    bytes: &'a [u8],
    text: &'a str,
    at: usize,
    lines: Vec<JsonLine>,
    current: JsonLine,
}

impl Printer<'_> {
    fn error(&self, expected: &'static str) -> JsonError {
        JsonError {
            offset: self.at,
            expected,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn push(&mut self, token: Token, text: &str) {
        self.current.parts.push((token, text.to_owned()));
    }

    fn new_line(&mut self, indent: usize) {
        let line = std::mem::replace(
            &mut self.current,
            JsonLine {
                indent,
                parts: Vec::new(),
            },
        );
        self.lines.push(line);
    }

    fn value(&mut self, depth: usize) -> Result<(), JsonError> {
        if depth >= DEEPEST {
            return Err(self.error("nesting at most 512 deep"));
        }
        match self.peek() {
            Some(b'{') => self.container(depth, b'}', true),
            Some(b'[') => self.container(depth, b']', false),
            Some(b'"') => {
                let raw = self.string()?;
                let text = self.text;
                self.push(Token::String, &text[raw]);
                Ok(())
            }
            Some(b'-' | b'0'..=b'9') => {
                let raw = self.number()?;
                let text = self.text;
                self.push(Token::Number, &text[raw]);
                Ok(())
            }
            Some(b't') => self.literal("true"),
            Some(b'f') => self.literal("false"),
            Some(b'n') => self.literal("null"),
            _ => Err(self.error("a value")),
        }
    }

    fn container(&mut self, depth: usize, close: u8, object: bool) -> Result<(), JsonError> {
        let (open_text, close_text) = if object { ("{", "}") } else { ("[", "]") };
        self.at += 1;
        self.push(Token::Punctuation, open_text);
        self.whitespace();
        if self.peek() == Some(close) {
            self.at += 1;
            self.push(Token::Punctuation, close_text);
            return Ok(());
        }
        loop {
            self.new_line(depth + 1);
            if object {
                if self.peek() != Some(b'"') {
                    return Err(self.error("a string key"));
                }
                let key = self.string()?;
                let text = self.text;
                self.push(Token::Key, &text[key]);
                self.whitespace();
                if self.peek() != Some(b':') {
                    return Err(self.error("':'"));
                }
                self.at += 1;
                self.push(Token::Punctuation, ": ");
                self.whitespace();
            }
            self.value(depth + 1)?;
            self.whitespace();
            match self.peek() {
                Some(b',') => {
                    self.at += 1;
                    self.push(Token::Punctuation, ",");
                    self.whitespace();
                }
                Some(byte) if byte == close => {
                    self.at += 1;
                    self.new_line(depth);
                    self.push(Token::Punctuation, close_text);
                    return Ok(());
                }
                _ => {
                    return Err(self.error(if object { "',' or '}'" } else { "',' or ']'" }));
                }
            }
        }
    }

    fn string(&mut self) -> Result<std::ops::Range<usize>, JsonError> {
        let start = self.at;
        self.at += 1;
        loop {
            match self.peek() {
                None => return Err(self.error("'\"'")),
                Some(b'"') => {
                    self.at += 1;
                    return Ok(start..self.at);
                }
                Some(b'\\') => {
                    self.at += 1;
                    match self.peek() {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                            self.at += 1;
                        }
                        Some(b'u') => {
                            self.at += 1;
                            for _ in 0..4 {
                                if !self.peek().is_some_and(|b| b.is_ascii_hexdigit()) {
                                    return Err(self.error("four hex digits"));
                                }
                                self.at += 1;
                            }
                        }
                        _ => return Err(self.error("an escape")),
                    }
                }
                Some(0x00..=0x1f) => return Err(self.error("no control characters in a string")),
                Some(_) => self.at += 1,
            }
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.at;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.at += 1;
        }
        self.at - start
    }

    fn number(&mut self) -> Result<std::ops::Range<usize>, JsonError> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.peek() {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(self.error("a digit")),
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            if self.digits() == 0 {
                return Err(self.error("a digit"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if self.digits() == 0 {
                return Err(self.error("a digit"));
            }
        }
        Ok(start..self.at)
    }

    fn literal(&mut self, word: &'static str) -> Result<(), JsonError> {
        if !self.bytes[self.at..].starts_with(word.as_bytes()) {
            return Err(self.error(word));
        }
        self.at += word.len();
        self.push(Token::Literal, word);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{JsonLine, pretty};

    fn lines(text: &str) -> Vec<String> {
        pretty(text.as_bytes())
            .map(|lines| lines.iter().map(spell).collect())
            .unwrap_or_else(|error| vec![error.message()])
    }

    fn spell(line: &JsonLine) -> String {
        let mut out = " ".repeat(line.indent * 2);
        for (_, part) in &line.parts {
            out.push_str(part);
        }
        out
    }

    #[test]
    fn objects_and_arrays_get_one_member_per_line() {
        assert_eq!(
            lines(r#"{"name":"Ada","tags":["a",1,true],"empty":{},"none":[] , "n":null}"#),
            [
                "{",
                r#"  "name": "Ada","#,
                r#"  "tags": ["#,
                r#"    "a","#,
                "    1,",
                "    true",
                "  ],",
                r#"  "empty": {},"#,
                r#"  "none": [],"#,
                r#"  "n": null"#,
                "}",
            ]
        );
    }

    #[test]
    fn scalars_alone_are_json() {
        assert_eq!(lines(" 12.5e-3 "), ["12.5e-3"]);
        assert_eq!(lines(r#""aé\n""#), [r#""aé\n""#]);
    }

    #[test]
    fn errors_name_what_was_expected_and_where() {
        assert_eq!(
            lines(r#"{"a":1 "b":2}"#),
            ["Not JSON: expected ',' or '}' at byte 7"]
        );
        assert_eq!(lines("[1,]"), ["Not JSON: expected a value at byte 3"]);
        assert_eq!(
            lines("01"),
            ["Not JSON: expected the end of the value at byte 1"]
        );
        assert_eq!(
            lines("{'a':1}"),
            ["Not JSON: expected a string key at byte 1"]
        );
        assert_eq!(
            lines("\"a\x01\""),
            ["Not JSON: expected no control characters in a string at byte 2"]
        );
        assert_eq!(lines("tru"), ["Not JSON: expected true at byte 0"]);
        assert_eq!(lines("-"), ["Not JSON: expected a digit at byte 1"]);
    }

    #[test]
    fn deep_nesting_is_an_error_not_a_stack_overflow() {
        let deep = "[".repeat(100_000);
        assert_eq!(
            lines(&deep),
            ["Not JSON: expected nesting at most 512 deep at byte 512"]
        );
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let alphabet = br#"{}[]",:0123456789-+.eEtrufalsn \u\\x"#;
        for _ in 0..5000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let length = (state % 24) as usize;
            let bytes: Vec<u8> = (0..length)
                .map(|i| alphabet[((state >> (i % 50)) as usize + i * 7) % alphabet.len()])
                .collect();
            let _ = pretty(&bytes);
        }
    }
}
