use crate::bits::bit_width;
use crate::chunk::MAX_CHUNK_VALUES;
use crate::delta::{read_varint, write_varint};
use crate::hybrid::{decode_hybrid, encode_hybrid};
use crate::snappy::{snappy_compress, snappy_decompress};
use crate::{DecodeError, Dictionary, damaged};

const MOST_CHUNK_BYTES: usize = 1 << 30;

/// How a [`StringChunk`] stores its values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringEncoding {
    /// Each value's length and bytes.
    Plain,
    /// The distinct values once, then each row's index into them as hybrid runs.
    Dictionary,
    /// The plain form compressed with Snappy.
    SnappyPlain,
}

/// The count of a string chunk's values and the least and greatest of them, compared byte by
/// byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringStats {
    /// The least value.
    pub min: Vec<u8>,
    /// The greatest value.
    pub max: Vec<u8>,
    /// How many values.
    pub count: u64,
}

/// A chunk of a string or binary column, encoded with whichever encoding is smallest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringChunk {
    encoding: StringEncoding,
    stats: Option<StringStats>,
    bytes: Vec<u8>,
}

fn write_bytes(value: &[u8], out: &mut Vec<u8>) {
    write_varint(value.len() as u64, out);
    out.extend_from_slice(value);
}

fn read_bytes(bytes: &[u8], at: &mut usize) -> Result<Vec<u8>, DecodeError> {
    let length = usize::try_from(read_varint(bytes, at)?).map_err(|_| damaged("a string is too long"))?;
    let end = at.checked_add(length).ok_or_else(|| damaged("a string is too long"))?;
    let value = bytes.get(*at..end).ok_or_else(|| damaged("a string runs past its bytes"))?;
    *at = end;
    Ok(value.to_vec())
}

fn plain(values: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for value in values {
        write_bytes(value, &mut out);
    }
    out
}

fn read_plain(bytes: &[u8], count: usize) -> Result<Vec<Vec<u8>>, DecodeError> {
    let mut at = 0;
    let mut values = Vec::with_capacity(count.min(bytes.len()));
    for _ in 0..count {
        values.push(read_bytes(bytes, &mut at)?);
    }
    if at != bytes.len() {
        return Err(damaged("a plain string chunk has bytes left over"));
    }
    Ok(values)
}

fn stats_of(values: &[Vec<u8>]) -> Option<StringStats> {
    Some(StringStats {
        min: values.iter().min()?.clone(),
        max: values.iter().max()?.clone(),
        count: values.len() as u64,
    })
}

impl StringChunk {
    /// Encodes `values`, at most [`MAX_CHUNK_VALUES`] of them (the rest belong in the next
    /// chunk).
    pub fn encode(values: &[&[u8]]) -> Self {
        let values = &values[..values.len().min(MAX_CHUNK_VALUES as usize)];
        let stats = values.iter().min().zip(values.iter().max()).map(|(min, max)| StringStats {
            min: min.to_vec(),
            max: max.to_vec(),
            count: values.len() as u64,
        });
        let plain_bytes = plain(values);
        let mut candidates = vec![
            (StringEncoding::SnappyPlain, snappy_compress(&plain_bytes)),
        ];
        let dictionary = Dictionary::of(values.iter().copied());
        if dictionary.pays_off() {
            let mut out = Vec::new();
            write_varint(dictionary.values.len() as u64, &mut out);
            for value in &dictionary.values {
                write_bytes(value, &mut out);
            }
            let width = bit_width(&dictionary.indexes);
            out.push(width);
            encode_hybrid(&dictionary.indexes, width, &mut out);
            candidates.push((StringEncoding::Dictionary, out));
        }
        let (encoding, bytes) = candidates
            .into_iter()
            .filter(|(_, bytes)| bytes.len() < plain_bytes.len())
            .min_by_key(|(_, bytes)| bytes.len())
            .unwrap_or((StringEncoding::Plain, plain_bytes));
        Self { encoding, stats, bytes }
    }

    /// The encoding chosen.
    pub fn encoding(&self) -> StringEncoding {
        self.encoding
    }

    /// The statistics, `None` for an empty chunk.
    pub fn stats(&self) -> Option<&StringStats> {
        self.stats.as_ref()
    }

    /// The encoded size in bytes.
    pub fn encoded_bytes(&self) -> usize {
        self.bytes.len()
    }

    /// The values.
    ///
    /// # Errors
    ///
    /// Damaged bytes.
    pub fn decode(&self) -> Result<Vec<Vec<u8>>, DecodeError> {
        let Some(stats) = &self.stats else {
            return Ok(Vec::new());
        };
        if stats.count > MAX_CHUNK_VALUES {
            return Err(damaged("a chunk holds more values than a chunk may"));
        }
        let count = usize::try_from(stats.count).map_err(|_| damaged("a chunk is too long"))?;
        match self.encoding {
            StringEncoding::Plain => read_plain(&self.bytes, count),
            StringEncoding::SnappyPlain => read_plain(&snappy_decompress(&self.bytes, MOST_CHUNK_BYTES)?, count),
            StringEncoding::Dictionary => {
                let mut at = 0;
                let distinct = usize::try_from(read_varint(&self.bytes, &mut at)?).map_err(|_| damaged("a dictionary is too long"))?;
                if distinct > count {
                    return Err(damaged("a dictionary has more values than its chunk"));
                }
                let mut entries = Vec::with_capacity(distinct);
                for _ in 0..distinct {
                    entries.push(read_bytes(&self.bytes, &mut at)?);
                }
                let width = *self.bytes.get(at).ok_or_else(|| damaged("a dictionary has no bit width"))?;
                let (indexes, used) = decode_hybrid(self.bytes.get(at + 1..).unwrap_or_default(), width, count)?;
                if at + 1 + used != self.bytes.len() {
                    return Err(damaged("a dictionary chunk has bytes left over"));
                }
                indexes
                    .into_iter()
                    .map(|index| {
                        usize::try_from(index)
                            .ok()
                            .and_then(|index| entries.get(index).cloned())
                            .ok_or_else(|| damaged("a row points past its dictionary"))
                    })
                    .collect()
            }
        }
    }

    /// The chunk as bytes: its encoding, statistics and data.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = vec![match self.encoding {
            StringEncoding::Plain => 0,
            StringEncoding::Dictionary => 1,
            StringEncoding::SnappyPlain => 2,
        }];
        match &self.stats {
            None => write_varint(0, &mut out),
            Some(stats) => {
                write_varint(stats.count, &mut out);
                write_bytes(&stats.min, &mut out);
                write_bytes(&stats.max, &mut out);
            }
        }
        out.extend_from_slice(&self.bytes);
        out
    }

    /// Reads what [`StringChunk::to_bytes`] wrote, checking the data against its statistics.
    ///
    /// # Errors
    ///
    /// Damaged bytes, or values that do not match the statistics.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        let (&tag, rest) = bytes.split_first().ok_or_else(|| damaged("a chunk is empty"))?;
        let encoding = match tag {
            0 => StringEncoding::Plain,
            1 => StringEncoding::Dictionary,
            2 => StringEncoding::SnappyPlain,
            _ => return Err(damaged("a string chunk has an unknown encoding")),
        };
        let mut at = 0;
        let count = read_varint(rest, &mut at)?;
        let stats = if count == 0 {
            None
        } else {
            let min = read_bytes(rest, &mut at)?;
            let max = read_bytes(rest, &mut at)?;
            Some(StringStats { min, max, count })
        };
        let chunk = Self {
            encoding,
            stats: stats.clone(),
            bytes: rest.get(at..).unwrap_or_default().to_vec(),
        };
        let values = chunk.decode()?;
        if stats != stats_of(&values) {
            return Err(damaged("a chunk's values do not match its statistics"));
        }
        Ok(chunk)
    }
}
