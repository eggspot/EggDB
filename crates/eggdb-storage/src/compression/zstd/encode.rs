use super::bits::{BitWriter, highest_bit};
use super::fse::EncodeTable;
use super::tables::{
    LITERAL_LENGTH_ACCURACY, LITERAL_LENGTH_BITS, LITERAL_LENGTH_COUNTS, LITERAL_LENGTH_START,
    MATCH_LENGTH_ACCURACY, MATCH_LENGTH_BITS, MATCH_LENGTH_COUNTS, MATCH_LENGTH_START,
    OFFSET_ACCURACY, OFFSET_COUNTS, literal_length_code, match_length_code,
};
use super::{BLOCK_MAXIMUM, MAGIC};

const MIN_MATCH: usize = 4;
const HASH_BITS: u32 = 17;
const LAST_BLOCK: u32 = 1;
const RAW_BLOCK: u32 = 0;
const RLE_BLOCK: u32 = 1 << 1;
const COMPRESSED_BLOCK: u32 = 2 << 1;
const LONGEST_MATCH: usize = 65_539 + 65_535;
const LARGEST_OFFSET: usize = (1 << 29) - 4;

struct Sequence {
    literal_length: usize,
    match_length: usize,
    offset: usize,
}

fn read_u32(input: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([input[at], input[at + 1], input[at + 2], input[at + 3]])
}

fn hash(word: u32) -> usize {
    (word.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
}

pub(super) fn compress(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() / 2 + 32);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    let size = input.len() as u64;
    let (flag, width) = if size < 256 {
        (0_u8, 1)
    } else if size < 65_536 + 256 {
        (1, 2)
    } else if size <= u64::from(u32::MAX) {
        (2, 4)
    } else {
        (3, 8)
    };
    out.push((flag << 6) | 0x20 | 0x04);
    let stored = if width == 2 { size - 256 } else { size };
    out.extend_from_slice(&stored.to_le_bytes()[..width]);
    let mut table = vec![u32::MAX; 1 << HASH_BITS];
    let mut start = 0;
    if input.is_empty() {
        block_header(&mut out, LAST_BLOCK | RAW_BLOCK, 0);
    }
    while start < input.len() {
        let end = (start + BLOCK_MAXIMUM).min(input.len());
        let last = if end == input.len() { LAST_BLOCK } else { 0 };
        write_block(&mut out, input, start, end, last, &mut table);
        start = end;
    }
    out.extend_from_slice(&(super::xxh64::hash(input) as u32).to_le_bytes());
    out
}

fn block_header(out: &mut Vec<u8>, flags: u32, size: usize) {
    let header = flags | ((size as u32) << 3);
    out.extend_from_slice(&header.to_le_bytes()[..3]);
}

fn write_block(
    out: &mut Vec<u8>,
    input: &[u8],
    start: usize,
    end: usize,
    last: u32,
    table: &mut [u32],
) {
    let block = &input[start..end];
    if block.iter().all(|&byte| byte == block[0]) {
        block_header(out, last | RLE_BLOCK, block.len());
        out.push(block[0]);
        return;
    }
    let (sequences, literals) = find_sequences(input, start, end, table);
    let body = compressed_body(&sequences, &literals);
    if body.len() >= block.len() {
        block_header(out, last | RAW_BLOCK, block.len());
        out.extend_from_slice(block);
    } else {
        block_header(out, last | COMPRESSED_BLOCK, body.len());
        out.extend_from_slice(&body);
    }
}

fn find_sequences(
    input: &[u8],
    start: usize,
    end: usize,
    table: &mut [u32],
) -> (Vec<Sequence>, Vec<u8>) {
    let mut sequences = Vec::new();
    let mut literals = Vec::new();
    let mut anchor = start;
    let mut at = start;
    while at + MIN_MATCH <= end {
        let word = read_u32(input, at);
        let slot = hash(word);
        let candidate = table[slot];
        table[slot] = at as u32;
        let usable = candidate != u32::MAX
            && (candidate as usize) < at
            && at - candidate as usize <= LARGEST_OFFSET
            && read_u32(input, candidate as usize) == word;
        if !usable {
            at += 1;
            continue;
        }
        let from = candidate as usize;
        let mut length = MIN_MATCH;
        while at + length < end && length < LONGEST_MATCH && input[from + length] == input[at + length]
        {
            length += 1;
        }
        literals.extend_from_slice(&input[anchor..at]);
        sequences.push(Sequence {
            literal_length: at - anchor,
            match_length: length,
            offset: at - from,
        });
        at += length;
        anchor = at;
    }
    literals.extend_from_slice(&input[anchor..end]);
    (sequences, literals)
}

fn literals_header(out: &mut Vec<u8>, size: usize) {
    if size < 32 {
        out.push((size as u8) << 3);
    } else if size < 4096 {
        let value = (1_u32 << 2) | ((size as u32) << 4);
        out.extend_from_slice(&value.to_le_bytes()[..2]);
    } else {
        let value = (3_u32 << 2) | ((size as u32) << 4);
        out.extend_from_slice(&value.to_le_bytes()[..3]);
    }
}

fn compressed_body(sequences: &[Sequence], literals: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(literals.len() + sequences.len() * 4 + 16);
    literals_header(&mut body, literals.len());
    body.extend_from_slice(literals);
    if let Some(section) = super::encode_huffman::compressed_literals(literals)
        && section.len() < body.len()
    {
        body = section;
    }
    let count = sequences.len();
    if count < 128 {
        body.push(count as u8);
    } else if count < 0x7F00 {
        body.push(((count >> 8) + 128) as u8);
        body.push(count as u8);
    } else {
        body.push(255);
        body.extend_from_slice(&((count - 0x7F00) as u16).to_le_bytes());
    }
    if count == 0 {
        return body;
    }
    body.push(0);
    let literal_table = EncodeTable::new(&LITERAL_LENGTH_COUNTS, LITERAL_LENGTH_ACCURACY);
    let match_table = EncodeTable::new(&MATCH_LENGTH_COUNTS, MATCH_LENGTH_ACCURACY);
    let offset_table = EncodeTable::new(&OFFSET_COUNTS, OFFSET_ACCURACY);
    let codes: Vec<(u8, u8, u8)> = sequences
        .iter()
        .map(|sequence| {
            (
                literal_length_code(sequence.literal_length),
                match_length_code(sequence.match_length),
                highest_bit((sequence.offset + 3) as u32) as u8,
            )
        })
        .collect();
    let mut bits = BitWriter::default();
    let (last_literal, last_match, last_offset) = codes[count - 1];
    let mut match_state = match_table.start(last_match);
    let mut offset_state = offset_table.start(last_offset);
    let mut literal_state = literal_table.start(last_literal);
    extra_bits(&mut bits, &sequences[count - 1], codes[count - 1]);
    for index in (0..count - 1).rev() {
        let (literal, matched, offset) = codes[index];
        offset_table.encode(&mut offset_state, offset, &mut bits);
        match_table.encode(&mut match_state, matched, &mut bits);
        literal_table.encode(&mut literal_state, literal, &mut bits);
        extra_bits(&mut bits, &sequences[index], codes[index]);
    }
    match_table.flush(match_state, &mut bits);
    offset_table.flush(offset_state, &mut bits);
    literal_table.flush(literal_state, &mut bits);
    body.extend_from_slice(&bits.close());
    body
}

fn extra_bits(bits: &mut BitWriter, sequence: &Sequence, codes: (u8, u8, u8)) {
    let (literal, matched, offset) = codes;
    let literal_bits = u32::from(LITERAL_LENGTH_BITS[usize::from(literal)]);
    let literal_extra = sequence.literal_length as u64
        - u64::from(LITERAL_LENGTH_START[usize::from(literal)]);
    bits.add(literal_extra, literal_bits);
    let match_bits = u32::from(MATCH_LENGTH_BITS[usize::from(matched)]);
    let match_extra =
        sequence.match_length as u64 - u64::from(MATCH_LENGTH_START[usize::from(matched)]);
    bits.add(match_extra, match_bits);
    let offset_value = (sequence.offset + 3) as u64;
    bits.add(offset_value - (1 << offset), u32::from(offset));
}
