use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Dialect, Session, SqlError};

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> Result<Vec<String>, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs)
}

fn last(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => outputs.last().cloned().unwrap_or_default(),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn fails(session: &mut Session<'_, MemoryStore>, sql: &str) -> SqlError {
    match run(session, sql) {
        Ok(outputs) => panic!("{sql} succeeded: {outputs:?}"),
        Err(error) => error,
    }
}

struct Seeded(u64);

impl Seeded {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

fn numbered(session: &mut Session<'_, MemoryStore>, rows: u64) {
    last(
        session,
        "CREATE TABLE t (id integer PRIMARY KEY, b integer, c text)",
    );
    let values: Vec<String> = (0..rows)
        .map(|id| format!("({id}, {}, 'c{}')", id % 100, id % 7))
        .collect();
    last(
        session,
        &format!("INSERT INTO t VALUES {}", values.join(", ")),
    );
}

#[test]
fn a_unique_index_refuses_duplicates_and_allows_many_nulls() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE people (id integer PRIMARY KEY, email text UNIQUE, city text)",
    );
    last(
        &mut session,
        "INSERT INTO people VALUES (1, 'a@x', 'oslo'), (2, NULL, 'rome'), (3, NULL, 'oslo')",
    );
    let error = fails(&mut session, "INSERT INTO people VALUES (4, 'a@x', 'nice')");
    assert_eq!(error.state().code(), "23505");
    assert_eq!(
        error.message(),
        "duplicate key value violates unique constraint \"people_email_key\""
    );
    assert_eq!(error.detail(), Some("Key (email)=(a@x) already exists."));
    let within = fails(
        &mut session,
        "INSERT INTO people VALUES (5, 'b@x', 'nice'), (6, 'b@x', 'nice')",
    );
    assert_eq!(within.state().code(), "23505");
    assert_eq!(
        last(&mut session, "SELECT count(*) FROM people"),
        "count\n3\nSELECT 1"
    );
    let update = fails(&mut session, "UPDATE people SET email = 'a@x' WHERE id = 2");
    assert_eq!(update.state().code(), "23505");
    last(
        &mut session,
        "INSERT INTO people VALUES (7, 'c@x', 'oslo'), (8, 'd@x', 'rome')",
    );
    assert_eq!(
        last(
            &mut session,
            "UPDATE people SET email = CASE id WHEN 7 THEN 'd@x' ELSE 'c@x' END WHERE id IN (7, 8)"
        ),
        "UPDATE 2"
    );
    assert_eq!(
        last(&mut session, "SELECT id FROM people WHERE email = 'c@x'"),
        "id\n8\nSELECT 1"
    );
    assert_eq!(session.check_indexes().map(|check| check.entries), Ok(5));
}

#[test]
fn create_unique_index_on_duplicates_fails_and_leaves_no_index() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    numbered(&mut session, 3_000);
    let before = store.snapshot();
    let error = fails(&mut session, "CREATE UNIQUE INDEX t_b ON t (b)");
    assert_eq!(error.message(), "could not create unique index \"t_b\"");
    assert_eq!(error.detail(), Some("Key (b)=(0) is duplicated."));
    assert_eq!(store.snapshot(), before);
    assert_eq!(
        last(&mut session, "CREATE INDEX t_b ON t (b)"),
        "CREATE INDEX"
    );
    assert_eq!(
        last(&mut session, "CREATE UNIQUE INDEX t_id_b ON t (id, b)"),
        "CREATE INDEX"
    );
    let check = session.check_indexes().unwrap();
    assert_eq!((check.tables, check.indexes, check.entries), (1, 2, 6_000));
    assert_eq!(
        fails(&mut session, "CREATE INDEX t_b ON t (c)")
            .state()
            .code(),
        "42P07"
    );
    let skipped = run(&mut session, "CREATE INDEX IF NOT EXISTS t_b ON t (c)").unwrap();
    assert_eq!(skipped, vec!["CREATE INDEX".to_owned()]);
    assert_eq!(last(&mut session, "DROP INDEX t_b"), "DROP INDEX");
    assert_eq!(
        fails(&mut session, "DROP INDEX t_b").state().code(),
        "42704"
    );
    assert_eq!(last(&mut session, "DROP INDEX IF EXISTS t_b"), "DROP INDEX");
    last(&mut session, "DROP TABLE t");
    assert!(store.snapshot().keys().all(|key| key[5] == 0x01));
}

#[test]
fn explain_names_the_index_and_an_indexed_lookup_reads_only_its_range() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    numbered(&mut session, 2_000);
    last(&mut session, "CREATE INDEX t_b ON t (b)");
    assert_eq!(
        last(&mut session, "EXPLAIN SELECT * FROM t WHERE b = 7"),
        "QUERY PLAN\nIndex Scan using t_b on t\nEXPLAIN"
    );
    assert_eq!(
        last(&mut session, "EXPLAIN SELECT * FROM t WHERE c = 'c1'"),
        "QUERY PLAN\nSeq Scan on t\nEXPLAIN"
    );
    assert_eq!(
        last(
            &mut session,
            "EXPLAIN SELECT id FROM t ORDER BY b DESC LIMIT 3"
        ),
        "QUERY PLAN\nLimit\n  ->  Index Scan Backward using t_b on t\nEXPLAIN"
    );
    assert_eq!(
        last(
            &mut session,
            "EXPLAIN SELECT id FROM t WHERE id > 1990 ORDER BY c"
        ),
        "QUERY PLAN\nSort\n  ->  Index Scan using t_pkey on t\nEXPLAIN"
    );
    let before = store.entries_read();
    assert_eq!(
        last(&mut session, "SELECT count(*) FROM t WHERE b = 7"),
        "count\n20\nSELECT 1"
    );
    let read = store.entries_read() - before;
    assert!(read <= 45, "{read} entries read for 20 rows");
    let before = store.entries_read();
    assert_eq!(
        last(
            &mut session,
            "SELECT id FROM t ORDER BY b DESC, id DESC LIMIT 2"
        ),
        "id\n1999\n1899\nSELECT 2"
    );
    let read = store.entries_read() - before;
    assert!(read <= 10, "{read} entries read for LIMIT 2");
    let before = store.entries_read();
    last(&mut session, "SELECT count(*) FROM t WHERE c = 'c1'");
    assert!(store.entries_read() - before >= 2_000);
}

fn random_rows(random: &mut Seeded, rows: u64) -> Vec<String> {
    (0..rows)
        .map(|id| {
            let a = match random.below(8) {
                0 => "NULL".to_owned(),
                _ => (random.below(20) as i64 - 5).to_string(),
            };
            let b = match random.below(6) {
                0 => "NULL".to_owned(),
                _ => format!(
                    "'{}'",
                    ["ant", "bee", "be", "cat", "", "b\u{e9}"][random.below(6) as usize]
                ),
            };
            let c = match random.below(7) {
                0 => "NULL".to_owned(),
                other => format!("{}.5", other as i64 - 3),
            };
            format!("({id}, {a}, {b}, {c})")
        })
        .collect()
}

fn random_condition(random: &mut Seeded) -> String {
    let number = random.below(22) as i64 - 6;
    let text = ["ant", "bee", "be", "cat", "", "b"][random.below(6) as usize];
    match random.below(16) {
        0 => format!("a = {number}"),
        1 => format!("a < {number}"),
        2 => format!("a >= {number}"),
        3 => format!("a BETWEEN {number} AND {}", number + random.below(6) as i64),
        4 => format!("a IN ({number}, {}, NULL, {number})", number + 2),
        5 => "a IS NULL".to_owned(),
        6 => format!("b = '{text}'"),
        7 => format!("b > '{text}'"),
        8 => format!("b LIKE '{text}%'"),
        9 => format!("a = {number} AND b = '{text}'"),
        10 => format!("a = {number} AND b < '{text}'"),
        11 => format!("c > {number}.5"),
        12 => format!("c <= {number}"),
        13 => format!("a > {number} AND a < {}", number + 4),
        14 => format!("id > {} AND a = {number}", random.below(60)),
        _ => format!("a = {number} OR b IS NULL"),
    }
}

#[test]
fn every_query_returns_the_same_rows_with_and_without_indexes() {
    let mut random = Seeded(0x5EED_1DE5_0000_0001);
    for round in 0..6 {
        let plain_store = MemoryStore::default();
        let indexed_store = MemoryStore::default();
        let mut plain = Session::new(&plain_store);
        let mut indexed = Session::new(&indexed_store);
        let create =
            "CREATE TABLE r (id integer PRIMARY KEY, a integer, b text, c double precision)";
        last(&mut plain, create);
        last(&mut indexed, create);
        let rows = random_rows(&mut random, 40 + round * 30);
        let insert = format!("INSERT INTO r VALUES {}", rows.join(", "));
        last(&mut plain, &insert);
        last(&mut indexed, &insert);
        last(&mut indexed, "CREATE INDEX r_a ON r (a)");
        last(&mut indexed, "CREATE INDEX r_b_a ON r (b, a)");
        last(&mut indexed, "CREATE INDEX r_c ON r (c)");
        for (query, index) in [
            ("SELECT * FROM r WHERE a = 1", "r_a"),
            ("SELECT * FROM r WHERE b = 'x' AND a < 3", "r_b_a"),
            ("SELECT * FROM r WHERE c > 1.5", "r_c"),
            ("SELECT * FROM r WHERE b LIKE 'be%'", "r_b_a"),
            ("SELECT * FROM r WHERE a IN (1, 2)", "r_a"),
        ] {
            let plan = last(&mut indexed, &format!("EXPLAIN {query}"));
            assert!(
                plan.contains(&format!("using {index} on r")),
                "{query}: {plan}"
            );
        }
        for _ in 0..150 {
            let condition = random_condition(&mut random);
            let queries = [
                format!("SELECT * FROM r WHERE {condition} ORDER BY id"),
                format!("SELECT id, a FROM r WHERE {condition} ORDER BY a, id"),
                format!("SELECT id FROM r WHERE {condition} ORDER BY a DESC, id DESC LIMIT 5"),
                format!("SELECT id FROM r WHERE {condition} ORDER BY b, a, id LIMIT 7 OFFSET 2"),
                format!("SELECT count(*), sum(a) FROM r WHERE {condition}"),
                format!("SELECT DISTINCT a FROM r WHERE {condition} ORDER BY a"),
            ];
            for query in &queries {
                let expected = run(&mut plain, query).map_err(|error| error.to_string());
                let found = run(&mut indexed, query).map_err(|error| error.to_string());
                assert_eq!(found, expected, "{query}");
            }
            let change = match random.below(4) {
                0 => format!("UPDATE r SET a = a + 1 WHERE {condition}"),
                1 => format!("DELETE FROM r WHERE {condition} AND id % 5 = 0"),
                2 => format!("UPDATE r SET b = 'cat' WHERE {condition}"),
                _ => {
                    let id = 1_000 + random.below(1_000_000);
                    format!(
                        "INSERT INTO r VALUES ({id}, {}, 'bee', 1.5)",
                        random.below(9)
                    )
                }
            };
            let expected = run(&mut plain, &change).map_err(|error| error.to_string());
            let found = run(&mut indexed, &change).map_err(|error| error.to_string());
            assert_eq!(found, expected, "{change}");
        }
        assert!(indexed.check_indexes().is_ok());
    }
}

#[test]
fn joins_look_up_the_inner_table_through_an_index() {
    let plain_store = MemoryStore::default();
    let indexed_store = MemoryStore::default();
    let mut plain = Session::new(&plain_store);
    let mut indexed = Session::new(&indexed_store);
    let setup = "CREATE TABLE customers (id integer PRIMARY KEY, name text);
        CREATE TABLE orders (id integer PRIMARY KEY, customer integer, total integer);
        INSERT INTO customers VALUES (1, 'ann'), (2, 'bob'), (3, 'cy');
        INSERT INTO orders VALUES (10, 1, 5), (11, 2, 7), (12, 1, 9), (13, NULL, 1), (14, 4, 2)";
    last(&mut plain, setup);
    last(&mut indexed, setup);
    last(
        &mut indexed,
        "CREATE INDEX orders_customer ON orders (customer)",
    );
    let queries = [
        "SELECT c.name, o.id FROM customers c JOIN orders o ON o.customer = c.id ORDER BY o.id",
        "SELECT c.name, o.id FROM customers c LEFT JOIN orders o ON o.customer = c.id ORDER BY c.id, o.id",
        "SELECT c.name, o.total FROM customers c JOIN orders o ON o.customer = c.id WHERE c.id = 1 ORDER BY o.total",
        "SELECT c.name, o.total FROM customers c LEFT JOIN orders o ON o.customer = c.id AND o.total > 6 WHERE c.id = 1",
    ];
    for query in queries {
        assert_eq!(run(&mut indexed, query), run(&mut plain, query), "{query}");
    }
    assert_eq!(
        last(
            &mut indexed,
            "EXPLAIN SELECT c.name FROM customers c JOIN orders o ON o.customer = c.id WHERE c.id = 1"
        ),
        "QUERY PLAN\nNested Loop\n  ->  Index Scan using customers_pkey on customers c\n  ->  Index Scan using orders_customer on orders o\nEXPLAIN"
    );
    let before = indexed_store.entries_read();
    last(
        &mut indexed,
        "SELECT o.total FROM customers c JOIN orders o ON o.customer = c.id WHERE c.id = 2",
    );
    assert!(indexed_store.entries_read() - before <= 6);
}

#[test]
fn mysql_unique_keys_drive_ignore_replace_and_on_duplicate_key_update() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    last(
        &mut session,
        "CREATE TABLE t (id INT PRIMARY KEY, code VARCHAR(10), n INT, UNIQUE KEY by_code (code), KEY by_n (n))",
    );
    last(
        &mut session,
        "INSERT INTO t VALUES (1, 'a', 10), (2, 'b', 20)",
    );
    let error = fails(&mut session, "INSERT INTO t VALUES (3, 'a', 30)");
    assert_eq!(error.vendor_code(), Some(1062));
    assert_eq!(error.message(), "Duplicate entry 'a' for key 't.by_code'");
    let primary = fails(&mut session, "INSERT INTO t VALUES (1, 'z', 30)");
    assert_eq!(primary.message(), "Duplicate entry '1' for key 't.PRIMARY'");
    last(&mut session, "INSERT IGNORE INTO t VALUES (3, 'a', 30)");
    assert_eq!(
        last(&mut session, "SHOW WARNINGS"),
        "Level|Code|Message\nWarning|1062|Duplicate entry 'a' for key 't.by_code'\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "REPLACE INTO t VALUES (1, 'b', 99)"),
        "INSERT 0 3"
    );
    assert_eq!(
        last(&mut session, "SELECT id, code, n FROM t ORDER BY id"),
        "id|code|n\n1|b|99\nSELECT 1"
    );
    last(
        &mut session,
        "INSERT INTO t VALUES (5, 'b', 1) ON DUPLICATE KEY UPDATE n = n + 1",
    );
    assert_eq!(
        last(&mut session, "SELECT id, code, n FROM t"),
        "id|code|n\n1|b|100\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SHOW INDEX FROM t")
            .lines()
            .map(|line| line.split('|').take(5).collect::<Vec<_>>().join("|"))
            .collect::<Vec<_>>(),
        vec![
            "Table|Non_unique|Key_name|Seq_in_index|Column_name",
            "t|0|PRIMARY|1|id",
            "t|0|by_code|1|code",
            "t|1|by_n|1|n",
            "SELECT 3",
        ]
    );
    let create = last(&mut session, "SHOW CREATE TABLE t");
    assert!(
        create.contains("  UNIQUE KEY `by_code` (`code`),\n  KEY `by_n` (`n`)"),
        "{create}"
    );
    assert_eq!(
        last(&mut session, "EXPLAIN SELECT * FROM t WHERE n = 100"),
        "id|select_type|table|partitions|type|possible_keys|key|key_len|ref|rows|filtered|Extra\n1|SIMPLE|t|NULL|ref|by_n|by_n|NULL|NULL|NULL|100.00|Using where\nSELECT 1"
    );
    let missing = fails(&mut session, "DROP INDEX nope ON t");
    assert_eq!(missing.vendor_code(), Some(1091));
    let twice = fails(&mut session, "CREATE INDEX by_n ON t (code)");
    assert_eq!(twice.vendor_code(), Some(1061));
    assert_eq!(last(&mut session, "DROP INDEX by_n ON t"), "DROP INDEX");
    last(
        &mut session,
        "CREATE TABLE u (a INT, b INT UNIQUE, KEY (a), INDEX (a, b))",
    );
    assert_eq!(
        last(&mut session, "SHOW INDEX FROM u")
            .lines()
            .map(|line| line.split('|').nth(2).unwrap_or("").to_owned())
            .collect::<Vec<_>>(),
        vec!["Key_name", "b", "a", "a_2", "a_2", ""]
    );
}

#[test]
fn sqlite_indexes_follow_sqlite() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    last(
        &mut session,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT UNIQUE, b INTEGER, UNIQUE (b, a))",
    );
    last(&mut session, "CREATE INDEX IF NOT EXISTS t_b ON t (b)");
    last(&mut session, "CREATE INDEX IF NOT EXISTS t_b ON t (a)");
    last(
        &mut session,
        "INSERT INTO t VALUES (1, 'x', 5), (2, 'y', 5)",
    );
    let error = fails(&mut session, "INSERT INTO t VALUES (3, 'x', 7)");
    assert_eq!(error.to_string(), "Error: UNIQUE constraint failed: t.a");
    assert_eq!(error.vendor_code(), Some(2067));
    let primary = fails(&mut session, "INSERT INTO t VALUES (1, 'q', 7)");
    assert_eq!(primary.vendor_code(), Some(1555));
    last(&mut session, "INSERT OR REPLACE INTO t VALUES (3, 'x', 9)");
    last(
        &mut session,
        "INSERT INTO t VALUES (4, 'y', 1) ON CONFLICT DO NOTHING",
    );
    assert_eq!(
        last(&mut session, "SELECT id, a, b FROM t ORDER BY id"),
        "id|a|b\n2|y|5\n3|x|9\nSELECT 2"
    );
    assert_eq!(
        last(&mut session, "PRAGMA index_list(t)"),
        "seq|name|unique|origin|partial\n0|t_b|0|c|0\n1|sqlite_autoindex_t_2|1|u|0\n2|sqlite_autoindex_t_1|1|u|0\nSELECT 3"
    );
    assert_eq!(
        last(&mut session, "PRAGMA index_info(sqlite_autoindex_t_2)"),
        "seqno|cid|name\n0|2|b\n1|1|a\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT type, name, sql FROM sqlite_schema WHERE type = 'index'"
        ),
        "type|name|sql\nindex|sqlite_autoindex_t_1|NULL\nindex|sqlite_autoindex_t_2|NULL\nindex|t_b|CREATE INDEX t_b ON t (b)\nSELECT 3"
    );
    assert_eq!(
        last(
            &mut session,
            "EXPLAIN QUERY PLAN SELECT * FROM t WHERE b = 5 AND a > 'a'"
        ),
        "id|parent|notused|detail\n2|0|0|SEARCH t USING INDEX sqlite_autoindex_t_2 (b=? AND a>?)\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "EXPLAIN QUERY PLAN SELECT * FROM t WHERE id = 2"
        ),
        "id|parent|notused|detail\n2|0|0|SEARCH t USING INTEGER PRIMARY KEY (rowid=?)\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "EXPLAIN QUERY PLAN SELECT * FROM t ORDER BY a"
        ),
        "id|parent|notused|detail\n2|0|0|SCAN t\n3|0|0|USE TEMP B-TREE FOR ORDER BY\nSELECT 2"
    );
    let missing = fails(&mut session, "DROP INDEX nope");
    assert_eq!(missing.to_string(), "Error: no such index: nope");
    let twice = fails(&mut session, "CREATE INDEX t_b ON t (a)");
    assert_eq!(twice.to_string(), "Error: index t_b already exists");
    assert_eq!(
        fails(&mut session, "EXPLAIN SELECT 1").state().code(),
        "0A000"
    );
}

#[test]
fn oracle_indexes_follow_oracle() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    last(
        &mut session,
        "CREATE TABLE emp (id NUMBER(9) PRIMARY KEY, email VARCHAR2(40) CONSTRAINT emp_email_uk UNIQUE, dept NUMBER(4))",
    );
    last(
        &mut session,
        "CREATE INDEX emp_dept_ix ON emp (dept) TABLESPACE users",
    );
    last(&mut session, "INSERT INTO emp VALUES (1, 'a@x', 10)");
    let error = fails(&mut session, "INSERT INTO emp VALUES (2, 'a@x', 10)");
    assert_eq!(error.vendor_code(), Some(1));
    assert!(
        error.to_string().contains("unique constraint (")
            && error.to_string().contains(".EMP_EMAIL_UK) violated"),
        "{error}"
    );
    last(&mut session, "INSERT INTO emp VALUES (3, 'b@x', 10)");
    let unique = fails(
        &mut session,
        "CREATE UNIQUE INDEX emp_dept_uk ON emp (dept)",
    );
    assert_eq!(unique.vendor_code(), Some(1452));
    assert_eq!(
        last(
            &mut session,
            "SELECT index_name, table_name, uniqueness FROM user_indexes ORDER BY index_name"
        ),
        "INDEX_NAME|TABLE_NAME|UNIQUENESS\nEMP_DEPT_IX|EMP|NONUNIQUE\nEMP_EMAIL_UK|EMP|UNIQUE\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT index_name, column_name, column_position FROM user_ind_columns WHERE index_name = 'EMP_DEPT_IX'"
        ),
        "INDEX_NAME|COLUMN_NAME|COLUMN_POSITION\nEMP_DEPT_IX|DEPT|1\nSELECT 1"
    );
    let missing = fails(&mut session, "DROP INDEX nope");
    assert_eq!(missing.vendor_code(), Some(1418));
    let used = fails(&mut session, "CREATE INDEX emp_dept_ix ON emp (email)");
    assert_eq!(used.vendor_code(), Some(955));
    last(&mut session, "DROP INDEX emp_dept_ix");
    assert_eq!(
        fails(&mut session, "EXPLAIN PLAN FOR SELECT * FROM emp")
            .state()
            .code(),
        "0A000"
    );
}

#[test]
fn a_damaged_index_is_found_by_the_check() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    numbered(&mut session, 50);
    last(&mut session, "CREATE INDEX t_b ON t (b)");
    assert!(session.check_indexes().is_ok());
    let good = store.snapshot();
    let mut missing = good.clone();
    let entry = missing.keys().find(|key| key[5] == 0x04).cloned().unwrap();
    missing.remove(&entry);
    store.restore(missing);
    let error = session.check_indexes().unwrap_err();
    assert_eq!(error.state().code(), "XX001");
    assert!(
        error.message().contains("is missing the entry of a row"),
        "{error}"
    );
    let mut extra = good.clone();
    let mut stray = entry.clone();
    let last_byte = stray.len() - 1;
    stray[last_byte] ^= 0x40;
    extra.insert(stray, Vec::new());
    store.restore(extra);
    assert!(session.check_indexes().is_err());
    store.restore(good);
    assert!(session.check_indexes().is_ok());
}

#[test]
fn hostile_index_statements_return_errors_and_never_panic() {
    let fragments = [
        "CREATE",
        "UNIQUE",
        "INDEX",
        "IF NOT EXISTS",
        "ON",
        "t",
        "i",
        "(",
        ")",
        ",",
        "a",
        "b",
        "DESC",
        "ASC",
        "NULLS FIRST",
        "NULLS LAST",
        "USING btree",
        "USING hash",
        "USING gist",
        "WHERE",
        "DROP INDEX",
        "IF EXISTS",
        "EXPLAIN",
        "SELECT * FROM t",
        "QUERY PLAN",
        "ORDER BY",
        "LIMIT 1",
        "=",
        "IN",
        "BETWEEN",
        "LIKE 'x%'",
        "1",
        "NULL",
        "KEY",
        "PRIMARY KEY",
        "CONSTRAINT",
        ";",
        "CREATE TABLE u (",
        "INSERT INTO t VALUES (1, 2)",
        "UPDATE t SET a = 1",
        "DELETE FROM t",
    ];
    for dialect in Dialect::ALL {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, dialect);
        last(
            &mut session,
            "CREATE TABLE t (a integer PRIMARY KEY, b integer)",
        );
        let mut random = Seeded(0xBAD_5EED ^ dialect as u64);
        for _ in 0..4_000 {
            let length = 1 + random.below(10);
            let mut sql = String::new();
            for _ in 0..length {
                sql.push_str(fragments[random.below(fragments.len() as u64) as usize]);
                sql.push(' ');
            }
            if let Err(error) = run(&mut session, &sql)
                && let Some(position) = error.position()
            {
                assert!(position <= sql.len(), "{sql}: {error}");
            }
            if session.in_block() {
                let _ = run(&mut session, "ROLLBACK");
            }
        }
        assert!(session.check_indexes().is_ok(), "{dialect:?}");
    }
}

fn entry_of(prefix: &[u8], value: i32, id: i32) -> Vec<u8> {
    let mut key = prefix.to_vec();
    crate::encoding::encode_key_value(&mut key, &crate::Value::Integer(value));
    crate::encoding::encode_key_value(&mut key, &crate::Value::Integer(id));
    key
}

#[test]
fn an_index_entry_that_disagrees_with_its_row_is_damage_not_a_wrong_answer() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    numbered(&mut session, 50);
    last(
        &mut session,
        "CREATE INDEX t_b ON t (b); CREATE TABLE u (id integer PRIMARY KEY, t_b integer); INSERT INTO u VALUES (1, 7)",
    );
    let mut damaged = store.snapshot();
    let prefix = damaged
        .keys()
        .find(|key| key[5] == 0x04)
        .map(|key| key[..14].to_vec())
        .unwrap();
    assert!(damaged.remove(&entry_of(&prefix, 8, 8)).is_some());
    damaged.insert(entry_of(&prefix, 7, 8), Vec::new());
    store.restore(damaged);
    for query in [
        "SELECT id FROM t WHERE b = 7",
        "SELECT t.id FROM u JOIN t ON t.b = u.t_b WHERE u.id = 1",
    ] {
        let error = fails(&mut session, query);
        assert_eq!(error.state().code(), "XX001", "{query}: {error}");
    }
    assert_eq!(session.check_indexes().unwrap_err().state().code(), "XX001");
}

#[test]
fn a_catalog_with_a_table_number_twice_or_a_name_that_is_not_utf8_is_damaged() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    numbered(&mut session, 3);
    let good = store.snapshot();
    let mut table_key = crate::encoding::NAMESPACE.to_vec();
    table_key.push(0x02);
    let definition = good
        .get(&[table_key.as_slice(), b"t"].concat())
        .unwrap()
        .clone();
    let mut twice = good.clone();
    twice.insert([table_key.as_slice(), b"t2"].concat(), definition.clone());
    store.restore(twice);
    assert_eq!(session.check_indexes().unwrap_err().state().code(), "XX001");
    assert_eq!(
        fails(&mut session, "CREATE INDEX t_b ON t (b)")
            .state()
            .code(),
        "XX001"
    );
    let mut not_utf8 = good.clone();
    not_utf8.insert([table_key.as_slice(), &[0xFF]].concat(), definition);
    store.restore(not_utf8);
    assert_eq!(session.check_indexes().unwrap_err().state().code(), "XX001");
    store.restore(good);
    assert!(session.check_indexes().is_ok());
}

#[test]
fn postgresql_index_syntax_is_a_syntax_error_in_the_other_dialects() {
    for (dialect, create) in [
        (Dialect::MySql, "CREATE TABLE g (a INT)"),
        (Dialect::Sqlite, "CREATE TABLE g (a INTEGER)"),
        (Dialect::Oracle, "CREATE TABLE g (a NUMBER(9))"),
    ] {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, dialect);
        last(&mut session, create);
        for sql in [
            "CREATE INDEX i ON ONLY g (a)",
            "CREATE INDEX i ON g USING btree (a)",
            "CREATE INDEX i ON g (a NULLS FIRST)",
        ] {
            let error = fails(&mut session, sql);
            let shown = error.to_string();
            let expected = match dialect {
                Dialect::MySql => {
                    shown.contains("You have an error in your SQL syntax")
                        && error.vendor_code() == Some(1064)
                }
                Dialect::Sqlite => {
                    shown.ends_with(": syntax error") && error.vendor_code() == Some(1)
                }
                _ => {
                    shown.contains("SQL command not properly ended")
                        && error.vendor_code() == Some(933)
                }
            };
            assert!(expected, "{dialect:?} {sql}: {shown}");
        }
    }
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    last(&mut session, "CREATE TABLE g (a TEXT)");
    let nocase = fails(&mut session, "CREATE INDEX i ON g (a COLLATE NOCASE)");
    assert_eq!(nocase.state().code(), "0A000");
    let mut mysql = Session::with_dialect(&store, Dialect::MySql);
    assert_eq!(
        fails(&mut mysql, "CREATE INDEX i ON g (a COLLATE utf8mb4_bin)").vendor_code(),
        Some(1064)
    );
}

fn random_typed_rows(random: &mut Seeded, rows: u64) -> Vec<String> {
    (0..rows)
        .map(|id| {
            let n = match random.below(7) {
                0 => "NULL".to_owned(),
                1 => "'NaN'".to_owned(),
                _ => format!(
                    "{}.{}",
                    random.below(9) as i64 - 4,
                    ["0", "5", "50", "25", "05"][random.below(5) as usize]
                ),
            };
            let d = match random.below(6) {
                0 => "NULL".to_owned(),
                _ => format!("'2024-02-{:02}'", 20 + random.below(9)),
            };
            let t = match random.below(6) {
                0 => "NULL".to_owned(),
                _ => format!(
                    "'2024-02-28 {:02}:{:02}:00.{}'",
                    random.below(24),
                    random.below(3) * 20,
                    random.below(3)
                ),
            };
            let z = match random.below(6) {
                0 => "NULL".to_owned(),
                _ => format!("'1999-12-31 23:00:00+{:02}'", random.below(4)),
            };
            format!("({id}, {n}, {d}, {t}, {z})")
        })
        .collect()
}

fn random_typed_condition(random: &mut Seeded) -> String {
    let number = format!("{}.5", random.below(9) as i64 - 4);
    let day = format!("'2024-02-{:02}'", 20 + random.below(9));
    let time = format!("'2024-02-28 {:02}:20:00.1'", random.below(24));
    match random.below(12) {
        0 => format!("n = {number}"),
        1 => format!("n < {number}"),
        2 => format!("n >= {}", random.below(5)),
        3 => "n = 'NaN'".to_owned(),
        4 => format!("d = {day}"),
        5 => format!("d > {day}"),
        6 => format!("d BETWEEN {day} AND '2024-02-27'"),
        7 => format!("t < {time}"),
        8 => format!("t >= {time} AND n IS NOT NULL"),
        9 => "z = '1999-12-31 21:00:00+00'".to_owned(),
        10 => format!("z < '2000-01-01 00:00:00+0{}'", random.below(3)),
        _ => format!("n IN ({number}, 0.50, 1.5)"),
    }
}

#[test]
fn numeric_and_time_keys_return_the_same_rows_with_and_without_indexes() {
    let mut random = Seeded(0x5EED_1DE5_0000_0019);
    for round in 0..4 {
        let plain_store = MemoryStore::default();
        let indexed_store = MemoryStore::default();
        let mut plain = Session::new(&plain_store);
        let mut indexed = Session::new(&indexed_store);
        let create = "CREATE TABLE s (id integer PRIMARY KEY, n numeric(6,2), d date, t timestamp, z timestamptz)";
        last(&mut plain, create);
        last(&mut indexed, create);
        let rows = random_typed_rows(&mut random, 40 + round * 30);
        let insert = format!("INSERT INTO s VALUES {}", rows.join(", "));
        last(&mut plain, &insert);
        last(&mut indexed, &insert);
        last(&mut indexed, "CREATE INDEX s_n ON s (n)");
        last(&mut indexed, "CREATE INDEX s_d_t ON s (d, t)");
        last(&mut indexed, "CREATE INDEX s_t ON s (t)");
        last(&mut indexed, "CREATE INDEX s_z ON s (z)");
        for (query, index) in [
            ("SELECT * FROM s WHERE n = 1.5", "s_n"),
            ("SELECT * FROM s WHERE d = '2024-02-21'", "s_d_t"),
            ("SELECT * FROM s WHERE t > '2024-02-28 10:00'", "s_t"),
            ("SELECT * FROM s WHERE z = '2000-01-01 00:00+00'", "s_z"),
        ] {
            let plan = last(&mut indexed, &format!("EXPLAIN {query}"));
            assert!(
                plan.contains(&format!("using {index} on s")),
                "{query}: {plan}"
            );
        }
        for _ in 0..120 {
            let condition = random_typed_condition(&mut random);
            let queries = [
                format!("SELECT * FROM s WHERE {condition} ORDER BY id"),
                format!("SELECT id, n FROM s WHERE {condition} ORDER BY n, id"),
                format!("SELECT id FROM s WHERE {condition} ORDER BY d DESC, t DESC, id LIMIT 5"),
                format!("SELECT count(*), sum(n), min(t), max(z) FROM s WHERE {condition}"),
                format!("SELECT DISTINCT n FROM s WHERE {condition} ORDER BY n"),
            ];
            for query in &queries {
                let expected = run(&mut plain, query).map_err(|error| error.to_string());
                let found = run(&mut indexed, query).map_err(|error| error.to_string());
                assert_eq!(found, expected, "{query}");
            }
            let change = match random.below(3) {
                0 => format!("UPDATE s SET n = n + 1 WHERE {condition}"),
                1 => format!("DELETE FROM s WHERE {condition} AND id % 4 = 0"),
                _ => {
                    let id = 1_000 + random.below(1_000_000);
                    format!(
                        "INSERT INTO s VALUES ({id}, 1.50, '2024-02-21', '2024-02-28 10:20', '2000-01-01 01:00+02')"
                    )
                }
            };
            let expected = run(&mut plain, &change).map_err(|error| error.to_string());
            let found = run(&mut indexed, &change).map_err(|error| error.to_string());
            assert_eq!(found, expected, "{change}");
        }
        assert!(indexed.check_indexes().is_ok());
    }
}

#[test]
fn a_unique_numeric_index_treats_trailing_zeros_as_the_same_value() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE u (id integer PRIMARY KEY, price numeric UNIQUE, doc json, body jsonb)",
    );
    last(&mut session, "INSERT INTO u VALUES (1, 1.5, '{}', '{}')");
    assert_eq!(
        fails(&mut session, "INSERT INTO u VALUES (2, 1.50, '[]', '[]')")
            .state()
            .code(),
        "23505"
    );
    assert_eq!(
        last(&mut session, "SELECT price FROM u WHERE price = 1.500"),
        "price\n1.5\nSELECT 1"
    );
    assert_eq!(
        fails(&mut session, "CREATE INDEX u_doc ON u (doc)")
            .state()
            .code(),
        "42704"
    );
    assert_eq!(
        last(&mut session, "CREATE INDEX u_body ON u (body)"),
        "CREATE INDEX"
    );
    assert_eq!(
        fails(&mut session, "CREATE TABLE v (doc json PRIMARY KEY)")
            .state()
            .code(),
        "42704"
    );
}

fn random_item_31_rows(random: &mut Seeded, rows: u64) -> Vec<String> {
    const INTERVALS: [&str; 8] = [
        "'1 mon'",
        "'30 days'",
        "'720 hours'",
        "'1 day'",
        "'-1 day 24:00:00'",
        "'1 year -2 mons'",
        "'0'",
        "'25 hours'",
    ];
    const CHARACTERS: [&str; 5] = ["'ab'", "'ab  '", "'a'", "''", "'b c'"];
    (0..rows)
        .map(|id| {
            let pick = |random: &mut Seeded, items: &[&str]| -> String {
                if random.below(7) == 0 {
                    "NULL".to_owned()
                } else {
                    items[random.below(items.len() as u64) as usize].to_owned()
                }
            };
            let i = pick(random, &INTERVALS);
            let c = pick(random, &CHARACTERS);
            let b = match random.below(6) {
                0 => "NULL".to_owned(),
                _ => format!(
                    "'\\x{:02x}{}'",
                    random.below(4),
                    ["", "00", "ff"][random.below(3) as usize]
                ),
            };
            let u = match random.below(6) {
                0 => "NULL".to_owned(),
                _ => format!(
                    "'{:08X}-0000-4000-8000-00000000000{}'",
                    random.below(3),
                    random.below(3)
                ),
            };
            let t = match random.below(6) {
                0 => "NULL".to_owned(),
                _ => format!(
                    "'{:02}:{:02}:00'",
                    random.below(25).min(23),
                    random.below(3) * 20
                ),
            };
            let z = match random.below(6) {
                0 => "NULL".to_owned(),
                _ => format!("'10:00:00+0{}'", random.below(3)),
            };
            format!("({id}, {i}, {c}, {b}, {u}, {t}, {z})")
        })
        .collect()
}

fn random_item_31_condition(random: &mut Seeded) -> String {
    match random.below(14) {
        0 => "i = '1 mon'".to_owned(),
        1 => "i = '30 days'".to_owned(),
        2 => format!("i < '{} days'", random.below(40)),
        3 => "i BETWEEN '0' AND '1 day'".to_owned(),
        4 => "c = 'ab'".to_owned(),
        5 => "c = 'ab   '".to_owned(),
        6 => "c > 'a'".to_owned(),
        7 => format!("b = '\\x{:02x}'", random.below(4)),
        8 => "b > '\\x01'".to_owned(),
        9 => format!("u = '{:08x}-0000-4000-8000-000000000001'", random.below(3)),
        10 => format!("t >= '{:02}:20'", random.below(24)),
        11 => "z = '09:00:00+00'".to_owned(),
        12 => format!("z < '10:00:00+0{}'", random.below(3)),
        _ => "i IN ('1 mon', '1 day', '2 days')".to_owned(),
    }
}

#[test]
fn interval_character_bytea_uuid_and_time_keys_return_the_same_rows_with_and_without_indexes() {
    let mut random = Seeded(0x5EED_1DE5_0000_0031);
    for round in 0..4 {
        let plain_store = MemoryStore::default();
        let indexed_store = MemoryStore::default();
        let mut plain = Session::new(&plain_store);
        let mut indexed = Session::new(&indexed_store);
        let create = "CREATE TABLE w (id integer PRIMARY KEY, i interval, c char(4), b bytea, u uuid, t time, z timetz)";
        last(&mut plain, create);
        last(&mut indexed, create);
        let rows = random_item_31_rows(&mut random, 40 + round * 30);
        let insert = format!("INSERT INTO w VALUES {}", rows.join(", "));
        last(&mut plain, &insert);
        last(&mut indexed, &insert);
        for column in ["i", "c", "b", "u", "t", "z"] {
            last(
                &mut indexed,
                &format!("CREATE INDEX w_{column} ON w ({column})"),
            );
        }
        for (query, index) in [
            ("SELECT * FROM w WHERE i = '1 mon'", "w_i"),
            ("SELECT * FROM w WHERE c = 'ab'", "w_c"),
            ("SELECT * FROM w WHERE b = '\\x01'", "w_b"),
            (
                "SELECT * FROM w WHERE u = '00000001-0000-4000-8000-000000000001'",
                "w_u",
            ),
            ("SELECT * FROM w WHERE t > '10:00'", "w_t"),
            ("SELECT * FROM w WHERE z = '10:00:00+01'", "w_z"),
        ] {
            let plan = last(&mut indexed, &format!("EXPLAIN {query}"));
            assert!(
                plan.contains(&format!("using {index} on w")),
                "{query}: {plan}"
            );
        }
        for _ in 0..120 {
            let condition = random_item_31_condition(&mut random);
            let queries = [
                format!("SELECT * FROM w WHERE {condition} ORDER BY id"),
                format!("SELECT id, i FROM w WHERE {condition} ORDER BY i, id"),
                format!("SELECT id FROM w WHERE {condition} ORDER BY c DESC, b, id LIMIT 5"),
                format!(
                    "SELECT count(*), max(c), min(b), max(u), min(t), max(z) FROM w WHERE {condition}"
                ),
                format!("SELECT count(DISTINCT i), count(DISTINCT c) FROM w WHERE {condition}"),
                format!("SELECT c, count(*) FROM w WHERE {condition} GROUP BY c ORDER BY c"),
            ];
            for query in &queries {
                let expected = run(&mut plain, query).map_err(|error| error.to_string());
                let found = run(&mut indexed, query).map_err(|error| error.to_string());
                assert_eq!(found, expected, "{query}");
            }
            let change = match random.below(3) {
                0 => format!("UPDATE w SET i = i + '1 day' WHERE {condition}"),
                1 => format!("DELETE FROM w WHERE {condition} AND id % 4 = 0"),
                _ => {
                    let id = 1_000 + random.below(1_000_000);
                    format!(
                        "INSERT INTO w VALUES ({id}, '1 mon', 'ab', '\\x01', '00000001-0000-4000-8000-000000000001', '10:20', '11:00+01')"
                    )
                }
            };
            let expected = run(&mut plain, &change).map_err(|error| error.to_string());
            let found = run(&mut indexed, &change).map_err(|error| error.to_string());
            assert_eq!(found, expected, "{change}");
        }
        assert!(indexed.check_indexes().is_ok());
    }
}

#[test]
fn unique_indexes_follow_the_equality_of_intervals_and_characters() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE q (id integer PRIMARY KEY, i interval UNIQUE, c char(3) UNIQUE, u uuid UNIQUE)",
    );
    last(
        &mut session,
        "INSERT INTO q VALUES (1, '1 mon', 'a', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11')",
    );
    for duplicate in [
        "INSERT INTO q VALUES (2, '30 days', 'b', NULL)",
        "INSERT INTO q VALUES (3, NULL, 'a  ', NULL)",
        "INSERT INTO q VALUES (4, NULL, NULL, '{A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11}')",
    ] {
        assert_eq!(
            fails(&mut session, duplicate).state().code(),
            "23505",
            "{duplicate}"
        );
    }
    assert_eq!(
        last(&mut session, "SELECT id, i FROM q WHERE i = '720 hours'"),
        "id|i\n1|1 mon\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT count(*) FROM q a JOIN q b ON a.c = b.c"
        ),
        "count\n1\nSELECT 1"
    );
    last(&mut session, "CREATE TABLE p (k interval PRIMARY KEY)");
    last(&mut session, "INSERT INTO p VALUES ('1 day')");
    assert_eq!(
        fails(&mut session, "INSERT INTO p VALUES ('24 hours')")
            .state()
            .code(),
        "23505"
    );
}

#[test]
fn descending_and_hash_indexes_keep_their_options_and_find_the_same_rows() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    numbered(&mut session, 500);
    last(
        &mut session,
        "CREATE INDEX t_b_desc ON t (b DESC, c NULLS FIRST); \
         CREATE INDEX t_c_hash ON t USING hash (c); \
         CREATE UNIQUE INDEX t_id_c ON t (id DESC, c)",
    );
    for (query, expected) in [
        ("SELECT count(*) FROM t WHERE b = 7", "count\n5\nSELECT 1"),
        ("SELECT count(*) FROM t WHERE c = 'c3'", "count\n71\nSELECT 1"),
        (
            "SELECT id FROM t WHERE b >= 98 ORDER BY b DESC, id LIMIT 3",
            "id\n99\n199\n299\nSELECT 3",
        ),
    ] {
        assert_eq!(last(&mut session, query), expected, "{query}");
    }
    let check = session.check_indexes().unwrap();
    assert_eq!((check.indexes, check.entries), (3, 1_500));
    let mut reopened = Session::new(&store);
    let tables = reopened.tables().unwrap();
    let t = tables.iter().find(|table| table.name == "t").unwrap();
    let shown: Vec<(&str, &str, Vec<(bool, bool)>)> = t
        .indexes
        .iter()
        .map(|index| {
            (
                index.name.as_str(),
                index.method.as_str(),
                index
                    .orders
                    .iter()
                    .map(|order| (order.descending, order.nulls_first))
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        shown,
        [
            ("t_b_desc", "btree", vec![(true, true), (false, true)]),
            ("t_c_hash", "hash", vec![(false, false)]),
            ("t_id_c", "btree", vec![(true, true), (false, false)]),
        ]
    );
    assert_eq!(
        t.index_statements_in(Dialect::PostgreSql),
        "CREATE INDEX t_b_desc ON t (b DESC, c NULLS FIRST);\n\
         CREATE INDEX t_c_hash ON t USING hash (c);\n\
         CREATE UNIQUE INDEX t_id_c ON t (id DESC, c);\n"
    );
    assert_eq!(
        t.index_statements_in(Dialect::MySql),
        "CREATE INDEX `t_b_desc` ON `t` (`b` DESC, `c`);\n\
         CREATE INDEX `t_c_hash` ON `t` (`c`);\n\
         CREATE UNIQUE INDEX `t_id_c` ON `t` (`id` DESC, `c`);\n"
    );
    let copy = MemoryStore::default();
    let mut restored = Session::new(&copy);
    last(
        &mut restored,
        "CREATE TABLE t (id integer PRIMARY KEY, b integer, c text)",
    );
    last(&mut restored, &t.index_statements_in(Dialect::PostgreSql));
    assert_eq!(restored.tables().unwrap()[0].indexes, t.indexes);
    let mut mysql = Session::with_dialect(&store, Dialect::MySql);
    let shown = last(&mut mysql, "SHOW INDEX FROM t");
    let collations: Vec<&str> = shown
        .lines()
        .skip(1)
        .filter(|line| line.contains("|t_b_desc|") || line.contains("|t_id_c|"))
        .map(|line| line.split('|').nth(5).unwrap_or_default())
        .collect();
    assert_eq!(collations, ["D", "A", "D", "A"]);
}
