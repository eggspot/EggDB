//! The `eggdb` command against a file another process has open.

use eggdb::{Database, Options};
use std::process::{Command, Output};

fn eggdb(directory: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .args(args)
        .current_dir(directory)
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_busy_file_exits_5_and_says_which_side_waits() {
    let directory = std::env::temp_dir().join(format!("eggdb-cli-busy-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("app.db");
    let db = Database::open(&path).unwrap();
    db.put("k", "v").unwrap();
    let writer = db;

    let read = eggdb(&directory, &["get", "app.db", "k", "--busy-timeout", "0"]);
    assert_eq!(read.status.code(), Some(5), "{}", stderr(&read));
    assert_eq!(
        stderr(&read),
        "eggdb: cannot open app.db: another program has it open for writing. Close it, or wait and run the command again.\n"
    );
    writer.close().unwrap();

    let reader = Database::open_with(&path, Options::new().read_only(true)).unwrap();
    let write = eggdb(
        &directory,
        &["put", "app.db", "k", "w", "--busy-timeout", "0"],
    );
    assert_eq!(write.status.code(), Some(5), "{}", stderr(&write));
    assert_eq!(
        stderr(&write),
        "eggdb: cannot open app.db: another program has it open. Close it before writing, or read with `eggdb get`, `scan`, `check` or `stats`, which work alongside readers.\n  it may be an AI agent's call through eggdb mcp, which holds the file for up to its time limit; run the command again, or see `eggdb audit app.db`.\n"
    );
    let alongside = eggdb(&directory, &["get", "app.db", "k"]);
    assert_eq!(alongside.status.code(), Some(0), "{}", stderr(&alongside));
    assert_eq!(alongside.stdout, b"v");
    drop(reader);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_writer_waits_for_a_file_another_program_holds_for_a_moment() {
    let directory =
        std::env::temp_dir().join(format!("eggdb-cli-busy-wait-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("app.db");
    let holder = Database::open(&path).unwrap();
    holder.put("k", "v").unwrap();
    let releasing = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        holder.close().unwrap();
    });
    let write = eggdb(&directory, &["put", "app.db", "k", "w"]);
    releasing.join().unwrap();
    assert_eq!(write.status.code(), Some(0), "{}", stderr(&write));
    let read = eggdb(&directory, &["get", "app.db", "k"]);
    assert_eq!(read.stdout, b"w");

    let reader = Database::open_with(&path, Options::new().read_only(true)).unwrap();
    let started = std::time::Instant::now();
    let refused = eggdb(
        &directory,
        &["put", "app.db", "k", "x", "--busy-timeout", "200"],
    );
    assert_eq!(refused.status.code(), Some(5), "{}", stderr(&refused));
    assert!(started.elapsed() >= std::time::Duration::from_millis(200));
    drop(reader);
    let _ = std::fs::remove_dir_all(&directory);
}

const BUSY_FOR_READING: &str = "eggdb: cannot open app.db: another program has it open for writing. Close it, or wait and run the command again.\n";
const BUSY_FOR_WRITING: &str = "eggdb: cannot open app.db: another program has it open. Close it before writing, or read with `eggdb get`, `scan`, `check` or `stats`, which work alongside readers.\n";

fn waited_then_busy(directory: &std::path::Path, args: &[&str], busy_line: &str) {
    let started = std::time::Instant::now();
    let refused = eggdb(directory, args);
    let waited = started.elapsed();
    assert_eq!(refused.status.code(), Some(5), "{args:?}: {}", stderr(&refused));
    assert!(
        waited >= std::time::Duration::from_millis(200),
        "{args:?} gave up after {waited:?}"
    );
    assert!(
        stderr(&refused).starts_with(busy_line),
        "{args:?}: {}",
        stderr(&refused)
    );
}

#[test]
fn backup_compact_export_restore_and_import_wait_for_the_busy_timeout_then_exit_5() {
    let directory =
        std::env::temp_dir().join(format!("eggdb-cli-busy-commands-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("app.db");
    Database::open(&path).unwrap().put("k", "v").unwrap();
    let exported = eggdb(&directory, &["export", "app.db", "app.jsonl"]);
    assert_eq!(exported.status.code(), Some(0), "{}", stderr(&exported));
    std::fs::write(
        directory.join("dump.sql"),
        "CREATE TABLE t (a int);\nINSERT INTO t VALUES (1);\n",
    )
    .unwrap();

    let writer = Database::open(&path).unwrap();
    waited_then_busy(
        &directory,
        &["backup", "app.db", "busy.bak", "--busy-timeout", "200"],
        BUSY_FOR_READING,
    );
    assert!(!directory.join("busy.bak").exists());
    waited_then_busy(
        &directory,
        &["compact", "app.db", "--busy-timeout", "200"],
        BUSY_FOR_READING,
    );
    waited_then_busy(
        &directory,
        &["export", "app.db", "busy.jsonl", "--busy-timeout", "200"],
        BUSY_FOR_READING,
    );
    writer.close().unwrap();

    let reader = Database::open_with(&path, Options::new().read_only(true)).unwrap();
    waited_then_busy(
        &directory,
        &["restore", "app.db", "dump.sql", "--busy-timeout", "200"],
        BUSY_FOR_WRITING,
    );
    waited_then_busy(
        &directory,
        &["import", "app.db", "app.jsonl", "--busy-timeout", "200"],
        BUSY_FOR_WRITING,
    );
    let at_once = eggdb(
        &directory,
        &["restore", "app.db", "dump.sql", "--busy-timeout", "0"],
    );
    assert_eq!(at_once.status.code(), Some(5), "{}", stderr(&at_once));
    drop(reader);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn backup_compact_restore_and_import_finish_once_the_program_holding_the_file_lets_go() {
    let directory =
        std::env::temp_dir().join(format!("eggdb-cli-busy-released-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("app.db");
    Database::open(&path).unwrap().put("k", "v").unwrap();
    let exported = eggdb(&directory, &["export", "app.db", "app.jsonl"]);
    assert_eq!(exported.status.code(), Some(0), "{}", stderr(&exported));
    std::fs::write(directory.join("dump.sql"), "CREATE TABLE t (a int);\n").unwrap();

    for args in [
        &["restore", "app.db", "dump.sql"][..],
        &["import", "app.db", "app.jsonl", "--on-conflict", "skip"][..],
        &["backup", "app.db", "released.bak"][..],
        &["compact", "app.db"][..],
    ] {
        let holder = Database::open(&path).unwrap();
        let releasing = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            holder.close().unwrap();
        });
        let ran = eggdb(&directory, args);
        releasing.join().unwrap();
        assert_eq!(ran.status.code(), Some(0), "{args:?}: {}", stderr(&ran));
    }
    let read = eggdb(&directory, &["get", "released.bak", "k"]);
    assert_eq!(read.stdout, b"v");
    let _ = std::fs::remove_dir_all(&directory);
}
