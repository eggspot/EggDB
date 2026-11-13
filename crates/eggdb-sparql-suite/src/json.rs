const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Number(String),
    Text(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub(crate) fn field(&self, name: &str) -> Option<&Json> {
        match self {
            Self::Object(fields) => fields.iter().find(|(key, _)| key == name).map(|(_, value)| value),
            _ => None,
        }
    }

    pub(crate) fn text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            _ => None,
        }
    }
}

struct Reader<'a> {
    text: &'a str,
    bytes: &'a [u8],
    at: usize,
}

pub(crate) fn parse(text: &str) -> Option<Json> {
    let mut reader = Reader { text, bytes: text.as_bytes(), at: 0 };
    let value = reader.value(0)?;
    reader.space();
    (reader.at == reader.bytes.len()).then_some(value)
}

impl Reader<'_> {
    fn space(&mut self) {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn take(&mut self, byte: u8) -> bool {
        self.space();
        let found = self.bytes.get(self.at) == Some(&byte);
        if found {
            self.at += 1;
        }
        found
    }

    fn literal(&mut self, word: &[u8], value: Json) -> Option<Json> {
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
            b'{' => {
                self.at += 1;
                let mut fields = Vec::new();
                if self.take(b'}') {
                    return Some(Json::Object(fields));
                }
                loop {
                    self.space();
                    let name = self.string()?;
                    if !self.take(b':') {
                        return None;
                    }
                    fields.push((name, self.value(depth + 1)?));
                    if self.take(b'}') {
                        return Some(Json::Object(fields));
                    }
                    if !self.take(b',') {
                        return None;
                    }
                }
            }
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
            b'"' => self.string().map(Json::Text),
            b't' => self.literal(b"true", Json::Bool(true)),
            b'f' => self.literal(b"false", Json::Bool(false)),
            b'n' => self.literal(b"null", Json::Null),
            _ => {
                let start = self.at;
                while matches!(self.bytes.get(self.at), Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')) {
                    self.at += 1;
                }
                let text = std::str::from_utf8(self.bytes.get(start..self.at)?).ok()?;
                (!text.is_empty()).then(|| Json::Number(text.to_owned()))
            }
        }
    }

    fn string(&mut self) -> Option<String> {
        if self.bytes.get(self.at) != Some(&b'"') {
            return None;
        }
        self.at += 1;
        let mut out = String::new();
        let mut pending_high: Option<u32> = None;
        loop {
            let byte = *self.bytes.get(self.at)?;
            match byte {
                b'"' => {
                    self.at += 1;
                    return Some(out);
                }
                b'\\' => {
                    let escape = *self.bytes.get(self.at + 1)?;
                    self.at += 2;
                    let character = match escape {
                        b'u' => {
                            let digits = std::str::from_utf8(self.bytes.get(self.at..self.at + 4)?).ok()?;
                            self.at += 4;
                            let code = u32::from_str_radix(digits, 16).ok()?;
                            if (0xD800..0xDC00).contains(&code) {
                                pending_high = Some(code);
                                continue;
                            }
                            let code = match pending_high.take() {
                                Some(high) if (0xDC00..0xE000).contains(&code) => 0x10000 + ((high - 0xD800) << 10) + (code - 0xDC00),
                                _ => code,
                            };
                            char::from_u32(code)?
                        }
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        other => char::from(other),
                    };
                    out.push(character);
                }
                _ => {
                    let character = self.text.get(self.at..)?.chars().next()?;
                    out.push(character);
                    self.at += character.len_utf8();
                }
            }
        }
    }
}
