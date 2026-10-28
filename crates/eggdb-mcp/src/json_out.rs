use eggdb_sql::json::{Json, Node};

#[derive(Debug, Default)]
pub(crate) struct JsonOut {
    text: String,
    needs_comma: Vec<bool>,
    after_key: bool,
}

impl JsonOut {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn len(&self) -> usize {
        self.text.len()
    }

    pub(crate) fn finish(self) -> String {
        self.text
    }

    fn value_start(&mut self) {
        if self.after_key {
            self.after_key = false;
            return;
        }
        if let Some(needs_comma) = self.needs_comma.last_mut() {
            if *needs_comma {
                self.text.push(',');
            }
            *needs_comma = true;
        }
    }

    pub(crate) fn begin_object(&mut self) -> &mut Self {
        self.value_start();
        self.text.push('{');
        self.needs_comma.push(false);
        self
    }

    pub(crate) fn end_object(&mut self) -> &mut Self {
        self.needs_comma.pop();
        self.text.push('}');
        self
    }

    pub(crate) fn begin_array(&mut self) -> &mut Self {
        self.value_start();
        self.text.push('[');
        self.needs_comma.push(false);
        self
    }

    pub(crate) fn end_array(&mut self) -> &mut Self {
        self.needs_comma.pop();
        self.text.push(']');
        self
    }

    pub(crate) fn key(&mut self, name: &str) -> &mut Self {
        self.value_start();
        push_string(&mut self.text, name);
        self.text.push(':');
        self.after_key = true;
        self
    }

    pub(crate) fn string(&mut self, text: &str) -> &mut Self {
        self.value_start();
        push_string(&mut self.text, text);
        self
    }

    pub(crate) fn unsigned(&mut self, number: u64) -> &mut Self {
        self.value_start();
        self.text.push_str(&number.to_string());
        self
    }

    pub(crate) fn signed(&mut self, number: i64) -> &mut Self {
        self.value_start();
        self.text.push_str(&number.to_string());
        self
    }

    pub(crate) fn boolean(&mut self, value: bool) -> &mut Self {
        self.value_start();
        self.text.push_str(if value { "true" } else { "false" });
        self
    }

    pub(crate) fn null(&mut self) -> &mut Self {
        self.value_start();
        self.text.push_str("null");
        self
    }

    pub(crate) fn encoded(&mut self, json: &str) -> &mut Self {
        self.value_start();
        self.text.push_str(json);
        self
    }

    pub(crate) fn node(&mut self, node: &Node) -> &mut Self {
        match &node.value {
            Json::Null => self.null(),
            Json::Boolean(value) => self.boolean(*value),
            Json::Number(text) => self.encoded(text),
            Json::String(text) => self.string(text),
            Json::Array(items) => {
                self.begin_array();
                for item in items {
                    self.node(item);
                }
                self.end_array()
            }
            Json::Object(members) => {
                self.begin_object();
                for (name, value) in members {
                    self.key(name).node(value);
                }
                self.end_object()
            }
        }
    }

    pub(crate) fn optional_string(&mut self, name: &str, value: Option<&str>) -> &mut Self {
        if let Some(value) = value {
            self.key(name).string(value);
        }
        self
    }
}

pub(crate) fn encoded_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    push_string(&mut out, text);
    out
}

pub(crate) fn push_string(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ' '..='~' => out.push(character),
            other => push_escaped(out, other),
        }
    }
    out.push('"');
}

fn push_escaped(out: &mut String, character: char) {
    let mut units = [0u16; 2];
    for unit in character.encode_utf16(&mut units) {
        out.push_str(&format!("\\u{unit:04x}"));
    }
}

#[cfg(test)]
mod tests {
    use super::{JsonOut, encoded_string};

    #[test]
    fn output_is_one_ascii_line_whatever_the_text_holds() {
        let hostile =
            "a\"}]}\n{\"jsonrpc\":\"2.0\"}\r\t\u{0}\u{7f}\u{85}\u{2028}\u{2029}\u{202e}é😀\\";
        let encoded = encoded_string(hostile);
        assert!(encoded.is_ascii());
        assert!(!encoded.contains('\n') && !encoded.contains('\r'));
        assert_eq!(
            encoded,
            "\"a\\\"}]}\\n{\\\"jsonrpc\\\":\\\"2.0\\\"}\\r\\t\\u0000\\u007f\\u0085\\u2028\\u2029\\u202e\\u00e9\\ud83d\\ude00\\\\\""
        );
        let parsed = eggdb_sql::json::parse_document(&encoded).unwrap();
        assert_eq!(
            parsed.value,
            eggdb_sql::json::Json::String(hostile.to_owned())
        );
    }

    #[test]
    fn objects_and_arrays_get_their_commas() {
        let mut out = JsonOut::new();
        out.begin_object()
            .key("a")
            .unsigned(1)
            .key("b")
            .begin_array()
            .boolean(true)
            .null()
            .signed(-2)
            .end_array()
            .key("c")
            .begin_object()
            .end_object()
            .optional_string("d", None)
            .optional_string("e", Some("x"))
            .end_object();
        assert_eq!(
            out.finish(),
            "{\"a\":1,\"b\":[true,null,-2],\"c\":{},\"e\":\"x\"}"
        );
    }
}
