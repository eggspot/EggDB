use crate::btree::BTree;
use crate::cursor::ScanOrder;
use crate::error::StorageError;
use crate::format::{Access, FileId, PageSize};
use crate::log::Durability;
use crate::page_cache::{CacheSize, PageCache};
use crate::page_file::{CreateOptions, PageFile};
use crate::test_rng::TestRng;
use eggdb_platform::{CrashMode, File, FileSystem, OpenMode, SimFile, SimFileSystem, SimMemory};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;

const DB: &str = "db";
const FILE_ID: FileId = FileId(*b"crash-sweep-file");
const CACHE_FRAMES: usize = 24;
const AUTO_CHECKPOINT_BYTES: u64 = 2 * 16 * 4096;
const CONTINUATION_STAMP: u64 = 1 << 32;
const EXHAUSTIVE_VARIABLE: &str = "EGGDB_CRASH_SWEEP";

type Model = BTreeMap<Vec<u8>, Vec<u8>>;
type Entry = (Vec<u8>, Vec<u8>);

#[derive(Debug, Clone)]
enum Change {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}

#[derive(Debug, Clone)]
enum Step {
    Commit(Vec<Change>),
    RolledBack(Vec<Change>),
    Checkpoint,
    Sync,
}

#[derive(Debug, Clone)]
struct Workload {
    durability: Durability,
    steps: Vec<Step>,
    history: Vec<Model>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    Reopen,
    PowerLoss(CrashMode),
}

#[derive(Debug, Default)]
struct Outcome {
    acknowledged: usize,
    attempted: usize,
    synced_through: usize,
    failed: Option<StorageError>,
}

#[derive(Debug, Clone, Copy)]
struct Sweep {
    every_ending_at_every_step: bool,
    torn_seeds: u64,
}

impl Sweep {
    fn from_environment() -> Self {
        let exhaustive = std::env::var_os(EXHAUSTIVE_VARIABLE).is_some();
        Self {
            every_ending_at_every_step: exhaustive,
            torn_seeds: if exhaustive { 4 } else { 2 },
        }
    }

    fn endings_at(&self, step: u64) -> Vec<Ending> {
        let mut endings = vec![Ending::Reopen, Ending::PowerLoss(CrashMode::DropUnsynced)];
        endings
            .extend((0..self.torn_seeds).map(|seed| Ending::PowerLoss(CrashMode::Torn { seed })));
        if self.every_ending_at_every_step {
            endings
        } else {
            vec![endings[step as usize % endings.len()]]
        }
    }
}

fn key(rng: &mut TestRng) -> Vec<u8> {
    let len = match rng.below(10) {
        0 => 100 + rng.below(300),
        _ => 1 + rng.below(24),
    };
    (0..len).map(|_| b'a' + rng.below(6) as u8).collect()
}

fn value(rng: &mut TestRng, key: &[u8]) -> Vec<u8> {
    let len = match rng.below(12) {
        0 => BTree::max_entry_bytes(PageSize::DEFAULT) - key.len(),
        1..=3 => (200 + rng.below(400)).min(BTree::max_entry_bytes(PageSize::DEFAULT) - key.len()),
        _ => rng.below(60),
    };
    rng.bytes(len)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Growing,
    Shrinking,
}

fn changes(rng: &mut TestRng, model: &mut Model, phase: Phase) -> Vec<Change> {
    let (count, deletes_in_eight) = match phase {
        Phase::Growing => (1 + rng.below(16), 1),
        Phase::Shrinking => (1 + rng.below(30), 7),
    };
    let mut changes = Vec::new();
    for _ in 0..count {
        if !model.is_empty() && rng.below(8) < deletes_in_eight {
            let existing = model.keys().nth(rng.below(model.len())).unwrap().clone();
            model.remove(&existing);
            changes.push(Change::Delete(existing));
        } else if !model.is_empty() && rng.below(4) == 0 {
            let existing = model.keys().nth(rng.below(model.len())).unwrap().clone();
            let value = value(rng, &existing);
            model.insert(existing.clone(), value.clone());
            changes.push(Change::Put(existing, value));
        } else {
            let key = key(rng);
            let value = value(rng, &key);
            model.insert(key.clone(), value.clone());
            changes.push(Change::Put(key, value));
        }
    }
    changes
}

fn workload(durability: Durability, seed: u64, transactions: usize, base: &Model) -> Workload {
    let mut rng = TestRng::new(seed);
    let mut model = base.clone();
    let mut steps = Vec::new();
    let mut history = vec![model.clone()];
    for transaction in 0..transactions {
        let phase = if transaction < transactions * 2 / 3 {
            Phase::Growing
        } else {
            Phase::Shrinking
        };
        if rng.below(8) == 0 {
            let mut scratch = model.clone();
            steps.push(Step::RolledBack(changes(&mut rng, &mut scratch, phase)));
        }
        let changed = changes(&mut rng, &mut model, phase);
        steps.push(Step::Commit(changed));
        history.push(model.clone());
        match rng.below(16) {
            0 => steps.push(Step::Checkpoint),
            1 => steps.push(Step::Sync),
            _ => {}
        }
    }
    Workload {
        durability,
        steps,
        history,
    }
}

fn open(fs: &SimFileSystem) -> Result<PageCache<SimFile>, StorageError> {
    let file = PageFile::open(fs, Path::new(DB), Access::ReadWrite)?;
    PageCache::new(file, &SimMemory::new(), CacheSize::Pages(CACHE_FRAMES))
}

fn created_tree(fs: &SimFileSystem) -> (PageCache<SimFile>, BTree) {
    let options = CreateOptions {
        page_size: PageSize::DEFAULT,
        file_id: FILE_ID,
    };
    let file = PageFile::create(fs, Path::new(DB), options).unwrap();
    let mut cache =
        PageCache::new(file, &SimMemory::new(), CacheSize::Pages(CACHE_FRAMES)).unwrap();
    let tree = BTree::create(&cache).unwrap();
    cache
        .commit(|superblock| {
            superblock.catalog_root = tree.root();
            superblock.clock_high_water = 0;
        })
        .unwrap();
    cache.sync().unwrap();
    (cache, tree)
}

fn apply(tree: &BTree, cache: &PageCache<SimFile>, changes: &[Change]) -> Result<(), StorageError> {
    for change in changes {
        match change {
            Change::Put(key, value) => tree.put(cache, key, value)?,
            Change::Delete(key) => {
                tree.delete(cache, key)?;
            }
        }
    }
    Ok(())
}

fn assert_refuses_further_work(cache: &mut PageCache<SimFile>, context: &str) {
    assert!(
        matches!(cache.commit(|_| {}), Err(StorageError::NeedsReopen)),
        "{context}: a commit after a failed write must need a reopen"
    );
    assert!(
        cache.sync().is_err(),
        "{context}: a sync after a failed write must not report success"
    );
}

fn run(
    fs: &SimFileSystem,
    cache: &mut PageCache<SimFile>,
    tree: &BTree,
    workload: &Workload,
    first_stamp: u64,
    context: &str,
) -> Outcome {
    let mut outcome = Outcome::default();
    let mut acknowledged_at_syncs = Vec::new();
    for step in &workload.steps {
        let result = match step {
            Step::Commit(changes) => {
                let applied = apply(tree, cache, changes);
                if let Err(error) = applied {
                    outcome.failed = Some(error);
                    break;
                }
                outcome.attempted += 1;
                let stamp = first_stamp + outcome.attempted as u64;
                let committed = cache.commit(|superblock| superblock.clock_high_water = stamp);
                if committed.is_ok() {
                    outcome.acknowledged += 1;
                    acknowledged_at_syncs.push(fs.sync_count());
                }
                committed
            }
            Step::RolledBack(changes) => {
                let applied = apply(tree, cache, changes);
                let rolled_back = cache.rollback();
                if let Err(error) = applied {
                    outcome.failed = Some(error);
                    break;
                }
                rolled_back
            }
            Step::Checkpoint => cache.checkpoint(),
            Step::Sync => cache.sync(),
        };
        if let Err(error) = result {
            outcome.failed = Some(error);
            assert_refuses_further_work(cache, context);
            break;
        }
    }
    let syncs = fs.sync_count();
    outcome.synced_through = acknowledged_at_syncs
        .iter()
        .rposition(|&at| at < syncs)
        .map_or(0, |index| index + 1);
    outcome
}

fn scanned(
    tree: &BTree,
    cache: &PageCache<SimFile>,
    order: ScanOrder,
) -> Result<Vec<Entry>, StorageError> {
    let mut cursor = tree.scan(Bound::Unbounded, Bound::Unbounded, order);
    let mut entries = Vec::new();
    while let Some((key, value)) = cursor.next(cache)? {
        entries.push((key.to_vec(), value.to_vec()));
    }
    Ok(entries)
}

fn recovered_commit(
    fs: &SimFileSystem,
    history: &[Model],
    first_stamp: u64,
    context: &str,
) -> usize {
    let cache = open(fs).unwrap_or_else(|error| panic!("{context}: reopen failed: {error}"));
    let stamp = cache.superblock().clock_high_water;
    let commit = stamp
        .checked_sub(first_stamp)
        .map(|commit| commit as usize)
        .filter(|&commit| commit < history.len())
        .unwrap_or_else(|| panic!("{context}: recovered stamp {stamp}, not a commit of this run"));
    let tree = BTree::open(&cache, cache.superblock().catalog_root)
        .unwrap_or_else(|error| panic!("{context}: tree: {error}"));
    let report = tree
        .check(&cache)
        .unwrap_or_else(|error| panic!("{context}: check: {error}"));
    let model = &history[commit];
    assert_eq!(
        report.entries,
        model.len() as u64,
        "{context}, commit {commit}"
    );
    let expected: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let forward = scanned(&tree, &cache, ScanOrder::Ascending)
        .unwrap_or_else(|error| panic!("{context}: scan: {error}"));
    assert!(
        forward == expected,
        "{context}: forward scan differs from commit {commit}"
    );
    let mut backward = scanned(&tree, &cache, ScanOrder::Descending)
        .unwrap_or_else(|error| panic!("{context}: reverse scan: {error}"));
    backward.reverse();
    assert!(
        backward == expected,
        "{context}: reverse scan differs from commit {commit}"
    );
    commit
}

fn assert_promised(
    durability: Durability,
    outcome: &Outcome,
    ending: Ending,
    commit: usize,
    context: &str,
) {
    assert!(
        commit <= outcome.attempted,
        "{context}: commit {commit} recovered, only {} attempted",
        outcome.attempted
    );
    let floor = match (durability, ending) {
        (_, Ending::Reopen) | (Durability::Full, _) => outcome.acknowledged,
        (Durability::Normal, Ending::PowerLoss(_)) => outcome.synced_through,
        (Durability::Off, Ending::PowerLoss(_)) => 0,
    };
    assert!(
        commit >= floor,
        "{context}: commit {commit} recovered, {floor} promised ({outcome:?})"
    );
}

fn end(fs: &SimFileSystem, ending: Ending) {
    if let Ending::PowerLoss(mode) = ending {
        fs.crash(mode);
    }
}

fn file_bytes(fs: &SimFileSystem) -> Vec<u8> {
    let raw = fs.open(Path::new(DB), OpenMode::Read).unwrap();
    let mut bytes = vec![0u8; raw.size().unwrap() as usize];
    raw.read_exact_at(&mut bytes, 0).unwrap();
    bytes
}

fn file_system_holding(bytes: &[u8]) -> SimFileSystem {
    let fs = SimFileSystem::new();
    let raw = fs.open(Path::new(DB), OpenMode::Create).unwrap();
    raw.write_all_at(bytes, 0).unwrap();
    raw.sync_all().unwrap();
    fs.sync_dir(Path::new("")).unwrap();
    fs
}

fn operations_of_a_clean_run(workload: &Workload) -> u64 {
    let fs = SimFileSystem::new();
    let (mut cache, tree) = created_tree(&fs);
    cache.set_durability(workload.durability).unwrap();
    cache.set_auto_checkpoint_bytes(Some(AUTO_CHECKPOINT_BYTES));
    let start = fs.operation_count();
    let outcome = run(&fs, &mut cache, &tree, workload, 0, "clean run");
    let operations = fs.operation_count() - start;
    assert!(outcome.failed.is_none(), "{outcome:?}");
    assert_eq!(outcome.acknowledged, workload.history.len() - 1);
    assert!(
        cache.log_stats().checkpoints >= 10,
        "{:?}",
        cache.log_stats()
    );
    assert!(
        cache.superblock().free_page_count > 0,
        "{:?}",
        cache.superblock()
    );
    drop(cache);
    let ending = match workload.durability {
        Durability::Off => Ending::Reopen,
        Durability::Full | Durability::Normal => Ending::PowerLoss(CrashMode::Torn { seed: 99 }),
    };
    end(&fs, ending);
    let commit = recovered_commit(&fs, &workload.history, 0, "clean run");
    if workload.durability != Durability::Normal {
        assert_eq!(commit, workload.history.len() - 1);
    }
    operations
}

fn heights_over_time(workload: &Workload) -> (u32, u32) {
    let fs = SimFileSystem::new();
    let (mut cache, tree) = created_tree(&fs);
    cache.set_auto_checkpoint_bytes(Some(AUTO_CHECKPOINT_BYTES));
    let mut tallest = 0;
    let mut last = 0;
    for step in &workload.steps {
        if let Step::Commit(changes) = step {
            apply(&tree, &cache, changes).unwrap();
            cache.commit(|_| {}).unwrap();
            last = tree.check(&cache).unwrap().height;
            tallest = tallest.max(last);
        }
    }
    (tallest, last)
}

fn crashed_run(
    workload: &Workload,
    step: u64,
    ending: Ending,
    context: &str,
) -> (SimFileSystem, Outcome) {
    let fs = SimFileSystem::new();
    let (mut cache, tree) = created_tree(&fs);
    cache.set_durability(workload.durability).unwrap();
    cache.set_auto_checkpoint_bytes(Some(AUTO_CHECKPOINT_BYTES));
    fs.fail_operation(fs.operation_count() + step);
    let outcome = run(&fs, &mut cache, &tree, workload, 0, context);
    assert!(
        outcome.failed.is_some(),
        "{context}: the injected fault was never reported"
    );
    drop(cache);
    end(&fs, ending);
    (fs, outcome)
}

fn continued_after_recovery(
    fs: &SimFileSystem,
    workload: &Workload,
    commit: usize,
    step: u64,
    context: &str,
) {
    let base = &workload.history[commit];
    let more = workload_after(workload.durability, step, base);
    let mut cache = open(fs).unwrap();
    cache.set_auto_checkpoint_bytes(Some(AUTO_CHECKPOINT_BYTES));
    let tree = BTree::open(&cache, cache.superblock().catalog_root).unwrap();
    let outcome = run(fs, &mut cache, &tree, &more, CONTINUATION_STAMP, context);
    assert!(
        outcome.failed.is_none(),
        "{context}: writing after recovery failed: {outcome:?}"
    );
    cache.checkpoint().unwrap();
    let last = more.history.len() - 1;
    cache
        .commit(|superblock| superblock.clock_high_water = CONTINUATION_STAMP + last as u64)
        .unwrap();
    drop(cache);
    end(fs, Ending::PowerLoss(CrashMode::Torn { seed: step }));
    let recovered = recovered_commit(fs, &more.history, CONTINUATION_STAMP, context);
    let floor = match workload.durability {
        Durability::Full | Durability::Normal => last,
        Durability::Off => 0,
    };
    assert!(
        recovered >= floor,
        "{context}: continued commit {recovered}, {floor} promised"
    );
}

fn workload_after(durability: Durability, step: u64, base: &Model) -> Workload {
    workload(durability, 1_000 + step, 6, base)
}

fn crash_during_recovery(bytes: &[u8], history: &[Model], expected: usize, context: &str) -> u64 {
    let fs = file_system_holding(bytes);
    let start = fs.operation_count();
    drop(open(&fs).unwrap());
    let operations = fs.operation_count() - start;
    let mut points = 0;
    for step in 0..operations {
        let fs = file_system_holding(bytes);
        fs.fail_operation(fs.operation_count() + step);
        let context = format!("{context}, recovery step {step}");
        assert!(
            open(&fs).is_err(),
            "{context}: the injected fault was never reported"
        );
        fs.crash(CrashMode::Torn { seed: step });
        let commit = recovered_commit(&fs, history, 0, &context);
        assert_eq!(commit, expected, "{context}: recovery is not idempotent");
        points += 1;
    }
    points
}

fn sweep(durability: Durability, seed: u64, transactions: usize) -> (u64, u64) {
    let options = Sweep::from_environment();
    let workload = workload(durability, seed, transactions, &Model::new());
    let operations = operations_of_a_clean_run(&workload);
    let (tallest, last) = heights_over_time(&workload);
    assert!(
        tallest >= 2 && last == 1,
        "the workload must split and then collapse the root: {tallest}, {last}"
    );
    let mut crash_points = 0;
    let mut recovery_points = 0;
    for step in 0..operations {
        for ending in options.endings_at(step) {
            let context =
                format!("{durability:?}, seed {seed}, step {step}/{operations}, {ending:?}");
            let (fs, outcome) = crashed_run(&workload, step, ending, &context);
            if durability == Durability::Off && ending != Ending::Reopen {
                if let Ok(cache) = open(&fs) {
                    let _ = BTree::open(&cache, cache.superblock().catalog_root)
                        .map(|tree| tree.check(&cache));
                }
                crash_points += 1;
                continue;
            }
            let bytes = file_bytes(&fs);
            let commit = recovered_commit(&fs, &workload.history, 0, &context);
            assert_promised(durability, &outcome, ending, commit, &context);
            crash_points += 1;
            if step % 7 == 0 && matches!(ending, Ending::PowerLoss(CrashMode::Torn { .. })) {
                recovery_points +=
                    crash_during_recovery(&bytes, &workload.history, commit, &context);
            }
            if let Ending::PowerLoss(CrashMode::Torn { seed: 0 }) = ending {
                continued_after_recovery(&fs, &workload, commit, step, &context);
            }
        }
    }
    eprintln!(
        "{durability:?}: {operations} operations, {crash_points} crash points, {recovery_points} crashes during recovery"
    );
    (crash_points, recovery_points)
}

#[test]
fn a_crash_at_any_operation_of_a_full_durability_workload_keeps_every_acknowledged_commit() {
    let (crash_points, recovery_points) = sweep(Durability::Full, 1, 90);
    assert!(crash_points > 1000, "{crash_points}");
    assert!(recovery_points > 100, "{recovery_points}");
}

#[test]
fn a_crash_at_any_operation_of_a_normal_durability_workload_keeps_every_synced_commit() {
    let (crash_points, recovery_points) = sweep(Durability::Normal, 2, 90);
    assert!(crash_points > 900, "{crash_points}");
    assert!(recovery_points > 100, "{recovery_points}");
}

#[test]
fn a_fault_at_any_operation_of_an_off_durability_workload_keeps_every_acknowledged_commit_without_a_power_loss()
 {
    let (crash_points, _) = sweep(Durability::Off, 3, 60);
    assert!(crash_points > 400, "{crash_points}");
}
