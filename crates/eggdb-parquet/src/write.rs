use crate::thrift::Writer;
use crate::{Cell, Column, ColumnType, ParquetError};

const MAGIC: &[u8] = b"PAR1";
/// The most rows [`write`] puts in one row group.
pub const ROW_GROUP_ROWS: usize = 131_072;
const PLAIN: i64 = 0;
const RLE: i64 = 3;

fn physical(column_type: ColumnType) -> i64 {
    match column_type {
        ColumnType::Boolean => 0,
        ColumnType::Int32 | ColumnType::Date => 1,
        ColumnType::Int64 | ColumnType::Timestamp { .. } => 2,
        ColumnType::Decimal { precision, .. } if precision <= 18 => 2,
        ColumnType::Float => 4,
        ColumnType::Double => 5,
        ColumnType::Text | ColumnType::Binary | ColumnType::Decimal { .. } => 6,
    }
}

fn value_bytes(column: &Column, cells: &[Cell], out: &mut Vec<u8>) -> Result<(), ParquetError> {
    let mismatch = || {
        ParquetError(format!(
            "column {} holds a value that does not fit its type",
            column.name
        ))
    };
    let mut bits = Vec::new();
    for cell in cells {
        match (column.column_type, cell) {
            (_, Cell::Null) => {}
            (ColumnType::Boolean, Cell::Boolean(flag)) => bits.push(u64::from(*flag)),
            (ColumnType::Int32 | ColumnType::Date, Cell::Int(value)) => {
                out.extend_from_slice(
                    &i32::try_from(*value).map_err(|_| mismatch())?.to_le_bytes(),
                );
            }
            (ColumnType::Int64 | ColumnType::Timestamp { .. }, Cell::Int(value)) => {
                out.extend_from_slice(&value.to_le_bytes())
            }
            (ColumnType::Decimal { precision, .. }, Cell::Decimal(value)) if precision <= 18 => {
                out.extend_from_slice(
                    &i64::try_from(*value).map_err(|_| mismatch())?.to_le_bytes(),
                );
            }
            (ColumnType::Decimal { .. }, Cell::Decimal(value)) => {
                out.extend_from_slice(&16u32.to_le_bytes());
                out.extend_from_slice(&value.to_be_bytes());
            }
            (ColumnType::Float, Cell::Float(value)) => {
                out.extend_from_slice(&(*value as f32).to_le_bytes())
            }
            (ColumnType::Double, Cell::Float(value)) => out.extend_from_slice(&value.to_le_bytes()),
            (ColumnType::Text | ColumnType::Binary, Cell::Bytes(bytes)) => {
                out.extend_from_slice(
                    &u32::try_from(bytes.len())
                        .map_err(|_| mismatch())?
                        .to_le_bytes(),
                );
                out.extend_from_slice(bytes);
            }
            _ => return Err(mismatch()),
        }
    }
    if column.column_type == ColumnType::Boolean {
        eggdb_columnar::pack(&bits, 1, out);
    }
    Ok(())
}

/// How [`write_compressed`] compresses each data page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression {
    /// No compression (Parquet's `UNCOMPRESSED`).
    #[default]
    Uncompressed,
    /// GZIP, with EggDB's own DEFLATE writer.
    Gzip,
    /// Zstandard.
    Zstd,
}

impl Compression {
    fn codec(self) -> i64 {
        match self {
            Self::Uncompressed => 0,
            Self::Gzip => 2,
            Self::Zstd => 6,
        }
    }

    fn compress(self, data: Vec<u8>) -> Vec<u8> {
        match self {
            Self::Uncompressed => data,
            Self::Gzip => eggdb_content::gzip(&data),
            Self::Zstd => eggdb_storage::compress_zstd(&data),
        }
    }
}

fn page(column: &Column, cells: &[Cell]) -> Result<Vec<u8>, ParquetError> {
    let mut data = Vec::new();
    if column.nullable {
        let defined: Vec<u64> = cells
            .iter()
            .map(|cell| u64::from(*cell != Cell::Null))
            .collect();
        let mut levels = Vec::new();
        eggdb_columnar::encode_hybrid(&defined, 1, &mut levels);
        data.extend_from_slice(
            &u32::try_from(levels.len())
                .map_err(|_| ParquetError("a column is too large".to_owned()))?
                .to_le_bytes(),
        );
        data.extend_from_slice(&levels);
    } else if cells.contains(&Cell::Null) {
        return Err(ParquetError(format!(
            "column {} is not nullable but holds a null",
            column.name
        )));
    }
    value_bytes(column, cells, &mut data)?;
    Ok(data)
}

/// A column chunk's statistics: its null count, and its smallest and largest values in their
/// plain encoding (byte arrays without their length), as Parquet's type-defined order sorts
/// them; NaN takes no part.
fn statistics(column: &Column, cells: &[Cell]) -> (i64, Option<(Vec<u8>, Vec<u8>)>) {
    let nulls = cells.iter().filter(|cell| **cell == Cell::Null).count() as i64;
    let present = cells.iter().filter(|cell| **cell != Cell::Null);
    let bounds = match column.column_type {
        ColumnType::Boolean => {
            let flags: Vec<bool> = present
                .filter_map(|cell| match cell {
                    Cell::Boolean(flag) => Some(*flag),
                    _ => None,
                })
                .collect();
            let encode = |flag: bool| vec![u8::from(flag)];
            flags
                .iter()
                .min()
                .zip(flags.iter().max())
                .map(|(low, high)| (encode(*low), encode(*high)))
        }
        ColumnType::Int32 | ColumnType::Date => {
            let values: Vec<i32> = present
                .filter_map(|cell| match cell {
                    Cell::Int(value) => i32::try_from(*value).ok(),
                    _ => None,
                })
                .collect();
            values
                .iter()
                .min()
                .zip(values.iter().max())
                .map(|(low, high)| (low.to_le_bytes().to_vec(), high.to_le_bytes().to_vec()))
        }
        ColumnType::Int64 | ColumnType::Timestamp { .. } => {
            let values: Vec<i64> = present
                .filter_map(|cell| match cell {
                    Cell::Int(value) => Some(*value),
                    _ => None,
                })
                .collect();
            values
                .iter()
                .min()
                .zip(values.iter().max())
                .map(|(low, high)| (low.to_le_bytes().to_vec(), high.to_le_bytes().to_vec()))
        }
        ColumnType::Decimal { precision, .. } => {
            let values: Vec<i128> = present
                .filter_map(|cell| match cell {
                    Cell::Decimal(value) => Some(*value),
                    _ => None,
                })
                .collect();
            let encode = |value: i128| -> Option<Vec<u8>> {
                if precision <= 18 {
                    i64::try_from(value).ok().map(|value| value.to_le_bytes().to_vec())
                } else {
                    Some(value.to_be_bytes().to_vec())
                }
            };
            values
                .iter()
                .min()
                .zip(values.iter().max())
                .and_then(|(low, high)| Some((encode(*low)?, encode(*high)?)))
        }
        ColumnType::Float | ColumnType::Double => {
            let values: Vec<f64> = present
                .filter_map(|cell| match cell {
                    Cell::Float(value) if !value.is_nan() => Some(*value),
                    _ => None,
                })
                .collect();
            let encode = |value: f64| {
                if column.column_type == ColumnType::Float {
                    (value as f32).to_le_bytes().to_vec()
                } else {
                    value.to_le_bytes().to_vec()
                }
            };
            values
                .iter()
                .copied()
                .reduce(f64::min)
                .zip(values.iter().copied().reduce(f64::max))
                .map(|(low, high)| (encode(low), encode(high)))
        }
        ColumnType::Text | ColumnType::Binary => {
            let values: Vec<&Vec<u8>> = present
                .filter_map(|cell| match cell {
                    Cell::Bytes(bytes) => Some(bytes),
                    _ => None,
                })
                .collect();
            values
                .iter()
                .min()
                .zip(values.iter().max())
                .map(|(low, high)| ((*low).clone(), (*high).clone()))
        }
    };
    (nulls, bounds)
}

fn schema_element(thrift: &mut Writer, column: &Column) {
    thrift.list_struct();
    thrift.int(1, false, physical(column.column_type));
    thrift.int(3, false, i64::from(column.nullable));
    thrift.binary(4, column.name.as_bytes());
    match column.column_type {
        ColumnType::Text => thrift.int(6, false, 0),
        ColumnType::Date => thrift.int(6, false, 6),
        ColumnType::Decimal { precision, scale } => {
            thrift.int(6, false, 5);
            thrift.int(7, false, i64::from(scale));
            thrift.int(8, false, i64::from(precision));
        }
        _ => {}
    }
    match column.column_type {
        ColumnType::Text => {
            thrift.begin_struct(10);
            thrift.begin_struct(1);
            thrift.end();
            thrift.end();
        }
        ColumnType::Date => {
            thrift.begin_struct(10);
            thrift.begin_struct(6);
            thrift.end();
            thrift.end();
        }
        ColumnType::Timestamp { utc } => {
            thrift.begin_struct(10);
            thrift.begin_struct(8);
            thrift.boolean(1, utc);
            thrift.begin_struct(2);
            thrift.begin_struct(2);
            thrift.end();
            thrift.end();
            thrift.end();
            thrift.end();
        }
        _ => {}
    }
    thrift.end();
}

/// Writes `columns` (all with the same number of values) as a Parquet file: one row group,
/// one uncompressed plain data page per column.
/// Writes `columns` as a Parquet file with row groups of up to 131,072 rows, each column chunk
/// one plain data page with its statistics.
pub fn write(columns: &[Column]) -> Result<Vec<u8>, ParquetError> {
    write_with_row_groups(columns, ROW_GROUP_ROWS)
}

struct Chunk {
    offset: i64,
    size: i64,
    uncompressed_size: i64,
    nulls: i64,
    bounds: Option<(Vec<u8>, Vec<u8>)>,
}

/// Writes `columns` as [`write`] does, with at most `rows_per_group` rows in each row group.
pub fn write_with_row_groups(
    columns: &[Column],
    rows_per_group: usize,
) -> Result<Vec<u8>, ParquetError> {
    write_compressed(columns, rows_per_group, Compression::Uncompressed)
}

/// Writes `columns` as [`write_with_row_groups`] does, each data page compressed with
/// `compression`.
pub fn write_compressed(
    columns: &[Column],
    rows_per_group: usize,
    compression: Compression,
) -> Result<Vec<u8>, ParquetError> {
    let rows = columns.first().map_or(0, |column| column.cells.len());
    if columns.iter().any(|column| column.cells.len() != rows) {
        return Err(ParquetError(
            "every column must hold the same number of values".to_owned(),
        ));
    }
    let rows_per_group = rows_per_group.max(1);
    let mut file = MAGIC.to_vec();
    let mut groups: Vec<(usize, Vec<Chunk>)> = Vec::new();
    let mut start = 0;
    loop {
        let end = (start + rows_per_group).min(rows);
        let mut chunks = Vec::with_capacity(columns.len());
        for column in columns {
            let cells = &column.cells[start..end];
            let plain = page(column, cells)?;
            let plain_size = i64::try_from(plain.len())
                .map_err(|_| ParquetError("a column is too large".to_owned()))?;
            let data = compression.compress(plain);
            let size = i64::try_from(data.len())
                .map_err(|_| ParquetError("a column is too large".to_owned()))?;
            let mut header = Writer::default();
            header.begin();
            header.int(1, false, 0);
            header.int(2, false, plain_size);
            header.int(3, false, size);
            header.begin_struct(5);
            header.int(1, false, cells.len() as i64);
            header.int(2, false, PLAIN);
            header.int(3, false, RLE);
            header.int(4, false, RLE);
            header.end();
            header.end();
            let offset = file.len() as i64;
            file.extend_from_slice(&header.out);
            file.extend_from_slice(&data);
            let (nulls, bounds) = statistics(column, cells);
            let written = file.len() as i64 - offset;
            chunks.push(Chunk {
                offset,
                size: written,
                uncompressed_size: written - size + plain_size,
                nulls,
                bounds,
            });
        }
        groups.push((end - start, chunks));
        start = end;
        if start >= rows {
            break;
        }
    }
    let mut meta = Writer::default();
    meta.begin();
    meta.int(1, false, 1);
    meta.begin_list(2, 12, columns.len() + 1);
    meta.list_struct();
    meta.binary(4, b"schema");
    meta.int(5, false, columns.len() as i64);
    meta.end();
    for column in columns {
        schema_element(&mut meta, column);
    }
    meta.int(3, true, rows as i64);
    meta.begin_list(4, 12, groups.len());
    for (group_rows, chunks) in &groups {
        meta.list_struct();
        meta.begin_list(1, 12, columns.len());
        for (column, chunk) in columns.iter().zip(chunks) {
            meta.list_struct();
            meta.int(2, true, chunk.offset);
            meta.begin_struct(3);
            meta.int(1, false, physical(column.column_type));
            meta.begin_list(2, 5, 2);
            meta.list_int(PLAIN);
            meta.list_int(RLE);
            meta.begin_list(3, 8, 1);
            meta.list_binary(column.name.as_bytes());
            meta.int(4, false, compression.codec());
            meta.int(5, true, *group_rows as i64);
            meta.int(6, true, chunk.uncompressed_size);
            meta.int(7, true, chunk.size);
            meta.int(9, true, chunk.offset);
            meta.begin_struct(12);
            meta.int(3, true, chunk.nulls);
            if let Some((low, high)) = &chunk.bounds {
                meta.binary(5, high);
                meta.binary(6, low);
            }
            meta.end();
            meta.end();
            meta.end();
        }
        let total: i64 = chunks.iter().map(|chunk| chunk.uncompressed_size).sum();
        meta.int(2, true, total);
        meta.int(3, true, *group_rows as i64);
        meta.end();
    }
    meta.binary(6, b"EggDB");
    meta.begin_list(7, 12, columns.len());
    for _ in columns {
        meta.list_struct();
        meta.begin_struct(1);
        meta.end();
        meta.end();
    }
    meta.end();
    let footer = meta.out;
    file.extend_from_slice(&footer);
    file.extend_from_slice(
        &u32::try_from(footer.len())
            .map_err(|_| ParquetError("the footer is too large".to_owned()))?
            .to_le_bytes(),
    );
    file.extend_from_slice(MAGIC);
    Ok(file)
}
