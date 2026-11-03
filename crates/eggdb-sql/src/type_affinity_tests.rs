use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Dialect, Session, SqlError};

fn run(session: &mut Session<'_>, sql: &str) -> Result<Vec<String>, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs)
}

fn last(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => outputs.last().cloned().unwrap_or_default(),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn state(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => panic!("{sql} succeeded: {outputs:?}"),
        Err(error) => error.state().code().to_owned(),
    }
}

fn sqlite(store: &MemoryStore) -> Session<'_> {
    Session::with_dialect(store, Dialect::Sqlite)
}

#[test]
fn each_affinity_turns_values_into_its_storage_class_as_sqlite_documents() {
    let store = MemoryStore::default();
    let mut session = sqlite(&store);
    last(
        &mut session,
        "CREATE TABLE kinds (id INTEGER PRIMARY KEY, t TEXT, n NUMERIC, r REAL, u, d DATE, b BOOLEAN)",
    );
    last(
        &mut session,
        "INSERT INTO kinds (t, n, r, u, d, b) VALUES \
         (5, '3.0e+5', '7', '8', '2024-02-29', TRUE), \
         (2.5, 'abc', 'seven', 8, 20240229, FALSE), \
         ('x', '12', 1, 1.0, NULL, NULL)",
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT typeof(t), quote(t), typeof(n), quote(n), typeof(r), quote(r), typeof(u), quote(u) \
             FROM kinds ORDER BY id"
        ),
        "typeof(t)|quote(t)|typeof(n)|quote(n)|typeof(r)|quote(r)|typeof(u)|quote(u)\n\
         text|'5'|integer|300000|real|7.0|text|'8'\n\
         text|'2.5'|text|'abc'|text|'seven'|integer|8\n\
         text|'x'|integer|12|real|1.0|real|1.0\n\
         SELECT 3"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT typeof(d), typeof(b), b FROM kinds WHERE id < 3 ORDER BY id"
        ),
        "typeof(d)|typeof(b)|b\ntext|integer|1\ninteger|integer|0\nSELECT 2"
    );
}

#[test]
fn a_string_compared_with_a_numeric_column_takes_its_affinity() {
    let store = MemoryStore::default();
    let mut session = sqlite(&store);
    last(
        &mut session,
        "CREATE TABLE prices (id INTEGER PRIMARY KEY, amount NUMERIC, label TEXT); \
         INSERT INTO prices (amount, label) VALUES (12, 'twelve'), ('2.50', 'two fifty')",
    );
    assert_eq!(
        last(&mut session, "SELECT id FROM prices WHERE amount = '12'"),
        "id\n1\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT label FROM prices WHERE amount = 2.5"),
        "label\ntwo fifty\nSELECT 1"
    );
}

#[test]
fn keys_on_dynamic_columns_order_null_numbers_text_and_blobs_as_sqlite_does() {
    let store = MemoryStore::default();
    let mut session = sqlite(&store);
    last(
        &mut session,
        "CREATE TABLE keyed (k PRIMARY KEY, v TEXT NOT NULL); \
         INSERT INTO keyed VALUES (3, 'integer'), ('3', 'text'), (x'03', 'blob'), (2.5, 'real'), \
         (-1, 'negative'), (9007199254740993, 'large'), ('', 'empty text')",
    );
    assert_eq!(
        state(&mut session, "INSERT INTO keyed VALUES (3.0, 'same number')"),
        "23505"
    );
    assert_eq!(
        last(&mut session, "SELECT v FROM keyed ORDER BY k"),
        "v\nnegative\nreal\ninteger\nlarge\nempty text\ntext\nblob\nSELECT 7"
    );
    assert_eq!(
        last(&mut session, "SELECT v FROM keyed WHERE k > 2.5 ORDER BY k"),
        "v\ninteger\nlarge\nempty text\ntext\nblob\nSELECT 5"
    );
    assert_eq!(
        last(&mut session, "SELECT v FROM keyed WHERE k = 3.0"),
        "v\ninteger\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT v FROM keyed WHERE k = 9007199254740992.0"),
        "v\nSELECT 0"
    );
    assert_eq!(
        last(&mut session, "SELECT v FROM keyed WHERE k = '3'"),
        "v\ntext\nSELECT 1"
    );
}

#[test]
fn indexes_on_dynamic_columns_find_and_order_rows_and_unique_ones_compare_numbers_by_value() {
    let store = MemoryStore::default();
    let mut session = sqlite(&store);
    last(
        &mut session,
        "CREATE TABLE things (id INTEGER PRIMARY KEY, name TEXT, size NUMERIC, raw BLOB); \
         CREATE INDEX things_name ON things (name); \
         CREATE UNIQUE INDEX things_size ON things (size); \
         CREATE INDEX things_raw ON things (raw); \
         INSERT INTO things (name, size, raw) VALUES ('b', 2, 'two'), ('a', '1.5', 2), (7, 3, x'00'), (NULL, NULL, NULL)",
    );
    assert_eq!(
        state(&mut session, "INSERT INTO things (size) VALUES ('2.0')"),
        "23505"
    );
    assert_eq!(
        last(&mut session, "SELECT id FROM things WHERE name = '7'"),
        "id\n3\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT id FROM things WHERE size >= 1.5 AND size < 3 ORDER BY size"
        ),
        "id\n2\n1\nSELECT 2"
    );
    assert_eq!(
        last(&mut session, "SELECT id FROM things WHERE raw > 1 ORDER BY raw"),
        "id\n2\n1\n3\nSELECT 3"
    );
    assert_eq!(
        last(&mut session, "SELECT id FROM things WHERE raw IS NULL"),
        "id\n4\nSELECT 1"
    );
    last(&mut session, "UPDATE things SET name = 'c' WHERE id = 1");
    assert_eq!(
        last(&mut session, "SELECT id FROM things WHERE name = 'c'"),
        "id\n1\nSELECT 1"
    );
    last(&mut session, "DELETE FROM things WHERE size = 3");
    assert_eq!(
        last(&mut session, "SELECT count(*) FROM things WHERE name = '7'"),
        "count(*)\n0\nSELECT 1"
    );
}

#[test]
fn tables_with_affinities_read_back_from_the_store_with_their_keys() {
    let store = MemoryStore::default();
    {
        let mut session = sqlite(&store);
        last(
            &mut session,
            "CREATE TABLE kept (k TEXT PRIMARY KEY, n NUMERIC, r REAL, b BLOB); \
             CREATE INDEX kept_n ON kept (n); \
             INSERT INTO kept VALUES (1, '10', 2, x'FF'), ('z', 'ten', 'two', 'text')",
        );
    }
    let mut session = sqlite(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT typeof(k), typeof(n), typeof(r), typeof(b) FROM kept ORDER BY k"
        ),
        "typeof(k)|typeof(n)|typeof(r)|typeof(b)\ntext|integer|real|blob\ntext|text|text|text\nSELECT 2"
    );
    assert_eq!(
        last(&mut session, "SELECT k FROM kept WHERE n = 10"),
        "k\n1\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT k FROM kept WHERE k = '1'"),
        "k\n1\nSELECT 1"
    );
    last(&mut session, "INSERT INTO kept (k, n) VALUES (2, 20)");
    assert_eq!(state(&mut session, "INSERT INTO kept (k) VALUES ('2')"), "23505");
    assert_eq!(
        last(&mut session, "PRAGMA table_info(kept)"),
        "cid|name|type|notnull|dflt_value|pk\n0|k|TEXT|0|NULL|1\n1|n|NUMERIC|0|NULL|0\n2|r|REAL|0|NULL|0\n3|b|BLOB|0|NULL|0\nSELECT 4"
    );
}

#[test]
fn casts_keep_their_static_types() {
    let store = MemoryStore::default();
    let mut session = sqlite(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT typeof(CAST(5 AS TEXT)), typeof(CAST('2' AS REAL))"
        ),
        "typeof(CAST(5 AS TEXT))|typeof(CAST('2' AS REAL))\ntext|real\nSELECT 1"
    );
}
