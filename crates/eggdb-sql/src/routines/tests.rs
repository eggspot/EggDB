use super::{
    ParameterMode, ReturnType, RoutineKind, RoutineLanguage, RoutineReturns, Volatility,
    identity_type_name, parse_definition,
};
use crate::dialect::Dialect;
use crate::error::SqlState;
use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::types::SqlType;
use crate::{Session, SqlError};

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> Result<String, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| {
        let mut shown = render(&result);
        for notice in &result.notices {
            shown.push_str("\nNOTICE: ");
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

#[test]
fn create_function_headers_parse_into_definitions() {
    let text = "CREATE OR REPLACE FUNCTION app.add(a integer, IN b integer DEFAULT 2, OUT total bigint) \
                LANGUAGE sql IMMUTABLE STRICT SECURITY DEFINER SET search_path = app, public \
                AS 'SELECT a + b'";
    let definition = parse_definition(text, Dialect::PostgreSql).unwrap();
    assert_eq!(definition.kind, RoutineKind::Function);
    assert_eq!(definition.language, RoutineLanguage::Sql);
    assert_eq!(definition.volatility, Volatility::Immutable);
    assert!(definition.strict && definition.security_definer);
    assert_eq!(definition.parameters.len(), 3);
    assert_eq!(definition.parameters[0].name.as_deref(), Some("a"));
    assert_eq!(definition.parameters[1].default.as_deref(), Some("2"));
    assert_eq!(definition.parameters[2].mode, ParameterMode::Out);
    assert_eq!(
        definition.returns,
        RoutineReturns::One(ReturnType::Type(SqlType::BigInt))
    );
    assert_eq!(definition.identity(), "integer, integer");
    assert_eq!(definition.required_inputs(), 1);
    assert_eq!(definition.body, "SELECT a + b");
    assert_eq!(
        definition.settings,
        vec![("search_path".to_owned(), "app, public".to_owned())]
    );
    assert_eq!(definition.text, text);
}

#[test]
fn unnamed_and_multiword_parameter_types_parse() {
    let definition = parse_definition(
        "CREATE FUNCTION f(double precision, character varying, timestamp with time zone, text text) \
         RETURNS SETOF integer LANGUAGE sql AS $$ SELECT 1 $$",
        Dialect::PostgreSql,
    )
    .unwrap();
    let names: Vec<Option<&str>> = definition
        .parameters
        .iter()
        .map(|parameter| parameter.name.as_deref())
        .collect();
    assert_eq!(names, vec![None, None, None, Some("text")]);
    assert_eq!(
        definition.identity(),
        "double precision, character varying, timestamp with time zone, text"
    );
    assert_eq!(
        definition.returns,
        RoutineReturns::Set(ReturnType::Type(SqlType::Integer))
    );
    assert!(definition.returns.is_set());
}

#[test]
fn returns_table_trigger_void_and_standard_bodies_parse() {
    let table = parse_definition(
        "CREATE FUNCTION pairs() RETURNS TABLE (k integer, v text) LANGUAGE sql AS 'SELECT 1, ''a'''",
        Dialect::PostgreSql,
    )
    .unwrap();
    assert_eq!(
        table.returns,
        RoutineReturns::Table(vec![
            ("k".to_owned(), SqlType::Integer),
            ("v".to_owned(), SqlType::Text)
        ])
    );
    let trigger = parse_definition(
        "CREATE FUNCTION stamp() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$",
        Dialect::PostgreSql,
    )
    .unwrap();
    assert_eq!(trigger.returns, RoutineReturns::One(ReturnType::Trigger));
    assert_eq!(trigger.language, RoutineLanguage::PlPgSql);
    let standard = parse_definition(
        "CREATE FUNCTION twice(n integer) RETURNS integer RETURN n * 2",
        Dialect::PostgreSql,
    )
    .unwrap();
    assert_eq!(standard.language, RoutineLanguage::Sql);
    assert_eq!(standard.body, "SELECT n * 2");
    let atomic = parse_definition(
        "CREATE PROCEDURE log_it(m text) BEGIN ATOMIC INSERT INTO log VALUES (CASE WHEN m = '' THEN NULL ELSE m END); END",
        Dialect::PostgreSql,
    )
    .unwrap();
    assert_eq!(atomic.kind, RoutineKind::Procedure);
    assert_eq!(atomic.returns, RoutineReturns::Nothing);
    assert_eq!(
        atomic.body,
        "INSERT INTO log VALUES (CASE WHEN m = '' THEN NULL ELSE m END);"
    );
}

#[test]
fn identity_type_names_leave_out_modifiers() {
    assert_eq!(identity_type_name(SqlType::Varchar(Some(20))), "character varying");
    assert_eq!(identity_type_name(SqlType::Integer), "integer");
}

#[test]
fn headers_without_a_body_language_or_result_are_refused() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let error = refusal(&mut session, "CREATE FUNCTION f() RETURNS integer LANGUAGE sql");
    assert_eq!(error.state(), SqlState::INVALID_FUNCTION_DEFINITION);
    let error = refusal(&mut session, "CREATE FUNCTION f() RETURNS integer AS 'SELECT 1'");
    assert_eq!(error.state(), SqlState::INVALID_FUNCTION_DEFINITION);
    let error = refusal(&mut session, "CREATE FUNCTION f() LANGUAGE sql AS 'SELECT 1'");
    assert_eq!(error.state(), SqlState::INVALID_FUNCTION_DEFINITION);
    let error = refusal(
        &mut session,
        "CREATE FUNCTION f() RETURNS integer LANGUAGE c AS 'lib', 'f'",
    );
    assert_eq!(error.state(), SqlState::FEATURE_NOT_SUPPORTED);
    let error = refusal(
        &mut session,
        "CREATE FUNCTION f(n integer) RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END'",
    );
    assert_eq!(error.state(), SqlState::INVALID_FUNCTION_DEFINITION);
    let error = refusal(
        &mut session,
        "CREATE FUNCTION f() RETURNS trigger LANGUAGE sql AS 'SELECT 1'",
    );
    assert_eq!(error.state(), SqlState::INVALID_FUNCTION_DEFINITION);
}

#[test]
fn overloads_are_kept_replaced_and_dropped_by_argument_types() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        answer(
            &mut session,
            "CREATE FUNCTION area(r integer) RETURNS integer LANGUAGE sql AS 'SELECT r * r'"
        ),
        "CREATE FUNCTION"
    );
    answer(
        &mut session,
        "CREATE FUNCTION area(w integer, h integer) RETURNS integer LANGUAGE sql AS 'SELECT w * h'",
    );
    let error = refusal(
        &mut session,
        "CREATE FUNCTION area(side integer) RETURNS integer LANGUAGE sql AS 'SELECT side'",
    );
    assert_eq!(error.state(), SqlState::DUPLICATE_FUNCTION);
    answer(
        &mut session,
        "CREATE OR REPLACE FUNCTION area(side integer) RETURNS integer LANGUAGE sql AS 'SELECT side'",
    );
    let error = refusal(
        &mut session,
        "CREATE OR REPLACE FUNCTION area(side integer) RETURNS text LANGUAGE sql AS 'SELECT ''x'''",
    );
    assert_eq!(error.state(), SqlState::INVALID_FUNCTION_DEFINITION);
    let error = refusal(&mut session, "DROP FUNCTION area");
    assert_eq!(error.state(), SqlState::AMBIGUOUS_FUNCTION);
    assert_eq!(answer(&mut session, "DROP FUNCTION area(integer)"), "DROP FUNCTION");
    assert_eq!(answer(&mut session, "DROP FUNCTION area"), "DROP FUNCTION");
    let error = refusal(&mut session, "DROP FUNCTION area(integer, integer)");
    assert_eq!(error.state(), SqlState::UNDEFINED_FUNCTION);
    assert_eq!(
        answer(&mut session, "DROP FUNCTION IF EXISTS area(integer)"),
        "DROP FUNCTION\nNOTICE: function area(integer) does not exist, skipping"
    );
}

#[test]
fn procedures_and_functions_are_told_apart() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        answer(
            &mut session,
            "CREATE PROCEDURE tidy(n integer) LANGUAGE sql AS 'SELECT n'"
        ),
        "CREATE PROCEDURE"
    );
    let error = refusal(&mut session, "DROP FUNCTION tidy(integer)");
    assert_eq!(error.state(), SqlState::WRONG_OBJECT_TYPE);
    let error = refusal(&mut session, "DROP FUNCTION tidy");
    assert_eq!(error.state(), SqlState::UNDEFINED_FUNCTION);
    assert_eq!(answer(&mut session, "DROP ROUTINE tidy"), "DROP ROUTINE");
}

#[test]
fn routines_live_in_schemas_and_survive_a_new_session() {
    let store = MemoryStore::default();
    {
        let mut session = Session::new(&store);
        answer(&mut session, "CREATE SCHEMA app");
        answer(
            &mut session,
            "CREATE FUNCTION app.one() RETURNS integer LANGUAGE sql AS 'SELECT 1'",
        );
        answer(&mut session, "ALTER FUNCTION app.one() OWNER TO someone");
        answer(&mut session, "ALTER FUNCTION app.one() RENAME TO uno");
        let error = refusal(&mut session, "DROP FUNCTION app.one()");
        assert_eq!(error.state(), SqlState::UNDEFINED_FUNCTION);
        answer(&mut session, "ALTER FUNCTION app.uno() SET SCHEMA public");
    }
    let mut session = Session::new(&store);
    let error = refusal(&mut session, "DROP FUNCTION app.uno()");
    assert_eq!(error.state(), SqlState::UNDEFINED_FUNCTION);
    assert_eq!(answer(&mut session, "DROP FUNCTION uno()"), "DROP FUNCTION");
}

#[test]
fn hostile_routine_statements_return_errors() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    crate::tests::fuzz(
        &mut session,
        &[
            "CREATE",
            "OR REPLACE",
            "FUNCTION",
            "PROCEDURE",
            "DROP",
            "ALTER",
            "ROUTINE",
            "f",
            "(",
            ")",
            ",",
            "a",
            "integer",
            "OUT",
            "INOUT",
            "VARIADIC",
            "DEFAULT",
            "RETURNS",
            "SETOF",
            "TABLE",
            "trigger",
            "LANGUAGE",
            "sql",
            "plpgsql",
            "AS",
            "'SELECT 1'",
            "$$BEGIN END$$",
            "RETURN",
            "BEGIN ATOMIC",
            "END",
            "CASE",
            "STRICT",
            "SET",
            "search_path",
            "=",
            "RENAME TO",
            "SET SCHEMA",
            "CASCADE",
            ";",
        ],
    );
}
