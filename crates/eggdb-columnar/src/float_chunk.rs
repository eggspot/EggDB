use crate::chunk::MAX_CHUNK_VALUES;
use crate::delta::{read_varint, write_varint};
use crate::{DecodeError, damaged};

/// How a [`FloatChunk`] stores its values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatEncoding {
    /// Eight little-endian bytes per value.
    Plain,
    /// Each value's bits XORed with the previous value's, the zero bits around the difference
    /// left out, as Gorilla time series compression does: a repeated value costs one bit.
    Xor,
}

/// The least and greatest value, the sum and the count of a float chunk; NaN values are left
/// out of the least and greatest, which are NaN only when every value is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloatStats {
    /// The least value that is not NaN.
    pub min: f64,
    /// The greatest value that is not NaN.
    pub max: f64,
    /// The sum.
    pub sum: f64,
    /// How many values.
    pub count: u64,
}

/// A chunk of a 64-bit float column, encoded with whichever encoding is smaller. Values come
/// back bit for bit, NaN payloads and negative zero included.
#[derive(Debug, Clone, PartialEq)]
pub struct FloatChunk {
    encoding: FloatEncoding,
    stats: Option<FloatStats>,
    bytes: Vec<u8>,
}

struct BitWriter {
    bytes: Vec<u8>,
    used: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self { bytes: Vec::new(), used: 8 }
    }

    fn push(&mut self, value: u64, width: u8) {
        for shift in (0..width).rev() {
            if self.used == 8 {
                self.bytes.push(0);
                self.used = 0;
            }
            let bit = ((value >> shift) & 1) as u8;
            if let Some(last) = self.bytes.last_mut() {
                *last |= bit << (7 - self.used);
            }
            self.used += 1;
        }
    }
}

struct BitReader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl BitReader<'_> {
    fn take(&mut self, width: u8) -> Result<u64, DecodeError> {
        let mut value = 0u64;
        for _ in 0..width {
            let byte = self
                .bytes
                .get(self.at / 8)
                .ok_or_else(|| damaged("float bits run past their bytes"))?;
            let bit = (byte >> (7 - (self.at % 8))) & 1;
            value = (value << 1) | u64::from(bit);
            self.at += 1;
        }
        Ok(value)
    }
}

fn xor_encode(values: &[f64]) -> Vec<u8> {
    let mut writer = BitWriter::new();
    let mut previous = 0u64;
    let mut window: Option<(u8, u8)> = None;
    for (at, value) in values.iter().enumerate() {
        let bits = value.to_bits();
        if at == 0 {
            writer.push(bits, 64);
            previous = bits;
            continue;
        }
        let difference = bits ^ previous;
        previous = bits;
        if difference == 0 {
            writer.push(0, 1);
            continue;
        }
        writer.push(1, 1);
        let leading = (difference.leading_zeros() as u8).min(63);
        let trailing = difference.trailing_zeros() as u8;
        match window {
            Some((known_leading, known_trailing)) if leading >= known_leading && trailing >= known_trailing => {
                writer.push(0, 1);
                let length = 64 - known_leading - known_trailing;
                writer.push(difference >> known_trailing, length);
            }
            _ => {
                writer.push(1, 1);
                let length = 64 - leading - trailing;
                writer.push(u64::from(leading), 6);
                writer.push(u64::from(length - 1), 6);
                writer.push(difference >> trailing, length);
                window = Some((leading, trailing));
            }
        }
    }
    writer.bytes
}

fn xor_decode(bytes: &[u8], count: usize) -> Result<Vec<f64>, DecodeError> {
    let mut reader = BitReader { bytes, at: 0 };
    let mut values = Vec::with_capacity(count);
    let mut previous = 0u64;
    let mut window: Option<(u8, u8)> = None;
    for at in 0..count {
        if at == 0 {
            previous = reader.take(64)?;
            values.push(f64::from_bits(previous));
            continue;
        }
        if reader.take(1)? == 1 {
            let (leading, length) = if reader.take(1)? == 0 {
                let (leading, trailing) = window.ok_or_else(|| damaged("a float difference reuses a window never set"))?;
                (leading, 64 - leading - trailing)
            } else {
                let leading = reader.take(6)? as u8;
                let length = reader.take(6)? as u8 + 1;
                if u16::from(leading) + u16::from(length) > 64 {
                    return Err(damaged("a float difference is wider than 64 bits"));
                }
                window = Some((leading, 64 - leading - length));
                (leading, length)
            };
            let trailing = 64 - leading - length;
            let meaningful = reader.take(length)?;
            previous ^= meaningful << trailing;
        }
        values.push(f64::from_bits(previous));
    }
    Ok(values)
}

fn stats_of(values: &[f64]) -> Option<FloatStats> {
    if values.is_empty() {
        return None;
    }
    let numbers = values.iter().copied().filter(|value| !value.is_nan());
    let min = numbers.clone().reduce(f64::min).unwrap_or(f64::NAN);
    let max = numbers.reduce(f64::max).unwrap_or(f64::NAN);
    Some(FloatStats {
        min,
        max,
        sum: values.iter().sum(),
        count: values.len() as u64,
    })
}

fn same_bits(left: f64, right: f64) -> bool {
    left.to_bits() == right.to_bits()
}

impl FloatChunk {
    /// Encodes `values`, at most [`MAX_CHUNK_VALUES`] of them (the rest belong in the next
    /// chunk).
    pub fn encode(values: &[f64]) -> Self {
        let values = &values[..values.len().min(MAX_CHUNK_VALUES as usize)];
        let mut plain = Vec::with_capacity(values.len() * 8);
        for value in values {
            plain.extend_from_slice(&value.to_le_bytes());
        }
        let xor = xor_encode(values);
        let (encoding, bytes) = if xor.len() < plain.len() {
            (FloatEncoding::Xor, xor)
        } else {
            (FloatEncoding::Plain, plain)
        };
        Self {
            encoding,
            stats: stats_of(values),
            bytes,
        }
    }

    /// The encoding chosen.
    pub fn encoding(&self) -> FloatEncoding {
        self.encoding
    }

    /// The statistics, `None` for an empty chunk.
    pub fn stats(&self) -> Option<FloatStats> {
        self.stats
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
    pub fn decode(&self) -> Result<Vec<f64>, DecodeError> {
        let Some(stats) = self.stats else {
            return Ok(Vec::new());
        };
        if stats.count > MAX_CHUNK_VALUES {
            return Err(damaged("a chunk holds more values than a chunk may"));
        }
        let count = usize::try_from(stats.count).map_err(|_| damaged("a chunk is too long"))?;
        match self.encoding {
            FloatEncoding::Plain => {
                if self.bytes.len() != count * 8 {
                    return Err(damaged("a plain float chunk has the wrong length"));
                }
                Ok(self
                    .bytes
                    .chunks_exact(8)
                    .map(|word| f64::from_le_bytes(word.try_into().unwrap_or_default()))
                    .collect())
            }
            FloatEncoding::Xor => xor_decode(&self.bytes, count),
        }
    }

    /// The chunk as bytes: its encoding, statistics and data.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = vec![match self.encoding {
            FloatEncoding::Plain => 0,
            FloatEncoding::Xor => 1,
        }];
        match self.stats {
            None => write_varint(0, &mut out),
            Some(stats) => {
                write_varint(stats.count, &mut out);
                for value in [stats.min, stats.max, stats.sum] {
                    out.extend_from_slice(&value.to_le_bytes());
                }
            }
        }
        out.extend_from_slice(&self.bytes);
        out
    }

    /// Reads what [`FloatChunk::to_bytes`] wrote, checking the data against its statistics.
    ///
    /// # Errors
    ///
    /// Damaged bytes, or values that do not match the statistics.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        let (&tag, rest) = bytes.split_first().ok_or_else(|| damaged("a chunk is empty"))?;
        let encoding = match tag {
            0 => FloatEncoding::Plain,
            1 => FloatEncoding::Xor,
            _ => return Err(damaged("a float chunk has an unknown encoding")),
        };
        let mut at = 0;
        let count = read_varint(rest, &mut at)?;
        let stats = if count == 0 {
            None
        } else {
            let fixed = rest
                .get(at..at + 24)
                .ok_or_else(|| damaged("a chunk's statistics are cut short"))?;
            at += 24;
            let read = |from: usize| f64::from_le_bytes(fixed.get(from..from + 8).and_then(|word| word.try_into().ok()).unwrap_or_default());
            Some(FloatStats {
                min: read(0),
                max: read(8),
                sum: read(16),
                count,
            })
        };
        let chunk = Self {
            encoding,
            stats,
            bytes: rest.get(at..).unwrap_or_default().to_vec(),
        };
        let values = chunk.decode()?;
        if let (Some(stored), Some(found)) = (stats, stats_of(&values))
            && !(same_bits(stored.min, found.min) && same_bits(stored.max, found.max) && same_bits(stored.sum, found.sum))
        {
            return Err(damaged("a chunk's values do not match its statistics"));
        }
        Ok(chunk)
    }
}
