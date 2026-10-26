//! The file lock seen from another process, in a test binary of its own, so that starting the
//! child holds no other test's database file open.

mod common;

use common::TempPath;
use eggdb::{Database, ErrorKind};
use std::process::Command;

const LOCK_PROBE: &str = "EGGDB_TEST_LOCK_PROBE";

#[test]
fn another_process_cannot_open_a_file_this_process_has_open() {
    let file = TempPath::new("locked-process");
    let _db = Database::open(file.path()).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lock_probe_from_another_process", "--nocapture"])
        .env(LOCK_PROBE, file.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("1 passed"), "{stdout}");
}

#[test]
fn lock_probe_from_another_process() {
    let Some(path) = std::env::var_os(LOCK_PROBE) else {
        return;
    };
    let error = Database::open(&path).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Locked, "{error}");
}
