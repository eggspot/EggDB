use crate::{ParquetError, damaged};

const MAX_DEPTH: usize = 32;
const MAX_LIST: usize = 1 << 24;

/// A value of Thrift's compact protocol, read without knowing the struct definitions.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Thrift {
    Bool(bool),
    Int(i64),
    Double(f64),
    Binary(Vec<u8>),
    List(Vec<Thrift>),
    Struct(Vec<(i16, Thrift)>),
}

impl Thrift {
    pub(crate) fn field(&self, id: i16) -> Option<&Thrift> {
        match self {
            Thrift::Struct(fields) => fields.iter().find(|(known, _)| *known == id).map(|(_, value)| value),
            _ => None,
        }
    }

    pub(crate) fn int(&self, id: i16) -> Option<i64> {
        match self.field(id)? {
            Thrift::Int(value) => Some(*value),
            _ => None,
        }
    }

    pub(crate) fn binary(&self, id: i16) -> Option<&[u8]> {
        match self.field(id)? {
            Thrift::Binary(bytes) => Some(bytes),
            _ => None,
        }
    }

    pub(crate) fn list(&self, id: i16) -> Option<&[Thrift]> {
        match self.field(id)? {
            Thrift::List(items) => Some(items),
            _ => None,
        }
    }

    pub(crate) fn boolean(&self, id: i16) -> Option<bool> {
        match self.field(id)? {
            Thrift::Bool(value) => Some(*value),
            _ => None,
        }
    }
}

pub(crate) struct Reader<'b> {
    bytes: &'b [u8],
    pub(crate) at: usize,
}

impl<'b> Reader<'b> {
    pub(crate) fn new(bytes: &'b [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn byte(&mut self) -> Result<u8, ParquetError> {
        let byte = *self.bytes.get(self.at).ok_or_else(|| damaged("Thrift data ends early"))?;
        self.at += 1;
        Ok(byte)
    }

    fn varint(&mut self) -> Result<u64, ParquetError> {
        eggdb_columnar::read_varint(self.bytes, &mut self.at).map_err(|error| damaged(&error.0))
    }

    fn zigzag(&mut self) -> Result<i64, ParquetError> {
        let raw = self.varint()?;
        Ok(((raw >> 1) as i64) ^ -((raw & 1) as i64))
    }

    fn value(&mut self, kind: u8, depth: usize) -> Result<Thrift, ParquetError> {
        if depth > MAX_DEPTH {
            return Err(damaged("Thrift data is nested too deeply"));
        }
        Ok(match kind {
            1 => Thrift::Bool(true),
            2 => Thrift::Bool(false),
            3 => Thrift::Int(i64::from(self.byte()? as i8)),
            4..=6 => Thrift::Int(self.zigzag()?),
            7 => {
                let raw = self.bytes.get(self.at..self.at + 8).ok_or_else(|| damaged("Thrift data ends early"))?;
                self.at += 8;
                Thrift::Double(f64::from_le_bytes(raw.try_into().unwrap_or_default()))
            }
            8 => {
                let length = usize::try_from(self.varint()?).map_err(|_| damaged("a Thrift string is too long"))?;
                let end = self.at.checked_add(length).filter(|end| *end <= self.bytes.len()).ok_or_else(|| damaged("a Thrift string runs past its bytes"))?;
                let bytes = self.bytes[self.at..end].to_vec();
                self.at = end;
                Thrift::Binary(bytes)
            }
            9 | 10 => {
                let header = self.byte()?;
                let element = header & 0x0f;
                let size = if header >> 4 == 0x0f { usize::try_from(self.varint()?).map_err(|_| damaged("a Thrift list is too long"))? } else { usize::from(header >> 4) };
                if size > MAX_LIST || size > self.bytes.len() - self.at + 1 {
                    return Err(damaged("a Thrift list is longer than its bytes"));
                }
                let mut items = Vec::with_capacity(size);
                for _ in 0..size {
                    let value = if element == 1 || element == 2 { Thrift::Bool(self.byte()? == 1) } else { self.value(element, depth + 1)? };
                    items.push(value);
                }
                Thrift::List(items)
            }
            11 => {
                let size = usize::try_from(self.varint()?).map_err(|_| damaged("a Thrift map is too long"))?;
                if size == 0 {
                    return Ok(Thrift::List(Vec::new()));
                }
                if size > MAX_LIST || size > self.bytes.len() - self.at {
                    return Err(damaged("a Thrift map is longer than its bytes"));
                }
                let kinds = self.byte()?;
                let mut items = Vec::with_capacity(size * 2);
                for _ in 0..size {
                    items.push(self.value(kinds >> 4, depth + 1)?);
                    items.push(self.value(kinds & 0x0f, depth + 1)?);
                }
                Thrift::List(items)
            }
            12 => self.structure(depth + 1)?,
            _ => return Err(damaged("Thrift data has an unknown type")),
        })
    }

    pub(crate) fn structure(&mut self, depth: usize) -> Result<Thrift, ParquetError> {
        if depth > MAX_DEPTH {
            return Err(damaged("Thrift data is nested too deeply"));
        }
        let mut fields = Vec::new();
        let mut last: i16 = 0;
        loop {
            let header = self.byte()?;
            if header == 0 {
                return Ok(Thrift::Struct(fields));
            }
            let kind = header & 0x0f;
            let delta = header >> 4;
            let id = if delta == 0 {
                i16::try_from(self.zigzag()?).map_err(|_| damaged("a Thrift field id is out of range"))?
            } else {
                last.checked_add(i16::from(delta)).ok_or_else(|| damaged("a Thrift field id is out of range"))?
            };
            last = id;
            let value = self.value(kind, depth)?;
            fields.push((id, value));
        }
    }
}

/// Writes Thrift's compact protocol.
#[derive(Default)]
pub(crate) struct Writer {
    pub(crate) out: Vec<u8>,
    last: Vec<i16>,
}

impl Writer {
    fn header(&mut self, id: i16, kind: u8) {
        let last = self.last.last().copied().unwrap_or(0);
        let delta = id - last;
        if (1..=15).contains(&delta) {
            self.out.push(((delta as u8) << 4) | kind);
        } else {
            self.out.push(kind);
            eggdb_columnar::write_varint(zigzag(i64::from(id)), &mut self.out);
        }
        if let Some(slot) = self.last.last_mut() {
            *slot = id;
        }
    }

    pub(crate) fn begin(&mut self) {
        self.last.push(0);
    }

    pub(crate) fn end(&mut self) {
        self.out.push(0);
        self.last.pop();
    }

    pub(crate) fn int(&mut self, id: i16, kind_i64: bool, value: i64) {
        self.header(id, if kind_i64 { 6 } else { 5 });
        eggdb_columnar::write_varint(zigzag(value), &mut self.out);
    }

    pub(crate) fn boolean(&mut self, id: i16, value: bool) {
        self.header(id, if value { 1 } else { 2 });
    }

    pub(crate) fn binary(&mut self, id: i16, bytes: &[u8]) {
        self.header(id, 8);
        eggdb_columnar::write_varint(bytes.len() as u64, &mut self.out);
        self.out.extend_from_slice(bytes);
    }

    pub(crate) fn begin_struct(&mut self, id: i16) {
        self.header(id, 12);
        self.begin();
    }

    pub(crate) fn begin_list(&mut self, id: i16, element: u8, size: usize) {
        self.header(id, 9);
        if size < 15 {
            self.out.push(((size as u8) << 4) | element);
        } else {
            self.out.push(0xf0 | element);
            eggdb_columnar::write_varint(size as u64, &mut self.out);
        }
    }

    pub(crate) fn list_int(&mut self, value: i64) {
        eggdb_columnar::write_varint(zigzag(value), &mut self.out);
    }

    pub(crate) fn list_binary(&mut self, bytes: &[u8]) {
        eggdb_columnar::write_varint(bytes.len() as u64, &mut self.out);
        self.out.extend_from_slice(bytes);
    }

    pub(crate) fn list_struct(&mut self) {
        self.begin();
    }
}

fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}
