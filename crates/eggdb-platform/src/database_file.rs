//! The file a database lives in: one of the operating system's, or one held in memory for a
//! database that lasts only as long as it is open.

use crate::error::Result;
use crate::fs::{File, FileLock, FileSystem, OpenMode};
use crate::os_fs::OsFile;
use crate::sim_fs::{SimFile, SimFileSystem};
use std::path::Path;

/// A database's file: a file of the operating system's, or a file in memory.
#[derive(Debug)]
pub enum DatabaseFile {
    /// A file on disk.
    Os(OsFile),
    /// A file held in memory, gone once its last handle is dropped.
    Memory(SimFile),
}

impl DatabaseFile {
    /// A new, empty file in memory, opened for writing.
    pub fn memory() -> Result<Self> {
        SimFileSystem::new()
            .open(Path::new("memory"), OpenMode::CreateNew)
            .map(Self::Memory)
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
        }
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize> {
        match self {
            Self::Os(file) => file.write_at(buf, offset),
            Self::Memory(file) => file.write_at(buf, offset),
        }
    }

    fn size(&self) -> Result<u64> {
        match self {
            Self::Os(file) => file.size(),
            Self::Memory(file) => file.size(),
        }
    }

    fn set_size(&self, size: u64) -> Result<()> {
        match self {
            Self::Os(file) => file.set_size(size),
            Self::Memory(file) => file.set_size(size),
        }
    }

    fn is_uncached(&self) -> bool {
        match self {
            Self::Os(file) => file.is_uncached(),
            Self::Memory(file) => file.is_uncached(),
        }
    }

    fn try_lock(&self, lock: FileLock) -> Result<()> {
        match self {
            Self::Os(file) => file.try_lock(lock),
            Self::Memory(file) => file.try_lock(lock),
        }
    }

    fn sync_data(&self) -> Result<()> {
        match self {
            Self::Os(file) => file.sync_data(),
            Self::Memory(_) => Ok(()),
        }
    }

    fn sync_all(&self) -> Result<()> {
        match self {
            Self::Os(file) => file.sync_all(),
            Self::Memory(_) => Ok(()),
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
