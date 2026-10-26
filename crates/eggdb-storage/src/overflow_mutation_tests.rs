use crate::btree::BTree;
use crate::btree_tests::tree_pages;
use crate::bytes::{write_u16, write_u32, write_u64};
use crate::crash_sweep_tests::{file_bytes, file_system_holding};
use crate::cursor::ScanOrder;
use crate::error::{FormatError, StorageError};
use crate::format::{Access, PAGE_HEADER_SIZE, PageChecksum, PageHeader, PageSize, PageType};
use crate::overflow_tests::{
    DB, FILE_ID, Model, chains, commit_changes, created_store, opened_cache, patterned, per_page,
    scanned,
};
use crate::page_cache::{CacheSize, PageCache};
use crate::page_file::PageFile;
use crate::store::Store;
use crate::test_rng::TestRng;
use crate::tree_page::Shape;
use eggdb_platform::{File, FileSystem, OpenMode, SimClock, SimFileSystem, SimMemory};
use std::path::Path;
use std::sync::Arc;

const CASES: u64 = 2400;
const PAGE: usize = 4096;
const LONGER_THAN_THE_FILE: &str = "the value is longer than the file could hold";

struct Fixture {
    bytes: Vec<u8>,
    model: Model,
    chains: Vec<Vec<u64>>,
    leaves: Vec<u64>,
    page_count: u64,
}

fn fixture() -> Fixture {
    let fs = SimFileSystem::new();
    let (store, tree) = created_store(&fs, PageSize::DEFAULT, 256);
    let page = per_page(PageSize::DEFAULT);
    let mut model = Model::new();
    let mut rng = TestRng::new(0x0F10);
    let mut puts = Vec::new();
    for index in 0..40u8 {
        let len = match index % 5 {
            0 => page * (1 + rng.below(4)) + rng.below(page),
            1 => 9 * page,
            _ => rng.below(120),
        };
        puts.push((vec![b'k', index], patterned(len, u64::from(index))));
    }
    commit_changes(&store, &tree, &mut model, &puts, &[]);
    store.checkpoint().unwrap();
    drop(store);
    let cache = opened_cache(&fs, Access::ReadOnly, 64);
    let chains: Vec<Vec<u64>> = chains(&cache).into_values().collect();
    let leaves = tree_pages(&cache)
        .into_iter()
        .filter(|&page| cache.read(page).unwrap().header().page_type == PageType::BTreeLeaf)
        .collect();
    let page_count = cache.superblock().page_count;
    drop(cache);
    assert_eq!(chains.len(), 16);
    Fixture {
        bytes: file_bytes(&fs),
        model,
        chains,
        leaves,
        page_count,
    }
}

fn checksums() -> PageChecksum {
    PageChecksum::new(FILE_ID, PageSize::DEFAULT)
}

fn page_of(bytes: &mut [u8], page_id: u64) -> &mut [u8] {
    let start = page_id as usize * PAGE;
    &mut bytes[start..start + PAGE]
}

fn rewrite(bytes: &mut [u8], page_id: u64, change: impl FnOnce(&mut PageHeader, &mut [u8])) {
    let page = page_of(bytes, page_id);
    let mut header = checksums().verify(page, page_id).unwrap();
    change(&mut header, &mut page[PAGE_HEADER_SIZE..]);
    checksums().write(page, page_id, &header).unwrap();
}

fn interesting_page(rng: &mut TestRng, fixture: &Fixture, own: &[u64]) -> u64 {
    let other = &fixture.chains[rng.below(fixture.chains.len())];
    match rng.below(10) {
        0 => 0,
        1 => 1,
        2 => fixture.page_count - 1 + rng.below(3) as u64,
        3 => u64::MAX - rng.below(2) as u64,
        4 => 1 << (40 + rng.below(9)),
        5 => own[rng.below(own.len())],
        6 => other[rng.below(other.len())],
        7 => fixture.leaves[rng.below(fixture.leaves.len())],
        _ => rng.below(fixture.page_count as usize) as u64,
    }
}

fn interesting_length(rng: &mut TestRng, fixture: &Fixture, original: u64) -> u64 {
    let page = per_page(PageSize::DEFAULT) as u64;
    match rng.below(9) {
        0 => 0,
        1 => original.wrapping_add(1 + rng.below(3) as u64),
        2 => original.saturating_sub(1 + rng.below(3) as u64),
        3 => original + page * (1 + rng.below(3) as u64),
        4 => original.saturating_sub(page),
        5 => (BTree::MAX_VALUE_BYTES as u64).wrapping_add(rng.below(2) as u64),
        6 => u64::MAX - rng.below(2) as u64,
        7 => (fixture.page_count + 1) * page,
        _ => rng.next_u64() % (2 * page * fixture.page_count),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mutation {
    FlippedBits,
    ChainField,
    ChainHeader,
    ChainData,
    SwappedChainPages,
    ReferenceLength,
    ReferencePage,
    LengthField,
    Truncated,
}

const MUTATIONS: [Mutation; 9] = [
    Mutation::FlippedBits,
    Mutation::ChainField,
    Mutation::ChainHeader,
    Mutation::ChainData,
    Mutation::SwappedChainPages,
    Mutation::ReferenceLength,
    Mutation::ReferencePage,
    Mutation::LengthField,
    Mutation::Truncated,
];

fn mutate_chain_page(rng: &mut TestRng, fixture: &Fixture, bytes: &mut [u8], kind: Mutation) {
    let chain = &fixture.chains[rng.below(fixture.chains.len())];
    let target = chain[rng.below(chain.len())];
    match kind {
        Mutation::FlippedBits => {
            let page = page_of(bytes, target);
            for _ in 0..1 + rng.below(3) {
                let bit = rng.below(PAGE * 8);
                page[bit / 8] ^= 1 << (bit % 8);
            }
        }
        Mutation::ChainField => {
            let value = interesting_page(rng, fixture, chain);
            let field = rng.below(4);
            let small = [0, 1, chain.len() as u32, u32::MAX, rng.next_u64() as u32][rng.below(5)];
            rewrite(bytes, target, |_, body| match field {
                0 => write_u64(body, 0, value),
                1 => write_u64(body, 8, value),
                2 => write_u32(body, 16, small),
                _ => write_u32(body, 20, small),
            });
        }
        Mutation::ChainHeader => {
            let types = [
                PageType::FreeList,
                PageType::BTreeInner,
                PageType::BTreeLeaf,
                PageType::LogSegment,
            ];
            let changed_type = types[rng.below(types.len())];
            let layout = rng.below(3) as u8;
            let change_type = rng.below(2) == 0;
            rewrite(bytes, target, |header, _| {
                if change_type {
                    header.page_type = changed_type;
                } else {
                    header.layout = layout;
                }
            });
        }
        Mutation::ChainData => {
            let at = 24 + rng.below(PAGE - PAGE_HEADER_SIZE - 24);
            let byte = rng.next_u64() as u8;
            rewrite(bytes, target, |_, body| body[at] = byte);
        }
        _ => {
            let other_chain = &fixture.chains[rng.below(fixture.chains.len())];
            let other = other_chain[rng.below(other_chain.len())];
            if other == target {
                return;
            }
            let first = page_of(bytes, target).to_vec();
            let second = page_of(bytes, other).to_vec();
            page_of(bytes, target).copy_from_slice(&second);
            page_of(bytes, other).copy_from_slice(&first);
            for page_id in [target, other] {
                let page = page_of(bytes, page_id);
                let header = PageHeader {
                    page_type: PageType::Overflow,
                    layout: page[5],
                    flags: 0,
                    lsn: u64::from_le_bytes(page[8..16].try_into().unwrap()),
                };
                checksums().write(page, page_id, &header).unwrap();
            }
        }
    }
}

fn mutate_leaf(rng: &mut TestRng, fixture: &Fixture, bytes: &mut [u8], kind: Mutation) -> bool {
    let leaf = fixture.leaves[rng.below(fixture.leaves.len())];
    let page = page_of(bytes, leaf).to_vec();
    let header = checksums().verify(&page, leaf).unwrap();
    let body = &page[PAGE_HEADER_SIZE..];
    let shape = Shape::parse(leaf, header, body).unwrap();
    let offset_of = |slice: &[u8]| slice.as_ptr() as usize - body.as_ptr() as usize;
    let cells: Vec<(usize, usize, bool, u64)> = (0..shape.count)
        .map(|index| {
            let cell = shape.cell_at(body, index).unwrap();
            let length = match shape.overflow_reference(&cell).unwrap() {
                Some(reference) => reference.value_bytes,
                None => cell.payload.len() as u64,
            };
            (
                offset_of(cell.key),
                offset_of(cell.payload),
                cell.overflow,
                length,
            )
        })
        .collect();
    let references: Vec<_> = cells.iter().filter(|cell| cell.2).collect();
    let mut huge_length = false;
    match kind {
        Mutation::ReferenceLength if !references.is_empty() => {
            let &&(_, at, _, original) = &references[rng.below(references.len())];
            let length = interesting_length(rng, fixture, original);
            let page_capacity = fixture.page_count * per_page(PageSize::DEFAULT) as u64;
            huge_length = length > page_capacity && length <= BTree::MAX_VALUE_BYTES as u64;
            rewrite(bytes, leaf, |_, body| write_u64(body, at, length));
        }
        Mutation::ReferencePage if !references.is_empty() => {
            let &&(_, at, _, _) = &references[rng.below(references.len())];
            let own: Vec<u64> = fixture.chains.iter().map(|chain| chain[0]).collect();
            let first = interesting_page(rng, fixture, &own);
            rewrite(bytes, leaf, |_, body| write_u64(body, at + 8, first));
        }
        _ => {
            let (key_at, _, overflow, length) = cells[rng.below(cells.len())];
            let field = if rng.below(2) == 0 {
                (length as u16) | 0x8000
            } else if overflow {
                0x8000 | rng.below(0x8000) as u16
            } else {
                rng.next_u64() as u16
            };
            rewrite(bytes, leaf, |_, body| write_u16(body, key_at - 2, field));
        }
    }
    huge_length
}

fn allowed(error: &StorageError) -> bool {
    matches!(
        error,
        StorageError::Format(_)
            | StorageError::PageOutOfRange { .. }
            | StorageError::PageNotInUse { .. }
            | StorageError::Truncated { .. }
            | StorageError::CorruptFreeList { .. }
            | StorageError::DoubleFree { .. }
            | StorageError::TransactionFailed
    )
}

fn expect_allowed<T>(result: Result<T, StorageError>, context: &str) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            assert!(allowed(&error), "{context}: {error:?}");
            None
        }
    }
}

fn read_everything(fs: &SimFileSystem, model: &Model, huge_length: bool, context: &str) -> bool {
    let Some(file) = expect_allowed(PageFile::open(fs, Path::new(DB), Access::ReadOnly), context)
    else {
        return true;
    };
    let cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(64)).unwrap();
    let store = Store::new(cache, Arc::new(SimClock::new(0)));
    let root = store.superblock().unwrap().catalog_root;
    let read = store.begin_read().unwrap();
    let Some(tree) = expect_allowed(read.open_tree(root), context) else {
        return true;
    };
    let mut every_get_worked = true;
    for key in model.keys() {
        match read.get(&tree, key) {
            Ok(_) => {}
            Err(error) => {
                assert!(allowed(&error), "{context}: get: {error:?}");
                if huge_length {
                    assert!(
                        matches!(
                            error,
                            StorageError::Format(
                                FormatError::CorruptOverflow {
                                    problem: LONGER_THAN_THE_FILE,
                                    ..
                                } | FormatError::CorruptTree { .. }
                            )
                        ),
                        "{context}: a length past the file must be refused before any allocation: {error:?}"
                    );
                }
                every_get_worked = false;
            }
        }
    }
    let scans = [ScanOrder::Ascending, ScanOrder::Descending]
        .map(|order| expect_allowed(scanned(&read, &tree, order), context));
    let Some(report) = expect_allowed(store.check(), context) else {
        return true;
    };
    assert!(every_get_worked, "{context}: check passed but a get failed");
    for scan in &scans {
        let scan = scan.as_ref().expect("check passed but a scan failed");
        assert_eq!(scan.len() as u64, report.tree.unwrap().entries, "{context}");
    }
    false
}

fn write_after(fs: &SimFileSystem, model: &Model, rng: &mut TestRng, context: &str) {
    let Some(file) = expect_allowed(
        PageFile::open(fs, Path::new(DB), Access::ReadWrite),
        context,
    ) else {
        return;
    };
    let cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(64)).unwrap();
    let store = Store::new(cache, Arc::new(SimClock::new(0)));
    let root = store.superblock().unwrap().catalog_root;
    let keys: Vec<&Vec<u8>> = model.keys().collect();
    let written = store.begin_write().and_then(|mut write| {
        let tree = write.open_tree(root)?;
        write.delete(&tree, keys[rng.below(keys.len())])?;
        let value = patterned(per_page(PageSize::DEFAULT) * 3, 7);
        write.put(&tree, keys[rng.below(keys.len())], &value)?;
        write.commit()
    });
    if expect_allowed(written, context).is_some() {
        expect_allowed(store.check(), context);
    }
}

#[test]
fn seeded_mutations_of_large_values_never_panic_hang_or_allocate_beyond_the_file() {
    let fixture = fixture();
    let mut rng = TestRng::new(0x5EED_0F10);
    let mut refused = [0u64; MUTATIONS.len()];
    for case in 0..CASES {
        let kind = MUTATIONS[case as usize % MUTATIONS.len()];
        let mut bytes = fixture.bytes.clone();
        let mut huge_length = false;
        match kind {
            Mutation::Truncated => {
                let pages = 2 + rng.below(fixture.page_count as usize - 2);
                bytes.truncate(pages * PAGE + rng.below(2) * rng.below(PAGE));
            }
            Mutation::ReferenceLength | Mutation::ReferencePage | Mutation::LengthField => {
                huge_length = mutate_leaf(&mut rng, &fixture, &mut bytes, kind);
            }
            _ => mutate_chain_page(&mut rng, &fixture, &mut bytes, kind),
        }
        let context = format!("case {case}, {kind:?}");
        let fs = file_system_holding(&bytes);
        let damage_found = read_everything(&fs, &fixture.model, huge_length, &context);
        refused[case as usize % MUTATIONS.len()] += u64::from(damage_found);
        if case % 4 == 0 {
            write_after(&fs, &fixture.model, &mut rng, &context);
        }
    }
    eprintln!("refused by check, per mutation: {refused:?}");
    for (kind, count) in MUTATIONS.iter().zip(refused) {
        if *kind != Mutation::ChainData {
            assert!(count > 0, "{kind:?} was never refused");
        }
    }
}

#[test]
fn the_mutation_fixture_passes_every_check_before_it_is_damaged() {
    let fixture = fixture();
    let fs = file_system_holding(&fixture.bytes);
    let raw = fs.open(Path::new(DB), OpenMode::Read).unwrap();
    assert_eq!(raw.size().unwrap(), fixture.page_count * PAGE as u64);
    let cache = opened_cache(&fs, Access::ReadOnly, 64);
    let store = Store::new(cache, Arc::new(SimClock::new(0)));
    let report = store.check().unwrap();
    assert_eq!(report.leaked_pages, 0);
    assert_eq!(report.tree.unwrap().overflow_values, 16);
}
