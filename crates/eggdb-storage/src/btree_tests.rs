use crate::btree::{BTree, TreeReport};
use crate::error::{FormatError, StorageError};
use crate::format::{Access, FileId, PageHeader, PageSize, PageType};
use crate::page_cache::{CacheSize, PageCache};
use crate::page_file::{CreateOptions, PageFile};
use crate::test_rng::TestRng;
use crate::tree_page::{Node, Shape, shortest_separator};
use eggdb_platform::{CrashMode, SimFile, SimFileSystem, SimMemory};
use std::collections::BTreeMap;
use std::path::Path;

const DB: &str = "db";

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

fn create(fs: &SimFileSystem, page_size: PageSize, frames: usize) -> PageCache<SimFile> {
    let options = CreateOptions {
        page_size,
        file_id: FileId(*b"b+tree-tests-id!"),
    };
    let file = PageFile::create(fs, Path::new(DB), options).unwrap();
    PageCache::new(file, &SimMemory::new(), CacheSize::Pages(frames)).unwrap()
}

fn open(fs: &SimFileSystem, access: Access, frames: usize) -> PageCache<SimFile> {
    let file = PageFile::open(fs, Path::new(DB), access).unwrap();
    PageCache::new(file, &SimMemory::new(), CacheSize::Pages(frames)).unwrap()
}

fn new_tree(cache: &mut PageCache<SimFile>) -> BTree {
    let tree = BTree::create(cache).unwrap();
    cache
        .commit(|superblock| superblock.catalog_root = tree.root())
        .unwrap();
    tree
}

fn stored_tree(cache: &PageCache<SimFile>) -> BTree {
    BTree::open(cache, cache.superblock().catalog_root).unwrap()
}

fn assert_matches(tree: &BTree, cache: &PageCache<SimFile>, model: &Model) {
    let report = tree.check(cache).unwrap();
    assert_eq!(report.entries, model.len() as u64);
    assert_eq!(
        report.key_bytes,
        model.keys().map(|key| key.len() as u64).sum::<u64>()
    );
    for (key, value) in model {
        assert_eq!(
            tree.get(cache, key).unwrap().as_ref(),
            Some(value),
            "{key:?}"
        );
    }
}

fn bytes_up_to(rng: &mut TestRng, least: usize, extra: usize) -> Vec<u8> {
    let len = least + rng.below(extra);
    rng.bytes(len)
}

fn random_entry(rng: &mut TestRng, page_size: PageSize) -> (Vec<u8>, Vec<u8>) {
    let max_key = BTree::max_key_bytes(page_size);
    let max_entry = BTree::max_entry_bytes(page_size);
    let key_len = match rng.below(10) {
        0 => max_key,
        1 => 0,
        _ => rng.below(40),
    };
    let alphabet = 1 + rng.below(4) as u8;
    let key: Vec<u8> = (0..key_len)
        .map(|_| b'a' + rng.below(usize::from(alphabet)) as u8)
        .collect();
    let value_len = match rng.below(10) {
        0 => max_entry - key_len,
        1 => 0,
        _ => rng.below(120).min(max_entry - key_len),
    };
    (key, rng.bytes(value_len))
}

fn random_workload(page_size: PageSize, seed: u64, puts: usize) {
    let fs = SimFileSystem::new();
    let mut cache = create(&fs, page_size, 4096);
    let tree = new_tree(&mut cache);
    let mut model = Model::new();
    let mut rng = TestRng::new(seed);
    for round in 0..puts {
        let (key, value) = if round % 5 == 0 && !model.is_empty() {
            let existing = model.keys().nth(rng.below(model.len())).unwrap().clone();
            let value = bytes_up_to(
                &mut rng,
                0,
                BTree::max_entry_bytes(page_size) - existing.len(),
            );
            (existing, value)
        } else {
            random_entry(&mut rng, page_size)
        };
        tree.put(&cache, &key, &value).unwrap();
        model.insert(key, value);
        if round % 500 == 499 {
            cache.commit(|_| {}).unwrap();
        }
    }
    cache.commit(|_| {}).unwrap();
    assert_matches(&tree, &cache, &model);
    drop(cache);
    let cache = open(&fs, Access::ReadOnly, 64);
    let tree = stored_tree(&cache);
    assert_matches(&tree, &cache, &model);
    let report = tree.check(&cache).unwrap();
    assert!(report.height >= 2, "{report:?}");
}

#[test]
fn thousands_of_random_puts_match_a_model_after_a_reopen() {
    for seed in 0..3 {
        random_workload(PageSize::DEFAULT, seed, 3000);
    }
}

#[test]
fn random_puts_with_64_kib_pages_match_a_model() {
    random_workload(PageSize::from_log2(16).unwrap(), 7, 3000);
}

#[test]
fn sequential_and_reverse_inserts_split_up_to_three_levels_and_fill_pages() {
    for reverse in [false, true] {
        let fs = SimFileSystem::new();
        let mut cache = create(&fs, PageSize::DEFAULT, 4096);
        let tree = new_tree(&mut cache);
        let mut model = Model::new();
        for index in 0..60_000u32 {
            let number = if reverse { 60_000 - index } else { index };
            let key = format!("key-{number:08}").into_bytes();
            let value = number.to_le_bytes().to_vec();
            tree.put(&cache, &key, &value).unwrap();
            model.insert(key, value);
        }
        cache.commit(|_| {}).unwrap();
        let report = tree.check(&cache).unwrap();
        assert_eq!(report.height, 3, "{report:?}");
        let per_leaf = report.entries / report.leaf_pages;
        assert!(per_leaf > 170, "{report:?}");
        drop(cache);
        let cache = open(&fs, Access::ReadOnly, 16);
        assert_matches(&stored_tree(&cache), &cache, &model);
    }
}

#[test]
fn keys_that_are_prefixes_of_each_other_and_the_empty_key_are_kept_apart() {
    let fs = SimFileSystem::new();
    let mut cache = create(&fs, PageSize::DEFAULT, 256);
    let tree = new_tree(&mut cache);
    assert_eq!(tree.get(&cache, b"").unwrap(), None);
    let empty = tree.check(&cache).unwrap();
    assert_eq!((empty.height, empty.leaf_pages, empty.entries), (1, 1, 0));
    assert!(BTree::open(&cache, 0).is_err());
    let mut model = Model::new();
    let max_key = BTree::max_key_bytes(PageSize::DEFAULT);
    for len in (0..=max_key).rev() {
        let key = vec![b'p'; len];
        let value = (len as u32).to_le_bytes().to_vec();
        tree.put(&cache, &key, &value).unwrap();
        model.insert(key, value);
    }
    assert_eq!(
        tree.get(&cache, b"").unwrap(),
        Some(0u32.to_le_bytes().to_vec())
    );
    assert_eq!(tree.get(&cache, b"q").unwrap(), None);
    assert_eq!(tree.get(&cache, &vec![b'p'; max_key + 1]).unwrap(), None);
    tree.put(&cache, b"pp", b"").unwrap();
    model.insert(b"pp".to_vec(), Vec::new());
    assert_matches(&tree, &cache, &model);
}

#[test]
fn keys_and_entries_over_the_limits_are_refused_before_any_change() {
    for log2 in [12, 16] {
        let page_size = PageSize::from_log2(log2).unwrap();
        let fs = SimFileSystem::new();
        let mut cache = create(&fs, page_size, 64);
        let tree = new_tree(&mut cache);
        let max_key = BTree::max_key_bytes(page_size);
        let max_entry = BTree::max_entry_bytes(page_size);
        assert_eq!(max_key, page_size.bytes() / 8);
        assert!(matches!(
            tree.put(&cache, &vec![1; max_key + 1], b""),
            Err(StorageError::KeyTooLarge { bytes, max }) if bytes == max_key + 1 && max == max_key
        ));
        assert!(matches!(
            tree.put(&cache, &vec![1; max_key], &vec![2; max_entry - max_key + 1]),
            Err(StorageError::EntryTooLarge { max, .. }) if max == max_entry
        ));
        assert_eq!(cache.stats().dirty_pages, 0);
        let mut model = Model::new();
        for index in 0..40u8 {
            let mut key = vec![index; max_key];
            key[0] = 0xFF - index;
            let value = vec![index; max_entry - max_key];
            tree.put(&cache, &key, &value).unwrap();
            model.insert(key, value);
        }
        assert_matches(&tree, &cache, &model);
    }
}

#[test]
fn overwrites_with_longer_and_shorter_values_reuse_the_page() {
    let fs = SimFileSystem::new();
    let mut cache = create(&fs, PageSize::DEFAULT, 64);
    let tree = new_tree(&mut cache);
    let mut rng = TestRng::new(3);
    let mut model = Model::new();
    for round in 0..2000 {
        let key = vec![b'k', rng.below(8) as u8];
        let value = bytes_up_to(&mut rng, 0, 300);
        tree.put(&cache, &key, &value).unwrap();
        model.insert(key, value);
        if round % 100 == 0 {
            assert_matches(&tree, &cache, &model);
        }
    }
    let report = tree.check(&cache).unwrap();
    assert_eq!(report.leaf_pages + report.inner_pages, 1, "{report:?}");
    assert_matches(&tree, &cache, &model);
}

#[test]
fn a_cache_too_small_for_a_split_fails_with_an_error_and_a_rollback_restores_the_tree() {
    let fs = SimFileSystem::new();
    let mut cache = create(&fs, PageSize::DEFAULT, 4096);
    let tree = new_tree(&mut cache);
    let mut model = Model::new();
    for index in 0..3000u32 {
        let key = index.to_be_bytes().to_vec();
        tree.put(&cache, &key, &[7; 40]).unwrap();
        model.insert(key, vec![7; 40]);
    }
    cache.commit(|_| {}).unwrap();
    drop(cache);
    let mut cache = open(&fs, Access::ReadWrite, 3);
    let tree = stored_tree(&cache);
    let mut failed = false;
    for index in 3000..4000u32 {
        match tree.put(&cache, &index.to_be_bytes(), &[8; 40]) {
            Ok(()) => {}
            Err(StorageError::CacheFull { .. }) => {
                failed = true;
                break;
            }
            Err(error) => panic!("{error}"),
        }
    }
    assert!(failed);
    cache.rollback().unwrap();
    assert_matches(&tree, &cache, &model);
}

#[test]
fn page_layouts_survive_encoding_and_pass_their_checks() {
    let page_size = PageSize::DEFAULT.bytes();
    let leaf = Node {
        level: 0,
        low: b"apple/0".to_vec(),
        high: Some(b"apple/9".to_vec()),
        keys: vec![
            b"apple/0".to_vec(),
            b"apple/1x".to_vec(),
            b"apple/5".to_vec(),
        ],
        values: vec![b"".to_vec(), b"one".to_vec(), vec![5; 100]],
        children: Vec::new(),
    };
    let inner = Node {
        level: 2,
        low: Vec::new(),
        high: None,
        keys: vec![b"b".to_vec(), b"m".to_vec()],
        values: Vec::new(),
        children: vec![10, 11, 12],
    };
    for node in [leaf, inner] {
        let header = crate::tree_page::header_for(node.level, 0);
        let mut body = vec![0xEE; page_size - 16];
        node.encode(9, &mut body).unwrap();
        let shape = Shape::parse(9, header, &body).unwrap();
        shape.validate(&body).unwrap();
        assert_eq!(shape.prefix_len, if node.level == 0 { 6 } else { 0 });
        assert_eq!(Node::decode(&shape, &body).unwrap(), node);
        assert_eq!(&body[0..2], &(node.keys.len() as u16).to_le_bytes());
    }
}

#[test]
fn separators_are_the_shortest_key_between_the_halves() {
    assert_eq!(shortest_separator(b"abc", b"abd"), b"abd");
    assert_eq!(shortest_separator(b"abc", b"b"), b"b");
    assert_eq!(shortest_separator(b"ab", b"abzz"), b"abz");
    assert_eq!(shortest_separator(b"", b"x"), b"x");
    assert_eq!(shortest_separator(b"apple", b"banana"), b"b");
}

fn tree_with_entries(fs: &SimFileSystem, seed: u64, count: usize) -> Model {
    let mut cache = create(fs, PageSize::DEFAULT, 512);
    let tree = new_tree(&mut cache);
    let mut rng = TestRng::new(seed);
    let mut model = Model::new();
    for _ in 0..count {
        let key = bytes_up_to(&mut rng, 4, 12);
        let value = bytes_up_to(&mut rng, 0, 60);
        tree.put(&cache, &key, &value).unwrap();
        model.insert(key, value);
    }
    cache.commit(|_| {}).unwrap();
    model
}

fn grow(fs: &SimFileSystem, model: &mut Model, puts: usize, fail_at: Option<u64>) -> u64 {
    let mut cache = open(fs, Access::ReadWrite, 512);
    let tree = stored_tree(&cache);
    let mut rng = TestRng::new(99);
    for _ in 0..puts {
        let key = bytes_up_to(&mut rng, 4, 12);
        let value = bytes_up_to(&mut rng, 0, 60);
        tree.put(&cache, &key, &value).unwrap();
        model.insert(key, value);
    }
    let start = fs.operation_count();
    if let Some(offset) = fail_at {
        fs.fail_operation(start + offset);
    }
    let committed = cache.commit(|_| {});
    assert_eq!(committed.is_ok(), fail_at.is_none());
    fs.operation_count() - start
}

fn committed_report(fs: &SimFileSystem) -> TreeReport {
    let cache = open(fs, Access::ReadOnly, 64);
    stored_tree(&cache).check(&cache).unwrap()
}

fn crash_at_every_operation_of_growing(entries: usize, puts: usize) -> (TreeReport, TreeReport) {
    let (operations, before, after) = {
        let fs = SimFileSystem::new();
        let mut model = tree_with_entries(&fs, 1, entries);
        let before = committed_report(&fs);
        let operations = grow(&fs, &mut model, puts, None);
        (operations, before, committed_report(&fs))
    };
    let mut outcomes = std::collections::BTreeSet::new();
    for step in 0..operations {
        for seed in 0..4 {
            let fs = SimFileSystem::new();
            let old = tree_with_entries(&fs, 1, entries);
            let mut new = old.clone();
            grow(&fs, &mut new, puts, Some(step));
            fs.crash(CrashMode::Torn { seed });
            let cache = open(&fs, Access::ReadOnly, 64);
            let tree = stored_tree(&cache);
            let entries = tree.check(&cache).unwrap().entries;
            let expected = if entries == new.len() as u64 {
                &new
            } else {
                &old
            };
            assert_matches(&tree, &cache, expected);
            outcomes.insert(entries == new.len() as u64);
        }
    }
    assert_eq!(outcomes.len(), 2);
    (before, after)
}

#[test]
fn a_crash_at_any_operation_of_a_commit_that_splits_leaves_the_old_or_the_new_tree() {
    let (before, after) = crash_at_every_operation_of_growing(300, 400);
    assert!(after.leaf_pages >= before.leaf_pages + 3, "{after:?}");
}

#[test]
fn a_crash_at_any_operation_of_a_commit_that_splits_the_root_leaves_the_old_or_the_new_tree() {
    let (before, after) = crash_at_every_operation_of_growing(20, 150);
    assert_eq!((before.height, after.height), (1, 2), "{after:?}");
}

fn tree_pages(cache: &PageCache<SimFile>) -> Vec<u64> {
    let first = cache.page_size().first_data_page();
    (first..cache.superblock().page_count)
        .filter(|&page| {
            cache.read(page).is_ok_and(|page| {
                matches!(
                    page.header().page_type,
                    PageType::BTreeLeaf | PageType::BTreeInner
                )
            })
        })
        .collect()
}

fn mutate(rng: &mut TestRng, header: &mut PageHeader, body: &mut [u8]) {
    for _ in 0..1 + rng.below(3) {
        match rng.below(6) {
            0 => {
                header.page_type = [
                    PageType::BTreeLeaf,
                    PageType::BTreeInner,
                    PageType::Overflow,
                ][rng.below(3)];
            }
            1 => header.layout = rng.below(3) as u8,
            2 => {
                let at = rng.below(32);
                body[at] = rng.next_u64() as u8;
            }
            3 => {
                let at = rng.below(body.len() - 1);
                body[at..at + 2].copy_from_slice(&(rng.below(4200) as u16).to_le_bytes());
            }
            4 => {
                let at = rng.below(body.len());
                body[at] ^= 1 << rng.below(8);
            }
            _ => {
                let a = rng.below(body.len() - 8);
                let b = rng.below(body.len() - 8);
                let saved: Vec<u8> = body[a..a + 8].to_vec();
                body.copy_within(b..b + 8, a);
                body[b..b + 8].copy_from_slice(&saved);
            }
        }
    }
}

#[test]
fn seeded_mutations_of_tree_pages_never_panic_or_hang() {
    let fs = SimFileSystem::new();
    let model = tree_with_entries(&fs, 5, 400);
    let keys: Vec<Vec<u8>> = model.keys().cloned().collect();
    let pages = tree_pages(&open(&fs, Access::ReadOnly, 64));
    assert!(pages.len() > 4);
    let mut rng = TestRng::new(11);
    let mut refused = 0;
    for _ in 0..300 {
        let target = pages[rng.below(pages.len())];
        let original = {
            let mut cache = open(&fs, Access::ReadWrite, 64);
            let mut page = cache.write(target).unwrap();
            let original = (page.header(), page.to_vec());
            let mut header = page.header();
            mutate(&mut rng, &mut header, &mut page);
            page.set_header(header).unwrap();
            drop(page);
            cache.commit(|_| {}).unwrap();
            original
        };
        {
            let mut cache = open(&fs, Access::ReadWrite, 64);
            let found = BTree::open(&cache, cache.superblock().catalog_root).and_then(|tree| {
                for key in keys.iter().step_by(7) {
                    tree.get(&cache, key)?;
                }
                tree.put(&cache, b"new key", b"new value")?;
                tree.put(&cache, &keys[rng.below(keys.len())], &[1; 200])?;
                tree.check(&cache)
            });
            if let Err(error) = found {
                assert!(
                    matches!(
                        error,
                        StorageError::Format(_)
                            | StorageError::PageOutOfRange { .. }
                            | StorageError::PageNotInUse { .. }
                    ),
                    "{error}"
                );
                refused += 1;
            }
            cache.rollback().unwrap();
        }
        let mut cache = open(&fs, Access::ReadWrite, 64);
        let mut page = cache.write(target).unwrap();
        page.set_header(original.0).unwrap();
        page.copy_from_slice(&original.1);
        drop(page);
        cache.commit(|_| {}).unwrap();
    }
    assert!(refused > 150, "{refused}");
    let cache = open(&fs, Access::ReadOnly, 64);
    assert_matches(&stored_tree(&cache), &cache, &model);
}

#[test]
fn hostile_child_pointers_and_levels_are_refused() {
    let fs = SimFileSystem::new();
    tree_with_entries(&fs, 6, 400);
    let mut cache = open(&fs, Access::ReadWrite, 64);
    let root = cache.superblock().catalog_root;
    {
        let mut page = cache.write(root).unwrap();
        let header = page.header();
        let mut body = page.to_vec();
        let shape = Shape::parse(root, header, &body).unwrap();
        let mut node = Node::decode(&shape, &body).unwrap();
        *node.children.last_mut().unwrap() = root;
        node.encode(root, &mut body).unwrap();
        page.copy_from_slice(&body);
    }
    cache.commit(|_| {}).unwrap();
    drop(cache);
    let cache = open(&fs, Access::ReadOnly, 64);
    let tree = stored_tree(&cache);
    let error = tree.get(&cache, &[0xFF; 16]).unwrap_err();
    assert!(
        matches!(error, StorageError::Format(FormatError::CorruptTree { .. })),
        "{error}"
    );
    assert!(tree.check(&cache).is_err());
}

#[test]
fn a_child_shared_by_two_parents_is_refused() {
    let fs = SimFileSystem::new();
    let model = tree_with_entries(&fs, 6, 400);
    let mut cache = open(&fs, Access::ReadWrite, 64);
    let root = cache.superblock().catalog_root;
    {
        let mut page = cache.write(root).unwrap();
        let header = page.header();
        let mut body = page.to_vec();
        let shape = Shape::parse(root, header, &body).unwrap();
        let mut node = Node::decode(&shape, &body).unwrap();
        assert_eq!(node.level, 1);
        node.children[1] = node.children[0];
        node.encode(root, &mut body).unwrap();
        page.copy_from_slice(&body);
    }
    cache.commit(|_| {}).unwrap();
    drop(cache);
    let cache = open(&fs, Access::ReadOnly, 64);
    let tree = stored_tree(&cache);
    let error = tree.check(&cache).unwrap_err();
    assert!(
        matches!(error, StorageError::Format(FormatError::CorruptTree { .. })),
        "{error}"
    );
    let shared_range_key = model
        .keys()
        .find(|key| tree.get(&cache, key).is_err())
        .cloned();
    assert!(shared_range_key.is_some());
}
