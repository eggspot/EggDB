const WINDOW: usize = 32 * 1024;
const HASH_BITS: u32 = 15;
const MOST_CHAIN: usize = 64;
const SHORTEST_MATCH: usize = 3;
const LONGEST_MATCH: usize = 258;
const END_OF_BLOCK: u16 = 256;
const NO_POSITION: u32 = u32::MAX;

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145,
    8193, 12289, 16385, 24577,
];
const DISTANCE_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

struct Bits {
    out: Vec<u8>,
    held: u64,
    count: u32,
}

impl Bits {
    fn put(&mut self, value: u32, count: u32) {
        self.held |= u64::from(value) << self.count;
        self.count += count;
        while self.count >= 8 {
            self.out.push(self.held as u8);
            self.held >>= 8;
            self.count -= 8;
        }
    }

    fn put_code(&mut self, code: u32, length: u32) {
        let reversed = code.reverse_bits() >> (32 - length);
        self.put(reversed, length);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.count > 0 {
            self.out.push(self.held as u8);
        }
        self.out
    }
}

fn literal(bits: &mut Bits, symbol: u16) {
    let symbol = u32::from(symbol);
    match symbol {
        0..=143 => bits.put_code(0x30 + symbol, 8),
        144..=255 => bits.put_code(0x190 + symbol - 144, 9),
        256..=279 => bits.put_code(symbol - 256, 7),
        _ => bits.put_code(0xC0 + symbol - 280, 8),
    }
}

fn copy(bits: &mut Bits, length: usize, distance: usize) {
    let length_index = LENGTH_BASE.iter().rposition(|base| usize::from(*base) <= length).unwrap_or(0);
    literal(bits, 257 + length_index as u16);
    bits.put((length - usize::from(LENGTH_BASE[length_index])) as u32, u32::from(LENGTH_EXTRA[length_index]));
    let distance_index = DISTANCE_BASE.iter().rposition(|base| usize::from(*base) <= distance).unwrap_or(0);
    bits.put_code(distance_index as u32, 5);
    bits.put((distance - usize::from(DISTANCE_BASE[distance_index])) as u32, u32::from(DISTANCE_EXTRA[distance_index]));
}

fn hash(input: &[u8], at: usize) -> usize {
    let value = u32::from(input[at]) << 16 | u32::from(input[at + 1]) << 8 | u32::from(input[at + 2]);
    (value.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
}

/// `input` as one raw DEFLATE block (RFC 1951) with the fixed Huffman codes and greedy LZ77
/// matches over a 32 KiB window.
pub fn deflate(input: &[u8]) -> Vec<u8> {
    let mut bits = Bits { out: Vec::with_capacity(input.len() / 2 + 16), held: 0, count: 0 };
    bits.put(1, 1);
    bits.put(1, 2);
    let mut head = vec![NO_POSITION; 1 << HASH_BITS];
    let mut previous = vec![NO_POSITION; WINDOW];
    let mut at = 0;
    let insert = |head: &mut Vec<u32>, previous: &mut Vec<u32>, position: usize| {
        if position + SHORTEST_MATCH <= input.len() {
            let slot = hash(input, position);
            previous[position % WINDOW] = head[slot];
            head[slot] = position as u32;
        }
    };
    while at < input.len() {
        let mut best = (0usize, 0usize);
        if at + SHORTEST_MATCH <= input.len() {
            let mut candidate = head[hash(input, at)];
            let mut steps = 0;
            while candidate != NO_POSITION && steps < MOST_CHAIN {
                let start = candidate as usize;
                if start >= at || at - start > WINDOW {
                    break;
                }
                let most = LONGEST_MATCH.min(input.len() - at);
                let length = (0..most).take_while(|offset| input[start + offset] == input[at + offset]).count();
                if length > best.0 {
                    best = (length, at - start);
                    if length == most {
                        break;
                    }
                }
                candidate = previous[start % WINDOW];
                steps += 1;
            }
        }
        if best.0 >= SHORTEST_MATCH {
            copy(&mut bits, best.0, best.1);
            for position in at..at + best.0 {
                insert(&mut head, &mut previous, position);
            }
            at += best.0;
        } else {
            literal(&mut bits, u16::from(input[at]));
            insert(&mut head, &mut previous, at);
            at += 1;
        }
    }
    literal(&mut bits, END_OF_BLOCK);
    bits.finish()
}

/// `input` as a gzip member (RFC 1952): a header with no name or time, the DEFLATE data, then
/// the CRC-32 and length.
pub fn gzip(input: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1F, 0x8B, 8, 0, 0, 0, 0, 0, 0, 0xFF];
    out.extend_from_slice(&deflate(input));
    out.extend_from_slice(&crate::zip::crc32(input).to_le_bytes());
    out.extend_from_slice(&(input.len() as u32).to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gzip_output_inflates_back_to_the_input() {
        let text = "the quick brown fox jumps over the lazy dog; ".repeat(50);
        for input in [Vec::new(), b"a".to_vec(), text.into_bytes(), (0..=255u8).cycle().take(70_000).collect()] {
            let packed = gzip(&input);
            assert_eq!(crate::gunzip(&packed, input.len() + 1).unwrap(), input);
        }
    }
}
