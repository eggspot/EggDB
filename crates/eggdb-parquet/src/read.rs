use crate::thrift::{Reader, Thrift};
use crate::{Cell, Column, ColumnType, MAX_VALUES, ParquetError, damaged, unsupported};

const MAGIC: &[u8] = b"PAR1";
const MAX_PAGE_BYTES: usize = 1 << 30;
const JULIAN_DAY_OF_EPOCH: i64 = 2_440_588;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Physical {
    Boolean,
    Int32,
    Int64,
    Int96,
    Float,
    Double,
    ByteArray,
    Fixed(usize),
}

struct Leaf {
    name: String,
    physical: Physical,
    column_type: ColumnType,
    nullable: bool,
    timestamp_divisor: i64,
    timestamp_multiplier: i64,
}

fn physical_of(element: &Thrift) -> Result<Physical, ParquetError> {
    Ok(match element.int(1) {
        Some(0) => Physical::Boolean,
        Some(1) => Physical::Int32,
        Some(2) => Physical::Int64,
        Some(3) => Physical::Int96,
        Some(4) => Physical::Float,
        Some(5) => Physical::Double,
        Some(6) => Physical::ByteArray,
        Some(7) => {
            let length = element.int(2).and_then(|length| usize::try_from(length).ok()).filter(|length| (1..=1 << 20).contains(length));
            Physical::Fixed(length.ok_or_else(|| damaged("a fixed-length column has no valid length"))?)
        }
        _ => return Err(damaged("a column has an unknown physical type")),
    })
}

fn leaf_of(element: &Thrift) -> Result<Leaf, ParquetError> {
    let name = String::from_utf8_lossy(element.binary(4).unwrap_or_default()).into_owned();
    let physical = physical_of(element)?;
    let nullable = match element.int(3) {
        Some(0) => false,
        Some(1) | None => true,
        _ => return Err(unsupported(&format!("the repeated column {name}"))),
    };
    let logical = element.field(10);
    let logical_kind = match logical {
        Some(Thrift::Struct(fields)) => fields.first().map(|(id, value)| (*id, value.clone())),
        _ => None,
    };
    let converted = element.int(6);
    let decimal = |scale: Option<i64>, precision: Option<i64>| ColumnType::Decimal {
        precision: precision.and_then(|value| u32::try_from(value).ok()).unwrap_or(38).min(38),
        scale: scale.and_then(|value| u32::try_from(value).ok()).unwrap_or(0).min(38),
    };
    let mut timestamp = (1, 1);
    let column_type = match (logical_kind, converted, physical) {
        (Some((1 | 4 | 12, _)), _, Physical::ByteArray) | (None, Some(0 | 4 | 19), Physical::ByteArray) => ColumnType::Text,
        (Some((5, value)), _, _) => decimal(value.int(1), value.int(2)),
        (None, Some(5), _) => decimal(element.int(7), element.int(8)),
        (Some((6, _)), _, Physical::Int32) | (None, Some(6), Physical::Int32) => ColumnType::Date,
        (Some((8, value)), _, Physical::Int64) => {
            let unit = match value.field(2) {
                Some(Thrift::Struct(fields)) => fields.first().map(|(id, _)| *id),
                _ => None,
            };
            timestamp = match unit {
                Some(1) => (1, 1_000),
                Some(3) => (1_000, 1),
                _ => (1, 1),
            };
            ColumnType::Timestamp { utc: value.boolean(1).unwrap_or(false) }
        }
        (None, Some(9), Physical::Int64) => {
            timestamp = (1, 1_000);
            ColumnType::Timestamp { utc: true }
        }
        (None, Some(10), Physical::Int64) => ColumnType::Timestamp { utc: true },
        (_, _, Physical::Int96) => ColumnType::Timestamp { utc: true },
        (_, _, Physical::Boolean) => ColumnType::Boolean,
        (_, _, Physical::Int32) => ColumnType::Int32,
        (_, _, Physical::Int64) => ColumnType::Int64,
        (_, _, Physical::Float) => ColumnType::Float,
        (_, _, Physical::Double) => ColumnType::Double,
        (_, _, Physical::ByteArray | Physical::Fixed(_)) => ColumnType::Binary,
    };
    Ok(Leaf { name, physical, column_type, nullable, timestamp_divisor: timestamp.0, timestamp_multiplier: timestamp.1 })
}

fn take<'b>(bytes: &'b [u8], at: &mut usize, count: usize) -> Result<&'b [u8], ParquetError> {
    let end = at.checked_add(count).filter(|end| *end <= bytes.len()).ok_or_else(|| damaged("values run past their page"))?;
    let slice = &bytes[*at..end];
    *at = end;
    Ok(slice)
}

fn signed_big_endian(bytes: &[u8]) -> Result<i128, ParquetError> {
    if bytes.len() > 16 {
        return Err(unsupported("a decimal wider than 16 bytes"));
    }
    let negative = bytes.first().is_some_and(|byte| byte & 0x80 != 0);
    let mut buffer = if negative { [0xffu8; 16] } else { [0u8; 16] };
    buffer[16 - bytes.len()..].copy_from_slice(bytes);
    Ok(i128::from_be_bytes(buffer))
}

fn plain(leaf: &Leaf, bytes: &[u8], count: usize) -> Result<Vec<Cell>, ParquetError> {
    let mut at = 0;
    let mut cells = Vec::with_capacity(count.min(bytes.len().saturating_mul(8) + 1));
    for index in 0..count {
        let cell = match leaf.physical {
            Physical::Boolean => {
                let byte = *bytes.get(index / 8).ok_or_else(|| damaged("booleans run past their page"))?;
                Cell::Boolean(byte >> (index % 8) & 1 == 1)
            }
            Physical::Int32 => {
                let value = i64::from(i32::from_le_bytes(take(bytes, &mut at, 4)?.try_into().unwrap_or_default()));
                if matches!(leaf.column_type, ColumnType::Decimal { .. }) { Cell::Decimal(i128::from(value)) } else { Cell::Int(value) }
            }
            Physical::Int64 => {
                let value = i64::from_le_bytes(take(bytes, &mut at, 8)?.try_into().unwrap_or_default());
                match leaf.column_type {
                    ColumnType::Decimal { .. } => Cell::Decimal(i128::from(value)),
                    ColumnType::Timestamp { .. } => Cell::Int(value.saturating_mul(leaf.timestamp_multiplier) / leaf.timestamp_divisor),
                    _ => Cell::Int(value),
                }
            }
            Physical::Int96 => {
                let raw = take(bytes, &mut at, 12)?;
                let nanos = i64::from_le_bytes(raw[..8].try_into().unwrap_or_default());
                let day = i64::from(i32::from_le_bytes(raw[8..].try_into().unwrap_or_default()));
                Cell::Int((day - JULIAN_DAY_OF_EPOCH).saturating_mul(86_400_000_000).saturating_add(nanos / 1_000))
            }
            Physical::Float => Cell::Float(f64::from(f32::from_le_bytes(take(bytes, &mut at, 4)?.try_into().unwrap_or_default()))),
            Physical::Double => Cell::Float(f64::from_le_bytes(take(bytes, &mut at, 8)?.try_into().unwrap_or_default())),
            Physical::ByteArray | Physical::Fixed(_) => {
                let length = match leaf.physical {
                    Physical::Fixed(length) => length,
                    _ => u32::from_le_bytes(take(bytes, &mut at, 4)?.try_into().unwrap_or_default()) as usize,
                };
                let raw = take(bytes, &mut at, length)?;
                if matches!(leaf.column_type, ColumnType::Decimal { .. }) { Cell::Decimal(signed_big_endian(raw)?) } else { Cell::Bytes(raw.to_vec()) }
            }
        };
        cells.push(cell);
    }
    Ok(cells)
}

fn levels(bytes: &[u8], count: usize) -> Result<Vec<u64>, ParquetError> {
    let (levels, _) = eggdb_columnar::decode_hybrid(bytes, 1, count).map_err(|error| damaged(&error.0))?;
    Ok(levels)
}

fn expand(present: Vec<Cell>, defined: &[u64]) -> Result<Vec<Cell>, ParquetError> {
    let mut values = present.into_iter();
    defined
        .iter()
        .map(|level| if *level == 1 { values.next().ok_or_else(|| damaged("a page has fewer values than its levels")) } else { Ok(Cell::Null) })
        .collect()
}

fn page_values(leaf: &Leaf, data: &[u8], encoding: i64, count: usize, dictionary: Option<&[Cell]>) -> Result<Vec<Cell>, ParquetError> {
    match encoding {
        0 => plain(leaf, data, count),
        2 | 8 => {
            let dictionary = dictionary.ok_or_else(|| damaged("a dictionary page is missing"))?;
            let (&width, rest) = data.split_first().ok_or_else(|| damaged("a dictionary page's indexes are missing"))?;
            let (indexes, _) = eggdb_columnar::decode_hybrid(rest, width, count).map_err(|error| damaged(&error.0))?;
            indexes
                .into_iter()
                .map(|index| usize::try_from(index).ok().and_then(|index| dictionary.get(index)).cloned().ok_or_else(|| damaged("a dictionary index is out of range")))
                .collect()
        }
        other => Err(unsupported(&format!("Parquet value encoding {other}"))),
    }
}

fn decompressed(codec: i64, data: &[u8], size: usize) -> Result<Vec<u8>, ParquetError> {
    if size > MAX_PAGE_BYTES {
        return Err(damaged("a page is larger than 1 GiB"));
    }
    match codec {
        0 => Ok(data.to_vec()),
        1 => crate::snappy::decompress(data, size),
        2 => Err(unsupported("GZIP-compressed Parquet")),
        6 => Err(unsupported("ZSTD-compressed Parquet")),
        other => Err(unsupported(&format!("Parquet compression codec {other}"))),
    }
}

fn read_chunk(file: &[u8], leaf: &Leaf, meta: &Thrift) -> Result<Vec<Cell>, ParquetError> {
    let codec = meta.int(4).unwrap_or(0);
    let wanted = meta.int(5).and_then(|count| u64::try_from(count).ok()).filter(|count| *count <= MAX_VALUES).ok_or_else(|| damaged("a column chunk has an invalid value count"))? as usize;
    let data_offset = meta.int(9).ok_or_else(|| damaged("a column chunk has no data page offset"))?;
    let start = meta.int(11).filter(|offset| *offset > 0 && *offset < data_offset).unwrap_or(data_offset);
    let mut at = usize::try_from(start).ok().filter(|at| *at < file.len()).ok_or_else(|| damaged("a column chunk starts outside the file"))?;
    let mut cells = Vec::with_capacity(wanted.min(1 << 20));
    let mut dictionary: Option<Vec<Cell>> = None;
    while cells.len() < wanted {
        let mut reader = Reader::new(&file[at..]);
        let header = reader.structure(0)?;
        at += reader.at;
        let compressed = header.int(3).and_then(|size| usize::try_from(size).ok()).ok_or_else(|| damaged("a page has no size"))?;
        let uncompressed = header.int(2).and_then(|size| usize::try_from(size).ok()).ok_or_else(|| damaged("a page has no size"))?;
        let body = file.get(at..at.saturating_add(compressed)).ok_or_else(|| damaged("a page runs past the file"))?;
        at += compressed;
        match header.int(1) {
            Some(2) => {
                let page = header.field(7).ok_or_else(|| damaged("a dictionary page has no header"))?;
                let count = page.int(1).and_then(|count| usize::try_from(count).ok()).filter(|count| *count as u64 <= MAX_VALUES).ok_or_else(|| damaged("a dictionary page has an invalid count"))?;
                let data = decompressed(codec, body, uncompressed)?;
                dictionary = Some(plain(leaf, &data, count)?);
            }
            Some(0) => {
                let page = header.field(5).ok_or_else(|| damaged("a data page has no header"))?;
                let count = page.int(1).and_then(|count| usize::try_from(count).ok()).ok_or_else(|| damaged("a data page has an invalid count"))?;
                let count = count.min(wanted - cells.len());
                let data = decompressed(codec, body, uncompressed)?;
                let (defined, values_at) = if leaf.nullable {
                    if page.int(3).unwrap_or(3) != 3 {
                        return Err(unsupported("bit-packed definition levels"));
                    }
                    let length = data.get(..4).map(|raw| u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize).ok_or_else(|| damaged("a page's levels are missing"))?;
                    let raw = data.get(4..4usize.saturating_add(length)).ok_or_else(|| damaged("a page's levels run past it"))?;
                    (Some(levels(raw, count)?), 4 + length)
                } else {
                    (None, 0)
                };
                let present = defined.as_ref().map_or(count, |defined| defined.iter().filter(|level| **level == 1).count());
                let values = page_values(leaf, &data[values_at..], page.int(2).unwrap_or(0), present, dictionary.as_deref())?;
                cells.extend(match defined {
                    Some(defined) => expand(values, &defined)?,
                    None => values,
                });
            }
            Some(3) => {
                let page = header.field(8).ok_or_else(|| damaged("a data page has no header"))?;
                let count = page.int(1).and_then(|count| usize::try_from(count).ok()).ok_or_else(|| damaged("a data page has an invalid count"))?;
                let count = count.min(wanted - cells.len());
                let definition_bytes = page.int(5).and_then(|length| usize::try_from(length).ok()).unwrap_or(0);
                let repetition_bytes = page.int(6).and_then(|length| usize::try_from(length).ok()).unwrap_or(0);
                let level_bytes = definition_bytes.checked_add(repetition_bytes).filter(|total| *total <= body.len()).ok_or_else(|| damaged("a page's levels run past it"))?;
                let values_raw = &body[level_bytes..];
                let data = if page.boolean(7).unwrap_or(true) { decompressed(codec, values_raw, uncompressed.saturating_sub(level_bytes))? } else { values_raw.to_vec() };
                let defined = if leaf.nullable { Some(levels(&body[repetition_bytes..level_bytes], count)?) } else { None };
                let present = defined.as_ref().map_or(count, |defined| defined.iter().filter(|level| **level == 1).count());
                let values = page_values(leaf, &data, page.int(4).unwrap_or(0), present, dictionary.as_deref())?;
                cells.extend(match defined {
                    Some(defined) => expand(values, &defined)?,
                    None => values,
                });
            }
            Some(1) => {}
            _ => return Err(damaged("a page has an unknown type")),
        }
        if at >= file.len() && cells.len() < wanted {
            return Err(damaged("a column chunk ends before its values"));
        }
    }
    Ok(cells)
}

/// Reads a whole Parquet file held in memory.
pub fn read(file: &[u8]) -> Result<Vec<Column>, ParquetError> {
    if file.len() < 12 || !file.starts_with(MAGIC) || !file.ends_with(MAGIC) {
        return Err(ParquetError("not a Parquet file: it does not start and end with PAR1".to_owned()));
    }
    let footer_length = u32::from_le_bytes(file[file.len() - 8..file.len() - 4].try_into().unwrap_or_default()) as usize;
    let footer_start = (file.len() - 8).checked_sub(footer_length).filter(|start| *start >= 4).ok_or_else(|| damaged("the footer is longer than the file"))?;
    let metadata = Reader::new(&file[footer_start..file.len() - 8]).structure(0)?;
    let schema = metadata.list(2).ok_or_else(|| damaged("the file has no schema"))?;
    let (root, elements) = schema.split_first().ok_or_else(|| damaged("the file has an empty schema"))?;
    if root.int(5).unwrap_or(0) as usize != elements.len() || elements.iter().any(|element| element.int(5).unwrap_or(0) > 0) {
        return Err(unsupported("nested Parquet columns"));
    }
    let leaves = elements.iter().map(leaf_of).collect::<Result<Vec<_>, _>>()?;
    let mut columns: Vec<Column> = leaves
        .iter()
        .map(|leaf| Column { name: leaf.name.clone(), column_type: leaf.column_type, nullable: leaf.nullable, cells: Vec::new() })
        .collect();
    let mut total = 0u64;
    for group in metadata.list(4).unwrap_or_default() {
        let chunks = group.list(1).ok_or_else(|| damaged("a row group has no columns"))?;
        if chunks.len() != leaves.len() {
            return Err(damaged("a row group's columns do not match the schema"));
        }
        for ((chunk, leaf), column) in chunks.iter().zip(&leaves).zip(&mut columns) {
            if chunk.binary(1).is_some() {
                return Err(unsupported("column chunks in other files"));
            }
            let meta = chunk.field(3).ok_or_else(|| damaged("a column chunk has no metadata"))?;
            let cells = read_chunk(file, leaf, meta)?;
            total += cells.len() as u64;
            if total > MAX_VALUES {
                return Err(ParquetError("the Parquet file holds more values than EggDB reads at once".to_owned()));
            }
            column.cells.extend(cells);
        }
    }
    Ok(columns)
}
