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
