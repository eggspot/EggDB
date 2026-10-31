use super::*;

const PAGE: usize = 512;

fn put_varint(out: &mut Vec<u8>, value: u64) {
    if value < 0x80 {
        out.push(value as u8);
        return;
    }
    let mut groups = Vec::new();
    let mut rest = value;
    while rest > 0 {
        groups.push((rest & 0x7f) as u8);
        rest >>= 7;
    }
    for (index, group) in groups.iter().rev().enumerate() {
        out.push(if index + 1 < groups.len() { group | 0x80 } else { *group });
    }
}

fn encoded_record(values: &[SqliteValue]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut body = Vec::new();
    for value in values {
        match value {
            SqliteValue::Null => put_varint(&mut types, 0),
            SqliteValue::Integer(number) => {
                put_varint(&mut types, 6);
                body.extend_from_slice(&number.to_be_bytes());
            }
            SqliteValue::Real(number) => {
                put_varint(&mut types, 7);
                body.extend_from_slice(&number.to_be_bytes());
            }
            SqliteValue::Text(text) => {
                put_varint(&mut types, 13 + 2 * text.len() as u64);
                body.extend_from_slice(text.as_bytes());
            }
            SqliteValue::Blob(bytes) => {
                put_varint(&mut types, 12 + 2 * bytes.len() as u64);
                body.extend_from_slice(bytes);
            }
        }
    }
    let mut record = Vec::new();
    put_varint(&mut record, types.len() as u64 + 1);
    record.extend_from_slice(&types);
    record.extend_from_slice(&body);
    record
}

fn leaf_page(start: usize, rows: &[(i64, Vec<SqliteValue>)]) -> Vec<u8> {
    let mut page = vec![0u8; PAGE];
    let mut content_end = PAGE;
    let mut pointers = Vec::new();
    for (rowid, values) in rows {
        let record = encoded_record(values);
        let mut cell = Vec::new();
        put_varint(&mut cell, record.len() as u64);
        put_varint(&mut cell, *rowid as u64);
        cell.extend_from_slice(&record);
        content_end -= cell.len();
        page[content_end..content_end + cell.len()].copy_from_slice(&cell);
        pointers.push(content_end as u16);
    }
    page[start] = TABLE_LEAF;
    page[start + 3..start + 5].copy_from_slice(&(rows.len() as u16).to_be_bytes());
    page[start + 5..start + 7].copy_from_slice(&(content_end as u16).to_be_bytes());
    for (index, pointer) in pointers.iter().enumerate() {
        let at = start + 8 + index * 2;
        page[at..at + 2].copy_from_slice(&pointer.to_be_bytes());
    }
    page
}

fn sample_database() -> Vec<u8> {
    let schema = vec![(
        1,
        vec![
            SqliteValue::Text("table".into()),
            SqliteValue::Text("t".into()),
            SqliteValue::Text("t".into()),
            SqliteValue::Integer(2),
            SqliteValue::Text("CREATE TABLE t(a INTEGER, b TEXT, c REAL, d BLOB)".into()),
        ],
    )];
    let mut first = leaf_page(HEADER_BYTES, &schema);
    first[..16].copy_from_slice(MAGIC);
    first[16..18].copy_from_slice(&(PAGE as u16).to_be_bytes());
    first[18] = 1;
    first[19] = 1;
    first[21] = 64;
    first[22] = 32;
    first[23] = 32;
    first[24..28].copy_from_slice(&1u32.to_be_bytes());
    first[28..32].copy_from_slice(&2u32.to_be_bytes());
    first[56..60].copy_from_slice(&1u32.to_be_bytes());
    first[92..96].copy_from_slice(&1u32.to_be_bytes());
    let rows = vec![
        (
            1,
            vec![
                SqliteValue::Integer(-5),
                SqliteValue::Text("five".into()),
                SqliteValue::Real(1.5),
                SqliteValue::Blob(vec![1, 2, 3]),
            ],
        ),
        (
            7,
            vec![
                SqliteValue::Null,
                SqliteValue::Text(String::new()),
                SqliteValue::Null,
                SqliteValue::Null,
            ],
        ),
    ];
    let second = leaf_page(0, &rows);
    [first, second].concat()
}

#[test]
fn a_hand_built_database_reads_its_schema_and_rows() {
    let file = SqliteFile::from_bytes(sample_database(), &[]).unwrap();
    let schema = file.schema().unwrap();
    assert_eq!(schema.len(), 1);
    assert_eq!(schema[0].kind, "table");
    assert_eq!(schema[0].root_page, 2);
    let rows = file.rows(2).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].rowid, 1);
    assert_eq!(rows[0].values[0], SqliteValue::Integer(-5));
    assert_eq!(rows[0].values[2], SqliteValue::Real(1.5));
    assert_eq!(rows[1].rowid, 7);
    assert_eq!(rows[1].values[1], SqliteValue::Text(String::new()));
}

#[test]
fn varints_and_serial_types_follow_the_format() {
    for value in [0u64, 0x7f, 0x80, 0x3fff, 0x4000, u64::from(u32::MAX), u64::MAX >> 8] {
        let mut bytes = Vec::new();
        put_varint(&mut bytes, value);
        let mut at = 0;
        assert_eq!(varint(&bytes, &mut at).unwrap(), value);
        assert_eq!(at, bytes.len());
    }
    assert_eq!(value_of(3, &[0xff, 0xff, 0xfe]).unwrap(), (SqliteValue::Integer(-2), 3));
    assert_eq!(value_of(8, &[]).unwrap().0, SqliteValue::Integer(0));
    assert!(value_of(10, &[]).is_err());
    assert!(value_of(6, &[0; 4]).is_err());
}

#[test]
fn hostile_files_are_errors_never_panics() {
    assert_eq!(
        SqliteFile::from_bytes(b"not a database".to_vec(), &[]).unwrap_err(),
        SqliteFormatError::NotSqlite
    );
    let sample = sample_database();
    let mut seed = 0x1234_5678_9abc_def0u64;
    for _ in 0..5000 {
        let mut bytes = sample.clone();
        for _ in 0..1 + (seed % 6) as usize {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let at = (seed % bytes.len() as u64) as usize;
            bytes[at] = (seed >> 24) as u8;
        }
        if let Ok(file) = SqliteFile::from_bytes(bytes, &[]) {
            let _ = file.schema();
            let _ = file.rows(2);
            let _ = file.rows(1);
        }
    }
}
