const DEEPEST: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Token {
    Punctuation,
    Key,
    String,
    Number,
    Literal,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct JsonLine {
    offset: u32,
    pub(crate) indent: u32,
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

pub(crate) const LARGEST_JSON: usize = 4 << 20;

pub(crate) fn pretty(bytes: &[u8]) -> Result<Vec<JsonLine>, JsonError> {
    if bytes.len() > LARGEST_JSON {
        return Err(JsonError {
            offset: 0,
            expected: "a value of at most 4 MiB for the JSON view",
        });
    }
    let text = std::str::from_utf8(bytes).map_err(|error| JsonError {
        offset: error.valid_up_to(),
        expected: "valid UTF-8",
    })?;
    let mut printer = Printer {
        bytes: text.as_bytes(),
        at: 0,
        lines: Vec::new(),
    };
    printer.whitespace();
    printer.new_line(0);
    printer.value(0)?;
    printer.whitespace();
    if printer.at != printer.bytes.len() {
        return Err(printer.error("the end of the value"));
    }
    Ok(printer.lines)
}

pub(crate) fn line_parts(bytes: &[u8], line: JsonLine) -> Vec<(Token, &str)> {
    let mut parts = Vec::new();
    let mut at = usize::try_from(line.offset).unwrap_or(usize::MAX);
    let next_significant = |from: usize| {
        bytes
            .get(from..)
            .and_then(|rest| rest.iter().find(|b| !b.is_ascii_whitespace()).copied())
    };
    let slice = |from: usize, to: usize| {
        bytes
            .get(from..to)
            .and_then(|part| std::str::from_utf8(part).ok())
            .unwrap_or("")
    };
    while let Some(&first) = bytes.get(at) {
        let end = match first {
            b'{' | b'[' | b'}' | b']' | b',' | b':' => at + 1,
            b'"' => string_end(bytes, at),
            _ => {
                at + bytes[at..]
                    .iter()
                    .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'+' | b'.'))
                    .count()
                    .max(1)
            }
        };
        let next = next_significant(end);
        let token = match first {
            b'"' if next == Some(b':') => Token::Key,
            b'"' => Token::String,
            b't' | b'f' | b'n' => Token::Literal,
            b'{' | b'[' | b'}' | b']' | b',' | b':' => Token::Punctuation,
            _ => Token::Number,
        };
        parts.push((token, if first == b':' { ": " } else { slice(at, end) }));
        at = end;
        let ends_line = match first {
            b'{' => next != Some(b'}'),
            b'[' => next != Some(b']'),
            b',' => true,
            b':' => false,
            _ => matches!(next, Some(b'}' | b']') | None),
        };
        if ends_line {
            break;
        }
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
    }
    parts
}

fn string_end(bytes: &[u8], start: usize) -> usize {
    let mut at = start + 1;
    while let Some(&byte) = bytes.get(at) {
        at += 1;
        match byte {
            b'"' => return at,
            b'\\' => at += 1,
            _ => {}
        }
    }
    bytes.len()
}

struct Printer<'a> {
    bytes: &'a [u8],
    at: usize,
    lines: Vec<JsonLine>,
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

    fn new_line(&mut self, indent: usize) {
        self.lines.push(JsonLine {
            offset: u32::try_from(self.at).unwrap_or(u32::MAX),
            indent: u32::try_from(indent).unwrap_or(u32::MAX),
        });
    }

    fn value(&mut self, depth: usize) -> Result<(), JsonError> {
        if depth >= DEEPEST {
            return Err(self.error("nesting at most 512 deep"));
        }
        match self.peek() {
            Some(b'{') => self.container(depth, b'}', true),
            Some(b'[') => self.container(depth, b']', false),
            Some(b'"') => {
                self.string()?;
                Ok(())
            }
            Some(b'-' | b'0'..=b'9') => {
                self.number()?;
                Ok(())
            }
            Some(b't') => self.literal("true"),
            Some(b'f') => self.literal("false"),
            Some(b'n') => self.literal("null"),
            _ => Err(self.error("a value")),
        }
    }

    fn container(&mut self, depth: usize, close: u8, object: bool) -> Result<(), JsonError> {
        self.at += 1;
        self.whitespace();
        if self.peek() == Some(close) {
            self.at += 1;
            return Ok(());
        }
        loop {
            self.new_line(depth + 1);
            if object {
                if self.peek() != Some(b'"') {
                    return Err(self.error("a string key"));
                }
                self.string()?;
                self.whitespace();
                if self.peek() != Some(b':') {
                    return Err(self.error("':'"));
                }
                self.at += 1;
                self.whitespace();
            }
            self.value(depth + 1)?;
            self.whitespace();
            match self.peek() {
                Some(b',') => {
                    self.at += 1;
                    self.whitespace();
                }
                Some(byte) if byte == close => {
                    self.new_line(depth);
                    self.at += 1;
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{JsonLine, pretty};

    fn lines(text: &str) -> Vec<String> {
        pretty(text.as_bytes())
            .map(|lines| {
                lines
                    .iter()
                    .map(|line| spell(text.as_bytes(), line))
                    .collect()
            })
            .unwrap_or_else(|error| vec![error.message()])
    }

    fn spell(bytes: &[u8], line: &JsonLine) -> String {
        let mut out = " ".repeat(line.indent as usize * 2);
        for (_, part) in super::line_parts(bytes, *line) {
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
    fn keys_are_told_from_strings_and_the_index_is_small() {
        let text = r#"{ "k" : "v" , "list" : [ "x" , { } ] }"#;
        let Ok(index) = pretty(text.as_bytes()) else {
            panic!("valid JSON");
        };
        let parts = super::line_parts(text.as_bytes(), index[1]);
        assert_eq!(parts[0], (super::Token::Key, "\"k\""));
        assert_eq!(parts[2], (super::Token::String, "\"v\""));
        assert_eq!(lines(text)[3], r#"    "x","#);
        assert_eq!(lines(text)[4], "    {}");
        assert_eq!(std::mem::size_of::<JsonLine>(), 8);
    }

    #[test]
    fn values_above_4_mib_are_not_indexed() {
        let big = format!("[{}1]", "1,".repeat(super::LARGEST_JSON / 2));
        assert!(pretty(big.as_bytes()).is_err());
        let fits = format!("[{}1]", "1,".repeat(super::LARGEST_JSON / 2 - 2));
        let Ok(index) = pretty(fits.as_bytes()) else {
            panic!("fits");
        };
        assert_eq!(index.len(), super::LARGEST_JSON / 2 + 1);
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
