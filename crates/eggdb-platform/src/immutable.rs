//! The file system's own write protection for archived files (D1022): the user immutable flag
//! on Apple systems and the BSDs' `chflags`, and the immutable attribute on Linux, which only a
//! process with `CAP_LINUX_IMMUTABLE` may set. Where neither can be set the caller keeps only
//! the read-only permission.

use std::path::Path;

/// Sets (`true`) or clears the immutable flag of the file at `path`. `Ok(false)` when the file
/// system, the platform or the process's privileges do not allow it.
pub fn set_immutable(path: &Path, immutable: bool) -> std::io::Result<bool> {
    imp::set_immutable(path, immutable)
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "netbsd", target_os = "openbsd", target_os = "dragonfly"))]
mod imp {
    use std::ffi::{CString, c_char, c_int, c_uint};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    const UF_IMMUTABLE: c_uint = 0x0000_0002;
    const EPERM: i32 = 1;
    const ENOTSUP: i32 = 45;
    const EOPNOTSUPP: i32 = 102;

    unsafe extern "C" {
        fn chflags(path: *const c_char, flags: c_uint) -> c_int;
    }

    pub(super) fn set_immutable(path: &Path, immutable: bool) -> std::io::Result<bool> {
        let path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "a path holds a NUL byte"))?;
        let flags = if immutable { UF_IMMUTABLE } else { 0 };
        // SAFETY: `path` is a NUL-terminated path owned by the CString above, alive for the call;
        // chflags reads it and keeps nothing.
        let result = unsafe { chflags(path.as_ptr(), flags) };
        if result == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error().is_some_and(|code| [EPERM, ENOTSUP, EOPNOTSUPP].contains(&code)) {
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

    const FS_IOC_GETFLAGS: c_ulong = 0x8008_6601;
    const FS_IOC_SETFLAGS: c_ulong = 0x4008_6602;
    const FS_IMMUTABLE_FL: c_int = 0x0000_0010;
    const EPERM: i32 = 1;
    const EACCES: i32 = 13;
    const ENOTTY: i32 = 25;
    const EINVAL: i32 = 22;
    const EOPNOTSUPP: i32 = 95;

    unsafe extern "C" {
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }

    fn unsupported(error: &std::io::Error) -> bool {
        error.raw_os_error().is_some_and(|code| [EPERM, EACCES, ENOTTY, EINVAL, EOPNOTSUPP].contains(&code))
    }

    pub(super) fn set_immutable(path: &Path, immutable: bool) -> std::io::Result<bool> {
        let file = std::fs::File::open(path)?;
        let mut flags: c_int = 0;
        // SAFETY: the descriptor belongs to the file opened above, open for the call, and
        // FS_IOC_GETFLAGS writes one int to `flags`, a live c_int.
        let read = unsafe { ioctl(file.as_raw_fd(), FS_IOC_GETFLAGS, &raw mut flags) };
        if read != 0 {
            let error = std::io::Error::last_os_error();
            return if unsupported(&error) { Ok(false) } else { Err(error) };
        }
        flags = if immutable { flags | FS_IMMUTABLE_FL } else { flags & !FS_IMMUTABLE_FL };
        // SAFETY: as above; FS_IOC_SETFLAGS reads one int from `flags`, a live c_int.
        let written = unsafe { ioctl(file.as_raw_fd(), FS_IOC_SETFLAGS, &raw const flags) };
        if written == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if unsupported(&error) { Ok(false) } else { Err(error) }
    }
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "linux",
    target_os = "android"
)))]
mod imp {
    use std::path::Path;

    pub(super) fn set_immutable(_path: &Path, _immutable: bool) -> std::io::Result<bool> {
        Ok(false)
    }
}
