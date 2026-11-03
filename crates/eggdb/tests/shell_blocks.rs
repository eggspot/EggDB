//! `eggdb shell` run as a real process with piped input: key-value commands inside a `BEGIN`
//! block (`expire`, `persist`, `cas`, `scan`, `ttl` with `put`, `get` and `del`) see the block's
//! own writes, and `COMMIT` keeps them with the block's SQL while `ROLLBACK` drops them all.

mod common;

use common::TempPath;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn eggdb(args: &[&str], file: &Path, stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .arg(args[0])
        .arg(file)
        .args(&args[1..])
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

fn created(name: &str) -> TempPath {
    let file = TempPath::new(name);
    let made = eggdb(&["create"], file.path(), b"");
    assert_eq!(made.status.code(), Some(0), "{}", text(&made.stderr));
    file
}

fn stdout_of(args: &[&str], file: &Path) -> String {
    let ran = eggdb(args, file, b"");
    assert_eq!(ran.status.code(), Some(0), "{}", text(&ran.stderr));
    text(&ran.stdout)
}

#[test]
fn a_committed_block_keeps_its_expiry_swap_and_sql_together() {
    let file = created("shell-block-commit");
    let script = "create table orders (id integer primary key, item text);\n\
        put lock free\n\
        put cart:1 egg\n\
        begin;\n\
        insert into orders values (1, 'egg');\n\
        cas lock free taken\n\
        put cart:2 ham\n\
        expire cart:1 1h\n\
        ttl cart:1\n\
        scan --prefix cart: -k\n\
        commit;\n";
    let ran = eggdb(&["shell"], file.path(), script.as_bytes());
    assert_eq!(ran.status.code(), Some(0), "{}", text(&ran.stderr));
    assert_eq!(
        text(&ran.stdout),
        "CREATE TABLE\nBEGIN\nINSERT 0 1\n1h\ncart:1\ncart:2\nCOMMIT\n"
    );
    assert!(
        text(&ran.stderr).contains("2 entries"),
        "{}",
        text(&ran.stderr)
    );
    assert_eq!(stdout_of(&["get", "lock"], file.path()), "taken");
    let left = stdout_of(&["ttl", "cart:1", "--format", "text"], file.path());
    assert!(left.starts_with("59m") || left == "1h\n", "{left}");
    assert_eq!(
        stdout_of(&["sql", "-c", "select item from orders;"], file.path())
            .lines()
            .filter(|line| line.trim() == "egg")
            .count(),
        1
    );
}

#[test]
fn a_rolled_back_block_drops_its_expiry_swap_deletes_and_sql() {
    let file = created("shell-block-rollback");
    let script = "create table orders (id integer primary key, item text);\n\
        put lock free\n\
        put cart:1 egg\n\
        put cart:3 jam\n\
        begin;\n\
        insert into orders values (1, 'egg');\n\
        cas lock free taken\n\
        get lock\n\
        expire cart:1 1h\n\
        del cart:3\n\
        scan --to m -k\n\
        rollback;\n";
    let ran = eggdb(&["shell"], file.path(), script.as_bytes());
    assert_eq!(ran.status.code(), Some(0), "{}", text(&ran.stderr));
    assert_eq!(
        text(&ran.stdout),
        "CREATE TABLE\nBEGIN\nINSERT 0 1\ntaken\ncart:1\nlock\nROLLBACK\n"
    );
    assert_eq!(stdout_of(&["get", "lock"], file.path()), "free");
    assert_eq!(
        stdout_of(&["ttl", "cart:1", "--format", "text"], file.path()),
        "never\n"
    );
    assert_eq!(stdout_of(&["get", "cart:3"], file.path()), "jam");
    let counted = stdout_of(
        &["sql", "-c", "select count(*) as n from orders;"],
        file.path(),
    );
    assert!(counted.contains(" 0\n"), "{counted}");
}
