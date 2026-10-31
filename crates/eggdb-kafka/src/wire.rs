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
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or("a Kafka request ends early")?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    pub(crate) fn i8(&mut self) -> Parsed<i8> {
        Ok(self.take(1)?[0] as i8)
    }

    pub(crate) fn i16(&mut self) -> Parsed<i16> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into().map_err(|_| "short")?))
    }

    pub(crate) fn i32(&mut self) -> Parsed<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().map_err(|_| "short")?))
    }

    pub(crate) fn i64(&mut self) -> Parsed<i64> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().map_err(|_| "short")?))
    }

    pub(crate) fn string(&mut self) -> Parsed<Option<String>> {
        let length = self.i16()?;
        if length < 0 {
            return Ok(None);
        }
        let bytes = self.take(length as usize)?;
        Ok(Some(String::from_utf8(bytes.to_vec()).map_err(|_| "a Kafka string is not UTF-8")?))
    }

    pub(crate) fn bytes(&mut self) -> Parsed<Option<&'b [u8]>> {
        let length = self.i32()?;
        if length < 0 {
            return Ok(None);
        }
        self.take(length as usize).map(Some)
    }

    pub(crate) fn array<T>(&mut self, mut each: impl FnMut(&mut Self) -> Parsed<T>) -> Parsed<Vec<T>> {
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
                self.out.extend_from_slice(&text.as_bytes()[..text.len().min(i16::MAX as usize)]);
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
}

pub(crate) fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 };
        }
    }
    !crc
}
