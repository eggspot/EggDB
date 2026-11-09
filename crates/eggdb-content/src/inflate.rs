use crate::ContentError;
use crate::zip::crc32;
use std::fmt;

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
const GZIP_DEFLATE: u8 = 8;
const GZIP_HEADER_CRC: u8 = 0x02;
const GZIP_EXTRA: u8 = 0x04;
const GZIP_NAME: u8 = 0x08;
const GZIP_COMMENT: u8 = 0x10;
const GZIP_RESERVED_FLAGS: u8 = 0xe0;
const GZIP_FIXED_HEADER_BYTES: usize = 10;
const GZIP_TRAILER_BYTES: usize = 8;
const ZLIB_DEFLATE: u8 = 8;
const ZLIB_MAX_WINDOW_INFO: u8 = 7;
const ZLIB_PRESET_DICTIONARY: u8 = 0x20;
const ZLIB_HEADER_BYTES: usize = 2;
const ZLIB_TRAILER_BYTES: usize = 4;
const ADLER_MODULUS: u32 = 65_521;
const ADLER_BYTES_BEFORE_OVERFLOW: usize = 5_552;

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
) -> Result<(), DecompressError> {
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
                return Err(corrupt("a distance reaches before the start").into());
            }
            let start = out.len() - distance;
            for offset in 0..length {
                let byte = out[start + offset];
                out.push(byte);
            }
        }
        if out.len() > limit {
            return Err(DecompressError::PastLimit(limit));
        }
    }
}

/// Why compressed data could not be decompressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecompressError {
    /// It expands past the limit given, in bytes.
    PastLimit(usize),
    /// It is damaged, cut short or not in the format asked for.
    Corrupt(ContentError),
}

impl fmt::Display for DecompressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PastLimit(limit) => write!(formatter, "the content expands past {limit} bytes"),
            Self::Corrupt(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for DecompressError {}

impl From<ContentError> for DecompressError {
    fn from(error: ContentError) -> Self {
        Self::Corrupt(error)
    }
}

impl From<DecompressError> for ContentError {
    fn from(error: DecompressError) -> Self {
        match error {
            DecompressError::Corrupt(error) => error,
            past_limit @ DecompressError::PastLimit(_) => ContentError(past_limit.to_string()),
        }
    }
}

fn inflate_prefix(data: &[u8], limit: usize) -> Result<(Vec<u8>, usize), DecompressError> {
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
                let length = u16::from_le_bytes([header[0], header[1]]);
                if length != !u16::from_le_bytes([header[2], header[3]]) {
                    return Err(corrupt("a stored block's length check fails").into());
                }
                bits.at += 4;
                let stored = data
                    .get(bits.at..bits.at + usize::from(length))
                    .ok_or_else(|| corrupt("a stored block ends early"))?;
                if out.len() + stored.len() > limit {
                    return Err(DecompressError::PastLimit(limit));
                }
                out.extend_from_slice(stored);
                bits.at += stored.len();
            }
            1 => {
                let (literals, distances) = fixed_tables()?;
                codes(&mut bits, &mut out, &literals, &distances, limit)?;
            }
            2 => {
                let (literals, distances) = dynamic_tables(&mut bits)?;
                codes(&mut bits, &mut out, &literals, &distances, limit)?;
            }
            _ => return Err(corrupt("a block has the reserved type 3").into()),
        }
        if last {
            return Ok((out, bits.at));
        }
    }
}

/// Decompresses a raw DEFLATE stream (RFC 1951), refusing to produce more than `limit` bytes.
pub fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>, ContentError> {
    inflate_prefix(data, limit)
        .map(|(out, _)| out)
        .map_err(ContentError::from)
}

fn u16_le_at(data: &[u8], at: usize) -> Option<usize> {
    data.get(at..at + 2)
        .map(|bytes| usize::from(u16::from_le_bytes([bytes[0], bytes[1]])))
}

fn gzip_header_bytes(member: &[u8]) -> Result<usize, DecompressError> {
    let ends_early = || corrupt("a gzip header ends early");
    let &[0x1f, 0x8b, method, flags, ..] = member else {
        return Err(corrupt("the data is not gzip").into());
    };
    if member.len() < GZIP_FIXED_HEADER_BYTES {
        return Err(ends_early().into());
    }
    if method != GZIP_DEFLATE {
        return Err(corrupt("a gzip member is not compressed with DEFLATE").into());
    }
    if flags & GZIP_RESERVED_FLAGS != 0 {
        return Err(corrupt("a gzip header sets reserved flags").into());
    }
    let mut at = GZIP_FIXED_HEADER_BYTES;
    if flags & GZIP_EXTRA != 0 {
        at += 2 + u16_le_at(member, at).ok_or_else(ends_early)?;
    }
    for field in [GZIP_NAME, GZIP_COMMENT] {
        if flags & field != 0 {
            let text = member.get(at..).ok_or_else(ends_early)?;
            at += 1 + text
                .iter()
                .position(|&byte| byte == 0)
                .ok_or_else(ends_early)?;
        }
    }
    if flags & GZIP_HEADER_CRC != 0 {
        let stored = u16_le_at(member, at).ok_or_else(ends_early)?;
        let header = member.get(..at).ok_or_else(ends_early)?;
        if crc32(header) & 0xffff != stored as u32 {
            return Err(corrupt("a gzip header's CRC-16 does not match").into());
        }
        at += 2;
    }
    if at > member.len() {
        return Err(ends_early().into());
    }
    Ok(at)
}

fn past_limit_of_whole(limit: usize) -> impl Fn(DecompressError) -> DecompressError {
    move |error| match error {
        DecompressError::PastLimit(_) => DecompressError::PastLimit(limit),
        other @ DecompressError::Corrupt(_) => other,
    }
}

/// Decompresses gzip data (RFC 1952), every member of it one after another, checking each
/// member's CRC-32 and length, and refusing to produce more than `limit` bytes in all.
pub fn gunzip(data: &[u8], limit: usize) -> Result<Vec<u8>, DecompressError> {
    let mut out: Vec<u8> = Vec::new();
    let mut at = 0;
    loop {
        let member = data.get(at..).unwrap_or_default();
        let header_bytes = gzip_header_bytes(member)?;
        let (decoded, consumed) = inflate_prefix(
            member.get(header_bytes..).unwrap_or_default(),
            limit.saturating_sub(out.len()),
        )
        .map_err(past_limit_of_whole(limit))?;
        let trailer_at = header_bytes + consumed;
        let trailer = member
            .get(trailer_at..trailer_at + GZIP_TRAILER_BYTES)
            .ok_or_else(|| corrupt("a gzip member ends before its trailer"))?;
        let stored_crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
        let stored_size = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
        if crc32(&decoded) != stored_crc {
            return Err(corrupt("a gzip member's CRC-32 does not match").into());
        }
        if decoded.len() as u32 != stored_size {
            return Err(corrupt("a gzip member's length does not match").into());
        }
        if out.is_empty() {
            out = decoded;
        } else {
            out.extend_from_slice(&decoded);
        }
        at += trailer_at + GZIP_TRAILER_BYTES;
        if at >= data.len() {
            return Ok(out);
        }
    }
}

pub(crate) fn adler32(bytes: &[u8]) -> u32 {
    let (mut low, mut high) = (1u32, 0u32);
    for run in bytes.chunks(ADLER_BYTES_BEFORE_OVERFLOW) {
        for &byte in run {
            low += u32::from(byte);
            high += low;
        }
        low %= ADLER_MODULUS;
        high %= ADLER_MODULUS;
    }
    (high << 16) | low
}

fn has_zlib_header(compression: u8, flags: u8) -> bool {
    compression & 0x0f == ZLIB_DEFLATE
        && compression >> 4 <= ZLIB_MAX_WINDOW_INFO
        && ((u16::from(compression) << 8) | u16::from(flags)) % 31 == 0
}

fn zlib_stream(data: &[u8], flags: u8, limit: usize) -> Result<Vec<u8>, DecompressError> {
    if flags & ZLIB_PRESET_DICTIONARY != 0 {
        return Err(corrupt("a zlib stream needs a preset dictionary").into());
    }
    let (out, consumed) = inflate_prefix(data.get(ZLIB_HEADER_BYTES..).unwrap_or_default(), limit)?;
    let trailer_at = ZLIB_HEADER_BYTES + consumed;
    let trailer = data
        .get(trailer_at..trailer_at + ZLIB_TRAILER_BYTES)
        .ok_or_else(|| corrupt("a zlib stream ends before its Adler-32"))?;
    if adler32(&out) != u32::from_be_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]) {
        return Err(corrupt("a zlib stream's Adler-32 does not match").into());
    }
    if trailer_at + ZLIB_TRAILER_BYTES != data.len() {
        return Err(corrupt("bytes follow the zlib stream").into());
    }
    Ok(out)
}

fn whole_raw_deflate(data: &[u8], limit: usize) -> Result<Vec<u8>, DecompressError> {
    let (out, consumed) = inflate_prefix(data, limit)?;
    if consumed != data.len() {
        return Err(corrupt("bytes follow the DEFLATE stream").into());
    }
    Ok(out)
}

/// Decompresses the HTTP `deflate` coding: a zlib stream (RFC 1950) with its Adler-32
/// checked, or, as browsers also accept, a raw DEFLATE stream (RFC 1951) without the zlib
/// wrapper; refuses to produce more than `limit` bytes.
pub fn zlib_inflate(data: &[u8], limit: usize) -> Result<Vec<u8>, DecompressError> {
    let &[compression, flags, ..] = data else {
        return whole_raw_deflate(data, limit);
    };
    if !has_zlib_header(compression, flags) {
        return whole_raw_deflate(data, limit);
    }
    match zlib_stream(data, flags, limit) {
        Err(DecompressError::Corrupt(zlib_error)) => match whole_raw_deflate(data, limit) {
            Ok(out) => Ok(out),
            Err(past_limit @ DecompressError::PastLimit(_)) => Err(past_limit),
            Err(DecompressError::Corrupt(_)) => Err(DecompressError::Corrupt(zlib_error)),
        },
        decoded => decoded,
    }
}
