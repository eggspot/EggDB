use crate::host::Host;
use crate::interpreter::{Interpreter, Native, Unwind, raise};
use crate::value::{TableRef, Value, number_text};

type Outcome = Result<Vec<Value>, Unwind>;

const MAX_DEPTH: usize = 1000;
const SPARSE_RATIO: usize = 2;
const SPARSE_SAFE: usize = 10;

enum Shape {
    Array(usize),
    Object,
}

fn shape(pairs: &[(Value, Value)]) -> Result<Shape, String> {
    if pairs.is_empty() {
        return Ok(Shape::Object);
    }
    let mut largest = 0usize;
    for (key, _) in pairs {
        match key {
            Value::Number(number) if *number >= 1.0 && number.fract() == 0.0 && *number < 1e15 => {
                largest = largest.max(*number as usize);
            }
            _ => return Ok(Shape::Object),
        }
    }
    if largest > pairs.len() * SPARSE_RATIO && largest > SPARSE_SAFE {
        return Err("Cannot serialise table: excessively sparse array".to_owned());
    }
    Ok(Shape::Array(largest))
}

fn encode_string(text: &[u8], out: &mut Vec<u8>) {
    out.push(b'"');
    for &byte in text {
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'/' => out.extend_from_slice(b"\\/"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            0x00..=0x1f | 0x7f => out.extend_from_slice(format!("\\u{byte:04x}").as_bytes()),
            other => out.push(other),
        }
    }
    out.push(b'"');
}

fn encode(value: &Value, null: &TableRef, depth: usize, out: &mut Vec<u8>) -> Result<(), String> {
    match value {
        Value::Nil => out.extend_from_slice(b"null"),
        Value::Boolean(flag) => out.extend_from_slice(if *flag { b"true" } else { b"false" }),
        Value::Number(number) => {
            if !number.is_finite() {
                return Err("Cannot serialise number: must not be NaN or Inf".to_owned());
            }
            out.extend_from_slice(number_text(*number).as_bytes());
        }
        Value::String(text) => encode_string(text, out),
        Value::Table(table) if table.identity() == null.identity() => out.extend_from_slice(b"null"),
        Value::Table(table) => {
            if depth >= MAX_DEPTH {
                return Err(format!("Cannot serialise, excessive nesting ({})", depth + 1));
            }
            let pairs = table.pairs();
            match shape(&pairs)? {
                Shape::Array(length) => {
                    out.push(b'[');
                    for at in 1..=length {
                        if at > 1 {
                            out.push(b',');
                        }
                        encode(&table.get(&Value::Number(at as f64)), null, depth + 1, out)?;
                    }
                    out.push(b']');
                }
                Shape::Object => {
                    out.push(b'{');
                    for (index, (key, item)) in pairs.iter().enumerate() {
                        if index > 0 {
                            out.push(b',');
                        }
                        match key {
                            Value::String(text) => encode_string(text, out),
                            Value::Number(_) => encode_string(&key.to_bytes().unwrap_or_default(), out),
                            other => {
                                return Err(format!(
                                    "Cannot serialise {}: table key must be a number or string",
                                    other.type_name()
                                ));
                            }
                        }
                        out.push(b':');
                        encode(item, null, depth + 1, out)?;
                    }
                    out.push(b'}');
                }
            }
        }
        Value::Function(_) => return Err("Cannot serialise function: type not supported".to_owned()),
    }
    Ok(())
}

struct Decoder<'a> {
    text: &'a [u8],
    at: usize,
    null: TableRef,
}

impl Decoder<'_> {
    fn error(&self, what: &str) -> String {
        format!("Expected {what} but found invalid token at character {}", self.at + 1)
    }

    fn skip_space(&mut self) {
        while self.text.get(self.at).is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn literal(&mut self, word: &[u8], value: Value) -> Result<Value, String> {
        if self.text[self.at..].starts_with(word) {
            self.at += word.len();
            Ok(value)
        } else {
            Err(self.error("value"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err(format!("Found too many nested data structures ({depth}) at character {}", self.at + 1));
        }
        self.skip_space();
        match self.text.get(self.at) {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(|text| Value::String(text.into())),
            Some(b't') => self.literal(b"true", Value::Boolean(true)),
            Some(b'f') => self.literal(b"false", Value::Boolean(false)),
            Some(b'n') => {
                let null = Value::Table(self.null.clone());
                self.literal(b"null", null)
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.error("value")),
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.at;
        let text = self.text;
        while text
            .get(self.at)
            .is_some_and(|byte| matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'))
        {
            self.at += 1;
        }
        std::str::from_utf8(&text[start..self.at])
            .ok()
            .and_then(|text| text.parse::<f64>().ok())
            .map(Value::Number)
            .ok_or_else(|| {
                self.at = start;
                self.error("value")
            })
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self.text.get(self.at..self.at + 4).ok_or_else(|| self.error("unicode escape"))?;
        let value = std::str::from_utf8(digits)
            .ok()
            .and_then(|digits| u32::from_str_radix(digits, 16).ok())
            .ok_or_else(|| self.error("unicode escape"))?;
        self.at += 4;
        Ok(value)
    }

    fn string(&mut self) -> Result<Vec<u8>, String> {
        self.at += 1;
        let mut out = Vec::new();
        loop {
            let Some(&byte) = self.text.get(self.at) else {
                return Err(self.error("closing quote"));
            };
            self.at += 1;
            match byte {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(&escape) = self.text.get(self.at) else {
                        return Err(self.error("escape"));
                    };
                    self.at += 1;
                    match escape {
                        b'"' | b'\\' | b'/' => out.push(escape),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let mut code = self.hex4()?;
                            if (0xd800..0xdc00).contains(&code) && self.text[self.at..].starts_with(b"\\u") {
                                self.at += 2;
                                let low = self.hex4()?;
                                if !(0xdc00..0xe000).contains(&low) {
                                    return Err(self.error("low surrogate"));
                                }
                                code = 0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00);
                            }
                            let character = char::from_u32(code).ok_or_else(|| self.error("unicode escape"))?;
                            let mut buffer = [0u8; 4];
                            out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
                        }
                        _ => return Err(self.error("escape")),
                    }
                }
                other => out.push(other),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, String> {
        self.at += 1;
        let table = TableRef::new();
        self.skip_space();
        if self.text.get(self.at) == Some(&b']') {
            self.at += 1;
            return Ok(Value::Table(table));
        }
        let mut index = 1usize;
        loop {
            let item = self.value(depth + 1)?;
            table.set(Value::Number(index as f64), item)?;
            index += 1;
            self.skip_space();
            match self.text.get(self.at) {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    return Ok(Value::Table(table));
                }
                _ => return Err(self.error("comma or array end")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, String> {
        self.at += 1;
        let table = TableRef::new();
        self.skip_space();
        if self.text.get(self.at) == Some(&b'}') {
            self.at += 1;
            return Ok(Value::Table(table));
        }
        loop {
            self.skip_space();
            if self.text.get(self.at) != Some(&b'"') {
                return Err(self.error("object key string"));
            }
            let key = self.string()?;
            self.skip_space();
            if self.text.get(self.at) != Some(&b':') {
                return Err(self.error("colon"));
            }
            self.at += 1;
            let item = self.value(depth + 1)?;
            table.set(Value::String(key.into()), item)?;
            self.skip_space();
            match self.text.get(self.at) {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(Value::Table(table));
                }
                _ => return Err(self.error("comma or object end")),
            }
        }
    }
}

fn json_encode(interpreter: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    if arguments.len() != 1 {
        return raise(line, "bad argument #1 to 'encode' (expected 1 argument)");
    }
    let mut out = Vec::new();
    match encode(&arguments[0], &interpreter.json_null, 0, &mut out) {
        Ok(()) => Ok(vec![Value::String(out.into())]),
        Err(message) => raise(line, message),
    }
}

fn json_decode(interpreter: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let Some(text) = arguments.first().and_then(Value::to_bytes) else {
        return raise(line, "bad argument #1 to 'decode' (string expected)");
    };
    let mut decoder = Decoder {
        text: &text,
        at: 0,
        null: interpreter.json_null.clone(),
    };
    let value = match decoder.value(1) {
        Ok(value) => value,
        Err(message) => return raise(line, message),
    };
    decoder.skip_space();
    if decoder.at != text.len() {
        let message = decoder.error("the end");
        return raise(line, message);
    }
    Ok(vec![value])
}

/// Installs the `cjson` library Redis gives scripts: `encode`, `decode` and the `null` sentinel.
pub(crate) fn install(interpreter: &mut Interpreter) {
    let library = TableRef::new();
    for (name, native) in [("encode", json_encode as Native), ("decode", json_decode)] {
        interpreter.register(&library, name, native);
    }
    let _ = library.set(Value::string("null"), Value::Table(interpreter.json_null.clone()));
    interpreter.set_global("cjson", Value::Table(library));
}
