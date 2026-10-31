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

fn values(session: &mut Session<'_, MemoryStore>) -> String {
    answer(session, "SELECT v FROM t ORDER BY v")
}

#[test]
fn rollback_to_a_savepoint_undoes_only_what_came_after_it() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(&mut session, "CREATE TABLE t (v integer PRIMARY KEY)");
    answer(
        &mut session,
        "BEGIN; INSERT INTO t VALUES (1); SAVEPOINT a; INSERT INTO t VALUES (2);
         SAVEPOINT b; INSERT INTO t VALUES (3); UPDATE t SET v = 10 WHERE v = 1",
    );
    answer(&mut session, "ROLLBACK TO SAVEPOINT b");
    assert_eq!(values(&mut session), "v\n1\n2\nSELECT 2");
    answer(&mut session, "ROLLBACK TO a");
    assert_eq!(values(&mut session), "v\n1\nSELECT 1");
    answer(&mut session, "INSERT INTO t VALUES (4); RELEASE SAVEPOINT a; COMMIT");
    assert_eq!(values(&mut session), "v\n1\n4\nSELECT 2");
}

#[test]
fn a_failed_statement_after_a_savepoint_can_be_rolled_back_to_it() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(&mut session, "CREATE TABLE t (v integer PRIMARY KEY)");
    answer(&mut session, "BEGIN; INSERT INTO t VALUES (1); SAVEPOINT s");
    let error = run(&mut session, "INSERT INTO t VALUES (2), (1)").unwrap_err();
    assert_eq!(error.state().code(), "23505");
    let aborted = run(&mut session, "SELECT 1").unwrap_err();
    assert_eq!(aborted.state().code(), "25P02");
    answer(&mut session, "ROLLBACK TO SAVEPOINT s");
    answer(&mut session, "INSERT INTO t VALUES (3); COMMIT");
    assert_eq!(values(&mut session), "v\n1\n3\nSELECT 2");
}

#[test]
fn savepoints_follow_each_dialect() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let error = run(&mut session, "SAVEPOINT s").unwrap_err();
    assert_eq!(error.state().code(), "25P01");
    answer(&mut session, "BEGIN");
    let error = run(&mut session, "ROLLBACK TO missing").unwrap_err();
    assert_eq!(error.state().code(), "3B001");
    answer(&mut session, "ROLLBACK");

    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    answer(&mut session, "CREATE TABLE t (v INTEGER PRIMARY KEY)");
    answer(&mut session, "SAVEPOINT outer; INSERT INTO t VALUES (5); RELEASE outer");
    assert_eq!(values(&mut session), "v\n5\nSELECT 1");

    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    answer(&mut session, "CREATE TABLE t (v INT PRIMARY KEY)");
    answer(
        &mut session,
        "START TRANSACTION; INSERT INTO t VALUES (1); SAVEPOINT s; INSERT INTO t VALUES (2);
         ROLLBACK TO SAVEPOINT s; COMMIT",
    );
    assert_eq!(values(&mut session), "v\n1\nSELECT 1");

    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    answer(&mut session, "CREATE TABLE t (v INT PRIMARY KEY)");
    answer(
        &mut session,
        "BEGIN TRANSACTION; INSERT INTO t VALUES (1); SAVE TRANSACTION s;
         INSERT INTO t VALUES (2); ROLLBACK TRANSACTION s; COMMIT",
    );
    assert_eq!(values(&mut session), "v\n1\n(1 row affected)");
}

#[test]
fn row_locking_clauses_and_lock_table_are_accepted() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(&mut session, "CREATE TABLE t (v integer PRIMARY KEY); INSERT INTO t VALUES (1)");
    answer(&mut session, "BEGIN");
    for sql in [
        "SELECT v FROM t FOR UPDATE",
        "SELECT v FROM t WHERE v = 1 FOR NO KEY UPDATE NOWAIT",
        "SELECT v FROM t FOR SHARE OF t SKIP LOCKED",
        "SELECT v FROM t FOR KEY SHARE",
        "LOCK TABLE t IN ACCESS EXCLUSIVE MODE NOWAIT",
    ] {
        answer(&mut session, sql);
    }
    answer(&mut session, "COMMIT");
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    answer(&mut session, "SELECT v FROM t LOCK IN SHARE MODE");
    answer(&mut session, "SELECT v FROM t FOR UPDATE SKIP LOCKED");
}
