const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Number(f64),
    Text(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub(crate) fn field(&self, name: &str) -> Option<&Json> {
        match self {
            Self::Object(fields) => fields
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub(crate) fn text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            _ => None,
        }
    }

    pub(crate) fn number(&self) -> Option<f64> {
        match self {
            Self::Number(number) => Some(*number),
            _ => None,
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

pub(crate) fn parse(text: &[u8]) -> Option<Json> {
    let mut reader = Reader { bytes: text, at: 0 };
    let value = reader.value(0)?;
    reader.space();
    (reader.at == text.len()).then_some(value)
}

impl Reader<'_> {
    fn space(&mut self) {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn take(&mut self, byte: u8) -> bool {
        self.space();
        if self.bytes.get(self.at) == Some(&byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn word(&mut self, word: &[u8], value: Json) -> Option<Json> {
        if self.bytes.get(self.at..self.at + word.len()) == Some(word) {
            self.at += word.len();
            Some(value)
        } else {
            None
        }
    }

    fn value(&mut self, depth: usize) -> Option<Json> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.space();
        match self.bytes.get(self.at)? {
            b'n' => self.word(b"null", Json::Null),
            b't' => self.word(b"true", Json::Bool(true)),
            b'f' => self.word(b"false", Json::Bool(false)),
            b'"' => self.string().map(Json::Text),
            b'[' => {
                self.at += 1;
                let mut items = Vec::new();
                if self.take(b']') {
                    return Some(Json::Array(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    if self.take(b']') {
                        return Some(Json::Array(items));
                    }
                    if !self.take(b',') {
                        return None;
                    }
                }
            }
            b'{' => {
                self.at += 1;
                let mut fields = Vec::new();
                if self.take(b'}') {
                    return Some(Json::Object(fields));
                }
                loop {
                    self.space();
                    let key = self.string()?;
                    if !self.take(b':') {
                        return None;
                    }
                    fields.push((key, self.value(depth + 1)?));
                    if self.take(b'}') {
                        return Some(Json::Object(fields));
                    }
                    if !self.take(b',') {
                        return None;
                    }
                }
            }
            _ => self.number().map(Json::Number),
        }
    }

    fn number(&mut self) -> Option<f64> {
        let start = self.at;
        while matches!(
            self.bytes.get(self.at),
            Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        ) {
            self.at += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.at])
            .ok()?
            .parse()
            .ok()
            .filter(|number: &f64| number.is_finite())
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = self.bytes.get(self.at..self.at + 4)?;
        self.at += 4;
        u32::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()
    }

    fn string(&mut self) -> Option<String> {
        if self.bytes.get(self.at) != Some(&b'"') {
            return None;
        }
        self.at += 1;
        let mut out = Vec::new();
        loop {
            let byte = *self.bytes.get(self.at)?;
            self.at += 1;
            match byte {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let escaped = *self.bytes.get(self.at)?;
                    self.at += 1;
                    let character = match escaped {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let first = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&first) {
                                if self.bytes.get(self.at..self.at + 2) != Some(b"\\u") {
                                    return None;
                                }
                                self.at += 2;
                                let second = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&second) {
                                    return None;
                                }
                                0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
                            } else {
                                first
                            };
                            char::from_u32(code)?
                        }
                        _ => return None,
                    };
                    let mut buffer = [0u8; 4];
                    out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
                }
                0..=0x1f => return None,
                other => out.push(other),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_parse_and_hostile_ones_are_refused() {
        let value = parse(br#"{"a": [1, -2.5e3, true, null], "b": "x\u00e9\ud83d\ude00"}"#).unwrap();
        assert_eq!(value.field("b").and_then(Json::text), Some("xé😀"));
        for hostile in [
            &b"{"[..],
            b"[1,]",
            b"\"\\ud800\"",
            b"1e999",
            b"{\"a\":1} x",
            &[b'['; 64],
        ] {
            assert!(parse(hostile).is_none(), "{hostile:?}");
        }
    }
}
