use std::fmt;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Why [`base64_decode`] refused its input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base64Error {
    /// The length is not a multiple of four.
    Length,
    /// A byte outside the alphabet, or padding anywhere but at the end.
    Symbol,
    /// The bits after the last byte are not zero, so the text is not the canonical encoding.
    TrailingBits,
}

impl fmt::Display for Base64Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Length => "base64 text must have a length that is a multiple of four",
            Self::Symbol => "base64 text has a character outside its alphabet",
            Self::TrailingBits => "base64 text is not in its canonical form",
        })
    }
}

impl std::error::Error for Base64Error {}

/// `bytes` in Base64 (RFC 4648, section 4) with `=` padding.
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let first = group[0];
        let second = group.get(1).copied().unwrap_or(0);
        let third = group.get(2).copied().unwrap_or(0);
        let bits = u32::from(first) << 16 | u32::from(second) << 8 | u32::from(third);
        let symbol = |shift: u32| char::from(ALPHABET[((bits >> shift) & 0x3f) as usize]);
        text.push(symbol(18));
        text.push(symbol(12));
        text.push(if group.len() > 1 { symbol(6) } else { '=' });
        text.push(if group.len() > 2 { symbol(0) } else { '=' });
    }
    text
}

fn symbol_value(symbol: u8) -> Option<u32> {
    Some(u32::from(match symbol {
        b'A'..=b'Z' => symbol - b'A',
        b'a'..=b'z' => symbol - b'a' + 26,
        b'0'..=b'9' => symbol - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    }))
}

/// The bytes that Base64 `text` (RFC 4648, section 4, padded) encodes. Whitespace, a missing
/// padding and bits left over after the last byte are refused.
pub fn base64_decode(text: &[u8]) -> Result<Vec<u8>, Base64Error> {
    if !text.len().is_multiple_of(4) {
        return Err(Base64Error::Length);
    }
    let mut bytes = Vec::with_capacity(text.len() / 4 * 3);
    let groups = text.len() / 4;
    for (index, group) in text.as_chunks::<4>().0.iter().enumerate() {
        let padding = match group {
            [_, _, b'=', b'='] => 2,
            [_, _, _, b'='] => 1,
            _ => 0,
        };
        if padding > 0 && index + 1 != groups {
            return Err(Base64Error::Symbol);
        }
        let mut bits = 0u32;
        for &symbol in &group[..4 - padding] {
            bits = bits << 6 | symbol_value(symbol).ok_or(Base64Error::Symbol)?;
        }
        bits <<= 6 * padding as u32;
        let decoded = [(bits >> 16) as u8, (bits >> 8) as u8, bits as u8];
        let kept = 3 - padding;
        if decoded[kept..].iter().any(|&byte| byte != 0) {
            return Err(Base64Error::TrailingBits);
        }
        bytes.extend_from_slice(&decoded[..kept]);
    }
    Ok(bytes)
}
