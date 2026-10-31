use crate::{DecodeError, damaged};

/// Appends `value` as an unsigned LEB128 varint.
pub fn write_varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Reads an unsigned LEB128 varint at `*at`, moving `*at` past it.
pub fn read_varint(bytes: &[u8], at: &mut usize) -> Result<u64, DecodeError> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at).ok_or_else(|| damaged("a varint runs past its bytes"))?;
        *at += 1;
        if shift == 63 && byte > 1 {
            return Err(damaged("a varint overflows 64 bits"));
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(damaged("a varint is longer than 10 bytes"))
}

fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// Encodes `values` as the first value and then each difference, zigzag varints: sorted or
/// slowly changing columns (times, ids) shrink to a byte or two per value.
pub fn encode_deltas(values: &[i64], out: &mut Vec<u8>) {
    write_varint(values.len() as u64, out);
    let mut previous = 0i64;
    for value in values {
        write_varint(zigzag(value.wrapping_sub(previous)), out);
        previous = *value;
    }
}

/// Decodes what [`encode_deltas`] wrote; returns the values and the bytes used.
pub fn decode_deltas(bytes: &[u8]) -> Result<(Vec<i64>, usize), DecodeError> {
    let mut at = 0;
    let count = read_varint(bytes, &mut at)?;
    if count > bytes.len() as u64 {
        return Err(damaged("a delta run claims more values than its bytes can hold"));
    }
    let mut values = Vec::with_capacity(count as usize);
    let mut previous = 0i64;
    for _ in 0..count {
        previous = previous.wrapping_add(unzigzag(read_varint(bytes, &mut at)?));
        values.push(previous);
    }
    Ok((values, at))
}
