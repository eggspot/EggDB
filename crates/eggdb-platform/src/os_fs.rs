use crate::error::{Error, ErrorKind, Result};
use crate::fs::{File, FileLock, FileSystem, OpenMode};
use crate::io::IoQueueOptions;
use crate::os_io::OsIoQueue;
use crate::uncached;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// The operating system's file system.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsFileSystem;

impl OsFileSystem {
    /// Creates a handle to the operating system's file system.
    pub fn new() -> Self {
        Self
    }
}

impl FileSystem for OsFileSystem {
    type File = OsFile;
    type IoQueue = OsIoQueue;

    fn open(&self, path: &Path, mode: OpenMode) -> Result<OsFile> {
        let file = open_options(mode)
            .open(path)
            .map_err(|error| open_error(path, mode, error, Error::from))?;
        refuse_directory(&file)?;
        Ok(OsFile::new(file, mode, false))
    }

    fn open_uncached(&self, path: &Path, mode: OpenMode) -> Result<OsFile> {
        let mut options = open_options(mode);
        uncached::request(&mut options)?;
        let opened = options
            .open(path)
            .map_err(|error| open_error(path, mode, error, uncached::open_error))
            .and_then(|file| refuse_directory(&file).map(|()| file))
            .and_then(|file| uncached::apply(&file).map(|()| file));
        match opened {
            Ok(file) => Ok(OsFile::new(file, mode, true)),
            Err(error) => {
                if mode == OpenMode::CreateNew && error.kind() == ErrorKind::Unsupported {
                    let _ = fs::remove_file(path);
                }
                Err(error)
            }
        }
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        fs::remove_file(path).map_err(|error| directory_given(path, error))
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        if fs::symlink_metadata(from).is_ok_and(|metadata| metadata.is_dir()) {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        fs::rename(from, to).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound
                || runs_through_a_file(from)
                || runs_through_a_file(to)
            {
                Error::from(error).with_kind(ErrorKind::NotFound)
            } else {
                directory_given(to, error)
            }
        })
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        fs::create_dir_all(path).map_err(|error| {
            let through_a_file = error.kind() == io::ErrorKind::NotADirectory;
            let error = Error::from(error);
            if through_a_file {
                error.with_kind(ErrorKind::AlreadyExists)
            } else {
                error
            }
        })
    }

    fn sync_dir(&self, path: &Path) -> Result<()> {
        sync_directory(path)
    }

    fn exists(&self, path: &Path) -> Result<bool> {
        match path.try_exists() {
            Ok(exists) => Ok(exists),
            Err(error) if error.kind() == io::ErrorKind::NotADirectory => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn io_queue(&self, options: IoQueueOptions) -> Result<OsIoQueue> {
        OsIoQueue::new(options)
    }
}

fn open_options(mode: OpenMode) -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(mode.is_writable());
    match mode {
        OpenMode::Read | OpenMode::ReadWrite => {}
        OpenMode::Create => {
            options.create(true);
        }
        OpenMode::CreateNew => {
            options.create_new(true);
        }
    }
    options
}

fn runs_through_a_file(path: &Path) -> bool {
    path.ancestors()
        .skip(1)
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .any(|ancestor| fs::metadata(ancestor).is_ok_and(|metadata| metadata.is_file()))
}

fn is_directory(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

fn open_error(
    path: &Path,
    mode: OpenMode,
    error: io::Error,
    convert: fn(io::Error) -> Error,
) -> Error {
    if !is_directory(path) {
        return convert(error);
    }
    let kind = match mode {
        OpenMode::CreateNew => ErrorKind::AlreadyExists,
        OpenMode::Read | OpenMode::ReadWrite | OpenMode::Create => ErrorKind::InvalidInput,
    };
    Error::from(error).with_kind(kind)
}

fn directory_given(path: &Path, error: io::Error) -> Error {
    let error = Error::from(error);
    if is_directory(path) {
        error.with_kind(ErrorKind::InvalidInput)
    } else {
        error
    }
}

fn refuse_directory(file: &fs::File) -> Result<()> {
    if file.metadata()?.is_dir() {
        return Err(Error::new(ErrorKind::InvalidInput));
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn file_given(path: &Path, error: io::Error) -> Error {
    let error = Error::from(error);
    if fs::metadata(path).is_ok_and(|metadata| !metadata.is_dir()) {
        error.with_kind(ErrorKind::InvalidInput)
    } else {
        error
    }
}

#[cfg(any(unix, windows))]
fn require_directory(directory: &fs::File) -> Result<()> {
    if directory.metadata()?.is_dir() {
        Ok(())
    } else {
        Err(Error::new(ErrorKind::InvalidInput))
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    let directory = fs::File::open(path).map_err(|error| file_given(path, error))?;
    require_directory(&directory)?;
    Ok(directory.sync_all()?)
}

#[cfg(windows)]
fn sync_directory(path: &Path) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let directory = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .map_err(|error| file_given(path, error))?;
    require_directory(&directory)?;
    Ok(directory.sync_all()?)
}

#[cfg(not(any(unix, windows)))]
fn sync_directory(path: &Path) -> Result<()> {
    if fs::metadata(path)?.is_dir() {
        Ok(())
    } else {
        Err(Error::new(ErrorKind::InvalidInput))
    }
}

/// A file opened through [`OsFileSystem`].
#[derive(Debug)]
pub struct OsFile {
    file: fs::File,
    writable: bool,
    uncached: bool,
    locked: AtomicBool,
    #[cfg(windows)]
    overlapped: std::sync::Mutex<Vec<(u64, std::os::windows::io::OwnedHandle)>>,
    #[cfg(not(any(unix, windows)))]
    cursor: std::sync::Mutex<()>,
}

#[cfg(unix)]
impl std::os::fd::AsRawFd for OsFile {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.file.as_raw_fd()
    }
}

impl OsFile {
    pub(crate) fn is_writable(&self) -> bool {
        self.writable
    }

    fn require_writable(&self) -> Result<()> {
        if self.is_writable() {
            Ok(())
        } else {
            Err(Error::new(ErrorKind::PermissionDenied))
        }
    }

    fn new(file: fs::File, mode: OpenMode, uncached: bool) -> Self {
        Self {
            file,
            writable: mode.is_writable(),
            uncached,
            locked: AtomicBool::new(false),
            #[cfg(windows)]
            overlapped: std::sync::Mutex::new(Vec::new()),
            #[cfg(not(any(unix, windows)))]
            cursor: std::sync::Mutex::new(()),
        }
    }
}

impl File for OsFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.positioned_read(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize> {
        self.require_writable()?;
        if buf.is_empty() {
            return Ok(0);
        }
        self.positioned_write(buf, offset)
    }

    fn size(&self) -> Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    fn set_size(&self, size: u64) -> Result<()> {
        self.require_writable()?;
        Ok(self.file.set_len(size)?)
    }

    fn is_uncached(&self) -> bool {
        self.uncached
    }

    fn try_lock(&self, lock: FileLock) -> Result<()> {
        if self.locked.swap(true, Ordering::AcqRel) {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        let locked = lock_file(&self.file, lock);
        if locked.is_err() {
            self.locked.store(false, Ordering::Release);
        }
        locked
    }

    fn sync_data(&self) -> Result<()> {
        self.require_writable()?;
        Ok(self.file.sync_data()?)
    }

    fn sync_all(&self) -> Result<()> {
        self.require_writable()?;
        Ok(self.file.sync_all()?)
    }
}

#[cfg(unix)]
impl OsFile {
    fn positioned_read(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        use std::os::unix::fs::FileExt;
        Ok(self.file.read_at(buf, offset)?)
    }

    fn positioned_write(&self, buf: &[u8], offset: u64) -> Result<usize> {
        use std::os::unix::fs::FileExt;
        Ok(self.file.write_at(buf, offset)?)
    }
}

#[cfg(windows)]
impl OsFile {
    fn positioned_read(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        use std::os::windows::fs::FileExt;
        Ok(self.file.seek_read(buf, offset)?)
    }

    fn positioned_write(&self, buf: &[u8], offset: u64) -> Result<usize> {
        use std::os::windows::fs::FileExt;
        Ok(self.file.seek_write(buf, offset)?)
    }
}

#[cfg(windows)]
impl OsFile {
    pub(crate) fn overlapped_handle(
        &self,
        queue: u64,
        reopen: impl FnOnce(&fs::File) -> Result<std::os::windows::io::OwnedHandle>,
    ) -> Result<(std::os::windows::io::RawHandle, bool)> {
        use std::os::windows::io::AsRawHandle;
        let mut handles = self
            .overlapped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, handle)) = handles.iter().find(|(owner, _)| *owner == queue) {
            return Ok((handle.as_raw_handle(), false));
        }
        let handle = reopen(&self.file)?;
        let raw = handle.as_raw_handle();
        handles.push((queue, handle));
        Ok((raw, true))
    }

    #[cfg(test)]
    pub(crate) fn overlapped_handle_count(&self) -> usize {
        self.overlapped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    pub(crate) fn close_overlapped_handle(&self, queue: u64) {
        self.overlapped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(owner, _)| *owner != queue);
    }
}

#[cfg(not(any(unix, windows)))]
impl OsFile {
    fn positioned_read(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        use std::io::{Read, Seek, SeekFrom};
        let _cursor = self
            .cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut file = &self.file;
        file.seek(SeekFrom::Start(offset))?;
        Ok(file.read(buf)?)
    }

    fn positioned_write(&self, buf: &[u8], offset: u64) -> Result<usize> {
        use std::io::{Seek, SeekFrom, Write};
        let _cursor = self
            .cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut file = &self.file;
        file.seek(SeekFrom::Start(offset))?;
        Ok(file.write(buf)?)
    }
}

#[cfg(not(windows))]
impl Drop for OsFile {
    fn drop(&mut self) {
        if *self.locked.get_mut() {
            let _ = self.file.unlock();
        }
    }
}

#[cfg(not(windows))]
fn lock_file(file: &fs::File, lock: FileLock) -> Result<()> {
    let locked = match lock {
        FileLock::Shared => file.try_lock_shared(),
        FileLock::Exclusive => file.try_lock(),
    };
    locked.map_err(|error| match error {
        fs::TryLockError::WouldBlock => Error::new(ErrorKind::Locked),
        fs::TryLockError::Error(error) => Error::from(error),
    })
}

#[cfg(windows)]
mod windows_lock {
    use std::ffi::c_void;
    use std::os::windows::io::RawHandle;

    pub(super) const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x1;
    pub(super) const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x2;
    pub(super) const ERROR_LOCK_VIOLATION: i32 = 33;
    const LOCKED_BYTE: u64 = 1 << 62;

    #[repr(C)]
    pub(super) struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut c_void,
    }

    impl Overlapped {
        pub(super) fn at_the_locked_byte() -> Self {
            Self {
                internal: 0,
                internal_high: 0,
                offset: LOCKED_BYTE as u32,
                offset_high: (LOCKED_BYTE >> 32) as u32,
                event: std::ptr::null_mut(),
            }
        }
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub(super) fn LockFileEx(
            file: RawHandle,
            flags: u32,
            reserved: u32,
            bytes_low: u32,
            bytes_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        pub(super) fn UnlockFileEx(
            file: RawHandle,
            reserved: u32,
            bytes_low: u32,
            bytes_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
    }
}

#[cfg(windows)]
impl Drop for OsFile {
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle;
        if !*self.locked.get_mut() {
            return;
        }
        let mut overlapped = windows_lock::Overlapped::at_the_locked_byte();
        // SAFETY: the handle is still open, since `self.file` is dropped after this runs, and
        // `overlapped` lives on this stack frame until the synchronous call returns. A failure
        // only leaves the lock for the operating system to release when the handle closes.
        let _ = unsafe {
            windows_lock::UnlockFileEx(self.file.as_raw_handle(), 0, 1, 0, &mut overlapped)
        };
    }
}

#[cfg(windows)]
fn lock_file(file: &fs::File, lock: FileLock) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_lock::{
        ERROR_LOCK_VIOLATION, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
        Overlapped,
    };

    let flags = match lock {
        FileLock::Shared => LOCKFILE_FAIL_IMMEDIATELY,
        FileLock::Exclusive => LOCKFILE_FAIL_IMMEDIATELY | LOCKFILE_EXCLUSIVE_LOCK,
    };
    let mut overlapped = Overlapped::at_the_locked_byte();
    // SAFETY: the handle is open for the duration of the call, the file was opened for
    // synchronous I/O so LockFileEx finishes before returning, and `overlapped` lives on this
    // stack frame until then.
    let locked = unsafe { LockFileEx(file.as_raw_handle(), flags, 0, 1, 0, &mut overlapped) };
    if locked != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION) {
        Err(Error::from(error).with_kind(ErrorKind::Locked))
    } else {
        Err(Error::from(error))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use crate::{ErrorKind, File, FileLock, FileSystem, OpenMode, OsFileSystem};
    use std::io::{PipeWriter, Read, Write};
    use std::os::unix::fs::PermissionsExt;
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
            let finished = self.spawning.take().map(JoinHandle::join);
            if !std::thread::panicking() {
                assert!(finished.unwrap().unwrap().unwrap().success());
            }
        }
    }

    #[test]
    fn a_dropped_handle_releases_its_lock_while_a_forked_child_still_has_a_copy() {
        let path = std::env::temp_dir().join(format!("eggdb-forked-lock-{}", std::process::id()));
        let fs = OsFileSystem::new();
        for lock in [FileLock::Exclusive, FileLock::Shared] {
            let owner = fs.open(&path, OpenMode::Create).unwrap();
            owner.try_lock(lock).unwrap();
            let child = ChildBeforeItsProgram::fork();
            drop(owner);
            let reopened = fs.open(&path, OpenMode::ReadWrite).unwrap();
            let relocked = reopened.try_lock(FileLock::Exclusive);
            drop(reopened);
            drop(child);
            relocked.unwrap();
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn syncing_an_unreadable_regular_file_as_a_directory_is_invalid() {
        let path = std::env::temp_dir().join(format!("eggdb-unreadable-{}", std::process::id()));
        std::fs::write(&path, b"x").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let synced = OsFileSystem::new().sync_dir(&path);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(synced.unwrap_err().kind(), ErrorKind::InvalidInput);
    }
}
