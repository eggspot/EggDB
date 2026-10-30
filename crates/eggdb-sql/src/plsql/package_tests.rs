use crate::memory_store::MemoryStore;
use crate::{Dialect, QueryResult, Session, SqlError};

const STATS_SPECIFICATION: &str = "CREATE OR REPLACE PACKAGE stats AS
  total NUMBER := 0;
  limit_value CONSTANT NUMBER := 100;
  too_big EXCEPTION;
  PRAGMA EXCEPTION_INIT(too_big, -20001);
  TYPE num_list IS TABLE OF NUMBER;
  PROCEDURE add_amount(n NUMBER);
  PROCEDURE add_amount(s VARCHAR2);
  FUNCTION current_total RETURN NUMBER;
END stats;";

const STATS_BODY: &str = "CREATE OR REPLACE PACKAGE BODY stats AS
  calls NUMBER := 0;
  PROCEDURE note(msg VARCHAR2) IS
  BEGIN
    calls := calls + 1;
    INSERT INTO log VALUES (calls, msg);
  END note;
  PROCEDURE add_amount(n NUMBER) IS
  BEGIN
    IF total + n > limit_value THEN
      RAISE too_big;
    END IF;
    total := total + n;
    note('number ' || n);
  END add_amount;
  PROCEDURE add_amount(s VARCHAR2) IS
  BEGIN
    add_amount(TO_NUMBER(s));
  END add_amount;
  FUNCTION current_total RETURN NUMBER IS
  BEGIN
    RETURN total;
  END current_total;
BEGIN
  note('initialized');
END stats;";

fn session(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = Session::with_dialect(store, Dialect::Oracle);
    run(&mut session, "CREATE TABLE log (n NUMBER, msg VARCHAR2(200))");
    session
}

fn stats_session(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = session(store);
    run(&mut session, STATS_SPECIFICATION);
    run(&mut session, STATS_BODY);
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
        .unwrap_or_else(|error| panic!("log: {error}"));
    rows
}

fn entries(rows: &[(&str, &str)]) -> Vec<(String, String)> {
    rows.iter()
        .map(|(n, msg)| ((*n).to_owned(), (*msg).to_owned()))
        .collect()
}

#[test]
fn package_variables_keep_their_values_between_calls() {
    let store = MemoryStore::new();
    let mut session = stats_session(&store);
    run(&mut session, "BEGIN stats.add_amount(5); stats.add_amount('7'); END;");
    run(&mut session, "CALL stats.add_amount(3)");
    assert_eq!(single(&mut session, "SELECT stats.current_total() FROM dual"), "15");
    assert_eq!(
        logged(&mut session),
        entries(&[
            ("1", "initialized"),
            ("2", "number 5"),
            ("3", "number 7"),
            ("4", "number 3"),
        ])
    );
}

#[test]
fn the_initialization_part_runs_once_per_session() {
    let store = MemoryStore::new();
    let mut first = stats_session(&store);
    run(&mut first, "BEGIN stats.add_amount(1); END;");
    run(&mut first, "BEGIN stats.add_amount(2); END;");
    let mut second = Session::with_dialect(&store, Dialect::Oracle);
    run(&mut second, "CALL stats.add_amount(0)");
    assert_eq!(single(&mut second, "SELECT stats.current_total() FROM dual"), "0");
    assert_eq!(single(&mut first, "SELECT stats.current_total() FROM dual"), "3");
    let initialized = logged(&mut first)
        .into_iter()
        .filter(|(_, msg)| msg == "initialized")
        .count();
    assert_eq!(initialized, 2);
}

#[test]
fn blocks_read_and_assign_public_package_variables() {
    let store = MemoryStore::new();
    let mut session = stats_session(&store);
    run(
        &mut session,
        "BEGIN
  stats.total := stats.total + 30;
  INSERT INTO log VALUES (stats.total, 'outside');
  INSERT INTO log VALUES (stats.limit_value, 'limit');
END;",
    );
    assert_eq!(single(&mut session, "SELECT stats.current_total() FROM dual"), "30");
    assert_eq!(
        logged(&mut session),
        entries(&[("1", "initialized"), ("30", "outside"), ("100", "limit")])
    );
}

#[test]
fn package_constants_and_private_items_are_protected() {
    let store = MemoryStore::new();
    let mut session = stats_session(&store);
    failure(&mut session, "BEGIN stats.limit_value := 1; END;");
    failure(
        &mut session,
        "BEGIN INSERT INTO log VALUES (stats.calls, 'private'); END;",
    );
    failure(&mut session, "BEGIN stats.note('private'); END;");
    assert_eq!(single(&mut session, "SELECT stats.current_total() FROM dual"), "0");
}

#[test]
fn package_types_declare_variables_outside_the_package() {
    let store = MemoryStore::new();
    let mut session = stats_session(&store);
    run(
        &mut session,
        "DECLARE
  l stats.num_list := stats.num_list(4, 5, 6);
BEGIN
  INSERT INTO log VALUES (l.COUNT, 'elements');
  INSERT INTO log VALUES (l(2), 'second');
END;",
    );
    assert_eq!(
        logged(&mut session),
        entries(&[("1", "initialized"), ("3", "elements"), ("5", "second")])
    );
}

#[test]
fn package_exceptions_are_caught_by_their_qualified_name() {
    let store = MemoryStore::new();
    let mut session = stats_session(&store);
    run(
        &mut session,
        "BEGIN
  stats.add_amount(500);
EXCEPTION
  WHEN stats.too_big THEN
    INSERT INTO log VALUES (SQLCODE, 'too big');
END;",
    );
    assert_eq!(
        logged(&mut session),
        entries(&[("-20001", "too big"), ("1", "initialized")])
    );
    let error = failure(&mut session, "BEGIN stats.add_amount(500); END;");
    assert_eq!(error.vendor_code(), Some(20001));
}

#[test]
fn a_package_cursor_stays_open_between_calls() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(&mut session, "INSERT INTO log VALUES (10, 'a')");
    run(&mut session, "INSERT INTO log VALUES (20, 'b')");
    run(
        &mut session,
        "CREATE OR REPLACE PACKAGE feed AS
  CURSOR entries IS SELECT n FROM log ORDER BY n;
  FUNCTION next_entry RETURN NUMBER;
END feed;",
    );
    run(
        &mut session,
        "CREATE OR REPLACE PACKAGE BODY feed AS
  FUNCTION next_entry RETURN NUMBER IS
    found_n NUMBER;
  BEGIN
    IF NOT entries%ISOPEN THEN
      OPEN entries;
    END IF;
    FETCH entries INTO found_n;
    IF entries%NOTFOUND THEN
      CLOSE entries;
      RETURN NULL;
    END IF;
    RETURN found_n;
  END next_entry;
END feed;",
    );
    assert_eq!(single(&mut session, "SELECT feed.next_entry() FROM dual"), "10");
    assert_eq!(single(&mut session, "SELECT feed.next_entry() FROM dual"), "20");
    assert_eq!(single(&mut session, "SELECT feed.next_entry() FROM dual"), "NULL");
    assert_eq!(single(&mut session, "SELECT feed.next_entry() FROM dual"), "10");
}

#[test]
fn parameterless_package_functions_are_called_without_parentheses() {
    let store = MemoryStore::new();
    let mut session = stats_session(&store);
    run(
        &mut session,
        "BEGIN
  stats.add_amount(4);
  INSERT INTO log VALUES (stats.current_total, 'total');
END;",
    );
    assert_eq!(
        logged(&mut session),
        entries(&[
            ("1", "initialized"),
            ("2", "number 4"),
            ("4", "total"),
        ])
    );
}

#[test]
fn replacing_a_package_discards_its_session_state() {
    let store = MemoryStore::new();
    let mut session = stats_session(&store);
    run(&mut session, "BEGIN stats.add_amount(9); END;");
    assert_eq!(single(&mut session, "SELECT stats.current_total() FROM dual"), "9");
    run(&mut session, STATS_BODY);
    run(&mut session, "CALL stats.add_amount(1)");
    assert_eq!(single(&mut session, "SELECT stats.current_total() FROM dual"), "1");
}

#[test]
fn calling_a_member_of_a_package_without_a_body_fails() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "CREATE PACKAGE lonely AS
  PROCEDURE p;
END lonely;",
    );
    let error = failure(&mut session, "BEGIN lonely.p; END;");
    assert_eq!(error.vendor_code(), Some(4067));
}

#[test]
fn an_initialization_error_leaves_the_package_uninstantiated() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "CREATE PACKAGE fragile AS
  ready NUMBER;
END fragile;",
    );
    run(
        &mut session,
        "CREATE PACKAGE BODY fragile AS
BEGIN
  SELECT n INTO ready FROM log WHERE msg = 'seed';
END fragile;",
    );
    let error = failure(
        &mut session,
        "BEGIN INSERT INTO log VALUES (fragile.ready, 'read'); END;",
    );
    assert_eq!(error.vendor_code(), Some(1403));
    run(&mut session, "INSERT INTO log VALUES (42, 'seed')");
    run(
        &mut session,
        "BEGIN INSERT INTO log VALUES (fragile.ready + 1, 'read'); END;",
    );
    assert_eq!(
        logged(&mut session),
        entries(&[("42", "seed"), ("43", "read")])
    );
}
