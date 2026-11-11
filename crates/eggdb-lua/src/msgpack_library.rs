use crate::host::Host;
use crate::interpreter::{Interpreter, Native, Unwind, raise};
use crate::value::{TableRef, Value};

type Outcome = Result<Vec<Value>, Unwind>;

const MAX_NESTING: usize = 16;
const MAX_DECODE_DEPTH: usize = 1000;

fn encode_bytes(text: &[u8], out: &mut Vec<u8>) {
    let length = text.len();
    if length < 32 {
        out.push(0xa0 | length as u8);
    } else if length <= 0xff {
        out.extend_from_slice(&[0xd9, length as u8]);
    } else if length <= 0xffff {
        out.push(0xda);
        out.extend_from_slice(&(length as u16).to_be_bytes());
    } else {
        out.push(0xdb);
        out.extend_from_slice(&(length as u32).to_be_bytes());
    }
    out.extend_from_slice(text);
}

fn encode_integer(value: i64, out: &mut Vec<u8>) {
    if value >= 0 {
        match value {
            0..=0x7f => out.push(value as u8),
            0x80..=0xff => out.extend_from_slice(&[0xcc, value as u8]),
            0x100..=0xffff => {
                out.push(0xcd);
                out.extend_from_slice(&(value as u16).to_be_bytes());
            }
            0x1_0000..=0xffff_ffff => {
                out.push(0xce);
                out.extend_from_slice(&(value as u32).to_be_bytes());
            }
            _ => {
                out.push(0xcf);
                out.extend_from_slice(&(value as u64).to_be_bytes());
            }
        }
    } else if value >= -32 {
        out.push(value as i8 as u8);
    } else if value >= -128 {
        out.extend_from_slice(&[0xd0, value as i8 as u8]);
    } else if value >= -32768 {
        out.push(0xd1);
        out.extend_from_slice(&(value as i16).to_be_bytes());
    } else if value >= i64::from(i32::MIN) {
        out.push(0xd2);
        out.extend_from_slice(&(value as i32).to_be_bytes());
    } else {
        out.push(0xd3);
        out.extend_from_slice(&value.to_be_bytes());
    }
}

fn encode_number(number: f64, out: &mut Vec<u8>) {
    if number.fract() == 0.0 && number >= -9.2e18 && number <= 9.2e18 {
        encode_integer(number as i64, out);
    } else if f64::from(number as f32) == number {
        out.push(0xca);
        out.extend_from_slice(&(number as f32).to_be_bytes());
    } else {
        out.push(0xcb);
        out.extend_from_slice(&number.to_be_bytes());
    }
}

fn header(out: &mut Vec<u8>, count: usize, fixed: u8, sixteen: u8, thirty_two: u8) {
    if count < 16 {
        out.push(fixed | count as u8);
    } else if count <= 0xffff {
        out.push(sixteen);
        out.extend_from_slice(&(count as u16).to_be_bytes());
    } else {
        out.push(thirty_two);
        out.extend_from_slice(&(count as u32).to_be_bytes());
    }
}

fn array_length(pairs: &[(Value, Value)]) -> Option<usize> {
    let mut expected = 1.0;
    for (key, _) in pairs {
        if *key != Value::Number(expected) {
            return None;
        }
        expected += 1.0;
    }
    Some(pairs.len())
}

fn encode(value: &Value, nesting: usize, out: &mut Vec<u8>) {
    match value {
        Value::Nil | Value::Function(_) => out.push(0xc0),
        Value::Boolean(flag) => out.push(if *flag { 0xc3 } else { 0xc2 }),
        Value::Number(number) => encode_number(*number, out),
        Value::String(text) => encode_bytes(text, out),
        Value::Table(_) if nesting >= MAX_NESTING => out.push(0xc0),
        Value::Table(table) => {
            let pairs = table.pairs();
            match array_length(&pairs) {
                Some(length) => {
                    header(out, length, 0x90, 0xdc, 0xdd);
                    for (_, item) in &pairs {
                        encode(item, nesting + 1, out);
                    }
                }
                None => {
                    header(out, pairs.len(), 0x80, 0xde, 0xdf);
                    for (key, item) in &pairs {
                        encode(key, nesting + 1, out);
                        encode(item, nesting + 1, out);
                    }
                }
            }
        }
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    at: usize,
}

const TRUNCATED: &str = "Missing bytes in input.";

impl Decoder<'_> {
    fn take(&mut self, count: usize) -> Result<&[u8], String> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or(TRUNCATED)?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn unsigned(&mut self, count: usize) -> Result<u64, String> {
        Ok(self.take(count)?.iter().fold(0u64, |value, byte| value << 8 | u64::from(*byte)))
    }

    fn text(&mut self, length: usize) -> Result<Value, String> {
        Ok(Value::String(self.take(length)?.to_vec().into()))
    }

    fn array(&mut self, length: usize, depth: usize) -> Result<Value, String> {
        let table = TableRef::new();
        for index in 1..=length {
            let item = self.value(depth + 1)?;
            table.set(Value::Number(index as f64), item)?;
        }
        Ok(Value::Table(table))
    }

    fn map(&mut self, length: usize, depth: usize) -> Result<Value, String> {
        let table = TableRef::new();
        for _ in 0..length {
            let key = self.value(depth + 1)?;
            let item = self.value(depth + 1)?;
            if !matches!(key, Value::Nil) {
                table.set(key, item)?;
            }
        }
        Ok(Value::Table(table))
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DECODE_DEPTH {
            return Err("Too many nested data structures.".to_owned());
        }
        let marker = self.take(1)?[0];
        Ok(match marker {
            0x00..=0x7f => Value::Number(f64::from(marker)),
            0x80..=0x8f => return self.map(usize::from(marker & 0x0f), depth),
            0x90..=0x9f => return self.array(usize::from(marker & 0x0f), depth),
            0xa0..=0xbf => return self.text(usize::from(marker & 0x1f)),
            0xc0 => Value::Nil,
            0xc2 => Value::Boolean(false),
            0xc3 => Value::Boolean(true),
            0xc4 | 0xd9 => {
                let length = self.unsigned(1)? as usize;
                return self.text(length);
            }
            0xc5 | 0xda => {
                let length = self.unsigned(2)? as usize;
                return self.text(length);
            }
            0xc6 | 0xdb => {
                let length = self.unsigned(4)? as usize;
                return self.text(length);
            }
            0xca => Value::Number(f64::from(f32::from_bits(self.unsigned(4)? as u32))),
            0xcb => Value::Number(f64::from_bits(self.unsigned(8)?)),
            0xcc => Value::Number(self.unsigned(1)? as f64),
            0xcd => Value::Number(self.unsigned(2)? as f64),
            0xce => Value::Number(self.unsigned(4)? as f64),
            0xcf => Value::Number(self.unsigned(8)? as f64),
            0xd0 => Value::Number(f64::from(self.unsigned(1)? as u8 as i8)),
            0xd1 => Value::Number(f64::from(self.unsigned(2)? as u16 as i16)),
            0xd2 => Value::Number(f64::from(self.unsigned(4)? as u32 as i32)),
            0xd3 => Value::Number(self.unsigned(8)? as i64 as f64),
            0xdc => {
                let length = self.unsigned(2)? as usize;
                return self.array(length, depth);
            }
            0xdd => {
                let length = self.unsigned(4)? as usize;
                return self.array(length, depth);
            }
            0xde => {
                let length = self.unsigned(2)? as usize;
                return self.map(length, depth);
            }
            0xdf => {
                let length = self.unsigned(4)? as usize;
                return self.map(length, depth);
            }
            0xe0..=0xff => Value::Number(f64::from(marker as i8)),
            _ => return Err("Bad data format in input.".to_owned()),
        })
    }
}

fn pack(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    if arguments.is_empty() {
        return raise(line, "MessagePack pack needs input.");
    }
    let mut out = Vec::new();
    for argument in &arguments {
        encode(argument, 0, &mut out);
    }
    Ok(vec![Value::String(out.into())])
}

fn unpack(_: &mut Interpreter, _: &mut dyn Host, arguments: Vec<Value>, line: u32) -> Outcome {
    let Some(bytes) = arguments.first().and_then(Value::to_bytes) else {
        return raise(line, "bad argument #1 to 'unpack' (string expected)");
    };
    let mut decoder = Decoder { bytes: &bytes, at: 0 };
    let mut values = Vec::new();
    while decoder.at < bytes.len() {
        match decoder.value(0) {
            Ok(value) => values.push(value),
            Err(message) => return raise(line, message),
        }
    }
    Ok(values)
}

/// Installs the `cmsgpack` library Redis gives scripts: `pack` and `unpack`.
pub(crate) fn install(interpreter: &mut Interpreter) {
    let library = TableRef::new();
    for (name, native) in [("pack", pack as Native), ("unpack", unpack)] {
        interpreter.register(&library, name, native);
    }
    interpreter.set_global("cmsgpack", Value::Table(library));
}
