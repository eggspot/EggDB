//! A small JSON reader and writer (RFC 8259) for JMAP requests and responses, with a nesting
//! limit so a hostile body cannot exhaust the stack.

const MAX_DEPTH: usize = 64;

/// A JSON value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub(crate) fn get(&self, name: &str) -> Option<&Json> {
        match self {
            Self::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, value)| value),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }

    pub(crate) fn as_array(&self) -> Option<&[Json]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    pub(crate) fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(number) if *number >= 0.0 && number.fract() == 0.0 && *number < 9.0e15 => Some(*number as u64),
            _ => None,
        }
    }

    pub(crate) fn to_text(&self) -> String {
        match self {
            Self::Null => "null".to_owned(),
            Self::Bool(flag) => flag.to_string(),
            Self::Number(number) if number.fract() == 0.0 && number.abs() < 9.0e15 => format!("{}", *number as i64),
            Self::Number(number) => format!("{number}"),
            Self::String(text) => string(text),
            Self::Array(items) => format!("[{}]", items.iter().map(Self::to_text).collect::<Vec<_>>().join(",")),
            Self::Object(members) => format!(
                "{{{}}}",
                members.iter().map(|(key, value)| format!("{}:{}", string(key), value.to_text())).collect::<Vec<_>>().join(",")
            ),
        }
    }
}

/// `text` as a JSON string.
pub(crate) fn string(text: &str) -> String {
    let mut out = String::from("\"");
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
    out
}

struct Reader<'t> {
    text: &'t str,
    at: usize,
}

impl Reader<'_> {
    fn skip(&mut self) {
        while self.text.as_bytes().get(self.at).is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, literal: &str) -> bool {
        if self.text[self.at..].starts_with(literal) {
            self.at += literal.len();
            return true;
        }
        false
    }

    fn value(&mut self, depth: usize) -> Option<Json> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.skip();
        let byte = *self.text.as_bytes().get(self.at)?;
        Some(match byte {
            b'n' if self.eat("null") => Json::Null,
            b't' if self.eat("true") => Json::Bool(true),
            b'f' if self.eat("false") => Json::Bool(false),
            b'"' => Json::String(self.string()?),
            b'[' => {
                self.at += 1;
                let mut items = Vec::new();
                self.skip();
                if self.eat("]") {
                    return Some(Json::Array(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.skip();
                    if self.eat("]") {
                        break;
                    }
                    if !self.eat(",") {
                        return None;
                    }
                }
                Json::Array(items)
            }
            b'{' => {
                self.at += 1;
                let mut members = Vec::new();
                self.skip();
                if self.eat("}") {
                    return Some(Json::Object(members));
                }
                loop {
                    self.skip();
                    if self.text.as_bytes().get(self.at) != Some(&b'"') {
                        return None;
                    }
                    let key = self.string()?;
                    self.skip();
                    if !self.eat(":") {
                        return None;
                    }
                    members.push((key, self.value(depth + 1)?));
                    self.skip();
                    if self.eat("}") {
                        break;
                    }
                    if !self.eat(",") {
                        return None;
                    }
                }
                Json::Object(members)
            }
            b'-' | b'0'..=b'9' => {
                let start = self.at;
                while self.text.as_bytes().get(self.at).is_some_and(|byte| byte.is_ascii_digit() || matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E')) {
                    self.at += 1;
                }
                Json::Number(self.text[start..self.at].parse().ok()?)
            }
            _ => return None,
        })
    }

    fn string(&mut self) -> Option<String> {
        self.at += 1;
        let mut out = String::new();
        loop {
            let character = self.text.get(self.at..)?.chars().next()?;
            self.at += character.len_utf8();
            match character {
                '"' => return Some(out),
                '\\' => {
                    let escaped = self.text.get(self.at..)?.chars().next()?;
                    self.at += 1;
                    out.push(match escaped {
                        '"' => '"',
                        '\\' => '\\',
                        '/' => '/',
                        'b' => '\u{8}',
                        'f' => '\u{c}',
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        'u' => {
                            let mut code = u32::from_str_radix(self.text.get(self.at..self.at + 4)?, 16).ok()?;
                            self.at += 4;
                            if (0xD800..0xDC00).contains(&code) && self.text[self.at..].starts_with("\\u") {
                                let low = u32::from_str_radix(self.text.get(self.at + 2..self.at + 6)?, 16).ok()?;
                                self.at += 6;
                                code = 0x10000 + ((code - 0xD800) << 10) + (low.checked_sub(0xDC00)? & 0x3ff);
                            }
                            char::from_u32(code)?
                        }
                        _ => return None,
                    });
                }
                other => out.push(other),
            }
        }
    }
}

/// The JSON document `text`, or `None` when it is not one.
pub(crate) fn parse(text: &str) -> Option<Json> {
    let mut reader = Reader { text, at: 0 };
    let value = reader.value(0)?;
    reader.skip();
    (reader.at == text.len()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_round_trip() {
        let document = parse(r#"{"a": [1, 2.5, "x\"yé"], "b": {"c": null, "d": true}}"#).unwrap();
        assert_eq!(document.get("a").and_then(Json::as_array).map(<[Json]>::len), Some(3));
        assert_eq!(document.to_text(), "{\"a\":[1,2.5,\"x\\\"yé\"],\"b\":{\"c\":null,\"d\":true}}");
        assert_eq!(parse("{\"a\": }"), None);
        assert_eq!(parse(&"[".repeat(500)), None);
    }
}
