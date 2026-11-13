pub(crate) struct Reader<'b> {
    bytes: &'b [u8],
    pub(crate) at: usize,
}

pub(crate) type Parsed<T> = Result<T, String>;

const MAX_ARRAY: i32 = 100_000;

impl<'b> Reader<'b> {
    pub(crate) fn new(bytes: &'b [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, count: usize) -> Parsed<&'b [u8]> {
        let end = self
            .at
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or("a Kafka request ends early")?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    pub(crate) fn i8(&mut self) -> Parsed<i8> {
        Ok(self.take(1)?[0] as i8)
    }

    pub(crate) fn i16(&mut self) -> Parsed<i16> {
        Ok(i16::from_be_bytes(
            self.take(2)?.try_into().map_err(|_| "short")?,
        ))
    }

    pub(crate) fn i32(&mut self) -> Parsed<i32> {
        Ok(i32::from_be_bytes(
            self.take(4)?.try_into().map_err(|_| "short")?,
        ))
    }

    pub(crate) fn i64(&mut self) -> Parsed<i64> {
        Ok(i64::from_be_bytes(
            self.take(8)?.try_into().map_err(|_| "short")?,
        ))
    }

    pub(crate) fn string(&mut self) -> Parsed<Option<String>> {
        let length = self.i16()?;
        if length < 0 {
            return Ok(None);
        }
        let bytes = self.take(length as usize)?;
        Ok(Some(
            String::from_utf8(bytes.to_vec()).map_err(|_| "a Kafka string is not UTF-8")?,
        ))
    }

    pub(crate) fn bytes(&mut self) -> Parsed<Option<&'b [u8]>> {
        let length = self.i32()?;
        if length < 0 {
            return Ok(None);
        }
        self.take(length as usize).map(Some)
    }

    /// An unsigned varint, as flexible versions encode lengths and tag counts.
    pub(crate) fn uvarint(&mut self) -> Parsed<u64> {
        let mut value = 0u64;
        for shift in (0..35).step_by(7) {
            let byte = self.take(1)?[0];
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err("a Kafka varint is longer than five bytes".to_owned())
    }

    /// A compact nullable string: its length plus one as a varint, 0 for null.
    pub(crate) fn compact_string(&mut self) -> Parsed<Option<String>> {
        let length = self.uvarint()?;
        if length == 0 {
            return Ok(None);
        }
        let bytes = self.take(usize::try_from(length - 1).map_err(|_| "a Kafka string is too long")?)?;
        Ok(Some(String::from_utf8(bytes.to_vec()).map_err(|_| "a Kafka string is not UTF-8")?))
    }

    /// A compact nullable array: its length plus one as a varint, 0 for null.
    pub(crate) fn compact_array<T>(
        &mut self,
        mut each: impl FnMut(&mut Self) -> Parsed<T>,
    ) -> Parsed<Option<Vec<T>>> {
        let length = self.uvarint()?;
        if length == 0 {
            return Ok(None);
        }
        let count = length - 1;
        if count > MAX_ARRAY as u64 || count as usize > self.bytes.len() - self.at {
            return Err("a Kafka array is longer than its request".to_owned());
        }
        (0..count).map(|_| each(self)).collect::<Parsed<Vec<T>>>().map(Some)
    }

    /// A UUID's 16 bytes.
    pub(crate) fn uuid(&mut self) -> Parsed<[u8; 16]> {
        self.take(16)?.try_into().map_err(|_| "short".to_owned())
    }

    /// Skips a tagged-fields section: its count, then each tag, size and bytes.
    pub(crate) fn skip_tags(&mut self) -> Parsed<()> {
        let count = self.uvarint()?;
        for _ in 0..count {
            self.uvarint()?;
            let size = self.uvarint()?;
            self.take(usize::try_from(size).map_err(|_| "a tagged field is too long")?)?;
        }
        Ok(())
    }

    pub(crate) fn array<T>(
        &mut self,
        mut each: impl FnMut(&mut Self) -> Parsed<T>,
    ) -> Parsed<Vec<T>> {
        let count = self.i32()?;
        if count < 0 {
            return Ok(Vec::new());
        }
        if count > MAX_ARRAY || count as usize > self.bytes.len() - self.at {
            return Err("a Kafka array is longer than its request".to_owned());
        }
        (0..count).map(|_| each(self)).collect()
    }
}

#[derive(Default)]
pub(crate) struct Writer {
    pub(crate) out: Vec<u8>,
}

impl Writer {
    pub(crate) fn i8(&mut self, value: i8) -> &mut Self {
        self.out.push(value as u8);
        self
    }

    pub(crate) fn i16(&mut self, value: i16) -> &mut Self {
        self.out.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub(crate) fn i32(&mut self, value: i32) -> &mut Self {
        self.out.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub(crate) fn i64(&mut self, value: i64) -> &mut Self {
        self.out.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub(crate) fn string(&mut self, value: Option<&str>) -> &mut Self {
        match value {
            Some(text) => {
                self.i16(text.len().min(i16::MAX as usize) as i16);
                self.out
                    .extend_from_slice(&text.as_bytes()[..text.len().min(i16::MAX as usize)]);
            }
            None => {
                self.i16(-1);
            }
        }
        self
    }

    pub(crate) fn bytes(&mut self, value: &[u8]) -> &mut Self {
        self.i32(value.len() as i32);
        self.out.extend_from_slice(value);
        self
    }

    pub(crate) fn count(&mut self, count: usize) -> &mut Self {
        self.i32(count as i32)
    }

    pub(crate) fn uvarint(&mut self, mut value: u64) -> &mut Self {
        while value >= 0x80 {
            self.out.push((value as u8 & 0x7F) | 0x80);
            value >>= 7;
        }
        self.out.push(value as u8);
        self
    }

    pub(crate) fn compact_string(&mut self, value: Option<&str>) -> &mut Self {
        match value {
            Some(text) => {
                self.uvarint(text.len() as u64 + 1);
                self.out.extend_from_slice(text.as_bytes());
            }
            None => {
                self.uvarint(0);
            }
        }
        self
    }

    /// The length of a compact array, or `None` for a null one.
    pub(crate) fn compact_count(&mut self, count: Option<usize>) -> &mut Self {
        self.uvarint(count.map_or(0, |count| count as u64 + 1))
    }

    pub(crate) fn uuid(&mut self, value: &[u8; 16]) -> &mut Self {
        self.out.extend_from_slice(value);
        self
    }

    /// An empty tagged-fields section.
    pub(crate) fn no_tags(&mut self) -> &mut Self {
        self.uvarint(0)
    }
}

/// The topic ID of `topic`: a fixed 128-bit hash of its name, with the RFC 4122 variant and
/// version bits set, so every broker and every restart gives a topic the same ID, and never the
/// all-zero ID Kafka uses for "none".
pub(crate) fn topic_id(topic: &str) -> [u8; 16] {
    let mut hash: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    for byte in topic.as_bytes() {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013B);
    }
    let mut id = hash.to_be_bytes();
    id[6] = (id[6] & 0x0F) | 0x40;
    id[8] = (id[8] & 0x3F) | 0x80;
    id
}

pub(crate) fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x82f6_3b78
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
