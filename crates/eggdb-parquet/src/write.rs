use crate::thrift::Writer;
use crate::{Cell, Column, ColumnType, ParquetError};

const MAGIC: &[u8] = b"PAR1";
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

fn value_bytes(column: &Column, out: &mut Vec<u8>) -> Result<(), ParquetError> {
    let mismatch = || ParquetError(format!("column {} holds a value that does not fit its type", column.name));
    let mut bits = Vec::new();
    for cell in &column.cells {
        match (column.column_type, cell) {
            (_, Cell::Null) => {}
            (ColumnType::Boolean, Cell::Boolean(flag)) => bits.push(u64::from(*flag)),
            (ColumnType::Int32 | ColumnType::Date, Cell::Int(value)) => {
                out.extend_from_slice(&i32::try_from(*value).map_err(|_| mismatch())?.to_le_bytes());
            }
            (ColumnType::Int64 | ColumnType::Timestamp { .. }, Cell::Int(value)) => out.extend_from_slice(&value.to_le_bytes()),
            (ColumnType::Decimal { precision, .. }, Cell::Decimal(value)) if precision <= 18 => {
                out.extend_from_slice(&i64::try_from(*value).map_err(|_| mismatch())?.to_le_bytes());
            }
            (ColumnType::Decimal { .. }, Cell::Decimal(value)) => {
                out.extend_from_slice(&16u32.to_le_bytes());
                out.extend_from_slice(&value.to_be_bytes());
            }
            (ColumnType::Float, Cell::Float(value)) => out.extend_from_slice(&(*value as f32).to_le_bytes()),
            (ColumnType::Double, Cell::Float(value)) => out.extend_from_slice(&value.to_le_bytes()),
            (ColumnType::Text | ColumnType::Binary, Cell::Bytes(bytes)) => {
                out.extend_from_slice(&u32::try_from(bytes.len()).map_err(|_| mismatch())?.to_le_bytes());
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

fn page(column: &Column) -> Result<Vec<u8>, ParquetError> {
    let mut data = Vec::new();
    if column.nullable {
        let defined: Vec<u64> = column.cells.iter().map(|cell| u64::from(*cell != Cell::Null)).collect();
        let mut levels = Vec::new();
        eggdb_columnar::encode_hybrid(&defined, 1, &mut levels);
        data.extend_from_slice(&u32::try_from(levels.len()).map_err(|_| ParquetError("a column is too large".to_owned()))?.to_le_bytes());
        data.extend_from_slice(&levels);
    } else if column.cells.contains(&Cell::Null) {
        return Err(ParquetError(format!("column {} is not nullable but holds a null", column.name)));
    }
    value_bytes(column, &mut data)?;
    Ok(data)
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
pub fn write(columns: &[Column]) -> Result<Vec<u8>, ParquetError> {
    let rows = columns.first().map_or(0, |column| column.cells.len());
    if columns.iter().any(|column| column.cells.len() != rows) {
        return Err(ParquetError("every column must hold the same number of values".to_owned()));
    }
    let mut file = MAGIC.to_vec();
    let mut chunks = Vec::with_capacity(columns.len());
    for column in columns {
        let data = page(column)?;
        let size = i64::try_from(data.len()).map_err(|_| ParquetError("a column is too large".to_owned()))?;
        let mut header = Writer::default();
        header.begin();
        header.int(1, false, 0);
        header.int(2, false, size);
        header.int(3, false, size);
        header.begin_struct(5);
        header.int(1, false, rows as i64);
        header.int(2, false, PLAIN);
        header.int(3, false, RLE);
        header.int(4, false, RLE);
        header.end();
        header.end();
        let offset = file.len() as i64;
        file.extend_from_slice(&header.out);
        file.extend_from_slice(&data);
        chunks.push((offset, file.len() as i64 - offset));
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
    meta.begin_list(4, 12, 1);
    meta.list_struct();
    meta.begin_list(1, 12, columns.len());
    for (column, (offset, size)) in columns.iter().zip(&chunks) {
        meta.list_struct();
        meta.int(2, true, *offset);
        meta.begin_struct(3);
        meta.int(1, false, physical(column.column_type));
        meta.begin_list(2, 5, 2);
        meta.list_int(PLAIN);
        meta.list_int(RLE);
        meta.begin_list(3, 8, 1);
        meta.list_binary(column.name.as_bytes());
        meta.int(4, false, 0);
        meta.int(5, true, rows as i64);
        meta.int(6, true, *size);
        meta.int(7, true, *size);
        meta.int(9, true, *offset);
        meta.end();
        meta.end();
    }
    let total: i64 = chunks.iter().map(|(_, size)| size).sum();
    meta.int(2, true, total);
    meta.int(3, true, rows as i64);
    meta.end();
    meta.binary(6, b"EggDB");
    meta.end();
    let footer = meta.out;
    file.extend_from_slice(&footer);
    file.extend_from_slice(&u32::try_from(footer.len()).map_err(|_| ParquetError("the footer is too large".to_owned()))?.to_le_bytes());
    file.extend_from_slice(MAGIC);
    Ok(file)
}
