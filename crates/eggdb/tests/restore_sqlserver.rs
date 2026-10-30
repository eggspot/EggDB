//! `eggdb restore --dialect sqlserver` as a real process: an SSMS Generate Scripts fixture
//! (UTF-16LE, CRLF) and a sqlpackage script restore and match results written by hand from the
//! scripts' data and Microsoft's documented semantics; the report lists what was skipped, ignored
//! and kept differently; other encodings, stdin, volume, errors, routes and sqlcmd commands.

use eggdb::Database;
use eggdb::sql::{Dialect, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT_DIRECTORY: AtomicU32 = AtomicU32::new(0);

struct TempDirectory {
    path: PathBuf,
}

impl TempDirectory {
    fn new(name: &str) -> Self {
        let number = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "eggdb-restore-sqlserver-{name}-{}-{number}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/sqlserver")
        .join(name)
}

fn eggdb(args: &[&dyn AsRef<std::ffi::OsStr>], stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .args(args.iter().map(|arg| arg.as_ref()))
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let stdin = stdin.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = input.write_all(&stdin);
    });
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    output
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn utf16_fixture_as_utf8(name: &str) -> String {
    let bytes = std::fs::read(fixture(name)).unwrap();
    assert_eq!(&bytes[..2], b"\xFF\xFE");
    let units: Vec<u16> = bytes[2..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    String::from_utf16(&units).unwrap()
}

fn shown(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        value => value.to_text().unwrap_or_default(),
    }
}

fn tsql(db: &Database, query: &str) -> String {
    let store = db.sql_store();
    let mut session = store.session(Dialect::SqlServer);
    let mut out = String::new();
    session
        .execute(query, &mut |result| {
            for row in &result.rows {
                let values: Vec<String> = row.iter().map(shown).collect();
                out.push_str(&values.join("|"));
                out.push('\n');
            }
        })
        .unwrap_or_else(|error| panic!("{query}: {error}"));
    session.close().unwrap();
    out
}

fn expected_results() -> Vec<(String, String)> {
    let expected = std::fs::read_to_string(fixture("shop-expected.txt")).unwrap();
    let mut sections: Vec<(String, String)> = Vec::new();
    for line in expected.split_inclusive('\n') {
        if let Some(query) = line.strip_prefix("== ") {
            sections.push((query.trim_end().to_owned(), String::new()));
        } else if let Some((_, rows)) = sections.last_mut() {
            rows.push_str(line);
        }
    }
    sections
}

fn restore_json(target: &Path, script: &dyn AsRef<std::ffi::OsStr>, stdin: &[u8]) -> Output {
    eggdb(
        &[
            &"restore",
            &"--dialect",
            &"sqlserver",
            &"--create",
            &target,
            script,
            &"--format",
            &"json",
        ],
        stdin,
    )
}

#[test]
fn the_ssms_fixture_restores_and_matches_the_hand_written_results() {
    let directory = TempDirectory::new("ssms");
    let target = directory.join("shop.db");
    let output = restore_json(&target, &fixture("shop-ssms.sql"), b"");
    let report = text(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}\n{report}",
        text(&output.stderr)
    );
    for part in [
        "\"ok\":true",
        "\"dialect\":\"sqlserver\"",
        "\"scripts\":[",
        "\"schemas\":1",
        "\"tables\":6",
        "\"rows\":19",
        "\"views\":2",
        "\"indexes\":2",
        "\"constraints\":9",
        "\"tables_skipped\":1",
        "\"rows_skipped\":2",
        "\"kind\":\"table\",\"name\":\"dbo.Docs\"",
        "\"kind\":\"procedure\",\"name\":\"dbo.TopCustomers\"",
        "\"kind\":\"function\",\"name\":\"dbo.OrderItems\"",
        "\"kind\":\"trigger\",\"name\":\"sales.trOrders\"",
        "\"kind\":\"synonym\",\"name\":\"dbo.Orders\"",
        "\"kind\":\"user or role\",\"name\":\"shopapp\"",
        "\"kind\":\"role membership\"",
        "is restored only as existence guards",
        "\"uses\":\"constraint FK_OrderLines_Products\"",
        "\"object\":\"dbo.Products.Margin\",\"difference\":\"computed column not kept\"",
        "\"object\":\"dbo.Products.Version\",\"difference\":\"rowversion column not kept\"",
        "\"object\":\"sales.Orders.PlacedAt\",\"difference\":\"values are stored in UTC; their offsets are lost\"",
        "\"object\":\"index IX_Orders_Open\",\"difference\":\"INCLUDE columns, the filter (a full index here) not kept\"",
        "\"object\":\"FK_OrderLines_Products\",\"difference\":\"not created: existing rows violate it",
        "\"object\":\"sales.Orders\",\"difference\":\"a constraint disabled in the source (NOCHECK CONSTRAINT) is enforced here\"",
        "\"grant\":1",
        "{\"kind\":\"USE\",\"statements\":3}",
        "{\"kind\":\"ALTER DATABASE\",\"statements\":6}",
        "{\"kind\":\"extended properties\",\"statements\":2}",
        "{\"kind\":\"ENABLE and DISABLE TRIGGER\",\"statements\":1}",
        "\"errors\":[]",
    ] {
        assert!(report.contains(part), "{part:?} in\n{report}");
    }
    let db = Database::open(&target).unwrap();
    for (query, rows) in expected_results() {
        assert_eq!(tsql(&db, &query), rows, "{query}");
    }
    assert_eq!(
        tsql(
            &db,
            "INSERT dbo.Customers (Name) VALUES (N'Defaulted'); SELECT Id, CAST(Active AS int) FROM dbo.Customers WHERE Name = N'Defaulted' AND Joined IS NOT NULL"
        ),
        "3|1\n"
    );
    assert_eq!(
        tsql(
            &db,
            "INSERT dbo.Coupons (Code) VALUES (N'NEW'); SELECT MAX(Id) FROM dbo.Coupons"
        ),
        "1020\n"
    );
    assert_eq!(
        tsql(
            &db,
            "INSERT sales.Orders (CustomerId, PlacedAt, Status) VALUES (1, SYSDATETIMEOFFSET(), 1); SELECT Id FROM sales.Orders WHERE Created > DATEADD(day, 29, GETDATE()) AND Created <= DATEADD(day, 30, GETDATE())"
        ),
        "4\n"
    );
    assert_eq!(
        tsql(
            &db,
            "SELECT seed_value, increment_value FROM sys.identity_columns WHERE object_id = OBJECT_ID('dbo.Coupons')"
        ),
        "1000|10\n"
    );
    assert_eq!(
        tsql(
            &db,
            "SELECT definition FROM sys.default_constraints WHERE parent_object_id = OBJECT_ID('sales.Orders')"
        ),
        "(dateadd(day,(30),getdate()))\n"
    );
    assert_eq!(
        tsql(
            &db,
            "INSERT dbo.Products (Name, Price, Cost) VALUES (N'Spoon', 1, 0.5); SELECT COUNT(Guid) FROM dbo.Products"
        ),
        "4\n"
    );
    let store = db.sql_store();
    let mut session = store.session(Dialect::SqlServer);
    let refused = session
        .execute(
            "INSERT sales.OrderLines (OrderId, [LineNo], ProductId, Quantity) VALUES (1, 9, 1, 0)",
            &mut |_| {},
        )
        .unwrap_err();
    assert_eq!(refused.vendor_code(), Some(547), "{refused}");
    session.close().unwrap();
    assert_eq!(
        tsql(
            &db,
            "DELETE FROM sales.Orders WHERE Id = 2; SELECT COUNT(*) FROM sales.OrderLines WHERE OrderId = 2"
        ),
        "0\n"
    );
    db.close().unwrap();
}

#[test]
fn the_text_report_lists_the_known_differences_and_client_commands() {
    let directory = TempDirectory::new("text");
    let target = directory.join("shop.db");
    let output = eggdb(
        &[
            &"restore",
            &"--dialect",
            &"mssql",
            &"--create",
            &target,
            &fixture("shop-sqlpackage.sql"),
            &"--format",
            &"text",
        ],
        b"",
    );
    let report = text(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}\n{report}",
        text(&output.stderr)
    );
    for part in [
        "  tables          2, 0 rows\n",
        "  indexes         1\n",
        "  constraints     4\n",
        "  differences     0\n",
        "client commands 6",
        "  :setvar ",
        "(used for $(name))",
        "  :on error ",
        "(eggdb restore's own flags decide)",
        "is restored only as existence guards",
        "ok: every table and row of the script is in ",
    ] {
        assert!(report.contains(part), "{part:?} in\n{report}");
    }
    let db = Database::open(&target).unwrap();
    assert_eq!(
        tsql(
            &db,
            "INSERT dbo.Customers (Name) VALUES (N'x'); SELECT CAST(Active AS int) FROM dbo.Customers"
        ),
        "1\n"
    );
    db.close().unwrap();
}

#[test]
fn utf8_lf_and_stdin_variants_restore_the_same_rows() {
    let script = utf16_fixture_as_utf8("shop-ssms.sql");
    let lf = script.replace("\r\n", "\n");
    let mut with_mark = b"\xEF\xBB\xBF".to_vec();
    with_mark.extend_from_slice(script.as_bytes());
    for (name, bytes) in [
        ("utf8", script.as_bytes().to_vec()),
        ("utf8-bom", with_mark),
        ("lf", lf.into_bytes()),
    ] {
        let directory = TempDirectory::new(name);
        let path = directory.join("shop.sql");
        std::fs::write(&path, &bytes).unwrap();
        let target = directory.join("shop.db");
        let output = restore_json(&target, &path, b"");
        assert_eq!(
            output.status.code(),
            Some(0),
            "{name}: {}",
            text(&output.stderr)
        );
        assert!(text(&output.stdout).contains("\"rows\":19"), "{name}");
        let target = directory.join("stdin.db");
        let output = restore_json(&target, &"-", &bytes);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{name} stdin: {}",
            text(&output.stderr)
        );
        assert!(text(&output.stdout).contains("\"rows\":19"), "{name} stdin");
    }
}

#[test]
fn a_generated_script_of_5000_rows_and_one_20_mib_value_restore_in_full() {
    let directory = TempDirectory::new("volume");
    let mut script = String::from(
        "CREATE TABLE [dbo].[Items]([Id] [int] NOT NULL, [Name] [nvarchar](40) NOT NULL, CONSTRAINT [PK_Items] PRIMARY KEY CLUSTERED ([Id] ASC))\r\nGO\r\n",
    );
    let mut sum_id = 0u64;
    let mut sum_length = 0u64;
    for id in 1..=5000u64 {
        let name = format!("item {id} {}", "x".repeat((id % 17) as usize));
        sum_id += id;
        sum_length += name.trim_end().chars().count() as u64;
        script.push_str(&format!(
            "INSERT [dbo].[Items] ([Id], [Name]) VALUES ({id}, N'{name}')\r\nGO\r\n"
        ));
    }
    script.push_str("CREATE TABLE [dbo].[Blobs]([Id] [int] NOT NULL PRIMARY KEY, [Data] [varbinary](max) NULL)\r\nGO\r\n");
    let blob_bytes = 20usize << 20;
    script.push_str("INSERT [dbo].[Blobs] ([Id], [Data]) VALUES (1, 0x");
    script.push_str(&"AB".repeat(blob_bytes));
    script.push_str(")\r\nGO\r\n");
    let path = directory.join("volume.sql");
    std::fs::write(&path, script).unwrap();
    let target = directory.join("volume.db");
    let output = restore_json(&target, &path, b"");
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert!(text(&output.stdout).contains("\"rows\":5001"));
    let db = Database::open(&target).unwrap();
    assert_eq!(
        tsql(
            &db,
            "SELECT COUNT(*), SUM(CAST(Id AS bigint)), SUM(LEN(Name)) FROM dbo.Items"
        ),
        format!("5000|{sum_id}|{sum_length}\n")
    );
    assert_eq!(
        tsql(&db, "SELECT COUNT(*) FROM dbo.Blobs WHERE Data IS NOT NULL"),
        "1\n"
    );
    db.close().unwrap();
}

#[test]
fn errors_stop_or_are_listed_and_a_failed_create_leaves_no_file() {
    let directory = TempDirectory::new("errors");
    let script = b"CREATE TABLE [dbo].[T]([Id] [int] NOT NULL PRIMARY KEY)\r\nGO\r\nINSERT [dbo].[T] ([Id]) VALUES (1)\r\nGO\r\nINSERT [dbo].[T] ([Id]) VALUES (1)\r\nGO\r\nINSERT [dbo].[T] ([Id]) VALUES (2)\r\nINSERT [dbo].[T] ([Id] VALUES (3)\r\nGO\r\nINSERT [dbo].[T] ([Id]) VALUES (4)\r\nGO\r\n";
    let path = directory.join("broken.sql");
    std::fs::write(&path, script).unwrap();
    let target = directory.join("broken.db");
    let output = eggdb(
        &[
            &"restore",
            &"--dialect",
            &"sqlserver",
            &"--create",
            &target,
            &path,
        ],
        b"",
    );
    assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
    assert!(
        text(&output.stderr).contains("line 5:"),
        "{}",
        text(&output.stderr)
    );
    assert!(!target.exists());
    assert!(!directory.join("broken.db.restoring").exists());
    let output = eggdb(
        &[
            &"restore",
            &"--dialect",
            &"sqlserver",
            &"--create",
            &target,
            &path,
            &"--keep-going",
            &"--format",
            &"json",
        ],
        b"",
    );
    assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
    let report = text(&output.stdout);
    assert!(report.contains("\"line\":5,"), "{report}");
    assert!(report.contains("\"line\":8,"), "{report}");
    let db = Database::open(&target).unwrap();
    assert_eq!(tsql(&db, "SELECT Id FROM dbo.T ORDER BY Id"), "1\n4\n");
    db.close().unwrap();
    let existing = directory.join("existing.db");
    let output = eggdb(
        &[
            &"restore",
            &"--dialect",
            &"sqlserver",
            &"--create",
            &existing,
            &"-",
        ],
        b"CREATE TABLE [dbo].[T]([Id] [int] NOT NULL PRIMARY KEY)\r\nGO\r\n",
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let output = eggdb(
        &[&"restore", &"--dialect", &"sqlserver", &existing, &"-"],
        b"INSERT [dbo].[T] ([Id]) VALUES (1)\r\nGO\r\nINSERT [dbo].[T] ([Id]) VALUES (1)\r\nGO\r\nINSERT [dbo].[T] ([Id]) VALUES (2)\r\nGO\r\n",
    );
    assert_eq!(output.status.code(), Some(3));
    assert!(
        text(&output.stderr).contains("line 3:"),
        "{}",
        text(&output.stderr)
    );
    let db = Database::open(&existing).unwrap();
    assert_eq!(tsql(&db, "SELECT Id FROM dbo.T ORDER BY Id"), "1\n");
    db.close().unwrap();
}

#[test]
fn files_that_are_not_scripts_are_routed_by_their_content() {
    let directory = TempDirectory::new("routes");
    let cases: [(&str, &[u8], &str, &str); 5] = [
        (
            "shop.bak",
            b"TAPE\0\0\x03\0\0\0",
            "a SQL Server backup (it starts with TAPE)",
            "Generate Scripts",
        ),
        (
            "shop.bacpac",
            b"PK\x03\x04\x14\0\0\0",
            "a BACPAC package (a zip archive)",
            "sqlpackage /Action:Import",
        ),
        (
            "shop.zip",
            b"PK\x03\x04\x14\0\0\0",
            "a zip archive, not a script",
            "unzip -l",
        ),
        (
            "shop.mdf",
            b"\x01\x0f\0\0\0\0\0\0data",
            "a SQL Server data file",
            "Attach it to a SQL Server",
        ),
        (
            "shop.bin",
            b"\x01\x0f\0\0\0\0\0\0data",
            "binary data (it holds NUL bytes)",
            "docs/migrate.md",
        ),
    ];
    for (name, bytes, what, advice) in cases {
        let path = directory.join(name);
        std::fs::write(&path, bytes).unwrap();
        let target = directory.join("target.db");
        for dialect in ["postgresql", "sqlserver"] {
            let output = eggdb(
                &[
                    &"restore",
                    &"--dialect",
                    &dialect,
                    &"--create",
                    &target,
                    &path,
                ],
                b"",
            );
            let stderr = text(&output.stderr);
            assert_eq!(output.status.code(), Some(2), "{name}: {stderr}");
            assert!(stderr.contains(what), "{name}: {stderr}");
            assert!(stderr.contains(advice), "{name}: {stderr}");
            assert!(!target.exists(), "{name}");
        }
    }
    let output = eggdb(
        &[
            &"restore",
            &"--create",
            &directory.join("utf16.db"),
            &fixture("shop-ssms.sql"),
        ],
        b"",
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(
        text(&output.stderr).contains("Add --dialect sqlserver."),
        "{}",
        text(&output.stderr)
    );
    let backup = directory.join("backup.bak");
    std::fs::write(
        &backup,
        b"CREATE TABLE t (id integer PRIMARY KEY);\nINSERT INTO t VALUES (1);\n",
    )
    .unwrap();
    let output = eggdb(
        &[&"restore", &"--create", &directory.join("pg.db"), &backup],
        b"",
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
}

#[test]
fn sqlcmd_commands_are_listed_and_include_and_shell_commands_never_run() {
    let directory = TempDirectory::new("sqlcmd");
    let sentinel = directory.join("sentinel");
    let included = directory.join("included.sql");
    std::fs::write(
        &included,
        b"CREATE TABLE [dbo].[Included]([Id] [int] NOT NULL PRIMARY KEY)\r\nGO\r\n",
    )
    .unwrap();
    let script = format!(
        "CREATE TABLE [dbo].[T]([Id] [int] IDENTITY(1,1) NOT NULL PRIMARY KEY, [Note] [nvarchar](40) NULL)\r\nGO\r\nINSERT [dbo].[T] ([Note]) VALUES (N'$(kept)')\r\nGO\r\n:r {}\r\n!! touch {}\r\n:setvar Who \"egg\"\r\nINSERT [dbo].[T] ([Note]) VALUES (N'$(Who)')\r\nGO 3\r\n",
        included.display(),
        sentinel.display()
    );
    let target = directory.join("sqlcmd.db");
    let output = eggdb(
        &[
            &"restore",
            &"--dialect",
            &"tsql",
            &"--create",
            &target,
            &"-",
            &"--format",
            &"json",
        ],
        script.as_bytes(),
    );
    let report = text(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}\n{report}",
        text(&output.stderr)
    );
    assert!(
        report.contains("\"command\":\":r\",\"reason\":\"never run\""),
        "{report}"
    );
    assert!(
        report.contains("\"command\":\"!!\",\"reason\":\"never run\""),
        "{report}"
    );
    assert!(!sentinel.exists());
    let db = Database::open(&target).unwrap();
    assert_eq!(
        tsql(
            &db,
            "SELECT Note, COUNT(*) FROM dbo.T GROUP BY Note ORDER BY Note"
        ),
        "$(kept)|1\negg|3\n"
    );
    db.close().unwrap();
    let output = eggdb(
        &[
            &"restore",
            &"--dialect",
            &"sqlserver",
            &"--create",
            &directory.join("undefined.db"),
            &"-",
        ],
        b":setvar A \"1\"\r\nSELECT N'$(missing)'\r\nGO\r\n",
    );
    assert_eq!(output.status.code(), Some(3));
    assert!(
        text(&output.stderr).contains("'missing' scripting variable not defined."),
        "{}",
        text(&output.stderr)
    );
}
