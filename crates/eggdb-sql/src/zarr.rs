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
}

struct Layout {
    shape: Vec<usize>,
    chunks: Vec<usize>,
    kind: Kind,
    big_endian: bool,
    codec: Codec,
    fill: f64,
    key: Box<dyn Fn(&[usize]) -> String>,
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
    })
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
    let mut big_endian = false;
    let mut codec = Codec::Raw;
    if let Some(Json::Array(codecs)) = member(metadata, "codecs").map(|node| &node.value) {
        for entry in codecs {
            match text(member(entry, "name")).unwrap_or_default() {
                "bytes" => {
                    big_endian = text(member(entry, "configuration").and_then(|configuration| member(configuration, "endian"))) == Some("big");
                }
                "crc32c" => {}
                other => codec = codec_named(other)?,
            }
        }
    }
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
    let chunk_elements: usize = layout.chunks.iter().product();
    let grid: Vec<usize> = layout.shape.iter().zip(&layout.chunks).map(|(length, chunk)| length.div_ceil(*chunk)).collect();
    let mut out = Vec::with_capacity(total);
    let mut chunk_index = vec![0usize; grid.len()];
    let chunk_count: usize = grid.iter().product();
    for _ in 0..chunk_count {
        let key = (layout.key)(&chunk_index);
        let data = match read(&format!("{base}/{key}")) {
            Ok(raw) => Some(match layout.codec {
                Codec::Raw => raw,
                Codec::Zlib => eggdb_content::zlib_inflate(&raw, chunk_elements * width).map_err(|error| invalid(error))?,
                Codec::Gzip => eggdb_content::gunzip(&raw, chunk_elements * width).map_err(|error| invalid(error))?,
                Codec::Zstd => eggdb_storage::decompress_zstd(&raw, chunk_elements * width).map_err(invalid)?,
            }),
            Err(_) => None,
        };
        let mut within = vec![0usize; grid.len()];
        for element in 0..chunk_elements {
            let coordinates: Vec<usize> = chunk_index.iter().zip(&layout.chunks).zip(&within).map(|((chunk, size), at)| chunk * size + at).collect();
            if coordinates.iter().zip(&layout.shape).all(|(at, length)| at < length) {
                let value = match &data {
                    Some(bytes) => bytes
                        .get(element * width..element * width + width)
                        .map_or(layout.fill, |raw| read_value(layout.kind, layout.big_endian, raw)),
                    None => layout.fill,
                };
                out.push((coordinates.iter().map(|at| *at as i64).collect(), value));
            }
            advance(&mut within, &layout.chunks);
        }
        advance(&mut chunk_index, &grid);
    }
    Ok(out)
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
}
