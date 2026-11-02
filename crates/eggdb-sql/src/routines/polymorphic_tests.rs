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
fn anyelement_takes_the_type_of_its_arguments_on_each_call() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE FUNCTION first_of(a anyelement, b anyelement) RETURNS anyelement \
             LANGUAGE plpgsql AS $$ BEGIN RETURN a; END $$",
            "CREATE FUNCTION same(v anyelement) RETURNS anyelement LANGUAGE sql AS 'SELECT v'",
        ],
    );
    assert_eq!(
        answer(&mut session, "SELECT first_of(7, 8) / 2 AS half"),
        "half\n3\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT first_of('pen'::text, 'ink') AS word"),
        "word\npen\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT first_of(1, '2') + 1 AS n"),
        "n\n2\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT same(42)"),
        "same\n42\nSELECT 1"
    );
    let error = refusal(&mut session, "SELECT first_of(1, 'x'::text)");
    assert_eq!(error.state(), SqlState::UNDEFINED_FUNCTION);
    assert_eq!(
        answer(
            &mut session,
            "SELECT prorettype FROM pg_proc WHERE proname = 'first_of'"
        ),
        "prorettype\n2283\nSELECT 1"
    );
    answer(
        &mut session,
        "DROP FUNCTION first_of(anyelement, anyelement)",
    );
    let error = refusal(&mut session, "SELECT first_of(1, 2)");
    assert_eq!(error.state(), SqlState::UNDEFINED_FUNCTION);
}

#[test]
fn anyarray_and_anyelement_resolve_to_an_array_and_its_element() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE FUNCTION first_item(items anyarray) RETURNS anyelement LANGUAGE plpgsql \
             AS $$ BEGIN RETURN items[1]; END $$",
            "CREATE FUNCTION twin(item anyelement) RETURNS anyarray LANGUAGE plpgsql \
             AS $$ BEGIN RETURN ARRAY[item, item]; END $$",
            "CREATE FUNCTION holds(items anyarray, item anyelement) RETURNS boolean \
             LANGUAGE plpgsql AS $$ BEGIN RETURN item = ANY (items); END $$",
        ],
    );
    assert_eq!(
        answer(&mut session, "SELECT first_item(ARRAY[4, 5]) * 2 AS n"),
        "n\n8\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT first_item(ARRAY['x', 'y']) AS s"),
        "s\nx\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT twin(7) AS pair"),
        "pair\n{7,7}\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT holds(ARRAY[1, 2], 2) AS found"),
        "found\nt\nSELECT 1"
    );
    let error = refusal(&mut session, "SELECT first_item(5)");
    assert_eq!(error.state(), SqlState::UNDEFINED_FUNCTION);
    let error = refusal(&mut session, "SELECT holds(ARRAY[1, 2], true)");
    assert_eq!(error.state(), SqlState::UNDEFINED_FUNCTION);
}

#[test]
fn anycompatible_arguments_meet_at_their_common_type() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE FUNCTION bigger(a anycompatible, b anycompatible) RETURNS anycompatible \
             LANGUAGE plpgsql AS $$ BEGIN IF a > b THEN RETURN a; END IF; RETURN b; END $$",
            "CREATE FUNCTION pair(a anycompatible, b anycompatible) RETURNS anycompatiblearray \
             LANGUAGE plpgsql AS $$ BEGIN RETURN ARRAY[a, b]; END $$",
            "CREATE FUNCTION how_many(VARIADIC items anycompatiblearray) RETURNS integer \
             LANGUAGE plpgsql AS $$ BEGIN RETURN cardinality(items); END $$",
        ],
    );
    assert_eq!(
        answer(&mut session, "SELECT bigger(1, 2.5) + 0.25 AS n"),
        "n\n2.75\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT pair(1, 2.5) AS both"),
        "both\n{1,2.5}\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT how_many(1, 2.5, 3) AS n"),
        "n\n3\nSELECT 1"
    );
    let error = refusal(&mut session, "SELECT bigger(1, true)");
    assert_eq!(error.state(), SqlState::UNDEFINED_FUNCTION);
}

#[test]
fn anynonarray_and_anyenum_refuse_arguments_of_the_wrong_kind() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE TYPE mood AS ENUM ('sad', 'happy')",
            "CREATE FUNCTION scalar_only(v anynonarray) RETURNS text LANGUAGE sql RETURN 'ok'",
            "CREATE FUNCTION mood_name(m anyenum) RETURNS text LANGUAGE plpgsql \
             AS $$ BEGIN RETURN m::text; END $$",
        ],
    );
    assert_eq!(
        answer(&mut session, "SELECT scalar_only(1)"),
        "scalar_only\nok\nSELECT 1"
    );
    let error = refusal(&mut session, "SELECT scalar_only(ARRAY[1])");
    assert_eq!(error.state(), SqlState::UNDEFINED_FUNCTION);
    assert_eq!(
        answer(&mut session, "SELECT mood_name('happy'::mood)"),
        "mood_name\nhappy\nSELECT 1"
    );
    let error = refusal(&mut session, "SELECT mood_name(1)");
    assert_eq!(error.state(), SqlState::UNDEFINED_FUNCTION);
}

#[test]
fn a_concrete_overload_wins_over_a_polymorphic_one() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE FUNCTION kind(n integer) RETURNS text LANGUAGE sql RETURN 'integer'",
            "CREATE FUNCTION kind(v anyelement) RETURNS text LANGUAGE sql RETURN 'any'",
        ],
    );
    assert_eq!(
        answer(&mut session, "SELECT kind(1)"),
        "kind\ninteger\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT kind(DATE '2026-11-02')"),
        "kind\nany\nSELECT 1"
    );
}

#[test]
fn set_returning_polymorphic_functions_run_in_from() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE FUNCTION both_of(a anyelement, b anyelement) RETURNS SETOF anyelement \
           LANGUAGE plpgsql AS $$ BEGIN RETURN NEXT a; RETURN NEXT b; END $$",
        ],
    );
    assert_eq!(
        answer(&mut session, "SELECT * FROM both_of(3, 4)"),
        "both_of\n3\n4\nSELECT 2"
    );
    assert_eq!(
        answer(&mut session, "SELECT * FROM both_of('a'::text, 'b')"),
        "both_of\na\nb\nSELECT 2"
    );
}

#[test]
fn results_and_arguments_that_cannot_be_resolved_are_errors() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE FUNCTION pick(a anyelement, b anyelement DEFAULT 'x'::text) RETURNS anyelement \
           LANGUAGE plpgsql AS $$ BEGIN RETURN b; END $$",
        ],
    );
    let error = refusal(
        &mut session,
        "CREATE FUNCTION bad(n integer) RETURNS anyelement LANGUAGE sql AS 'SELECT 1'",
    );
    assert_eq!(error.state(), SqlState::INVALID_FUNCTION_DEFINITION);
    let error = refusal(
        &mut session,
        "CREATE FUNCTION bad(v anyelement) RETURNS anycompatible LANGUAGE sql AS 'SELECT v'",
    );
    assert_eq!(error.state(), SqlState::INVALID_FUNCTION_DEFINITION);
    let error = refusal(&mut session, "SELECT pick(1)");
    assert_eq!(error.state(), SqlState::DATATYPE_MISMATCH);
    assert_eq!(
        answer(&mut session, "SELECT pick('a'::text)"),
        "pick\nx\nSELECT 1"
    );
}

#[test]
fn only_untyped_literals_take_their_type_from_the_other_polymorphic_arguments() {
    let store = MemoryStore::default();
    let mut session = session_with(
        &store,
        &[
            "CREATE FUNCTION same(a anyelement, b anyelement) RETURNS anyelement \
             LANGUAGE sql AS 'SELECT a'",
            "CREATE TABLE words (w text)",
            "INSERT INTO words VALUES ('5')",
        ],
    );
    assert_eq!(
        answer(&mut session, "SELECT same('5', 5) + 1 AS n"),
        "n\n6\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT same(5, '6') AS n"),
        "n\n5\nSELECT 1"
    );
    for sql in [
        "SELECT same('5'::text, 5)",
        "SELECT same(5, '5'::text)",
        "SELECT same(w, 5) FROM words",
        "SELECT * FROM same('5'::text, 5)",
    ] {
        assert_eq!(
            refusal(&mut session, sql).state(),
            SqlState::UNDEFINED_FUNCTION,
            "{sql}"
        );
    }
}
