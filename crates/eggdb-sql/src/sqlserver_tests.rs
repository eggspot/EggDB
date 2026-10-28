use crate::memory_store::MemoryStore;
use crate::{BatchStep, Dialect, QueryResult, Session, SqlError, Value};

fn batch(session: &mut Session<'_, MemoryStore>, text: &str) -> Result<Vec<BatchStep>, SqlError> {
    let mut steps = Vec::new();
    session.execute_batch(text, &mut |step| {
        steps.push(step);
        true
    })?;
    Ok(steps)
}

fn values(session: &mut Session<'_, MemoryStore>, text: &str) -> Vec<Vec<Value>> {
    let mut last = None;
    session
        .execute(text, &mut |result: QueryResult| last = Some(result.rows))
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    last.unwrap_or_default()
}

fn codes(steps: &[BatchStep]) -> Vec<Option<u16>> {
    steps
        .iter()
        .map(|step| step.outcome.as_ref().err().and_then(SqlError::vendor_code))
        .collect()
}

#[test]
fn a_syntax_error_anywhere_in_a_batch_runs_none_of_it() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    let error = batch(
        &mut session,
        "CREATE TABLE t (id INT PRIMARY KEY)\nSELECT * FORM t",
    )
    .expect_err("a syntax error");
    assert_eq!(error.vendor_code(), Some(102));
    let mut postgresql = Session::new(&store);
    assert!(postgresql.execute("SELECT * FROM t", &mut |_| {}).is_err());
}

#[test]
fn constraint_violations_end_their_statement_and_other_errors_end_the_batch() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    let steps = batch(
        &mut session,
        "CREATE TABLE t (id INT PRIMARY KEY) INSERT INTO t VALUES (1) INSERT INTO t VALUES (1) INSERT INTO t VALUES (2) SELECT * FROM missing INSERT INTO t VALUES (3)",
    )
    .unwrap();
    assert_eq!(codes(&steps), vec![None, None, Some(2627), None, Some(208)]);
    assert_eq!(
        values(&mut session, "SELECT count(*) AS n FROM t"),
        vec![vec![Value::BigInt(2)]]
    );
    assert!(steps.iter().all(|step| !step.in_transaction));
    assert!(steps[1].statement_start > steps[0].statement_start);
}

#[test]
fn an_error_that_rolls_back_the_transaction_ends_the_batch_without_committing_later_statements() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    batch(
        &mut session,
        "CREATE TABLE t (id INT PRIMARY KEY) INSERT INTO t VALUES (1)",
    )
    .unwrap();
    let steps = batch(
        &mut session,
        "BEGIN TRAN INSERT INTO t VALUES (2) INSERT INTO t VALUES (3), (1) INSERT INTO t VALUES (4) COMMIT",
    )
    .unwrap();
    assert_eq!(codes(&steps), vec![None, None, Some(2627)]);
    assert!(steps[1].in_transaction);
    assert!(!steps[2].in_transaction);
    assert_eq!(
        values(&mut session, "SELECT count(*) AS n FROM t"),
        vec![vec![Value::BigInt(1)]]
    );
}

#[test]
fn tables_made_in_transact_sql_and_in_postgresql_are_the_same_tables() {
    let store = MemoryStore::default();
    let mut sqlserver = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut sqlserver,
        "CREATE TABLE [dbo].[Bakes] ([ID] INT IDENTITY PRIMARY KEY, [Name] NVARCHAR(20))",
    );
    values(&mut sqlserver, "INSERT INTO BAKES (NAME) VALUES (N'bread')");
    let mut postgresql = Session::new(&store);
    assert_eq!(
        values(&mut postgresql, "SELECT id, name FROM public.bakes"),
        vec![vec![Value::Integer(1), Value::Text("bread".into())]]
    );
    values(
        &mut postgresql,
        "CREATE TABLE stock (id integer PRIMARY KEY, qty integer); INSERT INTO stock VALUES (1, 7)",
    );
    assert_eq!(
        values(&mut sqlserver, "SELECT TOP 1 [Qty] AS q FROM [Stock]"),
        vec![vec![Value::Integer(7)]]
    );
}

#[test]
fn no_error_of_the_sql_server_dialect_carries_postgresql_wording() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut session,
        "CREATE TABLE t (id INT PRIMARY KEY, name NVARCHAR(3) NOT NULL, made DATETIME2(3) DEFAULT GETDATE(), n INT DEFAULT ((0)))",
    );
    values(&mut session, "INSERT INTO t (id, name) VALUES (1, N'a')");
    let rows = values(&mut session, "SELECT n, made AS m FROM t");
    assert_eq!(rows[0][0], Value::Integer(0));
    assert!(matches!(rows[0][1], Value::Timestamp(_)));
    let postgresql_words = [
        "relation \"",
        "duplicate key value violates",
        "syntax error at or near",
        "column \"",
        "violates",
        "PostgreSQL",
        "operator does not exist",
        "invalid input syntax",
    ];
    for text in [
        "SELECT * FROM nowhere",
        "SELECT nope FROM t",
        "INSERT INTO t (id, name) VALUES (1, N'b')",
        "INSERT INTO t (id, name) VALUES (2, NULL)",
        "CREATE TABLE c (id INT PRIMARY KEY CHECK (id > 0))",
        "CREATE TABLE d (id INT PRIMARY KEY, at DATETIME2 DEFAULT SYSDATETIME() + 1)",
        "INSERT INTO t (id, name) VALUES (3, N'long')",
        "SELECT 1 / 0 AS x",
        "SELECT CAST(N'x' AS INT) AS x",
        "SELECT name + 1 AS x FROM t",
        "SELECT * FROM t WHERE",
        "SELEC 1",
        "SELECT 'unterminated",
        "SELECT version() AS v",
        "SELECT @@whatever AS v",
        "DROP TABLE nowhere",
        "CREATE TABLE t (id INT PRIMARY KEY)",
        "CREATE VIEW v AS SELECT 1 AS one",
        "SELECT id FROM t GROUP BY name",
        "SET XACT_ABORT ON",
        "COMMIT",
        "USE elsewhere",
        "SELECT (SELECT id FROM t UNION ALL SELECT id FROM t) AS x",
    ] {
        let error = session.execute(text, &mut |_| {}).expect_err(text);
        assert_eq!(error.vendor(), Some(Dialect::SqlServer), "{text}: {error}");
        let shown = error.to_string();
        assert!(shown.starts_with("Msg "), "{text}: {shown}");
        for word in postgresql_words {
            assert!(!shown.contains(word), "{text}: {shown}");
        }
        assert_eq!(error.hint(), None, "{text}");
        assert!(error.severity().is_some(), "{text}");
    }
}

#[test]
fn errors_are_worded_from_the_failing_statement_not_the_first_of_the_batch() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    let steps = batch(&mut session, "SELECT 1 AS a\nDROP TABLE nowhere").unwrap();
    assert_eq!(codes(&steps), vec![None, Some(3701)]);
    let error = session
        .execute("SELECT 1 AS a; DROP TABLE nowhere", &mut |_| {})
        .expect_err("a missing table");
    assert_eq!(error.vendor_code(), Some(3701));
    values(
        &mut session,
        "CREATE TABLE nn (id INT PRIMARY KEY, v INT NOT NULL) INSERT INTO nn VALUES (1, 1)",
    );
    let steps = batch(
        &mut session,
        "SET NOCOUNT ON\nUPDATE nn SET v = NULL WHERE id = 1\nINSERT INTO nn VALUES (2, NULL)",
    )
    .unwrap();
    let messages: Vec<String> = steps
        .iter()
        .filter_map(|step| {
            step.outcome
                .as_ref()
                .err()
                .map(|error| error.message().to_owned())
        })
        .collect();
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(messages[0].ends_with("UPDATE fails."), "{messages:?}");
    assert!(messages[1].ends_with("INSERT fails."), "{messages:?}");
}

#[test]
fn string_columns_joined_with_plus_are_refused_as_not_supported() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut session,
        "CREATE TABLE s (id INT PRIMARY KEY, name NVARCHAR(10), n INT) INSERT INTO s VALUES (1, N'a', 2)",
    );
    for text in [
        "SELECT name + name AS x FROM s",
        "SELECT name + n AS x FROM s",
        "SELECT n + name AS x FROM s",
    ] {
        let error = session.execute(text, &mut |_| {}).expect_err(text);
        assert_eq!(error.vendor_code(), Some(40517), "{text}: {error}");
    }
    assert_eq!(
        values(&mut session, "SELECT n + n AS x FROM s"),
        vec![vec![Value::Integer(4)]]
    );
}
