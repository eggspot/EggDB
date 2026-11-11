mod bits;
mod decode;
mod encode;
mod encode_huffman;
mod fse;
mod huffman;
mod tables;
mod xxh64;

const MAGIC: u32 = 0xFD2F_B528;
const BLOCK_MAXIMUM: usize = 128 * 1024;

/// Compresses `input` into one Zstandard frame of RFC 8878: a single segment with the content
/// size and checksum, raw, RLE or compressed blocks of at most 128 KiB, the compressed ones with
/// Huffman-coded literals (raw when that does not pay) and sequences coded with the predefined
/// distributions.
pub(crate) fn compress(input: &[u8]) -> Vec<u8> {
    encode::compress(input)
}

/// Decompresses the Zstandard frames in `input`, which must produce exactly `expected` bytes.
/// Every frame, block, table and bitstream is checked: hostile input is an error, never a panic
/// and never more than `expected` bytes of memory. Dictionaries are refused.
pub(crate) fn decompress(input: &[u8], expected: usize) -> Result<Vec<u8>, &'static str> {
    decode::decompress(input, expected)
}

#[cfg(test)]
mod tests;
