//! Copy-on-write clones of whole files, where the file system makes them (D1021): APFS's
//! `clonefile` on Apple systems and the `FICLONE` ioctl on Linux file systems with shared
//! extents (Btrfs, XFS, bcachefs). A clone takes no time and no space until either file
//! changes.

use std::path::Path;

/// Clones the file at `source` to the new file `target`, sharing its blocks until either file
/// changes. `Ok(false)` when the file system or the platform cannot clone, so the caller copies
/// instead; no file is left at `target` then.
pub fn clone_file(source: &Path, target: &Path) -> std::io::Result<bool> {
    imp::clone_file(source, target)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod imp {
    use std::ffi::{CString, c_char, c_int};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    const CLONE_NOFOLLOW: u32 = 0x0001;
    const EXDEV: i32 = 18;
    const ENOTSUP: i32 = 45;

    unsafe extern "C" {
        fn clonefile(source: *const c_char, target: *const c_char, flags: u32) -> c_int;
    }

    fn c_path(path: &Path) -> std::io::Result<CString> {
        CString::new(path.as_os_str().as_bytes()).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "a path holds a NUL byte"))
    }

    pub(super) fn clone_file(source: &Path, target: &Path) -> std::io::Result<bool> {
        let source = c_path(source)?;
        let target = c_path(target)?;
        // SAFETY: both pointers are NUL-terminated paths owned by the CStrings above, alive for
        // the call; clonefile reads them and keeps neither.
        let result = unsafe { clonefile(source.as_ptr(), target.as_ptr(), CLONE_NOFOLLOW) };
        if result == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error().is_some_and(|code| [EXDEV, ENOTSUP].contains(&code)) {
            return Ok(false);
        }
        Err(error)
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use std::ffi::{c_int, c_ulong};
    use std::os::fd::AsRawFd;
    use std::path::Path;

    const FICLONE: c_ulong = 0x4004_9409;
    const EXDEV: i32 = 18;
    const EINVAL: i32 = 22;
    const ENOTTY: i32 = 25;
    const EOPNOTSUPP: i32 = 95;

    unsafe extern "C" {
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }

    pub(super) fn clone_file(source: &Path, target: &Path) -> std::io::Result<bool> {
        let from = std::fs::File::open(source)?;
        let to = std::fs::OpenOptions::new().write(true).create_new(true).open(target)?;
        // SAFETY: both descriptors belong to files opened above and stay open for the call;
        // FICLONE takes the source descriptor as its one argument and writes no memory of ours.
        let result = unsafe { ioctl(to.as_raw_fd(), FICLONE, from.as_raw_fd()) };
        if result == 0 {
            to.sync_all()?;
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        drop(to);
        std::fs::remove_file(target)?;
        if error.raw_os_error().is_some_and(|code| [EXDEV, EINVAL, ENOTTY, EOPNOTSUPP].contains(&code)) {
            return Ok(false);
        }
        Err(error)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "linux", target_os = "android")))]
mod imp {
    use std::path::Path;

    pub(super) fn clone_file(_source: &Path, _target: &Path) -> std::io::Result<bool> {
        Ok(false)
    }
}
