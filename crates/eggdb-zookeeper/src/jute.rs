const MOST_STRING_BYTES: usize = 1 << 20;
const MOST_BUFFER_BYTES: usize = 1 << 20;
const MOST_VECTOR_ITEMS: usize = 1 << 16;

/// Reads jute records (big-endian integers, length-prefixed strings, buffers and vectors).
pub(crate) struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    pub(crate) fn new(bytes: &'b [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    pub(crate) fn finished(&self) -> bool {
        self.at >= self.bytes.len()
    }

    fn take(&mut self, count: usize) -> Option<&'b [u8]> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len())?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Some(taken)
    }

    pub(crate) fn int(&mut self) -> Option<i32> {
        Some(i32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    pub(crate) fn long(&mut self) -> Option<i64> {
        Some(i64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    pub(crate) fn boolean(&mut self) -> Option<bool> {
        Some(self.take(1)?[0] != 0)
    }

    pub(crate) fn buffer(&mut self) -> Option<Option<Vec<u8>>> {
        let length = self.int()?;
        if length < 0 {
            return Some(None);
        }
        let length = usize::try_from(length).ok().filter(|length| *length <= MOST_BUFFER_BYTES)?;
        Some(Some(self.take(length)?.to_vec()))
    }

    pub(crate) fn string(&mut self) -> Option<String> {
        let length = self.int()?;
        if length < 0 {
            return Some(String::new());
        }
        let length = usize::try_from(length).ok().filter(|length| *length <= MOST_STRING_BYTES)?;
        String::from_utf8(self.take(length)?.to_vec()).ok()
    }

    pub(crate) fn count(&mut self) -> Option<usize> {
        let count = self.int()?;
        if count < 0 {
            return Some(0);
        }
        usize::try_from(count).ok().filter(|count| *count <= MOST_VECTOR_ITEMS)
    }
}

/// Writes jute records.
#[derive(Default)]
pub(crate) struct Writer {
    pub(crate) out: Vec<u8>,
}

impl Writer {
    pub(crate) fn int(&mut self, value: i32) -> &mut Self {
        self.out.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub(crate) fn long(&mut self, value: i64) -> &mut Self {
        self.out.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub(crate) fn boolean(&mut self, value: bool) -> &mut Self {
        self.out.push(u8::from(value));
        self
    }

    pub(crate) fn buffer(&mut self, value: Option<&[u8]>) -> &mut Self {
        match value {
            Some(bytes) => {
                self.int(i32::try_from(bytes.len()).unwrap_or(i32::MAX));
                self.out.extend_from_slice(bytes);
            }
            None => {
                self.int(-1);
            }
        }
        self
    }

    pub(crate) fn string(&mut self, value: &str) -> &mut Self {
        self.buffer(Some(value.as_bytes()))
    }

    /// The record with its four-byte length in front, as frames go on the wire.
    pub(crate) fn framed(&self) -> Vec<u8> {
        let mut out = (self.out.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(&self.out);
        out
    }
}
