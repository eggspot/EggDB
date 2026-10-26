//! Files written by released versions of EggDB, which every later version must keep reading
//! (D20, D96). Each sample lives in `tests/data` and is never regenerated; a new release adds
//! its own. `docs/testing.md` says how each one was made.

use eggdb::{Database, Options};
use eggdb_platform::OsFileSystem;
use eggdb_storage::{Access, PageFile};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

type Entries = BTreeMap<Vec<u8>, Vec<u8>>;
type Change = (Vec<u8>, Option<Vec<u8>>);

const CHECKPOINTED_SAMPLE: &str = "v0.2.0.db";
const LOG_SAMPLE: &str = "v0.2.0-log-to-replay.db";
const EXPORT_SAMPLE: &str = "v0.2.0.jsonl";

const LONGEST_KEY_BYTES: usize = 512;
const LARGE_VALUE_LINE: &str = "EggDB v0.2.0 sample, large value\n";
const LARGE_VALUE_BYTES: usize = 6000;
const LOG_LARGE_VALUE_BYTES: usize = 5000;

static NEXT_DIRECTORY: AtomicU32 = AtomicU32::new(0);

struct TempDirectory {
    path: PathBuf,
}

impl TempDirectory {
    fn new(name: &str) -> Self {
        let number = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "eggdb-sample-test-{name}-{}-{number}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn copy_of(&self, sample: &str) -> PathBuf {
        let copy = self.path.join(sample);
        std::fs::copy(sample_path(sample), &copy).unwrap();
        copy
    }

    fn eggdb(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_eggdb"))
            .args(args)
            .current_dir(&self.path)
            .output()
            .unwrap()
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn sample_path(sample: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(sample)
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn repeated_line(line: &str, bytes: usize) -> Vec<u8> {
    line.bytes().cycle().take(bytes).collect()
}

fn v0_2_0_entries() -> Entries {
    let mut entries = Entries::new();
    let mut put = |key: &[u8], value: &[u8]| {
        entries.insert(key.to_vec(), value.to_vec());
    };
    put(b"", b"the empty key");
    put(&[0x00, 0xff, 0x10], &[0xde, 0xad, 0xbe, 0xef]);
    put(
        b"doc:json",
        br#"{"name":"Ada Lovelace","born":1815,"tags":["math","poetry"]}"#,
    );
    put(b"empty", b"");
    put(
        b"large",
        &repeated_line(LARGE_VALUE_LINE, LARGE_VALUE_BYTES),
    );
    for n in 1..=29 {
        put(
            format!("user:{n}").as_bytes(),
            format!("user number {n}").as_bytes(),
        );
    }
    put(b"user:2", b"Grace Hopper");
    put("名前".as_bytes(), "エッグ".as_bytes());
    put(
        &vec![b'k'; LONGEST_KEY_BYTES],
        b"the longest key with 4 KiB pages",
    );
    for n in (100..200).chain(250..400) {
        put(
            format!("item:{n}").as_bytes(),
            format!("item {n} of the v0.2.0 sample file").as_bytes(),
        );
    }
    entries
}

fn log_sample_commits() -> Vec<Vec<Change>> {
    vec![
        vec![
            (
                b"log:1".to_vec(),
                Some(b"first commit after the checkpoint".to_vec()),
            ),
            (b"user:1".to_vec(), Some(b"Ada Lovelace".to_vec())),
            (b"item:100".to_vec(), None),
        ],
        vec![
            (
                b"log:large".to_vec(),
                Some(repeated_line(
                    "replayed from the log\n",
                    LOG_LARGE_VALUE_BYTES,
                )),
            ),
            (b"large".to_vec(), None),
        ],
        vec![(b"log:2".to_vec(), Some(b"the last commit".to_vec()))],
    ]
}

fn v0_2_0_log_sample_entries() -> Entries {
    let mut entries = v0_2_0_entries();
    for commit in log_sample_commits() {
        for (key, value) in commit {
            match value {
                Some(value) => entries.insert(key, value),
                None => entries.remove(&key),
            };
        }
    }
    entries
}

fn scan_all(db: &Database) -> Entries {
    db.read(|tx| tx.scan(..).collect::<eggdb::Result<Entries>>())
        .unwrap()
}

fn scan_all_reversed(db: &Database) -> Vec<(Vec<u8>, Vec<u8>)> {
    db.read(|tx| tx.scan(..).rev().collect::<eggdb::Result<Vec<_>>>())
        .unwrap()
}

fn assert_holds(db: &Database, expected: &Entries) {
    assert_eq!(&scan_all(db), expected);
    let mut reversed = scan_all_reversed(db);
    reversed.reverse();
    assert!(reversed.into_iter().eq(expected.clone()));
    for (key, value) in expected {
        assert_eq!(db.get(key).unwrap().as_ref(), Some(value), "key {key:?}");
    }
    assert_eq!(db.get("user:30").unwrap(), None);
    assert_eq!(db.get("removed").unwrap(), None);
    assert_eq!(db.get("item:200").unwrap(), None);
}

#[test]
fn the_sample_files_are_the_bytes_v0_2_0_wrote() {
    for (sample, bytes, fingerprint) in [
        (CHECKPOINTED_SAMPLE, 188_416, 0x64c9_9726_6632_bfd0_u64),
        (LOG_SAMPLE, 188_416, 0x2a51_8c3b_3941_9d2d),
        (EXPORT_SAMPLE, 24_377, 0x38ea_0f5b_22ba_aa1e),
    ] {
        let contents = std::fs::read(sample_path(sample)).unwrap();
        assert_eq!(
            (contents.len(), fnv1a_64(&contents)),
            (bytes, fingerprint),
            "{sample} changed: sample files are never regenerated, only added to (D96)"
        );
    }
}

#[test]
fn a_v0_2_0_file_reads_back_every_entry_and_passes_its_check() {
    let dir = TempDirectory::new("read");
    let path = dir.copy_of(CHECKPOINTED_SAMPLE);
    let db = Database::open_with(&path, Options::new().read_only(true)).unwrap();
    let expected = v0_2_0_entries();
    assert_holds(&db, &expected);
    assert_eq!(
        db.read(|tx| Ok(tx.scan_prefix("user:").count())).unwrap(),
        29
    );

    let report = db.check().unwrap();
    assert_eq!(report.entries, expected.len() as u64);
    assert_eq!(report.tree_height, 2);
    assert_eq!(report.overflow_values, 1);
    assert_eq!(report.overflow_pages, 2);
    assert_eq!(report.leaked_pages, 0);

    let stats = db.stats().unwrap();
    assert_eq!(stats.page_size, 4096);
    assert_eq!(stats.page_count, 46);
    assert_eq!(stats.log_bytes_since_checkpoint, 0);
    assert_eq!(db.max_key_bytes(), LONGEST_KEY_BYTES);
    db.close().unwrap();
}

#[test]
fn a_v0_2_0_file_takes_new_commits_and_reopens() {
    let dir = TempDirectory::new("write");
    let path = dir.copy_of(CHECKPOINTED_SAMPLE);
    let mut expected = v0_2_0_entries();
    let db = Database::open(&path).unwrap();
    let before = db.stats().unwrap().last_commit;
    let large = repeated_line("written by a later version\n", 20_000);
    let mut tx = db.begin_write().unwrap();
    tx.put("after:v0.2.0", "a new key").unwrap();
    tx.put("user:3", "Margaret Hamilton").unwrap();
    tx.put("large", &large).unwrap();
    assert!(tx.delete("item:399").unwrap());
    assert!(tx.commit().unwrap() > before);
    db.close().unwrap();

    expected.insert(b"after:v0.2.0".to_vec(), b"a new key".to_vec());
    expected.insert(b"user:3".to_vec(), b"Margaret Hamilton".to_vec());
    expected.insert(b"large".to_vec(), large);
    expected.remove(b"item:399".as_slice());
    let db = Database::open(&path).unwrap();
    assert_holds(&db, &expected);
    assert_eq!(db.check().unwrap().leaked_pages, 0);
    db.close().unwrap();
}

#[test]
fn a_v0_2_0_file_with_a_log_to_replay_recovers_every_commit() {
    let dir = TempDirectory::new("log");
    let expected = v0_2_0_log_sample_entries();

    let path = dir.copy_of(LOG_SAMPLE);
    let file = PageFile::open(&OsFileSystem::new(), &path, Access::ReadOnly).unwrap();
    assert_eq!(file.log_stats().replayed_commits, 3);
    drop(file);

    let db = Database::open_with(&path, Options::new().read_only(true)).unwrap();
    assert_holds(&db, &expected);
    assert_eq!(db.check().unwrap().leaked_pages, 0);
    drop(db);

    let db = Database::open(&path).unwrap();
    assert_holds(&db, &expected);
    db.checkpoint().unwrap();
    assert_eq!(db.stats().unwrap().log_bytes_since_checkpoint, 0);
    db.close().unwrap();
    let db = Database::open_with(&path, Options::new().read_only(true)).unwrap();
    assert_holds(&db, &expected);
}

#[test]
fn the_eggdb_command_reads_a_v0_2_0_file() {
    let dir = TempDirectory::new("cli");
    dir.copy_of(CHECKPOINTED_SAMPLE);

    let check = dir.eggdb(&["check", CHECKPOINTED_SAMPLE, "--format", "json"]);
    assert_eq!(check.status.code(), Some(0), "{check:?}");
    let report = String::from_utf8(check.stdout).unwrap();
    assert!(report.starts_with("{\"ok\":true,"), "{report}");
    assert!(report.contains("\"entries\":286,"), "{report}");

    let get = dir.eggdb(&["get", CHECKPOINTED_SAMPLE, "user:2"]);
    assert_eq!(get.status.code(), Some(0));
    assert_eq!(get.stdout, b"Grace Hopper");

    let scan = dir.eggdb(&[
        "scan",
        CHECKPOINTED_SAMPLE,
        "--prefix",
        "user:",
        "--keys-only",
    ]);
    assert_eq!(scan.status.code(), Some(0));
    assert_eq!(String::from_utf8(scan.stdout).unwrap().lines().count(), 29);
}

#[test]
fn a_v0_2_0_export_imports_into_a_new_file() {
    let dir = TempDirectory::new("import");
    dir.copy_of(EXPORT_SAMPLE);
    let import = dir.eggdb(&["import", "--create", "imported.db", EXPORT_SAMPLE]);
    assert_eq!(import.status.code(), Some(0), "{import:?}");

    let db =
        Database::open_with(dir.path.join("imported.db"), Options::new().read_only(true)).unwrap();
    assert_holds(&db, &v0_2_0_entries());
}

#[test]
fn a_sample_file_is_never_overwritten_by_its_writer() {
    assert_eq!(
        write_the_log_sample(&sample_path(LOG_SAMPLE))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
}

#[test]
#[ignore = "writes tests/data/v0.2.0-log-to-replay.db once; samples are never regenerated (D96)"]
fn write_the_v0_2_0_log_sample() {
    write_the_log_sample(&sample_path(LOG_SAMPLE)).unwrap();
}

fn write_the_log_sample(target: &Path) -> std::io::Result<()> {
    if target.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "{} exists; sample files are never regenerated (D96)",
                target.display()
            ),
        ));
    }
    let building = target.with_extension("building");
    std::fs::copy(sample_path(CHECKPOINTED_SAMPLE), &building)?;
    let db = Database::open_with(&building, Options::new().auto_checkpoint_log_bytes(None))?;
    for commit in log_sample_commits() {
        db.write(|tx| {
            for (key, value) in &commit {
                match value {
                    Some(value) => tx.put(key, value)?,
                    None => {
                        tx.delete(key)?;
                    }
                }
            }
            Ok(())
        })?;
    }
    drop(db);
    std::fs::rename(&building, target)?;
    Ok(())
}
