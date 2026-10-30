use crate::memory_store::MemoryStore;
use crate::{BatchStep, Dialect, InfoMessage, NamedParameter, Session, SqlError, SqlType, Value};

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

fn refused(session: &mut Session<'_, MemoryStore>, text: &str) -> SqlError {
    session
        .execute_batch(text, &mut |_| true)
        .expect_err(text)
}

fn last_rows(steps: &[BatchStep]) -> Vec<Vec<Value>> {
    steps
        .iter()
        .rev()
        .find_map(|step| {
            step.outcome
                .as_ref()
                .ok()
                .filter(|result| result.columns.is_some())
        })
        .map(|result| result.rows.clone())
        .unwrap_or_default()
}

fn errors(steps: &[BatchStep]) -> Vec<Option<u32>> {
    steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().err())
        .map(SqlError::sqlserver_number)
        .collect()
}

fn messages(steps: &[BatchStep]) -> Vec<InfoMessage> {
    steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().ok())
        .flat_map(|result| result.info_messages.clone())
        .collect()
}

fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

#[test]
fn declared_variables_take_their_types_and_initial_values() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let steps = batch(
        &mut session,
        "DECLARE @a int = 5, @b varchar(3) = 'abcdef', @d int; SELECT @a AS a, @b AS b, @d AS d",
    );
    assert_eq!(steps.len(), 1);
    assert_eq!(
        last_rows(&steps),
        vec![vec![Value::Integer(5), text("abc"), Value::Null]]
    );
    let steps = batch(
        &mut session,
        "DECLARE @i int = 1; SET @i += 4; SET @i *= 2; SET @i = @i - 1; SELECT @i AS i",
    );
    assert_eq!(last_rows(&steps), vec![vec![Value::Integer(9)]]);
    let steps = batch(&mut session, "DECLARE @s nvarchar(20) = N'a'; SET @s += N'b'; SELECT @s AS s");
    assert_eq!(last_rows(&steps), vec![vec![text("ab")]]);
}

#[test]
fn compile_errors_of_batch_logic_run_nothing() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    for (text, code) in [
        ("SELECT @missing AS m", 137),
        ("DECLARE @a int; DECLARE @a int", 134),
        ("SET @a = 1 DECLARE @a int", 137),
        ("BREAK", 135),
        ("IF 1 = 1 CONTINUE", 136),
        ("RETURN 1", 178),
        ("THROW", 10704),
        ("BEGIN SELECT 1", 102),
        ("BEGIN END", 102),
        ("BEGIN TRY SELECT 1 END TRY", 102),
        ("GOTO done", 40517),
        ("DECLARE @a int = 1, @b int = @a", 40517),
        ("DECLARE @x int; SELECT @x = 1, 2", 141),
    ] {
        let error = refused(&mut session, text);
        assert_eq!(error.vendor_code(), Some(code), "{text}: {error}");
    }
}

#[test]
fn a_variable_declared_in_a_branch_that_does_not_run_still_exists() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let steps = batch(
        &mut session,
        "IF 1 = 0 BEGIN DECLARE @x int = 5 END SET @x = 7 SELECT @x AS x",
    );
    assert_eq!(last_rows(&steps), vec![vec![Value::Integer(7)]]);
}

#[test]
fn while_loops_with_break_and_continue() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let steps = batch(
        &mut session,
        "DECLARE @i int = 0, @sum int = 0
WHILE @i < 10
BEGIN
    SET @i += 1
    IF @i % 2 = 0 CONTINUE
    IF @i > 7 BREAK
    SET @sum += @i
END
SELECT @i AS i, @sum AS total",
    );
    assert_eq!(steps.len(), 1);
    assert_eq!(
        last_rows(&steps),
        vec![vec![Value::Integer(9), Value::Integer(16)]]
    );
    let steps = batch(
        &mut session,
        "DECLARE @outer int = 0, @inner int, @count int = 0 WHILE @outer < 3 BEGIN SET @outer += 1 SET @inner = 0 WHILE @inner < 3 BEGIN SET @inner += 1 IF @inner = 2 BREAK SET @count += 1 END END SELECT @count AS n",
    );
    assert_eq!(last_rows(&steps), vec![vec![Value::Integer(3)]]);
}

#[test]
fn if_else_chains_and_blocks() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let steps = batch(
        &mut session,
        "DECLARE @n int = 2
IF @n = 1 SELECT 'one' AS word
ELSE IF @n = 2 BEGIN SELECT 'two' AS word; SELECT 'still two' AS word END
ELSE SELECT 'many' AS word
SELECT 'after' AS word",
    );
    let words: Vec<Vec<Value>> = steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().ok())
        .flat_map(|result| result.rows.clone())
        .collect();
    assert_eq!(
        words,
        vec![vec![text("two")], vec![text("still two")], vec![text("after")]]
    );
}

#[test]
fn select_assigns_variables_from_the_last_row_and_accumulates_with_compound_operators() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    batch(
        &mut session,
        "CREATE TABLE t (id INT PRIMARY KEY, name NVARCHAR(10)) INSERT INTO t VALUES (1, N'a'), (2, N'b'), (3, N'c')",
    );
    let steps = batch(
        &mut session,
        "DECLARE @n int, @s nvarchar(100) = N'', @total int = 0, @none int = 9
SELECT @n = id FROM t ORDER BY id
SELECT @s += name, @total += id FROM t ORDER BY id
SELECT @none = id FROM t WHERE id > 10
SELECT @n AS n, @s AS s, @total AS total, @none AS none, @@ROWCOUNT AS counted",
    );
    assert_eq!(
        last_rows(&steps),
        vec![vec![
            Value::Integer(3),
            text("abc"),
            Value::Integer(6),
            Value::Integer(9),
            Value::Integer(0)
        ]]
    );
    let error = refused(&mut session, "DECLARE @s int = 0; SELECT @s = @s + id FROM t");
    assert_eq!(error.vendor_code(), Some(40517));
}

#[test]
fn try_catch_moves_errors_to_the_catch_block_with_their_details() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let steps = batch(
        &mut session,
        "BEGIN TRY
    SELECT 1 / 0 AS x
END TRY
BEGIN CATCH
    SELECT ERROR_NUMBER() AS number, ERROR_SEVERITY() AS severity, ERROR_STATE() AS state, ERROR_LINE() AS line, ERROR_MESSAGE() AS message
END CATCH
SELECT ERROR_NUMBER() AS outside",
    );
    assert!(errors(&steps).is_empty(), "{steps:?}");
    let rows: Vec<Vec<Value>> = steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().ok())
        .flat_map(|result| result.rows.clone())
        .collect();
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(8134),
                Value::Integer(16),
                Value::Integer(1),
                Value::Integer(2),
                text("Divide by zero error encountered.")
            ],
            vec![Value::Null]
        ]
    );
}

#[test]
fn throw_ends_the_batch_and_rethrows_the_caught_error() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let steps = batch(
        &mut session,
        "BEGIN TRY
    THROW 51000, 'custom failure', 3;
END TRY
BEGIN CATCH
    SELECT ERROR_NUMBER() AS n, ERROR_STATE() AS s, ERROR_MESSAGE() AS m;
    THROW;
END CATCH
SELECT 'not reached' AS x",
    );
    assert_eq!(
        last_rows(&steps),
        vec![vec![Value::Integer(51_000), Value::Integer(3), text("custom failure")]]
    );
    let failure = steps
        .last()
        .and_then(|step| step.outcome.as_ref().err())
        .expect("the rethrown error");
    assert_eq!(failure.sqlserver_number(), Some(51_000));
    assert_eq!(failure.sqlserver_state(), Some(3));
    assert_eq!(failure.severity(), Some(16));
    let steps = batch(&mut session, "THROW 50001, N'stop', 1; SELECT 1 AS x");
    assert_eq!(errors(&steps), vec![Some(50_001)]);
    assert_eq!(steps.len(), 1);
}

#[test]
fn nested_try_blocks_catch_errors_raised_in_an_inner_catch() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let steps = batch(
        &mut session,
        "BEGIN TRY
    BEGIN TRY
        THROW 50010, 'inner', 1
    END TRY
    BEGIN CATCH
        THROW 50020, 'from the catch', 1
    END CATCH
END TRY
BEGIN CATCH
    SELECT ERROR_NUMBER() AS n
END CATCH",
    );
    assert_eq!(last_rows(&steps), vec![vec![Value::Integer(50_020)]]);
}

#[test]
fn raiserror_sends_messages_or_raises_errors_by_severity() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let steps = batch(
        &mut session,
        "RAISERROR('Level %d of %s', 10, 1, 5, 'five')
RAISERROR('bad thing', 16, 2)
SELECT @@ERROR AS e
PRINT 'hello ' + CAST(42 AS varchar(10))
RAISERROR('flagged', 0, 1) WITH SETERROR
SELECT @@ERROR AS e",
    );
    assert_eq!(
        messages(&steps),
        vec![
            InfoMessage {
                number: 50_000,
                severity: 10,
                state: 1,
                text: "Level 5 of five".to_owned()
            },
            InfoMessage {
                number: 0,
                severity: 0,
                state: 1,
                text: "hello 42".to_owned()
            },
            InfoMessage {
                number: 50_000,
                severity: 0,
                state: 1,
                text: "flagged".to_owned()
            },
        ]
    );
    let failure = steps
        .iter()
        .find_map(|step| step.outcome.as_ref().err())
        .expect("the raised error");
    assert_eq!(failure.sqlserver_number(), Some(50_000));
    assert_eq!(failure.severity(), Some(16));
    assert_eq!(failure.sqlserver_state(), Some(2));
    assert_eq!(failure.message(), "bad thing");
    let rows: Vec<Vec<Value>> = steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().ok())
        .filter(|result| result.columns.is_some())
        .flat_map(|result| result.rows.clone())
        .collect();
    assert_eq!(
        rows,
        vec![vec![Value::Integer(50_000)], vec![Value::Integer(50_000)]]
    );
}

#[test]
fn error_and_rowcount_follow_each_statement() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    batch(&mut session, "CREATE TABLE t (id INT PRIMARY KEY) INSERT INTO t VALUES (1)");
    let steps = batch(
        &mut session,
        "INSERT INTO t VALUES (1)
SELECT @@ERROR AS first
SELECT @@ERROR AS second
UPDATE t SET id = id
IF @@ROWCOUNT = 1 SELECT 'one row' AS counted",
    );
    assert_eq!(errors(&steps), vec![Some(2627)]);
    let rows: Vec<Vec<Value>> = steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().ok())
        .filter(|result| result.columns.is_some())
        .flat_map(|result| result.rows.clone())
        .collect();
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(2627)],
            vec![Value::Integer(0)],
            vec![text("one row")]
        ]
    );
}

#[test]
fn return_ends_the_batch() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let steps = batch(&mut session, "SELECT 1 AS a; IF 1 = 1 RETURN; SELECT 2 AS b");
    assert_eq!(steps.len(), 1);
}

#[test]
fn table_variables_take_inserts_updates_and_deletes() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let steps = batch(
        &mut session,
        "DECLARE @t TABLE (id int, name nvarchar(20))
INSERT INTO @t VALUES (1, N'a'), (2, N'b'), (3, N'c')
UPDATE @t SET name = N'B' WHERE id = 2
DELETE FROM @t WHERE id = 1
INSERT @t (id) SELECT 4
SELECT id, name FROM @t ORDER BY id",
    );
    assert!(errors(&steps).is_empty(), "{steps:?}");
    assert_eq!(
        last_rows(&steps),
        vec![
            vec![Value::Integer(2), text("B")],
            vec![Value::Integer(3), text("c")],
            vec![Value::Integer(4), Value::Null]
        ]
    );
    let tags: Vec<String> = steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().ok())
        .map(|result| result.tag.clone())
        .collect();
    assert_eq!(
        tags,
        ["INSERT 0 3", "UPDATE 1", "DELETE 1", "INSERT 0 1", "SELECT 3"]
    );
    let steps = batch(
        &mut session,
        "DECLARE @t TABLE (id int) INSERT INTO @t VALUES (1, 2)",
    );
    assert_eq!(errors(&steps), vec![Some(213)]);
}

#[test]
fn dynamic_sql_runs_in_its_own_scope_with_parameters_and_outputs() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    batch(
        &mut session,
        "CREATE TABLE t (id INT PRIMARY KEY) INSERT INTO t VALUES (1), (2), (3)",
    );
    let steps = batch(
        &mut session,
        "DECLARE @sql nvarchar(100) = N'SELECT 40 + 2 AS answer'; EXEC (@sql)",
    );
    assert_eq!(last_rows(&steps), vec![vec![Value::Integer(42)]]);
    let steps = batch(
        &mut session,
        "DECLARE @count int
EXEC sp_executesql N'SELECT @c = COUNT(*) FROM t WHERE id > @min', N'@min int, @c int OUTPUT', @min = 1, @c = @count OUTPUT
SELECT @count AS n",
    );
    assert!(errors(&steps).is_empty(), "{steps:?}");
    assert_eq!(last_rows(&steps), vec![vec![Value::Integer(2)]]);
    let steps = batch(&mut session, "DECLARE @x int = 1; EXEC ('SELECT @x AS x')");
    assert_eq!(errors(&steps), vec![Some(137)]);
    let steps = batch(
        &mut session,
        "EXEC sp_executesql N'SELECT @a AS a', N'@a int', 5, 6",
    );
    assert_eq!(errors(&steps), vec![Some(8144)]);
    let steps = batch(
        &mut session,
        "EXEC ('EXEC (''SELECT ''''nested'''' AS level'')')",
    );
    assert_eq!(last_rows(&steps), vec![vec![text("nested")]]);
}

#[test]
fn name_resolution_errors_are_caught_only_from_a_lower_level() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let steps = batch(
        &mut session,
        "BEGIN TRY SELECT * FROM missing END TRY BEGIN CATCH SELECT 'caught' AS x END CATCH",
    );
    assert_eq!(errors(&steps), vec![Some(208)]);
    let steps = batch(
        &mut session,
        "BEGIN TRY EXEC ('SELECT * FROM missing') END TRY BEGIN CATCH SELECT ERROR_NUMBER() AS n END CATCH",
    );
    assert!(errors(&steps).is_empty(), "{steps:?}");
    assert_eq!(last_rows(&steps), vec![vec![Value::Integer(208)]]);
}

#[test]
fn existence_guards_run_their_statement_once() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let guarded = "IF NOT EXISTS (SELECT * FROM sys.objects WHERE object_id = OBJECT_ID(N'[dbo].[Bakers]') AND type in (N'U'))
BEGIN
CREATE TABLE [dbo].[Bakers]([Id] [int] NOT NULL PRIMARY KEY, [Name] [nvarchar](50) NOT NULL)
END";
    for _ in 0..2 {
        let steps = batch(&mut session, guarded);
        assert!(errors(&steps).is_empty(), "{steps:?}");
    }
    let view = "IF NOT EXISTS (SELECT * FROM sys.views WHERE object_id = OBJECT_ID(N'[dbo].[BakerNames]'))
EXEC dbo.sp_executesql @statement = N'CREATE VIEW [dbo].[BakerNames] AS SELECT [Name] FROM [dbo].[Bakers]'";
    for _ in 0..2 {
        let steps = batch(&mut session, view);
        assert!(errors(&steps).is_empty(), "{steps:?}");
    }
    let steps = batch(&mut session, "SELECT COUNT(*) AS n FROM dbo.BakerNames");
    assert_eq!(last_rows(&steps), vec![vec![Value::Integer(0)]]);
}

#[test]
fn execute_runs_batch_logic_and_returns_the_first_uncaught_error() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let mut rows = Vec::new();
    session
        .execute(
            "DECLARE @x int = 2; IF @x > 1 SELECT @x * 3 AS y",
            &mut |result| rows.extend(result.rows),
        )
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(rows, vec![vec![Value::Integer(6)]]);
    let error = session
        .execute(
            "BEGIN TRY THROW 50005, 'caught', 1 END TRY BEGIN CATCH END CATCH; THROW 50006, 'uncaught', 1",
            &mut |_| {},
        )
        .expect_err("the uncaught THROW");
    assert_eq!(error.sqlserver_number(), Some(50_006));
}

#[test]
fn parameters_of_a_parameterized_batch_are_variables_it_may_assign() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let parameters = [
        NamedParameter {
            name: "@P".to_owned(),
            declared_type: Some(SqlType::Integer),
            value: Value::Integer(21),
        },
        NamedParameter {
            name: "@note".to_owned(),
            declared_type: Some(SqlType::Varchar(Some(20))),
            value: Value::Null,
        },
    ];
    session
        .execute_parameterized_batch(
            "SET @p = @p * 2 SET @note = N'doubled'",
            &parameters,
            &mut |step| {
                assert!(step.outcome.is_ok(), "{step:?}");
                true
            },
        )
        .unwrap_or_else(|error| panic!("{error}"));
    let values: Vec<Value> = session
        .parameter_values_after_batch()
        .iter()
        .map(|parameter| parameter.value.clone())
        .collect();
    assert_eq!(values, vec![Value::Integer(42), text("doubled")]);
    let error = session
        .execute_parameterized_batch("DECLARE @p int", &parameters, &mut |_| true)
        .expect_err("a second @p");
    assert_eq!(error.vendor_code(), Some(134));
}

#[test]
fn sp_unprepare_in_a_batch_is_handed_to_the_server() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let steps = batch(&mut session, "EXEC sp_unprepare 7; EXEC sp_unprepare 9;");
    assert!(errors(&steps).is_empty(), "{steps:?}");
    assert_eq!(session.take_unprepare_requests(), vec![7, 9]);
    assert!(session.take_unprepare_requests().is_empty());
}

#[test]
fn hostile_batch_logic_returns_errors_and_never_panics() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let deep_if = format!("{}SELECT 1", "IF 1 = 1 ".repeat(5_000));
    let deep_begin = format!("{}SELECT 1{}", "BEGIN ".repeat(5_000), " END".repeat(5_000));
    let deep_dynamic = "DECLARE @s nvarchar(200) = N'EXEC sp_executesql @s, N''@s nvarchar(200)'', @s'
EXEC sp_executesql @s, N'@s nvarchar(200)', @s";
    for text in [
        deep_if.as_str(),
        deep_begin.as_str(),
        "WHILE",
        "IF",
        "BEGIN TRY END CATCH",
        "BEGIN CATCH END CATCH",
        "END",
        "ELSE SELECT 1",
        "DECLARE",
        "DECLARE @",
        "SET @@x = 1",
        "RAISERROR",
        "RAISERROR('x')",
        "THROW 1",
        "EXEC (",
        "EXEC sp_executesql",
        "SELECT @a +=",
    ] {
        let _ = session.execute_batch(text, &mut |_| true);
        let _ = session.execute(text, &mut |_| {});
    }
    let error = refused(&mut session, &deep_if);
    assert_eq!(error.vendor_code(), Some(191));
    let mut failures = Vec::new();
    session
        .execute_batch(deep_dynamic, &mut |step| {
            if let Err(error) = step.outcome {
                failures.push(error.vendor_code());
            }
            true
        })
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(failures, vec![Some(217)]);
}

fn shown(rows: &[Vec<Value>]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|value| value.to_text().unwrap_or_else(|| "NULL".to_owned()))
                .collect()
        })
        .collect()
}

#[test]
fn xact_abort_on_rolls_back_the_transaction_and_ends_the_batch_at_a_run_time_error() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    batch(&mut session, "CREATE TABLE t (id INT PRIMARY KEY)");
    let insert_twice = "BEGIN TRAN\nINSERT INTO t VALUES (1)\nINSERT INTO t VALUES (1)\nINSERT INTO t VALUES (2)\nCOMMIT";
    let aborted = batch(&mut session, &format!("SET XACT_ABORT ON\n{insert_twice}"));
    assert_eq!(errors(&aborted), vec![Some(2627)]);
    assert!(!session.in_block());
    assert_eq!(
        shown(&last_rows(&batch(
            &mut session,
            "SELECT COUNT(*) AS n, @@TRANCOUNT AS open FROM t"
        ))),
        vec![vec!["0".to_owned(), "0".to_owned()]]
    );
    let continued = batch(&mut session, &format!("SET XACT_ABORT OFF\n{insert_twice}"));
    assert_eq!(errors(&continued), vec![Some(2627)]);
    assert_eq!(
        shown(&last_rows(&batch(&mut session, "SELECT COUNT(*) AS n FROM t"))),
        vec![vec!["2".to_owned()]]
    );
    let raised = batch(
        &mut session,
        "SET XACT_ABORT ON\nBEGIN TRAN\nINSERT INTO t VALUES (30)\nRAISERROR('only a message', 16, 1)\nINSERT INTO t VALUES (31)\nCOMMIT",
    );
    assert_eq!(errors(&raised), vec![Some(50000)]);
    assert_eq!(
        shown(&last_rows(&batch(
            &mut session,
            "SELECT COUNT(*) AS n FROM t WHERE id >= 30"
        ))),
        vec![vec!["2".to_owned()]]
    );
    session.execute("BEGIN TRANSACTION", &mut |_| {}).unwrap();
    session.execute("INSERT INTO t VALUES (40)", &mut |_| {}).unwrap();
    let error = session
        .execute("INSERT INTO t VALUES (40)", &mut |_| {})
        .unwrap_err();
    assert_eq!(error.vendor_code(), Some(2627));
    assert!(!session.in_block());
    assert_eq!(
        shown(&last_rows(&batch(
            &mut session,
            "SELECT COUNT(*) AS n FROM t WHERE id = 40"
        ))),
        vec![vec!["0".to_owned()]]
    );
}

#[test]
fn xact_abort_on_inside_try_leaves_an_uncommittable_transaction_for_the_catch_block() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    batch(&mut session, "CREATE TABLE t (id INT PRIMARY KEY)");
    let caught = batch(
        &mut session,
        "SET XACT_ABORT ON\nBEGIN TRY\n  BEGIN TRAN\n  INSERT INTO t VALUES (10)\n  INSERT INTO t VALUES (10)\n  COMMIT\nEND TRY\nBEGIN CATCH\n  SELECT XACT_STATE() AS state, @@TRANCOUNT AS open\n  ROLLBACK\nEND CATCH",
    );
    assert!(errors(&caught).is_empty(), "{caught:?}");
    assert_eq!(
        shown(&last_rows(&caught)),
        vec![vec!["-1".to_owned(), "1".to_owned()]]
    );
    assert!(!session.in_block());
    let doomed = batch(
        &mut session,
        "BEGIN TRY\n  BEGIN TRAN\n  INSERT INTO t VALUES (20)\n  SELECT 1 / 0 AS x\nEND TRY\nBEGIN CATCH\n  INSERT INTO t VALUES (21)\nEND CATCH",
    );
    assert_eq!(errors(&doomed), vec![Some(3930)]);
    assert!(!session.in_block());
    let commit = batch(
        &mut session,
        "BEGIN TRY\n  BEGIN TRAN\n  INSERT INTO t VALUES (22)\n  INSERT INTO t VALUES (22)\nEND TRY\nBEGIN CATCH\n  COMMIT\nEND CATCH",
    );
    assert_eq!(errors(&commit), vec![Some(3930)]);
    let unhandled = batch(
        &mut session,
        "BEGIN TRY\n  BEGIN TRAN\n  INSERT INTO t VALUES (23)\n  INSERT INTO t VALUES (23)\nEND TRY\nBEGIN CATCH\n  SELECT 1 AS handled\nEND CATCH",
    );
    assert_eq!(errors(&unhandled), vec![Some(3998)]);
    assert!(!session.in_block());
    assert_eq!(
        shown(&last_rows(&batch(
            &mut session,
            "SELECT COUNT(*) AS n, XACT_STATE() AS state FROM t"
        ))),
        vec![vec!["0".to_owned(), "0".to_owned()]]
    );
}
