use crate::error::{Error, ErrorKind, Result};
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

/// The namespace operations of a file system.
///
/// Creating, renaming and removing a file only become durable once the parent directory is
/// synced with [`FileSystem::sync_dir`], as on POSIX systems; code that must survive a crash
/// syncs the directory after changing its entries.
pub trait FileSystem: Send + Sync {
    /// The handle type for open files.
    type File: File;

    /// Opens the file at `path`.
    fn open(&self, path: &Path, mode: OpenMode) -> Result<Self::File>;

    /// Removes the file at `path`. Open handles keep working until they are dropped.
    fn remove_file(&self, path: &Path) -> Result<()>;

    /// Renames `from` to `to`, replacing `to` if it exists.
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;

    /// Creates the directory at `path` and every missing parent.
    fn create_dir_all(&self, path: &Path) -> Result<()>;

    /// Makes the entries of the directory at `path` durable.
    fn sync_dir(&self, path: &Path) -> Result<()>;

    /// Whether a file or directory exists at `path`.
    fn exists(&self, path: &Path) -> Result<bool>;
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
    /// past the end extends the file, filling any gap with zeros.
    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize>;

    /// The size of the file in bytes.
    fn size(&self) -> Result<u64>;

    /// Truncates or extends the file to `size` bytes, filling any extension with zeros.
    fn set_size(&self, size: u64) -> Result<()>;

    /// Makes the file's contents and size durable.
    fn sync_data(&self) -> Result<()>;

    /// Makes the file's contents, size and other metadata durable.
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
