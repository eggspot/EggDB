//! Changed pages moved out of a full page cache before their transaction commits (D1333): a
//! scratch file holds them, one page per slot, until the commit logs them or a rollback drops
//! them. The database file sees nothing of them before the commit, so recovery is unchanged.

use crate::error::StorageError;
use crate::format::PageHeader;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use eggdb_platform_core::{ErrorKind, File};

/// Opens the scratch file a [`PageCache`](crate::PageCache) spills changed pages to, the first
/// time it needs one.
pub type SpillOpener = Box<dyn FnMut() -> Result<Box<dyn File>, StorageError> + Send>;

#[derive(Debug, Clone, Copy)]
pub(crate) struct SpilledPage {
    pub(crate) slot: u64,
    pub(crate) header: PageHeader,
    pub(crate) state: u8,
}

#[derive(Default)]
pub(crate) struct Spill {
    opener: Option<SpillOpener>,
    file: Option<Box<dyn File>>,
    pub(crate) pages: BTreeMap<u64, SpilledPage>,
    free_slots: Vec<u64>,
    slots: u64,
}

fn short_write() -> StorageError {
    StorageError::Io(eggdb_platform_core::Error::new(ErrorKind::WriteZero))
}

fn short_read() -> StorageError {
    StorageError::Io(eggdb_platform_core::Error::new(ErrorKind::UnexpectedEof))
}

impl Spill {
    pub(crate) fn set_opener(&mut self, opener: SpillOpener) {
        self.opener = Some(opener);
    }

    pub(crate) fn can_spill(&self) -> bool {
        self.opener.is_some()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    fn file(&mut self) -> Result<&dyn File, StorageError> {
        if self.file.is_none() {
            let opener = self.opener.as_mut().ok_or(StorageError::NeedsReopen)?;
            let file = opener()?;
            file.set_size(0).map_err(StorageError::Io)?;
            self.file = Some(file);
        }
        self.file.as_deref().ok_or(StorageError::NeedsReopen)
    }

    /// Writes `page` to a free slot and records it as `page_id`'s current bytes.
    pub(crate) fn keep(&mut self, page_id: u64, page: &[u8], header: PageHeader, state: u8) -> Result<(), StorageError> {
        let slot = match self.free_slots.pop() {
            Some(slot) => slot,
            None => {
                self.slots += 1;
                self.slots - 1
            }
        };
        let offset = slot * page.len() as u64;
        let written = self.file().and_then(|file| write_all_at(file, page, offset));
        if let Err(error) = written {
            self.free_slots.push(slot);
            return Err(error);
        }
        self.pages.insert(page_id, SpilledPage { slot, header, state });
        Ok(())
    }

    /// Reads the spilled bytes of `spilled` into `page`.
    pub(crate) fn read(&mut self, spilled: SpilledPage, page: &mut [u8]) -> Result<(), StorageError> {
        let offset = spilled.slot * page.len() as u64;
        let file = self.file()?;
        read_exact_at(file, page, offset)
    }

    /// Forgets `page_id`, freeing its slot.
    pub(crate) fn forget(&mut self, page_id: u64) -> Option<SpilledPage> {
        let spilled = self.pages.remove(&page_id)?;
        self.free_slots.push(spilled.slot);
        Some(spilled)
    }

    /// Forgets every spilled page and empties the scratch file.
    pub(crate) fn clear(&mut self) {
        self.pages.clear();
        self.free_slots.clear();
        self.slots = 0;
        if let Some(file) = &self.file {
            let _ = file.set_size(0);
        }
    }

    /// The spilled pages, in page order.
    pub(crate) fn listed(&self) -> Vec<(u64, SpilledPage)> {
        self.pages.iter().map(|(&page_id, &spilled)| (page_id, spilled)).collect()
    }
}

fn write_all_at(file: &dyn File, mut bytes: &[u8], mut offset: u64) -> Result<(), StorageError> {
    while !bytes.is_empty() {
        let written = file.write_at(bytes, offset).map_err(StorageError::Io)?;
        if written == 0 {
            return Err(short_write());
        }
        bytes = &bytes[written..];
        offset += written as u64;
    }
    Ok(())
}

fn read_exact_at(file: &dyn File, mut bytes: &mut [u8], mut offset: u64) -> Result<(), StorageError> {
    while !bytes.is_empty() {
        let read = file.read_at(bytes, offset).map_err(StorageError::Io)?;
        if read == 0 {
            return Err(short_read());
        }
        bytes = &mut bytes[read..];
        offset += read as u64;
    }
    Ok(())
}
