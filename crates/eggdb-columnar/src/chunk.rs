use crate::bits::{bit_width, pack, unpack};
use crate::delta::{decode_deltas, encode_deltas, read_varint, write_varint};
use crate::hybrid::{decode_hybrid, encode_hybrid};
use crate::{DecodeError, damaged};

/// How an [`IntegerChunk`] stores its values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// Offsets from the minimum, bit packed.
    FrameOfReference,
    /// Offsets from the minimum as hybrid runs: long runs of one value cost a few bytes.
    Runs,
    /// Zigzag varint differences from the previous value.
    Deltas,
}

/// The minimum, maximum, sum and count of a chunk's values, kept beside it so aggregates and
/// filters can skip it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkStats {
    /// The least value.
    pub min: i64,
    /// The greatest value.
    pub max: i64,
    /// The sum, wide enough not to overflow.
    pub sum: i128,
    /// How many values.
    pub count: u64,
}

/// A chunk of a 64-bit integer column, encoded with whichever of the encodings is smallest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegerChunk {
    encoding: Encoding,
    stats: Option<ChunkStats>,
    bytes: Vec<u8>,
}

/// The most values a chunk holds.
pub const MAX_CHUNK_VALUES: u64 = 1 << 24;

fn offsets(values: &[i64], min: i64) -> Vec<u64> {
    values.iter().map(|value| value.wrapping_sub(min) as u64).collect()
}

impl IntegerChunk {
    /// Encodes `values`, at most [`MAX_CHUNK_VALUES`] of them (the rest belong in the next
    /// chunk).
    pub fn encode(values: &[i64]) -> Self {
        let values = &values[..values.len().min(MAX_CHUNK_VALUES as usize)];
        let Some((&min, &max)) = values.iter().min().zip(values.iter().max()) else {
            return Self { encoding: Encoding::FrameOfReference, stats: None, bytes: Vec::new() };
        };
        let stats = ChunkStats { min, max, sum: values.iter().map(|value| i128::from(*value)).sum(), count: values.len() as u64 };
        let shifted = offsets(values, min);
        let width = bit_width(&shifted);
        let mut frame = vec![width];
        pack(&shifted, width, &mut frame);
        let mut runs = vec![width];
        encode_hybrid(&shifted, width, &mut runs);
        let mut deltas = Vec::new();
        encode_deltas(values, &mut deltas);
        let (encoding, bytes) = [(Encoding::FrameOfReference, frame), (Encoding::Runs, runs), (Encoding::Deltas, deltas)]
            .into_iter()
            .min_by_key(|(_, bytes)| bytes.len())
            .unwrap_or((Encoding::FrameOfReference, Vec::new()));
        Self { encoding, stats: Some(stats), bytes }
    }

    /// The encoding chosen.
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// The statistics, `None` for an empty chunk.
    pub fn stats(&self) -> Option<ChunkStats> {
        self.stats
    }

    /// The encoded size in bytes.
    pub fn encoded_bytes(&self) -> usize {
        self.bytes.len()
    }

    /// The values.
    pub fn decode(&self) -> Result<Vec<i64>, DecodeError> {
        let Some(stats) = self.stats else {
            return Ok(Vec::new());
        };
        if stats.count > MAX_CHUNK_VALUES {
            return Err(damaged("a chunk holds more values than a chunk may"));
        }
        let count = usize::try_from(stats.count).map_err(|_| damaged("a chunk is too long"))?;
        let values = match self.encoding {
            Encoding::Deltas => decode_deltas(&self.bytes)?.0,
            Encoding::FrameOfReference | Encoding::Runs => {
                let (&width, rest) = self.bytes.split_first().ok_or_else(|| damaged("a chunk has no bit width"))?;
                let shifted = if self.encoding == Encoding::Runs { decode_hybrid(rest, width, count)?.0 } else { unpack(rest, width, count)? };
                shifted.into_iter().map(|offset| stats.min.wrapping_add(offset as i64)).collect()
            }
        };
        if values.len() != count {
            return Err(damaged("a chunk holds a different number of values than its statistics"));
        }
        Ok(values)
    }

    /// The sum of the values greater than or equal to `low`: whole chunks outside the range are
    /// answered from the statistics, run-length chunks one run at a time.
    pub fn sum_at_least(&self, low: i64) -> Result<i128, DecodeError> {
        let Some(stats) = self.stats else {
            return Ok(0);
        };
        if stats.min >= low {
            return Ok(stats.sum);
        }
        if stats.max < low {
            return Ok(0);
        }
        Ok(self.decode()?.into_iter().filter(|value| *value >= low).map(i128::from).sum())
    }

    /// The chunk as bytes: its encoding, statistics and data.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = vec![match self.encoding {
            Encoding::FrameOfReference => 0,
            Encoding::Runs => 1,
            Encoding::Deltas => 2,
        }];
        match self.stats {
            None => write_varint(0, &mut out),
            Some(stats) => {
                write_varint(stats.count, &mut out);
                out.extend_from_slice(&stats.min.to_le_bytes());
                out.extend_from_slice(&stats.max.to_le_bytes());
                out.extend_from_slice(&stats.sum.to_le_bytes());
            }
        }
        out.extend_from_slice(&self.bytes);
        out
    }

    /// Reads what [`IntegerChunk::to_bytes`] wrote, checking the data against its statistics.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        let (&tag, rest) = bytes.split_first().ok_or_else(|| damaged("a chunk is empty"))?;
        let encoding = match tag {
            0 => Encoding::FrameOfReference,
            1 => Encoding::Runs,
            2 => Encoding::Deltas,
            _ => return Err(damaged("a chunk has an unknown encoding")),
        };
        let mut at = 0;
        let count = read_varint(rest, &mut at)?;
        let stats = if count == 0 {
            None
        } else {
            let fixed = rest.get(at..at + 32).ok_or_else(|| damaged("a chunk's statistics are cut short"))?;
            at += 32;
            let read = |from: usize| i64::from_le_bytes(fixed[from..from + 8].try_into().unwrap_or_default());
            Some(ChunkStats {
                min: read(0),
                max: read(8),
                sum: i128::from_le_bytes(fixed[16..32].try_into().unwrap_or_default()),
                count,
            })
        };
        let chunk = Self { encoding, stats, bytes: rest[at..].to_vec() };
        let values = chunk.decode()?;
        if let Some(stats) = stats {
            let consistent = values.iter().min() == Some(&stats.min)
                && values.iter().max() == Some(&stats.max)
                && values.iter().map(|value| i128::from(*value)).sum::<i128>() == stats.sum;
            if !consistent {
                return Err(damaged("a chunk's values do not match its statistics"));
            }
        }
        Ok(chunk)
    }
}
