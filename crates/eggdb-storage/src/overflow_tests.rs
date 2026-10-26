use crate::btree::BTree;
use crate::btree_tests::tree_pages;
use crate::bytes::{read_u32, read_u64, write_u32, write_u64};
use crate::cursor::ScanOrder;
use crate::error::{FormatError, StorageError};
use crate::format::{Access, FileId, NO_PAGE, PageHeader, PageSize, PageType};
use crate::overflow::{self, OverflowReference};
use crate::page_cache::{CacheSize, PageCache};
use crate::page_file::{CreateOptions, PageFile};
use crate::page_set::PageSet;
use crate::store::{FileReport, ReadTransaction, Store};
use crate::test_rng::TestRng;
use crate::tree_page::{Node, Shape, StoredValue, header_for};
use eggdb_platform::{File, FileSystem, OpenMode, SimClock, SimFile, SimFileSystem, SimMemory};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

pub(crate) const DB: &str = "db";
pub(crate) const FILE_ID: FileId = FileId(*b"overflow-tests!!");

const SHORT_COUNT: &str =
    "the page holds another number of the value's bytes than the value's length says";
const MISPLACED: &str = "the page belongs to another value or another place in the chain";

pub(crate) type Model = BTreeMap<Vec<u8>, Vec<u8>>;
pub(crate) type Entries = Vec<(Vec<u8>, Vec<u8>)>;
type KeyBounds<'a> = (Bound<&'a [u8]>, Bound<&'a [u8]>);

pub(crate) fn created_store(
    fs: &SimFileSystem,
    page_size: PageSize,
    frames: usize,
) -> (Store<SimFile>, BTree) {
    let options = CreateOptions {
        page_size,
        file_id: FILE_ID,
    };
    let file = PageFile::create(fs, Path::new(DB), options).unwrap();
    let cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(frames)).unwrap();
    let store = Store::new(cache, Arc::new(SimClock::new(0)));
    let mut write = store.begin_write().unwrap();
    let tree = write.create_tree().unwrap();
    write.set_catalog_root(tree.root());
    write.commit().unwrap();
    (store, tree)
}

pub(crate) fn opened_cache(
    fs: &SimFileSystem,
    access: Access,
    frames: usize,
) -> PageCache<SimFile> {
    let file = PageFile::open(fs, Path::new(DB), access).unwrap();
    PageCache::new(file, &SimMemory::new(), CacheSize::Pages(frames)).unwrap()
}

pub(crate) fn opened_store(
    fs: &SimFileSystem,
    access: Access,
    frames: usize,
) -> (Store<SimFile>, BTree) {
    let store = Store::new(opened_cache(fs, access, frames), Arc::new(SimClock::new(0)));
    let root = store.superblock().unwrap().catalog_root;
    let tree = store.begin_read().unwrap().open_tree(root).unwrap();
    (store, tree)
}

pub(crate) fn per_page(page_size: PageSize) -> usize {
    overflow::data_bytes_per_page(page_size)
}

pub(crate) fn patterned(len: usize, seed: u64) -> Vec<u8> {
    TestRng::new(seed).bytes(len)
}

fn spills(page_size: PageSize, key: &[u8], value: &[u8]) -> bool {
    key.len() + value.len() > BTree::max_inline_entry_bytes(page_size)
}

pub(crate) fn commit_changes(
    store: &Store<SimFile>,
    tree: &BTree,
    model: &mut Model,
    puts: &[(Vec<u8>, Vec<u8>)],
    deletes: &[Vec<u8>],
) {
    let mut write = store.begin_write().unwrap();
    for (key, value) in puts {
        write.put(tree, key, value).unwrap();
        model.insert(key.clone(), value.clone());
    }
    for key in deletes {
        assert_eq!(
            write.delete(tree, key).unwrap(),
            model.remove(key).is_some()
        );
    }
    write.commit().unwrap();
}

pub(crate) fn scanned(
    read: &ReadTransaction<'_, SimFile>,
    tree: &BTree,
    order: ScanOrder,
) -> Result<Entries, StorageError> {
    let mut cursor = tree.scan(Bound::Unbounded, Bound::Unbounded, order);
    let mut entries = Vec::new();
    while let Some((key, value)) = read.next(&mut cursor)? {
        entries.push((key.to_vec(), value.to_vec()));
    }
    Ok(entries)
}

fn assert_reads(read: &ReadTransaction<'_, SimFile>, tree: &BTree, model: &Model) {
    for (key, value) in model {
        assert_eq!(
            read.get(tree, key).unwrap().as_ref(),
            Some(value),
            "{key:?}"
        );
        let length = read.get_with(tree, key, <[u8]>::len).unwrap();
        assert_eq!(length, Some(value.len()));
    }
    let expected: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert!(scanned(read, tree, ScanOrder::Ascending).unwrap() == expected);
    let mut backward = scanned(read, tree, ScanOrder::Descending).unwrap();
    backward.reverse();
    assert!(backward == expected);
}

pub(crate) fn assert_holds(store: &Store<SimFile>, tree: &BTree, model: &Model) -> FileReport {
    let page_size = store.superblock().unwrap().page_size;
    assert_reads(&store.begin_read().unwrap(), tree, model);
    let report = store.check().unwrap();
    let tree_report = report.tree.unwrap();
    let spilled: Vec<&Vec<u8>> = model
        .iter()
        .filter(|(key, value)| spills(page_size, key, value))
        .map(|(_, value)| value)
        .collect();
    assert_eq!(tree_report.entries, model.len() as u64);
    assert_eq!(
        tree_report.value_bytes,
        model.values().map(|value| value.len() as u64).sum::<u64>()
    );
    assert_eq!(tree_report.overflow_values, spilled.len() as u64);
    assert_eq!(
        tree_report.overflow_pages,
        spilled
            .iter()
            .map(|value| overflow::pages_for(value.len() as u64, page_size))
            .sum::<u64>()
    );
    assert_eq!(report.leaked_pages, 0, "{report:?}");
    assert_eq!(
        page_size.first_data_page()
            + tree_report.inner_pages
            + tree_report.leaf_pages
            + tree_report.overflow_pages
            + report.free_pages
            + report.log_pages,
        report.page_count,
        "{report:?}"
    );
    report
}

fn boundary_lengths(page_size: PageSize, key_len: usize) -> Vec<usize> {
    let inline = BTree::max_inline_entry_bytes(page_size) - key_len;
    let page = per_page(page_size);
    vec![
        0,
        1,
        inline - 1,
        inline,
        inline + 1,
        page - 1,
        page,
        page + 1,
        2 * page,
        2 * page + 1,
        3 * page - 1,
        7 * page,
        10 * page + 7,
    ]
}

#[test]
fn values_at_every_size_boundary_round_trip_through_a_reopen() {
    for log2 in [12, 16] {
        let page_size = PageSize::from_log2(log2).unwrap();
        let fs = SimFileSystem::new();
        let (store, tree) = created_store(&fs, page_size, 256);
        let mut model = Model::new();
        let empty_key_value = patterned(BTree::max_inline_entry_bytes(page_size) + 1, 9);
        commit_changes(
            &store,
            &tree,
            &mut model,
            &[(Vec::new(), empty_key_value)],
            &[],
        );
        for (kind, key_len) in [1, 2, BTree::max_key_bytes(page_size)]
            .into_iter()
            .enumerate()
        {
            let mut puts = Vec::new();
            for (number, len) in boundary_lengths(page_size, key_len).into_iter().enumerate() {
                let mut key = vec![b'a' + kind as u8; key_len - 1];
                key.push(number as u8);
                let seed = u64::from(log2) << 16 | (kind as u64) << 8 | number as u64;
                puts.push((key, patterned(len, seed)));
            }
            commit_changes(&store, &tree, &mut model, &puts, &[]);
            assert_holds(&store, &tree, &model);
        }
        let report = assert_holds(&store, &tree, &model).tree.unwrap();
        assert_eq!(report.overflow_values, 28, "{report:?}");
        drop(store);
        let (store, tree) = opened_store(&fs, Access::ReadOnly, 64);
        assert_holds(&store, &tree, &model);
    }
}

#[test]
fn replacing_small_and_large_values_in_both_directions_frees_every_old_chain() {
    let fs = SimFileSystem::new();
    let page_size = PageSize::DEFAULT;
    let (store, tree) = created_store(&fs, page_size, 128);
    let mut model = Model::new();
    let page = per_page(page_size);
    let lengths = [
        10,
        3 * page,
        5 * page + 1,
        20,
        0,
        page + 9,
        page + 9,
        2 * page,
        700,
    ];
    for (round, len) in lengths.into_iter().enumerate() {
        let puts = [
            (b"a".to_vec(), patterned(len, round as u64)),
            (b"b".to_vec(), patterned(len / 2, 100 + round as u64)),
        ];
        commit_changes(&store, &tree, &mut model, &puts, &[]);
        assert_holds(&store, &tree, &model);
    }
    commit_changes(
        &store,
        &tree,
        &mut model,
        &[],
        &[b"a".to_vec(), b"b".to_vec()],
    );
    let report = assert_holds(&store, &tree, &model);
    assert_eq!(report.tree.unwrap().overflow_pages, 0);
    drop(store);
    let (store, tree) = opened_store(&fs, Access::ReadWrite, 128);
    assert_holds(&store, &tree, &model);
}

#[test]
fn a_rollback_keeps_the_committed_large_values_and_their_pages() {
    let fs = SimFileSystem::new();
    let page_size = PageSize::DEFAULT;
    let (store, tree) = created_store(&fs, page_size, 128);
    let mut model = Model::new();
    let page = per_page(page_size);
    let puts: Vec<_> = (0..4u8)
        .map(|index| {
            (
                vec![b'k', index],
                patterned(3 * page + 11, u64::from(index)),
            )
        })
        .collect();
    commit_changes(&store, &tree, &mut model, &puts, &[]);
    let before = assert_holds(&store, &tree, &model);
    {
        let mut write = store.begin_write().unwrap();
        write
            .put(&tree, b"k\x00", &patterned(9 * page, 50))
            .unwrap();
        write.put(&tree, b"k\x01", b"small now").unwrap();
        assert!(write.delete(&tree, b"k\x02").unwrap());
        write.put(&tree, b"new", &patterned(4 * page, 51)).unwrap();
        assert_eq!(
            write.get(&tree, b"k\x00").unwrap(),
            Some(patterned(9 * page, 50))
        );
        write.rollback().unwrap();
    }
    let after = assert_holds(&store, &tree, &model);
    assert_eq!(
        (after.page_count, after.free_pages),
        (before.page_count, before.free_pages)
    );
    {
        let mut write = store.begin_write().unwrap();
        write
            .put(&tree, b"k\x03", &patterned(6 * page, 52))
            .unwrap();
    }
    assert_holds(&store, &tree, &model);
}

#[test]
fn a_reader_keeps_reading_old_large_values_while_a_writer_replaces_deletes_and_reuses_their_pages()
{
    let fs = SimFileSystem::new();
    let page_size = PageSize::DEFAULT;
    let (store, tree) = created_store(&fs, page_size, 256);
    store.set_snapshot_memory(CacheSize::Pages(1024)).unwrap();
    let page = per_page(page_size);
    let mut model = Model::new();
    let puts: Vec<_> = (0..6u8)
        .map(|index| {
            let len = (usize::from(index) + 2) * page;
            (vec![b'v', index], patterned(len, 200 + u64::from(index)))
        })
        .collect();
    commit_changes(&store, &tree, &mut model, &puts, &[]);
    let old_model = model.clone();
    let freed_chains: Vec<u64> = store.with_cache(|cache| {
        [vec![b'v', 2], vec![b'v', 3]]
            .iter()
            .flat_map(|key| chain_of(cache, key))
            .collect()
    });
    let reader = store.begin_read().unwrap();
    let replaced = [
        (vec![b'v', 0], patterned(4 * page + 3, 300)),
        (vec![b'v', 1], b"short".to_vec()),
    ];
    let deleted = [vec![b'v', 2], vec![b'v', 3]];
    commit_changes(&store, &tree, &mut model, &replaced, &deleted);
    let middle_reader = store.begin_read().unwrap();
    let middle_model = model.clone();
    let reusing: Vec<_> = (0..4u8)
        .map(|index| {
            (
                vec![b'w', index],
                patterned(5 * page, 400 + u64::from(index)),
            )
        })
        .collect();
    commit_changes(&store, &tree, &mut model, &reusing, &[vec![b'v', 4]]);
    let reused = store.with_cache(|cache| {
        (0..4u8)
            .flat_map(|index| chain_of(cache, &[b'w', index]))
            .filter(|page| freed_chains.contains(page))
            .count()
    });
    assert_eq!(reused, freed_chains.len());
    assert_reads(&reader, &tree, &old_model);
    assert_reads(&middle_reader, &tree, &middle_model);
    assert_eq!(reader.check(&tree).unwrap().overflow_values, 6);
    drop(reader);
    drop(middle_reader);
    let report = assert_holds(&store, &tree, &model);
    assert_eq!(store.cache_stats().unwrap().before_images, 0);
    assert!(report.free_pages > 0, "{report:?}");
}

#[test]
fn scans_return_large_values_in_order_in_both_directions_and_release_large_buffers() {
    let fs = SimFileSystem::new();
    let page_size = PageSize::DEFAULT;
    let (store, tree) = created_store(&fs, page_size, 1024);
    let page = per_page(page_size);
    let mut model = Model::new();
    let mut rng = TestRng::new(77);
    for index in 0..120u32 {
        let len = match index % 7 {
            0 if index % 63 == 0 => 300 * page,
            0 | 1 | 4 => page + rng.below(3 * page),
            _ => rng.below(80),
        };
        let puts = [(index.to_be_bytes().to_vec(), rng.bytes(len))];
        commit_changes(&store, &tree, &mut model, &puts, &[]);
    }
    let read = store.begin_read().unwrap();
    assert_reads(&read, &tree, &model);
    let ranges: [KeyBounds<'_>; 4] = [
        (
            Bound::Included(&[0, 0, 0, 7]),
            Bound::Excluded(&[0, 0, 0, 50]),
        ),
        (
            Bound::Excluded(&[0, 0, 0, 7]),
            Bound::Included(&[0, 0, 0, 50]),
        ),
        (Bound::Unbounded, Bound::Included(&[0, 0, 0, 1])),
        (Bound::Included(&[0, 0, 0, 119]), Bound::Unbounded),
    ];
    for (start, end) in ranges {
        let expected: Vec<_> = model
            .range::<[u8], _>((start, end))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for order in [ScanOrder::Ascending, ScanOrder::Descending] {
            let mut cursor = tree.scan(start, end, order);
            let mut found = Vec::new();
            while let Some((key, value)) = read.next(&mut cursor).unwrap() {
                found.push((key.to_vec(), value.to_vec()));
            }
            if order == ScanOrder::Descending {
                found.reverse();
            }
            assert!(found == expected, "{start:?} {end:?} {order:?}");
        }
    }
    let mut cursor = tree.scan(Bound::Unbounded, Bound::Unbounded, ScanOrder::Ascending);
    assert_eq!(read.next(&mut cursor).unwrap().unwrap().1.len(), 300 * page);
    assert!(cursor.buffer_capacity() >= 300 * page);
    read.next(&mut cursor).unwrap().unwrap();
    assert!(
        cursor.buffer_capacity() < 1 << 20,
        "{}",
        cursor.buffer_capacity()
    );
}

#[test]
fn rewriting_large_values_reuses_freed_pages_instead_of_growing_the_file() {
    let fs = SimFileSystem::new();
    let (store, tree) = created_store(&fs, PageSize::DEFAULT, 512);
    let mut model = Model::new();
    let value_bytes = 100 << 10;
    let mut page_counts = Vec::new();
    for round in 0..60u64 {
        let puts = [(vec![b'k', (round % 4) as u8], patterned(value_bytes, round))];
        commit_changes(&store, &tree, &mut model, &puts, &[]);
        page_counts.push(store.superblock().unwrap().page_count);
    }
    let settled = page_counts[20];
    assert!(
        page_counts[20..].iter().all(|&count| count == settled),
        "{page_counts:?}"
    );
    let keys: Vec<Vec<u8>> = model.keys().cloned().collect();
    commit_changes(&store, &tree, &mut model, &[], &keys);
    for round in 0..20u64 {
        let len = value_bytes - round as usize * 409;
        let puts = [(vec![b'n', (round % 4) as u8], patterned(len, 1000 + round))];
        commit_changes(&store, &tree, &mut model, &puts, &[]);
        page_counts.push(store.superblock().unwrap().page_count);
    }
    let report = assert_holds(&store, &tree, &model);
    assert!(
        page_counts.iter().all(|&count| count <= settled),
        "{page_counts:?}"
    );
    eprintln!(
        "100 KiB values rewritten 80 times: the file settled at {settled} pages after 20 commits and never grew; {} pages free at the end",
        report.free_pages
    );
}

#[test]
fn a_value_needing_more_pages_than_the_cache_is_refused_and_the_transaction_stays_open() {
    let fs = SimFileSystem::new();
    let page_size = PageSize::DEFAULT;
    let (store, tree) = created_store(&fs, page_size, 40);
    let page = per_page(page_size);
    let mut write = store.begin_write().unwrap();
    write.put(&tree, b"small", b"1").unwrap();
    let error = write
        .put(&tree, b"big", &vec![1; 39 * page + 1])
        .unwrap_err();
    assert!(
        matches!(
            error,
            StorageError::ValueExceedsCache {
                pages: 41,
                frames: 40,
                ..
            }
        ),
        "{error}"
    );
    assert!(error.to_string().contains("larger cache"));
    write.put(&tree, b"fits", &vec![2; 30 * page]).unwrap();
    write.commit().unwrap();
    let mut model = Model::new();
    model.insert(b"small".to_vec(), b"1".to_vec());
    model.insert(b"fits".to_vec(), vec![2; 30 * page]);
    assert_holds(&store, &tree, &model);
    let mut write = store.begin_write().unwrap();
    let mut outcome = Ok(());
    for index in 0..10u8 {
        outcome = write.put(&tree, &[b'x', index], &vec![index; 12 * page]);
        if outcome.is_err() {
            break;
        }
    }
    assert!(matches!(outcome, Err(StorageError::CacheFull { .. })));
    assert!(matches!(
        write.put(&tree, b"after", b""),
        Err(StorageError::TransactionFailed)
    ));
    drop(write);
    assert_holds(&store, &tree, &model);
}

pub(crate) fn chains(cache: &PageCache<SimFile>) -> BTreeMap<u64, Vec<u64>> {
    let mut chains: BTreeMap<u64, BTreeMap<u32, u64>> = BTreeMap::new();
    for page_id in cache.page_size().first_data_page()..cache.superblock().page_count {
        let Ok(page) = cache.read(page_id) else {
            continue;
        };
        if page.header().page_type == PageType::Overflow {
            chains
                .entry(read_u64(&page, 8))
                .or_default()
                .insert(read_u32(&page, 16), page_id);
        }
    }
    chains
        .into_iter()
        .map(|(first, pages)| (first, pages.into_values().collect()))
        .collect()
}

fn leaf_holding(cache: &PageCache<SimFile>, key: &[u8]) -> (u64, Node) {
    for page_id in tree_pages(cache) {
        let page = cache.read(page_id).unwrap();
        let shape = Shape::parse(page_id, page.header(), &page).unwrap();
        let node = Node::decode(&shape, &page).unwrap();
        if shape.is_leaf() && node.keys.iter().any(|found| found == key) {
            return (page_id, node);
        }
    }
    panic!("no leaf holds {key:?}");
}

pub(crate) fn stored_value(cache: &PageCache<SimFile>, key: &[u8]) -> StoredValue {
    let (_, node) = leaf_holding(cache, key);
    let index = node.keys.iter().position(|found| found == key).unwrap();
    node.values[index].clone()
}

pub(crate) fn chain_of(cache: &PageCache<SimFile>, key: &[u8]) -> Vec<u64> {
    let StoredValue::Overflow(reference) = stored_value(cache, key) else {
        panic!("{key:?} is not a large value");
    };
    overflow::value_pages(cache, &reference).unwrap()
}

fn rewrite_leaf_entry(
    cache: &PageCache<SimFile>,
    key: &[u8],
    change: impl FnOnce(&mut StoredValue),
) {
    let (page_id, mut node) = leaf_holding(cache, key);
    let index = node.keys.iter().position(|found| found == key).unwrap();
    change(&mut node.values[index]);
    let mut page = cache.write(page_id).unwrap();
    node.encode(page_id, &mut page).unwrap();
}

fn rewrite_page(cache: &PageCache<SimFile>, page_id: u64, change: impl FnOnce(&mut [u8])) {
    let mut page = cache.write(page_id).unwrap();
    change(&mut page);
}

fn damaged(damage: impl FnOnce(&PageCache<SimFile>)) -> (SimFileSystem, Model) {
    let fs = SimFileSystem::new();
    let model = three_large_values(&fs);
    let mut cache = opened_cache(&fs, Access::ReadWrite, 128);
    damage(&cache);
    cache.commit(|_| {}).unwrap();
    (fs, model)
}

fn three_large_values(fs: &SimFileSystem) -> Model {
    let page = per_page(PageSize::DEFAULT);
    let (store, tree) = created_store(fs, PageSize::DEFAULT, 128);
    let mut model = Model::new();
    let puts = [
        (b"a".to_vec(), patterned(4 * page + 100, 1)),
        (b"b".to_vec(), patterned(3 * page, 2)),
        (b"c".to_vec(), b"inline".to_vec()),
        (b"d".to_vec(), patterned(2 * page + 1, 3)),
    ];
    commit_changes(&store, &tree, &mut model, &puts, &[]);
    store.checkpoint().unwrap();
    model
}

fn assert_refused(
    fs: &SimFileSystem,
    model: &Model,
    key: &[u8],
    expected: impl Fn(&StorageError) -> bool,
) {
    let (store, tree) = opened_store(fs, Access::ReadOnly, 64);
    let read = store.begin_read().unwrap();
    let error = read.get(&tree, key).unwrap_err();
    assert!(expected(&error), "get: {error}");
    let error = scanned(&read, &tree, ScanOrder::Ascending).unwrap_err();
    assert!(expected(&error), "scan: {error}");
    let error = scanned(&read, &tree, ScanOrder::Descending).unwrap_err();
    assert!(expected(&error), "reverse scan: {error}");
    let error = read.check(&tree).unwrap_err();
    assert!(expected(&error), "check: {error}");
    let error = store.check().unwrap_err();
    assert!(expected(&error), "store check: {error}");
    for (other, value) in model.iter().filter(|(other, _)| other.as_slice() != key) {
        assert_eq!(read.get(&tree, other).unwrap().as_ref(), Some(value));
    }
}

fn corrupt_overflow(problem: &'static str) -> impl Fn(&StorageError) -> bool {
    move |error| {
        matches!(
            error,
            StorageError::Format(FormatError::CorruptOverflow { problem: found, .. })
                if *found == problem
        )
    }
}

#[test]
fn a_damaged_byte_in_a_chain_page_fails_its_checksum() {
    let fs = SimFileSystem::new();
    let model = three_large_values(&fs);
    let chain = chain_of(&opened_cache(&fs, Access::ReadOnly, 64), b"a");
    let raw = fs.open(Path::new(DB), OpenMode::ReadWrite).unwrap();
    raw.write_all_at(&[0xAB], chain[2] * 4096 + 3000).unwrap();
    drop(raw);
    assert_refused(&fs, &model, b"a", |error| {
        matches!(
            error,
            StorageError::Format(FormatError::ChecksumMismatch { page }) if *page == chain[2]
        )
    });
}

#[test]
fn a_chain_cut_short_bent_into_a_cycle_or_pointing_elsewhere_is_refused() {
    type Change = fn(&mut [u8], &[u64]);
    let cases: [(Change, &str); 6] = [
        (
            |body, _| write_u64(body, 0, NO_PAGE),
            "the chain ends before the value does",
        ),
        (|body, chain| write_u64(body, 0, chain[0]), MISPLACED),
        (|body, chain| write_u64(body, 0, chain[2]), MISPLACED),
        (|body, _| write_u32(body, 16, 7), MISPLACED),
        (|body, chain| write_u64(body, 8, chain[1]), MISPLACED),
        (|body, _| write_u32(body, 20, 17), SHORT_COUNT),
    ];
    for (change, problem) in cases {
        let (fs, model) = damaged(|cache| {
            let chain = chain_of(cache, b"a");
            rewrite_page(cache, chain[2], |body| change(body, &chain));
        });
        assert_refused(&fs, &model, b"a", corrupt_overflow(problem));
    }
}

#[test]
fn a_last_page_that_names_a_next_page_or_holds_bytes_past_the_value_is_refused() {
    type Change = fn(&mut [u8], u64);
    let cases: [(Change, &str); 2] = [
        (
            |body, other| write_u64(body, 0, other),
            "the value's last page names a next page",
        ),
        (
            |body, _| *body.last_mut().unwrap() = 1,
            "the bytes after the value's end are not zero",
        ),
    ];
    for (change, problem) in cases {
        let (fs, model) = damaged(|cache| {
            let chain = chain_of(cache, b"a");
            let other = chain_of(cache, b"b")[0];
            rewrite_page(cache, chain[4], |body| change(body, other));
        });
        assert_refused(&fs, &model, b"a", corrupt_overflow(problem));
    }
}

#[test]
fn a_chain_leading_to_a_page_of_another_type_or_layout_is_refused() {
    let (fs, model) = damaged(|cache| {
        let chain = chain_of(cache, b"a");
        let mut page = cache.write(chain[3]).unwrap();
        let header = page.header();
        page.set_header(PageHeader {
            layout: 2,
            ..header
        })
        .unwrap();
    });
    assert_refused(
        &fs,
        &model,
        b"a",
        corrupt_overflow("the page has an unknown layout"),
    );
    let (fs, model) = damaged(|cache| {
        let chain = chain_of(cache, b"a");
        let root = cache.superblock().catalog_root;
        rewrite_page(cache, chain[2], |body| write_u64(body, 0, root));
    });
    assert_refused(
        &fs,
        &model,
        b"a",
        corrupt_overflow("a large value leads to a page of another type"),
    );
}

#[test]
fn a_reference_whose_length_disagrees_with_its_chain_is_refused() {
    let page = per_page(PageSize::DEFAULT) as u64;
    let lengths: [(u64, &str); 5] = [
        (1 << 29, "the value is longer than the file could hold"),
        (4 * page + 101, SHORT_COUNT),
        (4 * page + 99, SHORT_COUNT),
        (6 * page, SHORT_COUNT),
        (3 * page, "the value's last page names a next page"),
    ];
    for (value_bytes, problem) in lengths {
        let (fs, model) = damaged(|cache| {
            rewrite_leaf_entry(cache, b"a", |value| {
                let StoredValue::Overflow(reference) = value else {
                    panic!("not a large value");
                };
                reference.value_bytes = value_bytes;
            });
        });
        assert_refused(&fs, &model, b"a", corrupt_overflow(problem));
    }
}

#[test]
fn hostile_reference_fields_are_refused_as_damage() {
    let cases: [(u64, u64); 7] = [
        (0, 5),
        (u64::MAX, 5),
        (100, NO_PAGE),
        (1 << 30, 1 << 40),
        (100, 1 << 40),
        (100, 1),
        (100, u64::MAX),
    ];
    for (value_bytes, first_page) in cases {
        let fs = SimFileSystem::new();
        let model = three_large_values(&fs);
        {
            let mut cache = opened_cache(&fs, Access::ReadWrite, 64);
            let (page_id, _) = leaf_holding(&cache, b"a");
            let crafted = OverflowReference {
                value_bytes,
                first_page,
            }
            .encode();
            let mut page = cache.write(page_id).unwrap();
            let body = page.to_vec();
            let shape = Shape::parse(page_id, page.header(), &body).unwrap();
            let cell = shape.cell_at(&body, 0).unwrap();
            assert_eq!(cell.key, b"a");
            let at = cell.payload.as_ptr() as usize - body.as_ptr() as usize;
            page[at..at + crafted.len()].copy_from_slice(&crafted);
            drop(page);
            cache.commit(|_| {}).unwrap();
        }
        let store = Store::new(
            opened_cache(&fs, Access::ReadOnly, 64),
            Arc::new(SimClock::new(0)),
        );
        let root = store.superblock().unwrap().catalog_root;
        let read = store.begin_read().unwrap();
        let tree = read.open_tree(root);
        let error = tree
            .as_ref()
            .map_err(|_| ())
            .and_then(|tree| read.get(tree, b"a").map_err(|_| ()));
        assert!(error.is_err());
        let error = match &tree {
            Ok(tree) => read.get(tree, b"a").unwrap_err(),
            Err(_) => tree.unwrap_err(),
        };
        let leaf_refused = matches!(error, StorageError::Format(FormatError::CorruptTree { .. }));
        assert!(
            leaf_refused
                || matches!(
                    error,
                    StorageError::Format(FormatError::CorruptOverflow { .. })
                        | StorageError::PageOutOfRange { .. }
                ),
            "{value_bytes} {first_page}: {error}"
        );
        assert!(store.check().is_err());
        if !leaf_refused {
            let tree = read.open_tree(root).unwrap();
            assert_eq!(
                read.get(&tree, b"b").unwrap().as_ref(),
                model.get(&b"b"[..])
            );
        }
    }
}

#[test]
fn two_values_sharing_one_chain_fail_the_check() {
    let (fs, model) = damaged(|cache| {
        let b_chain_start = chain_of(cache, b"b")[0];
        rewrite_leaf_entry(cache, b"d", |value| {
            *value = StoredValue::Overflow(OverflowReference {
                value_bytes: 3 * per_page(PageSize::DEFAULT) as u64,
                first_page: b_chain_start,
            });
        });
    });
    let (store, tree) = opened_store(&fs, Access::ReadOnly, 64);
    let read = store.begin_read().unwrap();
    assert_eq!(
        read.get(&tree, b"d").unwrap().as_ref(),
        model.get(&b"b"[..])
    );
    let shared = corrupt_overflow("the page is reached twice");
    assert!(shared(&read.check(&tree).unwrap_err()));
    assert!(shared(&store.check().unwrap_err()));
}

#[test]
fn a_page_the_file_lost_track_of_is_reported_as_leaked() {
    let fs = SimFileSystem::new();
    let model = three_large_values(&fs);
    {
        let mut cache = opened_cache(&fs, Access::ReadWrite, 64);
        let mut page = cache.allocate(header_for(0, 0)).unwrap();
        let page_id = page.page_id();
        Node::empty_leaf().encode(page_id, &mut page).unwrap();
        drop(page);
        cache.commit(|_| {}).unwrap();
    }
    let (store, tree) = opened_store(&fs, Access::ReadOnly, 64);
    assert_reads(&store.begin_read().unwrap(), &tree, &model);
    assert_eq!(store.check().unwrap().leaked_pages, 1);
}

#[test]
fn a_page_with_two_uses_fails_the_page_accounting() {
    let fs = SimFileSystem::new();
    let (store, tree) = created_store(&fs, PageSize::DEFAULT, 64);
    let mut model = Model::new();
    let large = vec![1; 3 * per_page(PageSize::DEFAULT)];
    commit_changes(&store, &tree, &mut model, &[(b"a".to_vec(), large)], &[]);
    commit_changes(&store, &tree, &mut model, &[], &[b"a".to_vec()]);
    assert!(assert_holds(&store, &tree, &model).free_pages >= 3);
    store.with_cache(|cache| {
        let page_count = cache.superblock().page_count;
        let mut reachable = PageSet::for_pages(page_count).unwrap();
        for page_id in cache.page_size().first_data_page()..page_count {
            reachable.insert(page_id).unwrap();
        }
        let error = cache.account_pages(&reachable).unwrap_err();
        assert!(
            matches!(
                error,
                StorageError::Format(FormatError::PageUsedTwice {
                    first_use: "reachable from the tree",
                    ..
                })
            ),
            "{error}"
        );
    });
}

const OWNED_BY_ANOTHER_KEY: &str =
    "the value's pages belong to another value written in this transaction";

#[test]
fn replacing_or_deleting_a_value_whose_reference_points_past_the_file_fails_and_frees_nothing() {
    let fs = SimFileSystem::new();
    let model = three_large_values(&fs);
    {
        let mut cache = opened_cache(&fs, Access::ReadWrite, 128);
        let past_the_end = cache.superblock().page_count;
        rewrite_leaf_entry(&cache, b"a", |value| {
            let StoredValue::Overflow(reference) = value else {
                panic!("not a large value");
            };
            reference.first_page = past_the_end;
        });
        cache.commit(|_| {}).unwrap();
    }
    let (store, tree) = opened_store(&fs, Access::ReadWrite, 128);
    let before = store.superblock().unwrap();
    let same_length = patterned(model[&b"a"[..]].len(), 77);
    for change in 0..3 {
        let mut write = store.begin_write().unwrap();
        let error = match change {
            0 => write.put(&tree, b"a", &same_length).unwrap_err(),
            1 => write.put(&tree, b"a", b"small").unwrap_err(),
            _ => write.delete(&tree, b"a").unwrap_err(),
        };
        assert!(
            matches!(
                error,
                StorageError::PageOutOfRange { .. } | StorageError::PageNotInUse { .. }
            ),
            "{error}"
        );
        assert!(matches!(
            write.put(&tree, b"b", b""),
            Err(StorageError::TransactionFailed)
        ));
    }
    assert_eq!(store.superblock().unwrap(), before);
    let read = store.begin_read().unwrap();
    for key in [&b"b"[..], b"c", b"d"] {
        assert_eq!(read.get(&tree, key).unwrap().as_ref(), model.get(key));
    }
    assert!(store.check().is_err());
}

#[test]
fn a_reference_to_a_chain_another_key_wrote_in_this_transaction_is_refused() {
    for change in 0..3 {
        let fs = SimFileSystem::new();
        let model = three_large_values(&fs);
        let mut cache = opened_cache(&fs, Access::ReadWrite, 128);
        let tree = BTree::open(&cache, cache.superblock().catalog_root).unwrap();
        let new_value = patterned(2 * per_page(PageSize::DEFAULT), 5);
        tree.put(&cache, b"new", &new_value).unwrap();
        let StoredValue::Overflow(new_reference) = stored_value(&cache, b"new") else {
            panic!("not a large value");
        };
        rewrite_leaf_entry(&cache, b"d", |value| {
            *value = StoredValue::Overflow(new_reference);
        });
        let error = match change {
            0 => tree.delete(&cache, b"d").unwrap_err(),
            1 => tree.put(&cache, b"d", b"small").unwrap_err(),
            _ => tree.put(&cache, b"d", &new_value).unwrap_err(),
        };
        assert!(corrupt_overflow(OWNED_BY_ANOTHER_KEY)(&error), "{error}");
        assert_eq!(tree.get(&cache, b"new").unwrap(), Some(new_value));
        cache.rollback().unwrap();
        drop(cache);
        let (store, tree) = opened_store(&fs, Access::ReadOnly, 64);
        assert_holds(&store, &tree, &model);
    }
}

#[test]
fn one_key_rewritten_many_times_in_a_transaction_frees_each_chain_it_wrote() {
    let fs = SimFileSystem::new();
    let page = per_page(PageSize::DEFAULT);
    let (store, tree) = created_store(&fs, PageSize::DEFAULT, 256);
    let mut model = Model::new();
    let mut write = store.begin_write().unwrap();
    for (round, len) in [5 * page, 4 * page, 4, 2 * page].into_iter().enumerate() {
        write
            .put(&tree, b"x", &patterned(len, round as u64))
            .unwrap();
    }
    assert!(write.delete(&tree, b"x").unwrap());
    write.put(&tree, b"x", &patterned(3 * page, 9)).unwrap();
    write.put(&tree, b"y", &patterned(2 * page, 10)).unwrap();
    write.commit().unwrap();
    model.insert(b"x".to_vec(), patterned(3 * page, 9));
    model.insert(b"y".to_vec(), patterned(2 * page, 10));
    assert_holds(&store, &tree, &model);
}

#[test]
fn a_value_that_just_fits_the_cache_can_replace_one_as_large() {
    let fs = SimFileSystem::new();
    let page = per_page(PageSize::DEFAULT);
    let (store, tree) = created_store(&fs, PageSize::DEFAULT, 40);
    let mut model = Model::new();
    let largest = 39 * page;
    let puts = [(b"big".to_vec(), patterned(largest, 1))];
    commit_changes(&store, &tree, &mut model, &puts, &[]);
    let mut write = store.begin_write().unwrap();
    write.check_fits(b"big", &patterned(largest, 2)).unwrap();
    write.put(&tree, b"big", &patterned(largest, 2)).unwrap();
    write.put(&tree, b"big", &patterned(largest, 3)).unwrap();
    write.commit().unwrap();
    model.insert(b"big".to_vec(), patterned(largest, 3));
    assert_holds(&store, &tree, &model);
}
