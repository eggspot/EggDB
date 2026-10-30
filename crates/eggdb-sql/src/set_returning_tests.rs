use crate::memory::{WORK_MEMORY_LIMIT, WorkMemory, share_statement_memory};
use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::types::SqlType;
use crate::{ArrayElement, Session, SqlError};

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> Result<String, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs.pop().unwrap_or_default())
}

fn state(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    match run(session, sql) {
        Ok(output) => panic!("{sql} succeeded: {output}"),
        Err(error) => error.state().code().to_owned(),
    }
}

#[test]
fn rows_a_function_generates_count_against_the_statements_memory() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let _shared = share_statement_memory();
    let mut held = WorkMemory::new();
    held.charge(WORK_MEMORY_LIMIT - 100_000).unwrap();
    for sql in [
        "SELECT count(*) FROM generate_series(1, 1000000000000)",
        "SELECT count(*) FROM (SELECT generate_series(1, 1000000000000)) g",
        "SELECT count(*) FROM unnest(ARRAY(SELECT * FROM generate_series(1, 100000)))",
    ] {
        assert_eq!(state(&mut session, sql), "53200", "{sql}");
    }
}

#[test]
fn placements_that_do_not_run_yet_are_0a000() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(&mut session, "CREATE TABLE t (n integer PRIMARY KEY)").unwrap();
    run(&mut session, "INSERT INTO t VALUES (1), (2)").unwrap();
    for sql in [
        "SELECT * FROM upper('x')",
        "SELECT * FROM t, generate_series(1, t.n)",
        "SELECT * FROM t, generate_series(1, n)",
        "SELECT generate_series(1, 2) + 1",
        "SELECT n FROM t ORDER BY generate_series(1, 2)",
        "SELECT n FROM t GROUP BY generate_series(1, 2)",
        "SELECT * FROM generate_series(1, unnest(ARRAY[3]))",
        "SELECT * FROM generate_series(1.5, 3)",
        "UPDATE t SET n = generate_series(1, 2)",
        "SELECT (SELECT generate_series(1, t.n)) FROM t",
    ] {
        assert_eq!(state(&mut session, sql), "0A000", "{sql}");
    }
    assert_eq!(
        state(&mut session, "SELECT * FROM t, generate_series(1, nosuch)"),
        "42703"
    );
    assert_eq!(
        state(&mut session, "SELECT * FROM t, (VALUES (t.n)) v"),
        "42P01"
    );
    assert_eq!(state(&mut session, "SELECT * FROM unnest('{1}')"), "42725");
    assert_eq!(state(&mut session, "SELECT * FROM unnest(1)"), "42883");
}

#[test]
fn values_and_functions_in_from_are_described_without_running() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let description = session
        .describe(
            "SELECT * FROM generate_series(1, 1000000000000) WITH ORDINALITY AS g(n, i), (VALUES (1, 'a')) v",
            &[],
        )
        .unwrap();
    let columns: Vec<(String, Option<SqlType>)> = description
        .columns
        .unwrap_or_default()
        .into_iter()
        .map(|column| (column.name, column.sql_type))
        .collect();
    assert_eq!(
        columns,
        [
            ("n".to_owned(), Some(SqlType::BigInt)),
            ("i".to_owned(), Some(SqlType::BigInt)),
            ("column1".to_owned(), Some(SqlType::Integer)),
            ("column2".to_owned(), Some(SqlType::Text)),
        ]
    );
    let description = session
        .describe("SELECT ARRAY[1, 2] AS a, unnest(ARRAY['x']) AS u", &[])
        .unwrap();
    let types: Vec<Option<SqlType>> = description
        .columns
        .unwrap_or_default()
        .into_iter()
        .map(|column| column.sql_type)
        .collect();
    assert_eq!(
        types,
        [
            Some(SqlType::Array(ArrayElement::Integer)),
            Some(SqlType::Text)
        ]
    );
}

#[test]
fn a_correlated_function_in_from_runs_for_each_outer_value() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(
        &mut session,
        "CREATE TABLE t (id integer PRIMARY KEY, n integer)",
    )
    .unwrap();
    let rows: Vec<String> = (1..=300).map(|id| format!("({id}, {})", id % 3)).collect();
    run(
        &mut session,
        &format!("INSERT INTO t VALUES {}", rows.join(", ")),
    )
    .unwrap();
    assert_eq!(
        run(
            &mut session,
            "SELECT sum(s) FROM (SELECT (SELECT sum(g) FROM generate_series(1, t.n) g) AS s FROM t) x"
        )
        .unwrap(),
        "sum\n400\nSELECT 1"
    );
    assert_eq!(
        run(
            &mut session,
            "SELECT count(*) FROM t WHERE EXISTS (SELECT 1 FROM unnest(ARRAY[t.n]) u WHERE u = 2)"
        )
        .unwrap(),
        "count\n100\nSELECT 1"
    );
    assert_eq!(
        run(
            &mut session,
            "SELECT count(*) FROM t WHERE n = ANY (ARRAY(SELECT u FROM unnest(ARRAY[t.id % 5, 2]) u))"
        )
        .unwrap(),
        "count\n140\nSELECT 1"
    );
}

#[test]
fn generated_rows_feed_aggregates_and_regular_expressions() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        run(
            &mut session,
            "SELECT count(*), max(g::bigint * g) FROM generate_series(1, 1000) g"
        )
        .unwrap(),
        "count|max\n1000|1000000\nSELECT 1"
    );
    assert_eq!(
        run(
            &mut session,
            "SELECT count(*) FROM generate_series(1, 1000) g WHERE g::text ~ '^1[0-9]*0$'"
        )
        .unwrap(),
        "count\n12\nSELECT 1"
    );
}

#[test]
fn a_function_naming_any_sibling_from_item_is_0a000_even_when_the_outer_query_has_that_name() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(&mut session, "CREATE TABLE t1 (x integer PRIMARY KEY)").unwrap();
    run(&mut session, "CREATE TABLE t2 (y integer PRIMARY KEY)").unwrap();
    run(&mut session, "INSERT INTO t1 VALUES (1)").unwrap();
    run(&mut session, "INSERT INTO t2 VALUES (2), (3)").unwrap();
    for sql in [
        "SELECT t2.y, (SELECT count(*) FROM t2, generate_series(t1.x, t2.y) g) FROM t1, t2 ORDER BY 1",
        "SELECT t2.y, (SELECT count(*) FROM t2, generate_series(t1.x, y) g) FROM t1, t2 ORDER BY 1",
        "SELECT * FROM generate_series(1, 2) g JOIN generate_series(1, g.g) h ON true",
        "SELECT * FROM generate_series(1, 2) g, generate_series(1, g) h",
        "SELECT * FROM generate_series(1, g.g) h, generate_series(1, 2) g",
        "SELECT * FROM (SELECT 2 AS n) d, generate_series(1, d.n) h",
        "SELECT * FROM (SELECT 2 AS n) d, generate_series(1, n) h",
        "SELECT * FROM (VALUES (2)) v(n), generate_series(1, n + 1) h",
        "SELECT * FROM t1, generate_series(1, 1 + abs(x)) h",
    ] {
        assert_eq!(state(&mut session, sql), "0A000", "{sql}");
    }
}

#[test]
fn a_values_list_in_from_names_the_outer_query_and_never_its_siblings() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(&mut session, "CREATE TABLE t1 (x integer PRIMARY KEY)").unwrap();
    run(&mut session, "CREATE TABLE t2 (y integer PRIMARY KEY)").unwrap();
    run(&mut session, "INSERT INTO t1 VALUES (1)").unwrap();
    run(&mut session, "INSERT INTO t2 VALUES (2), (3)").unwrap();
    assert_eq!(
        run(
            &mut session,
            "SELECT x, (SELECT max(c) FROM (VALUES (x), (2)) v(c)) FROM (VALUES (1),(3)) t(x) ORDER BY 1"
        )
        .unwrap(),
        "x|max\n1|2\n3|3\nSELECT 2"
    );
    assert_eq!(
        run(
            &mut session,
            "SELECT t2.y, (SELECT count(*) FROM t2, (VALUES (t2.y)) v) FROM t1, t2 ORDER BY 1"
        )
        .unwrap(),
        "y|count\n2|2\n3|2\nSELECT 2"
    );
    assert_eq!(
        run(
            &mut session,
            "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS v(n) ORDER BY n DESC"
        )
        .unwrap(),
        "n|column2\n2|b\n1|a\nSELECT 2"
    );
    assert_eq!(
        state(&mut session, "SELECT * FROM (VALUES (1)) v(a, b)"),
        "42P10"
    );
    assert_eq!(
        state(&mut session, "SELECT * FROM t2, (VALUES (y)) v"),
        "42703"
    );
}

#[test]
fn rows_go_into_the_intermediate_once_and_every_copy_held_is_charged() {
    let budget = 2300 * 1024;
    for (sql, fits) in [
        ("SELECT count(*) FROM generate_series(1, 20000)", true),
        (
            "SELECT count(*) FROM (SELECT generate_series(1, 20000)) g",
            false,
        ),
    ] {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        let _shared = share_statement_memory();
        let mut held = WorkMemory::new();
        held.charge(WORK_MEMORY_LIMIT - budget).unwrap();
        if fits {
            assert_eq!(run(&mut session, sql).unwrap(), "count\n20000\nSELECT 1");
        } else {
            assert_eq!(state(&mut session, sql), "53200", "{sql}");
        }
    }
}

#[test]
fn a_function_in_from_naming_a_sibling_through_a_subquery_is_0a000() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    for sql in [
        "SELECT * FROM (VALUES (2)) w(x), generate_series(1, (SELECT x)) g",
        "SELECT * FROM (VALUES (2)) w(x), unnest((SELECT ARRAY[x])) g",
        "SELECT * FROM (VALUES (2)) w(x), generate_series(1, (SELECT w.x)) g",
    ] {
        assert_eq!(state(&mut session, sql), "0A000", "{sql}");
    }
    assert_eq!(
        run(
            &mut session,
            "SELECT count(*) AS n FROM (VALUES (5)) w(x), generate_series(1, (SELECT max(x) FROM (VALUES (3)) v(x))) g"
        )
        .unwrap(),
        "n\n3\nSELECT 1"
    );
    assert_eq!(
        state(
            &mut session,
            "SELECT * FROM (VALUES (2)) w(x), generate_series(1, (SELECT nowhere)) g"
        ),
        "42703"
    );
}

#[test]
fn set_returning_functions_in_one_select_list_run_in_lock_step() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(&mut session, "CREATE TABLE t (n integer PRIMARY KEY)").unwrap();
    run(&mut session, "INSERT INTO t VALUES (1), (2)").unwrap();
    assert_eq!(
        run(
            &mut session,
            "SELECT n, generate_series(1, n) AS g, unnest(ARRAY['a', 'b', 'c']) AS u FROM t ORDER BY n, u"
        )
        .unwrap(),
        "n|g|u\n1|1|a\n1|NULL|b\n1|NULL|c\n2|1|a\n2|2|b\n2|NULL|c\nSELECT 6"
    );
    assert_eq!(
        run(
            &mut session,
            "SELECT unnest('{}'::integer[]) AS a, generate_series(1, 0) AS b"
        )
        .unwrap(),
        "a|b\nSELECT 0"
    );
}

#[test]
fn window_ranking_functions_number_rows_and_describe_as_bigint() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(
        &mut session,
        "CREATE TABLE t (id integer PRIMARY KEY, g integer)",
    )
    .unwrap();
    run(&mut session, "INSERT INTO t VALUES (1, 1), (2, 1), (3, 2)").unwrap();
    assert_eq!(
        run(
            &mut session,
            "SELECT id, row_number() OVER (PARTITION BY g ORDER BY id DESC) AS r FROM t ORDER BY id"
        )
        .unwrap(),
        "id|r\n1|2\n2|1\n3|1\nSELECT 3"
    );
    let description = session
        .describe("SELECT id, rank() OVER (ORDER BY g) FROM t", &[])
        .unwrap();
    let types: Vec<Option<SqlType>> = description
        .columns
        .unwrap_or_default()
        .into_iter()
        .map(|column| column.sql_type)
        .collect();
    assert_eq!(types, [Some(SqlType::Integer), Some(SqlType::BigInt)]);
    assert_eq!(
        run(
            &mut session,
            "SELECT row_number() OVER (ORDER BY id) AS n, generate_series(1, 2) AS s FROM t ORDER BY 1, 2"
        )
        .unwrap(),
        "n|s\n1|1\n1|2\n2|1\n2|2\n3|1\n3|2\nSELECT 6"
    );
}
