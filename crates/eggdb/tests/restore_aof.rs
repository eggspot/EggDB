//! `eggdb restore` of Redis append-only files as a real process: a multi-part `appendonlydir/`
//! replays its base file and then its incremental files in sequence order, skips history files,
//! and refuses RDB preambles and hostile or broken manifests without touching the database.

use eggdb::Database;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT_DIRECTORY: AtomicU32 = AtomicU32::new(0);

struct TempDirectory {
    path: PathBuf,
}

impl TempDirectory {
    fn new(name: &str) -> Self {
        let number = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "eggdb-restore-aof-test-{name}-{}-{number}",
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

fn eggdb(args: &[&Path]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .arg("restore")
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn command(arguments: &[&str]) -> Vec<u8> {
    let mut request = format!("*{}\r\n", arguments.len()).into_bytes();
    for argument in arguments {
        request.extend_from_slice(format!("${}\r\n{argument}\r\n", argument.len()).as_bytes());
    }
    request
}

fn commands(list: &[&[&str]]) -> Vec<u8> {
    list.iter().flat_map(|arguments| command(arguments)).collect()
}

fn write_multi_part(directory: &Path, manifest: &str) {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::write(directory.join("appendonly.aof.manifest"), manifest).unwrap();
    std::fs::write(
        directory.join("appendonly.aof.1.base.aof"),
        commands(&[&["SELECT", "0"], &["SET", "base", "1"], &["SET", "counter", "10"]]),
    )
    .unwrap();
    std::fs::write(
        directory.join("appendonly.aof.1.incr.aof"),
        commands(&[&["SELECT", "2"], &["SET", "moved", "x"]]),
    )
    .unwrap();
    std::fs::write(
        directory.join("appendonly.aof.2.incr.aof"),
        commands(&[&["SET", "still-db-2", "y"], &["SELECT", "0"], &["INCR", "counter"]]),
    )
    .unwrap();
    std::fs::write(
        directory.join("appendonly.aof.0.base.aof"),
        commands(&[&["SET", "history", "never"]]),
    )
    .unwrap();
}

const MANIFEST: &str = "file appendonly.aof.0.base.aof seq 0 type h\n\
                        file appendonly.aof.2.incr.aof seq 2 type i\n\
                        file appendonly.aof.1.base.aof seq 1 type b\n\
                        file appendonly.aof.1.incr.aof seq 1 type i\n";

fn present(database: &Path, collection: &str, key: &str) -> bool {
    let db = Database::open(database).unwrap();
    let transaction = db.begin_read().unwrap();
    let Ok(collection) = transaction.collection(collection) else {
        return false;
    };
    collection.get(key).unwrap().is_some()
}

#[test]
fn a_multi_part_append_only_directory_restores_in_sequence_order() {
    let temp = TempDirectory::new("multi-part");
    let directory = temp.join("appendonlydir");
    write_multi_part(&directory, MANIFEST);
    let database = temp.join("app.eggdb");
    let restored = eggdb(&[&database, &directory]);
    assert!(restored.status.success(), "{}", text(&restored.stderr));
    let shown = directory.display();
    assert_eq!(
        text(&restored.stdout),
        format!(
            "restored {shown} (Redis multi-part append-only file, 3 files): 8 commands, 0 failed\n"
        )
    );
    assert!(present(&database, "resp0", "base"));
    assert!(present(&database, "resp0", "counter"));
    assert!(present(&database, "resp2", "moved"));
    assert!(present(&database, "resp2", "still-db-2"));
    assert!(!present(&database, "resp0", "history"));

    let by_manifest = temp.join("by-manifest.eggdb");
    let restored = eggdb(&[&by_manifest, &directory.join("appendonly.aof.manifest")]);
    assert!(restored.status.success(), "{}", text(&restored.stderr));
    assert!(present(&by_manifest, "resp2", "moved"));
}

#[test]
fn an_rdb_preamble_is_refused_with_the_way_out() {
    let temp = TempDirectory::new("rdb");
    let directory = temp.join("appendonlydir");
    write_multi_part(
        &directory,
        "file appendonly.aof.1.base.rdb seq 1 type b\nfile appendonly.aof.1.incr.aof seq 1 type i\n",
    );
    std::fs::write(
        directory.join("appendonly.aof.1.base.rdb"),
        b"REDIS0011\xfa\x09redis-ver\x057.2.4",
    )
    .unwrap();
    let database = temp.join("app.eggdb");
    let refused = eggdb(&[&database, &directory]);
    assert!(!refused.status.success());
    let message = text(&refused.stderr);
    assert!(message.contains("appendonly.aof.1.base.rdb"), "{message}");
    assert!(message.contains("RDB snapshot"), "{message}");
    assert!(message.contains("aof-use-rdb-preamble no"), "{message}");
}

#[test]
fn hostile_and_broken_manifests_are_refused_before_anything_is_written() {
    let cases = [
        ("traversal", "file ../secret.aof seq 1 type i\n", "not a plain file name"),
        ("absolute", "file /etc/passwd seq 1 type b\n", "not a plain file name"),
        (
            "huge-seq",
            "file appendonly.aof.1.incr.aof seq 184467440737095516160 type i\n",
            "seq",
        ),
        (
            "missing",
            "file appendonly.aof.1.base.aof seq 1 type b\nfile appendonly.aof.9.incr.aof seq 9 type i\n",
            "appendonly.aof.9.incr.aof, which cannot be read",
        ),
        ("empty", "# nothing\n", "no base or incremental file"),
    ];
    for (name, manifest, expected) in cases {
        let temp = TempDirectory::new(name);
        let directory = temp.join("appendonlydir");
        write_multi_part(&directory, manifest);
        let database = temp.join("app.eggdb");
        let refused = eggdb(&[&database, &directory]);
        assert!(!refused.status.success(), "{name}");
        let message = text(&refused.stderr);
        assert!(message.contains(expected), "{name}: {message}");
        assert!(!database.exists(), "{name}");
    }
}
