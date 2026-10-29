use crate::encoding::NAMESPACE;
use crate::memory_store::MemoryStore;
use crate::{QueryResult, Session, SqlError};
use std::collections::BTreeMap;

pub(crate) fn render(result: &QueryResult) -> String {
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
    assert_eq!(
        last(&mut session, "SELECT 1.5, 1.50 + 1, 10 / 4.0, 0.1 + 0.2"),
        "?column?|?column?|?column?|?column?\n1.5|2.50|2.5000000000000000|0.3\nSELECT 1"
    );
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
            "INSERT INTO people (id, name) VALUES ('9'::text, 'x')"
        ),
        "42804"
    );
    assert_eq!(
        last(
            &mut session,
            "INSERT INTO people (id, name) VALUES (90, 42), (91, true) RETURNING name"
        ),
        "name\n42\ntrue\nINSERT 0 2"
    );
    last(&mut session, "DELETE FROM people WHERE id >= 90");
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
        last(
            &mut session,
            "INSERT INTO people (id, name, age) VALUES (8, 'x', 2.5) RETURNING age"
        ),
        "age\n3\nINSERT 0 1"
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
    assert_eq!(state(&mut session, "CREATE TABLE u (a money)"), "0A000");
    assert_eq!(
        state(&mut session, "CREATE TABLE u (a numeric(0))"),
        "22023"
    );
    assert_eq!(
        state(&mut session, "CREATE TABLE u (a int, EXCLUDE (a WITH =))"),
        "0A000"
    );
    assert_eq!(state(&mut session, "CREATE INDEX i ON t (nosuch)"), "42703");
    assert_eq!(
        last(&mut session, "CREATE INDEX i ON t (x)"),
        "CREATE INDEX"
    );
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
        "SELECT * FROM people a FULL JOIN people b USING (id)",
        "SELECT * FROM people a NATURAL JOIN people b",
        "SELECT array_agg(name) FROM people",
        "SELECT age FROM people GROUP BY ROLLUP (age)",
        "SELECT DISTINCT ON (age) age FROM people",
        "SELECT * FROM people, LATERAL (SELECT 1) x",
        "SELECT '{{1}}'::int[]",
        "WITH x AS (DELETE FROM people RETURNING id) SELECT * FROM x",
        "INSERT INTO people SELECT * FROM people",
        "ALTER TABLE people RENAME CONSTRAINT a TO b",
        "CREATE TABLE ts (t money)",
        "CREATE TABLE js (j inet)",
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
    let sum = format!("SELECT 1{}", " + 1".repeat(100_000));
    assert_eq!(fails(&mut session, &sum).state().code(), "54001");
    let conjunction = format!("SELECT true{}", " AND true".repeat(100_000));
    assert_eq!(last(&mut session, &conjunction), "?column?\nt\nSELECT 1");
    let disjunction = format!("SELECT false{}", " OR 1 = 2".repeat(100_000));
    assert_eq!(last(&mut session, &disjunction), "?column?\nf\nSELECT 1");
    let casts = format!("SELECT 1{}", "::int".repeat(100_000));
    assert_eq!(fails(&mut session, &casts).state().code(), "54001");
    let typed = format!("SELECT {}'1'", "double precision ".repeat(100_000));
    assert_eq!(fails(&mut session, &typed).state().code(), "42601");
    assert_eq!(
        last(&mut session, "SELECT double precision '1.5', integer '7'"),
        "float8|int4\n1.5|7\nSELECT 1"
    );
    let longest_sum = format!("SELECT 1{}", " + 1".repeat(60));
    assert_eq!(last(&mut session, &longest_sum), "?column?\n61\nSELECT 1");
    let mut tallest = String::from("1");
    for _ in 0..40 {
        tallest = format!("1 + 1 * ({tallest})");
        let sql = format!("SELECT {tallest}");
        if let Err(error) = run(&mut session, &sql) {
            assert_eq!(error.state().code(), "54001", "{error}");
        }
    }
    let wide = format!("SELECT 1 IN ({})", vec!["2"; 50_000].join(", "));
    assert_eq!(last(&mut session, &wide), "?column?\nf\nSELECT 1");
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
        "JOIN",
        "LEFT JOIN",
        "CROSS JOIN",
        "INNER JOIN people q",
        "ON",
        "USING (id)",
        "p",
        "people p",
        "p.*",
        "p.id",
        "q.id",
        "GROUP BY",
        "HAVING",
        "DISTINCT",
        "count(*)",
        "count(",
        "sum(",
        "avg(",
        "min(",
        "max(",
        "FROM people p JOIN people q ON p.id = q.id",
    ];
    fuzz(&mut session, &fragments);
}

pub(crate) fn fuzz(session: &mut Session<'_, MemoryStore>, fragments: &[&str]) {
    let mut random = Seeded(0xDEAD_BEEF_CAFE_F00D);
    for _ in 0..20_000 {
        let length = 1 + random.below(14);
        let mut sql = String::new();
        for _ in 0..length {
            sql.push_str(fragments[random.below(fragments.len() as u64) as usize]);
            sql.push(' ');
        }
        if let Err(error) = run(session, &sql)
            && let Some(position) = error.position()
        {
            assert!(position <= sql.len(), "{sql}: {error}");
        }
        if session.in_block() {
            let _ = run(session, "ROLLBACK");
        }
    }
    let mut bytes = Seeded(7);
    for _ in 0..5_000 {
        let length = random.below(40) as usize;
        let raw: Vec<u8> = (0..length).map(|_| bytes.next() as u8).collect();
        let sql = String::from_utf8_lossy(&raw);
        let _ = run(session, &sql);
        if session.in_block() {
            let _ = run(session, "ROLLBACK");
        }
    }
}

#[test]
fn hostile_mysql_input_returns_errors_and_never_panics() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, crate::Dialect::MySql);
    last(
        &mut session,
        "CREATE TABLE `people` (`id` INT NOT NULL AUTO_INCREMENT, name VARCHAR(20) NOT NULL, age TINYINT, score DOUBLE, PRIMARY KEY (`id`)) ENGINE=InnoDB;
         INSERT INTO people (name, age, score) VALUES ('alice', 30, 9.25), (\"bob\", NULL, NULL);",
    );
    let fragments = [
        "SELECT",
        "INSERT INTO people VALUES",
        "INSERT IGNORE INTO people",
        "REPLACE INTO people",
        "INSERT INTO people (name) VALUE",
        "ON DUPLICATE KEY UPDATE",
        "age = VALUES(age)",
        "VALUES(",
        "(",
        ")",
        ",",
        ";",
        "'",
        "\"",
        "'a\\'b'",
        "\"x\\n\"",
        "'\\",
        "\\",
        "`",
        "`people`",
        "``",
        "`a``b`",
        "/*!",
        "/*!40101",
        "*/",
        "/*",
        "#",
        "\n",
        "--",
        "-- ",
        "@",
        "@@",
        "@a",
        "@@version",
        "@@session.",
        "@@global.sql_mode",
        ":=",
        "?",
        "1",
        "0",
        "2.5",
        "1e400",
        "99999999999999999999",
        "-",
        "/",
        "%",
        "DIV",
        "MOD",
        "<=>",
        "&&",
        "||",
        "!",
        "=",
        "NULL",
        "TRUE",
        "people",
        "id",
        "name",
        "WHERE",
        "ORDER BY",
        "LIMIT",
        "LIMIT 1, 1",
        "OFFSET",
        "AND",
        "IN",
        "LIKE",
        "IF(",
        "IFNULL(",
        "CONCAT(",
        "LENGTH(",
        "LAST_INSERT_ID()",
        "DATABASE()",
        "CREATE TABLE t (",
        "INT AUTO_INCREMENT",
        "PRIMARY KEY",
        "KEY k (",
        "TINYINT(1)",
        "VARCHAR(",
        "UNSIGNED",
        "ENGINE=InnoDB",
        "DEFAULT CHARSET=utf8mb4",
        "COMMENT 'c'",
        "DROP TABLE",
        "UPDATE people SET",
        "DELETE FROM",
        "SHOW TABLES",
        "SHOW FULL TABLES LIKE",
        "SHOW COLUMNS FROM",
        "SHOW CREATE TABLE",
        "SHOW VARIABLES LIKE",
        "SHOW WARNINGS",
        "SHOW COUNT(*) WARNINGS",
        "DESCRIBE",
        "DESC",
        "USE",
        "eggdb",
        "eggdb.people",
        "SET",
        "SET NAMES utf8mb4",
        "SET autocommit =",
        "SET SESSION",
        "SET @x =",
        "START TRANSACTION",
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
        "LOCK TABLES",
        "UNLOCK TABLES",
        "WRITE",
        "FROM DUAL",
        "JOIN",
        "\u{0}",
        "é",
        ".",
    ];
    fuzz(&mut session, &fragments);
}

#[test]
fn operators_bind_with_postgresql_precedence() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT 1 + 2 * 3 - 4 / 2, 2 * 3 ^ 2, -2 ^ 2, 10 - 2 - 3, 2 ^ 3 ^ 2"
        ),
        "?column?|?column?|?column?|?column?|?column?\n5|18|4|5|64\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT NOT 1 = 2, true OR false AND false, NOT false AND false, 'a' || 'b' = 'ab'"
        ),
        "?column?|?column?|?column?|?column?\nt|t|f|t\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT 1 BETWEEN 0 AND 2 AND true, 3 NOT BETWEEN 1 + 1 AND 2 * 2, 1 + 1 IN (2, 3) = true, NULL IS NULL = true, 2 NOTNULL"
        ),
        "?column?|?column?|?column?|?column?|?column?\nt|f|t|t|t\nSELECT 1"
    );
    assert_eq!(state(&mut session, "SELECT 1 = NOT true"), "42601");
    assert_eq!(state(&mut session, "SELECT 1 @@ 2"), "42883");
}

#[test]
fn review_cases_follow_postgresql() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT 2.0::float8 * 0, 0 * 1e-300::float8, NULL || NULL, nullif(1, '1'), true::text"
        ),
        "?column?|?column?|?column?|nullif|text\n0|0|NULL|NULL|true\nSELECT 1"
    );
    assert_eq!(
        state(&mut session, "SELECT 1e-300::float8 * 1e-300::float8"),
        "22003"
    );
    last(
        &mut session,
        "CREATE TABLE r (id int PRIMARY KEY, x real, label text); INSERT INTO r VALUES (1, 0.1, 'b'), (2, 0.5, 'A')",
    );
    assert_eq!(
        last(&mut session, "SELECT id FROM r WHERE x = 0.1"),
        "id\nSELECT 0"
    );
    assert_eq!(
        last(&mut session, "SELECT id FROM r WHERE x = 0.5"),
        "id\n2\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT x * 1.1 FROM r WHERE id = 1"),
        "?column?\n0.11000000163912774\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT lower(label) FROM r ORDER BY lower"),
        "lower\na\nb\nSELECT 2"
    );
    assert_eq!(
        last(&mut session, "SELECT label, label FROM r ORDER BY label"),
        "label|label\nA|A\nb|b\nSELECT 2"
    );
    assert_eq!(
        state(
            &mut session,
            "SELECT id AS label, label FROM r ORDER BY label"
        ),
        "42702"
    );
    last(&mut session, "BEGIN; INSERT INTO r VALUES (3, 1, 'c')");
    assert_eq!(state(&mut session, "SELECT 'unterminated"), "42601");
    assert_eq!(state(&mut session, "SELECT 1"), "25P02");
    assert_eq!(last(&mut session, "COMMIT"), "ROLLBACK");
    assert_eq!(
        last(&mut session, "SELECT id FROM r WHERE id = 3"),
        "id\nSELECT 0"
    );
}

#[test]
fn execute_while_stops_before_the_next_statement_when_told_to() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let mut seen = Vec::new();
    session
        .execute_while(
            "CREATE TABLE t (a integer PRIMARY KEY); INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)",
            &mut |result| {
                seen.push(result.tag);
                seen.len() < 2
            },
        )
        .unwrap();
    assert_eq!(seen, ["CREATE TABLE", "INSERT 0 1"]);
    assert_eq!(last(&mut session, "SELECT a FROM t"), "a\n1\nSELECT 1");
}

#[test]
fn a_cached_definition_follows_a_drop_and_a_new_table_of_the_same_name() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let mut other = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE t (a integer PRIMARY KEY); INSERT INTO t VALUES (1)",
    );
    assert_eq!(last(&mut session, "SELECT * FROM t"), "a\n1\nSELECT 1");
    last(
        &mut other,
        "DROP TABLE t; CREATE TABLE t (b text PRIMARY KEY, c integer); INSERT INTO t VALUES ('x', 2)",
    );
    assert_eq!(last(&mut session, "SELECT * FROM t"), "b|c\nx|2\nSELECT 1");
    last(&mut other, "DROP TABLE t");
    assert_eq!(state(&mut session, "SELECT * FROM t"), "42P01");
}

#[test]
fn literal_inserts_take_the_direct_path_and_match_the_general_one() {
    const TABLE: &str = "CREATE TABLE t (k bigint PRIMARY KEY, s smallint, d double precision, r real, b boolean, x text NOT NULL DEFAULT 'none', v varchar(3))";
    const VALUES: [&str; 26] = [
        "1",
        "-1",
        "32767",
        "32768",
        "-32769",
        "9223372036854775807",
        "9223372036854775808",
        "-9223372036854775808",
        "99999999999999999999",
        "1.5",
        "-2.5e3",
        ".5",
        "1e400",
        "'abc'",
        "'12'",
        "'x''y'",
        "E'a\\nb'",
        "''",
        "'true'",
        "'NaN'",
        "null",
        "true",
        "false",
        "$$q$$",
        "'-0'",
        "'1.5'",
    ];
    const COLUMNS: [&str; 7] = ["k", "s", "d", "r", "b", "x", "v"];
    let direct_store = MemoryStore::default();
    let general_store = MemoryStore::default();
    let mut direct = Session::new(&direct_store);
    let mut general = Session::new(&general_store);
    last(&mut direct, TABLE);
    last(&mut general, TABLE);
    let mut random = Seeded(41);
    for round in 0..3000u64 {
        let width = 1 + (random.next() % 7) as usize;
        let rows = 1 + (random.next() % 3) as usize;
        let mut tuples = Vec::new();
        for row in 0..rows {
            let mut cells = vec![format!("{}", (round * 3 + row as u64) % 2500)];
            for _ in 1..width {
                cells.push(VALUES[(random.next() % VALUES.len() as u64) as usize].to_owned());
            }
            if random.next().is_multiple_of(10) {
                cells[0] = VALUES[(random.next() % VALUES.len() as u64) as usize].to_owned();
            }
            tuples.push(format!("({})", cells.join(", ")));
        }
        let values = tuples.join(", ");
        let direct_sql = format!("INSERT INTO t VALUES {values}");
        let general_sql = format!(
            "INSERT INTO t ({}) VALUES {values}",
            COLUMNS[..width].join(", ")
        );
        let shift = general_sql.len() - direct_sql.len();
        let outcome = |session: &mut Session<'_, MemoryStore>, sql: &str, shift: usize| {
            run(session, sql).map_err(|error| {
                (
                    error.state(),
                    error.message().to_owned(),
                    error
                        .position()
                        .map(|position| position.saturating_sub(shift)),
                )
            })
        };
        assert_eq!(
            outcome(&mut direct, &direct_sql, 0),
            outcome(&mut general, &general_sql, shift),
            "{direct_sql}"
        );
    }
    let everything = "SELECT * FROM t ORDER BY k";
    assert_eq!(
        run(&mut direct, everything).unwrap(),
        run(&mut general, everything).unwrap()
    );
}

#[test]
fn rows_without_a_key_stop_at_the_last_row_identifier_in_one_statement_or_many() {
    use crate::store::{KeyValueStore, KeyValueWrite};
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(&mut session, "CREATE TABLE t (a integer)");
    let mut writer = store.begin_write().unwrap();
    let mut key = crate::encoding::rows_prefix(1);
    key.extend_from_slice(&(u64::MAX - 1).to_be_bytes());
    writer
        .put(
            &key,
            &crate::encoding::encode_row(&[crate::Value::Integer(1)]),
        )
        .unwrap();
    writer.commit().unwrap();
    assert_eq!(
        state(&mut session, "INSERT INTO t VALUES (2), (3)"),
        "54000"
    );
    assert_eq!(
        state(&mut session, "INSERT INTO t (a) VALUES (2), (3)"),
        "54000"
    );
    assert_eq!(last(&mut session, "SELECT a FROM t"), "a\n1\nSELECT 1");
    assert_eq!(last(&mut session, "INSERT INTO t VALUES (2)"), "INSERT 0 1");
    assert_eq!(state(&mut session, "INSERT INTO t VALUES (3)"), "54000");
    assert_eq!(last(&mut session, "SELECT a FROM t"), "a\n1\n2\nSELECT 2");
}

fn with_orders(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = with_people(store);
    last(
        &mut session,
        "CREATE TABLE orders (id integer PRIMARY KEY, person_id integer, amount integer, note text);
         INSERT INTO orders VALUES (10, 1, 100, 'a'), (11, 1, 50, NULL), (12, 3, 70, 'c'), (13, NULL, 5, 'x'), (14, 9, 1, 'orphan');
         CREATE TABLE a (k integer, x text);
         INSERT INTO a VALUES (1, 'a1'), (2, 'a2'), (NULL, 'an');
         CREATE TABLE b (k integer, y text);
         INSERT INTO b VALUES (2, 'b2'), (3, 'b3'), (NULL, 'bn');",
    );
    session
}

#[test]
fn inner_and_left_joins_follow_postgresql() {
    let store = MemoryStore::default();
    let mut session = with_orders(&store);
    let cases = [
        (
            "SELECT p.name, o.amount FROM people p JOIN orders o ON o.person_id = p.id ORDER BY o.id",
            "name|amount\nalice|100\nalice|50\ncarol|70\nSELECT 3",
        ),
        (
            "SELECT p.name, o.amount FROM people AS p INNER JOIN orders AS o ON p.id = o.person_id ORDER BY o.amount",
            "name|amount\nalice|50\ncarol|70\nalice|100\nSELECT 3",
        ),
        (
            "SELECT p.name, o.id FROM people p LEFT JOIN orders o ON o.person_id = p.id ORDER BY p.id, o.id",
            "name|id\nalice|10\nalice|11\nbob|NULL\ncarol|12\ndave|NULL\nSELECT 5",
        ),
        (
            "SELECT p.name, o.id FROM people p LEFT OUTER JOIN orders o ON o.person_id = p.id AND o.amount > 60 ORDER BY p.id",
            "name|id\nalice|10\nbob|NULL\ncarol|12\ndave|NULL\nSELECT 4",
        ),
        (
            "SELECT p.name, o.id FROM people p LEFT JOIN orders o ON o.person_id = p.id WHERE o.amount > 60 ORDER BY p.id",
            "name|id\nalice|10\ncarol|12\nSELECT 2",
        ),
        (
            "SELECT p.name FROM people p LEFT JOIN orders o ON o.person_id = p.id WHERE o.id IS NULL ORDER BY 1",
            "name\nbob\ndave\nSELECT 2",
        ),
        (
            "SELECT a.x, b.y FROM a JOIN b ON a.k = b.k",
            "x|y\na2|b2\nSELECT 1",
        ),
        (
            "SELECT a.x, b.y FROM a LEFT JOIN b ON a.k = b.k ORDER BY a.x",
            "x|y\na1|NULL\na2|b2\nan|NULL\nSELECT 3",
        ),
        (
            "SELECT a.x, b.y FROM a JOIN b ON a.k = b.k WHERE b.y <> 'b2'",
            "x|y\nSELECT 0",
        ),
        (
            "SELECT a.x, b.y FROM a LEFT JOIN b ON a.k = b.k WHERE b.y IS NULL ORDER BY 1",
            "x|y\na1|NULL\nan|NULL\nSELECT 2",
        ),
        (
            "SELECT p.name FROM orders o JOIN people p ON p.id = o.person_id WHERE p.age > 35 ORDER BY 1",
            "name\ncarol\nSELECT 1",
        ),
        (
            "SELECT o.id, p.name FROM orders o LEFT JOIN people p ON p.id = o.person_id WHERE p.name IS NULL ORDER BY 1",
            "id|name\n13|NULL\n14|NULL\nSELECT 2",
        ),
        (
            "SELECT a.x, b.y FROM a JOIN b ON a.k + 0 = b.k",
            "x|y\na2|b2\nSELECT 1",
        ),
        (
            "SELECT a.x, b.y FROM a JOIN b ON a.k < b.k ORDER BY 1, 2",
            "x|y\na1|b2\na1|b3\na2|b3\nSELECT 3",
        ),
        (
            "SELECT * FROM a JOIN b ON a.k = b.k",
            "k|x|k|y\n2|a2|2|b2\nSELECT 1",
        ),
        (
            "SELECT * FROM a JOIN b USING (k)",
            "k|x|y\n2|a2|b2\nSELECT 1",
        ),
        (
            "SELECT * FROM a LEFT JOIN b USING (k) ORDER BY x",
            "k|x|y\n1|a1|NULL\n2|a2|b2\nNULL|an|NULL\nSELECT 3",
        ),
        (
            "SELECT k, a.k, b.k FROM a LEFT JOIN b USING (k) ORDER BY x",
            "k|k|k\n1|1|NULL\n2|2|2\nNULL|NULL|NULL\nSELECT 3",
        ),
        (
            "SELECT o.*, p.name FROM orders o JOIN people p ON p.id = o.person_id WHERE o.id = 12",
            "id|person_id|amount|note|name\n12|3|70|c|carol\nSELECT 1",
        ),
        (
            "SELECT p.name, o.id, a.x FROM people p JOIN orders o ON o.person_id = p.id LEFT JOIN a ON a.k = p.id ORDER BY o.id",
            "name|id|x\nalice|10|a1\nalice|11|a1\ncarol|12|NULL\nSELECT 3",
        ),
        (
            "SELECT p.name FROM people p JOIN orders o ON o.person_id = p.id ORDER BY o.amount",
            "name\nalice\ncarol\nalice\nSELECT 3",
        ),
        (
            "SELECT a.x, b.y FROM a CROSS JOIN b ORDER BY 1, 2 LIMIT 2",
            "x|y\na1|b2\na1|b3\nSELECT 2",
        ),
        (
            "SELECT a.x, b.y FROM a, b WHERE a.k = b.k",
            "x|y\na2|b2\nSELECT 1",
        ),
        (
            "SELECT a.x, p.name FROM a, b JOIN people p ON p.id = b.k WHERE a.k = 1 ORDER BY 2",
            "x|name\na1|bob\na1|carol\nSELECT 2",
        ),
        (
            "SELECT p.name, q.name FROM people p JOIN people q ON q.id = p.id + 1 WHERE p.id = 1",
            "name|name\nalice|bob\nSELECT 1",
        ),
        (
            "SELECT o.id, p.name FROM orders o LEFT JOIN people p ON p.id = o.person_id ORDER BY o.id",
            "id|name\n10|alice\n11|alice\n12|carol\n13|NULL\n14|NULL\nSELECT 5",
        ),
        (
            "SELECT p.name FROM people p JOIN orders o ON o.person_id = p.id AND o.note = p.name ORDER BY 1",
            "name\nSELECT 0",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(last(&mut session, sql), expected, "{sql}");
    }
}

#[test]
fn join_errors_follow_postgresql() {
    let store = MemoryStore::default();
    let mut session = with_orders(&store);
    let error = fails(
        &mut session,
        "SELECT id FROM people p JOIN orders o ON o.person_id = p.id",
    );
    assert_eq!(error.state().code(), "42702");
    assert_eq!(error.message(), "column reference \"id\" is ambiguous");
    assert_eq!(error.position(), Some(7));
    let error = fails(
        &mut session,
        "SELECT x.id FROM people p JOIN orders o ON true",
    );
    assert_eq!(error.state().code(), "42P01");
    assert_eq!(error.message(), "missing FROM-clause entry for table \"x\"");
    let error = fails(
        &mut session,
        "SELECT * FROM a, b JOIN people p ON a.k = p.id",
    );
    assert_eq!(error.state().code(), "42P01");
    assert_eq!(
        error.message(),
        "invalid reference to FROM-clause entry for table \"a\""
    );
    assert!(error.hint().is_some());
    let cases = [
        ("SELECT p.nope FROM people p JOIN orders o ON true", "42703"),
        ("SELECT * FROM people JOIN people ON true", "42712"),
        ("SELECT * FROM people p JOIN orders p ON true", "42712"),
        ("SELECT * FROM a JOIN b USING (nope)", "42703"),
        ("SELECT * FROM people JOIN orders USING (amount)", "42703"),
        ("SELECT * FROM orders JOIN people USING (amount)", "42703"),
        ("SELECT * FROM people JOIN orders USING (id, id)", "42701"),
        ("SELECT * FROM people p JOIN orders o USING (name)", "42703"),
        ("SELECT * FROM people p JOIN orders o ON 1", "42804"),
        (
            "SELECT * FROM people p JOIN orders o ON p.name = o.id",
            "42883",
        ),
        ("SELECT * FROM people JOIN orders ON count(*) > 1", "42803"),
        ("SELECT * FROM a JOIN b", "42601"),
        ("SELECT * FROM a CROSS JOIN b ON true", "42601"),
        (
            "SELECT * FROM a JOIN b ON b.k = c.k JOIN people c ON true",
            "42P01",
        ),
        ("SELECT * FROM a FULL JOIN b USING (k)", "0A000"),
        ("SELECT * FROM a, LATERAL (SELECT 1) s", "0A000"),
        ("SELECT * FROM a NATURAL JOIN b", "0A000"),
        ("SELECT * FROM a JOIN (a JOIN b ON true) ON true", "0A000"),
        ("SELECT * FROM nope JOIN a ON true", "42P01"),
        ("SELECT * FROM a JOIN nope ON true", "42P01"),
    ];
    for (sql, expected) in cases {
        assert_eq!(state(&mut session, sql), expected, "{sql}");
    }
    let many = format!(
        "SELECT count(*) FROM a{}",
        (0..70).map(|n| format!(", a a{n}")).collect::<String>()
    );
    assert_eq!(state(&mut session, &many), "54000");
    let mismatch = fails(
        &mut session,
        "CREATE TABLE c (k text); SELECT * FROM a JOIN c USING (k)",
    );
    assert_eq!(mismatch.state().code(), "42883");
    assert_eq!(
        mismatch.message(),
        "operator does not exist: integer = text"
    );
}

#[test]
fn group_by_and_aggregates_follow_postgresql() {
    let store = MemoryStore::default();
    let mut session = with_orders(&store);
    let cases = [
        (
            "SELECT person_id, count(*), sum(amount), min(note), max(amount) FROM orders GROUP BY person_id ORDER BY person_id",
            "person_id|count|sum|min|max\n1|2|150|a|100\n3|1|70|c|70\n9|1|1|orphan|1\nNULL|1|5|x|5\nSELECT 4",
        ),
        (
            "SELECT count(*), count(age), sum(age), min(name), max(score) FROM people",
            "count|count|sum|min|max\n4|3|96|alice|NaN\nSELECT 1",
        ),
        (
            "SELECT avg(score) FROM people WHERE id < 4",
            "avg\n8.375\nSELECT 1",
        ),
        ("SELECT avg(age::float8) FROM people", "avg\n32\nSELECT 1"),
        (
            "SELECT count(*), count(age), sum(age), avg(score), min(age), max(name) FROM people WHERE id > 100",
            "count|count|sum|avg|min|max\n0|0|NULL|NULL|NULL|NULL\nSELECT 1",
        ),
        (
            "SELECT age, count(*) FROM people WHERE id > 100 GROUP BY age",
            "age|count\nSELECT 0",
        ),
        (
            "SELECT person_id, sum(amount) AS total FROM orders GROUP BY person_id HAVING sum(amount) > 10 ORDER BY total DESC",
            "person_id|total\n1|150\n3|70\nSELECT 2",
        ),
        (
            "SELECT count(*) FROM people HAVING count(*) > 10",
            "count\nSELECT 0",
        ),
        (
            "SELECT person_id FROM orders GROUP BY person_id ORDER BY count(*) DESC, person_id",
            "person_id\n1\n3\n9\nNULL\nSELECT 4",
        ),
        (
            "SELECT age > 28 AS old, count(*) FROM people GROUP BY 1 ORDER BY 1",
            "old|count\nf|1\nt|2\nNULL|1\nSELECT 3",
        ),
        (
            "SELECT age > 28 AS old, count(*) FROM people GROUP BY old ORDER BY old",
            "old|count\nf|1\nt|2\nNULL|1\nSELECT 3",
        ),
        (
            "SELECT age / 10 AS decade, count(*) FROM people GROUP BY age / 10 ORDER BY decade",
            "decade|count\n2|1\n3|1\n4|1\nNULL|1\nSELECT 4",
        ),
        (
            "SELECT count(*) * 2 + 1 AS n, max(amount) - min(amount) AS spread FROM orders",
            "n|spread\n11|99\nSELECT 1",
        ),
        (
            "SELECT p.id, p.name, count(o.id) FROM people p LEFT JOIN orders o ON o.person_id = p.id GROUP BY p.id ORDER BY p.id",
            "id|name|count\n1|alice|2\n2|bob|0\n3|carol|1\n4|dave|0\nSELECT 4",
        ),
        (
            "SELECT p.name, sum(o.amount) FROM people p JOIN orders o ON o.person_id = p.id GROUP BY p.name ORDER BY 2",
            "name|sum\ncarol|70\nalice|150\nSELECT 2",
        ),
        (
            "SELECT count(DISTINCT person_id), count(person_id), count(DISTINCT amount > 10) FROM orders",
            "count|count|count\n3|4|2\nSELECT 1",
        ),
        (
            "SELECT sum(DISTINCT amount / 50), sum(ALL amount / 50) FROM orders",
            "sum|sum\n3|4\nSELECT 1",
        ),
        ("SELECT DISTINCT active FROM people", "active\nt\nSELECT 1"),
        (
            "SELECT DISTINCT person_id FROM orders ORDER BY person_id",
            "person_id\n1\n3\n9\nNULL\nSELECT 4",
        ),
        (
            "SELECT DISTINCT person_id IS NULL AS missing FROM orders ORDER BY person_id IS NULL",
            "missing\nf\nt\nSELECT 2",
        ),
        (
            "SELECT DISTINCT a.k FROM a, b ORDER BY 1 LIMIT 2 OFFSET 1",
            "k\n2\nNULL\nSELECT 2",
        ),
        ("SELECT count(*)", "count\n1\nSELECT 1"),
        ("SELECT count(*) WHERE false", "count\n0\nSELECT 1"),
        (
            "SELECT person_id AS who, count(*) FROM orders GROUP BY who HAVING person_id = 1",
            "who|count\n1|2\nSELECT 1",
        ),
        (
            "SELECT max(score), min(score) FROM people",
            "max|min\nNaN|7.5\nSELECT 1",
        ),
        (
            "SELECT p.age + 1, count(*) FROM people p GROUP BY age + 1 ORDER BY 1",
            "?column?|count\n26|1\n31|1\n42|1\nNULL|1\nSELECT 4",
        ),
        (
            "SELECT a.x || '!', count(*) FROM a JOIN b ON a.k = b.k GROUP BY x || '!'",
            "?column?|count\na2!|1\nSELECT 1",
        ),
        (
            "SELECT * FROM people GROUP BY id ORDER BY id LIMIT 1",
            "id|name|age|score|active\n1|alice|30|9.25|t\nSELECT 1",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(last(&mut session, sql), expected, "{sql}");
    }
}

#[test]
fn grouping_errors_follow_postgresql() {
    let store = MemoryStore::default();
    let mut session = with_orders(&store);
    let error = fails(
        &mut session,
        "SELECT name, count(*) FROM people GROUP BY age",
    );
    assert_eq!(error.state().code(), "42803");
    assert_eq!(
        error.message(),
        "column \"people.name\" must appear in the GROUP BY clause or be used in an aggregate function"
    );
    assert_eq!(error.position(), Some(7));
    let error = fails(
        &mut session,
        "SELECT p.name, count(*) FROM people p JOIN orders o ON true GROUP BY o.id",
    );
    assert_eq!(
        error.message(),
        "column \"p.name\" must appear in the GROUP BY clause or be used in an aggregate function"
    );
    let cases = [
        ("SELECT count(*) FROM people GROUP BY count(*)", "42803"),
        ("SELECT count(*) FROM people GROUP BY 1", "42803"),
        ("SELECT * FROM people WHERE count(*) > 1", "42803"),
        ("SELECT sum(count(*)) FROM people", "42803"),
        ("SELECT name FROM people HAVING count(*) > 1", "42803"),
        ("SELECT name FROM people ORDER BY count(*)", "42803"),
        ("SELECT sum(name) FROM people", "42883"),
        ("SELECT avg(name) FROM people", "42883"),
        ("SELECT min(active) FROM people", "42883"),
        ("SELECT sum(*) FROM people", "42809"),
        ("SELECT count() FROM people", "42809"),
        ("SELECT count(age, name) FROM people", "42883"),
        ("SELECT age FROM people GROUP BY 6", "42P10"),
        ("SELECT age FROM people GROUP BY 0", "42P10"),
        ("SELECT age FROM people GROUP BY nope", "42703"),
        (
            "SELECT DISTINCT person_id FROM orders ORDER BY amount",
            "42P10",
        ),
        ("SELECT DISTINCT ON (person_id) amount FROM orders", "0A000"),
        ("SELECT age FROM people GROUP BY ROLLUP (age)", "0A000"),
        ("SELECT age FROM people GROUP BY ()", "0A000"),
        ("SELECT array_agg(name) FROM people", "0A000"),
        ("SELECT count(*) OVER () FROM people", "0A000"),
        ("SELECT count(*) FILTER (WHERE true) FROM people", "0A000"),
        ("UPDATE people SET age = count(*)", "42803"),
        ("UPDATE people SET age = 1 WHERE count(*) > 1", "42803"),
        ("DELETE FROM people WHERE count(*) > 1", "42803"),
        ("DELETE FROM people RETURNING count(*)", "42803"),
        ("INSERT INTO people VALUES (count(*), 'x')", "42803"),
        ("SELECT 1 LIMIT count(*)", "42803"),
        ("CREATE TABLE d (n int DEFAULT count(*))", "42803"),
        (
            "SELECT count(*) FROM people GROUP BY name HAVING 1",
            "42804",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(state(&mut session, sql), expected, "{sql}");
    }
}

#[test]
fn sums_keep_postgresql_result_types() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE n (i integer, b bigint, r real, d double precision);
         INSERT INTO n VALUES (2147483647, 9223372036854775807, 1.5, 1e308), (1, 1, 2.25, 1e308)",
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT sum(i), sum(r), sum(b) FROM n WHERE i = 1"
        ),
        "sum|sum|sum\n1|2.25|1\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT sum(i) FROM n"),
        "sum\n2147483648\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT sum(b) FROM n WHERE i = 1 HAVING sum(b) > 0 AND sum(b) IS NOT NULL ORDER BY sum(b)"
        ),
        "sum\n1\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT sum(b), avg(i) FROM n"),
        "sum|avg\n9223372036854775808|1073741824.00000000\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT sum(b) / 2 FROM n WHERE i = 1"),
        "?column?\n0.50000000000000000000\nSELECT 1"
    );
    assert_eq!(state(&mut session, "SELECT sum(d) FROM n"), "22003");
    assert_eq!(
        last(&mut session, "SELECT sum(d) FROM n WHERE i = 1"),
        "sum\n1e+308\nSELECT 1"
    );
}

#[test]
fn every_join_method_agrees_on_random_tables() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE l (id integer PRIMARY KEY, k integer, v text);
         CREATE TABLE r (id integer PRIMARY KEY, k integer, w integer);
         CREATE TABLE h (k integer, w integer)",
    );
    let mut random = Seeded(0x9E37_79B9_7F4A_7C15);
    let value = |random: &mut Seeded| {
        if random.below(6) == 0 {
            "NULL".to_owned()
        } else {
            random.below(12).to_string()
        }
    };
    for id in 0..60 {
        let k = value(&mut random);
        last(
            &mut session,
            &format!("INSERT INTO l VALUES ({id}, {k}, 'v{id}')"),
        );
        let k = value(&mut random);
        let w = value(&mut random);
        last(
            &mut session,
            &format!("INSERT INTO r VALUES ({id}, {k}, {w}); INSERT INTO h VALUES ({k}, {w})"),
        );
    }
    for kind in ["JOIN", "LEFT JOIN"] {
        let methods = [
            format!("SELECT l.id, r.w FROM l {kind} r ON r.k = l.k ORDER BY 1, 2"),
            format!("SELECT l.id, r.w FROM l {kind} r ON l.k = r.k + 0 ORDER BY 1, 2"),
            format!("SELECT l.id, r.w FROM l {kind} r USING (k) ORDER BY 1, 2"),
            format!("SELECT l.id, h.w FROM l {kind} h ON h.k = l.k ORDER BY 1, 2"),
        ];
        let expected = last(&mut session, &methods[0]);
        for sql in &methods[1..] {
            assert_eq!(last(&mut session, sql), expected, "{sql}");
        }
        let by_key = [
            format!("SELECT l.v, r.w FROM l {kind} r ON r.id = l.k ORDER BY 1, 2"),
            format!("SELECT l.v, r.w FROM l {kind} r ON r.id + 0 = l.k ORDER BY 1, 2"),
        ];
        assert_eq!(
            last(&mut session, &by_key[0]),
            last(&mut session, &by_key[1]),
            "{kind}"
        );
        for id in (0..60).step_by(7) {
            let looked_up = format!(
                "SELECT l.v, r.w FROM l {kind} r ON r.id = l.k WHERE l.id = {id} ORDER BY 1, 2"
            );
            let scanned = format!(
                "SELECT l.v, r.w FROM l {kind} r ON r.id + 0 = l.k WHERE l.id + 0 = {id} ORDER BY 1, 2"
            );
            assert_eq!(
                last(&mut session, &looked_up),
                last(&mut session, &scanned),
                "{looked_up}"
            );
        }
    }
    let grouped = last(
        &mut session,
        "SELECT k, count(*), count(w), sum(w), min(w), max(w) FROM r GROUP BY k ORDER BY k",
    );
    let mut groups: BTreeMap<Option<i64>, Vec<Option<i64>>> = BTreeMap::new();
    let rows = last(&mut session, "SELECT k, w FROM r");
    for line in rows.lines().skip(1).filter(|line| line.contains('|')) {
        let mut cells = line.split('|').map(|cell| cell.parse::<i64>().ok());
        let k = cells.next().flatten();
        let w = cells.next().flatten();
        groups.entry(k).or_default().push(w);
    }
    let mut text = "k|count|count|sum|min|max\n".to_owned();
    let shown = |value: Option<i64>| value.map_or("NULL".to_owned(), |value| value.to_string());
    let ordered: Vec<_> = groups
        .iter()
        .filter(|(k, _)| k.is_some())
        .chain(groups.iter().filter(|(k, _)| k.is_none()))
        .collect();
    for (k, values) in &ordered {
        let present: Vec<i64> = values.iter().flatten().copied().collect();
        let sum = (!present.is_empty()).then(|| present.iter().sum::<i64>());
        text.push_str(&format!(
            "{}|{}|{}|{}|{}|{}\n",
            shown(**k),
            values.len(),
            present.len(),
            shown(sum),
            shown(present.iter().min().copied()),
            shown(present.iter().max().copied()),
        ));
    }
    text.push_str(&format!("SELECT {}", ordered.len()));
    assert_eq!(grouped, text);
}

#[test]
fn where_equalities_join_comma_and_cross_joined_tables_like_on_conditions() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE l (id integer PRIMARY KEY, k integer, v text);
         CREATE TABLE r (id integer PRIMARY KEY, k integer, w integer);
         CREATE TABLE h (k integer, w integer)",
    );
    let mut random = Seeded(0x2545_F491_4F6C_DD1D);
    let value = |random: &mut Seeded| {
        if random.below(5) == 0 {
            "NULL".to_owned()
        } else {
            random.below(10).to_string()
        }
    };
    for id in 0..50 {
        let k = value(&mut random);
        last(
            &mut session,
            &format!("INSERT INTO l VALUES ({id}, {k}, 'v{id}')"),
        );
        let k = value(&mut random);
        let w = value(&mut random);
        last(
            &mut session,
            &format!("INSERT INTO r VALUES ({id}, {k}, {w}); INSERT INTO h VALUES ({k}, {w})"),
        );
    }
    let same = [
        [
            "SELECT l.id, r.w FROM l JOIN r ON r.k = l.k ORDER BY 1, 2",
            "SELECT l.id, r.w FROM l, r WHERE r.k = l.k ORDER BY 1, 2",
            "SELECT l.id, r.w FROM l CROSS JOIN r WHERE l.k = r.k ORDER BY 1, 2",
            "SELECT l.id, r.w FROM l, r WHERE r.k + 0 = l.k ORDER BY 1, 2",
        ],
        [
            "SELECT l.v, r.w FROM l JOIN r ON r.id = l.k WHERE l.id < 30 ORDER BY 1, 2",
            "SELECT l.v, r.w FROM l, r WHERE r.id = l.k AND l.id < 30 ORDER BY 1, 2",
            "SELECT l.v, r.w FROM r, l WHERE l.k = r.id AND l.id < 30 ORDER BY 1, 2",
            "SELECT l.v, r.w FROM l, r WHERE r.id + 0 = l.k AND l.id < 30 ORDER BY 1, 2",
        ],
        [
            "SELECT l.id, r.id, h.w FROM l JOIN r ON r.k = l.k JOIN h ON h.w = r.w AND h.k = l.k ORDER BY 1, 2, 3",
            "SELECT l.id, r.id, h.w FROM l, r, h WHERE r.k = l.k AND h.w = r.w AND h.k = l.k ORDER BY 1, 2, 3",
            "SELECT l.id, r.id, h.w FROM l, h, r WHERE l.k = h.k AND r.w = h.w AND r.k = l.k ORDER BY 1, 2, 3",
            "SELECT l.id, r.id, h.w FROM l, r, h WHERE r.k + 0 = l.k AND h.w + 0 = r.w AND h.k + 0 = l.k ORDER BY 1, 2, 3",
        ],
        [
            "SELECT l.id, r.id, h.w FROM l JOIN r ON r.k = l.k LEFT JOIN h ON h.k = r.w ORDER BY 1, 2, 3",
            "SELECT l.id, r.id, h.w FROM l, r LEFT JOIN h ON h.k = r.w WHERE r.k = l.k ORDER BY 1, 2, 3",
            "SELECT l.id, r.id, h.w FROM l CROSS JOIN r LEFT JOIN h ON h.k = r.w WHERE l.k = r.k ORDER BY 1, 2, 3",
            "SELECT l.id, r.id, h.w FROM l, r LEFT JOIN h ON h.k = r.w WHERE r.k + 0 = l.k ORDER BY 1, 2, 3",
        ],
        [
            "SELECT l.id, h.w FROM l LEFT JOIN r ON r.id = l.id, h WHERE h.k = r.k ORDER BY 1, 2",
            "SELECT l.id, h.w FROM l LEFT JOIN r ON r.id = l.id, h WHERE h.k + 0 = r.k ORDER BY 1, 2",
            "SELECT l.id, h.w FROM l LEFT JOIN r ON r.id = l.id JOIN h ON h.k = r.k ORDER BY 1, 2",
            "SELECT l.id, h.w FROM l LEFT JOIN r ON r.id = l.id CROSS JOIN h WHERE r.k = h.k ORDER BY 1, 2",
        ],
        [
            "SELECT l.id, r.id, h.w FROM l RIGHT JOIN r ON r.id = l.k, h WHERE h.k = r.k ORDER BY 1, 2, 3",
            "SELECT l.id, r.id, h.w FROM l RIGHT JOIN r ON r.id = l.k, h WHERE h.k + 0 = r.k ORDER BY 1, 2, 3",
            "SELECT l.id, r.id, h.w FROM l RIGHT JOIN r ON r.id = l.k JOIN h ON h.k = r.k ORDER BY 1, 2, 3",
            "SELECT l.id, r.id, h.w FROM l RIGHT JOIN r ON r.id = l.k CROSS JOIN h WHERE r.k = h.k ORDER BY 1, 2, 3",
        ],
        [
            "SELECT l.id, r.id FROM l, r WHERE l.k = r.k AND l.k IS NULL ORDER BY 1, 2",
            "SELECT l.id, r.id FROM l, r WHERE l.k + 0 = r.k AND l.k IS NULL ORDER BY 1, 2",
            "SELECT l.id, r.id FROM l JOIN r ON l.k = r.k WHERE l.k IS NULL ORDER BY 1, 2",
            "SELECT l.id, r.id FROM l, r WHERE (l.k = r.k) IS NULL AND l.k = r.k ORDER BY 1, 2",
        ],
        [
            "SELECT count(*) FROM l, r WHERE l.k = r.k OR l.id = r.id",
            "SELECT count(*) FROM l, r WHERE l.k + 0 = r.k OR l.id = r.id + 0",
            "SELECT count(*) FROM l, r WHERE NOT (l.k <> r.k AND l.id <> r.id)",
            "SELECT count(*) FROM l, r WHERE (l.k = r.k OR l.id = r.id) AND true",
        ],
    ];
    for queries in same {
        let expected = last(&mut session, queries[0]);
        assert!(
            !expected.ends_with("SELECT 0") || queries[0].contains("IS NULL"),
            "{expected}"
        );
        for sql in &queries[1..] {
            assert_eq!(last(&mut session, sql), expected, "{sql}");
        }
    }
    let plan = last(
        &mut session,
        "EXPLAIN SELECT l.id, h.w FROM l, h WHERE h.k = l.k",
    );
    assert!(plan.contains("Hash Join"), "{plan}");
}

fn parameter(declared_type: Option<crate::SqlType>, value: Option<&str>) -> crate::Parameter {
    crate::Parameter {
        declared_type,
        value: value.map(str::to_owned),
    }
}

fn run_prepared(
    session: &mut Session<'_, MemoryStore>,
    sql: &str,
    parameters: &[crate::Parameter],
) -> Result<Vec<String>, SqlError> {
    let mut outputs = Vec::new();
    session.execute_prepared(sql, parameters, &mut |result| {
        outputs.push(render(&result));
        true
    })?;
    Ok(outputs)
}

#[test]
fn parameters_take_text_values_typed_by_their_place_or_their_declaration() {
    use crate::SqlType;
    let store = MemoryStore::default();
    let mut session = with_people(&store);
    let prepared = session
        .prepare("SELECT name FROM people WHERE id = $1 OR age > $3", &[])
        .unwrap();
    assert_eq!(prepared.parameter_count, 3);
    assert!(!prepared.is_empty);
    assert!(session.prepare(" ; ", &[]).unwrap().is_empty);
    assert_eq!(
        run_prepared(
            &mut session,
            "SELECT name FROM people WHERE id = $1",
            &[parameter(None, Some("2"))],
        )
        .unwrap(),
        ["name\nbob\nSELECT 1"]
    );
    assert_eq!(
        run_prepared(
            &mut session,
            "INSERT INTO people (id, name, age) VALUES ($1, $2, $3)",
            &[
                parameter(Some(SqlType::Integer), Some("9")),
                parameter(None, Some("it's $1")),
                parameter(None, None),
            ],
        )
        .unwrap(),
        ["INSERT 0 1"]
    );
    assert_eq!(
        last(&mut session, "SELECT name, age FROM people WHERE id = 9"),
        "name|age\nit's $1|NULL\nSELECT 1"
    );
    assert_eq!(
        run_prepared(
            &mut session,
            "SELECT $1::int + $2",
            &[
                parameter(None, Some("40")),
                parameter(Some(SqlType::BigInt), Some("2"))
            ],
        )
        .unwrap(),
        ["?column?\n42\nSELECT 1"]
    );
    let error = run_prepared(
        &mut session,
        "SELECT name FROM people WHERE id = $1",
        &[parameter(Some(SqlType::Integer), Some("x"))],
    )
    .unwrap_err();
    assert_eq!(error.state().code(), "22P02");
    let error = run_prepared(&mut session, "SELECT $2", &[parameter(None, Some("1"))]).unwrap_err();
    assert_eq!(error.state().code(), "42P02");
    let error = session.prepare("SELECT 1; SELECT 2", &[]).unwrap_err();
    assert_eq!(error.state().code(), "42601");
    let error = session.prepare("SELECT $0", &[]).unwrap_err();
    assert_eq!(error.state().code(), "42P02");
    assert_eq!(state(&mut session, "SELECT $1"), "42P02");
}

#[test]
fn describe_gives_parameter_types_and_column_types_without_running() {
    use crate::{SqlType, TransactionStatus};
    let store = MemoryStore::default();
    let mut session = with_people(&store);
    let description = session
        .describe(
            "SELECT p.id, upper(name), age + 1, score * 2, id = $2, count(*), sum(age), avg(score), max(name) FROM people p WHERE id > $1 GROUP BY p.id, name, age, score",
            &[Some(SqlType::Integer)],
        )
        .unwrap();
    assert_eq!(description.parameter_types, [Some(SqlType::Integer), None]);
    let types: Vec<Option<SqlType>> = description
        .columns
        .unwrap()
        .iter()
        .map(|column| column.sql_type)
        .collect();
    assert_eq!(
        types,
        [
            Some(SqlType::Integer),
            Some(SqlType::Text),
            Some(SqlType::Integer),
            Some(SqlType::Double),
            Some(SqlType::Boolean),
            Some(SqlType::BigInt),
            Some(SqlType::BigInt),
            Some(SqlType::Double),
            Some(SqlType::Text),
        ]
    );
    assert_eq!(
        last(&mut session, "SELECT count(*) FROM people"),
        "count\n4\nSELECT 1"
    );
    let returning = session
        .describe("DELETE FROM people WHERE id = $1 RETURNING name", &[])
        .unwrap();
    assert_eq!(returning.columns.unwrap()[0].name, "name");
    assert_eq!(
        last(&mut session, "SELECT count(*) FROM people"),
        "count\n4\nSELECT 1"
    );
    assert_eq!(
        session
            .describe("INSERT INTO people (id, name) VALUES (7, 'x')", &[])
            .unwrap()
            .columns,
        None
    );
    assert_eq!(session.describe("BEGIN", &[]).unwrap().columns, None);
    assert_eq!(
        session
            .describe("SELECT * FROM nope", &[])
            .unwrap_err()
            .state()
            .code(),
        "42P01"
    );
    last(&mut session, "BEGIN");
    assert_eq!(session.transaction_status(), TransactionStatus::InBlock);
    assert_eq!(state(&mut session, "SELECT * FROM nope"), "42P01");
    assert_eq!(session.transaction_status(), TransactionStatus::Failed);
    assert_eq!(
        session
            .describe("SELECT 1", &[])
            .unwrap_err()
            .state()
            .code(),
        "25P02"
    );
    assert_eq!(session.describe("ROLLBACK", &[]).unwrap().columns, None);
    last(&mut session, "ROLLBACK");
    assert_eq!(session.transaction_status(), TransactionStatus::Idle);
}

#[test]
fn set_show_and_reset_remember_run_time_parameters() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    session.identify("ada", "shop");
    assert_eq!(
        last(&mut session, "SHOW server_version_num"),
        "server_version_num\n160000\nSHOW"
    );
    assert_eq!(last(&mut session, "SET extra_float_digits = 3"), "SET");
    assert_eq!(
        last(&mut session, "SHOW extra_float_digits"),
        "extra_float_digits\n3\nSHOW"
    );
    assert_eq!(last(&mut session, "SET datestyle TO iso, mdy"), "SET");
    assert_eq!(
        last(&mut session, "SHOW DateStyle"),
        "DateStyle\niso, mdy\nSHOW"
    );
    assert_eq!(last(&mut session, "SET TIME ZONE 'Europe/Paris'"), "SET");
    assert_eq!(session.parameter("timezone").unwrap(), "Europe/Paris");
    assert_eq!(last(&mut session, "SET client_encoding = 'utf-8'"), "SET");
    assert_eq!(
        state(&mut session, "SET client_encoding = 'LATIN1'"),
        "0A000"
    );
    assert_eq!(state(&mut session, "SET server_version = '1'"), "55P02");
    assert_eq!(state(&mut session, "SET no_such_thing = 1"), "42704");
    assert_eq!(state(&mut session, "SHOW no_such_thing"), "42704");
    assert_eq!(
        state(&mut session, "SET LOCAL statement_timeout = 0"),
        "0A000"
    );
    assert_eq!(last(&mut session, "SET myapp.tenant = 'blue'"), "SET");
    assert_eq!(
        last(&mut session, "SHOW myapp.tenant"),
        "myapp.tenant\nblue\nSHOW"
    );
    assert_eq!(
        last(&mut session, "SET statement_timeout TO DEFAULT"),
        "SET"
    );
    assert_eq!(last(&mut session, "RESET ALL"), "RESET");
    assert_eq!(session.parameter("extra_float_digits").unwrap(), "1");
    assert_eq!(session.parameter("session_authorization").unwrap(), "ada");
    assert_eq!(state(&mut session, "SHOW myapp.tenant"), "42704");
    assert!(last(&mut session, "SHOW ALL").lines().count() > 20);
    let reported: Vec<&str> = session
        .reported_parameters()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    for name in [
        "server_version",
        "client_encoding",
        "DateStyle",
        "TimeZone",
        "integer_datetimes",
        "standard_conforming_strings",
    ] {
        assert!(reported.contains(&name), "{name}");
    }
}

#[test]
fn session_functions_report_the_user_the_database_and_the_version() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    session.identify("ada", "shop");
    assert_eq!(
        last(
            &mut session,
            "SELECT current_user, session_user, user, current_database(), current_schema()"
        ),
        "current_user|session_user|current_user|current_database|current_schema\nada|ada|ada|shop|public\nSELECT 1"
    );
    let version = last(&mut session, "SELECT version()");
    assert!(
        version.starts_with("version\nPostgreSQL 16.0 compatible (EggDB "),
        "{version}"
    );
    assert_eq!(
        last(&mut session, "SELECT current_setting('server_encoding')"),
        "current_setting\nUTF8\nSELECT 1"
    );
}

#[test]
fn coalesce_has_the_common_type_of_its_arguments() {
    use crate::SqlType;
    let store = MemoryStore::default();
    let mut session = with_people(&store);
    let description = session
        .describe(
            "SELECT coalesce(age, 0::smallint), coalesce(id, score), coalesce(name, NULL), coalesce(name, active) FROM people",
            &[],
        )
        .unwrap();
    let types: Vec<Option<SqlType>> = description
        .columns
        .unwrap()
        .iter()
        .map(|column| column.sql_type)
        .collect();
    assert_eq!(
        types,
        [
            Some(SqlType::SmallInt),
            Some(SqlType::Double),
            Some(SqlType::Text),
            None
        ]
    );
}

#[test]
fn hostile_sqlite_input_returns_errors_and_never_panics() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, crate::Dialect::Sqlite);
    last(
        &mut session,
        "CREATE TABLE people (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, age INT, score REAL);
         INSERT INTO people (name, age, score) VALUES ('alice', 30, 9.25), ('bob', NULL, NULL);",
    );
    let fragments = [
        "SELECT",
        "INSERT INTO people VALUES",
        "INSERT OR REPLACE INTO people",
        "INSERT OR IGNORE INTO people",
        "INSERT OR ROLLBACK INTO people",
        "REPLACE INTO people",
        "ON CONFLICT",
        "ON CONFLICT(id) DO UPDATE SET age = excluded.age",
        "DO NOTHING",
        "excluded.",
        "PRAGMA",
        "PRAGMA table_info(",
        "PRAGMA user_version =",
        "PRAGMA table_list",
        "sqlite_master",
        "sqlite_schema",
        "FROM sqlite_schema",
        "WITHOUT ROWID",
        "STRICT",
        "CREATE TABLE t (",
        "INTEGER PRIMARY KEY",
        "AUTOINCREMENT",
        "REFERENCES people(id)",
        "BEGIN IMMEDIATE",
        "END",
        "COMMIT",
        "ROLLBACK",
        "(",
        ")",
        ",",
        ";",
        "'",
        "\"",
        "`",
        "[",
        "]",
        "[a b]",
        "\"x\"\"y\"",
        "/*",
        "*/",
        "--",
        "\n",
        "?",
        "?1",
        "?32767",
        "?0",
        ":name",
        "@v",
        "$x",
        ":",
        "$",
        "x'00'",
        "||",
        "==",
        "!=",
        "<>",
        "IS",
        "IS NOT",
        "GLOB",
        "'a*'",
        "'[a]'",
        "LIKE",
        "LIMIT",
        "LIMIT -1",
        "OFFSET",
        "1, 2",
        "WHERE",
        "NOT",
        "1",
        "-9223372036854775808",
        "1e999",
        "0.5",
        "NULL",
        "TRUE",
        "printf('%",
        "printf('%*.*f', 1000000, 99999, 1.5)",
        "printf('%s%d%q%Q%c%x%e%g'",
        "substr('abc', -9223372036854775808, -9223372036854775808)",
        "round(1e308, 30)",
        "iif(",
        "typeof(",
        "quote(",
        "hex(",
        "instr(",
        "max(1, 'a', NULL)",
        "last_insert_rowid()",
        "changes()",
        "id",
        "name",
        "people",
        "*",
        "/",
        "%",
        "0",
    ];
    fuzz(&mut session, &fragments);
    let error = run(
        &mut session,
        "SELECT replace(replace(printf('%1000s', ''), ' ', printf('%1000s', '')), ' ', printf('%1000s', '') || printf('%1000s', ''))",
    )
    .expect_err("about 2 GB");
    assert_eq!(error.to_string(), "Error: string or blob too big");
    assert_eq!(error.vendor_code(), Some(18));
}

#[test]
fn sqlite_functions_stay_inside_the_sqlite_dialect() {
    let store = MemoryStore::default();
    let mut postgresql = Session::new(&store);
    let error = run(&mut postgresql, "SELECT sqlite_upper('abc')").expect_err("postgresql");
    assert_eq!(
        error.message(),
        "function sqlite_upper(text) does not exist"
    );
    let mut mysql = Session::with_dialect(&store, crate::Dialect::MySql);
    let error = run(&mut mysql, "SELECT sqlite_typeof(1)").expect_err("mysql");
    assert_eq!(
        error.message(),
        "FUNCTION eggdb.sqlite_typeof does not exist"
    );
    let mut sqlite = Session::with_dialect(&store, crate::Dialect::Sqlite);
    for sql in ["SELECT sqlite_upper('abc')", "SELECT sqlite_truth(1)"] {
        let error = run(&mut sqlite, sql).expect_err(sql);
        assert!(
            error.message().starts_with("no such function: sqlite_"),
            "{sql}: {error}"
        );
    }
    assert_eq!(
        last(&mut sqlite, "SELECT upper('abc')"),
        "upper('abc')\nABC\nSELECT 1"
    );
}

#[test]
fn sqlite_parameters_number_like_sqlite_and_unbound_ones_are_null() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, crate::Dialect::Sqlite);
    let prepared = session
        .prepare("SELECT ?, ?5, :a, @b, :a, ?", &[])
        .expect("prepares");
    assert_eq!(prepared.parameter_count, 8);
    let parameters: Vec<crate::Parameter> = ["one", "two", "three"]
        .into_iter()
        .map(|value| crate::Parameter {
            declared_type: None,
            value: Some(value.to_owned()),
        })
        .collect();
    let mut rows = Vec::new();
    session
        .execute_prepared("SELECT ?, ?2, ?3, ?4", &parameters, &mut |result| {
            rows.extend(result.rows);
            true
        })
        .expect("runs");
    assert_eq!(
        rows,
        vec![vec![
            crate::Value::Text("one".into()),
            crate::Value::Text("two".into()),
            crate::Value::Text("three".into()),
            crate::Value::Null,
        ]]
    );
    let error = session
        .prepare("SELECT ?40000", &[])
        .expect_err("out of range");
    assert_eq!(
        error.to_string(),
        "Error: variable number must be between ?1 and ?32766"
    );
}

#[test]
fn sqlite_errors_print_in_sqlite_style_with_result_codes() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, crate::Dialect::Sqlite);
    last(
        &mut session,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, c TEXT NOT NULL)",
    );
    let cases = [
        ("SELECT * FORM t", "near \"FORM\": syntax error", 1),
        ("SELECT * FROM nowhere", "no such table: nowhere", 1),
        ("CREATE TABLE t (x)", "table t already exists", 1),
        (
            "INSERT INTO t (id) VALUES (1)",
            "NOT NULL constraint failed: t.c",
            1299,
        ),
        (
            "INSERT INTO t (nope) VALUES (1)",
            "table t has no column named nope",
            1,
        ),
    ];
    for (sql, message, code) in cases {
        let error = run(&mut session, sql).expect_err(sql);
        assert_eq!(error.to_string(), format!("Error: {message}"), "{sql}");
        assert_eq!(error.vendor_code(), Some(code), "{sql}");
        assert_eq!(error.vendor(), Some(crate::Dialect::Sqlite), "{sql}");
    }
    last(&mut session, "INSERT INTO t VALUES (1, 'a')");
    let error = run(&mut session, "INSERT INTO t VALUES (1, 'b')").expect_err("duplicate");
    assert_eq!(error.message(), "UNIQUE constraint failed: t.id");
    assert_eq!(
        error.vendor_code().and_then(crate::sqlite_result_code_name),
        Some("SQLITE_CONSTRAINT_PRIMARYKEY")
    );
}

#[test]
fn hostile_oracle_input_returns_errors_and_never_panics() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, crate::Dialect::Oracle);
    last(
        &mut session,
        "CREATE TABLE people (id NUMBER GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, name VARCHAR2(20) NOT NULL, age NUMBER(3), score BINARY_DOUBLE);
         INSERT INTO people (name, age, score) VALUES ('alice', 30, 9.25);
         INSERT INTO people (name, age, score) VALUES ('bob', NULL, NULL);
         COMMIT",
    );
    let fragments = [
        "SELECT",
        "FROM dual",
        "FROM DUAL d",
        "FROM people",
        "FROM user_tables",
        "FROM all_tab_columns",
        "INSERT INTO people VALUES",
        "INSERT INTO people (name) VALUES ('x')",
        "UPDATE people SET age =",
        "DELETE people",
        "TRUNCATE TABLE people",
        "DROP TABLE people CASCADE CONSTRAINTS PURGE",
        "CREATE TABLE t (",
        "VARCHAR2(10 CHAR)",
        "VARCHAR2(0)",
        "NUMBER(*,0)",
        "NUMBER(39)",
        "NUMBER(5,-2)",
        "CLOB",
        "GENERATED ALWAYS AS IDENTITY",
        "WHERE ROWNUM <=",
        "ROWNUM",
        "ROWNUM = 1",
        "ROWNUM > -9223372036854775808",
        "FETCH FIRST",
        "FETCH NEXT 9223372036854775807 ROWS ONLY",
        "OFFSET -1 ROWS",
        "ROWS ONLY",
        "ORDER BY",
        "GROUP BY",
        "MINUS",
        "CONNECT BY",
        "COMMIT",
        "ROLLBACK",
        "BEGIN",
        "MERGE INTO",
        "seq.NEXTVAL",
        "(",
        ")",
        ",",
        ";",
        "'",
        "''",
        "\"",
        "\"\"",
        "\"Name\"",
        "q'[",
        "q'<x>'",
        "nq'{y}'",
        "/*",
        "*/",
        "--",
        "\n",
        ":1",
        ":name",
        ":",
        "||",
        "^=",
        "~=",
        "!=",
        "=>",
        "@",
        "LIKE",
        "LIMIT 1",
        "NULL",
        "1",
        "-9223372036854775808",
        "1e999",
        "0.5",
        "USER",
        "SYSDATE",
        "NVL(",
        "NVL2(NULL, 1, 2)",
        "DECODE(1, 1, 'a', NULL)",
        "TO_CHAR(1234.5, 'FM9,999.99')",
        "TO_CHAR(-1e300, '999999999999999999999999999999999999999.99999')",
        "TO_NUMBER('x')",
        "SUBSTR('abc', -9223372036854775808, -9223372036854775808)",
        "INSTR('abc', 'b', -9223372036854775808, 9223372036854775807)",
        "INSTR('abc', 'bc', 9223372036854775807)",
        "ROUND(15.5, -9223372036854775808)",
        "WHERE ROWNUM <= :1",
        "DECODE(COALESCE(1, 1, 1, 1, 1, 1, 1, 1), 1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2)",
        "DECODE(1, 2",
        "UPPER(RPAD('ß', 200000000, 'ß'))",
        "LPAD('x', 9223372036854775807, 'ab')",
        "RPAD('x', 1e300)",
        "REPLACE('aaaa', 'a', 'bbbbbbbbbbbbbbbb')",
        "TRIM(LEADING 'ab' FROM 'abc')",
        "ROUND(1e308, -400)",
        "TRUNC(-9223372036854775808, -19)",
        "MOD(-9223372036854775808, -1)",
        "POWER(-2, 0.5)",
        "POWER(10, 400)",
        "ABS(-9223372036854775808)",
        "-9223372036854775808 / -1",
        "CHR(1114112)",
        "ASCII('é')",
        "GREATEST('a', 1)",
        "LEAST(1, 'a')",
        "id",
        "name",
        "people",
        "*",
        "/",
        "0",
    ];
    fuzz(&mut session, &fragments);
    let error = run(
        &mut session,
        "SELECT REPLACE(RPAD('x', 30000, 'x'), 'x', RPAD('y', 40000, 'y')) FROM dual",
    )
    .expect_err("about 1.2 GB");
    assert_eq!(
        error.to_string(),
        "ORA-01489: result of string concatenation is too long"
    );
}

#[test]
fn oracle_functions_stay_inside_the_oracle_dialect() {
    let store = MemoryStore::default();
    let mut postgresql = Session::new(&store);
    let error = run(&mut postgresql, "SELECT nvl(1, 2)").expect_err("postgresql");
    assert_eq!(
        error.message(),
        "function nvl(integer, integer) does not exist"
    );
    let error = run(&mut postgresql, "SELECT oracle_concat('a', 'b')").expect_err("internal");
    assert!(
        error.message().starts_with("function oracle_concat("),
        "{error}"
    );
    let mut mysql = Session::with_dialect(&store, crate::Dialect::MySql);
    let error = run(&mut mysql, "SELECT nvl2(1, 2, 3)").expect_err("mysql");
    assert_eq!(error.message(), "FUNCTION eggdb.nvl2 does not exist");
    let mut sqlite = Session::with_dialect(&store, crate::Dialect::Sqlite);
    let error = run(&mut sqlite, "SELECT initcap('abc')").expect_err("sqlite");
    assert_eq!(error.message(), "no such function: initcap");
    let mut oracle = Session::with_dialect(&store, crate::Dialect::Oracle);
    for sql in [
        "SELECT oracle_concat('a', 'b') FROM dual",
        "SELECT sqlite_upper('a') FROM dual",
        "SELECT char_length('a') FROM dual",
        "SELECT version() FROM dual",
    ] {
        let error = run(&mut oracle, sql).expect_err(sql);
        assert_eq!(error.vendor_code(), Some(904), "{sql}: {error}");
    }
}

#[test]
fn oracle_names_fold_to_upper_case_on_the_tables_of_every_dialect() {
    let store = MemoryStore::default();
    let mut postgresql = Session::new(&store);
    last(
        &mut postgresql,
        "CREATE TABLE shared (id integer PRIMARY KEY, label text); INSERT INTO shared VALUES (1, 'pg')",
    );
    let mut oracle = Session::with_dialect(&store, crate::Dialect::Oracle);
    assert_eq!(
        last(&mut oracle, "SELECT ID, \"LABEL\" FROM Shared"),
        "ID|LABEL\n1|pg\nSELECT 1"
    );
    last(
        &mut oracle,
        "CREATE TABLE \"Mixed\" (\"Key\" NUMBER(9) PRIMARY KEY); INSERT INTO shared VALUES (2, ''); COMMIT",
    );
    assert_eq!(
        last(&mut postgresql, "SELECT id FROM shared WHERE label IS NULL"),
        "id\n2\nSELECT 1"
    );
    assert_eq!(
        last(&mut postgresql, "SELECT \"Key\" FROM \"Mixed\""),
        "Key\nSELECT 0"
    );
    let tables = oracle.tables().expect("tables");
    let created: Vec<String> = tables
        .iter()
        .map(|table| table.create_statement_in(crate::Dialect::Oracle))
        .collect();
    assert_eq!(
        created,
        [
            "CREATE TABLE \"Mixed\" (\"Key\" NUMBER(10) PRIMARY KEY);",
            "CREATE TABLE SHARED (ID NUMBER(10) PRIMARY KEY, LABEL CLOB);",
        ]
    );
}

#[test]
fn oracle_keeps_a_transaction_open_until_commit_and_ddl_commits_it() {
    let store = MemoryStore::default();
    let mut oracle = Session::with_dialect(&store, crate::Dialect::Oracle);
    assert!(!oracle.autocommit());
    last(&mut oracle, "CREATE TABLE t (id NUMBER(9) PRIMARY KEY)");
    assert!(!oracle.in_block());
    last(&mut oracle, "INSERT INTO t VALUES (1)");
    assert!(oracle.in_block());
    let mut other = Session::with_dialect(&store, crate::Dialect::Oracle);
    let error = run(&mut other, "INSERT INTO t VALUES (2)").expect_err("one writer");
    assert_eq!(error.vendor_code(), Some(54), "{error}");
    last(&mut oracle, "CREATE TABLE u (id NUMBER(9) PRIMARY KEY)");
    assert!(!oracle.in_block());
    assert_eq!(
        last(&mut other, "SELECT COUNT(*) FROM t"),
        "COUNT(*)\n1\nSELECT 1"
    );
    let prepared = oracle
        .describe("SELECT id AS \"Id\", id FROM t WHERE id = :1", &[])
        .expect("describe");
    let names: Vec<String> = prepared
        .columns
        .unwrap_or_default()
        .into_iter()
        .map(|column| column.name)
        .collect();
    assert_eq!(names, ["Id", "ID"]);
}

#[test]
fn a_large_oracle_decode_stays_linear_and_obeys_the_argument_limit() {
    let store = MemoryStore::default();
    let mut oracle = Session::with_dialect(&store, crate::Dialect::Oracle);
    let ones = vec!["1"; 2000].join(", ");
    let pairs = vec!["1, 2"; 126].join(", ");
    let sql = format!("SELECT DECODE(COALESCE({ones}), {pairs}, 0) d FROM dual");
    let started = std::time::Instant::now();
    assert_eq!(last(&mut oracle, &sql), "D\n2\nSELECT 1");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    let too_many = format!(
        "SELECT DECODE(1, {}) FROM dual",
        vec!["1, 2"; 128].join(", ")
    );
    let error = run(&mut oracle, &too_many).expect_err("256 arguments");
    assert_eq!(
        error.to_string(),
        "ORA-00939: too many arguments for function"
    );
    assert_eq!(
        last(
            &mut oracle,
            "SELECT DECODE(0, 0, 'zero', 1 / 0) d FROM dual"
        ),
        "D\nzero\nSELECT 1"
    );
}

#[test]
fn rows_kept_for_a_rownum_limit_are_charged_to_the_work_memory() {
    let rows = (0..1000).map(|number| vec![crate::Value::Text(format!("row {number:>40}"))]);
    let mut memory = crate::memory::WorkMemory::with_limit(10_000);
    let error = crate::execute::first_rows(
        |visit| {
            for row in rows {
                if !visit(row)? {
                    break;
                }
            }
            Ok(())
        },
        usize::MAX,
        &mut memory,
    )
    .expect_err("past the limit");
    assert_eq!(error.state().code(), "53200");
    let mut memory = crate::memory::WorkMemory::with_limit(10_000);
    let kept = crate::execute::first_rows(
        |visit| {
            for number in 0.. {
                if !visit(vec![crate::Value::BigInt(number)])? {
                    break;
                }
            }
            Ok(())
        },
        3,
        &mut memory,
    )
    .expect("three rows");
    assert_eq!(kept.len(), 3);
}

#[test]
fn hostile_typed_statements_never_panic_in_any_dialect() {
    let shared = [
        "SELECT",
        "INSERT INTO typed VALUES (",
        "UPDATE typed SET n =",
        "WHERE",
        "FROM typed",
        "ORDER BY",
        "n",
        "d",
        "t",
        "z",
        "j",
        "(",
        ")",
        ",",
        "+",
        "-",
        "*",
        "/",
        "%",
        "=",
        "<",
        "1",
        "0",
        "-1",
        "9.99",
        "1e999",
        "123456789012345678901234567890.123456789",
        "'NaN'",
        "'2024-02-29'",
        "'2024-02-30'",
        "'0044-03-15 BC'",
        "'294276-12-31 23:59:59.999999'",
        "'5874897-12-31'",
        "'2024-02-29 13:45:01.1234567+05:30'",
        "'{\"a\": [1, {\"b\": 2}]}'",
        "'[[[[[[[['",
        "'{a,b}'",
        "sum(n)",
        "avg(n)",
        "max(d)",
        "round(",
        "trunc(",
        "ceil(",
        "floor(",
        "abs(",
        "extract(year from",
        "extract(epoch from",
        "CAST(",
        "AS",
        "DATE",
        "TIMESTAMP",
        "CREATE TABLE x (a",
        "PRIMARY KEY",
        "NOT NULL",
        "DEFAULT",
        "CREATE INDEX ON typed (n)",
        "CREATE INDEX typed_d ON typed (d, t)",
        "EXTRACT(",
        "YEAR_MONTH",
        "EPOCH",
        "WEEK",
        "HOUR",
        "SECOND",
        "FROM",
        "^ 1e400",
        "0.5 ^",
        "^ 99999999999999999999999",
        "'0050-01-01 BC'",
        "'5874897-12-31'",
        "'4714-11-24 BC'",
        "date_trunc('century',",
        "TRUNC(",
        "localtimestamp",
        "localtime",
        "current_date",
    ];
    let per_dialect: [(crate::Dialect, &str, &[&str]); 4] = [
        (
            crate::Dialect::PostgreSql,
            "CREATE TABLE typed (id serial PRIMARY KEY, n numeric(12,3), d date, t timestamp(3), z timestamptz, j jsonb)",
            &[
                "numeric(",
                "numeric(1000,",
                "decimal",
                "timestamptz",
                "with time zone",
                "json",
                "jsonb",
                "serial",
                "bigserial",
                "smallserial",
                "GENERATED ALWAYS AS IDENTITY",
                "GENERATED BY DEFAULT AS IDENTITY",
                "::numeric",
                "::numeric(3,1)",
                "::date",
                "::timestamptz",
                "::jsonb",
                "::json",
                "->",
                "->>",
                "#>",
                "#>>",
                "@>",
                "<@",
                "^",
                "now()",
                "current_date",
                "localtimestamp",
                "date_trunc('month',",
                "date_part('dow',",
                "jsonb_typeof(",
                "json_array_length(",
                "SET TimeZone = 'UTC+05:30'",
            ],
        ),
        (
            crate::Dialect::MySql,
            "CREATE TABLE typed (id INT AUTO_INCREMENT PRIMARY KEY, n DECIMAL(12,3), d DATE, t DATETIME(3), z TIMESTAMP, j JSON)",
            &[
                "DECIMAL(",
                "DECIMAL(66,",
                "DATETIME",
                "DATETIME(7)",
                "JSON",
                "NOW()",
                "CURDATE()",
                "UTC_TIMESTAMP()",
                "CURRENT_TIMESTAMP",
                "UNSIGNED",
            ],
        ),
        (
            crate::Dialect::Sqlite,
            "CREATE TABLE typed (id INTEGER PRIMARY KEY, n NUMERIC, d DATE, t DATETIME, z TIMESTAMP, j JSON)",
            &["NUMERIC", "DATETIME", "date('now')", "typeof("],
        ),
        (
            crate::Dialect::Oracle,
            "CREATE TABLE typed (id NUMBER GENERATED BY DEFAULT ON NULL AS IDENTITY PRIMARY KEY, n NUMBER(12,3), d DATE, t TIMESTAMP(3), z TIMESTAMP WITH TIME ZONE, j JSON)",
            &[
                "NUMBER",
                "NUMBER(38,",
                "NUMBER(*,",
                "TIMESTAMP(9)",
                "WITH LOCAL TIME ZONE",
                "SYSDATE",
                "SYSTIMESTAMP",
                "CURRENT_DATE",
                "FROM dual",
            ],
        ),
    ];
    for (dialect, create, extra) in per_dialect {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, dialect);
        run(&mut session, create).unwrap_or_else(|error| panic!("{dialect:?}: {error}"));
        let fragments: Vec<&str> = shared.iter().chain(extra.iter()).copied().collect();
        fuzz(&mut session, &fragments);
    }
}

#[test]
fn the_new_types_functions_stay_in_their_dialects() {
    let cases: [(crate::Dialect, &str); 8] = [
        (crate::Dialect::Sqlite, "SELECT now()"),
        (crate::Dialect::Sqlite, "SELECT jsonb_typeof('[]')"),
        (crate::Dialect::MySql, "SELECT date_trunc('day', NOW())"),
        (crate::Dialect::MySql, "SELECT '{\"a\": 1}' ->> 'a'"),
        (crate::Dialect::PostgreSql, "SELECT mysql_now()"),
        (crate::Dialect::PostgreSql, "SELECT concat_or_null('a')"),
        (crate::Dialect::Oracle, "SELECT NOW() FROM dual"),
        (
            crate::Dialect::Sqlite,
            "CREATE TABLE s (id serial PRIMARY KEY, n numeric(5,2))",
        ),
    ];
    for (dialect, sql) in cases {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, dialect);
        let outcome = run(&mut session, sql);
        if dialect == crate::Dialect::Sqlite && sql.starts_with("CREATE") {
            assert!(outcome.is_ok(), "{dialect:?} {sql}: {outcome:?}");
            assert_eq!(
                last(&mut session, "SELECT typeof(id) FROM s"),
                "typeof(id)\nSELECT 0"
            );
            continue;
        }
        assert!(outcome.is_err(), "{dialect:?} {sql}: {outcome:?}");
    }
}

#[test]
fn dates_at_the_ends_of_their_range_never_overflow() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT extract(year FROM DATE '5874897-12-31'), date_part('doy', DATE '5874897-12-31'), extract(epoch FROM DATE '4714-11-24 BC'), DATE '5874897-12-30' + 1, DATE '4714-11-25 BC' - 1"
        ),
        "extract|date_part|extract|?column?|?column?\n5874897|365|-210866803200|5874897-12-31|4714-11-24 BC\nSELECT 1"
    );
    for (sql, state) in [
        ("SELECT date_trunc('day', DATE '5874897-12-31')", "22008"),
        ("SELECT DATE '5874897-12-31' + 1", "22008"),
        ("SELECT DATE '4714-11-24 BC' - 1", "22008"),
        ("SELECT DATE '5874897-12-31'::timestamp", "22008"),
        ("SELECT DATE '5874897-12-31'::timestamptz", "22008"),
        ("SELECT DATE '2000-01-01' + 9223372036854775807", "22008"),
        (
            "SELECT DATE '2000-01-01' - (-9223372036854775807 - 1)",
            "22008",
        ),
        (
            "SELECT DATE '5874897-12-31' - DATE '4714-11-24 BC'",
            "00000",
        ),
        ("SELECT 0.5 ^ 99999999999999999999999", "00000"),
        ("SELECT 10::numeric ^ 400.5", "00000"),
        ("SELECT 1::numeric ^ 1e400", "00000"),
    ] {
        let found = run(&mut session, sql).map_or_else(
            |error| error.state().code().to_owned(),
            |_| "00000".to_owned(),
        );
        assert_eq!(found, state, "{sql}");
    }
    assert_eq!(
        last(&mut session, "SELECT 0.5 ^ 1e400 = 0, 1::numeric ^ 1e400"),
        "?column?|?column?\nt|1.0000000000000000\nSELECT 1"
    );
}

#[test]
fn equal_values_of_different_types_hash_alike() {
    use crate::types::{Value, compare_values, equality_key};
    let pairs = [
        ("2.5", 2.5f64),
        ("-0.125", -0.125),
        ("100000000000000000000", 1e20),
        ("3", 3.0),
    ];
    for (text, double) in pairs {
        let numeric = Value::Numeric(crate::numeric::Numeric::parse(text).unwrap());
        let float = Value::Double(double);
        assert_eq!(compare_values(&numeric, &float), std::cmp::Ordering::Equal);
        let (mut left, mut right) = (Vec::new(), Vec::new());
        equality_key(&mut left, &numeric);
        equality_key(&mut right, &float);
        assert_eq!(left, right, "{text}");
    }
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE m (id integer PRIMARY KEY, n numeric, d double precision, j jsonb UNIQUE);
         INSERT INTO m VALUES (1, 2.5, NULL, '{\"a\": 1.0}'), (2, NULL, 2.5, '[2.50]'), (3, 2.50, NULL, NULL)",
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT count(DISTINCT coalesce(n, d)), '{\"a\": 1.0}'::jsonb = '{\"a\": 1}'::jsonb, '[1.10]'::jsonb > '[1.1]'::jsonb FROM m"
        ),
        "count|?column?|?column?\n1|t|f\nSELECT 1"
    );
    assert_eq!(
        fails(
            &mut session,
            "INSERT INTO m (id, j) VALUES (4, '{\"a\": 1}')"
        )
        .state()
        .code(),
        "23505"
    );
    assert_eq!(
        last(&mut session, "SELECT j FROM m WHERE j = '[2.5]'"),
        "j\n[2.50]\nSELECT 1"
    );
}

#[test]
fn extract_takes_each_dialects_own_units() {
    let store = MemoryStore::default();
    let mut mysql = Session::with_dialect(&store, crate::Dialect::MySql);
    assert_eq!(
        last(
            &mut mysql,
            "SELECT EXTRACT(WEEK FROM DATE '2024-01-01') AS w, EXTRACT(SECOND FROM TIMESTAMP '2024-02-29 13:45:01.5') AS s, EXTRACT(YEAR_MONTH FROM DATE '2024-02-29') AS ym, EXTRACT(HOUR FROM DATE '2024-02-29') AS h"
        ),
        "w|s|ym|h\n0|1|202402|0\nSELECT 1"
    );
    for unit in ["EPOCH", "DOW", "ISODOW", "DOY", "CENTURY", "TIMEZONE"] {
        let error = fails(&mut mysql, &format!("SELECT EXTRACT({unit} FROM NOW())"));
        assert_eq!(error.vendor_code(), Some(1064), "{unit}: {error}");
    }
    assert_eq!(
        fails(&mut mysql, "SELECT TRUNC(1.5)").vendor_code(),
        Some(1305)
    );
    let mut oracle = Session::with_dialect(&store, crate::Dialect::Oracle);
    assert_eq!(
        last(
            &mut oracle,
            "SELECT EXTRACT(YEAR FROM DATE '2024-02-29') y, EXTRACT(SECOND FROM TIMESTAMP '2024-02-29 13:45:01.5') s FROM dual"
        ),
        "Y|S\n2024|1.5\nSELECT 1"
    );
    for (sql, code) in [
        (
            "SELECT EXTRACT(HOUR FROM DATE '2024-02-29') FROM dual",
            30076,
        ),
        ("SELECT EXTRACT(HOUR FROM SYSDATE) FROM dual", 30076),
        (
            "SELECT EXTRACT(TIMEZONE_HOUR FROM TIMESTAMP '2024-02-29 10:00:00') FROM dual",
            30076,
        ),
        ("SELECT EXTRACT(EPOCH FROM SYSDATE) FROM dual", 907),
        ("SELECT EXTRACT(DOW FROM SYSDATE) FROM dual", 907),
    ] {
        assert_eq!(fails(&mut oracle, sql).vendor_code(), Some(code), "{sql}");
    }
    let mut sqlite = Session::with_dialect(&store, crate::Dialect::Sqlite);
    for sql in [
        "SELECT localtimestamp",
        "SELECT localtime",
        "SELECT trunc(1.5)",
        "SELECT floor(1.5)",
    ] {
        let error = fails(&mut sqlite, sql);
        assert!(error.message().starts_with("no such"), "{sql}: {error}");
    }
    let mut postgresql = Session::new(&store);
    assert_eq!(
        last(
            &mut postgresql,
            "SELECT date_trunc('century', TIMESTAMP '0050-01-01 BC'), date_trunc('millennium', DATE '0100-01-01 BC')"
        ),
        "date_trunc|date_trunc\n0100-01-01 00:00:00 BC|1000-01-01 00:00:00+00 BC\nSELECT 1"
    );
}

#[test]
fn hostile_item_31_statements_never_panic_in_any_dialect() {
    let shared = [
        "SELECT",
        "INSERT INTO more VALUES (",
        "UPDATE more SET",
        "WHERE",
        "FROM more",
        "ORDER BY",
        "GROUP BY",
        "i",
        "c",
        "b",
        "t",
        "(",
        ")",
        ",",
        "+",
        "-",
        "*",
        "/",
        "||",
        "=",
        "<",
        "1",
        "-1",
        "1e308",
        "-1e308",
        "'NaN'",
        "'Infinity'",
        "9223372036854775807",
        "2147483647",
        "'1 day'",
        "'-2147483648 days'",
        "'2147483647 mons'",
        "'178956970 years 7 mons'",
        "'-9223372036854775808 microseconds'",
        "'P1Y2M3DT4H5M6.5S'",
        "'1-2'",
        "'1 2:03:04'",
        "'24:00:00'",
        "'23:59:59.9999999'",
        "'25:00'",
        "'10:00+15:59'",
        "'\\x0aff'",
        "'\\x0'",
        "'\\377\\\\'",
        "'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'",
        "'{a0eebc999c0b4ef8bb6d6bb9bd380a11}'",
        "'ab  '",
        "length(",
        "octet_length(",
        "substring(",
        "CAST(",
        "AS",
        "TIME",
        "CHAR(3)",
        "CREATE INDEX ON more (i)",
        "CREATE INDEX ON more (c, b)",
        "max(i)",
        "min(c)",
        "count(DISTINCT i)",
    ];
    let per_dialect: [(crate::Dialect, &str, &[&str]); 4] = [
        (
            crate::Dialect::PostgreSql,
            "CREATE TABLE more (id serial PRIMARY KEY, i interval, c char(4), b bytea, t time(3), z timetz, u uuid)",
            &[
                "interval",
                "interval(2)",
                "interval year to month",
                "interval day to second(3)",
                "YEAR",
                "DAY TO SECOND",
                "MONTH",
                "TO",
                "::interval",
                "::time",
                "::timetz",
                "::bytea",
                "::uuid",
                "::bpchar",
                "::char(2)",
                "::text",
                "time '",
                "timetz",
                "bytea",
                "uuid",
                "bpchar",
                "character(",
                "justify_days(",
                "justify_hours(",
                "justify_interval(",
                "age(",
                "gen_random_uuid()",
                "encode(",
                "decode(",
                "get_byte(",
                "'hex'",
                "'base64'",
                "'escape'",
                "'aGVsbG8='",
                "extract(epoch from",
                "date_part('hour',",
                "current_time",
                "localtime",
                "now()",
                "TIMESTAMP '2024-01-31'",
                "DATE '2024-02-29'",
                "FROM",
                "FOR",
                "SET TimeZone = 'UTC+05:30'",
            ],
        ),
        (
            crate::Dialect::MySql,
            "CREATE TABLE more (id INT AUTO_INCREMENT PRIMARY KEY, i TIME, c CHAR(4), b BLOB, t TIME(3), z VARBINARY(8), u BINARY(16))",
            &[
                "TIME(7)",
                "BLOB",
                "VARBINARY(",
                "BINARY(",
                "LONGBLOB",
                "LENGTH(",
                "TIME '10:00'",
                "INTERVAL '1' DAY",
            ],
        ),
        (
            crate::Dialect::Sqlite,
            "CREATE TABLE more (id INTEGER PRIMARY KEY, i TEXT, c CHARACTER(4), b TEXT, t TIME, z TEXT, u TEXT)",
            &["BLOB", "typeof(", "substr(", "hex("],
        ),
        (
            crate::Dialect::Oracle,
            "CREATE TABLE more (id NUMBER PRIMARY KEY, i NUMBER, c CHAR(4), b RAW(16), t BLOB, z LONG RAW, u NCHAR(3))",
            &[
                "RAW(",
                "BLOB",
                "LONG RAW",
                "NCHAR(",
                "FROM dual",
                "INTERVAL",
            ],
        ),
    ];
    for (dialect, create, extra) in per_dialect {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, dialect);
        run(&mut session, create).unwrap_or_else(|error| panic!("{dialect:?}: {error}"));
        let fragments: Vec<&str> = shared.iter().chain(extra.iter()).copied().collect();
        fuzz(&mut session, &fragments);
    }
}

#[test]
fn item_31_types_and_functions_stay_in_their_dialects() {
    let refused: [(crate::Dialect, &str); 16] = [
        (crate::Dialect::MySql, "SELECT mysql_curtime()"),
        (crate::Dialect::PostgreSql, "SELECT mysql_curtime()"),
        (crate::Dialect::MySql, "SELECT encode('a', 'hex')"),
        (crate::Dialect::MySql, "SELECT gen_random_uuid()"),
        (crate::Dialect::MySql, "SELECT justify_days(1)"),
        (crate::Dialect::MySql, "SELECT CAST('a' AS uuid)"),
        (crate::Dialect::MySql, "SELECT interval '1 day'"),
        (crate::Dialect::Sqlite, "SELECT get_byte('a', 0)"),
        (crate::Dialect::Sqlite, "SELECT age('2024-01-01')"),
        (crate::Dialect::Sqlite, "SELECT time '10:00'"),
        (crate::Dialect::Sqlite, "CREATE TABLE b (x BLOB)"),
        (crate::Dialect::Oracle, "SELECT gen_random_uuid() FROM dual"),
        (crate::Dialect::Oracle, "SELECT time '10:00' FROM dual"),
        (crate::Dialect::Oracle, "SELECT bytea 'x' FROM dual"),
        (
            crate::Dialect::Oracle,
            "CREATE TABLE i (x INTERVAL DAY TO SECOND)",
        ),
        (crate::Dialect::PostgreSql, "CREATE TABLE r (x raw(4))"),
    ];
    for (dialect, sql) in refused {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, dialect);
        let outcome = run(&mut session, sql);
        assert!(outcome.is_err(), "{dialect:?} {sql}: {outcome:?}");
    }
    let store = MemoryStore::default();
    let mut mysql = Session::with_dialect(&store, crate::Dialect::MySql);
    run(
        &mut mysql,
        "CREATE TABLE m (id INT PRIMARY KEY, t TIME, b BLOB, v VARBINARY(4))",
    )
    .unwrap();
    run(
        &mut mysql,
        "INSERT INTO m VALUES (1, '10:30:15', 'abc', 'x')",
    )
    .unwrap();
    assert_eq!(
        last(&mut mysql, "SELECT t, b, LENGTH(b), v FROM m"),
        "t|b|LENGTH(b)|v\n10:30:15|\\x616263|3|\\x78\nSELECT 1"
    );
    assert!(run(&mut mysql, "CREATE TABLE n (v VARBINARY)").is_err());
    let mut oracle = Session::with_dialect(&store, crate::Dialect::Oracle);
    run(
        &mut oracle,
        "CREATE TABLE o (id NUMBER PRIMARY KEY, c CHAR(4), r RAW(4))",
    )
    .unwrap();
    run(&mut oracle, "INSERT INTO o VALUES (1, 'ab', '0a')").unwrap();
    assert_eq!(
        last(&mut oracle, "SELECT c, LENGTH(c), r FROM o WHERE c = 'ab'"),
        "C|LENGTH(C)|R\nab  |4|\\x0a\nSELECT 1"
    );
}

#[test]
fn item_31_values_behave_as_postgresql_documents() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let cases = [
        (
            "SELECT interval '1 year 2 mons 3 days 04:05:06', interval 'P1Y2M3DT4H5M6S', interval '-1 day +02:00:00'",
            "interval|interval|interval\n1 year 2 mons 3 days 04:05:06|1 year 2 mons 3 days 04:05:06|-1 days +02:00:00\nSELECT 1",
        ),
        (
            "SELECT timestamp '2024-01-31' + interval '1 month', timestamp '2023-03-31' - interval '1 month', date '2024-02-29' + interval '1 year'",
            "?column?|?column?|?column?\n2024-02-29 00:00:00|2023-02-28 00:00:00|2025-02-28 00:00:00\nSELECT 1",
        ),
        (
            "SELECT interval '1 mon' = interval '30 days', interval '1 day' = interval '24 hours', interval '1 mon' < interval '31 days'",
            "?column?|?column?|?column?\nt|t|t\nSELECT 1",
        ),
        (
            "SELECT interval '1 mon' * 1.5, interval '3 days' / 2, interval '1 day' * 2, 2 * interval '1 hour'",
            "?column?|?column?|?column?|?column?\n1 mon 15 days|1 day 12:00:00|2 days|02:00:00\nSELECT 1",
        ),
        (
            "SELECT time '24:00:00', time '10:00' + interval '15 hours', time '10:00' - time '09:30', date '2024-01-01' + time '10:00'",
            "time|?column?|?column?|?column?\n24:00:00|01:00:00|00:30:00|2024-01-01 10:00:00\nSELECT 1",
        ),
        (
            "SELECT '\\x0aff'::bytea, 'a\\\\b\\001'::bytea, encode('\\x00ff'::bytea, 'escape'), length('\\x0102'::bytea), 'a'::bytea || 'b'::bytea",
            "bytea|bytea|encode|length|?column?\n\\x0aff|\\x615c6201|\\000\\377|2|\\x6162\nSELECT 1",
        ),
        (
            "SELECT 'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11'::uuid = '{a0eebc99-9c0b4ef8-bb6d6bb9-bd380a11}'::uuid",
            "?column?\nt\nSELECT 1",
        ),
        (
            "SELECT 'ab'::char(4) || '|', length('ab  '::char(4)), octet_length('ab'::char(4)), 'ab'::char(4) = 'ab  ', 'ab'::char(4)::text || '|'",
            "?column?|length|octet_length|?column?|?column?\nab||2|4|t|ab|\nSELECT 1",
        ),
        (
            "SELECT extract(epoch from interval '1 year'), extract(hour from time '13:45'), date_part('minute', interval '01:30:00')",
            "extract|extract|date_part\n31557600.000000|13|30\nSELECT 1",
        ),
        (
            "SELECT justify_hours(interval '27 hours'), justify_interval(interval '1 mon -1 hour'), age(timestamp '2024-03-01', timestamp '2023-01-31')",
            "justify_hours|justify_interval|age\n1 day 03:00:00|29 days 23:00:00|1 year 1 mon 1 day\nSELECT 1",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(last(&mut session, sql), expected, "{sql}");
    }
    let errors = [
        ("SELECT '\\x0'::bytea", "22023"),
        ("SELECT '\\x0g'::bytea", "22023"),
        ("SELECT 'a\\b'::bytea", "22P02"),
        ("SELECT 'x'::uuid", "22P02"),
        ("SELECT time '24:00:01'", "22008"),
        ("SELECT interval '1 fortnight'", "22007"),
        ("SELECT interval '2147483648 days'", "22015"),
        (
            "SELECT interval '2147483647 days' + interval '1 day'",
            "22008",
        ),
        ("SELECT interval '1 day' * 'NaN'::float8", "22008"),
        ("SELECT interval '1 day' * 1e308", "22008"),
        ("SELECT interval '1 day' / 0", "22012"),
        ("SELECT -interval '-178956970 years -8 mons'", "22008"),
        (
            "SELECT timestamp '294276-12-31' + interval '1 day'",
            "22008",
        ),
        ("SELECT get_byte('\\x01'::bytea, 1)", "2202E"),
        ("SELECT encode('\\x01'::bytea, 'rot13')", "22023"),
        ("SELECT decode('abc', 'hex')", "22023"),
        ("SELECT substring('abc', 1, -1)", "22011"),
        (
            "SELECT 'abcde'::char(3) = 'abc', CAST('abcdef' AS varchar(2))",
            "",
        ),
        ("SELECT interval '1 day' + 1", "42883"),
        (
            "SELECT time '10:00' + date '2024-01-01' - time '01:00'",
            "42883",
        ),
        ("SELECT '\\x01'::bytea || 1", "42883"),
        (
            "SELECT 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid = 'x'",
            "22P02",
        ),
        (
            "SELECT 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid < 1",
            "42883",
        ),
    ];
    for (sql, state) in errors {
        if state.is_empty() {
            assert!(run(&mut session, sql).is_ok(), "{sql}");
            continue;
        }
        let error = fails(&mut session, sql);
        assert_eq!(error.state().code(), state, "{sql}: {error}");
    }
    run(
        &mut session,
        "CREATE TABLE c (id integer PRIMARY KEY, code char(3))",
    )
    .unwrap();
    assert_eq!(
        fails(&mut session, "INSERT INTO c VALUES (1, 'abcd')")
            .state()
            .code(),
        "22001"
    );
    run(&mut session, "INSERT INTO c VALUES (1, 'ab   '), (2, 'x')").unwrap();
    assert_eq!(
        last(
            &mut session,
            "SELECT code || '|', length(code) FROM c ORDER BY id"
        ),
        "?column?|length\nab||2\nx||1\nSELECT 2"
    );
}

#[test]
fn mysql_and_oracle_read_strings_given_to_binary_columns_by_their_own_rules() {
    let store = MemoryStore::default();
    let mut mysql = Session::with_dialect(&store, crate::Dialect::MySql);
    run(
        &mut mysql,
        "CREATE TABLE bl (id INT PRIMARY KEY, b BLOB, t TEXT)",
    )
    .unwrap();
    run(
        &mut mysql,
        "INSERT INTO bl VALUES (1, 'abc', 'x'), (3, '\\\\x41', NULL), (4, '\\\\001', NULL), (5, 'a\\\\b', NULL)",
    )
    .unwrap();
    assert_eq!(
        last(
            &mut mysql,
            "SELECT id, b, LENGTH(b), CONCAT('x', b), CONCAT(b, b), CONCAT(b, NULL) FROM bl ORDER BY id"
        ),
        "id|b|LENGTH(b)|CONCAT('x', b)|CONCAT(b, b)|CONCAT(b, NULL)\n\
         1|\\x616263|3|\\x78616263|\\x616263616263|NULL\n\
         3|\\x5c783431|4|\\x785c783431|\\x5c7834315c783431|NULL\n\
         4|\\x5c303031|4|\\x785c303031|\\x5c3030315c303031|NULL\n\
         5|\\x615c62|3|\\x78615c62|\\x615c62615c62|NULL\n\
         SELECT 4"
    );
    assert_eq!(
        last(
            &mut mysql,
            "SELECT id FROM bl WHERE b = 'abc' OR b = '\\\\x41' ORDER BY id"
        ),
        "id\n1\n3\nSELECT 2"
    );
    let parameters = [crate::Parameter {
        declared_type: Some(crate::SqlType::Bytea),
        value: Some("\\x00ff".into()),
    }];
    mysql
        .execute_prepared(
            "INSERT INTO bl (id, b) VALUES (6, ?)",
            &parameters,
            &mut |_| true,
        )
        .unwrap();
    assert_eq!(
        last(&mut mysql, "SELECT LENGTH(b), b FROM bl WHERE id = 6"),
        "LENGTH(b)|b\n2|\\x00ff\nSELECT 1"
    );
    let mut oracle = Session::with_dialect(&store, crate::Dialect::Oracle);
    run(
        &mut oracle,
        "CREATE TABLE r (id NUMBER PRIMARY KEY, r RAW(8))",
    )
    .unwrap();
    run(&mut oracle, "INSERT INTO r VALUES (1, '0a')").unwrap();
    run(&mut oracle, "INSERT INTO r VALUES (2, 'ABC')").unwrap();
    assert_eq!(
        last(
            &mut oracle,
            "SELECT r, LENGTH(r) FROM r WHERE r = '0A' OR r = 'abc' ORDER BY id"
        ),
        "R|LENGTH(R)\n\\x0a|2\n\\x0abc|4\nSELECT 2"
    );
    let error = fails(&mut oracle, "INSERT INTO r VALUES (3, 'zz')");
    assert_eq!(error.vendor_code(), Some(1465), "{error}");
    let mut postgresql = Session::new(&store);
    assert_eq!(
        last(&mut postgresql, "SELECT '\\x41'::bytea, 'a\\\\b'::bytea"),
        "bytea|bytea\n\\x41|\\x615c62\nSELECT 1"
    );
}
