use crate::memory_store::MemoryStore;
use crate::{Dialect, QueryResult, Session, SqlError};

fn session(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = Session::with_dialect(store, Dialect::Oracle);
    run(&mut session, "CREATE TABLE log (n NUMBER, msg VARCHAR2(200))");
    run(
        &mut session,
        "CREATE TABLE emp (id NUMBER, name VARCHAR2(20), dept NUMBER)",
    );
    for row in ["1, 'ann', 10", "2, 'bob', 20", "3, 'cy', 10"] {
        run(&mut session, &format!("INSERT INTO emp VALUES ({row})"));
    }
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
fn an_integer_indexed_table_is_walked_with_first_and_next_in_key_order() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE\n  TYPE t_map IS TABLE OF VARCHAR2(20) INDEX BY PLS_INTEGER;\n  m t_map;\n  i PLS_INTEGER;\nBEGIN\n  m(3) := 'c';\n  m(1) := 'a';\n  m(2) := 'b';\n  i := m.FIRST;\n  WHILE i IS NOT NULL LOOP\n    INSERT INTO log VALUES (i, m(i));\n    i := m.NEXT(i);\n  END LOOP;\n  INSERT INTO log VALUES (m.COUNT * 10, 'count');\nEND;",
    );
    assert_eq!(
        logged(&mut session),
        [pair("1", "a"), pair("2", "b"), pair("3", "c"), pair("30", "count")]
    );
}

#[test]
fn a_string_indexed_table_keeps_its_keys_sorted() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE\n  TYPE t_capitals IS TABLE OF VARCHAR2(30) INDEX BY VARCHAR2(30);\n  c t_capitals;\n  k VARCHAR2(30);\n  n NUMBER := 0;\nBEGIN\n  c('France') := 'Paris';\n  c('Austria') := 'Vienna';\n  c('Japan') := 'Tokyo';\n  k := c.FIRST;\n  WHILE k IS NOT NULL LOOP\n    n := n + 1;\n    INSERT INTO log VALUES (n, k || '=' || c(k));\n    k := c.NEXT(k);\n  END LOOP;\nEND;",
    );
    assert_eq!(
        logged(&mut session),
        [
            pair("1", "Austria=Vienna"),
            pair("2", "France=Paris"),
            pair("3", "Japan=Tokyo")
        ]
    );
}

#[test]
fn a_nested_table_is_built_extended_trimmed_and_deleted() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE\n  TYPE t_list IS TABLE OF NUMBER;\n  v t_list := t_list(10, 20, 30);\nBEGIN\n  v.EXTEND;\n  v(4) := 40;\n  v.EXTEND(2, 1);\n  INSERT INTO log VALUES (v.COUNT, 'count after extend');\n  INSERT INTO log VALUES (v(6), 'copy of first');\n  v.TRIM(2);\n  v.DELETE(2);\n  INSERT INTO log VALUES (v.COUNT, 'count after trim and delete');\n  IF NOT v.EXISTS(2) THEN\n    INSERT INTO log VALUES (v.LAST, 'last');\n  END IF;\nEND;",
    );
    assert_eq!(
        logged(&mut session),
        [
            pair("3", "count after trim and delete"),
            pair("4", "last"),
            pair("6", "count after extend"),
            pair("10", "copy of first")
        ]
    );
}

#[test]
fn collection_misuse_raises_the_predefined_exceptions() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE\n  TYPE t_list IS TABLE OF NUMBER;\n  v t_list;\nBEGIN\n  v.EXTEND;\nEXCEPTION\n  WHEN COLLECTION_IS_NULL THEN\n    INSERT INTO log VALUES (SQLCODE, SQLERRM);\nEND;",
    );
    run(
        &mut session,
        "DECLARE\n  TYPE t_short IS VARRAY(3) OF NUMBER;\n  v t_short := t_short(1, 2);\nBEGIN\n  v(3) := 3;\nEXCEPTION\n  WHEN SUBSCRIPT_BEYOND_COUNT THEN\n    INSERT INTO log VALUES (1, 'beyond count');\nEND;",
    );
    run(
        &mut session,
        "DECLARE\n  TYPE t_short IS VARRAY(3) OF NUMBER;\n  v t_short := t_short(1, 2);\nBEGIN\n  v.EXTEND(2);\nEXCEPTION\n  WHEN SUBSCRIPT_OUTSIDE_LIMIT THEN\n    INSERT INTO log VALUES (2, 'outside limit ' || v.LIMIT);\nEND;",
    );
    run(
        &mut session,
        "DECLARE\n  TYPE t_map IS TABLE OF NUMBER INDEX BY PLS_INTEGER;\n  m t_map;\n  n NUMBER;\nBEGIN\n  n := m(7);\nEXCEPTION\n  WHEN NO_DATA_FOUND THEN\n    INSERT INTO log VALUES (3, 'missing key');\nEND;",
    );
    assert_eq!(
        logged(&mut session),
        [
            pair("-6531", "ORA-06531: Reference to uninitialized collection"),
            pair("1", "beyond count"),
            pair("2", "outside limit 3"),
            pair("3", "missing key")
        ]
    );
}

#[test]
fn bulk_collect_fills_a_table_of_rows_and_forall_counts_rows_per_index() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE\n  TYPE t_emps IS TABLE OF emp%ROWTYPE;\n  emps t_emps;\n  TYPE t_ids IS TABLE OF NUMBER INDEX BY PLS_INTEGER;\n  depts t_ids;\n  total NUMBER;\nBEGIN\n  SELECT * BULK COLLECT INTO emps FROM emp ORDER BY id;\n  INSERT INTO log VALUES (emps.COUNT, emps(2).name);\n  depts(1) := 10;\n  depts(2) := 30;\n  FORALL i IN 1..depts.COUNT\n    UPDATE emp SET name = UPPER(name) WHERE dept = depts(i);\n  total := SQL%ROWCOUNT;\n  INSERT INTO log VALUES (SQL%BULK_ROWCOUNT(1), 'dept 10');\n  INSERT INTO log VALUES (SQL%BULK_ROWCOUNT(2) + 100, 'dept 30');\n  INSERT INTO log VALUES (total + 1000, 'total');\nEND;",
    );
    assert_eq!(
        logged(&mut session),
        [
            pair("2", "dept 10"),
            pair("3", "bob"),
            pair("100", "dept 30"),
            pair("1002", "total")
        ]
    );
}

#[test]
fn a_cursor_is_fetched_in_batches_with_its_attributes() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE\n  CURSOR c IS SELECT id FROM emp ORDER BY id;\n  TYPE t_ids IS TABLE OF NUMBER;\n  ids t_ids;\n  batches NUMBER := 0;\nBEGIN\n  IF NOT c%ISOPEN THEN\n    OPEN c;\n  END IF;\n  LOOP\n    FETCH c BULK COLLECT INTO ids LIMIT 2;\n    batches := batches + 1;\n    INSERT INTO log VALUES (batches, ids.COUNT || ' rows, ' || c%ROWCOUNT || ' so far');\n    EXIT WHEN c%NOTFOUND;\n  END LOOP;\n  CLOSE c;\n  IF NOT c%ISOPEN THEN\n    INSERT INTO log VALUES (10, 'closed');\n  END IF;\nEND;",
    );
    assert_eq!(
        logged(&mut session),
        [
            pair("1", "2 rows, 2 so far"),
            pair("2", "1 rows, 3 so far"),
            pair("10", "closed")
        ]
    );
}

#[test]
fn a_single_row_fetch_sets_found_and_rowcount() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE\n  CURSOR c IS SELECT name FROM emp ORDER BY id;\n  v VARCHAR2(20);\nBEGIN\n  OPEN c;\n  LOOP\n    FETCH c INTO v;\n    EXIT WHEN c%NOTFOUND;\n    INSERT INTO log VALUES (c%ROWCOUNT, v);\n  END LOOP;\n  CLOSE c;\nEND;",
    );
    assert_eq!(
        logged(&mut session),
        [pair("1", "ann"), pair("2", "bob"), pair("3", "cy")]
    );
}

#[test]
fn save_exceptions_reports_every_failed_index() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(&mut session, "CREATE TABLE k (id NUMBER PRIMARY KEY)");
    run(
        &mut session,
        "DECLARE\n  TYPE t_ids IS TABLE OF NUMBER;\n  ids t_ids := t_ids(1, 2, 1, 3, 2);\n  dml_errors EXCEPTION;\n  PRAGMA EXCEPTION_INIT(dml_errors, -24381);\nBEGIN\n  FORALL i IN 1..ids.COUNT SAVE EXCEPTIONS\n    INSERT INTO k VALUES (ids(i));\nEXCEPTION\n  WHEN dml_errors THEN\n    FOR j IN 1..SQL%BULK_EXCEPTIONS.COUNT LOOP\n      INSERT INTO log VALUES (SQL%BULK_EXCEPTIONS(j).ERROR_INDEX, 'ORA-' || SQL%BULK_EXCEPTIONS(j).ERROR_CODE);\n    END LOOP;\nEND;",
    );
    assert_eq!(logged(&mut session), [pair("3", "ORA-1"), pair("5", "ORA-1")]);
    let mut kept = 0;
    session
        .execute("SELECT COUNT(*) FROM k", &mut |result: QueryResult| {
            kept = result.rows[0][0].to_text().unwrap_or_default().parse().unwrap_or(0);
        })
        .unwrap();
    assert_eq!(kept, 3);
}

#[test]
fn implicit_cursor_attributes_and_sqlerrm_of_a_code() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "BEGIN\n  UPDATE emp SET dept = dept WHERE dept = 10;\n  IF SQL%FOUND AND NOT SQL%ISOPEN THEN\n    INSERT INTO log VALUES (SQL%ROWCOUNT, SQLERRM(-1403));\n  END IF;\nEND;",
    );
    run(
        &mut session,
        "DECLARE\n  CURSOR c IS SELECT id FROM emp;\n  n NUMBER;\nBEGIN\n  n := c%ROWCOUNT;\nEXCEPTION\n  WHEN INVALID_CURSOR THEN\n    INSERT INTO log VALUES (SQLCODE, SQLERRM);\nEND;",
    );
    assert_eq!(
        logged(&mut session),
        [
            pair("-1001", "ORA-01001: invalid cursor"),
            pair("2", "ORA-01403: no data found")
        ]
    );
}

#[test]
fn a_table_of_records_is_passed_to_a_local_procedure_and_filled_field_by_field() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE\n  TYPE t_point IS RECORD (x NUMBER, y NUMBER);\n  TYPE t_points IS TABLE OF t_point INDEX BY PLS_INTEGER;\n  pts t_points;\n  PROCEDURE add_point(points IN OUT t_points, x NUMBER, y NUMBER) IS\n  BEGIN\n    points(points.COUNT + 1).x := x;\n    points(points.COUNT).y := y;\n  END;\nBEGIN\n  add_point(pts, 1, 2);\n  add_point(pts, 3, 4);\n  INSERT INTO log VALUES (pts.COUNT, pts(2).x || ',' || pts(2).y);\nEND;",
    );
    assert_eq!(logged(&mut session), [pair("2", "3,4")]);
}

#[test]
fn hostile_collection_use_fails_with_errors_not_panics() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    for block in [
        "DECLARE TYPE t IS TABLE OF NUMBER; v t := t(); BEGIN v.EXTEND(100000000); END;",
        "DECLARE TYPE t IS TABLE OF NUMBER INDEX BY PLS_INTEGER; v t; BEGIN v(NULL) := 1; END;",
        "DECLARE TYPE t IS TABLE OF NUMBER INDEX BY PLS_INTEGER; v t; BEGIN v(99999999999) := 1; END;",
        "DECLARE TYPE t IS TABLE OF NUMBER; v t := t(1); BEGIN v.TRIM(5); END;",
        "DECLARE TYPE t IS VARRAY(2) OF NUMBER; v t := t(1, 2, 3); BEGIN NULL; END;",
        "DECLARE TYPE t IS VARRAY(2) OF NUMBER; v t := t(1, 2); BEGIN v.DELETE(1); END;",
        "DECLARE TYPE t IS TABLE OF NUMBER INDEX BY PLS_INTEGER; v t; BEGIN v.EXTEND; END;",
        "DECLARE TYPE t IS TABLE OF NUMBER; v t := t(1); n NUMBER; BEGIN n := v.NOPE; END;",
        "DECLARE TYPE t IS TABLE OF NUMBER; v t := t(1); n NUMBER; BEGIN n := v(0); END;",
        "DECLARE CURSOR c IS SELECT id FROM emp; TYPE t IS TABLE OF NUMBER; v t; BEGIN OPEN c; FETCH c BULK COLLECT INTO v LIMIT 0; END;",
        "BEGIN FORALL i IN 1..3 INSERT INTO missing_table VALUES (i); END;",
    ] {
        let error = failure(&mut session, block);
        assert!(!error.message().is_empty(), "{block}");
    }
}
