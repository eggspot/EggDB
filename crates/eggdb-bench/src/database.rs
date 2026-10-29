use crate::allocations::CountingAllocator;
use crate::baseline::Metric;
use crate::bench_file::BenchFile;
use crate::entries::{key, value};
use crate::measure;
use eggdb::{Database, Durability, Options};
use eggdb_platform::{OsFile, OsFileSystem, OsMemory, SystemClock};
use eggdb_storage::{
    Access, BTree, CacheSize, CreateOptions, PageCache, PageFile, StorageError, Store,
};
use std::hint::black_box;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const READ_ENTRIES: u64 = 100_000;
const COUNTED_READS: u64 = 100_000;
const WRITE_ENTRIES: u64 = 1000;
const COUNTED_COMMITS: u64 = 1000;
const CACHE_BYTES: usize = 128 << 20;
const OPEN_CACHE_BYTES: usize = 16 << 20;

pub(crate) const READ: &str = "database.read";
pub(crate) const WRITE: &str = "database.write";
pub(crate) const OPEN: &str = "database.open";

pub(crate) fn failed<E: std::fmt::Display>(what: &str) -> impl Fn(E) -> String + '_ {
    move |error| format!("{what}: {error}")
}

fn database_with(path: &Path, entries: u64, durability: Durability) -> Result<Database, String> {
    let options = Options::new()
        .cache_bytes(CACHE_BYTES)
        .durability(durability);
    let db = Database::open_with(path, options).map_err(failed("open"))?;
    db.write(|tx| {
        for index in 0..entries {
            tx.put(key(index), value(index))?;
        }
        Ok(())
    })
    .map_err(failed("fill"))?;
    Ok(db)
}

fn store_at(
    path: &Path,
    access: Access,
    cache_bytes: usize,
) -> Result<Store<OsFile>, StorageError> {
    let fs = OsFileSystem::new();
    let file = match access {
        Access::ReadWrite => PageFile::create(&fs, path, CreateOptions::new())?,
        Access::ReadOnly => PageFile::open(&fs, path, access)?,
    };
    let cache = PageCache::new(file, &OsMemory::new(), CacheSize::Bytes(cache_bytes))?;
    Ok(Store::new(cache, Arc::new(SystemClock::new())))
}

fn store_with(
    path: &Path,
    entries: u64,
    durability: eggdb_storage::Durability,
) -> Result<(Store<OsFile>, BTree), String> {
    let store = store_at(path, Access::ReadWrite, CACHE_BYTES).map_err(failed("create"))?;
    store
        .set_durability(durability)
        .map_err(failed("durability"))?;
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

fn read_from_database(db: &Database, iterations: u64) -> Result<usize, String> {
    let mut folded = 0;
    for index in 0..iterations {
        let tx = db.begin_read().map_err(failed("begin read"))?;
        let found = tx
            .get(key(index % READ_ENTRIES))
            .map_err(failed("get"))?
            .ok_or("a stored key was not found")?;
        folded ^= found.len();
    }
    Ok(folded)
}

fn read_from_store(store: &Store<OsFile>, tree: &BTree, iterations: u64) -> Result<usize, String> {
    let mut folded = 0;
    for index in 0..iterations {
        let read = store.begin_read().map_err(failed("begin read"))?;
        let found = read
            .get(tree, &key(index % READ_ENTRIES))
            .map_err(failed("get"))?
            .ok_or("a stored key was not found")?;
        folded ^= found.len();
    }
    Ok(folded)
}

pub(crate) fn read(allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let subject_file = BenchFile::new("database-read")?;
    let reference_file = BenchFile::new("database-read-reference")?;
    let db = database_with(&subject_file.path, READ_ENTRIES, Durability::Off)?;
    let (store, tree) = store_with(
        &reference_file.path,
        READ_ENTRIES,
        eggdb_storage::Durability::Off,
    )?;
    black_box(read_from_database(&db, READ_ENTRIES)?);
    black_box(read_from_store(&store, &tree, READ_ENTRIES)?);
    let allocations = allocator.count();
    black_box(read_from_database(&db, COUNTED_READS)?);
    let allocations_per_op = (allocator.count() - allocations) as f64 / COUNTED_READS as f64;
    let measured = measure::paired(
        &mut |iterations| {
            black_box(read_from_database(&db, iterations)?);
            Ok(())
        },
        &mut |iterations| {
            black_box(read_from_store(&store, &tree, iterations)?);
            Ok(())
        },
    )?;
    Ok(vec![
        Metric::exact(READ, "allocations_per_op", allocations_per_op),
        Metric::ratio(READ, "ratio_to_store_read", measured.ratio),
        Metric::reported(READ, "ns_per_op", measured.subject_ns),
        Metric::reported(READ, "reference_ns_per_op", measured.reference_ns),
    ])
}

fn commit_to_database(db: &Database, commits: u64) -> Result<(), String> {
    for round in 0..commits {
        let mut tx = db.begin_write().map_err(failed("begin write"))?;
        tx.put(key(round % WRITE_ENTRIES), value(round))
            .map_err(failed("put"))?;
        tx.commit().map_err(failed("commit"))?;
    }
    Ok(())
}

fn commit_to_store(store: &Store<OsFile>, tree: &BTree, commits: u64) -> Result<(), String> {
    for round in 0..commits {
        let mut write = store.begin_write().map_err(failed("begin write"))?;
        write
            .put(tree, &key(round % WRITE_ENTRIES), &value(round))
            .map_err(failed("put"))?;
        write.commit().map_err(failed("commit"))?;
    }
    Ok(())
}

pub(crate) fn write(allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let subject_file = BenchFile::new("database-write")?;
    let reference_file = BenchFile::new("database-write-reference")?;
    let db = database_with(&subject_file.path, WRITE_ENTRIES, Durability::Off)?;
    let (store, tree) = store_with(
        &reference_file.path,
        WRITE_ENTRIES,
        eggdb_storage::Durability::Off,
    )?;
    commit_to_database(&db, COUNTED_COMMITS)?;
    commit_to_store(&store, &tree, COUNTED_COMMITS)?;
    let allocations = allocator.count();
    commit_to_database(&db, COUNTED_COMMITS)?;
    let allocations = (allocator.count() - allocations) as f64;
    let measured = measure::paired(
        &mut |iterations| commit_to_database(&db, iterations),
        &mut |iterations| commit_to_store(&store, &tree, iterations),
    )?;
    Ok(vec![
        Metric::reported(WRITE, "allocations_per_1000_commits", allocations),
        Metric::ratio(WRITE, "ratio_to_store_commit", measured.ratio),
        Metric::reported(WRITE, "ns_per_commit", measured.subject_ns),
        Metric::reported(WRITE, "reference_ns_per_commit", measured.reference_ns),
    ])
}

fn open_database(path: &Path, iterations: u64) -> Result<(), String> {
    let options = Options::new().read_only(true).cache_bytes(OPEN_CACHE_BYTES);
    for _ in 0..iterations {
        let db = Database::open_with(path, options.clone()).map_err(failed("open"))?;
        black_box(&db);
    }
    Ok(())
}

fn open_store(path: &Path, iterations: u64) -> Result<(), String> {
    for _ in 0..iterations {
        let store = store_at(path, Access::ReadOnly, OPEN_CACHE_BYTES).map_err(failed("open"))?;
        black_box(&store);
    }
    Ok(())
}

pub(crate) fn open(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let file = BenchFile::new("database-open")?;
    let db = database_with(&file.path, WRITE_ENTRIES, Durability::Full)?;
    db.close().map_err(failed("close"))?;
    let measured = measure::paired(
        &mut |iterations| open_database(&file.path, iterations),
        &mut |iterations| open_store(&file.path, iterations),
    )?;
    Ok(vec![
        Metric::ratio(OPEN, "ratio_to_store_open", measured.ratio),
        Metric::reported(OPEN, "ns_per_open", measured.subject_ns),
        Metric::reported(OPEN, "reference_ns_per_open", measured.reference_ns),
    ])
}

pub(crate) const READ_EXPIRING: &str = "database.read_expiring";
pub(crate) const INCREMENT: &str = "database.increment";
const LONG_TIME_TO_LIVE: std::time::Duration = std::time::Duration::from_secs(86_400);

fn expiring_database_with(path: &Path, entries: u64) -> Result<Database, String> {
    let options = Options::new()
        .cache_bytes(CACHE_BYTES)
        .durability(Durability::Off);
    let db = Database::open_with(path, options).map_err(failed("open"))?;
    db.write(|tx| {
        for index in 0..entries {
            tx.put_with_expiry(key(index), value(index), LONG_TIME_TO_LIVE)?;
        }
        Ok(())
    })
    .map_err(failed("fill"))?;
    Ok(db)
}

pub(crate) fn read_expiring(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let subject_file = BenchFile::new("database-read-expiring")?;
    let reference_file = BenchFile::new("database-read-expiring-reference")?;
    let expiring = expiring_database_with(&subject_file.path, READ_ENTRIES)?;
    let plain = database_with(&reference_file.path, READ_ENTRIES, Durability::Off)?;
    black_box(read_from_database(&expiring, READ_ENTRIES)?);
    black_box(read_from_database(&plain, READ_ENTRIES)?);
    let measured = measure::paired(
        &mut |iterations| {
            black_box(read_from_database(&expiring, iterations)?);
            Ok(())
        },
        &mut |iterations| {
            black_box(read_from_database(&plain, iterations)?);
            Ok(())
        },
    )?;
    Ok(vec![
        Metric::ratio(READ_EXPIRING, "ratio_to_plain_read", measured.ratio),
        Metric::reported(READ_EXPIRING, "ns_per_op", measured.subject_ns),
        Metric::reported(READ_EXPIRING, "reference_ns_per_op", measured.reference_ns),
    ])
}

fn increment_in_database(db: &Database, commits: u64) -> Result<(), String> {
    for round in 0..commits {
        db.increment(key(round % WRITE_ENTRIES), 1)
            .map_err(failed("increment"))?;
    }
    Ok(())
}

pub(crate) fn increment(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let subject_file = BenchFile::new("database-increment")?;
    let reference_file = BenchFile::new("database-increment-reference")?;
    let counters = Database::open_with(
        &subject_file.path,
        Options::new()
            .cache_bytes(CACHE_BYTES)
            .durability(Durability::Off),
    )
    .map_err(failed("open"))?;
    let plain = database_with(&reference_file.path, WRITE_ENTRIES, Durability::Off)?;
    increment_in_database(&counters, COUNTED_COMMITS)?;
    commit_to_database(&plain, COUNTED_COMMITS)?;
    let measured = measure::paired(
        &mut |iterations| increment_in_database(&counters, iterations),
        &mut |iterations| commit_to_database(&plain, iterations),
    )?;
    Ok(vec![
        Metric::ratio(INCREMENT, "ratio_to_put_commit", measured.ratio),
        Metric::reported(INCREMENT, "ns_per_increment", measured.subject_ns),
        Metric::reported(INCREMENT, "reference_ns_per_commit", measured.reference_ns),
    ])
}

pub(crate) const BACKUP: &str = "database.backup";
const BACKUP_ENTRIES: u64 = 10_000;

fn per_entry<'a>(
    round: &'a mut dyn FnMut() -> Result<Duration, String>,
) -> impl FnMut(u64) -> Result<Duration, String> + 'a {
    move |entries| {
        let rounds = entries.div_ceil(BACKUP_ENTRIES).max(1);
        let mut elapsed = Duration::ZERO;
        for _ in 0..rounds {
            elapsed += round()?;
        }
        Ok(elapsed.mul_f64(entries as f64 / (rounds * BACKUP_ENTRIES) as f64))
    }
}

fn backup_once(db: &Database, copy: &Path) -> Result<Duration, String> {
    let start = Instant::now();
    db.backup_to(copy).map_err(failed("backup"))?;
    let elapsed = start.elapsed();
    std::fs::remove_file(copy).map_err(failed("remove the backup"))?;
    Ok(elapsed)
}

fn fill_once(path: &Path) -> Result<Duration, String> {
    let start = Instant::now();
    let options = Options::new()
        .cache_bytes(CACHE_BYTES)
        .durability(Durability::Normal);
    let db = Database::open_with(path, options).map_err(failed("open"))?;
    db.write(|tx| {
        for index in 0..BACKUP_ENTRIES {
            tx.put(key(index), value(index))?;
        }
        Ok(())
    })
    .map_err(failed("fill"))?;
    db.close().map_err(failed("close"))?;
    let elapsed = start.elapsed();
    std::fs::remove_file(path).map_err(failed("remove the filled file"))?;
    Ok(elapsed)
}

pub(crate) fn backup(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let source_file = BenchFile::new("database-backup")?;
    let copy_file = BenchFile::new("database-backup-copy")?;
    let reference_file = BenchFile::new("database-backup-reference")?;
    let source = database_with(&source_file.path, BACKUP_ENTRIES, Durability::Off)?;
    let mut backup_round = || backup_once(&source, &copy_file.path);
    let mut fill_round = || fill_once(&reference_file.path);
    let measured = measure::paired_timed(
        &mut per_entry(&mut backup_round),
        &mut per_entry(&mut fill_round),
    )?;
    Ok(vec![
        Metric::ratio(BACKUP, "ratio_to_fill", measured.ratio),
        Metric::reported(BACKUP, "ns_per_entry", measured.subject_ns),
        Metric::reported(BACKUP, "reference_ns_per_entry", measured.reference_ns),
    ])
}
