use eggdb_graph::Value;

const MAX_DEPTH: usize = 64;

/// A PackStream value as read from a client: graph values plus the structures messages are.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Packed {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    Bytes(Vec<u8>),
    String(String),
    List(Vec<Packed>),
    Map(Vec<(String, Packed)>),
    Struct(u8, Vec<Packed>),
}

impl Packed {
    pub(crate) fn get(&self, key: &str) -> Option<&Packed> {
        match self {
            Packed::Map(entries) => entries.iter().find(|(name, _)| name == key).map(|(_, value)| value),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Packed::String(text) => Some(text),
            _ => None,
        }
    }

    pub(crate) fn to_value(&self) -> Result<Value, String> {
        Ok(match self {
            Packed::Null => Value::Null,
            Packed::Boolean(flag) => Value::Boolean(*flag),
            Packed::Integer(number) => Value::Integer(*number),
            Packed::Float(number) => Value::Float(*number),
            Packed::String(text) => Value::String(text.clone()),
            Packed::List(items) => Value::List(items.iter().map(Packed::to_value).collect::<Result<_, _>>()?),
            Packed::Map(entries) => Value::Map(entries.iter().map(|(name, value)| Ok((name.clone(), value.to_value()?))).collect::<Result<_, String>>()?),
            Packed::Bytes(_) => return Err("byte array parameters are not supported yet".to_owned()),
            Packed::Struct(..) => return Err("temporal and spatial parameters are not supported yet".to_owned()),
        })
    }
}

pub(crate) struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    pub(crate) fn new(bytes: &'b [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    pub(crate) fn finished(&self) -> bool {
        self.at == self.bytes.len()
    }

    fn take(&mut self, count: usize) -> Result<&'b [u8], String> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or("a PackStream value runs past its message")?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    fn length(&mut self, marker: u8, base: u8) -> Result<usize, String> {
        Ok(match marker - base {
            0 => usize::from(self.take(1)?[0]),
            1 => usize::from(u16::from_be_bytes(self.take(2)?.try_into().unwrap_or_default())),
            _ => u32::from_be_bytes(self.take(4)?.try_into().unwrap_or_default()) as usize,
        })
    }

    fn items(&mut self, count: usize, depth: usize) -> Result<Vec<Packed>, String> {
        if count > self.bytes.len() - self.at {
            return Err("a PackStream list is longer than its message".to_owned());
        }
        (0..count).map(|_| self.value(depth + 1)).collect()
    }

    fn entries(&mut self, count: usize, depth: usize) -> Result<Vec<(String, Packed)>, String> {
        if count > self.bytes.len() - self.at {
            return Err("a PackStream map is longer than its message".to_owned());
        }
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let Packed::String(key) = self.value(depth + 1)? else {
                return Err("a PackStream map key is not a string".to_owned());
            };
            entries.push((key, self.value(depth + 1)?));
        }
        Ok(entries)
    }

    pub(crate) fn value(&mut self, depth: usize) -> Result<Packed, String> {
        if depth > MAX_DEPTH {
            return Err("PackStream values are nested too deeply".to_owned());
        }
        let marker = self.take(1)?[0];
        Ok(match marker {
            0x00..=0x7f => Packed::Integer(i64::from(marker)),
            0xf0..=0xff => Packed::Integer(i64::from(marker as i8)),
            0xc0 => Packed::Null,
            0xc2 => Packed::Boolean(false),
            0xc3 => Packed::Boolean(true),
            0xc1 => Packed::Float(f64::from_be_bytes(self.take(8)?.try_into().unwrap_or_default())),
            0xc8 => Packed::Integer(i64::from(self.take(1)?[0] as i8)),
            0xc9 => Packed::Integer(i64::from(i16::from_be_bytes(self.take(2)?.try_into().unwrap_or_default()))),
            0xca => Packed::Integer(i64::from(i32::from_be_bytes(self.take(4)?.try_into().unwrap_or_default()))),
            0xcb => Packed::Integer(i64::from_be_bytes(self.take(8)?.try_into().unwrap_or_default())),
            0xcc..=0xce => {
                let length = self.length(marker, 0xcc)?;
                Packed::Bytes(self.take(length)?.to_vec())
            }
            0x80..=0x8f | 0xd0..=0xd2 => {
                let length = if marker <= 0x8f { usize::from(marker & 0x0f) } else { self.length(marker, 0xd0)? };
                Packed::String(String::from_utf8(self.take(length)?.to_vec()).map_err(|_| "a PackStream string is not UTF-8")?)
            }
            0x90..=0x9f | 0xd4..=0xd6 => {
                let length = if marker <= 0x9f { usize::from(marker & 0x0f) } else { self.length(marker, 0xd4)? };
                Packed::List(self.items(length, depth)?)
            }
            0xa0..=0xaf | 0xd8..=0xda => {
                let length = if marker <= 0xaf { usize::from(marker & 0x0f) } else { self.length(marker, 0xd8)? };
                Packed::Map(self.entries(length, depth)?)
            }
            0xb0..=0xbf => {
                let signature = self.take(1)?[0];
                Packed::Struct(signature, self.items(usize::from(marker & 0x0f), depth)?)
            }
            _ => return Err(format!("unknown PackStream marker {marker:#04x}")),
        })
    }
}

pub(crate) fn write_integer(out: &mut Vec<u8>, value: i64) {
    match value {
        -16..=127 => out.push(value as u8),
        -128..=-17 => {
            out.push(0xc8);
            out.push(value as u8);
        }
        -32_768..=32_767 => {
            out.push(0xc9);
            out.extend_from_slice(&(value as i16).to_be_bytes());
        }
        -2_147_483_648..=2_147_483_647 => {
            out.push(0xca);
            out.extend_from_slice(&(value as i32).to_be_bytes());
        }
        _ => {
            out.push(0xcb);
            out.extend_from_slice(&value.to_be_bytes());
        }
    }
}

fn write_size(out: &mut Vec<u8>, length: usize, tiny: u8, sized: u8) {
    match length {
        0..=15 => out.push(tiny | length as u8),
        16..=255 => {
            out.push(sized);
            out.push(length as u8);
        }
        256..=65_535 => {
            out.push(sized + 1);
            out.extend_from_slice(&(length as u16).to_be_bytes());
        }
        _ => {
            out.push(sized + 2);
            out.extend_from_slice(&(length as u32).to_be_bytes());
        }
    }
}

pub(crate) fn write_string(out: &mut Vec<u8>, text: &str) {
    write_size(out, text.len(), 0x80, 0xd0);
    out.extend_from_slice(text.as_bytes());
}

pub(crate) fn write_map_header(out: &mut Vec<u8>, length: usize) {
    write_size(out, length, 0xa0, 0xd8);
}

pub(crate) fn write_list_header(out: &mut Vec<u8>, length: usize) {
    write_size(out, length, 0x90, 0xd4);
}

fn write_properties(out: &mut Vec<u8>, properties: &[(String, Value)], element_ids: bool) {
    write_map_header(out, properties.len());
    for (name, value) in properties {
        write_string(out, name);
        write_value(out, value, element_ids);
    }
}

/// Writes a graph value; with `element_ids` (Bolt 5), nodes and relationships carry their
/// string element ids too.
pub(crate) fn write_value(out: &mut Vec<u8>, value: &Value, element_ids: bool) {
    match value {
        Value::Null => out.push(0xc0),
        Value::Boolean(flag) => out.push(if *flag { 0xc3 } else { 0xc2 }),
        Value::Integer(number) => write_integer(out, *number),
        Value::Float(number) => {
            out.push(0xc1);
            out.extend_from_slice(&number.to_be_bytes());
        }
        Value::String(text) => write_string(out, text),
        Value::List(items) => {
            write_list_header(out, items.len());
            for item in items {
                write_value(out, item, element_ids);
            }
        }
        Value::Map(entries) => write_properties(out, entries, element_ids),
        Value::Node(node) => {
            out.push(if element_ids { 0xb4 } else { 0xb3 });
            out.push(0x4e);
            write_integer(out, node.id as i64);
            write_list_header(out, node.labels.len());
            for label in &node.labels {
                write_string(out, label);
            }
            write_properties(out, &node.properties, element_ids);
            if element_ids {
                write_string(out, &node.id.to_string());
            }
        }
        Value::Relationship(relationship) => {
            out.push(if element_ids { 0xb8 } else { 0xb5 });
            out.push(0x52);
            write_integer(out, relationship.id as i64);
            write_integer(out, relationship.start as i64);
            write_integer(out, relationship.end as i64);
            write_string(out, &relationship.kind);
            write_properties(out, &relationship.properties, element_ids);
            if element_ids {
                write_string(out, &relationship.id.to_string());
                write_string(out, &relationship.start.to_string());
                write_string(out, &relationship.end.to_string());
            }
        }
    }
}
