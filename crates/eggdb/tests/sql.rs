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
    assert!(text(&no_file.stderr).contains("pass --create to put or sql"));
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
