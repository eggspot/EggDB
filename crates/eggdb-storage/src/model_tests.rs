use crate::btree::BTree;
use crate::cursor::{Cursor, ScanOrder};
use crate::error::StorageError;
use crate::format::{Access, FileId, PageSize};
use crate::log::Durability;
use crate::overflow;
use crate::page_cache::{CacheSize, PageCache};
use crate::page_file::{CreateOptions, PageFile};
use crate::store::{ReadTransaction, Store, WriteTransaction};
use crate::test_rng::TestRng;
use eggdb_platform::{CrashMode, SimClock, SimFile, SimFileSystem, SimMemory};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

const DB: &str = "db";
const FILE_ID: FileId = FileId(*b"model-test-file!");
const FRAMES: usize = 96;
const SMALL_FRAMES: usize = 40;
const AUTO_CHECKPOINT_BYTES: u64 = 256 * 1024;
const LONG_RUN_VARIABLE: &str = "EGGDB_MODEL_TEST";
const KEY_BYTES: [u8; 6] = [0x00, 0x01, b'a', b'b', 0xFE, 0xFF];

type Model = BTreeMap<Vec<u8>, Vec<u8>>;
type Entries = Vec<(Vec<u8>, Vec<u8>)>;
type Range = (Bound<Vec<u8>>, Bound<Vec<u8>>);

#[derive(Debug, Default)]
struct Counts {
    commits: u64,
    rollbacks: u64,
    failed_transactions: u64,
    refused_changes: u64,
    overflow_puts: u64,
    reopens: u64,
    read_only_opens: u64,
    power_losses: u64,
    faults: u64,
    lost_commits: u64,
    checkpoints: u64,
    checks: u64,
    scans: u64,
}

struct Commit {
    model: Model,
    synced_after: bool,
}

struct ModelRun {
    seed: u64,
    rng: TestRng,
    fs: SimFileSystem,
    durability: Durability,
    frames: usize,
    store: Option<Store<SimFile>>,
    tree: BTree,
    history: Vec<Commit>,
    counts: Counts,
    step: usize,
}

impl ModelRun {
    fn new(seed: u64) -> Self {
        let mut rng = TestRng::new(seed);
        let durability = match rng.below(3) {
            0 => Durability::Normal,
            1 if seed % 4 == 1 => Durability::Off,
            _ => Durability::Full,
        };
        let frames = if seed % 3 == 2 { SMALL_FRAMES } else { FRAMES };
        let fs = SimFileSystem::new();
        let options = CreateOptions {
            page_size: PageSize::DEFAULT,
            file_id: FILE_ID,
        };
        let file = PageFile::create(&fs, Path::new(DB), options).unwrap();
        let cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(frames)).unwrap();
        let store = configured(Store::new(cache, Arc::new(SimClock::new(0))), durability);
        let mut write = store.begin_write().unwrap();
        let tree = write.create_tree().unwrap();
        write.set_catalog_root(tree.root());
        write.commit().unwrap();
        store.sync().unwrap();
        Self {
            seed,
            rng,
            fs,
            durability,
            frames,
            store: Some(store),
            tree,
            history: vec![Commit {
                model: Model::new(),
                synced_after: true,
            }],
            counts: Counts::default(),
            step: 0,
        }
    }

    fn context(&self) -> String {
        format!(
            "seed {}, step {}, {:?}",
            self.seed, self.step, self.durability
        )
    }

    fn model(&self) -> &Model {
        &self.history.last().unwrap().model
    }

    fn store(&self) -> &Store<SimFile> {
        self.store.as_ref().unwrap()
    }

    fn run(&mut self, steps: usize) {
        for step in 0..steps {
            self.step = step;
            match self.rng.below(100) {
                0..=47 => self.transaction(),
                48..=49 => self.transaction_until_the_cache_is_full(),
                50..=59 => self.transaction_beside_a_reader(),
                60..=71 => self.verify_reads(),
                72..=74 => self.verify_check(),
                75..=77 => self.checkpoint(),
                78..=80 => self.sync(),
                81..=86 => self.reopen(),
                87..=89 => self.read_only_reopen(),
                90..=94 if self.durability != Durability::Off => self.power_loss(),
                95..=99 => self.fault_then_recover(),
                _ => self.transaction(),
            }
        }
        self.verify_reads();
        self.verify_check();
    }

    fn transaction(&mut self) {
        let context = self.context();
        let store = self.store.take().unwrap();
        let mut pending = self.model().clone();
        let outcome = self.write_changes(&store, &mut pending, &context);
        self.store = Some(store);
        self.finish(outcome, pending);
    }

    fn transaction_until_the_cache_is_full(&mut self) {
        let context = self.context();
        let tree = self.tree;
        let value = vec![0xC3; 3 * overflow::data_bytes_per_page(PageSize::DEFAULT)];
        let store = self.store.take().unwrap();
        let mut write = store.begin_write().unwrap();
        let mut full = None;
        for index in 0..self.frames as u32 {
            let key = [b"full:".as_slice(), &index.to_be_bytes()].concat();
            if let Err(error) = write.put(&tree, &key, &value) {
                full = Some(error);
                break;
            }
        }
        assert!(
            matches!(full, Some(StorageError::CacheFull { .. })),
            "{context}: {full:?}"
        );
        assert!(matches!(
            write.put(&tree, b"after", b"v"),
            Err(StorageError::TransactionFailed)
        ));
        assert!(matches!(
            write.commit(),
            Err(StorageError::TransactionFailed)
        ));
        self.store = Some(store);
        self.counts.failed_transactions += 1;
    }

    fn transaction_beside_a_reader(&mut self) {
        let context = self.context();
        let store = self.store.take().unwrap();
        let before = self.model().clone();
        let reader = store.begin_read().unwrap();
        let mut pending = before.clone();
        let outcome = self.write_changes(&store, &mut pending, &context);
        let tree = self.tree;
        let reader_context = format!("{context}, reader begun before the transaction");
        verify_snapshot(&mut self.rng, &reader, &tree, &before, &reader_context);
        drop(reader);
        self.store = Some(store);
        self.finish(outcome, pending);
    }

    fn finish(&mut self, outcome: Outcome, pending: Model) {
        match outcome {
            Outcome::Committed => {
                self.counts.commits += 1;
                self.history.push(Commit {
                    model: pending,
                    synced_after: false,
                });
                self.mark_synced_if(self.durability == Durability::Full);
            }
            Outcome::RolledBack => self.counts.rollbacks += 1,
            Outcome::Failed => self.counts.failed_transactions += 1,
        }
    }

    fn write_changes(
        &mut self,
        store: &Store<SimFile>,
        pending: &mut Model,
        context: &str,
    ) -> Outcome {
        let tree = self.tree;
        let mut write = store
            .begin_write()
            .unwrap_or_else(|error| panic!("{context}: begin_write: {error}"));
        let most = if self.rng.below(5) == 0 { 40 } else { 8 };
        let changes = 1 + self.rng.below(most);
        for _ in 0..changes {
            match self.change(&mut write, &tree, pending, context) {
                Ok(()) => {}
                Err(StorageError::CacheFull { .. }) => return Outcome::Failed,
                Err(error) => panic!("{context}: change: {error:?}"),
            }
            if self.rng.below(4) == 0 {
                let key = self.key(pending);
                let found = write
                    .get(&tree, &key)
                    .unwrap_or_else(|error| panic!("{context}: get in a write: {error}"));
                assert_eq!(
                    found.as_ref(),
                    pending.get(&key),
                    "{context}: get in a write"
                );
            }
        }
        if self.rng.below(6) == 0 {
            let (start, end) = self.range(pending);
            let order = self.order();
            let mut cursor = tree.scan(as_ref(&start), as_ref(&end), order);
            let scanned = drain(|cursor| write.next(cursor), &mut cursor)
                .unwrap_or_else(|error| panic!("{context}: scan in a write: {error}"));
            assert!(
                scanned == expected_range(pending, &start, &end, order),
                "{context}: scan in a write of {start:?}..{end:?} {order:?}"
            );
        }
        match self.rng.below(10) {
            0 => {
                write.rollback().unwrap();
                Outcome::RolledBack
            }
            1 => {
                drop(write);
                Outcome::RolledBack
            }
            _ => {
                write
                    .commit()
                    .unwrap_or_else(|error| panic!("{context}: commit: {error}"));
                Outcome::Committed
            }
        }
    }

    fn change(
        &mut self,
        write: &mut WriteTransaction<'_, SimFile>,
        tree: &BTree,
        pending: &mut Model,
        context: &str,
    ) -> Result<(), StorageError> {
        match self.rng.below(20) {
            0..=4 if !pending.is_empty() => {
                let key = pending.keys().nth(self.rng.below(pending.len())).unwrap();
                let key = key.clone();
                assert!(write.delete(tree, &key)?, "{context}: delete of a key");
                pending.remove(&key);
            }
            5 => {
                let key = self.key(pending);
                let found = write.delete(tree, &key)?;
                assert_eq!(found, pending.remove(&key).is_some(), "{context}: delete");
            }
            6 => {
                let max = BTree::max_key_bytes(PageSize::DEFAULT);
                let key = vec![b'k'; max + 1 + self.rng.below(3)];
                let refused = write.put(tree, &key, b"v");
                assert!(
                    matches!(refused, Err(StorageError::KeyTooLarge { .. })),
                    "{context}: {refused:?}"
                );
                self.counts.refused_changes += 1;
            }
            7 if self.rng.below(8) == 0 => {
                let bytes = (self.frames + 2) * overflow::data_bytes_per_page(PageSize::DEFAULT);
                let refused = write.put(tree, b"too large", &vec![7; bytes]);
                assert!(
                    matches!(refused, Err(StorageError::ValueExceedsCache { .. })),
                    "{context}: {refused:?}"
                );
                self.counts.refused_changes += 1;
            }
            _ => {
                let key = self.key(pending);
                let value = self.value(&key);
                if key.len() + value.len() > BTree::max_inline_entry_bytes(PageSize::DEFAULT) {
                    self.counts.overflow_puts += 1;
                }
                write.put(tree, &key, &value)?;
                pending.insert(key, value);
            }
        }
        Ok(())
    }

    fn key(&mut self, model: &Model) -> Vec<u8> {
        if !model.is_empty() && self.rng.below(3) == 0 {
            return model
                .keys()
                .nth(self.rng.below(model.len()))
                .unwrap()
                .clone();
        }
        let len = match self.rng.below(40) {
            0 => 0,
            1 => BTree::max_key_bytes(PageSize::DEFAULT),
            2 => 100 + self.rng.below(300),
            _ => 1 + self.rng.below(6),
        };
        (0..len)
            .map(|_| KEY_BYTES[self.rng.below(KEY_BYTES.len())])
            .collect()
    }

    fn value(&mut self, key: &[u8]) -> Vec<u8> {
        let inline = BTree::max_inline_entry_bytes(PageSize::DEFAULT).saturating_sub(key.len());
        let page = overflow::data_bytes_per_page(PageSize::DEFAULT);
        let len = match self.rng.below(16) {
            0 => 0,
            1 => inline,
            2 => inline + 1,
            3 | 4 => inline + 1 + self.rng.below(3 * page),
            5 if self.rng.below(4) == 0 => page * (4 + self.rng.below(12)) + self.rng.below(page),
            6 => page * (1 + self.rng.below(2)),
            _ => self.rng.below(64),
        };
        self.rng.bytes(len)
    }

    fn range(&mut self, model: &Model) -> Range {
        let start = self.bound(model);
        let end = self.bound(model);
        (start, end)
    }

    fn bound(&mut self, model: &Model) -> Bound<Vec<u8>> {
        match self.rng.below(3) {
            0 => Bound::Unbounded,
            1 => Bound::Included(self.key(model)),
            _ => Bound::Excluded(self.key(model)),
        }
    }

    fn order(&mut self) -> ScanOrder {
        if self.rng.below(2) == 0 {
            ScanOrder::Ascending
        } else {
            ScanOrder::Descending
        }
    }

    fn verify_reads(&mut self) {
        let context = self.context();
        let model = self.model().clone();
        let tree = self.tree;
        let store = self.store.take().unwrap();
        let read = store.begin_read().unwrap();
        verify_snapshot(&mut self.rng, &read, &tree, &model, &context);
        for _ in 0..4 {
            let (start, end) = self.range(&model);
            let order = self.order();
            let mut cursor = tree.scan(as_ref(&start), as_ref(&end), order);
            let scanned = drain(|cursor| read.next(cursor), &mut cursor)
                .unwrap_or_else(|error| panic!("{context}: range scan: {error}"));
            assert!(
                scanned == expected_range(&model, &start, &end, order),
                "{context}: range scan {start:?}..{end:?} {order:?}"
            );
            let prefix = self.prefix(&model);
            let order = self.order();
            let mut cursor = tree.scan_prefix(&prefix, order);
            let scanned = drain(|cursor| read.next(cursor), &mut cursor)
                .unwrap_or_else(|error| panic!("{context}: prefix scan: {error}"));
            assert!(
                scanned == expected_prefix(&model, &prefix, order),
                "{context}: prefix scan {prefix:?} {order:?}"
            );
            self.counts.scans += 2;
        }
        drop(read);
        self.store = Some(store);
    }

    fn prefix(&mut self, model: &Model) -> Vec<u8> {
        let mut key = self.key(model);
        let keep = match self.rng.below(4) {
            0 => 0,
            1 => key.len(),
            _ => self.rng.below(key.len().min(3) + 1),
        };
        key.truncate(keep);
        if self.rng.below(6) == 0 {
            key.push(0xFF);
        }
        key
    }

    fn verify_check(&mut self) {
        let context = self.context();
        let report = self
            .store()
            .check()
            .unwrap_or_else(|error| panic!("{context}: check: {error}"));
        let tree = report.tree.expect("the store has a tree");
        let model = self.model();
        let page_size = PageSize::DEFAULT;
        let spilled = model
            .iter()
            .filter(|(key, value)| {
                key.len() + value.len() > BTree::max_inline_entry_bytes(page_size)
            })
            .count() as u64;
        assert_eq!(tree.entries, model.len() as u64, "{context}: check entries");
        assert_eq!(
            tree.key_bytes,
            model.keys().map(|key| key.len() as u64).sum::<u64>(),
            "{context}: check key bytes"
        );
        assert_eq!(
            tree.value_bytes,
            model.values().map(|value| value.len() as u64).sum::<u64>(),
            "{context}: check value bytes"
        );
        assert_eq!(tree.overflow_values, spilled, "{context}: overflow values");
        assert_eq!(
            report.leaked_pages, 0,
            "{context}: leaked pages: {report:?}"
        );
        self.counts.checks += 1;
    }

    fn checkpoint(&mut self) {
        let context = self.context();
        self.store()
            .checkpoint()
            .unwrap_or_else(|error| panic!("{context}: checkpoint: {error}"));
        self.counts.checkpoints += 1;
        self.mark_synced_if(self.durability != Durability::Off);
    }

    fn sync(&mut self) {
        let context = self.context();
        self.store()
            .sync()
            .unwrap_or_else(|error| panic!("{context}: sync: {error}"));
        self.mark_synced_if(true);
    }

    fn mark_synced_if(&mut self, synced: bool) {
        if synced {
            self.history.last_mut().unwrap().synced_after = true;
        }
    }

    fn reopen(&mut self) {
        let use_close_like_checkpoint = self.rng.below(3) == 0;
        if use_close_like_checkpoint {
            self.checkpoint();
        }
        drop(self.store.take());
        self.counts.reopens += 1;
        self.recover(Ending::Reopen, None);
    }

    fn read_only_reopen(&mut self) {
        drop(self.store.take());
        let context = self.context();
        let file = PageFile::open(&self.fs, Path::new(DB), Access::ReadOnly)
            .unwrap_or_else(|error| panic!("{context}: read-only open: {error}"));
        let cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(self.frames)).unwrap();
        let store = Store::new(cache, Arc::new(SimClock::new(0)));
        assert!(matches!(store.begin_write(), Err(StorageError::ReadOnly)));
        self.store = Some(store);
        self.verify_reads();
        drop(self.store.take());
        self.counts.read_only_opens += 1;
        self.recover(Ending::Reopen, None);
    }

    fn power_loss(&mut self) {
        drop(self.store.take());
        let mode = self.crash_mode();
        self.counts.power_losses += 1;
        self.recover(Ending::PowerLoss(mode), None);
    }

    fn crash_mode(&mut self) -> CrashMode {
        if self.rng.below(3) == 0 {
            CrashMode::DropUnsynced
        } else {
            CrashMode::Torn {
                seed: self.rng.next_u64(),
            }
        }
    }

    fn fault_then_recover(&mut self) {
        let context = self.context();
        let store = self.store.take().unwrap();
        let fault_at = self.fs.operation_count() + self.rng.below(12) as u64;
        self.fs.fail_operation(fault_at);
        let mut unknown = None;
        for _ in 0..200 {
            let mut pending = self.model().clone();
            let tree = self.tree;
            let mut write = match store.begin_write() {
                Ok(write) => write,
                Err(StorageError::NeedsReopen) => break,
                Err(error) => panic!("{context}: begin_write after a fault: {error}"),
            };
            let mut changed = Ok(());
            for _ in 0..1 + self.rng.below(6) {
                changed = self.change(&mut write, &tree, &mut pending, &context);
                if changed.is_err() {
                    break;
                }
            }
            if let Err(error) = changed {
                assert!(
                    matches!(error, StorageError::Io(_) | StorageError::CacheFull { .. }),
                    "{context}: change under a fault: {error:?}"
                );
                drop(write);
                if matches!(error, StorageError::Io(_)) {
                    break;
                }
                continue;
            }
            let step = self.rng.below(4);
            let ended = match step {
                0 => store.checkpoint(),
                1 => store.sync(),
                _ => match write.commit() {
                    Ok(_) => {
                        self.counts.commits += 1;
                        self.history.push(Commit {
                            model: pending,
                            synced_after: self.durability == Durability::Full,
                        });
                        continue;
                    }
                    Err(error) => {
                        unknown = Some(pending);
                        Err(error)
                    }
                },
            };
            match ended {
                Ok(()) => {
                    if step == 1 || self.durability != Durability::Off {
                        self.mark_synced_if(true);
                    }
                }
                Err(StorageError::UncommittedChanges) => {}
                Err(error) => {
                    assert!(
                        matches!(error, StorageError::Io(_) | StorageError::NeedsReopen),
                        "{context}: error under a fault: {error:?}"
                    );
                    break;
                }
            }
        }
        assert!(
            self.fs.operation_count() > fault_at,
            "{context}: the injected fault was never reached"
        );
        drop(store);
        self.counts.faults += 1;
        let ending = if self.durability != Durability::Off && self.rng.below(2) == 0 {
            Ending::PowerLoss(self.crash_mode())
        } else {
            Ending::Reopen
        };
        if self.rng.below(4) == 0 {
            self.fail_during_recovery(ending);
        }
        self.recover(ending, unknown);
    }

    fn fail_during_recovery(&mut self, ending: Ending) {
        if let Ending::PowerLoss(mode) = ending {
            self.fs.crash(mode);
        }
        let context = self.context();
        self.fs
            .fail_operation(self.fs.operation_count() + self.rng.below(3) as u64);
        let opened = PageFile::open(&self.fs, Path::new(DB), Access::ReadWrite);
        if let Err(error) = &opened {
            assert!(
                matches!(error, StorageError::Io(_)),
                "{context}: recovery under a fault: {error:?}"
            );
        }
        drop(opened);
    }

    fn recover(&mut self, ending: Ending, unknown: Option<Model>) {
        let context = format!("{}, {ending:?}", self.context());
        if let Ending::PowerLoss(mode) = ending {
            self.fs.crash(mode);
        }
        let file = PageFile::open(&self.fs, Path::new(DB), Access::ReadWrite)
            .unwrap_or_else(|error| panic!("{context}: reopen: {error}"));
        let cache = PageCache::new(file, &SimMemory::new(), CacheSize::Pages(self.frames)).unwrap();
        let store = configured(
            Store::new(cache, Arc::new(SimClock::new(0))),
            self.durability,
        );
        let root = store.superblock().unwrap().catalog_root;
        assert_eq!(root, self.tree.root(), "{context}: the tree's root moved");
        let read = store.begin_read().unwrap();
        let tree = read
            .open_tree(root)
            .unwrap_or_else(|error| panic!("{context}: open the tree: {error}"));
        let mut cursor = tree.scan(Bound::Unbounded, Bound::Unbounded, ScanOrder::Ascending);
        let recovered: Model = drain(|cursor| read.next(cursor), &mut cursor)
            .unwrap_or_else(|error| panic!("{context}: scan after recovery: {error}"))
            .into_iter()
            .collect();
        drop(read);
        let floor = match ending {
            Ending::Reopen => self.history.len() - 1,
            Ending::PowerLoss(_) => self
                .history
                .iter()
                .rposition(|commit| commit.synced_after)
                .unwrap_or(0),
        };
        if unknown.as_ref() == Some(&recovered) {
            self.history.push(Commit {
                model: recovered,
                synced_after: true,
            });
        } else {
            let found = self.history[floor..]
                .iter()
                .rposition(|commit| commit.model == recovered)
                .map(|index| floor + index)
                .unwrap_or_else(|| {
                    panic!(
                        "{context}: the recovered state is no commit at or after the durable one ({floor} of {}), {} keys",
                        self.history.len(),
                        recovered.len()
                    )
                });
            self.counts.lost_commits += (self.history.len() - 1 - found) as u64;
            self.history.truncate(found + 1);
        }
        for commit in &mut self.history {
            commit.synced_after = true;
        }
        self.store = Some(store);
        self.verify_check();
    }
}

#[derive(Debug, Clone, Copy)]
enum Ending {
    Reopen,
    PowerLoss(CrashMode),
}

enum Outcome {
    Committed,
    RolledBack,
    Failed,
}

fn configured(store: Store<SimFile>, durability: Durability) -> Store<SimFile> {
    store.set_durability(durability).unwrap();
    store
        .set_auto_checkpoint_bytes(Some(AUTO_CHECKPOINT_BYTES))
        .unwrap();
    store
        .set_snapshot_memory(CacheSize::Pages(1 << 16))
        .unwrap();
    store
}

fn as_ref(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    bound.as_ref().map(Vec::as_slice)
}

fn contains(start: &Bound<Vec<u8>>, end: &Bound<Vec<u8>>, key: &[u8]) -> bool {
    let after_start = match start {
        Bound::Unbounded => true,
        Bound::Included(start) => key >= start.as_slice(),
        Bound::Excluded(start) => key > start.as_slice(),
    };
    let before_end = match end {
        Bound::Unbounded => true,
        Bound::Included(end) => key <= end.as_slice(),
        Bound::Excluded(end) => key < end.as_slice(),
    };
    after_start && before_end
}

fn ordered(entries: impl Iterator<Item = (Vec<u8>, Vec<u8>)>, order: ScanOrder) -> Entries {
    let mut entries: Entries = entries.collect();
    if order == ScanOrder::Descending {
        entries.reverse();
    }
    entries
}

fn expected_range(
    model: &Model,
    start: &Bound<Vec<u8>>,
    end: &Bound<Vec<u8>>,
    order: ScanOrder,
) -> Entries {
    let entries = model
        .iter()
        .filter(|(key, _)| contains(start, end, key))
        .map(|(key, value)| (key.clone(), value.clone()));
    ordered(entries, order)
}

fn expected_prefix(model: &Model, prefix: &[u8], order: ScanOrder) -> Entries {
    let entries = model
        .iter()
        .filter(|(key, _)| key.starts_with(prefix))
        .map(|(key, value)| (key.clone(), value.clone()));
    ordered(entries, order)
}

fn drain(
    mut next: impl FnMut(&mut Cursor) -> Result<Option<(&[u8], &[u8])>, StorageError>,
    cursor: &mut Cursor,
) -> Result<Entries, StorageError> {
    let mut entries = Vec::new();
    while let Some((key, value)) = next(cursor)? {
        entries.push((key.to_vec(), value.to_vec()));
    }
    Ok(entries)
}

fn verify_snapshot(
    rng: &mut TestRng,
    read: &ReadTransaction<'_, SimFile>,
    tree: &BTree,
    model: &Model,
    context: &str,
) {
    for (key, value) in model.iter().filter(|_| rng.below(3) == 0) {
        let found = read
            .get(tree, key)
            .unwrap_or_else(|error| panic!("{context}: get: {error}"));
        assert_eq!(found.as_ref(), Some(value), "{context}: get {key:?}");
    }
    for _ in 0..4 {
        let len = rng.below(4);
        let missing: Vec<u8> = (0..len).map(|_| KEY_BYTES[rng.below(6)]).collect();
        let found = read
            .get(tree, &missing)
            .unwrap_or_else(|error| panic!("{context}: get: {error}"));
        assert_eq!(
            found.as_ref(),
            model.get(&missing),
            "{context}: get {missing:?}"
        );
    }
    for order in [ScanOrder::Ascending, ScanOrder::Descending] {
        let mut cursor = tree.scan(Bound::Unbounded, Bound::Unbounded, order);
        let scanned = drain(|cursor| read.next(cursor), &mut cursor)
            .unwrap_or_else(|error| panic!("{context}: scan: {error}"));
        let expected = expected_range(model, &Bound::Unbounded, &Bound::Unbounded, order);
        assert!(scanned == expected, "{context}: full scan {order:?}");
    }
}

fn run_seeds(seeds: std::ops::Range<u64>, steps: usize) -> Counts {
    let mut total = Counts::default();
    for seed in seeds {
        let mut run = ModelRun::new(seed);
        run.run(steps);
        let counts = run.counts;
        total.commits += counts.commits;
        total.rollbacks += counts.rollbacks;
        total.failed_transactions += counts.failed_transactions;
        total.refused_changes += counts.refused_changes;
        total.overflow_puts += counts.overflow_puts;
        total.reopens += counts.reopens;
        total.read_only_opens += counts.read_only_opens;
        total.power_losses += counts.power_losses;
        total.faults += counts.faults;
        total.lost_commits += counts.lost_commits;
        total.checkpoints += counts.checkpoints;
        total.checks += counts.checks;
        total.scans += counts.scans;
    }
    eprintln!("{total:?}");
    total
}

fn long_run() -> bool {
    std::env::var_os(LONG_RUN_VARIABLE).is_some()
}

#[test]
fn random_transactions_reopens_and_crashes_always_match_an_in_memory_map() {
    let (seeds, steps) = if long_run() {
        (0..256, 3000)
    } else {
        (0..12, 250)
    };
    let counts = run_seeds(seeds, steps);
    assert!(counts.commits > 300, "{counts:?}");
    assert!(counts.rollbacks > 20, "{counts:?}");
    assert!(counts.overflow_puts > 100, "{counts:?}");
    assert!(
        counts.reopens > 10 && counts.read_only_opens > 5,
        "{counts:?}"
    );
    assert!(counts.power_losses > 10 && counts.faults > 10, "{counts:?}");
    assert!(counts.lost_commits > 0, "{counts:?}");
    assert!(counts.refused_changes > 10, "{counts:?}");
    assert!(counts.failed_transactions > 0, "{counts:?}");
}
