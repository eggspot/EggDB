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

fn films(session: &mut Session<'_>) {
    last(
        session,
        "CREATE TABLE films (code char(5) PRIMARY KEY, title varchar(40) NOT NULL, \
         date_prod date, kind varchar(10))",
    );
    last(
        session,
        "INSERT INTO films VALUES ('UA502', 'Bananas', '1971-07-13', 'Comedy'), \
         ('T_601', 'Yojimbo', '1961-06-16', 'Drama'), ('B6717', 'Tampopo', '2003-11-02', 'Comedy')",
    );
}

#[test]
fn postgresql_create_table_as_copies_rows_and_reports_select_n() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    films(&mut session);
    assert_eq!(
        last(
            &mut session,
            "CREATE TABLE films_recent AS SELECT * FROM films WHERE date_prod >= '2002-01-01'"
        ),
        "SELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT code, title FROM films_recent"),
        "code|title\nB6717|Tampopo\nSELECT 1"
    );
    last(&mut session, "INSERT INTO films_recent (code) VALUES ('X')");
    assert_eq!(
        last(
            &mut session,
            "SELECT count(*) FROM films_recent WHERE title IS NULL"
        ),
        "count\n1\nSELECT 1"
    );
}

#[test]
fn postgresql_create_table_as_names_columns_and_skips_data_on_request() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    films(&mut session);
    assert_eq!(
        last(
            &mut session,
            "CREATE TABLE comedies (id, name) AS SELECT code, title, kind FROM films \
             WHERE kind = 'Comedy' ORDER BY code"
        ),
        "SELECT 2"
    );
    assert_eq!(
        last(&mut session, "SELECT * FROM comedies"),
        "id|name|kind\nB6717|Tampopo|Comedy\nUA502|Bananas|Comedy\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "CREATE TABLE shape AS SELECT code, date_prod FROM films WITH NO DATA"
        ),
        "CREATE TABLE AS"
    );
    assert_eq!(
        last(&mut session, "SELECT * FROM shape"),
        "code|date_prod\nSELECT 0"
    );
    assert_eq!(
        last(
            &mut session,
            "CREATE TABLE everything AS SELECT code FROM films WITH DATA"
        ),
        "SELECT 3"
    );
    let error = fails(
        &mut session,
        "CREATE TABLE wrong (a, b, c) AS SELECT code, title FROM films",
    );
    assert_eq!(error.state().code(), "42601");
    assert_eq!(error.message(), "too many column names were specified");
}

#[test]
fn postgresql_create_table_if_not_exists_as_skips_an_existing_table() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    films(&mut session);
    last(&mut session, "CREATE TABLE copy AS SELECT code FROM films");
    assert_eq!(
        last(
            &mut session,
            "CREATE TABLE IF NOT EXISTS copy AS SELECT code FROM films"
        ),
        "CREATE TABLE AS"
    );
    assert_eq!(
        last(&mut session, "SELECT count(*) FROM copy"),
        "count\n3\nSELECT 1"
    );
    assert_eq!(
        fails(&mut session, "CREATE TABLE copy AS SELECT 1 AS one")
            .state()
            .code(),
        "42P07"
    );
}

#[test]
fn postgresql_create_temp_table_as_and_rollback() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    films(&mut session);
    last(
        &mut session,
        "CREATE TEMP TABLE scratch AS SELECT title FROM films WHERE kind = 'Drama'",
    );
    assert_eq!(
        last(&mut session, "SELECT title FROM scratch"),
        "title\nYojimbo\nSELECT 1"
    );
    last(&mut session, "BEGIN");
    last(&mut session, "CREATE TABLE gone AS SELECT 1 AS one");
    last(&mut session, "ROLLBACK");
    assert_eq!(
        fails(&mut session, "SELECT * FROM gone").state().code(),
        "42P01"
    );
}

#[test]
fn mysql_create_table_select_merges_declared_columns_and_keeps_not_null() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    last(
        &mut session,
        "CREATE TABLE test2 (b INT NOT NULL, c VARCHAR(10))",
    );
    last(
        &mut session,
        "INSERT INTO test2 VALUES (10, 'x'), (20, NULL)",
    );
    last(
        &mut session,
        "CREATE TABLE test (a INT NOT NULL AUTO_INCREMENT, PRIMARY KEY (a), KEY(b)) \
         ENGINE=InnoDB SELECT b, c FROM test2",
    );
    assert_eq!(
        last(&mut session, "SELECT a, b, c FROM test ORDER BY a"),
        "a|b|c\n1|10|x\n2|20|NULL\nSELECT 2"
    );
    last(&mut session, "CREATE TABLE plain AS SELECT b, c FROM test2");
    fails(&mut session, "INSERT INTO plain (b, c) VALUES (NULL, 'y')");
    last(&mut session, "INSERT INTO plain (b, c) VALUES (30, NULL)");
    last(&mut session, "CREATE TABLE foo (n INT)");
    last(&mut session, "INSERT INTO foo VALUES (1), (2)");
    last(
        &mut session,
        "CREATE TABLE bar (UNIQUE (n)) SELECT n FROM foo",
    );
    fails(&mut session, "INSERT INTO bar VALUES (1)");
    last(
        &mut session,
        "CREATE TABLE tagged (m INT) SELECT n FROM foo",
    );
    assert_eq!(
        last(&mut session, "SELECT * FROM tagged ORDER BY n"),
        "m|n\nNULL|1\nNULL|2\nSELECT 2"
    );
}

#[test]
fn mysql_values_statement_and_derived_table() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    assert_eq!(
        last(&mut session, "VALUES ROW(1,-2,3), ROW(5,7,9), ROW(4,6,8)"),
        "column_0|column_1|column_2\n1|-2|3\n5|7|9\n4|6|8\nSELECT 3"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT column_1 FROM (VALUES ROW(1, 2), ROW(3, 4)) AS t ORDER BY column_1 DESC"
        ),
        "column_1\n4\n2\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT a + b AS total FROM (VALUES ROW(1, 2), ROW(3, 4)) AS t (a, b)"
        ),
        "total\n3\n7\nSELECT 2"
    );
}

#[test]
fn mysql_row_subqueries_and_row_comparisons() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    last(&mut session, "CREATE TABLE t1 (col1 INT, col2 INT)");
    last(&mut session, "CREATE TABLE t2 (id INT, col3 INT, col4 INT)");
    last(&mut session, "INSERT INTO t1 VALUES (1, 2), (3, 4), (5, 6)");
    last(&mut session, "INSERT INTO t2 VALUES (10, 3, 4), (11, 5, 6)");
    assert_eq!(
        last(
            &mut session,
            "SELECT * FROM t1 WHERE (col1, col2) = (SELECT col3, col4 FROM t2 WHERE id = 10)"
        ),
        "col1|col2\n3|4\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT * FROM t1 WHERE ROW(col1, col2) = (SELECT col3, col4 FROM t2 WHERE id = 10)"
        ),
        "col1|col2\n3|4\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT col1 FROM t1 WHERE (col1, col2) IN (SELECT col3, col4 FROM t2) ORDER BY col1"
        ),
        "col1\n3\n5\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT col1 FROM t1 WHERE (col1, col2) > (1, 2) ORDER BY col1"
        ),
        "col1\n3\n5\nSELECT 2"
    );
    let error = fails(
        &mut session,
        "SELECT * FROM t1 WHERE (col1, col2) IN (SELECT col3 FROM t2)",
    );
    assert_eq!(error.vendor_code(), Some(1241));
    let error = fails(
        &mut session,
        "SELECT * FROM t1 WHERE (col1, col2) = (SELECT col3, col4 FROM t2)",
    );
    assert_eq!(error.state().code(), "21000");
}

#[test]
fn sqlite_create_table_as_declares_types_from_expression_affinity() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    last(&mut session, "CREATE TABLE t1 (a INTEGER, b REAL)");
    last(&mut session, "INSERT INTO t1 VALUES (1, 2.5), (2, 3.5)");
    last(
        &mut session,
        "CREATE TABLE t2 AS SELECT a, b + 1 AS c, CAST(b AS TEXT) AS d FROM t1",
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 't2'"
        ),
        "sql\nCREATE TABLE t2(a INT,c,d TEXT)\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT a, c, d FROM t2 ORDER BY a"),
        "a|c|d\n1|3.5|2.5\n2|4.5|3.5\nSELECT 2"
    );
    last(
        &mut session,
        "CREATE TABLE IF NOT EXISTS t2 AS SELECT a FROM t1",
    );
    assert_eq!(
        last(&mut session, "SELECT count(*) FROM t2"),
        "count(*)\n2\nSELECT 1"
    );
}

#[test]
fn sqlite_values_and_row_values() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    assert_eq!(
        last(&mut session, "VALUES (1, 2), (3, 4)"),
        "column1|column2\n1|2\n3|4\nSELECT 2"
    );
    last(&mut session, "CREATE TABLE t (a INTEGER, b INTEGER)");
    last(&mut session, "INSERT INTO t VALUES (1, 2), (1, 5), (2, 1)");
    assert_eq!(
        last(
            &mut session,
            "SELECT a, b FROM t WHERE (a, b) > (1, 2) ORDER BY a, b"
        ),
        "a|b\n1|5\n2|1\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT a, b FROM t WHERE (a, b) IN (VALUES (1, 5), (2, 1)) ORDER BY a"
        ),
        "a|b\n1|5\n2|1\nSELECT 2"
    );
    assert_eq!(
        last(&mut session, "SELECT a, b FROM t WHERE (a, b) = (1, 2)"),
        "a|b\n1|2\nSELECT 1"
    );
}

#[test]
fn postgresql_row_comparisons_follow_sql_null_rules() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(&mut session, "CREATE TABLE t (a int, b int)");
    last(&mut session, "CREATE TABLE s (x int, y int)");
    last(&mut session, "INSERT INTO t VALUES (1, 5), (2, 5)");
    last(&mut session, "INSERT INTO s VALUES (1, NULL)");
    assert_eq!(
        last(
            &mut session,
            "SELECT count(*) FROM t WHERE (a, b) IN (SELECT x, y FROM s)"
        ),
        "count\n0\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT a FROM t WHERE (a, b) NOT IN (SELECT x, y FROM s)"
        ),
        "a\n2\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT count(*) FROM t WHERE ((a, NULL::int) = (a, 2)) IS NULL"
        ),
        "count\n2\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT count(*) FROM t WHERE (a, b) < (2, NULL::int)"
        ),
        "count\n1\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT a FROM t WHERE (a, b) = (SELECT 2, 5)"),
        "a\n2\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT count(*) FROM t WHERE (a, b) = (SELECT x, y FROM s WHERE x = 9)"
        ),
        "count\n0\nSELECT 1"
    );
    let error = fails(&mut session, "SELECT (1, 2) = (1, 2, 3)");
    assert_eq!(
        error.message(),
        "unequal number of entries in row expressions"
    );
    let error = fails(
        &mut session,
        "SELECT count(*) FROM t WHERE (a, b) IN (SELECT x FROM s)",
    );
    assert_eq!(error.message(), "subquery has too few columns");
    assert_eq!(
        last(
            &mut session,
            "SELECT a FROM t WHERE (a, b) IN ((1, 5), (3, 3))"
        ),
        "a\n1\nSELECT 1"
    );
}

#[test]
fn oracle_create_table_as_keeps_not_null_and_needs_aliases() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    last(
        &mut session,
        "CREATE TABLE employees (employee_id NUMBER(6) NOT NULL, last_name VARCHAR2(25), salary NUMBER(8,2))",
    );
    last(
        &mut session,
        "INSERT INTO employees VALUES (100, 'King', 24000)",
    );
    last(
        &mut session,
        "CREATE TABLE emp_shape AS SELECT * FROM employees WHERE 1 = 0",
    );
    assert_eq!(
        last(&mut session, "SELECT COUNT(*) FROM emp_shape"),
        "COUNT(*)\n0\nSELECT 1"
    );
    fails(
        &mut session,
        "INSERT INTO emp_shape (employee_id, last_name) VALUES (NULL, 'x')",
    );
    last(
        &mut session,
        "CREATE TABLE raises (id, new_salary) AS SELECT employee_id, salary * 1.1 FROM employees",
    );
    assert_eq!(
        last(&mut session, "SELECT id FROM raises"),
        "ID\n100\nSELECT 1"
    );
    let error = fails(
        &mut session,
        "CREATE TABLE unnamed AS SELECT salary * 2 FROM employees",
    );
    assert_eq!(
        error.message(),
        "must name this expression with a column alias"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT last_name FROM employees WHERE (employee_id, salary) IN (SELECT id, 24000 FROM raises)"
        ),
        "LAST_NAME\nKing\nSELECT 1"
    );
}

#[test]
fn sqlserver_values_derived_table_names_its_columns() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    let shown = last(
        &mut session,
        "SELECT id, name FROM (VALUES (1, 'a'), (2, 'b')) AS t (id, name) ORDER BY id",
    );
    assert!(shown.starts_with("id|name\n1|a\n2|b\n"), "{shown}");
}

#[test]
fn a_wide_row_ordering_is_decided_without_nesting_one_level_per_field() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let ones = vec!["1"; 2000].join(", ");
    let mut raised = vec!["1"; 1999];
    raised.push("2");
    let raised = raised.join(", ");
    assert_eq!(
        last(&mut session, &format!("SELECT ({ones}) < ({raised})")),
        "?column?\nt\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, &format!("SELECT ({ones}) >= ({ones})")),
        "?column?\nt\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT (1, NULL::int, 3) < (1, 2, 0), (1, 2, NULL::int) < (1, 3, 0), ('a', 2) > ('a', 1)"
        ),
        "?column?|?column?|?column?\nNULL|t|t\nSELECT 1"
    );
}

#[test]
fn a_row_in_list_evaluates_its_left_row_once() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(&mut session, "CREATE SEQUENCE drawn");
    assert_eq!(
        last(
            &mut session,
            "SELECT (nextval('drawn'), 1) IN ((5, 1), (6, 1), (7, 1), (1, 1))"
        ),
        "?column?\nt\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT currval('drawn')"),
        "currval\n1\nSELECT 1"
    );
    let wide_left = vec!["coalesce(1, 2)"; 200].join(", ");
    let item = format!("({})", vec!["2"; 200].join(", "));
    let items = vec![item; 3000].join(", ");
    assert_eq!(
        last(
            &mut session,
            &format!("SELECT ({wide_left}) NOT IN ({items})")
        ),
        "?column?\nt\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT (1, NULL::int) IN ((2, 2), (3, 3)), (1, NULL::int) IN ((2, 2), (1, NULL)), \
             (1, NULL::int) NOT IN ((2, 2)), ('x', 1) IN (('x', 1))"
        ),
        "?column?|?column?|?column?|?column?\nf|NULL|t|t\nSELECT 1"
    );
}

#[test]
fn mysql_values_function_parses_inside_parentheses_in_on_duplicate_key_update() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    last(&mut session, "CREATE TABLE u (a int PRIMARY KEY, b int)");
    last(&mut session, "INSERT INTO u VALUES (1, 5)");
    last(
        &mut session,
        "INSERT INTO u VALUES (1, 5) ON DUPLICATE KEY UPDATE b = (VALUES(b) + 1)",
    );
    last(
        &mut session,
        "INSERT INTO u VALUES (1, 8) ON DUPLICATE KEY UPDATE b = ((VALUES(b)) * 2)",
    );
    assert_eq!(
        last(&mut session, "SELECT a, b FROM u"),
        "a|b\n1|16\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT * FROM (VALUES ROW(1, 2)) AS v"),
        "column_0|column_1\n1|2\nSELECT 1"
    );
}

#[test]
fn row_constructors_follow_each_dialects_rules_outside_postgresql() {
    let store = MemoryStore::default();
    let mut sqlite = Session::with_dialect(&store, Dialect::Sqlite);
    assert_eq!(
        last(
            &mut sqlite,
            "SELECT CASE WHEN (1, 2) = (SELECT 1, 2 UNION ALL SELECT 3, 4) THEN 'first' END AS r"
        ),
        "r\nfirst\nSELECT 1"
    );
    last(&mut sqlite, "CREATE TABLE pairs (a, b)");
    let error = fails(&mut sqlite, "INSERT INTO pairs VALUES ((1, 2), 3)");
    assert_eq!(error.message(), "row value misused");
    let mut mysql = Session::with_dialect(&store, Dialect::MySql);
    assert_eq!(fails(&mut mysql, "SELECT (1, 2)").vendor_code(), Some(1241));
    let mut oracle = Session::with_dialect(&store, Dialect::Oracle);
    let error = fails(&mut oracle, "SELECT 1 FROM dual WHERE (1, 2) < (2, 3)");
    assert_eq!(error.vendor_code(), Some(920));
    assert_eq!(
        last(&mut oracle, "SELECT 1 one FROM dual WHERE (1, 2) = (1, 2)"),
        "ONE\n1\nSELECT 1"
    );
}

#[test]
fn postgresql_create_table_as_takes_on_commit_and_refuses_storage_options() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(&mut session, "BEGIN");
    last(
        &mut session,
        "CREATE TEMP TABLE v2 ON COMMIT DROP AS SELECT 1 AS a",
    );
    assert_eq!(last(&mut session, "SELECT a FROM v2"), "a\n1\nSELECT 1");
    last(&mut session, "COMMIT");
    assert_eq!(
        fails(&mut session, "SELECT a FROM v2").state().code(),
        "42P01"
    );
    for create in [
        "CREATE TABLE v3 USING heap AS SELECT 1 AS a",
        "CREATE TABLE v3 WITH (fillfactor = 70) AS SELECT 1 AS a",
        "CREATE TABLE v3 (a) TABLESPACE pg_default AS SELECT 1",
    ] {
        assert_eq!(
            fails(&mut session, create).state().code(),
            "0A000",
            "{create}"
        );
    }
    assert_eq!(
        fails(
            &mut session,
            "CREATE TABLE v4 ON COMMIT DROP AS SELECT 1 AS a"
        )
        .state()
        .code(),
        "42P16"
    );
}
