use crate::{DecodeError, read_varint};

const MAX_LITERAL: usize = 1 << 16;

fn damaged_block(message: &str) -> DecodeError {
    DecodeError(message.to_owned())
}

/// Decompresses a Snappy block (the raw format of Google's public description), refusing
/// output larger than `limit` bytes.
pub fn snappy_decompress(input: &[u8], limit: usize) -> Result<Vec<u8>, DecodeError> {
    let mut at = 0;
    let length = read_varint(input, &mut at)?;
    let length = usize::try_from(length).ok().filter(|length| *length <= limit).ok_or_else(|| damaged_block("a Snappy block is larger than its page says"))?;
    let mut out = Vec::with_capacity(length);
    while at < input.len() {
        let tag = input[at];
        at += 1;
        match tag & 3 {
            0 => {
                let mut size = usize::from(tag >> 2);
                if size >= 60 {
                    let extra = size - 59;
                    let raw = input.get(at..at + extra).ok_or_else(|| damaged_block("a Snappy literal ends early"))?;
                    size = raw.iter().rev().fold(0usize, |total, byte| (total << 8) | usize::from(*byte));
                    at += extra;
                }
                let size = size + 1;
                let literal = input.get(at..at.saturating_add(size)).ok_or_else(|| damaged_block("a Snappy literal ends early"))?;
                if out.len() + size > length {
                    return Err(damaged_block("a Snappy block holds more than it says"));
                }
                out.extend_from_slice(literal);
                at += size;
            }
            kind => {
                let (size, offset) = match kind {
                    1 => {
                        let low = *input.get(at).ok_or_else(|| damaged_block("a Snappy copy ends early"))?;
                        at += 1;
                        (usize::from((tag >> 2) & 7) + 4, (usize::from(tag >> 5) << 8) | usize::from(low))
                    }
                    2 => {
                        let raw = input.get(at..at + 2).ok_or_else(|| damaged_block("a Snappy copy ends early"))?;
                        at += 2;
                        (usize::from(tag >> 2) + 1, usize::from(u16::from_le_bytes([raw[0], raw[1]])))
                    }
                    _ => {
                        let raw = input.get(at..at + 4).ok_or_else(|| damaged_block("a Snappy copy ends early"))?;
                        at += 4;
                        (usize::from(tag >> 2) + 1, u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize)
                    }
                };
                if offset == 0 || offset > out.len() || out.len() + size > length {
                    return Err(damaged_block("a Snappy copy points outside its block"));
                }
                let start = out.len() - offset;
                for index in 0..size {
                    let byte = out[start + index];
                    out.push(byte);
                }
            }
        }
    }
    if out.len() != length {
        return Err(damaged_block("a Snappy block holds less than it says"));
    }
    Ok(out)
}

/// Compresses `input` as a Snappy block of literals only: valid for every reader, and as fast
/// as copying, but no smaller than the input.
pub fn snappy_compress(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + input.len() / MAX_LITERAL * 3 + 8);
    crate::write_varint(input.len() as u64, &mut out);
    for literal in input.chunks(MAX_LITERAL) {
        let size = literal.len() - 1;
        if size < 60 {
            out.push((size as u8) << 2);
        } else if size < 256 {
            out.push(60 << 2);
            out.push(size as u8);
        } else {
            out.push(61 << 2);
            out.extend_from_slice(&(size as u16).to_le_bytes());
        }
        out.extend_from_slice(literal);
    }
    out
}
