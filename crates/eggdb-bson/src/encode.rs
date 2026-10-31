use crate::document::{Bson, Document};

fn cstring(out: &mut Vec<u8>, text: &str) {
    out.extend(text.bytes().filter(|byte| *byte != 0));
    out.push(0);
}

fn string(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as i32 + 1).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
    out.push(0);
}

fn entries<'e>(out: &mut Vec<u8>, entries: impl Iterator<Item = (std::borrow::Cow<'e, str>, &'e Bson)>) {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    for (name, value) in entries {
        out.push(kind(value));
        cstring(out, &name);
        write_value(out, value);
    }
    out.push(0);
    let length = (out.len() - start) as i32;
    out[start..start + 4].copy_from_slice(&length.to_le_bytes());
}

fn kind(value: &Bson) -> u8 {
    match value {
        Bson::MinKey => 0xFF,
        Bson::MaxKey => 0x7F,
        other => other.type_number() as u8,
    }
}

fn write_value(out: &mut Vec<u8>, value: &Bson) {
    match value {
        Bson::Double(number) => out.extend_from_slice(&number.to_le_bytes()),
        Bson::String(text) | Bson::JavaScript(text) | Bson::Symbol(text) => string(out, text),
        Bson::Document(document) => document.write(out),
        Bson::Array(items) => entries(
            out,
            items.iter().enumerate().map(|(at, item)| (std::borrow::Cow::Owned(at.to_string()), item)),
        ),
        Bson::Binary { subtype, bytes } => {
            out.extend_from_slice(&(bytes.len() as i32).to_le_bytes());
            out.push(*subtype);
            out.extend_from_slice(bytes);
        }
        Bson::Undefined | Bson::Null | Bson::MinKey | Bson::MaxKey => {}
        Bson::ObjectId(id) => out.extend_from_slice(id),
        Bson::Boolean(value) => out.push(u8::from(*value)),
        Bson::DateTime(milliseconds) => out.extend_from_slice(&milliseconds.to_le_bytes()),
        Bson::Regex { pattern, options } => {
            cstring(out, pattern);
            cstring(out, options);
        }
        Bson::DbPointer { namespace, id } => {
            string(out, namespace);
            out.extend_from_slice(id);
        }
        Bson::JavaScriptWithScope { code, scope } => {
            let start = out.len();
            out.extend_from_slice(&[0; 4]);
            string(out, code);
            scope.write(out);
            let length = (out.len() - start) as i32;
            out[start..start + 4].copy_from_slice(&length.to_le_bytes());
        }
        Bson::Int32(number) => out.extend_from_slice(&number.to_le_bytes()),
        Bson::Timestamp { time, increment } => {
            out.extend_from_slice(&increment.to_le_bytes());
            out.extend_from_slice(&time.to_le_bytes());
        }
        Bson::Int64(number) => out.extend_from_slice(&number.to_le_bytes()),
        Bson::Decimal128(bytes) => out.extend_from_slice(bytes),
    }
}

impl Document {
    /// Appends the document's BSON bytes to `out`. A NUL inside a name or a regular expression,
    /// which BSON cannot hold there, is left out.
    pub fn write(&self, out: &mut Vec<u8>) {
        entries(out, self.iter().map(|(name, value)| (std::borrow::Cow::Borrowed(name.as_str()), value)));
    }

    /// The document's BSON bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write(&mut out);
        out
    }
}
