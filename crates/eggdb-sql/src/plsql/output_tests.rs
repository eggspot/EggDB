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

fn lines(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|text| (*text).to_owned()).collect()
}

#[test]
fn put_line_put_and_new_line_build_the_server_output() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE\n  n NUMBER := 7;\nBEGIN\n  DBMS_OUTPUT.ENABLE;\n  DBMS_OUTPUT.PUT_LINE('n is ' || n);\n  dbms_output.put('a');\n  dbms_output.put('b');\n  dbms_output.new_line;\n  DBMS_OUTPUT.PUT('unfinished');\nEND;",
    );
    assert_eq!(session.take_server_output(), lines(&["n is 7", "ab"]));
    assert_eq!(session.take_server_output(), Vec::<String>::new());
    run(&mut session, "BEGIN DBMS_OUTPUT.NEW_LINE; END;");
    assert_eq!(session.take_server_output(), lines(&["unfinished"]));
}

#[test]
fn output_is_ignored_until_it_is_enabled() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(&mut session, "BEGIN DBMS_OUTPUT.PUT_LINE('lost'); END;");
    assert_eq!(session.take_server_output(), Vec::<String>::new());
    session.enable_server_output(None);
    run(&mut session, "BEGIN DBMS_OUTPUT.PUT_LINE('kept'); END;");
    assert_eq!(session.take_server_output(), lines(&["kept"]));
    session.disable_server_output();
    run(&mut session, "BEGIN DBMS_OUTPUT.PUT_LINE('lost again'); END;");
    assert_eq!(session.take_server_output(), Vec::<String>::new());
}

#[test]
fn a_stored_procedure_writes_server_output() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    session.enable_server_output(None);
    run(
        &mut session,
        "CREATE PROCEDURE greet (who VARCHAR2) AS\nBEGIN\n  DBMS_OUTPUT.PUT_LINE('Hello, ' || who);\nEND;",
    );
    run(&mut session, "BEGIN greet('world'); END;");
    run(&mut session, "CALL greet('again')");
    assert_eq!(
        session.take_server_output(),
        lines(&["Hello, world", "Hello, again"])
    );
}

#[test]
fn get_line_reads_the_buffer_back_with_its_status() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE\n  l VARCHAR2(100);\n  s INTEGER;\nBEGIN\n  DBMS_OUTPUT.ENABLE(NULL);\n  DBMS_OUTPUT.PUT_LINE('hello');\n  DBMS_OUTPUT.GET_LINE(l, s);\n  INSERT INTO log VALUES (s, l);\n  DBMS_OUTPUT.GET_LINE(l, s);\n  INSERT INTO log VALUES (s, NVL(l, 'none'));\nEND;",
    );
    let mut rows = Vec::new();
    session
        .execute("SELECT n, msg FROM log ORDER BY n", &mut |result: QueryResult| {
            for row in result.rows {
                let text = |at: usize| row.get(at).and_then(|value| value.to_text());
                rows.push((text(0), text(1)));
            }
        })
        .unwrap();
    assert_eq!(
        rows,
        [
            (Some("0".to_owned()), Some("hello".to_owned())),
            (Some("1".to_owned()), Some("none".to_owned())),
        ]
    );
    assert_eq!(session.take_server_output(), Vec::<String>::new());
}

#[test]
fn a_full_buffer_raises_oru_10027() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    let error = failure(
        &mut session,
        "BEGIN\n  DBMS_OUTPUT.ENABLE(2000);\n  FOR i IN 1..100 LOOP\n    DBMS_OUTPUT.PUT_LINE(RPAD('x', 100, 'x'));\n  END LOOP;\nEND;",
    );
    assert_eq!(error.vendor_code(), Some(20000), "{error}");
    assert!(error.message().contains("ORU-10027"), "{error}");
}

#[test]
fn hostile_output_calls_fail_without_panicking() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    for text in [
        "BEGIN DBMS_OUTPUT.PUT_LINE(); END;",
        "BEGIN DBMS_OUTPUT.NO_SUCH_MEMBER(1); END;",
        "BEGIN DBMS_OUTPUT.ENABLE('lots'); END;",
        "DECLARE n NUMBER; BEGIN DBMS_OUTPUT.GET_LINE(n); END;",
        "BEGIN DBMS_OUTPUT.GET_LINE(1, 2); END;",
    ] {
        assert!(session.execute(text, &mut |_| {}).is_err(), "{text}");
    }
}
