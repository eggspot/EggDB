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

/// A TIFF or BigTIFF file's first image as JSON: its size, bands, bits and sample format,
/// compression, photometric interpretation, tiles or strips, and, for a GeoTIFF, the pixel
/// scale, the tie point, the EPSG code of its coordinate reference system, whether pixels are
/// areas or points, the no-data value and the bounding box `[west, south, east, north]` the tie
/// point and scale give.
pub(crate) fn info(bytes: &[u8]) -> SqlResult<String> {
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
}
