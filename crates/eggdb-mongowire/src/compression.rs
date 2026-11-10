use crate::message::{HEADER_BYTES, MAX_MESSAGE_BYTES};

pub(crate) const OP_COMPRESSED: i32 = 2012;

/// A compressor of `OP_COMPRESSED`, by the id the wire carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Compressor {
    Noop,
    Snappy,
    Zlib,
}

/// The compressors this server negotiates in `hello`, in its order of preference.
pub(crate) const NEGOTIATED: [&str; 2] = ["snappy", "zlib"];

const ZLIB_STORED_BLOCK: usize = 65_535;

impl Compressor {
    fn of_id(id: u8) -> Option<Self> {
        match id {
            0 => Some(Compressor::Noop),
            1 => Some(Compressor::Snappy),
            2 => Some(Compressor::Zlib),
            _ => None,
        }
    }

    fn id(self) -> u8 {
        match self {
            Compressor::Noop => 0,
            Compressor::Snappy => 1,
            Compressor::Zlib => 2,
        }
    }

    fn compress(self, bytes: &[u8]) -> Vec<u8> {
        match self {
            Compressor::Noop => bytes.to_vec(),
            Compressor::Snappy => eggdb_columnar::snappy_compress(bytes),
            Compressor::Zlib => zlib_stored(bytes),
        }
    }

    fn decompress(self, bytes: &[u8], size: usize) -> Result<Vec<u8>, String> {
        match self {
            Compressor::Noop => Ok(bytes.to_vec()),
            Compressor::Snappy => eggdb_columnar::snappy_decompress(bytes, size)
                .map_err(|_| "the snappy payload is damaged".to_owned()),
            Compressor::Zlib => eggdb_content::zlib_inflate(bytes, size)
                .map_err(|_| "the zlib payload is damaged".to_owned()),
        }
    }
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut low, mut high) = (1u32, 0u32);
    for chunk in bytes.chunks(5552) {
        for byte in chunk {
            low += u32::from(*byte);
            high += low;
        }
        low %= 65_521;
        high %= 65_521;
    }
    (high << 16) | low
}

fn zlib_stored(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / ZLIB_STORED_BLOCK * 5 + 11);
    out.extend_from_slice(&[0x78, 0x01]);
    let mut blocks = bytes.chunks(ZLIB_STORED_BLOCK).peekable();
    if blocks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(block) = blocks.next() {
        let last = u8::from(blocks.peek().is_none());
        let length = block.len() as u16;
        out.push(last);
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&(!length).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&adler32(bytes).to_be_bytes());
    out
}

fn le_i32(bytes: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Reads an `OP_COMPRESSED` body (the bytes after the header): the original op code, the
/// compressor, and the original body.
pub(crate) fn decompress_body(body: &[u8]) -> Result<(i32, Compressor, Vec<u8>), String> {
    let original = le_i32(body, 0).ok_or("the compressed message is cut short")?;
    let size = le_i32(body, 4)
        .and_then(|size| usize::try_from(size).ok())
        .filter(|size| *size <= MAX_MESSAGE_BYTES - HEADER_BYTES)
        .ok_or("the uncompressed size is out of range")?;
    let compressor = body
        .get(8)
        .copied()
        .and_then(Compressor::of_id)
        .ok_or("the compressor is not one this server negotiates")?;
    if original == OP_COMPRESSED {
        return Err("a compressed message may not hold another".to_owned());
    }
    let restored = compressor.decompress(&body[9..], size)?;
    if restored.len() != size {
        return Err("the uncompressed size does not match".to_owned());
    }
    Ok((original, compressor, restored))
}

/// The reply message `reply` (header included) as an `OP_COMPRESSED` message with `compressor`.
pub(crate) fn compress_reply(reply: &[u8], compressor: Compressor) -> Vec<u8> {
    let Some((head, body)) = reply.split_at_checked(HEADER_BYTES) else {
        return reply.to_vec();
    };
    let compressed = compressor.compress(body);
    let mut out = Vec::with_capacity(HEADER_BYTES + 9 + compressed.len());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&head[4..12]);
    out.extend_from_slice(&OP_COMPRESSED.to_le_bytes());
    out.extend_from_slice(&head[12..16]);
    out.extend_from_slice(&(body.len() as i32).to_le_bytes());
    out.push(compressor.id());
    out.extend_from_slice(&compressed);
    let length = out.len() as i32;
    out[..4].copy_from_slice(&length.to_le_bytes());
    out
}
