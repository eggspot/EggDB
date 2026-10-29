use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Session, SqlError, TransactionStatus};

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> Result<Vec<String>, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs)
}

fn last(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => outputs.last().cloned().unwrap_or_default(),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn state(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => panic!("{sql} succeeded: {outputs:?}"),
        Err(error) => error.state().code().to_owned(),
    }
}

fn count(session: &mut Session<'_, MemoryStore>) -> String {
    let shown = last(session, "SELECT count(*) FROM t");
    shown.lines().nth(1).unwrap_or_default().to_owned()
}

fn with_table(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = Session::new(store);
    last(&mut session, "CREATE TABLE t (id integer PRIMARY KEY)");
    session
}

#[test]
fn the_statements_of_an_implicit_transaction_commit_together() {
    let store = MemoryStore::default();
    let mut session = with_table(&store);
    let mut reader = Session::new(&store);
    session.start_implicit_transaction();
    last(&mut session, "INSERT INTO t VALUES (1)");
    last(&mut session, "INSERT INTO t VALUES (2)");
    assert_eq!(session.transaction_status(), TransactionStatus::Idle);
    assert!(!session.in_block());
    assert_eq!(count(&mut session), "2");
    assert_eq!(count(&mut reader), "0");
    session.commit_implicit_transaction().unwrap();
    assert_eq!(count(&mut reader), "2");
    last(&mut session, "INSERT INTO t VALUES (3)");
    assert_eq!(count(&mut reader), "3");
}

#[test]
fn a_failure_rolls_back_everything_the_implicit_transaction_wrote() {
    let store = MemoryStore::default();
    let mut session = with_table(&store);
    session.start_implicit_transaction();
    last(&mut session, "INSERT INTO t VALUES (1)");
    assert_eq!(state(&mut session, "INSERT INTO t VALUES (1)"), "23505");
    session.roll_back_implicit_transaction().unwrap();
    assert_eq!(session.transaction_status(), TransactionStatus::Idle);
    assert_eq!(count(&mut session), "0");
    session.start_implicit_transaction();
    assert_eq!(
        state(
            &mut session,
            "INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); SELECT 1 / 0"
        ),
        "22012"
    );
    session.roll_back_implicit_transaction().unwrap();
    assert_eq!(count(&mut session), "0");
}

#[test]
fn begin_takes_in_what_the_implicit_transaction_wrote_and_commit_ends_it_with_a_warning() {
    let store = MemoryStore::default();
    let mut session = with_table(&store);
    let mut reader = Session::new(&store);
    session.start_implicit_transaction();
    let outputs = run(
        &mut session,
        "INSERT INTO t VALUES (1); BEGIN; INSERT INTO t VALUES (2)",
    )
    .unwrap();
    assert_eq!(outputs[1], "BEGIN");
    session.commit_implicit_transaction().unwrap();
    assert_eq!(session.transaction_status(), TransactionStatus::InBlock);
    assert_eq!(count(&mut reader), "0");
    last(&mut session, "ROLLBACK");
    assert_eq!(count(&mut session), "0");

    session.start_implicit_transaction();
    let mut notices = Vec::new();
    let error = session
        .execute(
            "INSERT INTO t VALUES (3); COMMIT; INSERT INTO t VALUES (4); SELECT 1 / 0",
            &mut |result| notices.extend(result.notices),
        )
        .unwrap_err();
    assert_eq!(error.state().code(), "22012");
    assert_eq!(notices, ["WARNING:  there is no transaction in progress"]);
    session.roll_back_implicit_transaction().unwrap();
    assert_eq!(count(&mut reader), "1");
}

#[test]
fn reading_statements_open_no_write_transaction() {
    let store = MemoryStore::default();
    let mut session = with_table(&store);
    let mut writer = Session::new(&store);
    session.start_implicit_transaction();
    assert_eq!(count(&mut session), "0");
    last(&mut writer, "INSERT INTO t VALUES (1)");
    session.commit_implicit_transaction().unwrap();
    assert_eq!(count(&mut session), "1");
}

#[test]
fn discard_all_and_reset_return_to_the_settings_sent_when_connecting() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    session.set_parameter("application_name", "npgsql").unwrap();
    session.remember_connection_settings();
    last(&mut session, "SET application_name = 'other'");
    assert_eq!(last(&mut session, "DISCARD ALL"), "DISCARD ALL");
    assert_eq!(session.parameter("application_name").unwrap(), "npgsql");
    last(&mut session, "SET application_name = 'other'");
    last(&mut session, "RESET application_name");
    assert_eq!(session.parameter("application_name").unwrap(), "npgsql");
    last(&mut session, "SET application_name = 'other'");
    last(&mut session, "RESET ALL");
    assert_eq!(session.parameter("application_name").unwrap(), "npgsql");
    last(&mut session, "BEGIN");
    assert_eq!(state(&mut session, "DISCARD ALL"), "25001");
    last(&mut session, "ROLLBACK");
    assert_eq!(last(&mut session, "DISCARD PLANS"), "DISCARD PLANS");
    assert_eq!(last(&mut session, "DISCARD TEMP"), "DISCARD TEMP");
    assert_eq!(last(&mut session, "DISCARD TEMPORARY"), "DISCARD TEMP");
    last(&mut session, "CREATE SEQUENCE s");
    last(&mut session, "SELECT nextval('s')");
    assert_eq!(last(&mut session, "DISCARD SEQUENCES"), "DISCARD SEQUENCES");
    assert_eq!(state(&mut session, "SELECT currval('s')"), "55000");
    assert_eq!(state(&mut session, "DISCARD everything"), "42601");
}

#[test]
fn a_cancel_stops_the_running_statement_until_it_is_cleared() {
    let store = MemoryStore::default();
    let mut session = with_table(&store);
    last(&mut session, "INSERT INTO t VALUES (1), (2), (3)");
    let signal = session.cancel_signal();
    signal.cancel();
    assert_eq!(state(&mut session, "SELECT id FROM t WHERE id > 0"), "57014");
    assert_eq!(state(&mut session, "SELECT pg_sleep(30)"), "57014");
    signal.clear();
    assert_eq!(count(&mut session), "3");
    assert_eq!(
        last(&mut session, "SELECT pg_sleep(0.01), pg_sleep(NULL) IS NULL"),
        "pg_sleep|?column?\n|t\nSELECT 1"
    );
}
