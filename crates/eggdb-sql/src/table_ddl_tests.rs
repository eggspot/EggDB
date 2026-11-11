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
    let itself = fails(&mut session, "ALTER TABLE t MODIFY a BIGINT AFTER a");
    assert_eq!(itself.vendor_code(), Some(1054));
    assert_eq!(itself.message(), "Unknown column 'a' in 't'");
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
fn mysql_show_create_table_shows_on_update_current_timestamp() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    last(
        &mut session,
        "CREATE TABLE stamped (id INT PRIMARY KEY, changed TIMESTAMP NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP, plain TIMESTAMP NULL)",
    );
    let create = last(&mut session, "SHOW CREATE TABLE stamped");
    let changed = create.lines().find(|line| line.contains("`changed`")).unwrap_or_default();
    assert!(changed.ends_with(" ON UPDATE CURRENT_TIMESTAMP,"), "{create}");
    let plain = create.lines().find(|line| line.contains("`plain`")).unwrap_or_default();
    assert!(!plain.contains("ON UPDATE"), "{create}");
}

#[test]
fn mysql_information_schema_lists_triggers() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    last(&mut session, "CREATE TABLE t (id INT PRIMARY KEY, n INT)");
    last(
        &mut session,
        "CREATE TRIGGER t_bi BEFORE INSERT ON t FOR EACH ROW SET NEW.n = NEW.n + 1",
    );
    let shown = last(
        &mut session,
        "SELECT TRIGGER_NAME, EVENT_MANIPULATION, ACTION_TIMING, ACTION_ORDER, ACTION_STATEMENT FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_TABLE = 't'",
    );
    for expected in ["t_bi", "INSERT", "BEFORE", "1", "SET NEW.n = NEW.n + 1"] {
        assert!(shown.contains(expected), "{expected} in {shown}");
    }
}

#[test]
fn mysql_trigger_bodies_run_case_loops_labels_leave_and_iterate() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    last(&mut session, "CREATE TABLE t (kind VARCHAR(1) PRIMARY KEY, n INT)");
    last(
        &mut session,
        "CREATE TRIGGER t_bi BEFORE INSERT ON t FOR EACH ROW \
         BEGIN \
           DECLARE i INT DEFAULT 0; \
           CASE NEW.kind WHEN 'a' THEN SET NEW.n = 1; WHEN 'b' THEN SET NEW.n = 2; ELSE SET NEW.n = 0; END CASE; \
           counting: LOOP \
             SET i = i + 1; \
             IF i >= 3 THEN LEAVE counting; END IF; \
             ITERATE counting; \
           END LOOP counting; \
           REPEAT SET i = i + 1; UNTIL i >= 5 END REPEAT; \
           SET NEW.n = NEW.n * 10 + i; \
         END",
    );
    last(&mut session, "INSERT INTO t (kind) VALUES ('a'), ('b'), ('z')");
    let rows = last(&mut session, "SELECT kind, n FROM t ORDER BY kind");
    for expected in ["15", "25", "5"] {
        assert!(rows.contains(expected), "{expected} in {rows}");
    }
}

#[test]
fn mysql_columns_of_unsigned_year_bit_enum_and_set_types_load() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    last(
        &mut session,
        "CREATE TABLE users (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, born YEAR, \
         active BIT(1), flags BIT(8), role ENUM('admin', 'member') NOT NULL DEFAULT 'member', \
         tags SET('a', 'bb', 'ccc') CHARACTER SET utf8mb4)",
    );
    last(
        &mut session,
        "INSERT INTO users (born, active, flags, tags) VALUES (1999, 1, 5, 'a,bb')",
    );
    let rows = last(&mut session, "SELECT id, born, flags, role, tags FROM users");
    for expected in ["1999", "member", "a,bb"] {
        assert!(rows.contains(expected), "{expected} in {rows}");
    }
    let error = fails(&mut session, "CREATE TABLE y (y YEAR(2))");
    assert_eq!(error.vendor_code(), Some(1818), "{error}");
    let error = fails(&mut session, "CREATE TABLE b (b BIT(65))");
    assert_eq!(error.vendor_code(), Some(1439), "{error}");
}

#[test]
fn mysql_trigger_bodies_read_cursors_and_catch_errors_with_handlers() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    last(&mut session, "CREATE TABLE prices (item VARCHAR(10) PRIMARY KEY, price INT)");
    last(&mut session, "INSERT INTO prices VALUES ('a', 3), ('b', 4), ('c', 5)");
    last(&mut session, "CREATE TABLE orders (id INT PRIMARY KEY, total INT, note VARCHAR(20))");
    last(&mut session, "CREATE TABLE seen (id INT PRIMARY KEY)");
    last(
        &mut session,
        "CREATE TRIGGER orders_bi BEFORE INSERT ON orders FOR EACH ROW \
         BEGIN \
           DECLARE done INT DEFAULT 0; \
           DECLARE p INT; \
           DECLARE sum_of INT DEFAULT 0; \
           DECLARE prices_of CURSOR FOR SELECT price FROM prices ORDER BY item; \
           DECLARE CONTINUE HANDLER FOR NOT FOUND SET done = 1; \
           DECLARE CONTINUE HANDLER FOR 1062 SET NEW.note = 'seen before'; \
           OPEN prices_of; \
           reading: LOOP \
             FETCH prices_of INTO p; \
             IF done = 1 THEN LEAVE reading; END IF; \
             SET sum_of = sum_of + p; \
           END LOOP reading; \
           CLOSE prices_of; \
           SET NEW.total = sum_of; \
           INSERT INTO seen VALUES (NEW.id % 2); \
         END",
    );
    last(&mut session, "INSERT INTO orders (id) VALUES (1), (3)");
    let rows = last(&mut session, "SELECT id, total, note FROM orders ORDER BY id");
    assert!(rows.contains("12"), "{rows}");
    assert!(rows.contains("seen before"), "{rows}");
    last(
        &mut session,
        "CREATE TRIGGER orders_bd BEFORE DELETE ON orders FOR EACH ROW \
         BEGIN \
           DECLARE EXIT HANDLER FOR SQLEXCEPTION BEGIN END; \
           INSERT INTO no_such_table VALUES (1); \
           INSERT INTO seen VALUES (100); \
         END",
    );
    last(&mut session, "DELETE FROM orders WHERE id = 1");
    assert!(
        !last(&mut session, "SELECT id FROM seen ORDER BY id").contains("100"),
        "the exit handler leaves the block before the second insert"
    );
    let error = fails(
        &mut session,
        "CREATE TRIGGER orders_ai AFTER INSERT ON orders FOR EACH ROW \
         BEGIN DECLARE UNDO HANDLER FOR SQLEXCEPTION BEGIN END; END",
    );
    assert!(error.message().contains("UNDO"), "{error}");
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
fn sqlite_schema_edits_the_kept_text_after_add_column_and_rename() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    last(
        &mut session,
        "CREATE TABLE pets(id integer primary key, name text, UNIQUE (name))",
    );
    last(&mut session, "create index  by_name on pets(name  desc)");
    last(
        &mut session,
        "ALTER TABLE pets ADD COLUMN age  integer DEFAULT 0",
    );
    assert_eq!(
        sqlite_text(&mut session, "pets"),
        "sql\nCREATE TABLE pets(id integer primary key, name text, age  integer DEFAULT 0, UNIQUE (name))\nSELECT 1"
    );
    last(&mut session, "ALTER TABLE pets RENAME TO animals");
    assert_eq!(
        sqlite_text(&mut session, "animals"),
        "sql\nCREATE TABLE \"animals\"(id integer primary key, name text, age  integer DEFAULT 0, UNIQUE (name))\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT tbl_name, sql FROM sqlite_schema WHERE name = 'by_name'"
        ),
        "tbl_name|sql\nanimals|CREATE INDEX by_name on \"animals\"(name  desc)\nSELECT 1"
    );
    last(
        &mut session,
        "ALTER TABLE animals RENAME COLUMN age TO years",
    );
    assert_eq!(
        sqlite_text(&mut session, "animals"),
        "sql\nCREATE TABLE \"animals\"(id integer primary key, name text, \"years\"  integer DEFAULT 0, UNIQUE (name))\nSELECT 1"
    );
    last(&mut session, "ALTER TABLE animals RENAME COLUMN name TO label");
    assert_eq!(
        sqlite_text(&mut session, "animals"),
        "sql\nCREATE TABLE \"animals\"(id integer primary key, \"label\" text, \"years\"  integer DEFAULT 0, UNIQUE (\"label\"))\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT sql FROM sqlite_schema WHERE name = 'by_name'"
        ),
        "sql\nCREATE INDEX by_name on \"animals\"(\"label\"  desc)\nSELECT 1"
    );
    last(&mut session, "ALTER TABLE animals DROP COLUMN years");
    assert_eq!(
        sqlite_text(&mut session, "animals"),
        "sql\nCREATE TABLE \"animals\"(id integer primary key, \"label\" text, UNIQUE (\"label\"))\nSELECT 1"
    );
    last(&mut session, "DROP TABLE animals");
    last(&mut session, "CREATE TABLE animals(kind text)");
    assert_eq!(
        sqlite_text(&mut session, "animals"),
        "sql\nCREATE TABLE animals(kind text)\nSELECT 1"
    );
}

#[test]
fn sqlite_schema_keeps_index_view_and_trigger_texts_as_written() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    last(&mut session, "CREATE TABLE t(a int, b text)");
    last(
        &mut session,
        "create unique index if not exists main.t_ab on t(a,b)",
    );
    last(
        &mut session,
        "CREATE VIEW IF NOT EXISTS main.big(x) AS select a from t where a > 10",
    );
    last(
        &mut session,
        "CREATE TRIGGER IF NOT EXISTS t_added AFTER INSERT ON t BEGIN SELECT 1; END",
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT type, name, tbl_name, rootpage, sql FROM sqlite_schema WHERE type <> 'table'"
        ),
        "type|name|tbl_name|rootpage|sql\n\
         index|t_ab|t|3|CREATE UNIQUE INDEX t_ab on t(a,b)\n\
         view|big|big|0|CREATE VIEW big(x) AS select a from t where a > 10\n\
         trigger|t_added|t|0|CREATE TRIGGER t_added AFTER INSERT ON t BEGIN SELECT 1; END\n\
         SELECT 3"
    );
    last(&mut session, "DROP INDEX t_ab");
    last(&mut session, "DROP VIEW big");
    last(&mut session, "CREATE INDEX t_ab ON t (a, b)");
    let mut postgres = Session::new(&store);
    last(
        &mut postgres,
        "CREATE VIEW big AS select a from t where a > 10",
    );
    drop(postgres);
    assert_eq!(
        last(
            &mut session,
            "SELECT type, sql FROM sqlite_schema WHERE name IN ('t_ab', 'big') ORDER BY type"
        ),
        "type|sql\nindex|CREATE INDEX t_ab ON t (a, b)\nview|CREATE VIEW big AS select a from t where a > 10\nSELECT 2"
    );
}

#[test]
fn a_catalog_written_before_the_index_and_view_texts_shows_generated_text_and_alters() {
    use crate::store::{KeyValueRead, KeyValueStore, KeyValueWrite};
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    last(&mut session, "CREATE TABLE t(a int)");
    last(&mut session, "CREATE INDEX t_a ON t(a)");
    last(&mut session, "CREATE VIEW v AS SELECT a FROM t");
    drop(session);
    let mut prefix = crate::encoding::NAMESPACE.to_vec();
    prefix.push(crate::sqlite_schema_text::SQLITE_TEXT_SPACE);
    let mut table_record = prefix.clone();
    table_record.extend_from_slice(b"t");
    let mut keep_table_only = store.begin_write().unwrap();
    let record = keep_table_only
        .get(&table_record)
        .unwrap()
        .expect("the table's text as written");
    crate::store::delete_prefix(&mut keep_table_only, &prefix).unwrap();
    keep_table_only.put(&table_record, &record).unwrap();
    keep_table_only.commit().unwrap();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    let shown = last(
        &mut session,
        "SELECT type, name, sql FROM sqlite_schema ORDER BY type",
    );
    assert!(
        shown.contains("index|t_a|CREATE INDEX t_a ON t (a)")
            && shown.contains("table|t|CREATE TABLE t(a int)")
            && shown.contains("view|v|CREATE VIEW v AS SELECT a FROM t"),
        "{shown}"
    );
    last(&mut session, "DROP VIEW v");
    last(&mut session, "ALTER TABLE t ADD COLUMN b text");
    last(&mut session, "ALTER TABLE t RENAME TO u");
    assert_eq!(
        last(
            &mut session,
            "SELECT sql FROM sqlite_schema WHERE name = 'u'"
        ),
        "sql\nCREATE TABLE \"u\"(a int, b text)\nSELECT 1"
    );
    let index = last(
        &mut session,
        "SELECT sql FROM sqlite_schema WHERE name = 't_a'",
    );
    assert!(index.contains("CREATE INDEX t_a ON u (a)"), "{index}");
    let mut wiped = store.begin_write().unwrap();
    crate::store::delete_prefix(&mut wiped, &prefix).unwrap();
    wiped.commit().unwrap();
    let text = sqlite_text(&mut session, "u");
    assert!(text.starts_with("sql\nCREATE TABLE u ("), "{text}");
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
