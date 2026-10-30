use crate::memory_store::MemoryStore;
use crate::{Dialect, QueryResult, Session, SqlError};

fn session(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = Session::with_dialect(store, Dialect::Oracle);
    run(&mut session, "CREATE TABLE log (n NUMBER, msg VARCHAR2(200))");
    session
}

fn run(session: &mut Session<'_, MemoryStore>, text: &str) {
    session
        .execute(text, &mut |_: QueryResult| {})
        .unwrap_or_else(|error| panic!("{text}: {error}"));
}

fn failure(session: &mut Session<'_, MemoryStore>, text: &str) -> SqlError {
    session
        .execute(text, &mut |_| {})
        .expect_err("the statement fails")
}

fn single(session: &mut Session<'_, MemoryStore>, text: &str) -> String {
    let mut found = None;
    session
        .execute(text, &mut |result: QueryResult| {
            if let Some(value) = result.rows.first().and_then(|row| row.first()) {
                found = Some(value.to_text().unwrap_or_else(|| "NULL".to_owned()));
            }
        })
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    found.unwrap_or_else(|| panic!("{text}: no value"))
}

fn logged(session: &mut Session<'_, MemoryStore>) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    session
        .execute("SELECT n, msg FROM log ORDER BY n", &mut |result: QueryResult| {
            for row in result.rows {
                let text = |at: usize| {
                    row.get(at)
                        .and_then(|value| value.to_text())
                        .unwrap_or_else(|| "NULL".to_owned())
                };
                rows.push((text(0), text(1)));
            }
        })
        .unwrap();
    rows
}

fn pair(n: &str, msg: &str) -> (String, String) {
    (n.to_owned(), msg.to_owned())
}

const LOG_IT: &str = "CREATE OR REPLACE PROCEDURE log_it (\n  p_n   IN log.n%TYPE,\n  p_msg IN log.msg%TYPE DEFAULT 'none'\n) AS\nBEGIN\n  INSERT INTO log VALUES (p_n, p_msg);\nEND log_it;";

const DOUBLE_IT: &str =
    "CREATE OR REPLACE FUNCTION double_it (n IN NUMBER) RETURN NUMBER IS\nBEGIN\n  RETURN n * 2;\nEND;";

#[test]
fn call_binds_positional_named_and_default_arguments() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(&mut session, LOG_IT);
    run(&mut session, "CALL log_it(1, 'positional')");
    run(&mut session, "CALL log_it(p_msg => 'named', p_n => 2)");
    run(&mut session, "CALL log_it(3)");
    assert_eq!(
        logged(&mut session),
        [pair("1", "positional"), pair("2", "named"), pair("3", "none")]
    );
}

#[test]
fn a_block_calls_a_stored_procedure_as_a_statement() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(&mut session, LOG_IT);
    run(
        &mut session,
        "BEGIN\n  log_it(1, 'from a block');\n  log_it(p_n => 2);\nEND;",
    );
    assert_eq!(
        logged(&mut session),
        [pair("1", "from a block"), pair("2", "none")]
    );
}

#[test]
fn a_stored_function_runs_in_sql_and_in_plsql() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(&mut session, DOUBLE_IT);
    assert_eq!(single(&mut session, "SELECT double_it(21) FROM dual"), "42");
    assert_eq!(single(&mut session, "SELECT double_it(n => 5) FROM dual"), "10");
    run(&mut session, "INSERT INTO log VALUES (4, 'row')");
    assert_eq!(
        single(&mut session, "SELECT double_it(n) FROM log WHERE n = 4"),
        "8"
    );
    run(
        &mut session,
        "DECLARE\n  v NUMBER;\nBEGIN\n  v := double_it(50);\n  INSERT INTO log VALUES (v, 'assigned');\nEND;",
    );
    assert!(logged(&mut session).contains(&pair("100", "assigned")));
}

#[test]
fn a_local_function_is_called_from_sql_inside_its_block() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE\n  FUNCTION triple (n NUMBER) RETURN NUMBER IS\n  BEGIN\n    RETURN n * 3;\n  END;\nBEGIN\n  INSERT INTO log SELECT triple(7), 'local' FROM dual;\nEND;",
    );
    assert_eq!(logged(&mut session), [pair("21", "local")]);
}

#[test]
fn calls_that_match_no_routine_get_oracle_errors() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(&mut session, DOUBLE_IT);
    run(&mut session, LOG_IT);
    let wrong = failure(&mut session, "SELECT double_it(1, 2) FROM dual");
    assert_eq!(wrong.vendor_code(), Some(6553), "{wrong}");
    assert!(wrong.message().contains("PLS-306"), "{wrong}");
    let unknown = failure(&mut session, "SELECT no_such_function(1) FROM dual");
    assert_eq!(unknown.vendor_code(), Some(904), "{unknown}");
    let missing = failure(&mut session, "CALL no_such_procedure(1)");
    assert_eq!(missing.vendor_code(), Some(6576), "{missing}");
    let function = failure(&mut session, "CALL double_it(1)");
    assert_eq!(function.vendor_code(), Some(6576), "{function}");
    let procedure = failure(&mut session, "SELECT log_it(1) FROM dual");
    assert_eq!(procedure.vendor_code(), Some(6553), "{procedure}");
    assert!(procedure.message().contains("PLS-221"), "{procedure}");
}

#[test]
fn commit_inside_a_block_keeps_the_transaction() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(&mut session, "INSERT INTO log VALUES (1, 'before')");
    run(
        &mut session,
        "BEGIN\n  INSERT INTO log VALUES (2, 'inside');\n  COMMIT;\nEND;",
    );
    run(&mut session, "ROLLBACK");
    assert_eq!(
        logged(&mut session),
        [pair("1", "before"), pair("2", "inside")]
    );
}

#[test]
fn rollback_inside_a_procedure_undoes_the_transaction() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "CREATE PROCEDURE undo_all AS\nBEGIN\n  INSERT INTO log VALUES (2, 'inside');\n  ROLLBACK;\nEND;",
    );
    run(&mut session, "INSERT INTO log VALUES (1, 'before')");
    run(&mut session, "CALL undo_all()");
    assert_eq!(logged(&mut session), []);
    run(&mut session, "INSERT INTO log VALUES (3, 'after')");
    run(&mut session, "COMMIT");
    assert_eq!(logged(&mut session), [pair("3", "after")]);
}

#[test]
fn a_function_called_from_sql_cannot_commit() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "CREATE FUNCTION commits RETURN NUMBER IS\nBEGIN\n  COMMIT;\n  RETURN 1;\nEND;",
    );
    let error = failure(&mut session, "SELECT commits() FROM dual");
    assert_eq!(error.vendor_code(), Some(14552), "{error}");
}

#[test]
fn execute_immediate_runs_plsql_blocks_and_creates_routines() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "BEGIN\n  EXECUTE IMMEDIATE 'CREATE PROCEDURE note (p VARCHAR2) AS BEGIN INSERT INTO log VALUES (1, p); END;';\n  EXECUTE IMMEDIATE 'BEGIN note(''dynamic''); END;';\nEND;",
    );
    assert_eq!(logged(&mut session), [pair("1", "dynamic")]);
}

#[test]
fn hostile_calls_fail_without_panicking() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(&mut session, DOUBLE_IT);
    for text in [
        "SELECT double_it( FROM dual",
        "SELECT double_it(n => ) FROM dual",
        "SELECT double_it(n => 1, 2) FROM dual",
        "SELECT double_it(n => 1, n => 2) FROM dual",
        "CALL double_it(n => 'x')",
        "SELECT double_it('not a number') FROM dual",
        "BEGIN EXECUTE IMMEDIATE 'BEGIN'; END;",
    ] {
        assert!(session.execute(text, &mut |_| {}).is_err(), "{text}");
    }
}
