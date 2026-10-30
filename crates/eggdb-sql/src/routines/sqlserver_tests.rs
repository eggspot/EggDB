use super::{ParameterMode, ReturnType, RoutineKind, RoutineLanguage, RoutineReturns, parse_definition};
use crate::dialect::Dialect;
use crate::memory_store::MemoryStore;
use crate::sqlserver_types::{Length, SqlServerType};
use crate::tsql::TsqlReturns;
use crate::types::SqlType;
use crate::{BatchStep, Session};

fn session(store: &MemoryStore) -> Session<'_, MemoryStore> {
    Session::with_dialect(store, Dialect::SqlServer)
}

fn batch(session: &mut Session<'_, MemoryStore>, text: &str) -> Vec<BatchStep> {
    let mut steps = Vec::new();
    session
        .execute_batch(text, &mut |step| {
            steps.push(step);
            true
        })
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    steps
}

fn errors(steps: &[BatchStep]) -> Vec<Option<u32>> {
    steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().err())
        .map(crate::SqlError::sqlserver_number)
        .collect()
}

fn succeeds(session: &mut Session<'_, MemoryStore>, text: &str) {
    let steps = batch(session, text);
    assert_eq!(errors(&steps), Vec::<Option<u32>>::new(), "{text}");
}

fn fails_with(session: &mut Session<'_, MemoryStore>, text: &str, number: u32) {
    let steps = batch(session, text);
    assert_eq!(errors(&steps), vec![Some(number)], "{text}");
}

#[test]
fn procedure_headers_parse_with_defaults_and_output_parameters() {
    let text = "CREATE PROCEDURE dbo.usp_GetTotal @Customer int, @Since date = NULL, \
                @Total money OUTPUT WITH RECOMPILE AS SET NOCOUNT ON; \
                SELECT @Total = 1; RETURN 0";
    let definition = parse_definition(text, Dialect::SqlServer).unwrap();
    assert_eq!(definition.kind, RoutineKind::Procedure);
    assert_eq!(definition.language, RoutineLanguage::Tsql);
    assert_eq!(definition.returns, RoutineReturns::Nothing);
    assert_eq!(definition.parameters.len(), 3);
    assert_eq!(definition.parameters[0].name.as_deref(), Some("@customer"));
    assert_eq!(definition.parameters[0].declared, Some(SqlServerType::Int));
    assert_eq!(definition.parameters[1].default.as_deref(), Some("NULL"));
    assert_eq!(definition.parameters[2].mode, ParameterMode::InOut);
    assert_eq!(
        definition.body,
        "SET NOCOUNT ON; \
                SELECT @Total = 1; RETURN 0"
    );
    assert_eq!(
        definition.tsql.map(|tsql| tsql.returns),
        Some(TsqlReturns::Status)
    );
}

#[test]
fn scalar_inline_and_multi_statement_functions_parse() {
    let scalar = parse_definition(
        "CREATE FUNCTION dbo.Twice (@n int) RETURNS int WITH SCHEMABINDING, RETURNS NULL ON NULL INPUT \
         AS BEGIN RETURN @n * 2 END",
        Dialect::SqlServer,
    )
    .unwrap();
    assert!(scalar.strict);
    assert_eq!(
        scalar.returns,
        RoutineReturns::One(ReturnType::Type(SqlType::Integer))
    );
    assert_eq!(scalar.body, "BEGIN RETURN @n * 2 END");

    let inline = parse_definition(
        "CREATE FUNCTION dbo.Recent (@days int) RETURNS TABLE AS RETURN (SELECT 1 AS a WHERE @days > 0)",
        Dialect::SqlServer,
    )
    .unwrap();
    assert_eq!(inline.returns, RoutineReturns::Set(ReturnType::Record));
    assert_eq!(inline.body, "SELECT 1 AS a WHERE @days > 0");

    let multi = parse_definition(
        "CREATE FUNCTION dbo.Numbers (@top int) RETURNS @result TABLE (n int NOT NULL, label nvarchar(20)) \
         AS BEGIN INSERT @result (n, label) VALUES (@top, N'top'); RETURN; END",
        Dialect::SqlServer,
    )
    .unwrap();
    assert_eq!(
        multi.returns,
        RoutineReturns::Table(vec![
            ("n".to_owned(), SqlType::Integer),
            ("label".to_owned(), SqlServerType::NVarChar(Length::Given(20)).engine_type()),
        ])
    );
}

#[test]
fn routine_headers_follow_sqlserver_rules() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    fails_with(&mut session, "SELECT 1; CREATE PROCEDURE p AS SELECT 1", 111);
    fails_with(&mut session, "CREATE FUNCTION f (@a int OUTPUT) RETURNS int AS BEGIN RETURN 1 END", 181);
    fails_with(&mut session, "CREATE FUNCTION f () RETURNS int AS BEGIN DECLARE @x int = 1 END", 455);
    fails_with(&mut session, "CREATE PROCEDURE p @a int, @a int AS SELECT 1", 134);
    fails_with(&mut session, "SELECT 1; RETURN 5", 178);
    fails_with(&mut session, "CREATE PROCEDURE p @a int READONLY AS SELECT 1", 346);
}

#[test]
fn procedures_and_functions_share_one_name_space_with_tables() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    succeeds(&mut session, "CREATE TABLE orders (id int)");
    succeeds(&mut session, "CREATE PROCEDURE dbo.get_orders AS SELECT id FROM orders");
    fails_with(&mut session, "CREATE PROCEDURE get_orders AS SELECT 1", 2714);
    fails_with(&mut session, "CREATE FUNCTION get_orders () RETURNS int AS BEGIN RETURN 1 END", 2714);
    fails_with(&mut session, "CREATE PROCEDURE orders AS SELECT 1", 2714);
    fails_with(&mut session, "CREATE OR ALTER PROCEDURE orders AS SELECT 1", 2010);
    fails_with(&mut session, "ALTER PROCEDURE missing AS SELECT 1", 208);
    succeeds(&mut session, "ALTER PROCEDURE get_orders AS SELECT 2");
    succeeds(&mut session, "CREATE OR ALTER PROCEDURE get_orders AS SELECT 3");
    fails_with(
        &mut session,
        "CREATE OR ALTER FUNCTION get_orders () RETURNS int AS BEGIN RETURN 1 END",
        2010,
    );
    fails_with(&mut session, "DROP FUNCTION get_orders", 3705);
    succeeds(&mut session, "DROP PROCEDURE get_orders");
    fails_with(&mut session, "DROP PROC get_orders", 3701);
    succeeds(&mut session, "DROP PROCEDURE IF EXISTS get_orders, also_missing");
}

#[test]
fn altered_procedures_keep_a_create_statement_as_their_text() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    succeeds(&mut session, "CREATE PROCEDURE p AS SELECT 1");
    succeeds(&mut session, "ALTER PROCEDURE p AS SELECT 2");
    let mut reader = Session::with_dialect(&store, Dialect::SqlServer);
    let routines = reader
        .read(|reader, _| super::every_routine(reader))
        .unwrap();
    assert_eq!(routines.len(), 1);
    assert_eq!(routines[0].routines[0].text, "CREATE PROCEDURE p AS SELECT 2");
}
