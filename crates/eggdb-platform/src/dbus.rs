//! D-Bus wire format, from the public D-Bus Specification, compiled and tested on every
//! operating system: typed values and their signatures, marshalling in either byte order,
//! messages with their header fields, the length of a message from its first 16 bytes, the
//! `EXTERNAL` authentication lines, and bus addresses. It is the base of the AT-SPI2
//! accessibility bridge on Linux and the BSDs (D256); the socket that carries it is the
//! caller's. Every reader returns errors as values: whatever a peer sends is untrusted.

use std::fmt;

/// The largest message the specification allows (128 MiB).
pub const MAX_MESSAGE_LENGTH: usize = 1 << 27;
/// The largest array the specification allows (64 MiB).
pub const MAX_ARRAY_LENGTH: usize = 1 << 26;
/// How deeply containers may nest in one value.
pub const MAX_DEPTH: usize = 64;
/// The line that ends authentication and starts the message stream.
pub const SASL_BEGIN: &[u8] = b"BEGIN\r\n";
/// The message bus's own name, path and interface.
pub const BUS_NAME: &str = "org.freedesktop.DBus";
/// The message bus's object path.
pub const BUS_PATH: &str = "/org/freedesktop/DBus";
/// The flag asking the receiver not to reply.
pub const NO_REPLY_EXPECTED: u8 = 0x1;

/// A message that does not follow the wire format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbusError(pub String);

impl fmt::Display for DbusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "D-Bus: {}", self.0)
    }
}

impl std::error::Error for DbusError {}

fn malformed<T>(what: impl Into<String>) -> Result<T, DbusError> {
    Err(DbusError(what.into()))
}

/// A typed D-Bus value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `y`.
    Byte(u8),
    /// `b`.
    Boolean(bool),
    /// `n`.
    Int16(i16),
    /// `q`.
    UInt16(u16),
    /// `i`.
    Int32(i32),
    /// `u`.
    UInt32(u32),
    /// `x`.
    Int64(i64),
    /// `t`.
    UInt64(u64),
    /// `d`.
    Double(f64),
    /// `s`.
    String(String),
    /// `o`.
    ObjectPath(String),
    /// `g`.
    Signature(String),
    /// `a` followed by the elements' signature, which an empty array still needs.
    Array(String, Vec<Value>),
    /// `(...)`.
    Struct(Vec<Value>),
    /// `{kv}`, only inside an array.
    DictEntry(Box<Value>, Box<Value>),
    /// `v`.
    Variant(Box<Value>),
}

impl Value {
    /// The value's signature.
    pub fn signature(&self) -> String {
        match self {
            Self::Byte(_) => "y".to_owned(),
            Self::Boolean(_) => "b".to_owned(),
            Self::Int16(_) => "n".to_owned(),
            Self::UInt16(_) => "q".to_owned(),
            Self::Int32(_) => "i".to_owned(),
            Self::UInt32(_) => "u".to_owned(),
            Self::Int64(_) => "x".to_owned(),
            Self::UInt64(_) => "t".to_owned(),
            Self::Double(_) => "d".to_owned(),
            Self::String(_) => "s".to_owned(),
            Self::ObjectPath(_) => "o".to_owned(),
            Self::Signature(_) => "g".to_owned(),
            Self::Array(element, _) => format!("a{element}"),
            Self::Struct(fields) => {
                let inner: String = fields.iter().map(Value::signature).collect();
                format!("({inner})")
            }
            Self::DictEntry(key, value) => format!("{{{}{}}}", key.signature(), value.signature()),
            Self::Variant(_) => "v".to_owned(),
        }
    }

    /// The string inside a `s`, `o` or `g` value.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) | Self::ObjectPath(text) | Self::Signature(text) => Some(text),
            _ => None,
        }
    }

    /// The number inside a `u` value.
    pub fn as_u32(&self) -> Option<u32> {
        match self {
            Self::UInt32(number) => Some(*number),
            _ => None,
        }
    }
}

/// The signature of a sequence of values, as a message body's `SIGNATURE` field holds it.
pub fn signature_of(values: &[Value]) -> String {
    values.iter().map(Value::signature).collect()
}

fn alignment(code: u8) -> usize {
    match code {
        b'n' | b'q' => 2,
        b'b' | b'i' | b'u' | b's' | b'o' | b'a' | b'h' => 4,
        b'x' | b't' | b'd' | b'(' | b'{' => 8,
        _ => 1,
    }
}

fn complete_type_length(signature: &[u8], depth: usize) -> Result<usize, DbusError> {
    if depth > MAX_DEPTH {
        return malformed("a signature nests too deeply");
    }
    let Some(&code) = signature.first() else {
        return malformed("a signature ends inside a type");
    };
    match code {
        b'y' | b'b' | b'n' | b'q' | b'i' | b'u' | b'x' | b't' | b'd' | b's' | b'o' | b'g'
        | b'v' => Ok(1),
        b'a' => {
            let rest = signature.get(1..).unwrap_or_default();
            if rest.first() == Some(&b'{') {
                let entry = rest;
                let key = entry.get(1).copied();
                if !matches!(
                    key,
                    Some(
                        b'y' | b'b'
                            | b'n'
                            | b'q'
                            | b'i'
                            | b'u'
                            | b'x'
                            | b't'
                            | b'd'
                            | b's'
                            | b'o'
                            | b'g'
                    )
                ) {
                    return malformed("a dictionary key is not a basic type");
                }
                let value_length =
                    complete_type_length(entry.get(2..).unwrap_or_default(), depth + 1)?;
                if entry.get(2 + value_length) != Some(&b'}') {
                    return malformed("a dictionary entry has more than a key and a value");
                }
                Ok(1 + 3 + value_length)
            } else {
                Ok(1 + complete_type_length(rest, depth + 1)?)
            }
        }
        b'(' => {
            let mut length = 1;
            loop {
                match signature.get(length) {
                    None => return malformed("a struct's signature is not closed"),
                    Some(b')') if length == 1 => return malformed("a struct is empty"),
                    Some(b')') => return Ok(length + 1),
                    Some(_) => {
                        length += complete_type_length(
                            signature.get(length..).unwrap_or_default(),
                            depth + 1,
                        )?;
                    }
                }
            }
        }
        other => malformed(format!(
            "a signature has the unknown type code {other:#04x}"
        )),
    }
}

/// Splits a signature into its complete types, or says why it is not a valid signature.
pub fn split_signature(signature: &str) -> Result<Vec<&str>, DbusError> {
    if signature.len() > 255 {
        return malformed("a signature is longer than 255 bytes");
    }
    let bytes = signature.as_bytes();
    let mut types = Vec::new();
    let mut start = 0;
    while start < bytes.len() {
        let length = complete_type_length(bytes.get(start..).unwrap_or_default(), 0)?;
        types.push(signature.get(start..start + length).unwrap_or_default());
        start += length;
    }
    Ok(types)
}

/// Marshals values in little-endian order into a buffer that starts 8-aligned.
#[derive(Debug, Default)]
pub struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    /// The bytes written so far.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    fn pad(&mut self, to: usize) {
        while !self.bytes.len().is_multiple_of(to) {
            self.bytes.push(0);
        }
    }

    fn u32(&mut self, number: u32) {
        self.pad(4);
        self.bytes.extend_from_slice(&number.to_le_bytes());
    }

    fn text(&mut self, text: &str) {
        self.u32(text.len() as u32);
        self.bytes.extend_from_slice(text.as_bytes());
        self.bytes.push(0);
    }

    /// Appends one value.
    pub fn write(&mut self, value: &Value) {
        match value {
            Value::Byte(byte) => self.bytes.push(*byte),
            Value::Boolean(flag) => self.u32(u32::from(*flag)),
            Value::Int16(number) => {
                self.pad(2);
                self.bytes.extend_from_slice(&number.to_le_bytes());
            }
            Value::UInt16(number) => {
                self.pad(2);
                self.bytes.extend_from_slice(&number.to_le_bytes());
            }
            Value::Int32(number) => {
                self.pad(4);
                self.bytes.extend_from_slice(&number.to_le_bytes());
            }
            Value::UInt32(number) => self.u32(*number),
            Value::Int64(number) => {
                self.pad(8);
                self.bytes.extend_from_slice(&number.to_le_bytes());
            }
            Value::UInt64(number) => {
                self.pad(8);
                self.bytes.extend_from_slice(&number.to_le_bytes());
            }
            Value::Double(number) => {
                self.pad(8);
                self.bytes.extend_from_slice(&number.to_le_bytes());
            }
            Value::String(text) | Value::ObjectPath(text) => self.text(text),
            Value::Signature(text) => {
                self.bytes.push(text.len() as u8);
                self.bytes.extend_from_slice(text.as_bytes());
                self.bytes.push(0);
            }
            Value::Array(element, values) => {
                self.pad(4);
                let length_at = self.bytes.len();
                self.bytes.extend_from_slice(&[0; 4]);
                self.pad(alignment(
                    element.as_bytes().first().copied().unwrap_or(b'y'),
                ));
                let start = self.bytes.len();
                for value in values {
                    self.write(value);
                }
                let length = (self.bytes.len() - start) as u32;
                if let Some(slot) = self.bytes.get_mut(length_at..length_at + 4) {
                    slot.copy_from_slice(&length.to_le_bytes());
                }
            }
            Value::Struct(fields) => {
                self.pad(8);
                for field in fields {
                    self.write(field);
                }
            }
            Value::DictEntry(key, value) => {
                self.pad(8);
                self.write(key);
                self.write(value);
            }
            Value::Variant(inner) => {
                self.write(&Value::Signature(inner.signature()));
                self.write(inner);
            }
        }
    }
}

/// Unmarshals values from a buffer that starts 8-aligned, in the byte order its message
/// declared.
#[derive(Debug)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
    big_endian: bool,
}

impl<'a> Reader<'a> {
    /// A reader at the start of `bytes`.
    pub fn new(bytes: &'a [u8], big_endian: bool) -> Self {
        Self {
            bytes,
            position: 0,
            big_endian,
        }
    }

    /// How far the reader has read.
    pub fn position(&self) -> usize {
        self.position
    }

    fn align(&mut self, to: usize) -> Result<(), DbusError> {
        let aligned = self.position.next_multiple_of(to);
        let padding = self
            .bytes
            .get(self.position..aligned)
            .ok_or_else(|| DbusError("the data ends inside padding".to_owned()))?;
        if padding.iter().any(|&byte| byte != 0) {
            return malformed("padding is not zero");
        }
        self.position = aligned;
        Ok(())
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], DbusError> {
        self.align(N)?;
        let end = self.position + N;
        let bytes: [u8; N] = self
            .bytes
            .get(self.position..end)
            .and_then(|slice| slice.try_into().ok())
            .ok_or_else(|| DbusError("the data ends inside a value".to_owned()))?;
        self.position = end;
        Ok(if self.big_endian {
            let mut swapped = bytes;
            swapped.reverse();
            swapped
        } else {
            bytes
        })
    }

    fn u32(&mut self) -> Result<u32, DbusError> {
        self.take::<4>().map(u32::from_le_bytes)
    }

    fn text(&mut self, length: usize) -> Result<String, DbusError> {
        let end = self.position.saturating_add(length);
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| DbusError("the data ends inside a string".to_owned()))?;
        if self.bytes.get(end) != Some(&0) {
            return malformed("a string is not NUL-terminated");
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| DbusError("a string is not UTF-8".to_owned()))?;
        if text.contains('\0') {
            return malformed("a string holds a NUL");
        }
        self.position = end + 1;
        Ok(text.to_owned())
    }

    fn signature(&mut self) -> Result<String, DbusError> {
        let length = *self
            .bytes
            .get(self.position)
            .ok_or_else(|| DbusError("the data ends before a signature".to_owned()))?;
        self.position += 1;
        let text = self.text(usize::from(length))?;
        split_signature(&text)?;
        Ok(text)
    }

    /// Reads the values of a signature of any number of complete types.
    pub fn read_all(&mut self, signature: &str) -> Result<Vec<Value>, DbusError> {
        split_signature(signature)?
            .into_iter()
            .map(|single| self.read(single.as_bytes(), 0))
            .collect()
    }

    fn read(&mut self, signature: &[u8], depth: usize) -> Result<Value, DbusError> {
        if depth > MAX_DEPTH {
            return malformed("a value nests too deeply");
        }
        let Some(&code) = signature.first() else {
            return malformed("an empty signature has no value");
        };
        Ok(match code {
            b'y' => Value::Byte(self.take::<1>()?[0]),
            b'b' => match self.u32()? {
                0 => Value::Boolean(false),
                1 => Value::Boolean(true),
                _ => return malformed("a boolean is neither 0 nor 1"),
            },
            b'n' => Value::Int16(self.take::<2>().map(i16::from_le_bytes)?),
            b'q' => Value::UInt16(self.take::<2>().map(u16::from_le_bytes)?),
            b'i' => Value::Int32(self.take::<4>().map(i32::from_le_bytes)?),
            b'u' => Value::UInt32(self.u32()?),
            b'x' => Value::Int64(self.take::<8>().map(i64::from_le_bytes)?),
            b't' => Value::UInt64(self.take::<8>().map(u64::from_le_bytes)?),
            b'd' => Value::Double(self.take::<8>().map(f64::from_le_bytes)?),
            b's' | b'o' => {
                let length = self.u32()? as usize;
                let text = self.text(length)?;
                if code == b'o' {
                    Value::ObjectPath(text)
                } else {
                    Value::String(text)
                }
            }
            b'g' => Value::Signature(self.signature()?),
            b'v' => {
                let inner = self.signature()?;
                if split_signature(&inner)?.len() != 1 {
                    return malformed("a variant does not hold exactly one type");
                }
                Value::Variant(Box::new(self.read(inner.as_bytes(), depth + 1)?))
            }
            b'a' => self.array(signature, depth)?,
            b'(' => {
                self.align(8)?;
                let inner = signature
                    .get(1..signature.len().saturating_sub(1))
                    .unwrap_or_default();
                let mut fields = Vec::new();
                let mut start = 0;
                while start < inner.len() {
                    let field = inner.get(start..).unwrap_or_default();
                    let length = complete_type_length(field, depth + 1)?;
                    fields.push(self.read(field.get(..length).unwrap_or_default(), depth + 1)?);
                    start += length;
                }
                Value::Struct(fields)
            }
            b'{' => {
                self.align(8)?;
                let key = self.read(signature.get(1..2).unwrap_or_default(), depth + 1)?;
                let value_signature = signature
                    .get(2..signature.len().saturating_sub(1))
                    .unwrap_or_default();
                let value = self.read(value_signature, depth + 1)?;
                Value::DictEntry(Box::new(key), Box::new(value))
            }
            other => return malformed(format!("cannot read the type code {other:#04x}")),
        })
    }

    fn array(&mut self, signature: &[u8], depth: usize) -> Result<Value, DbusError> {
        let length = self.u32()? as usize;
        if length > MAX_ARRAY_LENGTH {
            return malformed("an array is longer than 64 MiB");
        }
        let element = signature.get(1..).unwrap_or_default();
        self.align(alignment(element.first().copied().unwrap_or(b'y')))?;
        let end = self.position.saturating_add(length);
        if end > self.bytes.len() {
            return malformed("the data ends inside an array");
        }
        let mut values = Vec::new();
        while self.position < end {
            values.push(self.read(element, depth + 1)?);
        }
        if self.position != end {
            return malformed("an array's elements overrun its length");
        }
        let element = std::str::from_utf8(element)
            .map_err(|_| DbusError("a signature is not ASCII".to_owned()))?;
        Ok(Value::Array(element.to_owned(), values))
    }
}

/// What a message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    /// A call of a method on an object.
    MethodCall,
    /// A method's result.
    MethodReturn,
    /// A method's error.
    Error,
    /// A signal emitted by an object.
    Signal,
}

impl MessageKind {
    fn code(self) -> u8 {
        match self {
            Self::MethodCall => 1,
            Self::MethodReturn => 2,
            Self::Error => 3,
            Self::Signal => 4,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::MethodCall,
            2 => Self::MethodReturn,
            3 => Self::Error,
            4 => Self::Signal,
            _ => return None,
        })
    }
}

/// One D-Bus message: its kind, flags, serial, header fields and body.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// What the message is.
    pub kind: MessageKind,
    /// Its flags, such as [`NO_REPLY_EXPECTED`].
    pub flags: u8,
    /// The sender's serial for it, never 0.
    pub serial: u32,
    /// The object it is for or from.
    pub path: Option<String>,
    /// The interface of the method or signal.
    pub interface: Option<String>,
    /// The method's or signal's name.
    pub member: Option<String>,
    /// The error's name.
    pub error_name: Option<String>,
    /// The serial of the call this replies to.
    pub reply_serial: Option<u32>,
    /// The name it is sent to.
    pub destination: Option<String>,
    /// The unique name of its sender, filled in by the bus.
    pub sender: Option<String>,
    /// The body's values.
    pub body: Vec<Value>,
}

impl Message {
    fn empty(kind: MessageKind, serial: u32) -> Self {
        Self {
            kind,
            flags: 0,
            serial,
            path: None,
            interface: None,
            member: None,
            error_name: None,
            reply_serial: None,
            destination: None,
            sender: None,
            body: Vec::new(),
        }
    }

    /// A call of `interface.member` on `path` at `destination`.
    pub fn method_call(
        serial: u32,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        body: Vec<Value>,
    ) -> Self {
        Self {
            destination: Some(destination.to_owned()),
            path: Some(path.to_owned()),
            interface: Some(interface.to_owned()),
            member: Some(member.to_owned()),
            body,
            ..Self::empty(MessageKind::MethodCall, serial)
        }
    }

    /// The `Hello` call every connection makes first, which returns its unique name.
    pub fn hello(serial: u32) -> Self {
        Self::method_call(serial, BUS_NAME, BUS_PATH, BUS_NAME, "Hello", Vec::new())
    }

    /// The successful reply to `call`.
    pub fn method_return(serial: u32, call: &Message, body: Vec<Value>) -> Self {
        Self {
            reply_serial: Some(call.serial),
            destination: call.sender.clone(),
            body,
            ..Self::empty(MessageKind::MethodReturn, serial)
        }
    }

    /// The error reply to `call`, with a message for people.
    pub fn error(serial: u32, call: &Message, name: &str, text: &str) -> Self {
        Self {
            error_name: Some(name.to_owned()),
            reply_serial: Some(call.serial),
            destination: call.sender.clone(),
            body: vec![Value::String(text.to_owned())],
            ..Self::empty(MessageKind::Error, serial)
        }
    }

    /// A signal `interface.member` emitted by `path`.
    pub fn signal(
        serial: u32,
        path: &str,
        interface: &str,
        member: &str,
        body: Vec<Value>,
    ) -> Self {
        Self {
            path: Some(path.to_owned()),
            interface: Some(interface.to_owned()),
            member: Some(member.to_owned()),
            body,
            ..Self::empty(MessageKind::Signal, serial)
        }
    }

    fn header_fields(&self) -> Vec<Value> {
        let text = |code: u8, value: &Option<String>, make: fn(String) -> Value| {
            value.as_ref().map(|text| {
                Value::Struct(vec![
                    Value::Byte(code),
                    Value::Variant(Box::new(make(text.clone()))),
                ])
            })
        };
        let signature = signature_of(&self.body);
        [
            text(1, &self.path, Value::ObjectPath),
            text(2, &self.interface, Value::String),
            text(3, &self.member, Value::String),
            text(4, &self.error_name, Value::String),
            self.reply_serial.map(|serial| {
                Value::Struct(vec![
                    Value::Byte(5),
                    Value::Variant(Box::new(Value::UInt32(serial))),
                ])
            }),
            text(6, &self.destination, Value::String),
            text(7, &self.sender, Value::String),
            (!signature.is_empty()).then(|| {
                Value::Struct(vec![
                    Value::Byte(8),
                    Value::Variant(Box::new(Value::Signature(signature))),
                ])
            }),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// The message in little-endian wire format.
    pub fn encode(&self) -> Result<Vec<u8>, DbusError> {
        split_signature(&signature_of(&self.body))?;
        let mut body = Writer::default();
        for value in &self.body {
            body.write(value);
        }
        let body = body.into_bytes();
        let body_length = u32::try_from(body.len())
            .map_err(|_| DbusError("a message body is longer than 4 GiB".to_owned()))?;
        let mut message = Writer::default();
        message
            .bytes
            .extend_from_slice(&[b'l', self.kind.code(), self.flags, 1]);
        message.u32(body_length);
        message.u32(self.serial);
        message.write(&Value::Array("(yv)".to_owned(), self.header_fields()));
        message.pad(8);
        let mut bytes = message.into_bytes();
        bytes.extend_from_slice(&body);
        if bytes.len() > MAX_MESSAGE_LENGTH {
            return malformed("a message is longer than 128 MiB");
        }
        Ok(bytes)
    }

    /// Reads one whole message, as [`message_length`] measured it.
    pub fn decode(bytes: &[u8]) -> Result<Self, DbusError> {
        let big_endian = match bytes.first() {
            Some(b'l') => false,
            Some(b'B') => true,
            _ => return malformed("a message does not start with its byte order"),
        };
        let mut reader = Reader::new(bytes, big_endian);
        let [_, code, flags, version] = reader.take::<4>().map(|mut fixed| {
            if big_endian {
                fixed.reverse();
            }
            fixed
        })?;
        if version != 1 {
            return malformed(format!("protocol version {version} is not 1"));
        }
        let kind = MessageKind::from_code(code)
            .ok_or_else(|| DbusError(format!("message type {code} is unknown")))?;
        let body_length = reader.u32()? as usize;
        let serial = reader.u32()?;
        if serial == 0 {
            return malformed("a message's serial is 0");
        }
        let mut message = Self {
            flags,
            ..Self::empty(kind, serial)
        };
        let fields = reader.read(b"a(yv)", 0)?;
        let mut signature = String::new();
        if let Value::Array(_, fields) = fields {
            for field in fields {
                message.read_header_field(field, &mut signature)?;
            }
        }
        reader.align(8)?;
        let start = reader.position();
        let body = bytes
            .get(start..start.saturating_add(body_length))
            .ok_or_else(|| DbusError("the data ends inside a message body".to_owned()))?;
        if start + body_length != bytes.len() {
            return malformed("bytes follow the message body");
        }
        let mut body_reader = Reader::new(body, big_endian);
        message.body = body_reader.read_all(&signature)?;
        if body_reader.position() != body.len() {
            return malformed("the body is longer than its signature");
        }
        message.check_required_fields()?;
        Ok(message)
    }

    fn read_header_field(&mut self, field: Value, signature: &mut String) -> Result<(), DbusError> {
        let Value::Struct(parts) = field else {
            return malformed("a header field is not a struct");
        };
        let (Some(Value::Byte(code)), Some(Value::Variant(value))) = (parts.first(), parts.get(1))
        else {
            return malformed("a header field is not a code and a variant");
        };
        let text = || value.as_str().map(str::to_owned);
        let wrong = || DbusError(format!("header field {code} has the wrong type"));
        match (code, value.as_ref()) {
            (1, Value::ObjectPath(path)) => self.path = Some(path.clone()),
            (2, Value::String(_)) => self.interface = text(),
            (3, Value::String(_)) => self.member = text(),
            (4, Value::String(_)) => self.error_name = text(),
            (5, Value::UInt32(serial)) => self.reply_serial = Some(*serial),
            (6, Value::String(_)) => self.destination = text(),
            (7, Value::String(_)) => self.sender = text(),
            (8, Value::Signature(body)) => *signature = body.clone(),
            (1..=8, _) => return Err(wrong()),
            _ => {}
        }
        Ok(())
    }

    fn check_required_fields(&self) -> Result<(), DbusError> {
        let missing = match self.kind {
            MessageKind::MethodCall => self.path.is_none() || self.member.is_none(),
            MessageKind::Signal => {
                self.path.is_none() || self.interface.is_none() || self.member.is_none()
            }
            MessageKind::Error => self.error_name.is_none() || self.reply_serial.is_none(),
            MessageKind::MethodReturn => self.reply_serial.is_none(),
        };
        if missing {
            return malformed(format!("a {:?} lacks a required header field", self.kind));
        }
        Ok(())
    }
}

/// The whole length of the message whose first 16 bytes are `prefix`, so a reader knows how
/// much more to read; `None` until 16 bytes are there.
pub fn message_length(prefix: &[u8]) -> Result<Option<usize>, DbusError> {
    let Some(fixed) = prefix.get(..16) else {
        return Ok(None);
    };
    let big_endian = match fixed.first() {
        Some(b'l') => false,
        Some(b'B') => true,
        _ => return malformed("a message does not start with its byte order"),
    };
    let number = |at: usize| -> usize {
        let bytes: [u8; 4] = fixed
            .get(at..at + 4)
            .and_then(|slice| slice.try_into().ok())
            .unwrap_or_default();
        (if big_endian {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        }) as usize
    };
    let (body, fields) = (number(4), number(12));
    let length = fields
        .saturating_add(16)
        .next_multiple_of(8)
        .saturating_add(body);
    if length > MAX_MESSAGE_LENGTH {
        return malformed("a message is longer than 128 MiB");
    }
    Ok(Some(length))
}

/// The first bytes a client sends: the NUL byte and the `AUTH EXTERNAL` line for user `uid`.
pub fn sasl_external(uid: u32) -> Vec<u8> {
    let hex: String = uid
        .to_string()
        .bytes()
        .map(|digit| format!("{digit:02x}"))
        .collect();
    format!("\0AUTH EXTERNAL {hex}\r\n").into_bytes()
}

/// The server's answer to the `AUTH` line: its GUID when it accepted, an error otherwise.
pub fn sasl_accepted(line: &str) -> Result<String, DbusError> {
    let line = line.trim_end_matches(['\r', '\n']);
    match line.split_once(' ') {
        Some(("OK", guid)) if !guid.is_empty() => Ok(guid.to_owned()),
        _ => malformed(format!("the bus refused authentication: {line}")),
    }
}

/// Where a bus listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusAddress {
    /// A Unix socket at a file system path.
    UnixPath(String),
    /// A Unix socket in Linux's abstract namespace.
    UnixAbstract(String),
}

fn unescape(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut rest = value.bytes();
    while let Some(byte) = rest.next() {
        if byte == b'%' {
            let high = char::from(rest.next()?).to_digit(16)?;
            let low = char::from(rest.next()?).to_digit(16)?;
            bytes.push((high * 16 + low) as u8);
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).ok()
}

/// The Unix-socket addresses in a D-Bus address string (`unix:path=...;unix:abstract=...`),
/// in order; other transports and malformed entries are skipped.
pub fn parse_addresses(text: &str) -> Vec<BusAddress> {
    text.split(';')
        .filter_map(|entry| {
            let options = entry.strip_prefix("unix:")?;
            options.split(',').find_map(|option| {
                let (key, value) = option.split_once('=')?;
                let value = unescape(value)?;
                match key {
                    "path" => Some(BusAddress::UnixPath(value)),
                    "abstract" => Some(BusAddress::UnixAbstract(value)),
                    _ => None,
                }
            })
        })
        .collect()
}

/// The session bus's addresses from `DBUS_SESSION_BUS_ADDRESS`, or the socket `bus` in
/// `XDG_RUNTIME_DIR` when it is not set.
pub fn session_bus_addresses(address: Option<&str>, runtime_dir: Option<&str>) -> Vec<BusAddress> {
    match address {
        Some(address) => parse_addresses(address),
        None => runtime_dir
            .map(|dir| {
                vec![BusAddress::UnixPath(format!(
                    "{}/bus",
                    dir.trim_end_matches('/')
                ))]
            })
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(values: &[Value]) -> Vec<Value> {
        let mut writer = Writer::default();
        for value in values {
            writer.write(value);
        }
        let bytes = writer.into_bytes();
        let mut reader = Reader::new(&bytes, false);
        let read = reader.read_all(&signature_of(values)).unwrap();
        assert_eq!(reader.position(), bytes.len());
        read
    }

    #[test]
    fn every_type_round_trips_with_its_alignment() {
        let values = vec![
            Value::Byte(7),
            Value::Boolean(true),
            Value::Int16(-2),
            Value::UInt16(3),
            Value::Int32(-4),
            Value::UInt32(5),
            Value::Int64(-6),
            Value::UInt64(7),
            Value::Double(1.5),
            Value::String("Open a database".to_owned()),
            Value::ObjectPath("/org/a11y/atspi/accessible/1".to_owned()),
            Value::Signature("a(so)".to_owned()),
            Value::Array("u".to_owned(), vec![Value::UInt32(1), Value::UInt32(2)]),
            Value::Array("(so)".to_owned(), Vec::new()),
            Value::Array(
                "{ss}".to_owned(),
                vec![Value::DictEntry(
                    Box::new(Value::String("toolkit".to_owned())),
                    Box::new(Value::String("eggdb".to_owned())),
                )],
            ),
            Value::Struct(vec![
                Value::Int32(1),
                Value::Int32(2),
                Value::Int32(3),
                Value::Int32(4),
            ]),
            Value::Variant(Box::new(Value::Struct(vec![
                Value::String(":1.5".to_owned()),
                Value::ObjectPath("/".to_owned()),
            ]))),
        ];
        assert_eq!(round_trip(&values), values);
        assert_eq!(signature_of(&values), "ybnqiuxtdsogauaa(so)a{ss}(iiii)v");
    }

    #[test]
    fn padding_follows_the_specification() {
        let mut writer = Writer::default();
        writer.write(&Value::Byte(1));
        writer.write(&Value::UInt32(0x0102_0304));
        writer.write(&Value::Array("t".to_owned(), Vec::new()));
        assert_eq!(
            writer.into_bytes(),
            [1, 0, 0, 0, 4, 3, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn signatures_are_split_and_bad_ones_refused() {
        assert_eq!(
            split_signature("ua(so)a{sv}v").unwrap(),
            ["u", "a(so)", "a{sv}", "v"]
        );
        for bad in ["(", "()", "a", "a{vs}", "a{sss}", "z", "{ss}x", ")"] {
            assert!(split_signature(bad).is_err(), "{bad}");
        }
        assert!(split_signature(&format!("{}y", "a".repeat(80))).is_err());
        assert!(split_signature(&"x".repeat(256)).is_err());
    }

    #[test]
    fn a_message_encodes_and_decodes_with_its_header_fields() {
        let call = Message::method_call(
            7,
            "org.a11y.atspi.Registry",
            "/org/a11y/atspi/accessible/root",
            "org.a11y.atspi.Socket",
            "Embed",
            vec![Value::Struct(vec![
                Value::String(":1.42".to_owned()),
                Value::ObjectPath("/org/a11y/atspi/accessible/root".to_owned()),
            ])],
        );
        let bytes = call.encode().unwrap();
        assert_eq!(message_length(&bytes).unwrap(), Some(bytes.len()));
        assert_eq!(message_length(&bytes[..15]).unwrap(), None);
        assert_eq!(Message::decode(&bytes).unwrap(), call);
        let mut from_bus = call.clone();
        from_bus.sender = Some(":1.7".to_owned());
        let reply = Message::method_return(8, &from_bus, vec![Value::UInt32(1)]);
        assert_eq!(reply.destination.as_deref(), Some(":1.7"));
        assert_eq!(Message::decode(&reply.encode().unwrap()).unwrap(), reply);
        let error = Message::error(
            9,
            &from_bus,
            "org.freedesktop.DBus.Error.UnknownMethod",
            "no",
        );
        assert_eq!(Message::decode(&error.encode().unwrap()).unwrap(), error);
        let hello = Message::hello(1);
        assert_eq!(Message::decode(&hello.encode().unwrap()).unwrap(), hello);
    }

    #[test]
    fn a_big_endian_message_is_read() {
        let bytes: Vec<u8> = vec![
            b'B', 4, 0, 1, 0, 0, 0, 4, 0, 0, 0, 2, 0, 0, 0, 0x37, 1, 1, b'o', 0, 0, 0, 0, 2, b'/',
            b'a', 0, 0, 0, 0, 0, 0, 2, 1, b's', 0, 0, 0, 0, 1, b'i', 0, 0, 0, 0, 0, 0, 0, 3, 1,
            b's', 0, 0, 0, 0, 1, b'm', 0, 0, 0, 0, 0, 0, 0, 8, 1, b'g', 0, 1, b'u', 0, 0, 0, 0, 0,
            9,
        ];
        let message = Message::decode(&bytes).unwrap();
        assert_eq!(message.kind, MessageKind::Signal);
        assert_eq!(message.serial, 2);
        assert_eq!(message.path.as_deref(), Some("/a"));
        assert_eq!(message.member.as_deref(), Some("m"));
        assert_eq!(message.body, [Value::UInt32(9)]);
        assert_eq!(message_length(&bytes).unwrap(), Some(bytes.len()));
    }

    #[test]
    fn hostile_messages_are_errors_not_panics() {
        let good = Message::signal(3, "/a", "i.f", "M", vec![Value::String("x".to_owned())])
            .encode()
            .unwrap();
        for length in 0..good.len() {
            assert!(Message::decode(&good[..length]).is_err(), "cut at {length}");
        }
        for index in 0..good.len() {
            for flip in [0x01, 0x80, 0xff] {
                let mut bad = good.clone();
                bad[index] ^= flip;
                let _ = Message::decode(&bad);
                let _ = message_length(&bad);
            }
        }
        let mut huge = vec![b'l', 1, 0, 1];
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        huge.extend_from_slice(&1_u32.to_le_bytes());
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(message_length(&huge).is_err());
        assert!(Message::decode(&huge).is_err());
        assert!(Reader::new(&[0; 512], false).read_all("v").is_err());
        let nested_variants: Vec<u8> = std::iter::repeat_n([1_u8, b'v', 0], 100)
            .flatten()
            .collect();
        assert!(Reader::new(&nested_variants, false).read_all("v").is_err());
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(Message::decode(&trailing).is_err());
    }

    #[test]
    fn authentication_lines_follow_the_external_mechanism() {
        assert_eq!(sasl_external(1000), b"\0AUTH EXTERNAL 31303030\r\n");
        assert_eq!(
            sasl_accepted("OK 1234deadbeef\r\n").unwrap(),
            "1234deadbeef"
        );
        assert!(sasl_accepted("REJECTED EXTERNAL\r\n").is_err());
        assert!(sasl_accepted("OK\r\n").is_err());
    }

    #[test]
    fn addresses_are_parsed_with_escapes_and_a_runtime_dir_fallback() {
        assert_eq!(
            parse_addresses(
                "tcp:host=x,port=1;unix:path=/run/user/1000/bus;unix:abstract=/tmp/dbus-%41b,guid=12"
            ),
            [
                BusAddress::UnixPath("/run/user/1000/bus".to_owned()),
                BusAddress::UnixAbstract("/tmp/dbus-Ab".to_owned()),
            ]
        );
        assert!(parse_addresses("unix:path=%zz").is_empty());
        assert_eq!(
            session_bus_addresses(None, Some("/run/user/7/")),
            [BusAddress::UnixPath("/run/user/7/bus".to_owned())]
        );
        assert!(session_bus_addresses(None, None).is_empty());
    }
}
