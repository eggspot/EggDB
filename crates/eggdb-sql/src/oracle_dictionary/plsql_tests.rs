use crate::memory_store::MemoryStore;
use crate::{Dialect, QueryResult, Session};

const PROCEDURE: &str = "CREATE OR REPLACE PROCEDURE raise_salary(p_id IN NUMBER, p_amount IN NUMBER DEFAULT 100) IS
BEGIN
  UPDATE emp SET sal = sal + p_amount WHERE id = p_id;
END raise_salary;";

const FUNCTION: &str = "CREATE OR REPLACE FUNCTION bonus(p_sal emp.sal%TYPE) RETURN NUMBER IS
BEGIN
  RETURN p_sal * 0.1;
END;";

const SPECIFICATION: &str = "CREATE OR REPLACE PACKAGE emp_api AS
  FUNCTION total RETURN NUMBER;
  PROCEDURE hire(p_name VARCHAR2);
  PROCEDURE hire(p_name VARCHAR2, p_sal NUMBER);
END emp_api;";

const BODY: &str = "CREATE OR REPLACE PACKAGE BODY emp_api AS
  FUNCTION total RETURN NUMBER IS
    n NUMBER;
  BEGIN
    SELECT COUNT(*) INTO n FROM emp;
    RETURN n;
  END total;
  PROCEDURE hire(p_name VARCHAR2) IS
  BEGIN
    INSERT INTO emp VALUES (1, p_name, NULL);
  END hire;
  PROCEDURE hire(p_name VARCHAR2, p_sal NUMBER) IS
  BEGIN
    INSERT INTO emp VALUES (2, p_name, p_sal);
  END hire;
END emp_api;";

const TRIGGER: &str = "CREATE OR REPLACE TRIGGER emp_check
BEFORE INSERT OR UPDATE OF sal ON emp
FOR EACH ROW
WHEN (new.sal < 0)
BEGIN
  :new.sal := 0;
END;";

fn run(session: &mut Session<'_, MemoryStore>, text: &str) {
    session
        .execute(text, &mut |_: QueryResult| {})
        .unwrap_or_else(|error| panic!("{text}: {error}"));
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

fn rows(expected: &[&[&str]]) -> Vec<Vec<String>> {
    expected
        .iter()
        .map(|row| row.iter().map(|value| (*value).to_owned()).collect())
        .collect()
}

fn session_with_units(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = Session::with_dialect(store, Dialect::Oracle);
    run(
        &mut session,
        "CREATE TABLE emp (id NUMBER, name VARCHAR2(40), sal NUMBER)",
    );
    for unit in [PROCEDURE, FUNCTION, SPECIFICATION, BODY, TRIGGER] {
        run(&mut session, unit);
    }
    session
}

#[test]
fn the_unit_source_starts_at_the_unit_keyword() {
    assert_eq!(
        crate::plsql::unit_source("CREATE OR REPLACE EDITIONABLE PACKAGE p AS\nEND p;"),
        "PACKAGE p AS\nEND p;"
    );
    assert_eq!(
        crate::plsql::unit_source("create trigger t before insert on emp begin null; end;"),
        "trigger t before insert on emp begin null; end;"
    );
}

#[test]
fn user_source_lists_each_unit_line_by_line_from_its_keyword() {
    let store = MemoryStore::default();
    let mut session = session_with_units(&store);
    assert_eq!(
        texts(
            &mut session,
            "SELECT name, type, line, text FROM user_source WHERE name = 'RAISE_SALARY' ORDER BY line"
        ),
        rows(&[
            &[
                "RAISE_SALARY",
                "PROCEDURE",
                "1",
                "PROCEDURE raise_salary(p_id IN NUMBER, p_amount IN NUMBER DEFAULT 100) IS\n"
            ],
            &["RAISE_SALARY", "PROCEDURE", "2", "BEGIN\n"],
            &[
                "RAISE_SALARY",
                "PROCEDURE",
                "3",
                "  UPDATE emp SET sal = sal + p_amount WHERE id = p_id;\n"
            ],
            &["RAISE_SALARY", "PROCEDURE", "4", "END raise_salary;"],
        ])
    );
    assert_eq!(
        texts(
            &mut session,
            "SELECT type, COUNT(*) FROM user_source WHERE name = 'EMP_API' GROUP BY type ORDER BY type"
        ),
        rows(&[&["PACKAGE", "5"], &["PACKAGE BODY", "16"]])
    );
    assert_eq!(
        texts(
            &mut session,
            "SELECT text FROM user_source WHERE name = 'EMP_API' AND type = 'PACKAGE BODY' AND line = 1"
        ),
        rows(&[&["PACKAGE BODY emp_api AS\n"]])
    );
    assert_eq!(
        texts(
            &mut session,
            "SELECT type, text FROM user_source WHERE name = 'EMP_CHECK' AND line IN (1, 6) ORDER BY line"
        ),
        rows(&[&["TRIGGER", "TRIGGER emp_check\n"], &["TRIGGER", "  :new.sal := 0;\n"]])
    );
    assert_eq!(
        texts(
            &mut session,
            "SELECT COUNT(*) FROM all_source WHERE name = 'BONUS' AND type = 'FUNCTION'"
        ),
        rows(&[&["4"]])
    );
    assert_eq!(
        texts(&mut session, "SELECT COUNT(*) FROM user_errors"),
        rows(&[&["0"]])
    );
}

#[test]
fn user_objects_lists_procedures_functions_packages_their_bodies_and_triggers() {
    let store = MemoryStore::default();
    let mut session = session_with_units(&store);
    let query = "SELECT object_name, object_type, status FROM user_objects
        WHERE object_type IN ('PROCEDURE', 'FUNCTION', 'PACKAGE', 'PACKAGE BODY', 'TRIGGER')
        ORDER BY object_type, object_name";
    assert_eq!(
        texts(&mut session, query),
        rows(&[
            &["BONUS", "FUNCTION", "VALID"],
            &["EMP_API", "PACKAGE", "VALID"],
            &["EMP_API", "PACKAGE BODY", "VALID"],
            &["RAISE_SALARY", "PROCEDURE", "VALID"],
            &["EMP_CHECK", "TRIGGER", "VALID"],
        ])
    );
    run(&mut session, "DROP PACKAGE BODY emp_api");
    run(&mut session, "DROP PROCEDURE raise_salary");
    assert_eq!(
        texts(&mut session, query),
        rows(&[
            &["BONUS", "FUNCTION", "VALID"],
            &["EMP_API", "PACKAGE", "VALID"],
            &["EMP_CHECK", "TRIGGER", "VALID"],
        ])
    );
    assert_eq!(
        texts(
            &mut session,
            "SELECT COUNT(*) FROM user_source WHERE name IN ('RAISE_SALARY', 'EMP_API') AND type <> 'PACKAGE'"
        ),
        rows(&[&["0"]])
    );
}

#[test]
fn user_procedures_lists_standalone_units_and_public_package_subprograms_with_overloads() {
    let store = MemoryStore::default();
    let mut session = session_with_units(&store);
    assert_eq!(
        texts(
            &mut session,
            "SELECT object_name, procedure_name, subprogram_id, overload, object_type, authid
             FROM user_procedures ORDER BY object_name, subprogram_id"
        ),
        rows(&[
            &["BONUS", "NULL", "1", "NULL", "FUNCTION", "DEFINER"],
            &["EMP_API", "NULL", "0", "NULL", "PACKAGE", "DEFINER"],
            &["EMP_API", "TOTAL", "1", "NULL", "PACKAGE", "DEFINER"],
            &["EMP_API", "HIRE", "2", "1", "PACKAGE", "DEFINER"],
            &["EMP_API", "HIRE", "3", "2", "PACKAGE", "DEFINER"],
            &["EMP_CHECK", "NULL", "1", "NULL", "TRIGGER", "DEFINER"],
            &["RAISE_SALARY", "NULL", "1", "NULL", "PROCEDURE", "DEFINER"],
        ])
    );
}

#[test]
fn user_arguments_lists_parameters_and_function_results_with_resolved_types() {
    let store = MemoryStore::default();
    let mut session = session_with_units(&store);
    assert_eq!(
        texts(
            &mut session,
            "SELECT object_name, package_name, overload, argument_name, position, sequence,
                    data_type, in_out, defaulted
             FROM user_arguments ORDER BY object_name, overload, sequence"
        ),
        rows(&[
            &["BONUS", "NULL", "NULL", "NULL", "0", "1", "NUMBER", "OUT", "N"],
            &["BONUS", "NULL", "NULL", "P_SAL", "1", "2", "NUMBER", "IN", "N"],
            &["HIRE", "EMP_API", "1", "P_NAME", "1", "1", "VARCHAR2", "IN", "N"],
            &["HIRE", "EMP_API", "2", "P_NAME", "1", "1", "VARCHAR2", "IN", "N"],
            &["HIRE", "EMP_API", "2", "P_SAL", "2", "2", "NUMBER", "IN", "N"],
            &["RAISE_SALARY", "NULL", "NULL", "P_ID", "1", "1", "NUMBER", "IN", "N"],
            &["RAISE_SALARY", "NULL", "NULL", "P_AMOUNT", "2", "2", "NUMBER", "IN", "Y"],
            &["TOTAL", "EMP_API", "NULL", "NULL", "0", "1", "NUMBER", "OUT", "N"],
        ])
    );
}

#[test]
fn user_triggers_describes_timing_events_body_and_status() {
    let store = MemoryStore::default();
    let mut session = session_with_units(&store);
    let query = "SELECT trigger_name, trigger_type, triggering_event, base_object_type, table_name,
                referencing_names, status, action_type, before_row, after_statement
         FROM user_triggers";
    assert_eq!(
        texts(&mut session, query),
        rows(&[&[
            "EMP_CHECK",
            "BEFORE EACH ROW",
            "INSERT OR UPDATE",
            "TABLE",
            "EMP",
            "REFERENCING NEW AS NEW OLD AS OLD",
            "ENABLED",
            "PL/SQL",
            "YES",
            "NO",
        ]])
    );
    assert_eq!(
        texts(
            &mut session,
            "SELECT description, trigger_body FROM user_triggers WHERE trigger_name = 'EMP_CHECK'"
        ),
        rows(&[&[
            "emp_check\nBEFORE INSERT OR UPDATE OF sal ON emp\nFOR EACH ROW\n",
            "BEGIN\n  :new.sal := 0;\nEND;",
        ]])
    );
    run(&mut session, "ALTER TRIGGER emp_check DISABLE");
    assert_eq!(
        texts(&mut session, "SELECT status FROM user_triggers"),
        rows(&[&["DISABLED"]])
    );
}
