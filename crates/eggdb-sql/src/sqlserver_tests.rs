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
fn plus_joins_strings_and_converts_a_string_beside_a_number_by_precedence() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut session,
        "CREATE TABLE s (id INT PRIMARY KEY, name NVARCHAR(10), n INT, digits VARCHAR(5)) INSERT INTO s VALUES (1, N'a', 2, '40')",
    );
    assert_eq!(
        values(
            &mut session,
            "SELECT name + name AS x, n + digits AS y, digits + n AS z FROM s"
        ),
        vec![vec![
            Value::Text("aa".into()),
            Value::BigInt(42),
            Value::BigInt(42)
        ]]
    );
    for text in ["SELECT name + n AS x FROM s", "SELECT n + name AS x FROM s"] {
        let error = session.execute(text, &mut |_| {}).expect_err(text);
        assert_eq!(error.vendor_code(), Some(245), "{text}: {error}");
    }
    assert_eq!(
        values(&mut session, "SELECT n + n AS x, name + NULL AS y FROM s"),
        vec![vec![Value::Integer(4), Value::Null]]
    );
}

#[test]
fn other_dialects_keep_their_own_rules_for_mixed_types() {
    let store = MemoryStore::default();
    let mut postgresql = Session::new(&store);
    for text in ["SELECT 'a'::text + 1", "SELECT 1 + true"] {
        let error = postgresql.execute(text, &mut |_| {}).expect_err(text);
        assert_eq!(error.state().code(), "42883", "{text}");
    }
    values(
        &mut postgresql,
        "CREATE TABLE r (id integer PRIMARY KEY, n integer); INSERT INTO r VALUES (1, 10.6)",
    );
    assert_eq!(
        values(&mut postgresql, "SELECT n FROM r"),
        vec![vec![Value::Integer(11)]]
    );
}

#[test]
fn assignment_follows_sql_servers_conversions() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut session,
        "CREATE TABLE a (id INT PRIMARY KEY, n INT, flag BIT, guid UNIQUEIDENTIFIER, d DATE, bytes VARBINARY(MAX), m MONEY)",
    );
    values(
        &mut session,
        "INSERT INTO a VALUES (1, 10.6, 'TRUE', N'6f9619ff-8b86-d011-b42d-00c04fc964ff', '07/04/2020', 0x0102FF, 32.38)",
    );
    values(
        &mut session,
        "INSERT INTO a (id, n, flag) VALUES (2, -10.6, 0)",
    );
    let rows = values(
        &mut session,
        "SELECT n, flag, CONVERT(varchar(36), guid), CONVERT(varchar(10), d, 103), bytes, m FROM a ORDER BY id",
    );
    assert_eq!(rows[0][0], Value::Integer(10));
    assert_eq!(rows[0][1], Value::Boolean(true));
    assert_eq!(
        rows[0][2],
        Value::Text("6F9619FF-8B86-D011-B42D-00C04FC964FF".into())
    );
    assert_eq!(rows[0][3], Value::Text("04/07/2020".into()));
    assert_eq!(rows[0][4], Value::Bytea(vec![1, 2, 0xFF]));
    assert_eq!(
        Dialect::SqlServer.value_text(&rows[0][5]).as_deref(),
        Some("32.3800")
    );
    assert_eq!(rows[1][0], Value::Integer(-10));
    assert_eq!(rows[1][1], Value::Boolean(false));
}

#[test]
fn comparisons_of_different_types_convert_or_fail_and_never_silently_differ() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut session,
        "CREATE TABLE l (id INT PRIMARY KEY, code VARCHAR(5), d DATE) CREATE TABLE r (id INT PRIMARY KEY, n INT, g UNIQUEIDENTIFIER) INSERT INTO l VALUES (1, '2', '2020-01-01'), (2, 'x', NULL) INSERT INTO r VALUES (2, 2, NULL)",
    );
    assert_eq!(
        values(
            &mut session,
            "SELECT l.id FROM l JOIN r ON l.code = CAST(r.n AS varchar(5))"
        ),
        vec![vec![Value::Integer(1)]]
    );
    let joined = session.execute("SELECT l.id FROM l JOIN r ON l.code = r.n", &mut |_| {});
    assert!(
        matches!(&joined, Err(error) if error.vendor_code() == Some(245)),
        "{joined:?}"
    );
    assert_eq!(
        values(&mut session, "SELECT id FROM r WHERE n IN ('2', '3')"),
        vec![vec![Value::Integer(2)]]
    );
    let in_list = session.execute("SELECT id FROM r WHERE n IN ('x')", &mut |_| {});
    assert!(matches!(&in_list, Err(error) if error.vendor_code() == Some(245)));
    assert_eq!(
        values(
            &mut session,
            "SELECT CASE WHEN n = '2' THEN 1 ELSE 0 END FROM r"
        ),
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(
        values(&mut session, "SELECT id FROM r WHERE g = NULL OR n = NULL"),
        Vec::<Vec<Value>>::new()
    );
    let clash = session.execute("SELECT id FROM l WHERE d = 5", &mut |_| {});
    assert!(
        matches!(&clash, Err(error) if error.vendor_code() == Some(206)),
        "{clash:?}"
    );
}

#[test]
fn session_values_are_read_when_each_statement_runs() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut session,
        "CREATE TABLE i (id INT IDENTITY(1,1) PRIMARY KEY, name NVARCHAR(10)) CREATE TABLE p (id INT PRIMARY KEY)",
    );
    assert_eq!(
        values(&mut session, "SELECT SCOPE_IDENTITY()"),
        vec![vec![Value::Null]]
    );
    let steps = batch(
        &mut session,
        "INSERT INTO i (name) VALUES (N'a'), (N'b'), (N'c')\nSELECT @@ROWCOUNT AS n, SCOPE_IDENTITY() AS id\nINSERT INTO p VALUES (9)\nSELECT SCOPE_IDENTITY() AS id\nSET NOCOUNT ON\nSELECT @@ROWCOUNT AS n",
    )
    .expect("batch");
    let rows: Vec<Vec<Vec<Value>>> = steps
        .into_iter()
        .filter_map(|step| step.outcome.ok())
        .filter(|result| result.columns.is_some())
        .map(|result| result.rows)
        .collect();
    let id = |value: &Value| Dialect::SqlServer.value_text(value);
    assert_eq!(rows[0][0][0], Value::Integer(3));
    assert_eq!(id(&rows[0][0][1]).as_deref(), Some("3"));
    assert_eq!(id(&rows[1][0][0]).as_deref(), Some("3"));
    assert_eq!(rows[2][0][0], Value::Integer(0));
}

#[test]
fn output_returns_the_rows_a_statement_changed() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut session,
        "CREATE TABLE o (id INT IDENTITY(1,1) PRIMARY KEY, qty INT)",
    );
    assert_eq!(
        values(
            &mut session,
            "INSERT INTO o (qty) OUTPUT INSERTED.id, inserted.qty VALUES (5), (6)"
        ),
        vec![
            vec![Value::Integer(1), Value::Integer(5)],
            vec![Value::Integer(2), Value::Integer(6)]
        ]
    );
    assert_eq!(
        values(
            &mut session,
            "UPDATE o SET qty = qty * 2 OUTPUT inserted.qty WHERE id = 1"
        ),
        vec![vec![Value::Integer(10)]]
    );
    assert_eq!(
        values(&mut session, "DELETE FROM o OUTPUT deleted.* WHERE id = 2"),
        vec![vec![Value::Integer(2), Value::Integer(6)]]
    );
    for (text, code) in [
        ("UPDATE o SET qty = 1 OUTPUT deleted.qty", 40517),
        ("DELETE FROM o OUTPUT inserted.qty", 4104),
        (
            "INSERT INTO o (qty) OUTPUT inserted.id INTO x VALUES (1)",
            40517,
        ),
    ] {
        let error = session.execute(text, &mut |_| {}).expect_err(text);
        assert_eq!(error.vendor_code(), Some(code), "{text}: {error}");
    }
}

#[test]
fn defaults_fold_constants_to_the_columns_type() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut session,
        "CREATE TABLE f (id INT PRIMARY KEY, flag BIT DEFAULT ((0)), other BIT DEFAULT (CONVERT([bit],(1))), g UNIQUEIDENTIFIER DEFAULT (newsequentialid()), note NVARCHAR(10) DEFAULT (N''), at DATETIMEOFFSET DEFAULT (sysdatetimeoffset()))",
    );
    values(&mut session, "INSERT INTO f (id) VALUES (1)");
    let rows = values(&mut session, "SELECT flag, other, note, g, at FROM f");
    assert_eq!(rows[0][0], Value::Boolean(false));
    assert_eq!(rows[0][1], Value::Boolean(true));
    assert_eq!(rows[0][2], Value::Text(String::new()));
    assert!(matches!(rows[0][3], Value::Uuid(_)));
    assert!(matches!(rows[0][4], Value::TimestampTz(_)));
    let error = session
        .execute(
            "CREATE TABLE g (id INT PRIMARY KEY, due DATETIME DEFAULT (dateadd(day,(30),getdate())))",
            &mut |_| {},
        )
        .expect_err("a non-constant default");
    assert_eq!(error.vendor_code(), Some(40517));
}

#[test]
fn a_table_made_in_transact_sql_reads_back_in_postgresql() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut session,
        "CREATE TABLE x (id INT PRIMARY KEY, t TINYINT, b BIT, m MONEY, d DATETIME, d2 DATETIME2(7), o DATETIMEOFFSET(7), tm TIME(7), g UNIQUEIDENTIFIER, v VARBINARY(10))",
    );
    values(
        &mut session,
        "INSERT INTO x VALUES (1, 7, 1, 1.5, CAST(N'1996-07-04T00:00:00.000' AS DateTime), CAST(N'2020-01-01T10:00:00.1234567' AS DateTime2), CAST(N'2020-01-01T10:00:00.1234567+02:00' AS DateTimeOffset), CAST(N'13:45:10.1234567' AS Time), NEWID(), 0x01)",
    );
    let mut postgresql = Session::new(&store);
    let rows = values(
        &mut postgresql,
        "SELECT t, b, m::text, d::text, d2::text, o::text, tm::text, v FROM x",
    );
    assert_eq!(
        rows,
        vec![vec![
            Value::SmallInt(7),
            Value::Boolean(true),
            Value::Text("1.5000".into()),
            Value::Text("1996-07-04 00:00:00".into()),
            Value::Text("2020-01-01 10:00:00.123456".into()),
            Value::Text("2020-01-01 08:00:00.123456+00".into()),
            Value::Text("13:45:10.123456".into()),
            Value::Bytea(vec![1]),
        ]]
    );
}
