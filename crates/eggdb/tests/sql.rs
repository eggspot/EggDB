//! `eggdb sql` run as a real process: the psql-style table, scripts and stdin, PostgreSQL-style
//! errors with their exit codes, and transactions that a failure or a killed process leaves
//! all or nothing.

mod common;

use common::TempPath;
use eggdb::Database;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

fn eggdb_sql(file: &Path, args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .arg("sql")
        .arg(file)
        .args(args)
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

#[test]
fn statements_print_psql_tables_and_command_tags() {
    let file = TempPath::new("sql-tables");
    let output = eggdb_sql(
        file.path(),
        &[
            "--create",
            "-c",
            "CREATE TABLE users (id integer PRIMARY KEY, name text, score double precision); \
             INSERT INTO users VALUES (2, 'Grace', NULL), (1, 'Ada', 9.5), (10, 'Linus', -1e20::float8); \
             SELECT * FROM users; \
             UPDATE users SET score = 1 WHERE id = 2 RETURNING name, score; \
             DELETE FROM users WHERE id = 10",
        ],
        b"",
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "CREATE TABLE\n\
         INSERT 0 3\n \
         id | name  | score\n\
         ----+-------+--------\n  \
         1 | Ada   |    9.5\n  \
         2 | Grace |\n \
         10 | Linus | -1e+20\n\
         (3 rows)\n\
         \n \
         name  | score\n\
         -------+-------\n \
         Grace |     1\n\
         (1 row)\n\
         \n\
         UPDATE 1\n\
         DELETE 1\n"
    );
    assert_eq!(text(&output.stderr), "");
}

#[test]
fn scripts_and_stdin_run_in_order_and_errors_point_at_the_statement() {
    let file = TempPath::new("sql-script");
    let script = TempPath::new("sql-script-text");
    std::fs::write(
        script.path(),
        "CREATE TABLE t (k text PRIMARY KEY, n bigint NOT NULL);\n-- a comment\nINSERT INTO t VALUES ('a', 1);\n",
    )
    .unwrap();
    let ran = eggdb_sql(
        file.path(),
        &["--create", script.path().to_str().unwrap()],
        b"",
    );
    assert_eq!(ran.status.code(), Some(0), "{}", text(&ran.stderr));
    assert_eq!(text(&ran.stdout), "CREATE TABLE\nINSERT 0 1\n");

    let failed = eggdb_sql(
        file.path(),
        &[],
        b"INSERT INTO t VALUES ('b', 2);\nSELECT k\n  FROM nope;\nINSERT INTO t VALUES ('c', 3);",
    );
    assert_eq!(failed.status.code(), Some(3));
    assert_eq!(text(&failed.stdout), "INSERT 0 1\n");
    assert_eq!(
        text(&failed.stderr),
        "ERROR:  relation \"nope\" does not exist\nLINE 3:   FROM nope;\n               ^\nSQLSTATE:  42P01\n"
    );
    let rows = eggdb_sql(
        file.path(),
        &["-c", "SELECT k, n FROM t ORDER BY n DESC"],
        b"",
    );
    assert_eq!(
        text(&rows.stdout),
        " k | n\n---+---\n b | 2\n a | 1\n(2 rows)\n\n"
    );

    let duplicate = eggdb_sql(file.path(), &["-c", "INSERT INTO t VALUES ('a', 9)"], b"");
    assert_eq!(duplicate.status.code(), Some(3));
    assert_eq!(
        text(&duplicate.stderr),
        "ERROR:  duplicate key value violates unique constraint \"t_pkey\"\nDETAIL:  Key (k)=(a) already exists.\nSQLSTATE:  23505\n"
    );
}

#[test]
fn usage_mistakes_exit_2() {
    let file = TempPath::new("sql-usage");
    for args in [
        vec!["-c", "SELECT 1", "script.sql"],
        vec!["-c", "SELECT 1", "-c", "SELECT 2"],
        vec!["missing-script.sql"],
        vec!["--format", "json", "-c", "SELECT 1"],
    ] {
        let output = eggdb_sql(file.path(), &args, b"");
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            text(&output.stderr)
        );
    }
    let not_utf8 = eggdb_sql(file.path(), &["--create"], b"SELECT '\xff'");
    assert_eq!(not_utf8.status.code(), Some(2));
    let no_file = eggdb_sql(file.path(), &["-c", "SELECT 1"], b"");
    assert_eq!(no_file.status.code(), Some(3));
    assert!(text(&no_file.stderr).contains("pass --create to put, sql or serve"));
}

#[test]
fn a_failed_block_leaves_no_partial_table_and_the_file_stays_sound() {
    let file = TempPath::new("sql-rollback");
    let output = eggdb_sql(
        file.path(),
        &[
            "--create",
            "-c",
            "BEGIN; CREATE TABLE t (id int PRIMARY KEY); INSERT INTO t VALUES (1), (2); INSERT INTO t VALUES (2); COMMIT",
        ],
        b"",
    );
    assert_eq!(output.status.code(), Some(3));
    let after = eggdb_sql(file.path(), &["-c", "SELECT * FROM t"], b"");
    assert_eq!(after.status.code(), Some(3));
    assert!(text(&after.stderr).contains("relation \"t\" does not exist"));
    let unfinished = eggdb_sql(
        file.path(),
        &[
            "-c",
            "BEGIN; CREATE TABLE t (id int); INSERT INTO t VALUES (1)",
        ],
        b"",
    );
    assert_eq!(unfinished.status.code(), Some(0));
    assert!(text(&unfinished.stderr).contains("it was rolled back"));
    let db = Database::open(file.path()).unwrap();
    assert_eq!(db.begin_read().unwrap().scan(..).count(), 0);
    assert_eq!(db.check().unwrap().leaked_pages, 0);
}

#[test]
fn a_killed_process_leaves_the_whole_transaction_or_none_of_it() {
    let rows = 20_000;
    let mut script =
        String::from("BEGIN;\nCREATE TABLE big (id bigint PRIMARY KEY, label text);\n");
    for id in 0..rows {
        script.push_str(&format!("INSERT INTO big VALUES ({id}, 'row {id}');\n"));
    }
    script.push_str("COMMIT;\n");
    for delay_ms in [0u64, 20, 60, 150, 400] {
        let file = TempPath::new("sql-killed");
        Database::open(file.path()).unwrap().close().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_eggdb"))
            .arg("sql")
            .arg(file.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        std::thread::sleep(Duration::from_millis(delay_ms));
        let _ = child.kill();
        let _ = child.wait();
        let db = Database::open(file.path()).unwrap();
        let count = db.begin_read().unwrap().scan(..).count();
        assert!(
            count == 0 || count == rows + 2,
            "after {delay_ms} ms: {count} keys, neither none nor the whole transaction"
        );
        assert_eq!(db.check().unwrap().leaked_pages, 0);
        db.close().unwrap();
        let listed = eggdb_sql(file.path(), &["-c", "SELECT id FROM big WHERE id = 7"], b"");
        let stdout = text(&listed.stdout);
        if count == 0 {
            assert_eq!(listed.status.code(), Some(3));
        } else {
            assert_eq!(stdout, " id\n----\n  7\n(1 row)\n\n");
        }
    }
}

#[test]
fn a_killed_process_never_leaves_an_index_that_disagrees_with_its_rows() {
    let mut script = String::from(
        "CREATE TABLE items (id bigint PRIMARY KEY, code text UNIQUE, grp integer);\nCREATE INDEX items_grp ON items (grp);\n",
    );
    for id in 0..3_000 {
        if id % 150 == 0 {
            script.push_str("BEGIN;\n");
        }
        script.push_str(&format!(
            "INSERT INTO items VALUES ({id}, 'code {id}', {});\n",
            id % 17
        ));
        if id % 7 == 3 {
            script.push_str(&format!(
                "UPDATE items SET grp = grp + 1, code = 'moved {id}' WHERE id = {};\n",
                id - 2
            ));
        }
        if id % 11 == 5 {
            script.push_str(&format!("DELETE FROM items WHERE id = {};\n", id - 4));
        }
        if id % 150 == 149 {
            script.push_str("COMMIT;\n");
        }
    }
    let mut entries_seen = 0;
    for delay_ms in [0u64, 120, 300, 600, 1_000] {
        let file = TempPath::new("sql-index-killed");
        Database::open(file.path()).unwrap().close().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_eggdb"))
            .arg("sql")
            .arg(file.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let bytes = script.clone().into_bytes();
        let writer = std::thread::spawn(move || {
            let _ = input.write_all(&bytes);
        });
        std::thread::sleep(Duration::from_millis(delay_ms));
        let _ = child.kill();
        let _ = child.wait();
        let _ = writer.join();
        let db = Database::open(file.path()).unwrap();
        let store = db.sql_store();
        let mut session = store.session(eggdb::sql::Dialect::PostgreSql);
        match session.check_indexes() {
            Ok(check) => entries_seen = entries_seen.max(check.entries),
            Err(error) => panic!("after {delay_ms} ms: {error}"),
        }
        drop(session);
        assert_eq!(db.check().unwrap().leaked_pages, 0);
    }
    assert!(
        entries_seen > 0,
        "no run committed a row before it was killed"
    );
}

#[test]
fn a_killed_process_never_rewinds_a_sequence_or_breaks_a_foreign_key() {
    let mut script = String::from(
        "CREATE SEQUENCE ids;\nCREATE TABLE parents (id bigint PRIMARY KEY DEFAULT nextval('ids'), label text CHECK (label <> ''));\nCREATE TABLE children (id bigint PRIMARY KEY DEFAULT nextval('ids'), pid bigint REFERENCES parents ON DELETE CASCADE);\n",
    );
    for round in 0..1_500 {
        if round % 50 == 0 {
            script.push_str("BEGIN;\n");
        }
        script.push_str("INSERT INTO parents (label) VALUES ('p');\n");
        script.push_str("INSERT INTO children (pid) VALUES ((SELECT max(id) FROM parents));\n");
        script.push_str("INSERT INTO children (pid) VALUES ((SELECT min(id) FROM parents));\n");
        if round % 7 == 3 {
            script.push_str("DELETE FROM parents WHERE id = (SELECT min(id) FROM parents);\n");
        }
        if round % 50 == 49 {
            script.push_str("COMMIT;\n");
        }
        if round % 13 == 0 {
            script.push_str("BEGIN;\nSELECT nextval('ids');\nROLLBACK;\n");
        }
    }
    let mut highest_seen = 0;
    for delay_ms in [0u64, 150, 400, 800, 1_500] {
        let file = TempPath::new("sql-sequence-killed");
        Database::open(file.path()).unwrap().close().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_eggdb"))
            .arg("sql")
            .arg(file.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let bytes = script.clone().into_bytes();
        let writer = std::thread::spawn(move || {
            let _ = input.write_all(&bytes);
        });
        std::thread::sleep(Duration::from_millis(delay_ms));
        let _ = child.kill();
        let _ = child.wait();
        let _ = writer.join();
        let db = Database::open(file.path()).unwrap();
        let store = db.sql_store();
        let mut session = store.session(eggdb::sql::Dialect::PostgreSql);
        let mut results = Vec::new();
        let outcome = session.execute(
            "SELECT coalesce(max(id), 0) FROM parents; SELECT coalesce(max(id), 0) FROM children; SELECT count(*) FROM children c LEFT JOIN parents p ON c.pid = p.id WHERE p.id IS NULL AND c.pid IS NOT NULL; SELECT nextval('ids')",
            &mut |result| results.push(result),
        );
        match outcome {
            Ok(()) => {
                let number = |at: usize| results[at].rows[0][0].as_integer().unwrap();
                let highest = number(0).max(number(1));
                assert_eq!(number(2), 0, "after {delay_ms} ms: a child lost its parent");
                assert!(
                    number(3) > highest,
                    "after {delay_ms} ms: nextval gave {} after {highest} was stored",
                    number(3)
                );
                highest_seen = highest_seen.max(highest);
            }
            Err(error) => assert!(
                error.state().code() == "42P01",
                "after {delay_ms} ms: {error}"
            ),
        }
        session.check_indexes().unwrap();
        drop(session);
        assert_eq!(db.check().unwrap().leaked_pages, 0);
    }
    assert!(
        highest_seen > 0,
        "no run committed a row before it was killed"
    );
}

#[test]
fn a_killed_process_leaves_each_schema_with_its_table_and_view_or_none_of_them() {
    const ROUNDS: usize = 300;
    let mut script = String::new();
    for round in 0..ROUNDS {
        script.push_str(&format!(
            "BEGIN;\nCREATE SCHEMA s{round};\nCREATE TABLE s{round}.t (a int PRIMARY KEY);\nINSERT INTO s{round}.t VALUES ({round});\nCREATE VIEW s{round}.v AS SELECT a FROM s{round}.t;\nCOMMENT ON VIEW s{round}.v IS 'round {round}';\nCOMMIT;\n"
        ));
        if round % 3 == 2 {
            script.push_str(&format!(
                "BEGIN;\nDROP SCHEMA s{} CASCADE;\nCOMMIT;\n",
                round - 1
            ));
        }
    }
    let mut complete_seen = 0;
    for delay_ms in [0u64, 100, 300, 700, 1_500] {
        let file = TempPath::new("sql-schema-killed");
        Database::open(file.path()).unwrap().close().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_eggdb"))
            .arg("sql")
            .arg(file.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let bytes = script.clone().into_bytes();
        let writer = std::thread::spawn(move || {
            let _ = input.write_all(&bytes);
        });
        std::thread::sleep(Duration::from_millis(delay_ms));
        let _ = child.kill();
        let _ = child.wait();
        let _ = writer.join();
        let db = Database::open(file.path()).unwrap();
        let store = db.sql_store();
        let mut session = store.session(eggdb::sql::Dialect::PostgreSql);
        for round in 0..ROUNDS {
            let mut through_view = Vec::new();
            let view = session.execute(&format!("SELECT a FROM s{round}.v"), &mut |result| {
                through_view.push(result)
            });
            let table = session.execute(&format!("SELECT a FROM s{round}.t"), &mut |_| {});
            assert_eq!(
                view.is_ok(),
                table.is_ok(),
                "after {delay_ms} ms: schema s{round} kept a table without its view or the reverse"
            );
            if view.is_ok() {
                assert_eq!(through_view[0].rows.len(), 1);
                complete_seen += 1;
            }
        }
        session.check_indexes().unwrap();
        drop(session);
        assert_eq!(db.check().unwrap().leaked_pages, 0);
    }
    assert!(
        complete_seen > 0,
        "no run committed a schema before it was killed"
    );
}

#[test]
fn the_mysql_dialect_reads_mysql_and_reports_mysql_errors() {
    let file = TempPath::new("sql-mysql");
    let created = eggdb_sql(
        file.path(),
        &[
            "--create",
            "--dialect",
            "mysql",
            "-c",
            "CREATE TABLE `items` (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(20)); INSERT INTO items (name) VALUES (\"egg\"), ('jam'); SELECT id, name FROM items LIMIT 1, 1",
        ],
        b"",
    );
    assert_eq!(created.status.code(), Some(0), "{}", text(&created.stderr));
    assert!(
        text(&created.stdout).contains(" 2 | jam"),
        "{}",
        text(&created.stdout)
    );
    let missing = eggdb_sql(
        file.path(),
        &["--dialect", "mysql", "-c", "SELECT * FROM missing"],
        b"",
    );
    assert_eq!(missing.status.code(), Some(3));
    assert_eq!(
        text(&missing.stderr),
        "ERROR 1146 (42S02): Table 'eggdb.missing' doesn't exist\n"
    );
    let postgresql = eggdb_sql(
        file.path(),
        &["-c", "SELECT name FROM items WHERE id = 1"],
        b"",
    );
    assert!(text(&postgresql.stdout).contains("egg"));
    let unknown = eggdb_sql(file.path(), &["--dialect", "db2", "-c", "SELECT 1"], b"");
    assert_eq!(unknown.status.code(), Some(2));
    assert!(
        text(&unknown.stderr)
            .contains("--dialect takes postgresql, mysql, sqlite, oracle or sqlserver, not 'db2'")
    );
}

#[test]
fn the_sqlite_dialect_reads_sqlite_and_reports_sqlite_errors() {
    let file = TempPath::new("sql-sqlite");
    let created = eggdb_sql(
        file.path(),
        &[
            "--create",
            "--dialect",
            "sqlite",
            "-c",
            "CREATE TABLE [items] (id INTEGER PRIMARY KEY, name TEXT); INSERT INTO items (name) VALUES ('egg'), ('jam'); SELECT id, name, typeof(id) FROM items LIMIT 1, 1",
        ],
        b"",
    );
    assert_eq!(created.status.code(), Some(0), "{}", text(&created.stderr));
    assert!(
        text(&created.stdout).contains(" 2 | jam  | integer"),
        "{}",
        text(&created.stdout)
    );
    let missing = eggdb_sql(
        file.path(),
        &["--dialect", "sqlite", "-c", "SELECT * FORM items"],
        b"",
    );
    assert_eq!(missing.status.code(), Some(3));
    assert_eq!(
        text(&missing.stderr),
        "Error: near \"FORM\": syntax error\nResult code: SQLITE_ERROR (1)\n"
    );
    let schema = eggdb_sql(
        file.path(),
        &[
            "--dialect",
            "sqlite3",
            "-c",
            "SELECT sql FROM sqlite_schema",
        ],
        b"",
    );
    assert!(
        text(&schema.stdout).contains("CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)"),
        "{}",
        text(&schema.stdout)
    );
}

#[test]
fn the_oracle_dialect_reads_oracle_and_reports_ora_errors() {
    let file = TempPath::new("sql-oracle");
    let created = eggdb_sql(
        file.path(),
        &[
            "--create",
            "--dialect",
            "oracle",
            "-c",
            "CREATE TABLE items (id NUMBER(9) PRIMARY KEY, name VARCHAR2(10)); INSERT INTO items VALUES (1, 'egg'); INSERT INTO items VALUES (2, ''); SELECT id, NVL(name, '-') AS name FROM items ORDER BY id FETCH FIRST 1 ROWS ONLY",
        ],
        b"",
    );
    assert_eq!(created.status.code(), Some(0), "{}", text(&created.stderr));
    assert!(
        text(&created.stdout).contains(" ID | NAME"),
        "{}",
        text(&created.stdout)
    );
    let missing = eggdb_sql(
        file.path(),
        &["--dialect", "oracle", "-c", "SELECT * FROM nowhere"],
        b"",
    );
    assert_eq!(missing.status.code(), Some(3));
    assert_eq!(
        text(&missing.stderr),
        "ORA-00942: table or view does not exist\n"
    );
    let committed = eggdb_sql(
        file.path(),
        &["-c", "SELECT id FROM items WHERE name IS NULL"],
        b"",
    );
    assert!(
        text(&committed.stdout).contains("  2\n"),
        "{}",
        text(&committed.stdout)
    );
}

#[test]
fn the_sqlserver_dialect_reads_transact_sql_and_reports_sql_server_errors() {
    let file = TempPath::new("sql-sqlserver");
    let created = eggdb_sql(
        file.path(),
        &[
            "--create",
            "--dialect",
            "sqlserver",
            "-c",
            "CREATE TABLE [dbo].[Items] ([ID] INT IDENTITY(1,1) PRIMARY KEY, [Name] NVARCHAR(20)) INSERT INTO items (name) VALUES (N'egg'), (N'jam') SELECT TOP 1 [Name] AS [Last] FROM items ORDER BY id DESC",
        ],
        b"",
    );
    assert_eq!(created.status.code(), Some(0), "{}", text(&created.stderr));
    assert!(
        text(&created.stdout).contains("jam"),
        "{}",
        text(&created.stdout)
    );
    let missing = eggdb_sql(
        file.path(),
        &["--dialect", "sqlserver", "-c", "SELECT * FROM [Missing]"],
        b"",
    );
    assert_eq!(missing.status.code(), Some(3));
    assert_eq!(
        text(&missing.stderr),
        "Msg 208, Level 16, State 1: Invalid object name 'missing'.\n"
    );
}
