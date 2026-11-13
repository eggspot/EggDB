//! A database file on raw flash (D1083): the [`File`] the page file reads and writes, kept in a
//! wear-levelled flash region whose blocks are written atomically, so the engine, its log and
//! its two header slots run on a microcontroller's flash as on a disk.

use crate::sync::Mutex;
use alloc::vec;
use core::fmt;
use eggdb_format::{Flash, WearError, WearLevelled};
use eggdb_platform_core::{Error, ErrorKind, File, FileLock, Result};
#[cfg(not(feature = "std"))]
#[allow(unused_imports)]
use crate::prelude::*;

/// A file of bytes over a [`WearLevelled`] flash region: reads and writes are cut into the
/// region's blocks (read, changed and written back whole), its size is kept in the region's
/// journal, and every write is durable once it returns, so syncing has nothing left to do.
pub struct FlashFile<F: Flash> {
    region: Mutex<WearLevelled<F>>,
}

impl<F: Flash> fmt::Debug for FlashFile<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlashFile").finish_non_exhaustive()
    }
}

fn failed<E>(error: WearError<E>) -> Error {
    Error::new(match error {
        WearError::OutOfRange => ErrorKind::InvalidInput,
        WearError::Full => ErrorKind::WriteZero,
        WearError::Geometry => ErrorKind::Unsupported,
        WearError::Flash(_) => ErrorKind::Other,
    })
}

impl<F: Flash> FlashFile<F> {
    /// The file kept in `region`.
    pub fn new(region: WearLevelled<F>) -> Self {
        Self { region: Mutex::new(region) }
    }

    /// The most bytes the file can hold.
    pub fn capacity(&self) -> Result<u64> {
        let region = self.region.lock().map_err(|_| Error::new(ErrorKind::Other))?;
        Ok(u64::from(region.block_count()) * region.block_size() as u64)
    }

    fn change<T>(&self, body: impl FnOnce(&mut WearLevelled<F>) -> Result<T>) -> Result<T> {
        let mut region = self.region.lock().map_err(|_| Error::new(ErrorKind::Other))?;
        body(&mut region)
    }
}

fn blocks_of(offset: u64, length: usize, block: usize) -> impl Iterator<Item = (u32, usize, usize, usize)> {
    let block = block as u64;
    let end = offset + length as u64;
    let first = offset / block;
    let last = if length == 0 { first } else { (end - 1) / block + 1 };
    (first..last).map(move |index| {
        let start = (index * block).max(offset);
        let stop = ((index + 1) * block).min(end);
        (index as u32, (start - index * block) as usize, (start - offset) as usize, (stop - start) as usize)
    })
}

impl<F: Flash + Send> File for FlashFile<F> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.change(|region| {
            let size = region.logical_size();
            if offset >= size {
                return Ok(0);
            }
            let wanted = buf.len().min(usize::try_from(size - offset).unwrap_or(usize::MAX));
            let mut block = vec![0u8; region.block_size()];
            for (index, within, into, length) in blocks_of(offset, wanted, region.block_size()) {
                region.read_block(index, &mut block).map_err(failed)?;
                buf[into..into + length].copy_from_slice(&block[within..within + length]);
            }
            Ok(wanted)
        })
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.change(|region| {
            let end = offset.checked_add(buf.len() as u64).ok_or_else(|| Error::new(ErrorKind::InvalidInput))?;
            if end > u64::from(region.block_count()) * region.block_size() as u64 {
                return Err(Error::new(ErrorKind::WriteZero));
            }
            let mut block = vec![0u8; region.block_size()];
            for (index, within, from, length) in blocks_of(offset, buf.len(), region.block_size()) {
                if length < block.len() {
                    region.read_block(index, &mut block).map_err(failed)?;
                }
                block[within..within + length].copy_from_slice(&buf[from..from + length]);
                region.write_block(index, &block).map_err(failed)?;
            }
            if end > region.logical_size() {
                region.set_logical_size(end).map_err(failed)?;
            }
            Ok(buf.len())
        })
    }

    fn size(&self) -> Result<u64> {
        self.change(|region| Ok(region.logical_size()))
    }

    fn set_size(&self, size: u64) -> Result<()> {
        self.change(|region| {
            let block_bytes = region.block_size() as u64;
            if size > u64::from(region.block_count()) * block_bytes {
                return Err(Error::new(ErrorKind::WriteZero));
            }
            let old = region.logical_size();
            if size < old {
                let first_gone = size.div_ceil(block_bytes);
                let last = old.div_ceil(block_bytes);
                for index in first_gone..last {
                    region.trim_block(index as u32).map_err(failed)?;
                }
                if size % block_bytes != 0 {
                    let index = (size / block_bytes) as u32;
                    let mut block = vec![0u8; region.block_size()];
                    region.read_block(index, &mut block).map_err(failed)?;
                    block[(size % block_bytes) as usize..].fill(0);
                    region.write_block(index, &block).map_err(failed)?;
                }
            }
            region.set_logical_size(size).map_err(failed)
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
    use super::*;
    use crate::{CreateOptions, PageFile};

    #[derive(Debug)]
    struct Chip {
        bytes: Vec<u8>,
    }

    impl Flash for Chip {
        type Error = ();

        fn capacity(&self) -> u64 {
            self.bytes.len() as u64
        }

        fn sector_size(&self) -> u32 {
            4096
        }

        fn read(&mut self, offset: u64, buffer: &mut [u8]) -> core::result::Result<(), ()> {
            let start = offset as usize;
            buffer.copy_from_slice(self.bytes.get(start..start + buffer.len()).ok_or(())?);
            Ok(())
        }

        fn erase(&mut self, offset: u64) -> core::result::Result<(), ()> {
            let start = offset as usize;
            self.bytes.get_mut(start..start + 4096).ok_or(())?.fill(0xFF);
            Ok(())
        }

        fn program(&mut self, offset: u64, data: &[u8]) -> core::result::Result<(), ()> {
            let start = offset as usize;
            let target = self.bytes.get_mut(start..start + data.len()).ok_or(())?;
            if target.iter().zip(data).any(|(old, new)| old & new != *new) {
                return Err(());
            }
            target.copy_from_slice(data);
            Ok(())
        }
    }

    #[test]
    fn bytes_cross_blocks_and_the_size_is_kept() {
        let file = FlashFile::new(WearLevelled::format(Chip { bytes: vec![0xFF; 256 * 4096] }, 8).unwrap());
        file.write_all_at(&[7u8; 10_000], 3_000).unwrap();
        assert_eq!(file.size().unwrap(), 13_000);
        let mut read = vec![0u8; 13_000];
        file.read_exact_at(&mut read, 0).unwrap();
        assert!(read[..3_000].iter().all(|byte| *byte == 0));
        assert!(read[3_000..].iter().all(|byte| *byte == 7));
        file.set_size(5_000).unwrap();
        file.set_size(9_000).unwrap();
        let mut regrown = vec![1u8; 4_000];
        file.read_exact_at(&mut regrown, 5_000).unwrap();
        assert!(regrown.iter().all(|byte| *byte == 0), "bytes past a shrink read as zeros again");
    }

    #[test]
    fn a_database_file_is_made_on_flash_with_both_header_slots() {
        let file = FlashFile::new(WearLevelled::format(Chip { bytes: vec![0xFF; 512 * 4096] }, 8).unwrap());
        let page_file = PageFile::create_in(file, CreateOptions::default()).unwrap();
        assert!(!page_file.is_encrypted());
    }
}
