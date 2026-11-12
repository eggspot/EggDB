use crate::error::{SqlError, SqlResult, SqlState};

const PREAMBLE: usize = 128;
const MAGIC: &[u8] = b"DICM";
const UNDEFINED_LENGTH: u32 = 0xFFFF_FFFF;
const ITEM: (u16, u16) = (0xFFFE, 0xE000);
const ITEM_END: (u16, u16) = (0xFFFE, 0xE00D);
const SEQUENCE_END: (u16, u16) = (0xFFFE, 0xE0DD);
const MOST_DEPTH: usize = 16;
const MOST_ELEMENTS: usize = 100_000;
const IMPLICIT_LITTLE_ENDIAN: &str = "1.2.840.10008.1.2";
const EXPLICIT_BIG_ENDIAN: &str = "1.2.840.10008.1.2.2";
const DEFLATED: &str = "1.2.840.10008.1.2.1.99";
const LONG_LENGTH_VRS: [&str; 13] = ["OB", "OD", "OF", "OL", "OV", "OW", "SQ", "SV", "UC", "UN", "UR", "UT", "UV"];
const STRING_VRS: [&str; 16] = ["AE", "AS", "CS", "DA", "DT", "LO", "LT", "PN", "SH", "ST", "TM", "UC", "UI", "UR", "UT", "DS"];
const IMPLICIT_VRS: [((u16, u16), &str); 30] = [
    ((0x0008, 0x0016), "UI"),
    ((0x0008, 0x0018), "UI"),
    ((0x0008, 0x0020), "DA"),
    ((0x0008, 0x0030), "TM"),
    ((0x0008, 0x0060), "CS"),
    ((0x0008, 0x0070), "LO"),
    ((0x0008, 0x0090), "PN"),
    ((0x0008, 0x1030), "LO"),
    ((0x0008, 0x103E), "LO"),
    ((0x0010, 0x0010), "PN"),
    ((0x0010, 0x0020), "LO"),
    ((0x0010, 0x0030), "DA"),
    ((0x0010, 0x0040), "CS"),
    ((0x0018, 0x0050), "DS"),
    ((0x0020, 0x000D), "UI"),
    ((0x0020, 0x000E), "UI"),
    ((0x0020, 0x0010), "SH"),
    ((0x0020, 0x0011), "IS"),
    ((0x0020, 0x0013), "IS"),
    ((0x0028, 0x0002), "US"),
    ((0x0028, 0x0004), "CS"),
    ((0x0028, 0x0010), "US"),
    ((0x0028, 0x0011), "US"),
    ((0x0028, 0x0030), "DS"),
    ((0x0028, 0x0100), "US"),
    ((0x0028, 0x0101), "US"),
    ((0x0028, 0x0102), "US"),
    ((0x0028, 0x0103), "US"),
    ((0x7FE0, 0x0010), "OW"),
    ((0x0008, 0x0005), "CS"),
];

fn invalid(message: impl std::fmt::Display) -> SqlError {
    SqlError::new(SqlState::INVALID_TEXT_REPRESENTATION, format!("invalid DICOM file: {message}"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Encoding {
    ExplicitLittle,
    ImplicitLittle,
    ExplicitBig,
}

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
    elements: usize,
}

impl<'b> Reader<'b> {
    fn take(&mut self, count: usize) -> SqlResult<&'b [u8]> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or_else(|| invalid("an element runs past the end"))?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn u16(&mut self, big: bool) -> SqlResult<u16> {
        let raw: [u8; 2] = self.take(2)?.try_into().map_err(|_| invalid("short"))?;
        Ok(if big { u16::from_be_bytes(raw) } else { u16::from_le_bytes(raw) })
    }

    fn u32(&mut self, big: bool) -> SqlResult<u32> {
        let raw: [u8; 4] = self.take(4)?.try_into().map_err(|_| invalid("short"))?;
        Ok(if big { u32::from_be_bytes(raw) } else { u32::from_le_bytes(raw) })
    }
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    crate::json::write_string(text, &mut out);
    out
}

fn numbers<const N: usize>(bytes: &[u8], big: bool, read: fn([u8; N], bool) -> String) -> Vec<String> {
    bytes
        .chunks_exact(N)
        .filter_map(|chunk| chunk.try_into().ok().map(|raw| read(raw, big)))
        .collect()
}

fn float_json(number: f64) -> String {
    if number.is_finite() { format!("{number}") } else { "null".to_owned() }
}

fn value_json(vr: &str, bytes: &[u8], big: bool) -> Option<String> {
    let items: Vec<String> = match vr {
        "US" => numbers::<2>(bytes, big, |raw, big| (if big { u16::from_be_bytes(raw) } else { u16::from_le_bytes(raw) }).to_string()),
        "SS" => numbers::<2>(bytes, big, |raw, big| (if big { i16::from_be_bytes(raw) } else { i16::from_le_bytes(raw) }).to_string()),
        "UL" => numbers::<4>(bytes, big, |raw, big| (if big { u32::from_be_bytes(raw) } else { u32::from_le_bytes(raw) }).to_string()),
        "SL" => numbers::<4>(bytes, big, |raw, big| (if big { i32::from_be_bytes(raw) } else { i32::from_le_bytes(raw) }).to_string()),
        "UV" => numbers::<8>(bytes, big, |raw, big| (if big { u64::from_be_bytes(raw) } else { u64::from_le_bytes(raw) }).to_string()),
        "SV" => numbers::<8>(bytes, big, |raw, big| (if big { i64::from_be_bytes(raw) } else { i64::from_le_bytes(raw) }).to_string()),
        "FL" => numbers::<4>(bytes, big, |raw, big| float_json(f64::from(if big { f32::from_be_bytes(raw) } else { f32::from_le_bytes(raw) }))),
        "FD" => numbers::<8>(bytes, big, |raw, big| float_json(if big { f64::from_be_bytes(raw) } else { f64::from_le_bytes(raw) })),
        "AT" => numbers::<4>(bytes, big, |raw, big| {
            let (group, element) = if big {
                (u16::from_be_bytes([raw[0], raw[1]]), u16::from_be_bytes([raw[2], raw[3]]))
            } else {
                (u16::from_le_bytes([raw[0], raw[1]]), u16::from_le_bytes([raw[2], raw[3]]))
            };
            json_string(&format!("{group:04X}{element:04X}"))
        }),
        "IS" | "DS" => {
            let text = String::from_utf8_lossy(bytes);
            text.trim_matches(|character: char| character == ' ' || character == '\0')
                .split('\\')
                .map(str::trim)
                .map(|part| match part.parse::<f64>() {
                    Ok(number) if number.is_finite() && !part.is_empty() => part.trim_start_matches('+').to_owned(),
                    _ if part.is_empty() => "null".to_owned(),
                    _ => json_string(part),
                })
                .collect()
        }
        vr if STRING_VRS.contains(&vr) || vr == "SH" => {
            let text = String::from_utf8_lossy(bytes);
            let trimmed = text.trim_end_matches(['\0', ' ']);
            let parts: Vec<&str> = if matches!(vr, "LT" | "ST" | "UT" | "UR") { vec![trimmed] } else { trimmed.split('\\').collect() };
            parts
                .into_iter()
                .map(|part| {
                    let part = part.trim_matches(|character: char| character == ' ' || character == '\0');
                    if vr == "PN" {
                        format!("{{\"Alphabetic\":{}}}", json_string(part))
                    } else {
                        json_string(part)
                    }
                })
                .collect()
        }
        _ => return None,
    };
    if items.is_empty() || bytes.is_empty() {
        return None;
    }
    Some(format!("[{}]", items.join(",")))
}

fn skip_undefined(reader: &mut Reader<'_>) -> SqlResult<()> {
    loop {
        let group = reader.u16(false)?;
        let element = reader.u16(false)?;
        let length = reader.u32(false)?;
        if (group, element) == SEQUENCE_END {
            return Ok(());
        }
        if length != UNDEFINED_LENGTH {
            reader.take(length as usize)?;
        }
    }
}

fn dataset(reader: &mut Reader<'_>, encoding: Encoding, end: Option<usize>, depth: usize, until_item_end: bool) -> SqlResult<String> {
    if depth > MOST_DEPTH {
        return Err(invalid(format!("sequences nest deeper than {MOST_DEPTH}")));
    }
    let big = encoding == Encoding::ExplicitBig;
    let mut members = Vec::new();
    while end.is_none_or(|end| reader.at < end) && reader.at < reader.bytes.len() {
        reader.elements += 1;
        if reader.elements > MOST_ELEMENTS {
            return Err(invalid(format!("more than {MOST_ELEMENTS} elements")));
        }
        let group = reader.u16(big)?;
        let element = reader.u16(big)?;
        if (group, element) == ITEM_END || (group, element) == SEQUENCE_END {
            reader.u32(big)?;
            if until_item_end {
                break;
            }
            continue;
        }
        let explicit = encoding != Encoding::ImplicitLittle || group == 0x0002;
        let (vr, length) = if explicit {
            let vr = String::from_utf8_lossy(reader.take(2)?).into_owned();
            if LONG_LENGTH_VRS.contains(&vr.as_str()) {
                reader.take(2)?;
                (vr, reader.u32(big)?)
            } else {
                (vr, u32::from(reader.u16(big)?))
            }
        } else {
            let known = IMPLICIT_VRS.iter().find(|(tag, _)| *tag == (group, element)).map(|(_, vr)| (*vr).to_owned());
            let vr = known.unwrap_or_else(|| if element == 0 { "UL".to_owned() } else { "UN".to_owned() });
            (vr, reader.u32(big)?)
        };
        let key = json_string(&format!("{group:04X}{element:04X}"));
        if vr == "SQ" || (length == UNDEFINED_LENGTH && vr == "UN") {
            let sequence_end = (length != UNDEFINED_LENGTH).then(|| reader.at + length as usize);
            let mut items = Vec::new();
            while sequence_end.is_none_or(|end| reader.at < end) {
                let item_group = reader.u16(big)?;
                let item_element = reader.u16(big)?;
                let item_length = reader.u32(big)?;
                if (item_group, item_element) == SEQUENCE_END {
                    break;
                }
                if (item_group, item_element) != ITEM {
                    return Err(invalid("a sequence holds something other than items"));
                }
                let item_end = (item_length != UNDEFINED_LENGTH).then(|| reader.at + item_length as usize);
                let item_encoding = if vr == "UN" { Encoding::ImplicitLittle } else { encoding };
                items.push(dataset(reader, item_encoding, item_end, depth + 1, item_end.is_none())?);
            }
            members.push(format!("{key}:{{\"vr\":\"SQ\",\"Value\":[{}]}}", items.join(",")));
            continue;
        }
        if length == UNDEFINED_LENGTH {
            skip_undefined(reader)?;
            members.push(format!("{key}:{{\"vr\":{}}}", json_string(&vr)));
            continue;
        }
        let bytes = reader.take(length as usize)?;
        if group == 0x0002 && element == 0x0010 {
            let syntax = String::from_utf8_lossy(bytes).trim_end_matches(['\0', ' ']).to_owned();
            members.push(format!("{key}:{{\"vr\":\"UI\",\"Value\":[{}]}}", json_string(&syntax)));
            continue;
        }
        match value_json(&vr, bytes, big) {
            Some(value) => members.push(format!("{key}:{{\"vr\":{},\"Value\":{value}}}", json_string(&vr))),
            None => members.push(format!("{key}:{{\"vr\":{}}}", json_string(&vr))),
        }
    }
    Ok(format!("{{{}}}", members.join(",")))
}

fn transfer_syntax(meta: &[u8]) -> Option<String> {
    let mut at = 0;
    while at + 8 <= meta.len() {
        let group = u16::from_le_bytes([meta[at], meta[at + 1]]);
        let element = u16::from_le_bytes([meta[at + 2], meta[at + 3]]);
        let vr = &meta[at + 4..at + 6];
        let (length, header) = if LONG_LENGTH_VRS.iter().any(|long| long.as_bytes() == vr) {
            (u32::from_le_bytes(meta.get(at + 8..at + 12)?.try_into().ok()?) as usize, 12)
        } else {
            (usize::from(u16::from_le_bytes([meta[at + 6], meta[at + 7]])), 8)
        };
        if group != 0x0002 {
            return None;
        }
        let value = meta.get(at + header..at + header + length)?;
        if element == 0x0010 {
            return Some(String::from_utf8_lossy(value).trim_end_matches(['\0', ' ']).to_owned());
        }
        at += header + length;
    }
    None
}

/// A DICOM Part 10 file as DICOM's JSON model (PS3.18 F): one member per element, keyed by its
/// tag in hex, with its `vr` and its `Value` (strings, numbers, `{"Alphabetic": ...}` person
/// names, nested objects for sequences); bulk binary values (pixel data, `OB`, `OW`, ...) keep
/// their `vr` without their bytes.
pub(crate) fn parse(file: &[u8]) -> SqlResult<String> {
    let start = if file.get(PREAMBLE..PREAMBLE + MAGIC.len()) == Some(MAGIC) { PREAMBLE + MAGIC.len() } else { 0 };
    let body = &file[start..];
    let encoding = match transfer_syntax(body).as_deref() {
        Some(IMPLICIT_LITTLE_ENDIAN) => Encoding::ImplicitLittle,
        Some(EXPLICIT_BIG_ENDIAN) => Encoding::ExplicitBig,
        Some(DEFLATED) => return Err(invalid("the deflated transfer syntax is not supported")),
        Some(_) => Encoding::ExplicitLittle,
        None if start == 0 => Encoding::ImplicitLittle,
        None => Encoding::ExplicitLittle,
    };
    let mut reader = Reader { bytes: body, at: 0, elements: 0 };
    let meta_end = {
        let mut probe = 0;
        while probe + 4 <= body.len() && u16::from_le_bytes([body[probe], body[probe + 1]]) == 0x0002 {
            let vr = body.get(probe + 4..probe + 6).unwrap_or_default();
            let (length, header) = if LONG_LENGTH_VRS.iter().any(|long| long.as_bytes() == vr) {
                (body.get(probe + 8..probe + 12).and_then(|raw| raw.try_into().ok()).map_or(0, u32::from_le_bytes) as usize, 12)
            } else {
                (body.get(probe + 6..probe + 8).map_or(0, |raw| usize::from(u16::from_le_bytes([raw[0], raw[1]]))), 8)
            };
            probe += header + length;
        }
        probe.min(body.len())
    };
    let meta = dataset(&mut reader, Encoding::ExplicitLittle, Some(meta_end), 0, false)?;
    let rest = dataset(&mut reader, encoding, None, 0, false)?;
    let merged = match (meta.as_str(), rest.as_str()) {
        ("{}", rest) => rest.to_owned(),
        (meta, "{}") => meta.to_owned(),
        (meta, rest) => format!("{},{}", &meta[..meta.len() - 1], &rest[1..]),
    };
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(out: &mut Vec<u8>, group: u16, element: u16, vr: &str, value: &[u8]) {
        out.extend_from_slice(&group.to_le_bytes());
        out.extend_from_slice(&element.to_le_bytes());
        out.extend_from_slice(vr.as_bytes());
        if LONG_LENGTH_VRS.contains(&vr) {
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        } else {
            out.extend_from_slice(&(value.len() as u16).to_le_bytes());
        }
        out.extend_from_slice(value);
    }

    #[test]
    fn explicit_files_read_names_numbers_and_skip_pixel_data() {
        let mut file = vec![0u8; PREAMBLE];
        file.extend_from_slice(MAGIC);
        element(&mut file, 0x0002, 0x0010, "UI", b"1.2.840.10008.1.2.1\0");
        element(&mut file, 0x0010, 0x0010, "PN", b"Doe^Jane");
        element(&mut file, 0x0028, 0x0010, "US", &512u16.to_le_bytes());
        element(&mut file, 0x0028, 0x0030, "DS", b"0.5\\0.5 ");
        element(&mut file, 0x7FE0, 0x0010, "OW", &[0u8; 16]);
        let parsed = parse(&file).unwrap();
        assert!(parsed.contains("\"00100010\":{\"vr\":\"PN\",\"Value\":[{\"Alphabetic\":\"Doe^Jane\"}]}"), "{parsed}");
        assert!(parsed.contains("\"00280010\":{\"vr\":\"US\",\"Value\":[512]}"), "{parsed}");
        assert!(parsed.contains("\"00280030\":{\"vr\":\"DS\",\"Value\":[0.5,0.5]}"), "{parsed}");
        assert!(parsed.contains("\"7FE00010\":{\"vr\":\"OW\"}"), "{parsed}");
        assert!(crate::json::normalize(&parsed).is_ok());
        assert!(parse(&file[..file.len() - 4]).is_err());
    }
}
