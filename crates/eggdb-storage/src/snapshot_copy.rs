use crate::btree::BTree;
use crate::cursor::ScanOrder;
use crate::error::StorageError;
use crate::format::{Access, Features, FileId, NO_PAGE, PageSize};
use crate::lsm::LsmTree;
use crate::hybrid_clock::CommitStamp;
use crate::log::Durability;
use crate::overflow;
use crate::page_cache::{CacheSize, PageCache};
use crate::page_file::{CreateOptions, PageFile, parent_directory};
use crate::store::{ReadTransaction, Store};
use eggdb_platform::{Clock, File, FileSystem, Memory};
use std::fmt;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

const COPY_CACHE_BYTES: usize = 16 << 20;
const COPY_CACHE_MARGIN_PAGES: u64 = 64;
const BATCH_SHARE_OF_CACHE: u64 = 4;
const ENTRY_OVERHEAD_BYTES: u64 = 16;

/// Where [`ReadTransaction::copy_to`] builds a copy and where the finished copy goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyDestination<'p> {
    /// The path the finished copy gets.
    pub path: &'p Path,
    /// The path the copy is built at, renamed to `path` only once it is complete, durable and
    /// checked. It must be in the same directory as `path`, and no file may be there.
    pub staging: &'p Path,
    /// Whether a file at `path` is replaced, as compacting a file does with the file itself;
    /// otherwise a file at `path` refuses the copy.
    pub replaces: bool,
    /// Where the catalog tree keeps the roots of LSM trees (D1119), from the first key
    /// (inclusive) to the second (exclusive), each value an 8-byte big-endian root. The copy
    /// folds those trees' live entries into its catalog tree, leaves the root keys out and
    /// clears [`Features::LSM_TABLES`](crate::Features::LSM_TABLES), so it holds the same
    /// entries in plain B-tree form. `None` copies the catalog tree as it is.
    pub lsm_roots: Option<(&'p [u8], &'p [u8])>,
}

/// How far a copy has come, as [`ReadTransaction::copy_to_reporting`] reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CopyProgress {
    /// The entries copied and committed so far.
    pub entries: u64,
    /// The entries the snapshot holds.
    pub total_entries: u64,
}

/// What [`ReadTransaction::copy_to`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct CopyReport {
    /// The stamp of the commit the copy holds: the snapshot's.
    pub stamp: CommitStamp,
    /// The entries copied, every one the tree holds.
    pub entries: u64,
    /// The bytes of every key copied.
    pub key_bytes: u64,
    /// The bytes of every value copied.
    pub value_bytes: u64,
    /// The pages of the copy, the header slots and the log included.
    pub page_count: u64,
    /// The copy's free pages, the free list's own included.
    pub free_pages: u64,
}

/// Why [`ReadTransaction::copy_to`] failed, and what it left behind.
#[derive(Debug)]
pub enum CopyFailure {
    /// The copy was not made: the destination is as it was, and the staging file was removed,
    /// unless it was there before the copy began.
    NotCopied(StorageError),
    /// The copy is complete, durable and checked at the staging path, but renaming it to the
    /// destination failed: the destination is as it was, and the staging file is kept.
    NotRenamed(StorageError),
    /// The copy was renamed to the destination, but syncing the directory failed, so a power
    /// loss could still undo the rename.
    RenameNotDurable(StorageError),
}

impl CopyFailure {
    /// The storage error behind the failure.
    pub fn error(&self) -> &StorageError {
        match self {
            Self::NotCopied(error) | Self::NotRenamed(error) | Self::RenameNotDurable(error) => {
                error
            }
        }
    }

    /// The storage error behind the failure, taken out.
    pub fn into_error(self) -> StorageError {
        match self {
            Self::NotCopied(error) | Self::NotRenamed(error) | Self::RenameNotDurable(error) => {
                error
            }
        }
    }
}

impl fmt::Display for CopyFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotCopied(error) => write!(f, "the copy was not made: {error}"),
            Self::NotRenamed(error) => write!(
                f,
                "the copy is complete at its staging path, but renaming it failed: {error}"
            ),
            Self::RenameNotDurable(error) => write!(
                f,
                "the copy is in place, but syncing its directory failed, so a power loss could undo the rename: {error}"
            ),
        }
    }
}

impl std::error::Error for CopyFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error())
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Survey {
    entries: u64,
    largest_value_bytes: u64,
}

impl<F: File> ReadTransaction<'_, F> {
    /// Copies what this snapshot holds into a new file at `destination.path`, while the
    /// store's writer and other readers keep working: every entry of the tree the catalog
    /// root names, the snapshot's feature flags, and a clock high-water mark at the
    /// snapshot's stamp, so the copy's commits stamp above it. The copy is rebuilt entry by
    /// entry, so it holds no free or leaked pages beyond what its own log leaves.
    ///
    /// The copy is built at `destination.staging`, committed in batches the page cache holds,
    /// checkpointed and synced, then checked: its tree must hold exactly the snapshot's
    /// entries and no page may be unused. Only then is it renamed to `destination.path` and
    /// the directory synced, so a crash at any moment leaves `destination.path` as it was or
    /// the whole copy, never a part of it. [`CopyFailure`] says what a failure left behind.
    ///
    /// A file at `destination.path` refuses the copy with an
    /// [`ErrorKind::AlreadyExists`](eggdb_platform::ErrorKind::AlreadyExists) error, unless
    /// `destination.replaces`; so does a file at `destination.staging`, which is left alone. A
    /// snapshot whose features this release may only read is refused with
    /// [`FormatError::ReadOnlyFeatures`](crate::FormatError::ReadOnlyFeatures), since the copy
    /// would be written without knowing them. A copy that fails its check fails with
    /// [`StorageError::CopyIncomplete`]. A writer that changes more pages than the snapshot
    /// memory holds while the copy runs expires the snapshot, and the copy fails with
    /// [`StorageError::SnapshotTooOld`].
    pub fn copy_to<S: FileSystem>(
        &self,
        fs: &S,
        memory: &impl Memory,
        clock: Arc<dyn Clock>,
        destination: CopyDestination<'_>,
    ) -> Result<CopyReport, CopyFailure> {
        self.copy_to_reporting(fs, memory, clock, destination, &mut |_| {})
    }

    /// Copies the snapshot as [`ReadTransaction::copy_to`] does, calling `progress` after each
    /// batch of entries is committed to the copy (D1023).
    pub fn copy_to_reporting<S: FileSystem>(
        &self,
        fs: &S,
        memory: &impl Memory,
        clock: Arc<dyn Clock>,
        destination: CopyDestination<'_>,
        progress: &mut dyn FnMut(CopyProgress),
    ) -> Result<CopyReport, CopyFailure> {
        let report = self
            .build_copy(fs, memory, clock, destination, progress)
            .map_err(CopyFailure::NotCopied)?;
        refuse_existing(fs, destination).map_err(|error| {
            let _ = fs.remove_file(destination.staging);
            CopyFailure::NotCopied(error)
        })?;
        fs.rename(destination.staging, destination.path)
            .map_err(|error| CopyFailure::NotRenamed(error.into()))?;
        fs.sync_dir(parent_directory(destination.path))
            .map_err(|error| CopyFailure::RenameNotDurable(error.into()))?;
        Ok(report)
    }

    fn build_copy<S: FileSystem>(
        &self,
        fs: &S,
        memory: &impl Memory,
        clock: Arc<dyn Clock>,
        destination: CopyDestination<'_>,
        progress: &mut dyn FnMut(CopyProgress),
    ) -> Result<CopyReport, StorageError> {
        self.features().check(Access::ReadWrite)?;
        refuse_existing(fs, destination)?;
        let source = match self.catalog_root() {
            NO_PAGE => None,
            root => Some(self.open_tree(root)?),
        };
        let folded = match (&source, destination.lsm_roots) {
            (Some(tree), Some((start, end))) => self.folded_lsm_trees(tree, start, end)?,
            _ => Vec::new(),
        };
        let survey = match &source {
            Some(tree) => self.survey(tree, destination.lsm_roots, &folded)?,
            None => Survey::default(),
        };
        let options = CreateOptions {
            page_size: self.page_size(),
            file_id: FileId::random(),
        };
        let mut file = PageFile::create(fs, destination.staging, options).inspect_err(|error| {
            if !is_already_exists(error) {
                let _ = fs.remove_file(destination.staging);
            }
        })?;
        if let Some(cipher) = self.page_cipher()? {
            file.adopt_cipher(cipher).inspect_err(|_| {
                let _ = fs.remove_file(destination.staging);
            })?;
        }
        let skipped = destination.lsm_roots;
        self.fill(file, memory, clock, source, survey, Folding { skipped, trees: &folded }, progress)
            .inspect_err(|_| {
                let _ = fs.remove_file(destination.staging);
            })
    }

    fn folded_lsm_trees(
        &self,
        tree: &BTree,
        start: &[u8],
        end: &[u8],
    ) -> Result<Vec<LsmTree>, StorageError> {
        let mut cursor = tree.scan(Bound::Included(start), Bound::Excluded(end), ScanOrder::Ascending);
        let mut roots = Vec::new();
        while let Some((_, value)) = self.next(&mut cursor)? {
            let bytes: [u8; 8] = value.try_into().map_err(|_| {
                StorageError::from(crate::error::FormatError::CorruptLsm {
                    problem: "an LSM table's root is not 8 bytes",
                })
            })?;
            roots.push(u64::from_be_bytes(bytes));
        }
        roots.into_iter().map(|root| self.open_lsm_tree(root)).collect()
    }

    fn survey(
        &self,
        tree: &BTree,
        skipped: Option<(&[u8], &[u8])>,
        folded: &[LsmTree],
    ) -> Result<Survey, StorageError> {
        let mut cursor = tree.scan(Bound::Unbounded, Bound::Unbounded, ScanOrder::Ascending);
        let mut survey = Survey::default();
        while let Some((key, value_bytes)) = self.next_key_and_value_len(&mut cursor)? {
            if skipped.is_some_and(|(start, end)| key >= start && key < end) {
                continue;
            }
            survey.entries += 1;
            survey.largest_value_bytes = survey.largest_value_bytes.max(value_bytes);
        }
        for lsm in folded {
            let mut cursor = self.lsm_scan(lsm, Bound::Unbounded, Bound::Unbounded, ScanOrder::Ascending)?;
            while let Some((_, value)) = self.lsm_next(&mut cursor)? {
                survey.entries += 1;
                survey.largest_value_bytes = survey.largest_value_bytes.max(value.len() as u64);
            }
        }
        Ok(survey)
    }

    #[allow(clippy::too_many_arguments)]
    fn fill<G: File>(
        &self,
        file: PageFile<G>,
        memory: &impl Memory,
        clock: Arc<dyn Clock>,
        source: Option<BTree>,
        survey: Survey,
        folding: Folding<'_>,
        progress: &mut dyn FnMut(CopyProgress),
    ) -> Result<CopyReport, StorageError> {
        let page_size = file.page_size();
        let frames = copy_cache_frames(page_size, survey.largest_value_bytes);
        let mut cache = PageCache::new(file, memory, CacheSize::Pages(frames))?;
        cache.set_durability(Durability::Normal)?;
        let target = match source {
            Some(_) => Some(BTree::create(&cache)?),
            None => None,
        };
        let catalog_root = target.map_or(NO_PAGE, |tree| tree.root());
        let mut features = self.features();
        features.incompatible &= !(Features::COMPRESSED_VALUES | Features::ZSTD_VALUES);
        if folding.skipped.is_some() {
            features.incompatible &= !Features::LSM_TABLES;
        }
        let high_water = self.stamp().bits();
        cache.commit(|superblock| {
            superblock.catalog_root = catalog_root;
            superblock.features = features;
            superblock.clock_high_water = high_water;
        })?;
        let store = Store::new(cache, clock);
        let mut report = CopyReport {
            stamp: self.stamp(),
            ..CopyReport::default()
        };
        if let (Some(source), Some(target)) = (source, target) {
            let batch_bytes =
                (frames as u64).saturating_mul(page_size.bytes() as u64) / BATCH_SHARE_OF_CACHE;
            self.copy_entries(&source, &store, &target, batch_bytes, &mut report, survey.entries, folding, progress)?;
        }
        store.checkpoint()?;
        store.sync()?;
        let checked = store.check()?;
        let found = checked.tree.map_or(0, |tree| tree.entries);
        if found != survey.entries || found != report.entries || checked.leaked_pages != 0 {
            return Err(StorageError::CopyIncomplete {
                expected: survey.entries,
                found,
                leaked_pages: checked.leaked_pages,
            });
        }
        report.page_count = checked.page_count;
        report.free_pages = checked.free_pages;
        Ok(report)
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_entries<G: File>(
        &self,
        source: &BTree,
        store: &Store<G>,
        target: &BTree,
        batch_bytes: u64,
        report: &mut CopyReport,
        total: u64,
        folding: Folding<'_>,
        progress: &mut dyn FnMut(CopyProgress),
    ) -> Result<(), StorageError> {
        let mut cursor = source.scan(Bound::Unbounded, Bound::Unbounded, ScanOrder::Ascending);
        let mut batch = Batch {
            transaction: store.begin_write()?,
            bytes: 0,
            limit: batch_bytes,
        };
        while let Some((key, value)) = self.next(&mut cursor)? {
            if folding.skipped.is_some_and(|(start, end)| key >= start && key < end) {
                continue;
            }
            batch.put(store, target, key, value, report, total, progress)?;
        }
        for lsm in folding.trees {
            let mut lsm_cursor = self.lsm_scan(lsm, Bound::Unbounded, Bound::Unbounded, ScanOrder::Ascending)?;
            while let Some((key, value)) = self.lsm_next(&mut lsm_cursor)? {
                batch.put(store, target, key, value, report, total, progress)?;
            }
        }
        let transaction = batch.transaction;
        transaction.commit()?;
        progress(CopyProgress { entries: report.entries, total_entries: total });
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Folding<'f> {
    skipped: Option<(&'f [u8], &'f [u8])>,
    trees: &'f [LsmTree],
}

struct Batch<'s, G: File> {
    transaction: crate::store::WriteTransaction<'s, G>,
    bytes: u64,
    limit: u64,
}

impl<'s, G: File> Batch<'s, G> {
    #[allow(clippy::too_many_arguments)]
    fn put(
        &mut self,
        store: &'s Store<G>,
        target: &BTree,
        key: &[u8],
        value: &[u8],
        report: &mut CopyReport,
        total: u64,
        progress: &mut dyn FnMut(CopyProgress),
    ) -> Result<(), StorageError> {
        let entry_bytes = (key.len() + value.len()) as u64 + ENTRY_OVERHEAD_BYTES;
        if self.bytes > 0 && self.bytes.saturating_add(entry_bytes) > self.limit {
            let finished = core::mem::replace(&mut self.transaction, store.begin_write()?);
            finished.commit()?;
            progress(CopyProgress { entries: report.entries, total_entries: total });
            self.bytes = 0;
        }
        self.transaction.put(target, key, value)?;
        self.bytes = self.bytes.saturating_add(entry_bytes);
        report.entries += 1;
        report.key_bytes += key.len() as u64;
        report.value_bytes += value.len() as u64;
        Ok(())
    }
}

fn copy_cache_frames(page_size: PageSize, largest_value_bytes: u64) -> usize {
    let base = CacheSize::Bytes(COPY_CACHE_BYTES).pages(page_size) as u64;
    let for_largest_value =
        overflow::pages_for(largest_value_bytes, page_size).saturating_add(COPY_CACHE_MARGIN_PAGES);
    usize::try_from(base.max(for_largest_value)).unwrap_or(usize::MAX)
}

fn refuse_existing<S: FileSystem>(
    fs: &S,
    destination: CopyDestination<'_>,
) -> Result<(), StorageError> {
    if !destination.replaces && fs.exists(destination.path)? {
        return Err(already_exists());
    }
    Ok(())
}

fn is_already_exists(error: &StorageError) -> bool {
    matches!(error, StorageError::Io(error) if error.kind() == eggdb_platform::ErrorKind::AlreadyExists)
}

fn already_exists() -> StorageError {
    StorageError::Io(eggdb_platform::Error::new(
        eggdb_platform::ErrorKind::AlreadyExists,
    ))
}
