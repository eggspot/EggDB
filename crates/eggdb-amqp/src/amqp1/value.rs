const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    Ubyte(u8),
    Ushort(u16),
    Uint(u32),
    Ulong(u64),
    Signed(i64),
    Fixed(u8, Vec<u8>),
    Binary(Vec<u8>),
    String(String),
    Symbol(String),
    List(Vec<Value>),
    Map(Vec<(Value, Value)>),
    Array(Vec<Value>),
    Described(Box<Value>, Box<Value>),
}

impl Value {
    pub(crate) fn described(code: u64, fields: Vec<Value>) -> Self {
        let mut fields = fields;
        while fields.last() == Some(&Value::Null) {
            fields.pop();
        }
        Value::Described(Box::new(Value::Ulong(code)), Box::new(Value::List(fields)))
    }

    pub(crate) fn unsigned(&self) -> Option<u64> {
        match self {
            Value::Ubyte(value) => Some(u64::from(*value)),
            Value::Ushort(value) => Some(u64::from(*value)),
            Value::Uint(value) => Some(u64::from(*value)),
            Value::Ulong(value) => Some(*value),
            _ => None,
        }
    }

    pub(crate) fn flag(&self) -> Option<bool> {
        match self {
            Value::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub(crate) fn text(&self) -> Option<&str> {
        match self {
            Value::String(text) | Value::Symbol(text) => Some(text),
            _ => None,
        }
    }

    pub(crate) fn descriptor(&self) -> Option<u64> {
        match self {
            Value::Described(descriptor, _) => descriptor.unsigned(),
            _ => None,
        }
    }

    pub(crate) fn fields(&self) -> &[Value] {
        match self {
            Value::Described(_, inner) => match inner.as_ref() {
                Value::List(fields) => fields,
                _ => &[],
            },
            _ => &[],
        }
    }

    pub(crate) fn field(&self, index: usize) -> &Value {
        self.fields().get(index).unwrap_or(&Value::Null)
    }
}

fn fixed_width(constructor: u8) -> Option<usize> {
    match constructor {
        0x40..=0x45 => Some(0),
        0x50..=0x56 => Some(1),
        0x60..=0x6f => Some(2),
        0x70..=0x7f => Some(4),
        0x80..=0x8f => Some(8),
        0x90..=0x9f => Some(16),
        _ => None,
    }
}

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    fn take(&mut self, count: usize) -> Result<&'b [u8], String> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or("an AMQP value is cut short")?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn size(&mut self, wide: bool) -> Result<usize, String> {
        if wide {
            let raw = self.take(4)?;
            usize::try_from(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]])).map_err(|_| "an AMQP size does not fit".to_owned())
        } else {
            Ok(usize::from(self.byte()?))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err("AMQP values are nested too deeply".to_owned());
        }
        let constructor = self.byte()?;
        if constructor == 0x00 {
            let descriptor = self.value(depth + 1)?;
            let value = self.value(depth + 1)?;
            return Ok(Value::Described(Box::new(descriptor), Box::new(value)));
        }
        self.with_constructor(constructor, depth)
    }

    fn with_constructor(&mut self, constructor: u8, depth: usize) -> Result<Value, String> {
        let text = |bytes: &[u8]| String::from_utf8(bytes.to_vec()).map_err(|_| "an AMQP string is not UTF-8".to_owned());
        Ok(match constructor {
            0x40 => Value::Null,
            0x41 => Value::Bool(true),
            0x42 => Value::Bool(false),
            0x56 => Value::Bool(self.byte()? != 0),
            0x43 => Value::Uint(0),
            0x44 => Value::Ulong(0),
            0x50 => Value::Ubyte(self.byte()?),
            0x52 => Value::Uint(u32::from(self.byte()?)),
            0x53 => Value::Ulong(u64::from(self.byte()?)),
            0x60 => {
                let raw = self.take(2)?;
                Value::Ushort(u16::from_be_bytes([raw[0], raw[1]]))
            }
            0x70 => {
                let raw = self.take(4)?;
                Value::Uint(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
            }
            0x80 => {
                let raw = self.take(8)?;
                Value::Ulong(u64::from_be_bytes(raw.try_into().map_err(|_| "an AMQP ulong is cut short".to_owned())?))
            }
            0x51 => Value::Signed(i64::from(i8::from_be_bytes([self.byte()?]))),
            0x54 => Value::Signed(i64::from(i8::from_be_bytes([self.byte()?]))),
            0x55 => Value::Signed(i64::from(i8::from_be_bytes([self.byte()?]))),
            0x61 => {
                let raw = self.take(2)?;
                Value::Signed(i64::from(i16::from_be_bytes([raw[0], raw[1]])))
            }
            0x71 => {
                let raw = self.take(4)?;
                Value::Signed(i64::from(i32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]])))
            }
            0x81 => {
                let raw = self.take(8)?;
                Value::Signed(i64::from_be_bytes(raw.try_into().map_err(|_| "an AMQP long is cut short".to_owned())?))
            }
            0xa0 | 0xb0 => {
                let size = self.size(constructor == 0xb0)?;
                Value::Binary(self.take(size)?.to_vec())
            }
            0xa1 | 0xb1 => {
                let size = self.size(constructor == 0xb1)?;
                Value::String(text(self.take(size)?)?)
            }
            0xa3 | 0xb3 => {
                let size = self.size(constructor == 0xb3)?;
                Value::Symbol(text(self.take(size)?)?)
            }
            0xc0 | 0xd0 | 0xc1 | 0xd1 => {
                let wide = constructor & 0xf0 == 0xd0;
                let size = self.size(wide)?;
                let body = self.take(size)?;
                let mut inner = Reader { bytes: body, at: 0 };
                let count = inner.size(wide)?;
                if count > body.len() {
                    return Err("an AMQP compound claims more elements than it holds".to_owned());
                }
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    items.push(inner.value(depth + 1)?);
                }
                if constructor & 0x0f == 0x01 {
                    if count % 2 != 0 {
                        return Err("an AMQP map has an odd number of elements".to_owned());
                    }
                    let mut pairs = Vec::with_capacity(count / 2);
                    let mut items = items.into_iter();
                    while let (Some(key), Some(value)) = (items.next(), items.next()) {
                        pairs.push((key, value));
                    }
                    Value::Map(pairs)
                } else {
                    Value::List(items)
                }
            }
            0xe0 | 0xf0 => {
                let wide = constructor == 0xf0;
                let size = self.size(wide)?;
                let body = self.take(size)?;
                let mut inner = Reader { bytes: body, at: 0 };
                let count = inner.size(wide)?;
                if count > body.len() {
                    return Err("an AMQP array claims more elements than it holds".to_owned());
                }
                let element = inner.byte()?;
                if element == 0x00 {
                    return Err("described AMQP arrays are not supported".to_owned());
                }
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    items.push(inner.with_constructor(element, depth + 1)?);
                }
                Value::Array(items)
            }
            other => match fixed_width(other) {
                Some(width) => Value::Fixed(other, self.take(width)?.to_vec()),
                None => return Err(format!("0x{other:02x} is not an AMQP type constructor")),
            },
        })
    }
}

/// Decodes every value in `bytes`, one after another.
pub(crate) fn decode_all(bytes: &[u8]) -> Result<Vec<Value>, String> {
    let mut reader = Reader { bytes, at: 0 };
    let mut values = Vec::new();
    while reader.at < bytes.len() {
        values.push(reader.value(0)?);
    }
    Ok(values)
}

/// Decodes the one value at the start of `bytes` and returns how many bytes it used.
pub(crate) fn decode_first(bytes: &[u8]) -> Result<(Value, usize), String> {
    let mut reader = Reader { bytes, at: 0 };
    let value = reader.value(0)?;
    Ok((value, reader.at))
}

fn put_sized(out: &mut Vec<u8>, narrow: u8, wide: u8, bytes: &[u8]) {
    match u8::try_from(bytes.len()) {
        Ok(size) => {
            out.push(narrow);
            out.push(size);
        }
        Err(_) => {
            out.push(wide);
            out.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_be_bytes());
        }
    }
    out.extend_from_slice(bytes);
}

fn compound(out: &mut Vec<u8>, constructor: u8, count: usize, body: &[u8]) {
    out.push(constructor);
    let count = u32::try_from(count).unwrap_or(u32::MAX);
    out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(&count.to_be_bytes());
    out.extend_from_slice(body);
}

fn element_body(value: &Value, out: &mut Vec<u8>) -> Option<u8> {
    match value {
        Value::Symbol(text) => {
            out.extend_from_slice(&u32::try_from(text.len()).unwrap_or(u32::MAX).to_be_bytes());
            out.extend_from_slice(text.as_bytes());
            Some(0xb3)
        }
        Value::String(text) => {
            out.extend_from_slice(&u32::try_from(text.len()).unwrap_or(u32::MAX).to_be_bytes());
            out.extend_from_slice(text.as_bytes());
            Some(0xb1)
        }
        Value::Uint(number) => {
            out.extend_from_slice(&number.to_be_bytes());
            Some(0x70)
        }
        Value::Ulong(number) => {
            out.extend_from_slice(&number.to_be_bytes());
            Some(0x80)
        }
        _ => None,
    }
}

/// Appends the encoding of `value` to `out`.
pub(crate) fn encode(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.push(0x40),
        Value::Bool(true) => out.push(0x41),
        Value::Bool(false) => out.push(0x42),
        Value::Ubyte(number) => out.extend_from_slice(&[0x50, *number]),
        Value::Ushort(number) => {
            out.push(0x60);
            out.extend_from_slice(&number.to_be_bytes());
        }
        Value::Uint(number) => {
            out.push(0x70);
            out.extend_from_slice(&number.to_be_bytes());
        }
        Value::Ulong(number) => {
            out.push(0x80);
            out.extend_from_slice(&number.to_be_bytes());
        }
        Value::Signed(number) => {
            out.push(0x81);
            out.extend_from_slice(&number.to_be_bytes());
        }
        Value::Fixed(constructor, bytes) => {
            out.push(*constructor);
            out.extend_from_slice(bytes);
        }
        Value::Binary(bytes) => put_sized(out, 0xa0, 0xb0, bytes),
        Value::String(text) => put_sized(out, 0xa1, 0xb1, text.as_bytes()),
        Value::Symbol(text) => put_sized(out, 0xa3, 0xb3, text.as_bytes()),
        Value::List(items) => {
            let mut body = Vec::new();
            for item in items {
                encode(item, &mut body);
            }
            compound(out, 0xd0, items.len(), &body);
        }
        Value::Map(pairs) => {
            let mut body = Vec::new();
            for (key, value) in pairs {
                encode(key, &mut body);
                encode(value, &mut body);
            }
            compound(out, 0xd1, pairs.len() * 2, &body);
        }
        Value::Array(items) => {
            let mut body = Vec::new();
            let mut element = None;
            for item in items {
                element = element_body(item, &mut body).or(element);
            }
            let mut with_constructor = vec![element.unwrap_or(0x40)];
            with_constructor.extend_from_slice(&body);
            compound(out, 0xf0, items.len(), &with_constructor);
        }
        Value::Described(descriptor, inner) => {
            out.push(0x00);
            encode(descriptor, out);
            encode(inner, out);
        }
    }
}
