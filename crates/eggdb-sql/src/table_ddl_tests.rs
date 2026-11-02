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

fn fails(session: &mut Session<'_>, sql: &str) -> SqlError {
    match run(session, sql) {
        Ok(outputs) => panic!("{sql} succeeded: {outputs:?}"),
        Err(error) => error,
    }
}

fn sqlite_text(session: &mut Session<'_>, table: &str) -> String {
    last(
        session,
        &format!("SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = '{table}'"),
    )
}

#[test]
fn mysql_first_and_after_move_columns_with_their_values() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    last(&mut session, "CREATE TABLE t (id INT PRIMARY KEY, b INT)");
    last(&mut session, "CREATE INDEX by_b ON t (b)");
    last(&mut session, "INSERT INTO t VALUES (1, 10), (2, 20)");
    last(
        &mut session,
        "ALTER TABLE t ADD COLUMN a INT DEFAULT 7 FIRST",
    );
    assert_eq!(
        last(&mut session, "SELECT * FROM t ORDER BY id"),
        "a|id|b\n7|1|10\n7|2|20\nSELECT 2"
    );
    last(&mut session, "ALTER TABLE t ADD COLUMN c INT AFTER id");
    assert_eq!(
        last(&mut session, "SELECT * FROM t ORDER BY id"),
        "a|id|c|b\n7|1|NULL|10\n7|2|NULL|20\nSELECT 2"
    );
    last(&mut session, "ALTER TABLE t MODIFY b INT AFTER a");
    assert_eq!(
        last(&mut session, "SELECT * FROM t ORDER BY id"),
        "a|b|id|c\n7|10|1|NULL\n7|20|2|NULL\nSELECT 2"
    );
    last(&mut session, "ALTER TABLE t CHANGE c d INT FIRST");
    assert_eq!(
        last(&mut session, "SELECT * FROM t ORDER BY id"),
        "d|a|b|id\nNULL|7|10|1\nNULL|7|20|2\nSELECT 2"
    );
    last(&mut session, "INSERT INTO t VALUES (4, 5, 30, 3)");
    assert_eq!(
        last(&mut session, "SELECT id, d FROM t WHERE b = 30"),
        "id|d\n3|4\nSELECT 1"
    );
    let duplicate = fails(&mut session, "INSERT INTO t VALUES (1, 1, 1, 3)");
    assert_eq!(duplicate.vendor_code(), Some(1062));
    let missing = fails(&mut session, "ALTER TABLE t ADD COLUMN e INT AFTER nope");
    assert_eq!(missing.vendor_code(), Some(1054));
    assert_eq!(
        last(&mut session, "SELECT * FROM t WHERE id = 1"),
        "d|a|b|id\nNULL|7|10|1\nSELECT 1"
    );
}

#[test]
fn first_and_after_belong_to_mysql_only() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(&mut session, "CREATE TABLE t (id int)");
    fails(&mut session, "ALTER TABLE t ADD COLUMN a int FIRST");
}

#[test]
fn mysql_show_create_table_lists_foreign_keys_and_checks() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    last(&mut session, "CREATE TABLE p (id INT PRIMARY KEY)");
    last(
        &mut session,
        "CREATE TABLE c (id INT PRIMARY KEY, p_id INT, n INT, \
         CONSTRAINT c_p FOREIGN KEY (p_id) REFERENCES p (id) ON DELETE CASCADE, \
         CONSTRAINT positive CHECK (n > 0))",
    );
    let create = last(&mut session, "SHOW CREATE TABLE c");
    assert!(
        create.contains(
            "  CONSTRAINT `c_p` FOREIGN KEY (`p_id`) REFERENCES `p` (`id`) ON DELETE CASCADE,\n  CONSTRAINT `positive` CHECK ("
        ),
        "{create}"
    );
    assert!(create.contains("> 0)"), "{create}");
    let parent = last(&mut session, "SHOW CREATE TABLE p");
    assert!(!parent.contains("CONSTRAINT"), "{parent}");
}

#[test]
fn sqlite_schema_keeps_the_create_table_text_as_written() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    last(
        &mut session,
        "create table if not exists pets(id integer primary key,\n  name text NOT NULL) ",
    );
    let written = "sql\nCREATE TABLE pets(id integer primary key,\n  name text NOT NULL)\nSELECT 1";
    assert_eq!(sqlite_text(&mut session, "pets"), written);
    last(&mut session, "CREATE TABLE IF NOT EXISTS pets(other text)");
    assert_eq!(sqlite_text(&mut session, "pets"), written);
    assert_eq!(
        last(
            &mut session,
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND tbl_name = 'pets'"
        ),
        written
    );
    last(
        &mut session,
        "CREATE TABLE kv (k text PRIMARY KEY, v text) WITHOUT ROWID",
    );
    assert_eq!(
        sqlite_text(&mut session, "kv"),
        "sql\nCREATE TABLE kv (k text PRIMARY KEY, v text) WITHOUT ROWID\nSELECT 1"
    );
}

#[test]
fn sqlite_schema_shows_the_current_definition_after_a_change() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    last(&mut session, "CREATE TABLE pets(id integer primary key)");
    last(&mut session, "ALTER TABLE pets ADD COLUMN age integer");
    let altered = sqlite_text(&mut session, "pets");
    assert!(
        altered.starts_with("sql\nCREATE TABLE pets (") && altered.contains("age"),
        "{altered}"
    );
    last(&mut session, "ALTER TABLE pets RENAME TO animals");
    let renamed = sqlite_text(&mut session, "animals");
    assert!(
        renamed.starts_with("sql\nCREATE TABLE animals ("),
        "{renamed}"
    );
    last(&mut session, "DROP TABLE animals");
    last(&mut session, "CREATE TABLE animals(kind text)");
    assert_eq!(
        sqlite_text(&mut session, "animals"),
        "sql\nCREATE TABLE animals(kind text)\nSELECT 1"
    );
}

#[test]
fn tables_made_in_other_dialects_show_generated_sqlite_text() {
    let store = MemoryStore::default();
    let mut postgres = Session::new(&store);
    last(&mut postgres, "CREATE TABLE made (id int PRIMARY KEY)");
    drop(postgres);
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    let text = sqlite_text(&mut session, "made");
    assert!(text.starts_with("sql\nCREATE TABLE made ("), "{text}");
}
