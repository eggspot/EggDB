use crate::memory_store::MemoryStore;
use crate::{BatchStep, Dialect, Session, SqlError, SqlState};

fn run(session: &mut Session<'_>, text: &str) {
    session
        .execute(text, &mut |_| {})
        .unwrap_or_else(|error| panic!("{text}: {error}"));
}

fn fails(session: &mut Session<'_>, text: &str) -> SqlError {
    match session.execute(text, &mut |_| {}) {
        Ok(()) => panic!("{text} ran"),
        Err(error) => error,
    }
}

fn shown(session: &mut Session<'_>, query: &str) -> Vec<String> {
    let mut rows = Vec::new();
    session
        .execute(query, &mut |result| rows = result.rows)
        .unwrap_or_else(|error| panic!("{query}: {error}"));
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|value| value.to_text().unwrap_or_else(|| "NULL".to_owned()))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn batch(session: &mut Session<'_>, text: &str) -> Vec<BatchStep> {
    let mut steps = Vec::new();
    session
        .execute_batch(text, &mut |step| {
            steps.push(step);
            true
        })
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    steps
}

fn errors(steps: &[BatchStep]) -> Vec<Option<u16>> {
    steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().err())
        .map(SqlError::vendor_code)
        .collect()
}

#[test]
fn mysql_undoes_only_the_failing_statement_and_the_transaction_commits_the_rest() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    run(&mut session, "CREATE TABLE t (id INT PRIMARY KEY)");
    run(&mut session, "START TRANSACTION");
    run(&mut session, "INSERT INTO t VALUES (1)");
    assert_eq!(
        fails(&mut session, "INSERT INTO t VALUES (1)").vendor_code(),
        Some(1062)
    );
    assert!(session.in_block());
    run(&mut session, "INSERT INTO t VALUES (2)");
    assert_eq!(
        fails(&mut session, "INSERT INTO t VALUES (3), (4), (1)").vendor_code(),
        Some(1062)
    );
    assert!(session.in_block());
    assert_eq!(
        shown(&mut session, "SELECT id FROM t ORDER BY id"),
        ["1", "2"]
    );
    run(&mut session, "COMMIT");
    assert_eq!(
        shown(&mut session, "SELECT id FROM t ORDER BY id"),
        ["1", "2"]
    );
}

#[test]
fn mysql_undoes_a_failing_statement_after_a_savepoint_and_rollback_to_still_works() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    run(&mut session, "CREATE TABLE t (id INT PRIMARY KEY)");
    run(&mut session, "START TRANSACTION");
    run(&mut session, "INSERT INTO t VALUES (1)");
    run(&mut session, "SAVEPOINT a");
    run(&mut session, "INSERT INTO t VALUES (2)");
    fails(&mut session, "INSERT INTO t VALUES (3), (1)");
    assert!(!session.failed_after_savepoint());
    assert_eq!(
        shown(&mut session, "SELECT id FROM t ORDER BY id"),
        ["1", "2"]
    );
    run(&mut session, "INSERT INTO t VALUES (4)");
    run(&mut session, "ROLLBACK TO SAVEPOINT a");
    run(&mut session, "INSERT INTO t VALUES (5)");
    run(&mut session, "COMMIT");
    assert_eq!(
        shown(&mut session, "SELECT id FROM t ORDER BY id"),
        ["1", "5"]
    );
}

#[test]
fn mysql_undoes_a_failing_update_and_keeps_the_rows_it_had_not_reached() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    run(
        &mut session,
        "CREATE TABLE t (id INT PRIMARY KEY, code INT NOT NULL UNIQUE, note VARCHAR(10))",
    );
    run(
        &mut session,
        "INSERT INTO t VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c')",
    );
    run(&mut session, "START TRANSACTION");
    run(&mut session, "UPDATE t SET note = 'kept' WHERE id = 3");
    assert_eq!(
        fails(
            &mut session,
            "UPDATE t SET note = 'gone', code = CASE WHEN id = 3 THEN 20 ELSE code END WHERE id IN (1, 3)"
        )
        .vendor_code(),
        Some(1062)
    );
    run(&mut session, "COMMIT");
    assert_eq!(
        shown(&mut session, "SELECT id, code, note FROM t ORDER BY id"),
        ["1|10|a", "2|20|b", "3|30|kept"]
    );
}

#[test]
fn sqlite_aborts_the_statement_and_keeps_the_transaction() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    run(&mut session, "CREATE TABLE t (id INTEGER PRIMARY KEY)");
    run(&mut session, "BEGIN");
    run(&mut session, "INSERT INTO t VALUES (1)");
    assert_eq!(
        fails(&mut session, "INSERT INTO t VALUES (1)").vendor_code(),
        Some(1555)
    );
    run(&mut session, "INSERT INTO t VALUES (2)");
    fails(&mut session, "INSERT INTO t VALUES (3), (1)");
    fails(&mut session, "INSERT OR ABORT INTO t VALUES (4), (1)");
    assert!(session.in_block());
    run(&mut session, "COMMIT");
    assert_eq!(
        shown(&mut session, "SELECT id FROM t ORDER BY id"),
        ["1", "2"]
    );
}

#[test]
fn sqlite_insert_or_fail_keeps_the_rows_before_the_conflict_and_or_rollback_ends_the_transaction() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    run(&mut session, "CREATE TABLE t (id INTEGER PRIMARY KEY)");
    run(&mut session, "INSERT INTO t VALUES (1)");
    run(&mut session, "BEGIN");
    fails(&mut session, "INSERT OR FAIL INTO t VALUES (5), (1), (6)");
    assert!(session.in_block());
    run(&mut session, "COMMIT");
    fails(&mut session, "INSERT OR FAIL INTO t VALUES (7), (1), (8)");
    assert_eq!(
        shown(&mut session, "SELECT id FROM t ORDER BY id"),
        ["1", "5", "7"]
    );
    run(&mut session, "BEGIN");
    run(&mut session, "INSERT INTO t VALUES (10)");
    fails(&mut session, "INSERT OR ROLLBACK INTO t VALUES (11), (1)");
    assert!(!session.in_block());
    assert_eq!(
        shown(&mut session, "SELECT id FROM t ORDER BY id"),
        ["1", "5", "7"]
    );
}

#[test]
fn oracle_rolls_back_to_the_statement_and_the_transaction_commits_the_rest() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    run(&mut session, "CREATE TABLE t (id NUMBER PRIMARY KEY)");
    run(&mut session, "INSERT INTO t VALUES (1)");
    assert_eq!(
        fails(&mut session, "INSERT INTO t VALUES (1)").vendor_code(),
        Some(1)
    );
    run(&mut session, "INSERT INTO t VALUES (2)");
    assert_eq!(
        fails(
            &mut session,
            "INSERT INTO t SELECT 3 FROM dual UNION ALL SELECT 1 FROM dual"
        )
        .vendor_code(),
        Some(1)
    );
    assert!(session.in_block());
    run(&mut session, "COMMIT");
    assert_eq!(
        shown(&mut session, "SELECT id FROM t ORDER BY id"),
        ["1", "2"]
    );
}

#[test]
fn sql_server_ends_only_the_statement_with_xact_abort_off() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    batch(&mut session, "CREATE TABLE t (id INT PRIMARY KEY)");
    let steps = batch(
        &mut session,
        "BEGIN TRAN\nINSERT INTO t VALUES (1)\nINSERT INTO t VALUES (1)\nINSERT INTO t VALUES (2)\nINSERT INTO t VALUES (3), (1)\nCOMMIT",
    );
    assert_eq!(errors(&steps), [Some(2627), Some(2627)]);
    assert!(
        steps
            .iter()
            .filter(|step| step.outcome.is_err())
            .all(|step| step.in_transaction)
    );
    assert_eq!(
        shown(&mut session, "SELECT id FROM t ORDER BY id"),
        ["1", "2"]
    );
}

#[test]
fn sql_server_rolls_back_the_whole_transaction_with_xact_abort_on() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    batch(&mut session, "CREATE TABLE t (id INT PRIMARY KEY)");
    let steps = batch(
        &mut session,
        "SET XACT_ABORT ON\nBEGIN TRAN\nINSERT INTO t VALUES (1)\nINSERT INTO t VALUES (2), (1)\nINSERT INTO t VALUES (3)\nCOMMIT",
    );
    assert_eq!(errors(&steps), [Some(2627)]);
    assert!(!session.in_block());
    assert_eq!(
        shown(
            &mut session,
            "SELECT COUNT(*) AS n, @@TRANCOUNT AS open FROM t"
        ),
        ["0|0"]
    );
}

#[test]
fn postgresql_keeps_its_aborted_transaction() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(&mut session, "CREATE TABLE t (id integer PRIMARY KEY)");
    run(&mut session, "BEGIN");
    run(&mut session, "INSERT INTO t VALUES (1)");
    assert_eq!(
        fails(&mut session, "INSERT INTO t VALUES (1)").state(),
        SqlState::UNIQUE_VIOLATION
    );
    assert_eq!(
        fails(&mut session, "INSERT INTO t VALUES (2)").state(),
        SqlState::IN_FAILED_SQL_TRANSACTION
    );
    run(&mut session, "COMMIT");
    assert!(shown(&mut session, "SELECT id FROM t").is_empty());
}
