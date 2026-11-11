use super::bits::BitWriter;
use super::fse::EncodeTable;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

const MOST_BITS: u8 = 11;
const DIRECT_WEIGHTS_MOST: usize = 128;
const FSE_WEIGHTS_MOST: usize = 254;
const FSE_WEIGHTS_MOST_BYTES: usize = 127;
const WEIGHT_ACCURACY: u32 = 6;
const COMPRESSED_LITERALS: u64 = 2;
const SINGLE_STREAM_MOST: usize = 1023;
const MOST_RESCALES: usize = 64;
const SYMBOLS: usize = 256;

fn huffman_lengths(counts: &[u64; SYMBOLS]) -> Option<[u8; SYMBOLS]> {
    let symbols: Vec<usize> = (0..SYMBOLS).filter(|&symbol| counts[symbol] > 0).collect();
    if symbols.len() < 2 {
        return None;
    }
    let leaves = symbols.len();
    let mut parent = vec![usize::MAX; 2 * leaves - 1];
    let mut heap: BinaryHeap<Reverse<(u64, usize)>> = symbols
        .iter()
        .enumerate()
        .map(|(node, &symbol)| Reverse((counts[symbol], node)))
        .collect();
    let mut next = leaves;
    while heap.len() > 1 {
        let Reverse((first_weight, first)) = heap.pop()?;
        let Reverse((second_weight, second)) = heap.pop()?;
        parent[first] = next;
        parent[second] = next;
        heap.push(Reverse((first_weight + second_weight, next)));
        next += 1;
    }
    let root = next - 1;
    let mut lengths = [0_u8; SYMBOLS];
    for (leaf, &symbol) in symbols.iter().enumerate() {
        let mut depth = 0_u8;
        let mut node = leaf;
        while node != root {
            node = parent[node];
            depth = depth.saturating_add(1);
        }
        lengths[symbol] = depth;
    }
    Some(lengths)
}

fn limited_lengths(counts: &[u64; SYMBOLS]) -> Option<[u8; SYMBOLS]> {
    let mut scaled = *counts;
    for _ in 0..MOST_RESCALES {
        let lengths = huffman_lengths(&scaled)?;
        if lengths.iter().all(|&length| length <= MOST_BITS) {
            return Some(lengths);
        }
        for count in &mut scaled {
            if *count > 0 {
                *count = (*count / 2).max(1);
            }
        }
    }
    None
}

struct PlainBits {
    bytes: Vec<u8>,
    pending: u64,
    pending_bits: u32,
}

impl PlainBits {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            pending: 0,
            pending_bits: 0,
        }
    }

    fn add(&mut self, value: u64, count: u32) {
        if count == 0 {
            return;
        }
        self.pending |= (value & ((1_u64 << count) - 1)) << self.pending_bits;
        self.pending_bits += count;
        while self.pending_bits >= 8 {
            self.bytes.push(self.pending as u8);
            self.pending >>= 8;
            self.pending_bits -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.pending_bits > 0 {
            self.bytes.push(self.pending as u8);
        }
        self.bytes
    }
}

fn normalized_weights(weights: &[u8]) -> Option<Vec<i16>> {
    let mut histogram = [0_u64; MOST_BITS as usize + 1];
    for &weight in weights {
        histogram[usize::from(weight)] += 1;
    }
    if histogram.iter().filter(|&&count| count > 0).count() < 2 {
        return None;
    }
    let last = histogram.iter().rposition(|&count| count > 0)?;
    let size = 1_i64 << WEIGHT_ACCURACY;
    let total = weights.len() as u64;
    let mut counts: Vec<i16> = histogram[..=last]
        .iter()
        .map(|&count| {
            if count == 0 {
                0
            } else {
                (count * size as u64 / total).max(1) as i16
            }
        })
        .collect();
    let mut sum: i64 = counts.iter().map(|&count| i64::from(count)).sum();
    while sum != size {
        let largest = (0..counts.len()).max_by_key(|&symbol| counts[symbol])?;
        if sum < size {
            counts[largest] += (size - sum) as i16;
            sum = size;
        } else {
            if counts[largest] <= 1 {
                return None;
            }
            let taken = (sum - size).min(i64::from(counts[largest] - 1));
            counts[largest] -= taken as i16;
            sum -= taken;
        }
    }
    Some(counts)
}

fn distribution(counts: &[i16], accuracy: u32) -> Vec<u8> {
    let mut bits = PlainBits::new();
    bits.add(u64::from(accuracy - 5), 4);
    let mut remaining: i32 = (1 << accuracy) + 1;
    let mut threshold: i32 = 1 << accuracy;
    let mut width = accuracy + 1;
    let mut previous_zero = false;
    let mut symbol = 0;
    while remaining > 1 && symbol < counts.len() {
        if previous_zero {
            let mut run_start = symbol;
            while symbol < counts.len() && counts[symbol] == 0 {
                symbol += 1;
            }
            while symbol >= run_start + 24 {
                run_start += 24;
                bits.add(0xFFFF, 16);
            }
            while symbol >= run_start + 3 {
                run_start += 3;
                bits.add(3, 2);
            }
            bits.add((symbol - run_start) as u64, 2);
            if symbol >= counts.len() {
                break;
            }
        }
        let count = i32::from(counts[symbol]);
        let most = 2 * threshold - 1 - remaining;
        remaining -= count.abs();
        let mut value = count + 1;
        if value >= threshold {
            value += most;
        }
        let written = if value < most { width - 1 } else { width };
        bits.add(value as u64, written);
        previous_zero = count == 0;
        while remaining < threshold && threshold > 1 {
            width -= 1;
            threshold >>= 1;
        }
        symbol += 1;
    }
    bits.finish()
}

fn fse_tree(weights: &[u8]) -> Option<Vec<u8>> {
    let count = weights.len();
    if count <= 2 {
        return None;
    }
    let counts = normalized_weights(weights)?;
    let table = EncodeTable::new(&counts, WEIGHT_ACCURACY);
    let mut bits = BitWriter::default();
    let (mut first, mut second, mut index) = if count % 2 == 1 {
        let mut first = table.start(weights[count - 1]);
        let second = table.start(weights[count - 2]);
        table.encode(&mut first, weights[count - 3], &mut bits);
        (first, second, count - 3)
    } else {
        let second = table.start(weights[count - 1]);
        let first = table.start(weights[count - 2]);
        (first, second, count - 2)
    };
    while index >= 2 {
        table.encode(&mut second, weights[index - 1], &mut bits);
        table.encode(&mut first, weights[index - 2], &mut bits);
        index -= 2;
    }
    table.flush(second, &mut bits);
    table.flush(first, &mut bits);
    let header = distribution(&counts, WEIGHT_ACCURACY);
    let stream = bits.close();
    let size = header.len() + stream.len();
    if size > FSE_WEIGHTS_MOST_BYTES {
        return None;
    }
    let mut tree = Vec::with_capacity(1 + size);
    tree.push(size as u8);
    tree.extend_from_slice(&header);
    tree.extend_from_slice(&stream);
    Some(tree)
}

fn direct_tree(weights: &[u8]) -> Option<Vec<u8>> {
    if weights.len() > DIRECT_WEIGHTS_MOST {
        return None;
    }
    let mut tree = Vec::with_capacity(1 + weights.len().div_ceil(2));
    tree.push(u8::try_from(127 + weights.len()).ok()?);
    for pair in weights.chunks(2) {
        tree.push((pair[0] << 4) | pair.get(1).copied().unwrap_or(0));
    }
    Some(tree)
}

struct Code {
    weights: [u8; SYMBOLS],
    codes: [u16; SYMBOLS],
    lengths: [u8; SYMBOLS],
}

fn canonical(lengths: &[u8; SYMBOLS]) -> Code {
    let longest = lengths.iter().copied().max().unwrap_or(0);
    let mut weights = [0_u8; SYMBOLS];
    for (weight, &length) in weights.iter_mut().zip(lengths) {
        if length > 0 {
            *weight = longest + 1 - length;
        }
    }
    let mut rank_counts = [0_u32; MOST_BITS as usize + 2];
    for &weight in &weights {
        rank_counts[usize::from(weight)] += 1;
    }
    let mut rank_start = [0_u32; MOST_BITS as usize + 2];
    let mut next = 0_u32;
    for weight in 1..=usize::from(longest) {
        rank_start[weight] = next;
        next += rank_counts[weight] << (weight - 1);
    }
    let mut codes = [0_u16; SYMBOLS];
    for (symbol, &weight) in weights.iter().enumerate() {
        if weight == 0 {
            continue;
        }
        let weight = usize::from(weight);
        codes[symbol] = (rank_start[weight] >> (weight - 1)) as u16;
        rank_start[weight] += 1 << (weight - 1);
    }
    Code {
        weights,
        codes,
        lengths: *lengths,
    }
}

fn stream(literals: &[u8], code: &Code) -> Vec<u8> {
    let mut bits = BitWriter::default();
    for &literal in literals.iter().rev() {
        let symbol = usize::from(literal);
        bits.add(u64::from(code.codes[symbol]), u32::from(code.lengths[symbol]));
    }
    bits.close()
}

fn four_streams(literals: &[u8], code: &Code) -> Option<Vec<u8>> {
    let share = literals.len().div_ceil(4);
    if literals.len() < 3 * share {
        return None;
    }
    let parts = [
        &literals[..share],
        &literals[share..2 * share],
        &literals[2 * share..3 * share],
        &literals[3 * share..],
    ];
    let streams: Vec<Vec<u8>> = parts.iter().map(|part| stream(part, code)).collect();
    let mut out = Vec::with_capacity(6 + streams.iter().map(Vec::len).sum::<usize>());
    for written in &streams[..3] {
        out.extend_from_slice(&u16::try_from(written.len()).ok()?.to_le_bytes());
    }
    for written in &streams {
        out.extend_from_slice(written);
    }
    Some(out)
}

/// The literals section of RFC 8878 for `literals` with a Huffman tree of their own, its weights
/// written directly or FSE-compressed (whichever is shorter), in one stream or four; `None` when
/// the literals have fewer than two distinct bytes, use byte 255, or need a size the section
/// cannot describe.
pub(super) fn compressed_literals(literals: &[u8]) -> Option<Vec<u8>> {
    let mut counts = [0_u64; SYMBOLS];
    for &literal in literals {
        counts[usize::from(literal)] += 1;
    }
    let last = (0..SYMBOLS).rev().find(|&symbol| counts[symbol] > 0)?;
    if last == 0 || last > FSE_WEIGHTS_MOST {
        return None;
    }
    let code = canonical(&limited_lengths(&counts)?);
    let transmitted = &code.weights[..last];
    let tree = match (direct_tree(transmitted), fse_tree(transmitted)) {
        (Some(direct), Some(compressed)) if compressed.len() < direct.len() => compressed,
        (Some(direct), _) => direct,
        (None, Some(compressed)) => compressed,
        (None, None) => return None,
    };
    let regenerated = literals.len();
    let single = (regenerated <= SINGLE_STREAM_MOST)
        .then(|| stream(literals, &code))
        .filter(|written| tree.len() + written.len() <= SINGLE_STREAM_MOST);
    let (streams, size_format, width, header_bytes) = match single {
        Some(written) => (written, 0_u64, 10_u32, 3_usize),
        None => {
            let written = four_streams(literals, &code)?;
            let biggest = regenerated.max(tree.len() + written.len());
            let (size_format, width, header_bytes) = if biggest <= SINGLE_STREAM_MOST {
                (1, 10, 3)
            } else if biggest < 1 << 14 {
                (2, 14, 4)
            } else if biggest < 1 << 18 {
                (3, 18, 5)
            } else {
                return None;
            };
            (written, size_format, width, header_bytes)
        }
    };
    let compressed = tree.len() + streams.len();
    let header = COMPRESSED_LITERALS
        | (size_format << 2)
        | ((regenerated as u64) << 4)
        | ((compressed as u64) << (4 + width));
    let mut section = Vec::with_capacity(header_bytes + compressed);
    section.extend_from_slice(&header.to_le_bytes()[..header_bytes]);
    section.extend_from_slice(&tree);
    section.extend_from_slice(&streams);
    Some(section)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_stay_within_eleven_bits_and_form_a_complete_code() {
        let mut counts = [0_u64; SYMBOLS];
        let mut fibonacci = (1_u64, 1_u64);
        for count in counts.iter_mut().take(30) {
            *count = fibonacci.0;
            fibonacci = (fibonacci.1, fibonacci.0 + fibonacci.1);
        }
        let lengths = limited_lengths(&counts).expect("lengths");
        assert!(lengths.iter().all(|&length| length <= MOST_BITS));
        let kraft: u64 = lengths
            .iter()
            .filter(|&&length| length > 0)
            .map(|&length| 1_u64 << (u32::from(MOST_BITS) - u32::from(length)))
            .sum();
        assert_eq!(kraft, 1 << MOST_BITS);
    }

    #[test]
    fn weight_distributions_read_back_as_written() {
        let weights: Vec<u8> = (0..200_u32).map(|at| [0, 0, 1, 2, 2, 3, 0, 5, 1][at as usize % 9]).collect();
        let counts = normalized_weights(&weights).expect("normalized");
        assert_eq!(counts.iter().map(|&count| i64::from(count)).sum::<i64>(), 64);
        let written = distribution(&counts, WEIGHT_ACCURACY);
        let (read, accuracy, used) =
            super::super::fse::read_distribution(&written, WEIGHT_ACCURACY, 255).expect("read");
        assert_eq!(accuracy, WEIGHT_ACCURACY);
        assert_eq!(used, written.len());
        assert_eq!(read, counts);
    }

    #[test]
    fn fse_compressed_weights_read_back_as_a_huffman_tree() {
        for used_symbols in [150, 151] {
            let mut counts = [0_u64; SYMBOLS];
            for (symbol, count) in counts.iter_mut().enumerate().take(used_symbols) {
                *count = 1 + (symbol as u64 * 7_919) % 97;
            }
            let code = canonical(&limited_lengths(&counts).expect("lengths"));
            let transmitted = &code.weights[..used_symbols - 1];
            let tree = fse_tree(transmitted).expect("an FSE tree");
            assert!(tree[0] < 128);
            let (_, used) = super::super::huffman::read_table(&tree)
                .unwrap_or_else(|problem| panic!("{problem}"));
            assert_eq!(used, tree.len());
        }
    }

    #[test]
    fn canonical_codes_follow_the_rfc_example() {
        let mut lengths = [0_u8; SYMBOLS];
        lengths[..6].copy_from_slice(&[1, 2, 3, 0, 4, 4]);
        let code = canonical(&lengths);
        assert_eq!(&code.weights[..6], &[4, 3, 2, 0, 1, 1]);
        assert_eq!(&code.codes[..6], &[0b1, 0b01, 0b001, 0, 0b0000, 0b0001]);
    }
}
