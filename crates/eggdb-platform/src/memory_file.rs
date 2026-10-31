use crate::error::{Error, ErrorKind, Result};
use crate::fs::{File, FileLock};
use std::sync::RwLock;

/// A file held in memory for a database that lasts only as long as it is open: it keeps its
/// current bytes and nothing else, and its syncs do nothing.
#[derive(Debug, Default)]
pub struct MemoryFile {
    bytes: RwLock<Vec<u8>>,
}

impl MemoryFile {
    /// A new, empty file.
    pub fn new() -> Self {
        Self::default()
    }

    fn read_bytes<T>(&self, body: impl FnOnce(&Vec<u8>) -> T) -> T {
        let guard = self.bytes.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        body(&guard)
    }

    fn write_bytes<T>(&self, body: impl FnOnce(&mut Vec<u8>) -> T) -> T {
        let mut guard = self.bytes.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        body(&mut guard)
    }
}

fn index(value: u64) -> Result<usize> {
    usize::try_from(value).map_err(|_| Error::new(ErrorKind::InvalidInput))
}

fn grow(bytes: &mut Vec<u8>, size: usize) -> Result<()> {
    if size <= bytes.len() {
        return Ok(());
    }
    bytes
        .try_reserve(size - bytes.len())
        .map_err(|_| Error::new(ErrorKind::OutOfMemory))?;
    bytes.resize(size, 0);
    Ok(())
}

impl File for MemoryFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.read_bytes(|bytes| {
            let Some(available) = usize::try_from(offset)
                .ok()
                .and_then(|start| bytes.get(start..))
            else {
                return Ok(0);
            };
            let read = available.len().min(buf.len());
            buf[..read].copy_from_slice(&available[..read]);
            Ok(read)
        })
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let start = index(offset)?;
        let end = start
            .checked_add(buf.len())
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput))?;
        self.write_bytes(|bytes| {
            grow(bytes, end)?;
            bytes[start..end].copy_from_slice(buf);
            Ok(buf.len())
        })
    }

    fn size(&self) -> Result<u64> {
        self.read_bytes(|bytes| u64::try_from(bytes.len()).map_err(|_| Error::new(ErrorKind::InvalidInput)))
    }

    fn set_size(&self, size: u64) -> Result<()> {
        let size = index(size)?;
        self.write_bytes(|bytes| {
            if size < bytes.len() {
                bytes.truncate(size);
                bytes.shrink_to(size);
                return Ok(());
            }
            grow(bytes, size)
        })
    }

    fn is_uncached(&self) -> bool {
        false
    }

    fn try_lock(&self, _lock: FileLock) -> Result<()> {
        Ok(())
    }

    fn sync_data(&self) -> Result<()> {
        Ok(())
    }

    fn sync_all(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::MemoryFile;
    use crate::error::ErrorKind;
    use crate::fs::File;

    #[test]
    fn rewrites_keep_only_the_current_bytes_and_reads_past_the_end_are_short() {
        let file = MemoryFile::new();
        let page = vec![7u8; 1 << 16];
        for _ in 0..(20_000) {
            file.write_at(&page, 0).unwrap();
        }
        assert_eq!(file.size().unwrap(), 1 << 16);
        let mut read = [0u8; 8];
        assert_eq!(file.read_at(&mut read, (1 << 16) - 4).unwrap(), 4);
        assert_eq!(file.read_at(&mut read, 1 << 20).unwrap(), 0);
        file.set_size(10).unwrap();
        assert_eq!(file.size().unwrap(), 10);
        assert_eq!(
            file.write_at(b"x", u64::MAX).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
    }
}
