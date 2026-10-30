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
        .expect_err("the block fails")
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

#[test]
fn a_predefined_exception_is_caught_by_its_name_with_sqlerrm() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE\n  v NUMBER;\nBEGIN\n  SELECT n INTO v FROM log WHERE n = 99;\nEXCEPTION\n  WHEN NO_DATA_FOUND THEN\n    INSERT INTO log VALUES (SQLCODE, SQLERRM);\nEND;",
    );
    assert_eq!(logged(&mut session), [pair("100", "ORA-01403: no data found")]);
}

#[test]
fn a_user_exception_has_sqlcode_one() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE\n  late EXCEPTION;\nBEGIN\n  RAISE late;\nEXCEPTION\n  WHEN late THEN\n    INSERT INTO log VALUES (SQLCODE, SQLERRM);\nEND;",
    );
    assert_eq!(logged(&mut session), [pair("1", "User-Defined Exception")]);
}

#[test]
fn a_handled_exception_keeps_the_work_done_before_it() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(&mut session, "CREATE TABLE k (id NUMBER PRIMARY KEY)");
    run(
        &mut session,
        "DECLARE\n  duplicate EXCEPTION;\n  PRAGMA EXCEPTION_INIT(duplicate, -1);\nBEGIN\n  INSERT INTO log VALUES (2, 'before');\n  INSERT INTO k VALUES (1);\n  INSERT INTO k VALUES (1);\nEXCEPTION\n  WHEN duplicate THEN\n    INSERT INTO log VALUES (SQLCODE, 'duplicate');\nEND;",
    );
    assert_eq!(
        logged(&mut session),
        [pair("-1", "duplicate"), pair("2", "before")]
    );
}

#[test]
fn others_catches_what_no_named_handler_does() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE\n  x NUMBER;\nBEGIN\n  x := 1 / 0;\nEXCEPTION\n  WHEN NO_DATA_FOUND THEN\n    INSERT INTO log VALUES (0, 'wrong');\n  WHEN OTHERS THEN\n    INSERT INTO log VALUES (SQLCODE, SQLERRM);\nEND;",
    );
    assert_eq!(
        logged(&mut session),
        [pair("-1476", "ORA-01476: divisor is equal to zero")]
    );
}

#[test]
fn raise_application_error_reaches_the_client_with_its_number_and_line() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    let error = failure(
        &mut session,
        "BEGIN\n  RAISE_APPLICATION_ERROR(-20001, 'salary too high');\nEND;",
    );
    assert_eq!(error.vendor_code(), Some(20001));
    assert_eq!(error.message(), "salary too high");
    assert!(
        error
            .context()
            .is_some_and(|context| context.contains("ORA-06512: at line 2")),
        "{:?}",
        error.context()
    );
}

#[test]
fn an_application_error_number_outside_its_range_is_refused() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    let error = failure(&mut session, "BEGIN RAISE_APPLICATION_ERROR(-1, 'x'); END;");
    assert_eq!(error.vendor_code(), Some(21000));
}

#[test]
fn an_unhandled_user_exception_is_ora_06510() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    let error = failure(
        &mut session,
        "DECLARE\n  stop_now EXCEPTION;\nBEGIN\n  RAISE stop_now;\nEND;",
    );
    assert_eq!(error.vendor_code(), Some(6510));
}

#[test]
fn reraise_passes_the_handled_error_on() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    let error = failure(
        &mut session,
        "BEGIN\n  BEGIN\n    RAISE TOO_MANY_ROWS;\n  EXCEPTION\n    WHEN OTHERS THEN\n      RAISE;\n  END;\nEND;",
    );
    assert_eq!(error.vendor_code(), Some(1422));
}

#[test]
fn a_local_procedure_changes_a_variable_of_its_enclosing_block() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE\n  total NUMBER := 0;\n  PROCEDURE add_to_total(amount NUMBER) IS\n  BEGIN\n    total := total + amount;\n  END add_to_total;\nBEGIN\n  add_to_total(2);\n  add_to_total(3);\n  INSERT INTO log VALUES (total, 'total');\nEND;",
    );
    assert_eq!(logged(&mut session), [pair("5", "total")]);
}

#[test]
fn a_local_procedure_returns_out_parameters() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE\n  result NUMBER;\n  PROCEDURE twice(n IN NUMBER, doubled OUT NUMBER) IS\n  BEGIN\n    doubled := n * 2;\n  END;\nBEGIN\n  twice(21, result);\n  INSERT INTO log VALUES (result, 'out');\nEND;",
    );
    assert_eq!(logged(&mut session), [pair("42", "out")]);
}

#[test]
fn a_query_loop_declares_its_record() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(&mut session, "CREATE TABLE src (n NUMBER)");
    run(&mut session, "INSERT INTO src VALUES (1)");
    run(&mut session, "INSERT INTO src VALUES (2)");
    run(
        &mut session,
        "BEGIN\n  FOR r IN (SELECT n FROM src) LOOP\n    INSERT INTO log VALUES (r.n * 10, 'loop');\n  END LOOP;\nEND;",
    );
    assert_eq!(
        logged(&mut session),
        [pair("10", "loop"), pair("20", "loop")]
    );
}

#[test]
fn a_record_type_starts_with_its_field_defaults() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE\n  TYPE pair_t IS RECORD (a NUMBER := 7, b VARCHAR2(10));\n  p pair_t;\nBEGIN\n  INSERT INTO log VALUES (p.a, NVL(p.b, 'none'));\nEND;",
    );
    assert_eq!(logged(&mut session), [pair("7", "none")]);
}

#[test]
fn a_string_too_long_for_its_variable_is_a_value_error() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE\n  short VARCHAR2(3);\nBEGIN\n  short := 'abcdef';\nEXCEPTION\n  WHEN VALUE_ERROR THEN\n    INSERT INTO log VALUES (SQLCODE, 'value');\nEND;",
    );
    assert_eq!(logged(&mut session), [pair("-6502", "value")]);
}

#[test]
fn hostile_blocks_fail_without_panicking() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    for text in [
        "BEGIN RAISE_APPLICATION_ERROR(NULL, NULL); END;",
        "DECLARE e EXCEPTION; BEGIN RAISE e; EXCEPTION WHEN e THEN RAISE; END;",
        "DECLARE TYPE r IS RECORD (a NUMBER); v r; BEGIN v.b := 1; END;",
        "DECLARE PROCEDURE p IS BEGIN RAISE NO_DATA_FOUND; END; BEGIN p; END;",
    ] {
        assert!(session.execute(text, &mut |_| {}).is_err(), "{text}");
    }
}
