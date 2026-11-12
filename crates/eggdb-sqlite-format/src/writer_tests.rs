use crate::{
    KeyOrder, SqliteFile, SqliteObject, SqliteRow, SqliteValue, TextEncoding, encode_record, record,
    varint,
    write_database,
};

const PAGE: usize = 4096;

fn read_back(bytes: Vec<u8>) -> SqliteFile {
    SqliteFile::from_bytes(bytes, &[]).expect("the written file reads back")
}

fn u16_at(bytes: &[u8], at: usize) -> usize {
    usize::from(u16::from_be_bytes([bytes[at], bytes[at + 1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn walk_index(bytes: &[u8], page: u32, out: &mut Vec<Vec<SqliteValue>>) {
    let base = (page as usize - 1) * PAGE;
    let start = base + if page == 1 { 100 } else { 0 };
    let kind = bytes[start];
    let count = u16_at(bytes, start + 3);
    let header = if kind == 2 { 12 } else { 8 };
    for cell in 0..count {
        let mut at = base + u16_at(bytes, start + header + 2 * cell);
        if kind == 2 {
            walk_index(bytes, u32_at(bytes, at), out);
            at += 4;
        }
        let payload = usize::try_from(varint(bytes, &mut at).expect("a payload size")).unwrap();
        out.push(record(&bytes[at..at + payload], TextEncoding::Utf8).expect("an index record"));
    }
    if kind == 2 {
        walk_index(bytes, u32_at(bytes, start + 8), out);
    }
}

fn index_root(bytes: &[u8], name: &str) -> u32 {
    let file = read_back(bytes.to_vec());
    file.schema()
        .expect("a schema")
        .into_iter()
        .find(|entry| entry.name == name)
        .expect("the index is listed")
        .root_page
}

#[test]
fn records_use_the_smallest_serial_types() {
    assert_eq!(
        encode_record(&[
            SqliteValue::Integer(0),
            SqliteValue::Integer(1),
            SqliteValue::Integer(-1),
            SqliteValue::Null,
            SqliteValue::Text("ab".to_owned()),
        ]),
        vec![6, 8, 9, 1, 0, 17, 0xff, b'a', b'b']
    );
    let wide = encode_record(&[SqliteValue::Integer(i64::MIN), SqliteValue::Real(1.5)]);
    assert_eq!(&wide[..3], &[3, 6, 7]);
    assert_eq!(record(&wide, TextEncoding::Utf8).unwrap(), vec![
        SqliteValue::Integer(i64::MIN),
        SqliteValue::Real(1.5)
    ]);
}

#[test]
fn a_written_file_reads_back_with_its_schema_and_rows() {
    let mut rows: Vec<SqliteRow> = (0..3000i64)
        .map(|at| SqliteRow {
            rowid: at * 7 - 100,
            values: vec![
                SqliteValue::Text(format!("row {at}")),
                SqliteValue::Integer(at * 1_000_000_007),
                SqliteValue::Real(at as f64 / 3.0),
                SqliteValue::Null,
            ],
        })
        .collect();
    rows.push(SqliteRow {
        rowid: 1 << 40,
        values: vec![
            SqliteValue::Blob((0..20_000u32).map(|at| (at % 251) as u8).collect()),
            SqliteValue::Text("x".repeat(5000)),
            SqliteValue::Integer(i64::MAX),
            SqliteValue::Null,
        ],
    });
    rows.reverse();
    let objects = vec![
        SqliteObject::Table {
            name: "t".to_owned(),
            sql: "CREATE TABLE t(a, b, c, d)".to_owned(),
            rows: rows.clone(),
        },
        SqliteObject::Table {
            name: "empty".to_owned(),
            sql: "CREATE TABLE empty(a)".to_owned(),
            rows: Vec::new(),
        },
        SqliteObject::View {
            name: "v".to_owned(),
            sql: "CREATE VIEW v AS SELECT a FROM t".to_owned(),
        },
        SqliteObject::Trigger {
            name: "tr".to_owned(),
            table: "t".to_owned(),
            sql: "CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END".to_owned(),
        },
    ];
    let bytes = write_database(&objects).expect("written");
    assert_eq!(bytes.len() % PAGE, 0);
    assert_eq!(u32_at(&bytes, 28) as usize, bytes.len() / PAGE);
    let file = read_back(bytes);
    let schema = file.schema().expect("a schema");
    let kinds: Vec<(&str, &str, &str)> = schema
        .iter()
        .map(|entry| (entry.kind.as_str(), entry.name.as_str(), entry.table.as_str()))
        .collect();
    assert_eq!(kinds, vec![
        ("table", "t", "t"),
        ("table", "empty", "empty"),
        ("view", "v", "v"),
        ("trigger", "tr", "t")
    ]);
    assert_eq!(schema[2].root_page, 0);
    let mut expected = rows;
    expected.sort_by_key(|row| row.rowid);
    assert_eq!(file.rows(schema[0].root_page).expect("rows"), expected);
    assert!(file.rows(schema[1].root_page).expect("rows").is_empty());
}

#[test]
fn a_long_schema_spills_page_one_into_a_tree() {
    let objects: Vec<SqliteObject> = (0..300)
        .map(|at| SqliteObject::View {
            name: format!("v{at}"),
            sql: format!("CREATE VIEW v{at} AS SELECT '{}'", "y".repeat(200)),
        })
        .collect();
    let file = read_back(write_database(&objects).expect("written"));
    let schema = file.schema().expect("a schema");
    assert_eq!(schema.len(), 300);
    assert_eq!(schema[299].name, "v299");
}

#[test]
fn index_entries_are_sorted_and_each_is_kept_once() {
    let entries: Vec<Vec<SqliteValue>> = (0..4000i64)
        .map(|at| {
            vec![
                SqliteValue::Text(format!("{:0>300}", (at * 7919) % 4000)),
                SqliteValue::Integer(at),
            ]
        })
        .collect();
    let objects = vec![SqliteObject::Index {
        name: "i".to_owned(),
        table: "t".to_owned(),
        sql: Some("CREATE INDEX i ON t(a)".to_owned()),
        entries: entries.clone(),
        orders: Vec::new(),
    }];
    let bytes = write_database(&objects).expect("written");
    let root = index_root(&bytes, "i");
    let mut walked = Vec::new();
    walk_index(&bytes, root, &mut walked);
    let mut expected = entries;
    expected.sort_by(|left, right| {
        crate::compare_values(&left[0], &right[0]).then(crate::compare_values(&left[1], &right[1]))
    });
    assert_eq!(walked.len(), 4000);
    assert_eq!(walked, expected);
}

#[test]
fn descending_keys_and_value_classes_sort_as_sqlite_does() {
    let entries = vec![
        vec![SqliteValue::Text("b".to_owned()), SqliteValue::Integer(1)],
        vec![SqliteValue::Integer(3), SqliteValue::Integer(2)],
        vec![SqliteValue::Null, SqliteValue::Integer(3)],
        vec![SqliteValue::Real(2.5), SqliteValue::Integer(4)],
        vec![SqliteValue::Blob(vec![1]), SqliteValue::Integer(5)],
    ];
    let objects = vec![SqliteObject::Index {
        name: "d".to_owned(),
        table: "t".to_owned(),
        sql: None,
        entries,
        orders: vec![KeyOrder { descending: true }],
    }];
    let bytes = write_database(&objects).expect("written");
    let root = index_root(&bytes, "d");
    let mut walked = Vec::new();
    walk_index(&bytes, root, &mut walked);
    let rowids: Vec<SqliteValue> = walked.into_iter().map(|entry| entry[1].clone()).collect();
    assert_eq!(rowids, vec![
        SqliteValue::Integer(5),
        SqliteValue::Integer(1),
        SqliteValue::Integer(2),
        SqliteValue::Integer(4),
        SqliteValue::Integer(3)
    ]);
}

#[test]
fn index_records_read_the_written_index_in_key_order() {
    let entries: Vec<Vec<SqliteValue>> = (0..3000i64)
        .map(|at| {
            vec![
                SqliteValue::Text(format!("{:0>200}", (at * 7919) % 3000)),
                SqliteValue::Integer(at),
            ]
        })
        .collect();
    let objects = vec![SqliteObject::Index {
        name: "i".to_owned(),
        table: "t".to_owned(),
        sql: Some("CREATE INDEX i ON t(a)".to_owned()),
        entries: entries.clone(),
        orders: Vec::new(),
    }];
    let bytes = write_database(&objects).expect("written");
    let root = index_root(&bytes, "i");
    let file = read_back(bytes);
    let mut expected = entries;
    expected.sort_by(|left, right| {
        crate::compare_values(&left[0], &right[0]).then(crate::compare_values(&left[1], &right[1]))
    });
    assert_eq!(file.index_records(root).expect("records"), expected);
}

#[test]
fn without_rowid_rows_put_the_key_columns_back_in_place() {
    let entries = vec![
        vec![
            SqliteValue::Text("k1".to_owned()),
            SqliteValue::Integer(10),
            SqliteValue::Text("x".to_owned()),
        ],
        vec![
            SqliteValue::Text("k2".to_owned()),
            SqliteValue::Integer(20),
            SqliteValue::Null,
        ],
    ];
    let objects = vec![SqliteObject::Index {
        name: "w".to_owned(),
        table: "w".to_owned(),
        sql: None,
        entries,
        orders: Vec::new(),
    }];
    let bytes = write_database(&objects).expect("written");
    let root = index_root(&bytes, "w");
    let file = read_back(bytes);
    let rows = file.without_rowid_rows(root, 3, &[1]).expect("rows");
    assert_eq!(rows[0].values, vec![
        SqliteValue::Integer(10),
        SqliteValue::Text("k1".to_owned()),
        SqliteValue::Text("x".to_owned())
    ]);
    assert_eq!(rows[1].rowid, 0);
    assert!(file.without_rowid_rows(root, 3, &[3]).is_err());
    assert!(file.without_rowid_rows(root, 3, &[]).is_err());
    assert!(file.without_rowid_rows(root, 3, &[0, 0]).is_err());
}

#[test]
fn without_rowid_is_found_after_the_column_list() {
    assert!(crate::creates_without_rowid(
        "CREATE TABLE w(a TEXT PRIMARY KEY, b) WITHOUT ROWID"
    ));
    assert!(crate::creates_without_rowid(
        "CREATE TABLE w(a TEXT PRIMARY KEY, b) STRICT, without rowid;"
    ));
    assert!(!crate::creates_without_rowid(
        "CREATE TABLE w(a TEXT DEFAULT 'without rowid', b)"
    ));
}
