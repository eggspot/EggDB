//! The JSON the Connect REST API exchanges: a small reader with a depth limit and a writer.

use std::collections::BTreeMap;

const MOST_DEPTH: usize = 32;

/// A JSON value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Number(f64),
    Text(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}

impl Json {
    pub(crate) fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Self::Object(fields) => fields.get(key),
            _ => None,
        }
    }

    pub(crate) fn text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            _ => None,
        }
    }

    /// The value as a Connect configuration value: strings as they are, numbers and booleans
    /// as their text.
    pub(crate) fn config_text(&self) -> Option<String> {
        match self {
            Self::Text(text) => Some(text.clone()),
            Self::Bool(value) => Some(value.to_string()),
            Self::Number(value) if value.fract() == 0.0 && value.abs() < 1e15 => Some(format!("{}", *value as i64)),
            Self::Number(value) => Some(value.to_string()),
            _ => None,
        }
    }

    pub(crate) fn write(&self) -> String {
        let mut out = String::new();
        self.write_into(&mut out);
        out
    }

    fn write_into(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Self::Number(value) if value.is_finite() => out.push_str(&value.to_string()),
            Self::Number(_) => out.push_str("null"),
            Self::Text(text) => quote(text, out),
            Self::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    item.write_into(out);
                }
                out.push(']');
            }
            Self::Object(fields) => {
                out.push('{');
                for (index, (key, value)) in fields.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    quote(key, out);
                    out.push(':');
                    value.write_into(out);
                }
                out.push('}');
            }
        }
    }
}

fn quote(text: &str, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
}

/// An object from string pairs.
pub(crate) fn object(pairs: impl IntoIterator<Item = (String, Json)>) -> Json {
    Json::Object(pairs.into_iter().collect())
}

struct Parser<'t> {
    bytes: &'t [u8],
    at: usize,
}

impl Parser<'_> {
    fn space(&mut self) {
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        self.space();
        if self.bytes.get(self.at) == Some(&byte) {
            self.at += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", char::from(byte), self.at))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > MOST_DEPTH {
            return Err("the JSON is nested too deeply".to_owned());
        }
        self.space();
        match self.bytes.get(self.at) {
            Some(b'{') => {
                self.at += 1;
                let mut fields = BTreeMap::new();
                self.space();
                if self.bytes.get(self.at) == Some(&b'}') {
                    self.at += 1;
                    return Ok(Json::Object(fields));
                }
                loop {
                    self.space();
                    let key = self.string()?;
                    self.expect(b':')?;
                    let value = self.value(depth + 1)?;
                    fields.insert(key, value);
                    self.space();
                    match self.bytes.get(self.at) {
                        Some(b',') => self.at += 1,
                        Some(b'}') => {
                            self.at += 1;
                            return Ok(Json::Object(fields));
                        }
                        _ => return Err(format!("expected ',' or '}}' at byte {}", self.at)),
                    }
                }
            }
            Some(b'[') => {
                self.at += 1;
                let mut items = Vec::new();
                self.space();
                if self.bytes.get(self.at) == Some(&b']') {
                    self.at += 1;
                    return Ok(Json::Array(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.space();
                    match self.bytes.get(self.at) {
                        Some(b',') => self.at += 1,
                        Some(b']') => {
                            self.at += 1;
                            return Ok(Json::Array(items));
                        }
                        _ => return Err(format!("expected ',' or ']' at byte {}", self.at)),
                    }
                }
            }
            Some(b'"') => self.string().map(Json::Text),
            Some(b't') if self.bytes[self.at..].starts_with(b"true") => {
                self.at += 4;
                Ok(Json::Bool(true))
            }
            Some(b'f') if self.bytes[self.at..].starts_with(b"false") => {
                self.at += 5;
                Ok(Json::Bool(false))
            }
            Some(b'n') if self.bytes[self.at..].starts_with(b"null") => {
                self.at += 4;
                Ok(Json::Null)
            }
            Some(_) => {
                let start = self.at;
                while self.bytes.get(self.at).is_some_and(|byte| byte.is_ascii_digit() || b"+-.eE".contains(byte)) {
                    self.at += 1;
                }
                std::str::from_utf8(&self.bytes[start..self.at])
                    .ok()
                    .and_then(|text| text.parse::<f64>().ok())
                    .map(Json::Number)
                    .ok_or_else(|| format!("not a JSON value at byte {start}"))
            }
            None => Err("the JSON ends early".to_owned()),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        if self.bytes.get(self.at) != Some(&b'"') {
            return Err(format!("expected a string at byte {}", self.at));
        }
        self.at += 1;
        let mut out = Vec::new();
        loop {
            let byte = *self.bytes.get(self.at).ok_or("a JSON string is not closed")?;
            self.at += 1;
            match byte {
                b'"' => return String::from_utf8(out).map_err(|_| "a JSON string is not UTF-8".to_owned()),
                b'\\' => {
                    let escaped = *self.bytes.get(self.at).ok_or("a JSON escape ends early")?;
                    self.at += 1;
                    match escaped {
                        b'"' | b'\\' | b'/' => out.push(escaped),
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'u' => {
                            let hex = self.bytes.get(self.at..self.at + 4).ok_or("a JSON \\u escape ends early")?;
                            self.at += 4;
                            let code = std::str::from_utf8(hex).ok().and_then(|hex| u32::from_str_radix(hex, 16).ok()).ok_or("a bad \\u escape")?;
                            let character = char::from_u32(code).unwrap_or('\u{FFFD}');
                            let mut buffer = [0u8; 4];
                            out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
                        }
                        _ => return Err("an unknown JSON escape".to_owned()),
                    }
                }
                other => out.push(other),
            }
        }
    }
}

/// Reads `text` as one JSON value.
pub(crate) fn parse(text: &[u8]) -> Result<Json, String> {
    let mut parser = Parser { bytes: text, at: 0 };
    let value = parser.value(0)?;
    parser.space();
    if parser.at != text.len() {
        return Err("text follows the JSON value".to_owned());
    }
    Ok(value)
}
