//! Online backup and compaction through the embedded API and the `eggdb` command: a backup
//! taken while a writer commits holds exactly one commit, both keep collections, SQL tables,
//! times to live and counters, a compaction shrinks a file with freed pages beside a reader,
//! neither replaces a file it should not, and the command's reports and refusals.

mod common;

use common::TempPath;
use eggdb::{Database, Durability, ErrorKind, Options, SimClock};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

type Entries = BTreeMap<Vec<u8>, Vec<u8>>;

const KEYS: usize = 20_000;
const FIRST_KEY: &str = "k00000";
const LAST_KEY: &str = "k19999";
const OCTOBER_2026_NANOS: u64 = 1_792_000_000_000_000_000;
const TTL: Duration = Duration::from_secs(30);
const MICRO: Duration = Duration::from_micros(1);

fn key(index: usize) -> String {
    format!("k{index:05}")
}

fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

fn default_space(db: &Database) -> Entries {
    db.read(|tx| tx.scan(..).collect()).unwrap()
}

fn collection(db: &Database, name: &str) -> Entries {
    db.read(|tx| tx.collection(name)?.scan(..).collect())
        .unwrap()
}

fn fast() -> Options {
    Options::new().durability(Durability::Normal)
}

#[test]
fn a_backup_taken_while_a_writer_commits_holds_exactly_one_commit() {
    let file = TempPath::new("backup-online");
    let copies: Vec<TempPath> = (0..3)
        .map(|_| TempPath::new("backup-online-copy"))
        .collect();
    let db = Arc::new(Database::open_with(file.path(), fast()).unwrap());
    db.create_collection("c").unwrap();
    db.write(|tx| {
        for index in 0..KEYS {
            tx.put(key(index), "0")?;
        }
        tx.collection("c")?.put("x", "0")?;
        Ok(())
    })
    .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let commits = Arc::new(AtomicU64::new(0));
    let writer = {
        let (db, stop, commits) = (Arc::clone(&db), Arc::clone(&stop), Arc::clone(&commits));
        std::thread::spawn(move || {
            let mut round = 0u64;
            while !stop.load(Ordering::Acquire) {
                round += 1;
                let value = round.to_string();
                db.write(|tx| {
                    tx.put(FIRST_KEY, &value)?;
                    tx.put(LAST_KEY, &value)?;
                    tx.collection("c")?.put("x", &value)?;
                    Ok(())
                })
                .unwrap();
                commits.store(round, Ordering::Release);
            }
        })
    };
    while commits.load(Ordering::Acquire) < 5 {
        std::thread::yield_now();
    }

    let mut commits_during_backups = 0;
    for copy in &copies {
        let before = commits.load(Ordering::Acquire);
        let report = db.backup_to(copy.path()).unwrap();
        commits_during_backups += commits.load(Ordering::Acquire) - before;

        let copied = Database::open_with(copy.path(), Options::new().read_only(true)).unwrap();
        let check = copied.check().unwrap();
        assert_eq!(check.leaked_pages, 0);
        assert_eq!(check.entries, report.entries);
        assert_eq!(copied.stats().unwrap().page_count, report.page_count);
        let entries = default_space(&copied);
        assert_eq!(entries.len(), KEYS);
        let first = entries[FIRST_KEY.as_bytes()].clone();
        assert_eq!(
            entries[LAST_KEY.as_bytes()],
            first,
            "two commits in one copy"
        );
        assert_eq!(
            collection(&copied, "c")[b"x".as_slice()],
            first,
            "two commits in one copy"
        );
        let round: u64 = String::from_utf8(first).unwrap().parse().unwrap();
        assert!(round >= before, "the copy is older than the backup");
    }
    stop.store(true, Ordering::Release);
    writer.join().unwrap();
    assert!(
        commits_during_backups > 0,
        "the writer never committed during a backup"
    );
    assert_eq!(default_space(&db).len(), KEYS);
}

#[test]
fn a_backup_keeps_collections_times_to_live_and_counters() {
    let file = TempPath::new("backup-models");
    let copy = TempPath::new("backup-models-copy");
    let clock = Arc::new(SimClock::new(OCTOBER_2026_NANOS));
    let options = || Options::new().clock(clock.clone());
    let db = Database::open_with(file.path(), options()).unwrap();
    db.create_collection("sessions").unwrap();
    db.write(|tx| {
        tx.put("a", "kept")?;
        tx.put_with_expiry("b", "expiring", TTL)?;
        tx.put_with_expiry("gone", "already expired", MICRO)?;
        tx.increment("hits", 5)?;
        let mut sessions = tx.collection("sessions")?;
        sessions.put_with_expiry("s1", "alice", TTL)?;
        sessions.put("s2", "bob")?;
        Ok(())
    })
    .unwrap();
    clock.advance(TTL / 2);

    let report = db.backup_to(copy.path()).unwrap();
    assert_eq!(report.entries, db.check().unwrap().entries);
    assert_eq!(report.stamp, db.stats().unwrap().last_commit);
    drop(db);

    let copied = Database::open_with(copy.path(), options()).unwrap();
    assert!(copied.stats().unwrap().expiring_keys);
    assert_eq!(copied.get("a").unwrap(), Some(b"kept".to_vec()));
    assert_eq!(copied.get("gone").unwrap(), None);
    assert_eq!(copied.time_to_live("b").unwrap(), Some(TTL / 2));
    assert_eq!(
        copied.collection("sessions").time_to_live("s1").unwrap(),
        Some(TTL / 2)
    );
    assert_eq!(
        copied.collection("sessions").get("s2").unwrap(),
        Some(b"bob".to_vec())
    );
    assert_eq!(copied.increment("hits", 1).unwrap(), 6);
    assert_eq!(copied.collection_names().unwrap(), ["sessions"]);
    clock.advance(TTL / 2);
    assert_eq!(copied.get("b").unwrap(), None);
    assert_eq!(copied.collection("sessions").get("s1").unwrap(), None);
    copied.close().unwrap();
}

#[test]
fn a_backup_never_replaces_a_file_and_leaves_a_staging_file_it_did_not_make() {
    let file = TempPath::new("backup-exists");
    let copy = TempPath::new("backup-exists-copy");
    let db = Database::open(file.path()).unwrap();
    db.put("k", "v").unwrap();

    std::fs::write(copy.path(), b"someone's file").unwrap();
    let refused = db.backup_to(copy.path()).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::AlreadyExists);
    assert!(
        refused.to_string().contains("never replaces a file"),
        "{refused}"
    );
    assert_eq!(std::fs::read(copy.path()).unwrap(), b"someone's file");

    let refused = db.backup_to(file.path()).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::AlreadyExists);

    std::fs::remove_file(copy.path()).unwrap();
    let staging = beside(copy.path(), ".backing-up");
    std::fs::write(&staging, b"a backup still running").unwrap();
    let refused = db.backup_to(copy.path()).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::AlreadyExists);
    assert!(
        refused.to_string().contains(".backing-up exists"),
        "{refused}"
    );
    assert_eq!(std::fs::read(&staging).unwrap(), b"a backup still running");
    assert!(!copy.path().exists());
    std::fs::remove_file(&staging).unwrap();

    db.backup_to(copy.path()).unwrap();
    assert_eq!(
        Database::open(copy.path()).unwrap().get("k").unwrap(),
        Some(b"v".to_vec())
    );
}

#[test]
fn a_compacted_file_holds_every_key_in_fewer_pages() {
    let file = TempPath::new("compact-shrinks");
    let db = Database::open_with(file.path(), fast()).unwrap();
    db.create_collection("users").unwrap();
    db.write(|tx| {
        for index in 0..5000 {
            tx.put(key(index), vec![index as u8; 200])?;
        }
        let mut users = tx.collection("users")?;
        users.put("u:1", "Ada")?;
        users.put_with_expiry("u:2", "Grace", Duration::from_secs(3600))?;
        Ok(())
    })
    .unwrap();
    db.write(|tx| {
        for index in (0..5000).filter(|index| index % 20 != 0) {
            tx.delete(key(index))?;
        }
        Ok(())
    })
    .unwrap();
    let expected = default_space(&db);
    let users = collection(&db, "users");
    let entries = db.check().unwrap().entries;
    db.close().unwrap();

    let report = Database::compact_file(file.path()).unwrap();

    assert_eq!(report.entries, entries);
    assert!(report.free_pages_before > 0, "{report:?}");
    assert_eq!(report.leaked_pages_before, 0);
    assert!(report.pages_after < report.pages_before / 2, "{report:?}");
    assert!(report.bytes_after < report.bytes_before);
    assert!(!beside(file.path(), ".compacting").exists());
    let db = Database::open(file.path()).unwrap();
    let check = db.check().unwrap();
    assert_eq!(check.leaked_pages, 0);
    assert_eq!(check.entries, entries);
    assert_eq!(db.stats().unwrap().page_count, report.pages_after);
    assert_eq!(default_space(&db), expected);
    assert_eq!(collection(&db, "users"), users);
    assert!(
        db.collection("users")
            .time_to_live("u:2")
            .unwrap()
            .is_some()
    );
    db.put("after", "compaction").unwrap();
    db.close().unwrap();
}

#[test]
fn a_compaction_runs_beside_a_reader_and_is_refused_by_a_writer() {
    let file = TempPath::new("compact-locks");
    let db = Database::open(file.path()).unwrap();
    db.write(|tx| {
        for index in 0..500 {
            tx.put(key(index), "value")?;
        }
        Ok(())
    })
    .unwrap();

    let refused = Database::compact_file(file.path()).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Locked);
    db.close().unwrap();

    let reader = Database::open_with(file.path(), Options::new().read_only(true)).unwrap();
    let report = Database::compact_file(file.path()).unwrap();
    assert_eq!(report.entries, 500);
    assert_eq!(reader.get(key(7)).unwrap(), Some(b"value".to_vec()));
    assert_eq!(default_space(&reader).len(), 500);
    drop(reader);

    let db = Database::open(file.path()).unwrap();
    assert_eq!(default_space(&db).len(), 500);
    db.close().unwrap();
}

#[test]
fn a_compaction_left_by_a_crash_is_refused_until_its_file_is_removed() {
    let file = TempPath::new("compact-leftover");
    Database::open(file.path()).unwrap().put("k", "v").unwrap();
    let staging = beside(file.path(), ".compacting");
    std::fs::write(&staging, b"left by a crash").unwrap();

    let refused = Database::compact_file(file.path()).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::AlreadyExists);
    assert!(
        refused.to_string().contains(".compacting exists"),
        "{refused}"
    );
    assert_eq!(std::fs::read(&staging).unwrap(), b"left by a crash");
    assert_eq!(
        Database::open(file.path()).unwrap().get("k").unwrap(),
        Some(b"v".to_vec())
    );

    std::fs::remove_file(&staging).unwrap();
    Database::compact_file(file.path()).unwrap();
    assert_eq!(
        Database::open(file.path()).unwrap().get("k").unwrap(),
        Some(b"v".to_vec())
    );
}

struct Directory {
    path: PathBuf,
}

impl Directory {
    fn new(name: &str) -> Self {
        let file = TempPath::new(name);
        let path = file.path().with_extension("dir");
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn eggdb(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_eggdb"))
            .args(args)
            .current_dir(&self.path)
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    }

    fn eggdb_ok(&self, args: &[&str]) -> String {
        let output = self.eggdb(args);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{args:?}: {}",
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[track_caller]
fn assert_refused(output: &Output, code: i32, stderr: &str) {
    assert_eq!(output.status.code(), Some(code), "{}", text(&output.stderr));
    assert_eq!(text(&output.stderr), stderr);
    assert!(output.stdout.is_empty(), "{}", text(&output.stdout));
}

fn sample_database(directory: &Directory) {
    directory.eggdb_ok(&[
        "sql",
        "app.db",
        "--create",
        "-c",
        "CREATE TABLE users (id integer PRIMARY KEY, name text); INSERT INTO users VALUES (1, 'Ada'), (2, 'Grace')",
    ]);
    directory.eggdb_ok(&["collection", "add", "app.db", "sessions"]);
    directory.eggdb_ok(&[
        "put", "app.db", "-C", "sessions", "s:1", "alice", "--ttl", "1h",
    ]);
    directory.eggdb_ok(&["incr", "app.db", "hits", "41"]);
}

#[test]
fn eggdb_backup_copies_every_model_and_reports_in_text_and_json() {
    let directory = Directory::new("backup-cli");
    sample_database(&directory);

    let json = directory.eggdb_ok(&["backup", "app.db", "app.bak"]);
    assert!(
        json.starts_with(r#"{"ok":true,"file":"app.db","copy":"app.bak","commit_stamp":""#),
        "{json}"
    );
    assert!(
        json.contains(r#","page_size":4096,"copy_pages":"#),
        "{json}"
    );
    assert!(
        json.contains(r#","elapsed_ms":"#) && json.ends_with("}\n"),
        "{json}"
    );
    assert!(!directory.join("app.bak.backing-up").exists());
    let names = directory.eggdb_ok(&["sql", "app.bak", "-c", "SELECT name FROM users ORDER BY id"]);
    assert!(names.contains("Ada") && names.contains("Grace"), "{names}");
    assert_eq!(
        directory.eggdb_ok(&["get", "app.bak", "-C", "sessions", "s:1"]),
        "alice"
    );
    assert_eq!(directory.eggdb_ok(&["get", "app.bak", "hits"]), "41");
    let ttl = directory.eggdb_ok(&[
        "ttl", "app.bak", "-C", "sessions", "s:1", "--format", "text",
    ]);
    assert!(ttl.starts_with("59m") || ttl == "1h\n", "{ttl}");

    let report = directory.eggdb_ok(&["backup", "app.db", "text.bak", "--format", "text"]);
    let lines: Vec<&str> = report.lines().collect();
    assert_eq!(lines.len(), 5, "{report}");
    assert!(
        lines[0].starts_with("backed up app.db to text.bak as of commit 20"),
        "{report}"
    );
    assert!(lines[0].ends_with(')'), "{report}");
    assert!(lines[1].starts_with("  entries         "), "{report}");
    assert!(
        lines[2].starts_with("  copy            ") && lines[2].ends_with(" of 4 KiB"),
        "{report}"
    );
    assert!(lines[3].starts_with("  time            "), "{report}");
    assert!(
        lines[4].starts_with("ok: text.bak is complete: its "),
        "{report}"
    );
    assert!(
        lines[4].ends_with(" entries were counted again after writing and syncing."),
        "{report}"
    );
}

#[test]
fn eggdb_backup_refuses_what_it_must_not_write_before_opening_the_file() {
    let directory = Directory::new("backup-cli-refusals");
    sample_database(&directory);
    directory.eggdb_ok(&["backup", "app.db", "app.bak"]);

    assert_refused(
        &directory.eggdb(&["backup", "app.db", "app.bak"]),
        3,
        "eggdb: cannot back up app.db to app.bak: app.bak already exists; eggdb backup never replaces a file. Choose another name, or remove it first.\n",
    );
    assert_refused(
        &directory.eggdb(&["backup", "app.db", "app.db"]),
        3,
        "eggdb: cannot back up app.db to itself. Give the copy another name.\n",
    );
    assert_refused(
        &directory.eggdb(&["backup", "app.db", "missing/app.bak"]),
        3,
        "eggdb: cannot back up app.db to missing/app.bak: the directory missing does not exist. Create it first.\n  try: mkdir missing\n",
    );
    std::fs::write(directory.join("other.bak.backing-up"), b"left").unwrap();
    assert_refused(
        &directory.eggdb(&["backup", "app.db", "other.bak"]),
        3,
        "eggdb: cannot back up app.db to other.bak: other.bak.backing-up exists, left by a backup that did not finish or one still running. Remove it and back up again.\n",
    );
    assert_eq!(
        std::fs::read(directory.join("other.bak.backing-up")).unwrap(),
        b"left"
    );
    assert_refused(
        &directory.eggdb(&["backup", "app.db", "-"]),
        2,
        "eggdb: backup writes a file it can sync and verify, and stdout is not one. Give a path.\n  usage: eggdb backup <file> <copy>\n  try: eggdb help backup\n",
    );
    assert_refused(
        &directory.eggdb(&["backup", "missing.db", "fresh.bak"]),
        3,
        "eggdb: cannot open missing.db: no such file. Check the path, or create it with `eggdb create missing.db`\n",
    );
    assert!(!directory.join("fresh.bak").exists());
}

#[test]
fn eggdb_backup_and_compact_are_busy_under_a_writer_and_backup_works_in_the_shell() {
    let directory = Directory::new("backup-cli-busy");
    sample_database(&directory);
    let writer = Database::open(directory.join("app.db")).unwrap();

    let backup = directory.eggdb(&["backup", "app.db", "busy.bak"]);
    assert_eq!(backup.status.code(), Some(5), "{}", text(&backup.stderr));
    assert!(
        text(&backup.stderr).ends_with("\n  to copy a file while a program writes to it, use backup <copy> inside eggdb shell, or Database::backup_to in the program.\n"),
        "{}",
        text(&backup.stderr)
    );
    assert!(!directory.join("busy.bak").exists());
    let compact = directory.eggdb(&["compact", "app.db"]);
    assert_eq!(compact.status.code(), Some(5), "{}", text(&compact.stderr));
    assert!(!directory.join("app.db.compacting").exists());
    drop(writer);

    let shell = directory.eggdb(&["shell", "app.db", "-c", "backup shell.bak"]);
    assert_eq!(shell.status.code(), Some(0), "{}", text(&shell.stderr));
    assert!(
        text(&shell.stdout).starts_with("backed up app.db to shell.bak as of commit "),
        "{}",
        text(&shell.stdout)
    );
    assert_eq!(
        directory.eggdb_ok(&["get", "shell.bak", "-C", "sessions", "s:1"]),
        "alice"
    );
    let shell = directory.eggdb(&["shell", "app.db", "-c", "backup"]);
    assert_eq!(shell.status.code(), Some(2));
    assert!(
        text(&shell.stderr).contains("backup takes one argument, the file to write"),
        "{}",
        text(&shell.stderr)
    );
    let shell = directory.eggdb(&["shell", "app.db", "-c", "compact"]);
    assert_eq!(shell.status.code(), Some(2));
    assert_eq!(
        text(&shell.stderr),
        "eggdb: compact needs the file to itself, and this shell holds it. Quit the shell, then run it from the command line.\n  try: eggdb compact app.db\n"
    );
}

#[test]
fn eggdb_compact_reclaims_pages_and_keeps_every_model() {
    let directory = Directory::new("compact-cli");
    sample_database(&directory);
    let db = Database::open(directory.join("app.db")).unwrap();
    db.write(|tx| {
        for index in 0..3000 {
            tx.put(key(index), [9u8; 120])?;
        }
        Ok(())
    })
    .unwrap();
    db.write(|tx| {
        for index in 0..3000 {
            tx.delete(key(index))?;
        }
        Ok(())
    })
    .unwrap();
    db.close().unwrap();

    let report = directory.eggdb_ok(&["compact", "app.db", "--format", "text"]);
    let lines: Vec<&str> = report.lines().collect();
    assert_eq!(lines.len(), 7, "{report}");
    assert!(
        lines[0].starts_with("compacted app.db as of commit 20"),
        "{report}"
    );
    assert!(lines[1].starts_with("  entries         "), "{report}");
    assert!(
        lines[2].starts_with("  before          ") && lines[2].ends_with(" of 4 KiB"),
        "{report}"
    );
    assert!(lines[3].starts_with("  after           "), "{report}");
    assert!(
        lines[4].starts_with("  reclaimed       ") && lines[4].ends_with(" leaked)"),
        "{report}"
    );
    assert!(lines[5].starts_with("  time            "), "{report}");
    assert!(
        lines[6].starts_with(
            "ok: app.db was rewritten without free or leaked pages and swapped in whole; "
        ),
        "{report}"
    );
    assert!(!directory.join("app.db.compacting").exists());

    let json = directory.eggdb_ok(&["compact", "app.db"]);
    assert!(
        json.starts_with(r#"{"ok":true,"file":"app.db","commit_stamp":""#),
        "{json}"
    );
    assert!(json.contains(r#","leaked_pages_before":0,"#), "{json}");
    let names = directory.eggdb_ok(&["sql", "app.db", "-c", "SELECT name FROM users ORDER BY id"]);
    assert!(names.contains("Ada") && names.contains("Grace"), "{names}");
    assert_eq!(
        directory.eggdb_ok(&["get", "app.db", "-C", "sessions", "s:1"]),
        "alice"
    );
    assert_eq!(directory.eggdb_ok(&["get", "app.db", "hits"]), "41");
    let check = directory.eggdb_ok(&["check", "app.db"]);
    assert!(check.contains(r#""leaked_pages":0}"#), "{check}");

    std::fs::write(directory.join("app.db.compacting"), b"left").unwrap();
    assert_refused(
        &directory.eggdb(&["compact", "app.db"]),
        3,
        "eggdb: cannot compact app.db: app.db.compacting exists, left by a compact that did not finish or one still running. Remove it and compact again.\n",
    );
}
