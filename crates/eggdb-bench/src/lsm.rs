use crate::allocations::CountingAllocator;
use crate::baseline::Metric;
use crate::entries::{key, value};
use crate::measure;
use eggdb_platform::{OsMemory, SimFile, SimFileSystem, SystemClock};
use eggdb_storage::{
    BTree, CacheSize, CreateOptions, LsmTree, PageCache, PageFile, StorageError, Store,
};
use std::path::Path;
use std::sync::Arc;

const ENTRIES: u64 = 100_000;
const PUTS_PER_COMMIT: u64 = 100;
const FRAMES: usize = 32_768;
const SPREAD: u64 = 2_654_435_761;

pub(crate) const RANDOM_PUT: &str = "lsm.random_put";

fn failed(what: &str) -> impl Fn(StorageError) -> String + '_ {
    move |error| format!("{what}: {error}")
}

fn new_store(fs: &SimFileSystem, path: &str) -> Result<Store<SimFile>, String> {
    let file =
        PageFile::create(fs, Path::new(path), CreateOptions::new()).map_err(failed("create"))?;
    let cache =
        PageCache::new(file, &OsMemory::new(), CacheSize::Pages(FRAMES)).map_err(failed("cache"))?;
    Ok(Store::new(cache, Arc::new(SystemClock::new())))
}

fn scattered(index: u64) -> u64 {
    index.wrapping_mul(SPREAD) % ENTRIES
}

enum Tree {
    BTree(BTree),
    Lsm(LsmTree),
}

fn filled(fs: &SimFileSystem, path: &str, lsm: bool) -> Result<(Store<SimFile>, Tree), String> {
    let store = new_store(fs, path)?;
    let mut write = store.begin_write().map_err(failed("begin write"))?;
    let tree = if lsm {
        Tree::Lsm(write.create_lsm_tree().map_err(failed("create"))?)
    } else {
        Tree::BTree(write.create_tree().map_err(failed("create"))?)
    };
    write.commit().map_err(failed("commit"))?;
    let mut index = 0;
    while index < ENTRIES {
        let mut write = store.begin_write().map_err(failed("begin write"))?;
        for _ in 0..1000 {
            put(&mut write, &tree, index)?;
            index += 1;
        }
        write.commit().map_err(failed("commit"))?;
    }
    Ok((store, tree))
}

fn put(
    write: &mut eggdb_storage::WriteTransaction<'_, SimFile>,
    tree: &Tree,
    index: u64,
) -> Result<(), String> {
    let at = scattered(index);
    match tree {
        Tree::BTree(tree) => write.put(tree, &key(at), &value(index)),
        Tree::Lsm(tree) => write.lsm_put(tree, &key(at), &value(index)),
    }
    .map_err(failed("put"))
}

fn commits(store: &Store<SimFile>, tree: &Tree, iterations: u64, start: &mut u64) -> Result<(), String> {
    for _ in 0..iterations {
        let mut write = store.begin_write().map_err(failed("begin write"))?;
        for _ in 0..PUTS_PER_COMMIT {
            put(&mut write, tree, *start)?;
            *start += 1;
        }
        write.commit().map_err(failed("commit"))?;
    }
    Ok(())
}

pub(crate) fn random_put(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let fs = SimFileSystem::new();
    let (lsm_store, lsm_tree) = filled(&fs, "bench-lsm.db", true)?;
    let (btree_store, btree_tree) = filled(&fs, "bench-lsm-btree.db", false)?;
    let mut lsm_next = ENTRIES;
    let mut btree_next = ENTRIES;
    let measured = measure::paired(
        &mut |iterations| commits(&lsm_store, &lsm_tree, iterations, &mut lsm_next),
        &mut |iterations| commits(&btree_store, &btree_tree, iterations, &mut btree_next),
    )?;
    Ok(vec![
        Metric::reported(RANDOM_PUT, "lsm_to_btree", measured.ratio),
        Metric::reported(RANDOM_PUT, "lsm_ns_per_commit_of_100", measured.subject_ns),
        Metric::reported(RANDOM_PUT, "btree_ns_per_commit_of_100", measured.reference_ns),
    ])
}
