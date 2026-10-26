//! Waiting for a child process reports its exit code and its peak memory, checked against
//! children of this test binary.

use eggdb_platform::process::wait_with_peak_memory;
use std::process::{Command, Stdio};

const TOUCH_BYTES_VARIABLE: &str = "EGGDB_PROCESS_TEST_TOUCH_BYTES";
const TOUCHED_BYTES: usize = 64 << 20;
const PAGE_STRIDE: usize = 4096;

fn this_test_as_a_child(test: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--test-threads=1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
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
    let child = this_test_as_a_child("exits_with_three_when_run_as_a_child")
        .env(TOUCH_BYTES_VARIABLE, "0")
        .spawn()
        .unwrap();
    let exit = wait_with_peak_memory(child).unwrap();
    assert_eq!(exit.code, Some(3));
    assert!(exit.peak_resident_bytes > 0);
}

#[test]
fn a_child_that_touches_64_mib_reports_a_peak_of_at_least_that() {
    let child = this_test_as_a_child("touches_memory_when_run_as_a_child")
        .env(TOUCH_BYTES_VARIABLE, TOUCHED_BYTES.to_string())
        .spawn()
        .unwrap();
    let exit = wait_with_peak_memory(child).unwrap();
    assert_eq!(exit.code, Some(0));
    assert!(
        exit.peak_resident_bytes >= TOUCHED_BYTES as u64,
        "peak {} bytes",
        exit.peak_resident_bytes
    );
    assert!(exit.peak_resident_bytes < 4 * TOUCHED_BYTES as u64);
}
