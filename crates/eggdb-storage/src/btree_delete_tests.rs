use crate::btree::BTree;
use crate::btree_tests::{
    DB, Model, assert_matches, bytes_up_to, committed_report, create, new_tree, open, random_entry,
    stored_tree, tree_with_entries,
};
use crate::cursor::{Cursor, ScanOrder};
use crate::error::{FormatError, StorageError};
use crate::format::{Access, PageSize};
use crate::page_cache::PageCache;
use crate::page_file::PageFile;
use crate::test_rng::TestRng;
use crate::tree_page::{Node, Shape, header_for};
use eggdb_platform::{CrashMode, SimFile, SimFileSystem};
use std::ops::Bound;
use std::path::Path;

type Entries = Vec<(Vec<u8>, Vec<u8>)>;

fn collect(mut cursor: Cursor, cache: &PageCache<SimFile>) -> Result<Entries, StorageError> {
    let mut found = Vec::new();
    while let Some((key, value)) = cursor.next(cache)? {
        found.push((key.to_vec(), value.to_vec()));
    }
    Ok(found)
}

fn in_bounds(key: &[u8], start: Bound<&[u8]>, end: Bound<&[u8]>) -> bool {
    let above_start = match start {
        Bound::Included(start) => key >= start,
        Bound::Excluded(start) => key > start,
        Bound::Unbounded => true,
    };
    let below_end = match end {
        Bound::Included(end) => key <= end,
        Bound::Excluded(end) => key < end,
        Bound::Unbounded => true,
    };
    above_start && below_end
}

fn expected(model: &Model, start: Bound<&[u8]>, end: Bound<&[u8]>, order: ScanOrder) -> Entries {
    let mut entries: Entries = model
        .iter()
        .filter(|(key, _)| in_bounds(key, start, end))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if order == ScanOrder::Descending {
        entries.reverse();
    }
    entries
}

fn assert_scan(
    tree: &BTree,
    cache: &PageCache<SimFile>,
    model: &Model,
    start: Bound<&[u8]>,
    end: Bound<&[u8]>,
    order: ScanOrder,
) {
    let found = collect(tree.scan(start, end, order), cache).unwrap();
    assert_eq!(
        found,
        expected(model, start, end, order),
        "{start:?} {end:?} {order:?}"
    );
}

fn assert_full_scans(tree: &BTree, cache: &PageCache<SimFile>, model: &Model) {
    for order in [ScanOrder::Ascending, ScanOrder::Descending] {
        assert_scan(
            tree,
            cache,
            model,
            Bound::Unbounded,
            Bound::Unbounded,
            order,
        );
    }
}

fn random_bound<'a>(rng: &mut TestRng, key: &'a [u8]) -> Bound<&'a [u8]> {
    match rng.below(3) {
        0 => Bound::Included(key),
        1 => Bound::Excluded(key),
        _ => Bound::Unbounded,
    }
}

fn random_key(rng: &mut TestRng, model: &Model, page_size: PageSize) -> Vec<u8> {
    if !model.is_empty() && rng.below(2) == 0 {
        let key = model.keys().nth(rng.below(model.len())).unwrap();
        let mut key = key.clone();
        match rng.below(3) {
            0 => key.push(0),
            1 => {
                key.pop();
            }
            _ => {}
        }
        key
    } else {
        random_entry(rng, page_size).0
    }
}

fn order(rng: &mut TestRng) -> ScanOrder {
    if rng.below(2) == 0 {
        ScanOrder::Ascending
    } else {
        ScanOrder::Descending
    }
}

fn random_model_workload(page_size: PageSize, frames: usize, seed: u64, commit_every: usize) {
    let rounds = 6000;
    let fs = SimFileSystem::new();
    let mut cache = create(&fs, page_size, frames);
    let mut tree = new_tree(&mut cache);
    let mut model = Model::new();
    let mut rng = TestRng::new(seed);
    let mut deleted = 0;
    let mut tallest = 0;
    for round in 0..rounds {
        match rng.below(20) {
            0..=10 => {
                let (key, value) = random_entry(&mut rng, page_size);
                tree.put(&cache, &key, &value).unwrap();
                model.insert(key, value);
            }
            11..=17 => {
                let key = if model.is_empty() || rng.below(4) == 0 {
                    random_key(&mut rng, &model, page_size)
                } else {
                    model.keys().nth(rng.below(model.len())).unwrap().clone()
                };
                let removed = tree.delete(&cache, &key).unwrap();
                assert_eq!(removed, model.remove(&key).is_some(), "{key:?}");
                deleted += usize::from(removed);
            }
            18 => {
                let start = random_key(&mut rng, &model, page_size);
                let end = random_key(&mut rng, &model, page_size);
                let start = random_bound(&mut rng, &start);
                let end = random_bound(&mut rng, &end);
                let order = order(&mut rng);
                assert_scan(&tree, &cache, &model, start, end, order);
            }
            _ => {
                let key = random_key(&mut rng, &model, page_size);
                let prefix = &key[..rng.below(key.len() + 1).min(3)];
                let order = order(&mut rng);
                let found = collect(tree.scan_prefix(prefix, order), &cache).unwrap();
                let mut wanted: Entries = model
                    .iter()
                    .filter(|(key, _)| key.starts_with(prefix))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                if order == ScanOrder::Descending {
                    wanted.reverse();
                }
                assert_eq!(found, wanted, "{prefix:?}");
            }
        }
        if round % commit_every == commit_every - 1 {
            cache.commit(|_| {}).unwrap();
            tallest = tallest.max(tree.check(&cache).unwrap().height);
        }
        if round % 1500 == 1499 {
            cache.commit(|_| {}).unwrap();
            drop(cache);
            cache = open(&fs, Access::ReadWrite, frames);
            tree = stored_tree(&cache);
            assert_matches(&tree, &cache, &model);
            assert_full_scans(&tree, &cache, &model);
        }
    }
    assert!(deleted > rounds / 6, "{deleted}");
    assert!(tallest >= 2, "{tallest}");
    let mut keys: Vec<Vec<u8>> = model.keys().cloned().collect();
    for index in (1..keys.len()).rev() {
        keys.swap(index, rng.below(index + 1));
    }
    for (index, key) in keys.iter().enumerate() {
        assert!(tree.delete(&cache, key).unwrap());
        model.remove(key);
        if index % 97 == 0 {
            assert_matches(&tree, &cache, &model);
        }
    }
    cache.commit(|_| {}).unwrap();
    drop(cache);
    let cache = open(&fs, Access::ReadOnly, frames);
    let tree = stored_tree(&cache);
    let report = tree.check(&cache).unwrap();
    assert_eq!(report.entries, 0);
    assert_full_scans(&tree, &cache, &model);
}

#[test]
fn random_puts_deletes_and_scans_match_a_model_across_commits_and_reopens() {
    for seed in 0..3 {
        random_model_workload(PageSize::DEFAULT, 4096, seed, 300);
    }
}

#[test]
fn random_puts_deletes_and_scans_match_a_model_with_64_kib_pages_and_a_small_cache() {
    random_model_workload(PageSize::from_log2(16).unwrap(), 256, 21, 40);
}

fn numbered(number: u32) -> Vec<u8> {
    format!("key-{number:08}").into_bytes()
}

#[test]
fn deleting_every_key_merges_pages_shrinks_the_tree_to_its_root_and_frees_the_pages() {
    let count = 60_000u32;
    let orders: [Box<dyn Fn(u32) -> u32>; 3] = [
        Box::new(|index| index),
        Box::new(move |index| count - 1 - index),
        Box::new(move |index| index.wrapping_mul(7919) % count),
    ];
    for order in orders {
        let fs = SimFileSystem::new();
        let mut cache = create(&fs, PageSize::DEFAULT, 4096);
        cache.set_auto_checkpoint_bytes(None);
        let tree = new_tree(&mut cache);
        for number in 0..count {
            tree.put(&cache, &numbered(number), &number.to_le_bytes())
                .unwrap();
        }
        cache.commit(|_| {}).unwrap();
        let full = tree.check(&cache).unwrap();
        assert_eq!(full.height, 3, "{full:?}");
        for index in 0..count {
            assert!(tree.delete(&cache, &numbered(order(index))).unwrap());
            if index == count / 2 {
                let half = tree.check(&cache).unwrap();
                assert_eq!(half.entries, u64::from(count / 2 - 1));
                assert!(half.leaf_pages <= full.leaf_pages, "{half:?} {full:?}");
            }
        }
        assert!(!tree.delete(&cache, &numbered(0)).unwrap());
        cache.commit(|_| {}).unwrap();
        let empty = tree.check(&cache).unwrap();
        assert_eq!(
            (
                empty.height,
                empty.leaf_pages,
                empty.inner_pages,
                empty.entries
            ),
            (1, 1, 0, 0),
            "{empty:?}"
        );
        drop(cache);
        let freed = full.leaf_pages + full.inner_pages - 1;
        let available = |fs: &SimFileSystem| {
            PageFile::open(fs, Path::new(DB), Access::ReadOnly)
                .unwrap()
                .available_pages()
        };
        let free_when_empty = available(&fs);
        assert!(free_when_empty >= freed, "{free_when_empty} {freed}");
        let mut cache = open(&fs, Access::ReadWrite, 4096);
        cache.set_auto_checkpoint_bytes(None);
        let tree = stored_tree(&cache);
        for number in 0..count {
            tree.put(&cache, &numbered(number), b"again").unwrap();
        }
        cache.commit(|_| {}).unwrap();
        drop(cache);
        assert!(
            available(&fs) + freed <= free_when_empty + 2,
            "{} {free_when_empty} {freed}",
            available(&fs)
        );
        let cache = open(&fs, Access::ReadOnly, 64);
        let tree = stored_tree(&cache);
        assert_eq!(tree.check(&cache).unwrap().entries, u64::from(count));
    }
}

#[test]
fn deleting_a_missing_key_returns_false_and_changes_no_page() {
    let fs = SimFileSystem::new();
    let model = tree_with_entries(&fs, 4, 500);
    let cache = open(&fs, Access::ReadWrite, 64);
    let tree = stored_tree(&cache);
    for key in [&b"absent"[..], b"", &[0xFF; 600]] {
        assert!(!tree.delete(&cache, key).unwrap());
    }
    assert_eq!(cache.stats().dirty_pages, 0);
    let key = model.keys().nth(17).unwrap();
    assert!(tree.delete(&cache, key).unwrap());
    assert!(!tree.delete(&cache, key).unwrap());
    assert_eq!(tree.get(&cache, key).unwrap(), None);
    assert!(cache.stats().dirty_pages >= 1);
}

fn new_leaf(cache: &PageCache<SimFile>, node: &Node) -> u64 {
    let mut page = cache.allocate(header_for(node.level, 0)).unwrap();
    let page_id = page.page_id();
    node.encode(page_id, &mut page).unwrap();
    page_id
}

#[test]
fn a_merge_that_would_not_fit_once_the_prefix_shrinks_leaves_an_empty_leaf_in_place() {
    let fs = SimFileSystem::new();
    let mut cache = create(&fs, PageSize::DEFAULT, 512);
    let tree = new_tree(&mut cache);
    let shared = [&b"b"[..], &[b'x'; 300]].concat();
    let long_key = |number: u32| [&shared[..], format!("{number:05}").as_bytes()].concat();
    let leaf = |low: Vec<u8>, high: Option<Vec<u8>>, keys: Vec<Vec<u8>>| Node {
        level: 0,
        low,
        high,
        values: vec![Vec::new(); keys.len()],
        keys,
        children: Vec::new(),
    };
    let mut model = Model::new();
    let middle_keys: Vec<Vec<u8>> = (0..300).map(long_key).collect();
    for key in middle_keys.iter().chain([&b"a".to_vec()]) {
        model.insert(key.clone(), Vec::new());
    }
    let first = new_leaf(
        &cache,
        &leaf(Vec::new(), Some(long_key(0)), vec![b"a".to_vec()]),
    );
    let middle = new_leaf(
        &cache,
        &leaf(long_key(0), Some(long_key(9999)), middle_keys.clone()),
    );
    let last = new_leaf(&cache, &leaf(long_key(9999), None, Vec::new()));
    let root = Node {
        level: 1,
        low: Vec::new(),
        high: None,
        keys: vec![long_key(0), long_key(9999)],
        values: Vec::new(),
        children: vec![first, middle, last],
    };
    {
        let mut page = cache.write(tree.root()).unwrap();
        page.set_header(header_for(1, 0)).unwrap();
        root.encode(tree.root(), &mut page).unwrap();
    }
    cache.commit(|_| {}).unwrap();
    assert_matches(&tree, &cache, &model);
    assert!(tree.delete(&cache, b"a").unwrap());
    model.remove(&b"a"[..]);
    let report = tree.check(&cache).unwrap();
    assert_eq!(report.leaf_pages, 3, "{report:?}");
    assert_eq!(collect_leaf_entry_counts(&tree, &cache), [0, 300, 0]);
    assert_matches(&tree, &cache, &model);
    assert_full_scans(&tree, &cache, &model);
    for key in &middle_keys[10..] {
        assert!(tree.delete(&cache, key).unwrap());
        model.remove(key);
    }
    assert_eq!(collect_leaf_entry_counts(&tree, &cache), [10, 0]);
    cache.commit(|_| {}).unwrap();
    assert_matches(&tree, &cache, &model);
    assert_full_scans(&tree, &cache, &model);
}

fn collect_leaf_entry_counts(tree: &BTree, cache: &PageCache<SimFile>) -> Vec<usize> {
    let mut counts = Vec::new();
    let mut pending = vec![tree.root()];
    while let Some(page_id) = pending.pop() {
        let page = cache.read(page_id).unwrap();
        let shape = Shape::parse(page_id, page.header(), &page).unwrap();
        let node = Node::decode(&shape, &page).unwrap();
        if node.level == 0 {
            counts.push(node.keys.len());
        } else {
            pending.extend(node.children.iter().rev());
        }
    }
    counts
}

#[test]
fn every_kind_of_bound_in_both_orders_matches_a_model() {
    let fs = SimFileSystem::new();
    let mut cache = create(&fs, PageSize::DEFAULT, 512);
    let tree = new_tree(&mut cache);
    let mut model = Model::new();
    for number in (0..3000u32).step_by(3) {
        let key = numbered(number);
        let value = vec![number as u8; 40];
        tree.put(&cache, &key, &value).unwrap();
        model.insert(key, value);
    }
    assert!(tree.check(&cache).unwrap().leaf_pages > 10);
    let probes: Vec<Vec<u8>> = [0u32, 1, 3, 4, 1500, 1501, 2997, 2998, 2999, 5000]
        .iter()
        .map(|&number| numbered(number))
        .chain([Vec::new(), b"key-".to_vec(), vec![0xFF; 3]])
        .collect();
    let mut checked = 0;
    for start in &probes {
        for end in &probes {
            for start in [
                Bound::Included(&start[..]),
                Bound::Excluded(&start[..]),
                Bound::Unbounded,
            ] {
                for end in [
                    Bound::Included(&end[..]),
                    Bound::Excluded(&end[..]),
                    Bound::Unbounded,
                ] {
                    for order in [ScanOrder::Ascending, ScanOrder::Descending] {
                        assert_scan(&tree, &cache, &model, start, end, order);
                        checked += 1;
                    }
                }
            }
        }
    }
    assert_eq!(checked, 13 * 13 * 9 * 2);
}

#[test]
fn prefix_scans_return_exactly_the_keys_with_the_prefix() {
    let fs = SimFileSystem::new();
    let mut cache = create(&fs, PageSize::DEFAULT, 512);
    let tree = new_tree(&mut cache);
    let mut model = Model::new();
    let mut keys: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![0xFF],
        vec![0xFF, 0xFF],
        vec![0xFF, 0xFF, 0x00],
        vec![0xFE, 0xFF],
        vec![0x00],
        b"ab".to_vec(),
        b"ab\xFF".to_vec(),
        b"ac".to_vec(),
    ];
    for number in 0..2000u32 {
        keys.push(format!("p/{:x}", number % 97).into_bytes());
        keys.push(format!("p/{number}/x").into_bytes());
    }
    for key in keys {
        tree.put(&cache, &key, &key).unwrap();
        model.insert(key.clone(), key);
    }
    let prefixes: [&[u8]; 9] = [
        b"",
        b"\xFF",
        b"\xFF\xFF",
        b"\xFE",
        b"ab",
        b"p/",
        b"p/1",
        b"p/19",
        b"q",
    ];
    for prefix in prefixes {
        for order in [ScanOrder::Ascending, ScanOrder::Descending] {
            let found = collect(tree.scan_prefix(prefix, order), &cache).unwrap();
            let mut wanted: Vec<Vec<u8>> = model
                .keys()
                .filter(|key| key.starts_with(prefix))
                .cloned()
                .collect();
            if order == ScanOrder::Descending {
                wanted.reverse();
            }
            let found: Vec<Vec<u8>> = found.into_iter().map(|(key, _)| key).collect();
            assert_eq!(found, wanted, "{prefix:?} {order:?}");
        }
    }
}

#[test]
fn a_cursor_sees_changes_ahead_of_it_and_never_repeats_or_goes_back() {
    for order in [ScanOrder::Ascending, ScanOrder::Descending] {
        let fs = SimFileSystem::new();
        let mut cache = create(&fs, PageSize::DEFAULT, 512);
        let tree = new_tree(&mut cache);
        for number in 0..20_000u32 {
            tree.put(&cache, &numbered(number), &[3; 20]).unwrap();
        }
        let mut cursor = tree.scan(Bound::Unbounded, Bound::Unbounded, order);
        let mut rng = TestRng::new(8);
        let mut previous: Option<Vec<u8>> = None;
        let mut returned = 0;
        while let Some((key, _)) = cursor.next(&cache).unwrap() {
            let key = key.to_vec();
            if let Some(previous) = &previous {
                match order {
                    ScanOrder::Ascending => assert!(key > *previous),
                    ScanOrder::Descending => assert!(key < *previous),
                }
            }
            for _ in 0..3 {
                let number = rng.below(20_000) as u32;
                if rng.below(2) == 0 {
                    tree.delete(&cache, &numbered(number)).unwrap();
                } else {
                    tree.put(&cache, &numbered(number), &[4; 30]).unwrap();
                }
            }
            previous = Some(key);
            returned += 1;
        }
        assert!(returned > 5000, "{returned}");
        tree.check(&cache).unwrap();
    }
}

#[test]
fn a_cursor_reports_a_leaf_held_for_writing_and_resumes_where_it_was() {
    let fs = SimFileSystem::new();
    let model = tree_with_entries(&fs, 9, 600);
    let cache = open(&fs, Access::ReadWrite, 64);
    let tree = stored_tree(&cache);
    let leaves = leaf_pages(&tree, &cache);
    assert!(leaves.len() > 4);
    let mut held = Some(cache.write(leaves[2]).unwrap());
    let mut cursor = tree.scan(Bound::Unbounded, Bound::Unbounded, ScanOrder::Ascending);
    let mut found = Vec::new();
    let mut refused = 0;
    loop {
        match cursor.next(&cache) {
            Ok(Some((key, value))) => found.push((key.to_vec(), value.to_vec())),
            Ok(None) => break,
            Err(StorageError::PageBorrowed { page }) => {
                assert_eq!(page, leaves[2]);
                refused += 1;
                held = None;
            }
            Err(error) => panic!("{error}"),
        }
    }
    assert!(held.is_none());
    assert_eq!(refused, 1);
    let wanted: Entries = model.into_iter().collect();
    assert_eq!(found, wanted);
}

fn leaf_pages(tree: &BTree, cache: &PageCache<SimFile>) -> Vec<u64> {
    let mut leaves = Vec::new();
    let mut pending = vec![tree.root()];
    while let Some(page_id) = pending.pop() {
        let page = cache.read(page_id).unwrap();
        let shape = Shape::parse(page_id, page.header(), &page).unwrap();
        let node = Node::decode(&shape, &page).unwrap();
        if node.level == 0 {
            leaves.push(page_id);
        } else {
            pending.extend(node.children.iter().rev());
        }
    }
    leaves
}

fn shrink(fs: &SimFileSystem, model: &mut Model, deletes: usize, fail_at: Option<u64>) -> u64 {
    let mut cache = open(fs, Access::ReadWrite, 512);
    let tree = stored_tree(&cache);
    let mut rng = TestRng::new(31);
    for _ in 0..deletes {
        let key = model.keys().nth(rng.below(model.len())).unwrap().clone();
        assert!(tree.delete(&cache, &key).unwrap());
        model.remove(&key);
    }
    let start = fs.operation_count();
    if let Some(offset) = fail_at {
        fs.fail_operation(start + offset);
    }
    let committed = cache.commit(|_| {});
    assert_eq!(committed.is_ok(), fail_at.is_none());
    fs.operation_count() - start
}

#[test]
fn a_crash_at_any_operation_of_a_commit_that_merges_pages_leaves_the_old_or_the_new_tree() {
    let (entries, deletes) = (400, 360);
    let (operations, before, after) = {
        let fs = SimFileSystem::new();
        let mut model = tree_with_entries(&fs, 2, entries);
        let before = committed_report(&fs);
        let operations = shrink(&fs, &mut model, deletes, None);
        (operations, before, committed_report(&fs))
    };
    assert!(before.height > after.height, "{before:?} {after:?}");
    assert!(
        before.leaf_pages > after.leaf_pages + 5,
        "{before:?} {after:?}"
    );
    let mut outcomes = std::collections::BTreeSet::new();
    for step in 0..operations {
        for seed in 0..4 {
            let fs = SimFileSystem::new();
            let old = tree_with_entries(&fs, 2, entries);
            let mut new = old.clone();
            shrink(&fs, &mut new, deletes, Some(step));
            fs.crash(CrashMode::Torn { seed });
            let cache = open(&fs, Access::ReadOnly, 64);
            let tree = stored_tree(&cache);
            let found = tree.check(&cache).unwrap().entries;
            let expected = if found == new.len() as u64 {
                &new
            } else {
                &old
            };
            assert_matches(&tree, &cache, expected);
            assert_full_scans(&tree, &cache, expected);
            outcomes.insert(found == new.len() as u64);
        }
    }
    assert_eq!(outcomes.len(), 2);
}

fn rewrite_page(fs: &SimFileSystem, page_id: Option<u64>, change: impl FnOnce(&mut Node)) {
    let mut cache = open(fs, Access::ReadWrite, 64);
    let page_id = page_id.unwrap_or(cache.superblock().catalog_root);
    {
        let mut page = cache.write(page_id).unwrap();
        let header = page.header();
        let mut body = page.to_vec();
        let shape = Shape::parse(page_id, header, &body).unwrap();
        let mut node = Node::decode(&shape, &body).unwrap();
        change(&mut node);
        node.encode(page_id, &mut body).unwrap();
        page.copy_from_slice(&body);
    }
    cache.commit(|_| {}).unwrap();
}

fn is_corrupt_tree(error: &StorageError) -> bool {
    matches!(error, StorageError::Format(FormatError::CorruptTree { .. }))
}

#[test]
fn a_root_with_a_single_child_is_refused_by_check_and_collapsed_by_a_delete() {
    let fs = SimFileSystem::new();
    let model = tree_with_entries(&fs, 6, 400);
    let mut kept = None;
    rewrite_page(&fs, None, |node| {
        assert_eq!(node.level, 1);
        node.keys.clear();
        kept = node.children.pop();
        node.children = vec![kept.unwrap()];
    });
    let cache = open(&fs, Access::ReadWrite, 64);
    let tree = stored_tree(&cache);
    let error = tree.check(&cache).unwrap_err();
    assert!(is_corrupt_tree(&error), "{error}");
    let error = collect(
        tree.scan(Bound::Unbounded, Bound::Unbounded, ScanOrder::Ascending),
        &cache,
    )
    .unwrap_err();
    assert!(is_corrupt_tree(&error), "{error}");
    assert!(model.keys().any(|key| {
        tree.delete(&cache, key)
            .is_err_and(|error| is_corrupt_tree(&error))
    }));
}

#[test]
fn a_right_edge_leaf_with_a_high_fence_key_fails_scans_with_an_error() {
    let fs = SimFileSystem::new();
    tree_with_entries(&fs, 6, 400);
    let rightmost = {
        let cache = open(&fs, Access::ReadOnly, 64);
        *leaf_pages(&stored_tree(&cache), &cache).last().unwrap()
    };
    rewrite_page(&fs, Some(rightmost), |node| {
        assert_eq!(node.high, None);
        node.high = Some(vec![0xFF; 20]);
    });
    let cache = open(&fs, Access::ReadOnly, 64);
    let tree = stored_tree(&cache);
    for order in [ScanOrder::Ascending, ScanOrder::Descending] {
        let error =
            collect(tree.scan(Bound::Unbounded, Bound::Unbounded, order), &cache).unwrap_err();
        assert!(is_corrupt_tree(&error), "{error}");
    }
}

#[test]
fn deletes_and_scans_of_damaged_trees_fail_with_errors_and_never_panic_or_loop() {
    let fs = SimFileSystem::new();
    let model = tree_with_entries(&fs, 12, 500);
    let keys: Vec<Vec<u8>> = model.keys().cloned().collect();
    let pages = crate::btree_tests::tree_pages(&open(&fs, Access::ReadOnly, 64));
    let mut rng = TestRng::new(13);
    let mut refused = 0;
    for _ in 0..300 {
        let target = pages[rng.below(pages.len())];
        let original = {
            let mut cache = open(&fs, Access::ReadWrite, 64);
            let mut page = cache.write(target).unwrap();
            let original = (page.header(), page.to_vec());
            let mut header = page.header();
            crate::btree_tests::mutate(&mut rng, &mut header, &mut page);
            page.set_header(header).unwrap();
            drop(page);
            cache.commit(|_| {}).unwrap();
            original
        };
        {
            let mut cache = open(&fs, Access::ReadWrite, 64);
            let start = keys[rng.below(keys.len())].clone();
            let found = BTree::open(&cache, cache.superblock().catalog_root).and_then(|tree| {
                for order in [ScanOrder::Ascending, ScanOrder::Descending] {
                    collect(tree.scan(Bound::Unbounded, Bound::Unbounded, order), &cache)?;
                    collect(
                        tree.scan(Bound::Excluded(&start), Bound::Unbounded, order),
                        &cache,
                    )?;
                    collect(tree.scan_prefix(&start[..1], order), &cache)?;
                }
                for key in keys.iter().step_by(3) {
                    tree.delete(&cache, key)?;
                }
                tree.check(&cache)
            });
            if let Err(error) = found {
                assert!(
                    matches!(
                        error,
                        StorageError::Format(_)
                            | StorageError::PageOutOfRange { .. }
                            | StorageError::PageNotInUse { .. }
                            | StorageError::DoubleFree { .. }
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
fn random_overwrites_and_deletes_keep_values_as_long_as_the_model() {
    let fs = SimFileSystem::new();
    let mut cache = create(&fs, PageSize::DEFAULT, 256);
    let tree = new_tree(&mut cache);
    let mut rng = TestRng::new(5);
    let mut model = Model::new();
    for round in 0..4000 {
        let key = vec![b'k', rng.below(64) as u8];
        if rng.below(3) == 0 {
            assert_eq!(
                tree.delete(&cache, &key).unwrap(),
                model.remove(&key).is_some()
            );
        } else {
            let value = bytes_up_to(&mut rng, 0, 300);
            tree.put(&cache, &key, &value).unwrap();
            model.insert(key, value);
        }
        if round % 200 == 0 {
            assert_matches(&tree, &cache, &model);
            assert_full_scans(&tree, &cache, &model);
        }
    }
}

fn decoded(cache: &PageCache<SimFile>, page_id: u64) -> Node {
    let page = cache.read(page_id).unwrap();
    let shape = Shape::parse(page_id, page.header(), &page).unwrap();
    Node::decode(&shape, &page).unwrap()
}

#[test]
fn a_leaf_whose_fence_keys_differ_from_its_parent_s_bounds_fails_scans_instead_of_skipping_entries()
{
    for order in [ScanOrder::Ascending, ScanOrder::Descending] {
        let fs = SimFileSystem::new();
        tree_with_entries(&fs, 6, 400);
        let (leaves, neighbour) = {
            let cache = open(&fs, Access::ReadOnly, 64);
            let leaves = leaf_pages(&stored_tree(&cache), &cache);
            let neighbour = decoded(&cache, leaves[1]);
            (leaves, neighbour)
        };
        match order {
            ScanOrder::Ascending => rewrite_page(&fs, Some(leaves[0]), |node| {
                node.high = neighbour.high.clone();
            }),
            ScanOrder::Descending => rewrite_page(&fs, Some(leaves[2]), |node| {
                node.low = neighbour.low.clone();
            }),
        }
        let cache = open(&fs, Access::ReadOnly, 64);
        let tree = stored_tree(&cache);
        let error =
            collect(tree.scan(Bound::Unbounded, Bound::Unbounded, order), &cache).unwrap_err();
        assert!(is_corrupt_tree(&error), "{order:?}: {error}");
        assert!(
            tree.check(&cache)
                .is_err_and(|error| is_corrupt_tree(&error))
        );
    }
}

#[test]
fn bytes_written_through_a_page_handle_are_checked_again_before_the_tree_trusts_them() {
    let fs = SimFileSystem::new();
    let model = tree_with_entries(&fs, 6, 400);
    let cache = open(&fs, Access::ReadWrite, 64);
    let tree = stored_tree(&cache);
    assert_matches(&tree, &cache, &model);
    let leaf = leaf_pages(&tree, &cache)[0];
    let mut node = decoded(&cache, leaf);
    node.keys.swap(0, 1);
    {
        let mut page = cache.write(leaf).unwrap();
        let mut body = page.to_vec();
        node.encode(leaf, &mut body).unwrap();
        page.copy_from_slice(&body);
    }
    let first = model.keys().next().unwrap();
    let error = tree.get(&cache, first).unwrap_err();
    assert!(is_corrupt_tree(&error), "{error}");
}
