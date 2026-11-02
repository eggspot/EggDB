use crate::memory_store::MemoryStore;
use crate::{
    BatchStep, BcpCharacterReader, BcpFileFormat, BcpFileReader, BcpFormat, BcpTerminators,
    BcpUnicodeInput, BulkRows, Dialect, QueryResult, Session, SqlError, SqlResult, Value,
    write_bcp_table,
};

struct Rows(Vec<Vec<Value>>);

impl BulkRows for Rows {
    fn next_row(&mut self, _columns: usize, row: &mut Vec<Value>) -> SqlResult<bool> {
        row.clear();
        if self.0.is_empty() {
            return Ok(false);
        }
        row.extend(self.0.remove(0));
        Ok(true)
    }
}

fn batch(session: &mut Session<'_>, text: &str) -> Vec<BatchStep> {
    let mut steps = Vec::new();
    session
        .execute_batch(text, &mut |step| {
            steps.push(step);
            true
        })
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    steps
}

fn load(session: &mut Session<'_>, rows: &mut dyn BulkRows) -> Vec<BatchStep> {
    let mut steps = Vec::new();
    session
        .execute_bulk_load(rows, &mut |step| {
            steps.push(step);
            true
        })
        .unwrap_or_else(|error| panic!("bulk load: {error}"));
    steps
}

fn texts(session: &mut Session<'_>, text: &str) -> Vec<Vec<String>> {
    let mut last = Vec::new();
    session
        .execute(text, &mut |result: QueryResult| {
            last = result
                .rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|value| value.to_text().unwrap_or_else(|| "NULL".to_owned()))
                        .collect()
                })
                .collect();
        })
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    last
}

fn code(step: &BatchStep) -> Option<u16> {
    step.outcome.as_ref().err().and_then(SqlError::vendor_code)
}

fn tag(step: &BatchStep) -> &str {
    step.outcome
        .as_ref()
        .map(|result| result.tag.as_str())
        .unwrap_or("error")
}

fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

fn session_with_items(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::with_dialect(store, Dialect::SqlServer);
    batch(
        &mut session,
        "CREATE TABLE items (id INT IDENTITY(1,1) PRIMARY KEY, name NVARCHAR(20) NOT NULL, price DECIMAL(10,2) DEFAULT 1.50, added DATETIME2(3))",
    );
    session
}

#[test]
fn insert_bulk_waits_for_its_rows_and_loads_them_with_defaults_and_numbered_identity() {
    let store = MemoryStore::default();
    let mut session = session_with_items(&store);
    let steps = batch(
        &mut session,
        "insert bulk [dbo].[items] ([name] NVarChar(20) COLLATE Latin1_General_100_BIN2, [price] Decimal(10,2)) with (TABLOCK, CHECK_CONSTRAINTS, FIRE_TRIGGERS)",
    );
    assert_eq!(tag(&steps[0]), "INSERT BULK");
    assert!(session.bulk_load_pending());
    let steps = load(
        &mut session,
        &mut Rows(vec![
            vec![text("bread"), Value::Text("12.5".into())],
            vec![text("egg"), Value::Null],
        ]),
    );
    assert_eq!(tag(&steps[0]), "INSERT 0 2");
    assert!(!session.bulk_load_pending());
    assert_eq!(
        texts(
            &mut session,
            "SELECT id, name, price FROM items ORDER BY id"
        ),
        vec![
            vec!["1".to_owned(), "bread".to_owned(), "12.50".to_owned()],
            vec!["2".to_owned(), "egg".to_owned(), "1.50".to_owned()],
        ]
    );
    batch(
        &mut session,
        "INSERT BULK items (id Int, name NVarChar(20), price Decimal(10,2)) WITH (KEEP_NULLS, ORDER(id ASC), ROWS_PER_BATCH = 10)",
    );
    let steps = load(
        &mut session,
        &mut Rows(vec![vec![Value::Integer(100), text("pie"), Value::Null]]),
    );
    assert_eq!(tag(&steps[0]), "INSERT 0 1");
    assert_eq!(
        texts(&mut session, "SELECT name, price FROM items WHERE id = 100"),
        vec![vec!["pie".to_owned(), "NULL".to_owned()]]
    );
}

#[test]
fn a_failing_bulk_load_loads_nothing_and_rows_need_a_waiting_insert_bulk() {
    let store = MemoryStore::default();
    let mut session = session_with_items(&store);
    batch(&mut session, "INSERT BULK items (name NVarChar(20))");
    let steps = load(
        &mut session,
        &mut Rows(vec![vec![text("ok")], vec![Value::Null]]),
    );
    assert_eq!(code(&steps[0]), Some(515));
    assert_eq!(
        texts(&mut session, "SELECT count(*) AS n FROM items"),
        vec![vec!["0".to_owned()]]
    );
    batch(
        &mut session,
        "INSERT BULK items (name NVarChar(20), price Decimal(10,2))",
    );
    let steps = load(&mut session, &mut Rows(vec![vec![text("short")]]));
    assert_eq!(code(&steps[0]), Some(4815));
    batch(&mut session, "INSERT BULK items (name NVarChar(20))");
    batch(&mut session, "SELECT 1 AS one");
    assert!(!session.bulk_load_pending());
    let refused = session.execute_bulk_load(&mut Rows(Vec::new()), &mut |_| true);
    assert!(refused.is_err());
    let mut steps = Vec::new();
    session
        .execute_batch("INSERT BULK nothing_here", &mut |step| {
            steps.push(step);
            true
        })
        .unwrap();
    assert_eq!(code(&steps[0]), Some(208));
    let steps = batch(&mut session, "INSERT BULK items ([nope] Int)");
    assert_eq!(code(&steps[0]), Some(207));
    assert!(!session.bulk_load_pending());
    let error = session
        .execute_batch("INSERT BULK items WITH (SORTED)", &mut |_| true)
        .expect_err("an unknown option");
    assert_eq!(error.vendor_code(), Some(195));
}

#[test]
fn bcp_character_files_load_through_sql_servers_conversions() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    batch(
        &mut session,
        "CREATE TABLE people (id INT PRIMARY KEY, name NVARCHAR(20), score DECIMAL(5,1), seen BIT, joined DATETIME2(3) DEFAULT '2024-01-01')",
    );
    session.execute("INSERT BULK people", &mut |_| {}).unwrap();
    let mut data: &[u8] =
        b"1\tada\t9.5\t1\t2024-05-06 07:08:09.123\r\n2\t\t\t0\t\n3\t\0\t-1.0\t0\t2024-02-03\n";
    let mut reader = BcpCharacterReader::new(&mut data, BcpTerminators::default());
    let steps = load(&mut session, &mut reader);
    assert_eq!(tag(&steps[0]), "INSERT 0 3");
    let expected: Vec<Vec<String>> = [
        ["1", "ada", "9.5", "1"],
        ["2", "NULL", "NULL", "0"],
        ["3", "", "-1.0", "0"],
    ]
    .iter()
    .map(|row| row.iter().map(|cell| (*cell).to_owned()).collect())
    .collect();
    assert_eq!(
        texts(
            &mut session,
            "SELECT id, name, score, CAST(seen AS INT) AS seen FROM people ORDER BY id"
        ),
        expected
    );
    assert_eq!(
        texts(
            &mut session,
            "SELECT count(*) AS n FROM people WHERE joined IS NOT NULL"
        ),
        vec![vec!["3".to_owned()]]
    );
}

#[test]
fn fmtonly_describes_queries_without_rows_and_skips_other_statements() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    batch(
        &mut session,
        "CREATE TABLE t (id INT PRIMARY KEY, name NVARCHAR(MAX)) INSERT INTO t VALUES (1, N'a')",
    );
    let steps = batch(
        &mut session,
        "select @@trancount; SET FMTONLY ON select * from [dbo].[t] SET FMTONLY OFF exec ..sp_tablecollations_100 N'[dbo].[t]'",
    );
    assert_eq!(steps.len(), 5);
    let described = steps[2].outcome.as_ref().unwrap();
    assert!(steps[2].format_only);
    assert!(!steps[3].format_only);
    let names: Vec<&str> = described
        .columns
        .as_ref()
        .unwrap()
        .iter()
        .map(|column| column.name.as_str())
        .collect();
    assert_eq!(names, ["id", "name"]);
    assert!(described.rows.is_empty());
    let collations = &steps[4].outcome.as_ref().unwrap().rows;
    assert_eq!(collations.len(), 2);
    assert_eq!(collations[0][2], Value::Null);
    assert_eq!(
        collations[1][2],
        Value::Bytea(vec![0x09, 0x04, 0x00, 0x22, 0x00])
    );
    assert_eq!(collations[1][3], text("Latin1_General_100_BIN2"));
    let steps = batch(
        &mut session,
        "exec ..sp_tablecollations_100 N'.[t]' exec ..sp_tablecollations_100 N'[dbo].[missing]'",
    );
    assert_eq!(steps[0].outcome.as_ref().unwrap().rows.len(), 2);
    assert!(steps[1].outcome.as_ref().unwrap().rows.is_empty());
    batch(
        &mut session,
        "SET FMTONLY ON INSERT INTO t VALUES (2, N'b') SET FMTONLY OFF",
    );
    assert_eq!(
        texts(&mut session, "SELECT count(*) AS n FROM t"),
        vec![vec!["1".to_owned()]]
    );
}

fn first_error_code(session: &mut Session<'_>, text: &str) -> Option<u16> {
    let mut found = None;
    let outcome = session.execute_batch(text, &mut |step| {
        if found.is_none() {
            found = code(&step);
        }
        true
    });
    match outcome {
        Err(error) => error.vendor_code(),
        Ok(_) => found,
    }
}

#[test]
fn bcp_unicode_files_load_like_character_files() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    batch(
        &mut session,
        "CREATE TABLE cities (id INT PRIMARY KEY, name NVARCHAR(20))",
    );
    session.execute("INSERT BULK cities", &mut |_| {}).unwrap();
    let mut bytes = vec![0xFF, 0xFE];
    for unit in "1\tHà Nội\r\n2\t\n".encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let mut data: &[u8] = &bytes;
    let mut unicode = BcpUnicodeInput::new(&mut data);
    let mut reader = BcpCharacterReader::new(&mut unicode, BcpTerminators::default());
    let steps = load(&mut session, &mut reader);
    assert_eq!(tag(&steps[0]), "INSERT 0 2");
    assert_eq!(
        texts(&mut session, "SELECT id, name FROM cities ORDER BY id"),
        vec![
            vec!["1".to_owned(), "Hà Nội".to_owned()],
            vec!["2".to_owned(), "NULL".to_owned()],
        ]
    );
}

#[test]
fn bulk_insert_and_openrowset_bulk_never_read_server_files() {
    let store = MemoryStore::default();
    let mut session = session_with_items(&store);
    assert_eq!(
        first_error_code(
            &mut session,
            "BULK INSERT items FROM '/etc/passwd' WITH (FORMAT = 'CSV', FIELDTERMINATOR = ',', \
             ROWTERMINATOR = '0x0a', FIRSTROW = 2, LASTROW = 10, FORMATFILE = 'f.fmt', \
             CHECK_CONSTRAINTS, KEEPNULLS, KEEPIDENTITY, TABLOCK, ORDER (id ASC))"
        ),
        Some(4834)
    );
    assert_eq!(
        first_error_code(
            &mut session,
            "BULK INSERT items FROM N'C:\\data\\items.dat'"
        ),
        Some(4834)
    );
    assert_eq!(
        first_error_code(
            &mut session,
            "BULK INSERT items FROM 'items.dat' WITH (NO_SUCH_OPTION = 1)"
        ),
        Some(195)
    );
    assert_eq!(
        first_error_code(
            &mut session,
            "SELECT BulkColumn FROM OPENROWSET(BULK '/etc/hosts', SINGLE_CLOB) AS f"
        ),
        Some(4834)
    );
    assert_eq!(
        first_error_code(
            &mut session,
            "SELECT * FROM OPENROWSET(BULK 'items.dat', FORMATFILE = 'items.fmt', FIRSTROW = 2) AS f"
        ),
        Some(4834)
    );
    assert_eq!(
        texts(&mut session, "SELECT count(*) AS n FROM items"),
        vec![vec!["0".to_owned()]]
    );
}

#[test]
fn bcp_native_files_and_format_files_written_from_a_table_load_back() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    batch(
        &mut session,
        "CREATE TABLE goods (id INT PRIMARY KEY, name NVARCHAR(20), price DECIMAL(10,2), qty SMALLINT NOT NULL, added DATETIME2(0), code UNIQUEIDENTIFIER); \
         CREATE TABLE native_copy (id INT PRIMARY KEY, name NVARCHAR(20), price DECIMAL(10,2), qty SMALLINT NOT NULL, added DATETIME2(0), code UNIQUEIDENTIFIER); \
         CREATE TABLE format_copy (id INT PRIMARY KEY, name NVARCHAR(20), price DECIMAL(10,2), qty SMALLINT NOT NULL, added DATETIME2(0), code UNIQUEIDENTIFIER); \
         INSERT INTO goods VALUES (1, N'Bánh mì', 12.50, 3, '2024-05-06 07:08:09', '6F9619FF-8B86-D011-B42D-00C04FC964FF'), (2, NULL, NULL, -4, NULL, NULL), (3, N'', -0.01, 0, '1999-12-31 23:59:59', NULL)",
    );
    let mut native = Vec::new();
    let written = write_bcp_table(&mut session, "goods", &BcpFileFormat::Native, &mut native)
        .unwrap_or_else(|error| panic!("bcp out: {error}"));
    assert_eq!(written.rows, 3);
    assert_eq!(&native[..4], &[1, 0, 0, 0]);
    let format_file = written.format.to_format_file();
    assert!(format_file.starts_with("14.0\n6\n1\tSQLINT\t0\t4\t\"\"\t1\tid\t\"\"\n"));
    assert!(format_file.contains("\tSQLSMALLINT\t0\t2\t"));

    session
        .execute("INSERT BULK native_copy", &mut |_| {})
        .unwrap();
    let mut data: &[u8] = &native;
    let mut reader = BcpFileReader::native(&mut data);
    assert_eq!(tag(&load(&mut session, &mut reader)[0]), "INSERT 0 3");

    session
        .execute("INSERT BULK format_copy WITH (KEEP_NULLS)", &mut |_| {})
        .unwrap();
    let mut data: &[u8] = &native;
    let mut reader = BcpFileReader::new(&mut data, BcpFormat::parse(&format_file).unwrap());
    assert_eq!(tag(&load(&mut session, &mut reader)[0]), "INSERT 0 3");

    let original = texts(&mut session, "SELECT * FROM goods ORDER BY id");
    assert_eq!(original[0][1], "Bánh mì");
    assert_eq!(original[2][1], "");
    assert_eq!(
        texts(&mut session, "SELECT * FROM native_copy ORDER BY id"),
        original
    );
    assert_eq!(
        texts(&mut session, "SELECT * FROM format_copy ORDER BY id"),
        original
    );

    let mut character = Vec::new();
    write_bcp_table(
        &mut session,
        "goods",
        &BcpFileFormat::Character(BcpTerminators::default()),
        &mut character,
    )
    .unwrap();
    assert!(character.starts_with(b"1\tB\xc3\xa1nh m\xc3\xac\t12.50\t3\t"));
}
