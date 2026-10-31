pub(crate) const FRAME_METHOD: u8 = 1;
pub(crate) const FRAME_HEADER: u8 = 2;
pub(crate) const FRAME_BODY: u8 = 3;
pub(crate) const FRAME_HEARTBEAT: u8 = 8;
pub(crate) const FRAME_END: u8 = 0xce;
pub(crate) const FRAME_MAX: u32 = 131_072;

/// One frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) kind: u8,
    pub(crate) channel: u16,
    pub(crate) payload: Vec<u8>,
}

/// Takes one whole frame from the front of `buffer`, or `None` when more bytes are needed; an
/// error when the frame is malformed.
pub(crate) fn take_frame(buffer: &mut Vec<u8>) -> Result<Option<Frame>, String> {
    if buffer.len() < 7 {
        return Ok(None);
    }
    let size = u32::from_be_bytes([buffer[3], buffer[4], buffer[5], buffer[6]]);
    if size > FRAME_MAX {
        return Err("a frame is larger than the negotiated maximum".to_owned());
    }
    let total = 7 + size as usize + 1;
    if buffer.len() < total {
        return Ok(None);
    }
    if buffer[total - 1] != FRAME_END {
        return Err("a frame does not end with the frame-end octet".to_owned());
    }
    let frame = Frame { kind: buffer[0], channel: u16::from_be_bytes([buffer[1], buffer[2]]), payload: buffer[7..total - 1].to_vec() };
    buffer.drain(..total);
    Ok(Some(frame))
}

pub(crate) fn frame(kind: u8, channel: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.push(kind);
    out.extend_from_slice(&channel.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out.push(FRAME_END);
    out
}

pub(crate) struct Args<'b> {
    bytes: &'b [u8],
    at: usize,
    bits: Option<(u8, u8)>,
}

pub(crate) type Parsed<T> = Result<T, String>;

impl<'b> Args<'b> {
    pub(crate) fn new(bytes: &'b [u8]) -> Self {
        Self { bytes, at: 0, bits: None }
    }

    fn take(&mut self, count: usize) -> Parsed<&'b [u8]> {
        self.bits = None;
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or("a method's arguments end early")?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    pub(crate) fn octet(&mut self) -> Parsed<u8> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn short(&mut self) -> Parsed<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().map_err(|_| "short")?))
    }

    pub(crate) fn long(&mut self) -> Parsed<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().map_err(|_| "short")?))
    }

    pub(crate) fn longlong(&mut self) -> Parsed<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().map_err(|_| "short")?))
    }

    pub(crate) fn shortstr(&mut self) -> Parsed<String> {
        let length = usize::from(self.octet()?);
        String::from_utf8(self.take(length)?.to_vec()).map_err(|_| "a short string is not UTF-8".to_owned())
    }

    pub(crate) fn longstr(&mut self) -> Parsed<&'b [u8]> {
        let length = self.long()? as usize;
        self.take(length)
    }

    pub(crate) fn table(&mut self) -> Parsed<&'b [u8]> {
        self.longstr()
    }

    pub(crate) fn bit(&mut self) -> Parsed<bool> {
        let (byte, index) = match self.bits {
            Some((byte, index)) if index < 8 => (byte, index),
            _ => (self.take(1)?[0], 0),
        };
        self.bits = Some((byte, index + 1));
        Ok(byte >> index & 1 == 1)
    }

    pub(crate) fn rest(&self) -> &'b [u8] {
        &self.bytes[self.at..]
    }
}

#[derive(Default)]
pub(crate) struct Out {
    pub(crate) bytes: Vec<u8>,
}

impl Out {
    pub(crate) fn method(class: u16, method: u16) -> Self {
        let mut out = Self::default();
        out.short(class).short(method);
        out
    }

    pub(crate) fn octet(&mut self, value: u8) -> &mut Self {
        self.bytes.push(value);
        self
    }

    pub(crate) fn short(&mut self, value: u16) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub(crate) fn long(&mut self, value: u32) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub(crate) fn longlong(&mut self, value: u64) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub(crate) fn shortstr(&mut self, text: &str) -> &mut Self {
        let bytes = &text.as_bytes()[..text.len().min(255)];
        self.bytes.push(bytes.len() as u8);
        self.bytes.extend_from_slice(bytes);
        self
    }

    pub(crate) fn longstr(&mut self, bytes: &[u8]) -> &mut Self {
        self.long(bytes.len() as u32);
        self.bytes.extend_from_slice(bytes);
        self
    }

    pub(crate) fn empty_table(&mut self) -> &mut Self {
        self.long(0)
    }

    pub(crate) fn string_table(&mut self, entries: &[(&str, &str)]) -> &mut Self {
        let mut table = Vec::new();
        for (name, value) in entries {
            table.push(name.len() as u8);
            table.extend_from_slice(name.as_bytes());
            table.push(b'S');
            table.extend_from_slice(&(value.len() as u32).to_be_bytes());
            table.extend_from_slice(value.as_bytes());
        }
        self.longstr(&table)
    }
}
