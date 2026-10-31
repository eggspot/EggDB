//! EggDB's writer of the Apache Arrow IPC streaming format (D240), from the Arrow columnar format
//! specification and the FlatBuffers encoding it uses, both written by us: one schema message,
//! one record batch holding every row, and the end-of-stream marker, so pyarrow, Polars, DuckDB
//! and other Arrow readers load an EggDB table directly. Columns are the same
//! [`eggdb_parquet::Column`] values the Parquet writer takes. Apache Arrow is a trademark of the
//! Apache Software Foundation; EggDB is compatible with its format.

mod flatbuffer;

use eggdb_parquet::{Cell, Column, ColumnType};
use flatbuffer::{Builder, Field, Offset};

const METADATA_VERSION_V5: i16 = 4;
const HEADER_SCHEMA: u8 = 1;
const HEADER_RECORD_BATCH: u8 = 3;
const TYPE_INT: u8 = 2;
const TYPE_FLOATING_POINT: u8 = 3;
const TYPE_BINARY: u8 = 4;
const TYPE_UTF8: u8 = 5;
const TYPE_BOOL: u8 = 6;
const TYPE_DECIMAL: u8 = 7;
const TYPE_DATE: u8 = 8;
const TYPE_TIMESTAMP: u8 = 10;
const CONTINUATION: [u8; 4] = [0xff; 4];

fn type_of(builder: &mut Builder, column_type: ColumnType) -> (u8, Offset) {
    match column_type {
        ColumnType::Boolean => (TYPE_BOOL, builder.table(&[])),
        ColumnType::Int32 => (TYPE_INT, builder.table(&[Some(Field::I32(32)), Some(Field::Bool(true))])),
        ColumnType::Int64 => (TYPE_INT, builder.table(&[Some(Field::I32(64)), Some(Field::Bool(true))])),
        ColumnType::Float => (TYPE_FLOATING_POINT, builder.table(&[Some(Field::I16(1))])),
        ColumnType::Double => (TYPE_FLOATING_POINT, builder.table(&[Some(Field::I16(2))])),
        ColumnType::Text => (TYPE_UTF8, builder.table(&[])),
        ColumnType::Binary => (TYPE_BINARY, builder.table(&[])),
        ColumnType::Date => (TYPE_DATE, builder.table(&[Some(Field::I16(0))])),
        ColumnType::Timestamp { utc } => {
            let zone = utc.then(|| builder.string("UTC"));
            (TYPE_TIMESTAMP, builder.table(&[Some(Field::I16(2)), zone.map(Field::Offset)]))
        }
        ColumnType::Decimal { precision, scale } => (
            TYPE_DECIMAL,
            builder.table(&[Some(Field::I32(i32::try_from(precision).unwrap_or(38))), Some(Field::I32(i32::try_from(scale).unwrap_or(0))), Some(Field::I32(128))]),
        ),
    }
}

fn message(builder: Builder, header_type: u8, header: Offset, body_length: i64) -> Vec<u8> {
    let mut builder = builder;
    let root = builder.table(&[Some(Field::I16(METADATA_VERSION_V5)), Some(Field::U8(header_type)), Some(Field::Offset(header)), Some(Field::I64(body_length))]);
    builder.finish(root)
}

fn frame(metadata: &[u8], body: &[u8], out: &mut Vec<u8>) {
    let padded = metadata.len().div_ceil(8) * 8;
    out.extend_from_slice(&CONTINUATION);
    out.extend_from_slice(&u32::try_from(padded).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(metadata);
    out.resize(out.len() + padded - metadata.len(), 0);
    out.extend_from_slice(body);
}

fn schema_message(columns: &[Column]) -> Vec<u8> {
    let mut builder = Builder::new();
    let mut fields = Vec::with_capacity(columns.len());
    for column in columns {
        let name = builder.string(&column.name);
        let (type_id, type_table) = type_of(&mut builder, column.column_type);
        let children = builder.offsets(&[]);
        fields.push(builder.table(&[
            Some(Field::Offset(name)),
            Some(Field::Bool(column.nullable)),
            Some(Field::U8(type_id)),
            Some(Field::Offset(type_table)),
            None,
            Some(Field::Offset(children)),
        ]));
    }
    let fields = builder.offsets(&fields);
    let schema = builder.table(&[Some(Field::I16(0)), Some(Field::Offset(fields))]);
    message(builder, HEADER_SCHEMA, schema, 0)
}

struct Body {
    bytes: Vec<u8>,
    buffers: Vec<[i64; 2]>,
}

impl Body {
    fn push(&mut self, data: &[u8]) {
        let offset = self.bytes.len();
        self.bytes.extend_from_slice(data);
        self.bytes.resize(self.bytes.len().div_ceil(8) * 8, 0);
        self.buffers.push([offset as i64, data.len() as i64]);
    }
}

fn bitmap(bits: impl Iterator<Item = bool>, length: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; length.div_ceil(8)];
    for (index, bit) in bits.enumerate() {
        if bit {
            bytes[index / 8] |= 1 << (index % 8);
        }
    }
    bytes
}

fn column_buffers(column: &Column, body: &mut Body) -> Result<i64, String> {
    let length = column.cells.len();
    let nulls = column.cells.iter().filter(|cell| matches!(cell, Cell::Null)).count();
    if nulls == 0 {
        body.push(&[]);
    } else {
        body.push(&bitmap(column.cells.iter().map(|cell| !matches!(cell, Cell::Null)), length));
    }
    let wrong = |cell: &Cell| format!("column {} holds {cell:?}, which is not its type", column.name);
    match column.column_type {
        ColumnType::Boolean => {
            let mut values = Vec::with_capacity(length);
            for cell in &column.cells {
                values.push(match cell {
                    Cell::Boolean(flag) => *flag,
                    Cell::Null => false,
                    other => return Err(wrong(other)),
                });
            }
            body.push(&bitmap(values.into_iter(), length));
        }
        ColumnType::Int32 | ColumnType::Date => {
            let mut data = Vec::with_capacity(length * 4);
            for cell in &column.cells {
                let value = match cell {
                    Cell::Int(value) => i32::try_from(*value).map_err(|_| wrong(cell))?,
                    Cell::Null => 0,
                    other => return Err(wrong(other)),
                };
                data.extend_from_slice(&value.to_le_bytes());
            }
            body.push(&data);
        }
        ColumnType::Int64 | ColumnType::Timestamp { .. } => {
            let mut data = Vec::with_capacity(length * 8);
            for cell in &column.cells {
                let value = match cell {
                    Cell::Int(value) => *value,
                    Cell::Null => 0,
                    other => return Err(wrong(other)),
                };
                data.extend_from_slice(&value.to_le_bytes());
            }
            body.push(&data);
        }
        ColumnType::Float | ColumnType::Double => {
            let single = column.column_type == ColumnType::Float;
            let mut data = Vec::with_capacity(length * 8);
            for cell in &column.cells {
                let value = match cell {
                    Cell::Float(value) => *value,
                    Cell::Null => 0.0,
                    other => return Err(wrong(other)),
                };
                if single {
                    data.extend_from_slice(&(value as f32).to_le_bytes());
                } else {
                    data.extend_from_slice(&value.to_le_bytes());
                }
            }
            body.push(&data);
        }
        ColumnType::Decimal { .. } => {
            let mut data = Vec::with_capacity(length * 16);
            for cell in &column.cells {
                let value = match cell {
                    Cell::Decimal(value) => *value,
                    Cell::Null => 0,
                    other => return Err(wrong(other)),
                };
                data.extend_from_slice(&value.to_le_bytes());
            }
            body.push(&data);
        }
        ColumnType::Text | ColumnType::Binary => {
            let mut offsets = Vec::with_capacity((length + 1) * 4);
            let mut data = Vec::new();
            offsets.extend_from_slice(&0i32.to_le_bytes());
            for cell in &column.cells {
                match cell {
                    Cell::Bytes(bytes) => data.extend_from_slice(bytes),
                    Cell::Null => {}
                    other => return Err(wrong(other)),
                }
                let end = i32::try_from(data.len()).map_err(|_| format!("column {} holds more than 2 GiB of text", column.name))?;
                offsets.extend_from_slice(&end.to_le_bytes());
            }
            body.push(&offsets);
            body.push(&data);
        }
    }
    Ok(i64::try_from(nulls).unwrap_or(i64::MAX))
}

/// `columns`, all of the same length, as an Arrow IPC stream: a schema message, one record
/// batch and the end-of-stream marker.
pub fn write_stream(columns: &[Column]) -> Result<Vec<u8>, String> {
    let rows = columns.first().map_or(0, |column| column.cells.len());
    if columns.iter().any(|column| column.cells.len() != rows) {
        return Err("the columns differ in length".to_owned());
    }
    let mut out = Vec::new();
    frame(&schema_message(columns), &[], &mut out);
    let mut body = Body { bytes: Vec::new(), buffers: Vec::new() };
    let mut nodes = Vec::with_capacity(columns.len());
    for column in columns {
        let nulls = column_buffers(column, &mut body)?;
        nodes.push([i64::try_from(rows).unwrap_or(i64::MAX), nulls]);
    }
    let mut builder = Builder::new();
    let buffers = builder.structs(&body.buffers);
    let node_vector = builder.structs(&nodes);
    let batch = builder.table(&[Some(Field::I64(i64::try_from(rows).unwrap_or(i64::MAX))), Some(Field::Offset(node_vector)), Some(Field::Offset(buffers))]);
    let metadata = message(builder, HEADER_RECORD_BATCH, batch, i64::try_from(body.bytes.len()).unwrap_or(i64::MAX));
    frame(&metadata, &body.bytes, &mut out);
    out.extend_from_slice(&CONTINUATION);
    out.extend_from_slice(&[0, 0, 0, 0]);
    Ok(out)
}

#[cfg(test)]
mod tests;
