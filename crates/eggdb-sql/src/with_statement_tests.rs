use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Session, SqlError};

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

#[test]
fn data_modifying_with_queries_feed_the_statement_after_them() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE src (id integer PRIMARY KEY, v text);
         INSERT INTO src VALUES (1, 'a'), (2, 'b'), (3, 'c');
         CREATE TABLE dst (id integer PRIMARY KEY, v text)",
    );
    assert_eq!(
        answer(
            &mut session,
            "WITH moved AS (DELETE FROM src WHERE id >= 2 RETURNING id, v) INSERT INTO dst SELECT * FROM moved"
        ),
        "INSERT 0 2"
    );
    assert_eq!(
        answer(&mut session, "SELECT * FROM dst ORDER BY id"),
        "id|v\n2|b\n3|c\nSELECT 2"
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM src"),
        "count\n1\nSELECT 1"
    );
    assert_eq!(
        answer(
            &mut session,
            "WITH added AS (INSERT INTO src VALUES (9, 'z') RETURNING id) SELECT id * 2 AS twice FROM added"
        ),
        "twice\n18\nSELECT 1"
    );
    assert_eq!(
        answer(
            &mut session,
            "WITH t AS (SELECT 9 AS id) UPDATE src SET v = 'nine' WHERE id IN (SELECT id FROM t) RETURNING id, v"
        ),
        "id|v\n9|nine\nUPDATE 1"
    );
    assert_eq!(
        answer(
            &mut session,
            "WITH gone AS (DELETE FROM dst WHERE id = 2 RETURNING id), kept (n) AS (SELECT count(*) FROM dst) SELECT (SELECT count(*) FROM gone) AS gone"
        ),
        "gone\n1\nSELECT 1"
    );
}

#[test]
fn data_modifying_with_queries_keep_postgresqls_rules() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE t (id integer PRIMARY KEY); INSERT INTO t VALUES (1)",
    );
    assert_eq!(
        state(&mut session, "WITH d AS (DELETE FROM t) SELECT * FROM d"),
        "0A000"
    );
    assert_eq!(
        state(
            &mut session,
            "SELECT * FROM (WITH x AS (DELETE FROM t RETURNING id) SELECT * FROM x) s"
        ),
        "0A000"
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM t"),
        "count\n1\nSELECT 1"
    );
}
