use crate::encoding::NAMESPACE;
use crate::memory_store::MemoryStore;
use crate::{QueryResult, Session, SqlError};
use std::collections::BTreeMap;

fn render(result: &QueryResult) -> String {
    let mut out = String::new();
    if let Some(columns) = &result.columns {
        let names: Vec<&str> = columns.iter().map(|column| column.name.as_str()).collect();
        out.push_str(&names.join("|"));
        out.push('\n');
        for row in &result.rows {
            let cells: Vec<String> = row
                .iter()
                .map(|value| value.to_text().unwrap_or_else(|| "NULL".into()))
                .collect();
            out.push_str(&cells.join("|"));
            out.push('\n');
        }
    }
    out.push_str(&result.tag);
    out
}

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

fn state(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    fails(session, sql).state().code().to_owned()
}

fn with_people(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = Session::new(store);
    last(
        &mut session,
        "CREATE TABLE people (id integer PRIMARY KEY, name text NOT NULL, age smallint, score double precision, active boolean DEFAULT true);
         INSERT INTO people (id, name, age, score) VALUES
           (3, 'carol', 41, 7.5::float8), (1, 'alice', 30, 9.25::float8), (2, 'bob', NULL, NULL), (4, 'dave', 25, 'NaN');",
    );
    session
}

#[test]
fn select_without_from_follows_postgresql_arithmetic() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT 1 + 2 * 3, 7 / 2, -7 / 2, 7 % 3, -7 % 3"
        ),
        "?column?|?column?|?column?|?column?|?column?\n7|3|-3|1|-1\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT 2 ^ 3 AS p, -2 ^ 2 AS q"),
        "p|q\n8|4\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT 'a' || 'b' || 1, 'x' || NULL"),
        "?column?|?column?\nab1|NULL\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT 1.5::float8 + 1, 0.1::float8 + 0.2::float8"
        ),
        "?column?|?column?\n2.5|0.30000000000000004\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT '42'::integer + 1, CAST('t' AS boolean), 3.7::integer"
        ),
        "?column?|bool|int4\n43|t|4\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT 2147483647::bigint + 1, -2147483648"),
        "?column?|?column?\n2147483648|-2147483648\nSELECT 1"
    );
    assert_eq!(state(&mut session, "SELECT 2147483647 + 1"), "22003");
    assert_eq!(
        fails(&mut session, "SELECT 2147483647 + 1").message(),
        "integer out of range"
    );
    assert_eq!(
        state(&mut session, "SELECT 32767::smallint + 1::smallint"),
        "22003"
    );
    assert_eq!(state(&mut session, "SELECT 1 / 0"), "22012");
    assert_eq!(state(&mut session, "SELECT 1.0::float8 / 0"), "22012");
    assert_eq!(state(&mut session, "SELECT 1e308::float8 * 10"), "22003");
    assert_eq!(state(&mut session, "SELECT 'abc'::integer"), "22P02");
    assert_eq!(state(&mut session, "SELECT 1 + 'a'::text"), "42883");
    assert_eq!(state(&mut session, "SELECT 1.5"), "0A000");
    assert_eq!(state(&mut session, "SELECT 0 ^ -1"), "2201F");
}

#[test]
fn null_follows_three_valued_logic() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT NULL AND false, NULL AND true, NULL OR true, NULL OR false, NOT NULL::boolean"
        ),
        "?column?|?column?|?column?|?column?|?column?\nf|NULL|t|NULL|NULL\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT NULL = NULL, NULL IS NULL, 1 IS NOT NULL, NULL IS DISTINCT FROM NULL, 1 IS DISTINCT FROM NULL"
        ),
        "?column?|?column?|?column?|?column?|?column?\nNULL|t|t|f|t\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT 1 IN (1, NULL), 2 IN (1, NULL), 2 NOT IN (1, 3), NULL::integer BETWEEN 1 AND 2, (NULL::boolean) IS UNKNOWN"
        ),
        "?column?|?column?|?column?|?column?|?column?\nt|NULL|t|NULL|t\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT coalesce(NULL, 2, 3), nullif(1, 1), CASE WHEN NULL THEN 1 ELSE 2 END, CASE 3 WHEN 1 THEN 'a' WHEN 3 THEN 'c' END"
        ),
        "coalesce|nullif|case|case\n2|NULL|2|c\nSELECT 1"
    );
    assert_eq!(state(&mut session, "SELECT 1 AND true"), "42804");
}

#[test]
fn tables_take_rows_and_answer_queries() {
    let store = MemoryStore::default();
    let mut session = with_people(&store);
    assert_eq!(
        last(&mut session, "SELECT * FROM people"),
        "id|name|age|score|active\n1|alice|30|9.25|t\n2|bob|NULL|NULL|t\n3|carol|41|7.5|t\n4|dave|25|NaN|t\nSELECT 4"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT name FROM people WHERE age > 26 ORDER BY age DESC"
        ),
        "name\ncarol\nalice\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT p.name, p.age AS years FROM people p WHERE p.id = 2"
        ),
        "name|years\nbob|NULL\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT name FROM people ORDER BY age"),
        "name\ndave\nalice\ncarol\nbob\nSELECT 4"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT name FROM people ORDER BY age DESC NULLS LAST LIMIT 2 OFFSET 1"
        ),
        "name\nalice\ndave\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT name, score FROM people ORDER BY score DESC"
        ),
        "name|score\nbob|NULL\ndave|NaN\nalice|9.25\ncarol|7.5\nSELECT 4"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT id * 10 AS x FROM people ORDER BY x DESC LIMIT 1"
        ),
        "x\n40\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT name FROM people ORDER BY 1 LIMIT ALL OFFSET 3"
        ),
        "name\ndave\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT name FROM people WHERE name LIKE '_a%' ORDER BY id"
        ),
        "name\ncarol\ndave\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT upper(name), length(name) FROM people WHERE id IN (1, 3) AND age BETWEEN 30 AND 41"
        ),
        "upper|length\nALICE|5\nCAROL|5\nSELECT 2"
    );
    assert_eq!(
        last(&mut session, "SELECT id FROM people WHERE id = '3'"),
        "id\n3\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT id FROM people WHERE score < 8.0"),
        "id\n3\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT id FROM people WHERE id = 3 AND name = 'x'"
        ),
        "id\nSELECT 0"
    );
    assert_eq!(
        last(&mut session, "SELECT FROM people LIMIT 0"),
        "\nSELECT 0"
    );
    assert_eq!(state(&mut session, "SELECT nope FROM people"), "42703");
    assert_eq!(state(&mut session, "SELECT * FROM nope"), "42P01");
    assert_eq!(state(&mut session, "SELECT x.id FROM people"), "42P01");
    assert_eq!(
        state(&mut session, "SELECT * FROM people LIMIT -1"),
        "2201W"
    );
    assert_eq!(
        state(&mut session, "SELECT * FROM people OFFSET -1"),
        "2201X"
    );
    assert_eq!(
        state(&mut session, "SELECT name FROM people ORDER BY 2"),
        "42P10"
    );
    assert_eq!(
        state(&mut session, "SELECT * FROM people WHERE id"),
        "42804"
    );
    assert_eq!(
        state(&mut session, "SELECT * FROM people WHERE name = 1"),
        "42883"
    );
    assert_eq!(state(&mut session, "SELECT *"), "42601");
}

#[test]
fn inserts_check_constraints_and_return_rows() {
    let store = MemoryStore::default();
    let mut session = with_people(&store);
    let duplicate = fails(
        &mut session,
        "INSERT INTO people (id, name) VALUES (1, 'again')",
    );
    assert_eq!(duplicate.state().code(), "23505");
    assert_eq!(
        duplicate.message(),
        "duplicate key value violates unique constraint \"people_pkey\""
    );
    assert_eq!(duplicate.detail(), Some("Key (id)=(1) already exists."));
    let null = fails(&mut session, "INSERT INTO people (id) VALUES (9)");
    assert_eq!(null.state().code(), "23502");
    assert_eq!(
        null.message(),
        "null value in column \"name\" of relation \"people\" violates not-null constraint"
    );
    assert_eq!(
        null.detail(),
        Some("Failing row contains (9, null, null, null, t).")
    );
    assert_eq!(
        state(
            &mut session,
            "INSERT INTO people (id, name) VALUES (NULL, 'x')"
        ),
        "23502"
    );
    assert_eq!(
        state(
            &mut session,
            "INSERT INTO people (id, name, age) VALUES (9, 'x', 40000)"
        ),
        "22003"
    );
    assert_eq!(
        state(
            &mut session,
            "INSERT INTO people (id, name) VALUES ('x', 'x')"
        ),
        "22P02"
    );
    assert_eq!(
        state(
            &mut session,
            "INSERT INTO people (id, name) VALUES (9, true)"
        ),
        "42804"
    );
    assert_eq!(
        state(&mut session, "INSERT INTO people (id, nope) VALUES (9, 1)"),
        "42703"
    );
    assert_eq!(
        state(&mut session, "INSERT INTO people (id, id) VALUES (9, 1)"),
        "42701"
    );
    assert_eq!(
        state(&mut session, "INSERT INTO people (id, name) VALUES (9)"),
        "42601"
    );
    assert_eq!(
        state(&mut session, "INSERT INTO people (id) VALUES (9, 'x')"),
        "42601"
    );
    assert_eq!(
        state(&mut session, "INSERT INTO people VALUES (9, 'x'), (10)"),
        "42601"
    );
    assert_eq!(
        last(
            &mut session,
            "INSERT INTO people VALUES (5, 'eve', DEFAULT, 1, DEFAULT), (6, 'fay', 3, 2, DEFAULT) RETURNING id, name, active"
        ),
        "id|name|active\n5|eve|t\n6|fay|t\nINSERT 0 2"
    );
    assert_eq!(
        last(
            &mut session,
            "INSERT INTO people (id, name, score) VALUES (7, 'gus', 2.5) RETURNING *"
        ),
        "id|name|age|score|active\n7|gus|NULL|2.5|t\nINSERT 0 1"
    );
    assert_eq!(
        state(
            &mut session,
            "INSERT INTO people (id, name, age) VALUES (8, 'x', 2.5)"
        ),
        "0A000"
    );
}

#[test]
fn a_failed_multi_row_insert_leaves_no_row_behind() {
    let store = MemoryStore::default();
    let mut session = with_people(&store);
    let before = store.snapshot();
    assert_eq!(
        state(
            &mut session,
            "INSERT INTO people (id, name) VALUES (20, 'a'), (21, 'b'), (1, 'dup')"
        ),
        "23505"
    );
    assert_eq!(store.snapshot(), before);
}

#[test]
fn updates_and_deletes_change_matching_rows() {
    let store = MemoryStore::default();
    let mut session = with_people(&store);
    assert_eq!(
        last(
            &mut session,
            "UPDATE people SET age = age + 1 WHERE age IS NOT NULL"
        ),
        "UPDATE 3"
    );
    assert_eq!(
        last(&mut session, "SELECT age FROM people ORDER BY id"),
        "age\n31\nNULL\n42\n26\nSELECT 4"
    );
    assert_eq!(
        last(
            &mut session,
            "UPDATE people SET name = upper(name), score = DEFAULT WHERE id = 2 RETURNING name, score"
        ),
        "name|score\nBOB|NULL\nUPDATE 1"
    );
    assert_eq!(
        last(&mut session, "UPDATE people SET id = id + 10"),
        "UPDATE 4"
    );
    assert_eq!(
        last(&mut session, "SELECT id FROM people"),
        "id\n11\n12\n13\n14\nSELECT 4"
    );
    assert_eq!(
        state(&mut session, "UPDATE people SET id = 11 WHERE id = 12"),
        "23505"
    );
    assert_eq!(
        state(&mut session, "UPDATE people SET name = NULL"),
        "23502"
    );
    assert_eq!(state(&mut session, "UPDATE people SET nope = 1"), "42703");
    assert_eq!(
        state(&mut session, "UPDATE people SET age = 1, age = 2"),
        "42601"
    );
    assert_eq!(
        last(
            &mut session,
            "DELETE FROM people WHERE id > 12 RETURNING id"
        ),
        "id\n13\n14\nDELETE 2"
    );
    assert_eq!(
        last(&mut session, "DELETE FROM people WHERE false"),
        "DELETE 0"
    );
    assert_eq!(last(&mut session, "DELETE FROM people"), "DELETE 2");
    assert_eq!(
        last(&mut session, "SELECT * FROM people"),
        "id|name|age|score|active\nSELECT 0"
    );
}

#[test]
fn tables_without_a_primary_key_keep_duplicate_rows_in_insertion_order() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE log (msg varchar(5)); INSERT INTO log VALUES ('a'), ('a'), ('b   ')",
    );
    assert_eq!(
        last(&mut session, "SELECT msg, length(msg) FROM log"),
        "msg|length\na|1\na|1\nb   |4\nSELECT 3"
    );
    assert_eq!(
        state(&mut session, "INSERT INTO log VALUES ('toolong')"),
        "22001"
    );
    assert_eq!(
        last(&mut session, "INSERT INTO log VALUES ('abcde   ')"),
        "INSERT 0 1"
    );
    assert_eq!(
        last(&mut session, "SELECT 'abcdefg'::varchar(3)"),
        "varchar\nabc\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "DELETE FROM log WHERE msg = 'a'"),
        "DELETE 2"
    );
    assert_eq!(
        last(
            &mut session,
            "INSERT INTO log VALUES ('c'); SELECT msg FROM log"
        ),
        "msg\nb   \nabcde\nc\nSELECT 3"
    );
}

#[test]
fn composite_primary_keys_order_rows_by_their_columns() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE t (a text, b bigint, c real, PRIMARY KEY (a, b))",
    );
    last(
        &mut session,
        "INSERT INTO t VALUES ('b', 1, 1.5), ('a', 2, 2), ('a', -5, 3), ('', 0, 4)",
    );
    assert_eq!(
        last(&mut session, "SELECT a, b, c FROM t"),
        "a|b|c\n|0|4\na|-5|3\na|2|2\nb|1|1.5\nSELECT 4"
    );
    assert_eq!(
        last(&mut session, "SELECT c FROM t WHERE a = 'a' AND b = -5"),
        "c\n3\nSELECT 1"
    );
    assert_eq!(
        state(&mut session, "INSERT INTO t VALUES ('a', 2, 0)"),
        "23505"
    );
    assert_eq!(state(&mut session, "INSERT INTO t (b) VALUES (1)"), "23502");
}

#[test]
fn create_and_drop_table_follow_postgresql() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(last(&mut session, "create table T (X int)"), "CREATE TABLE");
    assert_eq!(
        last(
            &mut session,
            "insert into \"t\" values (1); select \"x\" from t"
        ),
        "x\n1\nSELECT 1"
    );
    assert_eq!(state(&mut session, "CREATE TABLE t (y int)"), "42P07");
    let skipped = run(&mut session, "CREATE TABLE IF NOT EXISTS t (y int)").unwrap_or_default();
    assert_eq!(skipped, vec!["CREATE TABLE".to_owned()]);
    assert_eq!(
        state(&mut session, "CREATE TABLE u (a int, a int)"),
        "42701"
    );
    assert_eq!(
        state(
            &mut session,
            "CREATE TABLE u (a int PRIMARY KEY, b int PRIMARY KEY)"
        ),
        "42P16"
    );
    assert_eq!(
        state(
            &mut session,
            "CREATE TABLE u (a int PRIMARY KEY, PRIMARY KEY (a))"
        ),
        "42P16"
    );
    assert_eq!(
        state(&mut session, "CREATE TABLE u (a int, PRIMARY KEY (b))"),
        "42703"
    );
    assert_eq!(
        state(&mut session, "CREATE TABLE u (a int DEFAULT b)"),
        "0A000"
    );
    assert_eq!(
        state(&mut session, "CREATE TABLE u (a nosuchtype)"),
        "42704"
    );
    assert_eq!(state(&mut session, "CREATE TABLE u (a numeric)"), "0A000");
    assert_eq!(
        state(&mut session, "CREATE TABLE u (a int UNIQUE)"),
        "0A000"
    );
    assert_eq!(state(&mut session, "CREATE INDEX i ON t (x)"), "0A000");
    assert_eq!(last(&mut session, "DROP TABLE t"), "DROP TABLE");
    assert_eq!(state(&mut session, "DROP TABLE t"), "42P01");
    assert_eq!(last(&mut session, "DROP TABLE IF EXISTS t"), "DROP TABLE");
    assert_eq!(
        last(&mut session, "CREATE TABLE t (x int); SELECT * FROM t"),
        "x\nSELECT 0"
    );
    last(&mut session, "DROP TABLE t");
    let keys: Vec<Vec<u8>> = store.snapshot().into_keys().collect();
    assert_eq!(keys.len(), 1, "only the catalog's meta key stays: {keys:?}");
    assert!(keys.iter().all(|key| key.starts_with(NAMESPACE)));
}

#[test]
fn not_supported_yet_answers_0a000_with_a_hint() {
    let store = MemoryStore::default();
    let mut session = with_people(&store);
    for sql in [
        "SELECT count(*) FROM people",
        "SELECT * FROM people a JOIN people b ON a.id = b.id",
        "SELECT * FROM people, people",
        "SELECT age FROM people GROUP BY age",
        "SELECT DISTINCT age FROM people",
        "SELECT * FROM people UNION SELECT * FROM people",
        "SELECT (SELECT 1)",
        "SELECT $1",
        "WITH x AS (SELECT 1) SELECT * FROM x",
        "INSERT INTO people SELECT * FROM people",
        "ALTER TABLE people ADD COLUMN x int",
        "CREATE TABLE ts (t timestamp)",
        "CREATE TABLE js (j jsonb)",
        "SAVEPOINT a",
    ] {
        let error = fails(&mut session, sql);
        assert_eq!(error.state().code(), "0A000", "{sql}: {error}");
        assert!(error.hint().is_some(), "{sql}");
    }
}

#[test]
fn syntax_errors_name_the_token_and_its_position() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let error = fails(&mut session, "SELECT 1 FROM WHERE");
    assert_eq!(error.state().code(), "42601");
    assert_eq!(error.message(), "syntax error at or near \"WHERE\"");
    assert_eq!(error.position(), Some(14));
    let end = fails(&mut session, "SELECT 1 +");
    assert_eq!(end.message(), "syntax error at end of input");
    assert_eq!(end.position(), Some(10));
    let second = fails(&mut session, "SELECT 1; SELEC 2");
    assert_eq!(second.position(), Some(10));
    assert_eq!(
        fails(&mut session, "SELECT 1 < 2 < 3").state().code(),
        "42601"
    );
    let nested = format!("SELECT {}1{}", "(-".repeat(30), ")".repeat(30));
    assert_eq!(last(&mut session, &nested), "?column?\n1\nSELECT 1");
    let deep = format!("SELECT {}1{}", "(".repeat(5000), ")".repeat(5000));
    assert_eq!(fails(&mut session, &deep).state().code(), "54001");
    let minus = format!("SELECT {}1", "- ".repeat(5000));
    assert_eq!(fails(&mut session, &minus).state().code(), "54001");
    let not = format!("SELECT {} true", "NOT ".repeat(5000));
    assert_eq!(fails(&mut session, &not).state().code(), "54001");
}

#[test]
fn transaction_blocks_commit_roll_back_and_abort_like_postgresql() {
    let store = MemoryStore::default();
    let mut session = with_people(&store);
    assert_eq!(
        run(&mut session, "BEGIN; INSERT INTO people (id, name) VALUES (10, 'x'); SELECT name FROM people WHERE id = 10").unwrap_or_default(),
        vec!["BEGIN".to_owned(), "INSERT 0 1".to_owned(), "name\nx\nSELECT 1".to_owned()]
    );
    assert_eq!(last(&mut session, "ROLLBACK"), "ROLLBACK");
    assert_eq!(
        last(&mut session, "SELECT name FROM people WHERE id = 10"),
        "name\nSELECT 0"
    );
    last(
        &mut session,
        "BEGIN; CREATE TABLE tmp (a int); INSERT INTO people (id, name) VALUES (11, 'y'); COMMIT",
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT name FROM people WHERE id = 11; SELECT * FROM tmp"
        ),
        "a\nSELECT 0"
    );
    let before = store.snapshot();
    last(
        &mut session,
        "BEGIN; INSERT INTO people (id, name) VALUES (12, 'z')",
    );
    assert_eq!(
        state(
            &mut session,
            "INSERT INTO people (id, name) VALUES (1, 'dup')"
        ),
        "23505"
    );
    assert_eq!(state(&mut session, "SELECT 1"), "25P02");
    assert_eq!(
        fails(&mut session, "SELECT 1").message(),
        "current transaction is aborted, commands ignored until end of transaction block"
    );
    assert_eq!(last(&mut session, "COMMIT"), "ROLLBACK");
    assert_eq!(store.snapshot(), before);
    assert_eq!(
        last(&mut session, "BEGIN READ ONLY; SELECT 1"),
        "?column?\n1\nSELECT 1"
    );
    assert_eq!(state(&mut session, "DELETE FROM people"), "25006");
    assert_eq!(last(&mut session, "ROLLBACK"), "ROLLBACK");
    assert!(!session.in_block());
    last(&mut session, "BEGIN; DELETE FROM people");
    assert_eq!(session.close(), Ok(true));
    assert_eq!(store.snapshot(), before);
}

#[test]
fn a_syntax_error_inside_a_block_aborts_it() {
    let store = MemoryStore::default();
    let mut session = with_people(&store);
    last(&mut session, "BEGIN; DELETE FROM people");
    assert_eq!(state(&mut session, "SELEC"), "42601");
    assert_eq!(state(&mut session, "SELECT 1"), "25P02");
    assert_eq!(last(&mut session, "ROLLBACK"), "ROLLBACK");
    assert_eq!(
        last(&mut session, "SELECT id FROM people WHERE id = 1"),
        "id\n1\nSELECT 1"
    );
}

struct Seeded(u64);

impl Seeded {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

#[test]
fn random_statements_agree_with_an_in_memory_model() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE m (k integer PRIMARY KEY, v text, n bigint)",
    );
    let mut model: BTreeMap<i32, (Option<String>, i64)> = BTreeMap::new();
    let mut random = Seeded(0x2545_F491_4F6C_DD1D);
    for step in 0..3_000 {
        let k = random.below(64) as i32 - 16;
        let n = random.below(1000) as i64 - 500;
        let in_block = random.below(10) == 0;
        let sql = match random.below(6) {
            0 | 1 => {
                let v = if random.below(5) == 0 {
                    "NULL".to_owned()
                } else {
                    format!("'v{n}'")
                };
                let sql = format!("INSERT INTO m VALUES ({k}, {v}, {n})");
                let outcome = run(&mut session, &sql);
                match model.entry(k) {
                    std::collections::btree_map::Entry::Occupied(_) => {
                        assert_eq!(
                            outcome.map_err(|e| e.state().code().to_owned()),
                            Err("23505".to_owned()),
                            "{step}: {sql}"
                        );
                    }
                    std::collections::btree_map::Entry::Vacant(slot) => {
                        assert!(outcome.is_ok(), "{step}: {sql}");
                        slot.insert(((v != "NULL").then(|| format!("v{n}")), n));
                    }
                }
                continue;
            }
            2 => {
                let sql = format!("UPDATE m SET n = n + {n} WHERE k >= {k}");
                let overflow = model
                    .range(k..)
                    .any(|(_, (_, value))| value.checked_add(n).is_none());
                assert!(!overflow);
                for (_, (_, value)) in model.range_mut(k..) {
                    *value += n;
                }
                sql
            }
            3 => {
                let sql = format!("DELETE FROM m WHERE k = {k} OR n < {}", n - 400);
                model.retain(|key, (_, value)| !(*key == k || *value < n - 400));
                sql
            }
            4 => {
                let sql = format!("UPDATE m SET v = v || 'x' WHERE v IS NOT NULL AND k < {k}");
                for (_, (v, _)) in model.range_mut(..k) {
                    if let Some(text) = v {
                        text.push('x');
                    }
                }
                sql
            }
            _ => {
                let expected_count = model.range(k..).count();
                let output = last(
                    &mut session,
                    &format!("SELECT k, v, n FROM m WHERE k >= {k} ORDER BY n DESC, k"),
                );
                let mut expected: Vec<(&i32, &(Option<String>, i64))> = model.range(k..).collect();
                expected.sort_by(|a, b| b.1.1.cmp(&a.1.1).then(a.0.cmp(b.0)));
                let mut text = "k|v|n\n".to_owned();
                for (key, (v, n)) in expected {
                    text.push_str(&format!("{key}|{}|{n}\n", v.as_deref().unwrap_or("NULL")));
                }
                text.push_str(&format!("SELECT {expected_count}"));
                assert_eq!(output, text, "step {step}");
                continue;
            }
        };
        let wrapped = if in_block {
            format!("BEGIN; {sql}; COMMIT")
        } else {
            sql.clone()
        };
        assert!(run(&mut session, &wrapped).is_ok(), "{step}: {wrapped}");
    }
}

#[test]
fn hostile_input_returns_errors_and_never_panics() {
    let store = MemoryStore::default();
    let mut session = with_people(&store);
    let fragments = [
        "SELECT",
        "INSERT INTO people VALUES",
        "(",
        ")",
        ",",
        ";",
        "'",
        "\"",
        "$$",
        "$1",
        "--",
        "/*",
        "*/",
        "1",
        "1.5",
        "1e400",
        "99999999999999999999",
        "-",
        "+",
        "*",
        "/",
        "%",
        "^",
        "||",
        "::",
        "=",
        "<>",
        "<",
        "NULL",
        "true",
        "people",
        "id",
        "name",
        "WHERE",
        "ORDER BY",
        "LIMIT",
        "OFFSET",
        "AND",
        "OR",
        "NOT",
        "IS",
        "IN",
        "BETWEEN",
        "LIKE",
        "CASE",
        "WHEN",
        "THEN",
        "ELSE",
        "END",
        "CAST",
        "AS",
        "integer",
        "text",
        "varchar(",
        "UPDATE",
        "SET",
        "DELETE FROM",
        "CREATE TABLE",
        "DROP TABLE",
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
        "PRIMARY KEY",
        "DEFAULT",
        "RETURNING",
        "E'\\x",
        "\u{0}",
        "\u{FFFD}",
        "é",
        ".",
        "[",
        "]",
        "coalesce(",
    ];
    let mut random = Seeded(0xDEAD_BEEF_CAFE_F00D);
    for _ in 0..20_000 {
        let length = 1 + random.below(14);
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
    let mut bytes = Seeded(7);
    for _ in 0..5_000 {
        let length = random.below(40) as usize;
        let raw: Vec<u8> = (0..length).map(|_| bytes.next() as u8).collect();
        let sql = String::from_utf8_lossy(&raw);
        let _ = run(&mut session, &sql);
        if session.in_block() {
            let _ = run(&mut session, "ROLLBACK");
        }
    }
}
