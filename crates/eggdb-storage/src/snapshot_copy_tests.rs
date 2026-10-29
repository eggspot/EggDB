use crate::btree::BTree;
use crate::crash_sweep_tests::{file_bytes, file_system_holding};
use crate::cursor::ScanOrder;
use crate::error::{FormatError, StorageError};
use crate::format::{Access, Features, FileId, NO_PAGE, PageSize};
use crate::page_cache::{CacheSize, PageCache};
use crate::page_file::{CreateOptions, PageFile};
use crate::snapshot_copy::{CopyDestination, CopyFailure, CopyReport};
use crate::store::{ReadTransaction, Store};
use eggdb_platform::{
    CrashMode, File, FileSystem, OpenMode, SimClock, SimFile, SimFileSystem, SimMemory,
};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

const DB: &str = "db";
const COPY: &str = "copy";
const COPY_STAGING: &str = "copy.backing-up";
const DB_STAGING: &str = "db.compacting";
const FRAMES: usize = 1024;
const OCTOBER_2026_NANOS: u64 = 1_792_000_000_000_000_000;

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    Reopen,
    PowerLoss(CrashMode),
}

const ENDINGS: [Ending; 4] = [
    Ending::Reopen,
    Ending::PowerLoss(CrashMode::DropUnsynced),
    Ending::PowerLoss(CrashMode::Torn { seed: 1 }),
    Ending::PowerLoss(CrashMode::Torn { seed: 2 }),
];

fn clock() -> Arc<SimClock> {
    Arc::new(SimClock::new(OCTOBER_2026_NANOS))
}

fn created(fs: &SimFileSystem, clock: &Arc<SimClock>) -> (Store<SimFile>, BTree) {
    let options = CreateOptions {
        page_size: PageSize::DEFAULT,
        file_id: FileId(*b"copy-tests-file!"),
    };
    let file = PageFile::create(fs, Path::new(DB), options).unwrap();
    let cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(FRAMES)).unwrap();
    let store = Store::new(cache, clock.clone());
    let mut write = store.begin_write().unwrap();
    let tree = write.create_tree().unwrap();
    write.set_catalog_root(tree.root());
    write.commit().unwrap();
    (store, tree)
}

fn opened(fs: &SimFileSystem, path: &str, access: Access, clock: &Arc<SimClock>) -> Store<SimFile> {
    let file = PageFile::open(fs, Path::new(path), access).unwrap();
    let cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(FRAMES)).unwrap();
    Store::new(cache, clock.clone())
}

fn key(index: usize) -> Vec<u8> {
    format!("key{index:05}").into_bytes()
}

fn value(index: usize) -> Vec<u8> {
    let len = if index % 37 == 0 {
        3 * 4096 + index
    } else {
        20 + index % 90
    };
    (0..len).map(|byte| (byte + index) as u8).collect()
}

fn put_all(
    store: &Store<SimFile>,
    tree: &BTree,
    model: &mut Model,
    indexes: impl Iterator<Item = usize>,
) {
    let mut write = store.begin_write().unwrap();
    for index in indexes {
        write.put(tree, &key(index), &value(index)).unwrap();
        model.insert(key(index), value(index));
    }
    write.commit().unwrap();
}

fn delete_all(
    store: &Store<SimFile>,
    tree: &BTree,
    model: &mut Model,
    indexes: impl Iterator<Item = usize>,
) {
    let mut write = store.begin_write().unwrap();
    for index in indexes {
        write.delete(tree, &key(index)).unwrap();
        model.remove(&key(index));
    }
    write.commit().unwrap();
}

fn read_all(read: &ReadTransaction<'_, SimFile>) -> Model {
    let mut found = Model::new();
    if read.catalog_root() == NO_PAGE {
        return found;
    }
    let tree = read.open_tree(read.catalog_root()).unwrap();
    let mut cursor = tree.scan(Bound::Unbounded, Bound::Unbounded, ScanOrder::Ascending);
    while let Some((key, value)) = read.next(&mut cursor).unwrap() {
        found.insert(key.to_vec(), value.to_vec());
    }
    found
}

fn contents(fs: &SimFileSystem, path: &str, clock: &Arc<SimClock>) -> Model {
    let store = opened(fs, path, Access::ReadOnly, clock);
    let read = store.begin_read().unwrap();
    read_all(&read)
}

fn copy(
    read: &ReadTransaction<'_, SimFile>,
    fs: &SimFileSystem,
    clock: &Arc<SimClock>,
    destination: CopyDestination<'_>,
) -> Result<CopyReport, CopyFailure> {
    read.copy_to(fs, &SimMemory::new(), clock.clone(), destination)
}

fn backup_destination() -> CopyDestination<'static> {
    CopyDestination {
        path: Path::new(COPY),
        staging: Path::new(COPY_STAGING),
        replaces: false,
    }
}

fn compaction_destination() -> CopyDestination<'static> {
    CopyDestination {
        path: Path::new(DB),
        staging: Path::new(DB_STAGING),
        replaces: true,
    }
}

fn raw_contents(fs: &SimFileSystem, path: &str) -> Vec<u8> {
    let file = fs.open(Path::new(path), OpenMode::Read).unwrap();
    let mut bytes = vec![0u8; file.size().unwrap() as usize];
    file.read_exact_at(&mut bytes, 0).unwrap();
    bytes
}

fn write_raw(fs: &SimFileSystem, path: &str, bytes: &[u8]) {
    let file = fs.open(Path::new(path), OpenMode::CreateNew).unwrap();
    file.write_all_at(bytes, 0).unwrap();
    file.sync_all().unwrap();
}

fn expiring_keys() -> Features {
    Features {
        read_only_compatible: Features::EXPIRING_KEYS,
        ..Features::default()
    }
}

#[test]
fn a_copy_holds_the_snapshot_and_none_of_the_commits_after_it() {
    let fs = SimFileSystem::new();
    let clock = clock();
    let (store, tree) = created(&fs, &clock);
    let mut model = Model::new();
    put_all(&store, &tree, &mut model, 0..300);
    let mut write = store.begin_write().unwrap();
    write.add_features(expiring_keys());
    write.commit().unwrap();
    let snapshot = model.clone();
    let read = store.begin_read().unwrap();
    delete_all(&store, &tree, &mut model, (0..300).step_by(2));
    put_all(&store, &tree, &mut model, 300..400);

    let report = copy(&read, &fs, &clock, backup_destination()).unwrap();

    assert_eq!(report.stamp, read.stamp());
    assert_eq!(report.entries, snapshot.len() as u64);
    assert_eq!(
        report.key_bytes,
        snapshot.keys().map(|key| key.len() as u64).sum::<u64>()
    );
    assert_eq!(
        report.value_bytes,
        snapshot
            .values()
            .map(|value| value.len() as u64)
            .sum::<u64>()
    );
    drop(read);
    assert!(!fs.exists(Path::new(COPY_STAGING)).unwrap());
    let copied = opened(&fs, COPY, Access::ReadOnly, &clock);
    let superblock = copied.superblock().unwrap();
    assert_eq!(superblock.features, expiring_keys());
    assert!(superblock.clock_high_water > report.stamp.bits());
    assert_ne!(superblock.file_id, store.superblock().unwrap().file_id);
    let checked = copied.check().unwrap();
    assert_eq!(checked.leaked_pages, 0);
    assert_eq!(checked.page_count, report.page_count);
    assert_eq!(read_all(&copied.begin_read().unwrap()), snapshot);
    assert_eq!(read_all(&store.begin_read().unwrap()), model);
}

#[test]
fn a_compacted_file_holds_the_same_entries_in_fewer_pages() {
    let fs = SimFileSystem::new();
    let clock = clock();
    let (store, tree) = created(&fs, &clock);
    let mut model = Model::new();
    put_all(&store, &tree, &mut model, 0..2000);
    delete_all(
        &store,
        &tree,
        &mut model,
        (0..2000).filter(|index| index % 10 != 0),
    );
    store.checkpoint().unwrap();
    let before = store.superblock().unwrap();
    assert!(before.free_page_count > 0, "{before:?}");
    drop(store);

    let source = opened(&fs, DB, Access::ReadOnly, &clock);
    let read = source.begin_read().unwrap();
    let report = copy(&read, &fs, &clock, compaction_destination()).unwrap();
    assert_eq!(read_all(&read), model);
    drop(read);
    drop(source);

    assert!(
        report.page_count < before.page_count,
        "{report:?} {before:?}"
    );
    assert!(!fs.exists(Path::new(DB_STAGING)).unwrap());
    let compacted = opened(&fs, DB, Access::ReadWrite, &clock);
    assert_eq!(
        compacted.superblock().unwrap().page_count,
        report.page_count
    );
    assert_eq!(compacted.check().unwrap().leaked_pages, 0);
    assert_eq!(read_all(&compacted.begin_read().unwrap()), model);
    let tree = compacted
        .begin_read()
        .unwrap()
        .open_tree(compacted.superblock().unwrap().catalog_root)
        .unwrap();
    put_all(&compacted, &tree, &mut model, 5000..5010);
    assert_eq!(read_all(&compacted.begin_read().unwrap()), model);
}

#[test]
fn a_copy_never_replaces_a_file_and_leaves_a_staging_file_it_did_not_make() {
    let fs = SimFileSystem::new();
    let clock = clock();
    let (store, tree) = created(&fs, &clock);
    put_all(&store, &tree, &mut Model::new(), 0..10);
    let read = store.begin_read().unwrap();

    write_raw(&fs, COPY, b"someone's file");
    let refused = copy(&read, &fs, &clock, backup_destination()).unwrap_err();
    assert!(matches!(
        &refused,
        CopyFailure::NotCopied(StorageError::Io(error))
            if error.kind() == eggdb_platform::ErrorKind::AlreadyExists
    ));
    assert_eq!(raw_contents(&fs, COPY), b"someone's file");
    assert!(!fs.exists(Path::new(COPY_STAGING)).unwrap());

    fs.remove_file(Path::new(COPY)).unwrap();
    write_raw(&fs, COPY_STAGING, b"a copy still running");
    let refused = copy(&read, &fs, &clock, backup_destination()).unwrap_err();
    assert!(matches!(
        &refused,
        CopyFailure::NotCopied(StorageError::Io(error))
            if error.kind() == eggdb_platform::ErrorKind::AlreadyExists
    ));
    assert_eq!(raw_contents(&fs, COPY_STAGING), b"a copy still running");
    assert!(!fs.exists(Path::new(COPY)).unwrap());
}

#[test]
fn a_copy_of_a_file_without_a_tree_is_an_empty_database() {
    let fs = SimFileSystem::new();
    let clock = clock();
    let file = PageFile::create(&fs, Path::new(DB), CreateOptions::new()).unwrap();
    let cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(FRAMES)).unwrap();
    let store = Store::new(cache, clock.clone());
    let read = store.begin_read().unwrap();

    let report = copy(&read, &fs, &clock, backup_destination()).unwrap();

    assert_eq!(report.entries, 0);
    let copied = opened(&fs, COPY, Access::ReadWrite, &clock);
    assert_eq!(copied.superblock().unwrap().catalog_root, NO_PAGE);
    assert_eq!(copied.check().unwrap().leaked_pages, 0);
}

#[test]
fn a_snapshot_with_features_this_release_may_only_read_is_not_copied() {
    let fs = SimFileSystem::new();
    let clock = clock();
    let (store, tree) = created(&fs, &clock);
    put_all(&store, &tree, &mut Model::new(), 0..10);
    let mut write = store.begin_write().unwrap();
    write.add_features(Features {
        read_only_compatible: 1 << 40,
        ..Features::default()
    });
    write.commit().unwrap();
    let read = store.begin_read().unwrap();

    let refused = copy(&read, &fs, &clock, backup_destination()).unwrap_err();

    assert!(matches!(
        refused,
        CopyFailure::NotCopied(StorageError::Format(FormatError::ReadOnlyFeatures { bits }))
            if bits == 1 << 40
    ));
    assert!(!fs.exists(Path::new(COPY)).unwrap());
    assert!(!fs.exists(Path::new(COPY_STAGING)).unwrap());
}

struct Source {
    bytes: Vec<u8>,
    model: Model,
}

fn source_with_free_pages() -> Source {
    let fs = SimFileSystem::new();
    let clock = clock();
    let (store, tree) = created(&fs, &clock);
    let mut model = Model::new();
    put_all(&store, &tree, &mut model, 0..400);
    let mut write = store.begin_write().unwrap();
    write.add_features(expiring_keys());
    write.commit().unwrap();
    delete_all(
        &store,
        &tree,
        &mut model,
        (0..400).filter(|index| index % 3 != 0),
    );
    put_all(&store, &tree, &mut model, 400..420);
    store.checkpoint().unwrap();
    drop(store);
    Source {
        bytes: file_bytes(&fs),
        model,
    }
}

fn copy_after_fault(
    source: &Source,
    destination: CopyDestination<'_>,
    fault_at: Option<u64>,
) -> (SimFileSystem, Result<CopyReport, CopyFailure>, u64) {
    let fs = file_system_holding(&source.bytes);
    let clock = clock();
    let store = opened(&fs, DB, Access::ReadOnly, &clock);
    let read = store.begin_read().unwrap();
    let start = fs.operation_count();
    if let Some(step) = fault_at {
        fs.fail_operation(start + step);
    }
    let copied = copy(&read, &fs, &clock, destination);
    let operations = fs.operation_count() - start;
    drop(read);
    drop(store);
    (fs, copied, operations)
}

fn end(fs: &SimFileSystem, ending: Ending) {
    if let Ending::PowerLoss(mode) = ending {
        fs.crash(mode);
    }
}

fn assert_whole_database(fs: &SimFileSystem, path: &str, model: &Model, context: &str) -> u64 {
    let clock = clock();
    let file = PageFile::open(fs, Path::new(path), Access::ReadOnly)
        .unwrap_or_else(|error| panic!("{context}: {path} does not open: {error}"));
    let cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(FRAMES)).unwrap();
    let store = Store::new(cache, clock);
    let checked = store
        .check()
        .unwrap_or_else(|error| panic!("{context}: {path} fails its check: {error}"));
    assert_eq!(checked.leaked_pages, 0, "{context}: {path} leaks pages");
    assert_eq!(
        store.superblock().unwrap().features,
        expiring_keys(),
        "{context}: {path} lost its feature flags"
    );
    let found = read_all(&store.begin_read().unwrap());
    assert!(
        found == *model,
        "{context}: {path} does not hold the snapshot"
    );
    checked.page_count
}

#[test]
fn a_crash_at_any_operation_of_a_compaction_leaves_the_old_file_or_the_compacted_one() {
    let source = source_with_free_pages();
    let (fs, clean, operations) = copy_after_fault(&source, compaction_destination(), None);
    let compacted = clean.unwrap();
    let original = file_system_holding(&source.bytes);
    let original_page_count = assert_whole_database(&original, DB, &source.model, "original");
    assert!(compacted.page_count < original_page_count, "{compacted:?}");
    end(&fs, Ending::PowerLoss(CrashMode::Torn { seed: 7 }));
    assert_eq!(
        assert_whole_database(&fs, DB, &source.model, "clean run"),
        compacted.page_count
    );

    let mut refused = 0;
    let mut renames_refused = 0;
    for step in 0..operations {
        let ending = ENDINGS[step as usize % ENDINGS.len()];
        let context = format!("fault at operation {step} of {operations}, then {ending:?}");
        let (fs, copied, _) = copy_after_fault(&source, compaction_destination(), Some(step));
        refused += u64::from(copied.is_err());
        if ending == Ending::Reopen && matches!(copied, Err(CopyFailure::NotCopied(_))) {
            assert!(
                !fs.exists(Path::new(DB_STAGING)).unwrap(),
                "{context}: the staging file was left behind"
            );
        }
        if ending == Ending::Reopen
            && let Err(CopyFailure::NotRenamed(_)) = &copied
        {
            renames_refused += 1;
            assert_whole_database(
                &fs,
                DB_STAGING,
                &source.model,
                &format!("{context}, the kept copy"),
            );
        }
        end(&fs, ending);
        let page_count = assert_whole_database(&fs, DB, &source.model, &context);
        assert!(
            page_count == original_page_count || page_count == compacted.page_count,
            "{context}: {page_count} pages, neither the original's nor the compacted file's"
        );
    }
    assert!(
        refused > operations / 2,
        "{refused} of {operations} faults were reported"
    );
    assert!(renames_refused <= 1, "{renames_refused} refused renames");
}

#[test]
fn a_crash_at_any_operation_of_a_backup_leaves_no_copy_or_the_whole_copy() {
    let source = source_with_free_pages();
    let (fs, clean, operations) = copy_after_fault(&source, backup_destination(), None);
    clean.unwrap();
    end(&fs, Ending::PowerLoss(CrashMode::Torn { seed: 7 }));
    assert_whole_database(&fs, COPY, &source.model, "clean run");

    let mut refused = 0;
    let mut renames_refused = 0;
    for step in 0..operations {
        let ending = ENDINGS[step as usize % ENDINGS.len()];
        let context = format!("fault at operation {step} of {operations}, then {ending:?}");
        let (fs, copied, _) = copy_after_fault(&source, backup_destination(), Some(step));
        refused += u64::from(copied.is_err());
        if ending == Ending::Reopen && matches!(copied, Err(CopyFailure::NotCopied(_))) {
            assert!(
                !fs.exists(Path::new(COPY_STAGING)).unwrap(),
                "{context}: the staging file was left behind"
            );
        }
        if ending == Ending::Reopen
            && let Err(CopyFailure::NotRenamed(_)) = &copied
        {
            renames_refused += 1;
            assert_whole_database(
                &fs,
                COPY_STAGING,
                &source.model,
                &format!("{context}, the kept copy"),
            );
        }
        end(&fs, ending);
        assert_whole_database(&fs, DB, &source.model, &format!("{context}, the source"));
        if fs.exists(Path::new(COPY)).unwrap() {
            assert_whole_database(&fs, COPY, &source.model, &format!("{context}, the copy"));
        }
    }
    assert!(
        refused > operations / 2,
        "{refused} of {operations} faults were reported"
    );
    assert!(renames_refused <= 1, "{renames_refused} refused renames");
}
