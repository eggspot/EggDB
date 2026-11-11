use super::bits::BitWriter;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

const MOST_BITS: u8 = 11;
const DIRECT_WEIGHTS_MOST: usize = 128;
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
/// written directly, in one stream or four; `None` when the literals have fewer than two
/// distinct bytes, a byte above 128 (whose weights would need FSE compression), or a size the
/// section cannot describe.
pub(super) fn compressed_literals(literals: &[u8]) -> Option<Vec<u8>> {
    let mut counts = [0_u64; SYMBOLS];
    for &literal in literals {
        counts[usize::from(literal)] += 1;
    }
    let last = (0..SYMBOLS).rev().find(|&symbol| counts[symbol] > 0)?;
    if last == 0 || last > DIRECT_WEIGHTS_MOST {
        return None;
    }
    let code = canonical(&limited_lengths(&counts)?);
    let mut tree = Vec::with_capacity(1 + last.div_ceil(2));
    tree.push(u8::try_from(127 + last).ok()?);
    for pair in code.weights[..last].chunks(2) {
        tree.push((pair[0] << 4) | pair.get(1).copied().unwrap_or(0));
    }
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
    fn canonical_codes_follow_the_rfc_example() {
        let mut lengths = [0_u8; SYMBOLS];
        lengths[..6].copy_from_slice(&[1, 2, 3, 0, 4, 4]);
        let code = canonical(&lengths);
        assert_eq!(&code.weights[..6], &[4, 3, 2, 0, 1, 1]);
        assert_eq!(&code.codes[..6], &[0b1, 0b01, 0b001, 0, 0b0000, 0b0001]);
    }
}
