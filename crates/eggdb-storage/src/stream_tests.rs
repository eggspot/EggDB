use crate::btree::BTree;
use crate::error::StorageError;
use crate::format::{FileId, PageSize};
use crate::overflow;
use crate::page_cache::{CacheSize, PageCache};
use crate::page_file::{CreateOptions, PageFile};
use crate::store::Store;
use eggdb_platform::{SimClock, SimFile, SimFileSystem, SimMemory};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

const FRAMES: usize = 256;

fn created() -> (Store<SimFile>, BTree) {
    let fs = SimFileSystem::new();
    let options = CreateOptions {
        page_size: PageSize::DEFAULT,
        file_id: FileId(*b"stream-test-file"),
    };
    let file = PageFile::create(&fs, Path::new("db"), options).unwrap();
    let cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(FRAMES)).unwrap();
    let store = Store::new(cache, Arc::new(SimClock::new(1)));
    let mut write = store.begin_write().unwrap();
    let tree = write.create_tree().unwrap();
    write.set_catalog_root(tree.root());
    write.commit().unwrap();
    (store, tree)
}

fn patterned(len: usize) -> Vec<u8> {
    (0..len).map(|index| (index * 31 % 251) as u8).collect()
}

struct Trickle<'a> {
    bytes: &'a [u8],
    step: usize,
}

impl Read for Trickle<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let taken = self.step.min(out.len()).min(self.bytes.len());
        out[..taken].copy_from_slice(&self.bytes[..taken]);
        self.bytes = &self.bytes[taken..];
        Ok(taken)
    }
}

fn read_in_pieces(reader: &mut impl Read, piece: usize) -> Vec<u8> {
    let mut all = Vec::new();
    let mut buffer = vec![0; piece];
    loop {
        let read = reader.read(&mut buffer).unwrap();
        if read == 0 {
            return all;
        }
        all.extend_from_slice(&buffer[..read]);
    }
}

#[test]
fn streamed_values_read_back_whole_and_in_pieces_of_any_size() {
    let (store, tree) = created();
    let per_page = overflow::data_bytes_per_page(PageSize::DEFAULT);
    let lengths = [0, 1, 700, per_page - 1, per_page, per_page + 1, 5 * per_page + 17, 150_000];
    let mut write = store.begin_write().unwrap();
    for (index, len) in lengths.iter().enumerate() {
        let value = patterned(*len);
        let mut source = Trickle {
            bytes: &value,
            step: 1 + index * 997,
        };
        let key = format!("value{index}");
        write
            .put_from_reader(&tree, key.as_bytes(), *len as u64, &mut source)
            .unwrap();
    }
    write.commit().unwrap();
    let read = store.begin_read().unwrap();
    for (index, len) in lengths.iter().enumerate() {
        let key = format!("value{index}");
        let expected = patterned(*len);
        assert_eq!(read.get(&tree, key.as_bytes()).unwrap(), Some(expected.clone()));
        for piece in [1, 7, per_page, 70_000] {
            let mut reader = read.value_reader(&tree, key.as_bytes()).unwrap().unwrap();
            assert_eq!(reader.len(), *len as u64);
            assert_eq!(read_in_pieces(&mut reader, piece), expected, "len {len} piece {piece}");
            assert_eq!(reader.remaining(), 0);
        }
    }
    assert!(read.value_reader(&tree, b"absent").unwrap().is_none());
    drop(read);
    assert_eq!(store.check().unwrap().leaked_pages, 0);
}

#[test]
fn a_reader_keeps_its_snapshot_while_the_value_is_replaced() {
    let (store, tree) = created();
    let first = patterned(40_000);
    let mut write = store.begin_write().unwrap();
    write.put(&tree, b"big", &first).unwrap();
    write.commit().unwrap();
    let read = store.begin_read().unwrap();
    let mut reader = read.value_reader(&tree, b"big").unwrap().unwrap();
    let mut start = vec![0; 1000];
    reader.read_exact(&mut start).unwrap();
    let mut write = store.begin_write().unwrap();
    let second = vec![9u8; 60_000];
    write
        .put_from_reader(&tree, b"big", second.len() as u64, &mut second.as_slice())
        .unwrap();
    write.commit().unwrap();
    let rest = read_in_pieces(&mut reader, 4096);
    assert_eq!([start, rest].concat(), first);
    drop(reader);
    drop(read);
    assert_eq!(store.begin_read().unwrap().get(&tree, b"big").unwrap(), Some(second));
}

#[test]
fn a_source_that_ends_early_or_runs_long_rolls_the_transaction_back() {
    let (store, tree) = created();
    let value = patterned(30_000);
    for (declared, given) in [(30_000_u64, 29_999), (30_000, 30_000 + 1), (500, 400), (500, 501)] {
        let mut write = store.begin_write().unwrap();
        write.put(&tree, b"kept", b"before").unwrap();
        let source = patterned(given);
        let outcome = write.put_from_reader(&tree, b"streamed", declared, &mut source.as_slice());
        assert!(outcome.is_err(), "declared {declared} given {given}");
        assert!(matches!(
            write.put(&tree, b"after", b"x"),
            Err(StorageError::TransactionFailed)
        ));
        drop(write);
    }
    let read = store.begin_read().unwrap();
    assert_eq!(read.get(&tree, b"kept").unwrap(), None);
    assert_eq!(read.get(&tree, b"streamed").unwrap(), None);
    drop(read);
    let mut write = store.begin_write().unwrap();
    write
        .put_from_reader(&tree, b"streamed", value.len() as u64, &mut value.as_slice())
        .unwrap();
    write.commit().unwrap();
    assert_eq!(store.check().unwrap().leaked_pages, 0);
}

#[test]
fn limits_are_checked_before_anything_is_read() {
    let (store, tree) = created();
    let mut write = store.begin_write().unwrap();
    let mut never_read = Trickle { bytes: &[], step: 1 };
    let too_large = BTree::MAX_VALUE_BYTES as u64 + 1;
    assert!(matches!(
        write.put_from_reader(&tree, b"k", too_large, &mut never_read),
        Err(StorageError::ValueTooLarge { .. })
    ));
    let beyond_cache = (FRAMES * PageSize::DEFAULT.bytes()) as u64;
    assert!(matches!(
        write.put_from_reader(&tree, b"k", beyond_cache, &mut never_read),
        Err(StorageError::ValueExceedsCache { .. })
    ));
    write.put(&tree, b"still", b"open").unwrap();
    write.commit().unwrap();
}
