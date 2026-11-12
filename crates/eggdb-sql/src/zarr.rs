use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{Json, Node, parse_document};

const MOST_ELEMENTS: usize = 10_000_000;
const MOST_RANK: usize = 32;

fn invalid(message: impl std::fmt::Display) -> SqlError {
    SqlError::new(SqlState::DATA_CORRUPTED, format!("invalid Zarr array: {message}"))
}

fn member<'n>(node: &'n Node, key: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members.iter().find(|(name, _)| name == key).map(|(_, value)| value),
        _ => None,
    }
}

fn text<'n>(node: Option<&'n Node>) -> Option<&'n str> {
    match &node?.value {
        Json::String(value) => Some(value),
        _ => None,
    }
}

fn sizes(node: Option<&Node>, what: &str) -> SqlResult<Vec<usize>> {
    let Some(Json::Array(items)) = node.map(|node| &node.value) else {
        return Err(invalid(format!("{what} is missing")));
    };
    if items.len() > MOST_RANK {
        return Err(invalid("too many dimensions"));
    }
    items
        .iter()
        .map(|item| match &item.value {
            Json::Number(number) => number.parse::<usize>().map_err(|_| invalid(format!("{what} holds {number}"))),
            _ => Err(invalid(format!("{what} holds a non-number"))),
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Boolean,
    Signed(usize),
    Unsigned(usize),
    Float(usize),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Codec {
    Raw,
    Zlib,
    Gzip,
    Zstd,
    Blosc,
}

struct Sharding {
    inner: Vec<usize>,
    codec: Codec,
    big_endian: bool,
    index_at_start: bool,
    index_checksum: bool,
}

struct Layout {
    shape: Vec<usize>,
    chunks: Vec<usize>,
    kind: Kind,
    big_endian: bool,
    codec: Codec,
    fill: f64,
    key: Box<dyn Fn(&[usize]) -> String>,
    sharding: Option<Sharding>,
}

fn fill_value(node: Option<&Node>) -> f64 {
    match node.map(|node| &node.value) {
        Some(Json::Number(number)) => number.parse().unwrap_or(0.0),
        Some(Json::String(word)) if word == "NaN" => f64::NAN,
        Some(Json::String(word)) if word == "Infinity" => f64::INFINITY,
        Some(Json::String(word)) if word == "-Infinity" => f64::NEG_INFINITY,
        Some(Json::Boolean(flag)) => f64::from(u8::from(*flag)),
        _ => 0.0,
    }
}

fn codec_named(name: &str) -> SqlResult<Codec> {
    Ok(match name {
        "zlib" => Codec::Zlib,
        "gzip" => Codec::Gzip,
        "zstd" => Codec::Zstd,
        "blosc" => Codec::Blosc,
        other => return Err(invalid(format!("the {other} compressor is not supported yet"))),
    })
}

fn version_two(metadata: &Node) -> SqlResult<Layout> {
    let dtype = text(member(metadata, "dtype")).ok_or_else(|| invalid("dtype is missing"))?;
    let (order, code) = dtype.split_at(dtype.len().min(1));
    let width = code.get(1..).and_then(|digits| digits.parse::<usize>().ok()).unwrap_or(1);
    let kind = match code.chars().next() {
        Some('b') => Kind::Boolean,
        Some('i') => Kind::Signed(width),
        Some('u') => Kind::Unsigned(width),
        Some('f') => Kind::Float(width),
        _ => return Err(invalid(format!("dtype {dtype} is not supported yet"))),
    };
    if text(member(metadata, "order")).is_some_and(|order| order != "C") {
        return Err(invalid("Fortran-ordered arrays are not supported yet"));
    }
    if member(metadata, "filters").is_some_and(|filters| filters.value != Json::Null) {
        return Err(invalid("filters are not supported yet"));
    }
    let codec = match member(metadata, "compressor") {
        None => Codec::Raw,
        Some(node) if node.value == Json::Null => Codec::Raw,
        Some(node) => codec_named(text(member(node, "id")).unwrap_or_default())?,
    };
    let separator = text(member(metadata, "dimension_separator")).unwrap_or(".").to_owned();
    Ok(Layout {
        shape: sizes(member(metadata, "shape"), "shape")?,
        chunks: sizes(member(metadata, "chunks"), "chunks")?,
        kind,
        big_endian: order == ">",
        codec,
        fill: fill_value(member(metadata, "fill_value")),
        key: Box::new(move |index: &[usize]| {
            if index.is_empty() {
                "0".to_owned()
            } else {
                index.iter().map(usize::to_string).collect::<Vec<_>>().join(&separator)
            }
        }),
        sharding: None,
    })
}

fn chunk_codecs(node: Option<&Node>) -> SqlResult<(bool, Codec, Option<Sharding>)> {
    let mut big_endian = false;
    let mut codec = Codec::Raw;
    let mut sharding = None;
    if let Some(Json::Array(codecs)) = node.map(|node| &node.value) {
        for entry in codecs {
            let configuration = member(entry, "configuration");
            match text(member(entry, "name")).unwrap_or_default() {
                "bytes" => {
                    big_endian = text(configuration.and_then(|configuration| member(configuration, "endian"))) == Some("big");
                }
                "crc32c" => {}
                "sharding_indexed" => {
                    let configuration = configuration.ok_or_else(|| invalid("sharding has no configuration"))?;
                    let inner = sizes(member(configuration, "chunk_shape"), "the shard's chunk_shape")?;
                    let (inner_big_endian, inner_codec, nested) = chunk_codecs(member(configuration, "codecs"))?;
                    if nested.is_some() {
                        return Err(invalid("nested sharding is not supported yet"));
                    }
                    let index_checksum = match member(configuration, "index_codecs").map(|node| &node.value) {
                        Some(Json::Array(index_codecs)) => index_codecs.iter().any(|codec| text(member(codec, "name")) == Some("crc32c")),
                        _ => false,
                    };
                    sharding = Some(Sharding {
                        inner,
                        codec: inner_codec,
                        big_endian: inner_big_endian,
                        index_at_start: text(member(configuration, "index_location")) == Some("start"),
                        index_checksum,
                    });
                }
                other => codec = codec_named(other)?,
            }
        }
    }
    Ok((big_endian, codec, sharding))
}

fn version_three(metadata: &Node) -> SqlResult<Layout> {
    let kind = match text(member(metadata, "data_type")).unwrap_or_default() {
        "bool" => Kind::Boolean,
        "int8" => Kind::Signed(1),
        "int16" => Kind::Signed(2),
        "int32" => Kind::Signed(4),
        "int64" => Kind::Signed(8),
        "uint8" => Kind::Unsigned(1),
        "uint16" => Kind::Unsigned(2),
        "uint32" => Kind::Unsigned(4),
        "uint64" => Kind::Unsigned(8),
        "float32" => Kind::Float(4),
        "float64" => Kind::Float(8),
        other => return Err(invalid(format!("data type {other} is not supported yet"))),
    };
    let grid = member(metadata, "chunk_grid").and_then(|grid| member(grid, "configuration"));
    let chunks = sizes(grid.and_then(|configuration| member(configuration, "chunk_shape")), "chunk_shape")?;
    let (big_endian, codec, sharding) = chunk_codecs(member(metadata, "codecs"))?;
    let encoding = member(metadata, "chunk_key_encoding");
    let v2_keys = text(encoding.and_then(|encoding| member(encoding, "name"))) == Some("v2");
    let separator = text(encoding.and_then(|encoding| member(encoding, "configuration")).and_then(|configuration| member(configuration, "separator")))
        .unwrap_or(if v2_keys { "." } else { "/" })
        .to_owned();
    Ok(Layout {
        shape: sizes(member(metadata, "shape"), "shape")?,
        chunks,
        kind,
        big_endian,
        codec,
        fill: fill_value(member(metadata, "fill_value")),
        key: Box::new(move |index: &[usize]| {
            let joined = index.iter().map(usize::to_string).collect::<Vec<_>>().join(&separator);
            if v2_keys {
                if joined.is_empty() { "0".to_owned() } else { joined }
            } else if joined.is_empty() {
                "c".to_owned()
            } else {
                format!("c{separator}{joined}")
            }
        }),
        sharding,
    })
}

fn element_width(kind: Kind) -> usize {
    match kind {
        Kind::Boolean => 1,
        Kind::Signed(width) | Kind::Unsigned(width) | Kind::Float(width) => width,
    }
}

fn read_value(kind: Kind, big_endian: bool, raw: &[u8]) -> f64 {
    let mut bytes = raw.to_vec();
    if !big_endian {
        bytes.reverse();
    }
    let unsigned = bytes.iter().fold(0u64, |value, byte| value << 8 | u64::from(*byte));
    match kind {
        Kind::Boolean => f64::from(u8::from(unsigned != 0)),
        Kind::Unsigned(_) => unsigned as f64,
        Kind::Signed(width) => {
            let shift = 64 - 8 * width as u32;
            ((unsigned << shift) as i64 >> shift) as f64
        }
        Kind::Float(4) => f64::from(f32::from_bits(unsigned as u32)),
        Kind::Float(_) => f64::from_bits(unsigned),
    }
}

/// Every element of a Zarr array (v2's `.zarray` or v3's `zarr.json`) at `path`, read through
/// `read` (which fails for a chunk that is absent, read as the fill value): its coordinates and
/// value, chunk by chunk in C order.
pub(crate) fn elements(path: &str, read: &dyn Fn(&str) -> SqlResult<Vec<u8>>) -> SqlResult<Vec<(Vec<i64>, f64)>> {
    let base = path.trim_end_matches('/');
    let (metadata, three) = match read(&format!("{base}/zarr.json")) {
        Ok(bytes) => (bytes, true),
        Err(_) => (read(&format!("{base}/.zarray"))?, false),
    };
    let metadata = String::from_utf8(metadata).map_err(|_| invalid("the metadata is not UTF-8"))?;
    let metadata = parse_document(&metadata).map_err(|error| invalid(error.message))?;
    let layout = if three { version_three(&metadata)? } else { version_two(&metadata)? };
    if layout.chunks.len() != layout.shape.len() || layout.chunks.contains(&0) {
        return Err(invalid("chunks and shape do not match"));
    }
    let total = layout.shape.iter().try_fold(1usize, |total, length| total.checked_mul(*length)).filter(|total| *total <= MOST_ELEMENTS)
        .ok_or_else(|| invalid(format!("an array may hold at most {MOST_ELEMENTS} elements")))?;
    let width = element_width(layout.kind);
    let (unit, codec, big_endian, per_shard) = match &layout.sharding {
        None => (layout.chunks.clone(), layout.codec, layout.big_endian, None),
        Some(sharding) => {
            if sharding.inner.len() != layout.chunks.len()
                || sharding.inner.contains(&0)
                || layout.chunks.iter().zip(&sharding.inner).any(|(shard, inner)| shard % inner != 0)
            {
                return Err(invalid("the shard's chunk_shape does not divide the shard"));
            }
            let per_shard: Vec<usize> = layout.chunks.iter().zip(&sharding.inner).map(|(shard, inner)| shard / inner).collect();
            (sharding.inner.clone(), sharding.codec, sharding.big_endian, Some(per_shard))
        }
    };
    let chunk_elements: usize = unit.iter().product();
    let chunk_bytes = chunk_elements.checked_mul(width).ok_or_else(|| invalid("a chunk is too large"))?;
    let grid: Vec<usize> = layout.shape.iter().zip(&unit).map(|(length, chunk)| length.div_ceil(*chunk)).collect();
    let mut out = Vec::with_capacity(total);
    let mut chunk_index = vec![0usize; grid.len()];
    let chunk_count: usize = grid.iter().product();
    let mut shard: Option<(String, Option<Vec<u8>>)> = None;
    for _ in 0..chunk_count {
        let encoded = match (&layout.sharding, &per_shard) {
            (Some(sharding), Some(per_shard)) => {
                let shard_index: Vec<usize> = chunk_index.iter().zip(per_shard).map(|(at, count)| at / count).collect();
                let within: Vec<usize> = chunk_index.iter().zip(per_shard).map(|(at, count)| at % count).collect();
                let key = (layout.key)(&shard_index);
                if shard.as_ref().is_none_or(|(held, _)| *held != key) {
                    let bytes = read(&format!("{base}/{key}")).ok();
                    shard = Some((key, bytes));
                }
                match shard.as_ref().and_then(|(_, bytes)| bytes.as_deref()) {
                    Some(bytes) => inner_chunk(bytes, sharding, per_shard, &within)?.map(<[u8]>::to_vec),
                    None => None,
                }
            }
            _ => read(&format!("{base}/{}", (layout.key)(&chunk_index))).ok(),
        };
        let data = match encoded {
            Some(raw) => Some(decode(codec, raw, chunk_bytes)?),
            None => None,
        };
        let mut within = vec![0usize; grid.len()];
        for element in 0..chunk_elements {
            let coordinates: Vec<usize> = chunk_index.iter().zip(&unit).zip(&within).map(|((chunk, size), at)| chunk * size + at).collect();
            if coordinates.iter().zip(&layout.shape).all(|(at, length)| at < length) {
                let value = match &data {
                    Some(bytes) => bytes
                        .get(element * width..element * width + width)
                        .map_or(layout.fill, |raw| read_value(layout.kind, big_endian, raw)),
                    None => layout.fill,
                };
                out.push((coordinates.iter().map(|at| *at as i64).collect(), value));
            }
            advance(&mut within, &unit);
        }
        advance(&mut chunk_index, &grid);
    }
    Ok(out)
}

fn decode(codec: Codec, raw: Vec<u8>, chunk_bytes: usize) -> SqlResult<Vec<u8>> {
    Ok(match codec {
        Codec::Raw => raw,
        Codec::Zlib => eggdb_content::zlib_inflate(&raw, chunk_bytes).map_err(|error| invalid(error))?,
        Codec::Gzip => eggdb_content::gunzip(&raw, chunk_bytes).map_err(|error| invalid(error))?,
        Codec::Zstd => eggdb_storage::decompress_zstd(&raw, chunk_bytes).map_err(invalid)?,
        Codec::Blosc => crate::blosc::decompress(&raw, chunk_bytes).map_err(invalid)?,
    })
}

const EMPTY_INNER_CHUNK: u64 = u64::MAX;

fn inner_chunk<'s>(shard: &'s [u8], sharding: &Sharding, per_shard: &[usize], within: &[usize]) -> SqlResult<Option<&'s [u8]>> {
    let count: usize = per_shard.iter().product();
    let index_bytes = count
        .checked_mul(16)
        .and_then(|bytes| bytes.checked_add(if sharding.index_checksum { 4 } else { 0 }))
        .filter(|bytes| *bytes <= shard.len())
        .ok_or_else(|| invalid("the shard is shorter than its index"))?;
    let index = if sharding.index_at_start { &shard[..index_bytes] } else { &shard[shard.len() - index_bytes..] };
    if sharding.index_checksum {
        let (entries, stored) = index.split_at(count * 16);
        if eggdb_storage::crc32c(entries).to_le_bytes() != stored {
            return Err(invalid("the shard index fails its CRC-32C"));
        }
    }
    let position = within.iter().zip(per_shard).fold(0usize, |position, (at, count)| position * count + at);
    let entry = &index[position * 16..position * 16 + 16];
    let offset = u64::from_le_bytes(entry[..8].try_into().map_err(|_| invalid("the shard index is short"))?);
    let length = u64::from_le_bytes(entry[8..].try_into().map_err(|_| invalid("the shard index is short"))?);
    if offset == EMPTY_INNER_CHUNK && length == EMPTY_INNER_CHUNK {
        return Ok(None);
    }
    let start = usize::try_from(offset).map_err(|_| invalid("a shard offset is too large"))?;
    let length = usize::try_from(length).map_err(|_| invalid("a shard length is too large"))?;
    start
        .checked_add(length)
        .and_then(|end| shard.get(start..end))
        .map(Some)
        .ok_or_else(|| invalid("an inner chunk runs past its shard"))
}

fn advance(point: &mut [usize], shape: &[usize]) {
    for axis in (0..point.len()).rev() {
        point[axis] += 1;
        if point[axis] < shape[axis] {
            return;
        }
        point[axis] = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_two_arrays_read_chunks_and_fill_missing_ones() {
        let files = |path: &str| -> SqlResult<Vec<u8>> {
            match path {
                "a/.zarray" => Ok(br#"{"zarr_format":2,"shape":[3],"chunks":[2],"dtype":"<i2","compressor":null,"fill_value":9,"order":"C","filters":null}"#.to_vec()),
                "a/0" => Ok([1i16, -2].iter().flat_map(|value| value.to_le_bytes()).collect()),
                _ => Err(invalid("absent")),
            }
        };
        let found = elements("a", &files).unwrap();
        assert_eq!(found, vec![(vec![0], 1.0), (vec![1], -2.0), (vec![2], 9.0)]);
    }

    fn shard_of(chunks: &[Option<Vec<u8>>]) -> Vec<u8> {
        let mut data = Vec::new();
        let mut index = Vec::new();
        for chunk in chunks {
            match chunk {
                Some(bytes) => {
                    index.extend_from_slice(&(data.len() as u64).to_le_bytes());
                    index.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                    data.extend_from_slice(bytes);
                }
                None => {
                    index.extend_from_slice(&u64::MAX.to_le_bytes());
                    index.extend_from_slice(&u64::MAX.to_le_bytes());
                }
            }
        }
        let checksum = eggdb_storage::crc32c(&index);
        data.extend_from_slice(&index);
        data.extend_from_slice(&checksum.to_le_bytes());
        data
    }

    #[test]
    fn version_three_shards_hold_inner_chunks_and_empty_ones_fill() {
        let shard = shard_of(&[Some(vec![1, 2]), None]);
        let metadata = br#"{"zarr_format":3,"node_type":"array","shape":[5],"data_type":"uint8","chunk_grid":{"name":"regular","configuration":{"chunk_shape":[4]}},"chunk_key_encoding":{"name":"default"},"fill_value":7,"codecs":[{"name":"sharding_indexed","configuration":{"chunk_shape":[2],"codecs":[{"name":"bytes"}],"index_codecs":[{"name":"bytes","configuration":{"endian":"little"}},{"name":"crc32c"}],"index_location":"end"}}]}"#;
        let files = |path: &str| -> SqlResult<Vec<u8>> {
            match path {
                "s/zarr.json" => Ok(metadata.to_vec()),
                "s/c/0" => Ok(shard.clone()),
                _ => Err(invalid("absent")),
            }
        };
        let values: Vec<f64> = elements("s", &files).unwrap().into_iter().map(|(_, value)| value).collect();
        assert_eq!(values, vec![1.0, 2.0, 7.0, 7.0, 7.0]);
        let mut broken = shard_of(&[Some(vec![1, 2]), None]);
        let last = broken.len() - 1;
        broken[last] ^= 1;
        let broken_files = |path: &str| -> SqlResult<Vec<u8>> {
            match path {
                "s/zarr.json" => Ok(metadata.to_vec()),
                "s/c/0" => Ok(broken.clone()),
                _ => Err(invalid("absent")),
            }
        };
        assert!(elements("s", &broken_files).is_err());
    }

    #[test]
    fn version_two_blosc_chunks_decode() {
        let mut chunk = vec![2u8, 1, 0x02, 2];
        chunk.extend_from_slice(&4u32.to_le_bytes());
        chunk.extend_from_slice(&4u32.to_le_bytes());
        chunk.extend_from_slice(&20u32.to_le_bytes());
        chunk.extend_from_slice(&[5, 0, 6, 0]);
        let files = |path: &str| -> SqlResult<Vec<u8>> {
            match path {
                "b/.zarray" => Ok(br#"{"zarr_format":2,"shape":[2],"chunks":[2],"dtype":"<u2","compressor":{"id":"blosc","cname":"lz4","clevel":5,"shuffle":1},"fill_value":0,"order":"C","filters":null}"#.to_vec()),
                "b/0" => Ok(chunk.clone()),
                _ => Err(invalid("absent")),
            }
        };
        let values: Vec<f64> = elements("b", &files).unwrap().into_iter().map(|(_, value)| value).collect();
        assert_eq!(values, vec![5.0, 6.0]);
    }
}
