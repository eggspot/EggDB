use crate::{read, write_file, write_stream};
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
    let offset = u16::from_le_bytes(
        bytes[vtable + 4 + 2 * index..vtable + 6 + 2 * index]
            .try_into()
            .unwrap(),
    ) as usize;
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
        Column {
            name: "id".to_owned(),
            column_type: ColumnType::Int64,
            nullable: false,
            cells: vec![Cell::Int(1), Cell::Int(2), Cell::Int(3)],
        },
        Column {
            name: "name".to_owned(),
            column_type: ColumnType::Text,
            nullable: true,
            cells: vec![
                Cell::Bytes(b"egg".to_vec()),
                Cell::Null,
                Cell::Bytes(b"omelette".to_vec()),
            ],
        },
        Column {
            name: "fresh".to_owned(),
            column_type: ColumnType::Boolean,
            nullable: true,
            cells: vec![Cell::Boolean(true), Cell::Boolean(false), Cell::Null],
        },
        Column {
            name: "price".to_owned(),
            column_type: ColumnType::Decimal {
                precision: 9,
                scale: 2,
            },
            nullable: true,
            cells: vec![Cell::Decimal(150), Cell::Decimal(-5), Cell::Null],
        },
        Column {
            name: "laid".to_owned(),
            column_type: ColumnType::Timestamp { utc: true },
            nullable: true,
            cells: vec![Cell::Int(0), Cell::Null, Cell::Int(1_000_000)],
        },
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
    assert_eq!(
        metadata[field(metadata, root, 1).unwrap()],
        1,
        "the first message is a schema"
    );
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
    assert_eq!(
        batch_metadata[field(batch_metadata, batch_root, 1).unwrap()],
        3,
        "the second message is a record batch"
    );
    let body_length = i64::from_le_bytes(
        batch_metadata[field(batch_metadata, batch_root, 3).unwrap()..][..8]
            .try_into()
            .unwrap(),
    ) as usize;
    assert_eq!(
        stream.len(),
        batch_start + 8 + batch_length + body_length + 8
    );
    assert_eq!(
        &stream[stream.len() - 8..],
        &[0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0]
    );
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

#[test]
fn a_written_stream_reads_back_and_so_does_a_file() {
    let columns = sample();
    let stream = write_stream(&columns).expect("the stream is written");
    assert_eq!(read(&stream).expect("the stream reads"), columns);
    let mut file = b"ARROW1\0\0".to_vec();
    file.extend_from_slice(&stream);
    assert_eq!(read(&file).expect("the file reads"), columns);
}

#[test]
fn damaged_streams_are_errors_not_panics() {
    let stream = write_stream(&sample()).expect("the stream is written");
    for cut in [0, 3, 8, 40, stream.len() / 2, stream.len() - 9] {
        let _ = read(&stream[..cut]);
    }
    let mut flipped = stream.clone();
    for at in (0..flipped.len()).step_by(7) {
        flipped[at] ^= 0x5a;
        let _ = read(&flipped);
    }
    assert!(read(b"").is_err());
}

#[test]
fn a_written_file_has_its_magic_footer_and_reads_back() {
    let columns = sample();
    let file = write_file(&columns).expect("the file is written");
    assert!(file.starts_with(b"ARROW1\0\0") && file.ends_with(b"ARROW1"));
    let footer_length =
        i32::from_le_bytes(file[file.len() - 10..file.len() - 6].try_into().unwrap()) as usize;
    assert!(footer_length > 0 && footer_length < file.len());
    assert_eq!(read(&file).expect("the file reads"), columns);
}

const HEADER_DICTIONARY_BATCH: u8 = 2;

fn batch_table(
    builder: &mut crate::flatbuffer::Builder,
    rows: usize,
    nodes: &[[i64; 2]],
    body: &crate::Body,
) -> crate::flatbuffer::Offset {
    use crate::flatbuffer::Field;
    let buffers = builder.structs(&body.buffers);
    let node_vector = builder.structs(nodes);
    builder.table(&[
        Some(Field::I64(rows as i64)),
        Some(Field::Offset(node_vector)),
        Some(Field::Offset(buffers)),
    ])
}

fn dictionary_message(id: i64, values: &[&str], delta: bool) -> (Vec<u8>, Vec<u8>) {
    use crate::flatbuffer::{Builder, Field};
    let column = Column {
        name: "values".to_owned(),
        column_type: ColumnType::Text,
        nullable: false,
        cells: values.iter().map(|value| Cell::Bytes(value.as_bytes().to_vec())).collect(),
    };
    let mut body = crate::Body {
        bytes: Vec::new(),
        buffers: Vec::new(),
    };
    let nulls = crate::column_buffers(&column, &mut body).expect("buffers");
    let mut builder = Builder::new();
    let data = batch_table(&mut builder, values.len(), &[[values.len() as i64, nulls]], &body);
    let header = builder.table(&[
        Some(Field::I64(id)),
        Some(Field::Offset(data)),
        Some(Field::Bool(delta)),
    ]);
    (
        crate::message(builder, HEADER_DICTIONARY_BATCH, header, body.bytes.len() as i64),
        body.bytes,
    )
}

fn dictionary_stream(with_dictionary: bool) -> Vec<u8> {
    use crate::flatbuffer::{Builder, Field};
    let mut builder = Builder::new();
    let name = builder.string("color");
    let type_table = builder.table(&[]);
    let index_type = builder.table(&[Some(Field::I32(8)), Some(Field::Bool(true))]);
    let encoding = builder.table(&[
        Some(Field::I64(7)),
        Some(Field::Offset(index_type)),
        Some(Field::Bool(false)),
    ]);
    let children = builder.offsets(&[]);
    let field = builder.table(&[
        Some(Field::Offset(name)),
        Some(Field::Bool(true)),
        Some(Field::U8(5)),
        Some(Field::Offset(type_table)),
        Some(Field::Offset(encoding)),
        Some(Field::Offset(children)),
    ]);
    let fields = builder.offsets(&[field]);
    let schema = builder.table(&[Some(Field::I16(0)), Some(Field::Offset(fields))]);
    let mut out = Vec::new();
    crate::frame(&crate::message(builder, crate::HEADER_SCHEMA, schema, 0), &[], &mut out);
    if with_dictionary {
        let (metadata, body) = dictionary_message(7, &["red", "green", "blue"], false);
        crate::frame(&metadata, &body, &mut out);
        let (metadata, body) = dictionary_message(7, &["violet"], true);
        crate::frame(&metadata, &body, &mut out);
    }
    let mut body = crate::Body {
        bytes: Vec::new(),
        buffers: Vec::new(),
    };
    body.push(&[0b0000_1011]);
    body.push(&[2, 0, 0, 3]);
    let mut builder = Builder::new();
    let batch = batch_table(&mut builder, 4, &[[4, 1]], &body);
    let metadata = crate::message(builder, crate::HEADER_RECORD_BATCH, batch, body.bytes.len() as i64);
    crate::frame(&metadata, &body.bytes, &mut out);
    out.extend_from_slice(&crate::CONTINUATION);
    out.extend_from_slice(&[0, 0, 0, 0]);
    out
}

#[test]
fn dictionary_encoded_columns_read_through_their_dictionaries_and_deltas() {
    let columns = read(&dictionary_stream(true)).expect("the stream reads");
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0].name, "color");
    assert_eq!(columns[0].column_type, ColumnType::Text);
    assert_eq!(
        columns[0].cells,
        vec![
            Cell::Bytes(b"blue".to_vec()),
            Cell::Bytes(b"red".to_vec()),
            Cell::Null,
            Cell::Bytes(b"violet".to_vec()),
        ]
    );
    assert!(read(&dictionary_stream(false)).is_err());
}

fn zstd_raw_frame(data: &[u8]) -> Vec<u8> {
    let mut frame = 0xFD2F_B528_u32.to_le_bytes().to_vec();
    frame.push(0x20);
    frame.push(u8::try_from(data.len()).unwrap());
    let header = 1 | (u32::try_from(data.len()).unwrap() << 3);
    frame.extend_from_slice(&header.to_le_bytes()[..3]);
    frame.extend_from_slice(data);
    frame
}

fn lz4_stored_frame(data: &[u8]) -> Vec<u8> {
    let mut frame = 0x184D_2204_u32.to_le_bytes().to_vec();
    frame.extend_from_slice(&[0x60, 0x40, 0]);
    frame.extend_from_slice(&(u32::try_from(data.len()).unwrap() | 1 << 31).to_le_bytes());
    frame.extend_from_slice(data);
    frame.extend_from_slice(&0_u32.to_le_bytes());
    frame
}

fn compressed_stream(codec: u8, wrap: impl Fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
    use crate::flatbuffer::{Builder, Field};
    let schema = crate::schema_message(&[Column {
        name: "n".to_owned(),
        column_type: ColumnType::Int32,
        nullable: false,
        cells: Vec::new(),
    }]);
    let mut out = Vec::new();
    crate::frame(&schema, &[], &mut out);
    let values: Vec<u8> = [7_i32, -1, 300].iter().flat_map(|value| value.to_le_bytes()).collect();
    let mut stored = 12_i64.to_le_bytes().to_vec();
    stored.extend_from_slice(&wrap(&values));
    let mut body = crate::Body {
        bytes: Vec::new(),
        buffers: Vec::new(),
    };
    body.push(&[]);
    body.push(&stored);
    let mut builder = Builder::new();
    let buffers = builder.structs(&body.buffers);
    let nodes = builder.structs(&[[3, 0]]);
    let compression = builder.table(&[Some(Field::U8(codec)), Some(Field::U8(0))]);
    let batch = builder.table(&[
        Some(Field::I64(3)),
        Some(Field::Offset(nodes)),
        Some(Field::Offset(buffers)),
        Some(Field::Offset(compression)),
    ]);
    let metadata = crate::message(builder, crate::HEADER_RECORD_BATCH, batch, body.bytes.len() as i64);
    crate::frame(&metadata, &body.bytes, &mut out);
    out.extend_from_slice(&crate::CONTINUATION);
    out.extend_from_slice(&[0, 0, 0, 0]);
    out
}

#[test]
fn compressed_batches_read_through_their_codec() {
    let expected = vec![Cell::Int(7), Cell::Int(-1), Cell::Int(300)];
    let zstd = read(&compressed_stream(1, zstd_raw_frame)).expect("zstd reads");
    assert_eq!(zstd[0].cells, expected);
    let lz4 = read(&compressed_stream(0, lz4_stored_frame)).expect("lz4 reads");
    assert_eq!(lz4[0].cells, expected);
    assert!(read(&compressed_stream(9, zstd_raw_frame)).is_err());
    let broken = read(&compressed_stream(1, |data: &[u8]| data.to_vec()));
    assert!(broken.is_err());
}

fn list_stream() -> Vec<u8> {
    use crate::flatbuffer::{Builder, Field};
    let mut builder = Builder::new();
    let item_name = builder.string("item");
    let int_type = builder.table(&[Some(Field::I32(32)), Some(Field::Bool(true))]);
    let no_children = builder.offsets(&[]);
    let item = builder.table(&[
        Some(Field::Offset(item_name)),
        Some(Field::Bool(true)),
        Some(Field::U8(2)),
        Some(Field::Offset(int_type)),
        None,
        Some(Field::Offset(no_children)),
    ]);
    let name = builder.string("scores");
    let list_type = builder.table(&[]);
    let children = builder.offsets(&[item]);
    let field = builder.table(&[
        Some(Field::Offset(name)),
        Some(Field::Bool(true)),
        Some(Field::U8(12)),
        Some(Field::Offset(list_type)),
        None,
        Some(Field::Offset(children)),
    ]);
    let fields = builder.offsets(&[field]);
    let schema = builder.table(&[Some(Field::I16(0)), Some(Field::Offset(fields))]);
    let mut out = Vec::new();
    crate::frame(&crate::message(builder, crate::HEADER_SCHEMA, schema, 0), &[], &mut out);
    let mut body = crate::Body {
        bytes: Vec::new(),
        buffers: Vec::new(),
    };
    body.push(&[]);
    body.push(&[0, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]);
    body.push(&[]);
    body.push(&[10, 0, 0, 0, 20, 0, 0, 0, 30, 0, 0, 0]);
    let mut builder = Builder::new();
    let batch = batch_table(&mut builder, 2, &[[2, 0], [3, 0]], &body);
    let metadata = crate::message(builder, crate::HEADER_RECORD_BATCH, batch, body.bytes.len() as i64);
    crate::frame(&metadata, &body.bytes, &mut out);
    out.extend_from_slice(&crate::CONTINUATION);
    out.extend_from_slice(&[0, 0, 0, 0]);
    out
}

#[test]
fn list_columns_read_as_json_arrays() {
    let columns = read(&list_stream()).expect("the stream reads");
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0].column_type, ColumnType::Text);
    assert_eq!(
        columns[0].cells,
        vec![Cell::Bytes(b"[10,20]".to_vec()), Cell::Bytes(b"[30]".to_vec())]
    );
    let mut damaged = list_stream();
    let length = damaged.len();
    damaged.truncate(length - 20);
    assert!(read(&damaged).is_err());
}
