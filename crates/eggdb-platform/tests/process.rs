//! Waiting for a child process reports its exit code and its peak memory, checked against
//! children of this test binary.

use eggdb_platform::ErrorKind;
use eggdb_platform::process::{ChildExit, run_with_peak_memory};
use std::process::{Command, Stdio};

const TOUCH_BYTES_VARIABLE: &str = "EGGDB_PROCESS_TEST_TOUCH_BYTES";
const TOUCHED_BYTES: usize = 64 << 20;
const PAGE_STRIDE: usize = 4096;
const PARENT_BYTES: usize = 256 << 20;

fn this_test_as_a_child(test: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--test-threads=1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn measured(command: Command) -> Option<ChildExit> {
    match run_with_peak_memory(command) {
        Ok(exit) => Some(exit),
        Err(error)
            if error.kind() == ErrorKind::PermissionDenied && std::env::var_os("CI").is_none() =>
        {
            None
        }
        Err(error) => panic!("running the child failed: {error}"),
    }
}

#[test]
fn touches_memory_when_run_as_a_child() {
    let Some(bytes) = std::env::var_os(TOUCH_BYTES_VARIABLE) else {
        return;
    };
    let bytes: usize = bytes.to_str().unwrap().parse().unwrap();
    let mut memory = vec![0u8; bytes];
    for byte in memory.iter_mut().step_by(PAGE_STRIDE) {
        *byte = 1;
    }
    std::hint::black_box(&memory);
}

#[test]
fn exits_with_three_when_run_as_a_child() {
    if std::env::var_os(TOUCH_BYTES_VARIABLE).is_some() {
        std::process::exit(3);
    }
}

#[test]
fn a_child_reports_its_exit_code_and_a_peak_above_zero() {
    let mut child = this_test_as_a_child("exits_with_three_when_run_as_a_child");
    child.env(TOUCH_BYTES_VARIABLE, "0");
    let Some(exit) = measured(child) else {
        return;
    };
    assert_eq!(exit.code, Some(3));
    assert!(exit.peak_resident_bytes > 0);
}

#[test]
fn a_child_that_touches_64_mib_reports_a_peak_of_at_least_that() {
    let mut child = this_test_as_a_child("touches_memory_when_run_as_a_child");
    child.env(TOUCH_BYTES_VARIABLE, TOUCHED_BYTES.to_string());
    let Some(exit) = measured(child) else {
        return;
    };
    assert_eq!(exit.code, Some(0));
    assert!(
        exit.peak_resident_bytes >= TOUCHED_BYTES as u64,
        "peak {} bytes",
        exit.peak_resident_bytes
    );
    assert!(exit.peak_resident_bytes < 4 * TOUCHED_BYTES as u64);
}

#[test]
fn a_child_started_by_a_large_parent_reports_only_its_own_peak() {
    let mut parent_memory = vec![0u8; PARENT_BYTES];
    for byte in parent_memory.iter_mut().step_by(PAGE_STRIDE) {
        *byte = 1;
    }
    std::hint::black_box(&parent_memory);
    let mut child = this_test_as_a_child("touches_memory_when_run_as_a_child");
    child.env(TOUCH_BYTES_VARIABLE, "0");
    let Some(exit) = measured(child) else {
        return;
    };
    drop(parent_memory);
    assert_eq!(exit.code, Some(0));
    assert!(
        exit.peak_resident_bytes < TOUCHED_BYTES as u64,
        "peak {} bytes",
        exit.peak_resident_bytes
    );
}

#[cfg(unix)]
#[test]
fn a_child_that_starts_another_program_in_its_place_is_followed_to_its_exit() {
    let mut child = Command::new("sh");
    child
        .arg("-c")
        .arg("exec \"$0\" --exact exits_with_three_when_run_as_a_child --test-threads=1")
        .arg(std::env::current_exe().unwrap())
        .env(TOUCH_BYTES_VARIABLE, "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Some(exit) = measured(child) else {
        return;
    };
    assert_eq!(exit.code, Some(3));
    assert!(exit.peak_resident_bytes > 0);
}
