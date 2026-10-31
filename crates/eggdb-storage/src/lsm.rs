use crate::btree::{AppendHint, BTree, TreeReport};
use crate::cursor::{Cursor, ScanOrder};
use crate::error::{FormatError, StorageError};
use crate::page_cache::{PageCache, PageSource};
use crate::page_set::PageSet;
use eggdb_platform::File;
use std::cmp::Ordering;
use std::ops::Bound;

const DELTA_KEY: &[u8] = b"d";
const MERGE_KEY: &[u8] = b"m";
const NEXT_RUN_KEY: &[u8] = b"n";
const RUN_PREFIX: u8 = b'r';
const LIVE: u8 = 1;
const TOMBSTONE: u8 = 0;
const DELTA_ENTRY_LIMIT: u64 = if cfg!(test) { 48 } else { 16_384 };
const MOST_RUNS: usize = if cfg!(test) { 3 } else { 8 };
const MERGE_STEP_ENTRIES: usize = if cfg!(test) { 16 } else { 256 };
const MOST_COMPONENTS: usize = 64;

fn damaged(problem: &'static str) -> StorageError {
    FormatError::CorruptLsm { problem }.into()
}

fn u64_at(bytes: &[u8], at: usize) -> Result<u64, StorageError> {
    let slice = bytes
        .get(at..at + 8)
        .ok_or_else(|| damaged("a manifest entry is too short"))?;
    let mut word = [0u8; 8];
    word.copy_from_slice(slice);
    Ok(u64::from_le_bytes(word))
}

#[derive(Debug, Clone, Copy)]
struct Component {
    tree: BTree,
    entries: u64,
}

impl Component {
    fn encode(self) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&self.tree.root().to_le_bytes());
        bytes[8..].copy_from_slice(&self.entries.to_le_bytes());
        bytes
    }

    fn decode(pages: &impl PageSource, bytes: &[u8]) -> Result<Self, StorageError> {
        if bytes.len() != 16 {
            return Err(damaged("a component entry is not 16 bytes"));
        }
        Ok(Self {
            tree: BTree::open_in(pages, u64_at(bytes, 0)?)?,
            entries: u64_at(bytes, 8)?,
        })
    }
}

#[derive(Debug, Clone)]
struct Run {
    number: u32,
    component: Component,
}

#[derive(Debug, Clone)]
struct Merge {
    target: Component,
    resume_after: Option<Vec<u8>>,
    sources: Vec<u32>,
}

impl Merge {
    fn encode(&self) -> Vec<u8> {
        let mut bytes = self.target.encode().to_vec();
        match &self.resume_after {
            Some(key) => {
                bytes.push(1);
                bytes.extend_from_slice(&(key.len() as u32).to_le_bytes());
                bytes.extend_from_slice(key);
            }
            None => bytes.push(0),
        }
        bytes.extend_from_slice(&(self.sources.len() as u32).to_le_bytes());
        for source in &self.sources {
            bytes.extend_from_slice(&source.to_le_bytes());
        }
        bytes
    }

    fn decode(pages: &impl PageSource, bytes: &[u8]) -> Result<Self, StorageError> {
        let short = || damaged("a merge entry is cut short");
        let target = Component::decode(pages, bytes.get(..16).ok_or_else(short)?)?;
        let mut at = 16;
        let resume_after = match bytes.get(at).copied().ok_or_else(short)? {
            0 => {
                at += 1;
                None
            }
            1 => {
                let length = read_u32(bytes, at + 1)? as usize;
                at += 5;
                let key = bytes.get(at..at + length).ok_or_else(short)?.to_vec();
                at += length;
                Some(key)
            }
            _ => return Err(damaged("a merge entry has an unknown resume marker")),
        };
        let count = read_u32(bytes, at)? as usize;
        at += 4;
        if count > MOST_COMPONENTS || bytes.len() != at + count * 4 {
            return Err(damaged("a merge entry lists another number of sources than it holds"));
        }
        let sources = (0..count)
            .map(|index| read_u32(bytes, at + index * 4))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            target,
            resume_after,
            sources,
        })
    }
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, StorageError> {
    let slice = bytes
        .get(at..at + 4)
        .ok_or_else(|| damaged("a manifest entry is too short"))?;
    let mut word = [0u8; 4];
    word.copy_from_slice(slice);
    Ok(u32::from_le_bytes(word))
}

fn run_key(number: u32) -> [u8; 5] {
    let mut key = [RUN_PREFIX, 0, 0, 0, 0];
    key[1..].copy_from_slice(&number.to_be_bytes());
    key
}

#[derive(Debug, Clone)]
struct Manifest {
    delta: Component,
    runs: Vec<Run>,
    merge: Option<Merge>,
    next_run: u32,
}

impl Manifest {
    fn read(pages: &impl PageSource, tree: &BTree) -> Result<Self, StorageError> {
        let required = |key: &[u8], problem| -> Result<Vec<u8>, StorageError> {
            tree.get_in(pages, key, |value| value.into_vec())?
                .ok_or_else(|| damaged(problem))
        };
        let delta = Component::decode(pages, &required(DELTA_KEY, "the manifest has no delta")?)?;
        let next = required(NEXT_RUN_KEY, "the manifest has no run counter")?;
        if next.len() != 4 {
            return Err(damaged("the run counter is not 4 bytes"));
        }
        let next_run = read_u32(&next, 0)?;
        let merge = match tree.get_in(pages, MERGE_KEY, |value| value.into_vec())? {
            Some(bytes) => Some(Merge::decode(pages, &bytes)?),
            None => None,
        };
        let mut runs = Vec::new();
        let mut cursor = tree.scan_prefix(&[RUN_PREFIX], ScanOrder::Descending);
        while let Some((key, value)) = cursor.next_in(pages)? {
            if key.len() != 5 || runs.len() >= MOST_COMPONENTS {
                return Err(damaged("a run entry has a malformed key or there are too many runs"));
            }
            let number = u32::from_be_bytes([key[1], key[2], key[3], key[4]]);
            if number >= next_run {
                return Err(damaged("a run is numbered at or above the run counter"));
            }
            let component = Component::decode(pages, value)?;
            runs.push(Run { number, component });
        }
        if let Some(merge) = &merge
            && merge
                .sources
                .iter()
                .any(|source| !runs.iter().any(|run| run.number == *source))
        {
            return Err(damaged("a merge names a run the manifest does not hold"));
        }
        Ok(Self {
            delta,
            runs,
            merge,
            next_run,
        })
    }

    fn components_newest_first(&self) -> impl Iterator<Item = &Component> {
        std::iter::once(&self.delta).chain(self.runs.iter().map(|run| &run.component))
    }
}

/// A log-structured merge tree whose components are B+trees: writes go to a small delta tree,
/// which becomes an immutable run once it holds enough entries, and runs are merged into one,
/// a bounded step at a time, once there are too many. Deletes are tombstones until a merge
/// that includes the oldest run drops them. Every component is a B+tree of the same file, so
/// commits, the log, checkpoints and snapshots treat it like any other tree, and no write
/// transaction holds more than a bounded number of a merge's pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LsmTree {
    manifest: BTree,
}

/// What [`LsmTree::check`] found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LsmReport {
    /// The components: the delta, the runs and a merge's target.
    pub components: u32,
    /// Entries across the components, tombstones and older versions included.
    pub stored_entries: u64,
    /// Pages of every component and of the manifest.
    pub pages: u64,
}

fn tagged(value: &[u8]) -> Vec<u8> {
    let mut stored = Vec::with_capacity(value.len() + 1);
    stored.push(LIVE);
    stored.extend_from_slice(value);
    stored
}

fn live_value(stored: &[u8]) -> Result<Option<&[u8]>, StorageError> {
    match stored.split_first() {
        Some((&LIVE, value)) => Ok(Some(value)),
        Some((&TOMBSTONE, [])) => Ok(None),
        _ => Err(damaged("an entry has an unknown tag")),
    }
}

impl LsmTree {
    /// The largest value an LSM tree stores: one byte less than [`BTree::MAX_VALUE_BYTES`].
    pub const MAX_VALUE_BYTES: usize = BTree::MAX_VALUE_BYTES - 1;

    /// Creates an empty tree: a manifest and an empty delta.
    pub fn create<F: File>(cache: &PageCache<F>) -> Result<Self, StorageError> {
        let manifest = BTree::create(cache)?;
        let delta = BTree::create(cache)?;
        let component = Component {
            tree: delta,
            entries: 0,
        };
        manifest.put(cache, DELTA_KEY, &component.encode())?;
        manifest.put(cache, NEXT_RUN_KEY, &1u32.to_le_bytes())?;
        Ok(Self { manifest })
    }

    /// Opens the tree whose manifest's root is page `root`.
    pub fn open<F: File>(cache: &PageCache<F>, root: u64) -> Result<Self, StorageError> {
        Self::open_in(cache, root)
    }

    pub(crate) fn open_in(pages: &impl PageSource, root: u64) -> Result<Self, StorageError> {
        let manifest = BTree::open_in(pages, root)?;
        Manifest::read(pages, &manifest)?;
        Ok(Self { manifest })
    }

    /// The manifest's root page, which names the tree.
    pub fn root(&self) -> u64 {
        self.manifest.root()
    }

    /// The value stored under `key`, or `None`.
    pub fn get<F: File>(
        &self,
        cache: &PageCache<F>,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.get_in(cache, key)
    }

    pub(crate) fn get_in(
        &self,
        pages: &impl PageSource,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, StorageError> {
        let manifest = Manifest::read(pages, &self.manifest)?;
        for component in manifest.components_newest_first() {
            if let Some(stored) = component.tree.get_in(pages, key, |value| value.into_vec())? {
                return Ok(live_value(&stored)?.map(<[u8]>::to_vec));
            }
        }
        Ok(None)
    }

    /// Stores `value` under `key`, replacing any value there. A full delta becomes a run, and
    /// a merge of the runs advances by a bounded step. Limits are those of [`BTree::put`], with
    /// [`LsmTree::MAX_VALUE_BYTES`] for values.
    pub fn put<F: File>(
        &self,
        cache: &PageCache<F>,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), StorageError> {
        if value.len() > Self::MAX_VALUE_BYTES {
            return Err(StorageError::ValueTooLarge {
                bytes: value.len(),
                max: Self::MAX_VALUE_BYTES,
            });
        }
        self.write(cache, key, &tagged(value))
    }

    /// Removes `key`, and returns whether it held a value.
    pub fn delete<F: File>(&self, cache: &PageCache<F>, key: &[u8]) -> Result<bool, StorageError> {
        if self.get_in(cache, key)?.is_none() {
            return Ok(false);
        }
        self.write(cache, key, &[TOMBSTONE])?;
        Ok(true)
    }

    fn write<F: File>(
        &self,
        cache: &PageCache<F>,
        key: &[u8],
        stored: &[u8],
    ) -> Result<(), StorageError> {
        BTree::check_fits(cache, key, stored)?;
        let mut manifest = Manifest::read(cache, &self.manifest)?;
        manifest.delta.tree.put(cache, key, stored)?;
        manifest.delta.entries += 1;
        self.manifest
            .put(cache, DELTA_KEY, &manifest.delta.encode())?;
        if manifest.delta.entries >= DELTA_ENTRY_LIMIT {
            self.seal_delta(cache, &mut manifest)?;
        }
        if manifest.merge.is_none() && manifest.runs.len() > MOST_RUNS {
            self.start_merge(cache, &mut manifest)?;
        }
        if manifest.merge.is_some() {
            self.merge_step(cache, &mut manifest, MERGE_STEP_ENTRIES)?;
        }
        Ok(())
    }

    fn seal_delta<F: File>(
        &self,
        cache: &PageCache<F>,
        manifest: &mut Manifest,
    ) -> Result<(), StorageError> {
        if manifest.runs.len() >= MOST_COMPONENTS - 1 {
            return Ok(());
        }
        let number = manifest.next_run;
        manifest.next_run = number
            .checked_add(1)
            .ok_or_else(|| damaged("the run counter is exhausted"))?;
        self.manifest
            .put(cache, &run_key(number), &manifest.delta.encode())?;
        manifest.runs.insert(
            0,
            Run {
                number,
                component: manifest.delta,
            },
        );
        manifest.delta = Component {
            tree: BTree::create(cache)?,
            entries: 0,
        };
        self.manifest
            .put(cache, DELTA_KEY, &manifest.delta.encode())?;
        self.manifest
            .put(cache, NEXT_RUN_KEY, &manifest.next_run.to_le_bytes())
    }

    fn start_merge<F: File>(
        &self,
        cache: &PageCache<F>,
        manifest: &mut Manifest,
    ) -> Result<(), StorageError> {
        let merge = Merge {
            target: Component {
                tree: BTree::create(cache)?,
                entries: 0,
            },
            resume_after: None,
            sources: manifest.runs.iter().map(|run| run.number).collect(),
        };
        self.manifest.put(cache, MERGE_KEY, &merge.encode())?;
        manifest.merge = Some(merge);
        Ok(())
    }

    fn merge_step<F: File>(
        &self,
        cache: &PageCache<F>,
        manifest: &mut Manifest,
        most: usize,
    ) -> Result<(), StorageError> {
        let Some(mut merge) = manifest.merge.clone() else {
            return Ok(());
        };
        let sources: Vec<Component> = merge
            .sources
            .iter()
            .filter_map(|number| manifest.runs.iter().find(|run| run.number == *number))
            .map(|run| run.component)
            .collect();
        let includes_oldest = manifest
            .runs
            .last()
            .is_some_and(|oldest| merge.sources.contains(&oldest.number));
        let start = match &merge.resume_after {
            Some(key) => Bound::Excluded(key.clone()),
            None => Bound::Unbounded,
        };
        let mut merged = Merged::new(
            sources.iter().map(|component| component.tree.root()).collect(),
            start,
            Bound::Unbounded,
            ScanOrder::Ascending,
        );
        let mut hint = AppendHint::default();
        let mut written = 0;
        let mut finished = true;
        while let Some((key, stored)) = merged.next_stored(cache)? {
            if written >= most {
                finished = false;
                break;
            }
            written += 1;
            merge.resume_after = Some(key.clone());
            if includes_oldest && live_value(&stored)?.is_none() {
                continue;
            }
            merge
                .target
                .tree
                .put_hinted(cache, &key, &stored, &mut hint)?;
            merge.target.entries += 1;
        }
        if !finished {
            self.manifest.put(cache, MERGE_KEY, &merge.encode())?;
            manifest.merge = Some(merge);
            return Ok(());
        }
        self.finish_merge(cache, manifest, merge)
    }

    fn finish_merge<F: File>(
        &self,
        cache: &PageCache<F>,
        manifest: &mut Manifest,
        merge: Merge,
    ) -> Result<(), StorageError> {
        let newest_source = merge.sources.iter().copied().max().unwrap_or(0);
        for source in &merge.sources {
            if let Some(at) = manifest.runs.iter().position(|run| run.number == *source) {
                let run = manifest.runs.remove(at);
                self.manifest.delete(cache, &run_key(run.number))?;
                free_tree(cache, &run.component.tree)?;
            }
        }
        self.manifest
            .put(cache, &run_key(newest_source), &merge.target.encode())?;
        let at = manifest
            .runs
            .iter()
            .position(|run| run.number < newest_source)
            .unwrap_or(manifest.runs.len());
        manifest.runs.insert(
            at,
            Run {
                number: newest_source,
                component: merge.target,
            },
        );
        self.manifest.delete(cache, MERGE_KEY)?;
        manifest.merge = None;
        Ok(())
    }

    /// Runs merge steps until no merge is left, within the current transaction. A large tree's
    /// merge dirties many pages, so call it on a tree that fits in the cache, or let puts
    /// advance merges a step at a time.
    pub fn finish_merges<F: File>(&self, cache: &PageCache<F>) -> Result<(), StorageError> {
        let mut manifest = Manifest::read(cache, &self.manifest)?;
        while manifest.merge.is_some() {
            self.merge_step(cache, &mut manifest, usize::MAX)?;
        }
        Ok(())
    }

    /// A cursor over the live entries with keys from `start` to `end`, in `order`. Like
    /// [`BTree::scan`], it holds no page between calls.
    pub fn scan_in(
        &self,
        pages: &impl PageSource,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        order: ScanOrder,
    ) -> Result<LsmCursor, StorageError> {
        let manifest = Manifest::read(pages, &self.manifest)?;
        let roots = manifest
            .components_newest_first()
            .map(|component| component.tree.root())
            .collect();
        Ok(LsmCursor {
            merged: Merged::new(roots, start.map(<[u8]>::to_vec), end.map(<[u8]>::to_vec), order),
            entry: (Vec::new(), Vec::new()),
        })
    }

    /// A cursor over the live entries with keys from `start` to `end` (see
    /// [`LsmTree::scan_in`]).
    pub fn scan<F: File>(
        &self,
        cache: &PageCache<F>,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
        order: ScanOrder,
    ) -> Result<LsmCursor, StorageError> {
        self.scan_in(cache, start, end, order)
    }

    /// Checks the manifest and every component as [`BTree::check`] does, and that no page
    /// belongs to two components.
    pub fn check<F: File>(&self, cache: &PageCache<F>) -> Result<LsmReport, StorageError> {
        self.check_in(cache)
    }

    pub(crate) fn check_in(&self, pages: &impl PageSource) -> Result<LsmReport, StorageError> {
        let manifest = Manifest::read(pages, &self.manifest)?;
        let mut used = PageSet::for_pages(pages.page_count())?;
        let mut report = LsmReport::default();
        let manifest_report = self.manifest.check_marking(pages, &mut used)?;
        report.pages += pages_of(&manifest_report);
        let merge_target = manifest.merge.as_ref().map(|merge| merge.target);
        for component in manifest.components_newest_first().chain(merge_target.iter()) {
            let found = component.tree.check_marking(pages, &mut used)?;
            report.components += 1;
            report.stored_entries += found.entries;
            report.pages += pages_of(&found);
        }
        Ok(report)
    }
}

fn pages_of(report: &TreeReport) -> u64 {
    report.inner_pages + report.leaf_pages + report.overflow_pages
}

fn free_tree<F: File>(cache: &PageCache<F>, tree: &BTree) -> Result<(), StorageError> {
    let mut used = PageSet::for_pages(PageSource::page_count(cache))?;
    tree.check_marking(cache, &mut used)?;
    for page in used.pages() {
        cache.free(page)?;
    }
    Ok(())
}

struct Merged {
    cursors: Vec<Cursor>,
    heads: Vec<Option<(Vec<u8>, Vec<u8>)>>,
    started: bool,
    order: ScanOrder,
}

impl Merged {
    fn new(roots: Vec<u64>, start: Bound<Vec<u8>>, end: Bound<Vec<u8>>, order: ScanOrder) -> Self {
        let cursors = roots
            .into_iter()
            .map(|root| Cursor::new(root, start.clone(), end.clone(), order))
            .collect::<Vec<_>>();
        let heads = vec![None; cursors.len()];
        Self {
            cursors,
            heads,
            started: false,
            order,
        }
    }

    fn refill(&mut self, pages: &impl PageSource, at: usize) -> Result<(), StorageError> {
        self.heads[at] = self.cursors[at]
            .next_in(pages)?
            .map(|(key, value)| (key.to_vec(), value.to_vec()));
        Ok(())
    }

    fn next_stored(
        &mut self,
        pages: &impl PageSource,
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>, StorageError> {
        if !self.started {
            for at in 0..self.cursors.len() {
                self.refill(pages, at)?;
            }
            self.started = true;
        }
        let mut chosen: Option<usize> = None;
        for (at, head) in self.heads.iter().enumerate() {
            let Some((key, _)) = head else {
                continue;
            };
            chosen = match chosen {
                None => Some(at),
                Some(best) => {
                    let best_key = &self.heads[best].as_ref().map(|(key, _)| key);
                    let order = best_key.map_or(Ordering::Less, |best_key| key.cmp(best_key));
                    let better = match self.order {
                        ScanOrder::Ascending => order == Ordering::Less,
                        ScanOrder::Descending => order == Ordering::Greater,
                    };
                    if better { Some(at) } else { Some(best) }
                }
            };
        }
        let Some(chosen) = chosen else {
            return Ok(None);
        };
        let Some(entry) = self.heads[chosen].clone() else {
            return Ok(None);
        };
        for at in 0..self.heads.len() {
            if self.heads[at]
                .as_ref()
                .is_some_and(|(key, _)| *key == entry.0)
            {
                self.refill(pages, at)?;
            }
        }
        Ok(Some(entry))
    }
}

/// A cursor over an [`LsmTree`]'s live entries, from [`LsmTree::scan`].
pub struct LsmCursor {
    merged: Merged,
    entry: (Vec<u8>, Vec<u8>),
}

impl std::fmt::Debug for LsmCursor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LsmCursor")
            .field("components", &self.merged.cursors.len())
            .finish_non_exhaustive()
    }
}

impl LsmCursor {
    /// The next live entry's key and value, or `None` once the range is done.
    pub fn next<F: File>(
        &mut self,
        cache: &PageCache<F>,
    ) -> Result<Option<(&[u8], &[u8])>, StorageError> {
        self.next_in(cache)
    }

    pub(crate) fn next_in(
        &mut self,
        pages: &impl PageSource,
    ) -> Result<Option<(&[u8], &[u8])>, StorageError> {
        loop {
            let Some((key, stored)) = self.merged.next_stored(pages)? else {
                return Ok(None);
            };
            if live_value(&stored)?.is_some() {
                self.entry = (key, stored);
                return Ok(Some((&self.entry.0, &self.entry.1[1..])));
            }
        }
    }
}
