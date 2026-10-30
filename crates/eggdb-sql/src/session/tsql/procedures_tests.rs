use crate::dialect::Dialect;
use crate::memory_store::MemoryStore;
use crate::types::{SqlType, Value};
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

fn rows(steps: &[BatchStep]) -> Vec<Vec<Value>> {
    steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().ok())
        .filter(|result| result.columns.is_some())
        .flat_map(|result| result.rows.clone())
        .collect()
}

fn printed(steps: &[BatchStep]) -> Vec<String> {
    steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().ok())
        .flat_map(|result| result.info_messages.iter().map(|message| message.text.clone()))
        .collect()
}

fn succeeds(session: &mut Session<'_, MemoryStore>, text: &str) -> Vec<BatchStep> {
    let steps = batch(session, text);
    assert_eq!(errors(&steps), Vec::<Option<u32>>::new(), "{text}");
    steps
}

fn fails_with(session: &mut Session<'_, MemoryStore>, text: &str, number: u32) {
    let steps = batch(session, text);
    assert_eq!(errors(&steps), vec![Some(number)], "{text}");
}

fn create_add_numbers(session: &mut Session<'_, MemoryStore>) {
    succeeds(
        session,
        "CREATE PROCEDURE dbo.add_numbers @a int, @b int = 10, @sum int OUTPUT AS \
         SET NOCOUNT ON; \
         SET @sum = @a + @b; \
         SELECT @a AS a, @b AS b; \
         RETURN 7",
    );
}

#[test]
fn exec_binds_positional_arguments_defaults_output_parameters_and_the_status() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    create_add_numbers(&mut session);
    let steps = succeeds(
        &mut session,
        "DECLARE @s int, @rc int; \
         EXEC @rc = dbo.add_numbers 1, @sum = @s OUTPUT; \
         SELECT @s AS s, @rc AS rc",
    );
    assert_eq!(
        rows(&steps),
        vec![
            vec![Value::Integer(1), Value::Integer(10)],
            vec![Value::Integer(11), Value::Integer(7)],
        ]
    );
}

#[test]
fn exec_takes_named_arguments_in_any_order_and_the_default_keyword() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    create_add_numbers(&mut session);
    let steps = succeeds(
        &mut session,
        "DECLARE @s int; \
         EXECUTE add_numbers @b = DEFAULT, @sum = @s OUT, @a = 5; \
         SELECT @s",
    );
    assert_eq!(
        rows(&steps),
        vec![
            vec![Value::Integer(5), Value::Integer(10)],
            vec![Value::Integer(15)],
        ]
    );
}

#[test]
fn arguments_that_do_not_fit_the_parameters_fail_with_sqlserver_numbers() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    create_add_numbers(&mut session);
    succeeds(
        &mut session,
        "CREATE FUNCTION dbo.one() RETURNS int AS BEGIN RETURN 1 END",
    );
    fails_with(&mut session, "DECLARE @s int; EXEC add_numbers @sum = @s OUTPUT", 201);
    fails_with(&mut session, "DECLARE @s int; EXEC add_numbers 1, 2, @s OUTPUT, 4", 8144);
    fails_with(&mut session, "DECLARE @s int; EXEC add_numbers @a = 1, @c = 2, @sum = @s", 8145);
    fails_with(&mut session, "DECLARE @s int; EXEC add_numbers @a = 1, @a = 2, @sum = @s", 8143);
    fails_with(&mut session, "DECLARE @s int; EXEC add_numbers @s OUTPUT, 1, @s", 8162);
    fails_with(&mut session, "EXEC no_such_procedure", 2812);
    fails_with(&mut session, "EXEC dbo.one", 2809);
}

#[test]
fn procedures_write_print_and_return_without_a_value() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    succeeds(&mut session, "CREATE TABLE audit (id int, note nvarchar(20))");
    succeeds(
        &mut session,
        "CREATE PROC record_note @id int, @note nvarchar(20) = N'none' AS \
         IF @id < 0 RETURN; \
         INSERT INTO audit (id, note) VALUES (@id, @note); \
         PRINT 'recorded ' + @note",
    );
    let steps = succeeds(
        &mut session,
        "EXEC record_note 1, N'first'; EXEC record_note -1; EXEC record_note 2",
    );
    assert_eq!(printed(&steps), ["recorded first", "recorded none"]);
    let steps = succeeds(&mut session, "SELECT id, note FROM audit ORDER BY id");
    assert_eq!(
        rows(&steps),
        vec![
            vec![Value::Integer(1), Value::Text("first".into())],
            vec![Value::Integer(2), Value::Text("none".into())],
        ]
    );
}

#[test]
fn nested_procedures_see_their_nesting_level_and_recursion_stops_at_32() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    succeeds(&mut session, "CREATE PROCEDURE inner_level AS SELECT @@NESTLEVEL AS level");
    succeeds(&mut session, "CREATE PROCEDURE outer_level AS EXEC inner_level");
    let steps = succeeds(&mut session, "SELECT @@NESTLEVEL; EXEC outer_level");
    assert_eq!(
        rows(&steps),
        vec![vec![Value::Integer(0)], vec![Value::Integer(2)]]
    );
    succeeds(&mut session, "CREATE PROCEDURE forever AS EXEC forever");
    let steps = batch(&mut session, "EXEC forever");
    assert!(errors(&steps).contains(&Some(217)), "{:?}", errors(&steps));
}

#[test]
fn errors_raised_in_a_procedure_reach_the_callers_catch_block() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    succeeds(
        &mut session,
        "CREATE PROCEDURE fails_always AS RAISERROR('procedure failed', 16, 1)",
    );
    let steps = succeeds(
        &mut session,
        "BEGIN TRY EXEC fails_always; SELECT 0 END TRY \
         BEGIN CATCH SELECT ERROR_NUMBER(), ERROR_MESSAGE() END CATCH",
    );
    assert_eq!(
        rows(&steps),
        vec![vec![
            Value::Integer(50_000),
            Value::Text("procedure failed".into())
        ]]
    );
}

#[test]
fn insert_exec_fills_a_table_variable_from_a_procedures_result_set() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    create_add_numbers(&mut session);
    let steps = succeeds(
        &mut session,
        "DECLARE @t TABLE (a int, b int); \
         INSERT INTO @t EXEC add_numbers 2, 3, NULL; \
         SELECT a, b FROM @t",
    );
    assert_eq!(
        rows(&steps),
        vec![vec![Value::Integer(2), Value::Integer(3)]]
    );
}

#[test]
fn a_procedure_that_leaves_a_transaction_open_reports_error_266() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    succeeds(&mut session, "CREATE PROCEDURE opens AS BEGIN TRANSACTION");
    fails_with(&mut session, "EXEC opens", 266);
    succeeds(&mut session, "IF @@TRANCOUNT > 0 ROLLBACK");
}

#[test]
fn stored_procedures_are_found_by_the_names_clients_write() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    create_add_numbers(&mut session);
    for written in ["add_numbers", "dbo.add_numbers", "[dbo].[ADD_NUMBERS]", "db..add_numbers"] {
        let found = session
            .stored_procedure(written)
            .unwrap_or_else(|error| panic!("{written}: {error}"))
            .unwrap_or_else(|| panic!("{written} was not found"));
        assert_eq!(found.quoted_name, "[dbo].[add_numbers]");
        let names: Vec<(&str, bool)> = found
            .parameters
            .iter()
            .map(|parameter| (parameter.name.as_str(), parameter.output))
            .collect();
        assert_eq!(names, [("@a", false), ("@b", false), ("@sum", true)]);
        assert_eq!(found.parameters[0].sql_type, SqlType::Integer);
    }
    assert_eq!(session.stored_procedure("other").ok().flatten(), None);
    assert_eq!(session.stored_procedure("p; DROP TABLE t").ok().flatten(), None);
}
