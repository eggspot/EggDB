use crate::error::{Error, ErrorKind, Result};
use std::fs;
use std::io;

pub(crate) fn request(options: &mut fs::OpenOptions) -> Result<()> {
    platform::request(options)
}

pub(crate) fn apply(file: &fs::File) -> Result<()> {
    platform::apply(file)
}

pub(crate) fn open_error(error: io::Error) -> Error {
    let refused = platform::is_refusal(&error);
    let error = Error::from(error);
    if refused {
        error.with_kind(ErrorKind::Unsupported)
    } else {
        error
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod platform {
    use super::{Error, ErrorKind, Result, fs};

    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "x86",
        target_arch = "riscv64",
        target_arch = "s390x",
        target_arch = "loongarch64"
    ))]
    const O_DIRECT: Option<i32> = Some(0o40000);
    #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
    const O_DIRECT: Option<i32> = Some(0o200000);
    #[cfg(any(target_arch = "powerpc", target_arch = "powerpc64"))]
    const O_DIRECT: Option<i32> = Some(0o400000);
    #[cfg(any(target_arch = "mips", target_arch = "mips64"))]
    const O_DIRECT: Option<i32> = Some(0x8000);
    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "x86",
        target_arch = "riscv64",
        target_arch = "s390x",
        target_arch = "loongarch64",
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "powerpc",
        target_arch = "powerpc64",
        target_arch = "mips",
        target_arch = "mips64"
    )))]
    const O_DIRECT: Option<i32> = None;

    const EINVAL: i32 = 22;

    pub(super) fn is_refusal(error: &std::io::Error) -> bool {
        error.raw_os_error() == Some(EINVAL)
    }

    pub(super) fn request(options: &mut fs::OpenOptions) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let flag = O_DIRECT.ok_or_else(|| Error::new(ErrorKind::Unsupported))?;
        options.custom_flags(flag);
        Ok(())
    }

    pub(super) fn apply(_file: &fs::File) -> Result<()> {
        Ok(())
    }
}

#[cfg(target_vendor = "apple")]
mod platform {
    use super::{Error, ErrorKind, Result, fs};
    use std::ffi::c_int;
    use std::io;
    use std::os::fd::AsRawFd;

    const F_NOCACHE: c_int = 48;

    unsafe extern "C" {
        fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    }

    pub(super) fn is_refusal(_error: &io::Error) -> bool {
        false
    }

    pub(super) fn request(_options: &mut fs::OpenOptions) -> Result<()> {
        Ok(())
    }

    pub(super) fn apply(file: &fs::File) -> Result<()> {
        // SAFETY: `file` keeps the descriptor open for the duration of the call, and F_NOCACHE
        // takes one integer argument, passed through the variadic declaration as C expects.
        let result = unsafe { fcntl(file.as_raw_fd(), F_NOCACHE, 1 as c_int) };
        if result == -1 {
            return Err(Error::from(io::Error::last_os_error()).with_kind(ErrorKind::Unsupported));
        }
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    use super::{Result, fs};

    const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;

    pub(super) fn is_refusal(_error: &std::io::Error) -> bool {
        false
    }

    pub(super) fn request(options: &mut fs::OpenOptions) -> Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(FILE_FLAG_NO_BUFFERING);
        Ok(())
    }

    pub(super) fn apply(_file: &fs::File) -> Result<()> {
        Ok(())
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    windows
)))]
mod platform {
    use super::{Error, ErrorKind, Result, fs};

    pub(super) fn is_refusal(_error: &std::io::Error) -> bool {
        false
    }

    pub(super) fn request(_options: &mut fs::OpenOptions) -> Result<()> {
        Err(Error::new(ErrorKind::Unsupported))
    }

    pub(super) fn apply(_file: &fs::File) -> Result<()> {
        Ok(())
    }
}
