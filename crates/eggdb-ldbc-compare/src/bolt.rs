use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const MAGIC: [u8; 4] = [0x60, 0x60, 0xB0, 0x17];
const HELLO: u8 = 0x01;
const RUN: u8 = 0x10;
const PULL: u8 = 0x3F;
const RESET: u8 = 0x0F;
const SUCCESS: u8 = 0x70;
const RECORD: u8 = 0x71;
const IGNORED: u8 = 0x7E;
const FAILURE: u8 = 0x7F;
const MAX_CHUNK: usize = 0xFFFF;
const MAX_MESSAGE: usize = 64 << 20;
const MAX_DEPTH: usize = 64;
const TIMEOUT: Duration = Duration::from_secs(300);

/// A PackStream value: what queries take as parameters and what records hold. Graph
/// structures (nodes, relationships, paths) come back as their tag and fields.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    List(Vec<Value>),
    Map(Vec<(String, Value)>),
    Structure(u8, Vec<Value>),
}

impl Value {
    pub(crate) fn map(entries: Vec<(&str, Value)>) -> Value {
        Value::Map(entries.into_iter().map(|(key, value)| (key.to_owned(), value)).collect())
    }
}

fn write_size(out: &mut Vec<u8>, size: usize, tiny: u8, eight: u8) {
    if size < 16 {
        out.push(tiny | size as u8);
    } else if size <= 0xFF {
        out.extend_from_slice(&[eight, size as u8]);
    } else if size <= 0xFFFF {
        out.push(eight + 1);
        out.extend_from_slice(&(size as u16).to_be_bytes());
    } else {
        out.push(eight + 2);
        out.extend_from_slice(&(size as u32).to_be_bytes());
    }
}

fn encode(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.push(0xC0),
        Value::Bool(flag) => out.push(if *flag { 0xC3 } else { 0xC2 }),
        Value::Int(number) => {
            let number = *number;
            if (-16..128).contains(&number) {
                out.push(number as i8 as u8);
            } else if i8::try_from(number).is_ok() {
                out.extend_from_slice(&[0xC8, number as i8 as u8]);
            } else if let Ok(small) = i16::try_from(number) {
                out.push(0xC9);
                out.extend_from_slice(&small.to_be_bytes());
            } else if let Ok(medium) = i32::try_from(number) {
                out.push(0xCA);
                out.extend_from_slice(&medium.to_be_bytes());
            } else {
                out.push(0xCB);
                out.extend_from_slice(&number.to_be_bytes());
            }
        }
        Value::Float(number) => {
            out.push(0xC1);
            out.extend_from_slice(&number.to_be_bytes());
        }
        Value::Text(text) => {
            if text.len() < 16 {
                out.push(0x80 | text.len() as u8);
            } else {
                write_size(out, text.len(), 0x80, 0xD0);
            }
            out.extend_from_slice(text.as_bytes());
        }
        Value::List(items) => {
            if items.len() < 16 {
                out.push(0x90 | items.len() as u8);
            } else {
                write_size(out, items.len(), 0x90, 0xD4);
            }
            items.iter().for_each(|item| encode(item, out));
        }
        Value::Map(entries) => {
            if entries.len() < 16 {
                out.push(0xA0 | entries.len() as u8);
            } else {
                write_size(out, entries.len(), 0xA0, 0xD8);
            }
            for (key, item) in entries {
                encode(&Value::Text(key.clone()), out);
                encode(item, out);
            }
        }
        Value::Structure(tag, fields) => {
            out.push(0xB0 | fields.len() as u8);
            out.push(*tag);
            fields.iter().for_each(|field| encode(field, out));
        }
    }
}

struct Decoder<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl Decoder<'_> {
    fn take(&mut self, count: usize) -> Result<&[u8], String> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or("a PackStream value is cut short")?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn length(&mut self, marker: u8, base: u8) -> Result<usize, String> {
        Ok(match marker - base {
            0 => usize::from(self.byte()?),
            1 => usize::from(u16::from_be_bytes([self.byte()?, self.byte()?])),
            _ => u32::from_be_bytes(self.take(4)?.try_into().map_err(|_| "a length is cut short")?) as usize,
        })
    }

    fn items(&mut self, count: usize, depth: usize) -> Result<Vec<Value>, String> {
        let mut items = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            items.push(self.value(depth + 1)?);
        }
        Ok(items)
    }

    fn entries(&mut self, count: usize, depth: usize) -> Result<Vec<(String, Value)>, String> {
        let mut entries = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            let Value::Text(key) = self.value(depth + 1)? else {
                return Err("a PackStream map key is not text".into());
            };
            entries.push((key, self.value(depth + 1)?));
        }
        Ok(entries)
    }

    fn text(&mut self, length: usize) -> Result<Value, String> {
        let bytes = self.take(length)?;
        String::from_utf8(bytes.to_vec()).map(Value::Text).map_err(|_| "a PackStream string is not UTF-8".into())
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err("a PackStream value nests too deeply".into());
        }
        let marker = self.byte()?;
        Ok(match marker {
            0x00..=0x7F => Value::Int(i64::from(marker)),
            0xF0..=0xFF => Value::Int(i64::from(marker as i8)),
            0x80..=0x8F => self.text(usize::from(marker & 0x0F))?,
            0x90..=0x9F => Value::List(self.items(usize::from(marker & 0x0F), depth)?),
            0xA0..=0xAF => Value::Map(self.entries(usize::from(marker & 0x0F), depth)?),
            0xB0..=0xBF => {
                let tag = self.byte()?;
                Value::Structure(tag, self.items(usize::from(marker & 0x0F), depth)?)
            }
            0xC0 => Value::Null,
            0xC1 => Value::Float(f64::from_be_bytes(self.take(8)?.try_into().map_err(|_| "a float is cut short")?)),
            0xC2 => Value::Bool(false),
            0xC3 => Value::Bool(true),
            0xC8 => Value::Int(i64::from(self.byte()? as i8)),
            0xC9 => Value::Int(i64::from(i16::from_be_bytes(self.take(2)?.try_into().map_err(|_| "an integer is cut short")?))),
            0xCA => Value::Int(i64::from(i32::from_be_bytes(self.take(4)?.try_into().map_err(|_| "an integer is cut short")?))),
            0xCB => Value::Int(i64::from_be_bytes(self.take(8)?.try_into().map_err(|_| "an integer is cut short")?)),
            0xCC..=0xCE => {
                let length = self.length(marker, 0xCC)?;
                Value::List(self.take(length)?.iter().map(|byte| Value::Int(i64::from(*byte))).collect())
            }
            0xD0..=0xD2 => {
                let length = self.length(marker, 0xD0)?;
                self.text(length)?
            }
            0xD4..=0xD6 => {
                let length = self.length(marker, 0xD4)?;
                Value::List(self.items(length, depth)?)
            }
            0xD8..=0xDA => {
                let length = self.length(marker, 0xD8)?;
                Value::Map(self.entries(length, depth)?)
            }
            other => return Err(format!("the PackStream marker {other:#04x} is not known")),
        })
    }
}

/// A Bolt connection, speaking version 5.0 or 4.4 with basic authentication in `HELLO`.
#[derive(Debug)]
pub(crate) struct BoltClient {
    stream: TcpStream,
}

impl BoltClient {
    pub(crate) fn connect(address: &str, user: &str, password: &str) -> Result<Self, String> {
        let mut stream = TcpStream::connect(address).map_err(|error| error.to_string())?;
        stream.set_read_timeout(Some(TIMEOUT)).map_err(|error| error.to_string())?;
        stream.set_nodelay(true).map_err(|error| error.to_string())?;
        let mut handshake = MAGIC.to_vec();
        handshake.extend_from_slice(&[0, 0, 0, 5, 0, 0, 4, 4, 0, 0, 0, 0, 0, 0, 0, 0]);
        stream.write_all(&handshake).map_err(|error| error.to_string())?;
        let mut chosen = [0u8; 4];
        stream.read_exact(&mut chosen).map_err(|error| error.to_string())?;
        if chosen == [0, 0, 0, 0] {
            return Err("the server speaks neither Bolt 5.0 nor 4.4".into());
        }
        let mut client = Self { stream };
        let extra = Value::map(vec![
            ("user_agent", Value::Text("eggdb-ldbc-compare/0.1".into())),
            ("scheme", Value::Text("basic".into())),
            ("principal", Value::Text(user.to_owned())),
            ("credentials", Value::Text(password.to_owned())),
        ]);
        client.send(HELLO, &[extra])?;
        client.summary()?;
        Ok(client)
    }

    fn send(&mut self, signature: u8, fields: &[Value]) -> Result<(), String> {
        let mut message = Vec::new();
        encode(&Value::Structure(signature, fields.to_vec()), &mut message);
        let mut framed = Vec::with_capacity(message.len() + 4);
        for chunk in message.chunks(MAX_CHUNK) {
            framed.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
            framed.extend_from_slice(chunk);
        }
        framed.extend_from_slice(&[0, 0]);
        self.stream.write_all(&framed).map_err(|error| error.to_string())
    }

    fn receive(&mut self) -> Result<(u8, Vec<Value>), String> {
        let mut message = Vec::new();
        loop {
            let mut size = [0u8; 2];
            self.stream.read_exact(&mut size).map_err(|error| error.to_string())?;
            let size = usize::from(u16::from_be_bytes(size));
            if size == 0 {
                if message.is_empty() {
                    continue;
                }
                break;
            }
            if message.len() + size > MAX_MESSAGE {
                return Err("a Bolt message is too large".into());
            }
            let start = message.len();
            message.resize(start + size, 0);
            self.stream.read_exact(&mut message[start..]).map_err(|error| error.to_string())?;
        }
        match (Decoder { bytes: &message, at: 0 }).value(0)? {
            Value::Structure(signature, fields) => Ok((signature, fields)),
            _ => Err("a Bolt message is not a structure".into()),
        }
    }

    fn summary(&mut self) -> Result<Vec<Value>, String> {
        let (signature, fields) = self.receive()?;
        match signature {
            SUCCESS => Ok(fields),
            FAILURE => Err(failure_text(&fields)),
            IGNORED => Err("the server ignored the request".into()),
            other => Err(format!("an unexpected Bolt message {other:#04x}")),
        }
    }

    /// Runs `query` with `parameters` in an autocommit transaction and returns its records.
    pub(crate) fn run(&mut self, query: &str, parameters: Vec<(String, Value)>) -> Result<Vec<Vec<Value>>, String> {
        self.send(RUN, &[Value::Text(query.to_owned()), Value::Map(parameters), Value::Map(Vec::new())])?;
        self.send(PULL, &[Value::map(vec![("n", Value::Int(-1))])])?;
        let mut failed = self.summary().err();
        let mut records = Vec::new();
        loop {
            let (signature, fields) = self.receive()?;
            match signature {
                RECORD => match fields.into_iter().next() {
                    Some(Value::List(values)) => records.push(values),
                    _ => return Err("a Bolt record holds no list".into()),
                },
                SUCCESS => break,
                FAILURE => {
                    failed.get_or_insert(failure_text(&fields));
                    break;
                }
                IGNORED => break,
                other => return Err(format!("an unexpected Bolt message {other:#04x}")),
            }
        }
        if let Some(message) = failed {
            self.send(RESET, &[])?;
            let _ = self.summary();
            return Err(format!("{message} (in {query})"));
        }
        Ok(records)
    }
}

fn failure_text(fields: &[Value]) -> String {
    match fields.first() {
        Some(Value::Map(entries)) => entries
            .iter()
            .find(|(key, _)| key == "message")
            .and_then(|(_, value)| match value {
                Value::Text(text) => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "the server failed the request".into()),
        _ => "the server failed the request".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packstream_values_round_trip() {
        let value = Value::map(vec![
            ("small", Value::Int(-3)),
            ("large", Value::Int(1 << 40)),
            ("text", Value::Text("a longer string than sixteen bytes".into())),
            ("list", Value::List((0..20).map(Value::Int).collect())),
            ("float", Value::Float(1.5)),
        ]);
        let mut bytes = Vec::new();
        encode(&value, &mut bytes);
        assert_eq!((Decoder { bytes: &bytes, at: 0 }).value(0).unwrap(), value);
    }
}
