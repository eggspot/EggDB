use crate::memory_store::MemoryStore;
use crate::{Dialect, QueryResult, Session, SqlError};

fn session(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = Session::with_dialect(store, Dialect::Oracle);
    run(
        &mut session,
        "CREATE TABLE emp (id NUMBER PRIMARY KEY, name VARCHAR2(20), sal NUMBER, dept NUMBER)",
    );
    run(&mut session, "INSERT INTO emp VALUES (1, 'KING', 5000, 10)");
    run(&mut session, "INSERT INTO emp VALUES (2, 'BLAKE', 2850, 30)");
    run(&mut session, "INSERT INTO emp VALUES (3, 'ALLEN', 1600, 30)");
    session.enable_server_output(None);
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

fn printed(session: &mut Session<'_, MemoryStore>, text: &str) -> Vec<String> {
    run(session, text);
    session.take_server_output()
}

fn single(session: &mut Session<'_, MemoryStore>, text: &str) -> String {
    let mut found = None;
    session
        .execute(text, &mut |result: QueryResult| {
            if let Some(row) = result.rows.first() {
                found = row.first().and_then(|value| value.to_text());
            }
        })
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    found.unwrap_or_else(|| "NULL".to_owned())
}

fn lines(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|text| (*text).to_owned()).collect()
}

#[test]
fn a_nested_block_hides_an_outer_variable_that_its_label_still_reaches() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    let output = printed(
        &mut session,
        "<<outer>>
DECLARE
  greeting CONSTANT VARCHAR2(20) := 'Hello';
  x NUMBER := 1;
BEGIN
  DECLARE
    x NUMBER := 2;
  BEGIN
    DBMS_OUTPUT.PUT_LINE(greeting || ': inner ' || x || ', outer ' || outer.x);
  END;
  DBMS_OUTPUT.PUT_LINE('back to ' || x);
END outer;",
    );
    assert_eq!(output, lines(&["Hello: inner 2, outer 1", "back to 1"]));
}

#[test]
fn a_record_type_and_a_rowtype_variable_hold_a_row() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    let output = printed(
        &mut session,
        "DECLARE
  TYPE emp_rec IS RECORD (name VARCHAR2(20), sal NUMBER := 100);
  r emp_rec;
  e emp%ROWTYPE;
BEGIN
  r.name := 'ADAMS';
  SELECT * INTO e FROM emp WHERE id = 1;
  DBMS_OUTPUT.PUT_LINE(r.name || ' ' || r.sal || ' / ' || e.name || ' ' || e.sal);
END;",
    );
    assert_eq!(output, lines(&["ADAMS 100 / KING 5000"]));
}

#[test]
fn in_out_parameters_of_a_local_procedure_swap_two_values() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    let output = printed(
        &mut session,
        "DECLARE
  a NUMBER := 1;
  b NUMBER := 2;
  PROCEDURE swap (x IN OUT NUMBER, y IN OUT NUMBER) IS
    t NUMBER := x;
  BEGIN
    x := y;
    y := t;
  END swap;
BEGIN
  swap(a, b);
  DBMS_OUTPUT.PUT_LINE(a || ' ' || b);
END;",
    );
    assert_eq!(output, lines(&["2 1"]));
}

#[test]
fn an_error_leaving_a_procedure_names_the_line_of_each_unit() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "CREATE PROCEDURE check_sal (p_id NUMBER) IS
  v NUMBER;
BEGIN
  SELECT sal INTO v FROM emp WHERE id = p_id;
  IF v > 3000 THEN
    RAISE_APPLICATION_ERROR(-20002, 'salary of ' || p_id || ' is over 3000');
  END IF;
END check_sal;",
    );
    run(&mut session, "BEGIN\n  check_sal(3);\nEND;");
    let error = failure(&mut session, "BEGIN\n  check_sal(1);\nEND;");
    assert_eq!(error.vendor_code(), Some(20002));
    assert_eq!(error.message(), "salary of 1 is over 3000");
    let context = error.context().unwrap_or_default();
    assert!(context.contains("CHECK_SAL\", line 6"), "{context}");
    assert!(context.contains("ORA-06512: at line 2"), "{context}");
}

#[test]
fn a_function_summing_with_a_cursor_for_loop_is_called_from_sql() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "CREATE FUNCTION dept_total (p_dept NUMBER) RETURN NUMBER IS
  total NUMBER := 0;
BEGIN
  FOR r IN (SELECT sal FROM emp WHERE dept = p_dept) LOOP
    total := total + r.sal;
  END LOOP;
  RETURN total;
END dept_total;",
    );
    assert_eq!(single(&mut session, "SELECT dept_total(30) FROM dual"), "4450");
    assert_eq!(single(&mut session, "SELECT dept_total(99) FROM dual"), "0");
}

#[test]
fn a_package_initializes_once_per_session_and_keeps_its_state() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "CREATE PACKAGE counter AS
  PROCEDURE bump;
  FUNCTION value_of RETURN NUMBER;
END counter;",
    );
    run(
        &mut session,
        "CREATE PACKAGE BODY counter AS
  n NUMBER;
  PROCEDURE bump IS
  BEGIN
    n := n + 1;
  END bump;
  FUNCTION value_of RETURN NUMBER IS
  BEGIN
    RETURN n;
  END value_of;
BEGIN
  n := 10;
  DBMS_OUTPUT.PUT_LINE('counter starts at ' || n);
END counter;",
    );
    let block = "DECLARE\n  v NUMBER;\nBEGIN\n  counter.bump;\n  v := counter.value_of;\n  DBMS_OUTPUT.PUT_LINE(v);\nEND;";
    assert_eq!(printed(&mut session, block), lines(&["counter starts at 10", "11"]));
    assert_eq!(printed(&mut session, block), lines(&["12"]));
}

#[test]
fn an_exception_raised_in_a_handler_goes_to_the_enclosing_block() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    let output = printed(
        &mut session,
        "BEGIN
  BEGIN
    RAISE NO_DATA_FOUND;
  EXCEPTION
    WHEN NO_DATA_FOUND THEN
      DBMS_OUTPUT.PUT_LINE('inner handler');
      RAISE ZERO_DIVIDE;
  END;
EXCEPTION
  WHEN ZERO_DIVIDE THEN
    DBMS_OUTPUT.PUT_LINE('outer handler: ' || SQLERRM);
END;",
    );
    assert_eq!(
        output,
        lines(&["inner handler", "outer handler: ORA-01476: divisor is equal to zero"])
    );
}

#[test]
fn a_simple_case_statement_without_a_match_raises_case_not_found() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    let output = printed(
        &mut session,
        "DECLARE
  grade CHAR(1) := 'B';
BEGIN
  CASE grade
    WHEN 'A' THEN DBMS_OUTPUT.PUT_LINE('Excellent');
    WHEN 'B' THEN DBMS_OUTPUT.PUT_LINE('Very Good');
  END CASE;
  grade := 'X';
  CASE grade
    WHEN 'A' THEN DBMS_OUTPUT.PUT_LINE('Excellent');
  END CASE;
EXCEPTION
  WHEN CASE_NOT_FOUND THEN
    DBMS_OUTPUT.PUT_LINE('no grade ' || grade);
END;",
    );
    assert_eq!(output, lines(&["Very Good", "no grade X"]));
}

#[test]
fn a_bulk_fetch_with_a_limit_reads_a_cursor_in_batches() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    let output = printed(
        &mut session,
        "DECLARE
  CURSOR c IS SELECT name FROM emp ORDER BY id;
  TYPE name_list IS TABLE OF VARCHAR2(20);
  names name_list;
BEGIN
  OPEN c;
  LOOP
    FETCH c BULK COLLECT INTO names LIMIT 2;
    EXIT WHEN names.COUNT = 0;
    DBMS_OUTPUT.PUT_LINE(names.COUNT || ' fetched, ' || c%ROWCOUNT || ' so far');
  END LOOP;
  CLOSE c;
END;",
    );
    assert_eq!(output, lines(&["2 fetched, 2 so far", "1 fetched, 3 so far"]));
}
