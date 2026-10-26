use crate::allocations::CountingAllocator;
use crate::baseline::Metric;
use crate::entries::{key, value};
use crate::measure;
use eggdb_platform::{OsMemory, SimFile, SimFileSystem, SystemClock};
use eggdb_storage::{BTree, CacheSize, CreateOptions, PageCache, PageFile, StorageError, Store};
use std::hint::black_box;
use std::path::Path;
use std::sync::Arc;

const READ_ENTRIES: u64 = 100_000;
const COUNTED_READS: u64 = 100_000;
const WRITE_ENTRIES: u64 = 1000;
const COUNTED_COMMITS: u64 = 1000;
const FRAMES: usize = 32_768;
const STORE_PATH: &str = "bench-transaction-store.db";
const REFERENCE_PATH: &str = "bench-transaction-reference.db";

pub(crate) const READ: &str = "transaction.read";
pub(crate) const WRITE: &str = "transaction.write";

fn failed(what: &str) -> impl Fn(StorageError) -> String + '_ {
    move |error| format!("{what}: {error}")
}

fn new_cache(fs: &SimFileSystem, path: &str) -> Result<PageCache<SimFile>, String> {
    let file =
        PageFile::create(fs, Path::new(path), CreateOptions::new()).map_err(failed("create"))?;
    PageCache::new(file, &OsMemory::new(), CacheSize::Pages(FRAMES)).map_err(failed("cache"))
}

fn store_with(fs: &SimFileSystem, entries: u64) -> Result<(Store<SimFile>, BTree), String> {
    let store = Store::new(new_cache(fs, STORE_PATH)?, Arc::new(SystemClock::new()));
    let mut write = store.begin_write().map_err(failed("begin write"))?;
    let tree = write.create_tree().map_err(failed("create tree"))?;
    write.set_catalog_root(tree.root());
    for index in 0..entries {
        write
            .put(&tree, &key(index), &value(index))
            .map_err(failed("put"))?;
    }
    write.commit().map_err(failed("commit"))?;
    Ok((store, tree))
}

fn reference_with(fs: &SimFileSystem, entries: u64) -> Result<(PageCache<SimFile>, BTree), String> {
    let mut cache = new_cache(fs, REFERENCE_PATH)?;
    let tree = BTree::create(&cache).map_err(failed("create tree"))?;
    for index in 0..entries {
        tree.put(&cache, &key(index), &value(index))
            .map_err(failed("put"))?;
    }
    cache
        .commit(|superblock| superblock.catalog_root = tree.root())
        .map_err(failed("commit"))?;
    Ok((cache, tree))
}

fn read_in_transactions(
    store: &Store<SimFile>,
    tree: &BTree,
    iterations: u64,
) -> Result<u8, String> {
    let mut folded = 0u8;
    for index in 0..iterations {
        let read = store.begin_read().map_err(failed("begin read"))?;
        let found = read
            .get_with(tree, &key(index % READ_ENTRIES), |value| value[0])
            .map_err(failed("get"))?;
        folded ^= found.ok_or("a stored key was not found")?;
    }
    Ok(folded)
}

fn read_from_cache(
    cache: &PageCache<SimFile>,
    tree: &BTree,
    iterations: u64,
) -> Result<u8, String> {
    let mut folded = 0u8;
    for index in 0..iterations {
        let found = tree
            .get_with(cache, &key(index % READ_ENTRIES), |value| value[0])
            .map_err(failed("get"))?;
        folded ^= found.ok_or("a stored key was not found")?;
    }
    Ok(folded)
}

pub(crate) fn read(allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let fs = SimFileSystem::new();
    let (store, tree) = store_with(&fs, READ_ENTRIES)?;
    let (cache, reference_tree) = reference_with(&fs, READ_ENTRIES)?;
    black_box(read_in_transactions(&store, &tree, READ_ENTRIES)?);
    black_box(read_from_cache(&cache, &reference_tree, READ_ENTRIES)?);
    let operations = fs.operation_count();
    let allocations = allocator.count();
    black_box(read_in_transactions(&store, &tree, COUNTED_READS)?);
    let per_op = |count: u64| count as f64 / COUNTED_READS as f64;
    let allocations_per_op = per_op(allocator.count() - allocations);
    let operations_per_op = per_op(fs.operation_count() - operations);
    let measured = measure::paired(
        &mut |iterations| {
            black_box(read_in_transactions(&store, &tree, iterations)?);
            Ok(())
        },
        &mut |iterations| {
            black_box(read_from_cache(&cache, &reference_tree, iterations)?);
            Ok(())
        },
    )?;
    Ok(vec![
        Metric::exact(READ, "allocations_per_op", allocations_per_op),
        Metric::exact(READ, "file_operations_per_op", operations_per_op),
        Metric::ratio(READ, "ratio_to_btree_get", measured.ratio),
        Metric::reported(READ, "ns_per_op", measured.subject_ns),
        Metric::reported(READ, "reference_ns_per_op", measured.reference_ns),
    ])
}

fn commit_in_transactions(
    store: &Store<SimFile>,
    tree: &BTree,
    commits: u64,
) -> Result<(), String> {
    for round in 0..commits {
        let mut write = store.begin_write().map_err(failed("begin write"))?;
        write
            .put(tree, &key(round % WRITE_ENTRIES), &value(round))
            .map_err(failed("put"))?;
        write.commit().map_err(failed("commit"))?;
    }
    Ok(())
}

fn commit_through_cache(
    cache: &mut PageCache<SimFile>,
    tree: &BTree,
    commits: u64,
) -> Result<(), String> {
    for round in 0..commits {
        tree.put(cache, &key(round % WRITE_ENTRIES), &value(round))
            .map_err(failed("put"))?;
        cache.commit(|_| {}).map_err(failed("commit"))?;
    }
    Ok(())
}

pub(crate) fn write(allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let fs = SimFileSystem::new();
    let (store, tree) = store_with(&fs, WRITE_ENTRIES)?;
    let (mut cache, reference_tree) = reference_with(&fs, WRITE_ENTRIES)?;
    commit_in_transactions(&store, &tree, COUNTED_COMMITS)?;
    let operations = fs.operation_count();
    let syncs = fs.sync_count();
    let allocations = allocator.count();
    commit_in_transactions(&store, &tree, COUNTED_COMMITS)?;
    let allocations = (allocator.count() - allocations) as f64;
    let operations = (fs.operation_count() - operations) as f64;
    let syncs = (fs.sync_count() - syncs) as f64;
    let measured = measure::paired(
        &mut |iterations| commit_in_transactions(&store, &tree, iterations),
        &mut |iterations| commit_through_cache(&mut cache, &reference_tree, iterations),
    )?;
    Ok(vec![
        Metric::exact(WRITE, "file_operations_per_1000_commits", operations),
        Metric::exact(WRITE, "syncs_per_1000_commits", syncs),
        Metric::reported(WRITE, "allocations_per_1000_commits", allocations),
        Metric::ratio(WRITE, "ratio_to_page_cache_commit", measured.ratio),
        Metric::reported(WRITE, "ns_per_commit", measured.subject_ns),
        Metric::reported(WRITE, "reference_ns_per_commit", measured.reference_ns),
    ])
}
