const MIN_MATCH: usize = 4;
const LAST_LITERALS: usize = 5;
const MATCH_FIND_LIMIT: usize = 12;
const MAX_OFFSET: usize = 65_535;
const HASH_BITS: u32 = 16;
const RUN_MASK: usize = 15;
const MOST_EXPANSION: usize = 256;

fn read_u32(input: &[u8], at: usize) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&input[at..at + 4]);
    u32::from_le_bytes(word)
}

fn hash(sequence: u32) -> usize {
    (sequence.wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
}

fn put_length(out: &mut Vec<u8>, mut rest: usize) {
    while rest >= 255 {
        out.push(255);
        rest -= 255;
    }
    out.push(rest as u8);
}

fn put_sequence(out: &mut Vec<u8>, literals: &[u8], match_length: Option<(usize, usize)>) {
    let literal_code = literals.len().min(RUN_MASK);
    let match_code = match_length.map_or(0, |(_, length)| (length - MIN_MATCH).min(RUN_MASK));
    out.push(((literal_code << 4) | match_code) as u8);
    if literals.len() >= RUN_MASK {
        put_length(out, literals.len() - RUN_MASK);
    }
    out.extend_from_slice(literals);
    if let Some((offset, length)) = match_length {
        out.extend_from_slice(&(offset as u16).to_le_bytes());
        if length - MIN_MATCH >= RUN_MASK {
            put_length(out, length - MIN_MATCH - RUN_MASK);
        }
    }
}

/// Compresses `input` into one block of the LZ4 block format, as its public format description
/// lays it out: sequences of a token, literals, a two-byte offset and extra length bytes, the
/// last five bytes always literals.
pub(crate) fn compress(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + input.len() / 255 + 16);
    let length = input.len();
    if length < MATCH_FIND_LIMIT + 1 {
        put_sequence(&mut out, input, None);
        return out;
    }
    let mut table = vec![u32::MAX; 1 << HASH_BITS];
    let match_limit = length - MATCH_FIND_LIMIT;
    let extend_limit = length - LAST_LITERALS;
    let mut anchor = 0;
    let mut at = 0;
    while at < match_limit {
        let sequence = read_u32(input, at);
        let slot = hash(sequence);
        let candidate = table[slot];
        table[slot] = at as u32;
        if candidate != u32::MAX {
            let candidate = candidate as usize;
            if at - candidate <= MAX_OFFSET && read_u32(input, candidate) == sequence {
                let mut matched = MIN_MATCH;
                while at + matched < extend_limit && input[candidate + matched] == input[at + matched] {
                    matched += 1;
                }
                put_sequence(&mut out, &input[anchor..at], Some((at - candidate, matched)));
                at += matched;
                anchor = at;
                continue;
            }
        }
        at += 1;
    }
    put_sequence(&mut out, &input[anchor..], None);
    out
}

fn length_bytes(input: &[u8], at: &mut usize, mut length: usize, most: usize) -> Result<usize, &'static str> {
    loop {
        let byte = *input.get(*at).ok_or("a length runs past the block")?;
        *at += 1;
        length = length.checked_add(usize::from(byte)).ok_or("a length overflows")?;
        if length > most {
            return Err("a length runs past the decompressed size");
        }
        if byte != 255 {
            return Ok(length);
        }
    }
}

/// Decompresses one LZ4 block that must give exactly `expected` bytes. Hostile input is an
/// error: a length or offset that runs past the block or the output, and output of another
/// size.
pub(crate) fn decompress(input: &[u8], expected: usize) -> Result<Vec<u8>, &'static str> {
    if expected > input.len().saturating_mul(MOST_EXPANSION).saturating_add(MATCH_FIND_LIMIT) {
        return Err("the decompressed size is larger than any block of this length can give");
    }
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve_exact(expected)
        .map_err(|_| "the decompressed size cannot be allocated")?;
    let mut at = 0;
    loop {
        let token = usize::from(*input.get(at).ok_or("the block ends before its last sequence")?);
        at += 1;
        let mut literals = token >> 4;
        if literals == RUN_MASK {
            literals = length_bytes(input, &mut at, literals, expected)?;
        }
        let end = at.checked_add(literals).ok_or("literals overflow")?;
        let copied = input.get(at..end).ok_or("literals run past the block")?;
        if out.len() + literals > expected {
            return Err("literals run past the decompressed size");
        }
        out.extend_from_slice(copied);
        at = end;
        if at == input.len() {
            break;
        }
        let offset_bytes = input.get(at..at + 2).ok_or("an offset runs past the block")?;
        let offset = usize::from(u16::from_le_bytes([offset_bytes[0], offset_bytes[1]]));
        at += 2;
        if offset == 0 || offset > out.len() {
            return Err("an offset points before the output");
        }
        let mut matched = (token & RUN_MASK) + MIN_MATCH;
        if token & RUN_MASK == RUN_MASK {
            matched = length_bytes(input, &mut at, matched, expected)?;
        }
        if out.len() + matched > expected {
            return Err("a match runs past the decompressed size");
        }
        let start = out.len() - offset;
        for index in 0..matched {
            let byte = out[start + index];
            out.push(byte);
        }
    }
    if out.len() != expected {
        return Err("the block holds another number of bytes than its header says");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hand_written_blocks_decode_to_their_bytes() {
        assert_eq!(decompress(&[0x00], 0).unwrap(), b"");
        assert_eq!(decompress(&[0x30, b'a', b'b', b'c'], 3).unwrap(), b"abc");
        assert_eq!(
            decompress(&[0x11, b'a', 0x01, 0x00, 0x00], 6).unwrap(),
            b"aaaaaa"
        );
        let mut long_literals = vec![0xF0, 5];
        long_literals.extend(std::iter::repeat_n(b'x', 20));
        assert_eq!(decompress(&long_literals, 20).unwrap(), vec![b'x'; 20]);
        assert_eq!(compress(b""), [0x00]);
        assert_eq!(compress(b"abc"), [0x30, b'a', b'b', b'c']);
    }

    #[test]
    fn every_kind_of_input_round_trips_and_repetition_shrinks() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut noise = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u8
        };
        let inputs: Vec<Vec<u8>> = vec![
            vec![7; 100_000],
            b"the quick brown fox jumps over the lazy dog. ".repeat(500),
            (0..70_000).map(|_| noise()).collect(),
            (0..200_000u32).map(|index| (index % 251) as u8).collect(),
            (0..13).collect(),
            (0..12).collect(),
        ];
        for input in inputs {
            let block = compress(&input);
            assert_eq!(decompress(&block, input.len()).unwrap(), input);
        }
        assert!(compress(&vec![7; 100_000]).len() < 1000);
    }

    #[test]
    fn hostile_blocks_are_errors_never_panics() {
        let cases: [(&[u8], usize); 8] = [
            (&[], 0),
            (&[0x10], 1),
            (&[0x10, b'a', 0x00, 0x00, 0x00], 6),
            (&[0x10, b'a', 0x05, 0x00, 0x00], 6),
            (&[0xF0, 255, 255], 10),
            (&[0x1F, b'a', 0x01, 0x00, 255, 255, 255], 6),
            (&[0x30, b'a', b'b', b'c'], 2),
            (&[0x11, b'a', 0x01], 6),
        ];
        for (block, expected) in cases {
            assert!(decompress(block, expected).is_err(), "{block:?}");
        }
        let mut seed = 1u64;
        for _ in 0..20_000 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let length = (seed >> 58) as usize;
            let block: Vec<u8> = (0..length)
                .map(|index| (seed >> (index % 56)) as u8)
                .collect();
            let _ = decompress(&block, (seed >> 40) as usize % 4096);
        }
    }
}
