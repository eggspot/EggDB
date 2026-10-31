use super::dump_sql;
use crate::dialect::Dialect;
use crate::memory_store::MemoryStore;
use crate::restore::{RestoreOptions, restore_dump};
use crate::session::Session;
use crate::tests::render;

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    let mut out = Vec::new();
    session.execute(sql, &mut |result| out.push(render(&result))).unwrap_or_else(|error| panic!("{sql}: {error}"));
    out.join("\n")
}

fn source() -> MemoryStore {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(
        &mut session,
        "CREATE TABLE people (id integer PRIMARY KEY, name text NOT NULL, born date, score numeric(6,2), active boolean, photo bytea, seen timestamp);
         CREATE INDEX people_name ON people (name);
         INSERT INTO people VALUES (1, 'Ada O''Brien', '1815-12-10', 99.50, true, '\\x0102', '2024-01-01 10:00:00'),
                                   (2, 'back\\slash', NULL, NULL, false, NULL, NULL);
         CREATE TABLE empty (x int);",
    );
    let _ = session.close();
    store
}

#[test]
fn a_postgresql_dump_restores_to_equal_rows() {
    let store = source();
    let mut session = Session::new(&store);
    let mut dump = Vec::new();
    let report = dump_sql(&mut session, Dialect::PostgreSql, &mut dump).unwrap();
    assert_eq!((report.tables, report.rows, report.indexes), (2, 2, 1));
    let restored = MemoryStore::default();
    let mut target = Session::new(&restored);
    let mut input = dump.as_slice();
    let outcome = restore_dump(&mut target, &mut input, RestoreOptions::default(), &mut |_| {});
    assert!(outcome.error.is_none(), "{:?}\n{}", outcome.error, String::from_utf8_lossy(&dump));
    let query = "SELECT id, name, born, score, active, photo, seen FROM people ORDER BY id";
    assert_eq!(run(&mut target, query), run(&mut session, query));
}

#[test]
fn other_dialects_write_their_own_syntax() {
    let store = source();
    let mut session = Session::new(&store);
    for (dialect, expected) in [
        (Dialect::MySql, "INSERT INTO `people` (`id`, `name`, `born`, `score`, `active`, `photo`, `seen`) VALUES"),
        (Dialect::SqlServer, "N'Ada O''Brien'"),
        (Dialect::Oracle, "HEXTORAW('0102')"),
        (Dialect::Sqlite, "X'0102'"),
    ] {
        let mut dump = Vec::new();
        dump_sql(&mut session, dialect, &mut dump).unwrap();
        let text = String::from_utf8(dump).unwrap();
        assert!(text.contains(expected), "{dialect:?}:\n{text}");
    }
    let mut dump = Vec::new();
    dump_sql(&mut session, Dialect::MySql, &mut dump).unwrap();
    assert!(String::from_utf8(dump).unwrap().contains("'back\\\\slash'"));
    let mut sqlite = Vec::new();
    dump_sql(&mut session, Dialect::Sqlite, &mut sqlite).unwrap();
    let restored = MemoryStore::default();
    let mut target = Session::with_dialect(&restored, Dialect::Sqlite);
    target.execute(&String::from_utf8(sqlite).unwrap(), &mut |_| {}).unwrap();
    assert_eq!(run(&mut target, "SELECT count(*) FROM people"), run(&mut target, "SELECT 2"));
}
