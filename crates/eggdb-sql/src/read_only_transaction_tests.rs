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
