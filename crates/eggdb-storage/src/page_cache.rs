use crate::before_images::{BeforeImages, PageImage};
use crate::cells;
use crate::error::{FormatError, StorageError};
use crate::format::{NO_PAGE, PAGE_HEADER_SIZE, PageHeader, PageSize, PageType, Superblock};
use crate::frames::{FrameMut, FrameRef, Frames};
use crate::log::{Durability, LogStats};
use crate::page_file::{NO_STAMP, PageFile};
use crate::page_table::PageTable;
use eggdb_platform::{ErrorKind, File, Memory};
use std::cell::{Cell, Ref, RefCell};
use std::fmt;
use std::ops::{Deref, DerefMut};

pub(crate) const MAX_FRAMES: u32 = u32::MAX;

const REFERENCED: u8 = 1;
const DIRTY: u8 = 2;
const CONTENTS_CHECKED: u8 = 4;
const ALLOCATED: u8 = 8;

const EMPTY_FRAME_HEADER: PageHeader = PageHeader {
    page_type: PageType::FreeList,
    layout: 0,
    flags: 0,
    lsn: 0,
};

/// How much memory a [`PageCache`] keeps pages in, fixed when the cache is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheSize {
    /// This many pages.
    Pages(usize),
    /// As many whole pages as fit in this many bytes.
    Bytes(usize),
}

impl CacheSize {
    /// How many pages of `page_size` this size holds.
    pub fn pages(self, page_size: PageSize) -> usize {
        match self {
            Self::Pages(pages) => pages,
            Self::Bytes(bytes) => bytes / page_size.bytes(),
        }
    }
}

/// What a [`PageCache`] holds and has done since it was created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct CacheStats {
    /// How many pages the cache can hold at once.
    pub frames: usize,
    /// How many pages it holds now.
    pub cached_pages: usize,
    /// How many of them hold changes the next commit writes.
    pub dirty_pages: usize,
    /// Page lookups answered from memory.
    pub hits: u64,
    /// Page lookups that read the page from the file, including reads that failed.
    pub misses: u64,
    /// Pages dropped from memory to make room for others.
    pub evictions: u64,
    /// Pages logged and written to the file by commits.
    pub pages_written: u64,
    /// Committed images of pages changed since read transactions began, kept in memory for
    /// those transactions.
    pub before_images: usize,
}

/// The pages of a [`PageFile`] kept in a fixed amount of memory (D77): the buffer manager that
/// every structure of the engine reads and changes pages through.
///
/// Its memory is allocated once, when it is created, as [`CacheSize`] frames of one page each,
/// and it never grows. [`PageCache::read`] and [`PageCache::write`] pin a page in a frame and
/// return a handle, [`PageRef`] or [`PageMut`], that keeps it there until the handle is
/// dropped; many pages can be pinned at once. A page that is not in memory is read from the
/// file and its checksum verified, in a frame chosen by clock (second-chance) eviction: a
/// frame is taken from a page that is neither pinned nor changed, and was not used since the
/// clock hand last passed it.
///
/// Changed pages stay in memory until [`PageCache::commit`] logs them and writes them through
/// the [`PageFile`] (no-steal): nothing a transaction changes reaches the file before it
/// commits, and [`PageCache::rollback`] drops it all. A transaction that changes more pages
/// than the cache holds, or a cache whose frames are all pinned, fails with
/// [`StorageError::CacheFull`].
///
/// A page being written excludes every other handle to it, and a page being read excludes
/// writers; a conflicting request fails with [`StorageError::PageBorrowed`] rather than
/// waiting. The cache serves one thread; commits and rollbacks need it exclusively, so no page
/// handle outlives them. A [`Store`](crate::Store) puts it behind a lock and adds read
/// transactions, for which the cache keeps the committed images of pages the writer changes
/// (D86).
pub struct PageCache<F: File> {
    file: RefCell<PageFile<F>>,
    frames: Frames,
    table: PageTable,
    pages: Box<[Cell<u64>]>,
    headers: Box<[Cell<PageHeader>]>,
    states: Box<[Cell<u8>]>,
    hand: Cell<usize>,
    dirty_pages: Cell<usize>,
    hits: Cell<u64>,
    misses: Cell<u64>,
    evictions: Cell<u64>,
    pages_written: Cell<u64>,
    dirty_frames: RefCell<Vec<usize>>,
    commit_order: Vec<usize>,
    images: RefCell<BeforeImages>,
}

impl<F: File> PageCache<F> {
    /// Puts `file` behind a cache of `size`, taking its memory from `memory` now and never
    /// again. A size of no pages, or of more pages than [`u32::MAX`], is refused with
    /// [`StorageError::InvalidCacheSize`], and memory that can't be had fails with the
    /// platform's error.
    pub fn new(
        file: PageFile<F>,
        memory: &impl Memory,
        size: CacheSize,
    ) -> Result<Self, StorageError> {
        let page_size = file.page_size();
        let frames = size.pages(page_size);
        if frames == 0 || u32::try_from(frames).is_err() {
            return Err(StorageError::InvalidCacheSize { pages: frames });
        }
        let frame_memory = Frames::new(memory, frames, page_size.bytes())?;
        Ok(Self {
            frames: frame_memory,
            table: PageTable::for_frames(frames)?,
            pages: cells::filled(frames, NO_PAGE)?,
            headers: cells::filled(frames, EMPTY_FRAME_HEADER)?,
            states: cells::filled(frames, 0)?,
            file: RefCell::new(file),
            hand: Cell::new(0),
            dirty_pages: Cell::new(0),
            hits: Cell::new(0),
            misses: Cell::new(0),
            evictions: Cell::new(0),
            pages_written: Cell::new(0),
            dirty_frames: RefCell::new(cells::reserved(frames)?),
            commit_order: cells::reserved(frames)?,
            images: RefCell::new(BeforeImages::new(frames)),
        })
    }

    /// The page size of the file.
    pub fn page_size(&self) -> PageSize {
        self.file.borrow().page_size()
    }

    /// The superblock of the last commit.
    pub fn superblock(&self) -> Superblock {
        *self.file.borrow().superblock()
    }

    /// How commits wait for the disk (see [`PageFile::durability`]).
    pub fn durability(&self) -> Durability {
        self.file.borrow().durability()
    }

    /// Sets how later commits wait for the disk (see [`PageFile::set_durability`]).
    pub fn set_durability(&mut self, durability: Durability) -> Result<(), StorageError> {
        self.file.get_mut().set_durability(durability)
    }

    /// Makes every commit so far durable with one sync (see [`PageFile::sync`]).
    pub fn sync(&mut self) -> Result<(), StorageError> {
        self.file.get_mut().sync()
    }

    /// Writes a checkpoint (see [`PageFile::checkpoint`]). Pages changed since the last commit
    /// refuse it with [`StorageError::UncommittedChanges`]. The pages in memory stay valid: a
    /// checkpoint writes committed pages where they belong, unchanged.
    pub fn checkpoint(&mut self) -> Result<(), StorageError> {
        if self.dirty_pages.get() > 0 {
            return Err(StorageError::UncommittedChanges);
        }
        self.file.get_mut().checkpoint()
    }

    /// How much log a commit may leave before it checkpoints automatically (see
    /// [`PageFile::auto_checkpoint_bytes`]).
    pub fn auto_checkpoint_bytes(&self) -> Option<u64> {
        self.file.borrow().auto_checkpoint_bytes()
    }

    /// Sets how much log a commit may leave before it checkpoints automatically (see
    /// [`PageFile::set_auto_checkpoint_bytes`]).
    pub fn set_auto_checkpoint_bytes(&mut self, log_bytes: Option<u64>) {
        self.file.get_mut().set_auto_checkpoint_bytes(log_bytes);
    }

    /// What the log has replayed and checkpointed since the file was opened (see
    /// [`PageFile::log_stats`]).
    pub fn log_stats(&self) -> LogStats {
        self.file.borrow().log_stats()
    }

    /// What the cache holds and has done.
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            frames: self.frames.count(),
            cached_pages: self.table.len(),
            dirty_pages: self.dirty_pages.get(),
            hits: self.hits.get(),
            misses: self.misses.get(),
            evictions: self.evictions.get(),
            pages_written: self.pages_written.get(),
            before_images: self.images.borrow().len(),
        }
    }

    /// Pins data page `page_id` for reading, reading it from the file and verifying its
    /// checksum if it is not in memory. A page that is free, outside the file or failing its
    /// checksum is refused, the last with [`FormatError::ChecksumMismatch`] naming it.
    /// After a failed commit, or a commit whose automatic checkpoint failed, every page is
    /// refused with [`StorageError::NeedsReopen`].
    #[inline(always)]
    pub fn read(&self, page_id: u64) -> Result<PageRef<'_>, StorageError> {
        let frame = self.locate(page_id)?;
        let bytes = self
            .frames
            .read(frame)
            .ok_or(StorageError::PageBorrowed { page: page_id })?;
        Ok(PageRef {
            bytes,
            page_id,
            header: &self.headers[frame],
            state: &self.states[frame],
        })
    }

    /// Pins data page `page_id` for changing, reading it in as [`PageCache::read`] does. The
    /// change stays in memory until the next commit. The page must be in use, as for
    /// [`PageFile::write_page`].
    ///
    /// The page counts as unchecked afterwards (see [`PageRef::contents_checked`]): bytes
    /// written through the handle are checked again, by the structure that owns the page,
    /// before it trusts them.
    pub fn write(&self, page_id: u64) -> Result<PageMut<'_>, StorageError> {
        let page = self.write_keeping_check(page_id)?;
        page.state.set(page.state.get() & !CONTENTS_CHECKED);
        Ok(page)
    }

    pub(crate) fn write_keeping_check(&self, page_id: u64) -> Result<PageMut<'_>, StorageError> {
        self.file.borrow().check_writable()?;
        let (frame, first_change) = match self.table.get(page_id) {
            Some(frame) if self.is_dirty(frame as usize) => {
                self.hit(frame as usize);
                (frame as usize, false)
            }
            _ => {
                self.file.borrow().check_page_writable(page_id)?;
                (self.locate(page_id)?, true)
            }
        };
        let bytes = self
            .frames
            .write(frame)
            .ok_or(StorageError::PageBorrowed { page: page_id })?;
        if first_change {
            self.keep_before_image(page_id, frame, &bytes)?;
        }
        self.mark_dirty(frame);
        Ok(PageMut {
            bytes,
            page_id,
            header: &self.headers[frame],
            state: &self.states[frame],
        })
    }

    /// Allocates a page through [`PageFile::allocate`] and pins it for writing, zeroed and with
    /// `header`. It is written to the file at the next commit.
    pub fn allocate(&self, header: PageHeader) -> Result<PageMut<'_>, StorageError> {
        check_header(&header)?;
        self.file.borrow().check_writable()?;
        let (frame, mut bytes) = self.claim_frame()?;
        let page_id = self.file.borrow_mut().allocate()?;
        bytes.fill(0);
        self.install(frame, page_id, header, REFERENCED | ALLOCATED);
        self.mark_dirty(frame);
        Ok(PageMut {
            bytes,
            page_id,
            header: &self.headers[frame],
            state: &self.states[frame],
        })
    }

    /// Gives page `page_id` back through [`PageFile::free`], dropping it from memory with any
    /// change to it. A pinned page is refused with [`StorageError::PageBorrowed`].
    pub fn free(&self, page_id: u64) -> Result<(), StorageError> {
        let cached = self.table.get(page_id).map(|frame| frame as usize);
        if cached.is_some_and(|frame| self.frames.is_latched(frame)) {
            return Err(StorageError::PageBorrowed { page: page_id });
        }
        self.file.borrow_mut().free(page_id)?;
        let kept = match cached {
            Some(frame) if self.states[frame].get() & ALLOCATED != 0 => Ok(()),
            _ => self.keep_freed_page(page_id, cached),
        };
        if let Some(frame) = cached {
            self.discard(frame);
        }
        kept
    }

    /// Commits every changed page, in page order, through the [`PageFile`]'s write-ahead log
    /// with `update` (see [`PageFile::commit`]): each page is logged whole, and written in
    /// place once the log is durable. The pages stay in memory, no longer changed.
    ///
    /// Everything that can refuse the commit is checked before the first write: the update
    /// (see [`PageFile::commit`]), and that no changed page is still held by a handle, which
    /// only a leaked handle can cause. A refused commit writes nothing and leaves the changes
    /// in memory, to commit again or roll back. A failed write or sync leaves the file refusing
    /// changes until it is opened again (D59).
    pub fn commit(&mut self, update: impl FnOnce(&mut Superblock)) -> Result<(), StorageError> {
        self.commit_stamped(NO_STAMP, update)
    }

    pub(crate) fn commit_stamped(
        &mut self,
        stamp: u64,
        update: impl FnOnce(&mut Superblock),
    ) -> Result<(), StorageError> {
        let mut order = std::mem::take(&mut self.commit_order);
        order.clear();
        let states = &self.states;
        order.extend(
            self.dirty_frames
                .get_mut()
                .iter()
                .copied()
                .filter(|&frame| states[frame].get() & DIRTY != 0),
        );
        order.sort_unstable_by_key(|&frame| (self.pages[frame].get(), frame));
        order.dedup();
        let committed = self.write_and_commit(&order, stamp, update);
        if committed.is_ok() {
            self.images.get_mut().commit();
        }
        if committed.is_ok() && self.file.get_mut().has_failed() {
            self.drop_every_page();
        } else if committed.is_ok() {
            for &frame in &order {
                self.states[frame].set(self.states[frame].get() & !(DIRTY | ALLOCATED));
            }
            self.dirty_pages.set(0);
            self.dirty_frames.get_mut().clear();
            self.pages_written
                .set(self.pages_written.get() + order.len() as u64);
        } else if self.file.get_mut().has_failed() {
            self.drop_every_page();
        }
        self.commit_order = order;
        committed
    }

    /// Drops every change since the last commit: changed pages leave memory, and pages
    /// allocated or freed since are as the last commit left them (see [`PageFile::rollback`]).
    /// The changed pages leave memory even when the file refuses, such as after a failed
    /// commit.
    pub fn rollback(&mut self) -> Result<(), StorageError> {
        self.drop_changes();
        self.images.get_mut().discard_pending();
        self.file.get_mut().rollback()
    }

    pub(crate) fn check_writable(&self) -> Result<(), StorageError> {
        self.file.borrow().check_writable()
    }

    pub(crate) fn set_before_image_budget(&mut self, pages: usize) {
        self.images.get_mut().set_budget(pages);
    }

    pub(crate) fn begin_snapshot(&mut self) -> Result<u64, StorageError> {
        let version = self.images.get_mut().register_reader()?;
        let mut images = std::mem::replace(self.images.get_mut(), BeforeImages::new(0));
        let kept = self.keep_uncaptured_changes(&mut images);
        if kept.is_err() {
            images.release_reader(version);
        }
        *self.images.get_mut() = images;
        kept.map(|()| version)
    }

    pub(crate) fn end_snapshot(&mut self, version: u64) {
        self.images.get_mut().release_reader(version);
    }

    pub(crate) fn read_at(
        &self,
        page_id: u64,
        version: u64,
    ) -> Result<SnapshotPage<'_>, StorageError> {
        if self.file.borrow().has_failed() {
            return Err(StorageError::NeedsReopen);
        }
        let images = self
            .images
            .try_borrow()
            .map_err(|_| StorageError::PageBorrowed { page: page_id })?;
        images.check_live(version)?;
        match Ref::filter_map(images, |images| images.image_for(page_id, version)) {
            Ok(image) => Ok(SnapshotPage::Image { page_id, image }),
            Err(_) => self.read(page_id).map(SnapshotPage::Current),
        }
    }

    #[cold]
    #[inline(never)]
    fn keep_before_image(
        &self,
        page_id: u64,
        frame: usize,
        page: &[u8],
    ) -> Result<(), StorageError> {
        let mut images = self
            .images
            .try_borrow_mut()
            .map_err(|_| StorageError::PageBorrowed { page: page_id })?;
        if !images.wants_images() {
            return Ok(());
        }
        let checked = self.states[frame].get() & CONTENTS_CHECKED != 0;
        images.keep_pending(page_id, self.headers[frame].get(), checked, page)
    }

    fn keep_freed_page(&self, page_id: u64, cached: Option<usize>) -> Result<(), StorageError> {
        let mut images = self
            .images
            .try_borrow_mut()
            .map_err(|_| StorageError::PageBorrowed { page: page_id })?;
        if !images.wants_image_of(page_id) {
            return Ok(());
        }
        if let Some(frame) = cached.filter(|&frame| !self.is_dirty(frame)) {
            let bytes = self
                .frames
                .read(frame)
                .ok_or(StorageError::PageBorrowed { page: page_id })?;
            let checked = self.states[frame].get() & CONTENTS_CHECKED != 0;
            return images.keep_pending(page_id, self.headers[frame].get(), checked, &bytes);
        }
        self.keep_committed_image(&mut images, page_id)
    }

    fn keep_uncaptured_changes(&self, images: &mut BeforeImages) -> Result<(), StorageError> {
        if !images.wants_images() {
            return Ok(());
        }
        let dirty_frames = self.dirty_frames.borrow();
        for &frame in dirty_frames.iter() {
            let state = self.states[frame].get();
            if state & DIRTY != 0 && state & ALLOCATED == 0 {
                self.keep_committed_image(images, self.pages[frame].get())?;
            }
        }
        let file = self.file.borrow();
        for page_id in file.freed_committed_pages() {
            self.keep_committed_image(images, page_id)?;
        }
        Ok(())
    }

    fn keep_committed_image(
        &self,
        images: &mut BeforeImages,
        page_id: u64,
    ) -> Result<(), StorageError> {
        if !images.wants_image_of(page_id) {
            return Ok(());
        }
        let page_bytes = self.page_size().bytes();
        let mut page = Vec::new();
        page.try_reserve_exact(page_bytes)
            .map_err(|_| StorageError::Io(eggdb_platform::Error::new(ErrorKind::OutOfMemory)))?;
        page.resize(page_bytes, 0);
        let header = self.file.borrow().read_page(page_id, &mut page)?;
        images.keep_pending(page_id, header, false, &page)
    }

    fn drop_every_page(&mut self) {
        for frame in 0..self.frames.count() {
            if self.pages[frame].get() != NO_PAGE {
                self.discard(frame);
            }
        }
        self.dirty_frames.get_mut().clear();
        self.images.get_mut().discard_all();
    }

    fn drop_changes(&mut self) {
        let dirty_frames = std::mem::take(self.dirty_frames.get_mut());
        for &frame in &dirty_frames {
            if self.is_dirty(frame) {
                self.discard(frame);
            }
        }
        *self.dirty_frames.get_mut() = dirty_frames;
        self.dirty_frames.get_mut().clear();
    }

    fn write_and_commit(
        &mut self,
        order: &[usize],
        stamp: u64,
        update: impl FnOnce(&mut Superblock),
    ) -> Result<(), StorageError> {
        self.check_frames_writable(order)?;
        let prepared = self.file.get_mut().prepare_commit(stamp, update)?;
        if let Err(error) = self.log_frames(order) {
            self.file.get_mut().abort_commit();
            return Err(error);
        }
        self.file.get_mut().finish_commit(prepared)
    }

    fn check_frames_writable(&self, order: &[usize]) -> Result<(), StorageError> {
        let file = self.file.borrow();
        for &frame in order {
            let page_id = self.pages[frame].get();
            if self.frames.is_latched(frame) {
                return Err(StorageError::PageBorrowed { page: page_id });
            }
            file.check_page_writable(page_id)?;
        }
        Ok(())
    }

    fn log_frames(&self, order: &[usize]) -> Result<(), StorageError> {
        let mut file = self.file.borrow_mut();
        for &frame in order {
            let page_id = self.pages[frame].get();
            let mut bytes = self
                .frames
                .write(frame)
                .ok_or(StorageError::PageBorrowed { page: page_id })?;
            let header = self.headers[frame].get();
            let lsn = file.log_page(page_id, &mut bytes, &header)?;
            self.headers[frame].set(PageHeader { lsn, ..header });
        }
        Ok(())
    }

    #[inline(always)]
    fn locate(&self, page_id: u64) -> Result<usize, StorageError> {
        match self.table.get(page_id) {
            Some(frame) => {
                self.hit(frame as usize);
                Ok(frame as usize)
            }
            None => self.load(page_id),
        }
    }

    #[inline(always)]
    fn hit(&self, frame: usize) {
        self.hits.set(self.hits.get() + 1);
        self.states[frame].set(self.states[frame].get() | REFERENCED);
    }

    fn load(&self, page_id: u64) -> Result<usize, StorageError> {
        let file = self.file.borrow();
        if file.has_failed() {
            return Err(StorageError::NeedsReopen);
        }
        file.check_page_in_use(page_id)?;
        drop(file);
        let (frame, mut bytes) = self.claim_frame()?;
        self.misses.set(self.misses.get() + 1);
        let header = self.file.borrow().read_page(page_id, &mut bytes)?;
        drop(bytes);
        self.install(frame, page_id, header, REFERENCED);
        Ok(frame)
    }

    fn claim_frame(&self) -> Result<(usize, FrameMut<'_>), StorageError> {
        let count = self.frames.count();
        for _ in 0..count.saturating_mul(2) {
            let frame = self.hand.get();
            self.hand
                .set(if frame + 1 == count { 0 } else { frame + 1 });
            let state = self.states[frame].get();
            if state & DIRTY != 0 {
                continue;
            }
            if state & REFERENCED != 0 {
                self.states[frame].set(state & !REFERENCED);
                continue;
            }
            if let Some(bytes) = self.frames.write(frame) {
                if self.pages[frame].get() != NO_PAGE {
                    self.evictions.set(self.evictions.get() + 1);
                }
                self.empty(frame);
                return Ok((frame, bytes));
            }
        }
        Err(self.cache_full())
    }

    fn cache_full(&self) -> StorageError {
        let frames = self.frames.count();
        StorageError::CacheFull {
            frames,
            pinned: (0..frames)
                .filter(|&frame| self.frames.is_latched(frame))
                .count(),
            dirty: self.dirty_pages.get(),
        }
    }

    fn install(&self, frame: usize, page_id: u64, header: PageHeader, state: u8) {
        self.pages[frame].set(page_id);
        self.headers[frame].set(header);
        self.states[frame].set(state);
        self.table.insert(page_id, frame as u32);
    }

    fn discard(&self, frame: usize) {
        if self.is_dirty(frame) {
            self.dirty_pages.set(self.dirty_pages.get() - 1);
        }
        self.empty(frame);
    }

    fn empty(&self, frame: usize) {
        let page_id = self.pages[frame].replace(NO_PAGE);
        if page_id != NO_PAGE {
            self.table.remove(page_id);
        }
        self.states[frame].set(0);
    }

    fn mark_dirty(&self, frame: usize) {
        let state = self.states[frame].get();
        if state & DIRTY == 0 {
            self.states[frame].set(state | DIRTY);
            self.dirty_pages.set(self.dirty_pages.get() + 1);
            let mut dirty_frames = self.dirty_frames.borrow_mut();
            if dirty_frames.len() == dirty_frames.capacity() {
                dirty_frames.retain(|&listed| self.is_dirty(listed) && listed != frame);
                dirty_frames.sort_unstable();
                dirty_frames.dedup();
            }
            dirty_frames.push(frame);
        }
    }

    fn is_dirty(&self, frame: usize) -> bool {
        self.states[frame].get() & DIRTY != 0
    }
}

impl<F: File> fmt::Debug for PageCache<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageCache")
            .field("file", &self.file)
            .field("frames", &self.frames)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

pub(crate) trait TreePage: Deref<Target = [u8]> {
    fn page_id(&self) -> u64;
    fn header(&self) -> PageHeader;
    fn contents_checked(&self) -> bool;
    fn set_contents_checked(&self);
}

pub(crate) trait PageSource {
    type Page<'a>: TreePage
    where
        Self: 'a;

    fn page(&self, page_id: u64) -> Result<Self::Page<'_>, StorageError>;
}

impl<F: File> PageSource for PageCache<F> {
    type Page<'a>
        = PageRef<'a>
    where
        Self: 'a;

    #[inline(always)]
    fn page(&self, page_id: u64) -> Result<PageRef<'_>, StorageError> {
        self.read(page_id)
    }
}

pub(crate) struct Snapshot<'a, F: File> {
    pub(crate) cache: &'a PageCache<F>,
    pub(crate) version: u64,
}

impl<F: File> PageSource for Snapshot<'_, F> {
    type Page<'a>
        = SnapshotPage<'a>
    where
        Self: 'a;

    fn page(&self, page_id: u64) -> Result<SnapshotPage<'_>, StorageError> {
        self.cache.read_at(page_id, self.version)
    }
}

pub(crate) enum SnapshotPage<'a> {
    Current(PageRef<'a>),
    Image {
        page_id: u64,
        image: Ref<'a, PageImage>,
    },
}

impl Deref for SnapshotPage<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Current(page) => page,
            Self::Image { image, .. } => &image.bytes[PAGE_HEADER_SIZE..],
        }
    }
}

impl TreePage for SnapshotPage<'_> {
    fn page_id(&self) -> u64 {
        match self {
            Self::Current(page) => page.page_id,
            Self::Image { page_id, .. } => *page_id,
        }
    }

    fn header(&self) -> PageHeader {
        match self {
            Self::Current(page) => page.header(),
            Self::Image { image, .. } => image.header,
        }
    }

    fn contents_checked(&self) -> bool {
        match self {
            Self::Current(page) => page.contents_checked(),
            Self::Image { image, .. } => image.checked.get(),
        }
    }

    fn set_contents_checked(&self) {
        match self {
            Self::Current(page) => page.set_contents_checked(),
            Self::Image { image, .. } => image.checked.set(true),
        }
    }
}

impl TreePage for PageRef<'_> {
    #[inline(always)]
    fn page_id(&self) -> u64 {
        self.page_id
    }

    #[inline(always)]
    fn header(&self) -> PageHeader {
        self.header.get()
    }

    #[inline(always)]
    fn contents_checked(&self) -> bool {
        self.state.get() & CONTENTS_CHECKED != 0
    }

    #[inline(always)]
    fn set_contents_checked(&self) {
        self.state.set(self.state.get() | CONTENTS_CHECKED);
    }
}

fn check_header(header: &PageHeader) -> Result<(), StorageError> {
    if header.flags != 0 {
        return Err(FormatError::InvalidField { field: "flags" }.into());
    }
    Ok(())
}

/// A page pinned in a [`PageCache`] for reading, which stays in memory until this handle is
/// dropped. It dereferences to the page's contents after its header.
#[derive(Debug)]
pub struct PageRef<'a> {
    bytes: FrameRef<'a>,
    page_id: u64,
    header: &'a Cell<PageHeader>,
    state: &'a Cell<u8>,
}

impl PageRef<'_> {
    /// The page's number.
    pub fn page_id(&self) -> u64 {
        self.page_id
    }

    /// The page's header.
    pub fn header(&self) -> PageHeader {
        self.header.get()
    }

    /// Whether the structure that owns the page has checked its contents since the cache last
    /// read it from the file. The cache clears this whenever it reads a page in, and
    /// [`PageCache::write`] clears it too, so a structure checks each page once per read from
    /// the file, or after a change it did not make itself, instead of on every use (D9).
    pub fn contents_checked(&self) -> bool {
        self.state.get() & CONTENTS_CHECKED != 0
    }

    pub(crate) fn set_contents_checked(&self) {
        self.state.set(self.state.get() | CONTENTS_CHECKED);
    }
}

impl Deref for PageRef<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes[PAGE_HEADER_SIZE..]
    }
}

/// A page pinned in a [`PageCache`] for changing, which no other handle can reach until this
/// one is dropped. It dereferences to the page's contents after its header; the change is
/// written at the next commit.
#[derive(Debug)]
pub struct PageMut<'a> {
    bytes: FrameMut<'a>,
    page_id: u64,
    header: &'a Cell<PageHeader>,
    state: &'a Cell<u8>,
}

impl PageMut<'_> {
    /// The page's number.
    pub fn page_id(&self) -> u64 {
        self.page_id
    }

    /// The page's header.
    pub fn header(&self) -> PageHeader {
        self.header.get()
    }

    /// Sets the header the page is written with. Flags other than zero are refused, as the
    /// format requires.
    pub fn set_header(&mut self, header: PageHeader) -> Result<(), StorageError> {
        check_header(&header)?;
        self.header.set(header);
        Ok(())
    }

    /// Whether the structure that owns the page has checked its contents since the cache last
    /// read it from the file (see [`PageRef::contents_checked`]). A page allocated in this
    /// cache starts unchecked.
    pub fn contents_checked(&self) -> bool {
        self.state.get() & CONTENTS_CHECKED != 0
    }

    pub(crate) fn set_contents_checked(&self) {
        self.state.set(self.state.get() | CONTENTS_CHECKED);
    }
}

impl Deref for PageMut<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes[PAGE_HEADER_SIZE..]
    }
}

impl DerefMut for PageMut<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.bytes[PAGE_HEADER_SIZE..]
    }
}

#[cfg(test)]
mod tests {
    use super::{CacheSize, PageCache};
    use crate::error::{FormatError, StorageError};
    use crate::format::{Access, FileId, PageHeader, PageSize, PageType, Superblock};
    use crate::page_file::{CreateOptions, PageFile};
    use crate::test_rng::TestRng;
    use eggdb_platform::{
        CrashMode, ErrorKind, File, FileSystem, OpenMode, SimFile, SimFileSystem, SimMemory,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    const DB: &str = "db";

    fn leaf() -> PageHeader {
        PageHeader {
            page_type: PageType::BTreeLeaf,
            layout: 1,
            flags: 0,
            lsn: 0,
        }
    }

    fn create(fs: &SimFileSystem, frames: usize) -> PageCache<SimFile> {
        let options = CreateOptions {
            page_size: PageSize::DEFAULT,
            file_id: FileId(*b"page-cache-tests"),
        };
        let file = PageFile::create(fs, Path::new(DB), options).unwrap();
        PageCache::new(file, &SimMemory::new(), CacheSize::Pages(frames)).unwrap()
    }

    fn open(fs: &SimFileSystem, access: Access, frames: usize) -> PageCache<SimFile> {
        let file = PageFile::open(fs, Path::new(DB), access).unwrap();
        PageCache::new(file, &SimMemory::new(), CacheSize::Pages(frames)).unwrap()
    }

    fn allocate_filled(cache: &PageCache<SimFile>, fill: u8) -> u64 {
        let mut page = cache.allocate(leaf()).unwrap();
        page.fill(fill);
        page.page_id()
    }

    fn committed_pages(fs: &SimFileSystem, count: usize, frames: usize) -> Vec<u64> {
        let mut cache = create(fs, frames);
        let mut pages = Vec::new();
        for chunk in 0..count.div_ceil(frames) {
            for _ in 0..frames.min(count - chunk * frames) {
                pages.push(allocate_filled(&cache, pages.len() as u8));
            }
            cache.commit(|_| {}).unwrap();
        }
        pages
    }

    fn fill_of(cache: &PageCache<SimFile>, page_id: u64) -> u8 {
        let page = cache.read(page_id).unwrap();
        assert!(page.iter().all(|&byte| byte == page[0]));
        page[0]
    }

    #[test]
    fn a_second_read_is_a_hit() {
        let fs = SimFileSystem::new();
        let pages = committed_pages(&fs, 3, 4);
        let cache = open(&fs, Access::ReadOnly, 4);
        assert_eq!(fill_of(&cache, pages[1]), 1);
        assert_eq!(fill_of(&cache, pages[1]), 1);
        let stats = cache.stats();
        assert_eq!((stats.hits, stats.misses, stats.evictions), (1, 1, 0));
        assert_eq!((stats.frames, stats.cached_pages), (4, 1));
    }

    #[test]
    fn a_pinned_page_is_never_evicted() {
        let fs = SimFileSystem::new();
        let pages = committed_pages(&fs, 6, 2);
        let cache = open(&fs, Access::ReadOnly, 2);
        let pinned = cache.read(pages[0]).unwrap();
        for (fill, &page) in pages.iter().enumerate().skip(1) {
            assert_eq!(fill_of(&cache, page), fill as u8);
        }
        assert_eq!(pinned[0], 0);
        drop(pinned);
        let before = cache.stats();
        fill_of(&cache, pages[0]);
        assert_eq!(cache.stats().hits, before.hits + 1);
        assert_eq!(before.evictions, 4);
    }

    #[test]
    fn the_clock_gives_a_recently_used_page_a_second_chance() {
        let fs = SimFileSystem::new();
        let pages = committed_pages(&fs, 5, 3);
        let cache = open(&fs, Access::ReadOnly, 3);
        for &page in &pages[..3] {
            fill_of(&cache, page);
        }
        fill_of(&cache, pages[3]);
        assert!(cache.table.get(pages[0]).is_none());
        fill_of(&cache, pages[1]);
        fill_of(&cache, pages[4]);
        assert!(cache.table.get(pages[1]).is_some());
        assert!(cache.table.get(pages[2]).is_none());
    }

    #[test]
    fn every_frame_pinned_is_a_readable_error() {
        let fs = SimFileSystem::new();
        let pages = committed_pages(&fs, 3, 2);
        let cache = open(&fs, Access::ReadOnly, 2);
        let _first = cache.read(pages[0]).unwrap();
        let _second = cache.read(pages[1]).unwrap();
        let error = cache.read(pages[2]).unwrap_err();
        assert!(matches!(
            error,
            StorageError::CacheFull {
                frames: 2,
                pinned: 2,
                dirty: 0
            }
        ));
        assert!(error.to_string().contains("2 pinned by open page handles"));
    }

    #[test]
    fn changes_stay_in_memory_until_their_commit_and_beyond_the_budget_fail() {
        let fs = SimFileSystem::new();
        let mut cache = create(&fs, 3);
        cache.commit(|_| {}).unwrap();
        let before = fs.operation_count();
        let pages: Vec<u64> = (0..3).map(|fill| allocate_filled(&cache, fill)).collect();
        assert_eq!(fs.operation_count(), before);
        let error = cache.allocate(leaf()).unwrap_err();
        assert!(matches!(
            error,
            StorageError::CacheFull {
                frames: 3,
                pinned: 0,
                dirty: 3
            }
        ));
        assert!(
            error
                .to_string()
                .contains("3 changed by the uncommitted transaction")
        );
        assert_eq!(fs.operation_count(), before);
        assert_eq!(cache.stats().dirty_pages, 3);
        cache.commit(|_| {}).unwrap();
        assert_eq!(cache.stats().dirty_pages, 0);
        assert_eq!(cache.stats().pages_written, 3);
        let fourth = allocate_filled(&cache, 9);
        assert_eq!(cache.stats().evictions, 1);
        cache.commit(|_| {}).unwrap();
        drop(cache);
        let cache = open(&fs, Access::ReadOnly, 2);
        for (fill, &page) in pages.iter().enumerate() {
            assert_eq!(fill_of(&cache, page), fill as u8);
        }
        assert_eq!(fill_of(&cache, fourth), 9);
    }

    #[test]
    fn a_rollback_drops_every_change_since_the_commit() {
        let fs = SimFileSystem::new();
        let pages = committed_pages(&fs, 2, 4);
        let mut cache = open(&fs, Access::ReadWrite, 4);
        cache.write(pages[0]).unwrap().fill(7);
        let added = allocate_filled(&cache, 8);
        cache.free(pages[1]).unwrap();
        assert_eq!(cache.stats().dirty_pages, 2);
        cache.rollback().unwrap();
        assert_eq!(cache.stats().dirty_pages, 0);
        assert_eq!(fill_of(&cache, pages[0]), 0);
        assert_eq!(fill_of(&cache, pages[1]), 1);
        assert!(matches!(
            cache.read(added),
            Err(StorageError::PageOutOfRange { .. })
        ));
        assert_eq!(allocate_filled(&cache, 3), added);
        cache.commit(|_| {}).unwrap();
    }

    fn assert_refused_commit_wrote_nothing(
        fs: &SimFileSystem,
        mut cache: PageCache<SimFile>,
        update: impl FnOnce(&mut Superblock),
        expected: StorageError,
    ) {
        let before = fs.operation_count();
        let error = cache.commit(update).unwrap_err();
        assert_eq!(error.to_string(), expected.to_string());
        assert_eq!(fs.operation_count(), before);
        assert!(cache.stats().dirty_pages > 0);
        cache.rollback().unwrap();
        assert_eq!(cache.stats().dirty_pages, 0);
        cache.commit(|_| {}).unwrap();
    }

    #[test]
    fn a_commit_refused_for_its_update_writes_no_page() {
        let fs = SimFileSystem::new();
        let pages = committed_pages(&fs, 2, 4);
        let mut cache = open(&fs, Access::ReadWrite, 4);
        cache
            .commit(|superblock| superblock.catalog_root = pages[0])
            .unwrap();
        cache.write(pages[1]).unwrap().fill(9);
        assert_refused_commit_wrote_nothing(
            &fs,
            cache,
            |superblock| superblock.catalog_root = 999_999_999,
            FormatError::InvalidField {
                field: "catalog_root",
            }
            .into(),
        );
        let cache = open(&fs, Access::ReadOnly, 4);
        assert_eq!(cache.superblock().catalog_root, pages[0]);
        assert_eq!(fill_of(&cache, pages[1]), 1);
    }

    #[test]
    fn a_commit_refused_for_freeing_the_catalog_root_writes_no_page() {
        let fs = SimFileSystem::new();
        let pages = committed_pages(&fs, 2, 4);
        let mut cache = open(&fs, Access::ReadWrite, 4);
        cache
            .commit(|superblock| superblock.catalog_root = pages[0])
            .unwrap();
        cache.write(pages[1]).unwrap().fill(9);
        cache.free(pages[0]).unwrap();
        assert_refused_commit_wrote_nothing(
            &fs,
            cache,
            |_| {},
            FormatError::InvalidField {
                field: "catalog_root",
            }
            .into(),
        );
        let cache = open(&fs, Access::ReadOnly, 4);
        assert_eq!(fill_of(&cache, pages[0]), 0);
        assert_eq!(fill_of(&cache, pages[1]), 1);
    }

    #[test]
    fn a_leaked_page_handle_refuses_the_commit_before_any_write() {
        let fs = SimFileSystem::new();
        let pages = committed_pages(&fs, 3, 4);
        let cache = open(&fs, Access::ReadWrite, 4);
        cache.write(pages[0]).unwrap().fill(8);
        let mut leaked = cache.write(pages[2]).unwrap();
        leaked.fill(8);
        std::mem::forget(leaked);
        assert_refused_commit_wrote_nothing(
            &fs,
            cache,
            |_| {},
            StorageError::PageBorrowed { page: pages[2] },
        );
        let cache = open(&fs, Access::ReadOnly, 4);
        for (fill, &page) in pages.iter().enumerate() {
            assert_eq!(fill_of(&cache, page), fill as u8);
        }
    }

    #[test]
    fn the_largest_size_is_refused_by_the_memory_budget_without_aborting() {
        let fs = SimFileSystem::new();
        drop(create(&fs, 1));
        let memory = SimMemory::new();
        memory.set_budget(1 << 20);
        let file = PageFile::open(&fs, Path::new(DB), Access::ReadOnly).unwrap();
        assert!(matches!(
            PageCache::new(file, &memory, CacheSize::Pages(u32::MAX as usize)),
            Err(StorageError::Io(error)) if error.kind() == ErrorKind::OutOfMemory
        ));
    }

    #[test]
    fn a_page_being_written_excludes_every_other_handle() {
        let fs = SimFileSystem::new();
        let pages = committed_pages(&fs, 2, 4);
        let cache = open(&fs, Access::ReadWrite, 4);
        let writer = cache.write(pages[0]).unwrap();
        for result in [cache.read(pages[0]).err(), cache.write(pages[0]).err()] {
            assert!(
                matches!(result, Some(StorageError::PageBorrowed { page }) if page == pages[0])
            );
        }
        assert!(matches!(
            cache.free(pages[0]),
            Err(StorageError::PageBorrowed { .. })
        ));
        drop(writer);
        let readers = (cache.read(pages[0]).unwrap(), cache.read(pages[0]).unwrap());
        assert!(matches!(
            cache.write(pages[0]),
            Err(StorageError::PageBorrowed { .. })
        ));
        drop(readers);
        cache.free(pages[0]).unwrap();
        assert!(matches!(
            cache.read(pages[0]),
            Err(StorageError::PageNotInUse { .. })
        ));
    }

    #[test]
    fn a_write_through_a_handle_clears_the_contents_checked_bit_and_the_owners_write_keeps_it() {
        let fs = SimFileSystem::new();
        let pages = committed_pages(&fs, 1, 4);
        let cache = open(&fs, Access::ReadWrite, 4);
        cache.read(pages[0]).unwrap().set_contents_checked();
        assert!(cache.read(pages[0]).unwrap().contents_checked());
        assert!(
            cache
                .write_keeping_check(pages[0])
                .unwrap()
                .contents_checked()
        );
        assert!(!cache.write(pages[0]).unwrap().contents_checked());
        assert!(!cache.read(pages[0]).unwrap().contents_checked());
    }

    #[test]
    fn freeing_a_changed_page_drops_its_change() {
        let fs = SimFileSystem::new();
        let mut cache = create(&fs, 2);
        let page = allocate_filled(&cache, 1);
        assert_eq!(cache.stats().dirty_pages, 1);
        cache.free(page).unwrap();
        assert_eq!(cache.stats().dirty_pages, 0);
        assert_eq!(cache.stats().cached_pages, 0);
        cache.commit(|_| {}).unwrap();
        assert_eq!(cache.stats().pages_written, 0);
    }

    #[test]
    fn a_damaged_page_fails_its_checksum_by_name_and_is_not_cached() {
        let fs = SimFileSystem::new();
        let pages = committed_pages(&fs, 2, 4);
        let cache = open(&fs, Access::ReadWrite, 4);
        let raw = fs.open(Path::new(DB), OpenMode::ReadWrite).unwrap();
        raw.write_all_at(&[0xEE], pages[1] * 4096 + 1000).unwrap();
        drop(raw);
        let error = cache.read(pages[1]).unwrap_err();
        assert!(matches!(
            error,
            StorageError::Format(FormatError::ChecksumMismatch { page }) if page == pages[1]
        ));
        assert!(error.to_string().contains(&format!("page {}", pages[1])));
        assert_eq!(cache.stats().cached_pages, 0);
        assert_eq!(fill_of(&cache, pages[0]), 0);
        assert!(cache.read(pages[1]).is_err());
    }

    #[test]
    fn an_injected_read_fault_is_an_error_and_the_next_read_succeeds() {
        let fs = SimFileSystem::new();
        let pages = committed_pages(&fs, 1, 4);
        let cache = open(&fs, Access::ReadOnly, 4);
        fs.fail_operation(fs.operation_count());
        let error = cache.read(pages[0]).unwrap_err();
        assert!(
            matches!(error, StorageError::Io(error) if error.kind() == ErrorKind::InjectedFault)
        );
        assert_eq!(cache.stats().cached_pages, 0);
        assert_eq!(fill_of(&cache, pages[0]), 0);
    }

    #[test]
    fn after_a_failed_commit_every_read_and_change_is_refused_until_a_reopen() {
        let mut failed = 0;
        for offset in 0..12 {
            let fs = SimFileSystem::new();
            let pages = committed_pages(&fs, 2, 4);
            let mut cache = open(&fs, Access::ReadWrite, 4);
            cache.write(pages[0]).unwrap().fill(7);
            cache.write(pages[1]).unwrap().fill(8);
            assert_eq!(fill_of(&cache, pages[0]), 7);
            fs.fail_operation(fs.operation_count() + offset);
            if cache.commit(|_| {}).is_ok() {
                continue;
            }
            failed += 1;
            assert_eq!(cache.stats().dirty_pages, 0, "offset {offset}");
            assert_eq!(cache.stats().cached_pages, 0, "offset {offset}");
            for &page in &pages {
                assert!(matches!(cache.read(page), Err(StorageError::NeedsReopen)));
            }
            assert!(matches!(cache.rollback(), Err(StorageError::NeedsReopen)));
            assert!(matches!(
                cache.write(pages[0]),
                Err(StorageError::NeedsReopen)
            ));
            assert_eq!(cache.stats().dirty_pages, 0);
        }
        assert!(failed >= 3, "{failed}");
    }

    #[test]
    fn one_frame_serves_one_page_at_a_time() {
        let fs = SimFileSystem::new();
        let pages = committed_pages(&fs, 3, 1);
        let mut cache = open(&fs, Access::ReadWrite, 1);
        for (fill, &page) in pages.iter().enumerate() {
            assert_eq!(fill_of(&cache, page), fill as u8);
        }
        cache.write(pages[2]).unwrap().fill(5);
        assert!(matches!(
            cache.read(pages[0]),
            Err(StorageError::CacheFull { frames: 1, .. })
        ));
        cache.commit(|_| {}).unwrap();
        assert_eq!(fill_of(&cache, pages[0]), 0);
        assert_eq!(fill_of(&cache, pages[2]), 5);
    }

    #[test]
    fn sizes_outside_the_allowed_range_are_refused() {
        let fs = SimFileSystem::new();
        drop(create(&fs, 1));
        for size in [CacheSize::Pages(0), CacheSize::Bytes(4095)] {
            let file = PageFile::open(&fs, Path::new(DB), Access::ReadOnly).unwrap();
            assert!(matches!(
                PageCache::new(file, &SimMemory::new(), size),
                Err(StorageError::InvalidCacheSize { pages: 0 })
            ));
        }
        let file = PageFile::open(&fs, Path::new(DB), Access::ReadOnly).unwrap();
        let cache = PageCache::new(file, &SimMemory::new(), CacheSize::Bytes(10_000)).unwrap();
        assert_eq!(cache.stats().frames, 2);
    }

    #[test]
    fn memory_that_can_not_be_had_is_an_error() {
        let fs = SimFileSystem::new();
        drop(create(&fs, 1));
        let memory = SimMemory::new();
        memory.set_budget(4096);
        let file = PageFile::open(&fs, Path::new(DB), Access::ReadOnly).unwrap();
        assert!(matches!(
            PageCache::new(file, &memory, CacheSize::Pages(2)),
            Err(StorageError::Io(error)) if error.kind() == ErrorKind::OutOfMemory
        ));
    }

    #[test]
    fn a_read_only_cache_refuses_changes_and_bad_headers_are_refused() {
        let fs = SimFileSystem::new();
        let pages = committed_pages(&fs, 1, 2);
        let cache = open(&fs, Access::ReadOnly, 2);
        assert!(matches!(cache.write(pages[0]), Err(StorageError::ReadOnly)));
        assert!(matches!(
            cache.allocate(leaf()),
            Err(StorageError::ReadOnly)
        ));
        let mut cache = cache;
        assert!(matches!(cache.rollback(), Err(StorageError::ReadOnly)));
        assert!(matches!(cache.commit(|_| {}), Err(StorageError::ReadOnly)));
        drop(cache);
        let cache = open(&fs, Access::ReadWrite, 2);
        let flagged = PageHeader { flags: 1, ..leaf() };
        assert!(matches!(
            cache.allocate(flagged),
            Err(StorageError::Format(FormatError::InvalidField {
                field: "flags"
            }))
        ));
        let mut page = cache.write(pages[0]).unwrap();
        assert!(page.set_header(flagged).is_err());
        page.set_header(PageHeader { lsn: 9, ..leaf() }).unwrap();
        assert_eq!(page.header().lsn, 9);
    }

    #[test]
    fn the_cache_moves_between_threads_with_its_file() {
        fn assert_send<T: Send>() {}
        assert_send::<PageCache<SimFile>>();
    }

    #[test]
    fn large_pages_fill_whole_frames() {
        let fs = SimFileSystem::new();
        let options = CreateOptions {
            page_size: PageSize::from_log2(16).unwrap(),
            file_id: FileId(*b"page-cache-tests"),
        };
        let file = PageFile::create(&fs, Path::new(DB), options).unwrap();
        let mut cache = PageCache::new(file, &SimMemory::new(), CacheSize::Bytes(1 << 17)).unwrap();
        assert_eq!(cache.stats().frames, 2);
        let pages: Vec<u64> = (0..3)
            .map(|fill| {
                let page = allocate_filled(&cache, fill);
                cache.commit(|_| {}).unwrap();
                page
            })
            .collect();
        assert_eq!(cache.superblock().log_segment_pages, 2);
        assert!(pages.windows(2).all(|pair| pair[0] < pair[1]));
        for (fill, &page) in pages.iter().enumerate() {
            let bytes = cache.read(page).unwrap();
            assert_eq!(bytes.len(), 65536 - 16);
            assert!(bytes.iter().all(|&byte| byte == fill as u8));
        }
    }

    #[test]
    fn header_slots_and_pages_past_the_end_are_refused() {
        let fs = SimFileSystem::new();
        let cache = create(&fs, 2);
        for page in [0, 1, 2, u64::MAX] {
            assert!(matches!(
                cache.read(page),
                Err(StorageError::PageOutOfRange { .. })
            ));
            assert!(matches!(
                cache.write(page),
                Err(StorageError::PageOutOfRange { .. })
            ));
        }
        assert_eq!(cache.stats().misses, 0);
    }

    #[test]
    fn a_workload_larger_than_the_cache_matches_a_model() {
        let fs = SimFileSystem::new();
        let mut cache = create(&fs, 8);
        let mut model: BTreeMap<u64, u8> = BTreeMap::new();
        let mut committed = model.clone();
        let mut rng = TestRng::new(0xCAC4E);
        for step in 0..20_000 {
            let pages: Vec<u64> = model.keys().copied().collect();
            let pick = |rng: &mut TestRng| pages[rng.below(pages.len())];
            let outcome = match rng.below(10) {
                0..=3 if !pages.is_empty() => {
                    let page = pick(&mut rng);
                    cache.read(page).map(|bytes| {
                        assert!(
                            bytes.iter().all(|&byte| byte == model[&page]),
                            "step {step}"
                        );
                    })
                }
                4..=5 if !pages.is_empty() => {
                    let page = pick(&mut rng);
                    let fill = rng.next_u64() as u8;
                    cache.write(page).map(|mut bytes| {
                        bytes.fill(fill);
                        model.insert(page, fill);
                    })
                }
                6 if pages.len() > 4 => {
                    let page = pick(&mut rng);
                    cache.free(page).map(|()| {
                        model.remove(&page);
                    })
                }
                7 => {
                    let fill = rng.next_u64() as u8;
                    cache.allocate(leaf()).map(|mut bytes| {
                        bytes.fill(fill);
                        model.insert(bytes.page_id(), fill);
                    })
                }
                8 => cache.commit(|_| {}).map(|()| committed = model.clone()),
                _ => cache.rollback().map(|()| model = committed.clone()),
            };
            match outcome {
                Ok(()) => {}
                Err(StorageError::CacheFull { dirty: 8, .. }) => {
                    cache.commit(|_| {}).unwrap();
                    committed = model.clone();
                }
                Err(error) => panic!("step {step}: {error}"),
            }
            assert!(cache.stats().dirty_pages <= 8);
        }
        cache.commit(|_| {}).unwrap();
        let stats = cache.stats();
        assert!(stats.evictions > 1000 && stats.hits > 1000, "{stats:?}");
        drop(cache);
        let cache = open(&fs, Access::ReadOnly, 3);
        for (&page, &fill) in &model {
            assert_eq!(fill_of(&cache, page), fill);
        }
    }

    fn first_commit(fs: &SimFileSystem) -> Vec<u64> {
        let mut cache = create(fs, 16);
        let pages: Vec<u64> = (0..12).map(|_| allocate_filled(&cache, 1)).collect();
        cache
            .commit(|superblock| superblock.catalog_root = pages[0])
            .unwrap();
        pages
    }

    fn second_commit(fs: &SimFileSystem, pages: &[u64], fail_at: Option<u64>) -> u64 {
        let mut cache = open(fs, Access::ReadWrite, 16);
        for &page in &pages[6..] {
            cache.free(page).unwrap();
        }
        let added: Vec<u64> = (0..8).map(|_| allocate_filled(&cache, 2)).collect();
        let start = fs.operation_count();
        if let Some(offset) = fail_at {
            fs.fail_operation(start + offset);
        }
        let committed = cache.commit(|superblock| superblock.catalog_root = added[0]);
        assert_eq!(committed.is_ok(), fail_at.is_none());
        fs.operation_count() - start
    }

    fn reachable_state(fs: &SimFileSystem, pages: &[u64]) -> (bool, BTreeMap<u64, u8>) {
        let cache = open(fs, Access::ReadOnly, 4);
        let superblock = cache.superblock();
        let in_use: BTreeSet<u64> = if superblock.catalog_root == pages[0] {
            pages.iter().copied().collect()
        } else {
            let added = superblock.catalog_root;
            pages[..6].iter().copied().chain(added..added + 8).collect()
        };
        let fills = in_use
            .into_iter()
            .map(|page| (page, fill_of(&cache, page)))
            .collect();
        (superblock.catalog_root != pages[0], fills)
    }

    #[test]
    fn a_crash_at_any_operation_of_a_commit_leaves_the_last_committed_state_readable() {
        let operations = {
            let fs = SimFileSystem::new();
            let pages = first_commit(&fs);
            second_commit(&fs, &pages, None)
        };
        let mut outcomes = BTreeSet::new();
        for step in 0..operations {
            for seed in 0..4 {
                let fs = SimFileSystem::new();
                let pages = first_commit(&fs);
                second_commit(&fs, &pages, Some(step));
                fs.crash(CrashMode::Torn { seed });
                let (committed, fills) = reachable_state(&fs, &pages);
                let expected_new = |page: &u64| if pages.contains(page) { 1 } else { 2 };
                for (page, fill) in &fills {
                    assert_eq!(*fill, expected_new(page), "step {step}, page {page}");
                }
                outcomes.insert(committed);
            }
        }
        assert_eq!(outcomes, BTreeSet::from([false, true]));
    }
}
