use crate::allocations::CountingAllocator;
use crate::baseline::Metric;
use crate::measure;
use crate::page_cache::TemporaryFile;
use eggdb_platform::{FileSystem, OsFileSystem, OsMemory, SimFileSystem};
use eggdb_storage::{
    CacheSize, CreateOptions, Durability, PageCache, PageFile, PageHeader, PageType, StorageError,
};
use std::path::Path;

const COUNTED_COMMITS: u64 = 1000;
const FRAMES: usize = 16;
const SIM_PATH: &str = "bench-log.db";

pub(crate) const COMMIT_FULL: &str = "log.commit_full";
pub(crate) const COMMIT_NORMAL: &str = "log.commit_normal";
pub(crate) const COMMIT_OFF: &str = "log.commit_off";

fn failed(what: &str) -> impl Fn(StorageError) -> String + '_ {
    move |error| format!("{what}: {error}")
}

fn one_page_cache<S: FileSystem>(
    fs: &S,
    path: &Path,
    durability: Durability,
) -> Result<(PageCache<S::File>, u64), String> {
    let file = PageFile::create(fs, path, CreateOptions::new()).map_err(failed("create"))?;
    let mut cache = PageCache::new(file, &OsMemory::new(), CacheSize::Pages(FRAMES))
        .map_err(failed("cache"))?;
    let header = PageHeader {
        page_type: PageType::BTreeLeaf,
        layout: 1,
        flags: 0,
        lsn: 0,
    };
    let page = cache
        .allocate(header)
        .map_err(failed("allocate"))?
        .page_id();
    cache.commit(|_| {}).map_err(failed("commit"))?;
    cache
        .set_durability(durability)
        .map_err(failed("durability"))?;
    Ok((cache, page))
}

fn overwrite_and_commit<F: eggdb_platform::File>(
    cache: &mut PageCache<F>,
    page: u64,
    commits: u64,
) -> Result<(), String> {
    for round in 0..commits {
        cache
            .write(page)
            .map_err(failed("write"))?
            .fill(round as u8);
        cache.commit(|_| {}).map_err(failed("commit"))?;
    }
    Ok(())
}

fn counted(name: &'static str, durability: Durability) -> Result<Vec<Metric>, String> {
    let fs = SimFileSystem::new();
    let (mut cache, page) = one_page_cache(&fs, Path::new(SIM_PATH), durability)?;
    let operations = fs.operation_count();
    let syncs = fs.sync_count();
    overwrite_and_commit(&mut cache, page, COUNTED_COMMITS)?;
    Ok(vec![
        Metric::exact(
            name,
            "file_operations_per_1000_commits",
            (fs.operation_count() - operations) as f64,
        ),
        Metric::exact(
            name,
            "syncs_per_1000_commits",
            (fs.sync_count() - syncs) as f64,
        ),
    ])
}

pub(crate) fn commit_full(_: &CountingAllocator) -> Result<Vec<Metric>, String> {
    counted(COMMIT_FULL, Durability::Full)
}

pub(crate) fn commit_normal(_: &CountingAllocator) -> Result<Vec<Metric>, String> {
    counted(COMMIT_NORMAL, Durability::Normal)
}

pub(crate) fn commit_off(allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let mut metrics = counted(COMMIT_OFF, Durability::Off)?;
    let file = TemporaryFile::new();
    let (mut cache, page) = one_page_cache(&OsFileSystem::new(), &file.path, Durability::Off)?;
    overwrite_and_commit(&mut cache, page, COUNTED_COMMITS)?;
    let allocations = allocator.count();
    overwrite_and_commit(&mut cache, page, COUNTED_COMMITS)?;
    let allocations = (allocator.count() - allocations) as f64;
    let ns_per_commit =
        measure::single(&mut |iterations| overwrite_and_commit(&mut cache, page, iterations))?;
    metrics.push(Metric::reported(
        COMMIT_OFF,
        "allocations_per_1000_commits",
        allocations,
    ));
    metrics.push(Metric::reported(COMMIT_OFF, "ns_per_commit", ns_per_commit));
    Ok(metrics)
}
