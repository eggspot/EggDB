use crate::dialect::Dialect;
use crate::memory_store::MemoryStore;
use crate::types::Value;
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

fn column_names(steps: &[BatchStep]) -> Vec<String> {
    steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().ok())
        .filter_map(|result| result.columns.as_ref())
        .flat_map(|columns| columns.iter().map(|column| column.name.clone()))
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

fn integers(values: &[&[i32]]) -> Vec<Vec<Value>> {
    values
        .iter()
        .map(|row| row.iter().map(|value| Value::Integer(*value)).collect())
        .collect()
}

fn with_numbers(session: &mut Session<'_, MemoryStore>) {
    succeeds(session, "CREATE TABLE numbers (id int PRIMARY KEY, n int)");
    succeeds(session, "INSERT INTO numbers VALUES (1, 10), (2, 20), (3, 30)");
}

fn create_twice(session: &mut Session<'_, MemoryStore>) {
    succeeds(
        session,
        "CREATE FUNCTION dbo.twice(@n int) RETURNS int AS BEGIN RETURN @n * 2 END",
    );
}

#[test]
fn a_scalar_function_returns_its_value_in_a_select_list() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    create_twice(&mut session);
    let steps = succeeds(&mut session, "SELECT dbo.twice(21)");
    assert_eq!(rows(&steps), integers(&[&[42]]));
}

#[test]
fn a_scalar_function_runs_once_per_row_and_in_where() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    with_numbers(&mut session);
    create_twice(&mut session);
    let steps = succeeds(&mut session, "SELECT id, dbo.twice(n) FROM numbers ORDER BY id");
    assert_eq!(rows(&steps), integers(&[&[1, 20], &[2, 40], &[3, 60]]));
    let steps = succeeds(&mut session, "SELECT id FROM numbers WHERE dbo.twice(n) > 30 ORDER BY id");
    assert_eq!(rows(&steps), integers(&[&[2], &[3]]));
}

#[test]
fn a_scalar_function_result_can_be_assigned_to_a_variable() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    create_twice(&mut session);
    let steps = succeeds(&mut session, "DECLARE @x int; SET @x = dbo.twice(4); SELECT @x");
    assert_eq!(rows(&steps), integers(&[&[8]]));
}

#[test]
fn default_takes_the_parameter_default_and_missing_arguments_fail() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    succeeds(
        &mut session,
        "CREATE FUNCTION dbo.scaled(@n int, @by int = 3) RETURNS int AS BEGIN RETURN @n * @by END",
    );
    let steps = succeeds(&mut session, "SELECT dbo.scaled(2, DEFAULT), dbo.scaled(2, 5)");
    assert_eq!(rows(&steps), integers(&[&[6, 10]]));
    fails_with(&mut session, "SELECT dbo.scaled(2)", 313);
    fails_with(&mut session, "SELECT dbo.scaled(1, 2, 3)", 8144);
}

#[test]
fn a_function_body_runs_declare_while_and_if() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    succeeds(
        &mut session,
        "CREATE FUNCTION dbo.factorial(@n int) RETURNS bigint AS BEGIN \
         DECLARE @result bigint = 1, @i int = 2; \
         IF @n < 0 RETURN NULL; \
         WHILE @i <= @n BEGIN SET @result = @result * @i; SET @i = @i + 1; END \
         RETURN @result; \
         END",
    );
    let steps = succeeds(&mut session, "SELECT dbo.factorial(5), dbo.factorial(-1)");
    assert_eq!(rows(&steps), vec![vec![Value::BigInt(120), Value::Null]]);
}

#[test]
fn a_function_reads_tables_with_select_assignment() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    with_numbers(&mut session);
    succeeds(
        &mut session,
        "CREATE FUNCTION dbo.total_above(@floor int) RETURNS int AS BEGIN \
         DECLARE @total int; \
         SELECT @total = SUM(n) FROM numbers WHERE n > @floor; \
         RETURN ISNULL(@total, 0); \
         END",
    );
    let steps = succeeds(&mut session, "SELECT dbo.total_above(15), dbo.total_above(100)");
    assert_eq!(rows(&steps), integers(&[&[50, 0]]));
}

#[test]
fn a_function_calls_itself_and_too_deep_recursion_fails_with_217() {
    let deep = std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(|| {
            let store = MemoryStore::new();
            let mut session = session(&store);
            succeeds(
                &mut session,
                "CREATE FUNCTION dbo.fact(@n int) RETURNS int AS BEGIN \
                 IF @n <= 1 RETURN 1; \
                 RETURN @n * dbo.fact(@n - 1); \
                 END",
            );
            let steps = succeeds(&mut session, "SELECT dbo.fact(5)");
            assert_eq!(rows(&steps), integers(&[&[120]]));
            succeeds(
                &mut session,
                "CREATE FUNCTION dbo.forever(@n int) RETURNS int AS BEGIN RETURN dbo.forever(@n + 1) END",
            );
            fails_with(&mut session, "SELECT dbo.forever(1)", 217);
        })
        .expect("a thread for deep recursion starts");
    deep.join().expect("the deep recursion thread finishes");
}

#[test]
fn an_inline_table_valued_function_is_a_parameterized_view() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    with_numbers(&mut session);
    succeeds(
        &mut session,
        "CREATE FUNCTION dbo.at_least(@floor int) RETURNS TABLE AS RETURN (SELECT id, n FROM numbers WHERE n >= @floor)",
    );
    let steps = succeeds(&mut session, "SELECT id, n FROM dbo.at_least(15) ORDER BY id");
    assert_eq!(rows(&steps), integers(&[&[2, 20], &[3, 30]]));
    let steps = succeeds(&mut session, "SELECT * FROM at_least(1000)");
    assert_eq!(rows(&steps), Vec::<Vec<Value>>::new());
    assert_eq!(column_names(&steps), vec!["id".to_owned(), "n".to_owned()]);
}

#[test]
fn a_multi_statement_table_valued_function_returns_its_table_variable() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    succeeds(
        &mut session,
        "CREATE FUNCTION dbo.squares(@count int) RETURNS @result TABLE (number int, square int) AS BEGIN \
         DECLARE @i int = 1; \
         WHILE @i <= @count BEGIN INSERT INTO @result VALUES (@i, @i * @i); SET @i = @i + 1; END \
         DELETE FROM @result WHERE number = 2; \
         UPDATE @result SET square = square + 1000 WHERE number = 3; \
         RETURN; \
         END",
    );
    let steps = succeeds(&mut session, "SELECT number, square FROM dbo.squares(3) AS s");
    assert_eq!(rows(&steps), integers(&[&[1, 1], &[3, 1009]]));
    let steps = succeeds(&mut session, "SELECT * FROM dbo.squares(0)");
    assert_eq!(rows(&steps), Vec::<Vec<Value>>::new());
    assert_eq!(column_names(&steps), vec!["number".to_owned(), "square".to_owned()]);
}

#[test]
fn a_function_body_with_side_effects_is_refused_at_create() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    with_numbers(&mut session);
    fails_with(
        &mut session,
        "CREATE FUNCTION dbo.writes() RETURNS int AS BEGIN INSERT INTO numbers VALUES (9, 9); RETURN 1; END",
        443,
    );
    fails_with(
        &mut session,
        "CREATE FUNCTION dbo.prints() RETURNS int AS BEGIN PRINT 'hello'; RETURN 1; END",
        443,
    );
    fails_with(
        &mut session,
        "CREATE FUNCTION dbo.tries() RETURNS int AS BEGIN BEGIN TRY RETURN 1; END TRY BEGIN CATCH RETURN 0; END CATCH; RETURN 2; END",
        443,
    );
    fails_with(
        &mut session,
        "CREATE FUNCTION dbo.shows() RETURNS int AS BEGIN SELECT n FROM numbers; RETURN 1; END",
        444,
    );
    fails_with(
        &mut session,
        "CREATE FUNCTION dbo.executes() RETURNS int AS BEGIN EXEC sp_who; RETURN 1; END",
        557,
    );
}

#[test]
fn a_procedure_called_as_a_function_fails_with_4121() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    succeeds(&mut session, "CREATE PROCEDURE dbo.just_one AS SELECT 1");
    fails_with(&mut session, "SELECT dbo.just_one()", 4121);
}

#[test]
fn a_function_declared_with_returns_null_on_null_input_skips_its_body() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    succeeds(
        &mut session,
        "CREATE FUNCTION dbo.or_zero(@n int) RETURNS int WITH RETURNS NULL ON NULL INPUT AS BEGIN RETURN ISNULL(@n, 0) END",
    );
    let steps = succeeds(&mut session, "SELECT dbo.or_zero(NULL), dbo.or_zero(4)");
    assert_eq!(rows(&steps), vec![vec![Value::Null, Value::Integer(4)]]);
}

#[test]
fn a_function_parameter_converts_to_its_declared_type() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    create_twice(&mut session);
    let steps = succeeds(&mut session, "SELECT dbo.twice('7')");
    assert_eq!(rows(&steps), integers(&[&[14]]));
}
