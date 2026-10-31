//! Columnar encodings for EggDB (D201): the building blocks of columnar tables and of the
//! Parquet and Arrow formats. Values of a column are kept together and encoded with bit packing,
//! the run-length/bit-packed hybrid Parquet's specification describes, zigzag varint deltas and
//! dictionaries; [`IntegerChunk`] picks the smallest encoding for a chunk and aggregates it
//! without decoding runs value by value. Every decoder checks its input: damaged bytes are an
//! error, never a panic.

mod bits;
mod chunk;
mod delta;
mod dictionary;
mod hybrid;
mod snappy;

pub use bits::{bit_width, pack, unpack};
pub use chunk::{ChunkStats, Encoding, IntegerChunk, MAX_CHUNK_VALUES};
pub use delta::{decode_deltas, encode_deltas, read_varint, write_varint};
pub use dictionary::Dictionary;
pub use hybrid::{decode_hybrid, encode_hybrid};
pub use snappy::{snappy_compress, snappy_decompress};

/// Why encoded bytes could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "damaged column data: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

fn damaged(message: &str) -> DecodeError {
    DecodeError(message.to_owned())
}

#[cfg(test)]
mod tests;
