use crate::ContentError;

const LENGTH_BASES: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DISTANCE_BASES: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DISTANCE_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];
const MAX_BITS: usize = 15;

fn corrupt(message: &str) -> ContentError {
    ContentError(format!("the compressed data is corrupt: {message}"))
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
    buffer: u64,
    count: u32,
}

impl Bits<'_> {
    fn take(&mut self, wanted: u32) -> Result<u32, ContentError> {
        while self.count < wanted {
            let byte = *self
                .data
                .get(self.at)
                .ok_or_else(|| corrupt("it ends early"))?;
            self.at += 1;
            self.buffer |= u64::from(byte) << self.count;
            self.count += 8;
        }
        let value = (self.buffer & ((1u64 << wanted) - 1)) as u32;
        self.buffer >>= wanted;
        self.count -= wanted;
        Ok(value)
    }

    fn align(&mut self) {
        self.buffer = 0;
        self.count = 0;
    }
}

struct Huffman {
    counts: [u16; MAX_BITS + 1],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Self, ContentError> {
        let mut counts = [0u16; MAX_BITS + 1];
        for &length in lengths {
            let slot = counts
                .get_mut(usize::from(length))
                .ok_or_else(|| corrupt("a code is too long"))?;
            *slot += 1;
        }
        let mut left: i32 = 1;
        for &count in &counts[1..] {
            left = (left << 1) - i32::from(count);
            if left < 0 {
                return Err(corrupt("a code table is over-subscribed"));
            }
        }
        let mut offsets = [0u16; MAX_BITS + 2];
        for length in 1..=MAX_BITS {
            offsets[length + 1] = offsets[length] + counts[length];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (symbol, &length) in lengths.iter().enumerate() {
            if length != 0 {
                let slot = &mut offsets[usize::from(length)];
                if let Some(entry) = symbols.get_mut(usize::from(*slot)) {
                    *entry = symbol as u16;
                }
                *slot += 1;
            }
        }
        Ok(Self { counts, symbols })
    }

    fn decode(&self, bits: &mut Bits<'_>) -> Result<u16, ContentError> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for &count in &self.counts[1..] {
            code |= bits.take(1)? as i32;
            let count = i32::from(count);
            if code - first < count {
                return self
                    .symbols
                    .get((index + code - first) as usize)
                    .copied()
                    .ok_or_else(|| corrupt("a code has no symbol"));
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(corrupt("a code is not in its table"))
    }
}

fn fixed_tables() -> Result<(Huffman, Huffman), ContentError> {
    let mut lengths = [8u8; 288];
    lengths[144..256].fill(9);
    lengths[256..280].fill(7);
    Ok((Huffman::new(&lengths)?, Huffman::new(&[5u8; 30])?))
}

fn dynamic_tables(bits: &mut Bits<'_>) -> Result<(Huffman, Huffman), ContentError> {
    let literal_count = bits.take(5)? as usize + 257;
    let distance_count = bits.take(5)? as usize + 1;
    let length_count = bits.take(4)? as usize + 4;
    if literal_count > 286 || distance_count > 30 {
        return Err(corrupt("a block declares too many codes"));
    }
    let mut code_lengths = [0u8; 19];
    for &position in &CODE_LENGTH_ORDER[..length_count] {
        code_lengths[position] = bits.take(3)? as u8;
    }
    let code_length_table = Huffman::new(&code_lengths)?;
    let mut lengths = vec![0u8; literal_count + distance_count];
    let mut filled = 0;
    while filled < lengths.len() {
        let symbol = code_length_table.decode(bits)?;
        let (value, repeat) = match symbol {
            0..=15 => (symbol as u8, 1),
            16 => {
                let previous = *lengths
                    .get(filled.wrapping_sub(1))
                    .filter(|_| filled > 0)
                    .ok_or_else(|| corrupt("a repeat has nothing before it"))?;
                (previous, 3 + bits.take(2)? as usize)
            }
            17 => (0, 3 + bits.take(3)? as usize),
            _ => (0, 11 + bits.take(7)? as usize),
        };
        if filled + repeat > lengths.len() {
            return Err(corrupt("code lengths run past the table"));
        }
        lengths[filled..filled + repeat].fill(value);
        filled += repeat;
    }
    if lengths[256] == 0 {
        return Err(corrupt("a block has no end code"));
    }
    Ok((
        Huffman::new(&lengths[..literal_count])?,
        Huffman::new(&lengths[literal_count..])?,
    ))
}

fn codes(
    bits: &mut Bits<'_>,
    out: &mut Vec<u8>,
    literals: &Huffman,
    distances: &Huffman,
    limit: usize,
) -> Result<(), ContentError> {
    loop {
        let symbol = usize::from(literals.decode(bits)?);
        if symbol < 256 {
            out.push(symbol as u8);
        } else if symbol == 256 {
            return Ok(());
        } else {
            let at = symbol - 257;
            let (&base, &extra) = LENGTH_BASES
                .get(at)
                .zip(LENGTH_EXTRA.get(at))
                .ok_or_else(|| corrupt("a length code is out of range"))?;
            let length = usize::from(base) + bits.take(u32::from(extra))? as usize;
            let at = usize::from(distances.decode(bits)?);
            let (&base, &extra) = DISTANCE_BASES
                .get(at)
                .zip(DISTANCE_EXTRA.get(at))
                .ok_or_else(|| corrupt("a distance code is out of range"))?;
            let distance = usize::from(base) + bits.take(u32::from(extra))? as usize;
            if distance > out.len() {
                return Err(corrupt("a distance reaches before the start"));
            }
            let start = out.len() - distance;
            for offset in 0..length {
                let byte = out[start + offset];
                out.push(byte);
            }
        }
        if out.len() > limit {
            return Err(ContentError(format!(
                "the content expands past {limit} bytes"
            )));
        }
    }
}

/// Decompresses a raw DEFLATE stream (RFC 1951), refusing to produce more than `limit` bytes.
pub fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>, ContentError> {
    let mut bits = Bits {
        data,
        at: 0,
        buffer: 0,
        count: 0,
    };
    let mut out = Vec::new();
    loop {
        let last = bits.take(1)? == 1;
        match bits.take(2)? {
            0 => {
                bits.align();
                let header = data
                    .get(bits.at..bits.at + 4)
                    .ok_or_else(|| corrupt("a stored block ends early"))?;
                let length = usize::from(u16::from_le_bytes([header[0], header[1]]));
                if u16::from_le_bytes([header[0], header[1]])
                    != !u16::from_le_bytes([header[2], header[3]])
                {
                    return Err(corrupt("a stored block's length check fails"));
                }
                bits.at += 4;
                let stored = data
                    .get(bits.at..bits.at + length)
                    .ok_or_else(|| corrupt("a stored block ends early"))?;
                out.extend_from_slice(stored);
                bits.at += length;
                if out.len() > limit {
                    return Err(ContentError(format!(
                        "the content expands past {limit} bytes"
                    )));
                }
            }
            1 => {
                let (literals, distances) = fixed_tables()?;
                codes(&mut bits, &mut out, &literals, &distances, limit)?;
            }
            2 => {
                let (literals, distances) = dynamic_tables(&mut bits)?;
                codes(&mut bits, &mut out, &literals, &distances, limit)?;
            }
            _ => return Err(corrupt("a block has the reserved type 3")),
        }
        if last {
            return Ok(out);
        }
    }
}
