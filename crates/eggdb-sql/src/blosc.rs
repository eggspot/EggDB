//! Blosc 1 chunks as Zarr stores them (D806): the 16-byte header, the block starts, split
//! streams, byte shuffle, and the BloscLZ, LZ4, Snappy, zlib and Zstandard compressors.

const HEADER_BYTES: usize = 16;
const BYTE_SHUFFLE: u8 = 0x01;
const MEMCPYED: u8 = 0x02;
const BIT_SHUFFLE: u8 = 0x04;
const DONT_SPLIT: u8 = 0x10;
const MOST_SPLITS: usize = 16;
const LEAST_SPLIT_ELEMENTS: usize = 128;
const BLOSCLZ_FAR_DISTANCE: usize = 8191;

fn word(input: &[u8], at: usize) -> Result<u32, &'static str> {
    input
        .get(at..at + 4)
        .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .ok_or("the Blosc chunk is truncated")
}

/// The bytes one Blosc 1 chunk holds, at most `limit` of them. Hostile input is an error, never a
/// panic.
pub(crate) fn decompress(input: &[u8], limit: usize) -> Result<Vec<u8>, &'static str> {
    let header = input.get(..HEADER_BYTES).ok_or("the Blosc chunk is truncated")?;
    let flags = header[2];
    let typesize = usize::from(header[3]).max(1);
    let nbytes = word(header, 4)? as usize;
    let blocksize = word(header, 8)? as usize;
    if nbytes > limit {
        return Err("the Blosc chunk holds more bytes than the array allows");
    }
    if flags & BIT_SHUFFLE != 0 {
        return Err("bit-shuffled and Blosc2 chunks are not supported yet");
    }
    if flags & MEMCPYED != 0 {
        return input
            .get(HEADER_BYTES..HEADER_BYTES + nbytes)
            .map(<[u8]>::to_vec)
            .ok_or("the Blosc chunk is truncated");
    }
    if nbytes == 0 {
        return Ok(Vec::new());
    }
    if blocksize == 0 {
        return Err("the Blosc block size is zero");
    }
    let compressor = flags >> 5;
    let blocks = nbytes.div_ceil(blocksize);
    if HEADER_BYTES + blocks.saturating_mul(4) > input.len() {
        return Err("the Blosc chunk is truncated");
    }
    let mut out = Vec::with_capacity(nbytes);
    for block in 0..blocks {
        let start = word(input, HEADER_BYTES + 4 * block)? as usize;
        let leftover = block + 1 == blocks && nbytes % blocksize != 0;
        let size = if leftover { nbytes % blocksize } else { blocksize };
        let split = flags & DONT_SPLIT == 0
            && !leftover
            && typesize <= MOST_SPLITS
            && blocksize / typesize >= LEAST_SPLIT_ELEMENTS;
        let splits = if split { typesize } else { 1 };
        let split_size = size / splits;
        let mut decoded = Vec::with_capacity(size);
        let mut at = start;
        for _ in 0..splits {
            let compressed = word(input, at)?;
            at += 4;
            let compressed = usize::try_from(i32::from_le_bytes(compressed.to_le_bytes()))
                .map_err(|_| "the Blosc stream size is negative")?;
            let stream = input
                .get(at..at + compressed)
                .ok_or("the Blosc stream runs past the chunk")?;
            at += compressed;
            if compressed == split_size {
                decoded.extend_from_slice(stream);
            } else {
                decoded.extend(decompress_stream(compressor, stream, split_size)?);
            }
        }
        if decoded.len() != size {
            return Err("a Blosc block decodes to the wrong size");
        }
        if flags & BYTE_SHUFFLE != 0 && typesize > 1 {
            decoded = unshuffle(&decoded, typesize);
        }
        out.extend_from_slice(&decoded);
    }
    Ok(out)
}

fn decompress_stream(compressor: u8, stream: &[u8], size: usize) -> Result<Vec<u8>, &'static str> {
    let out = match compressor {
        0 => blosclz(stream, size)?,
        1 => eggdb_storage::decompress_lz4_block(stream, size)?,
        2 => eggdb_columnar::snappy_decompress(stream, size)
            .map_err(|_| "a Blosc Snappy stream is corrupt")?,
        3 => eggdb_content::zlib_inflate(stream, size)
            .map_err(|_| "a Blosc zlib stream is corrupt")?,
        4 => eggdb_storage::decompress_zstd(stream, size)?,
        _ => return Err("the Blosc compressor is unknown"),
    };
    if out.len() != size {
        return Err("a Blosc stream decodes to the wrong size");
    }
    Ok(out)
}

fn unshuffle(shuffled: &[u8], typesize: usize) -> Vec<u8> {
    let elements = shuffled.len() / typesize;
    let mut out = vec![0u8; shuffled.len()];
    for element in 0..elements {
        for byte in 0..typesize {
            out[element * typesize + byte] = shuffled[byte * elements + element];
        }
    }
    let tail = elements * typesize;
    out[tail..].copy_from_slice(&shuffled[tail..]);
    out
}

fn blosclz(input: &[u8], size: usize) -> Result<Vec<u8>, &'static str> {
    const CORRUPT: &str = "a BloscLZ stream is corrupt";
    let mut out: Vec<u8> = Vec::with_capacity(size);
    let mut at = 0usize;
    let next = |at: &mut usize| -> Result<usize, &'static str> {
        let byte = *input.get(*at).ok_or(CORRUPT)?;
        *at += 1;
        Ok(usize::from(byte))
    };
    let mut control = next(&mut at)? & 31;
    loop {
        if control >= 32 {
            let mut length = (control >> 5) - 1;
            let mut distance = (control & 31) << 8;
            if length == 6 {
                loop {
                    let code = next(&mut at)?;
                    length += code;
                    if code != 255 {
                        break;
                    }
                }
            }
            let code = next(&mut at)?;
            length += 3;
            distance += code;
            if code == 255 && control & 31 == 31 {
                let high = next(&mut at)?;
                let low = next(&mut at)?;
                distance = (high << 8) + low + BLOSCLZ_FAR_DISTANCE;
            }
            let back = distance + 1;
            if back > out.len() || out.len() + length > size {
                return Err(CORRUPT);
            }
            let from = out.len() - back;
            for offset in 0..length {
                let byte = out[from + offset];
                out.push(byte);
            }
        } else {
            let count = control + 1;
            let literal = input.get(at..at + count).ok_or(CORRUPT)?;
            if out.len() + count > size {
                return Err(CORRUPT);
            }
            out.extend_from_slice(literal);
            at += count;
        }
        if at >= input.len() {
            break;
        }
        control = next(&mut at)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(flags: u8, typesize: u8, nbytes: u32, blocksize: u32, total: u32) -> Vec<u8> {
        let mut out = vec![2, 1, flags, typesize];
        out.extend_from_slice(&nbytes.to_le_bytes());
        out.extend_from_slice(&blocksize.to_le_bytes());
        out.extend_from_slice(&total.to_le_bytes());
        out
    }

    #[test]
    fn a_memcpyed_chunk_is_its_bytes() {
        let mut chunk = header(MEMCPYED, 1, 3, 3, 19);
        chunk.extend_from_slice(b"abc");
        assert_eq!(decompress(&chunk, 3).unwrap(), b"abc");
        assert!(decompress(&chunk, 2).is_err());
        assert!(decompress(&chunk[..17], 3).is_err());
    }

    #[test]
    fn stored_streams_are_unshuffled_by_byte() {
        let shuffled = [1u8, 2, 3, 0, 0, 0];
        let mut chunk = header(BYTE_SHUFFLE | DONT_SPLIT, 2, 6, 6, 0);
        chunk.extend_from_slice(&20u32.to_le_bytes());
        chunk.extend_from_slice(&6i32.to_le_bytes());
        chunk.extend_from_slice(&shuffled);
        assert_eq!(decompress(&chunk, 64).unwrap(), vec![1, 0, 2, 0, 3, 0]);
    }

    #[test]
    fn blosclz_literals_and_matches_decode() {
        let stream = [2u8, b'a', b'b', b'c', (2 + 1) << 5, 2];
        assert_eq!(blosclz(&stream, 8).unwrap(), b"abcabcab");
        assert!(blosclz(&[0, b'a', (2 + 1) << 5, 5], 8).is_err());
        assert!(blosclz(&[5, b'a'], 8).is_err());
    }

    #[test]
    fn bit_shuffled_chunks_are_refused() {
        let chunk = header(BIT_SHUFFLE, 4, 16, 16, 0);
        assert!(decompress(&chunk, 64).is_err());
    }
}
