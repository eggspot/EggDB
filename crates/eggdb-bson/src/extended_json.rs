use crate::document::{Bson, Document};
use std::fmt::Write;

fn quoted(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let triple = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for at in 0..4 {
            if at <= chunk.len() {
                out.push(char::from(ALPHABET[((triple >> (18 - 6 * at)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn double(out: &mut String, value: f64) {
    if value.is_nan() {
        out.push_str("{\"$numberDouble\":\"NaN\"}");
    } else if value.is_infinite() {
        out.push_str(if value > 0.0 { "{\"$numberDouble\":\"Infinity\"}" } else { "{\"$numberDouble\":\"-Infinity\"}" });
    } else if value.fract() == 0.0 && value.abs() < 1e15 {
        let _ = write!(out, "{value:.1}");
    } else {
        let _ = write!(out, "{value}");
    }
}

fn document(out: &mut String, value: &Document) {
    out.push('{');
    for (at, (name, item)) in value.iter().enumerate() {
        if at > 0 {
            out.push_str(", ");
        }
        quoted(out, name);
        out.push_str(": ");
        write_value(out, item);
    }
    out.push('}');
}

fn write_value(out: &mut String, value: &Bson) {
    match value {
        Bson::Double(number) => double(out, *number),
        Bson::String(text) => quoted(out, text),
        Bson::Document(inner) => document(out, inner),
        Bson::Array(items) => {
            out.push('[');
            for (at, item) in items.iter().enumerate() {
                if at > 0 {
                    out.push_str(", ");
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Bson::Binary { subtype, bytes } => {
            let _ = write!(out, "{{\"$binary\": {{\"base64\": \"{}\", \"subType\": \"{subtype:02x}\"}}}}", base64(bytes));
        }
        Bson::Undefined => out.push_str("{\"$undefined\": true}"),
        Bson::ObjectId(id) => {
            let _ = write!(out, "{{\"$oid\": \"{}\"}}", hex(id));
        }
        Bson::Boolean(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Bson::DateTime(milliseconds) => {
            let _ = write!(out, "{{\"$date\": {{\"$numberLong\": \"{milliseconds}\"}}}}");
        }
        Bson::Null => out.push_str("null"),
        Bson::Regex { pattern, options } => {
            out.push_str("{\"$regularExpression\": {\"pattern\": ");
            quoted(out, pattern);
            out.push_str(", \"options\": ");
            quoted(out, options);
            out.push_str("}}");
        }
        Bson::DbPointer { namespace, id } => {
            out.push_str("{\"$dbPointer\": {\"$ref\": ");
            quoted(out, namespace);
            let _ = write!(out, ", \"$id\": {{\"$oid\": \"{}\"}}}}}}", hex(id));
        }
        Bson::JavaScript(code) => {
            out.push_str("{\"$code\": ");
            quoted(out, code);
            out.push('}');
        }
        Bson::Symbol(text) => {
            out.push_str("{\"$symbol\": ");
            quoted(out, text);
            out.push('}');
        }
        Bson::JavaScriptWithScope { code, scope } => {
            out.push_str("{\"$code\": ");
            quoted(out, code);
            out.push_str(", \"$scope\": ");
            document(out, scope);
            out.push('}');
        }
        Bson::Int32(number) => {
            let _ = write!(out, "{number}");
        }
        Bson::Timestamp { time, increment } => {
            let _ = write!(out, "{{\"$timestamp\": {{\"t\": {time}, \"i\": {increment}}}}}");
        }
        Bson::Int64(number) => {
            let _ = write!(out, "{number}");
        }
        Bson::Decimal128(_) => {
            let _ = write!(out, "{{\"$numberDecimal\": \"{}\"}}", value.as_f64().unwrap_or(f64::NAN));
        }
        Bson::MinKey => out.push_str("{\"$minKey\": 1}"),
        Bson::MaxKey => out.push_str("{\"$maxKey\": 1}"),
    }
}

/// The document as relaxed Extended JSON (MongoDB's Extended JSON v2 specification): numbers
/// and booleans as plain JSON, other types as their `$`-keyed forms.
pub fn relaxed_json(value: &Document) -> String {
    let mut out = String::new();
    document(&mut out, value);
    out
}
