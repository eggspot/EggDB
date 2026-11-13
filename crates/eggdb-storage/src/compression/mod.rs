#[cfg(not(feature = "std"))]
#[allow(unused_imports)]
use crate::prelude::*;

pub(crate) mod lz4;
pub(crate) mod zstd;

/// Decompresses one Zstandard frame (RFC 8878, EggDB's own codec) that must give exactly
/// `expected` bytes, as Arrow's and Parquet's `ZSTD` buffers hold them. Hostile input is an
/// error, never a panic, and never more than `expected` bytes of memory.
pub fn decompress_zstd(input: &[u8], expected: usize) -> Result<Vec<u8>, &'static str> {
    zstd::decompress(input, expected)
}

/// `input` as one Zstandard frame of EggDB's own encoder.
pub fn compress_zstd(input: &[u8]) -> Vec<u8> {
    zstd::compress(input)
}

/// `input` as one LZ4 frame of independent compressed blocks, with no checksums.
pub fn compress_lz4_frame(input: &[u8]) -> Vec<u8> {
    lz4::compress_frame(input)
}

/// Decompresses one LZ4 frame that must give exactly `expected` bytes, as Arrow's `LZ4_FRAME`
/// buffers hold them. Hostile input is an error, never a panic.
pub fn decompress_lz4_frame(input: &[u8], expected: usize) -> Result<Vec<u8>, &'static str> {
    lz4::decompress_frame(input, expected)
}

/// Decompresses one raw LZ4 block (no frame) that must give exactly `expected` bytes, as Blosc
/// and Zarr store them. Hostile input is an error, never a panic.
pub fn decompress_lz4_block(input: &[u8], expected: usize) -> Result<Vec<u8>, &'static str> {
    lz4::decompress(input, expected)
}

/// Whether large values are compressed when written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ValueCompression {
    /// Values are stored as given.
    #[default]
    Off,
    /// A value of at least 4 KiB whose LZ4 block saves at least a sixteenth of it is stored
    /// compressed on overflow pages of layout 2, which sets the file's incompatible feature
    /// [`crate::Features::COMPRESSED_VALUES`].
    Lz4,
    /// A value of at least 4 KiB whose Zstandard frame (RFC 8878, our own codec) saves at least a
    /// sixteenth of it is stored compressed on overflow pages of layout 3, which sets the file's
    /// incompatible feature [`crate::Features::ZSTD_VALUES`]. Slower to write than
    /// [`ValueCompression::Lz4`], for cold data.
    Zstd,
}
