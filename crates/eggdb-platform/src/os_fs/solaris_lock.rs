use crate::error::{Error, ErrorKind, Result};
use crate::fs::FileLock;
use std::collections::HashMap;
use std::ffi::{c_int, c_short};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::sync::{Mutex, PoisonError};

const F_SETLK: c_int = 6;
const F_RDLCK: c_short = 1;
const F_WRLCK: c_short = 2;
const F_UNLCK: c_short = 3;
const SEEK_SET: c_short = 0;
const EAGAIN: i32 = 11;
const EACCES: i32 = 13;

#[repr(C)]
struct Flock {
    kind: c_short,
    whence: c_short,
    start: i64,
    length: i64,
    system_id: c_int,
    process_id: c_int,
    padding: [i64; 4],
}

unsafe extern "C" {
    fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
}

type FileKey = (u64, u64);

#[derive(Default)]
struct Holders {
    shared: usize,
    exclusive: bool,
}

#[derive(Default)]
struct Registry {
    files: HashMap<FileKey, Holders>,
    handles: HashMap<RawFd, (FileKey, FileLock)>,
}

static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);

fn key_of(file: &fs::File) -> Result<FileKey> {
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

fn set_lock(file: &fs::File, kind: c_short) -> io::Result<()> {
    let request = Flock {
        kind,
        whence: SEEK_SET,
        start: 0,
        length: 0,
        system_id: 0,
        process_id: 0,
        padding: [0; 4],
    };
    // SAFETY: request is a struct flock read during the call, and the descriptor stays open
    // because file is borrowed.
    let result = unsafe { fcntl(file.as_raw_fd(), F_SETLK, &request as *const Flock) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn acquire(file: &fs::File, lock: FileLock) -> Result<()> {
    let key = key_of(file)?;
    let mut guard = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    let registry = guard.get_or_insert_with(Registry::default);
    let holders = registry.files.entry(key).or_default();
    let conflicts = holders.exclusive || (lock == FileLock::Exclusive && holders.shared > 0);
    if conflicts {
        if holders.shared == 0 && !holders.exclusive {
            registry.files.remove(&key);
        }
        return Err(Error::new(ErrorKind::Locked));
    }
    let first_in_process = holders.shared == 0;
    if first_in_process {
        let kind = match lock {
            FileLock::Shared => F_RDLCK,
            FileLock::Exclusive => F_WRLCK,
        };
        if let Err(error) = set_lock(file, kind) {
            registry.files.remove(&key);
            return Err(match error.raw_os_error() {
                Some(EAGAIN | EACCES) => Error::new(ErrorKind::Locked),
                _ => Error::from(error),
            });
        }
    }
    let holders = registry.files.entry(key).or_default();
    match lock {
        FileLock::Shared => holders.shared += 1,
        FileLock::Exclusive => holders.exclusive = true,
    }
    registry.handles.insert(file.as_raw_fd(), (key, lock));
    Ok(())
}

pub(super) fn release(file: &fs::File) {
    let mut guard = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(registry) = guard.as_mut() else {
        return;
    };
    let Some((key, lock)) = registry.handles.remove(&file.as_raw_fd()) else {
        return;
    };
    let Some(holders) = registry.files.get_mut(&key) else {
        return;
    };
    match lock {
        FileLock::Shared => holders.shared = holders.shared.saturating_sub(1),
        FileLock::Exclusive => holders.exclusive = false,
    }
    if holders.shared == 0 && !holders.exclusive {
        registry.files.remove(&key);
        let _ = set_lock(file, F_UNLCK);
    }
}
