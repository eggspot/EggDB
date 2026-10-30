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
        vec![vec![Value::Integer(2)]]
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
        vec![vec![Value::Integer(1)]]
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
    values(
        &mut session,
        "CREATE TABLE c (id INT PRIMARY KEY CHECK (id > 0))",
    );
    values(&mut session, "INSERT INTO c VALUES (1)");
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
        "INSERT INTO c VALUES (0)",
        "ALTER TABLE c ADD CONSTRAINT c_small CHECK (id > 5)",
        "CREATE TABLE d (id INT IDENTITY(1, 0) PRIMARY KEY)",
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
        "CREATE VIEW v AS SELECT 1 AS one ORDER BY one",
        "ALTER VIEW nowhere AS SELECT 1 AS one",
        "SELECT 1 AS one; CREATE VIEW v AS SELECT 1 AS one",
        "SELECT id FROM t GROUP BY name",
        "SET NOEXEC ON",
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
}

#[test]
fn any_transact_sql_default_is_kept_as_written_and_runs_in_transact_sql() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut session,
        "CREATE TABLE g (id INT PRIMARY KEY, due DATETIME DEFAULT (dateadd(day,(30),getdate())), code NVARCHAR(10) DEFAULT (CONCAT(N'x', DATEPART(year, CAST('20240115' AS date)))), n INT DEFAULT (abs(-2) * 3))",
    );
    values(&mut session, "INSERT INTO g (id) VALUES (1)");
    let rows = values(
        &mut session,
        "SELECT DATEDIFF(day, GETDATE(), due) AS days, code, n FROM g",
    );
    assert!(matches!(rows[0][0], Value::Integer(29 | 30)), "{rows:?}");
    assert_eq!(rows[0][1], Value::Text("x2024".into()));
    assert_eq!(rows[0][2], Value::Integer(6));
    values(&mut session, "ALTER TABLE g ADD z INT NULL");
    values(
        &mut session,
        "ALTER TABLE g ADD CONSTRAINT df_g_z DEFAULT (len(N'four')) FOR z",
    );
    values(&mut session, "INSERT INTO g (id) VALUES (2)");
    let rows = values(&mut session, "SELECT z FROM g WHERE id = 2");
    assert_eq!(rows[0][0], Value::Integer(4));
    let mut postgresql = Session::new(&store);
    postgresql
        .execute("INSERT INTO g (id) VALUES (3)", &mut |_| {})
        .unwrap();
    let rows = values(&mut postgresql, "SELECT n, z FROM g WHERE id = 3");
    assert_eq!(rows[0], [Value::Integer(6), Value::Integer(4)]);
    let shown = values(
        &mut postgresql,
        "SELECT column_default FROM information_schema.columns WHERE table_name = 'g' AND column_name = 'due'",
    );
    assert_eq!(
        shown[0][0].to_text().as_deref(),
        Some("dateadd(day,(30),getdate())")
    );
    for refused in [
        "CREATE TABLE h (id INT PRIMARY KEY, n INT DEFAULT ((SELECT 1)))",
        "CREATE TABLE h (id INT PRIMARY KEY, a INT, n INT DEFAULT (a + 1))",
    ] {
        let error = session
            .execute(refused, &mut |_| {})
            .expect_err(refused);
        assert_eq!(error.state().code(), "0A000", "{refused}: {error}");
    }
}

#[test]
fn identity_seeds_and_increments_number_new_rows_from_the_stored_definition() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut session,
        "CREATE TABLE up (id INT IDENTITY(100, 7) PRIMARY KEY, note NVARCHAR(5))",
    );
    values(
        &mut session,
        "CREATE TABLE down (id BIGINT IDENTITY(0, -2) PRIMARY KEY, note NVARCHAR(5))",
    );
    values(
        &mut session,
        "INSERT INTO up (note) VALUES (N'a'), (N'b')\nINSERT INTO down (note) VALUES (N'a'), (N'b'), (N'c')",
    );
    let rows = values(&mut session, "SELECT id FROM up ORDER BY id");
    assert_eq!(rows, [[Value::Integer(100)], [Value::Integer(107)]]);
    let rows = values(&mut session, "SELECT id FROM down ORDER BY id DESC");
    assert_eq!(
        rows,
        [[Value::BigInt(0)], [Value::BigInt(-2)], [Value::BigInt(-4)]]
    );
    let rows = values(&mut session, "SELECT SCOPE_IDENTITY() AS last");
    assert_eq!(rows[0][0].to_text().as_deref(), Some("-4"));
    values(&mut session, "DELETE FROM up WHERE id = 107");
    let mut later = Session::with_dialect(&store, Dialect::SqlServer);
    values(&mut later, "INSERT INTO up (note) VALUES (N'c')");
    let rows = values(&mut later, "SELECT id FROM up ORDER BY id");
    assert_eq!(rows, [[Value::Integer(100)], [Value::Integer(107)]]);
    let rows = values(
        &mut later,
        "SELECT CAST(seed_value AS int) AS seed, CAST(increment_value AS int) AS step FROM sys.identity_columns WHERE object_id = OBJECT_ID('up')",
    );
    assert_eq!(rows, [[Value::Integer(100), Value::Integer(7)]]);
    for (refused, why) in [
        (
            "CREATE TABLE z1 (id INT IDENTITY(1, 0) PRIMARY KEY)",
            "The IDENTITY increment, other than zero, of column 'id' must be a whole number within the range of int.",
        ),
        (
            "CREATE TABLE z2 (id SMALLINT IDENTITY(40000, 1) PRIMARY KEY)",
            "The IDENTITY seed of column 'id' must be a whole number within the range of smallint.",
        ),
        (
            "CREATE TABLE z3 (id INT IDENTITY(1, 9999999999) PRIMARY KEY)",
            "The IDENTITY increment, other than zero, of column 'id' must be a whole number within the range of int.",
        ),
        (
            "CREATE TABLE z4 (id BIGINT IDENTITY(99999999999999999999, 1) PRIMARY KEY)",
            "The IDENTITY seed of column 'id' must be a whole number within the range of bigint.",
        ),
    ] {
        let error = later.execute(refused, &mut |_| {}).expect_err(refused);
        assert_eq!(error.state().code(), "22023", "{refused}: {error}");
        assert_eq!(error.vendor_code(), Some(50000), "{refused}");
        assert_eq!(error.message(), why, "{refused}");
    }
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

#[test]
fn errors_after_a_conversion_keep_their_own_numbers() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut session,
        "CREATE TABLE e (id INT PRIMARY KEY, digits VARCHAR(5), at DATETIME) INSERT INTO e VALUES (1, '5', '2020-01-01 10:00')",
    );
    for (text, code) in [
        ("SELECT digits / 0 FROM e", 8134),
        ("SELECT digits - digits FROM e", 402),
        ("SELECT at + 1e10 FROM e", 517),
        ("SELECT CONVERT(date, '2020-01-01', 999)", 281),
    ] {
        let error = session.execute(text, &mut |_| {}).expect_err(text);
        assert_eq!(error.vendor_code(), Some(code), "{text}: {error}");
    }
    assert_eq!(
        values(
            &mut session,
            "SELECT DATEDIFF(day, at - 1, at + 1.5) FROM e"
        ),
        vec![vec![Value::Integer(2)]]
    );
}

fn stored(store: &MemoryStore, relation: &str) -> Vec<u8> {
    use crate::store::{KeyValueRead, KeyValueStore};
    store
        .begin_read()
        .unwrap()
        .get(&crate::encoding::table_key(relation))
        .unwrap()
        .unwrap()
}

fn error_of(session: &mut Session<'_, MemoryStore>, text: &str) -> SqlError {
    session.execute(text, &mut |_| {}).expect_err(text)
}

#[test]
fn views_and_checks_written_in_transact_sql_are_stored_with_its_dialect_code() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut session,
        "CREATE TABLE [dbo].[Orders] ([OrderID] INT NOT NULL PRIMARY KEY, [Qty] INT NOT NULL, CONSTRAINT [CK_Orders_Qty] CHECK ([Qty] > 0))",
    );
    values(
        &mut session,
        "CREATE VIEW [dbo].[vOrders] AS SELECT [OrderID], [Qty] FROM [dbo].[Orders]",
    );
    let table =
        crate::encoding::TableDefinition::decode("orders", &stored(&store, "orders")).unwrap();
    assert_eq!(table.checks.len(), 1);
    assert_eq!(table.checks[0].name, "ck_orders_qty");
    assert_eq!(table.checks[0].dialect, Dialect::SqlServer);
    assert_eq!(table.checks[0].expression, "[Qty] > 0");
    let view = stored(&store, "vorders");
    assert_eq!(view[..2], [8, 4]);
    let decoded = crate::encoding::ViewDefinition::decode("vorders", &view).unwrap();
    assert_eq!(decoded.dialect, Dialect::SqlServer);
    assert_eq!(decoded.query, "SELECT [OrderID], [Qty] FROM [dbo].[Orders]");
}

#[test]
fn transact_sql_checks_and_views_work_from_every_dialect_and_back() {
    let store = MemoryStore::default();
    let mut tsql = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut tsql,
        "CREATE TABLE [dbo].[Stock] ([ID] INT NOT NULL PRIMARY KEY, [Qty] INT NOT NULL CHECK ([Qty] >= 0), [Name] NVARCHAR(20) NULL)",
    );
    values(
        &mut tsql,
        "CREATE VIEW [dbo].[vStock] AS SELECT [ID], ISNULL([Name], N'none') AS [Shown] FROM [dbo].[Stock]",
    );
    let mut postgresql = Session::new(&store);
    values(&mut postgresql, "INSERT INTO stock VALUES (1, 5, NULL)");
    let error = error_of(&mut postgresql, "INSERT INTO stock VALUES (2, -1, 'x')");
    assert_eq!(error.state().code(), "23514");
    assert_eq!(error.vendor(), None);
    assert!(
        error.message().contains("violates check constraint"),
        "{error}"
    );
    assert_eq!(
        values(&mut postgresql, "SELECT shown FROM vstock"),
        vec![vec![Value::Text("none".into())]]
    );
    values(
        &mut postgresql,
        "CREATE TABLE pg_made (id integer PRIMARY KEY, n integer CHECK (n < 10))",
    );
    let error = error_of(&mut tsql, "INSERT INTO pg_made VALUES (1, 10)");
    assert_eq!(error.vendor_code(), Some(547));
    let error = error_of(&mut tsql, "INSERT INTO Stock VALUES (3, -2, NULL)");
    assert_eq!(error.vendor_code(), Some(547));
    assert!(error.message().contains("CHECK constraint"), "{error}");
    let mut mysql = Session::with_dialect(&store, Dialect::MySql);
    assert!(
        mysql
            .execute("INSERT INTO stock VALUES (4, -3, NULL)", &mut |_| {})
            .is_err()
    );
    assert_eq!(
        values(&mut mysql, "SELECT shown FROM vstock"),
        vec![vec![Value::Text("none".into())]]
    );
}

#[test]
fn a_renamed_column_is_written_back_into_transact_sql_checks_in_brackets() {
    let store = MemoryStore::default();
    let mut tsql = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut tsql,
        "CREATE TABLE t (id INT PRIMARY KEY, qty INT NOT NULL, CONSTRAINT ck_qty CHECK (qty > 0 AND [qty] < 100))",
    );
    let mut postgresql = Session::new(&store);
    values(
        &mut postgresql,
        "ALTER TABLE t RENAME COLUMN qty TO \"a]b c\"",
    );
    let table = crate::encoding::TableDefinition::decode("t", &stored(&store, "t")).unwrap();
    assert_eq!(
        table.checks[0].expression,
        "[a]]b c] > 0 AND [a]]b c] < 100"
    );
    assert_eq!(
        error_of(&mut tsql, "INSERT INTO t VALUES (1, 0)").vendor_code(),
        Some(547)
    );
    values(&mut tsql, "INSERT INTO t VALUES (1, 5)");
    let error = error_of(
        &mut postgresql,
        "ALTER TABLE t RENAME COLUMN \"a]b c\" TO \"MixedCase\"",
    );
    assert_eq!(error.state().code(), "0A000");
    assert!(error.message().contains("ck_qty"), "{error}");
    let table = crate::encoding::TableDefinition::decode("t", &stored(&store, "t")).unwrap();
    assert_eq!(table.columns[1].name, "a]b c");
    values(
        &mut postgresql,
        "ALTER TABLE t RENAME COLUMN \"a]b c\" TO amount",
    );
    assert_eq!(
        error_of(&mut tsql, "INSERT INTO t VALUES (2, 100)").vendor_code(),
        Some(547)
    );
    values(&mut postgresql, "ALTER TABLE t RENAME COLUMN id TO \"Id\"");
}

#[test]
fn unnamed_transact_sql_checks_get_names_that_transact_sql_can_drop() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut session,
        "CREATE TABLE [Items] ([ID] INT PRIMARY KEY, [Price] INT CHECK ([Price] >= 0), CHECK ([ID] > 0), CHECK ([Price] < 1000))",
    );
    let table =
        crate::encoding::TableDefinition::decode("items", &stored(&store, "items")).unwrap();
    let names: Vec<&str> = table
        .checks
        .iter()
        .map(|check| check.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["ck__items__price", "ck__items__id", "ck__items__price1"]
    );
    values(
        &mut session,
        "ALTER TABLE Items DROP CONSTRAINT CK__Items__Price",
    );
    values(&mut session, "INSERT INTO Items VALUES (1, -5)");
}

#[test]
fn subqueries_and_aggregates_in_a_transact_sql_check_get_sql_servers_errors() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(&mut session, "CREATE TABLE t (id INT PRIMARY KEY, n INT)");
    for (text, code) in [
        ("CREATE TABLE s (a INT, CHECK (a IN (SELECT 1)))", 1046),
        (
            "ALTER TABLE t ADD CONSTRAINT c1 CHECK ((SELECT 1) = n)",
            1046,
        ),
        ("CREATE TABLE g (a INT CHECK (COUNT(*) > 0))", 175),
        ("ALTER TABLE t ADD CONSTRAINT c2 CHECK (SUM(n) > 0)", 175),
    ] {
        let error = error_of(&mut session, text);
        assert_eq!(error.vendor_code(), Some(code), "{text}");
        assert_eq!(error.severity(), Some(15), "{text}");
    }
}

#[test]
fn an_unnamed_view_column_keeps_each_other_dialects_own_error() {
    let store = MemoryStore::default();
    let mut mysql = Session::with_dialect(&store, Dialect::MySql);
    let error = mysql
        .execute("CREATE VIEW v AS SELECT 1 AS ''", &mut |_| {})
        .expect_err("an empty column name");
    assert_ne!(error.vendor_code(), Some(4511));
    assert!(
        error.message().contains("invalid view column name"),
        "{error}"
    );
}

#[test]
fn create_view_must_be_alone_in_its_batch() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    for text in [
        "SELECT 1 AS a\nCREATE VIEW v AS SELECT 1 AS one",
        "CREATE VIEW v AS SELECT 1 AS one\nSELECT 2 AS two",
        "CREATE TABLE t (id INT PRIMARY KEY);\nALTER VIEW v AS SELECT 1 AS one",
    ] {
        let error = batch(&mut session, text).expect_err(text);
        assert_eq!(error.vendor_code(), Some(111), "{text}");
        assert_eq!(error.severity(), Some(15), "{text}");
    }
    let steps = batch(&mut session, ";\nCREATE VIEW v AS SELECT 1 AS one;").unwrap();
    assert_eq!(codes(&steps), vec![None]);
    let mut postgresql = Session::new(&store);
    assert!(postgresql.execute("SELECT * FROM t", &mut |_| {}).is_err());
    assert_eq!(
        values(&mut postgresql, "SELECT one FROM v"),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn view_options_and_replacement_follow_sql_servers_rules() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(&mut session, "CREATE TABLE t (id INT PRIMARY KEY, n INT)");
    values(&mut session, "INSERT INTO t VALUES (1, 10), (2, 20)");
    values(
        &mut session,
        "CREATE VIEW v WITH SCHEMABINDING, VIEW_METADATA AS SELECT id FROM dbo.t",
    );
    for (text, code) in [
        ("CREATE VIEW w WITH ENCRYPTION AS SELECT id FROM t", 40517),
        ("CREATE OR REPLACE VIEW w AS SELECT id FROM t", 102),
        ("ALTER OR ALTER VIEW v AS SELECT id FROM t", 156),
        ("CREATE VIEW v AS SELECT id FROM t", 2714),
        ("ALTER VIEW t AS SELECT id FROM t", 50000),
        ("CREATE VIEW w AS SELECT id, n + 1 FROM t", 4511),
        ("CREATE VIEW w (a, b, c) AS SELECT *, n + 1 FROM t", 4511),
    ] {
        assert_eq!(
            error_of(&mut session, text).vendor_code(),
            Some(code),
            "{text}"
        );
    }
    values(
        &mut session,
        "CREATE VIEW named (a, b) AS SELECT id, n + 1 FROM t WHERE id = 1",
    );
    assert_eq!(
        values(&mut session, "SELECT b FROM named"),
        vec![vec![Value::Integer(11)]]
    );
    values(&mut session, "ALTER VIEW v AS SELECT n FROM t WHERE id = 2");
    assert_eq!(
        values(&mut session, "SELECT n FROM v"),
        vec![vec![Value::Integer(20)]]
    );
    values(
        &mut session,
        "CREATE OR ALTER VIEW v AS SELECT TOP 1 n FROM t ORDER BY n",
    );
    assert_eq!(
        values(&mut session, "SELECT n FROM v"),
        vec![vec![Value::Integer(10)]]
    );
    values(&mut session, "CREATE OR ALTER VIEW w AS SELECT * FROM t");
    values(&mut session, "DROP TABLE t");
    assert_eq!(
        error_of(&mut session, "SELECT * FROM w").vendor_code(),
        Some(208)
    );
    values(&mut session, "CREATE TABLE t (id INT PRIMARY KEY, n INT)");
    values(&mut session, "INSERT INTO t VALUES (3, 30)");
    assert_eq!(
        values(&mut session, "SELECT * FROM w"),
        vec![vec![Value::Integer(3), Value::Integer(30)]]
    );
    values(&mut session, "DROP TABLE t");
    values(&mut session, "CREATE TABLE t (n INT)");
    assert_eq!(
        error_of(&mut session, "SELECT * FROM w").vendor_code(),
        Some(4413)
    );
}

#[test]
fn stored_transact_sql_texts_keep_their_meaning() {
    const CHECKS: [(&str, &[(&str, bool)]); 4] = [
        (
            "[Qty] BETWEEN 1 AND 99",
            &[
                ("1, 1, N'a', '2024-01-01'", true),
                ("2, 100, N'a', '2024-01-01'", false),
            ],
        ),
        (
            "ISNULL([Code], N'x') <> N''",
            &[
                ("1, 1, NULL, '2024-01-01'", true),
                ("2, 1, N'', '2024-01-01'", false),
            ],
        ),
        (
            "[Day] >= CONVERT(date, '20240101', 112) AND DATEDIFF(day, '20240101', [Day]) < 31",
            &[
                ("1, 1, N'a', '2024-01-31'", true),
                ("2, 1, N'a', '2024-02-01'", false),
            ],
        ),
        (
            "LEN([Code]) <= 3 OR [Code] IS NULL",
            &[
                ("1, 1, N'abc', '2024-01-01'", true),
                ("2, 1, N'abcd', '2024-01-01'", false),
            ],
        ),
    ];
    const VIEWS: [(&str, &[&[i64]]); 3] = [
        (
            "SELECT [ID], [Qty] FROM [dbo].[Base] WHERE [Qty] > 1",
            &[&[2, 5], &[3, 9]],
        ),
        (
            "SELECT TOP (2) [ID] FROM [dbo].[Base] ORDER BY [Qty] DESC",
            &[&[3], &[2]],
        ),
        (
            "SELECT COUNT(*) AS [N], SUM([Qty]) AS [Total] FROM [dbo].[Base] WHERE [Code] + N'!' = N'b!'",
            &[&[2, 14]],
        ),
    ];
    for (index, (check, rows)) in CHECKS.iter().enumerate() {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, Dialect::SqlServer);
        values(
            &mut session,
            &format!(
                "CREATE TABLE [dbo].[Corpus] ([ID] INT PRIMARY KEY, [Qty] INT NOT NULL, [Code] NVARCHAR(10) NULL, [Day] DATE NOT NULL, CONSTRAINT [CK_Corpus_{index}] CHECK ({check}))"
            ),
        );
        let table =
            crate::encoding::TableDefinition::decode("corpus", &stored(&store, "corpus")).unwrap();
        assert_eq!(table.checks[0].expression, *check);
        for (row, accepted) in *rows {
            let outcome =
                session.execute(&format!("INSERT INTO Corpus VALUES ({row})"), &mut |_| {});
            assert_eq!(
                outcome.is_ok(),
                *accepted,
                "{check} with {row}: {outcome:?}"
            );
        }
    }
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut session,
        "CREATE TABLE [dbo].[Base] ([ID] INT PRIMARY KEY, [Qty] INT NOT NULL, [Code] NVARCHAR(10) NULL)",
    );
    values(
        &mut session,
        "INSERT INTO Base VALUES (1, 1, N'a'), (2, 5, N'b'), (3, 9, N'b')",
    );
    for (index, (query, expected)) in VIEWS.iter().enumerate() {
        values(
            &mut session,
            &format!("CREATE VIEW [dbo].[vCorpus{index}] AS {query}"),
        );
        let view = crate::encoding::ViewDefinition::decode(
            &format!("vcorpus{index}"),
            &stored(&store, &format!("vcorpus{index}")),
        )
        .unwrap();
        assert_eq!(view.query, *query);
        let found = values(&mut session, &format!("SELECT * FROM vCorpus{index}"));
        let found: Vec<Vec<i64>> = found
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| value.as_integer().unwrap())
                    .collect()
            })
            .collect();
        let expected: Vec<Vec<i64>> = expected.iter().map(|row| row.to_vec()).collect();
        assert_eq!(found, expected, "{query}");
    }
}

#[test]
fn stored_transact_sql_defaults_keep_their_text_and_meaning() {
    const DEFAULTS: [(&str, &str, &str, &str); 6] = [
        ("INT", "(abs(-2)*(3))", "abs(-2)*(3)", "6"),
        ("NVARCHAR(10)", "(CONCAT(N'a',N'b'))", "CONCAT(N'a',N'b')", "ab"),
        (
            "INT",
            "(datepart(year,CONVERT([date],'20240115',(112))))",
            "datepart(year,CONVERT([date],'20240115',(112)))",
            "2024",
        ),
        ("INT", "(isnull(NULL,(7)))", "isnull(NULL,(7))", "7"),
        ("INT", "((len(N'four')+(1)))", "len(N'four')+(1)", "5"),
        (
            "NVARCHAR(10)",
            "(CONVERT([nvarchar](10),dateadd(day,(1),CONVERT([date],'20240131',(112))),(23)))",
            "CONVERT([nvarchar](10),dateadd(day,(1),CONVERT([date],'20240131',(112))),(23))",
            "2024-02-01",
        ),
    ];
    for (index, (column_type, written, kept, value)) in DEFAULTS.iter().enumerate() {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, Dialect::SqlServer);
        values(
            &mut session,
            &format!(
                "CREATE TABLE [dbo].[Defaults] ([ID] INT PRIMARY KEY, [V] {column_type} NULL CONSTRAINT [DF_Defaults_{index}] DEFAULT {written})"
            ),
        );
        let table = crate::encoding::TableDefinition::decode("defaults", &stored(&store, "defaults"))
            .unwrap();
        assert_eq!(table.columns[1].default.as_deref(), Some(*kept), "{written}");
        assert_eq!(table.default_dialect(1), Dialect::SqlServer, "{written}");
        values(&mut session, "INSERT INTO Defaults (ID) VALUES (1)");
        let rows = values(&mut session, "SELECT V FROM Defaults");
        assert_eq!(rows[0][0].to_text().as_deref(), Some(*value), "{written}");
    }
}

#[test]
fn hostile_views_and_checks_in_transact_sql_return_errors_and_never_panic() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(&mut session, "CREATE TABLE t (id INT PRIMARY KEY, n INT)");
    let deep = format!("{}1{}", "(".repeat(20_000), ")".repeat(20_000));
    let long = format!("SELECT id FROM t WHERE n <> {}", "1+".repeat(600_000));
    for text in [
        format!("CREATE VIEW v1 AS SELECT {deep} AS x"),
        format!("CREATE TABLE c1 (id INT CHECK (id > {deep}))"),
        format!("ALTER TABLE t ADD CONSTRAINT c2 CHECK (n > {deep})"),
        format!("CREATE VIEW v2 AS {long}1"),
        "CREATE VIEW v3 AS SELECT (SELECT COUNT(*) FROM t) AS x ORDER BY x".to_owned(),
        "CREATE VIEW v4 AS".to_owned(),
        "CREATE VIEW [v5 AS SELECT 1 AS one".to_owned(),
        "ALTER TABLE t ADD CONSTRAINT c3 CHECK ((SELECT 1) = 1)".to_owned(),
        "ALTER TABLE t ADD CONSTRAINT c4 CHECK (COUNT(*) > 0)".to_owned(),
        "CREATE VIEW v6 WITH AS SELECT 1 AS one".to_owned(),
        format!("CREATE TABLE d1 (id INT PRIMARY KEY, n INT DEFAULT (abs({deep})))"),
        format!("ALTER TABLE t ADD DEFAULT (abs({deep})) FOR n"),
        "CREATE TABLE d2 (id INT PRIMARY KEY, n INT DEFAULT (@local + 1))".to_owned(),
        "CREATE TABLE d3 (id INT PRIMARY KEY, n INT DEFAULT ((1, 2)))".to_owned(),
        format!(
            "CREATE TABLE d4 (id INT IDENTITY(1, {}) PRIMARY KEY)",
            "9".repeat(400)
        ),
        "CREATE TABLE d5 (id INT IDENTITY(-, 1) PRIMARY KEY)".to_owned(),
        "CREATE TABLE d6 (id INT IDENTITY(1) PRIMARY KEY)".to_owned(),
    ] {
        let error = session.execute(&text, &mut |_| {}).expect_err("refused");
        assert!(error.vendor_code().is_some(), "{error}");
    }
}

fn parameterized(
    session: &mut Session<'_, MemoryStore>,
    text: &str,
    parameters: &[crate::NamedParameter],
) -> Result<Vec<BatchStep>, SqlError> {
    let mut steps = Vec::new();
    session.execute_parameterized_batch(text, parameters, &mut |step| {
        steps.push(step);
        true
    })?;
    Ok(steps)
}

fn named(name: &str, declared_type: Option<crate::SqlType>, value: Value) -> crate::NamedParameter {
    crate::NamedParameter {
        name: name.to_owned(),
        declared_type,
        value,
    }
}

fn rows_of(steps: &[BatchStep]) -> Vec<Vec<Value>> {
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

#[test]
fn named_parameters_bind_typed_values_converted_to_their_declared_types() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut session,
        "CREATE TABLE items (id INT PRIMARY KEY, name NVARCHAR(40), price DECIMAL(10,2))",
    );
    let price = crate::SqlType::Numeric(Some(crate::NumericModifier {
        precision: 10,
        scale: 2,
    }));
    let steps = parameterized(
        &mut session,
        "INSERT INTO items VALUES (@id, @Name, @price)",
        &[
            named("@ID", Some(crate::SqlType::Integer), Value::Integer(7)),
            named(
                "@name",
                Some(crate::SqlType::Varchar(Some(40))),
                Value::Text("egg".into()),
            ),
            named(
                "@price",
                Some(price),
                Value::Numeric(crate::Numeric::parse("12.5").unwrap()),
            ),
        ],
    )
    .unwrap();
    assert!(steps[0].outcome.as_ref().unwrap().tag.starts_with("INSERT"));
    let found = parameterized(
        &mut session,
        "SELECT name, CAST(price AS varchar(20)) AS price FROM items WHERE id = @p1",
        &[named(
            "@p1",
            Some(crate::SqlType::Integer),
            Value::Text("7".into()),
        )],
    )
    .unwrap();
    assert_eq!(
        rows_of(&found),
        vec![vec![Value::Text("egg".into()), Value::Text("12.50".into())]]
    );
    let nothing = parameterized(
        &mut session,
        "SELECT COUNT(*) AS n FROM items WHERE name = @p1",
        &[named("@p1", Some(crate::SqlType::Text), Value::Null)],
    )
    .unwrap();
    assert_eq!(rows_of(&nothing), vec![vec![Value::Integer(0)]]);
    let error = parameterized(&mut session, "SELECT @missing", &[]).expect_err("undeclared");
    assert_eq!(error.vendor_code(), Some(137));
    let error = batch(&mut session, "SELECT @p1").expect_err("no parameters");
    assert_eq!(error.vendor_code(), Some(137));
    session
        .check_parameterized_batch("SELECT @p1", &[named("@p1", None, Value::Null)])
        .unwrap();
    let error = session
        .check_parameterized_batch(
            "SELECT * FORM t WHERE id = @p1",
            &[named("@p1", None, Value::Null)],
        )
        .expect_err("a syntax error");
    assert_eq!(error.vendor_code(), Some(102));
}

#[test]
fn parameter_declarations_read_names_types_and_output_markers() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    let declared = session
        .parse_parameter_declarations(
            "@P0 int,@p1 nvarchar(4000) OUTPUT, @p2 decimal(5,2) out, @p3 nvarchar(max),@p4 datetime2",
        )
        .unwrap();
    let shown: Vec<(String, crate::SqlType, bool)> = declared
        .into_iter()
        .map(|declaration| (declaration.name, declaration.sql_type, declaration.output))
        .collect();
    assert_eq!(
        shown,
        vec![
            ("@p0".into(), crate::SqlType::Integer, false),
            ("@p1".into(), crate::SqlType::Varchar(Some(4000)), true),
            (
                "@p2".into(),
                crate::SqlType::Numeric(Some(crate::NumericModifier {
                    precision: 5,
                    scale: 2
                })),
                true
            ),
            ("@p3".into(), crate::SqlType::Text, false),
            ("@p4".into(), crate::SqlType::Timestamp(Some(6)), false),
        ]
    );
    assert!(session.parse_parameter_declarations("").unwrap().is_empty());
    for (text, code) in [
        ("@p1 foo", 2715),
        ("@p1 int, @P1 int", 134),
        ("p1 int", 102),
        ("@p1 int @p2 int", 102),
        ("@@p1 int", 102),
    ] {
        let error = session.parse_parameter_declarations(text).expect_err(text);
        assert_eq!(error.vendor_code(), Some(code), "{text}");
    }
}

#[test]
fn implicit_transactions_keep_changes_open_until_commit_or_rollback() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(&mut session, "CREATE TABLE t (id INT PRIMARY KEY)");
    let steps = batch(
        &mut session,
        "set implicit_transactions on INSERT INTO t VALUES (1) SELECT @@TRANCOUNT AS n",
    )
    .unwrap();
    assert!(!steps[0].in_transaction);
    assert!(steps[1].in_transaction);
    assert_eq!(rows_of(&steps), vec![vec![Value::Integer(1)]]);
    let steps = batch(&mut session, "IF @@TRANCOUNT > 0 ROLLBACK TRAN").unwrap();
    assert_eq!(steps[0].outcome.as_ref().unwrap().tag, "ROLLBACK");
    assert!(!steps[0].in_transaction);
    let steps = batch(
        &mut session,
        "CREATE TABLE u (id INT PRIMARY KEY) INSERT INTO t VALUES (2) IF @@TRANCOUNT > 0 COMMIT TRAN",
    )
    .unwrap();
    assert!(steps[0].in_transaction);
    assert!(!steps[2].in_transaction);
    let steps = batch(
        &mut session,
        "set implicit_transactions off IF @@TRANCOUNT > 0 COMMIT TRAN INSERT INTO t VALUES (3)",
    )
    .unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[1].outcome.as_ref().unwrap().tag, "INSERT 0 1");
    assert!(steps.iter().all(|step| !step.in_transaction));
    assert_eq!(
        values(&mut session, "SELECT id FROM t ORDER BY id"),
        vec![vec![Value::Integer(2)], vec![Value::Integer(3)]]
    );
    assert_eq!(
        values(&mut session, "SELECT COUNT(*) AS n FROM u"),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn if_and_else_choose_their_statements_by_any_condition() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(&mut session, "CREATE TABLE t (id INT PRIMARY KEY)");
    let steps = batch(&mut session, "IF 1 = 0 SELECT 1 AS n ELSE SELECT 2 AS n").unwrap();
    assert_eq!(rows_of(&steps), vec![vec![Value::Integer(2)]]);
    let steps = batch(
        &mut session,
        "IF 'a' = 'a' SELECT 3 AS n; ELSE SELECT 4 AS n",
    )
    .unwrap();
    assert_eq!(rows_of(&steps), vec![vec![Value::Integer(3)]]);
    let steps = batch(&mut session, "IF NULL = 1 SELECT 5 AS n SELECT 6 AS n").unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(rows_of(&steps), vec![vec![Value::Integer(6)]]);
    let steps = batch(
        &mut session,
        "IF NOT EXISTS (SELECT 1 FROM t) INSERT INTO t VALUES (1) IF EXISTS (SELECT 1 FROM t WHERE id = 1) SELECT 7 AS n",
    )
    .unwrap();
    assert_eq!(rows_of(&steps), vec![vec![Value::Integer(7)]]);
    let steps = batch(&mut session, "IF 1 = 1 BEGIN SELECT 8 AS n SELECT 9 AS n END").unwrap();
    assert_eq!(steps.len(), 2);
    let steps = batch(&mut session, "IF id = 1 SELECT 1").unwrap();
    assert_eq!(codes(&steps), vec![Some(207)]);
}

#[test]
fn a_connection_reset_rolls_back_and_restores_the_options_of_a_new_login() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(&mut session, "CREATE TABLE t (id INT PRIMARY KEY)");
    let steps = batch(
        &mut session,
        "SET NOCOUNT ON SET IMPLICIT_TRANSACTIONS ON BEGIN TRAN INSERT INTO t VALUES (1)",
    )
    .unwrap();
    assert!(steps.last().unwrap().no_count);
    session.reset_connection(true).unwrap();
    assert!(session.in_block());
    session.reset_connection(false).unwrap();
    assert!(!session.in_block());
    let steps = batch(
        &mut session,
        "INSERT INTO t VALUES (2) SELECT @@TRANCOUNT AS n",
    )
    .unwrap();
    assert!(!steps[0].no_count);
    assert!(!steps[0].in_transaction);
    assert_eq!(rows_of(&steps), vec![vec![Value::Integer(0)]]);
    assert_eq!(
        values(&mut session, "SELECT id FROM t"),
        vec![vec![Value::Integer(2)]]
    );
}

fn column_types(session: &mut Session<'_, MemoryStore>, text: &str) -> Vec<Option<crate::SqlType>> {
    let mut last = None;
    session
        .execute(text, &mut |result: QueryResult| last = result.columns)
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    last.unwrap_or_default()
        .into_iter()
        .map(|column| column.sql_type)
        .collect()
}

fn decimal(precision: u16, scale: i16) -> Option<crate::SqlType> {
    Some(crate::SqlType::Numeric(Some(crate::NumericModifier {
        precision,
        scale,
    })))
}

#[test]
fn sums_averages_and_quotients_take_sql_servers_result_types() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut session,
        "CREATE TABLE s (id INT PRIMARY KEY, qty INT NOT NULL, price DECIMAL(10,2) NOT NULL, big BIGINT NOT NULL)",
    );
    values(
        &mut session,
        "INSERT INTO s VALUES (1, 2147483647, 1.50, 1), (2, 1, 2.25, 2)",
    );
    assert_eq!(
        column_types(
            &mut session,
            "SELECT SUM(price) AS a, AVG(price) AS b, SUM(big) AS c, AVG(qty) AS d, COUNT_BIG(*) AS e FROM s"
        ),
        vec![
            decimal(38, 2),
            decimal(38, 6),
            Some(crate::SqlType::BigInt),
            Some(crate::SqlType::Integer),
            Some(crate::SqlType::BigInt),
        ]
    );
    assert_eq!(
        column_types(&mut session, "SELECT price / 3 AS q FROM s"),
        vec![decimal(21, 13)]
    );
    assert_eq!(
        values(&mut session, "SELECT price / 3 AS q FROM s WHERE id = 2"),
        vec![vec![Value::Numeric(
            crate::Numeric::parse("0.7500000000000").unwrap()
        )]]
    );
    let error = session
        .execute("SELECT SUM(qty) AS total FROM s", &mut |_| {})
        .expect_err("an int overflow");
    assert_eq!(error.vendor_code(), Some(8115));
    let mut postgresql = Session::new(&store);
    assert_eq!(
        values(&mut postgresql, "SELECT sum(qty) AS total FROM s"),
        vec![vec![Value::BigInt(2_147_483_648)]]
    );
}

#[test]
fn at_at_identity_follows_the_last_identity_value() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    values(
        &mut session,
        "CREATE TABLE i (id INT IDENTITY(1,1) PRIMARY KEY, note NVARCHAR(10))",
    );
    values(&mut session, "INSERT INTO i (note) VALUES (N'a'), (N'b')");
    assert_eq!(
        values(&mut session, "SELECT @@IDENTITY AS last_id"),
        vec![vec![Value::Numeric(crate::Numeric::parse("2").unwrap())]]
    );
}

#[test]
fn string_split_and_string_agg_go_both_ways_through_a_variable() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    let steps = batch(
        &mut session,
        "DECLARE @list NVARCHAR(100) = N'c,a,b' SELECT STRING_AGG(value, '+') WITHIN GROUP (ORDER BY value) AS joined FROM STRING_SPLIT(@list, ',')",
    )
    .unwrap();
    assert_eq!(rows_of(&steps), vec![vec![Value::Text("a+b+c".into())]]);
}

#[test]
fn hostile_arguments_to_the_common_functions_return_errors_or_values_and_never_panic() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    let long = "9".repeat(5_000);
    let texts = [
        "SELECT REPLICATE(CAST('ab' AS varchar(max)), 2147483647) AS r".to_owned(),
        "SELECT SPACE(2147483647) AS s".to_owned(),
        "SELECT SUBSTRING('abc', -2147483648, 2147483647) AS s".to_owned(),
        "SELECT STUFF('abc', 2147483647, 2147483647, 'x') AS s".to_owned(),
        "SELECT LEFT('abc', 9223372036854775807) AS s".to_owned(),
        "SELECT ROUND(1.5, -2147483648) AS r, ROUND(1e308, 2147483647) AS f".to_owned(),
        "SELECT CEILING(CAST(1e308 AS float)) AS c".to_owned(),
        "SELECT EOMONTH('2020-01-01', 2147483647) AS e".to_owned(),
        "SELECT DATEDIFF_BIG(nanosecond, '0001-01-01', '9999-12-31') AS d".to_owned(),
        "SELECT CHOOSE(2147483647, 'a') AS c".to_owned(),
        format!("SELECT FORMAT(1, '{long}') AS f"),
        "SELECT FORMAT(1, 'N99') AS f, FORMAT(1.5, 'E99') AS e, FORMAT(CAST('2020-01-01' AS date), 'fffffffff') AS d".to_owned(),
        "SELECT FORMAT(-9223372036854775808, 'X') AS x, FORMAT(1, '\\') AS slash, FORMAT(1, ';;;') AS sections".to_owned(),
        "SELECT CONCAT_WS(NULL, NULL, NULL) AS c, DATALENGTH(REPLICATE('a', 9000)) AS d".to_owned(),
        "SELECT TRY_CONVERT(varbinary(10), 'zz', 2) AS b, CONVERT(varchar(10), CAST(1e308 AS float), 2) AS f".to_owned(),
        "SELECT value FROM STRING_SPLIT(REPLICATE(',', 8000), ',')".to_owned(),
        "SELECT DATENAME(tzoffset, SYSDATETIMEOFFSET()) AS z, DATEPART(nanosecond, '9999-12-31 23:59:59.9999999') AS n".to_owned(),
    ];
    for text in &texts {
        let _ = session.execute(text, &mut |_| {});
    }
    assert_eq!(
        values(&mut session, "SELECT 1 AS n"),
        vec![vec![Value::Integer(1)]]
    );
}

fn texts(rows: Vec<Vec<Value>>) -> Vec<Vec<String>> {
    rows.into_iter()
        .map(|row| {
            row.iter()
                .map(|value| value.to_text().unwrap_or_else(|| "NULL".to_owned()))
                .collect()
        })
        .collect()
}

#[test]
fn sequences_number_with_sql_server_defaults_cycles_and_next_value_for() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    session
        .execute(
            "CREATE SEQUENCE dbo.counter AS int START WITH 5 INCREMENT BY 5 MINVALUE 5 MAXVALUE 15 CYCLE NO CACHE",
            &mut |_| {},
        )
        .unwrap();
    let mut drawn = Vec::new();
    for _ in 0..4 {
        drawn.extend(texts(values(
            &mut session,
            "SELECT NEXT VALUE FOR dbo.counter AS n",
        )));
    }
    assert_eq!(drawn, vec![vec!["5"], vec!["10"], vec!["15"], vec!["5"]]);
    session
        .execute(
            "CREATE SEQUENCE [tiny] AS [tinyint]; CREATE SEQUENCE big; CREATE SEQUENCE down AS smallint INCREMENT BY -1",
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(
        texts(values(
            &mut session,
            "SELECT NEXT VALUE FOR tiny AS t, NEXT VALUE FOR big AS b, NEXT VALUE FOR down AS d"
        )),
        vec![vec![
            "0".to_owned(),
            i64::MIN.to_string(),
            i16::MAX.to_string()
        ]]
    );
    session
        .execute("ALTER SEQUENCE tiny RESTART WITH 7", &mut |_| {})
        .unwrap();
    assert_eq!(
        texts(values(&mut session, "SELECT NEXT VALUE FOR tiny AS t")),
        vec![vec!["7"]]
    );
    session
        .execute(
            "CREATE TABLE o (id BIGINT PRIMARY KEY DEFAULT (NEXT VALUE FOR big), n INT); INSERT INTO o (n) VALUES (1)",
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(
        texts(values(&mut session, "SELECT id FROM o")),
        vec![vec![(i64::MIN + 1).to_string()]]
    );
    let refused = session
        .execute("CREATE SEQUENCE wide AS decimal(20, 0)", &mut |_| {})
        .unwrap_err();
    assert_eq!(refused.vendor_code(), Some(11702));
    session
        .execute("DROP SEQUENCE IF EXISTS dbo.counter", &mut |_| {})
        .unwrap();
    assert!(
        session
            .execute("SELECT NEXT VALUE FOR dbo.counter AS n", &mut |_| {})
            .is_err()
    );
    assert_eq!(session.close(), Ok(false));
}

#[test]
fn ms_description_extended_properties_are_comments_shown_in_sys_extended_properties() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    session
        .execute(
            "CREATE TABLE people (id INT PRIMARY KEY, name NVARCHAR(20)); \
             EXEC sys.sp_addextendedproperty @name=N'MS_Description', @value=N'Our people', @level0type=N'SCHEMA', @level0name=N'dbo', @level1type=N'TABLE', @level1name=N'People'; \
             EXEC sp_addextendedproperty N'MS_Description', N'Full name', N'SCHEMA', N'dbo', N'TABLE', N'People', N'COLUMN', N'Name'",
            &mut |_| {},
        )
        .unwrap();
    let listed = "SELECT minor_id, CAST(value AS nvarchar(50)) AS v FROM sys.extended_properties ORDER BY minor_id";
    assert_eq!(
        texts(values(&mut session, listed)),
        vec![vec!["0", "Our people"], vec!["2", "Full name"]]
    );
    session
        .execute(
            "EXEC sp_updateextendedproperty @name=N'MS_Description', @value=N'Name of the person', @level0type=N'SCHEMA', @level0name=N'dbo', @level1type=N'TABLE', @level1name=N'People', @level2type=N'COLUMN', @level2name=N'Name'; \
             EXEC sp_dropextendedproperty @name=N'MS_Description', @level0type=N'SCHEMA', @level0name=N'dbo', @level1type=N'TABLE', @level1name=N'People'",
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(
        texts(values(&mut session, listed)),
        vec![vec!["2", "Name of the person"]]
    );
    for text in [
        "EXEC sp_addextendedproperty @name=N'Caption', @value=N'x', @level0type=N'SCHEMA', @level0name=N'dbo', @level1type=N'TABLE', @level1name=N'People'",
        "EXEC sp_addextendedproperty @name=N'MS_Description', @value=N'x'",
        "EXEC sp_addextendedproperty @name=N'MS_Description', @value=N'x', @level0type=N'USER', @level0name=N'dbo'",
        "EXEC sp_addextendedproperty @name=N'MS_Description', @value=N'x', @level0type=N'SCHEMA', @level0name=N'dbo', @level1type=N'PROCEDURE', @level1name=N'p'",
    ] {
        let error = session.execute(text, &mut |_| {}).unwrap_err();
        assert_eq!(error.vendor_code(), Some(40517), "{text}: {error}");
    }
    let unknown = session
        .execute(
            "EXEC sp_addextendedproperty @name=N'MS_Description', @nothing=N'x'",
            &mut |_| {},
        )
        .unwrap_err();
    assert_eq!(unknown.vendor_code(), Some(8145));
    assert_eq!(session.close(), Ok(false));
}

#[test]
fn hostile_sequences_descriptions_and_xact_abort_return_errors_and_never_panic() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    let long = "9".repeat(5_000);
    let texts = [
        "CREATE SEQUENCE s AS decimal(18, 0) MINVALUE -999999999999999999 MAXVALUE 999999999999999999".to_owned(),
        "CREATE SEQUENCE s2 START WITH 99999999999999999999".to_owned(),
        format!("CREATE SEQUENCE s3 INCREMENT BY {long}"),
        "CREATE SEQUENCE s4 AS tinyint START WITH 300".to_owned(),
        "CREATE SEQUENCE s5 INCREMENT BY 0".to_owned(),
        "CREATE SEQUENCE s6 MINVALUE 10 MAXVALUE 1".to_owned(),
        "CREATE SEQUENCE s7 AS sql_variant".to_owned(),
        "CREATE SEQUENCE s8 MINVALUE - MAXVALUE".to_owned(),
        "CREATE SEQUENCE s9 AS bigint INCREMENT BY -9223372036854775808 MINVALUE -9223372036854775808 MAXVALUE 9223372036854775807".to_owned(),
        "SELECT NEXT VALUE FOR s9 AS a, NEXT VALUE FOR s9 AS b".to_owned(),
        "SELECT NEXT VALUE FOR".to_owned(),
        "SELECT NEXT VALUE FOR nowhere AS n".to_owned(),
        "SELECT NEXT VALUE FOR s OVER (ORDER BY 1) AS n".to_owned(),
        "ALTER SEQUENCE nowhere RESTART".to_owned(),
        "DROP SEQUENCE nowhere".to_owned(),
        "EXEC sp_addextendedproperty".to_owned(),
        "EXEC sp_addextendedproperty NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL".to_owned(),
        "EXEC sp_addextendedproperty N'MS_Description', N'x', N'SCHEMA', N'dbo', N'TABLE', N'nowhere', N'COLUMN', N'c', N'extra'".to_owned(),
        "EXEC sp_dropextendedproperty N'MS_Description', N'SCHEMA', N'nowhere'".to_owned(),
        format!("EXEC sp_addextendedproperty N'MS_Description', N'{long}', N'SCHEMA', N'{long}'"),
        "EXEC sp_addextendedproperty @name = 1 / 0".to_owned(),
        "SET XACT_ABORT".to_owned(),
        "SET XACT_ABORT MAYBE".to_owned(),
    ];
    for text in &texts {
        let _ = session.execute(text, &mut |_| {});
        let _ = session.execute_batch(text, &mut |_| true);
    }
    assert_eq!(
        values(&mut session, "SELECT 1 AS n"),
        vec![vec![Value::Integer(1)]]
    );
}
