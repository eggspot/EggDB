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
            let length = element
                .int(2)
                .and_then(|length| usize::try_from(length).ok())
                .filter(|length| (1..=1 << 20).contains(length));
            Physical::Fixed(
                length.ok_or_else(|| damaged("a fixed-length column has no valid length"))?,
            )
        }
        _ => return Err(damaged("a column has an unknown physical type")),
    })
}

fn leaf_of(element: &Thrift) -> Result<Leaf, ParquetError> {
    leaf_with(element, false)
}

fn leaf_with(element: &Thrift, allow_repeated: bool) -> Result<Leaf, ParquetError> {
    let name = String::from_utf8_lossy(element.binary(4).unwrap_or_default()).into_owned();
    let physical = physical_of(element)?;
    let nullable = match element.int(3) {
        Some(0) => false,
        Some(1) | None => true,
        Some(2) if allow_repeated => true,
        _ => return Err(unsupported(&format!("the repeated column {name}"))),
    };
    let logical = element.field(10);
    let logical_kind = match logical {
        Some(Thrift::Struct(fields)) => fields.first().map(|(id, value)| (*id, value.clone())),
        _ => None,
    };
    let converted = element.int(6);
    let decimal = |scale: Option<i64>, precision: Option<i64>| ColumnType::Decimal {
        precision: precision
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(38)
            .min(38),
        scale: scale
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(0)
            .min(38),
    };
    let mut timestamp = (1, 1);
    let column_type = match (logical_kind, converted, physical) {
        (Some((1 | 4 | 12, _)), _, Physical::ByteArray)
        | (None, Some(0 | 4 | 19), Physical::ByteArray) => ColumnType::Text,
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
            ColumnType::Timestamp {
                utc: value.boolean(1).unwrap_or(false),
            }
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
    Ok(Leaf {
        name,
        physical,
        column_type,
        nullable,
        timestamp_divisor: timestamp.0,
        timestamp_multiplier: timestamp.1,
    })
}

fn take<'b>(bytes: &'b [u8], at: &mut usize, count: usize) -> Result<&'b [u8], ParquetError> {
    let end = at
        .checked_add(count)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| damaged("values run past their page"))?;
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
                let byte = *bytes
                    .get(index / 8)
                    .ok_or_else(|| damaged("booleans run past their page"))?;
                Cell::Boolean(byte >> (index % 8) & 1 == 1)
            }
            Physical::Int32 => {
                let value = i64::from(i32::from_le_bytes(
                    take(bytes, &mut at, 4)?.try_into().unwrap_or_default(),
                ));
                if matches!(leaf.column_type, ColumnType::Decimal { .. }) {
                    Cell::Decimal(i128::from(value))
                } else {
                    Cell::Int(value)
                }
            }
            Physical::Int64 => {
                let value =
                    i64::from_le_bytes(take(bytes, &mut at, 8)?.try_into().unwrap_or_default());
                match leaf.column_type {
                    ColumnType::Decimal { .. } => Cell::Decimal(i128::from(value)),
                    ColumnType::Timestamp { .. } => Cell::Int(
                        value.saturating_mul(leaf.timestamp_multiplier) / leaf.timestamp_divisor,
                    ),
                    _ => Cell::Int(value),
                }
            }
            Physical::Int96 => {
                let raw = take(bytes, &mut at, 12)?;
                let nanos = i64::from_le_bytes(raw[..8].try_into().unwrap_or_default());
                let day = i64::from(i32::from_le_bytes(raw[8..].try_into().unwrap_or_default()));
                Cell::Int(
                    (day - JULIAN_DAY_OF_EPOCH)
                        .saturating_mul(86_400_000_000)
                        .saturating_add(nanos / 1_000),
                )
            }
            Physical::Float => Cell::Float(f64::from(f32::from_le_bytes(
                take(bytes, &mut at, 4)?.try_into().unwrap_or_default(),
            ))),
            Physical::Double => Cell::Float(f64::from_le_bytes(
                take(bytes, &mut at, 8)?.try_into().unwrap_or_default(),
            )),
            Physical::ByteArray | Physical::Fixed(_) => {
                let length = match leaf.physical {
                    Physical::Fixed(length) => length,
                    _ => {
                        u32::from_le_bytes(take(bytes, &mut at, 4)?.try_into().unwrap_or_default())
                            as usize
                    }
                };
                let raw = take(bytes, &mut at, length)?;
                if matches!(leaf.column_type, ColumnType::Decimal { .. }) {
                    Cell::Decimal(signed_big_endian(raw)?)
                } else {
                    Cell::Bytes(raw.to_vec())
                }
            }
        };
        cells.push(cell);
    }
    Ok(cells)
}

fn levels(bytes: &[u8], count: usize) -> Result<Vec<u64>, ParquetError> {
    let (levels, _) =
        eggdb_columnar::decode_hybrid(bytes, 1, count).map_err(|error| damaged(&error.0))?;
    Ok(levels)
}

fn expand(present: Vec<Cell>, defined: &[u64]) -> Result<Vec<Cell>, ParquetError> {
    let mut values = present.into_iter();
    defined
        .iter()
        .map(|level| {
            if *level == 1 {
                values
                    .next()
                    .ok_or_else(|| damaged("a page has fewer values than its levels"))
            } else {
                Ok(Cell::Null)
            }
        })
        .collect()
}

fn page_values(
    leaf: &Leaf,
    data: &[u8],
    encoding: i64,
    count: usize,
    dictionary: Option<&[Cell]>,
) -> Result<Vec<Cell>, ParquetError> {
    match encoding {
        0 => plain(leaf, data, count),
        2 | 8 => {
            let dictionary = dictionary.ok_or_else(|| damaged("a dictionary page is missing"))?;
            let (&width, rest) = data
                .split_first()
                .ok_or_else(|| damaged("a dictionary page's indexes are missing"))?;
            let (indexes, _) = eggdb_columnar::decode_hybrid(rest, width, count)
                .map_err(|error| damaged(&error.0))?;
            indexes
                .into_iter()
                .map(|index| {
                    usize::try_from(index)
                        .ok()
                        .and_then(|index| dictionary.get(index))
                        .cloned()
                        .ok_or_else(|| damaged("a dictionary index is out of range"))
                })
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
        1 => eggdb_columnar::snappy_decompress(data, size)
            .map_err(|error| damaged(&format!("a Snappy block: {}", error.0))),
        2 => eggdb_content::gunzip(data, size).map_err(|error| damaged(&format!("a GZIP page: {error}"))),
        6 => eggdb_storage::decompress_zstd(data, size).map_err(|error| damaged(&format!("a ZSTD page: {error}"))),
        other => Err(unsupported(&format!("Parquet compression codec {other}"))),
    }
}

fn read_chunk(file: &[u8], leaf: &Leaf, meta: &Thrift) -> Result<Vec<Cell>, ParquetError> {
    let codec = meta.int(4).unwrap_or(0);
    let wanted =
        meta.int(5)
            .and_then(|count| u64::try_from(count).ok())
            .filter(|count| *count <= MAX_VALUES)
            .ok_or_else(|| damaged("a column chunk has an invalid value count"))? as usize;
    let data_offset = meta
        .int(9)
        .ok_or_else(|| damaged("a column chunk has no data page offset"))?;
    let start = meta
        .int(11)
        .filter(|offset| *offset > 0 && *offset < data_offset)
        .unwrap_or(data_offset);
    let mut at = usize::try_from(start)
        .ok()
        .filter(|at| *at < file.len())
        .ok_or_else(|| damaged("a column chunk starts outside the file"))?;
    let mut cells = Vec::with_capacity(wanted.min(1 << 20));
    let mut dictionary: Option<Vec<Cell>> = None;
    while cells.len() < wanted {
        let mut reader = Reader::new(&file[at..]);
        let header = reader.structure(0)?;
        at += reader.at;
        let compressed = header
            .int(3)
            .and_then(|size| usize::try_from(size).ok())
            .ok_or_else(|| damaged("a page has no size"))?;
        let uncompressed = header
            .int(2)
            .and_then(|size| usize::try_from(size).ok())
            .ok_or_else(|| damaged("a page has no size"))?;
        let body = file
            .get(at..at.saturating_add(compressed))
            .ok_or_else(|| damaged("a page runs past the file"))?;
        at += compressed;
        match header.int(1) {
            Some(2) => {
                let page = header
                    .field(7)
                    .ok_or_else(|| damaged("a dictionary page has no header"))?;
                let count = page
                    .int(1)
                    .and_then(|count| usize::try_from(count).ok())
                    .filter(|count| *count as u64 <= MAX_VALUES)
                    .ok_or_else(|| damaged("a dictionary page has an invalid count"))?;
                let data = decompressed(codec, body, uncompressed)?;
                dictionary = Some(plain(leaf, &data, count)?);
            }
            Some(0) => {
                let page = header
                    .field(5)
                    .ok_or_else(|| damaged("a data page has no header"))?;
                let count = page
                    .int(1)
                    .and_then(|count| usize::try_from(count).ok())
                    .ok_or_else(|| damaged("a data page has an invalid count"))?;
                let count = count.min(wanted - cells.len());
                let data = decompressed(codec, body, uncompressed)?;
                let (defined, values_at) = if leaf.nullable {
                    if page.int(3).unwrap_or(3) != 3 {
                        return Err(unsupported("bit-packed definition levels"));
                    }
                    let length = data
                        .get(..4)
                        .map(|raw| u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize)
                        .ok_or_else(|| damaged("a page's levels are missing"))?;
                    let raw = data
                        .get(4..4usize.saturating_add(length))
                        .ok_or_else(|| damaged("a page's levels run past it"))?;
                    (Some(levels(raw, count)?), 4 + length)
                } else {
                    (None, 0)
                };
                let present = defined.as_ref().map_or(count, |defined| {
                    defined.iter().filter(|level| **level == 1).count()
                });
                let values = page_values(
                    leaf,
                    &data[values_at..],
                    page.int(2).unwrap_or(0),
                    present,
                    dictionary.as_deref(),
                )?;
                cells.extend(match defined {
                    Some(defined) => expand(values, &defined)?,
                    None => values,
                });
            }
            Some(3) => {
                let page = header
                    .field(8)
                    .ok_or_else(|| damaged("a data page has no header"))?;
                let count = page
                    .int(1)
                    .and_then(|count| usize::try_from(count).ok())
                    .ok_or_else(|| damaged("a data page has an invalid count"))?;
                let count = count.min(wanted - cells.len());
                let definition_bytes = page
                    .int(5)
                    .and_then(|length| usize::try_from(length).ok())
                    .unwrap_or(0);
                let repetition_bytes = page
                    .int(6)
                    .and_then(|length| usize::try_from(length).ok())
                    .unwrap_or(0);
                let level_bytes = definition_bytes
                    .checked_add(repetition_bytes)
                    .filter(|total| *total <= body.len())
                    .ok_or_else(|| damaged("a page's levels run past it"))?;
                let values_raw = &body[level_bytes..];
                let data = if page.boolean(7).unwrap_or(true) {
                    decompressed(codec, values_raw, uncompressed.saturating_sub(level_bytes))?
                } else {
                    values_raw.to_vec()
                };
                let defined = if leaf.nullable {
                    Some(levels(&body[repetition_bytes..level_bytes], count)?)
                } else {
                    None
                };
                let present = defined.as_ref().map_or(count, |defined| {
                    defined.iter().filter(|level| **level == 1).count()
                });
                let values = page_values(
                    leaf,
                    &data,
                    page.int(4).unwrap_or(0),
                    present,
                    dictionary.as_deref(),
                )?;
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
        return Err(ParquetError(
            "not a Parquet file: it does not start and end with PAR1".to_owned(),
        ));
    }
    let footer_length = u32::from_le_bytes(
        file[file.len() - 8..file.len() - 4]
            .try_into()
            .unwrap_or_default(),
    ) as usize;
    let footer_start = (file.len() - 8)
        .checked_sub(footer_length)
        .filter(|start| *start >= 4)
        .ok_or_else(|| damaged("the footer is longer than the file"))?;
    let metadata = Reader::new(&file[footer_start..file.len() - 8]).structure(0)?;
    let schema = metadata
        .list(2)
        .ok_or_else(|| damaged("the file has no schema"))?;
    let (root, elements) = schema
        .split_first()
        .ok_or_else(|| damaged("the file has an empty schema"))?;
    let flat = root.int(5).unwrap_or(0) as usize == elements.len()
        && elements
            .iter()
            .all(|element| element.int(5).unwrap_or(0) == 0 && element.int(3) != Some(2));
    if !flat {
        return read_nested(file, &metadata, root, elements);
    }
    let leaves = elements
        .iter()
        .map(leaf_of)
        .collect::<Result<Vec<_>, _>>()?;
    let mut columns: Vec<Column> = leaves
        .iter()
        .map(|leaf| Column {
            name: leaf.name.clone(),
            column_type: leaf.column_type,
            nullable: leaf.nullable,
            cells: Vec::new(),
        })
        .collect();
    let mut total = 0u64;
    for group in metadata.list(4).unwrap_or_default() {
        let chunks = group
            .list(1)
            .ok_or_else(|| damaged("a row group has no columns"))?;
        if chunks.len() != leaves.len() {
            return Err(damaged("a row group's columns do not match the schema"));
        }
        for ((chunk, leaf), column) in chunks.iter().zip(&leaves).zip(&mut columns) {
            if chunk.binary(1).is_some() {
                return Err(unsupported("column chunks in other files"));
            }
            let meta = chunk
                .field(3)
                .ok_or_else(|| damaged("a column chunk has no metadata"))?;
            let cells = read_chunk(file, leaf, meta)?;
            total += cells.len() as u64;
            if total > MAX_VALUES {
                return Err(ParquetError(
                    "the Parquet file holds more values than EggDB reads at once".to_owned(),
                ));
            }
            column.cells.extend(cells);
        }
    }
    Ok(columns)
}

const MOST_SCHEMA_DEPTH: usize = 32;
const REPEATED: i64 = 2;
const CONVERTED_MAP: i64 = 1;
const CONVERTED_MAP_KEY_VALUE: i64 = 2;
const CONVERTED_LIST: i64 = 3;
const LOGICAL_MAP: i64 = 2;
const LOGICAL_LIST: i64 = 3;

struct SchemaNode {
    name: String,
    repetition: i64,
    annotation: Option<i64>,
    children: Vec<SchemaNode>,
    leaf: Option<usize>,
    definition: u64,
}

struct LeafLevels {
    element: usize,
    definition: u64,
    repetition: u64,
}

fn schema_nodes(
    elements: &[Thrift],
    at: &mut usize,
    count: usize,
    levels: (u64, u64),
    leaves: &mut Vec<LeafLevels>,
    depth: usize,
) -> Result<Vec<SchemaNode>, ParquetError> {
    if depth > MOST_SCHEMA_DEPTH {
        return Err(unsupported("Parquet schemas nested deeper than 32 levels"));
    }
    let mut nodes = Vec::with_capacity(count.min(elements.len()));
    for _ in 0..count {
        let index = *at;
        let element = elements.get(index).ok_or_else(|| damaged("the schema has fewer elements than its groups say"))?;
        *at += 1;
        let repetition = element.int(3).unwrap_or(0);
        let definition = levels.0 + u64::from(repetition != 0);
        let repeat = levels.1 + u64::from(repetition == REPEATED);
        let children_count = usize::try_from(element.int(5).unwrap_or(0)).map_err(|_| damaged("a group has a negative child count"))?;
        let logical = match element.field(10) {
            Some(Thrift::Struct(fields)) => fields.first().map(|(id, _)| i64::from(*id)),
            _ => None,
        };
        let annotation = match (logical, element.int(6)) {
            (Some(LOGICAL_LIST), _) | (_, Some(CONVERTED_LIST)) => Some(CONVERTED_LIST),
            (Some(LOGICAL_MAP), _) | (_, Some(CONVERTED_MAP | CONVERTED_MAP_KEY_VALUE)) => Some(CONVERTED_MAP),
            _ => None,
        };
        let (children, leaf) = if children_count > 0 {
            (schema_nodes(elements, at, children_count, (definition, repeat), leaves, depth + 1)?, None)
        } else {
            leaves.push(LeafLevels {
                element: index,
                definition,
                repetition: repeat,
            });
            (Vec::new(), Some(leaves.len() - 1))
        };
        nodes.push(SchemaNode {
            name: String::from_utf8_lossy(element.binary(4).unwrap_or_default()).into_owned(),
            repetition,
            annotation,
            children,
            leaf,
            definition,
        });
    }
    Ok(nodes)
}

enum Output {
    Flat(usize),
    List { leaf: usize, outer_definition: u64 },
    Map { key: usize, value: usize, outer_definition: u64 },
    Struct { definition: u64, required: bool, fields: Vec<(String, usize)> },
}

fn output_of(node: &SchemaNode) -> Result<Output, ParquetError> {
    let unsupported_shape = || unsupported(&format!("the nested Parquet column {} (lists, maps and structs of plain values are read)", node.name));
    if let Some(leaf) = node.leaf {
        return Ok(if node.repetition == REPEATED {
            Output::List { leaf, outer_definition: node.definition - 1 }
        } else {
            Output::Flat(leaf)
        });
    }
    let plain_leaf = |child: &SchemaNode| child.leaf.filter(|_| child.repetition != REPEATED);
    match (node.annotation, node.children.as_slice()) {
        (Some(CONVERTED_LIST), [repeated]) if repeated.repetition == REPEATED => {
            let leaf = match (repeated.leaf, repeated.children.as_slice()) {
                (Some(leaf), _) => leaf,
                (None, [element]) => plain_leaf(element).ok_or_else(unsupported_shape)?,
                _ => return Err(unsupported_shape()),
            };
            Ok(Output::List { leaf, outer_definition: repeated.definition - 1 })
        }
        (Some(CONVERTED_MAP), [repeated]) if repeated.repetition == REPEATED => match repeated.children.as_slice() {
            [key, value] => Ok(Output::Map {
                key: plain_leaf(key).ok_or_else(unsupported_shape)?,
                value: plain_leaf(value).ok_or_else(unsupported_shape)?,
                outer_definition: repeated.definition - 1,
            }),
            _ => Err(unsupported_shape()),
        },
        (None, children) if node.repetition != REPEATED && !children.is_empty() => {
            let fields = children
                .iter()
                .map(|child| plain_leaf(child).map(|leaf| (child.name.clone(), leaf)).ok_or_else(unsupported_shape))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Output::Struct { definition: node.definition, required: node.repetition == 0, fields })
        }
        _ => Err(unsupported_shape()),
    }
}

fn level_width(most: u64) -> u8 {
    (64 - most.leading_zeros()) as u8
}

struct Leveled {
    repetitions: Vec<u64>,
    definitions: Vec<u64>,
    values: Vec<Cell>,
}

fn read_leveled_chunk(file: &[u8], leaf: &Leaf, meta: &Thrift, levels: &LeafLevels) -> Result<Leveled, ParquetError> {
    let codec = meta.int(4).unwrap_or(0);
    let wanted = meta
        .int(5)
        .and_then(|count| u64::try_from(count).ok())
        .filter(|count| *count <= MAX_VALUES)
        .ok_or_else(|| damaged("a column chunk has an invalid value count"))? as usize;
    let data_offset = meta.int(9).ok_or_else(|| damaged("a column chunk has no data page offset"))?;
    let start = meta.int(11).filter(|offset| *offset > 0 && *offset < data_offset).unwrap_or(data_offset);
    let mut at = usize::try_from(start)
        .ok()
        .filter(|at| *at < file.len())
        .ok_or_else(|| damaged("a column chunk starts outside the file"))?;
    let mut out = Leveled {
        repetitions: Vec::with_capacity(wanted.min(1 << 20)),
        definitions: Vec::with_capacity(wanted.min(1 << 20)),
        values: Vec::new(),
    };
    let mut dictionary: Option<Vec<Cell>> = None;
    let (rep_width, def_width) = (level_width(levels.repetition), level_width(levels.definition));
    let decode_levels = |bytes: &[u8], width: u8, count: usize| -> Result<Vec<u64>, ParquetError> {
        if width == 0 {
            return Ok(vec![0; count]);
        }
        eggdb_columnar::decode_hybrid(bytes, width, count)
            .map(|(levels, _)| levels)
            .map_err(|error| damaged(&error.0))
    };
    while out.definitions.len() < wanted {
        let mut reader = Reader::new(&file[at..]);
        let header = reader.structure(0)?;
        at += reader.at;
        let size = |id: i16| header.int(id).and_then(|size| usize::try_from(size).ok()).ok_or_else(|| damaged("a page has no size"));
        let (compressed, uncompressed) = (size(3)?, size(2)?);
        let body = file.get(at..at.saturating_add(compressed)).ok_or_else(|| damaged("a page runs past the file"))?;
        at += compressed;
        let (repetitions, definitions, values_data, encoding) = match header.int(1) {
            Some(2) => {
                let page = header.field(7).ok_or_else(|| damaged("a dictionary page has no header"))?;
                let count = page
                    .int(1)
                    .and_then(|count| usize::try_from(count).ok())
                    .filter(|count| *count as u64 <= MAX_VALUES)
                    .ok_or_else(|| damaged("a dictionary page has an invalid count"))?;
                dictionary = Some(plain(leaf, &decompressed(codec, body, uncompressed)?, count)?);
                continue;
            }
            Some(0) => {
                let page = header.field(5).ok_or_else(|| damaged("a data page has no header"))?;
                let count = page.int(1).and_then(|count| usize::try_from(count).ok()).ok_or_else(|| damaged("a data page has an invalid count"))?;
                let count = count.min(wanted - out.definitions.len());
                let data = decompressed(codec, body, uncompressed)?;
                let mut cursor = 0usize;
                let mut prefixed = |width: u8| -> Result<Vec<u64>, ParquetError> {
                    if width == 0 {
                        return Ok(vec![0; count]);
                    }
                    let length = data
                        .get(cursor..cursor + 4)
                        .map(|raw| u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize)
                        .ok_or_else(|| damaged("a page's levels are missing"))?;
                    let raw = data.get(cursor + 4..(cursor + 4).saturating_add(length)).ok_or_else(|| damaged("a page's levels run past it"))?;
                    cursor += 4 + length;
                    decode_levels(raw, width, count)
                };
                let repetitions = prefixed(rep_width)?;
                let definitions = prefixed(def_width)?;
                (repetitions, definitions, data[cursor..].to_vec(), page.int(2).unwrap_or(0))
            }
            Some(3) => {
                let page = header.field(8).ok_or_else(|| damaged("a data page has no header"))?;
                let count = page.int(1).and_then(|count| usize::try_from(count).ok()).ok_or_else(|| damaged("a data page has an invalid count"))?;
                let count = count.min(wanted - out.definitions.len());
                let definition_bytes = page.int(5).and_then(|length| usize::try_from(length).ok()).unwrap_or(0);
                let repetition_bytes = page.int(6).and_then(|length| usize::try_from(length).ok()).unwrap_or(0);
                let level_bytes = definition_bytes
                    .checked_add(repetition_bytes)
                    .filter(|total| *total <= body.len())
                    .ok_or_else(|| damaged("a page's levels run past it"))?;
                let repetitions = decode_levels(&body[..repetition_bytes], rep_width, count)?;
                let definitions = decode_levels(&body[repetition_bytes..level_bytes], def_width, count)?;
                let values_raw = &body[level_bytes..];
                let data = if page.boolean(7).unwrap_or(true) {
                    decompressed(codec, values_raw, uncompressed.saturating_sub(level_bytes))?
                } else {
                    values_raw.to_vec()
                };
                (repetitions, definitions, data, page.int(4).unwrap_or(0))
            }
            Some(1) => continue,
            _ => return Err(damaged("a page has an unknown type")),
        };
        let present = definitions.iter().filter(|level| **level == levels.definition).count();
        out.values.extend(page_values(leaf, &values_data, encoding, present, dictionary.as_deref())?);
        out.repetitions.extend(repetitions);
        out.definitions.extend(definitions);
        if at >= file.len() && out.definitions.len() < wanted {
            return Err(damaged("a column chunk ends before its values"));
        }
    }
    Ok(out)
}

fn json_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn civil_date(days: i64) -> String {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let of_era = shifted.rem_euclid(146_097);
    let year_of_era = (of_era - of_era / 1460 + of_era / 36_524 - of_era / 146_096) / 365;
    let day_of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn value_json(column_type: ColumnType, cell: &Cell) -> String {
    match (column_type, cell) {
        (_, Cell::Null) => "null".to_owned(),
        (_, Cell::Boolean(flag)) => flag.to_string(),
        (ColumnType::Date, Cell::Int(days)) => json_text(&civil_date(*days)),
        (ColumnType::Timestamp { .. }, Cell::Int(micros)) => {
            let days = micros.div_euclid(86_400_000_000);
            let within = micros.rem_euclid(86_400_000_000);
            json_text(&format!(
                "{}T{:02}:{:02}:{:02}.{:06}",
                civil_date(days),
                within / 3_600_000_000,
                within / 60_000_000 % 60,
                within / 1_000_000 % 60,
                within % 1_000_000
            ))
        }
        (_, Cell::Int(number)) => number.to_string(),
        (_, Cell::Float(number)) if number.is_finite() => number.to_string(),
        (_, Cell::Float(number)) => json_text(&number.to_string()),
        (ColumnType::Text, Cell::Bytes(bytes)) => json_text(&String::from_utf8_lossy(bytes)),
        (_, Cell::Bytes(bytes)) => json_text(&bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>()),
        (ColumnType::Decimal { scale, .. }, Cell::Decimal(unscaled)) => {
            let digits = unscaled.unsigned_abs().to_string();
            let scale = scale as usize;
            let padded = if digits.len() <= scale { format!("{}{digits}", "0".repeat(scale + 1 - digits.len())) } else { digits };
            let (whole, fraction) = padded.split_at(padded.len() - scale);
            let sign = if *unscaled < 0 { "-" } else { "" };
            if scale == 0 { format!("{sign}{whole}") } else { format!("{sign}{whole}.{fraction}") }
        }
        (_, Cell::Decimal(unscaled)) => unscaled.to_string(),
    }
}

/// Each level entry's value as JSON: the next value where the entry reaches the leaf's full
/// definition level, `null` where the leaf itself is missing.
fn entry_values(leveled: &Leveled, full: u64, column_type: ColumnType) -> Result<Vec<String>, ParquetError> {
    let mut values = leveled.values.iter();
    leveled
        .definitions
        .iter()
        .map(|definition| {
            if *definition == full {
                values.next().map(|cell| value_json(column_type, cell)).ok_or_else(|| damaged("a page has fewer values than its levels"))
            } else {
                Ok("null".to_owned())
            }
        })
        .collect()
}

fn assembled_list(leveled: &Leveled, entries: &[String], outer: u64, map_values: Option<(&Leveled, &[String])>) -> Result<Vec<Cell>, ParquetError> {
    let mut records: Vec<Option<Vec<String>>> = Vec::new();
    for (index, (repetition, definition)) in leveled.repetitions.iter().zip(&leveled.definitions).enumerate() {
        if *repetition == 0 {
            records.push(if *definition < outer { None } else { Some(Vec::new()) });
        }
        let record = records.last_mut().ok_or_else(|| damaged("a column chunk does not start a record"))?;
        if *definition > outer {
            let items = record.as_mut().ok_or_else(|| damaged("a null list has items"))?;
            let key = entries.get(index).ok_or_else(|| damaged("levels and values disagree"))?;
            items.push(match map_values {
                None => key.clone(),
                Some((values, value_entries)) => {
                    if values.definitions.len() != leveled.definitions.len() {
                        return Err(damaged("a map's keys and values have different lengths"));
                    }
                    let key = if key.starts_with('"') { key.clone() } else { json_text(key) };
                    format!("{key}:{}", value_entries.get(index).ok_or_else(|| damaged("levels and values disagree"))?)
                }
            });
        }
    }
    Ok(records
        .into_iter()
        .map(|record| match (record, map_values.is_some()) {
            (None, _) => Cell::Null,
            (Some(items), false) => Cell::Bytes(format!("[{}]", items.join(",")).into_bytes()),
            (Some(items), true) => Cell::Bytes(format!("{{{}}}", items.join(",")).into_bytes()),
        })
        .collect())
}

/// Reads a Parquet file whose schema has groups or repeated fields: its flat columns as usual, and
/// each list, map or struct of plain values as one text column of JSON (D838).
fn read_nested(file: &[u8], metadata: &Thrift, root: &Thrift, elements: &[Thrift]) -> Result<Vec<Column>, ParquetError> {
    let mut at = 0;
    let mut leaf_levels = Vec::new();
    let top_count = usize::try_from(root.int(5).unwrap_or(0)).map_err(|_| damaged("the root has a negative child count"))?;
    let tops = schema_nodes(elements, &mut at, top_count, (0, 0), &mut leaf_levels, 0)?;
    if at != elements.len() {
        return Err(damaged("the schema has more elements than its groups hold"));
    }
    let leaves = leaf_levels
        .iter()
        .map(|levels| leaf_with(&elements[levels.element], true))
        .collect::<Result<Vec<_>, _>>()?;
    let outputs = tops.iter().map(output_of).collect::<Result<Vec<_>, _>>()?;
    let mut columns: Vec<Column> = tops
        .iter()
        .zip(&outputs)
        .map(|(node, output)| Column {
            name: node.name.clone(),
            column_type: match output {
                Output::Flat(leaf) => leaves[*leaf].column_type,
                _ => ColumnType::Text,
            },
            nullable: match output {
                Output::Flat(leaf) => leaves[*leaf].nullable,
                _ => true,
            },
            cells: Vec::new(),
        })
        .collect();
    let mut total = 0u64;
    for group in metadata.list(4).unwrap_or_default() {
        let chunks = group.list(1).ok_or_else(|| damaged("a row group has no columns"))?;
        if chunks.len() != leaves.len() {
            return Err(damaged("a row group's columns do not match the schema"));
        }
        let meta_of = |leaf: usize| -> Result<&Thrift, ParquetError> {
            let chunk = &chunks[leaf];
            if chunk.binary(1).is_some() {
                return Err(unsupported("column chunks in other files"));
            }
            chunk.field(3).ok_or_else(|| damaged("a column chunk has no metadata"))
        };
        let leveled = |leaf: usize| read_leveled_chunk(file, &leaves[leaf], meta_of(leaf)?, &leaf_levels[leaf]);
        for (output, column) in outputs.iter().zip(&mut columns) {
            let cells = match output {
                Output::Flat(leaf) => read_chunk(file, &leaves[*leaf], meta_of(*leaf)?)?,
                Output::List { leaf, outer_definition } => {
                    let read = leveled(*leaf)?;
                    let entries = entry_values(&read, leaf_levels[*leaf].definition, leaves[*leaf].column_type)?;
                    assembled_list(&read, &entries, *outer_definition, None)?
                }
                Output::Map { key, value, outer_definition } => {
                    let keys = leveled(*key)?;
                    let values = leveled(*value)?;
                    let key_entries = entry_values(&keys, leaf_levels[*key].definition, leaves[*key].column_type)?;
                    let value_entries = entry_values(&values, leaf_levels[*value].definition, leaves[*value].column_type)?;
                    assembled_list(&keys, &key_entries, *outer_definition, Some((&values, &value_entries)))?
                }
                Output::Struct { definition, required, fields } => {
                    let mut reads = Vec::with_capacity(fields.len());
                    for (name, leaf) in fields {
                        let read = leveled(*leaf)?;
                        let entries = entry_values(&read, leaf_levels[*leaf].definition, leaves[*leaf].column_type)?;
                        reads.push((name, read, entries));
                    }
                    let rows = reads.first().map_or(0, |(_, read, _)| read.definitions.len());
                    if reads.iter().any(|(_, read, _)| read.definitions.len() != rows) {
                        return Err(damaged("a struct's fields have different lengths"));
                    }
                    (0..rows)
                        .map(|row| {
                            let present = *required || reads.first().is_some_and(|(_, read, _)| read.definitions[row] >= *definition);
                            if !present {
                                return Cell::Null;
                            }
                            let members: Vec<String> = reads.iter().map(|(name, _, entries)| format!("{}:{}", json_text(name), entries[row])).collect();
                            Cell::Bytes(format!("{{{}}}", members.join(",")).into_bytes())
                        })
                        .collect()
                }
            };
            total += cells.len() as u64;
            if total > MAX_VALUES {
                return Err(ParquetError("the Parquet file holds more values than EggDB reads at once".to_owned()));
            }
            column.cells.extend(cells);
        }
    }
    Ok(columns)
}

#[cfg(test)]
mod nested_tests {
    use super::*;

    fn leveled(repetitions: &[u64], definitions: &[u64], values: Vec<Cell>) -> Leveled {
        Leveled {
            repetitions: repetitions.to_vec(),
            definitions: definitions.to_vec(),
            values,
        }
    }

    #[test]
    fn three_level_lists_assemble_nulls_empties_and_null_items() {
        let read = leveled(&[0, 0, 0, 1, 1], &[0, 1, 3, 2, 3], vec![Cell::Int(7), Cell::Int(9)]);
        let entries = entry_values(&read, 3, ColumnType::Int32).unwrap();
        let cells = assembled_list(&read, &entries, 1, None).unwrap();
        assert_eq!(
            cells,
            vec![Cell::Null, Cell::Bytes(b"[]".to_vec()), Cell::Bytes(b"[7,null,9]".to_vec())]
        );
        let short = leveled(&[0], &[3], Vec::new());
        assert!(entry_values(&short, 3, ColumnType::Int32).is_err());
        let orphan = leveled(&[1], &[3], vec![Cell::Int(1)]);
        let entries = entry_values(&orphan, 3, ColumnType::Int32).unwrap();
        assert!(assembled_list(&orphan, &entries, 1, None).is_err());
    }

    #[test]
    fn maps_pair_keys_with_values_and_text_values_are_quoted() {
        let keys = leveled(&[0, 1, 0], &[2, 2, 1], vec![Cell::Bytes(b"a".to_vec()), Cell::Bytes(b"b".to_vec())]);
        let values = leveled(&[0, 1, 0], &[3, 2, 1], vec![Cell::Float(1.5)]);
        let key_entries = entry_values(&keys, 2, ColumnType::Text).unwrap();
        let value_entries = entry_values(&values, 3, ColumnType::Double).unwrap();
        let cells = assembled_list(&keys, &key_entries, 1, Some((&values, &value_entries))).unwrap();
        assert_eq!(cells, vec![Cell::Bytes(b"{\"a\":1.5,\"b\":null}".to_vec()), Cell::Bytes(b"{}".to_vec())]);
        assert_eq!(value_json(ColumnType::Date, &Cell::Int(19_723)), "\"2024-01-01\"");
        assert_eq!(value_json(ColumnType::Decimal { precision: 5, scale: 2 }, &Cell::Decimal(-5)), "-0.05");
        assert_eq!(level_width(0), 0);
        assert_eq!(level_width(3), 2);
    }

    #[test]
    fn schema_shapes_map_to_outputs() {
        let leaf = |name: &str, repetition: i64, leaf: usize, definition: u64| SchemaNode {
            name: name.to_owned(),
            repetition,
            annotation: None,
            children: Vec::new(),
            leaf: Some(leaf),
            definition,
        };
        let list = SchemaNode {
            name: "tags".to_owned(),
            repetition: 1,
            annotation: Some(CONVERTED_LIST),
            children: vec![SchemaNode {
                name: "list".to_owned(),
                repetition: REPEATED,
                annotation: None,
                children: vec![leaf("element", 1, 0, 3)],
                leaf: None,
                definition: 2,
            }],
            leaf: None,
            definition: 1,
        };
        assert!(matches!(output_of(&list).unwrap(), Output::List { leaf: 0, outer_definition: 1 }));
        let point = SchemaNode {
            name: "point".to_owned(),
            repetition: 1,
            annotation: None,
            children: vec![leaf("x", 0, 1, 1), leaf("y", 0, 2, 1)],
            leaf: None,
            definition: 1,
        };
        assert!(matches!(output_of(&point).unwrap(), Output::Struct { definition: 1, required: false, .. }));
        let nested = SchemaNode {
            name: "deep".to_owned(),
            repetition: 1,
            annotation: None,
            children: vec![point],
            leaf: None,
            definition: 1,
        };
        assert!(output_of(&nested).is_err());
        assert!(matches!(output_of(&leaf("ids", REPEATED, 4, 1)).unwrap(), Output::List { leaf: 4, outer_definition: 0 }));
    }
}
