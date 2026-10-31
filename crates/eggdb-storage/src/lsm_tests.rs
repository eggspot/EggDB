use crate::cursor::ScanOrder;
use crate::format::{Access, FileId, PageSize};
use crate::lsm::LsmTree;
use crate::page_cache::{CacheSize, PageCache};
use crate::page_file::{CreateOptions, PageFile};
use crate::store::Store;
use crate::test_rng::TestRng;
use eggdb_platform::{CrashMode, SimClock, SimFile, SimFileSystem, SimMemory};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

const DB: &str = "lsm";
const FRAMES: usize = 512;

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

fn created(fs: &SimFileSystem) -> (Store<SimFile>, LsmTree) {
    let options = CreateOptions {
        page_size: PageSize::DEFAULT,
        file_id: FileId(*b"lsm-tests-file!!"),
    };
    let file = PageFile::create(fs, Path::new(DB), options).unwrap();
    let cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(FRAMES)).unwrap();
    let store = Store::new(cache, Arc::new(SimClock::new(1)));
    let mut write = store.begin_write().unwrap();
    let tree = write.create_lsm_tree().unwrap();
    write.set_catalog_root(tree.root());
    write.commit().unwrap();
    (store, tree)
}

fn reopened(fs: &SimFileSystem) -> (Store<SimFile>, LsmTree) {
    let file = PageFile::open(fs, Path::new(DB), Access::ReadWrite).unwrap();
    let cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(FRAMES)).unwrap();
    let store = Store::new(cache, Arc::new(SimClock::new(1)));
    let root = store.superblock().unwrap().catalog_root;
    let tree = store.begin_read().unwrap().open_lsm_tree(root).unwrap();
    (store, tree)
}

fn key(index: u64) -> Vec<u8> {
    format!("k{index:06}").into_bytes()
}

fn scanned(store: &Store<SimFile>, tree: &LsmTree, order: ScanOrder) -> Vec<(Vec<u8>, Vec<u8>)> {
    let read = store.begin_read().unwrap();
    let mut cursor = read
        .lsm_scan(tree, Bound::Unbounded, Bound::Unbounded, order)
        .unwrap();
    let mut found = Vec::new();
    while let Some((key, value)) = read.lsm_next(&mut cursor).unwrap() {
        found.push((key.to_vec(), value.to_vec()));
    }
    found
}

fn assert_matches(store: &Store<SimFile>, tree: &LsmTree, model: &Model, context: &str) {
    let expected: Vec<(Vec<u8>, Vec<u8>)> =
        model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert!(scanned(store, tree, ScanOrder::Ascending) == expected, "{context}: ascending");
    let mut backward = scanned(store, tree, ScanOrder::Descending);
    backward.reverse();
    assert!(backward == expected, "{context}: descending");
    let read = store.begin_read().unwrap();
    for index in 0..300 {
        let key = key(index);
        assert_eq!(read.lsm_get(tree, &key).unwrap(), model.get(&key).cloned(), "{context}: {index}");
    }
}

#[test]
fn random_puts_and_deletes_across_seals_and_merges_match_a_model() {
    let fs = SimFileSystem::new();
    let (store, tree) = created(&fs);
    let mut model = Model::new();
    let mut rng = TestRng::new(7);
    for round in 0..40 {
        let mut write = store.begin_write().unwrap();
        for _ in 0..25 {
            let key = key(rng.below(300) as u64);
            if rng.below(4) == 0 {
                let removed = write.lsm_delete(&tree, &key).unwrap();
                assert_eq!(removed, model.remove(&key).is_some());
            } else {
                let value = vec![(round % 251) as u8; 1 + rng.below(200)];
                write.lsm_put(&tree, &key, &value).unwrap();
                model.insert(key, value);
            }
        }
        let report = write.lsm_check(&tree).unwrap();
        assert!(report.components >= 1);
        write.commit().unwrap();
        assert_matches(&store, &tree, &model, &format!("round {round}"));
    }
    let mut write = store.begin_write().unwrap();
    write.lsm_finish_merges(&tree).unwrap();
    write.commit().unwrap();
    assert_matches(&store, &tree, &model, "merged");
    drop(store);
    let (store, tree) = reopened(&fs);
    assert_matches(&store, &tree, &model, "reopened");
}

#[test]
fn a_reader_keeps_its_snapshot_while_runs_merge_under_it() {
    let fs = SimFileSystem::new();
    let (store, tree) = created(&fs);
    let mut write = store.begin_write().unwrap();
    for index in 0..100 {
        write.lsm_put(&tree, &key(index), b"old").unwrap();
    }
    write.commit().unwrap();
    let read = store.begin_read().unwrap();
    let mut write = store.begin_write().unwrap();
    for index in 0..400 {
        write.lsm_put(&tree, &key(index % 150), b"new").unwrap();
    }
    write.lsm_finish_merges(&tree).unwrap();
    write.commit().unwrap();
    for index in 0..100 {
        assert_eq!(read.lsm_get(&tree, &key(index)).unwrap(), Some(b"old".to_vec()));
    }
    assert_eq!(read.lsm_get(&tree, &key(120)).unwrap(), None);
    drop(read);
    let after = store.begin_read().unwrap();
    assert_eq!(after.lsm_get(&tree, &key(120)).unwrap(), Some(b"new".to_vec()));
}

#[test]
fn a_crash_keeps_the_last_durable_commit() {
    let fs = SimFileSystem::new();
    let (store, tree) = created(&fs);
    let mut model = Model::new();
    for round in 0..12u64 {
        let mut write = store.begin_write().unwrap();
        for index in 0..30 {
            let key = key((round * 31 + index) % 200);
            let value = format!("{round}-{index}").into_bytes();
            write.lsm_put(&tree, &key, &value).unwrap();
            model.insert(key, value);
        }
        write.commit().unwrap();
    }
    let mut write = store.begin_write().unwrap();
    for index in 0..30 {
        write.lsm_put(&tree, &key(index), b"lost").unwrap();
    }
    drop(write);
    drop(store);
    fs.crash(CrashMode::DropUnsynced);
    let (store, tree) = reopened(&fs);
    assert_matches(&store, &tree, &model, "after the crash");
}

#[test]
fn values_at_the_limit_and_beyond_are_handled() {
    let fs = SimFileSystem::new();
    let (store, tree) = created(&fs);
    let mut write = store.begin_write().unwrap();
    assert_eq!(LsmTree::MAX_VALUE_BYTES, crate::btree::BTree::MAX_VALUE_BYTES - 1);
    write.lsm_put(&tree, b"", b"empty key").unwrap();
    write.lsm_put(&tree, b"large", &vec![9u8; 20_000]).unwrap();
    assert_eq!(write.lsm_get(&tree, b"").unwrap(), Some(b"empty key".to_vec()));
    assert_eq!(write.lsm_get(&tree, b"large").unwrap().map(|value| value.len()), Some(20_000));
    write.commit().unwrap();
}
