//! The file a database lives in: one of the operating system's, or one held in memory for a
//! database that lasts only as long as it is open.

use crate::error::Result;
use crate::fs::{File, FileLock};
use crate::memory_file::MemoryFile;
use crate::os_fs::OsFile;

/// A database's file: a file of the operating system's, a file in memory, or a file of the
/// caller's own, such as one whose pages live in object storage (D1099).
#[derive(Debug)]
pub enum DatabaseFile {
    /// A file on disk.
    Os(OsFile),
    /// A file held in memory, gone once its last handle is dropped.
    Memory(MemoryFile),
    /// A file of the caller's own.
    Custom(CustomFile),
}

/// A [`File`] the caller supplies, held by a [`DatabaseFile`].
pub struct CustomFile(pub Box<dyn File + Send + Sync>);

impl std::fmt::Debug for CustomFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CustomFile")
    }
}

impl DatabaseFile {
    /// A new, empty file in memory, opened for writing.
    pub fn memory() -> Result<Self> {
        Ok(Self::Memory(MemoryFile::new()))
    }

    /// A file in memory holding `bytes`, opened for writing.
    pub fn memory_with(bytes: Vec<u8>) -> Self {
        Self::Memory(MemoryFile::with_bytes(bytes))
    }

    /// A database file over `file`, a [`File`] of the caller's own.
    pub fn custom(file: Box<dyn File + Send + Sync>) -> Self {
        Self::Custom(CustomFile(file))
    }

    /// Whether the file lives in memory.
    pub fn is_memory(&self) -> bool {
        matches!(self, Self::Memory(_))
    }
}

impl File for DatabaseFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        match self {
            Self::Os(file) => file.read_at(buf, offset),
            Self::Memory(file) => file.read_at(buf, offset),
            Self::Custom(file) => file.0.read_at(buf, offset),
        }
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize> {
        match self {
            Self::Os(file) => file.write_at(buf, offset),
            Self::Memory(file) => file.write_at(buf, offset),
            Self::Custom(file) => file.0.write_at(buf, offset),
        }
    }

    fn size(&self) -> Result<u64> {
        match self {
            Self::Os(file) => file.size(),
            Self::Memory(file) => file.size(),
            Self::Custom(file) => file.0.size(),
        }
    }

    fn set_size(&self, size: u64) -> Result<()> {
        match self {
            Self::Os(file) => file.set_size(size),
            Self::Memory(file) => file.set_size(size),
            Self::Custom(file) => file.0.set_size(size),
        }
    }

    fn is_uncached(&self) -> bool {
        match self {
            Self::Os(file) => file.is_uncached(),
            Self::Memory(file) => file.is_uncached(),
            Self::Custom(file) => file.0.is_uncached(),
        }
    }

    fn try_lock(&self, lock: FileLock) -> Result<()> {
        match self {
            Self::Os(file) => file.try_lock(lock),
            Self::Memory(file) => file.try_lock(lock),
            Self::Custom(file) => file.0.try_lock(lock),
        }
    }

    fn unlock(&self) -> Result<()> {
        match self {
            Self::Os(file) => file.unlock(),
            Self::Memory(file) => file.unlock(),
            Self::Custom(file) => file.0.unlock(),
        }
    }

    fn sync_data(&self) -> Result<()> {
        match self {
            Self::Os(file) => file.sync_data(),
            Self::Memory(_) => Ok(()),
            Self::Custom(file) => file.0.sync_data(),
        }
    }

    fn sync_all(&self) -> Result<()> {
        match self {
            Self::Os(file) => file.sync_all(),
            Self::Memory(_) => Ok(()),
            Self::Custom(file) => file.0.sync_all(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_memory_file_reads_back_what_was_written_and_starts_empty() {
        let file = DatabaseFile::memory().unwrap();
        assert!(file.is_memory());
        assert_eq!(file.size().unwrap(), 0);
        file.write_at(b"hello", 10).unwrap();
        let mut read = [0u8; 15];
        assert_eq!(file.read_at(&mut read, 0).unwrap(), 15);
        assert_eq!(&read[10..], b"hello");
        assert_eq!(&read[..10], &[0u8; 10]);
        file.sync_all().unwrap();
        file.try_lock(FileLock::Exclusive).unwrap();
    }
}
