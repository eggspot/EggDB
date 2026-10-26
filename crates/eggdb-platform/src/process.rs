//! Child processes: running one and learning the most memory it held, which benchmarks use to
//! measure a program's footprint from outside it.

use crate::error::Result;
use std::process::Command;

/// How a child process ended, and the most memory it held at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildExit {
    /// The code the child exited with, or `None` when a signal ended it.
    pub code: Option<i32>,
    /// The child's peak resident set size in bytes: its peak working set on Windows.
    pub peak_resident_bytes: u64,
}

/// Runs `command` to its end, then returns its exit code and the most memory it held. The
/// command is consumed, since on Linux and Android it gains a step that runs before `exec`.
///
/// The peak is the child's own: on Linux and Android its `VmHWM` read when it exits, which a
/// parent's memory never inflates, through `ptrace` (so a tracer such as a debugger must not
/// hold the child); on Apple's systems and the BSDs the `ru_maxrss` of `wait4`; on Windows the
/// peak working set. The operating system counts it in whole pages or kibibytes.
///
/// # Errors
///
/// The error `spawn` gives when the command cannot start;
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) where the operating system does
/// not report a child's peak memory (after the child has ended); and the operating system's
/// error when waiting for the child or tracing it fails.
pub fn run_with_peak_memory(command: Command) -> Result<ChildExit> {
    imp::run_with_peak_memory(command)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
use crate::os_process_linux as imp;

#[cfg(all(
    target_pointer_width = "64",
    any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    )
))]
use crate::os_process_unix as imp;

#[cfg(windows)]
use crate::os_process_windows as imp;

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    all(
        target_pointer_width = "64",
        any(
            target_vendor = "apple",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        )
    )
)))]
mod imp {
    use super::ChildExit;
    use crate::error::{Error, ErrorKind, Result};
    use std::process::Command;

    pub(crate) fn run_with_peak_memory(mut command: Command) -> Result<ChildExit> {
        command.status()?;
        Err(Error::new(ErrorKind::Unsupported))
    }
}
