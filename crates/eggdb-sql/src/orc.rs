use eggdb_parquet::{Cell, Column, ColumnType};

const MOST_VALUES: usize = 1 << 28;
const MOST_STREAM_BYTES: usize = 1 << 30;
const MOST_TYPES: usize = 100_000;
const MOST_STRIPES: usize = 1_000_000;
const SECONDS_1970_TO_2015: i64 = 1_420_070_400;
const ZSTD_MAGIC: u32 = 0xFD2F_B528;

const PRESENT: u64 = 0;
const DATA: u64 = 1;
const LENGTH: u64 = 2;
const DICTIONARY_DATA: u64 = 3;
const SECONDARY: u64 = 5;

fn damaged(message: impl std::fmt::Display) -> String {
    format!("the ORC file is damaged: {message}")
}

#[derive(Debug, Clone, Copy)]
enum Field<'b> {
    Varint(u64),
    Bytes(&'b [u8]),
    Fixed,
}

fn varint(bytes: &[u8], at: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at).ok_or_else(|| damaged("a varint is cut short"))?;
        *at += 1;
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(damaged("a varint is too long"))
}

fn wide_varint(bytes: &[u8], at: &mut usize) -> Result<u128, String> {
    let mut value = 0u128;
    for shift in (0..128).step_by(7) {
        let byte = *bytes.get(*at).ok_or_else(|| damaged("a varint is cut short"))?;
        *at += 1;
        value |= u128::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(damaged("a varint is too long"))
}

fn zigzag(value: u64) -> i64 {
    (value >> 1) as i64 ^ -((value & 1) as i64)
}

fn fields(bytes: &[u8]) -> Result<Vec<(u64, Field<'_>)>, String> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let key = varint(bytes, &mut at)?;
        let field = match key & 7 {
            0 => Field::Varint(varint(bytes, &mut at)?),
            1 => {
                at += 8;
                Field::Fixed
            }
            2 => {
                let length = usize::try_from(varint(bytes, &mut at)?).map_err(|_| damaged("a field is too long"))?;
                let end = at.checked_add(length).filter(|end| *end <= bytes.len()).ok_or_else(|| damaged("a field is cut short"))?;
                let taken = &bytes[at..end];
                at = end;
                Field::Bytes(taken)
            }
            5 => {
                at += 4;
                Field::Fixed
            }
            other => return Err(damaged(format!("protobuf wire type {other}"))),
        };
        out.push((key >> 3, field));
    }
    Ok(out)
}

fn number(fields: &[(u64, Field<'_>)], wanted: u64) -> Option<u64> {
    fields.iter().rev().find_map(|(field, value)| match value {
        Field::Varint(number) if *field == wanted => Some(*number),
        _ => None,
    })
}

fn messages<'b>(fields: &[(u64, Field<'b>)], wanted: u64) -> Vec<&'b [u8]> {
    fields
        .iter()
        .filter_map(|(field, value)| match value {
            Field::Bytes(bytes) if *field == wanted => Some(*bytes),
            _ => None,
        })
        .collect()
}

fn numbers(fields: &[(u64, Field<'_>)], wanted: u64) -> Result<Vec<u64>, String> {
    let mut out = Vec::new();
    for (field, value) in fields {
        if *field != wanted {
            continue;
        }
        match value {
            Field::Varint(number) => out.push(*number),
            Field::Bytes(packed) => {
                let mut at = 0;
                while at < packed.len() {
                    out.push(varint(packed, &mut at)?);
                }
            }
            Field::Fixed => {}
        }
    }
    Ok(out)
}

fn zstd_content_size(chunk: &[u8]) -> Option<usize> {
    if u32::from_le_bytes(chunk.get(..4)?.try_into().ok()?) != ZSTD_MAGIC {
        return None;
    }
    let descriptor = *chunk.get(4)?;
    let single_segment = descriptor & 0x20 != 0;
    let dictionary_bytes = [0usize, 1, 2, 4][usize::from(descriptor & 3)];
    let at = 5 + usize::from(!single_segment) + dictionary_bytes;
    let raw = |count: usize| chunk.get(at..at + count).map(|bytes| bytes.iter().rev().fold(0u64, |value, byte| value << 8 | u64::from(*byte)));
    let size = match descriptor >> 6 {
        0 if single_segment => raw(1)?,
        0 => return None,
        1 => raw(2)? + 256,
        2 => raw(4)?,
        _ => raw(8)?,
    };
    usize::try_from(size).ok()
}

fn decompress(codec: u64, data: &[u8]) -> Result<Vec<u8>, String> {
    if codec == 0 {
        return Ok(data.to_vec());
    }
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let header = data.get(at..at + 3).ok_or_else(|| damaged("a compression chunk header is cut short"))?;
        let value = usize::from(header[0]) | usize::from(header[1]) << 8 | usize::from(header[2]) << 16;
        let length = value >> 1;
        let chunk = data.get(at + 3..at + 3 + length).ok_or_else(|| damaged("a compression chunk is cut short"))?;
        at += 3 + length;
        if value & 1 == 1 {
            out.extend_from_slice(chunk);
        } else {
            let room = MOST_STREAM_BYTES.saturating_sub(out.len());
            let plain = match codec {
                1 => eggdb_content::inflate(chunk, room).map_err(|error| damaged(format!("a ZLIB chunk: {}", error.0)))?,
                2 => eggdb_columnar::snappy_decompress(chunk, room).map_err(|error| damaged(format!("a Snappy chunk: {}", error.0)))?,
                5 => {
                    let size = zstd_content_size(chunk).ok_or("ZSTD chunks without a content size are not supported yet")?;
                    if size > room {
                        return Err(damaged("a stream is larger than 1 GiB"));
                    }
                    eggdb_storage::decompress_zstd(chunk, size).map_err(|error| damaged(format!("a ZSTD chunk: {error}")))?
                }
                3 => return Err("LZO-compressed ORC files are not supported yet".to_owned()),
                4 => return Err("LZ4-compressed ORC files are not supported yet".to_owned()),
                other => return Err(format!("ORC compression kind {other} is not supported yet")),
            };
            out.extend_from_slice(&plain);
        }
        if out.len() > MOST_STREAM_BYTES {
            return Err(damaged("a stream is larger than 1 GiB"));
        }
    }
    Ok(out)
}

fn byte_rle(bytes: &[u8], count: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(count.min(MOST_VALUES));
    let mut at = 0;
    while out.len() < count {
        let header = *bytes.get(at).ok_or_else(|| damaged("a byte run is cut short"))? as i8;
        at += 1;
        if header >= 0 {
            let value = *bytes.get(at).ok_or_else(|| damaged("a byte run is cut short"))?;
            at += 1;
            out.extend(std::iter::repeat_n(value, header as usize + 3));
        } else {
            let literals = bytes.get(at..at + header.unsigned_abs() as usize).ok_or_else(|| damaged("byte literals are cut short"))?;
            at += literals.len();
            out.extend_from_slice(literals);
        }
    }
    out.truncate(count);
    Ok(out)
}

fn booleans(bytes: &[u8], count: usize) -> Result<Vec<bool>, String> {
    let packed = byte_rle(bytes, count.div_ceil(8))?;
    Ok((0..count).map(|index| packed[index / 8] & (0x80 >> (index % 8)) != 0).collect())
}

fn integers_v1(bytes: &[u8], count: usize, signed: bool) -> Result<Vec<i64>, String> {
    let mut out = Vec::with_capacity(count.min(MOST_VALUES));
    let mut at = 0;
    let read = |at: &mut usize| -> Result<i64, String> {
        let raw = varint(bytes, at)?;
        Ok(if signed { zigzag(raw) } else { raw as i64 })
    };
    while out.len() < count {
        let header = *bytes.get(at).ok_or_else(|| damaged("an integer run is cut short"))? as i8;
        at += 1;
        if header >= 0 {
            let delta = i64::from(*bytes.get(at).ok_or_else(|| damaged("an integer run is cut short"))? as i8);
            at += 1;
            let base = read(&mut at)?;
            for step in 0..header as i64 + 3 {
                out.push(base.wrapping_add(step.wrapping_mul(delta)));
            }
        } else {
            for _ in 0..header.unsigned_abs() {
                out.push(read(&mut at)?);
            }
        }
    }
    out.truncate(count);
    Ok(out)
}

fn decoded_width(code: u8) -> u32 {
    match code {
        0..=23 => u32::from(code) + 1,
        24 => 26,
        25 => 28,
        26 => 30,
        27 => 32,
        28 => 40,
        29 => 48,
        30 => 56,
        _ => 64,
    }
}

fn closest_fixed_bits(bits: u32) -> u32 {
    match bits {
        0 => 1,
        1..=24 => bits,
        25..=26 => 26,
        27..=28 => 28,
        29..=30 => 30,
        31..=32 => 32,
        33..=40 => 40,
        41..=48 => 48,
        49..=56 => 56,
        _ => 64,
    }
}

fn unpack(bytes: &[u8], at: &mut usize, width: u32, count: usize) -> Result<Vec<u64>, String> {
    let total_bits = (width as usize).checked_mul(count).ok_or_else(|| damaged("a run is too long"))?;
    let end = at.checked_add(total_bits.div_ceil(8)).filter(|end| *end <= bytes.len()).ok_or_else(|| damaged("packed integers are cut short"))?;
    let packed = &bytes[*at..end];
    let mut out = Vec::with_capacity(count);
    let mut bit = 0usize;
    for _ in 0..count {
        let mut value = 0u64;
        for _ in 0..width {
            let set = packed[bit / 8] & (0x80 >> (bit % 8)) != 0;
            value = value << 1 | u64::from(set);
            bit += 1;
        }
        out.push(value);
    }
    *at = end;
    Ok(out)
}

fn big_endian(bytes: &[u8], at: &mut usize, count: usize) -> Result<u64, String> {
    let raw = bytes.get(*at..*at + count).ok_or_else(|| damaged("a value is cut short"))?;
    *at += count;
    Ok(raw.iter().fold(0u64, |value, byte| value << 8 | u64::from(*byte)))
}

fn integers_v2(bytes: &[u8], count: usize, signed: bool) -> Result<Vec<i64>, String> {
    let mut out: Vec<i64> = Vec::with_capacity(count.min(MOST_VALUES));
    let mut at = 0;
    let finish = |raw: u64| if signed { zigzag(raw) } else { raw as i64 };
    while out.len() < count {
        let header = *bytes.get(at).ok_or_else(|| damaged("an integer run is cut short"))?;
        at += 1;
        match header >> 6 {
            0 => {
                let width = usize::from((header >> 3) & 7) + 1;
                let repeat = usize::from(header & 7) + 3;
                let value = finish(big_endian(bytes, &mut at, width)?);
                out.extend(std::iter::repeat_n(value, repeat));
            }
            1 => {
                let width = decoded_width((header >> 1) & 0x1F);
                let length = (usize::from(header & 1) << 8 | usize::from(*bytes.get(at).ok_or_else(|| damaged("a run is cut short"))?)) + 1;
                at += 1;
                out.extend(unpack(bytes, &mut at, width, length)?.into_iter().map(finish));
            }
            2 => {
                let width = decoded_width((header >> 1) & 0x1F);
                let third = *bytes.get(at).ok_or_else(|| damaged("a run is cut short"))?;
                let length = (usize::from(header & 1) << 8 | usize::from(third)) + 1;
                let (fourth, fifth) = (
                    *bytes.get(at + 1).ok_or_else(|| damaged("a run is cut short"))?,
                    *bytes.get(at + 2).ok_or_else(|| damaged("a run is cut short"))?,
                );
                at += 3;
                let base_bytes = usize::from((fourth >> 5) & 7) + 1;
                let patch_width = decoded_width(fourth & 0x1F);
                let gap_width = u32::from((fifth >> 5) & 7) + 1;
                let patches = usize::from(fifth & 0x1F);
                let raw_base = big_endian(bytes, &mut at, base_bytes)?;
                let sign = 1u64 << (base_bytes * 8 - 1);
                let base = if raw_base & sign != 0 { -((raw_base & !sign) as i64) } else { raw_base as i64 };
                let mut values = unpack(bytes, &mut at, width, length)?;
                let entry_width = closest_fixed_bits(patch_width + gap_width);
                if entry_width > 64 {
                    return Err(damaged("a patch is too wide"));
                }
                let mut position = 0usize;
                for entry in unpack(bytes, &mut at, entry_width, patches)? {
                    let gap = (entry >> patch_width) as usize;
                    let patch = if patch_width >= 64 { entry } else { entry & ((1u64 << patch_width) - 1) };
                    position += gap;
                    if gap == 255 && patch == 0 {
                        continue;
                    }
                    if let Some(value) = values.get_mut(position) {
                        *value |= patch.checked_shl(width).unwrap_or(0);
                    }
                }
                out.extend(values.into_iter().map(|value| base.wrapping_add(value as i64)));
            }
            _ => {
                let code = (header >> 1) & 0x1F;
                let width = if code == 0 { 0 } else { decoded_width(code) };
                let length = (usize::from(header & 1) << 8 | usize::from(*bytes.get(at).ok_or_else(|| damaged("a run is cut short"))?)) + 1;
                at += 1;
                let base = finish(varint(bytes, &mut at)?);
                let delta_base = zigzag(varint(bytes, &mut at)?);
                let mut previous = base;
                out.push(base);
                if length > 1 {
                    previous = previous.wrapping_add(delta_base);
                    out.push(previous);
                }
                if width == 0 {
                    for _ in 2..length {
                        previous = previous.wrapping_add(delta_base);
                        out.push(previous);
                    }
                } else if length > 2 {
                    for delta in unpack(bytes, &mut at, width, length - 2)? {
                        previous = if delta_base < 0 { previous.wrapping_sub(delta as i64) } else { previous.wrapping_add(delta as i64) };
                        out.push(previous);
                    }
                }
            }
        }
        if out.len() > MOST_VALUES {
            return Err(damaged("too many values"));
        }
    }
    out.truncate(count);
    Ok(out)
}

fn integers(bytes: &[u8], count: usize, signed: bool, version_two: bool) -> Result<Vec<i64>, String> {
    if version_two { integers_v2(bytes, count, signed) } else { integers_v1(bytes, count, signed) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Boolean,
    Byte,
    Short,
    Int,
    Long,
    Float,
    Double,
    Text,
    Binary,
    Timestamp,
    Decimal { precision: u32, scale: u32 },
    Date,
}

fn kind_of(code: u64, precision: u64, scale: u64) -> Option<Kind> {
    Some(match code {
        0 => Kind::Boolean,
        1 => Kind::Byte,
        2 => Kind::Short,
        3 => Kind::Int,
        4 => Kind::Long,
        5 => Kind::Float,
        6 => Kind::Double,
        7 | 16 | 17 => Kind::Text,
        8 => Kind::Binary,
        9 | 18 => Kind::Timestamp,
        14 => Kind::Decimal {
            precision: u32::try_from(precision).unwrap_or(38).min(38),
            scale: u32::try_from(scale).unwrap_or(0).min(38),
        },
        15 => Kind::Date,
        _ => return None,
    })
}

fn column_type(kind: Kind) -> ColumnType {
    match kind {
        Kind::Boolean => ColumnType::Boolean,
        Kind::Byte | Kind::Short | Kind::Int => ColumnType::Int32,
        Kind::Long => ColumnType::Int64,
        Kind::Float => ColumnType::Float,
        Kind::Double => ColumnType::Double,
        Kind::Text => ColumnType::Text,
        Kind::Binary => ColumnType::Binary,
        Kind::Timestamp => ColumnType::Timestamp { utc: false },
        Kind::Decimal { precision, scale } => ColumnType::Decimal { precision, scale },
        Kind::Date => ColumnType::Date,
    }
}

struct Streams<'s> {
    kinds: Vec<(u64, Vec<u8>)>,
    encoding: u64,
    dictionary_size: usize,
    _file: std::marker::PhantomData<&'s ()>,
}

impl Streams<'_> {
    fn get(&self, kind: u64) -> Option<&[u8]> {
        self.kinds.iter().find(|(found, _)| *found == kind).map(|(_, bytes)| bytes.as_slice())
    }

    fn need(&self, kind: u64) -> Result<&[u8], String> {
        self.get(kind).ok_or_else(|| damaged(format!("a column is missing its stream of kind {kind}")))
    }

    fn version_two(&self) -> bool {
        matches!(self.encoding, 2 | 3)
    }

    fn dictionary(&self) -> bool {
        matches!(self.encoding, 1 | 3)
    }
}

fn byte_strings(streams: &Streams<'_>, count: usize) -> Result<Vec<Vec<u8>>, String> {
    let version_two = streams.version_two();
    let slices = |lengths: &[i64], data: &[u8]| -> Result<Vec<Vec<u8>>, String> {
        let mut at = 0usize;
        let mut out = Vec::with_capacity(lengths.len());
        for length in lengths {
            let length = usize::try_from(*length).map_err(|_| damaged("a negative length"))?;
            let piece = data.get(at..at + length).ok_or_else(|| damaged("string data is cut short"))?;
            at += length;
            out.push(piece.to_vec());
        }
        Ok(out)
    };
    if streams.dictionary() {
        let lengths = integers(streams.need(LENGTH)?, streams.dictionary_size, false, version_two)?;
        let dictionary = slices(&lengths, streams.get(DICTIONARY_DATA).unwrap_or_default())?;
        let indexes = integers(streams.need(DATA)?, count, false, version_two)?;
        indexes
            .into_iter()
            .map(|index| dictionary.get(usize::try_from(index).unwrap_or(usize::MAX)).cloned().ok_or_else(|| damaged("a dictionary index is out of range")))
            .collect()
    } else {
        let lengths = integers(streams.need(LENGTH)?, count, false, version_two)?;
        slices(&lengths, streams.get(DATA).unwrap_or_default())
    }
}

fn rescaled(unscaled: i128, from: i64, to: u32) -> i128 {
    let difference = i64::from(to) - from;
    match difference.cmp(&0) {
        std::cmp::Ordering::Equal => unscaled,
        std::cmp::Ordering::Greater => unscaled.saturating_mul(10i128.saturating_pow(difference.min(38) as u32)),
        std::cmp::Ordering::Less => unscaled / 10i128.saturating_pow((-difference).min(38) as u32),
    }
}

fn values(kind: Kind, streams: &Streams<'_>, count: usize) -> Result<Vec<Cell>, String> {
    let version_two = streams.version_two();
    Ok(match kind {
        Kind::Boolean => booleans(streams.need(DATA)?, count)?.into_iter().map(Cell::Boolean).collect(),
        Kind::Byte => byte_rle(streams.need(DATA)?, count)?.into_iter().map(|byte| Cell::Int(i64::from(byte as i8))).collect(),
        Kind::Short | Kind::Int | Kind::Long | Kind::Date => {
            integers(streams.need(DATA)?, count, true, version_two)?.into_iter().map(Cell::Int).collect()
        }
        Kind::Float => {
            let data = streams.need(DATA)?;
            (0..count)
                .map(|index| {
                    data.get(index * 4..index * 4 + 4)
                        .map(|raw| Cell::Float(f64::from(f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))))
                        .ok_or_else(|| damaged("float data is cut short"))
                })
                .collect::<Result<_, _>>()?
        }
        Kind::Double => {
            let data = streams.need(DATA)?;
            (0..count)
                .map(|index| {
                    data.get(index * 8..index * 8 + 8)
                        .and_then(|raw| raw.try_into().ok())
                        .map(|raw| Cell::Float(f64::from_le_bytes(raw)))
                        .ok_or_else(|| damaged("double data is cut short"))
                })
                .collect::<Result<_, _>>()?
        }
        Kind::Text | Kind::Binary => byte_strings(streams, count)?.into_iter().map(Cell::Bytes).collect(),
        Kind::Timestamp => {
            let seconds = integers(streams.need(DATA)?, count, true, version_two)?;
            let nanos = integers(streams.need(SECONDARY)?, count, false, version_two)?;
            seconds
                .into_iter()
                .zip(nanos)
                .map(|(seconds, encoded)| {
                    let zeros = (encoded & 7) as u32;
                    let mut nanos = encoded >> 3;
                    if zeros != 0 {
                        nanos = nanos.saturating_mul(10i64.pow(zeros + 1));
                    }
                    Cell::Int(
                        seconds
                            .saturating_add(SECONDS_1970_TO_2015)
                            .saturating_mul(1_000_000)
                            .saturating_add(nanos / 1_000),
                    )
                })
                .collect()
        }
        Kind::Decimal { scale, .. } => {
            let data = streams.need(DATA)?;
            let scales = integers(streams.need(SECONDARY)?, count, true, version_two)?;
            let mut at = 0;
            let mut out = Vec::with_capacity(count);
            for value_scale in scales {
                let raw = wide_varint(data, &mut at)?;
                let unscaled = (raw >> 1) as i128 ^ -((raw & 1) as i128);
                out.push(Cell::Decimal(rescaled(unscaled, value_scale, scale)));
            }
            out
        }
    })
}

/// The flat columns of an ORC file (Apache ORC's format, versions 0.11 and 0.12): booleans,
/// integers, floats, strings, binary, dates, timestamps and decimals, with no, ZLIB, Snappy or
/// ZSTD compression, direct and dictionary encodings and integer run-length encodings 1 and 2.
/// Nested columns (lists, maps, structs, unions) are left out.
pub(crate) fn read(bytes: &[u8]) -> Result<Vec<Column>, String> {
    let length = bytes.len();
    let postscript_length = usize::from(*bytes.last().ok_or_else(|| damaged("the file is empty"))?);
    let postscript_start = length.checked_sub(1 + postscript_length).ok_or_else(|| damaged("the postscript is cut short"))?;
    let postscript = fields(&bytes[postscript_start..length - 1])?;
    if messages(&postscript, 8000).first() != Some(&&b"ORC"[..]) || !bytes.starts_with(b"ORC") {
        return Err("the file is not an ORC file".to_owned());
    }
    let codec = number(&postscript, 2).unwrap_or(0);
    let footer_length = usize::try_from(number(&postscript, 1).unwrap_or(0)).map_err(|_| damaged("the footer is too long"))?;
    let footer_start = postscript_start.checked_sub(footer_length).ok_or_else(|| damaged("the footer is cut short"))?;
    let footer_bytes = decompress(codec, &bytes[footer_start..postscript_start])?;
    let footer = fields(&footer_bytes)?;
    let types = messages(&footer, 4);
    if types.len() > MOST_TYPES {
        return Err(damaged("too many types"));
    }
    let parsed_types: Vec<Vec<(u64, Field<'_>)>> = types.iter().map(|raw| fields(raw)).collect::<Result<_, _>>()?;
    let root = parsed_types.first().ok_or_else(|| damaged("the file has no types"))?;
    if number(root, 1).unwrap_or(0) != 12 {
        return Err("ORC files whose root type is not a struct are not supported yet".to_owned());
    }
    let children = numbers(root, 2)?;
    let names: Vec<String> = messages(root, 3).iter().map(|raw| String::from_utf8_lossy(raw).into_owned()).collect();
    let mut columns: Vec<(usize, Kind, Column)> = Vec::new();
    for (index, child) in children.iter().enumerate() {
        let child = usize::try_from(*child).map_err(|_| damaged("a type id is out of range"))?;
        let Some(described) = parsed_types.get(child) else {
            return Err(damaged("a column's type is missing"));
        };
        let Some(kind) = kind_of(number(described, 1).unwrap_or(u64::MAX), number(described, 5).unwrap_or(38), number(described, 6).unwrap_or(0)) else {
            continue;
        };
        columns.push((
            child,
            kind,
            Column {
                name: names.get(index).cloned().unwrap_or_else(|| format!("_col{index}")),
                column_type: column_type(kind),
                nullable: true,
                cells: Vec::new(),
            },
        ));
    }
    let stripes = messages(&footer, 3);
    if stripes.len() > MOST_STRIPES {
        return Err(damaged("too many stripes"));
    }
    let mut total = 0usize;
    for stripe in stripes {
        let stripe = fields(stripe)?;
        let field = |number_of: u64| usize::try_from(number(&stripe, number_of).unwrap_or(0)).map_err(|_| damaged("a stripe is too large"));
        let (offset, index_length, data_length, footer_length, rows) = (field(1)?, field(2)?, field(3)?, field(4)?, field(5)?);
        total = total.saturating_add(rows.saturating_mul(columns.len().max(1)));
        if total > MOST_VALUES {
            return Err("the ORC file holds more than 268,435,456 values".to_owned());
        }
        let footer_at = offset.saturating_add(index_length).saturating_add(data_length);
        let stripe_footer_raw = bytes.get(footer_at..footer_at.saturating_add(footer_length)).ok_or_else(|| damaged("a stripe footer is cut short"))?;
        let stripe_footer_bytes = decompress(codec, stripe_footer_raw)?;
        let stripe_footer = fields(&stripe_footer_bytes)?;
        let encodings: Vec<Vec<(u64, Field<'_>)>> = messages(&stripe_footer, 2).iter().map(|raw| fields(raw)).collect::<Result<_, _>>()?;
        let mut placed: Vec<(u64, u64, usize, usize)> = Vec::new();
        let mut at = offset;
        for stream in messages(&stripe_footer, 1) {
            let stream = fields(stream)?;
            let stream_length = usize::try_from(number(&stream, 3).unwrap_or(0)).map_err(|_| damaged("a stream is too large"))?;
            placed.push((number(&stream, 1).unwrap_or(u64::MAX), number(&stream, 2).unwrap_or(u64::MAX), at, stream_length));
            at = at.saturating_add(stream_length);
        }
        for (type_id, kind, column) in &mut columns {
            let mut kinds = Vec::new();
            for (stream_kind, stream_column, start, stream_length) in &placed {
                if *stream_column != *type_id as u64 || !matches!(*stream_kind, PRESENT | DATA | LENGTH | DICTIONARY_DATA | SECONDARY) {
                    continue;
                }
                let raw = bytes.get(*start..start.saturating_add(*stream_length)).ok_or_else(|| damaged("a stream is cut short"))?;
                kinds.push((*stream_kind, decompress(codec, raw)?));
            }
            let encoding = encodings.get(*type_id);
            let streams = Streams {
                kinds,
                encoding: encoding.and_then(|encoding| number(encoding, 1)).unwrap_or(0),
                dictionary_size: encoding
                    .and_then(|encoding| number(encoding, 2))
                    .and_then(|size| usize::try_from(size).ok())
                    .unwrap_or(0)
                    .min(MOST_VALUES),
                _file: std::marker::PhantomData,
            };
            let present = match streams.get(PRESENT) {
                Some(raw) => booleans(raw, rows)?,
                None => vec![true; rows],
            };
            let count = present.iter().filter(|present| **present).count();
            let mut found = values(*kind, &streams, count)?.into_iter();
            for present in present {
                column.cells.push(if present { found.next().unwrap_or(Cell::Null) } else { Cell::Null });
            }
        }
    }
    Ok(columns.into_iter().map(|(_, _, column)| column).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_length_encodings_decode_the_specification_examples() {
        assert_eq!(integers_v2(&[0x0A, 0x27, 0x10], 5, false).unwrap(), vec![10_000; 5]);
        assert_eq!(
            integers_v2(&[0x5E, 0x03, 0x5C, 0xA1, 0xAB, 0x1E, 0xDE, 0xAD, 0xBE, 0xEF], 4, false).unwrap(),
            vec![23_713, 43_806, 57_005, 48_879]
        );
        assert_eq!(
            integers_v2(&[0xC6, 0x09, 0x02, 0x02, 0x22, 0x42, 0x42, 0x46], 10, false).unwrap(),
            vec![2, 3, 5, 7, 11, 13, 17, 19, 23, 29]
        );
        assert_eq!(integers_v1(&[0x61, 0x00, 0x07], 100, false).unwrap(), vec![7; 100]);
        assert_eq!(integers_v1(&[0x61, 0xFF, 0x64], 100, false).unwrap(), (1..=100).rev().collect::<Vec<i64>>());
        assert_eq!(byte_rle(&[0x61, 0x00], 100).unwrap(), vec![0u8; 100]);
        assert_eq!(booleans(&[0xFF, 0x80], 8).unwrap(), vec![true, false, false, false, false, false, false, false]);
    }
}
