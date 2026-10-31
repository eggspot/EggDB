use crate::document::{Bson, Document};
use std::fmt;

/// The deepest nesting of documents and arrays a read accepts.
pub const MAX_DEPTH: usize = 128;

/// Why bytes are not a valid BSON document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BsonError {
    /// What is wrong.
    pub message: String,
    /// The byte offset where it was found.
    pub offset: usize,
}

impl fmt::Display for BsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid BSON at byte {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for BsonError {}

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    fn fail<T>(&self, message: &str) -> Result<T, BsonError> {
        Err(BsonError { message: message.to_owned(), offset: self.at })
    }

    fn take(&mut self, count: usize) -> Result<&'b [u8], BsonError> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len());
        match end {
            Some(end) => {
                let taken = &self.bytes[self.at..end];
                self.at = end;
                Ok(taken)
            }
            None => self.fail("the value runs past the end of its document"),
        }
    }

    fn byte(&mut self) -> Result<u8, BsonError> {
        Ok(self.take(1)?[0])
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], BsonError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn i32(&mut self) -> Result<i32, BsonError> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    fn i64(&mut self) -> Result<i64, BsonError> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    fn cstring(&mut self) -> Result<String, BsonError> {
        let rest = &self.bytes[self.at..];
        let Some(length) = rest.iter().position(|byte| *byte == 0) else {
            return self.fail("a name is not terminated");
        };
        let text = self.take(length)?;
        self.at += 1;
        match std::str::from_utf8(text) {
            Ok(text) => Ok(text.to_owned()),
            Err(_) => self.fail("a name is not UTF-8"),
        }
    }

    fn string(&mut self) -> Result<String, BsonError> {
        let length = self.i32()?;
        let Ok(length) = usize::try_from(length) else {
            return self.fail("a string has a negative length");
        };
        if length == 0 {
            return self.fail("a string's length leaves no room for its terminator");
        }
        let text = self.take(length)?;
        if text[length - 1] != 0 {
            return self.fail("a string is not terminated");
        }
        match std::str::from_utf8(&text[..length - 1]) {
            Ok(text) => Ok(text.to_owned()),
            Err(_) => self.fail("a string is not UTF-8"),
        }
    }

    fn document(&mut self, depth: usize) -> Result<Document, BsonError> {
        Ok(Document::from_entries(self.entries(depth)?))
    }

    fn entries(&mut self, depth: usize) -> Result<Vec<(String, Bson)>, BsonError> {
        if depth > MAX_DEPTH {
            return self.fail("documents are nested too deeply");
        }
        let start = self.at;
        let length = self.i32()?;
        let Some(end) = usize::try_from(length).ok().filter(|length| *length >= 5).and_then(|length| start.checked_add(length)) else {
            return self.fail("a document's length is too small");
        };
        if end > self.bytes.len() {
            return self.fail("a document's length runs past the end of its container");
        }
        let mut entries = Vec::new();
        loop {
            let kind = self.byte()?;
            if kind == 0 {
                break;
            }
            let name = self.cstring()?;
            let value = self.value(kind, depth)?;
            if self.at >= end {
                return self.fail("an element runs past the end of its document");
            }
            entries.push((name, value));
        }
        if self.at != end {
            return self.fail("a document's length does not match its elements");
        }
        Ok(entries)
    }

    fn value(&mut self, kind: u8, depth: usize) -> Result<Bson, BsonError> {
        Ok(match kind {
            0x01 => Bson::Double(f64::from_le_bytes(self.array()?)),
            0x02 => Bson::String(self.string()?),
            0x03 => Bson::Document(self.document(depth + 1)?),
            0x04 => Bson::Array(self.entries(depth + 1)?.into_iter().map(|(_, value)| value).collect()),
            0x05 => {
                let length = self.i32()?;
                let Ok(length) = usize::try_from(length) else {
                    return self.fail("binary data has a negative length");
                };
                let subtype = self.byte()?;
                let mut bytes = self.take(length)?;
                if subtype == 0x02 {
                    let inner = Reader { bytes, at: 0 }.i32().ok().and_then(|inner| usize::try_from(inner).ok());
                    if inner != Some(length.saturating_sub(4)) {
                        return self.fail("old binary data's inner length does not match");
                    }
                    bytes = &bytes[4..];
                }
                Bson::Binary { subtype, bytes: bytes.to_vec() }
            }
            0x06 => Bson::Undefined,
            0x07 => Bson::ObjectId(self.array()?),
            0x08 => match self.byte()? {
                0 => Bson::Boolean(false),
                1 => Bson::Boolean(true),
                _ => return self.fail("a boolean is neither 0 nor 1"),
            },
            0x09 => Bson::DateTime(self.i64()?),
            0x0A => Bson::Null,
            0x0B => Bson::Regex { pattern: self.cstring()?, options: self.cstring()? },
            0x0C => Bson::DbPointer { namespace: self.string()?, id: self.array()? },
            0x0D => Bson::JavaScript(self.string()?),
            0x0E => Bson::Symbol(self.string()?),
            0x0F => {
                let start = self.at;
                let length = self.i32()?;
                let code = self.string()?;
                let scope = self.document(depth + 1)?;
                if usize::try_from(length).ok() != Some(self.at - start) {
                    return self.fail("code with scope has a wrong length");
                }
                Bson::JavaScriptWithScope { code, scope }
            }
            0x10 => Bson::Int32(self.i32()?),
            0x11 => {
                let increment = u32::from_le_bytes(self.array()?);
                let time = u32::from_le_bytes(self.array()?);
                Bson::Timestamp { time, increment }
            }
            0x12 => Bson::Int64(self.i64()?),
            0x13 => Bson::Decimal128(self.array()?),
            0xFF => Bson::MinKey,
            0x7F => Bson::MaxKey,
            _ => return self.fail("an element has an unknown type"),
        })
    }
}

/// Reads one document from the start of `bytes`; returns it and the number of bytes it took.
pub fn read_document(bytes: &[u8]) -> Result<(Document, usize), BsonError> {
    let mut reader = Reader { bytes, at: 0 };
    let document = reader.document(0)?;
    Ok((document, reader.at))
}

impl Document {
    /// Reads `bytes`, which must hold exactly one document.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, BsonError> {
        let (document, used) = read_document(bytes)?;
        if used != bytes.len() {
            return Err(BsonError { message: "bytes follow the document".to_owned(), offset: used });
        }
        Ok(document)
    }
}
