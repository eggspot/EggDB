use crate::memory_store::MemoryStore;
use crate::{Dialect, QueryResult, Session, SqlError};

fn session(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = Session::with_dialect(store, Dialect::Oracle);
    run(
        &mut session,
        "CREATE TABLE emp (id NUMBER, name VARCHAR2(40), sal NUMBER)",
    );
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

fn texts(session: &mut Session<'_, MemoryStore>, query: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    session
        .execute(query, &mut |result: QueryResult| {
            for row in result.rows {
                rows.push(
                    row.iter()
                        .map(|value| value.to_text().unwrap_or_else(|| "NULL".to_owned()))
                        .collect(),
                );
            }
        })
        .unwrap_or_else(|error| panic!("{query}: {error}"));
    rows
}

fn messages(session: &mut Session<'_, MemoryStore>) -> Vec<String> {
    texts(session, "SELECT msg FROM log ORDER BY n")
        .into_iter()
        .filter_map(|row| row.into_iter().next())
        .collect()
}

fn row(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

const LOGGING_TRIGGER: &str = "CREATE OR REPLACE TRIGGER emp_audit
AFTER INSERT OR UPDATE OR DELETE ON emp
FOR EACH ROW
DECLARE
  n NUMBER;
BEGIN
  SELECT COUNT(*) + 1 INTO n FROM log;
  IF INSERTING THEN
    INSERT INTO log VALUES (n, 'insert ' || :new.name);
  ELSIF UPDATING THEN
    INSERT INTO log VALUES (n, 'update ' || :old.sal || ' to ' || :new.sal);
  ELSIF DELETING THEN
    INSERT INTO log VALUES (n, 'delete ' || :old.name);
  END IF;
END;";

#[test]
fn a_before_row_trigger_changes_the_new_row() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "CREATE TRIGGER emp_names BEFORE INSERT OR UPDATE ON emp FOR EACH ROW
BEGIN
  :new.name := UPPER(:new.name);
  IF :new.sal IS NULL THEN
    :new.sal := 1000;
  END IF;
END;",
    );
    run(&mut session, "INSERT INTO emp (id, name) VALUES (1, 'king')");
    run(&mut session, "UPDATE emp SET name = 'blake' WHERE id = 1");
    assert_eq!(
        texts(&mut session, "SELECT id, name, sal FROM emp"),
        vec![row(&["1", "BLAKE", "1000"])]
    );
}

#[test]
fn an_after_row_trigger_sees_the_event_and_both_rows() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(&mut session, LOGGING_TRIGGER);
    run(&mut session, "INSERT INTO emp VALUES (1, 'KING', 5000)");
    run(&mut session, "UPDATE emp SET sal = 6000 WHERE id = 1");
    run(&mut session, "DELETE FROM emp WHERE id = 1");
    assert_eq!(
        messages(&mut session),
        row(&["insert KING", "update 5000 to 6000", "delete KING"])
    );
}

#[test]
fn updating_a_column_tells_which_columns_the_update_sets() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "CREATE TRIGGER emp_sal AFTER UPDATE ON emp FOR EACH ROW
BEGIN
  IF UPDATING('SAL') THEN
    INSERT INTO log VALUES (1, 'salary changed');
  ELSE
    INSERT INTO log VALUES (2, 'other change');
  END IF;
END;",
    );
    run(&mut session, "INSERT INTO emp VALUES (1, 'KING', 5000)");
    run(&mut session, "UPDATE emp SET name = 'BLAKE'");
    run(&mut session, "UPDATE emp SET sal = 10");
    assert_eq!(
        messages(&mut session),
        row(&["salary changed", "other change"])
    );
}

#[test]
fn update_of_columns_and_a_when_clause_with_referencing_names_filter_the_rows() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "CREATE TRIGGER emp_raise
AFTER UPDATE OF sal ON emp
REFERENCING NEW AS nw OLD AS od
FOR EACH ROW
WHEN (nw.sal > od.sal)
BEGIN
  INSERT INTO log VALUES (:nw.id, 'raise for ' || :nw.name);
END;",
    );
    run(&mut session, "INSERT INTO emp VALUES (1, 'KING', 5000)");
    run(&mut session, "INSERT INTO emp VALUES (2, 'BLAKE', 3000)");
    run(&mut session, "UPDATE emp SET name = 'SCOTT' WHERE id = 2");
    run(
        &mut session,
        "UPDATE emp SET sal = CASE id WHEN 1 THEN 4000 ELSE 3500 END",
    );
    assert_eq!(messages(&mut session), row(&["raise for SCOTT"]));
}

#[test]
fn a_statement_trigger_fires_once_per_statement() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "CREATE TRIGGER emp_changes AFTER INSERT OR DELETE ON emp
DECLARE
  n NUMBER;
BEGIN
  SELECT COUNT(*) + 1 INTO n FROM log;
  IF DELETING THEN
    INSERT INTO log VALUES (n, 'rows deleted');
  ELSE
    INSERT INTO log VALUES (n, 'rows inserted');
  END IF;
END;",
    );
    run(&mut session, "INSERT INTO emp VALUES (1, 'KING', 5000)");
    run(
        &mut session,
        "INSERT INTO emp SELECT id + 1, name, sal FROM emp",
    );
    run(&mut session, "DELETE FROM emp");
    assert_eq!(
        messages(&mut session),
        row(&["rows inserted", "rows inserted", "rows deleted"])
    );
}

#[test]
fn an_error_in_a_trigger_stops_the_statement_with_the_trigger_named() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "CREATE TRIGGER emp_check BEFORE INSERT ON emp FOR EACH ROW
BEGIN
  IF :new.sal > 10000 THEN
    RAISE_APPLICATION_ERROR(-20001, 'salary too high');
  END IF;
END;",
    );
    let error = failure(&mut session, "INSERT INTO emp VALUES (1, 'KING', 50000)");
    assert_eq!(error.vendor_code(), Some(20001));
    assert_eq!(error.message(), "salary too high");
    let context = error.context().unwrap_or_default().to_owned();
    assert!(context.contains("EMP_CHECK\", line 3"), "{context}");
    assert!(
        context.contains("ORA-04088: error during execution of trigger '")
            && context.contains("EMP_CHECK'"),
        "{context}"
    );
    assert!(texts(&mut session, "SELECT id FROM emp").is_empty());
}

#[test]
fn row_triggers_fire_for_dml_inside_a_plsql_block() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(&mut session, LOGGING_TRIGGER);
    run(
        &mut session,
        "BEGIN
  FOR i IN 1..3 LOOP
    INSERT INTO emp VALUES (i, 'E' || i, i * 100);
  END LOOP;
  UPDATE emp SET sal = 0 WHERE id = 3;
END;",
    );
    assert_eq!(
        messages(&mut session),
        row(&["insert E1", "insert E2", "insert E3", "update 300 to 0"])
    );
}

#[test]
fn alter_trigger_disables_enables_renames_and_drop_removes_it() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(&mut session, LOGGING_TRIGGER);
    run(&mut session, "ALTER TRIGGER emp_audit DISABLE");
    run(&mut session, "INSERT INTO emp VALUES (1, 'KING', 5000)");
    run(&mut session, "ALTER TRIGGER emp_audit ENABLE");
    run(&mut session, "ALTER TRIGGER emp_audit COMPILE");
    run(&mut session, "INSERT INTO emp VALUES (2, 'BLAKE', 3000)");
    run(&mut session, "ALTER TABLE emp DISABLE ALL TRIGGERS");
    run(&mut session, "INSERT INTO emp VALUES (3, 'SCOTT', 3000)");
    run(&mut session, "ALTER TABLE emp ENABLE ALL TRIGGERS");
    run(&mut session, "ALTER TRIGGER emp_audit RENAME TO emp_history");
    run(&mut session, "INSERT INTO emp VALUES (4, 'FORD', 3000)");
    run(&mut session, "DROP TRIGGER emp_history");
    run(&mut session, "INSERT INTO emp VALUES (5, 'ADAMS', 1100)");
    assert_eq!(messages(&mut session), row(&["insert BLAKE", "insert FORD"]));
    let missing = failure(&mut session, "DROP TRIGGER emp_history");
    assert_eq!(missing.vendor_code(), Some(4080));
    assert_eq!(missing.message(), "trigger 'EMP_HISTORY' does not exist");
    assert_eq!(
        failure(&mut session, "ALTER TRIGGER nothing ENABLE").vendor_code(),
        Some(4080)
    );
}

#[test]
fn a_second_trigger_with_the_same_name_needs_or_replace() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(&mut session, LOGGING_TRIGGER);
    let duplicate = failure(
        &mut session,
        "CREATE TRIGGER emp_audit AFTER DELETE ON log BEGIN NULL; END;",
    );
    assert_eq!(duplicate.vendor_code(), Some(4081));
    run(
        &mut session,
        "CREATE OR REPLACE TRIGGER emp_audit AFTER DELETE ON log BEGIN NULL; END;",
    );
    run(&mut session, "INSERT INTO emp VALUES (1, 'KING', 5000)");
    assert!(messages(&mut session).is_empty());
}

#[test]
fn an_instead_of_trigger_writes_through_a_view() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "CREATE VIEW emp_names AS SELECT id, name FROM emp",
    );
    run(
        &mut session,
        "CREATE TRIGGER emp_names_insert INSTEAD OF INSERT ON emp_names
BEGIN
  INSERT INTO emp VALUES (:new.id, :new.name, 0);
END;",
    );
    run(&mut session, "INSERT INTO emp_names VALUES (7, 'MILLER')");
    assert_eq!(
        texts(&mut session, "SELECT id, name, sal FROM emp"),
        vec![row(&["7", "MILLER", "0"])]
    );
}

#[test]
fn commit_inside_a_trigger_is_refused() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "CREATE TRIGGER emp_commit AFTER INSERT ON emp FOR EACH ROW BEGIN COMMIT; END;",
    );
    let error = failure(&mut session, "INSERT INTO emp VALUES (1, 'KING', 5000)");
    assert_eq!(error.vendor_code(), Some(4092));
    assert_eq!(error.message(), "cannot COMMIT in a trigger");
}

#[test]
fn a_call_body_runs_a_procedure_for_each_row() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "CREATE PROCEDURE note_hire(p_id NUMBER, p_name VARCHAR2) IS
BEGIN
  INSERT INTO log VALUES (p_id, 'hired ' || p_name);
END;",
    );
    run(
        &mut session,
        "CREATE TRIGGER emp_hire AFTER INSERT ON emp FOR EACH ROW CALL note_hire(:new.id, :new.name)",
    );
    run(&mut session, "INSERT INTO emp VALUES (1, 'KING', 5000)");
    assert_eq!(messages(&mut session), row(&["hired KING"]));
}

#[test]
fn trigger_definitions_oracle_refuses_are_refused_at_create() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    let cases: [(&str, Option<u16>); 7] = [
        (
            "CREATE TRIGGER t1 AFTER INSERT ON emp BEGIN INSERT INTO log VALUES (:new.id, 'x'); END;",
            Some(4082),
        ),
        (
            "CREATE TRIGGER t2 AFTER INSERT ON emp FOR EACH ROW BEGIN :new.sal := 1; END;",
            Some(4084),
        ),
        (
            "CREATE TRIGGER t3 BEFORE UPDATE ON emp FOR EACH ROW BEGIN :old.sal := 1; END;",
            Some(4085),
        ),
        (
            "CREATE TRIGGER t4 AFTER INSERT ON emp WHEN (new.sal > 0) BEGIN NULL; END;",
            Some(4077),
        ),
        (
            "CREATE TRIGGER t5 INSTEAD OF INSERT ON emp BEGIN NULL; END;",
            Some(25002),
        ),
        (
            "CREATE TRIGGER t6 BEFORE INSERT ON emp FOR EACH ROW BEGIN :new.salary := 1; END;",
            Some(6550),
        ),
        (
            "CREATE TRIGGER t7 BEFORE INSERT ON emp FOR EACH ROW BEGIN undeclared_variable := 1; END;",
            Some(6550),
        ),
    ];
    for (text, code) in cases {
        let error = failure(&mut session, text);
        assert_eq!(error.vendor_code(), code, "{text}: {error}");
    }
    let bad_bind = failure(
        &mut session,
        "CREATE TRIGGER t8 BEFORE INSERT ON emp FOR EACH ROW BEGIN :new.salary := 1; END;",
    );
    assert!(
        bad_bind.message().contains("PLS-00049: bad bind variable 'NEW.SALARY'"),
        "{bad_bind}"
    );
    let compound = failure(
        &mut session,
        "CREATE TRIGGER t9 FOR INSERT ON emp COMPOUND TRIGGER BEFORE STATEMENT IS BEGIN NULL; END BEFORE STATEMENT; END;",
    );
    assert_eq!(compound.state(), crate::SqlState::FEATURE_NOT_SUPPORTED);
    let ddl = failure(
        &mut session,
        "CREATE TRIGGER t10 AFTER LOGON ON DATABASE BEGIN NULL; END;",
    );
    assert_eq!(ddl.state(), crate::SqlState::FEATURE_NOT_SUPPORTED);
    let missing_column = failure(
        &mut session,
        "CREATE TRIGGER t11 AFTER UPDATE OF salary ON emp FOR EACH ROW BEGIN NULL; END;",
    );
    assert_eq!(missing_column.vendor_code(), Some(904));
}

#[test]
fn hostile_trigger_text_returns_errors() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    for text in [
        "CREATE TRIGGER",
        "CREATE TRIGGER t",
        "CREATE TRIGGER t BEFORE",
        "CREATE TRIGGER t BEFORE INSERT ON",
        "CREATE TRIGGER t BEFORE INSERT ON emp REFERENCING",
        "CREATE TRIGGER t BEFORE INSERT ON emp FOR EACH ROW WHEN (",
        "CREATE TRIGGER t BEFORE INSERT ON emp FOR EACH ROW BEGIN",
        "CREATE TRIGGER t BEFORE INSERT ON emp FOR EACH ROW CALL",
        "CREATE TRIGGER t BEFORE INSERT ON emp FOLLOWS",
        "ALTER TRIGGER",
        "ALTER TRIGGER t RENAME",
        "DROP TRIGGER",
        "ALTER TABLE emp ENABLE ALL",
    ] {
        assert!(session.execute(text, &mut |_| {}).is_err(), "{text}");
    }
}
