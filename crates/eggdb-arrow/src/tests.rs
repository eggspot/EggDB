use crate::write_stream;
use eggdb_parquet::{Cell, Column, ColumnType};

fn u32_at(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
}

fn field(bytes: &[u8], table: usize, index: usize) -> Option<usize> {
    let vtable = table - i32::from_le_bytes(bytes[table..table + 4].try_into().unwrap()) as usize;
    let vtable_size = u16::from_le_bytes(bytes[vtable..vtable + 2].try_into().unwrap()) as usize;
    if 4 + 2 * index >= vtable_size {
        return None;
    }
    let offset = u16::from_le_bytes(bytes[vtable + 4 + 2 * index..vtable + 6 + 2 * index].try_into().unwrap()) as usize;
    (offset != 0).then_some(table + offset)
}

fn follow(bytes: &[u8], at: usize) -> usize {
    at + u32_at(bytes, at)
}

fn string(bytes: &[u8], at: usize) -> String {
    let start = follow(bytes, at);
    let length = u32_at(bytes, start);
    String::from_utf8(bytes[start + 4..start + 4 + length].to_vec()).unwrap()
}

fn sample() -> Vec<Column> {
    vec![
        Column { name: "id".to_owned(), column_type: ColumnType::Int64, nullable: false, cells: vec![Cell::Int(1), Cell::Int(2), Cell::Int(3)] },
        Column { name: "name".to_owned(), column_type: ColumnType::Text, nullable: true, cells: vec![Cell::Bytes(b"egg".to_vec()), Cell::Null, Cell::Bytes(b"omelette".to_vec())] },
        Column { name: "fresh".to_owned(), column_type: ColumnType::Boolean, nullable: true, cells: vec![Cell::Boolean(true), Cell::Boolean(false), Cell::Null] },
        Column { name: "price".to_owned(), column_type: ColumnType::Decimal { precision: 9, scale: 2 }, nullable: true, cells: vec![Cell::Decimal(150), Cell::Decimal(-5), Cell::Null] },
        Column { name: "laid".to_owned(), column_type: ColumnType::Timestamp { utc: true }, nullable: true, cells: vec![Cell::Int(0), Cell::Null, Cell::Int(1_000_000)] },
    ]
}

#[test]
fn the_stream_frames_a_schema_a_batch_and_the_end() {
    let stream = write_stream(&sample()).unwrap();
    assert_eq!(&stream[..4], &[0xff; 4]);
    let schema_length = u32_at(&stream, 4);
    assert_eq!(schema_length % 8, 0);
    let metadata = &stream[8..8 + schema_length];
    let root = u32_at(metadata, 0);
    assert_eq!(metadata[field(metadata, root, 1).unwrap()], 1, "the first message is a schema");
    let schema = follow(metadata, field(metadata, root, 2).unwrap());
    let fields = follow(metadata, field(metadata, schema, 1).unwrap());
    assert_eq!(u32_at(metadata, fields), 5);
    let names: Vec<String> = (0..5)
        .map(|index| {
            let entry = fields + 4 + 4 * index;
            let column = follow(metadata, entry);
            string(metadata, field(metadata, column, 0).unwrap())
        })
        .collect();
    assert_eq!(names, ["id", "name", "fresh", "price", "laid"]);
    let batch_start = 8 + schema_length;
    assert_eq!(&stream[batch_start..batch_start + 4], &[0xff; 4]);
    let batch_length = u32_at(&stream, batch_start + 4);
    let batch_metadata = &stream[batch_start + 8..batch_start + 8 + batch_length];
    let batch_root = u32_at(batch_metadata, 0);
    assert_eq!(batch_metadata[field(batch_metadata, batch_root, 1).unwrap()], 3, "the second message is a record batch");
    let body_length = i64::from_le_bytes(batch_metadata[field(batch_metadata, batch_root, 3).unwrap()..][..8].try_into().unwrap()) as usize;
    assert_eq!(stream.len(), batch_start + 8 + batch_length + body_length + 8);
    assert_eq!(&stream[stream.len() - 8..], &[0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0]);
}

#[test]
fn columns_of_different_lengths_or_wrong_cells_are_refused() {
    let mut uneven = sample();
    uneven[0].cells.pop();
    assert!(write_stream(&uneven).is_err());
    let mut wrong = sample();
    wrong[0].cells[0] = Cell::Bytes(b"x".to_vec());
    assert!(write_stream(&wrong).is_err());
    assert!(write_stream(&[]).is_ok());
}
