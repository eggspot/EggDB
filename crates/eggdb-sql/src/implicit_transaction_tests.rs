use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{ArrayElement, Parameter, Session, SqlError, SqlType, TransactionStatus};

fn run(session: &mut Session<'_>, sql: &str) -> Result<Vec<String>, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs)
}

fn last(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => outputs.last().cloned().unwrap_or_default(),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn state(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => panic!("{sql} succeeded: {outputs:?}"),
        Err(error) => error.state().code().to_owned(),
    }
}

fn count(session: &mut Session<'_>) -> String {
    let shown = last(session, "SELECT count(*) FROM t");
    shown.lines().nth(1).unwrap_or_default().to_owned()
}

fn with_table(store: &MemoryStore) -> Session<'_> {
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
    assert_eq!(
        state(&mut session, "SELECT id FROM t WHERE id > 0"),
        "57014"
    );
    assert_eq!(state(&mut session, "SELECT pg_sleep(30)"), "57014");
    signal.clear();
    assert_eq!(count(&mut session), "3");
    assert_eq!(
        last(
            &mut session,
            "SELECT pg_sleep(0.01), pg_sleep(NULL) IS NULL"
        ),
        "pg_sleep|?column?\n|t\nSELECT 1"
    );
}

fn children(session: &mut Session<'_>) -> String {
    let shown = last(session, "SELECT count(*) FROM child");
    shown.lines().nth(1).unwrap_or_default().to_owned()
}

#[test]
fn deferred_foreign_keys_are_checked_when_the_implicit_transaction_commits() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(&mut session, "CREATE TABLE parent (id integer PRIMARY KEY)");
    last(
        &mut session,
        "CREATE TABLE child (id integer PRIMARY KEY, parent_id integer REFERENCES parent DEFERRABLE INITIALLY DEFERRED)",
    );
    session.start_implicit_transaction();
    last(&mut session, "INSERT INTO child VALUES (1, 99)");
    let committed = session.commit_implicit_transaction();
    assert_eq!(
        committed.map_err(|error| error.state().code().to_owned()),
        Err("23503".to_owned())
    );
    assert_eq!(children(&mut session), "0");
    session.start_implicit_transaction();
    last(&mut session, "INSERT INTO child VALUES (2, 7)");
    last(&mut session, "INSERT INTO parent VALUES (7)");
    session.commit_implicit_transaction().unwrap();
    assert_eq!(children(&mut session), "1");
}

#[test]
fn notifications_of_an_implicit_transaction_are_sent_when_it_commits() {
    let store = MemoryStore::default();
    let mut session = with_table(&store);
    session.start_implicit_transaction();
    last(&mut session, "INSERT INTO t VALUES (1)");
    last(&mut session, "NOTIFY jobs, 'one'");
    assert!(session.take_notifications().is_empty());
    session.commit_implicit_transaction().unwrap();
    let sent = session.take_notifications();
    assert_eq!(sent.len(), 1, "{sent:?}");
    session.start_implicit_transaction();
    last(&mut session, "INSERT INTO t VALUES (2)");
    last(&mut session, "NOTIFY jobs, 'two'");
    session.roll_back_implicit_transaction().unwrap();
    assert!(session.take_notifications().is_empty());
}

const NPGSQL_RESET: &str = "SET SESSION AUTHORIZATION DEFAULT;RESET ALL;CLOSE ALL;UNLISTEN *;\
                            SELECT pg_advisory_unlock_all();DISCARD SEQUENCES;DISCARD TEMP";

#[test]
fn npgsqls_reset_of_a_connection_holding_prepared_statements_runs_as_one_query() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let mut other = Session::new(&store);
    session.set_parameter("application_name", "npgsql").unwrap();
    session.remember_connection_settings();
    last(&mut session, "SET application_name = 'other'");
    last(&mut session, "LISTEN jobs");
    last(&mut session, "SELECT pg_advisory_lock(7)");
    last(&mut session, "CREATE SEQUENCE s");
    last(&mut session, "SELECT nextval('s')");
    session.start_implicit_transaction();
    let outputs = run(&mut session, NPGSQL_RESET).unwrap();
    session.commit_implicit_transaction().unwrap();
    assert_eq!(
        outputs,
        [
            "SET",
            "RESET",
            "CLOSE CURSOR ALL",
            "UNLISTEN",
            "pg_advisory_unlock_all\n\nSELECT 1",
            "DISCARD SEQUENCES",
            "DISCARD TEMP"
        ]
    );
    assert_eq!(session.parameter("application_name").unwrap(), "npgsql");
    assert_eq!(state(&mut session, "SELECT currval('s')"), "55000");
    assert_eq!(
        last(&mut other, "SELECT pg_try_advisory_lock(7)"),
        "pg_try_advisory_lock\nt\nSELECT 1"
    );
}

#[test]
fn session_authorization_is_set_and_reset_only_to_the_sessions_own_user() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let user = session.parameter("session_authorization").unwrap();
    assert_eq!(
        last(&mut session, "SET SESSION AUTHORIZATION DEFAULT"),
        "SET"
    );
    assert_eq!(state(&mut session, "SET AUTHORIZATION DEFAULT"), "0A000");
    assert_eq!(
        last(&mut session, &format!("SET SESSION AUTHORIZATION '{user}'")),
        "SET"
    );
    assert_eq!(
        last(
            &mut session,
            &format!("SET SESSION AUTHORIZATION \"{user}\"")
        ),
        "SET"
    );
    assert_eq!(last(&mut session, "RESET SESSION AUTHORIZATION"), "RESET");
    assert_eq!(
        state(&mut session, "SET SESSION AUTHORIZATION someone_else"),
        "0A000"
    );
    assert_eq!(session.parameter("session_authorization").unwrap(), user);
    assert_eq!(state(&mut session, "SET SESSION AUTHORIZATION"), "42601");
}

fn first_column_type(session: &mut Session<'_>, sql: &str) -> Option<SqlType> {
    let mut found = None;
    session
        .execute(sql, &mut |result| {
            found = result
                .columns
                .as_ref()
                .and_then(|columns| columns.first())
                .and_then(|column| column.sql_type);
        })
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    found
}

#[test]
fn functions_that_return_nothing_report_the_void_type() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let void = Some(SqlType::Catalog(crate::CatalogType::Void));
    for sql in [
        "SELECT pg_advisory_unlock_all()",
        "SELECT pg_advisory_lock(1)",
        "SELECT pg_advisory_xact_lock(2)",
        "SELECT pg_sleep(0)",
        "SELECT pg_notify('jobs', 'x')",
    ] {
        assert_eq!(first_column_type(&mut session, sql), void, "{sql}");
    }
    assert_eq!(
        first_column_type(&mut session, "SELECT pg_try_advisory_lock(3)"),
        Some(SqlType::Boolean)
    );
    last(
        &mut session,
        "CREATE FUNCTION nothing() RETURNS void LANGUAGE plpgsql AS $$ BEGIN END $$",
    );
    assert_eq!(first_column_type(&mut session, "SELECT nothing()"), void);
    last(
        &mut session,
        "CREATE FUNCTION nothing_in_sql() RETURNS void LANGUAGE sql AS 'SELECT 1'",
    );
    assert_eq!(
        first_column_type(&mut session, "SELECT nothing_in_sql()"),
        void
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT nothing() IS NULL AS p, nothing_in_sql() IS NULL AS s, nothing() AS v"
        ),
        "p|s|v\nf|t|\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT typname, typtype, typlen FROM pg_type WHERE oid = 2278"
        ),
        "typname|typtype|typlen\nvoid|p|4\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT 'void'::regtype::oid AS o, ''::void AS v"
        ),
        "o|v\n2278|\nSELECT 1"
    );
}

fn prepared_rows(session: &mut Session<'_>, sql: &str, parameters: &[Parameter]) -> String {
    let mut outputs = Vec::new();
    session
        .execute_prepared(sql, parameters, &mut |result| {
            outputs.push(render(&result));
            true
        })
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    outputs.pop().unwrap_or_default()
}

fn array_parameter(element: ArrayElement, text: &str) -> Parameter {
    Parameter {
        declared_type: Some(SqlType::Array(element)),
        value: Some(text.to_owned()),
    }
}

#[test]
fn array_parameters_bind_as_arrays_of_their_declared_element_type() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE tagged (id integer PRIMARY KEY, tags text[], scores bigint[])",
    );
    let inserted = prepared_rows(
        &mut session,
        "INSERT INTO tagged VALUES ($1, $2, $3)",
        &[
            Parameter {
                declared_type: Some(SqlType::Integer),
                value: Some("1".to_owned()),
            },
            array_parameter(ArrayElement::Text, "{a,\"b c\",NULL}"),
            array_parameter(ArrayElement::BigInt, "{7,-1}"),
        ],
    );
    assert_eq!(inserted, "INSERT 0 1");
    assert_eq!(
        last(
            &mut session,
            "SELECT tags, scores, cardinality(tags) FROM tagged"
        ),
        "tags|scores|cardinality\n{a,\"b c\",NULL}|{7,-1}|3\nSELECT 1"
    );
    assert_eq!(
        prepared_rows(
            &mut session,
            "SELECT id FROM tagged WHERE id = ANY($1)",
            &[array_parameter(ArrayElement::Integer, "{3,1}")],
        ),
        "id\n1\nSELECT 1"
    );
    assert_eq!(
        prepared_rows(
            &mut session,
            "SELECT $1",
            &[array_parameter(ArrayElement::Integer, "{}")],
        ),
        "?column?\n{}\nSELECT 1"
    );
    let mut outputs = Vec::new();
    let refused = session.execute_prepared(
        "SELECT $1",
        &[array_parameter(ArrayElement::Integer, "{1,x}")],
        &mut |result| {
            outputs.push(render(&result));
            true
        },
    );
    assert_eq!(refused.unwrap_err().state().code(), "22P02");
}
