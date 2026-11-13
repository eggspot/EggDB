const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
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

    pub(crate) fn typed(kind: &str, value: Json) -> Json {
        Json::Object(vec![("@type".to_owned(), Json::Text(kind.to_owned())), ("@value".to_owned(), value)])
    }

    pub(crate) fn object(fields: Vec<(&str, Json)>) -> Json {
        Json::Object(fields.into_iter().map(|(name, value)| (name.to_owned(), value)).collect())
    }

    pub(crate) fn write(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
            Self::Integer(number) => out.push_str(&number.to_string()),
            Self::Float(number) if number.is_finite() => {
                let text = number.to_string();
                out.push_str(&text);
                if !text.contains(['.', 'e', 'E']) {
                    out.push_str(".0");
                }
            }
            Self::Float(number) if number.is_nan() => out.push_str("\"NaN\""),
            Self::Float(number) if *number > 0.0 => out.push_str("\"Infinity\""),
            Self::Float(_) => out.push_str("\"-Infinity\""),
            Self::Text(text) => write_text(text, out),
            Self::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Self::Object(fields) => {
                out.push('{');
                for (index, (name, value)) in fields.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write_text(name, out);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }

    pub(crate) fn to_text(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }
}

fn write_text(text: &str, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if u32::from(character) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(character))),
            character => out.push(character),
        }
    }
    out.push('"');
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

    fn literal(&mut self, word: &[u8]) -> bool {
        if self.bytes.get(self.at..self.at + word.len()) == Some(word) {
            self.at += word.len();
            true
        } else {
            false
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
            b't' => self.literal(b"true").then_some(Json::Bool(true)),
            b'f' => self.literal(b"false").then_some(Json::Bool(false)),
            b'n' => self.literal(b"null").then_some(Json::Null),
            _ => self.number(),
        }
    }

    fn number(&mut self) -> Option<Json> {
        let start = self.at;
        while matches!(self.bytes.get(self.at), Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')) {
            self.at += 1;
        }
        let text = std::str::from_utf8(self.bytes.get(start..self.at)?).ok()?;
        if text.is_empty() {
            return None;
        }
        if !text.contains(['.', 'e', 'E'])
            && let Ok(number) = text.parse::<i64>()
        {
            return Some(Json::Integer(number));
        }
        text.parse::<f64>().ok().map(Json::Float)
    }

    fn hex(&mut self) -> Option<u32> {
        let digits = std::str::from_utf8(self.bytes.get(self.at..self.at + 4)?).ok()?;
        self.at += 4;
        u32::from_str_radix(digits, 16).ok()
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
                    let escape = *self.bytes.get(self.at)?;
                    self.at += 1;
                    let character = match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let first = self.hex()?;
                            let code = if (0xD800..0xDC00).contains(&first) {
                                if !self.literal(b"\\u") {
                                    return None;
                                }
                                let second = self.hex()?;
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
                byte if byte < 0x20 => return None,
                byte => out.push(byte),
            }
        }
    }
}
