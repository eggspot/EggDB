use crate::allocations::CountingAllocator;
use crate::baseline::Metric;
use crate::measure;
use eggdb_platform::{File, FileSystem, OsFileSystem, OsMemory, SimFileSystem};
use eggdb_storage::{
    Access, CacheSize, CreateOptions, PageCache, PageFile, PageHeader, PageType, StorageError,
};
use std::collections::HashMap;
use std::hint::black_box;
use std::path::{Path, PathBuf};

const HIT_PAGES: usize = 1024;
const MISS_FILE_PAGES: usize = 4096;
const MISS_FRAMES: usize = 64;
const COUNTED_OPERATIONS: u64 = 100_000;
const COMMITTED_PAGES: usize = 100;
const SIM_PATH: &str = "bench.db";

pub(crate) const HIT: &str = "page_cache.hit";
pub(crate) const MISS: &str = "page_cache.miss";
pub(crate) const COMMIT: &str = "page_cache.commit";

fn leaf() -> PageHeader {
    PageHeader {
        page_type: PageType::BTreeLeaf,
        layout: 1,
        flags: 0,
        lsn: 0,
    }
}

fn failed(what: &str) -> impl Fn(StorageError) -> String + '_ {
    move |error| format!("{what}: {error}")
}

fn create_file<S: FileSystem>(fs: &S, path: &Path, pages: usize) -> Result<Vec<u64>, String> {
    let file = PageFile::create(fs, path, CreateOptions::new()).map_err(failed("create"))?;
    let mut cache =
        PageCache::new(file, &OsMemory::new(), CacheSize::Pages(pages)).map_err(failed("cache"))?;
    let mut ids = Vec::with_capacity(pages);
    for index in 0..pages {
        let mut page = cache.allocate(leaf()).map_err(failed("allocate"))?;
        page.fill(index as u8);
        ids.push(page.page_id());
    }
    cache.commit(|_| {}).map_err(failed("commit"))?;
    Ok(ids)
}

fn open_cache<S: FileSystem>(
    fs: &S,
    path: &Path,
    frames: usize,
) -> Result<PageCache<S::File>, String> {
    let file = PageFile::open(fs, path, Access::ReadOnly).map_err(failed("open"))?;
    PageCache::new(file, &OsMemory::new(), CacheSize::Pages(frames)).map_err(failed("cache"))
}

fn scattered(pages: &[u64]) -> Vec<u64> {
    let mut order = pages.to_vec();
    let mut state = 0x5EED_u64;
    for index in (1..order.len()).rev() {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        order.swap(index, (state >> 33) as usize % (index + 1));
    }
    order
}

fn read_first_bytes<F: File>(
    cache: &PageCache<F>,
    order: &[u64],
    iterations: u64,
) -> Result<u8, String> {
    let mask = order.len() - 1;
    let mut folded = 0u8;
    for index in 0..iterations as usize {
        let page_id = order[index & mask];
        match cache.read(black_box(page_id)) {
            Ok(page) => folded ^= page[0],
            Err(error) => return Err(read_failed(page_id, error)),
        }
    }
    Ok(folded)
}

#[cold]
fn read_failed(page_id: u64, error: StorageError) -> String {
    format!("reading page {page_id}: {error}")
}

pub(crate) fn hit(allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let fs = SimFileSystem::new();
    let path = Path::new(SIM_PATH);
    let pages = create_file(&fs, path, HIT_PAGES)?;
    let cache = open_cache(&fs, path, HIT_PAGES)?;
    let order = scattered(&pages);
    black_box(read_first_bytes(&cache, &order, HIT_PAGES as u64)?);
    let misses = cache.stats().misses;

    let operations = fs.operation_count();
    let allocations = allocator.count();
    black_box(read_first_bytes(&cache, &order, COUNTED_OPERATIONS)?);
    let allocations_per_op = (allocator.count() - allocations) as f64 / COUNTED_OPERATIONS as f64;
    let operations_per_op = (fs.operation_count() - operations) as f64 / COUNTED_OPERATIONS as f64;

    let frames: HashMap<u64, usize> = pages
        .iter()
        .enumerate()
        .map(|(frame, &page)| (page, frame))
        .collect();
    let page_bytes = cache.page_size().bytes();
    let memory = vec![1u8; HIT_PAGES * page_bytes];
    let measured = measure::paired(
        &mut |iterations| {
            black_box(read_first_bytes(&cache, &order, iterations)?);
            Ok(())
        },
        &mut |iterations| {
            let mask = order.len() - 1;
            let mut folded = 0u8;
            for index in 0..iterations as usize {
                let page_id = order[index & mask];
                let frame = frames
                    .get(&black_box(page_id))
                    .ok_or_else(|| format!("the reference has no page {page_id}"))?;
                folded ^= memory[frame * page_bytes + 16];
            }
            black_box(folded);
            Ok(())
        },
    )?;
    if cache.stats().misses != misses {
        return Err(format!("{HIT}: a read missed the cache"));
    }
    Ok(vec![
        Metric::exact(HIT, "allocations_per_op", allocations_per_op),
        Metric::exact(HIT, "file_operations_per_op", operations_per_op),
        Metric::ratio(HIT, "ratio_to_reference", measured.ratio),
        Metric::reported(HIT, "ns_per_op", measured.subject_ns),
        Metric::reported(HIT, "reference_ns_per_op", measured.reference_ns),
    ])
}

pub(crate) struct TemporaryFile {
    pub(crate) path: PathBuf,
}

impl TemporaryFile {
    pub(crate) fn new() -> Self {
        let name = format!("eggdb-bench-{}.db", std::process::id());
        let path = std::env::temp_dir().join(name);
        let _ = OsFileSystem::new().remove_file(&path);
        Self { path }
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = OsFileSystem::new().remove_file(&self.path);
    }
}

pub(crate) fn miss(allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let fs = SimFileSystem::new();
    let path = Path::new(SIM_PATH);
    let pages = create_file(&fs, path, MISS_FILE_PAGES)?;
    let cache = open_cache(&fs, path, MISS_FRAMES)?;
    black_box(read_first_bytes(&cache, &pages, MISS_FILE_PAGES as u64)?);
    let before = cache.stats();
    let operations = fs.operation_count();
    black_box(read_first_bytes(&cache, &pages, COUNTED_OPERATIONS)?);
    let after = cache.stats();
    let per_op = |count: u64| count as f64 / COUNTED_OPERATIONS as f64;
    let operations_per_op = per_op(fs.operation_count() - operations);
    let evictions_per_op = per_op(after.evictions - before.evictions);
    let misses_per_op = per_op(after.misses - before.misses);

    let os = OsFileSystem::new();
    let file = TemporaryFile::new();
    let pages = create_file(&os, &file.path, MISS_FILE_PAGES)?;
    let cache = open_cache(&os, &file.path, MISS_FRAMES)?;
    black_box(read_first_bytes(&cache, &pages, MISS_FILE_PAGES as u64)?);
    let allocations = allocator.count();
    black_box(read_first_bytes(&cache, &pages, COUNTED_OPERATIONS)?);
    let allocations_per_op = per_op(allocator.count() - allocations);
    let ns_per_op = measure::single(&mut |iterations| {
        black_box(read_first_bytes(&cache, &pages, iterations)?);
        Ok(())
    })?;
    Ok(vec![
        Metric::exact(MISS, "misses_per_op", misses_per_op),
        Metric::exact(MISS, "file_operations_per_op", operations_per_op),
        Metric::exact(MISS, "evictions_per_op", evictions_per_op),
        Metric::exact(MISS, "allocations_per_op", allocations_per_op),
        Metric::reported(MISS, "ns_per_op", ns_per_op),
    ])
}

pub(crate) fn commit(_: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let fs = SimFileSystem::new();
    let path = Path::new(SIM_PATH);
    create_file(&fs, path, 1)?;
    let file = PageFile::open(&fs, path, Access::ReadWrite).map_err(failed("open"))?;
    let mut cache = PageCache::new(file, &OsMemory::new(), CacheSize::Pages(COMMITTED_PAGES))
        .map_err(failed("cache"))?;
    for _ in 0..COMMITTED_PAGES {
        cache.allocate(leaf()).map_err(failed("allocate"))?.fill(7);
    }
    let operations = fs.operation_count();
    cache.commit(|_| {}).map_err(failed("commit"))?;
    let operations = (fs.operation_count() - operations) as f64;
    Ok(vec![Metric::exact(
        COMMIT,
        "file_operations_per_100_new_pages",
        operations,
    )])
}
