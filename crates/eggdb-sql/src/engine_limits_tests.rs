use crate::memory_store::MemoryStore;
use crate::types::Value;
use crate::{QueryResult, Session, SqlError};
use eggdb_platform_core::time::Instant;

fn last(session: &mut Session<'_>, sql: &str) -> Result<QueryResult, SqlError> {
    let mut results = Vec::new();
    session.execute(sql, &mut |result| results.push(result))?;
    Ok(results.pop().unwrap_or_else(|| QueryResult::command("")))
}

fn rows(session: &mut Session<'_>, sql: &str) -> Vec<Vec<Value>> {
    match last(session, sql) {
        Ok(result) => result.rows,
        Err(error) => panic!("{sql}: {error:?}"),
    }
}

fn state(session: &mut Session<'_>, sql: &str) -> String {
    match last(session, sql) {
        Ok(result) => panic!("{sql} succeeded with {} rows", result.rows.len()),
        Err(error) => error.state().code().to_owned(),
    }
}

fn numbers(session: &mut Session<'_>, count: u32) {
    let values: Vec<String> = (1..=count).map(|n| format!("({n})")).collect();
    rows(session, "CREATE TABLE t (n integer PRIMARY KEY)");
    rows(
        session,
        &format!("INSERT INTO t VALUES {}", values.join(", ")),
    );
}

#[test]
fn translate_upper_lower_and_initcap_stop_at_the_value_limit_when_they_grow_text() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    session.set_max_value_bytes(25);
    assert_eq!(
        rows(&mut session, "SELECT translate(repeat('a', 10), 'a', 'b')"),
        vec![vec![Value::Text("b".repeat(10))]]
    );
    assert_eq!(
        state(&mut session, "SELECT translate(repeat('a', 10), 'a', '€')"),
        "54000"
    );
    assert_eq!(
        state(&mut session, "SELECT upper(repeat('ȿ', 10))"),
        "54000"
    );
    assert_eq!(
        state(&mut session, "SELECT lower(repeat('Ⱥ', 10))"),
        "54000"
    );
    assert_eq!(
        rows(&mut session, "SELECT upper(repeat('ab', 10))"),
        vec![vec![Value::Text("AB".repeat(10))]]
    );
    session.set_max_value_bytes(20);
    assert_eq!(
        state(&mut session, "SELECT initcap(repeat('ȿ', 10))"),
        "54000"
    );
    assert_eq!(
        rows(&mut session, "SELECT initcap('hello world')"),
        vec![vec![Value::Text("Hello World".to_owned())]]
    );
}

#[test]
fn array_agg_stops_as_its_array_grows_past_the_value_limit() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    session.set_max_value_bytes(1000);
    assert_eq!(
        state(
            &mut session,
            "SELECT array_agg(repeat('x', 100)) FROM generate_series(1, 50)"
        ),
        "54000"
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT cardinality(array_agg(g)) FROM generate_series(1, 5) g"
        ),
        vec![vec![Value::Integer(5)]]
    );
}

#[test]
fn a_session_deadline_stops_a_join_that_reads_nothing_more_from_the_store() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    numbers(&mut session, 20);
    session.set_deadline(Some(Instant::now()));
    assert_eq!(
        state(
            &mut session,
            "SELECT count(*) FROM t a CROSS JOIN t b CROSS JOIN t c"
        ),
        "57014"
    );
    session.set_deadline(None);
    assert_eq!(
        rows(
            &mut session,
            "SELECT count(*) FROM t a CROSS JOIN t b CROSS JOIN t c"
        ),
        vec![vec![Value::BigInt(8000)]]
    );
}

#[test]
fn the_result_row_cap_cuts_selects_and_leaves_writes_and_aggregates_whole() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    numbers(&mut session, 50);
    session.set_max_result_rows(Some(5));
    assert_eq!(rows(&mut session, "SELECT n FROM t").len(), 5);
    assert_eq!(rows(&mut session, "SELECT n FROM t LIMIT NULL").len(), 5);
    assert_eq!(rows(&mut session, "SELECT n FROM t LIMIT 1000").len(), 5);
    assert_eq!(rows(&mut session, "SELECT n FROM t LIMIT 3").len(), 3);
    assert_eq!(
        rows(&mut session, "SELECT n FROM t ORDER BY n DESC")
            .into_iter()
            .map(|row| row.into_iter().next())
            .collect::<Vec<_>>(),
        (46..=50)
            .rev()
            .map(|n| Some(Value::Integer(n)))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        rows(&mut session, "SELECT n FROM t UNION ALL SELECT n FROM t").len(),
        5
    );
    assert_eq!(
        rows(
            &mut session,
            "WITH small AS (SELECT n FROM t WHERE n <= 30) SELECT n FROM small"
        )
        .len(),
        5
    );
    assert_eq!(
        rows(&mut session, "SELECT count(*) FROM t"),
        vec![vec![Value::BigInt(50)]]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT count(*) FROM (SELECT n FROM t) AS inner_rows"
        ),
        vec![vec![Value::BigInt(50)]]
    );
    assert_eq!(
        rows(
            &mut session,
            "INSERT INTO t SELECT n + 100 FROM t RETURNING n"
        )
        .len(),
        50
    );
    session.set_max_result_rows(None);
    assert_eq!(rows(&mut session, "SELECT n FROM t").len(), 100);
}
