use crate::bits::{pack, unpack};
use crate::delta::{read_varint, write_varint};
use crate::{DecodeError, damaged};

const MIN_REPEAT: usize = 8;

fn value_bytes(width: u8) -> usize {
    usize::from(width).div_ceil(8)
}

fn repeated_run(value: u64, count: usize, width: u8, out: &mut Vec<u8>) {
    write_varint((count as u64) << 1, out);
    out.extend_from_slice(&value.to_le_bytes()[..value_bytes(width)]);
}

fn packed_run(values: &[u64], width: u8, out: &mut Vec<u8>) {
    let groups = values.len().div_ceil(8);
    write_varint(((groups as u64) << 1) | 1, out);
    let mut padded = values.to_vec();
    padded.resize(groups * 8, 0);
    pack(&padded, width, out);
}

/// Encodes `values` (each below `2^width`) as Parquet's run-length/bit-packed hybrid: runs of
/// eight or more equal values become one repeated run, the rest bit-packed groups of eight.
pub fn encode_hybrid(values: &[u64], width: u8, out: &mut Vec<u8>) {
    let mut at = 0;
    let mut literal_start = 0;
    while at < values.len() {
        let mut end = at + 1;
        while end < values.len() && values[end] == values[at] {
            end += 1;
        }
        if end - at >= MIN_REPEAT {
            let need = (8 - (at - literal_start) % 8) % 8;
            if end - at - need >= MIN_REPEAT {
                let literal_end = at + need;
                if literal_end > literal_start {
                    packed_run(&values[literal_start..literal_end], width, out);
                }
                repeated_run(values[at], end - literal_end, width, out);
                literal_start = end;
            }
        }
        at = end;
    }
    if literal_start < values.len() {
        packed_run(&values[literal_start..], width, out);
    }
}

/// Decodes `count` values of a hybrid run sequence; returns them and the bytes used.
pub fn decode_hybrid(bytes: &[u8], width: u8, count: usize) -> Result<(Vec<u64>, usize), DecodeError> {
    if width > 64 {
        return Err(damaged("a bit width is above 64"));
    }
    let mut out = Vec::with_capacity(count.min(1 << 16));
    let mut at = 0;
    while out.len() < count {
        let header = read_varint(bytes, &mut at)?;
        if header & 1 == 1 {
            let groups = usize::try_from(header >> 1).map_err(|_| damaged("a run is too long"))?;
            let values = groups.checked_mul(8).ok_or_else(|| damaged("a run is too long"))?;
            let length = groups.checked_mul(usize::from(width)).ok_or_else(|| damaged("a run is too long"))?;
            let packed = bytes.get(at..at.saturating_add(length)).ok_or_else(|| damaged("a bit-packed run runs past its bytes"))?;
            let wanted = (count - out.len()).min(values);
            out.extend(unpack(packed, width, wanted)?);
            at += length;
        } else {
            let repeat = usize::try_from(header >> 1).map_err(|_| damaged("a run is too long"))?;
            if repeat == 0 {
                return Err(damaged("a repeated run is empty"));
            }
            let size = value_bytes(width);
            let raw = bytes.get(at..at + size).ok_or_else(|| damaged("a repeated run runs past its bytes"))?;
            let mut buffer = [0u8; 8];
            buffer[..size].copy_from_slice(raw);
            at += size;
            let wanted = (count - out.len()).min(repeat);
            out.extend(std::iter::repeat_n(u64::from_le_bytes(buffer), wanted));
        }
    }
    Ok((out, at))
}
