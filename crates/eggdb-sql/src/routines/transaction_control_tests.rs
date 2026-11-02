use crate::error::SqlState;
use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Session, SqlError};

fn run(session: &mut Session<'_>, sql: &str) -> Result<String, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs.pop().unwrap_or_default())
}

fn answer(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(output) => output,
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn refusal(session: &mut Session<'_>, sql: &str) -> SqlError {
    match run(session, sql) {
        Ok(output) => panic!("{sql} succeeded: {output}"),
        Err(error) => error,
    }
}

fn session_with<'s>(store: &'s MemoryStore, statements: &[&str]) -> Session<'s> {
    let mut session = Session::new(store);
    for statement in statements {
        answer(&mut session, statement);
    }
    session
}

#[test]
fn procedures_commit_and_roll_back_and_start_a_new_transaction_after_each() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE TABLE t (n integer)",
            "CREATE PROCEDURE steps() LANGUAGE plpgsql AS $$ BEGIN \
             INSERT INTO t VALUES (1); COMMIT; \
             INSERT INTO t VALUES (2); ROLLBACK; \
             INSERT INTO t VALUES (3); COMMIT AND CHAIN; \
             INSERT INTO t VALUES (4); ROLLBACK AND CHAIN; \
             INSERT INTO t VALUES (5); END $$",
        ],
    );
    assert_eq!(answer(&mut session, "CALL steps()"), "CALL");
    assert_eq!(
        answer(&mut session, "SELECT n FROM t ORDER BY n"),
        "n\n1\n3\n5\nSELECT 3"
    );
}

#[test]
fn work_committed_before_an_error_stays_committed() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE TABLE t (n integer)",
            "CREATE PROCEDURE half_done() LANGUAGE plpgsql AS $$ BEGIN \
             INSERT INTO t VALUES (10); COMMIT; \
             INSERT INTO t VALUES (11); PERFORM 1 / 0; END $$",
        ],
    );
    let error = refusal(&mut session, "CALL half_done()");
    assert_eq!(error.state(), SqlState::DIVISION_BY_ZERO);
    assert_eq!(
        answer(&mut session, "SELECT n FROM t ORDER BY n"),
        "n\n10\nSELECT 1"
    );
}

#[test]
fn the_postgresql_manual_loops_commit_every_other_row_and_keep_their_query_open() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE TABLE test1 (a integer)",
            "CREATE PROCEDURE transaction_test1() LANGUAGE plpgsql AS $$ BEGIN \
             FOR i IN 0..9 LOOP INSERT INTO test1 (a) VALUES (i); \
             IF i % 2 = 0 THEN COMMIT; ELSE ROLLBACK; END IF; END LOOP; END $$",
            "CREATE TABLE test2 (x integer)",
            "INSERT INTO test2 VALUES (1), (2), (3)",
            "CREATE TABLE test3 (y integer)",
            "CREATE PROCEDURE transaction_test2() LANGUAGE plpgsql AS $$ DECLARE r record; BEGIN \
             FOR r IN SELECT * FROM test2 ORDER BY x LOOP \
             INSERT INTO test3 (y) VALUES (r.x * 10); COMMIT; END LOOP; END $$",
        ],
    );
    answer(&mut session, "CALL transaction_test1()");
    assert_eq!(
        answer(&mut session, "SELECT a FROM test1 ORDER BY a"),
        "a\n0\n2\n4\n6\n8\nSELECT 5"
    );
    answer(&mut session, "CALL transaction_test2()");
    assert_eq!(
        answer(&mut session, "SELECT y FROM test3 ORDER BY y"),
        "y\n10\n20\n30\nSELECT 3"
    );
}

#[test]
fn do_blocks_and_nested_calls_may_end_transactions() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE TABLE t (n integer)",
            "CREATE PROCEDURE inner_commit(v integer) LANGUAGE plpgsql AS $$ BEGIN \
             INSERT INTO t VALUES (v); COMMIT; END $$",
            "CREATE PROCEDURE outer_call() LANGUAGE plpgsql AS $$ BEGIN \
             CALL inner_commit(30); INSERT INTO t VALUES (31); ROLLBACK; END $$",
        ],
    );
    answer(
        &mut session,
        "DO $$ BEGIN INSERT INTO t VALUES (20); COMMIT; INSERT INTO t VALUES (21); ROLLBACK; END $$",
    );
    answer(&mut session, "CALL outer_call()");
    assert_eq!(
        answer(&mut session, "SELECT n FROM t ORDER BY n"),
        "n\n20\n30\nSELECT 2"
    );
}

#[test]
fn transaction_control_is_refused_where_postgresql_refuses_it() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE TABLE t (n integer)",
            "CREATE PROCEDURE commits() LANGUAGE plpgsql AS $$ BEGIN \
             INSERT INTO t VALUES (1); COMMIT; END $$",
            "CREATE FUNCTION commits_in_function() RETURNS void LANGUAGE plpgsql AS $$ BEGIN \
             COMMIT; END $$",
            "CREATE FUNCTION calls_procedure() RETURNS void LANGUAGE plpgsql AS $$ BEGIN \
             CALL commits(); END $$",
            "CREATE PROCEDURE commits_in_handler() LANGUAGE plpgsql AS $$ BEGIN \
             BEGIN INSERT INTO t VALUES (2); ROLLBACK; \
             EXCEPTION WHEN division_by_zero THEN NULL; END; END $$",
            "CREATE PROCEDURE calls_in_handler() LANGUAGE plpgsql AS $$ BEGIN \
             BEGIN CALL commits(); EXCEPTION WHEN division_by_zero THEN NULL; END; END $$",
        ],
    );
    let error = refusal(&mut session, "SELECT commits_in_function()");
    assert_eq!(error.state(), SqlState::INVALID_TRANSACTION_TERMINATION);
    assert!(
        error
            .to_string()
            .contains("invalid transaction termination")
    );
    let error = refusal(&mut session, "SELECT calls_procedure()");
    assert_eq!(error.state(), SqlState::INVALID_TRANSACTION_TERMINATION);
    let error = refusal(&mut session, "CALL commits_in_handler()");
    assert_eq!(error.state(), SqlState::INVALID_TRANSACTION_TERMINATION);
    assert!(
        error
            .to_string()
            .contains("cannot roll back while a subtransaction is active")
    );
    let error = refusal(&mut session, "CALL calls_in_handler()");
    assert_eq!(error.state(), SqlState::INVALID_TRANSACTION_TERMINATION);
    answer(&mut session, "BEGIN");
    let error = refusal(&mut session, "CALL commits()");
    assert_eq!(error.state(), SqlState::INVALID_TRANSACTION_TERMINATION);
    answer(&mut session, "ROLLBACK");
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM t"),
        "count\n0\nSELECT 1"
    );
}

#[test]
fn a_returned_refcursor_stays_open_for_the_caller_until_the_transaction_ends() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE TABLE items (id integer, name text)",
            "INSERT INTO items VALUES (1, 'a'), (2, 'b'), (3, 'c')",
            "CREATE FUNCTION open_items(c refcursor) RETURNS refcursor LANGUAGE plpgsql AS $$ \
             BEGIN OPEN c FOR SELECT id, name FROM items ORDER BY id; RETURN c; END $$",
            "CREATE FUNCTION open_unnamed() RETURNS refcursor LANGUAGE plpgsql AS $$ \
             DECLARE c refcursor; BEGIN OPEN c FOR SELECT name FROM items ORDER BY id DESC; \
             RETURN c; END $$",
        ],
    );
    answer(&mut session, "BEGIN");
    assert_eq!(
        answer(&mut session, "SELECT open_items('items_cursor')"),
        "open_items\nitems_cursor\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "FETCH 2 FROM items_cursor"),
        "id|name\n1|a\n2|b\nFETCH 2"
    );
    assert_eq!(
        answer(&mut session, "FETCH ALL IN items_cursor"),
        "id|name\n3|c\nFETCH 1"
    );
    assert_eq!(answer(&mut session, "CLOSE items_cursor"), "CLOSE CURSOR");
    let unnamed = answer(&mut session, "SELECT open_unnamed()");
    assert!(
        unnamed.starts_with("open_unnamed\n<unnamed portal "),
        "{unnamed}"
    );
    let name = unnamed.lines().nth(1).unwrap_or_default().to_owned();
    assert_eq!(
        answer(&mut session, &format!("FETCH ALL FROM \"{name}\"")),
        "name\nc\nb\na\nFETCH 3"
    );
    let error = refusal(&mut session, "FETCH ALL FROM items_cursor");
    assert_eq!(error.state(), SqlState::INVALID_CURSOR_NAME);
    answer(&mut session, "ROLLBACK");
    let error = refusal(&mut session, &format!("FETCH ALL FROM \"{name}\""));
    assert_eq!(error.state(), SqlState::INVALID_CURSOR_NAME);
    answer(&mut session, "SELECT open_items('autocommitted')");
    let error = refusal(&mut session, "FETCH ALL FROM autocommitted");
    assert_eq!(error.state(), SqlState::INVALID_CURSOR_NAME);
}

#[test]
fn procedures_hand_back_refcursors_through_inout_parameters_and_other_routines_read_them() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE TABLE items (id integer)",
            "INSERT INTO items VALUES (1), (2), (3)",
            "CREATE PROCEDURE get_items(INOUT c refcursor) LANGUAGE plpgsql AS $$ \
             BEGIN OPEN c FOR SELECT id FROM items ORDER BY id; END $$",
            "CREATE FUNCTION next_id(c refcursor) RETURNS integer LANGUAGE plpgsql AS $$ \
             DECLARE v integer; BEGIN FETCH c INTO v; RETURN v; END $$",
        ],
    );
    answer(&mut session, "BEGIN");
    assert_eq!(answer(&mut session, "CALL get_items('pc')"), "c\npc\nCALL");
    assert_eq!(
        answer(&mut session, "SELECT next_id('pc')"),
        "next_id\n1\nSELECT 1"
    );
    assert_eq!(answer(&mut session, "FETCH NEXT FROM pc"), "id\n2\nFETCH 1");
    assert_eq!(
        answer(&mut session, "SELECT next_id('pc')"),
        "next_id\n3\nSELECT 1"
    );
    assert_eq!(answer(&mut session, "FETCH pc"), "id\nFETCH 0");
    answer(&mut session, "COMMIT");
}

#[test]
fn declared_cursors_fetch_move_and_close_as_postgresql_does() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE TABLE n (v integer)",
            "INSERT INTO n VALUES (1), (2), (3), (4), (5)",
        ],
    );
    let error = refusal(&mut session, "DECLARE c CURSOR FOR SELECT v FROM n");
    assert_eq!(error.state(), SqlState::NO_ACTIVE_SQL_TRANSACTION);
    answer(&mut session, "BEGIN");
    assert_eq!(
        answer(
            &mut session,
            "DECLARE c SCROLL CURSOR FOR SELECT v FROM n ORDER BY v"
        ),
        "DECLARE CURSOR"
    );
    assert_eq!(
        answer(&mut session, "FETCH FORWARD 2 FROM c"),
        "v\n1\n2\nFETCH 2"
    );
    assert_eq!(answer(&mut session, "MOVE FORWARD 1 IN c"), "MOVE 1");
    assert_eq!(answer(&mut session, "FETCH PRIOR FROM c"), "v\n2\nFETCH 1");
    assert_eq!(
        answer(&mut session, "FETCH ABSOLUTE -1 FROM c"),
        "v\n5\nFETCH 1"
    );
    assert_eq!(
        answer(&mut session, "FETCH BACKWARD 2 FROM c"),
        "v\n4\n3\nFETCH 2"
    );
    assert_eq!(
        answer(&mut session, "FETCH RELATIVE 0 FROM c"),
        "v\n3\nFETCH 1"
    );
    assert_eq!(answer(&mut session, "FETCH FIRST FROM c"), "v\n1\nFETCH 1");
    assert_eq!(
        answer(&mut session, "FETCH ALL FROM c"),
        "v\n2\n3\n4\n5\nFETCH 4"
    );
    assert_eq!(answer(&mut session, "FETCH NEXT FROM c"), "v\nFETCH 0");
    assert_eq!(
        answer(&mut session, "FETCH BACKWARD ALL FROM c"),
        "v\n5\n4\n3\n2\n1\nFETCH 5"
    );
    answer(
        &mut session,
        "DECLARE forward_only NO SCROLL CURSOR FOR SELECT v FROM n",
    );
    answer(&mut session, "FETCH 2 FROM forward_only");
    let error = refusal(&mut session, "FETCH PRIOR FROM forward_only");
    assert_eq!(error.state(), SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE);
    answer(&mut session, "ROLLBACK");
    answer(&mut session, "BEGIN");
    answer(&mut session, "DECLARE c CURSOR FOR SELECT 1");
    let error = refusal(&mut session, "DECLARE c CURSOR FOR SELECT 1");
    assert_eq!(error.state(), SqlState::DUPLICATE_CURSOR);
    answer(&mut session, "ROLLBACK");
    answer(&mut session, "BEGIN");
    answer(
        &mut session,
        "DECLARE held CURSOR WITH HOLD FOR SELECT v FROM n ORDER BY v",
    );
    answer(&mut session, "DECLARE dropped CURSOR FOR SELECT v FROM n");
    answer(&mut session, "COMMIT");
    assert_eq!(answer(&mut session, "FETCH 1 FROM held"), "v\n1\nFETCH 1");
    let error = refusal(&mut session, "FETCH 1 FROM dropped");
    assert_eq!(error.state(), SqlState::INVALID_CURSOR_NAME);
    assert_eq!(answer(&mut session, "CLOSE ALL"), "CLOSE CURSOR ALL");
    let error = refusal(&mut session, "FETCH 1 FROM held");
    assert_eq!(error.state(), SqlState::INVALID_CURSOR_NAME);
    let error = refusal(&mut session, "CLOSE missing");
    assert_eq!(error.state(), SqlState::INVALID_CURSOR_NAME);
}

#[test]
fn a_commit_or_rollback_inside_a_procedure_closes_the_cursors_it_opened() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE TABLE items (id integer)",
            "INSERT INTO items VALUES (1), (2)",
            "CREATE PROCEDURE fetch_after_commit() LANGUAGE plpgsql AS $$ \
             DECLARE c refcursor := 'kept'; v integer; BEGIN \
             OPEN c FOR SELECT id FROM items ORDER BY id; COMMIT; FETCH c INTO v; END $$",
            "CREATE PROCEDURE fetch_after_rollback() LANGUAGE plpgsql AS $$ \
             DECLARE c CURSOR FOR SELECT id FROM items; v integer; BEGIN \
             OPEN c; ROLLBACK; FETCH c INTO v; END $$",
            "CREATE PROCEDURE opens(INOUT c refcursor) LANGUAGE plpgsql AS $$ BEGIN \
             OPEN c FOR SELECT id FROM items; END $$",
            "CREATE PROCEDURE fetch_after_nested_commit() LANGUAGE plpgsql AS $$ \
             DECLARE c refcursor := 'handed_over'; v integer; BEGIN \
             CALL opens(c); COMMIT; FETCH c INTO v; END $$",
            "CREATE PROCEDURE reopen_after_commit() LANGUAGE plpgsql AS $$ \
             DECLARE c CURSOR FOR SELECT id FROM items ORDER BY id; v integer; BEGIN \
             OPEN c; COMMIT; OPEN c; FETCH c INTO v; INSERT INTO items VALUES (v * 10); \
             CLOSE c; END $$",
        ],
    );
    let error = refusal(&mut session, "CALL fetch_after_commit()");
    assert_eq!(error.state(), SqlState::INVALID_CURSOR_NAME);
    assert!(
        error.to_string().contains("cursor \"kept\" does not exist"),
        "{error}"
    );
    let error = refusal(&mut session, "CALL fetch_after_rollback()");
    assert_eq!(error.state(), SqlState::INVALID_CURSOR_NAME);
    assert!(
        error.to_string().contains("cursor \"c\" does not exist"),
        "{error}"
    );
    let error = refusal(&mut session, "CALL fetch_after_nested_commit()");
    assert_eq!(error.state(), SqlState::INVALID_CURSOR_NAME);
    assert!(
        error
            .to_string()
            .contains("cursor \"handed_over\" does not exist"),
        "{error}"
    );
    assert_eq!(answer(&mut session, "CALL reopen_after_commit()"), "CALL");
    assert_eq!(
        answer(&mut session, "SELECT id FROM items ORDER BY id"),
        "id\n1\n2\n10\nSELECT 3"
    );
}

#[test]
fn cursor_loops_and_with_hold_cursors_survive_a_commit_inside_a_procedure() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE TABLE items (id integer)",
            "INSERT INTO items VALUES (1), (2), (3)",
            "CREATE TABLE copies (id integer)",
            "CREATE PROCEDURE copy_each() LANGUAGE plpgsql AS $$ \
             DECLARE c CURSOR FOR SELECT id FROM items ORDER BY id; BEGIN \
             FOR r IN c LOOP INSERT INTO copies VALUES (r.id); COMMIT; END LOOP; END $$",
            "BEGIN",
            "DECLARE held CURSOR WITH HOLD FOR SELECT id FROM items ORDER BY id",
            "COMMIT",
            "CREATE PROCEDURE read_held() LANGUAGE plpgsql AS $$ \
             DECLARE c refcursor := 'held'; v integer; BEGIN \
             COMMIT; FETCH c INTO v; INSERT INTO copies VALUES (v * 100); END $$",
        ],
    );
    assert_eq!(answer(&mut session, "CALL copy_each()"), "CALL");
    assert_eq!(
        answer(&mut session, "SELECT id FROM copies ORDER BY id"),
        "id\n1\n2\n3\nSELECT 3"
    );
    assert_eq!(answer(&mut session, "CALL read_held()"), "CALL");
    assert_eq!(answer(&mut session, "FETCH 1 FROM held"), "id\n2\nFETCH 1");
    assert_eq!(
        answer(&mut session, "SELECT max(id) FROM copies"),
        "max\n100\nSELECT 1"
    );
}

#[test]
fn a_procedure_with_a_set_clause_cannot_end_its_transaction() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE TABLE t (n integer)",
            "CREATE PROCEDURE with_setting() LANGUAGE plpgsql SET search_path = public AS $$ \
             BEGIN INSERT INTO t VALUES (1); COMMIT; END $$",
        ],
    );
    let error = refusal(&mut session, "CALL with_setting()");
    assert_eq!(error.state(), SqlState::INVALID_TRANSACTION_TERMINATION);
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM t"),
        "count\n0\nSELECT 1"
    );
}

#[test]
fn a_procedure_takes_transaction_level_advisory_locks_on_both_sides_of_its_commit() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE TABLE t (n integer)",
            "CREATE PROCEDURE lock_commit_lock() LANGUAGE plpgsql AS $$ BEGIN \
             PERFORM pg_advisory_xact_lock(42); INSERT INTO t VALUES (1); COMMIT; \
             PERFORM pg_advisory_xact_lock(42); INSERT INTO t VALUES (2); END $$",
        ],
    );
    assert_eq!(answer(&mut session, "CALL lock_commit_lock()"), "CALL");
    let mut other = Session::new(&store);
    assert_eq!(
        answer(&mut other, "SELECT pg_try_advisory_lock(42)"),
        "pg_try_advisory_lock\nt\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM t"),
        "count\n2\nSELECT 1"
    );
}

#[test]
fn refcursor_is_a_type_of_its_own() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE FUNCTION named(c refcursor) RETURNS refcursor LANGUAGE plpgsql AS $$ \
             BEGIN RETURN c; END $$",
            "CREATE FUNCTION variable_types() RETURNS text LANGUAGE plpgsql AS $$ \
             DECLARE c refcursor; b CURSOR FOR SELECT 1; BEGIN \
             RETURN pg_typeof(c)::text || ' ' || pg_typeof(b)::text; END $$",
            "CREATE TABLE kept (c refcursor)",
        ],
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT pg_typeof(1) AS a, pg_typeof('x') AS b, pg_typeof('x'::refcursor) AS c, \
             pg_typeof(named('n')) AS d"
        ),
        "a|b|c|d\ninteger|unknown|refcursor|refcursor\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT variable_types()"),
        "variable_types\nrefcursor refcursor\nSELECT 1"
    );
    let mut columns = Vec::new();
    session
        .execute("SELECT named('n')", &mut |result| {
            columns.extend(result.columns.unwrap_or_default());
        })
        .unwrap();
    assert_eq!(
        columns.first().and_then(|column| column.sql_type),
        Some(crate::types::SqlType::Catalog(
            crate::pg_catalog::CatalogType::RefCursor
        ))
    );
    assert_eq!(
        crate::pg_catalog::type_oid(crate::types::SqlType::Catalog(
            crate::pg_catalog::CatalogType::RefCursor
        )),
        1790
    );
    answer(&mut session, "INSERT INTO kept VALUES ('stored')");
    assert_eq!(
        answer(&mut session, "SELECT c, pg_typeof(c) AS t FROM kept"),
        "c|t\nstored|text\nSELECT 1"
    );
}
