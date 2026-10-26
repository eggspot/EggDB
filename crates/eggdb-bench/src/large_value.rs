use crate::allocations::CountingAllocator;
use crate::baseline::Metric;
use crate::bench_file::BenchFile;
use crate::database::failed;
use crate::measure;
use eggdb::{Database, Durability, Options};
use eggdb_platform::{File, FileSystem, OpenMode, OsFileSystem, SimFileSystem, SystemClock};
use eggdb_storage::{BTree, CacheSize, CreateOptions, PageCache, PageFile, Store};
use std::hint::black_box;
use std::path::Path;
use std::sync::Arc;

const VALUE_BYTES: usize = 100 << 10;
const KEYS: u64 = 4;
const COUNTED_PUTS: u64 = 100;
const COUNTED_GETS: u64 = 1000;
const CACHE_BYTES: usize = 16 << 20;
const SIM_PATH: &str = "bench-large-value.db";

pub(crate) const PUT: &str = "large_value.put";
pub(crate) const GET: &str = "large_value.get";

fn large_value(round: u64) -> Vec<u8> {
    (0..VALUE_BYTES)
        .map(|index| (index as u64).wrapping_mul(31).wrapping_add(round) as u8)
        .collect()
}

fn key(round: u64) -> [u8; 8] {
    (round % KEYS).to_be_bytes()
}

fn database(path: &Path) -> Result<Database, String> {
    let options = Options::new()
        .durability(Durability::Off)
        .cache_bytes(CACHE_BYTES);
    Database::open_with(path, options).map_err(failed("open"))
}

fn put_values(db: &Database, values: &[Vec<u8>], puts: u64) -> Result<(), String> {
    for round in 0..puts {
        db.put(key(round), &values[(round % 2) as usize])
            .map_err(failed("put"))?;
    }
    Ok(())
}

fn write_twice(file: &impl File, value: &[u8], writes: u64) -> Result<(), String> {
    for round in 0..writes {
        let slot = (round % KEYS) * 2 * VALUE_BYTES as u64;
        file.write_all_at(value, slot).map_err(failed("write"))?;
        file.write_all_at(value, slot + VALUE_BYTES as u64)
            .map_err(failed("write"))?;
    }
    Ok(())
}

fn simulated_store(fs: &SimFileSystem) -> Result<(Store<eggdb_platform::SimFile>, BTree), String> {
    let file = PageFile::create(fs, Path::new(SIM_PATH), CreateOptions::new())
        .map_err(failed("create"))?;
    let cache = PageCache::new(
        file,
        &eggdb_platform::OsMemory::new(),
        CacheSize::Bytes(CACHE_BYTES),
    )
    .map_err(failed("cache"))?;
    let store = Store::new(cache, Arc::new(SystemClock::new()));
    let mut write = store.begin_write().map_err(failed("begin write"))?;
    let tree = write.create_tree().map_err(failed("create tree"))?;
    write.set_catalog_root(tree.root());
    write.commit().map_err(failed("commit"))?;
    Ok((store, tree))
}

fn counted_puts(values: &[Vec<u8>]) -> Result<Vec<Metric>, String> {
    let fs = SimFileSystem::new();
    let (store, tree) = simulated_store(&fs)?;
    let put = |rounds: std::ops::Range<u64>| -> Result<(), String> {
        for round in rounds {
            let mut write = store.begin_write().map_err(failed("begin write"))?;
            write
                .put(&tree, &key(round), &values[(round % 2) as usize])
                .map_err(failed("put"))?;
            write.commit().map_err(failed("commit"))?;
        }
        Ok(())
    };
    put(0..4 * COUNTED_PUTS)?;
    let operations = fs.operation_count();
    let syncs = fs.sync_count();
    put(4 * COUNTED_PUTS..5 * COUNTED_PUTS)?;
    let operations = (fs.operation_count() - operations) as f64;
    let syncs = (fs.sync_count() - syncs) as f64;
    let file = store.check().map_err(failed("check"))?;
    let tree_report = file.tree.ok_or("the tree is missing")?;
    Ok(vec![
        Metric::exact(PUT, "file_operations_per_100_puts", operations),
        Metric::exact(PUT, "syncs_per_100_puts", syncs),
        Metric::exact(
            PUT,
            "overflow_pages_per_value",
            (tree_report.overflow_pages / tree_report.overflow_values) as f64,
        ),
        Metric::exact(PUT, "file_pages_after_500_puts", file.page_count as f64),
        Metric::exact(PUT, "leaked_pages_after_500_puts", file.leaked_pages as f64),
    ])
}

pub(crate) fn put(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let values = [large_value(1), large_value(2)];
    let mut metrics = counted_puts(&values)?;
    let subject_file = BenchFile::new("large-value-put")?;
    let reference_file = BenchFile::new("large-value-put-reference")?;
    let db = database(&subject_file.path)?;
    put_values(&db, &values, 4 * KEYS)?;
    let reference = OsFileSystem::new()
        .open(&reference_file.path, OpenMode::Create)
        .map_err(failed("open"))?;
    write_twice(&reference, &values[0], KEYS)?;
    let measured = measure::paired(
        &mut |iterations| put_values(&db, &values, iterations),
        &mut |iterations| write_twice(&reference, &values[0], iterations),
    )?;
    metrics.extend([
        Metric::ratio(PUT, "ratio_to_writing_the_value_twice", measured.ratio),
        Metric::reported(PUT, "ns_per_put", measured.subject_ns),
        Metric::reported(PUT, "reference_ns_per_put", measured.reference_ns),
    ]);
    Ok(metrics)
}

fn get_values(db: &Database, gets: u64) -> Result<usize, String> {
    let mut folded = 0;
    for round in 0..gets {
        let value = db
            .get(key(round))
            .map_err(failed("get"))?
            .ok_or("a stored key was not found")?;
        folded ^= value.len() ^ usize::from(value[round as usize % VALUE_BYTES]);
    }
    Ok(folded)
}

fn copy_values(values: &[Vec<u8>], copies: u64) -> usize {
    let mut folded = 0;
    for round in 0..copies {
        let value = black_box(&values[(round % KEYS) as usize]).clone();
        folded ^= value.len() ^ usize::from(value[round as usize % VALUE_BYTES]);
    }
    folded
}

pub(crate) fn get(allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let file = BenchFile::new("large-value-get")?;
    let db = database(&file.path)?;
    let values: Vec<Vec<u8>> = (0..KEYS).map(large_value).collect();
    db.write(|tx| {
        for (round, value) in values.iter().enumerate() {
            tx.put(key(round as u64), value)?;
        }
        Ok(())
    })
    .map_err(failed("fill"))?;
    black_box(get_values(&db, KEYS)?);
    let allocations = allocator.count();
    black_box(get_values(&db, COUNTED_GETS)?);
    let allocations_per_get = (allocator.count() - allocations) as f64 / COUNTED_GETS as f64;
    let measured = measure::paired(
        &mut |iterations| {
            black_box(get_values(&db, iterations)?);
            Ok(())
        },
        &mut |iterations| {
            black_box(copy_values(&values, iterations));
            Ok(())
        },
    )?;
    Ok(vec![
        Metric::exact(GET, "allocations_per_get", allocations_per_get),
        Metric::ratio(GET, "ratio_to_copying_the_value", measured.ratio),
        Metric::reported(GET, "ns_per_get", measured.subject_ns),
        Metric::reported(GET, "reference_ns_per_get", measured.reference_ns),
    ])
}
