use super::bits::{BackwardBits, Problem};
use super::fse::{DecodeState, DecodeTable, decode_table, read_distribution, rle_table};
use super::huffman::{HuffmanTable, read_table};
use super::tables::{
    LITERAL_LENGTH_ACCURACY, LITERAL_LENGTH_BITS, LITERAL_LENGTH_COUNTS, LITERAL_LENGTH_START,
    MATCH_LENGTH_ACCURACY, MATCH_LENGTH_BITS, MATCH_LENGTH_COUNTS, MATCH_LENGTH_START,
    OFFSET_ACCURACY, OFFSET_COUNTS,
};
use super::{BLOCK_MAXIMUM, MAGIC};

const SKIPPABLE_MAGIC_LOW: u32 = 0x184D_2A50;
const SKIPPABLE_MAGIC_MASK: u32 = 0xFFFF_FFF0;
const LARGEST_LITERAL_LENGTH_CODE: usize = 35;
const LARGEST_MATCH_LENGTH_CODE: usize = 52;
const LARGEST_OFFSET_CODE: usize = 31;
const MOST_LENGTH_ACCURACY: u32 = 9;
const MOST_OFFSET_ACCURACY: u32 = 8;

struct FrameState {
    huffman: Option<HuffmanTable>,
    literal_lengths: Option<DecodeTable>,
    offsets: Option<DecodeTable>,
    match_lengths: Option<DecodeTable>,
    repeats: [usize; 3],
}

impl FrameState {
    fn new() -> Self {
        Self {
            huffman: None,
            literal_lengths: None,
            offsets: None,
            match_lengths: None,
            repeats: [1, 4, 8],
        }
    }
}

fn byte(input: &[u8], at: usize) -> Result<u8, Problem> {
    input
        .get(at)
        .copied()
        .ok_or("a Zstandard frame ends early")
}

fn little_endian(input: &[u8], at: usize, width: usize) -> Result<u64, Problem> {
    let bytes = input
        .get(at..at + width)
        .ok_or("a Zstandard frame ends early")?;
    Ok(bytes
        .iter()
        .rev()
        .fold(0_u64, |value, &byte| (value << 8) | u64::from(byte)))
}

pub(super) fn decompress(input: &[u8], expected: usize) -> Result<Vec<u8>, Problem> {
    let mut out = Vec::new();
    out.try_reserve_exact(expected)
        .map_err(|_| "no memory for a decompressed value")?;
    let mut at = 0;
    while at < input.len() {
        let magic = little_endian(input, at, 4)? as u32;
        if magic & SKIPPABLE_MAGIC_MASK == SKIPPABLE_MAGIC_LOW {
            let size = little_endian(input, at + 4, 4)? as usize;
            at = at
                .checked_add(8 + size)
                .filter(|&end| end <= input.len())
                .ok_or("a skippable frame runs past its input")?;
            continue;
        }
        if magic != MAGIC {
            return Err("a Zstandard frame has the wrong magic number");
        }
        at = frame(input, at + 4, &mut out, expected)?;
    }
    if out.len() != expected {
        return Err("a Zstandard value decompresses to the wrong length");
    }
    Ok(out)
}

fn frame(input: &[u8], mut at: usize, out: &mut Vec<u8>, expected: usize) -> Result<usize, Problem> {
    let frame_start = out.len();
    let descriptor = byte(input, at)?;
    at += 1;
    let content_size_flag = descriptor >> 6;
    let single_segment = descriptor & 0x20 != 0;
    if descriptor & 0x08 != 0 {
        return Err("a Zstandard frame sets its reserved bit");
    }
    let checksum = descriptor & 0x04 != 0;
    let dictionary_bytes = [0, 1, 2, 4][usize::from(descriptor & 3)];
    let window = if single_segment {
        None
    } else {
        let window_byte = byte(input, at)?;
        at += 1;
        let exponent = u32::from(window_byte >> 3);
        let base = 1_u64 << (10 + exponent);
        Some(base + (base / 8) * u64::from(window_byte & 7))
    };
    if dictionary_bytes > 0 {
        let dictionary = little_endian(input, at, dictionary_bytes)?;
        if dictionary != 0 {
            return Err("Zstandard dictionaries are not supported");
        }
        at += dictionary_bytes;
    }
    let content_bytes = match (content_size_flag, single_segment) {
        (0, false) => 0,
        (0, true) => 1,
        (1, _) => 2,
        (2, _) => 4,
        _ => 8,
    };
    let content_size = if content_bytes > 0 {
        let value = little_endian(input, at, content_bytes)?;
        at += content_bytes;
        Some(if content_bytes == 2 { value + 256 } else { value })
    } else {
        None
    };
    if let Some(size) = content_size
        && size > (expected - frame_start) as u64
    {
        return Err("a Zstandard frame is larger than its value");
    }
    let window = window.or(content_size).unwrap_or(0);
    let block_maximum = (window.min(BLOCK_MAXIMUM as u64) as usize).max(1);
    let mut state = FrameState::new();
    loop {
        let header = little_endian(input, at, 3)? as u32;
        at += 3;
        let last = header & 1 == 1;
        let kind = (header >> 1) & 3;
        let size = (header >> 3) as usize;
        let room = expected - out.len();
        match kind {
            0 => {
                if size > block_maximum || size > room {
                    return Err("a raw block is larger than allowed");
                }
                let data = input
                    .get(at..at + size)
                    .ok_or("a raw block runs past its frame")?;
                out.extend_from_slice(data);
                at += size;
            }
            1 => {
                if size > block_maximum || size > room {
                    return Err("an RLE block is larger than allowed");
                }
                let value = byte(input, at)?;
                out.resize(out.len() + size, value);
                at += 1;
            }
            2 => {
                if size > block_maximum {
                    return Err("a compressed block is larger than allowed");
                }
                let data = input
                    .get(at..at + size)
                    .ok_or("a compressed block runs past its frame")?;
                let before = out.len();
                compressed_block(data, out, &mut state, frame_start, room.min(block_maximum))?;
                if out.len() - before > block_maximum {
                    return Err("a compressed block decompresses to more than allowed");
                }
                at += size;
            }
            _ => return Err("a Zstandard block has the reserved type"),
        }
        if last {
            break;
        }
    }
    if let Some(size) = content_size
        && (out.len() - frame_start) as u64 != size
    {
        return Err("a Zstandard frame's content is not its stated size");
    }
    if checksum {
        let stored = little_endian(input, at, 4)? as u32;
        at += 4;
        if super::xxh64::hash(&out[frame_start..]) as u32 != stored {
            return Err("a Zstandard frame's checksum does not match");
        }
    }
    Ok(at)
}

fn compressed_block(
    data: &[u8],
    out: &mut Vec<u8>,
    state: &mut FrameState,
    frame_start: usize,
    room: usize,
) -> Result<(), Problem> {
    let (literals, used) = literals_section(data, state, room)?;
    let rest = data.get(used..).ok_or("a block's literals run past it")?;
    sequences_section(rest, &literals, out, state, frame_start, room)
}

fn literals_section(
    data: &[u8],
    state: &mut FrameState,
    room: usize,
) -> Result<(Vec<u8>, usize), Problem> {
    let first = byte(data, 0)?;
    let kind = first & 3;
    let size_format = (first >> 2) & 3;
    match kind {
        0 | 1 => {
            let (regenerated, header) = match size_format {
                0 | 2 => (usize::from(first >> 3), 1),
                1 => (
                    (usize::from(first) >> 4) + (usize::from(byte(data, 1)?) << 4),
                    2,
                ),
                _ => (
                    (usize::from(first) >> 4)
                        + (usize::from(byte(data, 1)?) << 4)
                        + (usize::from(byte(data, 2)?) << 12),
                    3,
                ),
            };
            if regenerated > room {
                return Err("a block has more literals than room");
            }
            if kind == 0 {
                let bytes = data
                    .get(header..header + regenerated)
                    .ok_or("raw literals run past their block")?;
                Ok((bytes.to_vec(), header + regenerated))
            } else {
                Ok((vec![byte(data, header)?; regenerated], header + 1))
            }
        }
        _ => {
            let (streams, width, header) = match size_format {
                0 => (1, 10, 3),
                1 => (4, 10, 3),
                2 => (4, 14, 4),
                _ => (4, 18, 5),
            };
            let bits = little_endian(data, 0, header)?;
            let mask = (1_u64 << width) - 1;
            let regenerated = ((bits >> 4) & mask) as usize;
            let compressed = ((bits >> (4 + width)) & mask) as usize;
            if regenerated > room {
                return Err("a block has more literals than room");
            }
            let body = data
                .get(header..header + compressed)
                .ok_or("compressed literals run past their block")?;
            let mut used_by_tree = 0;
            if kind == 2 {
                let (table, used) = read_table(body)?;
                state.huffman = Some(table);
                used_by_tree = used;
            }
            let table = state
                .huffman
                .as_ref()
                .ok_or("treeless literals come before any Huffman tree")?;
            let payload = body
                .get(used_by_tree..)
                .ok_or("a Huffman tree runs past its literals")?;
            let mut literals = Vec::with_capacity(regenerated);
            if streams == 1 {
                table.decode_stream(payload, regenerated, &mut literals)?;
            } else {
                let jump = payload.get(..6).ok_or("a jump table is missing")?;
                let sizes = [
                    usize::from(u16::from_le_bytes([jump[0], jump[1]])),
                    usize::from(u16::from_le_bytes([jump[2], jump[3]])),
                    usize::from(u16::from_le_bytes([jump[4], jump[5]])),
                ];
                let share = regenerated.div_ceil(4);
                let last_share = regenerated
                    .checked_sub(3 * share)
                    .ok_or("four literal streams cannot share so few literals")?;
                let mut start = 6;
                for (index, count) in [share, share, share, last_share].into_iter().enumerate() {
                    let end = if index < 3 {
                        start + sizes[index]
                    } else {
                        payload.len()
                    };
                    let stream = payload
                        .get(start..end)
                        .ok_or("a literal stream runs past its literals")?;
                    table.decode_stream(stream, count, &mut literals)?;
                    start = end;
                }
            }
            Ok((literals, header + compressed))
        }
    }
}

fn table_for(
    mode: u8,
    data: &[u8],
    at: &mut usize,
    previous: &mut Option<DecodeTable>,
    predefined: (&[i16], u32),
    most_accuracy: u32,
    largest_symbol: usize,
) -> Result<DecodeTable, Problem> {
    let table = match mode {
        0 => decode_table(predefined.0, predefined.1)?,
        1 => {
            let symbol = byte(data, *at)?;
            if usize::from(symbol) > largest_symbol {
                return Err("an RLE sequence symbol is out of range");
            }
            *at += 1;
            rle_table(symbol)
        }
        2 => {
            let rest = data.get(*at..).ok_or("a sequence table runs past its block")?;
            let (counts, accuracy, used) = read_distribution(rest, most_accuracy, largest_symbol)?;
            *at += used;
            decode_table(&counts, accuracy)?
        }
        _ => previous
            .clone()
            .ok_or("a repeated sequence table has nothing to repeat")?,
    };
    *previous = Some(table.clone());
    Ok(table)
}

fn sequences_section(
    data: &[u8],
    literals: &[u8],
    out: &mut Vec<u8>,
    state: &mut FrameState,
    frame_start: usize,
    room: usize,
) -> Result<(), Problem> {
    let block_start = out.len();
    let first = byte(data, 0)?;
    let (count, mut at) = match first {
        0 => {
            if literals.len() > room {
                return Err("a block's literals pass its room");
            }
            out.extend_from_slice(literals);
            return Ok(());
        }
        1..=127 => (usize::from(first), 1),
        128..=254 => (
            ((usize::from(first) - 128) << 8) + usize::from(byte(data, 1)?),
            2,
        ),
        255 => (
            usize::from(byte(data, 1)?) + (usize::from(byte(data, 2)?) << 8) + 0x7F00,
            3,
        ),
    };
    let modes = byte(data, at)?;
    at += 1;
    if modes & 3 != 0 {
        return Err("a sequences section sets its reserved bits");
    }
    let literal_lengths = table_for(
        modes >> 6,
        data,
        &mut at,
        &mut state.literal_lengths,
        (&LITERAL_LENGTH_COUNTS, LITERAL_LENGTH_ACCURACY),
        MOST_LENGTH_ACCURACY,
        LARGEST_LITERAL_LENGTH_CODE,
    )?;
    let offsets = table_for(
        (modes >> 4) & 3,
        data,
        &mut at,
        &mut state.offsets,
        (&OFFSET_COUNTS, OFFSET_ACCURACY),
        MOST_OFFSET_ACCURACY,
        LARGEST_OFFSET_CODE,
    )?;
    let match_lengths = table_for(
        (modes >> 2) & 3,
        data,
        &mut at,
        &mut state.match_lengths,
        (&MATCH_LENGTH_COUNTS, MATCH_LENGTH_ACCURACY),
        MOST_LENGTH_ACCURACY,
        LARGEST_MATCH_LENGTH_CODE,
    )?;
    let stream = data.get(at..).ok_or("a sequences bitstream is missing")?;
    let mut bits = BackwardBits::new(stream)?;
    let mut literal_state = DecodeState::new(&literal_lengths, &mut bits);
    let mut offset_state = DecodeState::new(&offsets, &mut bits);
    let mut match_state = DecodeState::new(&match_lengths, &mut bits);
    let mut literal_at = 0_usize;
    for index in 0..count {
        let offset_code = u32::from(offset_state.symbol());
        let match_code = usize::from(match_state.symbol());
        let literal_code = usize::from(literal_state.symbol());
        if offset_code > LARGEST_OFFSET_CODE as u32
            || match_code > LARGEST_MATCH_LENGTH_CODE
            || literal_code > LARGEST_LITERAL_LENGTH_CODE
        {
            return Err("a sequence code is out of range");
        }
        let offset_value = (1_u64 << offset_code) + bits.read(offset_code);
        let match_length = MATCH_LENGTH_START[match_code] as usize
            + bits.read(u32::from(MATCH_LENGTH_BITS[match_code])) as usize;
        let literal_length = LITERAL_LENGTH_START[literal_code] as usize
            + bits.read(u32::from(LITERAL_LENGTH_BITS[literal_code])) as usize;
        let offset = resolve_offset(&mut state.repeats, offset_value, literal_length)?;
        if index + 1 < count {
            literal_state.update(&mut bits);
            match_state.update(&mut bits);
            offset_state.update(&mut bits);
        }
        let literal_end = literal_at
            .checked_add(literal_length)
            .filter(|&end| end <= literals.len())
            .ok_or("a sequence takes more literals than the block has")?;
        if out.len() - block_start + literal_length + match_length > room {
            return Err("a block's sequences pass its room");
        }
        out.extend_from_slice(&literals[literal_at..literal_end]);
        literal_at = literal_end;
        if offset == 0 || offset > out.len() - frame_start {
            return Err("a match reaches before the start of its frame");
        }
        let from = out.len() - offset;
        for step in 0..match_length {
            let value = out[from + step];
            out.push(value);
        }
    }
    if !bits.finished_exactly() {
        return Err("a sequences bitstream's length does not match its sequences");
    }
    let tail = &literals[literal_at..];
    if out.len() - block_start + tail.len() > room {
        return Err("a block's literals pass its room");
    }
    out.extend_from_slice(tail);
    Ok(())
}

fn resolve_offset(
    repeats: &mut [usize; 3],
    offset_value: u64,
    literal_length: usize,
) -> Result<usize, Problem> {
    if offset_value > 3 {
        let offset = usize::try_from(offset_value - 3).map_err(|_| "a match offset is too large")?;
        *repeats = [offset, repeats[0], repeats[1]];
        return Ok(offset);
    }
    let code = offset_value as usize + usize::from(literal_length == 0);
    let offset = match code {
        1 => repeats[0],
        2 => {
            let chosen = repeats[1];
            *repeats = [chosen, repeats[0], repeats[2]];
            chosen
        }
        3 => {
            let chosen = repeats[2];
            *repeats = [chosen, repeats[0], repeats[1]];
            chosen
        }
        _ => {
            let chosen = repeats[0].checked_sub(1).ok_or("a repeated offset is zero")?;
            *repeats = [chosen, repeats[0], repeats[1]];
            chosen
        }
    };
    Ok(offset)
}
