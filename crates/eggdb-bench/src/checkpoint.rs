use crate::allocations::CountingAllocator;
use crate::baseline::Metric;
use crate::measure;
use crate::page_cache::TemporaryFile;
use eggdb_platform::{File, FileSystem, OsFileSystem, OsMemory, SimFileSystem};
use eggdb_storage::{
    Access, CacheSize, CreateOptions, Durability, PageCache, PageFile, PageHeader, PageType,
    StorageError, crc32c,
};
use std::hint::black_box;
use std::path::Path;

const PAGES: usize = 12;
const FRAMES: usize = 64;
const COMMITS_BEFORE_A_CHECKPOINT: u64 = 40;
const LONG_LOG_COMMITS: u64 = 1000;
const SIM_PATH: &str = "bench-checkpoint.db";

pub(crate) const CHECKPOINT: &str = "log.checkpoint";
pub(crate) const RECOVERY: &str = "log.recovery";

fn failed(what: &str) -> impl Fn(StorageError) -> String + '_ {
    move |error| format!("{what}: {error}")
}

fn leaf() -> PageHeader {
    PageHeader {
        page_type: PageType::BTreeLeaf,
        layout: 1,
        flags: 0,
        lsn: 0,
    }
}

fn pages_without_auto_checkpoints<S: FileSystem>(
    fs: &S,
    path: &Path,
    durability: Durability,
) -> Result<(PageCache<S::File>, Vec<u64>), String> {
    let file = PageFile::create(fs, path, CreateOptions::new()).map_err(failed("create"))?;
    let mut cache = PageCache::new(file, &OsMemory::new(), CacheSize::Pages(FRAMES))
        .map_err(failed("cache"))?;
    cache.set_auto_checkpoint_bytes(None);
    let pages = (0..PAGES)
        .map(|_| cache.allocate(leaf()).map(|page| page.page_id()))
        .collect::<Result<Vec<u64>, StorageError>>()
        .map_err(failed("allocate"))?;
    cache.commit(|_| {}).map_err(failed("commit"))?;
    cache
        .set_durability(durability)
        .map_err(failed("durability"))?;
    Ok((cache, pages))
}

fn commits<F: File>(cache: &mut PageCache<F>, pages: &[u64], count: u64) -> Result<(), String> {
    for round in 0..count {
        let page = pages[round as usize % pages.len()];
        cache
            .write(page)
            .map_err(failed("write"))?
            .fill(round as u8);
        cache.commit(|_| {}).map_err(failed("commit"))?;
    }
    Ok(())
}

pub(crate) fn checkpoint(_: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let fs = SimFileSystem::new();
    let (mut cache, pages) =
        pages_without_auto_checkpoints(&fs, Path::new(SIM_PATH), Durability::Full)?;
    commits(&mut cache, &pages, COMMITS_BEFORE_A_CHECKPOINT)?;
    let operations = fs.operation_count();
    let syncs = fs.sync_count();
    cache.checkpoint().map_err(failed("checkpoint"))?;
    let mut metrics = vec![
        Metric::exact(
            CHECKPOINT,
            "file_operations_per_checkpoint",
            (fs.operation_count() - operations) as f64,
        ),
        Metric::exact(
            CHECKPOINT,
            "syncs_per_checkpoint",
            (fs.sync_count() - syncs) as f64,
        ),
    ];
    let file = TemporaryFile::new();
    let (mut cache, pages) =
        pages_without_auto_checkpoints(&OsFileSystem::new(), &file.path, Durability::Off)?;
    let ns = measure::single(&mut |iterations| {
        for _ in 0..iterations {
            commits(&mut cache, &pages, 1)?;
            cache.checkpoint().map_err(failed("checkpoint"))?;
        }
        Ok(())
    })?;
    metrics.push(Metric::reported(
        CHECKPOINT,
        "ns_per_commit_and_checkpoint_unsynced",
        ns,
    ));
    Ok(metrics)
}

fn reopened<S: FileSystem>(fs: &S, path: &Path) -> Result<PageCache<S::File>, String> {
    let file = PageFile::open(fs, path, Access::ReadOnly).map_err(failed("open"))?;
    PageCache::new(file, &OsMemory::new(), CacheSize::Pages(FRAMES)).map_err(failed("cache"))
}

fn counted_open(checkpointed: bool) -> Result<(u64, u64), String> {
    let fs = SimFileSystem::new();
    let path = Path::new(SIM_PATH);
    let (mut cache, pages) = pages_without_auto_checkpoints(&fs, path, Durability::Full)?;
    commits(&mut cache, &pages, LONG_LOG_COMMITS)?;
    if checkpointed {
        cache.checkpoint().map_err(failed("checkpoint"))?;
    }
    drop(cache);
    let operations = fs.operation_count();
    let cache = reopened(&fs, path)?;
    Ok((
        fs.operation_count() - operations,
        cache.log_stats().replayed_bytes,
    ))
}

pub(crate) fn recovery(_: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let (long_operations, long_bytes) = counted_open(false)?;
    let (checkpointed_operations, checkpointed_bytes) = counted_open(true)?;
    let mut metrics = vec![
        Metric::exact(
            RECOVERY,
            "file_operations_to_open_a_1000_commit_log",
            long_operations as f64,
        ),
        Metric::exact(
            RECOVERY,
            "log_bytes_replayed_from_a_1000_commit_log",
            long_bytes as f64,
        ),
        Metric::exact(
            RECOVERY,
            "file_operations_to_open_after_a_checkpoint",
            checkpointed_operations as f64,
        ),
        Metric::exact(
            RECOVERY,
            "log_bytes_replayed_after_a_checkpoint",
            checkpointed_bytes as f64,
        ),
    ];
    let os = OsFileSystem::new();
    let file = TemporaryFile::new();
    let (mut cache, pages) = pages_without_auto_checkpoints(&os, &file.path, Durability::Off)?;
    commits(&mut cache, &pages, LONG_LOG_COMMITS)?;
    drop(cache);
    let replayed = reopened(&os, &file.path)?.log_stats().replayed_bytes;
    let log = vec![0x5Au8; replayed as usize];
    let measured = measure::paired(
        &mut |iterations| {
            for _ in 0..iterations {
                black_box(reopened(&os, &file.path)?);
            }
            Ok(())
        },
        &mut |iterations| {
            for _ in 0..iterations {
                black_box(crc32c(black_box(&log)));
            }
            Ok(())
        },
    )?;
    metrics.push(Metric::ratio(
        RECOVERY,
        "ratio_to_crc32c_of_the_replayed_log",
        measured.ratio,
    ));
    metrics.push(Metric::reported(
        RECOVERY,
        "ns_to_open_a_1000_commit_log",
        measured.subject_ns,
    ));
    let mut cache = PageCache::new(
        PageFile::open(&os, &file.path, Access::ReadWrite).map_err(failed("open"))?,
        &OsMemory::new(),
        CacheSize::Pages(FRAMES),
    )
    .map_err(failed("cache"))?;
    cache.checkpoint().map_err(failed("checkpoint"))?;
    drop(cache);
    let ns_after_checkpoint = measure::single(&mut |iterations| {
        for _ in 0..iterations {
            black_box(reopened(&os, &file.path)?);
        }
        Ok(())
    })?;
    metrics.push(Metric::reported(
        RECOVERY,
        "ns_to_open_after_a_checkpoint",
        ns_after_checkpoint,
    ));
    Ok(metrics)
}
