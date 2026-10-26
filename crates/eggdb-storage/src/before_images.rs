use crate::error::StorageError;
use crate::format::PageHeader;
use eggdb_platform::{Error, ErrorKind};
use std::cell::Cell;
use std::collections::BTreeMap;

#[derive(Debug)]
pub(crate) struct PageImage {
    pub(crate) header: PageHeader,
    pub(crate) checked: Cell<bool>,
    pub(crate) bytes: Box<[u8]>,
}

#[derive(Debug)]
pub(crate) struct BeforeImages {
    images: BTreeMap<(u64, u64), PageImage>,
    images_per_version: BTreeMap<u64, usize>,
    readers: Vec<(u64, usize)>,
    committed: u64,
    expired_below: u64,
    budget_pages: usize,
}

impl BeforeImages {
    pub(crate) fn new(budget_pages: usize) -> Self {
        Self {
            images: BTreeMap::new(),
            images_per_version: BTreeMap::new(),
            readers: Vec::new(),
            committed: 0,
            expired_below: 0,
            budget_pages,
        }
    }

    fn pending(&self) -> u64 {
        self.committed.saturating_add(1)
    }

    pub(crate) fn len(&self) -> usize {
        self.images.len()
    }

    pub(crate) fn set_budget(&mut self, budget_pages: usize) {
        self.budget_pages = budget_pages;
        while self.images.len() > self.budget_pages {
            if !self.expire_oldest() {
                self.expire_every_reader();
            }
        }
    }

    pub(crate) fn register_reader(&mut self) -> Result<u64, StorageError> {
        let version = self.committed;
        match self.readers.last_mut() {
            Some((newest, count)) if *newest == version => *count += 1,
            _ => {
                self.readers.try_reserve(1).map_err(|_| out_of_memory())?;
                self.readers.push((version, 1));
            }
        }
        Ok(version)
    }

    pub(crate) fn release_reader(&mut self, version: u64) {
        let Ok(index) = self
            .readers
            .binary_search_by_key(&version, |&(listed, _)| listed)
        else {
            return;
        };
        self.readers[index].1 -= 1;
        if self.readers[index].1 == 0 {
            self.readers.remove(index);
            self.prune();
        }
    }

    pub(crate) fn check_live(&self, version: u64) -> Result<(), StorageError> {
        if version < self.expired_below {
            return Err(StorageError::SnapshotTooOld);
        }
        Ok(())
    }

    pub(crate) fn wants_images(&self) -> bool {
        self.newest_live_reader().is_some()
    }

    fn newest_live_reader(&self) -> Option<u64> {
        self.readers
            .last()
            .map(|&(newest, _)| newest)
            .filter(|&newest| newest >= self.expired_below)
    }

    pub(crate) fn wants_image_of(&self, page_id: u64) -> bool {
        !self.has_pending_image(page_id) && self.is_needed(page_id)
    }

    fn is_needed(&self, page_id: u64) -> bool {
        let Some(newest) = self.newest_live_reader() else {
            return false;
        };
        let last_change = self
            .images
            .range((page_id, 0)..=(page_id, u64::MAX))
            .next_back()
            .map(|(&(_, tagged), _)| tagged);
        last_change.is_none_or(|tagged| tagged <= newest)
    }

    pub(crate) fn has_pending_image(&self, page_id: u64) -> bool {
        self.images.contains_key(&(page_id, self.pending()))
    }

    pub(crate) fn image_for(&self, page_id: u64, version: u64) -> Option<&PageImage> {
        if self.images.is_empty() {
            return None;
        }
        self.images
            .range((page_id, version.saturating_add(1))..=(page_id, u64::MAX))
            .next()
            .map(|(_, image)| image)
    }

    pub(crate) fn keep_pending(
        &mut self,
        page_id: u64,
        header: PageHeader,
        checked: bool,
        page: &[u8],
    ) -> Result<(), StorageError> {
        if !self.wants_image_of(page_id) {
            return Ok(());
        }
        while self.images.len() >= self.budget_pages {
            if !self.expire_oldest() {
                self.expire_every_reader();
            }
            if !self.wants_images() {
                return Ok(());
            }
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(page.len())
            .map_err(|_| out_of_memory())?;
        bytes.extend_from_slice(page);
        let pending = self.pending();
        self.images.insert(
            (page_id, pending),
            PageImage {
                header,
                checked: Cell::new(checked),
                bytes: bytes.into_boxed_slice(),
            },
        );
        *self.images_per_version.entry(pending).or_insert(0) += 1;
        Ok(())
    }

    pub(crate) fn commit(&mut self) {
        self.committed = self.pending();
        self.prune();
    }

    pub(crate) fn discard_pending(&mut self) {
        self.drop_version(self.pending());
        self.expired_below = self.expired_below.min(self.committed);
    }

    pub(crate) fn discard_all(&mut self) {
        self.images.clear();
        self.images_per_version.clear();
    }

    fn expire_every_reader(&mut self) {
        self.expired_below = self.pending();
        self.discard_all();
    }

    fn expire_oldest(&mut self) -> bool {
        let Some((&oldest, _)) = self.images_per_version.first_key_value() else {
            return false;
        };
        self.expired_below = self.expired_below.max(oldest);
        self.drop_version(oldest);
        self.prune();
        true
    }

    fn prune(&mut self) {
        let oldest_live = self
            .readers
            .iter()
            .map(|&(version, _)| version)
            .find(|&version| version >= self.expired_below);
        let needed_from = oldest_live.map_or(u64::MAX, |version| version.saturating_add(1));
        while let Some((&version, _)) = self.images_per_version.first_key_value() {
            if version >= needed_from {
                break;
            }
            self.drop_version(version);
        }
    }

    fn drop_version(&mut self, version: u64) {
        if self.images_per_version.remove(&version).is_some() {
            self.images.retain(|&(_, tagged), _| tagged != version);
        }
    }
}

fn out_of_memory() -> StorageError {
    StorageError::Io(Error::new(ErrorKind::OutOfMemory))
}
