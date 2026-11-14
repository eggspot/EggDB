//! QUIC's variable-length integers (RFC 9000 section 16): the two high bits of the first byte
//! give the length, 1, 2, 4 or 8 bytes, for values below 2^62.

use crate::QuicCodecError;

/// The largest value a variable-length integer holds: 2^62 - 1.
pub const MAX_VARINT: u64 = (1 << 62) - 1;

/// Reads bytes off the front of a slice, every read checked.
#[derive(Debug, Clone)]
pub struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    /// A cursor at the start of `bytes`.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    /// How many bytes were read.
    pub fn position(&self) -> usize {
        self.position
    }

    /// The bytes not read yet.
    pub fn remaining(&self) -> &'a [u8] {
        self.bytes.get(self.position..).unwrap_or_default()
    }

    /// Whether every byte was read.
    pub fn is_empty(&self) -> bool {
        self.remaining().is_empty()
    }

    /// The next `length` bytes.
    pub fn take(&mut self, length: usize) -> Result<&'a [u8], QuicCodecError> {
        let end = self.position.checked_add(length).ok_or(QuicCodecError::Truncated)?;
        let taken = self.bytes.get(self.position..end).ok_or(QuicCodecError::Truncated)?;
        self.position = end;
        Ok(taken)
    }

    /// The next byte.
    pub fn u8(&mut self) -> Result<u8, QuicCodecError> {
        Ok(self.take(1)?[0])
    }

    /// The next big-endian 32-bit integer.
    pub fn u32(&mut self) -> Result<u32, QuicCodecError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// The next variable-length integer.
    pub fn varint(&mut self) -> Result<u64, QuicCodecError> {
        let first = self.u8()?;
        let length = 1usize << (first >> 6);
        let mut value = u64::from(first & 0x3f);
        for byte in self.take(length - 1)? {
            value = (value << 8) | u64::from(*byte);
        }
        Ok(value)
    }

    /// A variable-length integer read as a length, which must fit what is left.
    pub fn length(&mut self) -> Result<usize, QuicCodecError> {
        let value = usize::try_from(self.varint()?).map_err(|_| QuicCodecError::Truncated)?;
        if value > self.remaining().len() {
            return Err(QuicCodecError::Truncated);
        }
        Ok(value)
    }
}

/// How many bytes `value` takes as a variable-length integer.
pub fn varint_length(value: u64) -> usize {
    match value {
        0..=63 => 1,
        64..=16_383 => 2,
        16_384..=1_073_741_823 => 4,
        _ => 8,
    }
}

/// Appends `value`, which must be at most [`MAX_VARINT`], in its shortest form.
pub fn write_varint(out: &mut Vec<u8>, value: u64) -> Result<(), QuicCodecError> {
    if value > MAX_VARINT {
        return Err(QuicCodecError::InvalidValue);
    }
    match varint_length(value) {
        1 => out.push(value as u8),
        2 => out.extend_from_slice(&((value as u16) | 0x4000).to_be_bytes()),
        4 => out.extend_from_slice(&((value as u32) | 0x8000_0000).to_be_bytes()),
        _ => out.extend_from_slice(&(value | 0xc000_0000_0000_0000).to_be_bytes()),
    }
    Ok(())
}

/// Appends `value` as a variable-length integer of exactly `length` bytes (1, 2, 4 or 8), as a
/// packet's Length field written before the payload is known.
pub fn write_varint_with_length(out: &mut Vec<u8>, value: u64, length: usize) -> Result<(), QuicCodecError> {
    if varint_length(value) > length {
        return Err(QuicCodecError::InvalidValue);
    }
    match length {
        1 => out.push(value as u8),
        2 => out.extend_from_slice(&((value as u16) | 0x4000).to_be_bytes()),
        4 => out.extend_from_slice(&((value as u32) | 0x8000_0000).to_be_bytes()),
        8 => out.extend_from_slice(&(value | 0xc000_0000_0000_0000).to_be_bytes()),
        _ => return Err(QuicCodecError::InvalidValue),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_9000_appendix_a_1_examples_decode() {
        for (bytes, value) in [
            (&[0xc2, 0x19, 0x7c, 0x5e, 0xff, 0x14, 0xe8, 0x8c][..], 151_288_809_941_952_652u64),
            (&[0x9d, 0x7f, 0x3e, 0x7d][..], 494_878_333),
            (&[0x7b, 0xbd][..], 15_293),
            (&[0x25][..], 37),
            (&[0x40, 0x25][..], 37),
        ] {
            assert_eq!(Cursor::new(bytes).varint(), Ok(value));
        }
    }

    #[test]
    fn values_round_trip_in_their_shortest_form() {
        for value in [0, 63, 64, 16_383, 16_384, 1_073_741_823, 1_073_741_824, MAX_VARINT] {
            let mut out = Vec::new();
            write_varint(&mut out, value).unwrap();
            assert_eq!(out.len(), varint_length(value));
            assert_eq!(Cursor::new(&out).varint(), Ok(value));
        }
        assert_eq!(write_varint(&mut Vec::new(), MAX_VARINT + 1), Err(QuicCodecError::InvalidValue));
    }

    #[test]
    fn a_length_past_the_input_is_truncated() {
        assert_eq!(Cursor::new(&[0x05, 1, 2]).length(), Err(QuicCodecError::Truncated));
        assert_eq!(Cursor::new(&[0x80]).varint(), Err(QuicCodecError::Truncated));
    }
}
