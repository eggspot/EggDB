use crate::error::{Error, ErrorKind, Result};
use crate::io::{IoQueue, IoQueueOptions};
use std::path::Path;

/// How [`FileSystem::open`] opens a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpenMode {
    /// Open an existing file for reading only.
    Read,
    /// Open an existing file for reading and writing.
    ReadWrite,
    /// Open a file for reading and writing, creating it empty if it does not exist.
    Create,
    /// Create a new empty file for reading and writing; fail with
    /// [`ErrorKind::AlreadyExists`] if it exists.
    CreateNew,
}

impl OpenMode {
    /// Whether a file opened this way accepts writes.
    pub fn is_writable(self) -> bool {
        !matches!(self, Self::Read)
    }
}

/// The lock [`File::try_lock`] takes on a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileLock {
    /// Shared with other shared locks, refused while an exclusive lock is held.
    Shared,
    /// Held by one handle only, refused while any other lock is held.
    Exclusive,
}

/// The namespace operations of a file system.
///
/// Creating, renaming and removing a file only become durable once the parent directory is
/// synced with [`FileSystem::sync_dir`], as on POSIX systems; code that must survive a crash
/// syncs the directory after changing its entries.
///
/// Every file system answers a wrong path the same way: a path that runs through a regular
/// file (`file/x`) is [`ErrorKind::NotFound`], a directory given where a file is needed, or a
/// file where a directory is needed, is [`ErrorKind::InvalidInput`], and creating a name that
/// is taken is [`ErrorKind::AlreadyExists`].
pub trait FileSystem: Send + Sync {
    /// The handle type for open files.
    type File: File;

    /// The queue type for asynchronous operations on those files.
    type IoQueue: IoQueue<File = Self::File>;

    /// Opens the file at `path`. A directory is refused with [`ErrorKind::InvalidInput`], or
    /// with [`ErrorKind::AlreadyExists`] for [`OpenMode::CreateNew`].
    fn open(&self, path: &Path, mode: OpenMode) -> Result<Self::File>;

    /// Opens the file at `path` so that its reads and writes bypass the operating system's
    /// cache, for an engine that caches pages itself.
    ///
    /// Every buffer address, offset and length used with the handle must be a multiple of
    /// [`AlignedBuffer::ALIGNMENT`](crate::AlignedBuffer::ALIGNMENT): use
    /// [`AlignedBuffer`](crate::AlignedBuffer) and whole pages. Bypassing the cache does not
    /// make writes durable, so [`File::sync_data`] is still required. Fails with
    /// [`ErrorKind::Unsupported`] when the platform or the file system refuses, and leaves the
    /// choice of falling back to [`FileSystem::open`] to the caller. A refused
    /// [`OpenMode::CreateNew`] leaves no file behind; a refused [`OpenMode::Create`] may leave
    /// an empty one, as the operating system creates the file before refusing.
    fn open_uncached(&self, path: &Path, mode: OpenMode) -> Result<Self::File>;

    /// Removes the file at `path`. Open handles keep working until they are dropped. A
    /// directory is refused with [`ErrorKind::InvalidInput`].
    fn remove_file(&self, path: &Path) -> Result<()>;

    /// Renames the file `from` to `to`, replacing the file at `to` if there is one. A directory
    /// at either end is refused with [`ErrorKind::InvalidInput`].
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;

    /// Creates the directory at `path` and every missing parent. Fails with
    /// [`ErrorKind::AlreadyExists`] when `path` or one of its parents is a regular file.
    fn create_dir_all(&self, path: &Path) -> Result<()>;

    /// Makes the entries of the directory at `path` durable. On targets that offer no way to
    /// flush a directory (neither Unix nor Windows, such as WASI), it only checks that the
    /// directory exists. A regular file is refused with [`ErrorKind::InvalidInput`].
    fn sync_dir(&self, path: &Path) -> Result<()>;

    /// Whether a file or directory exists at `path`; a path that runs through a regular file
    /// does not exist.
    fn exists(&self, path: &Path) -> Result<bool>;

    /// Creates a queue for asynchronous reads, writes and syncs of this file system's files.
    /// Fails with [`ErrorKind::InvalidInput`] when the options ask for no capacity, more than
    /// [`IoQueueOptions::MAX_CAPACITY`] operations or more than
    /// [`IoQueueOptions::MAX_THREADS`] threads, and with the operating system's error when a
    /// thread cannot be started.
    fn io_queue(&self, options: IoQueueOptions) -> Result<Self::IoQueue>;
}

/// An open file with positional reads and writes.
///
/// Every read and write names its offset, so one handle can be shared between threads without a
/// cursor. Written bytes become durable only after [`File::sync_data`] or [`File::sync_all`]
/// returns successfully.
pub trait File: Send + Sync + std::fmt::Debug {
    /// Reads into `buf` starting at `offset` and returns how many bytes were read. Returns 0 at
    /// or after the end of the file.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize>;

    /// Writes from `buf` starting at `offset` and returns how many bytes were written. Writing
    /// past the end extends the file, filling any gap with zeros; an empty `buf` changes
    /// nothing. Fails with [`ErrorKind::PermissionDenied`] through a handle opened with
    /// [`OpenMode::Read`], as [`File::set_size`] does.
    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize>;

    /// The size of the file in bytes.
    fn size(&self) -> Result<u64>;

    /// Truncates or extends the file to `size` bytes, filling any extension with zeros. Fails
    /// with [`ErrorKind::PermissionDenied`] through a read-only handle.
    fn set_size(&self, size: u64) -> Result<()>;

    /// Whether the handle was opened with [`FileSystem::open_uncached`] and so requires
    /// aligned buffers, offsets and lengths.
    fn is_uncached(&self) -> bool;

    /// Locks the file for this handle without waiting, failing with [`ErrorKind::Locked`] when
    /// another handle, in this process or another, holds a lock that conflicts with `lock`.
    ///
    /// The lock is advisory: it stops other locks, not reads or writes. It is held until the
    /// handle is dropped, or until a crash in a simulation, and the operating system releases
    /// it when the process ends, so it never outlives its owner. Dropping the handle releases it
    /// at once, even while a child process started meanwhile still has a copy of the handle
    /// because it has not yet started its own program. A handle takes one lock for
    /// its life; a second call fails with [`ErrorKind::InvalidInput`]. Fails with
    /// [`ErrorKind::Unsupported`] on targets without file locks.
    fn try_lock(&self, lock: FileLock) -> Result<()>;

    /// Makes the file's contents and size durable. Fails with
    /// [`ErrorKind::PermissionDenied`] through a handle opened with [`OpenMode::Read`] on every
    /// operating system, as Windows cannot flush a file it may not write.
    fn sync_data(&self) -> Result<()>;

    /// Makes the file's contents, size and other metadata durable. Fails like
    /// [`File::sync_data`] through a read-only handle.
    fn sync_all(&self) -> Result<()>;

    /// Fills `buf` from `offset`, failing with [`ErrorKind::UnexpectedEof`] if the file ends
    /// first.
    fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> Result<()> {
        while !buf.is_empty() {
            let read = self.read_at(buf, offset)?;
            if read == 0 {
                return Err(Error::new(ErrorKind::UnexpectedEof));
            }
            buf = &mut buf[read..];
            offset = advance(offset, read)?;
        }
        Ok(())
    }

    /// Writes all of `buf` from `offset`.
    fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> Result<()> {
        while !buf.is_empty() {
            let written = self.write_at(buf, offset)?;
            if written == 0 {
                return Err(Error::new(ErrorKind::WriteZero));
            }
            buf = &buf[written..];
            offset = advance(offset, written)?;
        }
        Ok(())
    }
}

pub(crate) fn advance(offset: u64, by: usize) -> Result<u64> {
    u64::try_from(by)
        .ok()
        .and_then(|by| offset.checked_add(by))
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput))
}
