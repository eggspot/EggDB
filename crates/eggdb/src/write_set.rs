//! The changes of a concurrent transaction (D1337): kept in memory until they pass a budget,
//! then written out as sorted runs to scratch files beside the database, so a transaction may
//! change more than memory holds. Lookups read the memory, then the runs from the newest;
//! ordered walks merge them all, the newest change of a key winning.

use crate::error::{Error, ErrorKind, Result};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MEMORY_BUDGET_BYTES: usize = 32 << 20;
const BLOCK_BYTES: u64 = 64 << 10;
const ENTRY_OVERHEAD: usize = 48;

static NEXT_RUN: AtomicU64 = AtomicU64::new(0);

type Entry = (Vec<u8>, Option<Vec<u8>>);

fn io_error(error: std::io::Error) -> Error {
    Error::new(ErrorKind::Io, format!("a concurrent transaction's scratch file: {error}"))
}

fn damaged() -> Error {
    Error::new(ErrorKind::Io, "a concurrent transaction's scratch file was cut short")
}

#[derive(Debug)]
struct SpilledRun {
    file: File,
    path: PathBuf,
    blocks: Vec<(Vec<u8>, u64)>,
    length: u64,
    entries: usize,
}

impl Drop for SpilledRun {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn write_entry(out: &mut impl Write, key: &[u8], value: Option<&[u8]>) -> std::io::Result<u64> {
    out.write_all(&(key.len() as u32).to_le_bytes())?;
    out.write_all(key)?;
    match value {
        Some(value) => {
            out.write_all(&[1])?;
            out.write_all(&(value.len() as u32).to_le_bytes())?;
            out.write_all(value)?;
            Ok(9 + key.len() as u64 + value.len() as u64)
        }
        None => {
            out.write_all(&[0])?;
            Ok(5 + key.len() as u64)
        }
    }
}

fn read_entry(input: &mut impl Read) -> Result<Entry> {
    let mut word = [0u8; 4];
    input.read_exact(&mut word).map_err(|_| damaged())?;
    let mut key = vec![0u8; u32::from_le_bytes(word) as usize];
    input.read_exact(&mut key).map_err(|_| damaged())?;
    let mut flag = [0u8; 1];
    input.read_exact(&mut flag).map_err(|_| damaged())?;
    if flag[0] == 0 {
        return Ok((key, None));
    }
    input.read_exact(&mut word).map_err(|_| damaged())?;
    let mut value = vec![0u8; u32::from_le_bytes(word) as usize];
    input.read_exact(&mut value).map_err(|_| damaged())?;
    Ok((key, Some(value)))
}

impl SpilledRun {
    fn write(directory: &Path, stem: &str, entries: &BTreeMap<Vec<u8>, Option<Vec<u8>>>) -> Result<Self> {
        let number = NEXT_RUN.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!("{stem}-txn-{}-{number}.spill", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(io_error)?;
        let mut run = Self { file, path, blocks: Vec::new(), length: 0, entries: entries.len() };
        let mut out = BufWriter::new(&run.file);
        let mut block_start = None;
        for (key, value) in entries {
            if block_start.is_none_or(|start| run.length - start >= BLOCK_BYTES) {
                run.blocks.push((key.clone(), run.length));
                block_start = Some(run.length);
            }
            run.length += write_entry(&mut out, key, value.as_deref()).map_err(io_error)?;
        }
        out.flush().map_err(io_error)?;
        drop(out);
        Ok(run)
    }

    fn reader_at(&self, offset: u64) -> Result<BufReader<File>> {
        let mut file = self.file.try_clone().map_err(io_error)?;
        file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        Ok(BufReader::new(file))
    }

    fn block_for(&self, key: &[u8]) -> usize {
        self.blocks.partition_point(|(first, _)| first.as_slice() <= key).saturating_sub(1)
    }

    fn get(&self, key: &[u8]) -> Result<Option<Option<Vec<u8>>>> {
        let Some(&(_, start)) = self.blocks.get(self.block_for(key)) else {
            return Ok(None);
        };
        let end = self.blocks.get(self.block_for(key) + 1).map_or(self.length, |(_, offset)| *offset);
        let mut reader = self.reader_at(start)?.take(end - start);
        let mut read = 0u64;
        while read < end - start {
            let (found, value) = read_entry(&mut reader)?;
            read += 5 + found.len() as u64 + value.as_ref().map_or(0, |value| 4 + value.len() as u64);
            match found.as_slice().cmp(key) {
                std::cmp::Ordering::Equal => return Ok(Some(value)),
                std::cmp::Ordering::Greater => return Ok(None),
                std::cmp::Ordering::Less => {}
            }
        }
        Ok(None)
    }

    fn walk_from(&self, start: &Bound<Vec<u8>>) -> Result<RunWalk> {
        let offset = match start {
            Bound::Included(key) | Bound::Excluded(key) => self.blocks.get(self.block_for(key)).map_or(0, |(_, offset)| *offset),
            Bound::Unbounded => 0,
        };
        Ok(RunWalk { reader: self.reader_at(offset)?, left: self.length - offset })
    }
}

struct RunWalk {
    reader: BufReader<File>,
    left: u64,
}

impl RunWalk {
    fn next(&mut self) -> Result<Option<Entry>> {
        if self.left == 0 {
            return Ok(None);
        }
        let (key, value) = read_entry(&mut self.reader)?;
        let used = 5 + key.len() as u64 + value.as_ref().map_or(0, |value| 4 + value.len() as u64);
        self.left = self.left.checked_sub(used).ok_or_else(damaged)?;
        Ok(Some((key, value)))
    }
}

/// A concurrent transaction's changes: `Some(value)` puts, `None` deletes.
#[derive(Debug)]
pub(crate) struct WriteSet {
    memory: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    memory_bytes: usize,
    budget: usize,
    runs: Vec<SpilledRun>,
    directory: Option<(PathBuf, String)>,
}

impl WriteSet {
    /// An empty set that spills beside the database file `database`, or never for an
    /// in-memory database.
    pub(crate) fn beside(database: &Path, in_memory: bool) -> Self {
        let directory = (!in_memory).then(|| {
            let directory = database.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
            let stem = database.file_name().map_or_else(|| "eggdb".to_owned(), |name| name.to_string_lossy().into_owned());
            (directory.to_path_buf(), stem)
        });
        Self { memory: BTreeMap::new(), memory_bytes: 0, budget: MEMORY_BUDGET_BYTES, runs: Vec::new(), directory }
    }

    #[cfg(test)]
    pub(crate) fn with_budget(mut self, budget: usize) -> Self {
        self.budget = budget;
        self
    }

    /// About how many changes it holds (a key changed again after a spill counts twice).
    pub(crate) fn len(&self) -> usize {
        self.memory.len() + self.runs.iter().map(|run| run.entries).sum::<usize>()
    }

    /// How many runs it has written to scratch files.
    pub(crate) fn spilled_runs(&self) -> usize {
        self.runs.len()
    }

    /// The change to `key`: `None` when it has none.
    pub(crate) fn get(&self, key: &[u8]) -> Result<Option<Option<Vec<u8>>>> {
        if let Some(found) = self.memory.get(key) {
            return Ok(Some(found.clone()));
        }
        for run in self.runs.iter().rev() {
            if let Some(found) = run.get(key)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// Records a change to `key`, writing the changes in memory out as a run when they pass
    /// the budget.
    pub(crate) fn insert(&mut self, key: Vec<u8>, value: Option<Vec<u8>>) -> Result<()> {
        let added = ENTRY_OVERHEAD + key.len() + value.as_ref().map_or(0, Vec::len);
        if let Some(old) = self.memory.insert(key, value) {
            self.memory_bytes = self.memory_bytes.saturating_sub(old.map_or(0, |old| old.len()));
        }
        self.memory_bytes += added;
        if self.memory_bytes > self.budget
            && let Some((directory, stem)) = &self.directory
        {
            let run = SpilledRun::write(directory, stem, &self.memory)?;
            self.runs.push(run);
            self.memory.clear();
            self.memory_bytes = 0;
        }
        Ok(())
    }

    /// Calls `visit` with each changed key in `start..end`, in key order, with its newest change.
    pub(crate) fn for_each_in(
        &self,
        start: &Bound<Vec<u8>>,
        end: &Bound<Vec<u8>>,
        mut visit: impl FnMut(&[u8], Option<&[u8]>) -> Result<()>,
    ) -> Result<()> {
        if is_empty_range(start, end) {
            return Ok(());
        }
        let mut walks = Vec::with_capacity(self.runs.len());
        for run in &self.runs {
            walks.push(run.walk_from(start)?);
        }
        let mut heads: Vec<Option<Entry>> = Vec::with_capacity(walks.len() + 1);
        for walk in &mut walks {
            heads.push(walk.next()?);
        }
        let mut memory = self.memory.range::<Vec<u8>, _>((start.clone(), end.clone()));
        heads.push(memory.next().map(|(key, value)| (key.clone(), value.clone())));
        loop {
            let smallest = heads.iter().flatten().map(|(key, _)| key.as_slice()).min().map(<[u8]>::to_vec);
            let Some(key) = smallest else {
                return Ok(());
            };
            let mut newest: Option<Option<Vec<u8>>> = None;
            for (index, head) in heads.iter_mut().enumerate() {
                if head.as_ref().is_some_and(|(found, _)| *found == key) {
                    let (_, value) = head.take().ok_or_else(damaged)?;
                    newest = Some(value);
                    *head = match walks.get_mut(index) {
                        Some(walk) => walk.next()?,
                        None => memory.next().map(|(key, value)| (key.clone(), value.clone())),
                    };
                }
            }
            if !in_start(&key, start) {
                continue;
            }
            if !in_end(&key, end) {
                return Ok(());
            }
            if let Some(value) = newest {
                visit(&key, value.as_deref())?;
            }
        }
    }

    /// Calls `visit` with every changed key in key order, with its newest change.
    pub(crate) fn for_each(&self, visit: impl FnMut(&[u8], Option<&[u8]>) -> Result<()>) -> Result<()> {
        self.for_each_in(&Bound::Unbounded, &Bound::Unbounded, visit)
    }
}

fn is_empty_range(start: &Bound<Vec<u8>>, end: &Bound<Vec<u8>>) -> bool {
    match (start, end) {
        (Bound::Included(start), Bound::Included(end)) => start > end,
        (Bound::Included(start) | Bound::Excluded(start), Bound::Excluded(end))
        | (Bound::Excluded(start), Bound::Included(end)) => start >= end,
        _ => false,
    }
}

fn in_start(key: &[u8], start: &Bound<Vec<u8>>) -> bool {
    match start {
        Bound::Included(start) => key >= start.as_slice(),
        Bound::Excluded(start) => key > start.as_slice(),
        Bound::Unbounded => true,
    }
}

fn in_end(key: &[u8], end: &Bound<Vec<u8>>) -> bool {
    match end {
        Bound::Included(end) => key <= end.as_slice(),
        Bound::Excluded(end) => key < end.as_slice(),
        Bound::Unbounded => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collected(set: &WriteSet) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        let mut found = Vec::new();
        set.for_each(|key, value| {
            found.push((key.to_vec(), value.map(<[u8]>::to_vec)));
            Ok(())
        })
        .unwrap();
        found
    }

    #[test]
    fn changes_spill_to_runs_and_read_back_newest_first() {
        let directory = std::env::temp_dir().join(format!("eggdb-write-set-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut set = WriteSet::beside(&directory.join("db"), false).with_budget(4_000);
        let mut model = BTreeMap::new();
        for round in 0..5u32 {
            for index in 0..200u32 {
                let key = format!("k{:05}", (index * 7 + round * 13) % 500).into_bytes();
                let value = (index % 9 != 0).then(|| vec![round as u8; 20]);
                set.insert(key.clone(), value.clone()).unwrap();
                model.insert(key, value);
            }
        }
        assert!(set.spilled_runs() > 1);
        for (key, value) in &model {
            assert_eq!(set.get(key).unwrap(), Some(value.clone()));
        }
        assert_eq!(set.get(b"absent").unwrap(), None);
        let expected: Vec<_> = model.clone().into_iter().collect();
        assert_eq!(collected(&set), expected);
        let mut ranged = Vec::new();
        set.for_each_in(&Bound::Included(b"k00100".to_vec()), &Bound::Excluded(b"k00200".to_vec()), |key, value| {
            ranged.push((key.to_vec(), value.map(<[u8]>::to_vec)));
            Ok(())
        })
        .unwrap();
        let expected: Vec<_> = model.range(b"k00100".to_vec()..b"k00200".to_vec()).map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(ranged, expected);
        drop(set);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
        std::fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn an_in_memory_database_never_spills() {
        let mut set = WriteSet::beside(Path::new(":memory:"), true).with_budget(10);
        for index in 0..50u32 {
            set.insert(index.to_be_bytes().to_vec(), Some(vec![1; 10])).unwrap();
        }
        assert_eq!(set.spilled_runs(), 0);
        assert_eq!(set.len(), 50);
    }
}
