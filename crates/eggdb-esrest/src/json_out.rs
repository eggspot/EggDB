use eggdb_sql::json::{Json, Node};

/// Builds JSON text.
#[derive(Debug, Default)]
pub(crate) struct Out {
    text: String,
    needs_comma: Vec<bool>,
}

pub(crate) fn escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
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
    out
}

impl Out {
    fn separate(&mut self) {
        if let Some(needs) = self.needs_comma.last_mut() {
            if *needs {
                self.text.push(',');
            }
            *needs = true;
        }
    }

    pub(crate) fn begin_object(&mut self) -> &mut Self {
        self.separate();
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
        self.separate();
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
        self.separate();
        self.text.push_str(&escaped(name));
        self.text.push(':');
        if let Some(needs) = self.needs_comma.last_mut() {
            *needs = false;
        }
        self
    }

    pub(crate) fn string(&mut self, value: &str) -> &mut Self {
        self.separate();
        self.text.push_str(&escaped(value));
        self
    }

    pub(crate) fn raw(&mut self, json: &str) -> &mut Self {
        self.separate();
        self.text.push_str(json);
        self
    }

    pub(crate) fn number(&mut self, value: f64) -> &mut Self {
        if value.is_finite() {
            if value == value.trunc() && value.abs() < 1e15 {
                self.raw(&format!("{}", value as i64))
            } else {
                self.raw(&format!("{value}"))
            }
        } else {
            self.raw("null")
        }
    }

    pub(crate) fn integer(&mut self, value: u64) -> &mut Self {
        self.raw(&value.to_string())
    }

    pub(crate) fn boolean(&mut self, value: bool) -> &mut Self {
        self.raw(if value { "true" } else { "false" })
    }

    pub(crate) fn null(&mut self) -> &mut Self {
        self.raw("null")
    }

    pub(crate) fn finish(&mut self) -> String {
        std::mem::take(&mut self.text)
    }
}

/// A JSON value written back as text.
pub(crate) fn written(node: &Node) -> String {
    match &node.value {
        Json::Null => "null".to_owned(),
        Json::Boolean(value) => value.to_string(),
        Json::Number(text) => text.clone(),
        Json::String(text) => escaped(text),
        Json::Array(items) => format!("[{}]", items.iter().map(written).collect::<Vec<_>>().join(",")),
        Json::Object(members) => format!(
            "{{{}}}",
            members
                .iter()
                .map(|(key, value)| format!("{}:{}", escaped(key), written(value)))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}
