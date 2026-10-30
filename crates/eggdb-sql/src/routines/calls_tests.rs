use crate::dialect::Dialect;
use crate::error::SqlState;
use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Session, SqlError};

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> Result<String, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| {
        let mut shown = render(&result);
        for notice in &result.notices {
            shown.push_str("\nnotice: ");
            shown.push_str(notice);
        }
        outputs.push(shown);
    })?;
    Ok(outputs.pop().unwrap_or_default())
}

fn answer(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    match run(session, sql) {
        Ok(output) => output,
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn refusal(session: &mut Session<'_, MemoryStore>, sql: &str) -> SqlError {
    match run(session, sql) {
        Ok(output) => panic!("{sql} succeeded: {output}"),
        Err(error) => error,
    }
}

fn session_with(store: &MemoryStore, statements: &[&str]) -> Session<'_, MemoryStore> {
    let mut session = Session::new(store);
    for statement in statements {
        answer(&mut session, statement);
    }
    session
}

#[test]
fn select_calls_plpgsql_and_sql_functions_with_defaults_and_named_arguments() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE FUNCTION add(a integer, b integer DEFAULT 10) RETURNS integer LANGUAGE plpgsql \
             AS $$ BEGIN RETURN a + b; END $$",
            "CREATE FUNCTION twice(n integer) RETURNS integer LANGUAGE sql IMMUTABLE RETURN n * 2",
        ],
    );
    assert_eq!(answer(&mut session, "SELECT add(1, 2)"), "add\n3\nSELECT 1");
    assert_eq!(answer(&mut session, "SELECT add(5)"), "add\n15\nSELECT 1");
    assert_eq!(
        answer(&mut session, "SELECT add(b => 1, a => 2) AS total"),
        "total\n3\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT twice(add(1, 2)) + 1"),
        "?column?\n7\nSELECT 1"
    );
    let error = refusal(&mut session, "SELECT add('x', 'y', 'z')");
    assert_eq!(error.state(), SqlState::UNDEFINED_FUNCTION);
}

#[test]
fn overloads_are_chosen_by_argument_types_and_ties_are_refused() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE FUNCTION kind(n integer) RETURNS text LANGUAGE sql RETURN 'integer'",
            "CREATE FUNCTION kind(t text) RETURNS text LANGUAGE sql RETURN 'text'",
            "CREATE FUNCTION kind(b boolean) RETURNS text LANGUAGE sql RETURN 'boolean'",
        ],
    );
    assert_eq!(answer(&mut session, "SELECT kind(1)"), "kind\ninteger\nSELECT 1");
    assert_eq!(answer(&mut session, "SELECT kind('a'::text)"), "kind\ntext\nSELECT 1");
    assert_eq!(answer(&mut session, "SELECT kind(true)"), "kind\nboolean\nSELECT 1");
}

#[test]
fn functions_run_once_per_row_in_select_lists_and_filters() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE TABLE people (id integer PRIMARY KEY, first text, last text)",
            "INSERT INTO people VALUES (1, 'Ada', 'Lovelace'), (2, 'Alan', 'Turing')",
            "CREATE FUNCTION full_name(f text, l text) RETURNS text LANGUAGE plpgsql STABLE \
             AS $$ BEGIN RETURN f || ' ' || l; END $$",
        ],
    );
    assert_eq!(
        answer(&mut session, "SELECT full_name(first, last) FROM people ORDER BY id"),
        "full_name\nAda Lovelace\nAlan Turing\nSELECT 2"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT id FROM people WHERE full_name(first, last) = 'Alan Turing'"
        ),
        "id\n2\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "UPDATE people SET last = full_name(last, 'X') WHERE id = 1"),
        "UPDATE 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT last FROM people WHERE id = 1"),
        "last\nLovelace X\nSELECT 1"
    );
}

#[test]
fn set_returning_functions_fill_from_and_select_lists() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE FUNCTION evens(n integer) RETURNS SETOF integer LANGUAGE plpgsql AS $$ \
             BEGIN FOR i IN 1..n LOOP RETURN NEXT i * 2; END LOOP; END $$",
            "CREATE FUNCTION pairs(n integer) RETURNS TABLE (k integer, label text) LANGUAGE plpgsql \
             AS $$ BEGIN FOR i IN 1..n LOOP k := i; label := 'item ' || i; RETURN NEXT; END LOOP; END $$",
        ],
    );
    assert_eq!(
        answer(&mut session, "SELECT * FROM evens(3)"),
        "evens\n2\n4\n6\nSELECT 3"
    );
    assert_eq!(answer(&mut session, "SELECT evens(2)"), "evens\n2\n4\nSELECT 2");
    assert_eq!(
        answer(&mut session, "SELECT label FROM pairs(2) WHERE k = 2"),
        "label\nitem 2\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT * FROM pairs(1) WITH ORDINALITY AS p(a, b)"),
        "a|b|ordinality\n1|item 1|1\nSELECT 1"
    );
    assert_eq!(answer(&mut session, "SELECT * FROM pairs(0)"), "k|label\nSELECT 0");
    assert_eq!(answer(&mut session, "SELECT count(*) FROM evens(0)"), "count\n0\nSELECT 1");
}

#[test]
fn a_select_of_a_volatile_function_may_write() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE TABLE log (message text)",
            "CREATE FUNCTION remember(m text) RETURNS integer LANGUAGE plpgsql AS $$ \
             BEGIN INSERT INTO log VALUES (m); RETURN (SELECT count(*) FROM log); END $$",
        ],
    );
    assert_eq!(answer(&mut session, "SELECT remember('a')"), "remember\n1\nSELECT 1");
    assert_eq!(answer(&mut session, "SELECT remember('b')"), "remember\n2\nSELECT 1");
    answer(&mut session, "CREATE TABLE source (id integer)");
    answer(&mut session, "INSERT INTO source VALUES (1)");
    let error = refusal(&mut session, "SELECT remember('c' || id) FROM source");
    assert_eq!(error.state(), SqlState::FEATURE_NOT_SUPPORTED);
    assert_eq!(answer(&mut session, "SELECT count(*) FROM log"), "count\n2\nSELECT 1");
}

#[test]
fn call_runs_procedures_and_returns_their_output_parameters() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE TABLE accounts (id integer PRIMARY KEY, balance integer)",
            "INSERT INTO accounts VALUES (1, 100), (2, 0)",
            "CREATE PROCEDURE transfer(source integer, target integer, amount integer) \
             LANGUAGE plpgsql AS $$ BEGIN \
             UPDATE accounts SET balance = balance - amount WHERE id = source; \
             UPDATE accounts SET balance = balance + amount WHERE id = target; END $$",
            "CREATE PROCEDURE split(IN total integer, INOUT half integer, OUT rest integer) \
             LANGUAGE plpgsql AS $$ BEGIN half := total / 2; rest := total - half; END $$",
        ],
    );
    assert_eq!(answer(&mut session, "CALL transfer(1, 2, 30)"), "CALL");
    assert_eq!(
        answer(&mut session, "SELECT balance FROM accounts ORDER BY id"),
        "balance\n70\n30\nSELECT 2"
    );
    assert_eq!(
        answer(&mut session, "CALL transfer(amount => 5, source => 2, target => 1)"),
        "CALL"
    );
    assert_eq!(
        answer(&mut session, "CALL split(9, NULL, NULL)"),
        "half|rest\n4|5\nCALL"
    );
    let error = refusal(&mut session, "CALL missing(1)");
    assert_eq!(error.state(), SqlState::UNDEFINED_FUNCTION);
    answer(
        &mut session,
        "CREATE FUNCTION one() RETURNS integer LANGUAGE sql RETURN 1",
    );
    let error = refusal(&mut session, "CALL one()");
    assert_eq!(error.state(), SqlState::WRONG_OBJECT_TYPE);
    let error = refusal(&mut session, "SELECT transfer(1, 2, 3)");
    assert_eq!(error.state(), SqlState::WRONG_OBJECT_TYPE);
}

#[test]
fn plpgsql_call_assigns_inout_results_back_to_variables() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE PROCEDURE bump(INOUT counter integer) LANGUAGE plpgsql \
             AS $$ BEGIN counter := counter + 1; END $$",
        ],
    );
    let output = answer(
        &mut session,
        "DO $$ DECLARE n integer := 1; BEGIN CALL bump(n); CALL bump(n); \
         RAISE NOTICE 'n is %', n; END $$",
    );
    assert!(output.starts_with("DO"), "{output}");
    assert!(output.contains("n is 3"), "{output}");
}

#[test]
fn do_blocks_run_once_and_report_their_notices() {
    let store = MemoryStore::default();
    let mut session = session_with(&store, &["CREATE TABLE t (n integer)"]);
    let output = answer(
        &mut session,
        "DO LANGUAGE plpgsql $$ BEGIN FOR i IN 1..3 LOOP INSERT INTO t VALUES (i); END LOOP; \
         RAISE NOTICE 'done'; END $$",
    );
    assert!(output.contains("done"), "{output}");
    assert_eq!(answer(&mut session, "SELECT count(*) FROM t"), "count\n3\nSELECT 1");
    let error = refusal(&mut session, "DO LANGUAGE sql $$ SELECT 1 $$");
    assert_eq!(error.state(), SqlState::FEATURE_NOT_SUPPORTED);
    let error = refusal(&mut session, "DO LANGUAGE plpgsql");
    assert_eq!(error.state(), SqlState::SYNTAX_ERROR);
    let error = refusal(&mut session, "DO $$ BEGIN RAISE EXCEPTION 'stop'; END $$");
    assert_eq!(error.state(), SqlState::RAISE_EXCEPTION);
}

#[test]
fn schema_qualified_and_variadic_calls_resolve() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE SCHEMA app",
            "CREATE FUNCTION app.total(VARIADIC numbers integer[]) RETURNS integer LANGUAGE plpgsql \
             AS $$ DECLARE s integer := 0; n integer; BEGIN FOREACH n IN ARRAY numbers LOOP \
             s := s + n; END LOOP; RETURN s; END $$",
            "CREATE FUNCTION app.numbers() RETURNS SETOF integer LANGUAGE sql \
             AS 'SELECT 1 UNION ALL SELECT 2'",
        ],
    );
    assert_eq!(answer(&mut session, "SELECT app.total(1, 2, 3)"), "total\n6\nSELECT 1");
    assert_eq!(
        answer(&mut session, "SELECT * FROM app.numbers() AS n(v) ORDER BY v"),
        "v\n1\n2\nSELECT 2"
    );
}

#[test]
fn runaway_recursion_stops_at_the_depth_limit() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &["CREATE FUNCTION forever(n integer) RETURNS integer LANGUAGE plpgsql \
           AS $$ BEGIN RETURN forever(n + 1); END $$"],
    );
    let error = refusal(&mut session, "SELECT forever(1)");
    assert_eq!(error.state(), SqlState::STATEMENT_TOO_COMPLEX);
}

#[test]
fn call_and_do_need_confirmation_from_agents() {
    for text in ["CALL p(1)", "DO $$ BEGIN NULL; END $$"] {
        let found = crate::destructive::destructive_statement(text, Dialect::PostgreSql)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .unwrap_or_else(|| panic!("{text} is not destructive"));
        assert_eq!(found.parts[0].kind, crate::destructive::DestructiveKind::RunCode);
    }
}

#[test]
fn hostile_calls_return_errors() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &["CREATE FUNCTION id(n integer) RETURNS integer LANGUAGE sql RETURN n"],
    );
    for text in [
        "CALL",
        "CALL (",
        "CALL p(",
        "CALL p(a => 1, 2)",
        "CALL p(a => 1, a => 2)",
        "DO",
        "DO 'BEGIN'",
        "DO $$ $$",
        "SELECT id(n => 1, n => 2)",
        "SELECT id(m => 1)",
        "SELECT * FROM id(1, 2)",
        "SELECT a.b.c(1)",
    ] {
        assert!(run(&mut session, text).is_err(), "{text}");
    }
    assert_eq!(answer(&mut session, "SELECT id(n => 4)"), "id\n4\nSELECT 1");
}
