use crate::error::{SqlError, SqlResult, SqlState};

const MOST_ENTRIES: usize = 1_000;
const MOST_VALUE_BYTES: usize = 64 * 1024;
const MOST_XMP_BYTES: usize = 1024 * 1024;
const MOST_SEGMENTS: usize = 10_000;
const EXIF_POINTER: u16 = 0x8769;
const GPS_POINTER: u16 = 0x8825;
const INTEROPERABILITY_POINTER: u16 = 0xA005;
const MAKER_NOTE: u16 = 0x927C;
const XMP_JPEG_SIGNATURE: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const XMP_PNG_KEYWORD: &[u8] = b"XML:com.adobe.xmp";

const IMAGE_TAGS: [(u16, &str); 14] = [
    (0x0100, "ImageWidth"),
    (0x0101, "ImageLength"),
    (0x010E, "ImageDescription"),
    (0x010F, "Make"),
    (0x0110, "Model"),
    (0x0112, "Orientation"),
    (0x011A, "XResolution"),
    (0x011B, "YResolution"),
    (0x0128, "ResolutionUnit"),
    (0x0131, "Software"),
    (0x0132, "DateTime"),
    (0x013B, "Artist"),
    (0x8298, "Copyright"),
    (0x0213, "YCbCrPositioning"),
];

const EXIF_TAGS: [(u16, &str); 30] = [
    (0x829A, "ExposureTime"),
    (0x829D, "FNumber"),
    (0x8822, "ExposureProgram"),
    (0x8827, "PhotographicSensitivity"),
    (0x9000, "ExifVersion"),
    (0x9003, "DateTimeOriginal"),
    (0x9004, "DateTimeDigitized"),
    (0x9010, "OffsetTime"),
    (0x9011, "OffsetTimeOriginal"),
    (0x9012, "OffsetTimeDigitized"),
    (0x9201, "ShutterSpeedValue"),
    (0x9202, "ApertureValue"),
    (0x9204, "ExposureBiasValue"),
    (0x9205, "MaxApertureValue"),
    (0x9207, "MeteringMode"),
    (0x9209, "Flash"),
    (0x920A, "FocalLength"),
    (0x9290, "SubSecTime"),
    (0x9291, "SubSecTimeOriginal"),
    (0xA001, "ColorSpace"),
    (0xA002, "PixelXDimension"),
    (0xA003, "PixelYDimension"),
    (0xA402, "ExposureMode"),
    (0xA403, "WhiteBalance"),
    (0xA405, "FocalLengthIn35mmFilm"),
    (0xA406, "SceneCaptureType"),
    (0xA420, "ImageUniqueID"),
    (0xA431, "BodySerialNumber"),
    (0xA433, "LensMake"),
    (0xA434, "LensModel"),
];

const GPS_TAGS: [(u16, &str); 14] = [
    (0x0000, "GPSVersionID"),
    (0x0001, "GPSLatitudeRef"),
    (0x0002, "GPSLatitude"),
    (0x0003, "GPSLongitudeRef"),
    (0x0004, "GPSLongitude"),
    (0x0005, "GPSAltitudeRef"),
    (0x0006, "GPSAltitude"),
    (0x0007, "GPSTimeStamp"),
    (0x000C, "GPSSpeedRef"),
    (0x000D, "GPSSpeed"),
    (0x0010, "GPSImgDirectionRef"),
    (0x0011, "GPSImgDirection"),
    (0x0012, "GPSMapDatum"),
    (0x001D, "GPSDateStamp"),
];

fn invalid(message: impl std::fmt::Display) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, format!("image_metadata: {message}"))
}

fn quoted(text: &str) -> String {
    let mut out = String::new();
    crate::json::write_string(text, &mut out);
    out
}

fn number(value: f64) -> String {
    if value.is_finite() {
        let text = format!("{value}");
        if text.contains(['e', 'E']) { format!("{value:.6}") } else { text }
    } else {
        "null".to_owned()
    }
}

#[derive(Default)]
struct Found {
    format: &'static str,
    width: Option<u64>,
    height: Option<u64>,
    exif: Option<Vec<u8>>,
    xmp: Option<String>,
}

struct Tiff<'b> {
    bytes: &'b [u8],
    little: bool,
}

impl<'b> Tiff<'b> {
    fn new(bytes: &'b [u8]) -> Option<Self> {
        let little = match bytes.get(..2)? {
            b"II" => true,
            b"MM" => false,
            _ => return None,
        };
        let tiff = Self { bytes, little };
        (tiff.u16(2)? == 42).then_some(tiff)
    }

    fn u16(&self, at: usize) -> Option<u16> {
        let raw: [u8; 2] = self.bytes.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if self.little { u16::from_le_bytes(raw) } else { u16::from_be_bytes(raw) })
    }

    fn u32(&self, at: usize) -> Option<u32> {
        let raw: [u8; 4] = self.bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if self.little { u32::from_le_bytes(raw) } else { u32::from_be_bytes(raw) })
    }

    fn first_directory(&self) -> Option<usize> {
        self.u32(4).map(|offset| offset as usize)
    }

    fn entries(&self, directory: usize) -> Vec<(u16, u16, u32, usize)> {
        let Some(count) = self.u16(directory) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for index in 0..usize::from(count).min(MOST_ENTRIES) {
            let at = directory + 2 + index * 12;
            let (Some(tag), Some(kind), Some(count)) = (self.u16(at), self.u16(at + 2), self.u32(at + 4)) else {
                break;
            };
            let size = type_size(kind).saturating_mul(count as usize);
            let data = if size <= 4 { at + 8 } else { self.u32(at + 8).map_or(usize::MAX, |offset| offset as usize) };
            out.push((tag, kind, count, data));
        }
        out
    }

    fn value(&self, kind: u16, count: u32, data: usize) -> Option<String> {
        let count = count as usize;
        let size = type_size(kind).checked_mul(count)?;
        if size > MOST_VALUE_BYTES {
            return None;
        }
        let raw = self.bytes.get(data..data.checked_add(size)?)?;
        let values: Vec<String> = match kind {
            2 => {
                let text = String::from_utf8_lossy(raw);
                return Some(quoted(text.trim_end_matches('\0').trim()));
            }
            1 => raw.iter().map(|byte| byte.to_string()).collect(),
            6 => raw.iter().map(|byte| (*byte as i8).to_string()).collect(),
            7 => {
                let printable = raw.iter().all(|byte| byte.is_ascii_graphic() || *byte == b' ' || *byte == 0);
                return Some(if printable {
                    quoted(String::from_utf8_lossy(raw).trim_end_matches('\0').trim())
                } else {
                    quoted(&raw.iter().map(|byte| format!("{byte:02x}")).collect::<String>())
                });
            }
            3 => (0..count).filter_map(|at| self.u16(data + at * 2)).map(|value| value.to_string()).collect(),
            8 => (0..count).filter_map(|at| self.u16(data + at * 2)).map(|value| (value as i16).to_string()).collect(),
            4 => (0..count).filter_map(|at| self.u32(data + at * 4)).map(|value| value.to_string()).collect(),
            9 => (0..count).filter_map(|at| self.u32(data + at * 4)).map(|value| (value as i32).to_string()).collect(),
            5 | 10 => (0..count)
                .filter_map(|at| Some((self.u32(data + at * 8)?, self.u32(data + at * 8 + 4)?)))
                .map(|(numerator, denominator)| {
                    let (numerator, denominator) = if kind == 10 {
                        (f64::from(numerator as i32), f64::from(denominator as i32))
                    } else {
                        (f64::from(numerator), f64::from(denominator))
                    };
                    if denominator == 0.0 { "null".to_owned() } else { number(numerator / denominator) }
                })
                .collect(),
            11 => (0..count).filter_map(|at| self.u32(data + at * 4)).map(|bits| number(f64::from(f32::from_bits(bits)))).collect(),
            _ => return None,
        };
        Some(match values.as_slice() {
            [single] => single.clone(),
            _ => format!("[{}]", values.join(",")),
        })
    }

    fn directory_json(&self, directory: usize, names: &[(u16, &str)], pointers: &mut Vec<(u16, usize)>) -> Vec<(String, String)> {
        let mut members = Vec::new();
        for (tag, kind, count, data) in self.entries(directory) {
            if matches!(tag, EXIF_POINTER | GPS_POINTER | INTEROPERABILITY_POINTER) {
                if let Some(offset) = self.u32(data) {
                    pointers.push((tag, offset as usize));
                }
                continue;
            }
            if tag == MAKER_NOTE {
                continue;
            }
            let Some(value) = self.value(kind, count, data) else {
                continue;
            };
            let name = names
                .iter()
                .find(|(known, _)| *known == tag)
                .map_or_else(|| format!("Tag0x{tag:04X}"), |(_, name)| (*name).to_owned());
            members.push((name, value));
        }
        members
    }
}

fn type_size(kind: u16) -> usize {
    match kind {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 => 8,
        _ => 0,
    }
}

fn object(members: &[(String, String)]) -> String {
    format!(
        "{{{}}}",
        members.iter().map(|(name, value)| format!("{}:{value}", quoted(name))).collect::<Vec<_>>().join(",")
    )
}

fn degrees(value: &str, reference: Option<&str>) -> Option<String> {
    let parts: Vec<f64> = value.trim_matches(['[', ']']).split(',').map(|part| part.trim().parse().ok()).collect::<Option<_>>()?;
    let [whole, minutes, seconds] = parts.as_slice() else {
        return None;
    };
    let mut decimal = whole + minutes / 60.0 + seconds / 3600.0;
    if reference.is_some_and(|reference| reference.contains('S') || reference.contains('W')) {
        decimal = -decimal;
    }
    Some(number(decimal))
}

fn exif_json(block: &[u8]) -> Option<(String, Option<u64>, Option<u64>)> {
    let tiff = Tiff::new(block)?;
    let mut pointers = Vec::new();
    let image = tiff.directory_json(tiff.first_directory()?, &IMAGE_TAGS, &mut pointers);
    let mut exif = Vec::new();
    let mut gps = Vec::new();
    for (tag, offset) in pointers.clone() {
        match tag {
            EXIF_POINTER => exif = tiff.directory_json(offset, &EXIF_TAGS, &mut Vec::new()),
            GPS_POINTER => gps = tiff.directory_json(offset, &GPS_TAGS, &mut Vec::new()),
            _ => {}
        }
    }
    let lookup = |members: &[(String, String)], name: &str| members.iter().find(|(known, _)| known == name).map(|(_, value)| value.clone());
    let reference = |name: &str| lookup(&gps, name).map(|value| value.trim_matches('"').to_owned());
    if let Some(latitude) = lookup(&gps, "GPSLatitude").and_then(|value| degrees(&value, reference("GPSLatitudeRef").as_deref())) {
        gps.push(("latitude".to_owned(), latitude));
    }
    if let Some(longitude) = lookup(&gps, "GPSLongitude").and_then(|value| degrees(&value, reference("GPSLongitudeRef").as_deref())) {
        gps.push(("longitude".to_owned(), longitude));
    }
    let width = lookup(&exif, "PixelXDimension").or_else(|| lookup(&image, "ImageWidth")).and_then(|value| value.parse().ok());
    let height = lookup(&exif, "PixelYDimension").or_else(|| lookup(&image, "ImageLength")).and_then(|value| value.parse().ok());
    let mut members = vec![("image".to_owned(), object(&image))];
    if !exif.is_empty() {
        members.push(("exif".to_owned(), object(&exif)));
    }
    if !gps.is_empty() {
        members.push(("gps".to_owned(), object(&gps)));
    }
    Some((object(&members), width, height))
}

fn jpeg(bytes: &[u8], found: &mut Found) {
    found.format = "jpeg";
    let mut at = 2;
    for _ in 0..MOST_SEGMENTS {
        let Some(&[0xFF, marker]) = bytes.get(at..at + 2) else {
            return;
        };
        if marker == 0xD9 || marker == 0xDA {
            return;
        }
        if (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            at += 2;
            continue;
        }
        let Some(length) = bytes.get(at + 2..at + 4).map(|raw| usize::from(u16::from_be_bytes([raw[0], raw[1]]))) else {
            return;
        };
        let Some(segment) = bytes.get(at + 4..(at + 2 + length).min(bytes.len())) else {
            return;
        };
        match marker {
            0xE1 if segment.starts_with(b"Exif\0\0") => found.exif = Some(segment[6..].to_vec()),
            0xE1 if segment.starts_with(XMP_JPEG_SIGNATURE) && segment.len() <= MOST_XMP_BYTES => {
                found.xmp = Some(String::from_utf8_lossy(&segment[XMP_JPEG_SIGNATURE.len()..]).into_owned());
            }
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                if let Some(raw) = segment.get(1..5) {
                    found.height = Some(u64::from(u16::from_be_bytes([raw[0], raw[1]])));
                    found.width = Some(u64::from(u16::from_be_bytes([raw[2], raw[3]])));
                }
            }
            _ => {}
        }
        at += 2 + length.max(2);
    }
}

fn png(bytes: &[u8], found: &mut Found) {
    found.format = "png";
    let mut at = 8;
    for _ in 0..MOST_SEGMENTS {
        let Some(length) = bytes.get(at..at + 4).map(|raw| u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize) else {
            return;
        };
        let Some(kind) = bytes.get(at + 4..at + 8) else {
            return;
        };
        let Some(data) = bytes.get(at + 8..(at + 8).saturating_add(length)) else {
            return;
        };
        match kind {
            b"IHDR" if data.len() >= 8 => {
                found.width = Some(u64::from(u32::from_be_bytes([data[0], data[1], data[2], data[3]])));
                found.height = Some(u64::from(u32::from_be_bytes([data[4], data[5], data[6], data[7]])));
            }
            b"eXIf" => found.exif = Some(data.to_vec()),
            b"iTXt" if data.starts_with(XMP_PNG_KEYWORD) && data.len() <= MOST_XMP_BYTES => {
                let rest = &data[XMP_PNG_KEYWORD.len()..];
                if rest.get(1) == Some(&0) {
                    let mut fields = rest.get(3..).unwrap_or_default().splitn(3, |byte| *byte == 0);
                    let _language = fields.next();
                    let _translated = fields.next();
                    if let Some(text) = fields.next() {
                        found.xmp = Some(String::from_utf8_lossy(text).into_owned());
                    }
                }
            }
            b"IEND" => return,
            _ => {}
        }
        at = at.saturating_add(12).saturating_add(length);
    }
}

fn webp(bytes: &[u8], found: &mut Found) {
    found.format = "webp";
    let mut at = 12;
    for _ in 0..MOST_SEGMENTS {
        let Some(kind) = bytes.get(at..at + 4) else {
            return;
        };
        let Some(length) = bytes.get(at + 4..at + 8).map(|raw| u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize) else {
            return;
        };
        let Some(data) = bytes.get(at + 8..(at + 8).saturating_add(length)) else {
            return;
        };
        let three = |at: usize| data.get(at..at + 3).map(|raw| u64::from(raw[0]) | u64::from(raw[1]) << 8 | u64::from(raw[2]) << 16);
        match kind {
            b"VP8X" => {
                found.width = three(4).map(|value| value + 1);
                found.height = three(7).map(|value| value + 1);
            }
            b"VP8L" if data.first() == Some(&0x2F) && data.len() >= 5 => {
                let bits = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
                found.width.get_or_insert(u64::from(bits & 0x3FFF) + 1);
                found.height.get_or_insert(u64::from((bits >> 14) & 0x3FFF) + 1);
            }
            b"VP8 " if data.len() >= 10 => {
                found.width.get_or_insert(u64::from(u16::from_le_bytes([data[6], data[7]]) & 0x3FFF));
                found.height.get_or_insert(u64::from(u16::from_le_bytes([data[8], data[9]]) & 0x3FFF));
            }
            b"EXIF" => {
                let block = data.strip_prefix(b"Exif\0\0").unwrap_or(data);
                found.exif = Some(block.to_vec());
            }
            b"XMP " if data.len() <= MOST_XMP_BYTES => found.xmp = Some(String::from_utf8_lossy(data).into_owned()),
            _ => {}
        }
        at = at.saturating_add(8).saturating_add(length).saturating_add(length % 2);
    }
}

/// `image_metadata(bytea)`: an image's format, size, EXIF tags (the image, Exif and GPS
/// directories, with decimal `latitude` and `longitude`) and XMP packet, from JPEG, PNG, TIFF
/// and WebP files.
pub(crate) fn metadata(bytes: &[u8]) -> SqlResult<String> {
    let mut found = Found::default();
    if bytes.starts_with(&[0xFF, 0xD8]) {
        jpeg(bytes, &mut found);
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        png(bytes, &mut found);
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        webp(bytes, &mut found);
    } else if Tiff::new(bytes).is_some() {
        found.format = "tiff";
        found.exif = Some(bytes.to_vec());
    } else {
        return Err(invalid("the bytes are not a JPEG, PNG, TIFF or WebP image"));
    }
    let mut members = vec![("format".to_owned(), quoted(found.format))];
    let mut exif = None;
    if let Some(block) = &found.exif
        && let Some((json, width, height)) = exif_json(block)
    {
        found.width = found.width.or(width);
        found.height = found.height.or(height);
        exif = Some(json);
    }
    if let Some(width) = found.width {
        members.push(("width".to_owned(), width.to_string()));
    }
    if let Some(height) = found.height {
        members.push(("height".to_owned(), height.to_string()));
    }
    if let Some(exif) = exif {
        members.push(("exif".to_owned(), exif));
    }
    if let Some(xmp) = &found.xmp {
        members.push(("xmp".to_owned(), quoted(xmp.trim_end_matches('\0'))));
    }
    Ok(object(&members))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_exif_gives_camera_tags_and_decimal_coordinates() {
        let mut tiff = b"II*\x00\x08\x00\x00\x00".to_vec();
        tiff.extend_from_slice(&2u16.to_le_bytes());
        tiff.extend_from_slice(&0x010Fu16.to_le_bytes());
        tiff.extend_from_slice(&2u16.to_le_bytes());
        tiff.extend_from_slice(&4u32.to_le_bytes());
        tiff.extend_from_slice(b"Egg\0");
        tiff.extend_from_slice(&GPS_POINTER.to_le_bytes());
        tiff.extend_from_slice(&4u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&38u32.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(&2u16.to_le_bytes());
        tiff.extend_from_slice(&0x0001u16.to_le_bytes());
        tiff.extend_from_slice(&2u16.to_le_bytes());
        tiff.extend_from_slice(&2u32.to_le_bytes());
        tiff.extend_from_slice(b"S\0\0\0");
        tiff.extend_from_slice(&0x0002u16.to_le_bytes());
        tiff.extend_from_slice(&5u16.to_le_bytes());
        tiff.extend_from_slice(&3u32.to_le_bytes());
        tiff.extend_from_slice(&68u32.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        for (numerator, denominator) in [(10u32, 1u32), (30, 1), (0, 1)] {
            tiff.extend_from_slice(&numerator.to_le_bytes());
            tiff.extend_from_slice(&denominator.to_le_bytes());
        }
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
        jpeg.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
        jpeg.extend_from_slice(b"Exif\0\0");
        jpeg.extend_from_slice(&tiff);
        jpeg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x10, 0x00, 0x20, 0x01, 0x01, 0x11, 0x00, 0xFF, 0xD9]);
        let json = metadata(&jpeg).unwrap();
        assert!(json.contains(r#""format":"jpeg""#), "{json}");
        assert!(json.contains(r#""width":32"#) && json.contains(r#""height":16"#), "{json}");
        assert!(json.contains(r#""Make":"Egg""#), "{json}");
        assert!(json.contains(r#""latitude":-10.5"#), "{json}");
    }
}
