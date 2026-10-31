use crate::{DecodeError, damaged};

/// The bits needed for the largest of `values` (0 when all are 0).
pub fn bit_width(values: &[u64]) -> u8 {
    values.iter().fold(0u64, |all, value| all | value).checked_ilog2().map_or(0, |log| log as u8 + 1)
}

/// Packs `values` into `width` bits each, least significant bit first, as Parquet's
/// bit-packed runs lay them out. Bits of a value above `width` are dropped.
pub fn pack(values: &[u64], width: u8, out: &mut Vec<u8>) {
    let width = u32::from(width.min(64));
    if width == 0 {
        return;
    }
    let mut buffer: u128 = 0;
    let mut filled = 0u32;
    let mask = if width == 64 { u64::MAX } else { (1u64 << width) - 1 };
    for value in values {
        buffer |= u128::from(value & mask) << filled;
        filled += width;
        while filled >= 8 {
            out.push(buffer as u8);
            buffer >>= 8;
            filled -= 8;
        }
    }
    if filled > 0 {
        out.push(buffer as u8);
    }
}

/// Reads `count` values of `width` bits from `bytes`.
pub fn unpack(bytes: &[u8], width: u8, count: usize) -> Result<Vec<u64>, DecodeError> {
    if width > 64 {
        return Err(damaged("a bit width is above 64"));
    }
    let width = u32::from(width);
    if width == 0 {
        return Ok(vec![0; count]);
    }
    let needed = (count as u128 * u128::from(width)).div_ceil(8);
    if needed > bytes.len() as u128 {
        return Err(damaged("bit-packed values run past their bytes"));
    }
    let mask = if width == 64 { u64::MAX } else { (1u64 << width) - 1 };
    let mut out = Vec::with_capacity(count);
    let mut buffer: u128 = 0;
    let mut filled = 0u32;
    let mut next = bytes.iter();
    for _ in 0..count {
        while filled < width {
            let byte = next.next().ok_or_else(|| damaged("bit-packed values run past their bytes"))?;
            buffer |= u128::from(*byte) << filled;
            filled += 8;
        }
        out.push(buffer as u64 & mask);
        buffer >>= width;
        filled -= width;
    }
    Ok(out)
}
