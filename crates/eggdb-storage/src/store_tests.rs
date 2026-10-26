use crate::btree::BTree;
use crate::cursor::ScanOrder;
use crate::error::StorageError;
use crate::format::{Access, FileId, PageSize};
use crate::hybrid_clock::CommitStamp;
use crate::log::Durability;
use crate::page_cache::{CacheSize, PageCache};
use crate::page_file::{CreateOptions, PageFile};
use crate::store::{ReadTransaction, Store, WriteTransaction};
use crate::test_rng::TestRng;
use eggdb_platform::{CrashMode, SimClock, SimFile, SimFileSystem, SimMemory};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const DB: &str = "db";
const OCTOBER_2026_NANOS: u64 = 1_792_000_000_000_000_000;
const HOUR: Duration = Duration::from_secs(3600);

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

fn cache(fs: &SimFileSystem, frames: usize) -> PageCache<SimFile> {
    let file = PageFile::open(fs, Path::new(DB), Access::ReadWrite).unwrap();
    PageCache::new(file, &SimMemory::new(), CacheSize::Pages(frames)).unwrap()
}

fn created(fs: &SimFileSystem, frames: usize, clock: &Arc<SimClock>) -> (Store<SimFile>, BTree) {
    let options = CreateOptions {
        page_size: PageSize::DEFAULT,
        file_id: FileId(*b"store-tests-file"),
    };
    let file = PageFile::create(fs, Path::new(DB), options).unwrap();
    let cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(frames)).unwrap();
    let store = Store::new(cache, clock.clone());
    let mut write = store.begin_write().unwrap();
    let tree = write.create_tree().unwrap();
    write.set_catalog_root(tree.root());
    write.commit().unwrap();
    (store, tree)
}

fn reopened(fs: &SimFileSystem, frames: usize, clock: &Arc<SimClock>) -> (Store<SimFile>, BTree) {
    let store = Store::new(cache(fs, frames), clock.clone());
    let root = store.superblock().unwrap().catalog_root;
    let tree = store.begin_read().unwrap().open_tree(root).unwrap();
    (store, tree)
}

fn october_clock() -> Arc<SimClock> {
    Arc::new(SimClock::new(OCTOBER_2026_NANOS))
}

fn key(index: usize) -> Vec<u8> {
    format!("key{index:05}").into_bytes()
}

fn read_all(read: &ReadTransaction<'_, SimFile>, tree: &BTree, order: ScanOrder) -> Model {
    let mut cursor = tree.scan(Bound::Unbounded, Bound::Unbounded, order);
    let mut found = Model::new();
    let mut previous: Option<Vec<u8>> = None;
    while let Some((key, value)) = read.next(&mut cursor).unwrap() {
        if let Some(previous) = &previous {
            match order {
                ScanOrder::Ascending => assert!(previous.as_slice() < key),
                ScanOrder::Descending => assert!(previous.as_slice() > key),
            }
        }
        previous = Some(key.to_vec());
        found.insert(key.to_vec(), value.to_vec());
    }
    found
}

fn written_all(write: &WriteTransaction<'_, SimFile>, tree: &BTree) -> Model {
    let mut cursor = tree.scan(Bound::Unbounded, Bound::Unbounded, ScanOrder::Ascending);
    let mut found = Model::new();
    while let Some((key, value)) = write.next(&mut cursor).unwrap() {
        found.insert(key.to_vec(), value.to_vec());
    }
    found
}

fn committed(store: &Store<SimFile>, tree: &BTree) -> Model {
    let read = store.begin_read().unwrap();
    read_all(&read, tree, ScanOrder::Ascending)
}

fn fill(store: &Store<SimFile>, tree: &BTree, entries: usize, value_len: usize) -> Model {
    let mut write = store.begin_write().unwrap();
    let mut model = Model::new();
    for index in 0..entries {
        let value = vec![index as u8; value_len];
        write.put(tree, &key(index), &value).unwrap();
        model.insert(key(index), value);
    }
    write.commit().unwrap();
    model
}

fn assert_tree_checks(store: &Store<SimFile>, tree: &BTree, model: &Model) {
    let report = store.with_cache(|cache| tree.check(cache)).unwrap();
    assert_eq!(report.entries, model.len() as u64);
}

#[test]
fn a_second_writer_is_refused_until_the_first_ends() {
    let fs = SimFileSystem::new();
    let (store, tree) = created(&fs, 32, &october_clock());
    let mut first = store.begin_write().unwrap();
    assert!(matches!(
        store.begin_write(),
        Err(StorageError::WriterActive)
    ));
    first.put(&tree, b"a", b"1").unwrap();
    first.commit().unwrap();
    let second = store.begin_write().unwrap();
    assert!(matches!(
        store.begin_write(),
        Err(StorageError::WriterActive)
    ));
    second.rollback().unwrap();
    drop(store.begin_write().unwrap());
    let third = store.begin_write().unwrap();
    assert!(
        StorageError::WriterActive
            .to_string()
            .contains("one writer at a time")
    );
    drop(third);
    assert_eq!(committed(&store, &tree).len(), 1);
}

#[test]
fn a_rollback_or_a_drop_forgets_every_change_the_transaction_saw() {
    let fs = SimFileSystem::new();
    let (store, tree) = created(&fs, 32, &october_clock());
    let model = fill(&store, &tree, 300, 40);
    let last = store.last_commit().unwrap();
    for explicit in [true, false] {
        let mut write = store.begin_write().unwrap();
        for index in 0..300 {
            if index % 2 == 0 {
                assert!(write.delete(&tree, &key(index)).unwrap());
            } else {
                write.put(&tree, &key(index), b"changed").unwrap();
            }
        }
        write.put(&tree, b"new", b"entry").unwrap();
        assert_eq!(write.get(&tree, b"new").unwrap().unwrap(), b"entry");
        assert_eq!(write.get(&tree, &key(0)).unwrap(), None);
        assert_eq!(written_all(&write, &tree).len(), 151);
        if explicit {
            write.rollback().unwrap();
        } else {
            drop(write);
        }
        assert_eq!(committed(&store, &tree), model);
        assert_eq!(store.last_commit().unwrap(), last);
        assert_eq!(store.cache_stats().unwrap().dirty_pages, 0);
        assert_tree_checks(&store, &tree, &model);
    }
    drop(store);
    let (store, tree) = reopened(&fs, 32, &october_clock());
    assert_eq!(committed(&store, &tree), model);
}

#[test]
fn a_reader_sees_the_last_commit_before_it_began_while_the_writer_commits() {
    let fs = SimFileSystem::new();
    let (store, tree) = created(&fs, 64, &october_clock());
    let before = fill(&store, &tree, 400, 60);
    let reader = store.begin_read().unwrap();
    let stamp = reader.stamp();
    assert_eq!(stamp, store.last_commit().unwrap());
    let mut after = before.clone();
    for round in 0..5 {
        let mut write = store.begin_write().unwrap();
        for index in (round..400).step_by(3) {
            if index % 4 == 0 {
                write.delete(&tree, &key(index)).unwrap();
                after.remove(&key(index));
            } else {
                let value = vec![round as u8 + 100; 90];
                write.put(&tree, &key(index), &value).unwrap();
                after.insert(key(index), value);
            }
        }
        assert_eq!(read_all(&reader, &tree, ScanOrder::Ascending), before);
        let committed_stamp = write.commit().unwrap();
        assert!(committed_stamp > stamp);
        assert_eq!(read_all(&reader, &tree, ScanOrder::Descending), before);
        assert_eq!(
            reader.get(&tree, &key(4)).unwrap(),
            before.get(&key(4)).cloned()
        );
    }
    assert!(store.cache_stats().unwrap().before_images > 0);
    assert_eq!(reader.stamp(), stamp);
    assert_eq!(committed(&store, &tree), after);
    drop(reader);
    assert_eq!(store.cache_stats().unwrap().before_images, 0);
    assert_tree_checks(&store, &tree, &after);
}

#[test]
fn a_reader_begun_inside_a_write_transaction_sees_neither_its_changes_nor_its_commit() {
    let fs = SimFileSystem::new();
    let (store, tree) = created(&fs, 128, &october_clock());
    let before = fill(&store, &tree, 600, 50);
    let pages_before = store.superblock().unwrap().free_page_count;
    let mut write = store.begin_write().unwrap();
    for index in 0..560 {
        write.delete(&tree, &key(index)).unwrap();
    }
    write.put(&tree, b"zzz", b"late").unwrap();
    let reader = store.begin_read().unwrap();
    assert_eq!(read_all(&reader, &tree, ScanOrder::Ascending), before);
    write.commit().unwrap();
    assert!(store.superblock().unwrap().free_page_count > pages_before);
    let mut refill = store.begin_write().unwrap();
    for index in 0..200 {
        refill.put(&tree, &key(10_000 + index), &[7; 120]).unwrap();
    }
    refill.commit().unwrap();
    assert_eq!(read_all(&reader, &tree, ScanOrder::Ascending), before);
    assert_eq!(read_all(&reader, &tree, ScanOrder::Descending), before);
    let fresh = store.begin_read().unwrap();
    let now = read_all(&fresh, &tree, ScanOrder::Ascending);
    assert_eq!(now.len(), 40 + 1 + 200);
    assert_eq!(fresh.get(&tree, b"zzz").unwrap().unwrap(), b"late");
    assert_eq!(reader.get(&tree, b"zzz").unwrap(), None);
}

#[test]
fn readers_outlive_checkpoints_recycled_log_segments_and_reused_pages() {
    for durability in [Durability::Full, Durability::Normal] {
        let fs = SimFileSystem::new();
        let (store, tree) = created(&fs, 64, &october_clock());
        store.set_durability(durability).unwrap();
        store.set_auto_checkpoint_bytes(Some(32 * 1024)).unwrap();
        store
            .set_snapshot_memory(CacheSize::Bytes(4 << 20))
            .unwrap();
        let first = fill(&store, &tree, 300, 100);
        let reader = store.begin_read().unwrap();
        let mut rng = TestRng::new(7);
        for round in 0..150 {
            let mut write = store.begin_write().unwrap();
            for _ in 0..4 {
                let index = rng.below(300);
                if round % 3 == 0 {
                    write.delete(&tree, &key(index)).unwrap();
                } else {
                    write.put(&tree, &key(index), &rng.bytes(150)).unwrap();
                }
            }
            write.commit().unwrap();
            if round % 50 == 49 {
                store.checkpoint().unwrap();
            }
        }
        assert!(
            store.log_stats().unwrap().checkpoints >= 10,
            "{durability:?}"
        );
        assert_eq!(read_all(&reader, &tree, ScanOrder::Ascending), first);
        assert_eq!(read_all(&reader, &tree, ScanOrder::Descending), first);
    }
}

#[derive(Debug)]
enum Change {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}

fn random_change(rng: &mut TestRng) -> Change {
    let key = key(rng.below(500));
    if rng.below(3) == 0 {
        Change::Delete(key)
    } else {
        let len = if rng.below(10) == 0 {
            300 + rng.below(300)
        } else {
            rng.below(80)
        };
        Change::Put(key, rng.bytes(len))
    }
}

fn assert_reader_matches(
    reader: &ReadTransaction<'_, SimFile>,
    tree: &BTree,
    model: &Model,
    rng: &mut TestRng,
    context: &str,
) {
    for _ in 0..8 {
        let key = key(rng.below(520));
        assert_eq!(
            reader.get(tree, &key).unwrap().as_ref(),
            model.get(&key),
            "{context}"
        );
    }
    if rng.below(6) == 0 {
        let order = if rng.below(2) == 0 {
            ScanOrder::Ascending
        } else {
            ScanOrder::Descending
        };
        assert!(read_all(reader, tree, order) == *model, "{context}");
    }
}

fn snapshot_isolation_against_a_model(durability: Durability, seed: u64) {
    let fs = SimFileSystem::new();
    let (store, tree) = created(&fs, 48, &october_clock());
    store.set_durability(durability).unwrap();
    store.set_auto_checkpoint_bytes(Some(64 * 1024)).unwrap();
    store
        .set_snapshot_memory(CacheSize::Pages(1 << 20))
        .unwrap();
    let mut rng = TestRng::new(seed);
    let mut model = Model::new();
    let mut readers: Vec<(ReadTransaction<'_, SimFile>, Model, CommitStamp)> = Vec::new();
    let mut last_stamp = store.last_commit().unwrap();
    let (mut commits, mut failed, mut most_images) = (0, 0, 0);
    for round in 0..400 {
        if rng.below(3) == 0 {
            readers.push((store.begin_read().unwrap(), model.clone(), last_stamp));
        }
        let mut write = store.begin_write().unwrap();
        let mut pending = model.clone();
        let mut ok = true;
        for _ in 0..1 + rng.below(24) {
            let applied = match random_change(&mut rng) {
                Change::Put(key, value) => write.put(&tree, &key, &value).map(|()| {
                    pending.insert(key, value);
                }),
                Change::Delete(key) => write.delete(&tree, &key).map(|found| {
                    assert_eq!(found, pending.remove(&key).is_some());
                }),
            };
            if let Err(error) = applied {
                assert!(matches!(error, StorageError::CacheFull { .. }), "{error}");
                assert!(matches!(
                    write.put(&tree, b"k", b"v"),
                    Err(StorageError::TransactionFailed)
                ));
                ok = false;
                failed += 1;
                break;
            }
            if rng.below(12) == 0 {
                readers.push((store.begin_read().unwrap(), model.clone(), last_stamp));
            }
        }
        if ok && rng.below(6) == 0 {
            drop(write);
        } else if ok {
            let stamp = write.commit().unwrap();
            assert!(stamp > last_stamp);
            last_stamp = stamp;
            model = pending;
            commits += 1;
        }
        most_images = most_images.max(store.cache_stats().unwrap().before_images);
        for (index, (reader, expected, stamp)) in readers.iter().enumerate() {
            assert_eq!(reader.stamp(), *stamp);
            let context = format!("{durability:?} round {round}, reader {index}");
            assert_reader_matches(reader, &tree, expected, &mut rng, &context);
        }
        readers.retain(|_| rng.below(5) != 0);
        if rng.below(40) == 0 {
            store.checkpoint().unwrap();
        }
    }
    drop(readers);
    assert_eq!(store.cache_stats().unwrap().before_images, 0);
    assert!(commits > 250 && most_images > 30, "{commits} {most_images}");
    assert!(store.log_stats().unwrap().checkpoints > 5);
    eprintln!("{durability:?}: {commits} commits, {failed} failed, {most_images} images at most");
    assert_tree_checks(&store, &tree, &model);
    drop(store);
    let (store, tree) = reopened(&fs, 48, &october_clock());
    assert_eq!(committed(&store, &tree), model);
}

#[test]
fn snapshots_match_a_model_while_a_writer_commits_and_rolls_back_many_times() {
    snapshot_isolation_against_a_model(Durability::Full, 11);
}

#[test]
fn snapshots_match_a_model_under_normal_durability() {
    snapshot_isolation_against_a_model(Durability::Normal, 12);
}

#[test]
fn readers_on_other_threads_always_see_one_whole_commit() {
    fn assert_send_and_sync<T: Send + Sync>() {}
    assert_send_and_sync::<Store<SimFile>>();
    let fs = SimFileSystem::new();
    let (store, tree) = created(&fs, 64, &october_clock());
    let entries = 120;
    let mut write = store.begin_write().unwrap();
    for index in 0..entries {
        write.put(&tree, &key(index), &0u64.to_le_bytes()).unwrap();
    }
    write.commit().unwrap();
    store
        .set_snapshot_memory(CacheSize::Pages(1 << 20))
        .unwrap();
    std::thread::scope(|scope| {
        for _ in 0..3 {
            scope.spawn(|| {
                let mut previous = (CommitStamp::NONE, 0);
                for _ in 0..60 {
                    let reader = store.begin_read().unwrap();
                    let values = read_all(&reader, &tree, ScanOrder::Ascending);
                    assert_eq!(values.len(), entries);
                    let first = values.values().next().unwrap().clone();
                    assert!(values.values().all(|value| *value == first));
                    let round = u64::from_le_bytes(first.try_into().unwrap());
                    assert!((reader.stamp(), round) >= previous);
                    previous = (reader.stamp(), round);
                    let got = reader.get(&tree, &key(entries / 2)).unwrap().unwrap();
                    assert_eq!(got, round.to_le_bytes());
                }
            });
        }
        for round in 1..=100u64 {
            let mut write = store.begin_write().unwrap();
            for index in 0..entries {
                write.put(&tree, &key(index), &round.to_le_bytes()).unwrap();
            }
            write.commit().unwrap();
        }
    });
}

#[test]
fn a_reader_left_too_far_behind_fails_as_too_old_and_the_writer_goes_on() {
    let fs = SimFileSystem::new();
    let (store, tree) = created(&fs, 64, &october_clock());
    let before = fill(&store, &tree, 600, 60);
    store.set_snapshot_memory(CacheSize::Pages(4)).unwrap();
    let old = store.begin_read().unwrap();
    assert_eq!(old.get(&tree, &key(1)).unwrap().unwrap(), before[&key(1)]);
    let mut write = store.begin_write().unwrap();
    for index in (0..600).step_by(20) {
        write.put(&tree, &key(index), b"new").unwrap();
    }
    let stamp = write.commit().unwrap();
    for _ in 0..2 {
        assert!(matches!(
            old.get(&tree, &key(1)),
            Err(StorageError::SnapshotTooOld)
        ));
    }
    let mut cursor = tree.scan_prefix(b"key", ScanOrder::Ascending);
    assert!(matches!(
        old.next(&mut cursor),
        Err(StorageError::SnapshotTooOld)
    ));
    assert!(StorageError::SnapshotTooOld.to_string().contains("too old"));
    let fresh = store.begin_read().unwrap();
    assert_eq!(fresh.stamp(), stamp);
    assert_eq!(fresh.get(&tree, &key(20)).unwrap().unwrap(), b"new");
    drop(old);
    let mut small = store.begin_write().unwrap();
    small.put(&tree, &key(0), b"newer").unwrap();
    small.commit().unwrap();
    assert_eq!(fresh.get(&tree, &key(0)).unwrap().unwrap(), b"new");
    assert_eq!(store.cache_stats().unwrap().before_images, 1);
}

#[test]
fn a_reader_living_across_many_commits_keeps_one_image_per_changed_page() {
    let fs = SimFileSystem::new();
    let (store, tree) = created(&fs, 64, &october_clock());
    let before = fill(&store, &tree, 400, 60);
    let reader = store.begin_read().unwrap();
    for round in 0..500u32 {
        let mut write = store.begin_write().unwrap();
        write.put(&tree, &key(7), &round.to_le_bytes()).unwrap();
        write.commit().unwrap();
    }
    assert_eq!(store.cache_stats().unwrap().before_images, 1);
    assert_eq!(
        reader.get(&tree, &key(7)).unwrap().unwrap(),
        before[&key(7)]
    );
    let middle = store.begin_read().unwrap();
    for round in 0..500u32 {
        let mut write = store.begin_write().unwrap();
        write.put(&tree, &key(7), &round.to_le_bytes()).unwrap();
        write.commit().unwrap();
    }
    assert_eq!(store.cache_stats().unwrap().before_images, 2);
    assert_eq!(
        middle.get(&tree, &key(7)).unwrap().unwrap(),
        499u32.to_le_bytes()
    );
    assert_eq!(read_all(&reader, &tree, ScanOrder::Ascending), before);
    drop(reader);
    assert_eq!(store.cache_stats().unwrap().before_images, 1);
}

fn change_every_twentieth(write: &mut WriteTransaction<'_, SimFile>, tree: &BTree) {
    for index in (0..600).step_by(20) {
        write.put(tree, &key(index), b"new").unwrap();
    }
}

fn assert_readers_see(store: &Store<SimFile>, tree: &BTree, model: &Model) {
    let fresh = store.begin_read().unwrap();
    assert_eq!(
        fresh.get(tree, &key(1)).unwrap().as_ref(),
        model.get(&key(1))
    );
    assert_eq!(read_all(&fresh, tree, ScanOrder::Ascending), *model);
}

#[test]
fn a_rolled_back_writer_that_expired_snapshots_leaves_new_and_current_readers_working() {
    for explicit in [true, false] {
        let fs = SimFileSystem::new();
        let (store, tree) = created(&fs, 64, &october_clock());
        let before = fill(&store, &tree, 600, 60);
        store.set_snapshot_memory(CacheSize::Pages(4)).unwrap();
        let old = store.begin_read().unwrap();
        let mut write = store.begin_write().unwrap();
        change_every_twentieth(&mut write, &tree);
        assert!(matches!(
            old.get(&tree, &key(1)),
            Err(StorageError::SnapshotTooOld)
        ));
        if explicit {
            write.rollback().unwrap();
        } else {
            drop(write);
        }
        assert_eq!(read_all(&old, &tree, ScanOrder::Ascending), before);
        drop(old);
        assert_readers_see(&store, &tree, &before);
    }
}

#[test]
fn snapshot_memory_lowered_inside_a_write_transaction_then_rolled_back_leaves_readers_working() {
    let fs = SimFileSystem::new();
    let (store, tree) = created(&fs, 64, &october_clock());
    let before = fill(&store, &tree, 600, 60);
    let old = store.begin_read().unwrap();
    let mut write = store.begin_write().unwrap();
    change_every_twentieth(&mut write, &tree);
    assert!(store.cache_stats().unwrap().before_images > 2);
    store.set_snapshot_memory(CacheSize::Pages(2)).unwrap();
    assert!(matches!(
        old.get(&tree, &key(1)),
        Err(StorageError::SnapshotTooOld)
    ));
    drop(write);
    assert_eq!(old.get(&tree, &key(1)).unwrap().unwrap(), before[&key(1)]);
    assert_readers_see(&store, &tree, &before);
}

#[test]
fn a_failed_put_that_rolled_back_an_over_budget_writer_leaves_readers_working() {
    let fs = SimFileSystem::new();
    let before = {
        let (store, tree) = created(&fs, 64, &october_clock());
        fill(&store, &tree, 600, 60)
    };
    let (store, tree) = reopened(&fs, 8, &october_clock());
    store.set_snapshot_memory(CacheSize::Pages(2)).unwrap();
    let old = store.begin_read().unwrap();
    let mut write = store.begin_write().unwrap();
    let failure = (0..600)
        .step_by(20)
        .find_map(|index| write.put(&tree, &key(index), b"new").err());
    assert!(
        matches!(failure, Some(StorageError::CacheFull { .. })),
        "{failure:?}"
    );
    drop(write);
    assert_eq!(old.get(&tree, &key(1)).unwrap().unwrap(), before[&key(1)]);
    drop(old);
    assert_readers_see(&store, &tree, &before);
}

#[test]
fn a_refused_commit_of_an_over_budget_writer_leaves_readers_working() {
    let fs = SimFileSystem::new();
    let before = {
        let (store, tree) = created(&fs, 64, &october_clock());
        fill(&store, &tree, 600, 60)
    };
    cache(&fs, 32)
        .commit(|superblock| superblock.clock_high_water = u64::MAX)
        .unwrap();
    let (store, tree) = reopened(&fs, 64, &october_clock());
    store.set_snapshot_memory(CacheSize::Pages(4)).unwrap();
    let old = store.begin_read().unwrap();
    let mut write = store.begin_write().unwrap();
    change_every_twentieth(&mut write, &tree);
    assert!(matches!(write.commit(), Err(StorageError::ClockExhausted)));
    drop(old);
    assert_readers_see(&store, &tree, &before);
}

fn sequential_tree(fs: &SimFileSystem) -> Model {
    let (store, tree) = created(fs, 64, &october_clock());
    let model = fill(&store, &tree, 200, 100);
    let report = store.with_cache(|cache| tree.check(cache)).unwrap();
    assert_eq!(report.height, 2);
    model
}

#[test]
fn a_put_that_fails_in_the_middle_of_a_split_rolls_the_whole_transaction_back() {
    let fs = SimFileSystem::new();
    let model = sequential_tree(&fs);
    let inside_the_first_leaf = b"key00010x".to_vec();
    {
        let mut raw = cache(&fs, 2);
        let tree = BTree::open(&raw, raw.superblock().catalog_root).unwrap();
        let error = tree
            .put(&raw, &inside_the_first_leaf, &[1; 100])
            .unwrap_err();
        assert!(matches!(error, StorageError::CacheFull { .. }), "{error}");
        assert_eq!(raw.stats().dirty_pages, 2, "the split changed pages first");
        raw.rollback().unwrap();
    }
    let (store, tree) = reopened(&fs, 2, &october_clock());
    let reader = store.begin_read().unwrap();
    let mut write = store.begin_write().unwrap();
    write
        .put(&tree, &key(150), b"kept until the failure")
        .unwrap();
    let error = write
        .put(&tree, &inside_the_first_leaf, &[1; 100])
        .unwrap_err();
    assert!(matches!(error, StorageError::CacheFull { .. }), "{error}");
    assert_eq!(store.cache_stats().unwrap().dirty_pages, 0);
    for refused in [
        write.put(&tree, b"a", b"b").err(),
        write.delete(&tree, b"a").err(),
        write.get(&tree, b"a").err(),
    ] {
        assert!(matches!(refused, Some(StorageError::TransactionFailed)));
    }
    let next = store.begin_write().unwrap();
    assert!(matches!(
        write.commit(),
        Err(StorageError::TransactionFailed)
    ));
    assert_eq!(read_all(&reader, &tree, ScanOrder::Ascending), model);
    assert_eq!(written_all(&next, &tree), model);
    drop(next);
    assert_tree_checks(&store, &tree, &model);
}

#[test]
fn a_change_refused_before_it_starts_leaves_the_transaction_open() {
    let fs = SimFileSystem::new();
    let (store, tree) = created(&fs, 32, &october_clock());
    let mut write = store.begin_write().unwrap();
    write.put(&tree, b"a", b"1").unwrap();
    let long_key = vec![b'k'; BTree::max_key_bytes(PageSize::DEFAULT) + 1];
    assert!(matches!(
        write.put(&tree, &long_key, b""),
        Err(StorageError::KeyTooLarge { .. })
    ));
    let long_value = vec![0; BTree::max_entry_bytes(PageSize::DEFAULT)];
    assert!(matches!(
        write.put(&tree, b"b", &long_value),
        Err(StorageError::EntryTooLarge { .. })
    ));
    write.put(&tree, b"c", b"3").unwrap();
    write.commit().unwrap();
    assert_eq!(committed(&store, &tree).len(), 2);
}

#[test]
fn commit_stamps_rise_with_the_wall_clock_never_fall_and_survive_a_reopen() {
    let fs = SimFileSystem::new();
    let clock = october_clock();
    let (store, tree) = created(&fs, 32, &clock);
    let created_at = store.last_commit().unwrap();
    assert_eq!(created_at.unix_micros(), OCTOBER_2026_NANOS / 1000);
    let mut stamps = vec![created_at];
    for round in 0..5u8 {
        let mut write = store.begin_write().unwrap();
        assert_eq!(write.began_after(), *stamps.last().unwrap());
        write.put(&tree, b"k", &[round]).unwrap();
        stamps.push(write.commit().unwrap());
    }
    assert_eq!(stamps[5].logical(), 5);
    let reader = store.begin_read().unwrap();
    assert_eq!(reader.stamp(), stamps[5]);
    clock.advance(HOUR);
    let mut write = store.begin_write().unwrap();
    write.put(&tree, b"k", b"later").unwrap();
    let later = write.commit().unwrap();
    assert_eq!(later.unix_micros(), stamps[5].unix_micros() + 3_600_000_000);
    assert_eq!(later.logical(), 0);
    assert_eq!(reader.stamp(), stamps[5]);
    clock.set_unix_time_nanos(OCTOBER_2026_NANOS - 24 * HOUR.as_nanos() as u64);
    let mut write = store.begin_write().unwrap();
    write.put(&tree, b"k", b"clock set back").unwrap();
    let after_setback = write.commit().unwrap();
    assert_eq!(after_setback.bits(), later.bits() + 1);
    assert_eq!(
        store.superblock().unwrap().clock_high_water,
        after_setback.bits()
    );
    drop(reader);
    drop(store);
    fs.crash(CrashMode::Torn { seed: 1 });
    let (store, tree) = reopened(&fs, 32, &clock);
    assert_eq!(store.last_commit().unwrap(), after_setback);
    assert_eq!(store.begin_read().unwrap().stamp(), after_setback);
    let mut write = store.begin_write().unwrap();
    write.put(&tree, b"k", b"after reopen").unwrap();
    assert_eq!(write.commit().unwrap().bits(), after_setback.bits() + 1);
}

#[test]
fn an_exhausted_clock_refuses_the_commit_and_rolls_it_back() {
    let fs = SimFileSystem::new();
    let clock = october_clock();
    let (store, tree) = created(&fs, 32, &clock);
    drop(store);
    {
        let mut raw = cache(&fs, 32);
        raw.commit(|superblock| superblock.clock_high_water = u64::MAX)
            .unwrap();
    }
    let (store, _) = reopened(&fs, 32, &clock);
    let mut write = store.begin_write().unwrap();
    write.put(&tree, b"k", b"v").unwrap();
    assert!(matches!(write.commit(), Err(StorageError::ClockExhausted)));
    assert!(committed(&store, &tree).is_empty());
    assert_eq!(store.cache_stats().unwrap().dirty_pages, 0);
    drop(store.begin_write().unwrap());
}

#[test]
fn a_store_over_a_read_only_file_refuses_writers_and_serves_readers() {
    let fs = SimFileSystem::new();
    let (store, tree) = created(&fs, 32, &october_clock());
    let model = fill(&store, &tree, 30, 10);
    let last = store.last_commit().unwrap();
    drop(store);
    let file = PageFile::open(&fs, Path::new(DB), Access::ReadOnly).unwrap();
    let cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(8)).unwrap();
    let store = Store::new(cache, october_clock());
    assert!(matches!(store.begin_write(), Err(StorageError::ReadOnly)));
    assert_eq!(store.last_commit().unwrap(), last);
    assert_eq!(committed(&store, &tree), model);
}

#[test]
fn a_failed_commit_leaves_the_store_needing_a_reopen_for_writers_and_readers() {
    let fs = SimFileSystem::new();
    let (store, tree) = created(&fs, 32, &october_clock());
    let model = fill(&store, &tree, 50, 20);
    let reader = store.begin_read().unwrap();
    let mut write = store.begin_write().unwrap();
    write.put(&tree, &key(3), b"lost").unwrap();
    fs.fail_operation(fs.operation_count());
    assert!(matches!(write.commit(), Err(StorageError::Io(_))));
    assert!(matches!(
        store.begin_write(),
        Err(StorageError::NeedsReopen)
    ));
    assert!(matches!(
        reader.get(&tree, &key(3)),
        Err(StorageError::NeedsReopen)
    ));
    drop(reader);
    drop(store);
    let (store, tree) = reopened(&fs, 32, &october_clock());
    assert_eq!(committed(&store, &tree), model);
}
