use crate::error::{SqlError, SqlResult, SqlState};

const MOST_ENTRIES: u64 = 4_096;
const MOST_VALUES: u64 = 1 << 20;
const PIXEL_SCALE: u16 = 33_550;
const TIE_POINT: u16 = 33_922;
const GEO_KEYS: u16 = 34_735;
const NO_DATA: u16 = 42_113;
const PROJECTED_CRS_KEY: u16 = 3_072;
const GEOGRAPHIC_CRS_KEY: u16 = 2_048;
const RASTER_TYPE_KEY: u16 = 1_025;

fn invalid(message: impl std::fmt::Display) -> SqlError {
    SqlError::new(SqlState::INVALID_TEXT_REPRESENTATION, format!("invalid TIFF file: {message}"))
}

struct Reader<'b> {
    bytes: &'b [u8],
    big_endian: bool,
}

impl Reader<'_> {
    fn slice(&self, at: u64, count: u64) -> SqlResult<&[u8]> {
        let start = usize::try_from(at).map_err(|_| invalid("an offset is too large"))?;
        let count = usize::try_from(count).map_err(|_| invalid("a field is too large"))?;
        self.bytes
            .get(start..start.checked_add(count).ok_or_else(|| invalid("a field is too large"))?)
            .ok_or_else(|| invalid("a field runs past the end"))
    }

    fn unsigned(&self, raw: &[u8]) -> u64 {
        let mut value = 0u64;
        if self.big_endian {
            for byte in raw {
                value = value << 8 | u64::from(*byte);
            }
        } else {
            for byte in raw.iter().rev() {
                value = value << 8 | u64::from(*byte);
            }
        }
        value
    }

    fn number(&self, at: u64, width: u64) -> SqlResult<u64> {
        Ok(self.unsigned(self.slice(at, width)?))
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Values {
    Integers(Vec<u64>),
    Reals(Vec<f64>),
    Text(String),
}

fn type_width(kind: u16) -> Option<u64> {
    Some(match kind {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 | 16 | 17 | 18 => 8,
        _ => return None,
    })
}

fn values(reader: &Reader<'_>, kind: u16, count: u64, at: u64) -> SqlResult<Option<Values>> {
    let Some(width) = type_width(kind) else {
        return Ok(None);
    };
    if count > MOST_VALUES {
        return Err(invalid("a tag holds too many values"));
    }
    let raw = reader.slice(at, width * count)?;
    Ok(Some(match kind {
        2 => Values::Text(String::from_utf8_lossy(raw).trim_end_matches('\\0').to_owned()),
        5 | 10 => Values::Reals(
            raw.chunks_exact(8)
                .map(|pair| {
                    let numerator = reader.unsigned(&pair[..4]) as f64;
                    let denominator = reader.unsigned(&pair[4..]) as f64;
                    if denominator == 0.0 { 0.0 } else { numerator / denominator }
                })
                .collect(),
        ),
        11 => Values::Reals(raw.chunks_exact(4).map(|chunk| f64::from(f32::from_bits(reader.unsigned(chunk) as u32))).collect()),
        12 => Values::Reals(raw.chunks_exact(8).map(|chunk| f64::from_bits(reader.unsigned(chunk))).collect()),
        _ => Values::Integers(raw.chunks_exact(width as usize).map(|chunk| reader.unsigned(chunk)).collect()),
    }))
}

fn first_integer(tags: &[(u16, Values)], tag: u16) -> Option<u64> {
    tags.iter().find(|(found, _)| *found == tag).and_then(|(_, values)| match values {
        Values::Integers(list) => list.first().copied(),
        Values::Reals(list) => list.first().map(|value| *value as u64),
        Values::Text(_) => None,
    })
}

fn reals(tags: &[(u16, Values)], tag: u16) -> Option<Vec<f64>> {
    tags.iter().find(|(found, _)| *found == tag).and_then(|(_, values)| match values {
        Values::Reals(list) => Some(list.clone()),
        Values::Integers(list) => Some(list.iter().map(|value| *value as f64).collect()),
        Values::Text(_) => None,
    })
}

fn json_number(value: f64) -> String {
    if value.is_finite() { format!("{value}") } else { "null".to_owned() }
}

fn directory(bytes: &[u8]) -> SqlResult<(Vec<(u16, Values)>, bool)> {
    let big_endian = match bytes.get(..2) {
        Some(b"II") => false,
        Some(b"MM") => true,
        _ => return Err(invalid("no byte-order mark")),
    };
    let reader = Reader { bytes, big_endian };
    let version = reader.number(2, 2)?;
    let (bigtiff, first) = match version {
        42 => (false, reader.number(4, 4)?),
        43 => (true, reader.number(8, 8)?),
        other => return Err(invalid(format!("version {other} is not TIFF"))),
    };
    let (count_width, entry_width, value_width) = if bigtiff { (8, 20, 8) } else { (2, 12, 4) };
    let entries = reader.number(first, count_width)?;
    if entries > MOST_ENTRIES {
        return Err(invalid("the first directory has too many entries"));
    }
    let mut tags = Vec::new();
    for index in 0..entries {
        let at = first + count_width + index * entry_width;
        let tag = reader.number(at, 2)? as u16;
        let kind = reader.number(at + 2, 2)? as u16;
        let count = reader.number(at + 4, value_width)?;
        let Some(width) = type_width(kind) else {
            continue;
        };
        let inline_at = at + 4 + value_width;
        let location = if width.saturating_mul(count) <= value_width { inline_at } else { reader.number(inline_at, value_width)? };
        if let Some(found) = values(&reader, kind, count, location)? {
            tags.push((tag, found));
        }
    }
    Ok((tags, bigtiff))
}

/// A TIFF or BigTIFF file's first image as JSON: its size, bands, bits and sample format,
/// compression, photometric interpretation, tiles or strips, and, for a GeoTIFF, the pixel
/// scale, the tie point, the EPSG code of its coordinate reference system, whether pixels are
/// areas or points, the no-data value and the bounding box `[west, south, east, north]` the tie
/// point and scale give.
pub(crate) fn info(bytes: &[u8]) -> SqlResult<String> {
    let (tags, bigtiff) = directory(bytes)?;
    let width = first_integer(&tags, 256).ok_or_else(|| invalid("the image has no width"))?;
    let height = first_integer(&tags, 257).ok_or_else(|| invalid("the image has no height"))?;
    let mut members = vec![
        format!("\"width\":{width}"),
        format!("\"height\":{height}"),
        format!("\"bands\":{}", first_integer(&tags, 277).unwrap_or(1)),
        format!("\"bits_per_sample\":{}", first_integer(&tags, 258).unwrap_or(1)),
        format!(
            "\"sample_format\":\"{}\"",
            match first_integer(&tags, 339).unwrap_or(1) {
                2 => "signed",
                3 => "float",
                _ => "unsigned",
            }
        ),
        format!(
            "\"compression\":\"{}\"",
            match first_integer(&tags, 259).unwrap_or(1) {
                1 => "none".to_owned(),
                5 => "lzw".to_owned(),
                7 => "jpeg".to_owned(),
                8 | 32_946 => "deflate".to_owned(),
                32_773 => "packbits".to_owned(),
                50_000 => "zstd".to_owned(),
                other => other.to_string(),
            }
        ),
        format!("\"bigtiff\":{bigtiff}"),
    ];
    if let Some(photometric) = first_integer(&tags, 262) {
        members.push(format!("\"photometric\":{photometric}"));
    }
    match (first_integer(&tags, 322), first_integer(&tags, 323)) {
        (Some(tile_width), Some(tile_height)) => members.push(format!("\"tiles\":[{tile_width},{tile_height}]")),
        _ => {
            if let Some(rows) = first_integer(&tags, 278) {
                members.push(format!("\"rows_per_strip\":{rows}"));
            }
        }
    }
    let scale = reals(&tags, PIXEL_SCALE);
    let tie = reals(&tags, TIE_POINT);
    if let Some(scale) = &scale {
        members.push(format!("\"pixel_scale\":[{}]", scale.iter().map(|value| json_number(*value)).collect::<Vec<_>>().join(",")));
    }
    if let Some(tie) = &tie {
        members.push(format!("\"tie_point\":[{}]", tie.iter().map(|value| json_number(*value)).collect::<Vec<_>>().join(",")));
    }
    if let Some(Values::Integers(keys)) = tags.iter().find(|(tag, _)| *tag == GEO_KEYS).map(|(_, values)| values) {
        let key = |wanted: u16| {
            keys.chunks_exact(4).skip(1).find(|entry| entry[0] == u64::from(wanted) && entry[1] == 0).map(|entry| entry[3])
        };
        if let Some(epsg) = key(PROJECTED_CRS_KEY).or_else(|| key(GEOGRAPHIC_CRS_KEY)).filter(|code| *code != 32_767) {
            members.push(format!("\"epsg\":{epsg}"));
        }
        if let Some(raster) = key(RASTER_TYPE_KEY) {
            members.push(format!("\"raster_type\":\"{}\"", if raster == 2 { "point" } else { "area" }));
        }
    }
    if let Some((_, Values::Text(no_data))) = tags.iter().find(|(tag, _)| *tag == NO_DATA) {
        let mut out = String::new();
        crate::json::write_string(no_data.trim(), &mut out);
        members.push(format!("\"no_data\":{out}"));
    }
    if let (Some(scale), Some(tie)) = (scale.filter(|scale| scale.len() >= 2), tie.filter(|tie| tie.len() >= 5)) {
        let west = tie[3] - tie[0] * scale[0];
        let north = tie[4] + tie[1] * scale[1];
        let east = west + width as f64 * scale[0];
        let south = north - height as f64 * scale[1];
        members.push(format!(
            "\"bbox\":[{},{},{},{}]",
            json_number(west),
            json_number(south),
            json_number(east),
            json_number(north)
        ));
    }
    Ok(format!("{{{}}}", members.join(",")))
}

const MOST_PIXEL_VALUES: usize = 50_000_000;

fn integers(tags: &[(u16, Values)], tag: u16) -> Option<Vec<u64>> {
    tags.iter().find(|(found, _)| *found == tag).and_then(|(_, values)| match values {
        Values::Integers(list) => Some(list.clone()),
        _ => None,
    })
}

fn lzw_decode(data: &[u8], most: usize) -> SqlResult<Vec<u8>> {
    const CLEAR: usize = 256;
    const END: usize = 257;
    let mut table: Vec<Vec<u8>> = (0..=255u8).map(|byte| vec![byte]).collect();
    table.push(Vec::new());
    table.push(Vec::new());
    let mut out = Vec::new();
    let mut width = 9u32;
    let mut bit = 0usize;
    let mut previous: Option<usize> = None;
    let read = |bit: &mut usize, width: u32| -> Option<usize> {
        let mut code = 0usize;
        for _ in 0..width {
            let byte = *data.get(*bit / 8)?;
            code = code << 1 | usize::from(byte >> (7 - *bit % 8) & 1);
            *bit += 1;
        }
        Some(code)
    };
    while let Some(code) = read(&mut bit, width) {
        if code == END {
            break;
        }
        if code == CLEAR {
            table.truncate(258);
            width = 9;
            previous = None;
            continue;
        }
        let entry = if code < table.len() {
            table[code].clone()
        } else if let Some(previous) = previous.filter(|_| code == table.len()) {
            let mut entry = table[previous].clone();
            entry.push(table[previous][0]);
            entry
        } else {
            return Err(invalid("an LZW code is out of range"));
        };
        out.extend_from_slice(&entry);
        if out.len() > most {
            return Err(invalid("a strip decodes past its size"));
        }
        if let Some(previous) = previous {
            let mut added = table[previous].clone();
            added.push(entry[0]);
            table.push(added);
        }
        previous = Some(code);
        width = match table.len() + 1 {
            length if length >= 2048 => 12,
            length if length >= 1024 => 11,
            length if length >= 512 => 10,
            _ => 9,
        };
    }
    Ok(out)
}

/// A raster read from a TIFF's first image: its size and bands, its values row by row with
/// the bands of each pixel together, and, for a GeoTIFF, the map position of its top-left
/// corner and the pixel size.
pub(crate) struct Raster {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) bands: usize,
    pub(crate) values: Vec<f64>,
    pub(crate) origin: Option<(f64, f64, f64, f64)>,
}

/// The pixel values of a TIFF's first image: strips or tiles, uncompressed, deflate or LZW,
/// with the horizontal-differencing predictor, 8- to 64-bit integers or 32- and 64-bit floats,
/// pixels interleaved (planar configuration 1).
pub(crate) fn raster(bytes: &[u8]) -> SqlResult<Raster> {
    let (tags, _) = directory(bytes)?;
    let big_endian = bytes.get(..2) == Some(b"MM");
    let width = usize::try_from(first_integer(&tags, 256).ok_or_else(|| invalid("the image has no width"))?).map_err(|_| invalid("too wide"))?;
    let height = usize::try_from(first_integer(&tags, 257).ok_or_else(|| invalid("the image has no height"))?).map_err(|_| invalid("too tall"))?;
    let bands = usize::try_from(first_integer(&tags, 277).unwrap_or(1)).map_err(|_| invalid("too many bands"))?.max(1);
    let bits = first_integer(&tags, 258).unwrap_or(8);
    let format = first_integer(&tags, 339).unwrap_or(1);
    let compression = first_integer(&tags, 259).unwrap_or(1);
    let predictor = first_integer(&tags, 317).unwrap_or(1);
    if first_integer(&tags, 284).unwrap_or(1) != 1 {
        return Err(invalid("planar (band-separate) images are not supported yet"));
    }
    if !matches!(bits, 8 | 16 | 32 | 64) || (format == 3 && !matches!(bits, 32 | 64)) {
        return Err(invalid(format!("{bits}-bit samples are not supported yet")));
    }
    if !matches!(predictor, 1 | 2) {
        return Err(invalid("the floating-point predictor is not supported yet"));
    }
    let total = width.checked_mul(height).and_then(|pixels| pixels.checked_mul(bands)).filter(|total| *total <= MOST_PIXEL_VALUES)
        .ok_or_else(|| invalid(format!("an image may hold at most {MOST_PIXEL_VALUES} values")))?;
    let sample_bytes = (bits / 8) as usize;
    let (block_width, block_height, offsets, counts) = match (first_integer(&tags, 322), first_integer(&tags, 323)) {
        (Some(tile_width), Some(tile_height)) => (
            tile_width as usize,
            tile_height as usize,
            integers(&tags, 324).ok_or_else(|| invalid("tiles without offsets"))?,
            integers(&tags, 325).ok_or_else(|| invalid("tiles without byte counts"))?,
        ),
        _ => (
            width,
            first_integer(&tags, 278).map_or(height, |rows| rows as usize).max(1),
            integers(&tags, 273).ok_or_else(|| invalid("strips without offsets"))?,
            integers(&tags, 279).ok_or_else(|| invalid("strips without byte counts"))?,
        ),
    };
    if block_width == 0 || block_height == 0 {
        return Err(invalid("a tile has no size"));
    }
    let across = width.div_ceil(block_width);
    let mut values = vec![0f64; total];
    let block_bytes = block_width * block_height * bands * sample_bytes;
    for (index, (offset, count)) in offsets.iter().zip(&counts).enumerate() {
        let start = usize::try_from(*offset).map_err(|_| invalid("an offset is too large"))?;
        let length = usize::try_from(*count).map_err(|_| invalid("a block is too large"))?;
        let raw = bytes.get(start..start.checked_add(length).ok_or_else(|| invalid("a block is too large"))?).ok_or_else(|| invalid("a block runs past the end"))?;
        let mut block = match compression {
            1 => raw.to_vec(),
            5 => lzw_decode(raw, block_bytes)?,
            8 | 32_946 => eggdb_content::zlib_inflate(raw, block_bytes).map_err(|error| invalid(error))?,
            other => return Err(invalid(format!("compression {other} is not supported yet"))),
        };
        block.resize(block_bytes, 0);
        let (block_x, block_y) = (index % across * block_width, index / across * block_height);
        for row in 0..block_height {
            let y = block_y + row;
            if y >= height {
                break;
            }
            let mut previous = vec![0i128; bands];
            for column in 0..block_width {
                let x = block_x + column;
                for band in 0..bands {
                    let at = ((row * block_width + column) * bands + band) * sample_bytes;
                    let raw = &block[at..at + sample_bytes];
                    let unsigned = {
                        let mut value = 0u64;
                        if big_endian {
                            for byte in raw {
                                value = value << 8 | u64::from(*byte);
                            }
                        } else {
                            for byte in raw.iter().rev() {
                                value = value << 8 | u64::from(*byte);
                            }
                        }
                        value
                    };
                    let mut value = match (format, bits) {
                        (3, 32) => f64::from(f32::from_bits(unsigned as u32)),
                        (3, _) => f64::from_bits(unsigned),
                        (2, 8) => f64::from(unsigned as u8 as i8),
                        (2, 16) => f64::from(unsigned as u16 as i16),
                        (2, 32) => f64::from(unsigned as u32 as i32),
                        (2, _) => unsigned as i64 as f64,
                        _ => unsigned as f64,
                    };
                    if predictor == 2 && format != 3 {
                        let modulus = 1i128 << bits;
                        let summed = (previous[band] + value as i128).rem_euclid(modulus);
                        previous[band] = summed;
                        value = if format == 2 && summed >= modulus / 2 { (summed - modulus) as f64 } else { summed as f64 };
                    }
                    if x < width {
                        values[(y * width + x) * bands + band] = value;
                    }
                }
            }
        }
    }
    let origin = match (reals(&tags, PIXEL_SCALE), reals(&tags, TIE_POINT)) {
        (Some(scale), Some(tie)) if scale.len() >= 2 && tie.len() >= 5 => {
            Some((tie[3] - tie[0] * scale[0], tie[4] + tie[1] * scale[1], scale[0], scale[1]))
        }
        _ => None,
    };
    Ok(Raster { width, height, bands, values, origin })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(out: &mut Vec<u8>, tag: u16, kind: u16, count: u32, value: u32) {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn geotiff_headers_give_size_crs_and_bounds() {
        let mut file = b"II".to_vec();
        file.extend_from_slice(&42u16.to_le_bytes());
        file.extend_from_slice(&8u32.to_le_bytes());
        let entries = 6u16;
        file.extend_from_slice(&entries.to_le_bytes());
        let data_at = 8 + 2 + u32::from(entries) * 12 + 4;
        entry(&mut file, 256, 3, 1, 100);
        entry(&mut file, 257, 3, 1, 50);
        entry(&mut file, 259, 3, 1, 1);
        entry(&mut file, PIXEL_SCALE, 12, 3, data_at);
        entry(&mut file, TIE_POINT, 12, 6, data_at + 24);
        entry(&mut file, GEO_KEYS, 3, 8, data_at + 72);
        file.extend_from_slice(&0u32.to_le_bytes());
        for value in [0.5f64, 0.5, 0.0, 0.0, 0.0, 0.0, 100.0, 20.0, 0.0] {
            file.extend_from_slice(&value.to_le_bytes());
        }
        for value in [1u16, 1, 0, 1, GEOGRAPHIC_CRS_KEY, 0, 1, 4326] {
            file.extend_from_slice(&value.to_le_bytes());
        }
        let found = info(&file).unwrap();
        assert!(found.contains("\"width\":100,\"height\":50"), "{found}");
        assert!(found.contains("\"epsg\":4326"), "{found}");
        assert!(found.contains("\"bbox\":[100,-5,150,20]"), "{found}");
        assert!(crate::json::normalize(&found).is_ok());
        assert!(info(b"XX").is_err());
    }

    #[test]
    fn lzw_strips_decode_with_their_growing_codes() {
        let decoded = lzw_decode(&[0x80, 0x01, 0xC0, 0xF0, 0x10], 16).unwrap();
        assert_eq!(decoded, vec![7, 7]);
    }
}
