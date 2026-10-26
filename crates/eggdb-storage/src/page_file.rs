use crate::bytes::{read_u32, read_u64, write_u32, write_u64};
use crate::error::{FormatError, StorageError};
use crate::format::{
    Access, FileId, HEADER_SLOT_SIZE, NO_PAGE, PAGE_HEADER_SIZE, PAGE_VOLUME_MASK, PageChecksum,
    PageHeader, PageSize, PageType, RECYCLED_SEGMENTS, Superblock,
};
use eggdb_platform::{ErrorKind, File, FileLock, FileSystem, OpenMode};
use std::collections::BTreeSet;
use std::path::Path;

const FREE_LIST_LAYOUT: u8 = 1;
const FREE_LIST_NEXT_AT: usize = PAGE_HEADER_SIZE;
const FREE_LIST_COUNT_AT: usize = PAGE_HEADER_SIZE + 8;
const FREE_LIST_ENTRIES_AT: usize = PAGE_HEADER_SIZE + 16;
const DEFAULT_LOG_SEGMENT_BYTES: usize = 64 << 10;

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
/// writes, growth, and a free list (`docs/format.md`).
///
/// Changes become durable only at [`PageFile::commit`], which syncs the pages and then writes
/// the next superblock to the slot the current one is not in. A crash at any moment leaves the
/// header, the page count and the free list as the last commit left them, and pages allocated
/// since then unreachable. A page freed since the last commit is not handed out again until a
/// commit has made the free durable, and writes to free pages are refused, so the page file
/// itself never overwrites a page of the committed state.
///
/// Overwriting a page of the committed state in place, through [`PageFile::write_page`], is
/// not undone by a crash: the write-ahead log makes such writes safe, by logging a change
/// before its page is written and replaying it after a crash.
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
    superblock: Superblock,
    checksums: PageChecksum,
    page_count: u64,
    available: BTreeSet<u64>,
    freed: BTreeSet<u64>,
    free_list_pages: BTreeSet<u64>,
    damaged_header_slot: Option<usize>,
    failed: bool,
}

impl<F: File> PageFile<F> {
    /// Creates a new database file at `path`, refusing a path that already exists, and makes
    /// the file and its directory entry durable.
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
            log_segment_pages: (DEFAULT_LOG_SEGMENT_BYTES / page_size.bytes()).max(1) as u32,
            recycled_segments: [NO_PAGE; RECYCLED_SEGMENTS],
        };
        file.set_size(superblock.page_count * page_size.bytes() as u64)?;
        file.write_all_at(&superblock.encode()?, 0)?;
        file.sync_all()?;
        fs.sync_dir(parent_directory(path))?;
        Ok(Self::from_parts(file, Access::ReadWrite, superblock))
    }

    /// Opens the database file at `path` for `access`, choosing its current superblock,
    /// checking its features, its length and its free list.
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
        let mut page_file = Self::from_parts(file, access, superblock);
        page_file.damaged_header_slot = choice.damaged_slot;
        page_file.load_free_list()?;
        Ok(page_file)
    }

    fn from_parts(file: F, access: Access, superblock: Superblock) -> Self {
        Self {
            file,
            access,
            checksums: PageChecksum::new(superblock.file_id, superblock.page_size),
            page_count: superblock.page_count,
            superblock,
            available: BTreeSet::new(),
            freed: BTreeSet::new(),
            free_list_pages: BTreeSet::new(),
            damaged_header_slot: None,
            failed: false,
        }
    }

    /// The superblock of the last commit.
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

    /// Reads data page `page_id` into `page`, which must be one page long, and checks it.
    pub fn read_page(&self, page_id: u64, page: &mut [u8]) -> Result<PageHeader, StorageError> {
        self.check_data_page(page_id)?;
        self.check_length(page)?;
        self.file.read_exact_at(page, self.offset(page_id))?;
        Ok(self.checksums.verify(page, page_id)?)
    }

    /// Writes `header` and the checksum into `page`, which must be one page long, and writes
    /// it as data page `page_id`. The page must be in use: allocated, or part of the committed
    /// state and not freed. A write to a free page, to a page freed since the last commit or to
    /// a page of the free list is refused with [`StorageError::PageNotInUse`].
    pub fn write_page(
        &mut self,
        page_id: u64,
        page: &mut [u8],
        header: &PageHeader,
    ) -> Result<(), StorageError> {
        self.check_writable()?;
        self.check_data_page(page_id)?;
        if self.available.contains(&page_id)
            || self.freed.contains(&page_id)
            || self.free_list_pages.contains(&page_id)
        {
            return Err(StorageError::PageNotInUse { page: page_id });
        }
        self.checksums.write(page, page_id, header)?;
        let written = self
            .file
            .write_all_at(page, self.offset(page_id))
            .map_err(StorageError::from);
        self.fail_on_error(written)
    }

    /// Hands out a page to write: a free page of the committed state, or a new page at the end
    /// of the file.
    pub fn allocate(&mut self) -> Result<u64, StorageError> {
        self.check_writable()?;
        if let Some(page) = self.available.pop_first() {
            return Ok(page);
        }
        self.grow()
    }

    /// Gives back a page that is no longer used. It becomes free at the next commit, and is
    /// handed out again only after it.
    pub fn free(&mut self, page_id: u64) -> Result<(), StorageError> {
        self.check_writable()?;
        self.check_data_page(page_id)?;
        if self.available.contains(&page_id)
            || self.freed.contains(&page_id)
            || self.free_list_pages.contains(&page_id)
        {
            return Err(StorageError::DoubleFree { page: page_id });
        }
        self.freed.insert(page_id);
        Ok(())
    }

    /// Makes every change since the last commit durable: writes the new free list, syncs the
    /// file, writes the next superblock with `update` applied to it, and syncs again.
    ///
    /// `update` sets the fields the page file doesn't own, such as the catalog root and the
    /// log's position; the commit itself sets the page size, file identity, generation, page
    /// count and free list, whatever `update` did to them.
    ///
    /// The free list is written to pages that are free in the committed state, or to new pages
    /// at the end, never over the pages of the committed list, so a crash before the new
    /// superblock is durable leaves the old state whole.
    pub fn commit(&mut self, update: impl FnOnce(&mut Superblock)) -> Result<(), StorageError> {
        self.check_writable()?;
        let committed = self.write_free_list_and_superblock(update);
        self.fail_on_error(committed)
    }

    fn write_free_list_and_superblock(
        &mut self,
        update: impl FnOnce(&mut Superblock),
    ) -> Result<(), StorageError> {
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
                None => self.grow()?,
            };
            list_pages.push(page);
        }
        let mut next = self.superblock;
        update(&mut next);
        next.page_size = self.superblock.page_size;
        next.file_id = self.superblock.file_id;
        next.generation =
            self.superblock
                .generation
                .checked_add(1)
                .ok_or(FormatError::InvalidField {
                    field: "generation",
                })?;
        next.page_count = self.page_count;
        next.free_list_head = list_pages.first().copied().unwrap_or(NO_PAGE);
        next.free_page_count = (entries.len() + list_pages.len()) as u64;
        let slot = next.encode()?;

        self.write_free_list(&list_pages, &entries)?;
        let bytes = self.page_count * self.page_size().bytes() as u64;
        if self.file.size()? < bytes {
            self.file.set_size(bytes)?;
        }
        self.file.sync_data()?;
        self.file
            .write_all_at(&slot, (next.slot() * HEADER_SLOT_SIZE) as u64)?;
        self.file.sync_data()?;

        self.superblock = next;
        self.available = entries;
        self.freed.clear();
        self.free_list_pages = list_pages.iter().copied().collect();
        Ok(())
    }

    fn grow(&mut self) -> Result<u64, StorageError> {
        let page = self.page_count;
        if (page + 1) & PAGE_VOLUME_MASK != 0 {
            return Err(StorageError::FileFull);
        }
        self.page_count += 1;
        Ok(page)
    }

    fn write_free_list(
        &mut self,
        list_pages: &[u64],
        entries: &BTreeSet<u64>,
    ) -> Result<(), StorageError> {
        let capacity = self.free_list_capacity();
        let mut remaining = entries.iter().copied();
        let mut page = vec![0u8; self.page_size().bytes()];
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
            self.checksums.write(&mut page, page_id, &header)?;
            self.file.write_all_at(&page, self.offset(page_id))?;
        }
        Ok(())
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
        let superblock = &self.superblock;
        let in_segment = |first: u64| {
            first != NO_PAGE
                && (first..first + u64::from(superblock.log_segment_pages)).contains(&page_id)
        };
        page_id == superblock.catalog_root
            || in_segment(superblock.log_head_page)
            || superblock
                .recycled_segments
                .iter()
                .any(|&first| in_segment(first))
    }

    fn free_list_capacity(&self) -> usize {
        (self.page_size().bytes() - FREE_LIST_ENTRIES_AT) / 8
    }

    fn offset(&self, page_id: u64) -> u64 {
        page_id * self.page_size().bytes() as u64
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

    fn check_writable(&self) -> Result<(), StorageError> {
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
    use crate::format::{Access, FileId, PageChecksum, PageHeader, PageSize, PageType};
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
        assert_eq!(header, leaf(page_id));
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
        assert_eq!(pages, [2, 3, 4]);
        for &page in &pages {
            write_filled(&mut file, page, page as u8);
        }
        file.commit(|superblock| superblock.catalog_root = 3)
            .unwrap();
        drop(file);
        let file = open(&fs, Access::ReadWrite).unwrap();
        assert_eq!(file.superblock().generation, 1);
        assert_eq!(file.superblock().page_count, 5);
        assert_eq!(file.superblock().catalog_root, 3);
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
    fn a_crafted_generation_at_its_limit_refuses_a_commit_instead_of_wrapping() {
        let fs = SimFileSystem::new();
        let mut file = create(&fs);
        file.superblock.generation = u64::MAX;
        assert!(matches!(
            file.commit(|_| {}),
            Err(StorageError::Format(FormatError::InvalidField {
                field: "generation"
            }))
        ));
        assert!(matches!(file.allocate(), Err(StorageError::NeedsReopen)));
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
                expected_bytes: 12288,
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
        file.commit(|superblock| superblock.checkpoint_lsn = 9)
            .unwrap();
        drop(file);
        let raw = fs.open(Path::new(DB), OpenMode::ReadWrite).unwrap();
        raw.write_all_at(&[0xFF; 8], 100).unwrap();
        drop(raw);
        let file = open(&fs, Access::ReadOnly).unwrap();
        assert_eq!(file.superblock().generation, 1);
        assert_eq!(file.damaged_header_slot(), Some(0));
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
        assert_eq!(page, 1);
        write_filled(&mut file, page, 9);
        file.commit(|_| {}).unwrap();
        drop(file);
        let file = open(&fs, Access::ReadOnly).unwrap();
        assert_eq!(file.page_size().bytes(), 65536);
        assert_eq!(read_fill(&file, 1).unwrap(), 9);
        assert_eq!(file.superblock().log_segment_pages, 1);
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
            raw.write_all_at(&image[start..start + 4096], target * 4096)
                .unwrap();
            raw.sync_all().unwrap();
        }
        assert!(refused_lists > 100);
    }
}
