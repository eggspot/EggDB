use super::bits::{BackwardBits, Problem, highest_bit};
use super::fse::{DecodeState, decode_table, read_distribution};

const MOST_BITS: u32 = 11;
const MOST_WEIGHT_ACCURACY: u32 = 6;

#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    symbol: u8,
    bits: u8,
}

#[derive(Debug, Clone, Default)]
pub(super) struct HuffmanTable {
    most_bits: u32,
    slots: Vec<Slot>,
}

pub(super) fn read_table(bytes: &[u8]) -> Result<(HuffmanTable, usize), Problem> {
    let header = *bytes.first().ok_or("a Huffman tree description is missing")?;
    let (weights, used) = if header < 128 {
        let size = usize::from(header);
        let stream = bytes
            .get(1..1 + size)
            .ok_or("a Huffman tree's weights run past the block")?;
        (fse_weights(stream)?, 1 + size)
    } else {
        let count = usize::from(header - 127);
        let size = count.div_ceil(2);
        let packed = bytes
            .get(1..1 + size)
            .ok_or("a Huffman tree's weights run past the block")?;
        let mut weights = Vec::with_capacity(count);
        for index in 0..count {
            let byte = packed[index / 2];
            weights.push(if index % 2 == 0 { byte >> 4 } else { byte & 15 });
        }
        (weights, 1 + size)
    };
    Ok((build(&weights)?, used))
}

fn fse_weights(stream: &[u8]) -> Result<Vec<u8>, Problem> {
    let (counts, accuracy, used) = read_distribution(stream, MOST_WEIGHT_ACCURACY, 255)?;
    let table = decode_table(&counts, accuracy)?;
    let body = stream
        .get(used..)
        .filter(|body| !body.is_empty())
        .ok_or("a Huffman tree's weights are missing")?;
    let mut bits = BackwardBits::new(body)?;
    let mut first = DecodeState::new(&table, &mut bits);
    let mut second = DecodeState::new(&table, &mut bits);
    let mut weights = Vec::new();
    loop {
        if weights.len() >= 255 {
            return Err("a Huffman tree has too many weights");
        }
        weights.push(first.symbol());
        first.update(&mut bits);
        if bits.overflowed() {
            weights.push(second.symbol());
            break;
        }
        weights.push(second.symbol());
        second.update(&mut bits);
        if bits.overflowed() {
            weights.push(first.symbol());
            break;
        }
    }
    Ok(weights)
}

fn build(weights: &[u8]) -> Result<HuffmanTable, Problem> {
    if weights.is_empty() || weights.len() > 255 {
        return Err("a Huffman tree has no weights or too many");
    }
    let mut total = 0_u32;
    for &weight in weights {
        if u32::from(weight) > MOST_BITS {
            return Err("a Huffman weight is above the largest code length");
        }
        if weight > 0 {
            total += 1 << (weight - 1);
        }
    }
    if total == 0 {
        return Err("a Huffman tree has only zero weights");
    }
    let most_bits = highest_bit(total) + 1;
    if most_bits > MOST_BITS {
        return Err("a Huffman tree's codes are longer than eleven bits");
    }
    let rest = (1_u32 << most_bits) - total;
    if !rest.is_power_of_two() {
        return Err("a Huffman tree's weights leave no power of two for the last symbol");
    }
    let mut all: Vec<u8> = weights.to_vec();
    all.push((highest_bit(rest) + 1) as u8);
    let mut rank_counts = [0_u32; MOST_BITS as usize + 2];
    for &weight in &all {
        rank_counts[usize::from(weight)] += 1;
    }
    let mut rank_start = [0_u32; MOST_BITS as usize + 2];
    let mut next = 0_u32;
    for weight in 1..=most_bits as usize {
        rank_start[weight] = next;
        next += rank_counts[weight] << (weight - 1);
    }
    let size = 1_usize << most_bits;
    let mut slots = vec![Slot::default(); size];
    for (symbol, &weight) in all.iter().enumerate() {
        if weight == 0 {
            continue;
        }
        let length = 1_u32 << (weight - 1);
        let start = rank_start[usize::from(weight)];
        let bits = (most_bits + 1 - u32::from(weight)) as u8;
        for slot in &mut slots[start as usize..(start + length) as usize] {
            *slot = Slot {
                symbol: symbol as u8,
                bits,
            };
        }
        rank_start[usize::from(weight)] += length;
    }
    Ok(HuffmanTable { most_bits, slots })
}

impl HuffmanTable {
    pub(super) fn decode_stream(
        &self,
        stream: &[u8],
        count: usize,
        out: &mut Vec<u8>,
    ) -> Result<(), Problem> {
        if self.slots.is_empty() {
            return Err("compressed literals have no Huffman tree");
        }
        let mut bits = BackwardBits::new(stream)?;
        for _ in 0..count {
            let slot = self.slots[bits.peek(self.most_bits) as usize];
            bits.skip(u32::from(slot.bits));
            out.push(slot.symbol);
        }
        if !bits.finished_exactly() {
            return Err("a Huffman stream's length does not match its literals");
        }
        Ok(())
    }
}
