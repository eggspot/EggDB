use crate::allocations::CountingAllocator;
use crate::baseline::Metric;
use crate::measure;
use eggdb_platform::{File, OsMemory, SimFileSystem};
use eggdb_storage::{Access, BTree, CacheSize, CreateOptions, PageCache, PageFile, StorageError};
use std::collections::BTreeMap;
use std::hint::black_box;
use std::path::Path;

const GET_ENTRIES: u64 = 100_000;
const COUNTED_GETS: u64 = 100_000;
const COUNTED_PUTS: u64 = 100_000;
const FRAMES: usize = 32_768;
const SIM_PATH: &str = "bench-btree.db";

pub(crate) const GET: &str = "btree.get";
pub(crate) const PUT: &str = "btree.put";

fn failed(what: &str) -> impl Fn(StorageError) -> String + '_ {
    move |error| format!("{what}: {error}")
}

fn key(index: u64) -> [u8; 16] {
    let mut state = index.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut mixed = [0u8; 16];
    for half in mixed.as_chunks_mut::<8>().0 {
        state = (state ^ (state >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        state = (state ^ (state >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        state ^= state >> 31;
        *half = state.to_le_bytes();
    }
    mixed
}

fn value(index: u64) -> [u8; 16] {
    let mut value = [0u8; 16];
    value[..8].copy_from_slice(&index.to_le_bytes());
    value
}

fn new_tree(fs: &SimFileSystem) -> Result<(PageCache<eggdb_platform::SimFile>, BTree), String> {
    let file = PageFile::create(fs, Path::new(SIM_PATH), CreateOptions::new())
        .map_err(failed("create"))?;
    let mut cache = PageCache::new(file, &OsMemory::new(), CacheSize::Pages(FRAMES))
        .map_err(failed("cache"))?;
    let tree = BTree::create(&cache).map_err(failed("create tree"))?;
    cache
        .commit(|superblock| superblock.catalog_root = tree.root())
        .map_err(failed("commit"))?;
    Ok((cache, tree))
}

fn put_range<F: File>(
    cache: &PageCache<F>,
    tree: &BTree,
    range: std::ops::Range<u64>,
) -> Result<(), String> {
    for index in range {
        tree.put(cache, &key(index), &value(index))
            .map_err(failed("put"))?;
    }
    Ok(())
}

fn get_range<F: File>(cache: &PageCache<F>, tree: &BTree, iterations: u64) -> Result<u8, String> {
    let mut folded = 0u8;
    for index in 0..iterations {
        let found = tree
            .get_with(cache, &key(index % GET_ENTRIES), |value| value[0])
            .map_err(failed("get"))?;
        folded ^= found.ok_or("a stored key was not found")?;
    }
    Ok(folded)
}

pub(crate) fn get(allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let fs = SimFileSystem::new();
    {
        let (mut cache, tree) = new_tree(&fs)?;
        put_range(&cache, &tree, 0..GET_ENTRIES)?;
        cache.commit(|_| {}).map_err(failed("commit"))?;
    }
    let file =
        PageFile::open(&fs, Path::new(SIM_PATH), Access::ReadOnly).map_err(failed("open"))?;
    let cache = PageCache::new(file, &OsMemory::new(), CacheSize::Pages(FRAMES))
        .map_err(failed("cache"))?;
    let tree = BTree::open(&cache, cache.superblock().catalog_root).map_err(failed("open tree"))?;
    black_box(get_range(&cache, &tree, GET_ENTRIES)?);
    let before = cache.stats();
    let operations = fs.operation_count();
    let allocations = allocator.count();
    black_box(get_range(&cache, &tree, COUNTED_GETS)?);
    let per_op = |count: u64| count as f64 / COUNTED_GETS as f64;
    let allocations_per_op = per_op(allocator.count() - allocations);
    let operations_per_op = per_op(fs.operation_count() - operations);
    let after = cache.stats();
    let page_reads_per_op = per_op(after.hits - before.hits);
    if after.misses != before.misses {
        return Err(format!("{GET}: a page read missed the cache"));
    }

    let reference: BTreeMap<Vec<u8>, Vec<u8>> = (0..GET_ENTRIES)
        .map(|index| (key(index).to_vec(), value(index).to_vec()))
        .collect();
    let measured = measure::paired(
        &mut |iterations| {
            black_box(get_range(&cache, &tree, iterations)?);
            Ok(())
        },
        &mut |iterations| {
            let mut folded = 0u8;
            for index in 0..iterations {
                let found = reference
                    .get(&key(black_box(index % GET_ENTRIES))[..])
                    .ok_or("the reference lost a key")?;
                folded ^= found[0];
            }
            black_box(folded);
            Ok(())
        },
    )?;
    Ok(vec![
        Metric::exact(GET, "page_reads_per_op", page_reads_per_op),
        Metric::exact(GET, "allocations_per_op", allocations_per_op),
        Metric::exact(GET, "file_operations_per_op", operations_per_op),
        Metric::ratio(GET, "ratio_to_std_btreemap", measured.ratio),
        Metric::reported(GET, "ns_per_op", measured.subject_ns),
        Metric::reported(GET, "reference_ns_per_op", measured.reference_ns),
    ])
}

pub(crate) fn put(allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let fs = SimFileSystem::new();
    let (cache, tree) = new_tree(&fs)?;
    let pages = cache.stats().cached_pages;
    let operations = fs.operation_count();
    let allocations = allocator.count();
    put_range(&cache, &tree, 0..COUNTED_PUTS)?;
    let allocations = allocator.count() - allocations;
    let operations = fs.operation_count() - operations;
    let pages = cache.stats().cached_pages - pages;

    let measured = measure::paired(
        &mut |iterations| {
            let fs = SimFileSystem::new();
            let (cache, tree) = new_tree(&fs)?;
            put_range(&cache, &tree, 0..iterations)
        },
        &mut |iterations| {
            let mut reference = BTreeMap::new();
            for index in 0..iterations {
                reference.insert(key(index).to_vec(), value(index).to_vec());
            }
            black_box(reference);
            Ok(())
        },
    )?;
    Ok(vec![
        Metric::exact(PUT, "pages_allocated_per_100000_puts", pages as f64),
        Metric::exact(PUT, "allocations_per_100000_puts", allocations as f64),
        Metric::exact(PUT, "file_operations_per_100000_puts", operations as f64),
        Metric::ratio(PUT, "ratio_to_std_btreemap", measured.ratio),
        Metric::reported(PUT, "ns_per_op", measured.subject_ns),
        Metric::reported(PUT, "reference_ns_per_op", measured.reference_ns),
    ])
}
