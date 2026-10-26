//! The file lock seen from other processes: a second process, and a child process started while
//! a database is open.

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

#[cfg(unix)]
mod forked_child {
    use super::TempPath;
    use eggdb::{Database, Options};
    use std::io::{PipeWriter, Read, Write};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, ExitStatus};
    use std::thread::JoinHandle;

    struct ChildBeforeItsProgram {
        start_program: PipeWriter,
        spawning: Option<JoinHandle<std::io::Result<ExitStatus>>>,
    }

    impl ChildBeforeItsProgram {
        fn fork() -> Self {
            let (forked, forked_signal) = std::io::pipe().unwrap();
            let (start_signal, start_program) = std::io::pipe().unwrap();
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "exit 0"]);
            // SAFETY: the closure runs in the forked child before its program starts and only
            // writes one byte to a pipe and reads one byte from another, which neither allocate
            // nor take locks.
            unsafe {
                command.pre_exec(move || {
                    (&forked_signal).write_all(b"f")?;
                    (&start_signal).read_exact(&mut [0])
                })
            };
            let spawning = std::thread::spawn(move || command.status());
            (&forked).read_exact(&mut [0]).unwrap();
            Self {
                start_program,
                spawning: Some(spawning),
            }
        }
    }

    impl Drop for ChildBeforeItsProgram {
        fn drop(&mut self) {
            let _ = (&self.start_program).write_all(b"s");
            if let Some(spawning) = self.spawning.take() {
                let status = spawning.join().unwrap().unwrap();
                assert!(status.success() || std::thread::panicking());
            }
        }
    }

    #[test]
    fn a_closed_writer_reopens_while_a_child_forked_before_the_close_is_still_starting() {
        let file = TempPath::new("forked-writer");
        let writer = Database::open(file.path()).unwrap();
        writer.put("k", "v").unwrap();
        let _child = ChildBeforeItsProgram::fork();
        writer.close().unwrap();
        let reopened = Database::open(file.path()).unwrap();
        assert_eq!(reopened.get("k").unwrap().as_deref(), Some(&b"v"[..]));
    }

    #[test]
    fn a_closed_reader_lets_a_writer_in_while_a_child_forked_before_the_close_is_still_starting() {
        let file = TempPath::new("forked-reader");
        Database::open(file.path()).unwrap().close().unwrap();
        let reader = Database::open_with(file.path(), Options::new().read_only(true)).unwrap();
        let _child = ChildBeforeItsProgram::fork();
        reader.close().unwrap();
        Database::open(file.path()).unwrap().put("k", "v").unwrap();
    }
}
