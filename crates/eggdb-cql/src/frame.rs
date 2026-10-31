pub(crate) const VERSION: u8 = 4;
pub(crate) const MAX_BODY: usize = 16 << 20;

pub(crate) const ERROR: u8 = 0x00;
pub(crate) const STARTUP: u8 = 0x01;
pub(crate) const READY: u8 = 0x02;
pub(crate) const AUTHENTICATE: u8 = 0x03;
pub(crate) const OPTIONS: u8 = 0x05;
pub(crate) const SUPPORTED: u8 = 0x06;
pub(crate) const QUERY: u8 = 0x07;
pub(crate) const RESULT: u8 = 0x08;
pub(crate) const PREPARE: u8 = 0x09;
pub(crate) const EXECUTE: u8 = 0x0a;
pub(crate) const REGISTER: u8 = 0x0b;
pub(crate) const BATCH: u8 = 0x0d;
pub(crate) const AUTH_RESPONSE: u8 = 0x0f;
pub(crate) const AUTH_SUCCESS: u8 = 0x10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) version: u8,
    pub(crate) flags: u8,
    pub(crate) stream: i16,
    pub(crate) opcode: u8,
    pub(crate) body: Vec<u8>,
}

/// The next whole request at the start of `buffer`, removed from it, `None` when more bytes are
/// needed, or why the input is refused.
pub(crate) fn take_request(buffer: &mut Vec<u8>) -> Result<Option<Request>, String> {
    let Some(header) = buffer.get(..9) else {
        return Ok(None);
    };
    let length = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;
    if length > MAX_BODY {
        return Err(format!("a frame body of {length} bytes is larger than 16 MiB"));
    }
    if header[0] & 0x80 != 0 {
        return Err("a client sent a response frame".to_owned());
    }
    if buffer.len() < 9 + length {
        return Ok(None);
    }
    let request = Request {
        version: header[0],
        flags: header[1],
        stream: i16::from_be_bytes([header[2], header[3]]),
        opcode: header[4],
        body: buffer[9..9 + length].to_vec(),
    };
    buffer.drain(..9 + length);
    Ok(Some(request))
}

pub(crate) fn response(version: u8, stream: i16, opcode: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(9 + body.len());
    out.push(0x80 | version);
    out.push(0);
    out.extend_from_slice(&stream.to_be_bytes());
    out.push(opcode);
    out.extend_from_slice(&u32::try_from(body.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(body);
    out
}

pub(crate) struct Body<'b> {
    bytes: &'b [u8],
    at: usize,
}

const CUT_SHORT: &str = "a request body is cut short";

impl<'b> Body<'b> {
    pub(crate) fn new(bytes: &'b [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.at >= self.bytes.len()
    }

    fn take(&mut self, count: usize) -> Result<&'b [u8], String> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or_else(|| CUT_SHORT.to_owned())?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    pub(crate) fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn short(&mut self) -> Result<u16, String> {
        let raw = self.take(2)?;
        Ok(u16::from_be_bytes([raw[0], raw[1]]))
    }

    pub(crate) fn int(&mut self) -> Result<i32, String> {
        let raw = self.take(4)?;
        Ok(i32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    pub(crate) fn long(&mut self) -> Result<i64, String> {
        let raw = self.take(8)?;
        Ok(i64::from_be_bytes(raw.try_into().map_err(|_| CUT_SHORT.to_owned())?))
    }

    pub(crate) fn string(&mut self) -> Result<String, String> {
        let length = usize::from(self.short()?);
        String::from_utf8(self.take(length)?.to_vec()).map_err(|_| "a string is not UTF-8".to_owned())
    }

    pub(crate) fn long_string(&mut self) -> Result<String, String> {
        let length = usize::try_from(self.int()?).map_err(|_| "a long string has a negative length".to_owned())?;
        String::from_utf8(self.take(length)?.to_vec()).map_err(|_| "a long string is not UTF-8".to_owned())
    }

    pub(crate) fn bytes(&mut self) -> Result<Option<Vec<u8>>, String> {
        let length = self.int()?;
        if length < 0 {
            return Ok(None);
        }
        Ok(Some(self.take(usize::try_from(length).unwrap_or(usize::MAX))?.to_vec()))
    }

    pub(crate) fn short_bytes(&mut self) -> Result<Vec<u8>, String> {
        let length = usize::from(self.short()?);
        Ok(self.take(length)?.to_vec())
    }

    pub(crate) fn string_map(&mut self) -> Result<Vec<(String, String)>, String> {
        let count = self.short()?;
        (0..count).map(|_| Ok((self.string()?, self.string()?))).collect()
    }

    pub(crate) fn string_list(&mut self) -> Result<Vec<String>, String> {
        let count = self.short()?;
        (0..count).map(|_| self.string()).collect()
    }
}

#[derive(Default)]
pub(crate) struct Out {
    pub(crate) bytes: Vec<u8>,
}

impl Out {
    pub(crate) fn short(&mut self, value: u16) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub(crate) fn int(&mut self, value: i32) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub(crate) fn string(&mut self, value: &str) -> &mut Self {
        let length = u16::try_from(value.len()).unwrap_or(u16::MAX);
        self.short(length);
        self.bytes.extend_from_slice(&value.as_bytes()[..usize::from(length)]);
        self
    }

    pub(crate) fn bytes(&mut self, value: Option<&[u8]>) -> &mut Self {
        match value {
            Some(value) => {
                self.int(i32::try_from(value.len()).unwrap_or(i32::MAX));
                self.bytes.extend_from_slice(value);
            }
            None => {
                self.int(-1);
            }
        }
        self
    }

    pub(crate) fn short_bytes(&mut self, value: &[u8]) -> &mut Self {
        self.short(u16::try_from(value.len()).unwrap_or(u16::MAX));
        self.bytes.extend_from_slice(value);
        self
    }

    pub(crate) fn string_multimap(&mut self, entries: &[(&str, &[&str])]) -> &mut Self {
        self.short(u16::try_from(entries.len()).unwrap_or(u16::MAX));
        for (key, values) in entries {
            self.string(key);
            self.short(u16::try_from(values.len()).unwrap_or(u16::MAX));
            for value in *values {
                self.string(value);
            }
        }
        self
    }
}
