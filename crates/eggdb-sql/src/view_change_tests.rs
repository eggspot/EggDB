use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Dialect, Session, SqlError};

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> Result<String, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs.pop().unwrap_or_default())
}

fn answer(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    match run(session, sql) {
        Ok(output) => output,
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn state(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    match run(session, sql) {
        Ok(output) => panic!("{sql} succeeded: {output}"),
        Err(error) => error.state().code().to_owned(),
    }
}

fn with_table(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = Session::new(store);
    answer(
        &mut session,
        "CREATE TABLE t (id integer PRIMARY KEY, n integer, label text DEFAULT 'none');
         INSERT INTO t VALUES (1, 5, 'a'), (2, 15, 'b');
         CREATE VIEW big AS SELECT id, n, n * 2 AS twice FROM t WHERE n > 10",
    );
    session
}

#[test]
fn simple_views_take_inserts_updates_and_deletes() {
    let store = MemoryStore::default();
    let mut session = with_table(&store);
    assert_eq!(answer(&mut session, "UPDATE big SET n = n + 1"), "UPDATE 1");
    assert_eq!(
        answer(&mut session, "SELECT n FROM t ORDER BY id"),
        "n\n5\n16\nSELECT 2"
    );
    assert_eq!(
        answer(&mut session, "INSERT INTO big (id, n) VALUES (3, 20)"),
        "INSERT 0 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT label FROM t WHERE id = 3"),
        "label\nnone\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "DELETE FROM big WHERE id = 3"),
        "DELETE 1"
    );
    assert_eq!(
        answer(&mut session, "UPDATE big SET n = 1 RETURNING id, twice"),
        "id|twice\n2|2\nUPDATE 1"
    );
    assert_eq!(answer(&mut session, "SELECT count(*) FROM big"), "count\n0\nSELECT 1");
    assert_eq!(state(&mut session, "UPDATE big SET twice = 1"), "0A000");
    assert_eq!(
        state(&mut session, "INSERT INTO big VALUES (4, 30, 60)"),
        "0A000"
    );
    answer(
        &mut session,
        "CREATE VIEW counts AS SELECT count(*) AS c FROM t",
    );
    assert_eq!(state(&mut session, "UPDATE counts SET c = 1"), "55000");
}

#[test]
fn check_options_refuse_rows_the_view_would_not_show() {
    let store = MemoryStore::default();
    let mut session = with_table(&store);
    answer(
        &mut session,
        "CREATE VIEW checked AS SELECT id, n FROM t WHERE n < 100 WITH CHECK OPTION",
    );
    assert_eq!(
        state(&mut session, "INSERT INTO checked VALUES (5, 500)"),
        "44000"
    );
    assert_eq!(
        answer(&mut session, "INSERT INTO checked VALUES (5, 50)"),
        "INSERT 0 1"
    );
    assert_eq!(
        state(&mut session, "UPDATE checked SET n = 1000 WHERE id = 5"),
        "44000"
    );
    answer(
        &mut session,
        "CREATE VIEW low AS SELECT * FROM checked WHERE n < 60 WITH LOCAL CHECK OPTION",
    );
    assert_eq!(
        state(&mut session, "INSERT INTO low VALUES (6, 70)"),
        "44000"
    );
    assert_eq!(
        answer(&mut session, "INSERT INTO low VALUES (7, 55)"),
        "INSERT 0 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM t WHERE id IN (5, 6, 7)"),
        "count\n2\nSELECT 1"
    );
}

#[test]
fn temporary_tables_belong_to_their_session_and_its_memory() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TEMP TABLE tt (id integer PRIMARY KEY, v text); INSERT INTO tt VALUES (1, 'x')",
    );
    assert_eq!(
        answer(&mut session, "SELECT * FROM tt"),
        "id|v\n1|x\nSELECT 1"
    );
    let mut other = Session::new(&store);
    assert_eq!(state(&mut other, "SELECT * FROM tt"), "42P01");
    answer(
        &mut session,
        "BEGIN; INSERT INTO tt VALUES (2, 'y'); ROLLBACK",
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM tt"),
        "count\n1\nSELECT 1"
    );
    answer(
        &mut session,
        "CREATE TABLE p (a integer); INSERT INTO p VALUES (1); CREATE TEMP TABLE p (a integer)",
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM p"),
        "count\n0\nSELECT 1"
    );
    answer(&mut session, "DROP TABLE p");
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM p"),
        "count\n1\nSELECT 1"
    );
    answer(
        &mut session,
        "CREATE TEMP TABLE od (a integer) ON COMMIT DELETE ROWS; BEGIN; INSERT INTO od VALUES (1)",
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM od"),
        "count\n1\nSELECT 1"
    );
    answer(&mut session, "COMMIT");
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM od"),
        "count\n0\nSELECT 1"
    );
    answer(&mut session, "CREATE TEMP VIEW tv AS SELECT v FROM tt");
    assert_eq!(answer(&mut session, "SELECT * FROM tv"), "v\nx\nSELECT 1");
    answer(&mut session, "CREATE VIEW automatic AS SELECT id FROM tt");
    assert_eq!(state(&mut other, "SELECT * FROM automatic"), "42P01");
    assert!(
        store
            .snapshot()
            .keys()
            .all(|key| !key.windows(7).any(|window| window == b"pg_temp")),
        "a temporary object reached the database"
    );
    answer(&mut session, "DISCARD TEMP");
    assert_eq!(state(&mut session, "SELECT * FROM tt"), "42P01");
}

#[test]
fn every_dialect_has_its_temporary_tables() {
    let store = MemoryStore::default();
    let mut sqlserver = Session::with_dialect(&store, Dialect::SqlServer);
    let found = run(
        &mut sqlserver,
        "CREATE TABLE #work (id INT); INSERT INTO #work VALUES (7); SELECT id FROM #work",
    )
    .unwrap();
    assert!(found.contains('7'), "{found}");
    let mut mysql = Session::with_dialect(&store, Dialect::MySql);
    run(
        &mut mysql,
        "CREATE TEMPORARY TABLE mt (a int); INSERT INTO mt VALUES (1); DROP TEMPORARY TABLE mt",
    )
    .unwrap();
    let mut sqlite = Session::with_dialect(&store, Dialect::Sqlite);
    let found = run(
        &mut sqlite,
        "CREATE TEMP TABLE st (a integer); INSERT INTO temp.st VALUES (3); SELECT a FROM st",
    )
    .unwrap();
    assert!(found.contains('3'), "{found}");
    assert!(
        store
            .snapshot()
            .keys()
            .all(|key| !key.windows(7).any(|window| window == b"pg_temp")),
        "a temporary object reached the database"
    );
}
