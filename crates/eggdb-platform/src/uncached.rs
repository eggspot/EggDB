#[cfg(any(target_os = "linux", target_os = "android", windows))]
use crate::aligned::AlignedBuffer;
use crate::error::{Error, ErrorKind, Result};
use std::fs;
use std::io;

#[cfg(any(target_os = "linux", target_os = "android", windows))]
fn fits_our_alignment(required: u32) -> bool {
    let required = required as usize;
    required != 0 && required.is_power_of_two() && AlignedBuffer::ALIGNMENT.is_multiple_of(required)
}

#[cfg(any(target_os = "linux", target_os = "android", windows))]
fn refuse_unless_it_fits(required: u32) -> Result<()> {
    if fits_our_alignment(required) {
        Ok(())
    } else {
        Err(Error::new(ErrorKind::Unsupported))
    }
}

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
    #[cfg(any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "mips32r6",
        target_arch = "mips64r6"
    ))]
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
        target_arch = "mips64",
        target_arch = "mips32r6",
        target_arch = "mips64r6"
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

    pub(super) fn apply(file: &fs::File) -> Result<()> {
        match dio_alignment::read(file) {
            Some((memory, offset)) => {
                super::refuse_unless_it_fits(memory)?;
                super::refuse_unless_it_fits(offset)
            }
            None => Ok(()),
        }
    }

    mod dio_alignment {
        use std::ffi::{c_char, c_int, c_long, c_uint};
        use std::fs;
        use std::os::fd::AsRawFd;

        const AT_EMPTY_PATH: c_int = 0x1000;
        const STATX_DIOALIGN: c_uint = 0x2000;
        const MASK_AT: usize = 0;
        const DIO_MEMORY_ALIGN_AT: usize = 152;
        const DIO_OFFSET_ALIGN_AT: usize = 156;

        cfg_select! {
            target_arch = "x86_64" => { const SYS_STATX: Option<c_long> = Some(332); }
            any(target_arch = "x86", target_arch = "powerpc", target_arch = "powerpc64") => {
                const SYS_STATX: Option<c_long> = Some(383);
            }
            target_arch = "arm" => { const SYS_STATX: Option<c_long> = Some(397); }
            target_arch = "s390x" => { const SYS_STATX: Option<c_long> = Some(379); }
            any(target_arch = "aarch64", target_arch = "riscv64", target_arch = "loongarch64") => {
                const SYS_STATX: Option<c_long> = Some(291);
            }
            _ => { const SYS_STATX: Option<c_long> = None; }
        }

        unsafe extern "C" {
            fn syscall(number: c_long, ...) -> c_long;
        }

        pub(super) fn read(file: &fs::File) -> Option<(u32, u32)> {
            let number = SYS_STATX?;
            let mut buffer = [0_u64; 32];
            let empty: &[c_char; 1] = &[0];
            // SAFETY: statx reads the NUL-terminated empty path and writes at most 256 bytes
            // into buffer, which is 256 bytes and 8-byte aligned; the descriptor stays open for
            // the call because file is borrowed.
            let result = unsafe {
                syscall(
                    number,
                    file.as_raw_fd() as c_long,
                    empty.as_ptr(),
                    AT_EMPTY_PATH as c_long,
                    STATX_DIOALIGN as c_long,
                    buffer.as_mut_ptr(),
                )
            };
            if result != 0 {
                return None;
            }
            let bytes: Vec<u8> = buffer.iter().flat_map(|word| word.to_ne_bytes()).collect();
            let field = |at: usize| {
                u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
            };
            if field(MASK_AT) & STATX_DIOALIGN == 0 {
                return None;
            }
            Some((field(DIO_MEMORY_ALIGN_AT), field(DIO_OFFSET_ALIGN_AT)))
        }
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

    pub(super) fn apply(file: &fs::File) -> Result<()> {
        match logical_sector_size(file) {
            Some(sector) => super::refuse_unless_it_fits(sector),
            None => Ok(()),
        }
    }

    const FILE_STORAGE_INFO_CLASS: i32 = 16;

    const FILE_STORAGE_INFO_WORDS: usize = 7;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandleEx(
            file: *mut std::ffi::c_void,
            class: i32,
            information: *mut std::ffi::c_void,
            size: u32,
        ) -> i32;
    }

    fn logical_sector_size(file: &fs::File) -> Option<u32> {
        use std::os::windows::io::AsRawHandle;
        let mut info = [0_u32; FILE_STORAGE_INFO_WORDS];
        // SAFETY: info has the size of FILE_STORAGE_INFO, seven 32-bit fields, and is written
        // only during the call; the handle stays open because file is borrowed.
        let result = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FILE_STORAGE_INFO_CLASS,
                info.as_mut_ptr().cast(),
                size_of::<[u32; FILE_STORAGE_INFO_WORDS]>() as u32,
            )
        };
        (result != 0).then_some(info[0])
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
