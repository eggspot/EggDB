//! Child processes: waiting for one and learning the most memory it held, which benchmarks
//! use to measure a program's footprint from outside it.

use crate::error::Result;
use std::process::Child;

/// How a child process ended, and the most memory it held at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildExit {
    /// The code the child exited with, or `None` when a signal ended it.
    pub code: Option<i32>,
    /// The child's peak resident set size in bytes: its peak working set on Windows.
    pub peak_resident_bytes: u64,
}

/// Waits for `child` to exit, then returns its exit code and the most memory it held.
///
/// `child` must not have been waited for already. The operating system reports the peak in
/// whole pages or kibibytes, so it is a multiple of its unit.
///
/// # Errors
///
/// [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) where the operating system does
/// not report a child's peak memory (after waiting for it), and the operating system's error
/// when the wait fails.
pub fn wait_with_peak_memory(child: Child) -> Result<ChildExit> {
    imp::wait_with_peak_memory(child)
}

#[cfg(all(
    target_pointer_width = "64",
    any(
        target_os = "linux",
        target_os = "android",
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
    all(
        target_pointer_width = "64",
        any(
            target_os = "linux",
            target_os = "android",
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
    use std::process::Child;

    pub(crate) fn wait_with_peak_memory(mut child: Child) -> Result<ChildExit> {
        child.wait()?;
        Err(Error::new(ErrorKind::Unsupported))
    }
}
