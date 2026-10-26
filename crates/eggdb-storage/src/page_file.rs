use crate::bytes::{read_u32, read_u64, write_u32, write_u64};
use crate::error::{FormatError, StorageError};
use crate::format::{
    Access, FileId, HEADER_SLOT_SIZE, NO_PAGE, PAGE_HEADER_SIZE, PAGE_VOLUME_MASK, PageChecksum,
    PageHeader, PageSize, PageType, RECYCLED_SEGMENTS, Superblock,
};
use crate::log::{
    self, COMMIT_PAYLOAD, CommitFields, Durability, LOG_BLOCK, LogGeometry, LogWriter, Segment,
};
use eggdb_platform::{ErrorKind, File, FileLock, FileSystem, OpenMode};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::Path;

const FREE_LIST_LAYOUT: u8 = 1;
const FREE_LIST_NEXT_AT: usize = PAGE_HEADER_SIZE;
const FREE_LIST_COUNT_AT: usize = PAGE_HEADER_SIZE + 8;
const FREE_LIST_ENTRIES_AT: usize = PAGE_HEADER_SIZE + 16;
const NORMAL_UNWRITTEN_PAGES: usize = 1024;
const NORMAL_UNSYNCED_LOG_BYTES: u64 = 1 << 20;
const NO_STAMP: u64 = 0;

/// How [`PageFile::create`] sets up a new file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CreateOptions {
    /// The page size, fixed for the file's life.
    pub page_size: PageSize,
    /// The file's identity, which seeds every page checksum.
    pub file_id: FileId,
}

impl CreateOptions {
    /// The default page size and a fresh random identity.
    pub fn new() -> Self {
        Self {
            page_size: PageSize::DEFAULT,
            file_id: FileId::random(),
        }
    }
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// A database file seen as numbered, checksummed pages: the two header slots, page reads and
/// writes, growth, a free list and the write-ahead log (`docs/format.md`).
///
/// Changes become durable only at [`PageFile::commit`]. A commit appends a whole image of every
/// page it writes, and a commit record holding the new page count, free list and catalog root,
/// to the write-ahead log inside the file; with [`Durability::Full`] it then syncs the log, and
/// only after that writes the pages in place. Opening the file replays every committed record
/// since the last checkpoint, so a crash at any moment, even one that tears a page being
/// overwritten in place, leaves exactly the commits whose records were durable. The header
/// slots are written only when the log is created; checkpoints, which write them again and
/// let the log be recycled, are item 7 of the v0.2.0 plan.
///
/// A page freed since the last commit is not handed out again until a commit has made the
/// free durable, and writes to free pages and to the log's pages are refused.
///
/// Opening for writing takes an exclusive lock on the file and opening for reading a shared
/// one, so a second writer, in this process or another, is refused with
/// [`StorageError::Locked`]. Where the platform has no file locks (WASI), the file opens
/// unlocked. A failed write or sync stops the file accepting changes until it
/// is opened again (D59).
#[derive(Debug)]
pub struct PageFile<F: File> {
    file: F,
    access: Access,
    header: Superblock,
    superblock: Superblock,
    checksums: PageChecksum,
    page_count: u64,
    file_bytes: u64,
    available: BTreeSet<u64>,
    reused: Vec<u64>,
    freed: BTreeSet<u64>,
    free_list_pages: BTreeSet<u64>,
    staged: BTreeMap<u64, (PageHeader, Vec<u8>)>,
    log: Option<LogWriter>,
    log_pages: BTreeMap<u64, u64>,
    unwritten: BTreeMap<u64, u64>,
    durability: Durability,
    scratch: Vec<u8>,
    page_count_before_commit: Option<u64>,
    damaged_header_slot: Option<usize>,
    failed: bool,
}

impl<F: File> PageFile<F> {
    /// Creates a new database file at `path`, refusing a path that already exists, and makes
    /// the file and its directory entry durable. The log is created by the first change.
    pub fn create<S: FileSystem<File = F>>(
        fs: &S,
        path: &Path,
        options: CreateOptions,
    ) -> Result<Self, StorageError> {
        let file = fs.open(path, OpenMode::CreateNew)?;
        lock(&file, FileLock::Exclusive)?;
        let page_size = options.page_size;
        let superblock = Superblock {
            page_size,
            generation: 0,
            features: Default::default(),
            file_id: options.file_id,
            page_count: page_size.first_data_page(),
            free_list_head: NO_PAGE,
            free_page_count: 0,
            catalog_root: NO_PAGE,
            checkpoint_lsn: 0,
            log_head_page: NO_PAGE,
            log_head_sequence: 0,
            clock_high_water: 0,
            log_segment_pages: log::default_segment_pages(page_size),
            recycled_segments: [NO_PAGE; RECYCLED_SEGMENTS],
        };
        let file_bytes = superblock.page_count * page_size.bytes() as u64;
        file.set_size(file_bytes)?;
        file.write_all_at(&superblock.encode()?, 0)?;
        file.sync_all()?;
        fs.sync_dir(parent_directory(path))?;
        Ok(Self::from_parts(
            file,
            Access::ReadWrite,
            superblock,
            file_bytes,
        ))
    }

    /// Opens the database file at `path` for `access`: chooses its current superblock, checks
    /// its features and length, replays the commits its log holds since the last checkpoint,
    /// and checks its free list.
    ///
    /// A log damaged where it had been made durable refuses the open with
    /// [`FormatError::CorruptLog`]; a log that ends in records a crash cut off opens at its
    /// last whole commit. Opening for writing writes the replayed pages in place, after
    /// syncing the log, so later commits start from them; opening for reading serves them
    /// from the log.
    pub fn open<S: FileSystem<File = F>>(
        fs: &S,
        path: &Path,
        access: Access,
    ) -> Result<Self, StorageError> {
        let (mode, lock_kind) = match access {
            Access::ReadOnly => (OpenMode::Read, FileLock::Shared),
            Access::ReadWrite => (OpenMode::ReadWrite, FileLock::Exclusive),
        };
        let file = fs.open(path, mode)?;
        lock(&file, lock_kind)?;
        let mut slots = vec![0u8; 2 * HEADER_SLOT_SIZE];
        let read = read_up_to(&file, &mut slots)?;
        if read < HEADER_SLOT_SIZE {
            return Err(FormatError::NotADatabase.into());
        }
        let (slot0, slot1) = slots.split_at(HEADER_SLOT_SIZE);
        let choice = Superblock::choose(slot0, slot1)?;
        let superblock = choice.superblock;
        superblock.features.check(access)?;
        let expected_bytes = superblock
            .page_count
            .checked_mul(superblock.page_size.bytes() as u64)
            .ok_or(FormatError::InvalidField {
                field: "page_count",
            })?;
        let actual_bytes = file.size()?;
        if actual_bytes < expected_bytes {
            return Err(StorageError::Truncated {
                expected_bytes,
                actual_bytes,
            });
        }
        let mut page_file = Self::from_parts(file, access, superblock, actual_bytes);
        page_file.damaged_header_slot = choice.damaged_slot;
        if superblock.log_head_page != NO_PAGE {
            page_file.recover()?;
        }
        page_file.superblock.features.check(access)?;
        page_file.load_free_list()?;
        Ok(page_file)
    }

    fn from_parts(file: F, access: Access, superblock: Superblock, file_bytes: u64) -> Self {
        Self {
            file,
            access,
            checksums: PageChecksum::new(superblock.file_id, superblock.page_size),
            page_count: superblock.page_count,
            file_bytes,
            header: superblock,
            superblock,
            available: BTreeSet::new(),
            reused: Vec::new(),
            freed: BTreeSet::new(),
            free_list_pages: BTreeSet::new(),
            staged: BTreeMap::new(),
            log: None,
            log_pages: BTreeMap::new(),
            unwritten: BTreeMap::new(),
            durability: Durability::Full,
            scratch: Vec::new(),
            page_count_before_commit: None,
            damaged_header_slot: None,
            failed: false,
        }
    }

    /// The superblock as of the last commit: the header the file opened with, updated by every
    /// commit since. Its generation and log fields are those of the header in the file.
    pub fn superblock(&self) -> &Superblock {
        &self.superblock
    }

    /// The page size of the file.
    pub fn page_size(&self) -> PageSize {
        self.superblock.page_size
    }

    /// How many pages the file holds now, including pages allocated since the last commit.
    pub fn page_count(&self) -> u64 {
        self.page_count
    }

    /// How many free pages [`PageFile::allocate`] can hand out before the file grows. Pages
    /// freed since the last commit are not counted until the next commit.
    pub fn available_pages(&self) -> u64 {
        self.available.len() as u64
    }

    /// The header slot that failed its checksum when the file was opened, if one did: torn by
    /// a crash during a header update, or damaged since.
    pub fn damaged_header_slot(&self) -> Option<usize> {
        self.damaged_header_slot
    }

    /// How commits wait for the disk; [`Durability::Full`] unless changed.
    pub fn durability(&self) -> Durability {
        self.durability
    }

    /// Sets how later commits wait for the disk. Leaving [`Durability::Normal`] first syncs
    /// the commits it left unsynced, as [`PageFile::sync`] does.
    pub fn set_durability(&mut self, durability: Durability) -> Result<(), StorageError> {
        if self.durability == Durability::Normal && durability != Durability::Normal {
            self.sync()?;
        }
        self.durability = durability;
        Ok(())
    }

    /// Makes every commit so far durable with one sync of the log, and then writes the pages
    /// of commits made under [`Durability::Normal`] in place. Commits made since the last sync
    /// share it: this is group commit for a single writer. Does nothing when everything is
    /// already durable or the file has no log yet.
    pub fn sync(&mut self) -> Result<(), StorageError> {
        self.check_writable()?;
        let synced = self.sync_log();
        self.fail_on_error(synced)
    }

    fn sync_log(&mut self) -> Result<(), StorageError> {
        if let Some(log) = self.log.as_mut() {
            let lsn = log.lsn();
            log.sync_to(&self.file, lsn)?;
        }
        self.drain_unwritten()
    }

    /// Reads data page `page_id` into `page`, which must be one page long, and checks it.
    ///
    /// A page changed through [`PageFile::write_page`] since the last commit reads as changed,
    /// and a committed page whose image is still only in the log is read from there.
    pub fn read_page(&self, page_id: u64, page: &mut [u8]) -> Result<PageHeader, StorageError> {
        self.check_data_page(page_id)?;
        self.check_length(page)?;
        if let Some((_, staged)) = self.staged.get(&page_id) {
            page.copy_from_slice(staged);
        } else if let Some(&offset) = self.unwritten.get(&page_id) {
            self.file.read_exact_at(page, offset)?;
        } else {
            self.file.read_exact_at(page, self.offset(page_id))?;
        }
        Ok(self.checksums.verify(page, page_id)?)
    }

    /// Writes `header` and the checksum into `page`, which must be one page long, and keeps a
    /// copy of it as data page `page_id` until the next commit logs it and writes it in place,
    /// or a rollback drops it. The page must be in use: allocated, or part of the committed
    /// state and not freed. A write to a free page, to a page freed since the last commit, to
    /// a page of the free list or to a page of the log is refused with
    /// [`StorageError::PageNotInUse`].
    pub fn write_page(
        &mut self,
        page_id: u64,
        page: &mut [u8],
        header: &PageHeader,
    ) -> Result<(), StorageError> {
        self.ensure_log()?;
        self.check_page_writable(page_id)?;
        self.checksums.write(page, page_id, header)?;
        self.staged.insert(page_id, (*header, page.to_vec()));
        Ok(())
    }

    /// Hands out a page to write: a free page of the committed state, or a new page at the end
    /// of the file.
    pub fn allocate(&mut self) -> Result<u64, StorageError> {
        self.ensure_log()?;
        if let Some(page) = self.available.pop_first() {
            self.reused.push(page);
            return Ok(page);
        }
        grow(&mut self.page_count)
    }

    /// Gives back a page that is no longer used. It becomes free at the next commit, and is
    /// handed out again only after it.
    pub fn free(&mut self, page_id: u64) -> Result<(), StorageError> {
        self.ensure_log()?;
        self.check_data_page(page_id)?;
        if self.is_log_page(page_id) {
            return Err(StorageError::PageNotInUse { page: page_id });
        }
        if self.available.contains(&page_id)
            || self.freed.contains(&page_id)
            || self.free_list_pages.contains(&page_id)
        {
            return Err(StorageError::DoubleFree { page: page_id });
        }
        self.freed.insert(page_id);
        self.staged.remove(&page_id);
        Ok(())
    }

    /// Drops every change since the last commit: pages written through
    /// [`PageFile::write_page`] are forgotten, and the free pages and the page count are the
    /// committed state's again, so the pages allocated since can be handed out again.
    pub fn rollback(&mut self) -> Result<(), StorageError> {
        self.check_writable()?;
        self.abort_commit();
        self.staged.clear();
        self.available.extend(self.reused.drain(..));
        self.freed.clear();
        self.page_count = self.superblock.page_count;
        Ok(())
    }

    /// Makes every change since the last commit durable: logs an image of each page written
    /// since then and of the new free list, then a commit record with the superblock fields
    /// `update` sets, and, under [`Durability::Full`], syncs the log before writing the pages
    /// in place.
    ///
    /// `update` sets the fields the page file doesn't own: the catalog root, the clock's high
    /// water mark and the feature flags. Whatever it does to the others (page size, identity,
    /// generation, page count, free list and the log's fields) is undone.
    ///
    /// The free list is written to pages that are free in the committed state, or to new pages
    /// at the end, never over the pages of the committed list, and only when it changed. A
    /// commit that can't be made, such as one whose catalog root would be a free page, which
    /// the file could not be opened with again, is refused with [`FormatError::InvalidField`]
    /// before anything is written, and leaves the changes since the last commit in place.
    pub fn commit(&mut self, update: impl FnOnce(&mut Superblock)) -> Result<(), StorageError> {
        let prepared = self.prepare_commit(update)?;
        let mut staged = std::mem::take(&mut self.staged);
        for (&page_id, (header, page)) in staged.iter_mut() {
            if let Err(error) = self.log_page(page_id, page, header) {
                self.abort_commit();
                self.staged = staged;
                return Err(error);
            }
        }
        let finished = self.finish_commit(prepared);
        if finished.is_err() && !self.failed {
            self.staged = staged;
        }
        finished
    }

    pub(crate) fn prepare_commit(
        &mut self,
        update: impl FnOnce(&mut Superblock),
    ) -> Result<PreparedCommit, StorageError> {
        self.ensure_log()?;
        self.check_writable()?;
        let list_changed = !self.reused.is_empty()
            || !self.freed.is_empty()
            || self.page_count != self.superblock.page_count;
        let mut page_count = self.page_count;
        let (entries, list_pages) = if list_changed {
            self.next_free_list(&mut page_count)?
        } else {
            (None, Vec::new())
        };
        let mut next = self.superblock;
        update(&mut next);
        let committed = &self.superblock;
        next.page_size = committed.page_size;
        next.file_id = committed.file_id;
        next.generation = committed.generation;
        next.checkpoint_lsn = committed.checkpoint_lsn;
        next.log_head_page = committed.log_head_page;
        next.log_head_sequence = committed.log_head_sequence;
        next.log_segment_pages = committed.log_segment_pages;
        next.recycled_segments = committed.recycled_segments;
        next.page_count = page_count;
        if let Some(entries) = &entries {
            next.free_list_head = list_pages.first().copied().unwrap_or(NO_PAGE);
            next.free_page_count = (entries.len() + list_pages.len()) as u64;
        }
        next.check()?;
        match &entries {
            Some(entries) => check_not_listed(&next, entries, |pages| {
                list_pages.iter().any(|page| pages.contains(page))
            })?,
            None => check_not_listed(&next, &self.available, |pages| {
                self.free_list_pages.range(pages.clone()).next().is_some()
            })?,
        }
        let log = self.log.as_mut().ok_or(StorageError::NeedsReopen)?;
        let before = self.page_count;
        self.page_count = page_count;
        if let Err(error) = log.begin(&mut self.page_count) {
            log.abort();
            self.page_count = before;
            return Err(error);
        }
        self.page_count_before_commit = Some(before);
        Ok(PreparedCommit {
            superblock: next,
            entries,
            list_pages,
        })
    }

    pub(crate) fn log_page(
        &mut self,
        page_id: u64,
        page: &mut [u8],
        header: &PageHeader,
    ) -> Result<u64, StorageError> {
        self.check_page_in_use(page_id)?;
        self.check_length(page)?;
        let log = self.log.as_mut().ok_or(StorageError::NeedsReopen)?;
        log.reserve(8 + page.len(), &mut self.page_count)?;
        let lsn = log.lsn();
        let header = PageHeader { lsn, ..*header };
        self.checksums.write(page, page_id, &header)?;
        log.put_image(page_id, page);
        Ok(lsn)
    }

    pub(crate) fn abort_commit(&mut self) {
        if let Some(before) = self.page_count_before_commit.take() {
            self.page_count = before;
            if let Some(log) = self.log.as_mut() {
                log.abort();
            }
        }
    }

    pub(crate) fn finish_commit(&mut self, prepared: PreparedCommit) -> Result<(), StorageError> {
        if let Err(error) = self.seal_commit(&prepared) {
            self.abort_commit();
            return Err(error);
        }
        self.page_count_before_commit = None;
        let written = self.write_commit();
        self.fail_on_error(written)?;
        let PreparedCommit {
            mut superblock,
            entries,
            list_pages,
        } = prepared;
        superblock.page_count = self.page_count;
        self.superblock = superblock;
        if let Some(entries) = entries {
            self.available = entries;
            self.free_list_pages = list_pages.into_iter().collect();
        }
        self.reused.clear();
        self.freed.clear();
        if let Some(log) = self.log.as_ref() {
            let segment_pages = u64::from(self.superblock.log_segment_pages);
            for segment in log.new_segments() {
                self.log_pages
                    .insert(segment.first_page, segment.first_page + segment_pages);
            }
        }
        Ok(())
    }

    fn seal_commit(&mut self, prepared: &PreparedCommit) -> Result<(), StorageError> {
        if let Some(entries) = &prepared.entries {
            self.log_free_list(&prepared.list_pages, entries)?;
        }
        let log = self.log.as_mut().ok_or(StorageError::NeedsReopen)?;
        log.reserve(COMMIT_PAYLOAD, &mut self.page_count)?;
        let mut next = prepared.superblock;
        next.page_count = self.page_count;
        log.put_commit(&CommitFields::of(&next, NO_STAMP));
        log.seal(&mut self.page_count)
    }

    fn write_commit(&mut self) -> Result<(), StorageError> {
        let page_bytes = self.page_size().bytes() as u64;
        let wanted = self.page_count * page_bytes;
        let log = self.log.as_mut().ok_or(StorageError::NeedsReopen)?;
        if self.file_bytes < wanted {
            let grown_pages = (wanted - self.file_bytes) / page_bytes;
            let segment_pages =
                log.new_segments().len() as u64 * u64::from(self.superblock.log_segment_pages);
            let unlogged_pages = grown_pages.saturating_sub(segment_pages);
            self.file.set_size(wanted)?;
            self.file_bytes = wanted;
            if self.durability != Durability::Full && unlogged_pages > log.images().len() as u64 {
                self.file.sync_data()?;
            }
        }
        log.write(&self.file)?;
        match self.durability {
            Durability::Full => {
                let lsn = log.lsn();
                log.sync_to(&self.file, lsn)?;
                self.drain_unwritten()?;
                self.write_logged_pages_in_place()
            }
            Durability::Off => self.write_logged_pages_in_place(),
            Durability::Normal => {
                let waiting = self.unwritten.len() + log.images().len();
                if waiting > NORMAL_UNWRITTEN_PAGES
                    || log.unsynced_bytes() >= NORMAL_UNSYNCED_LOG_BYTES
                {
                    let lsn = log.lsn();
                    log.sync_to(&self.file, lsn)?;
                    for image in log.images() {
                        self.unwritten.insert(image.page_id, image.file_offset);
                    }
                    return self.drain_unwritten();
                }
                for image in log.images() {
                    self.unwritten.insert(image.page_id, image.file_offset);
                }
                Ok(())
            }
        }
    }

    fn write_logged_pages_in_place(&self) -> Result<(), StorageError> {
        let Some(log) = self.log.as_ref() else {
            return Ok(());
        };
        let page_bytes = self.page_size().bytes();
        for image in log.images() {
            let bytes = &log.buffer()[image.buffer_start..image.buffer_start + page_bytes];
            self.file.write_all_at(bytes, self.offset(image.page_id))?;
        }
        Ok(())
    }

    fn drain_unwritten(&mut self) -> Result<(), StorageError> {
        if self.unwritten.is_empty() {
            return Ok(());
        }
        let mut page = std::mem::take(&mut self.scratch);
        page.resize(self.page_size().bytes(), 0);
        for (&page_id, &offset) in &self.unwritten {
            self.file.read_exact_at(&mut page, offset)?;
            self.file.write_all_at(&page, self.offset(page_id))?;
        }
        self.unwritten.clear();
        self.scratch = page;
        Ok(())
    }

    fn next_free_list(
        &self,
        page_count: &mut u64,
    ) -> Result<(Option<BTreeSet<u64>>, Vec<u64>), StorageError> {
        let capacity = self.free_list_capacity();
        let mut entries: BTreeSet<u64> = self.available.clone();
        entries.extend(self.freed.iter().copied());
        entries.extend(self.free_list_pages.iter().copied());
        let mut list_pages = Vec::new();
        let mut reusable = self.available.clone();
        while list_pages.len() * capacity < entries.len() {
            let page = match reusable.pop_first() {
                Some(page) => {
                    entries.remove(&page);
                    page
                }
                None => grow(page_count)?,
            };
            list_pages.push(page);
        }
        Ok((Some(entries), list_pages))
    }

    fn log_free_list(
        &mut self,
        list_pages: &[u64],
        entries: &BTreeSet<u64>,
    ) -> Result<(), StorageError> {
        let capacity = self.free_list_capacity();
        let mut remaining = entries.iter().copied();
        let mut page = std::mem::take(&mut self.scratch);
        page.resize(self.page_size().bytes(), 0);
        let mut logged = Ok(());
        for (index, &page_id) in list_pages.iter().enumerate() {
            page.fill(0);
            let next = list_pages.get(index + 1).copied().unwrap_or(NO_PAGE);
            write_u64(&mut page, FREE_LIST_NEXT_AT, next);
            let mut count = 0;
            for entry in remaining.by_ref().take(capacity) {
                write_u64(&mut page, FREE_LIST_ENTRIES_AT + 8 * count, entry);
                count += 1;
            }
            write_u32(&mut page, FREE_LIST_COUNT_AT, count as u32);
            let header = PageHeader {
                page_type: PageType::FreeList,
                layout: FREE_LIST_LAYOUT,
                flags: 0,
                lsn: 0,
            };
            logged = self.log_list_page(page_id, &mut page, &header);
            if logged.is_err() {
                break;
            }
        }
        self.scratch = page;
        logged
    }

    fn log_list_page(
        &mut self,
        page_id: u64,
        page: &mut [u8],
        header: &PageHeader,
    ) -> Result<(), StorageError> {
        let log = self.log.as_mut().ok_or(StorageError::NeedsReopen)?;
        log.reserve(8 + page.len(), &mut self.page_count)?;
        let header = PageHeader {
            lsn: log.lsn(),
            ..*header
        };
        self.checksums.write(page, page_id, &header)?;
        log.put_image(page_id, page);
        Ok(())
    }

    fn geometry(&self, segment_pages: u32) -> LogGeometry {
        LogGeometry::new(self.header.file_id, self.header.page_size, segment_pages)
    }

    fn ensure_log(&mut self) -> Result<(), StorageError> {
        self.check_writable()?;
        if self.log.is_some() {
            return Ok(());
        }
        let page_size = self.page_size();
        let segment_pages = if log::segment_holds_a_page(page_size, self.header.log_segment_pages) {
            self.header.log_segment_pages
        } else {
            log::default_segment_pages(page_size)
        };
        let first_page = self.superblock.page_count;
        let page_count = first_page
            .checked_add(u64::from(segment_pages))
            .filter(|&count| count & PAGE_VOLUME_MASK == 0)
            .ok_or(StorageError::FileFull)?;
        let segment = Segment {
            first_page,
            sequence: self.header.log_head_sequence.checked_add(1).ok_or(
                FormatError::InvalidField {
                    field: "log_head_sequence",
                },
            )?,
            start_lsn: self.header.checkpoint_lsn,
        };
        let mut next = self.superblock;
        next.generation =
            self.header
                .generation
                .checked_add(1)
                .ok_or(FormatError::InvalidField {
                    field: "generation",
                })?;
        next.page_count = page_count;
        next.log_head_page = first_page;
        next.log_head_sequence = segment.sequence;
        next.checkpoint_lsn = segment.start_lsn;
        next.log_segment_pages = segment_pages;
        let slot = next.encode()?;
        let geometry = self.geometry(segment_pages);
        let mut block = vec![0u8; LOG_BLOCK];
        geometry.encode_segment_header(&segment, &mut block);
        let created = self.write_new_log(&next, &slot, &block);
        self.fail_on_error(created)?;
        self.header = next;
        self.superblock = next;
        self.page_count = page_count;
        self.log_pages.insert(first_page, page_count);
        self.log = Some(LogWriter::new(
            geometry,
            segment,
            LOG_BLOCK,
            false,
            segment.start_lsn,
        ));
        Ok(())
    }

    fn write_new_log(
        &mut self,
        next: &Superblock,
        slot: &[u8],
        block: &[u8],
    ) -> Result<(), StorageError> {
        let page_bytes = self.page_size().bytes() as u64;
        let segment_start = next.log_head_page * page_bytes;
        if self.file_bytes > segment_start {
            self.file.set_size(segment_start)?;
        }
        self.file_bytes = next.page_count * page_bytes;
        self.file.set_size(self.file_bytes)?;
        self.file.write_all_at(block, segment_start)?;
        self.file.sync_data()?;
        self.file
            .write_all_at(slot, (next.slot() * HEADER_SLOT_SIZE) as u64)?;
        self.file.sync_data()?;
        Ok(())
    }

    fn recover(&mut self) -> Result<(), StorageError> {
        let geometry = self.geometry(self.header.log_segment_pages);
        let scan = log::scan(&self.file, &geometry, &self.checksums, &self.header)?;
        let damaged_commit = || -> StorageError {
            match scan.commit_at {
                Some((segment, offset)) => log::corrupt(&segment, offset),
                None => FormatError::InvalidField {
                    field: "page_count",
                }
                .into(),
            }
        };
        let page_bytes = self.page_size().bytes() as u64;
        let mut state = self.header;
        if let Some(commit) = scan.commit {
            let proven_pages =
                (self.file_bytes / page_bytes).saturating_add(scan.images.len() as u64);
            if commit.page_count < self.header.page_count || commit.page_count > proven_pages {
                return Err(damaged_commit());
            }
            commit.apply(&mut state);
        }
        state.page_count = state.page_count.max(scan.linked_page_count);
        state.check().map_err(|_| damaged_commit())?;
        state.features.check(self.access)?;
        let segment_pages = u64::from(geometry.segment_pages());
        for segment in &scan.segments {
            self.log_pages
                .insert(segment.first_page, segment.first_page + segment_pages);
        }
        for image in &scan.images {
            if image.page_id >= state.page_count || self.is_log_page(image.page_id) {
                return Err(self.corrupt_log_at(image.file_offset));
            }
            self.unwritten.insert(image.page_id, image.file_offset);
        }
        self.superblock = state;
        self.page_count = state.page_count;
        if self.access == Access::ReadOnly {
            return Ok(());
        }
        let wanted = state
            .page_count
            .checked_mul(page_bytes)
            .ok_or_else(damaged_commit)?;
        if self.file_bytes != wanted {
            self.file.set_size(wanted)?;
            self.file_bytes = wanted;
        }
        self.file.sync_data()?;
        self.drain_unwritten()?;
        self.log = Some(LogWriter::new(
            geometry,
            scan.tail,
            scan.resume_offset,
            scan.needs_fresh_segment,
            scan.end_lsn,
        ));
        Ok(())
    }

    fn corrupt_log_at(&self, file_offset: u64) -> StorageError {
        let page_bytes = self.page_size().bytes() as u64;
        let page = file_offset / page_bytes;
        let segment = self
            .log_pages
            .range(..=page)
            .next_back()
            .map_or(page, |(&first, _)| first);
        FormatError::CorruptLog {
            segment,
            offset: file_offset - segment * page_bytes,
        }
        .into()
    }

    fn load_free_list(&mut self) -> Result<(), StorageError> {
        let capacity = self.free_list_capacity();
        let total = self.superblock.free_page_count;
        let mut entries = BTreeSet::new();
        let mut list_pages = BTreeSet::new();
        let mut next = self.superblock.free_list_head;
        let mut page = vec![0u8; self.page_size().bytes()];
        let listed = |entries: &BTreeSet<u64>, list_pages: &BTreeSet<u64>| {
            (entries.len() + list_pages.len()) as u64
        };
        while next != NO_PAGE {
            if self.check_data_page(next).is_err()
                || entries.contains(&next)
                || !list_pages.insert(next)
                || self.is_in_use(next)
            {
                return Err(corrupt_free_list(next));
            }
            let header = self.read_page(next, &mut page)?;
            if header.page_type != PageType::FreeList || header.layout != FREE_LIST_LAYOUT {
                return Err(corrupt_free_list(next));
            }
            let count = read_u32(&page, FREE_LIST_COUNT_AT) as usize;
            if count > capacity || listed(&entries, &list_pages) + count as u64 > total {
                return Err(corrupt_free_list(next));
            }
            for index in 0..count {
                let entry = read_u64(&page, FREE_LIST_ENTRIES_AT + 8 * index);
                if self.check_data_page(entry).is_err()
                    || self.is_in_use(entry)
                    || list_pages.contains(&entry)
                    || !entries.insert(entry)
                {
                    return Err(corrupt_free_list(next));
                }
            }
            next = read_u64(&page, FREE_LIST_NEXT_AT);
        }
        if listed(&entries, &list_pages) != total {
            return Err(corrupt_free_list(self.superblock.free_list_head));
        }
        self.available = entries;
        self.free_list_pages = list_pages;
        Ok(())
    }

    fn is_in_use(&self, page_id: u64) -> bool {
        self.is_log_page(page_id)
            || pages_in_use_by(&self.superblock).any(|(_, pages)| pages.contains(&page_id))
    }

    fn is_log_page(&self, page_id: u64) -> bool {
        self.log_pages
            .range(..=page_id)
            .next_back()
            .is_some_and(|(_, &end)| page_id < end)
    }

    fn free_list_capacity(&self) -> usize {
        (self.page_size().bytes() - FREE_LIST_ENTRIES_AT) / 8
    }

    fn offset(&self, page_id: u64) -> u64 {
        page_id * self.page_size().bytes() as u64
    }

    pub(crate) fn check_page_writable(&self, page_id: u64) -> Result<(), StorageError> {
        self.check_writable()?;
        self.check_page_in_use(page_id)
    }

    pub(crate) fn check_page_in_use(&self, page_id: u64) -> Result<(), StorageError> {
        self.check_data_page(page_id)?;
        if self.available.contains(&page_id)
            || self.freed.contains(&page_id)
            || self.free_list_pages.contains(&page_id)
            || self.is_log_page(page_id)
        {
            return Err(StorageError::PageNotInUse { page: page_id });
        }
        Ok(())
    }

    fn check_data_page(&self, page_id: u64) -> Result<(), StorageError> {
        if page_id >= self.page_size().first_data_page() && page_id < self.page_count {
            Ok(())
        } else {
            Err(StorageError::PageOutOfRange {
                page: page_id,
                page_count: self.page_count,
            })
        }
    }

    fn check_length(&self, page: &[u8]) -> Result<(), StorageError> {
        if page.len() == self.page_size().bytes() {
            Ok(())
        } else {
            Err(FormatError::WrongLength {
                expected: self.page_size().bytes(),
                actual: page.len(),
            }
            .into())
        }
    }

    pub(crate) fn check_writable(&self) -> Result<(), StorageError> {
        if self.access == Access::ReadOnly {
            return Err(StorageError::ReadOnly);
        }
        if self.failed {
            return Err(StorageError::NeedsReopen);
        }
        Ok(())
    }

    fn fail_on_error<T>(&mut self, result: Result<T, StorageError>) -> Result<T, StorageError> {
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}

fn lock(file: &impl File, kind: FileLock) -> Result<(), StorageError> {
    match file.try_lock(kind) {
        Err(error) if cfg!(target_os = "wasi") && error.kind() == ErrorKind::Unsupported => Ok(()),
        locked => Ok(locked?),
    }
}

#[derive(Debug)]
pub(crate) struct PreparedCommit {
    superblock: Superblock,
    entries: Option<BTreeSet<u64>>,
    list_pages: Vec<u64>,
}

fn grow(page_count: &mut u64) -> Result<u64, StorageError> {
    let page = *page_count;
    if (page + 1) & PAGE_VOLUME_MASK != 0 {
        return Err(StorageError::FileFull);
    }
    *page_count += 1;
    Ok(page)
}

fn pages_in_use_by(
    superblock: &Superblock,
) -> impl Iterator<Item = (&'static str, Range<u64>)> + '_ {
    let segment = |first: u64| first..first + u64::from(superblock.log_segment_pages);
    let catalog_root = superblock.catalog_root;
    let catalog =
        (catalog_root != NO_PAGE).then(|| ("catalog_root", catalog_root..catalog_root + 1));
    let log_head = (superblock.log_head_page != NO_PAGE)
        .then(|| ("log_head_page", segment(superblock.log_head_page)));
    let recycled = superblock
        .recycled_segments
        .iter()
        .filter(|&&first| first != NO_PAGE)
        .map(move |&first| ("recycled_segments", segment(first)));
    catalog.into_iter().chain(log_head).chain(recycled)
}

fn check_not_listed(
    superblock: &Superblock,
    entries: &BTreeSet<u64>,
    lists_a_page_of: impl Fn(&Range<u64>) -> bool,
) -> Result<(), FormatError> {
    for (field, pages) in pages_in_use_by(superblock) {
        let listed = entries.range(pages.clone()).next().is_some() || lists_a_page_of(&pages);
        if listed {
            return Err(FormatError::InvalidField { field });
        }
    }
    Ok(())
}

fn corrupt_free_list(page: u64) -> StorageError {
    StorageError::CorruptFreeList { page }
}

fn read_up_to(file: &impl File, buf: &mut [u8]) -> Result<usize, StorageError> {
    let mut filled = 0;
    while filled < buf.len() {
        let read = file.read_at(&mut buf[filled..], filled as u64)?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

fn parent_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

#[cfg(test)]
mod tests {
    use super::{CreateOptions, PageFile};
    use crate::error::{FormatError, StorageError};
    use crate::format::{
        Access, FileId, HEADER_SLOT_SIZE, NO_PAGE, PageChecksum, PageHeader, PageSize, PageType,
    };
    use crate::test_rng::TestRng;
    use eggdb_platform::{CrashMode, File, FileSystem, OpenMode, SimFile, SimFileSystem};
    use std::collections::BTreeSet;
    use std::path::Path;

    const DB: &str = "db";

    fn options() -> CreateOptions {
        CreateOptions {
            page_size: PageSize::DEFAULT,
            file_id: FileId(*b"page-file-tests!"),
        }
    }

    fn leaf(lsn: u64) -> PageHeader {
        PageHeader {
            page_type: PageType::BTreeLeaf,
            layout: 1,
            flags: 0,
            lsn,
        }
    }

    fn create(fs: &SimFileSystem) -> PageFile<SimFile> {
        PageFile::create(fs, Path::new(DB), options()).unwrap()
    }

    fn open(fs: &SimFileSystem, access: Access) -> Result<PageFile<SimFile>, StorageError> {
        PageFile::open(fs, Path::new(DB), access)
    }

    fn write_filled(file: &mut PageFile<SimFile>, page_id: u64, fill: u8) {
        let mut page = vec![fill; file.page_size().bytes()];
        file.write_page(page_id, &mut page, &leaf(page_id)).unwrap();
    }

    fn read_fill(file: &PageFile<SimFile>, page_id: u64) -> Result<u8, StorageError> {
        let mut page = vec![0; file.page_size().bytes()];
        let header = file.read_page(page_id, &mut page)?;
        assert_eq!(header.page_type, PageType::BTreeLeaf);
        Ok(page[100])
    }

    fn allocate_all(file: &mut PageFile<SimFile>) -> BTreeSet<u64> {
        (0..file.available_pages())
            .map(|_| file.allocate().unwrap())
            .collect()
    }

    #[test]
    fn written_pages_survive_a_commit_and_a_reopen() {
        let fs = SimFileSystem::new();
        let mut file = create(&fs);
        let pages: Vec<u64> = (0..3).map(|_| file.allocate().unwrap()).collect();
        assert_eq!(pages, [18, 19, 20]);
        for &page in &pages {
            write_filled(&mut file, page, page as u8);
        }
        file.commit(|superblock| superblock.catalog_root = 19)
            .unwrap();
        drop(file);
        let file = open(&fs, Access::ReadWrite).unwrap();
        assert_eq!(file.superblock().generation, 1);
        assert_eq!(file.superblock().log_head_page, 2);
        assert_eq!(file.superblock().page_count, 21);
        assert_eq!(file.superblock().catalog_root, 19);
        for &page in &pages {
            assert_eq!(read_fill(&file, page).unwrap(), page as u8);
        }
        assert_eq!(file.damaged_header_slot(), None);
    }

    #[test]
    fn one_writer_or_many_readers_hold_the_file() {
        let fs = SimFileSystem::new();
        let writer = create(&fs);
        assert!(matches!(
            open(&fs, Access::ReadWrite),
            Err(StorageError::Locked)
        ));
        assert!(matches!(
            open(&fs, Access::ReadOnly),
            Err(StorageError::Locked)
        ));
        drop(writer);
        let readers = [open(&fs, Access::ReadOnly), open(&fs, Access::ReadOnly)];
        assert!(readers.iter().all(Result::is_ok));
        assert!(matches!(
            open(&fs, Access::ReadWrite),
            Err(StorageError::Locked)
        ));
        drop(readers);
        open(&fs, Access::ReadWrite).unwrap();
    }

    #[test]
    fn creating_over_an_existing_file_is_refused() {
        let fs = SimFileSystem::new();
        drop(create(&fs));
        let error = PageFile::create(&fs, Path::new(DB), options()).unwrap_err();
        assert!(
            matches!(error, StorageError::Io(error) if error.kind() == eggdb_platform::ErrorKind::AlreadyExists)
        );
    }

    #[test]
    fn a_freed_page_is_handed_out_again_only_after_a_commit() {
        let fs = SimFileSystem::new();
        let mut file = create(&fs);
        let (a, b) = (file.allocate().unwrap(), file.allocate().unwrap());
        write_filled(&mut file, a, 1);
        write_filled(&mut file, b, 2);
        file.commit(|_| {}).unwrap();
        file.free(a).unwrap();
        assert_ne!(file.allocate().unwrap(), a);
        file.commit(|_| {}).unwrap();
        assert_eq!(allocate_all(&mut file), BTreeSet::from([a]));
    }

    #[test]
    fn writing_a_free_or_freed_page_is_refused() {
        let fs = SimFileSystem::new();
        let mut file = create(&fs);
        let (a, b) = (file.allocate().unwrap(), file.allocate().unwrap());
        write_filled(&mut file, a, 1);
        write_filled(&mut file, b, 1);
        file.commit(|_| {}).unwrap();
        file.free(a).unwrap();
        let mut page = vec![0; 4096];
        assert!(matches!(
            file.write_page(a, &mut page, &leaf(a)),
            Err(StorageError::PageNotInUse { page }) if page == a
        ));
        file.commit(|_| {}).unwrap();
        assert!(matches!(
            file.write_page(a, &mut page, &leaf(a)),
            Err(StorageError::PageNotInUse { .. })
        ));
        let list_page = *file.free_list_pages.first().unwrap();
        assert!(matches!(
            file.write_page(list_page, &mut page, &leaf(list_page)),
            Err(StorageError::PageNotInUse { .. })
        ));
        assert_eq!(file.allocate().unwrap(), a);
        write_filled(&mut file, a, 2);
    }

    #[test]
    fn freeing_twice_or_outside_the_data_pages_is_refused() {
        let fs = SimFileSystem::new();
        let mut file = create(&fs);
        let page = file.allocate().unwrap();
        file.free(page).unwrap();
        assert!(matches!(file.free(page), Err(StorageError::DoubleFree { page: p }) if p == page));
        assert!(matches!(
            file.free(1),
            Err(StorageError::PageOutOfRange { page: 1, .. })
        ));
        assert!(matches!(
            file.free(99),
            Err(StorageError::PageOutOfRange { page: 99, .. })
        ));
        let mut buffer = vec![0; 4096];
        assert!(matches!(
            file.read_page(0, &mut buffer),
            Err(StorageError::PageOutOfRange { page: 0, .. })
        ));
    }

    #[test]
    fn a_free_list_spanning_many_pages_survives_a_reopen() {
        let fs = SimFileSystem::new();
        let mut file = create(&fs);
        let pages: Vec<u64> = (0..1500).map(|_| file.allocate().unwrap()).collect();
        for &page in &pages {
            write_filled(&mut file, page, 7);
        }
        file.commit(|_| {}).unwrap();
        for &page in pages.iter().step_by(2) {
            file.free(page).unwrap();
        }
        file.commit(|_| {}).unwrap();
        let free_page_count = file.superblock().free_page_count;
        assert_eq!(free_page_count, 750 + file.free_list_pages.len() as u64);
        assert!(file.free_list_pages.len() > 1);
        drop(file);
        let mut file = open(&fs, Access::ReadWrite).unwrap();
        assert_eq!(file.superblock().free_page_count, free_page_count);
        let reused = allocate_all(&mut file);
        assert_eq!(reused.len(), 750);
        assert!(reused.iter().all(|page| page % 2 == 0));
        for page in pages.iter().skip(1).step_by(2) {
            assert_eq!(read_fill(&file, *page).unwrap(), 7);
        }
        file.commit(|_| {}).unwrap();
    }

    #[derive(Debug, PartialEq)]
    struct Committed {
        generation: u64,
        page_count: u64,
        free: BTreeSet<u64>,
    }

    fn committed(mut file: PageFile<SimFile>) -> Committed {
        let generation = file.superblock().generation;
        let page_count = file.superblock().page_count;
        let mut free = allocate_all(&mut file);
        free.extend(file.free_list_pages.iter().copied());
        Committed {
            generation,
            page_count,
            free,
        }
    }

    fn first_commit(fs: &SimFileSystem) -> (PageFile<SimFile>, Vec<u64>) {
        let mut file = create(fs);
        let pages: Vec<u64> = (0..1400).map(|_| file.allocate().unwrap()).collect();
        for &page in &pages {
            write_filled(&mut file, page, 1);
        }
        file.commit(|_| {}).unwrap();
        for &page in &pages[..600] {
            file.free(page).unwrap();
        }
        file.commit(|_| {}).unwrap();
        assert!(file.available_pages() > 500 && file.free_list_pages.len() > 1);
        (file, pages)
    }

    fn second_commit(fs: &SimFileSystem, fail_at: Option<u64>) -> (Vec<u64>, u64) {
        let (mut file, pages) = first_commit(fs);
        for &page in &pages[600..1200] {
            file.free(page).unwrap();
        }
        let start = fs.operation_count();
        if let Some(offset) = fail_at {
            fs.fail_operation(start + offset);
        }
        let mut write_and_commit = || {
            for _ in 0..3 {
                let page = file.allocate()?;
                let mut buffer = vec![2; 4096];
                file.write_page(page, &mut buffer, &leaf(page))?;
            }
            file.commit(|_| {})
        };
        let committed = write_and_commit();
        assert_eq!(committed.is_ok(), fail_at.is_none());
        (pages, fs.operation_count() - start)
    }

    #[test]
    fn a_crash_at_any_operation_of_a_commit_leaves_the_old_or_the_new_state() {
        let old = {
            let fs = SimFileSystem::new();
            drop(first_commit(&fs));
            committed(open(&fs, Access::ReadWrite).unwrap())
        };
        let (new, operations) = {
            let fs = SimFileSystem::new();
            let operations = second_commit(&fs, None).1;
            (committed(open(&fs, Access::ReadWrite).unwrap()), operations)
        };
        assert_ne!(old, new);
        let mut outcomes = BTreeSet::new();
        for step in 0..operations {
            for seed in 0..4 {
                let fs = SimFileSystem::new();
                let (pages, _) = second_commit(&fs, Some(step));
                fs.crash(CrashMode::Torn { seed });
                let file = open(&fs, Access::ReadWrite).unwrap();
                for &page in &pages[600..] {
                    assert_eq!(read_fill(&file, page).unwrap(), 1, "step {step}");
                }
                let state = committed(file);
                assert!(state == old || state == new, "step {step}: {state:?}");
                outcomes.insert(state == new);
            }
        }
        assert_eq!(outcomes, BTreeSet::from([false, true]));
    }

    #[test]
    fn a_crafted_generation_at_its_limit_refuses_a_commit_before_any_write() {
        let fs = SimFileSystem::new();
        let mut file = create(&fs);
        file.header.generation = u64::MAX;
        let before = fs.operation_count();
        for _ in 0..2 {
            assert!(matches!(
                file.commit(|_| {}),
                Err(StorageError::Format(FormatError::InvalidField {
                    field: "generation"
                }))
            ));
        }
        assert!(file.allocate().is_err());
        assert_eq!(fs.operation_count(), before);
    }

    #[test]
    fn an_invalid_update_writes_nothing() {
        let fs = SimFileSystem::new();
        let mut file = create(&fs);
        let page = file.allocate().unwrap();
        write_filled(&mut file, page, 1);
        file.commit(|_| {}).unwrap();
        file.free(page).unwrap();
        let before = fs.operation_count();
        assert!(
            file.commit(|superblock| superblock.catalog_root = 999)
                .is_err()
        );
        assert_eq!(fs.operation_count(), before);
    }

    #[test]
    fn a_commit_that_would_list_the_catalog_root_as_free_is_refused_before_any_write() {
        let fs = SimFileSystem::new();
        let mut file = create(&fs);
        let root = file.allocate().unwrap();
        write_filled(&mut file, root, 4);
        file.commit(|superblock| superblock.catalog_root = root)
            .unwrap();
        file.free(root).unwrap();
        let before = fs.operation_count();
        assert!(matches!(
            file.commit(|_| {}),
            Err(StorageError::Format(FormatError::InvalidField {
                field: "catalog_root"
            }))
        ));
        assert_eq!(fs.operation_count(), before);
        file.rollback().unwrap();
        file.commit(|_| {}).unwrap();
        drop(file);
        let file = open(&fs, Access::ReadWrite).unwrap();
        assert_eq!(file.superblock().catalog_root, root);
        assert_eq!(read_fill(&file, root).unwrap(), 4);
    }

    #[test]
    fn the_log_s_pages_can_not_be_freed_written_or_moved_by_a_commit() {
        let fs = SimFileSystem::new();
        let mut file = create(&fs);
        file.allocate().unwrap();
        let log_head = file.superblock().log_head_page;
        assert!(matches!(
            file.free(log_head + 3),
            Err(StorageError::PageNotInUse { .. })
        ));
        let mut page = vec![0; 4096];
        assert!(matches!(
            file.write_page(log_head, &mut page, &leaf(log_head)),
            Err(StorageError::PageNotInUse { .. })
        ));
        file.commit(|superblock| {
            superblock.log_head_page = NO_PAGE;
            superblock.checkpoint_lsn = 77;
        })
        .unwrap();
        assert_eq!(file.superblock().log_head_page, log_head);
        assert_eq!(file.superblock().checkpoint_lsn, 0);
    }

    #[test]
    fn a_rollback_restores_the_committed_free_pages_and_page_count() {
        let fs = SimFileSystem::new();
        let mut file = create(&fs);
        let pages: Vec<u64> = (0..4).map(|_| file.allocate().unwrap()).collect();
        for &page in &pages {
            write_filled(&mut file, page, 1);
        }
        file.commit(|_| {}).unwrap();
        file.free(pages[0]).unwrap();
        file.commit(|_| {}).unwrap();
        let committed_count = file.page_count();
        let committed_free = file.available_pages();
        let reused = file.allocate().unwrap();
        let grown = file.allocate().unwrap();
        file.free(pages[1]).unwrap();
        write_filled(&mut file, grown, 2);
        file.rollback().unwrap();
        assert_eq!(file.page_count(), committed_count);
        assert_eq!(file.available_pages(), committed_free);
        assert_eq!(file.allocate().unwrap(), reused);
        assert_eq!(file.allocate().unwrap(), grown);
        file.free(pages[1]).unwrap();
        file.commit(|_| {}).unwrap();
        drop(file);
        let file = open(&fs, Access::ReadOnly).unwrap();
        assert_eq!(file.superblock().page_count, committed_count + 2);
    }

    #[test]
    fn a_read_only_open_refuses_changes_and_unknown_read_only_features_refuse_writers() {
        let fs = SimFileSystem::new();
        let mut file = create(&fs);
        file.commit(|superblock| superblock.features.read_only_compatible = 1 << 5)
            .unwrap();
        drop(file);
        assert!(matches!(
            open(&fs, Access::ReadWrite),
            Err(StorageError::Format(FormatError::ReadOnlyFeatures {
                bits: 32
            }))
        ));
        let mut reader = open(&fs, Access::ReadOnly).unwrap();
        assert!(matches!(reader.allocate(), Err(StorageError::ReadOnly)));
        assert!(matches!(reader.commit(|_| {}), Err(StorageError::ReadOnly)));
    }

    #[test]
    fn a_file_shorter_than_its_header_says_is_refused() {
        let fs = SimFileSystem::new();
        let mut file = create(&fs);
        let page = file.allocate().unwrap();
        write_filled(&mut file, page, 3);
        file.commit(|_| {}).unwrap();
        drop(file);
        let raw = fs.open(Path::new(DB), OpenMode::ReadWrite).unwrap();
        raw.set_size(4096 * 2 + 100).unwrap();
        drop(raw);
        assert!(matches!(
            open(&fs, Access::ReadOnly),
            Err(StorageError::Truncated {
                expected_bytes: 73728,
                actual_bytes: 8292
            })
        ));
        let raw = fs.open(Path::new(DB), OpenMode::ReadWrite).unwrap();
        raw.set_size(100).unwrap();
        drop(raw);
        assert!(matches!(
            open(&fs, Access::ReadOnly),
            Err(StorageError::Format(FormatError::NotADatabase))
        ));
    }

    #[test]
    fn a_failed_sync_stops_every_later_change() {
        let fs = SimFileSystem::new();
        let mut file = create(&fs);
        let page = file.allocate().unwrap();
        write_filled(&mut file, page, 3);
        fs.fail_randomly(1, 1_000_000);
        assert!(file.commit(|_| {}).is_err());
        fs.fail_randomly(1, 0);
        assert!(matches!(file.allocate(), Err(StorageError::NeedsReopen)));
        assert!(matches!(
            file.commit(|_| {}),
            Err(StorageError::NeedsReopen)
        ));
    }

    #[test]
    fn a_torn_header_update_is_reported_after_opening_the_older_state() {
        let fs = SimFileSystem::new();
        let mut file = create(&fs);
        file.commit(|_| {}).unwrap();
        drop(file);
        let raw = fs.open(Path::new(DB), OpenMode::ReadWrite).unwrap();
        raw.write_all_at(&[0xFF; 8], HEADER_SLOT_SIZE as u64 + 100)
            .unwrap();
        drop(raw);
        let file = open(&fs, Access::ReadOnly).unwrap();
        assert_eq!(file.superblock().generation, 0);
        assert_eq!(file.superblock().log_head_page, NO_PAGE);
        assert_eq!(file.damaged_header_slot(), Some(1));
    }

    #[test]
    fn large_pages_put_both_header_slots_in_page_zero() {
        let fs = SimFileSystem::new();
        let options = CreateOptions {
            page_size: PageSize::from_log2(16).unwrap(),
            ..options()
        };
        let mut file = PageFile::create(&fs, Path::new(DB), options).unwrap();
        let page = file.allocate().unwrap();
        assert_eq!((file.superblock().log_head_page, page), (1, 3));
        write_filled(&mut file, page, 9);
        file.commit(|_| {}).unwrap();
        drop(file);
        let file = open(&fs, Access::ReadOnly).unwrap();
        assert_eq!(file.page_size().bytes(), 65536);
        assert_eq!(read_fill(&file, 3).unwrap(), 9);
        assert_eq!(file.superblock().log_segment_pages, 2);
    }

    fn without_log(fs: &SimFileSystem) {
        let file = open(fs, Access::ReadWrite).unwrap();
        let mut legacy = *file.superblock();
        legacy.generation = file.header.generation + 1;
        legacy.log_head_page = NO_PAGE;
        legacy.log_head_sequence = 0;
        legacy.checkpoint_lsn = 0;
        let slot = legacy.encode().unwrap();
        file.file.sync_data().unwrap();
        file.file
            .write_all_at(&slot, (legacy.slot() * HEADER_SLOT_SIZE) as u64)
            .unwrap();
        file.file.sync_data().unwrap();
    }

    fn rewrite_checksum(raw: &SimFile, page_id: u64) {
        let checksums = PageChecksum::new(options().file_id, PageSize::DEFAULT);
        let mut page = vec![0u8; 4096];
        raw.read_exact_at(&mut page, page_id * 4096).unwrap();
        let header = PageHeader {
            page_type: PageType::FreeList,
            layout: 1,
            flags: 0,
            lsn: 0,
        };
        checksums.write(&mut page, page_id, &header).unwrap();
        raw.write_all_at(&page, page_id * 4096).unwrap();
    }

    #[test]
    fn hostile_files_never_panic_and_free_lists_never_hand_out_a_page_twice() {
        let pristine = SimFileSystem::new();
        let (list_pages, page_count) = {
            let mut file = create(&pristine);
            let pages: Vec<u64> = (0..1200).map(|_| file.allocate().unwrap()).collect();
            file.commit(|_| {}).unwrap();
            for &page in &pages {
                file.free(page).unwrap();
            }
            file.commit(|_| {}).unwrap();
            (
                file.free_list_pages.iter().copied().collect::<Vec<u64>>(),
                file.page_count(),
            )
        };
        without_log(&pristine);
        let image = {
            let raw = pristine.open(Path::new(DB), OpenMode::Read).unwrap();
            let mut bytes = vec![0u8; raw.size().unwrap() as usize];
            raw.read_exact_at(&mut bytes, 0).unwrap();
            bytes
        };
        let mut rng = TestRng::new(0xBAD_F11E);
        let mut refused_lists = 0;
        let fs = SimFileSystem::new();
        let raw = fs.open(Path::new(DB), OpenMode::CreateNew).unwrap();
        raw.write_all_at(&image, 0).unwrap();
        raw.sync_all().unwrap();
        for case in 0..600 {
            let target = if case % 2 == 0 {
                list_pages[rng.below(list_pages.len())]
            } else {
                rng.below(page_count as usize) as u64
            };
            for _ in 0..1 + rng.below(4) {
                let at = target * 4096 + 16 + rng.below(4080) as u64;
                raw.write_all_at(&[rng.next_u64() as u8], at).unwrap();
            }
            if case % 2 == 0 && target >= 2 {
                rewrite_checksum(&raw, target);
            }
            match open(&fs, Access::ReadWrite) {
                Err(StorageError::CorruptFreeList { .. }) => refused_lists += 1,
                Ok(mut file) => {
                    let mut seen = BTreeSet::new();
                    for page in allocate_all(&mut file) {
                        assert!(seen.insert(page));
                        assert!((2..page_count).contains(&page));
                    }
                }
                Err(_) => {}
            }
            let start = target as usize * 4096;
            raw.set_size(image.len() as u64).unwrap();
            raw.write_all_at(&image[..2 * HEADER_SLOT_SIZE], 0).unwrap();
            raw.write_all_at(&image[start..start + 4096], target * 4096)
                .unwrap();
            raw.sync_all().unwrap();
        }
        assert!(refused_lists > 100);
    }
}
