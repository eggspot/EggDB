//! Punching holes in files (D1334): `fallocate` with `FALLOC_FL_PUNCH_HOLE` on 64-bit Linux and
//! Android, `fcntl(F_PUNCHHOLE)` on macOS and iOS. Elsewhere the bytes stay, which reads the same.

#[cfg(all(any(target_os = "linux", target_os = "android"), target_pointer_width = "64"))]
pub(super) fn punch(file: &std::fs::File, offset: u64, len: u64) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    const FALLOC_FL_KEEP_SIZE: i32 = 0x01;
    const FALLOC_FL_PUNCH_HOLE: i32 = 0x02;
    unsafe extern "C" {
        fn fallocate(fd: i32, mode: i32, offset: i64, len: i64) -> i32;
    }
    let (Ok(offset), Ok(len)) = (i64::try_from(offset), i64::try_from(len)) else {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    };
    // SAFETY: fallocate takes plain integers, and the descriptor stays open while `file` is borrowed.
    let result = unsafe { fallocate(file.as_raw_fd(), FALLOC_FL_KEEP_SIZE | FALLOC_FL_PUNCH_HOLE, offset, len) };
    if result == 0 { Ok(()) } else { unsupported_is_fine(std::io::Error::last_os_error()) }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(super) fn punch(file: &std::fs::File, offset: u64, len: u64) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    const F_PUNCHHOLE: i32 = 99;
    const BLOCK: u64 = 4096;
    #[repr(C)]
    struct PunchHole {
        flags: u32,
        reserved: u32,
        offset: i64,
        length: i64,
    }
    unsafe extern "C" {
        fn fcntl(fd: i32, command: i32, ...) -> i32;
    }
    let start = offset.next_multiple_of(BLOCK);
    let end = (offset + len) / BLOCK * BLOCK;
    if end <= start {
        return Ok(());
    }
    let (Ok(start), Ok(length)) = (i64::try_from(start), i64::try_from(end - start)) else {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    };
    let hole = PunchHole { flags: 0, reserved: 0, offset: start, length };
    // SAFETY: F_PUNCHHOLE reads one `fpunchhole_t`, which `hole` matches and outlives the call.
    let result = unsafe { fcntl(file.as_raw_fd(), F_PUNCHHOLE, &hole as *const PunchHole) };
    if result == 0 { Ok(()) } else { unsupported_is_fine(std::io::Error::last_os_error()) }
}

#[cfg(not(any(
    all(any(target_os = "linux", target_os = "android"), target_pointer_width = "64"),
    target_os = "macos",
    target_os = "ios"
)))]
pub(super) fn punch(_file: &std::fs::File, _offset: u64, _len: u64) -> std::io::Result<()> {
    Ok(())
}

#[cfg(any(
    all(any(target_os = "linux", target_os = "android"), target_pointer_width = "64"),
    target_os = "macos",
    target_os = "ios"
))]
fn unsupported_is_fine(error: std::io::Error) -> std::io::Result<()> {
    if error.kind() == std::io::ErrorKind::Unsupported || matches!(error.raw_os_error(), Some(95 | 45 | 102 | 25)) {
        Ok(())
    } else {
        Err(error)
    }
}
