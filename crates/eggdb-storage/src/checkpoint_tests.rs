use crate::btree_tests::{self, Model as TreeModel, assert_matches, bytes_up_to, stored_tree};
use crate::crc32c::crc32c_append;
use crate::error::{FormatError, StorageError};
use crate::format::{
    Access, FileId, HEADER_SLOT_SIZE, NO_PAGE, PageChecksum, PageHeader, PageSize, PageType,
    RECYCLED_SEGMENTS, Superblock,
};
use crate::log::{
    COMMIT_PAYLOAD, CommitFields, Durability, LOG_BLOCK, LogGeometry, LogWriter, RECORD_HEADER,
    Segment, WriterStart,
};
use crate::page_cache::{CacheSize, PageCache};
use crate::page_file::{CreateOptions, PageFile};
use crate::test_rng::TestRng;
use eggdb_platform::{CrashMode, File, FileSystem, OpenMode, SimFile, SimFileSystem, SimMemory};
use std::collections::BTreeMap;
use std::path::Path;

const DB: &str = "db";
const FILE_ID: FileId = FileId(*b"checkpoint-tests");
const PAGE: u64 = 4096;
const SEGMENT_PAGES: u64 = 16;

type Model = BTreeMap<u64, u8>;

fn leaf() -> PageHeader {
    PageHeader {
        page_type: PageType::BTreeLeaf,
        layout: 1,
        flags: 0,
        lsn: 0,
    }
}

fn create(fs: &SimFileSystem, frames: usize) -> PageCache<SimFile> {
    let options = CreateOptions {
        page_size: PageSize::DEFAULT,
        file_id: FILE_ID,
    };
    let file = PageFile::create(fs, Path::new(DB), options).unwrap();
    PageCache::new(file, &SimMemory::new(), CacheSize::Pages(frames)).unwrap()
}

fn open(fs: &SimFileSystem, access: Access) -> Result<PageCache<SimFile>, StorageError> {
    let file = PageFile::open(fs, Path::new(DB), access)?;
    PageCache::new(file, &SimMemory::new(), CacheSize::Pages(64))
}

fn fill_of(cache: &PageCache<SimFile>, page: u64) -> u8 {
    let bytes = cache.read(page).unwrap();
    assert!(bytes.iter().all(|&byte| byte == bytes[0]), "page {page}");
    bytes[0]
}

fn assert_model(cache: &PageCache<SimFile>, model: &Model, context: &str) {
    for (&page, &fill) in model {
        assert_eq!(fill_of(cache, page), fill, "page {page}, {context}");
    }
}

fn allocated(cache: &mut PageCache<SimFile>, count: usize) -> Model {
    let model = (0..count)
        .map(|_| (cache.allocate(leaf()).unwrap().page_id(), 0))
        .collect();
    cache.commit(|_| {}).unwrap();
    model
}

fn change(
    cache: &mut PageCache<SimFile>,
    model: &mut Model,
    rng: &mut TestRng,
    fill: u8,
) -> Result<(), StorageError> {
    let pages: Vec<u64> = model.keys().copied().collect();
    let mut next = model.clone();
    for _ in 0..1 + rng.below(3) {
        let page = pages[rng.below(pages.len())];
        cache.write(page)?.fill(fill);
        next.insert(page, fill);
    }
    cache.commit(|superblock| superblock.clock_high_water = u64::from(fill))?;
    *model = next;
    Ok(())
}

fn rounds(cache: &mut PageCache<SimFile>, model: &mut Model, rng: &mut TestRng, count: usize) {
    for round in 0..count {
        change(cache, model, rng, (round % 251) as u8 + 1).unwrap();
    }
}

fn raw(fs: &SimFileSystem) -> SimFile {
    fs.open(Path::new(DB), OpenMode::ReadWrite).unwrap()
}

fn file_bytes(fs: &SimFileSystem) -> Vec<u8> {
    let raw = raw(fs);
    let mut bytes = vec![0u8; raw.size().unwrap() as usize];
    raw.read_exact_at(&mut bytes, 0).unwrap();
    bytes
}

fn replace_file(fs: &SimFileSystem, bytes: &[u8]) {
    let raw = raw(fs);
    raw.set_size(bytes.len() as u64).unwrap();
    raw.write_all_at(bytes, 0).unwrap();
    raw.sync_all().unwrap();
}

fn segment_range(first: u64) -> std::ops::Range<usize> {
    (first * PAGE) as usize..((first + SEGMENT_PAGES) * PAGE) as usize
}

fn header_block(fs: &SimFileSystem, first: u64) -> Vec<u8> {
    let mut block = vec![0u8; LOG_BLOCK];
    raw(fs).read_exact_at(&mut block, first * PAGE).unwrap();
    block
}

fn first_recycled(superblock: &Superblock) -> u64 {
    superblock
        .recycled_segments
        .iter()
        .copied()
        .find(|&first| first != NO_PAGE)
        .unwrap()
}

fn grow_log_by_segments(
    cache: &mut PageCache<SimFile>,
    model: &mut Model,
    rng: &mut TestRng,
    segments: u64,
) {
    let target = cache.superblock().page_count + segments * SEGMENT_PAGES;
    let mut round = 0u8;
    while cache.superblock().page_count < target {
        round = round.wrapping_add(1).max(1);
        change(cache, model, rng, round).unwrap();
    }
}

#[test]
fn opening_after_a_checkpoint_replays_only_the_log_written_since() {
    let fs = SimFileSystem::new();
    let mut cache = create(&fs, 64);
    cache.set_auto_checkpoint_bytes(None);
    let mut model = allocated(&mut cache, 10);
    let mut rng = TestRng::new(1);
    rounds(&mut cache, &mut model, &mut rng, 40);
    drop(cache);
    let mut cache = open(&fs, Access::ReadWrite).unwrap();
    let whole = cache.log_stats();
    assert_eq!(whole.replayed_commits, 41);
    assert!(whole.replayed_bytes > 40 * 8192, "{whole:?}");
    cache.set_auto_checkpoint_bytes(None);
    cache.checkpoint().unwrap();
    assert_eq!(cache.log_stats().checkpoints, 1);
    assert_eq!(cache.log_stats().bytes_since_checkpoint, 0);
    rounds(&mut cache, &mut model, &mut rng, 3);
    let since = cache.log_stats().bytes_since_checkpoint;
    drop(cache);
    fs.crash(CrashMode::Torn { seed: 7 });
    let cache = open(&fs, Access::ReadOnly).unwrap();
    let tail = cache.log_stats();
    assert_eq!(tail.replayed_commits, 3);
    assert_eq!(tail.replayed_bytes, since);
    assert!(
        tail.replayed_pages <= 9 && tail.replayed_pages >= 3,
        "{tail:?}"
    );
    assert_model(&cache, &model, "after the checkpoint");
}

#[test]
fn a_checkpoint_with_no_new_log_writes_nothing_and_uncommitted_changes_refuse_one() {
    let fs = SimFileSystem::new();
    let mut cache = create(&fs, 16);
    let before = fs.operation_count();
    cache.checkpoint().unwrap();
    assert_eq!(fs.operation_count(), before);
    let mut model = allocated(&mut cache, 3);
    cache.checkpoint().unwrap();
    let after_first = fs.operation_count();
    cache.checkpoint().unwrap();
    assert_eq!(fs.operation_count(), after_first);
    assert_eq!(cache.log_stats().checkpoints, 1);
    let page = *model.keys().next().unwrap();
    cache.write(page).unwrap().fill(4);
    assert!(matches!(
        cache.checkpoint(),
        Err(StorageError::UncommittedChanges)
    ));
    cache.rollback().unwrap();
    let added = cache.allocate(leaf()).unwrap().page_id();
    cache.free(added).unwrap();
    assert!(matches!(
        cache.checkpoint(),
        Err(StorageError::UncommittedChanges)
    ));
    cache.rollback().unwrap();
    assert_eq!(fs.operation_count(), after_first);
    let mut rng = TestRng::new(2);
    change(&mut cache, &mut model, &mut rng, 9).unwrap();
    cache.checkpoint().unwrap();
    drop(cache);
    let reader = open(&fs, Access::ReadOnly).unwrap();
    assert_eq!(reader.log_stats().replayed_commits, 0);
    assert_model(&reader, &model, "reopened");
    let mut reader = reader;
    assert!(matches!(reader.checkpoint(), Err(StorageError::ReadOnly)));
}

#[test]
fn commits_checkpoint_on_their_own_and_reuse_the_log_s_segments_as_it_wraps_around() {
    let fs = SimFileSystem::new();
    let mut cache = create(&fs, 64);
    let mut model = allocated(&mut cache, 12);
    let mut rng = TestRng::new(3);
    let mut sizes = Vec::new();
    for lap in 0..8u64 {
        rounds(&mut cache, &mut model, &mut rng, 150);
        sizes.push(cache.superblock().page_count);
        assert!(
            cache.log_stats().bytes_since_checkpoint <= 8 * SEGMENT_PAGES * PAGE,
            "{:?}",
            cache.log_stats()
        );
        drop(cache);
        fs.crash(CrashMode::Torn { seed: lap });
        cache = open(&fs, Access::ReadWrite).unwrap();
        assert_model(&cache, &model, &format!("lap {lap}"));
        assert!(cache.log_stats().replayed_bytes <= 8 * SEGMENT_PAGES * PAGE);
    }
    assert!(
        sizes
            .windows(2)
            .skip(1)
            .all(|pair| pair[1] <= pair[0] + SEGMENT_PAGES)
    );
    let data_and_log = 2 + 12 + 9 * SEGMENT_PAGES + 8 * SEGMENT_PAGES;
    assert!(sizes[7] <= data_and_log, "{sizes:?}");
    let superblock = cache.superblock();
    assert!(
        superblock
            .recycled_segments
            .iter()
            .filter(|&&first| first != NO_PAGE)
            .count()
            >= 3,
        "{superblock:?}"
    );
}

#[test]
fn a_log_without_automatic_checkpoints_grows_and_replays_whole() {
    let fs = SimFileSystem::new();
    let mut cache = create(&fs, 64);
    cache.set_auto_checkpoint_bytes(None);
    assert_eq!(cache.auto_checkpoint_bytes(), None);
    let mut model = allocated(&mut cache, 12);
    let mut rng = TestRng::new(4);
    rounds(&mut cache, &mut model, &mut rng, 300);
    assert_eq!(cache.log_stats().checkpoints, 0);
    let grown = cache.superblock().page_count;
    assert!(grown > 2 + 12 + 20 * SEGMENT_PAGES, "{grown}");
    drop(cache);
    let mut cache = open(&fs, Access::ReadWrite).unwrap();
    assert_eq!(cache.log_stats().replayed_commits, 301);
    assert_eq!(
        cache.auto_checkpoint_bytes(),
        Some(crate::log::DEFAULT_AUTO_CHECKPOINT_BYTES)
    );
    change(&mut cache, &mut model, &mut rng, 7).unwrap();
    assert_eq!(cache.log_stats().checkpoints, 1);
    drop(cache);
    let cache = open(&fs, Access::ReadOnly).unwrap();
    assert_eq!(cache.log_stats().replayed_commits, 0);
    assert!(cache.superblock().free_page_count > 10 * SEGMENT_PAGES);
    assert_model(
        &cache,
        &model,
        "after the checkpoint freed the extra segments",
    );
}

fn before_checkpoint(
    fs: &SimFileSystem,
    durability: Durability,
    big_commit: bool,
) -> (PageCache<SimFile>, Model) {
    let mut cache = create(fs, 256);
    cache.set_auto_checkpoint_bytes(None);
    let mut model = allocated(&mut cache, if big_commit { 150 } else { 12 });
    cache.set_durability(durability).unwrap();
    let mut rng = TestRng::new(5);
    rounds(&mut cache, &mut model, &mut rng, 20);
    cache.checkpoint().unwrap();
    rounds(&mut cache, &mut model, &mut rng, 30);
    if big_commit {
        for (&page, fill) in model.iter_mut() {
            cache.write(page).unwrap().fill(200);
            *fill = 200;
        }
        cache.commit(|_| {}).unwrap();
    }
    cache.sync().unwrap();
    (cache, model)
}

fn crash_at_every_operation_of_a_checkpoint(durability: Durability, big_commit: bool) {
    let operations = {
        let fs = SimFileSystem::new();
        let (mut cache, model) = before_checkpoint(&fs, durability, big_commit);
        let start = fs.operation_count();
        cache.checkpoint().unwrap();
        let operations = fs.operation_count() - start;
        drop(cache);
        let cache = open(&fs, Access::ReadOnly).unwrap();
        assert_eq!(cache.log_stats().replayed_commits, 0);
        assert_model(&cache, &model, "checkpointed");
        operations
    };
    assert!(operations >= 3, "{operations}");
    for step in 0..operations {
        for seed in 0..4 {
            let context = format!("{durability:?}, big {big_commit}, step {step}, seed {seed}");
            let fs = SimFileSystem::new();
            let (mut cache, mut model) = before_checkpoint(&fs, durability, big_commit);
            fs.fail_operation(fs.operation_count() + step);
            assert!(cache.checkpoint().is_err(), "{context}");
            assert!(matches!(
                cache.commit(|_| {}),
                Err(StorageError::NeedsReopen)
            ));
            drop(cache);
            fs.crash(CrashMode::Torn { seed });
            let mut cache = open(&fs, Access::ReadWrite).unwrap();
            assert_model(&cache, &model, &context);
            cache.set_auto_checkpoint_bytes(None);
            let mut rng = TestRng::new(seed);
            rounds(&mut cache, &mut model, &mut rng, 12);
            cache.checkpoint().unwrap();
            rounds(&mut cache, &mut model, &mut rng, 2);
            drop(cache);
            fs.crash(CrashMode::Torn { seed: seed + 10 });
            let mut file = PageFile::open(&fs, Path::new(DB), Access::ReadWrite).unwrap();
            let mut handed_out = std::collections::BTreeSet::new();
            while file.available_pages() > 0 {
                let page = file.allocate().unwrap();
                assert!(!model.contains_key(&page), "{context}: page {page}");
                assert!(handed_out.insert(page));
            }
            drop(file);
            let cache = open(&fs, Access::ReadOnly).unwrap();
            assert_model(&cache, &model, &context);
        }
    }
}

#[test]
fn a_crash_at_any_operation_of_a_checkpoint_keeps_every_commit() {
    crash_at_every_operation_of_a_checkpoint(Durability::Full, false);
    crash_at_every_operation_of_a_checkpoint(Durability::Normal, false);
}

#[test]
fn a_crash_at_any_operation_of_a_checkpoint_that_frees_extra_segments_keeps_every_commit() {
    crash_at_every_operation_of_a_checkpoint(Durability::Full, true);
}

#[test]
fn extra_segments_freed_by_a_checkpoint_are_handed_out_as_pages() {
    let fs = SimFileSystem::new();
    let (mut cache, model) = before_checkpoint(&fs, Durability::Full, true);
    let free_before = cache.superblock().free_page_count;
    cache.checkpoint().unwrap();
    let superblock = cache.superblock();
    assert_eq!(
        superblock
            .recycled_segments
            .iter()
            .filter(|&&first| first != NO_PAGE)
            .count(),
        RECYCLED_SEGMENTS
    );
    assert!(superblock.free_page_count >= free_before + 2 * SEGMENT_PAGES);
    drop(cache);
    let mut file = PageFile::open(&fs, Path::new(DB), Access::ReadWrite).unwrap();
    let mut handed_out = std::collections::BTreeSet::new();
    while file.available_pages() > 0 {
        handed_out.insert(file.allocate().unwrap());
    }
    assert!(handed_out.len() as u64 >= 2 * SEGMENT_PAGES);
    assert!(handed_out.iter().all(|page| !model.contains_key(page)));
    let head = superblock.log_head_page;
    let reserved: Vec<u64> = std::iter::once(head)
        .chain(superblock.recycled_segments)
        .filter(|&first| first != NO_PAGE)
        .collect();
    for page in &handed_out {
        assert!(
            reserved
                .iter()
                .all(|&first| !(first..first + SEGMENT_PAGES).contains(page)),
            "page {page}"
        );
    }
}

fn head_start_lsn(fs: &SimFileSystem, head: u64) -> u64 {
    let block = header_block(fs, head);
    u64::from_le_bytes(block[8..16].try_into().unwrap())
}

fn left_the_head_segment(fs: &SimFileSystem, cache: &PageCache<SimFile>) -> bool {
    let superblock = cache.superblock();
    let capacity = SEGMENT_PAGES * PAGE - LOG_BLOCK as u64;
    let position = superblock.checkpoint_lsn + cache.log_stats().bytes_since_checkpoint;
    position >= head_start_lsn(fs, superblock.log_head_page) + capacity
}

type Setup = fn(&SimFileSystem) -> (PageCache<SimFile>, Model);

fn a_fresh_log(fs: &SimFileSystem) -> (PageCache<SimFile>, Model) {
    let mut cache = create(fs, 64);
    cache.set_auto_checkpoint_bytes(None);
    let model = allocated(&mut cache, 8);
    (cache, model)
}

fn a_log_with_recycled_segments(fs: &SimFileSystem) -> (PageCache<SimFile>, Model) {
    let (mut cache, mut model) = a_fresh_log(fs);
    let mut rng = TestRng::new(12);
    grow_log_by_segments(&mut cache, &mut model, &mut rng, 2);
    cache.checkpoint().unwrap();
    assert!(cache.superblock().recycled_segments[1] != NO_PAGE);
    (cache, model)
}

fn a_reopened_log_with_recycled_segments(fs: &SimFileSystem) -> (PageCache<SimFile>, Model) {
    let (cache, model) = a_log_with_recycled_segments(fs);
    let generation = cache.superblock().generation;
    drop(cache);
    let mut cache = open(fs, Access::ReadWrite).unwrap();
    cache.set_auto_checkpoint_bytes(None);
    assert_eq!(cache.superblock().generation, generation + 1);
    (cache, model)
}

fn rounds_until_the_log_leaves_its_head(setup: Setup) -> usize {
    let fs = SimFileSystem::new();
    let (mut cache, mut model) = setup(&fs);
    let page_count = cache.superblock().page_count;
    let recycled = cache.superblock().recycled_segments[0] != NO_PAGE;
    let mut rng = TestRng::new(11);
    for round in 0..200 {
        change(&mut cache, &mut model, &mut rng, 100).unwrap();
        if left_the_head_segment(&fs, &cache) {
            assert_eq!(cache.superblock().page_count == page_count, recycled);
            return round;
        }
    }
    panic!("no commit left the head segment");
}

fn up_to_the_crossing_commit(
    fs: &SimFileSystem,
    setup: Setup,
    crossing: usize,
    fail_at: Option<u64>,
) -> (PageCache<SimFile>, Model, Model, u64) {
    let (mut cache, mut model) = setup(fs);
    let mut rng = TestRng::new(11);
    for _ in 0..crossing {
        change(&mut cache, &mut model, &mut rng, 100).unwrap();
    }
    let old = model.clone();
    let start = fs.operation_count();
    if let Some(step) = fail_at {
        fs.fail_operation(start + step);
    }
    let committed = change(&mut cache, &mut model, &mut rng, 101);
    assert_eq!(committed.is_ok(), fail_at.is_none());
    (cache, old, model, fs.operation_count() - start)
}

fn crash_at_every_operation_of_a_commit_leaving_its_segment(setup: Setup) {
    let crossing = rounds_until_the_log_leaves_its_head(setup);
    let (new, operations) = {
        let fs = SimFileSystem::new();
        let (cache, _, new, operations) = up_to_the_crossing_commit(&fs, setup, crossing, None);
        assert!(left_the_head_segment(&fs, &cache));
        (new, operations)
    };
    let mut outcomes = std::collections::BTreeSet::new();
    for step in 0..operations {
        for seed in 0..4 {
            let context = format!("step {step}, seed {seed}");
            let fs = SimFileSystem::new();
            let (cache, old, _, _) = up_to_the_crossing_commit(&fs, setup, crossing, Some(step));
            drop(cache);
            fs.crash(CrashMode::Torn { seed });
            let mut cache = open(&fs, Access::ReadWrite).unwrap();
            let is_new = cache.superblock().clock_high_water == 101;
            let mut model = if is_new { new.clone() } else { old };
            assert_model(&cache, &model, &context);
            outcomes.insert(is_new);
            let mut rng = TestRng::new(seed);
            for fill in 102..110 {
                change(&mut cache, &mut model, &mut rng, fill).unwrap();
            }
            drop(cache);
            fs.crash(CrashMode::Torn { seed: seed + 20 });
            let cache = open(&fs, Access::ReadOnly).unwrap();
            assert_model(&cache, &model, &context);
        }
    }
    assert_eq!(outcomes.len(), 2);
}

#[test]
fn a_crash_at_any_operation_of_a_commit_that_grows_a_new_segment_keeps_the_last_durable_commit() {
    crash_at_every_operation_of_a_commit_leaving_its_segment(a_fresh_log);
}

#[test]
fn a_crash_at_any_operation_of_a_commit_that_reuses_a_recycled_segment_keeps_the_last_durable_commit()
 {
    crash_at_every_operation_of_a_commit_leaving_its_segment(a_log_with_recycled_segments);
}

#[test]
fn a_crash_at_any_operation_of_the_first_commit_after_a_reopen_to_leave_its_segment_keeps_the_last_durable_commit()
 {
    crash_at_every_operation_of_a_commit_leaving_its_segment(a_reopened_log_with_recycled_segments);
}

#[test]
fn a_reopened_writer_reserves_new_sequences_once_before_using_them() {
    let fs = SimFileSystem::new();
    let (mut cache, mut model) = a_reopened_log_with_recycled_segments(&fs);
    let before = cache.superblock();
    let mut rng = TestRng::new(19);
    let mut fill = 0u8;
    while !left_the_head_segment(&fs, &cache) {
        fill += 1;
        change(&mut cache, &mut model, &mut rng, fill).unwrap();
    }
    let after = cache.superblock();
    assert_eq!(after.generation, before.generation + 1);
    assert_eq!(
        after.log_sequence_reserved,
        before.log_sequence_reserved + 1 + crate::log::SEQUENCE_RESERVATION
    );
    let recycled = first_recycled(&before);
    let block = header_block(&fs, recycled);
    let sequence = u64::from_le_bytes(block[16..24].try_into().unwrap());
    assert_eq!(sequence, before.log_sequence_reserved + 1);
    grow_log_by_segments(&mut cache, &mut model, &mut rng, 1);
    assert_eq!(cache.superblock().generation, after.generation);
    drop(cache);
    let cache = open(&fs, Access::ReadOnly).unwrap();
    assert_model(&cache, &model, "reopened");
}

#[test]
fn stale_records_left_in_a_recycled_segment_by_a_lost_commit_never_replay() {
    let fs = SimFileSystem::new();
    let (mut cache, mut model) = a_log_with_recycled_segments(&fs);
    let recycled = first_recycled(&cache.superblock());
    let pages: Vec<u64> = model.keys().copied().collect();
    let boundary = {
        let superblock = cache.superblock();
        head_start_lsn(&fs, superblock.log_head_page) + SEGMENT_PAGES * PAGE - LOG_BLOCK as u64
    };
    let mut fill = 1u8;
    let before_lost = loop {
        let before = file_bytes(&fs);
        fill += 1;
        cache.write(pages[1]).unwrap().fill(fill);
        cache.commit(|_| {}).unwrap();
        if header_block(&fs, recycled) != before[segment_range(recycled)][..LOG_BLOCK] {
            let superblock = cache.superblock();
            let position = superblock.checkpoint_lsn + cache.log_stats().bytes_since_checkpoint;
            assert!(
                position > boundary,
                "the lost commit must end in the recycled segment"
            );
            break before;
        }
        model.insert(pages[1], fill);
    };
    let lost = file_bytes(&fs);
    drop(cache);
    let mut crashed = before_lost.clone();
    crashed[segment_range(recycled)].copy_from_slice(&lost[segment_range(recycled)]);
    replace_file(&fs, &crashed);
    let mut cache = open(&fs, Access::ReadWrite).unwrap();
    assert_model(&cache, &model, "the commit whose link was lost");
    let tail = cache.superblock().log_head_page;
    cache.write(pages[2]).unwrap().fill(0xEE);
    cache.commit(|_| {}).unwrap();
    let rewritten = file_bytes(&fs);
    assert!(
        rewritten[segment_range(recycled)][..LOG_BLOCK]
            != lost[segment_range(recycled)][..LOG_BLOCK],
        "the next commit gave the recycled segment the same identity as the lost one"
    );
    drop(cache);
    let mut torn = crashed;
    torn[..2 * HEADER_SLOT_SIZE].copy_from_slice(&rewritten[..2 * HEADER_SLOT_SIZE]);
    torn[segment_range(tail)].copy_from_slice(&rewritten[segment_range(tail)]);
    replace_file(&fs, &torn);
    for access in [Access::ReadOnly, Access::ReadWrite] {
        let cache = open(&fs, access).unwrap();
        assert_model(&cache, &model, "the next commit whose segment was lost");
    }
}

struct Forgery {
    geometry: LogGeometry,
    superblock: Superblock,
    head: Segment,
    offset: usize,
}

fn checkpointed_for_forgery(fs: &SimFileSystem) -> (Forgery, Model) {
    let (mut cache, mut model) = a_log_with_recycled_segments(fs);
    let mut rng = TestRng::new(13);
    rounds(&mut cache, &mut model, &mut rng, 3);
    cache.checkpoint().unwrap();
    let superblock = cache.superblock();
    drop(cache);
    let head = Segment {
        first_page: superblock.log_head_page,
        sequence: superblock.log_head_sequence,
        start_lsn: head_start_lsn(fs, superblock.log_head_page),
    };
    let forgery = Forgery {
        geometry: LogGeometry::new(FILE_ID, PageSize::DEFAULT, superblock.log_segment_pages),
        superblock,
        head,
        offset: LOG_BLOCK + (superblock.checkpoint_lsn - head.start_lsn) as usize,
    };
    (forgery, model)
}

fn forge(
    fs: &SimFileSystem,
    forgery: &Forgery,
    next_sequence: u64,
    flush: impl FnOnce(&mut LogWriter, &mut u64),
) {
    forge_commit(fs, forgery, next_sequence, flush, |_| {});
}

fn forge_commit(
    fs: &SimFileSystem,
    forgery: &Forgery,
    next_sequence: u64,
    flush: impl FnOnce(&mut LogWriter, &mut u64),
    update: impl FnOnce(&mut CommitFields),
) {
    let mut writer = LogWriter::new(
        forgery.geometry,
        WriterStart {
            segment: forgery.head,
            offset: forgery.offset,
            needs_fresh_segment: false,
            durable_lsn: 0,
            recycled: [NO_PAGE; RECYCLED_SEGMENTS],
            next_sequence,
        },
    );
    let mut page_count = forgery.superblock.page_count;
    writer.begin(&mut page_count).unwrap();
    flush(&mut writer, &mut page_count);
    writer.reserve(COMMIT_PAYLOAD, &mut page_count).unwrap();
    let mut fields = CommitFields::of(&forgery.superblock, 0);
    fields.clock_high_water = 0xF0F0;
    update(&mut fields);
    writer.put_commit(&fields);
    writer.seal(&mut page_count).unwrap();
    let raw = raw(fs);
    if raw.size().unwrap() < page_count * PAGE {
        raw.set_size(page_count * PAGE).unwrap();
    }
    writer.write(&raw).unwrap();
    raw.sync_all().unwrap();
}

fn image_of(page_id: u64) -> Vec<u8> {
    let mut page = vec![0x3C; PAGE as usize];
    PageChecksum::new(FILE_ID, PageSize::DEFAULT)
        .write(&mut page, page_id, &leaf())
        .unwrap();
    page
}

fn assert_corrupt_log(fs: &SimFileSystem, context: &str) {
    let before = file_bytes(fs);
    for access in [Access::ReadOnly, Access::ReadWrite] {
        let error = open(fs, access).unwrap_err();
        assert!(
            matches!(error, StorageError::Format(FormatError::CorruptLog { .. })),
            "{context}: {error}"
        );
    }
    assert_eq!(file_bytes(fs), before, "{context}");
}

#[test]
fn a_replayed_image_of_a_recycled_segment_refuses_the_open() {
    let fs = SimFileSystem::new();
    let (forgery, model) = checkpointed_for_forgery(&fs);
    let data_page = *model.keys().next().unwrap();
    forge(
        &fs,
        &forgery,
        forgery.head.sequence + 1,
        |writer, page_count| {
            writer.reserve(8 + PAGE as usize, page_count).unwrap();
            writer.put_image(data_page, &image_of(data_page));
        },
    );
    let cache = open(&fs, Access::ReadOnly).unwrap();
    assert_eq!(cache.superblock().clock_high_water, 0xF0F0);
    assert_eq!(fill_of(&cache, data_page), 0x3C);
    drop(cache);
    for index in [0, 1] {
        let fs = SimFileSystem::new();
        let (forgery, _) = checkpointed_for_forgery(&fs);
        let target = forgery.superblock.recycled_segments[index] + 3;
        forge(
            &fs,
            &forgery,
            forgery.head.sequence + 1,
            |writer, page_count| {
                writer.reserve(8 + PAGE as usize, page_count).unwrap();
                writer.put_image(target, &image_of(target));
            },
        );
        assert_corrupt_log(&fs, &format!("recycled segment {index}"));
    }
}

fn forced_link(writer: &mut LogWriter, page_count: &mut u64) {
    writer
        .reserve((SEGMENT_PAGES * PAGE) as usize, page_count)
        .unwrap();
}

#[test]
fn a_link_naming_a_sequence_the_header_never_reserved_refuses_the_open() {
    let fs = SimFileSystem::new();
    let (forgery, _) = checkpointed_for_forgery(&fs);
    let reserved = forgery.superblock.log_sequence_reserved;
    assert!(reserved > forgery.head.sequence);
    forge(&fs, &forgery, reserved, forced_link);
    let cache = open(&fs, Access::ReadOnly).unwrap();
    assert_eq!(cache.superblock().clock_high_water, 0xF0F0);
    drop(cache);
    for sequence in [reserved + 1, u64::MAX - 1, forgery.head.sequence] {
        let fs = SimFileSystem::new();
        let (forgery, _) = checkpointed_for_forgery(&fs);
        forge(&fs, &forgery, sequence, forced_link);
        assert_corrupt_log(&fs, &format!("sequence {sequence}"));
    }
}

#[test]
fn a_link_to_a_recycled_segment_the_header_does_not_list_refuses_the_open() {
    let fs = SimFileSystem::new();
    let (forgery, model) = checkpointed_for_forgery(&fs);
    let data_page = *model.keys().next().unwrap();
    let mut forged = forgery;
    forged.superblock.page_count = data_page;
    let next = forged.head.sequence + 1;
    let mut writer = LogWriter::new(
        forged.geometry,
        WriterStart {
            segment: forged.head,
            offset: forged.offset,
            needs_fresh_segment: false,
            durable_lsn: 0,
            recycled: [
                data_page, NO_PAGE, NO_PAGE, NO_PAGE, NO_PAGE, NO_PAGE, NO_PAGE,
            ],
            next_sequence: next,
        },
    );
    let mut page_count = forged.superblock.page_count;
    writer.begin(&mut page_count).unwrap();
    forced_link(&mut writer, &mut page_count);
    writer.seal(&mut page_count).unwrap();
    let raw = raw(&fs);
    writer.write(&raw).unwrap();
    raw.sync_all().unwrap();
    assert_corrupt_log(&fs, "a data page linked as a recycled segment");
}

#[test]
fn an_older_header_whose_log_was_recycled_refuses_the_open() {
    let fs = SimFileSystem::new();
    let (mut cache, mut model) = a_log_with_recycled_segments(&fs);
    let old_head = cache.superblock().log_head_page;
    let mut rng = TestRng::new(14);
    grow_log_by_segments(&mut cache, &mut model, &mut rng, 1);
    let mut fill = 0u8;
    while !left_the_head_segment(&fs, &cache) {
        fill = fill.wrapping_add(1).max(1);
        change(&mut cache, &mut model, &mut rng, fill).unwrap();
    }
    cache.checkpoint().unwrap();
    let superblock = cache.superblock();
    assert!(superblock.recycled_segments.contains(&old_head));
    let old_head_block = header_block(&fs, old_head);
    while header_block(&fs, old_head) == old_head_block {
        fill = fill.wrapping_add(1).max(1);
        change(&mut cache, &mut model, &mut rng, fill).unwrap();
    }
    drop(cache);
    let raw = raw(&fs);
    let newest = superblock.slot() * HEADER_SLOT_SIZE;
    raw.write_all_at(&[0xFF; 4], newest as u64 + 64).unwrap();
    raw.sync_all().unwrap();
    drop(raw);
    let error = open(&fs, Access::ReadOnly).unwrap_err();
    assert!(
        matches!(
            error,
            StorageError::Format(FormatError::InvalidField {
                field: "log_head_sequence"
            })
        ),
        "{error}"
    );
}

#[test]
fn a_crash_at_any_operation_of_recovering_a_wrapped_log_keeps_the_replayed_state() {
    let wrapped = |fs: &SimFileSystem| {
        let mut cache = create(fs, 64);
        let mut model = allocated(&mut cache, 12);
        cache.set_durability(Durability::Normal).unwrap();
        let mut rng = TestRng::new(15);
        rounds(&mut cache, &mut model, &mut rng, 400);
        cache.sync().unwrap();
        assert!(cache.log_stats().checkpoints >= 3);
        assert!(cache.log_stats().bytes_since_checkpoint > 0);
        model
    };
    let operations = {
        let fs = SimFileSystem::new();
        wrapped(&fs);
        let before = fs.operation_count();
        let cache = open(&fs, Access::ReadWrite).unwrap();
        assert!(cache.log_stats().replayed_commits > 0);
        fs.operation_count() - before
    };
    for step in 0..operations {
        for seed in 0..3 {
            let fs = SimFileSystem::new();
            let model = wrapped(&fs);
            fs.fail_operation(fs.operation_count() + step);
            assert!(open(&fs, Access::ReadWrite).is_err());
            fs.crash(CrashMode::Torn { seed });
            let cache = open(&fs, Access::ReadWrite).unwrap();
            assert_model(&cache, &model, &format!("step {step}, seed {seed}"));
        }
    }
}

#[test]
fn a_small_cache_serves_committed_pages_across_checkpoints_under_every_durability() {
    for durability in [Durability::Full, Durability::Normal, Durability::Off] {
        let fs = SimFileSystem::new();
        let mut cache = create(&fs, 3);
        let mut model = Model::new();
        for _ in 0..12 {
            let page = cache.allocate(leaf()).unwrap().page_id();
            model.insert(page, 0);
            cache.commit(|_| {}).unwrap();
        }
        cache.set_durability(durability).unwrap();
        cache.set_auto_checkpoint_bytes(Some(3 * PAGE * SEGMENT_PAGES));
        let mut rng = TestRng::new(16);
        for round in 0..300 {
            change(&mut cache, &mut model, &mut rng, (round % 250) as u8 + 1).unwrap();
            if round % 7 == 0 {
                let page = *model.keys().nth(rng.below(model.len())).unwrap();
                assert_eq!(fill_of(&cache, page), model[&page], "{durability:?}");
            }
        }
        assert!(cache.log_stats().checkpoints >= 5, "{durability:?}");
        assert!(cache.stats().evictions > 100);
        assert_model(&cache, &model, &format!("{durability:?}"));
        cache.sync().unwrap();
        drop(cache);
        let cache = open(&fs, Access::ReadOnly).unwrap();
        assert_model(&cache, &model, &format!("{durability:?}, reopened"));
    }
}

#[test]
fn mutated_wrapped_logs_never_panic_or_hang() {
    let pristine = SimFileSystem::new();
    let mut cache = create(&pristine, 64);
    let mut model = allocated(&mut cache, 12);
    let mut rng = TestRng::new(17);
    rounds(&mut cache, &mut model, &mut rng, 230);
    let superblock = cache.superblock();
    drop(cache);
    let image = file_bytes(&pristine);
    let geometry = LogGeometry::new(FILE_ID, PageSize::DEFAULT, superblock.log_segment_pages);
    let segments: Vec<u64> = std::iter::once(superblock.log_head_page)
        .chain(superblock.recycled_segments)
        .filter(|&first| first != NO_PAGE)
        .collect();
    let fs = SimFileSystem::new();
    let raw = fs.open(Path::new(DB), OpenMode::CreateNew).unwrap();
    let mut rng = TestRng::new(0xC4EC_4901);
    let mut opened = 0;
    for case in 0..300 {
        let mut bytes = image.clone();
        let first = segments[rng.below(segments.len())];
        let range = segment_range(first);
        match case % 4 {
            0 => {
                for _ in 0..1 + rng.below(8) {
                    let at = range.start + rng.below(range.len());
                    bytes[at] = rng.next_u64() as u8;
                }
            }
            1 => {
                let block = range.start + LOG_BLOCK * (1 + rng.below(SEGMENT_PAGES as usize - 1));
                let sequence = u64::from_le_bytes(
                    bytes[range.start + 16..range.start + 24]
                        .try_into()
                        .unwrap(),
                );
                let start_lsn = u64::from_le_bytes(
                    bytes[range.start + 8..range.start + 16].try_into().unwrap(),
                );
                let lsn = start_lsn + (block - range.start - LOG_BLOCK) as u64;
                let fields = [
                    segments[rng.below(segments.len())] + rng.below(3) as u64,
                    [0, superblock.page_count + rng.below(40) as u64][rng.below(2)],
                    superblock.log_head_sequence + rng.below(4) as u64,
                ];
                bytes[block + 4..block + 8].copy_from_slice(&24u32.to_le_bytes());
                bytes[block + 8] = 5;
                bytes[block + 9] = 0;
                for (index, field) in fields.iter().enumerate() {
                    let at = block + RECORD_HEADER + 8 * index;
                    bytes[at..at + 8].copy_from_slice(&field.to_le_bytes());
                }
                let seed = geometry.record_seed(sequence, lsn);
                let checksum = crc32c_append(seed, &bytes[block + 4..block + RECORD_HEADER + 24]);
                bytes[block..block + 4].copy_from_slice(&checksum.to_le_bytes());
            }
            2 => {
                let at = range.start + rng.below(range.len());
                let end = (at + 512).min(range.end);
                rng.fill(&mut bytes[at..end]);
            }
            _ => {
                let cut = 2 * PAGE as usize + rng.below(bytes.len() - 2 * PAGE as usize);
                bytes.truncate(cut);
            }
        }
        raw.set_size(0).unwrap();
        raw.write_all_at(&bytes, 0).unwrap();
        raw.sync_all().unwrap();
        for access in [Access::ReadOnly, Access::ReadWrite] {
            if let Ok(mut cache) = open(&fs, access) {
                opened += 1;
                for &page in model.keys() {
                    let _ = cache.read(page).map(|bytes| bytes[0]);
                }
                if access == Access::ReadWrite {
                    let _ = cache.checkpoint();
                }
            }
        }
    }
    assert!(opened > 100, "{opened}");
}

fn tree_rounds(
    fs: &SimFileSystem,
    models: &mut Vec<TreeModel>,
    fail_at: Option<u64>,
) -> (u64, usize) {
    let mut cache = btree_tests::open(fs, Access::ReadWrite, 512);
    cache.set_auto_checkpoint_bytes(Some(2 * PAGE * SEGMENT_PAGES));
    let tree = stored_tree(&cache);
    let mut rng = TestRng::new(18);
    let mut model = models.last().unwrap().clone();
    let start = fs.operation_count();
    if let Some(step) = fail_at {
        fs.fail_operation(start + step);
    }
    let mut committed = 0;
    for _ in 0..12 {
        let mut next = model.clone();
        let mut changed = Ok(());
        for _ in 0..40 {
            let key = bytes_up_to(&mut rng, 4, 12);
            if next.len() > 20 && rng.below(4) == 0 {
                let existing = next.keys().next().unwrap().clone();
                next.remove(&existing);
                changed = changed.and_then(|()| tree.delete(&cache, &existing).map(|_| ()));
            } else {
                let value = bytes_up_to(&mut rng, 0, 200);
                next.insert(key.clone(), value.clone());
                changed = changed.and_then(|()| tree.put(&cache, &key, &value));
            }
        }
        if fail_at.is_none() || models.len() < 13 {
            models.push(next.clone());
        }
        if changed.and_then(|()| cache.commit(|_| {})).is_err() {
            break;
        }
        committed += 1;
        model = next;
    }
    (fs.operation_count() - start, committed)
}

#[test]
fn a_tree_reopens_whole_after_a_crash_at_any_operation_of_commits_that_checkpoint() {
    let (operations, models) = {
        let fs = SimFileSystem::new();
        let mut models = vec![btree_tests::tree_with_entries(&fs, 3, 300)];
        let (operations, committed) = tree_rounds(&fs, &mut models, None);
        assert_eq!(committed, 12);
        let cache = btree_tests::open(&fs, Access::ReadOnly, 64);
        assert!(cache.log_stats().replayed_commits < 12);
        assert_matches(&stored_tree(&cache), &cache, models.last().unwrap());
        (operations, models)
    };
    for step in (0..operations).step_by(3) {
        for seed in 0..2 {
            let context = format!("step {step}, seed {seed}");
            let fs = SimFileSystem::new();
            let mut attempted = vec![btree_tests::tree_with_entries(&fs, 3, 300)];
            let (_, committed) = tree_rounds(&fs, &mut attempted, Some(step));
            fs.crash(CrashMode::Torn { seed });
            let cache = btree_tests::open(&fs, Access::ReadWrite, 64);
            let tree = stored_tree(&cache);
            let entries = tree.check(&cache).unwrap().entries;
            let candidates = [committed, committed + 1];
            let matching = candidates
                .iter()
                .filter(|&&index| index < models.len())
                .find(|&&index| models[index].len() as u64 == entries)
                .unwrap_or_else(|| panic!("{context}: {entries} entries"));
            assert_matches(&tree, &cache, &models[*matching]);
        }
    }
}

fn unsynced_normal_commits(fs: &SimFileSystem) -> (PageCache<SimFile>, BTreeMap<u64, Model>) {
    let mut cache = create(fs, 64);
    cache.set_auto_checkpoint_bytes(None);
    let mut model = allocated(&mut cache, 12);
    cache.set_durability(Durability::Normal).unwrap();
    let mut history = BTreeMap::from([(0, model.clone())]);
    let mut rng = TestRng::new(20);
    for fill in 1..=40u8 {
        change(&mut cache, &mut model, &mut rng, fill).unwrap();
        history.insert(u64::from(fill), model.clone());
        if fill == 20 {
            cache.checkpoint().unwrap();
        }
    }
    (cache, history)
}

#[test]
fn a_crash_at_any_operation_of_a_checkpoint_of_unsynced_normal_commits_keeps_a_whole_prefix() {
    let operations = {
        let fs = SimFileSystem::new();
        let (mut cache, history) = unsynced_normal_commits(&fs);
        let start = fs.operation_count();
        cache.checkpoint().unwrap();
        let operations = fs.operation_count() - start;
        drop(cache);
        fs.crash(CrashMode::Torn { seed: 1 });
        let cache = open(&fs, Access::ReadOnly).unwrap();
        assert_eq!(cache.superblock().clock_high_water, 40);
        assert_model(&cache, &history[&40], "checkpointed");
        operations
    };
    let mut rounds_kept = std::collections::BTreeSet::new();
    for step in 0..operations {
        for seed in 0..4 {
            let context = format!("step {step}, seed {seed}");
            let fs = SimFileSystem::new();
            let (mut cache, history) = unsynced_normal_commits(&fs);
            fs.fail_operation(fs.operation_count() + step);
            assert!(cache.checkpoint().is_err(), "{context}");
            drop(cache);
            fs.crash(CrashMode::Torn { seed });
            let mut cache = open(&fs, Access::ReadWrite).unwrap();
            let round = cache.superblock().clock_high_water;
            assert!(round >= 20, "{context}: round {round}");
            let mut model = history[&round].clone();
            assert_model(&cache, &model, &context);
            rounds_kept.insert(round);
            let mut rng = TestRng::new(seed);
            change(&mut cache, &mut model, &mut rng, 99).unwrap();
            drop(cache);
            fs.crash(CrashMode::Torn { seed: seed + 30 });
            let cache = open(&fs, Access::ReadOnly).unwrap();
            assert_model(&cache, &model, &context);
        }
    }
    assert!(rounds_kept.contains(&40), "{rounds_kept:?}");
    assert!(rounds_kept.len() > 1, "{rounds_kept:?}");
}

fn with_newest_slot_damaged(bytes: &[u8]) -> Vec<u8> {
    let (slot0, slot1) = bytes[..2 * HEADER_SLOT_SIZE].split_at(HEADER_SLOT_SIZE);
    let newest = Superblock::choose(slot0, slot1).unwrap().superblock.slot();
    let mut damaged = bytes.to_vec();
    let at = newest * HEADER_SLOT_SIZE + 64;
    damaged[at..at + 4].copy_from_slice(&[0xFF; 4]);
    damaged
}

fn assert_refused_or_latest(bytes: &[u8], model: &Model, context: &str) -> bool {
    let fs = SimFileSystem::new();
    let raw = fs.open(Path::new(DB), OpenMode::CreateNew).unwrap();
    raw.write_all_at(&with_newest_slot_damaged(bytes), 0)
        .unwrap();
    raw.sync_all().unwrap();
    drop(raw);
    let Ok(cache) = open(&fs, Access::ReadOnly) else {
        return false;
    };
    for (&page, &fill) in model {
        let read = cache.read(page).map(|bytes| bytes[0]);
        assert_eq!(
            read.ok(),
            Some(fill),
            "{context}: an older header opened a state that never existed, page {page}"
        );
    }
    true
}

#[test]
fn a_damaged_newest_header_slot_never_opens_a_state_that_never_existed() {
    for seed in 0..6 {
        let fs = SimFileSystem::new();
        let mut cache = create(&fs, 256);
        let mut model = allocated(&mut cache, 150);
        let mut rng = TestRng::new(seed);
        let (mut refused, mut opened) = (0, 0);
        for round in 0..400usize {
            let fill = (round % 250) as u8 + 1;
            if round % 97 == 96 {
                for (&page, value) in model.iter_mut() {
                    cache.write(page).unwrap().fill(fill);
                    *value = fill;
                }
                cache.commit(|_| {}).unwrap();
                for _ in 0..32 {
                    let mut page = cache.allocate(leaf()).unwrap();
                    page.fill(fill);
                    model.insert(page.page_id(), fill);
                }
                cache.commit(|_| {}).unwrap();
            } else {
                change(&mut cache, &mut model, &mut rng, fill).unwrap();
            }
            if round % 3 == 0 || round % 97 == 96 {
                let context = format!("seed {seed}, round {round}");
                if assert_refused_or_latest(&file_bytes(&fs), &model, &context) {
                    opened += 1;
                } else {
                    refused += 1;
                }
            }
        }
        assert!(
            cache.log_stats().checkpoints >= 8,
            "{:?}",
            cache.log_stats()
        );
        assert!(
            refused > 0 && opened > 0,
            "seed {seed}: {refused} refused, {opened} opened"
        );
    }
}

#[test]
fn a_damaged_newest_header_slot_after_freeing_extra_segments_never_opens_a_mixed_state() {
    let fs = SimFileSystem::new();
    let (mut cache, model) = before_checkpoint(&fs, Durability::Full, true);
    let older_head = cache.superblock().log_head_page;
    let free_before = cache.superblock().free_page_count;
    cache.checkpoint().unwrap();
    assert!(cache.superblock().free_page_count > free_before + SEGMENT_PAGES);
    drop(cache);
    let mut file = PageFile::open(&fs, Path::new(DB), Access::ReadWrite).unwrap();
    file.set_auto_checkpoint_bytes(None);
    let mut freed_segments = Vec::new();
    while file.available_pages() > 0 {
        let page = file.allocate().unwrap();
        let block = header_block(&fs, page);
        if page != older_head && block[4] == PageType::LogSegment.to_byte() && block[5] == 1 {
            freed_segments.push(page);
        }
    }
    file.commit(|_| {}).unwrap();
    drop(file);
    assert!(freed_segments.len() >= 2, "{freed_segments:?}");
    let raw = raw(&fs);
    for &first in &freed_segments {
        for page in first..first + SEGMENT_PAGES {
            raw.write_all_at(&image_of(page), page * PAGE).unwrap();
        }
    }
    raw.sync_all().unwrap();
    drop(raw);
    let bytes = file_bytes(&fs);
    assert_refused_or_latest(&bytes, &model, "after freed segments became data pages");
}

#[test]
fn a_replayed_free_list_that_names_a_data_page_refuses_the_open_before_any_write() {
    let fs = SimFileSystem::new();
    let (forgery, model) = checkpointed_for_forgery(&fs);
    let data_page = *model.keys().next().unwrap();
    forge_commit(
        &fs,
        &forgery,
        forgery.head.sequence + 1,
        |_, _| {},
        |fields| {
            fields.free_list_head = data_page;
            fields.free_page_count = 1;
        },
    );
    let before = file_bytes(&fs);
    let syncs = fs.sync_count();
    for access in [Access::ReadWrite, Access::ReadOnly] {
        assert!(matches!(
            open(&fs, access),
            Err(StorageError::CorruptFreeList { page }) if page == data_page
        ));
    }
    assert_eq!(file_bytes(&fs), before);
    assert_eq!(fs.sync_count(), syncs);
}

#[test]
fn a_commit_naming_a_log_page_as_the_catalog_root_is_refused_and_the_file_stays_usable() {
    let fs = SimFileSystem::new();
    let (mut cache, mut model) = a_fresh_log(&fs);
    let mut rng = TestRng::new(21);
    grow_log_by_segments(&mut cache, &mut model, &mut rng, 1);
    let newest_segment = cache.superblock().page_count - SEGMENT_PAGES;
    for root in [
        newest_segment,
        newest_segment + 5,
        cache.superblock().log_head_page,
    ] {
        let before = fs.operation_count();
        assert!(matches!(
            cache.commit(|superblock| superblock.catalog_root = root),
            Err(StorageError::Format(FormatError::InvalidField {
                field: "catalog_root"
            }))
        ));
        assert_eq!(fs.operation_count(), before);
    }
    let root = *model.keys().next().unwrap();
    cache
        .commit(|superblock| superblock.catalog_root = root)
        .unwrap();
    change(&mut cache, &mut model, &mut rng, 50).unwrap();
    cache.checkpoint().unwrap();
    drop(cache);
    let cache = open(&fs, Access::ReadOnly).unwrap();
    assert_eq!(cache.superblock().catalog_root, root);
    assert_model(&cache, &model, "after the refused commit");
}

#[test]
fn a_failed_automatic_checkpoint_keeps_the_commit_and_refuses_every_later_page() {
    let commit_with_failure = |fail_last: bool| {
        let fs = SimFileSystem::new();
        let mut cache = create(&fs, 16);
        let mut model = allocated(&mut cache, 4);
        cache.set_auto_checkpoint_bytes(Some(1));
        let page = *model.keys().next().unwrap();
        cache.write(page).unwrap().fill(6);
        model.insert(page, 6);
        let start = fs.operation_count();
        if fail_last {
            fs.fail_operation(start + 5);
        }
        let committed = cache.commit(|superblock| superblock.clock_high_water = 6);
        (fs, cache, model, committed, start)
    };
    let (fs, cache, _, committed, start) = commit_with_failure(false);
    committed.unwrap();
    assert_eq!(fs.operation_count() - start, 6);
    assert_eq!(cache.log_stats().checkpoints, 1);
    let (fs, mut cache, model, committed, _) = commit_with_failure(true);
    committed.unwrap();
    assert_eq!(cache.stats().cached_pages, 0);
    for &page in model.keys() {
        assert!(matches!(cache.read(page), Err(StorageError::NeedsReopen)));
        assert!(matches!(cache.write(page), Err(StorageError::NeedsReopen)));
    }
    assert!(matches!(cache.checkpoint(), Err(StorageError::NeedsReopen)));
    drop(cache);
    fs.crash(CrashMode::Torn { seed: 4 });
    let cache = open(&fs, Access::ReadOnly).unwrap();
    assert_eq!(cache.superblock().clock_high_water, 6);
    assert_model(&cache, &model, "after the failed checkpoint");
}

fn header_slots(fs: &SimFileSystem) -> (Superblock, Result<Superblock, FormatError>) {
    let bytes = file_bytes(fs);
    let (slot0, slot1) = bytes[..2 * HEADER_SLOT_SIZE].split_at(HEADER_SLOT_SIZE);
    let newest = Superblock::choose(slot0, slot1).unwrap().superblock;
    let older = match newest.slot() {
        0 => Superblock::decode(slot1, 1),
        _ => Superblock::decode(slot0, 0),
    };
    (newest, older)
}

fn assert_older_slot_is_a_copy(fs: &SimFileSystem, context: &str) {
    let (newest, older) = header_slots(fs);
    let older = older.unwrap_or_else(|error| panic!("{context}: older slot: {error}"));
    assert_eq!(older.generation + 1, newest.generation, "{context}");
    assert_eq!(
        Superblock {
            generation: newest.generation,
            ..older
        },
        newest,
        "{context}"
    );
}

fn reuse_free_pages_without_a_header_write(fs: &SimFileSystem, older: &Superblock) {
    let mut file = PageFile::open(fs, Path::new(DB), Access::ReadWrite).unwrap();
    file.set_auto_checkpoint_bytes(None);
    let generation = file.superblock().generation;
    let mut reused = Vec::new();
    while file.available_pages() > 0 {
        reused.push(file.allocate().unwrap());
    }
    file.commit(|_| {}).unwrap();
    assert_eq!(file.superblock().generation, generation);
    drop(file);
    let raw = raw(fs);
    let older_head = older.log_head_page..older.log_head_page + SEGMENT_PAGES;
    let kept = |page: &u64| older_head.contains(page) || *page == older.free_list_head;
    for page in reused.into_iter().filter(|page| !kept(page)) {
        raw.write_all_at(&image_of(page), page * PAGE).unwrap();
    }
    raw.sync_all().unwrap();
}

#[test]
fn a_crash_between_a_freeing_checkpoint_s_header_writes_never_opens_a_mixed_state_after_the_newest_slot_is_damaged()
 {
    let operations = {
        let fs = SimFileSystem::new();
        let (mut cache, _) = before_checkpoint(&fs, Durability::Full, true);
        let start = fs.operation_count();
        cache.checkpoint().unwrap();
        fs.operation_count() - start
    };
    let mut windows = 0;
    for step in 0..operations {
        for mode in [CrashMode::DropUnsynced, CrashMode::Torn { seed: step }] {
            let context = format!("step {step}, {mode:?}");
            let fs = SimFileSystem::new();
            let (mut cache, model) = before_checkpoint(&fs, Durability::Full, true);
            let free_before = cache.superblock().free_page_count;
            fs.fail_operation(fs.operation_count() + step);
            assert!(cache.checkpoint().is_err(), "{context}");
            drop(cache);
            fs.crash(mode);
            let (newest, older) = header_slots(&fs);
            let older = older.unwrap_or(newest);
            let in_the_window = older.log_head_page != newest.log_head_page
                && newest.free_page_count > free_before + SEGMENT_PAGES;
            windows += u64::from(in_the_window);
            drop(open(&fs, Access::ReadWrite).unwrap());
            assert_older_slot_is_a_copy(&fs, &context);
            assert!(
                assert_refused_or_latest(&file_bytes(&fs), &model, &context),
                "{context}: the copy in the older slot must open the same state"
            );
            reuse_free_pages_without_a_header_write(&fs, &older);
            assert_refused_or_latest(&file_bytes(&fs), &model, &context);
        }
    }
    assert!(windows > 0, "no crash fell between the two header writes");
}

#[test]
fn a_crash_in_the_first_flush_into_a_recycled_segment_never_opens_a_mixed_state_after_the_newest_slot_is_damaged()
 {
    let setup: Setup = a_log_with_recycled_segments;
    let crossing = rounds_until_the_log_leaves_its_head(setup);
    let (new, operations) = {
        let fs = SimFileSystem::new();
        let (_, _, new, operations) = up_to_the_crossing_commit(&fs, setup, crossing, None);
        (new, operations)
    };
    let mut cut_off_flushes = 0;
    for step in 0..operations {
        for seed in 0..6 {
            let context = format!("step {step}, seed {seed}");
            let fs = SimFileSystem::new();
            let (cache, old, _, _) = up_to_the_crossing_commit(&fs, setup, crossing, Some(step));
            drop(cache);
            fs.crash(CrashMode::Torn { seed });
            let reopened = open(&fs, Access::ReadWrite).unwrap();
            let is_new = reopened.superblock().clock_high_water == 101;
            let model = if is_new { new.clone() } else { old };
            cut_off_flushes += u64::from(reopened.log_stats().replayed_bytes > 0 || !is_new);
            drop(reopened);
            assert_older_slot_is_a_copy(&fs, &context);
            assert!(
                assert_refused_or_latest(&file_bytes(&fs), &model, &context),
                "{context}: the copy in the older slot must open the same state"
            );
        }
    }
    assert!(cut_off_flushes > 0);
}

#[test]
fn opening_for_writing_copies_the_newest_header_to_the_older_slot_once_and_reading_never_writes() {
    let fs = SimFileSystem::new();
    let (mut cache, model) = a_log_with_recycled_segments(&fs);
    cache.checkpoint().unwrap();
    drop(cache);
    let (newest, older) = header_slots(&fs);
    assert!(older.is_ok_and(|older| older.checkpoint_lsn != newest.checkpoint_lsn));
    let before = file_bytes(&fs);
    drop(open(&fs, Access::ReadOnly).unwrap());
    assert_eq!(file_bytes(&fs), before);
    let syncs = fs.sync_count();
    drop(open(&fs, Access::ReadWrite).unwrap());
    assert_eq!(
        fs.sync_count(),
        syncs + 1,
        "the copy shares the open's sync"
    );
    assert_older_slot_is_a_copy(&fs, "first open for writing");
    let copied = file_bytes(&fs);
    drop(open(&fs, Access::ReadWrite).unwrap());
    assert_eq!(
        file_bytes(&fs)[..2 * HEADER_SLOT_SIZE],
        copied[..2 * HEADER_SLOT_SIZE]
    );
    let damaged = with_newest_slot_damaged(&copied);
    assert!(assert_refused_or_latest(&copied, &model, "copy"));
    replace_file(&fs, &damaged);
    drop(open(&fs, Access::ReadWrite).unwrap());
    assert_older_slot_is_a_copy(&fs, "a damaged slot is written over");
    let file = PageFile::open(&fs, Path::new(DB), Access::ReadOnly).unwrap();
    assert_eq!(file.superblock().generation, newest.generation + 1);
    assert_eq!(file.damaged_header_slot(), None);
    drop(file);
    let cache = open(&fs, Access::ReadOnly).unwrap();
    assert_model(&cache, &model, "after the damaged slot was written over");
}
