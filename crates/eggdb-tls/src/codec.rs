//! The presentation language of TLS (RFC 8446 section 3): big-endian integers of one to four
//! bytes and vectors that carry their length in a one-, two- or three-byte prefix. Reading never
//! panics: a short or over-long input is a [`CodecError`].

/// Why TLS bytes were refused while decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// The input ends inside a field.
    Truncated,
    /// Bytes after the last field of a structure.
    TrailingData,
    /// A vector whose length is outside the bounds its definition allows.
    LengthOutOfRange,
    /// A field value the protocol forbids here.
    IllegalValue,
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Truncated => "the TLS message ends early",
            Self::TrailingData => "the TLS message has bytes after its last field",
            Self::LengthOutOfRange => "a TLS vector has a length its definition forbids",
            Self::IllegalValue => "a TLS field has a value the protocol forbids",
        })
    }
}

impl std::error::Error for CodecError {}

/// Reads TLS fields from the front of a byte slice.
#[derive(Debug, Clone, Copy)]
pub struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    /// A reader over `bytes`.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    /// Whether every byte has been read.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The bytes not read yet.
    pub fn remaining(&self) -> &'a [u8] {
        self.bytes
    }

    /// Succeeds only when every byte has been read.
    pub fn finish(self) -> Result<(), CodecError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(CodecError::TrailingData)
        }
    }

    /// The next `length` bytes.
    pub fn read_bytes(&mut self, length: usize) -> Result<&'a [u8], CodecError> {
        let (taken, rest) = self
            .bytes
            .split_at_checked(length)
            .ok_or(CodecError::Truncated)?;
        self.bytes = rest;
        Ok(taken)
    }

    /// The next `N` bytes as an array.
    pub fn read_array<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        let mut array = [0u8; N];
        array.copy_from_slice(self.read_bytes(N)?);
        Ok(array)
    }

    /// A one-byte integer.
    pub fn read_u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.read_array::<1>()?[0])
    }

    /// A two-byte big-endian integer.
    pub fn read_u16(&mut self) -> Result<u16, CodecError> {
        Ok(u16::from_be_bytes(self.read_array()?))
    }

    /// A three-byte big-endian integer.
    pub fn read_u24(&mut self) -> Result<u32, CodecError> {
        let [high, middle, low] = self.read_array()?;
        Ok(u32::from_be_bytes([0, high, middle, low]))
    }

    /// A four-byte big-endian integer.
    pub fn read_u32(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_be_bytes(self.read_array()?))
    }

    /// A vector with a one-byte length, whose length must lie in `bounds`.
    pub fn read_vector_u8(
        &mut self,
        bounds: std::ops::RangeInclusive<usize>,
    ) -> Result<&'a [u8], CodecError> {
        let length = usize::from(self.read_u8()?);
        self.read_bounded(length, bounds)
    }

    /// A vector with a two-byte length, whose length must lie in `bounds`.
    pub fn read_vector_u16(
        &mut self,
        bounds: std::ops::RangeInclusive<usize>,
    ) -> Result<&'a [u8], CodecError> {
        let length = usize::from(self.read_u16()?);
        self.read_bounded(length, bounds)
    }

    /// A vector with a three-byte length, whose length must lie in `bounds`.
    pub fn read_vector_u24(
        &mut self,
        bounds: std::ops::RangeInclusive<usize>,
    ) -> Result<&'a [u8], CodecError> {
        let length = usize::try_from(self.read_u24()?).map_err(|_| CodecError::LengthOutOfRange)?;
        self.read_bounded(length, bounds)
    }

    fn read_bounded(
        &mut self,
        length: usize,
        bounds: std::ops::RangeInclusive<usize>,
    ) -> Result<&'a [u8], CodecError> {
        if !bounds.contains(&length) {
            return Err(CodecError::LengthOutOfRange);
        }
        self.read_bytes(length)
    }
}

/// Writes TLS fields to a growing buffer.
#[derive(Debug, Clone, Default)]
pub struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    /// An empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// The bytes written.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// The number of bytes written so far.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether nothing has been written.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Raw bytes.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// A one-byte integer.
    pub fn write_u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    /// A two-byte big-endian integer.
    pub fn write_u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    /// A three-byte big-endian integer; `value` must be below 2^24.
    pub fn write_u24(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes()[1..]);
    }

    /// A four-byte big-endian integer.
    pub fn write_u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    /// A vector with a one-byte length holding what `contents` writes.
    pub fn write_vector_u8(&mut self, contents: impl FnOnce(&mut Writer)) {
        self.write_prefixed(1, contents);
    }

    /// A vector with a two-byte length holding what `contents` writes.
    pub fn write_vector_u16(&mut self, contents: impl FnOnce(&mut Writer)) {
        self.write_prefixed(2, contents);
    }

    /// A vector with a three-byte length holding what `contents` writes.
    pub fn write_vector_u24(&mut self, contents: impl FnOnce(&mut Writer)) {
        self.write_prefixed(3, contents);
    }

    fn write_prefixed(&mut self, prefix_bytes: usize, contents: impl FnOnce(&mut Writer)) {
        let start = self.bytes.len();
        self.bytes.resize(start + prefix_bytes, 0);
        contents(self);
        let length = (self.bytes.len() - start - prefix_bytes) as u64;
        let encoded = length.to_be_bytes();
        self.bytes[start..start + prefix_bytes].copy_from_slice(&encoded[8 - prefix_bytes..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_round_trip_big_endian() {
        let mut writer = Writer::new();
        writer.write_u8(0x01);
        writer.write_u16(0x0203);
        writer.write_u24(0x040506);
        writer.write_u32(0x0708090a);
        let bytes = writer.into_bytes();
        assert_eq!(bytes, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.read_u8(), Ok(1));
        assert_eq!(reader.read_u16(), Ok(0x0203));
        assert_eq!(reader.read_u24(), Ok(0x040506));
        assert_eq!(reader.read_u32(), Ok(0x0708090a));
        assert_eq!(reader.finish(), Ok(()));
    }

    #[test]
    fn vectors_carry_their_length_in_the_prefix() {
        let mut writer = Writer::new();
        writer.write_vector_u16(|inner| {
            inner.write_vector_u8(|innermost| innermost.write_bytes(b"abc"));
        });
        let bytes = writer.into_bytes();
        assert_eq!(bytes, [0, 4, 3, b'a', b'b', b'c']);
        let mut reader = Reader::new(&bytes);
        let mut inner = Reader::new(reader.read_vector_u16(0..=65535).unwrap());
        assert_eq!(inner.read_vector_u8(1..=255), Ok(&b"abc"[..]));
        assert_eq!(inner.finish(), Ok(()));
    }

    #[test]
    fn short_and_out_of_bounds_input_is_refused() {
        assert_eq!(Reader::new(&[0]).read_u16(), Err(CodecError::Truncated));
        assert_eq!(Reader::new(&[0, 5, 1]).read_vector_u16(0..=10), Err(CodecError::Truncated));
        assert_eq!(Reader::new(&[0]).read_vector_u8(1..=10), Err(CodecError::LengthOutOfRange));
        assert_eq!(Reader::new(&[1, 2]).finish(), Err(CodecError::TrailingData));
        assert_eq!(
            Reader::new(&[0xff, 0xff, 0xff]).read_vector_u24(0..=10),
            Err(CodecError::LengthOutOfRange)
        );
    }
}
