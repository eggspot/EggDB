use crate::memory_store::MemoryStore;
use crate::new_files::NewFileWriter;
use crate::{Dialect, Session, SqlState};
use eggdb_sqlite_format::{SqliteFile, SqliteValue};
use std::sync::Mutex;

#[derive(Default)]
struct WrittenFiles(Mutex<Vec<(String, Vec<u8>)>>);

impl NewFileWriter for WrittenFiles {
    fn write_new_file(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let mut files = self.0.lock().map_err(|_| "poisoned".to_owned())?;
        if files.iter().any(|(written, _)| written == path) {
            return Err("output file already exists".to_owned());
        }
        files.push((path.to_owned(), bytes.to_vec()));
        Ok(())
    }
}

impl WrittenFiles {
    fn file(&self, path: &str) -> SqliteFile {
        let files = self.0.lock().expect("not poisoned");
        let (_, bytes) = files
            .iter()
            .find(|(written, _)| written == path)
            .expect("the file was written");
        SqliteFile::from_bytes(bytes.clone(), &[]).expect("an SQLite file")
    }
}

fn run(session: &mut Session<'_>, sql: &str) -> Result<(), crate::SqlError> {
    session.execute(sql, &mut |_| {})
}

#[test]
fn vacuum_into_writes_tables_rows_indexes_and_views() {
    let store = MemoryStore::default();
    let files = WrittenFiles::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    session.set_new_file_writer(&files);
    run(
        &mut session,
        "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT UNIQUE, score REAL);
         CREATE TABLE tags (tag TEXT PRIMARY KEY, note BLOB);
         CREATE INDEX people_score ON people (score DESC);
         CREATE VIEW high AS SELECT name FROM people WHERE score > 2;
         INSERT INTO people VALUES (7, 'ann', 3.5), (3, 'bob', 1.0), (9, NULL, 2.5);
         INSERT INTO tags VALUES ('b', x'01'), ('a', NULL)",
    )
    .expect("setup");
    run(&mut session, "VACUUM INTO 'copy.db'").expect("vacuum into");
    let file = files.file("copy.db");
    let schema = file.schema().expect("a schema");
    let names: Vec<(&str, &str)> = schema
        .iter()
        .map(|entry| (entry.kind.as_str(), entry.name.as_str()))
        .collect();
    assert!(names.contains(&("table", "people")), "{names:?}");
    assert!(names.contains(&("index", "sqlite_autoindex_people_1")), "{names:?}");
    assert!(names.contains(&("index", "sqlite_autoindex_tags_1")), "{names:?}");
    assert!(names.contains(&("index", "people_score")), "{names:?}");
    assert!(names.contains(&("view", "high")), "{names:?}");
    let people = schema
        .iter()
        .find(|entry| entry.name == "people")
        .expect("people");
    let rows = file.rows(people.root_page).expect("rows");
    let rowids: Vec<i64> = rows.iter().map(|row| row.rowid).collect();
    assert_eq!(rowids, vec![3, 7, 9]);
    assert_eq!(rows[1].values, vec![
        SqliteValue::Null,
        SqliteValue::Text("ann".to_owned()),
        SqliteValue::Real(3.5)
    ]);
    let tags = schema
        .iter()
        .find(|entry| entry.name == "tags")
        .expect("tags");
    assert_eq!(file.rows(tags.root_page).expect("rows").len(), 2);
}

#[test]
fn vacuum_into_is_refused_without_a_file_writer_inside_a_block_and_over_a_file() {
    let store = MemoryStore::default();
    let mut served = Session::with_dialect(&store, Dialect::Sqlite);
    let error = run(&mut served, "VACUUM INTO 'x.db'").expect_err("no file writer");
    assert_eq!(error.state(), SqlState::FEATURE_NOT_SUPPORTED, "{error}");
    let files = WrittenFiles::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    session.set_new_file_writer(&files);
    run(&mut session, "BEGIN").expect("begin");
    let error = run(&mut session, "VACUUM INTO 'x.db'").expect_err("inside a block");
    assert_eq!(error.state(), SqlState::ACTIVE_SQL_TRANSACTION, "{error}");
    run(&mut session, "COMMIT").expect("commit");
    run(&mut session, "VACUUM main INTO 'x.db'").expect("first copy");
    let error = run(&mut session, "VACUUM INTO 'x.db'").expect_err("file exists");
    assert_eq!(error.state(), SqlState::IO_ERROR, "{error}");
}

#[test]
fn vacuum_into_writes_without_rowid_tables_in_key_order() {
    let store = MemoryStore::default();
    let files = WrittenFiles::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    session.set_new_file_writer(&files);
    run(
        &mut session,
        "CREATE TABLE w (n INTEGER, k TEXT PRIMARY KEY, v TEXT UNIQUE) WITHOUT ROWID",
    )
    .expect("setup");
    run(
        &mut session,
        "INSERT INTO w VALUES (1, 'b', 'x'), (2, 'a', 'y'), (3, 'c', NULL)",
    )
    .expect("rows");
    run(&mut session, "VACUUM INTO 'w.db'").expect("written");
    let file = files.file("w.db");
    let schema = file.schema().expect("schema");
    let table = schema
        .iter()
        .find(|entry| entry.kind == "table" && entry.name == "w")
        .expect("the table");
    let rows = file
        .without_rowid_rows(table.root_page, 3, &[1])
        .expect("rows");
    let keys: Vec<SqliteValue> = rows.iter().map(|row| row.values[1].clone()).collect();
    assert_eq!(keys, vec![
        SqliteValue::Text("a".to_owned()),
        SqliteValue::Text("b".to_owned()),
        SqliteValue::Text("c".to_owned())
    ]);
    assert_eq!(rows[0].values[0], SqliteValue::Integer(2));
    let unique = schema
        .iter()
        .find(|entry| entry.kind == "index")
        .expect("the UNIQUE index");
    let entries = file.index_records(unique.root_page).expect("entries");
    assert_eq!(entries[0], vec![SqliteValue::Null, SqliteValue::Text("c".to_owned())]);
}
