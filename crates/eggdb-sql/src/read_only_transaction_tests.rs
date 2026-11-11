use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Dialect, QueryResult, Session, SqlError};

fn results(session: &mut Session<'_>, sql: &str) -> Result<Vec<QueryResult>, SqlError> {
    let mut results = Vec::new();
    session.execute(sql, &mut |result| results.push(result))?;
    Ok(results)
}

fn answer(session: &mut Session<'_>, sql: &str) -> String {
    match results(session, sql) {
        Ok(results) => results.last().map(render).unwrap_or_default(),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn refusal(session: &mut Session<'_>, sql: &str) -> SqlError {
    match results(session, sql) {
        Ok(results) => panic!("{sql} succeeded: {:?}", results.last().map(render)),
        Err(error) => error,
    }
}

fn notices(session: &mut Session<'_>, sql: &str) -> Vec<String> {
    match results(session, sql) {
        Ok(results) => results
            .last()
            .map(|result| result.notices.clone())
            .unwrap_or_default(),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn shown(session: &mut Session<'_>, name: &str) -> String {
    answer(session, &format!("SHOW {name}"))
        .lines()
        .nth(1)
        .unwrap_or_default()
        .to_owned()
}

fn postgresql_session(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::new(store);
    answer(
        &mut session,
        "CREATE TABLE films (code integer PRIMARY KEY, title text)",
    );
    answer(&mut session, "INSERT INTO films VALUES (1, 'Bananas')");
    session
}

#[test]
fn set_transaction_read_only_refuses_every_write_and_names_the_command() {
    let store = MemoryStore::default();
    let mut session = postgresql_session(&store);
    answer(&mut session, "BEGIN");
    assert_eq!(shown(&mut session, "transaction_read_only"), "off");
    answer(&mut session, "SET TRANSACTION READ ONLY");
    assert_eq!(shown(&mut session, "transaction_read_only"), "on");
    let error = refusal(&mut session, "INSERT INTO films VALUES (2, 'Sleeper')");
    assert_eq!(error.state().code(), "25006");
    assert_eq!(
        error.message(),
        "cannot execute INSERT in a read-only transaction"
    );
    answer(&mut session, "ROLLBACK");
    assert_eq!(shown(&mut session, "transaction_read_only"), "off");
    for (statement, command) in [
        ("UPDATE films SET title = 'Love and Death'", "UPDATE"),
        ("DELETE FROM films", "DELETE"),
        ("CREATE TABLE other (n integer)", "CREATE TABLE"),
        ("ALTER TABLE films ADD COLUMN kind text", "ALTER TABLE"),
        ("DROP TABLE films", "DROP TABLE"),
        ("TRUNCATE films", "TRUNCATE TABLE"),
        ("COMMENT ON TABLE films IS 'all of them'", "COMMENT"),
    ] {
        answer(&mut session, "START TRANSACTION READ ONLY");
        let error = refusal(&mut session, statement);
        assert_eq!(error.state().code(), "25006", "{statement}");
        assert_eq!(
            error.message(),
            format!("cannot execute {command} in a read-only transaction"),
        );
        answer(&mut session, "ROLLBACK");
    }
    assert_eq!(
        answer(&mut session, "SELECT title FROM films"),
        "title\nBananas\nSELECT 1"
    );
}

#[test]
fn a_read_only_transaction_writes_temporary_tables_but_never_creates_or_truncates_them() {
    let store = MemoryStore::default();
    let mut session = postgresql_session(&store);
    answer(&mut session, "CREATE TEMP TABLE scratch (n integer)");
    answer(&mut session, "BEGIN READ ONLY");
    answer(&mut session, "INSERT INTO scratch VALUES (1), (2)");
    answer(&mut session, "UPDATE scratch SET n = n * 10");
    answer(&mut session, "COMMIT");
    assert_eq!(
        answer(&mut session, "SELECT n FROM scratch ORDER BY n"),
        "n\n10\n20\nSELECT 2"
    );
    answer(&mut session, "BEGIN READ ONLY");
    answer(&mut session, "DELETE FROM scratch WHERE n = 10");
    answer(&mut session, "ROLLBACK");
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM scratch"),
        "count\n2\nSELECT 1"
    );
    answer(&mut session, "BEGIN READ ONLY");
    let error = refusal(&mut session, "CREATE TEMP TABLE more_scratch (n integer)");
    assert_eq!(
        error.message(),
        "cannot execute CREATE TABLE in a read-only transaction"
    );
    answer(&mut session, "ROLLBACK");
    answer(&mut session, "BEGIN READ ONLY");
    let error = refusal(&mut session, "TRUNCATE scratch");
    assert_eq!(error.state().code(), "25006");
    answer(&mut session, "ROLLBACK");
}

#[test]
fn nextval_is_refused_in_a_read_only_transaction() {
    let store = MemoryStore::default();
    let mut session = postgresql_session(&store);
    answer(&mut session, "CREATE SEQUENCE serial_numbers");
    answer(&mut session, "BEGIN READ ONLY");
    let error = refusal(&mut session, "SELECT nextval('serial_numbers')");
    assert_eq!(error.state().code(), "25006");
    assert_eq!(
        error.message(),
        "cannot execute nextval() in a read-only transaction"
    );
    answer(&mut session, "ROLLBACK");
    answer(&mut session, "BEGIN READ ONLY");
    let error = refusal(&mut session, "SELECT setval('serial_numbers', 10)");
    assert_eq!(error.state().code(), "25006");
    assert_eq!(
        error.message(),
        "cannot execute setval() in a read-only transaction"
    );
    answer(&mut session, "ROLLBACK");
    assert_eq!(
        answer(&mut session, "SELECT nextval('serial_numbers')"),
        "nextval\n1\nSELECT 1"
    );
}

#[test]
fn default_transaction_read_only_makes_every_new_transaction_read_only() {
    let store = MemoryStore::default();
    let mut session = postgresql_session(&store);
    answer(&mut session, "SET default_transaction_read_only = on");
    assert_eq!(shown(&mut session, "transaction_read_only"), "on");
    let error = refusal(&mut session, "INSERT INTO films VALUES (2, 'Sleeper')");
    assert_eq!(error.state().code(), "25006");
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM films"),
        "count\n1\nSELECT 1"
    );
    answer(&mut session, "BEGIN");
    assert_eq!(shown(&mut session, "transaction_read_only"), "on");
    assert_eq!(
        refusal(&mut session, "DELETE FROM films").state().code(),
        "25006"
    );
    answer(&mut session, "ROLLBACK");
    answer(&mut session, "BEGIN READ WRITE");
    answer(&mut session, "INSERT INTO films VALUES (2, 'Sleeper')");
    answer(&mut session, "COMMIT");
    answer(
        &mut session,
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
    );
    assert_eq!(shown(&mut session, "default_transaction_read_only"), "off");
    answer(&mut session, "INSERT INTO films VALUES (3, 'Zelig')");
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM films"),
        "count\n3\nSELECT 1"
    );
    assert_eq!(
        refusal(&mut session, "SET default_transaction_read_only = maybe")
            .state()
            .code(),
        "22023"
    );
}

#[test]
fn set_transaction_after_the_first_query_is_refused_as_postgresql_refuses_it() {
    let store = MemoryStore::default();
    let mut session = postgresql_session(&store);
    answer(&mut session, "BEGIN");
    answer(&mut session, "SELECT 1");
    let error = refusal(
        &mut session,
        "SET TRANSACTION ISOLATION LEVEL READ COMMITTED",
    );
    assert_eq!(error.state().code(), "25001");
    assert_eq!(
        error.message(),
        "SET TRANSACTION ISOLATION LEVEL must be called before any query"
    );
    answer(&mut session, "ROLLBACK");
    answer(&mut session, "BEGIN");
    answer(
        &mut session,
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
    );
    answer(&mut session, "SELECT 1");
    answer(
        &mut session,
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
    );
    answer(&mut session, "COMMIT");
    answer(&mut session, "BEGIN READ ONLY");
    answer(&mut session, "SELECT 1");
    let error = refusal(&mut session, "SET TRANSACTION READ WRITE");
    assert_eq!(error.state().code(), "25001");
    assert_eq!(
        error.message(),
        "transaction read-write mode must be set before any query"
    );
    answer(&mut session, "ROLLBACK");
    answer(&mut session, "BEGIN READ ONLY");
    answer(&mut session, "SET TRANSACTION READ WRITE");
    answer(&mut session, "INSERT INTO films VALUES (2, 'Sleeper')");
    answer(&mut session, "COMMIT");
    answer(&mut session, "BEGIN");
    answer(
        &mut session,
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE, READ ONLY, DEFERRABLE",
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM films"),
        "count\n2\nSELECT 1"
    );
    answer(&mut session, "COMMIT");
}

#[test]
fn a_transaction_that_wrote_can_turn_read_only_and_still_commits_what_it_wrote() {
    let store = MemoryStore::default();
    let mut session = postgresql_session(&store);
    answer(&mut session, "BEGIN");
    answer(&mut session, "INSERT INTO films VALUES (2, 'Sleeper')");
    answer(&mut session, "SET transaction_read_only = on");
    assert_eq!(shown(&mut session, "transaction_read_only"), "on");
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM films"),
        "count\n2\nSELECT 1"
    );
    answer(&mut session, "COMMIT");
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM films"),
        "count\n2\nSELECT 1"
    );
    answer(&mut session, "BEGIN");
    answer(&mut session, "INSERT INTO films VALUES (3, 'Zelig')");
    answer(&mut session, "SET TRANSACTION READ ONLY");
    assert_eq!(
        refusal(&mut session, "INSERT INTO films VALUES (4, 'Manhattan')")
            .state()
            .code(),
        "25006"
    );
    answer(&mut session, "ROLLBACK");
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM films"),
        "count\n2\nSELECT 1"
    );
}

#[test]
fn set_transaction_outside_a_block_warns_and_changes_nothing() {
    let store = MemoryStore::default();
    let mut session = postgresql_session(&store);
    assert_eq!(
        notices(&mut session, "SET TRANSACTION READ ONLY"),
        vec!["WARNING:  SET TRANSACTION can only be used in transaction blocks".to_owned()]
    );
    answer(&mut session, "INSERT INTO films VALUES (2, 'Sleeper')");
    assert_eq!(shown(&mut session, "transaction_read_only"), "off");
}

#[test]
fn a_savepoint_rollback_in_a_read_only_transaction_undoes_its_temporary_writes() {
    let store = MemoryStore::default();
    let mut session = postgresql_session(&store);
    answer(&mut session, "CREATE TEMP TABLE scratch (n integer)");
    answer(&mut session, "BEGIN READ ONLY");
    answer(&mut session, "INSERT INTO scratch VALUES (1)");
    answer(&mut session, "SAVEPOINT before_two");
    answer(&mut session, "INSERT INTO scratch VALUES (2)");
    answer(&mut session, "ROLLBACK TO SAVEPOINT before_two");
    answer(&mut session, "COMMIT");
    assert_eq!(
        answer(&mut session, "SELECT n FROM scratch"),
        "n\n1\nSELECT 1"
    );
}

#[test]
fn mysql_read_only_transactions_refuse_writes_with_error_1792() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    answer(&mut session, "CREATE TABLE t (n INT)");
    answer(&mut session, "START TRANSACTION READ ONLY");
    let error = refusal(&mut session, "INSERT INTO t VALUES (1)");
    assert_eq!(error.vendor_code(), Some(1792));
    assert_eq!(
        error.message(),
        "Cannot execute statement in a READ ONLY transaction."
    );
    answer(&mut session, "COMMIT");
    answer(&mut session, "SET TRANSACTION READ ONLY");
    assert_eq!(
        refusal(&mut session, "INSERT INTO t VALUES (1)").vendor_code(),
        Some(1792)
    );
    answer(&mut session, "INSERT INTO t VALUES (2)");
    answer(&mut session, "START TRANSACTION");
    let error = refusal(&mut session, "SET TRANSACTION READ ONLY");
    assert_eq!(error.vendor_code(), Some(1568));
    answer(&mut session, "ROLLBACK");
    answer(&mut session, "SET SESSION TRANSACTION READ ONLY");
    assert_eq!(
        answer(&mut session, "SELECT @@transaction_read_only AS ro"),
        "ro\n1\nSELECT 1"
    );
    assert_eq!(
        refusal(&mut session, "UPDATE t SET n = 3").vendor_code(),
        Some(1792)
    );
    answer(&mut session, "SET SESSION TRANSACTION READ WRITE");
    answer(
        &mut session,
        "START TRANSACTION WITH CONSISTENT SNAPSHOT, READ WRITE",
    );
    answer(&mut session, "INSERT INTO t VALUES (3)");
    answer(&mut session, "COMMIT");
    assert_eq!(
        answer(&mut session, "SELECT count(*) AS c FROM t"),
        "c\n2\nSELECT 1"
    );
}

#[test]
fn oracle_read_only_transactions_refuse_changes_with_ora_01456() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    answer(&mut session, "CREATE TABLE emp (empno NUMBER)");
    answer(&mut session, "SET TRANSACTION READ ONLY NAME 'report'");
    let error = refusal(&mut session, "INSERT INTO emp VALUES (7369)");
    assert_eq!(error.vendor_code(), Some(1456));
    assert_eq!(
        error.message(),
        "may not perform insert/delete/update operation inside a READ ONLY transaction"
    );
    let error = refusal(&mut session, "SET TRANSACTION READ WRITE");
    assert_eq!(error.vendor_code(), Some(1453));
    answer(&mut session, "COMMIT");
    answer(&mut session, "INSERT INTO emp VALUES (7369)");
    let error = refusal(&mut session, "SET TRANSACTION READ ONLY");
    assert_eq!(error.vendor_code(), Some(1453));
    answer(&mut session, "COMMIT");
    answer(&mut session, "SET TRANSACTION READ ONLY");
    answer(&mut session, "CREATE TABLE dept (deptno NUMBER)");
    answer(&mut session, "INSERT INTO dept VALUES (10)");
    answer(&mut session, "COMMIT");
}

#[test]
fn a_refused_set_transaction_aborts_the_postgresql_transaction() {
    let store = MemoryStore::default();
    let mut session = postgresql_session(&store);
    for refused in [
        "SET TRANSACTION READ WRITE",
        "SET transaction_isolation = 'bogus'",
        "SET transaction_read_only = maybe",
    ] {
        answer(&mut session, "BEGIN READ ONLY");
        answer(&mut session, "SELECT 1");
        refusal(&mut session, refused);
        for after in [
            "SELECT 2",
            "SAVEPOINT later",
            "CREATE TEMPORARY TABLE scratch (n integer)",
        ] {
            assert_eq!(
                refusal(&mut session, after).state().code(),
                "25P02",
                "{after} after {refused}"
            );
        }
        answer(&mut session, "ROLLBACK");
    }
    answer(&mut session, "BEGIN");
    answer(&mut session, "INSERT INTO films VALUES (2, 'Sleeper')");
    assert_eq!(
        refusal(
            &mut session,
            "SET TRANSACTION ISOLATION LEVEL READ COMMITTED"
        )
        .state()
        .code(),
        "25001"
    );
    assert_eq!(
        refusal(&mut session, "INSERT INTO films VALUES (3, 'Zelig')")
            .state()
            .code(),
        "25P02"
    );
    assert_eq!(answer(&mut session, "COMMIT"), "ROLLBACK");
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM films"),
        "count\n1\nSELECT 1"
    );
}

#[test]
fn a_read_only_transaction_refuses_a_write_that_matches_no_rows() {
    let store = MemoryStore::default();
    let mut session = postgresql_session(&store);
    answer(&mut session, "CREATE TABLE empty (id integer)");
    answer(&mut session, "CREATE TEMPORARY TABLE scratch (id integer)");
    answer(&mut session, "SET default_transaction_read_only = on");
    for (statement, command) in [
        ("UPDATE empty SET id = 1", "UPDATE"),
        ("DELETE FROM films WHERE false", "DELETE"),
        ("INSERT INTO empty SELECT 1 WHERE false", "INSERT"),
    ] {
        let error = refusal(&mut session, statement);
        assert_eq!(error.state().code(), "25006", "{statement}");
        assert_eq!(
            error.message(),
            format!("cannot execute {command} in a read-only transaction"),
            "{statement}"
        );
    }
    answer(&mut session, "UPDATE scratch SET id = 1");
    answer(&mut session, "INSERT INTO scratch VALUES (1)");
    answer(&mut session, "DELETE FROM scratch WHERE false");
}

#[test]
fn a_write_inside_with_that_matches_no_rows_is_refused_in_a_read_only_transaction() {
    let store = MemoryStore::default();
    let mut session = postgresql_session(&store);
    answer(&mut session, "CREATE TEMPORARY TABLE scratch (id integer)");
    answer(&mut session, "BEGIN READ ONLY");
    let error = refusal(
        &mut session,
        "WITH gone AS (DELETE FROM films WHERE false RETURNING *) SELECT * FROM gone",
    );
    assert_eq!(error.state().code(), "25006");
    assert_eq!(
        error.message(),
        "cannot execute DELETE in a read-only transaction"
    );
    answer(&mut session, "ROLLBACK");
    answer(&mut session, "BEGIN READ ONLY");
    let error = refusal(
        &mut session,
        "WITH none AS (SELECT 1 WHERE false) UPDATE films SET title = 'x' WHERE false",
    );
    assert_eq!(error.state().code(), "25006");
    answer(&mut session, "ROLLBACK");
    answer(&mut session, "BEGIN READ ONLY");
    answer(
        &mut session,
        "WITH gone AS (DELETE FROM scratch WHERE false RETURNING *) SELECT * FROM gone",
    );
    answer(&mut session, "ROLLBACK");
}

#[test]
fn call_and_do_outside_a_block_are_refused_when_new_transactions_are_read_only() {
    let store = MemoryStore::default();
    let mut session = postgresql_session(&store);
    answer(
        &mut session,
        "CREATE PROCEDURE add_film(n integer) LANGUAGE plpgsql AS $$ BEGIN INSERT INTO films VALUES (n, 'Added'); END $$",
    );
    answer(
        &mut session,
        "CREATE PROCEDURE add_and_commit(n integer) LANGUAGE plpgsql AS $$ BEGIN INSERT INTO films VALUES (n, 'Added'); COMMIT; END $$",
    );
    for setting in [
        "SET default_transaction_read_only = on",
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY",
    ] {
        answer(&mut session, setting);
        for statement in [
            "CALL add_film(2)",
            "CALL add_and_commit(3)",
            "DO $$ BEGIN INSERT INTO films VALUES (4, 'Done'); END $$",
        ] {
            let error = refusal(&mut session, statement);
            assert_eq!(error.state().code(), "25006", "{setting}: {statement}");
        }
        answer(&mut session, "SET default_transaction_read_only = off");
    }
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM films"),
        "count\n1\nSELECT 1"
    );
}

#[test]
fn row_locking_selects_are_refused_in_a_read_only_transaction_and_lock_table_is_taken() {
    let store = MemoryStore::default();
    let mut session = postgresql_session(&store);
    answer(&mut session, "CREATE TEMP TABLE scratch (n integer)");
    for (clause, command) in [
        ("FOR UPDATE", "SELECT FOR UPDATE"),
        ("FOR NO KEY UPDATE", "SELECT FOR NO KEY UPDATE"),
        ("FOR SHARE", "SELECT FOR SHARE"),
        ("FOR KEY SHARE", "SELECT FOR KEY SHARE"),
        ("FOR UPDATE OF films NOWAIT", "SELECT FOR UPDATE"),
    ] {
        answer(&mut session, "BEGIN READ ONLY");
        let error = refusal(&mut session, &format!("SELECT * FROM films {clause}"));
        assert_eq!(error.state().code(), "25006", "{clause}");
        assert_eq!(
            error.message(),
            format!("cannot execute {command} in a read-only transaction")
        );
        assert_eq!(
            refusal(&mut session, "SELECT 1").state().code(),
            "25P02",
            "{clause}"
        );
        answer(&mut session, "ROLLBACK");
    }
    answer(&mut session, "BEGIN READ ONLY");
    answer(&mut session, "INSERT INTO scratch VALUES (1)");
    assert_eq!(
        answer(&mut session, "SELECT n FROM scratch FOR UPDATE"),
        "n\n1\nSELECT 1"
    );
    answer(&mut session, "LOCK TABLE films IN ACCESS EXCLUSIVE MODE");
    answer(&mut session, "COMMIT");
    answer(&mut session, "SET default_transaction_read_only = on");
    assert_eq!(
        refusal(&mut session, "SELECT title FROM films FOR SHARE")
            .state()
            .code(),
        "25006"
    );
    answer(&mut session, "SET default_transaction_read_only = off");
    assert_eq!(
        answer(&mut session, "SELECT title FROM films FOR UPDATE"),
        "title\nBananas\nSELECT 1"
    );
}

#[test]
fn a_writing_function_called_from_select_in_a_read_only_block_is_refused_with_25006() {
    let store = MemoryStore::default();
    let mut session = postgresql_session(&store);
    answer(
        &mut session,
        "CREATE FUNCTION add_film() RETURNS integer LANGUAGE plpgsql AS $$ BEGIN INSERT INTO films VALUES (9, 'Added'); RETURN 9; END $$",
    );
    answer(
        &mut session,
        "CREATE FUNCTION film_count() RETURNS bigint LANGUAGE plpgsql AS $$ BEGIN RETURN (SELECT count(*) FROM films); END $$",
    );
    answer(&mut session, "BEGIN READ ONLY");
    assert_eq!(
        answer(&mut session, "SELECT film_count()"),
        "film_count\n1\nSELECT 1"
    );
    let error = refusal(&mut session, "SELECT add_film()");
    assert_eq!(error.state().code(), "25006");
    answer(&mut session, "ROLLBACK");
    assert_eq!(
        answer(&mut session, "SELECT add_film()"),
        "add_film\n9\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM films"),
        "count\n2\nSELECT 1"
    );
}

#[test]
fn default_transaction_read_only_applies_to_each_transaction_of_a_procedure_that_commits() {
    let store = MemoryStore::default();
    let mut session = postgresql_session(&store);
    answer(
        &mut session,
        "CREATE PROCEDURE count_and_commit() LANGUAGE plpgsql AS $$ BEGIN PERFORM count(*) FROM films; COMMIT; PERFORM count(*) FROM films; END $$",
    );
    answer(
        &mut session,
        "CREATE PROCEDURE commit_then_add(n integer) LANGUAGE plpgsql AS $$ BEGIN COMMIT; INSERT INTO films VALUES (n, 'Added'); END $$",
    );
    answer(&mut session, "SET default_transaction_read_only = on");
    assert_eq!(answer(&mut session, "CALL count_and_commit()"), "CALL");
    let error = refusal(&mut session, "CALL commit_then_add(2)");
    assert_eq!(error.state().code(), "25006");
    assert_eq!(
        error.message(),
        "cannot execute CALL in a read-only transaction"
    );
    answer(&mut session, "SET default_transaction_read_only = off");
    answer(&mut session, "CALL commit_then_add(3)");
    assert_eq!(
        answer(&mut session, "SELECT code FROM films ORDER BY code"),
        "code\n1\n3\nSELECT 2"
    );
}

#[test]
fn set_transaction_in_an_implicit_transaction_before_it_writes_sets_that_transaction() {
    let store = MemoryStore::default();
    let mut session = postgresql_session(&store);
    session.start_implicit_transaction();
    let outputs = results(
        &mut session,
        "SET TRANSACTION READ ONLY; SHOW transaction_read_only",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert!(outputs[0].notices.is_empty(), "{:?}", outputs[0].notices);
    assert_eq!(render(&outputs[1]).lines().nth(1), Some("on"));
    let error = refusal(&mut session, "INSERT INTO films VALUES (2, 'Sleeper')");
    assert_eq!(error.state().code(), "25006");
    session.roll_back_implicit_transaction().unwrap();
    assert_eq!(shown_setting(&mut session), "off");

    answer(&mut session, "SET default_transaction_read_only = on");
    session.start_implicit_transaction();
    results(
        &mut session,
        "SET TRANSACTION READ WRITE; INSERT INTO films VALUES (2, 'Sleeper')",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    session.commit_implicit_transaction().unwrap();
    assert_eq!(shown_setting(&mut session), "on");
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM films"),
        "count\n2\nSELECT 1"
    );

    let warned = notices(&mut session, "SET TRANSACTION READ ONLY");
    assert_eq!(
        warned,
        ["WARNING:  SET TRANSACTION can only be used in transaction blocks"]
    );
}

fn shown_setting(session: &mut Session<'_>) -> String {
    shown(session, "transaction_read_only")
}

#[test]
fn row_locking_reaches_the_tables_behind_partitions_views_and_derived_tables() {
    let store = MemoryStore::default();
    let mut session = postgresql_session(&store);
    for sql in [
        "CREATE TABLE parted (id integer, day date) PARTITION BY RANGE (day)",
        "CREATE TABLE parted_2024 PARTITION OF parted FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')",
        "INSERT INTO parted VALUES (1, '2024-05-05')",
        "CREATE VIEW film_titles AS SELECT title FROM films",
        "CREATE TEMP TABLE scratch (n integer)",
        "INSERT INTO scratch VALUES (7)",
    ] {
        answer(&mut session, sql);
    }
    for query in [
        "SELECT * FROM parted FOR UPDATE",
        "SELECT * FROM film_titles FOR SHARE",
        "SELECT * FROM (SELECT * FROM films) s FOR UPDATE",
        "SELECT * FROM (SELECT * FROM (SELECT title FROM film_titles) i) s FOR UPDATE",
        "SELECT n FROM scratch, LATERAL (SELECT title FROM films) f FOR UPDATE",
        "WITH c AS (SELECT * FROM parted) SELECT * FROM c FOR UPDATE",
        "DECLARE locked CURSOR FOR SELECT * FROM films FOR UPDATE",
    ] {
        answer(&mut session, "BEGIN READ ONLY");
        let error = refusal(&mut session, query);
        assert_eq!(error.state().code(), "25006", "{query}");
        answer(&mut session, "ROLLBACK");
    }
    answer(&mut session, "BEGIN READ ONLY");
    for (query, rows) in [
        (
            "SELECT * FROM (SELECT n FROM scratch) s FOR UPDATE",
            "n\n7\nSELECT 1",
        ),
        (
            "WITH c AS (SELECT n FROM scratch) SELECT n FROM c FOR UPDATE",
            "n\n7\nSELECT 1",
        ),
        (
            "WITH RECURSIVE c (n) AS (SELECT n FROM scratch UNION ALL SELECT n + 1 FROM c WHERE n < 8) SELECT n FROM c FOR UPDATE",
            "n\n7\n8\nSELECT 2",
        ),
    ] {
        assert_eq!(answer(&mut session, query), rows, "{query}");
    }
    answer(&mut session, "COMMIT");
}

#[test]
fn row_locking_a_table_with_row_security_is_refused_for_a_checked_role_in_a_read_only_transaction()
{
    let store = MemoryStore::default();
    let mut admin = postgresql_session(&store);
    for sql in [
        "CREATE USER ada LOGIN",
        "GRANT SELECT ON films TO ada",
        "ALTER TABLE films ENABLE ROW LEVEL SECURITY",
        "CREATE POLICY every_film ON films FOR SELECT TO PUBLIC USING (true)",
    ] {
        answer(&mut admin, sql);
    }
    let mut ada = Session::new(&store);
    if let Err(error) = ada.identify_role("ada", "eggdb") {
        panic!("ada: {error}");
    }
    assert_eq!(
        answer(&mut ada, "SELECT title FROM films"),
        "title\nBananas\nSELECT 1"
    );
    answer(&mut ada, "BEGIN READ ONLY");
    let error = refusal(&mut ada, "SELECT title FROM films FOR UPDATE");
    assert_eq!(error.state().code(), "25006");
    answer(&mut ada, "ROLLBACK");
}

#[test]
fn an_exported_snapshot_shows_another_session_what_the_exporter_sees() {
    let store = MemoryStore::default();
    let mut exporter = Session::new(&store);
    let mut writer = Session::new(&store);
    let mut importer = Session::new(&store);
    answer(&mut exporter, "CREATE TABLE t (x int); INSERT INTO t VALUES (1)");
    answer(&mut exporter, "BEGIN READ ONLY");
    let exported = answer(&mut exporter, "SELECT pg_export_snapshot()");
    let id = exported.lines().nth(1).unwrap_or_default().to_owned();
    answer(&mut writer, "INSERT INTO t VALUES (2)");
    answer(&mut importer, "BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY");
    answer(&mut importer, &format!("SET TRANSACTION SNAPSHOT '{id}'"));
    assert_eq!(
        answer(&mut importer, "SELECT count(*) FROM t"),
        "count\n1\nSELECT 1"
    );
    let error = refusal(&mut importer, &format!("SET TRANSACTION SNAPSHOT '{id}'"));
    assert_eq!(error.state().code(), "25001", "{error}");
    answer(&mut importer, "COMMIT");
    answer(&mut exporter, "COMMIT");
    answer(&mut importer, "BEGIN READ ONLY");
    let error = refusal(&mut importer, &format!("SET TRANSACTION SNAPSHOT '{id}'"));
    assert_eq!(error.state().code(), "22023", "{error}");
    answer(&mut importer, "ROLLBACK");
    let error = refusal(&mut importer, "SET TRANSACTION SNAPSHOT '00000001-00000001-1'");
    assert_eq!(error.state().code(), "25P01", "{error}");
    let error = refusal(&mut importer, "SELECT pg_export_snapshot()");
    assert_eq!(error.state().code(), "0A000", "{error}");
}

#[test]
fn lock_timeout_cancels_a_statement_waiting_for_the_writer() {
    let store = MemoryStore::default();
    let mut holder = Session::new(&store);
    let mut waiter = Session::new(&store);
    answer(&mut holder, "CREATE TABLE lt (x int)");
    answer(&mut holder, "BEGIN");
    answer(&mut holder, "INSERT INTO lt VALUES (1)");
    answer(&mut waiter, "SET lock_timeout = '10ms'");
    let error = refusal(&mut waiter, "INSERT INTO lt VALUES (2)");
    assert_eq!(error.state().code(), "55P03", "{error}");
    assert_eq!(error.message(), "canceling statement due to lock timeout");
    answer(&mut holder, "COMMIT");
    answer(&mut waiter, "INSERT INTO lt VALUES (2)");
}

#[test]
fn show_transaction_isolation_gives_the_level_the_transaction_asked_for() {
    let store = MemoryStore::default();
    let mut session = postgresql_session(&store);
    let shown = |session: &mut Session<'_>, name: &str| answer(session, &format!("SHOW {name}"));
    assert!(shown(&mut session, "transaction_isolation").contains("serializable"));
    answer(&mut session, "BEGIN ISOLATION LEVEL READ COMMITTED");
    assert!(shown(&mut session, "transaction_isolation").contains("read committed"));
    answer(&mut session, "COMMIT");
    assert!(shown(&mut session, "transaction_isolation").contains("serializable"));
    answer(
        &mut session,
        "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL REPEATABLE READ",
    );
    assert!(shown(&mut session, "default_transaction_isolation").contains("repeatable read"));
    answer(&mut session, "BEGIN");
    assert!(shown(&mut session, "transaction_isolation").contains("repeatable read"));
    answer(&mut session, "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED");
    assert!(shown(&mut session, "transaction_isolation").contains("read uncommitted"));
    answer(&mut session, "ROLLBACK");
    assert_eq!(
        refusal(&mut session, "SET default_transaction_isolation = 'sometimes'").state().code(),
        "22023"
    );
}
