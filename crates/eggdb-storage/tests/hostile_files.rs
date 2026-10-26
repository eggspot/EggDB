//! Hostile files: a deterministic, seeded fuzzer that damages valid database files in every
//! way a disk, a bug or an attacker could, then opens, reads, scans, checks and writes them.
//! Every outcome must be a value: no panic, no hang and no allocation sized by the damage
//! beyond the file (D9, `docs/testing.md`, "Hostile files").
//!
//! `EGGDB_FUZZ_CASES` sets how many cases run (1,600 by default) and `EGGDB_FUZZ_SEED` the
//! seed they are drawn from, so a long campaign runs as
//! `EGGDB_FUZZ_CASES=200000 cargo test --release -p eggdb-storage --test hostile_files`, and
//! a failure names the seed and case that reproduce it.

use eggdb_platform::{
    CrashMode, File, FileSystem, OpenMode, SimClock, SimFile, SimFileSystem, SimMemory,
};
use eggdb_storage::{
    Access, BTree, CacheSize, CreateOptions, Cursor, Durability, FileId, HEADER_SLOT_SIZE, NO_PAGE,
    PAGE_HEADER_SIZE, PageCache, PageChecksum, PageFile, PageHeader, PageSize, PageType,
    ReadTransaction, ScanOrder, StorageError, Store, Superblock, crc32c, crc32c_append,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::ops::Bound;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DB: &str = "db";
const DEFAULT_CASES: u64 = 1600;
const DEFAULT_SEED: u64 = 0x0D15_EA5E;
const CASE_TIME_LIMIT: Duration = Duration::from_secs(60);
const CACHE_FRAMES: usize = 64;
const ALLOCATION_ALLOWANCE: usize = 16 << 20;
const SUPERBLOCK_CHECKSUM_AT: usize = 8;
const SUPERBLOCK_BYTES: usize = 512;

thread_local! {
    static LARGEST_ALLOCATION: Cell<usize> = const { Cell::new(0) };
}

struct LargestAllocationPerThread;

impl LargestAllocationPerThread {
    fn record(size: usize) {
        let _ = LARGEST_ALLOCATION.try_with(|largest| largest.set(largest.get().max(size)));
    }

    fn take() -> usize {
        LARGEST_ALLOCATION.with(|largest| largest.replace(0))
    }
}

// SAFETY: every call goes unchanged to the system allocator, which meets `GlobalAlloc`'s
// contract; remembering the largest size asked for changes nothing about the memory.
unsafe impl GlobalAlloc for LargestAllocationPerThread {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::record(layout.size());
        // SAFETY: the caller meets `alloc`'s contract, which `System.alloc` shares.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        Self::record(layout.size());
        // SAFETY: the caller meets `alloc_zeroed`'s contract, which `System.alloc_zeroed` shares.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::record(new_size);
        // SAFETY: the caller meets `realloc`'s contract, and `ptr` came from this allocator,
        // which is the system allocator underneath.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller meets `dealloc`'s contract, and `ptr` came from this allocator,
        // which is the system allocator underneath.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: LargestAllocationPerThread = LargestAllocationPerThread;

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u64() % bound as u64) as usize
        }
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> Option<T> {
        (!items.is_empty()).then(|| items[self.below(items.len())])
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next_u64() as u8).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Free,
    Inner,
    Leaf,
    Overflow,
    LogHead,
    Unchecksummed,
}

struct Fixture {
    name: &'static str,
    bytes: Vec<u8>,
    page_size: PageSize,
    file_id: FileId,
    keys: Vec<Vec<u8>>,
    pages: BTreeMap<u64, Kind>,
}

impl Fixture {
    fn page_bytes(&self) -> usize {
        self.page_size.bytes()
    }

    fn page_count(&self) -> u64 {
        (self.bytes.len() / self.page_bytes()) as u64
    }

    fn pages_of(&self, kind: Kind) -> Vec<u64> {
        self.pages
            .iter()
            .filter(|(_, page_kind)| **page_kind == kind)
            .map(|(&page, _)| page)
            .collect()
    }

    fn checksums(&self) -> PageChecksum {
        PageChecksum::new(self.file_id, self.page_size)
    }
}

fn file_system_holding(bytes: &[u8]) -> SimFileSystem {
    let fs = SimFileSystem::new();
    let raw = fs.open(Path::new(DB), OpenMode::Create).unwrap();
    raw.write_all_at(bytes, 0).unwrap();
    raw.sync_all().unwrap();
    fs.sync_dir(Path::new("")).unwrap();
    fs
}

fn file_bytes(fs: &SimFileSystem) -> Vec<u8> {
    let raw = fs.open(Path::new(DB), OpenMode::Read).unwrap();
    let mut bytes = vec![0u8; raw.size().unwrap() as usize];
    raw.read_exact_at(&mut bytes, 0).unwrap();
    bytes
}

fn store_over(file: PageFile<SimFile>) -> Result<Store<SimFile>, StorageError> {
    let cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(CACHE_FRAMES))?;
    Ok(Store::new(cache, Arc::new(SimClock::new(0))))
}

fn value_for(rng: &mut Rng, page_size: PageSize) -> Vec<u8> {
    let page = page_size.bytes();
    let len = match rng.below(10) {
        0 => page / 4 + rng.below(page / 4),
        1 => page + rng.below(4 * page),
        2 => 40 * 1024 + rng.below(page),
        _ => rng.below(80),
    };
    rng.bytes(len)
}

fn key_for(rng: &mut Rng) -> Vec<u8> {
    let len = if rng.below(12) == 0 {
        60 + rng.below(200)
    } else {
        1 + rng.below(10)
    };
    (0..len).map(|_| b"abc\x00\xFF"[rng.below(5)]).collect()
}

struct Builder {
    fs: SimFileSystem,
    store: Store<SimFile>,
    tree: BTree,
    keys: BTreeMap<Vec<u8>, ()>,
    rng: Rng,
    page_size: PageSize,
}

impl Builder {
    fn new(seed: u64, page_size: PageSize, auto_checkpoint: Option<u64>) -> Self {
        let fs = SimFileSystem::new();
        let options = CreateOptions {
            page_size,
            file_id: FileId(*b"hostile-file-fuz"),
        };
        let file = PageFile::create(&fs, Path::new(DB), options).unwrap();
        let cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(256)).unwrap();
        let store = Store::new(cache, Arc::new(SimClock::new(0)));
        store.set_auto_checkpoint_bytes(auto_checkpoint).unwrap();
        let mut write = store.begin_write().unwrap();
        let tree = write.create_tree().unwrap();
        write.set_catalog_root(tree.root());
        write.commit().unwrap();
        Self {
            fs,
            store,
            tree,
            keys: BTreeMap::new(),
            rng: Rng(seed),
            page_size,
        }
    }

    fn commits(&mut self, count: usize, changes: usize, deletes_in_ten: usize) {
        for _ in 0..count {
            let mut write = self.store.begin_write().unwrap();
            for _ in 0..changes {
                if !self.keys.is_empty() && self.rng.below(10) < deletes_in_ten {
                    let index = self.rng.below(self.keys.len());
                    let key = self.keys.keys().nth(index).unwrap().clone();
                    write.delete(&self.tree, &key).unwrap();
                    self.keys.remove(&key);
                } else {
                    let key = key_for(&mut self.rng);
                    let value = value_for(&mut self.rng, self.page_size);
                    write.put(&self.tree, &key, &value).unwrap();
                    self.keys.insert(key, ());
                }
            }
            write.commit().unwrap();
        }
    }

    fn finish(self, name: &'static str) -> Fixture {
        drop(self.store);
        fixture_from(name, file_bytes(&self.fs), self.keys.into_keys().collect())
    }
}

fn fixture_from(name: &'static str, bytes: Vec<u8>, keys: Vec<Vec<u8>>) -> Fixture {
    let (slot0, slot1) = bytes[..2 * HEADER_SLOT_SIZE].split_at(HEADER_SLOT_SIZE);
    let superblock = Superblock::choose(slot0, slot1).unwrap().superblock;
    let page_size = superblock.page_size;
    let checksums = PageChecksum::new(superblock.file_id, page_size);
    let page_bytes = page_size.bytes();
    let pages = (page_size.first_data_page()..(bytes.len() / page_bytes) as u64)
        .map(|page| {
            let start = page as usize * page_bytes;
            let kind = match checksums.verify(&bytes[start..start + page_bytes], page) {
                Ok(header) => match header.page_type {
                    PageType::FreeList => Kind::Free,
                    PageType::BTreeInner => Kind::Inner,
                    PageType::BTreeLeaf => Kind::Leaf,
                    PageType::Overflow => Kind::Overflow,
                    _ => Kind::LogHead,
                },
                Err(_) => Kind::Unchecksummed,
            };
            (page, kind)
        })
        .collect();
    Fixture {
        name,
        bytes,
        page_size,
        file_id: superblock.file_id,
        keys,
        pages,
    }
}

fn checkpointed() -> Fixture {
    let mut builder = Builder::new(1, PageSize::DEFAULT, None);
    builder.commits(30, 14, 1);
    builder.commits(10, 12, 6);
    builder.store.checkpoint().unwrap();
    builder.finish("checkpointed")
}

fn with_log_to_replay() -> Fixture {
    let mut builder = Builder::new(2, PageSize::DEFAULT, None);
    builder.commits(20, 12, 1);
    builder.store.checkpoint().unwrap();
    builder.commits(12, 8, 4);
    builder.finish("log to replay")
}

fn with_wrapped_log() -> Fixture {
    let mut builder = Builder::new(3, PageSize::DEFAULT, Some(96 * 1024));
    builder.commits(90, 6, 3);
    builder.store.checkpoint().unwrap();
    builder.commits(4, 6, 3);
    builder.finish("wrapped log")
}

fn cut_off_by_a_crash() -> Fixture {
    let mut builder = Builder::new(4, PageSize::DEFAULT, Some(128 * 1024));
    builder.store.set_durability(Durability::Normal).unwrap();
    builder.commits(30, 10, 2);
    builder.store.sync().unwrap();
    builder.commits(6, 10, 2);
    let keys: Vec<Vec<u8>> = builder.keys.keys().cloned().collect();
    let fs = builder.fs.clone();
    drop(builder.store);
    fs.crash(CrashMode::Torn { seed: 4 });
    fixture_from("cut off by a crash", file_bytes(&fs), keys)
}

fn with_large_pages() -> Fixture {
    let page_size = PageSize::from_log2(14).unwrap();
    let mut builder = Builder::new(5, page_size, None);
    builder.commits(40, 16, 1);
    builder.store.checkpoint().unwrap();
    builder.commits(4, 8, 3);
    builder.finish("16 KiB pages")
}

#[derive(Debug, Clone, Copy)]
enum Mutation {
    FlippedBits,
    FlippedBitsIn(Kind),
    Truncated,
    Extended,
    ZeroedPage,
    ZeroedRange,
    SwappedPages,
    SwappedPagesRechecked,
    CopiedPageRechecked,
    HeaderField { both_slots: bool },
    HeaderBytes,
    PageBody(Kind),
    PageHeaderFields(Kind),
    FreeListFields,
    LogBytes,
    LogBlockCopied,
}

const MUTATIONS: [Mutation; 22] = [
    Mutation::FlippedBits,
    Mutation::FlippedBitsIn(Kind::Leaf),
    Mutation::FlippedBitsIn(Kind::Unchecksummed),
    Mutation::Truncated,
    Mutation::Extended,
    Mutation::ZeroedPage,
    Mutation::ZeroedRange,
    Mutation::SwappedPages,
    Mutation::SwappedPagesRechecked,
    Mutation::CopiedPageRechecked,
    Mutation::HeaderField { both_slots: false },
    Mutation::HeaderField { both_slots: true },
    Mutation::HeaderBytes,
    Mutation::PageBody(Kind::Inner),
    Mutation::PageBody(Kind::Leaf),
    Mutation::PageBody(Kind::Overflow),
    Mutation::PageBody(Kind::Free),
    Mutation::PageHeaderFields(Kind::Leaf),
    Mutation::PageHeaderFields(Kind::Overflow),
    Mutation::FreeListFields,
    Mutation::LogBytes,
    Mutation::LogBlockCopied,
];

fn interesting_number(rng: &mut Rng, fixture: &Fixture) -> u64 {
    let pages = fixture.page_count();
    match rng.below(12) {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => pages.saturating_sub(1),
        4 => pages,
        5 => pages + 1 + rng.below(16) as u64,
        6 => u64::MAX - rng.below(2) as u64,
        7 => 1 << (31 + rng.below(30)),
        8 => u64::from(u32::MAX),
        9 => fixture.bytes.len() as u64 + rng.below(4096) as u64,
        _ => rng.below(pages as usize) as u64,
    }
}

fn page_range(fixture: &Fixture, page: u64) -> std::ops::Range<usize> {
    let start = page as usize * fixture.page_bytes();
    start..start + fixture.page_bytes()
}

fn rechecked(bytes: &mut [u8], fixture: &Fixture, page: u64, header: &PageHeader) {
    let range = page_range(fixture, page);
    let _ = fixture.checksums().write(&mut bytes[range], page, header);
}

fn header_of(fixture: &Fixture, page: u64) -> Option<PageHeader> {
    fixture
        .checksums()
        .verify(&fixture.bytes[page_range(fixture, page)], page)
        .ok()
}

fn reseal_slot(bytes: &mut [u8], slot: usize) {
    let start = slot * HEADER_SLOT_SIZE;
    let superblock = &mut bytes[start..start + SUPERBLOCK_BYTES];
    let checksum = crc32c_append(
        crc32c(&superblock[..SUPERBLOCK_CHECKSUM_AT]),
        &superblock[SUPERBLOCK_CHECKSUM_AT + 4..],
    );
    superblock[SUPERBLOCK_CHECKSUM_AT..SUPERBLOCK_CHECKSUM_AT + 4]
        .copy_from_slice(&checksum.to_le_bytes());
}

fn newest_slot(bytes: &[u8]) -> usize {
    let (slot0, slot1) = bytes[..2 * HEADER_SLOT_SIZE].split_at(HEADER_SLOT_SIZE);
    Superblock::choose(slot0, slot1).map_or(0, |choice| choice.superblock.slot())
}

fn mutate_header(rng: &mut Rng, fixture: &Fixture, bytes: &mut [u8], both_slots: bool) {
    let fields: [(usize, usize); 16] = [
        (12, 2),
        (14, 1),
        (16, 8),
        (24, 8),
        (40, 8),
        (64, 8),
        (72, 8),
        (80, 8),
        (88, 8),
        (96, 8),
        (104, 8),
        (112, 8),
        (120, 8),
        (128, 4),
        (136 + 8 * rng.below(7), 8),
        (192, 8),
    ];
    let (at, size) = fields[rng.below(fields.len())];
    let value = interesting_number(rng, fixture).to_le_bytes();
    let newest = newest_slot(bytes);
    let slots: &[usize] = if both_slots { &[0, 1] } else { &[newest] };
    for &slot in slots {
        let start = slot * HEADER_SLOT_SIZE + at;
        bytes[start..start + size].copy_from_slice(&value[..size]);
        reseal_slot(bytes, slot);
    }
}

fn mutate_page_body(rng: &mut Rng, fixture: &Fixture, bytes: &mut [u8], kind: Kind) {
    let Some(page) = rng.pick(&fixture.pages_of(kind)) else {
        return;
    };
    let Some(header) = header_of(fixture, page) else {
        return;
    };
    let range = page_range(fixture, page);
    let body = &mut bytes[range.start + PAGE_HEADER_SIZE..range.end];
    for _ in 0..1 + rng.below(3) {
        let at = match rng.below(3) {
            0 => rng.below(64.min(body.len())),
            _ => rng.below(body.len()),
        };
        match rng.below(4) {
            0 => body[at] = rng.next_u64() as u8,
            1 if at + 2 <= body.len() => {
                let value = [0u16, 1, 0x7FFF, 0x8000, 0xFFFF][rng.below(5)];
                body[at..at + 2].copy_from_slice(&value.to_le_bytes());
            }
            2 if at + 8 <= body.len() => {
                let value = interesting_number(rng, fixture);
                body[at..at + 8].copy_from_slice(&value.to_le_bytes());
            }
            _ => body[at] ^= 1 << rng.below(8),
        }
    }
    rechecked(bytes, fixture, page, &header);
}

fn mutate_page_header(rng: &mut Rng, fixture: &Fixture, bytes: &mut [u8], kind: Kind) {
    let Some(page) = rng.pick(&fixture.pages_of(kind)) else {
        return;
    };
    let Some(mut header) = header_of(fixture, page) else {
        return;
    };
    let types = [
        PageType::FreeList,
        PageType::BTreeInner,
        PageType::BTreeLeaf,
        PageType::Overflow,
        PageType::LogSegment,
    ];
    match rng.below(4) {
        0 => header.page_type = types[rng.below(types.len())],
        1 => header.layout = rng.next_u64() as u8,
        2 => header.flags = 1 << rng.below(16),
        _ => header.lsn = interesting_number(rng, fixture),
    }
    rechecked(bytes, fixture, page, &header);
}

fn mutate_free_list(rng: &mut Rng, fixture: &Fixture, bytes: &mut [u8]) {
    let Some(page) = rng.pick(&fixture.pages_of(Kind::Free)) else {
        return;
    };
    let Some(header) = header_of(fixture, page) else {
        return;
    };
    let range = page_range(fixture, page);
    let page_bytes = &mut bytes[range];
    let value = interesting_number(rng, fixture);
    match rng.below(3) {
        0 => page_bytes[16..24].copy_from_slice(&value.to_le_bytes()),
        1 => page_bytes[24..28].copy_from_slice(&(value as u32).to_le_bytes()),
        _ => {
            let count = u32::from_le_bytes(page_bytes[24..28].try_into().unwrap()) as usize;
            let index = rng.below(count.max(1));
            let at = (32 + 8 * index).min(page_bytes.len() - 8);
            page_bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
        }
    }
    rechecked(bytes, fixture, page, &header);
}

fn swap_pages(rng: &mut Rng, fixture: &Fixture, bytes: &mut [u8], recheck: bool, copy: bool) {
    let pages: Vec<u64> = fixture.pages.keys().copied().collect();
    let (Some(first), Some(second)) = (rng.pick(&pages), rng.pick(&pages)) else {
        return;
    };
    if first == second {
        return;
    }
    let first_bytes = bytes[page_range(fixture, first)].to_vec();
    let second_bytes = bytes[page_range(fixture, second)].to_vec();
    bytes[page_range(fixture, second)].copy_from_slice(&first_bytes);
    if !copy {
        bytes[page_range(fixture, first)].copy_from_slice(&second_bytes);
    }
    if recheck {
        if let Some(header) = header_of(fixture, first) {
            rechecked(bytes, fixture, second, &header);
        }
        if let (false, Some(header)) = (copy, header_of(fixture, second)) {
            rechecked(bytes, fixture, first, &header);
        }
    }
}

fn mutate(rng: &mut Rng, fixture: &Fixture, bytes: &mut Vec<u8>, mutation: Mutation) {
    let page_bytes = fixture.page_bytes();
    match mutation {
        Mutation::FlippedBits => {
            for _ in 0..1 + rng.below(4) {
                let bit = rng.below(bytes.len() * 8);
                bytes[bit / 8] ^= 1 << (bit % 8);
            }
        }
        Mutation::FlippedBitsIn(kind) => {
            if let Some(page) = rng.pick(&fixture.pages_of(kind)) {
                let range = page_range(fixture, page);
                let bit = rng.below(page_bytes * 8);
                bytes[range.start + bit / 8] ^= 1 << (bit % 8);
            }
        }
        Mutation::Truncated => {
            let len = match rng.below(3) {
                0 => rng.below(bytes.len()),
                1 => rng.below(fixture.page_count() as usize) * page_bytes,
                _ => bytes.len() - 1 - rng.below(page_bytes),
            };
            bytes.truncate(len);
        }
        Mutation::Extended => {
            let extra = rng.below(4 * page_bytes) + 1;
            let garbage = if rng.below(2) == 0 {
                vec![0; extra]
            } else {
                rng.bytes(extra)
            };
            bytes.extend_from_slice(&garbage);
        }
        Mutation::ZeroedPage => {
            let page = rng.below(fixture.page_count() as usize) as u64;
            bytes[page_range(fixture, page)].fill(0);
        }
        Mutation::ZeroedRange => {
            let start = rng.below(bytes.len());
            let end = (start + 1 + rng.below(3 * page_bytes)).min(bytes.len());
            bytes[start..end].fill(0);
        }
        Mutation::SwappedPages => swap_pages(rng, fixture, bytes, false, false),
        Mutation::SwappedPagesRechecked => swap_pages(rng, fixture, bytes, true, false),
        Mutation::CopiedPageRechecked => swap_pages(rng, fixture, bytes, true, true),
        Mutation::HeaderField { both_slots } => mutate_header(rng, fixture, bytes, both_slots),
        Mutation::HeaderBytes => {
            let slot = rng.below(2);
            let at = slot * HEADER_SLOT_SIZE + rng.below(SUPERBLOCK_BYTES);
            bytes[at] = rng.next_u64() as u8;
            if rng.below(2) == 0 {
                reseal_slot(bytes, slot);
            }
        }
        Mutation::PageBody(kind) => mutate_page_body(rng, fixture, bytes, kind),
        Mutation::PageHeaderFields(kind) => mutate_page_header(rng, fixture, bytes, kind),
        Mutation::FreeListFields => mutate_free_list(rng, fixture, bytes),
        Mutation::LogBytes => {
            let log: Vec<u64> = fixture
                .pages_of(Kind::LogHead)
                .into_iter()
                .chain(fixture.pages_of(Kind::Unchecksummed))
                .collect();
            if let Some(page) = rng.pick(&log) {
                let range = page_range(fixture, page);
                for _ in 0..1 + rng.below(3) {
                    let at = range.start + rng.below(page_bytes);
                    bytes[at] = rng.next_u64() as u8;
                }
            }
        }
        Mutation::LogBlockCopied => {
            let log = fixture.pages_of(Kind::Unchecksummed);
            if let (Some(from), Some(to)) = (rng.pick(&log), rng.pick(&log)) {
                let block = 512;
                let from = page_range(fixture, from).start + block * rng.below(page_bytes / block);
                let to = page_range(fixture, to).start + block * rng.below(page_bytes / block);
                let copied = bytes[from..from + block].to_vec();
                bytes[to..to + block].copy_from_slice(&copied);
            }
        }
    }
}

struct Exercise<'f> {
    fixture: &'f Fixture,
    rng: Rng,
    entry_limit: usize,
    context: String,
}

impl Exercise<'_> {
    fn drained(
        &self,
        read: &ReadTransaction<'_, SimFile>,
        cursor: &mut Cursor,
    ) -> Result<Vec<Vec<u8>>, StorageError> {
        let mut keys = Vec::new();
        while let Some((key, _)) = read.next(cursor)? {
            keys.push(key.to_vec());
            assert!(
                keys.len() <= self.entry_limit,
                "{}: a scan returned more entries than the file can hold",
                self.context
            );
        }
        Ok(keys)
    }

    fn read_everything(&mut self, store: &Store<SimFile>) -> Result<bool, StorageError> {
        let root = store.superblock()?.catalog_root;
        let read = store.begin_read()?;
        if root == NO_PAGE {
            return Ok(store.check().is_ok());
        }
        let tree = read.open_tree(root)?;
        let mut every_read_worked = true;
        for key in self.fixture.keys.iter().take(300) {
            every_read_worked &= read.get(&tree, key).is_ok();
        }
        let mut scanned = Vec::new();
        for order in [ScanOrder::Ascending, ScanOrder::Descending] {
            let mut cursor = tree.scan(Bound::Unbounded, Bound::Unbounded, order);
            match self.drained(&read, &mut cursor) {
                Ok(keys) => scanned.push(keys.len()),
                Err(_) => every_read_worked = false,
            }
        }
        let prefix = key_for(&mut self.rng);
        let prefix = &prefix[..self.rng.below(3).min(prefix.len())];
        let order = [ScanOrder::Ascending, ScanOrder::Descending][self.rng.below(2)];
        let mut cursor = tree.scan_prefix(prefix, order);
        every_read_worked &= self.drained(&read, &mut cursor).is_ok();
        let (start, end) = (key_for(&mut self.rng), key_for(&mut self.rng));
        let mut cursor = tree.scan(Bound::Included(&start), Bound::Excluded(&end), order);
        every_read_worked &= self.drained(&read, &mut cursor).is_ok();
        drop(read);
        let Ok(report) = store.check() else {
            return Ok(false);
        };
        assert!(
            every_read_worked,
            "{}: the check passed but a read failed",
            self.context
        );
        let entries = report.tree.map_or(0, |tree| tree.entries) as usize;
        assert!(
            scanned.iter().all(|&count| count == entries),
            "{}: the check counted {entries} entries, the scans {scanned:?}",
            self.context
        );
        Ok(true)
    }

    fn write_some(&mut self, store: &Store<SimFile>) -> Result<(), StorageError> {
        let mut write = store.begin_write()?;
        let tree = match write.catalog_root() {
            NO_PAGE => {
                let tree = write.create_tree()?;
                write.set_catalog_root(tree.root());
                tree
            }
            root => write.open_tree(root)?,
        };
        if let Some(key) = self
            .fixture
            .keys
            .get(self.rng.below(self.fixture.keys.len()))
        {
            write.delete(&tree, key)?;
        }
        let key = key_for(&mut self.rng);
        write.put(&tree, &key, b"small")?;
        let large = self.rng.bytes(3 * self.fixture.page_bytes());
        write.put(&tree, b"large value", &large)?;
        write.commit()?;
        store.checkpoint()
    }

    fn run(&mut self, bytes: &[u8]) -> Verdict {
        let fs = file_system_holding(bytes);
        let mut verdict = Verdict::default();
        if let Ok(store) = PageFile::open(&fs, Path::new(DB), Access::ReadOnly).and_then(store_over)
        {
            verdict.opened = true;
            verdict.passed_check = self.read_everything(&store).unwrap_or(false);
        }
        let Ok(store) = PageFile::open(&fs, Path::new(DB), Access::ReadWrite).and_then(store_over)
        else {
            return verdict;
        };
        let passed_before = self.read_everything(&store).unwrap_or(false);
        let written = self.write_some(&store);
        if passed_before && written.is_ok() {
            let checked = store.check();
            assert!(
                checked.is_ok(),
                "{}: a file that passed the check fails it after a commit: {checked:?}",
                self.context
            );
        }
        drop(store);
        let reopened = PageFile::open(&fs, Path::new(DB), Access::ReadOnly).and_then(store_over);
        if passed_before && written.is_ok() {
            let store = reopened.unwrap_or_else(|error| {
                panic!(
                    "{}: a file that passed the check and took a commit fails to reopen: {error:?}",
                    self.context
                )
            });
            assert!(
                self.read_everything(&store).unwrap_or(false),
                "{}: a file that passed the check fails it after a commit and a reopen",
                self.context
            );
        } else if let Ok(store) = reopened {
            let _ = self.read_everything(&store);
        }
        verdict
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Verdict {
    opened: bool,
    passed_check: bool,
}

fn environment_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn run_case(fixtures: &[Fixture], seed: u64, case: u64) -> (Verdict, usize, String) {
    let mut rng = Rng(seed ^ case.wrapping_mul(0xA076_1D64_78BD_642F));
    let fixture = &fixtures[case as usize % fixtures.len()];
    let first = MUTATIONS[(case / fixtures.len() as u64) as usize % MUTATIONS.len()];
    let mut mutations = vec![first];
    for _ in 0..rng.below(3) {
        mutations.push(MUTATIONS[rng.below(MUTATIONS.len())]);
    }
    mutations.sort_by_key(|mutation| matches!(mutation, Mutation::Truncated));
    let mut bytes = fixture.bytes.clone();
    for &mutation in &mutations {
        mutate(&mut rng, fixture, &mut bytes, mutation);
    }
    let context = format!(
        "seed {seed}, case {case}, fixture {:?}, {mutations:?}",
        fixture.name
    );
    let mut exercise = Exercise {
        fixture,
        rng,
        entry_limit: bytes.len() / 2 + 1000,
        context: context.clone(),
    };
    LargestAllocationPerThread::take();
    let verdict = exercise.run(&bytes);
    let largest = LargestAllocationPerThread::take();
    (verdict, largest, context)
}

#[derive(Default)]
struct Progress {
    running: BTreeMap<usize, (u64, Instant)>,
    failures: Vec<String>,
    opened: u64,
    passed_check: u64,
}

#[test]
fn hostile_files_never_panic_hang_or_allocate_beyond_the_file() {
    let cases = environment_number("EGGDB_FUZZ_CASES", DEFAULT_CASES);
    let seed = environment_number("EGGDB_FUZZ_SEED", DEFAULT_SEED);
    let fixtures = [
        checkpointed(),
        with_log_to_replay(),
        with_wrapped_log(),
        cut_off_by_a_crash(),
        with_large_pages(),
    ];
    for fixture in &fixtures {
        assert!(
            fixture.pages_of(Kind::Overflow).len() > 2 && fixture.pages_of(Kind::Leaf).len() > 1,
            "{}: {:?}",
            fixture.name,
            fixture
                .pages
                .values()
                .fold(BTreeMap::new(), |mut counts, kind| {
                    *counts.entry(format!("{kind:?}")).or_insert(0) += 1;
                    counts
                })
        );
    }
    let workers = std::thread::available_parallelism().map_or(2, |count| count.get().min(8));
    let next_case = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let progress = Mutex::new(Progress::default());
    std::thread::scope(|scope| {
        for worker in 0..workers {
            let (fixtures, next_case, stop, progress) = (&fixtures, &next_case, &stop, &progress);
            scope.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let case = next_case.fetch_add(1, Ordering::Relaxed);
                    if case >= cases {
                        break;
                    }
                    progress
                        .lock()
                        .unwrap()
                        .running
                        .insert(worker, (case, Instant::now()));
                    let outcome = catch_unwind(AssertUnwindSafe(|| run_case(fixtures, seed, case)));
                    let mut progress = progress.lock().unwrap();
                    progress.running.remove(&worker);
                    match outcome {
                        Ok((verdict, largest, context)) => {
                            let file_bytes = fixtures[case as usize % fixtures.len()].bytes.len();
                            if largest > 2 * file_bytes + ALLOCATION_ALLOWANCE {
                                progress.failures.push(format!(
                                    "{context}: one allocation of {largest} bytes for a file of {file_bytes}"
                                ));
                            }
                            progress.opened += u64::from(verdict.opened);
                            progress.passed_check += u64::from(verdict.passed_check);
                        }
                        Err(panic) => {
                            let message = panic
                                .downcast_ref::<String>()
                                .cloned()
                                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                                .unwrap_or_default();
                            progress
                                .failures
                                .push(format!("seed {seed}, case {case}: panicked: {message}"));
                        }
                    }
                }
            });
        }
        loop {
            std::thread::sleep(Duration::from_millis(50));
            let progress = progress.lock().unwrap();
            if let Some((case, started)) = progress
                .running
                .values()
                .find(|(_, started)| started.elapsed() > CASE_TIME_LIMIT)
            {
                stop.store(true, Ordering::Relaxed);
                panic!(
                    "seed {seed}, case {case} has run for {:?}: a hostile file hangs",
                    started.elapsed()
                );
            }
            if next_case.load(Ordering::Relaxed) >= cases && progress.running.is_empty() {
                break;
            }
            if progress.failures.len() > 20 {
                stop.store(true, Ordering::Relaxed);
            }
            if stop.load(Ordering::Relaxed) && progress.running.is_empty() {
                break;
            }
        }
    });
    let progress = progress.into_inner().unwrap();
    eprintln!(
        "{cases} hostile files: {} opened, {} passed the check",
        progress.opened, progress.passed_check
    );
    assert!(
        progress.failures.is_empty(),
        "{} failures:\n{}",
        progress.failures.len(),
        progress.failures.join("\n")
    );
    assert!(progress.opened > cases / 4 && progress.passed_check > cases / 20);
    assert!(progress.passed_check < progress.opened);
}

#[test]
fn every_fixture_opens_and_passes_the_check_before_it_is_damaged() {
    for fixture in [
        checkpointed(),
        with_log_to_replay(),
        with_wrapped_log(),
        cut_off_by_a_crash(),
        with_large_pages(),
    ] {
        let mut exercise = Exercise {
            fixture: &fixture,
            rng: Rng(7),
            entry_limit: usize::MAX,
            context: fixture.name.to_string(),
        };
        let verdict = exercise.run(&fixture.bytes);
        assert!(verdict.opened && verdict.passed_check, "{}", fixture.name);
    }
}
