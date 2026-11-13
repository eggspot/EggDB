use crate::{CommitStamp, Error, ErrorKind, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

const MAGIC: &[u8; 8] = b"EGGCHG01";
const SEGMENT_BYTES: u64 = 64 << 20;
const PENDING: u8 = 1;
const COMMITTED: u8 = 2;
const ABORTED: u8 = 3;
const BASE: u8 = 4;
const GAP: u8 = 5;
const MOST_RECORD_BYTES: u64 = 1 << 34;
const SEGMENT_PREFIX: &str = "changes-";
const SEGMENT_SUFFIX: &str = ".eggcl";

/// One key's change in an archived commit: its new stored bytes, or `None` once deleted.
pub type ArchivedChange = (Vec<u8>, Option<Vec<u8>>);

/// What the change archive holds, in order: the stamp the archive started from, each commit's
/// changes with its stamp, and the stamps where commits were made without archiving them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchivedEvent {
    /// The archive starts after the commit with this stamp.
    Base(CommitStamp),
    /// One commit: its stamp and every key it changed.
    Commit(CommitStamp, Vec<ArchivedChange>),
    /// Commits up to this stamp were made while the archive was not kept, so no replay may
    /// cross it.
    Gap(CommitStamp),
}

struct Segment {
    file: File,
    bytes: u64,
    number: u64,
}

/// The continuous change archive of a database (D1019): every commit's changed keys,
/// appended before the commit and confirmed after it, in segment files of a directory.
pub(crate) struct ChangeArchive {
    pub(crate) directory: PathBuf,
    segment: Mutex<Option<Segment>>,
    failed: AtomicBool,
}

impl std::fmt::Debug for ChangeArchive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangeArchive").field("directory", &self.directory).finish_non_exhaustive()
    }
}

fn archive_error(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Io, message)
}

fn segment_name(number: u64) -> String {
    format!("{SEGMENT_PREFIX}{number:016x}{SEGMENT_SUFFIX}")
}

fn segment_numbers(directory: &Path) -> Result<Vec<u64>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(archive_error(format!("cannot read the change archive {}: {error}", directory.display()))),
    };
    let mut numbers: Vec<u64> = entries
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let hex = name.strip_prefix(SEGMENT_PREFIX)?.strip_suffix(SEGMENT_SUFFIX)?;
            u64::from_str_radix(hex, 16).ok()
        })
        .collect();
    numbers.sort_unstable();
    Ok(numbers)
}

fn record(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut checked = Vec::with_capacity(payload.len() + 1);
    checked.push(kind);
    checked.extend_from_slice(payload);
    let mut out = Vec::with_capacity(checked.len() + 16);
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&eggdb_format::crc32c(&checked).to_le_bytes());
    out.extend_from_slice(&checked);
    out
}

fn changes_payload(changes: &[ArchivedChange]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(changes.len() as u64).to_le_bytes());
    for (key, value) in changes {
        out.extend_from_slice(&(key.len() as u64).to_le_bytes());
        out.extend_from_slice(key);
        match value {
            Some(value) => {
                out.push(1);
                out.extend_from_slice(&(value.len() as u64).to_le_bytes());
                out.extend_from_slice(value);
            }
            None => out.push(0),
        }
    }
    out
}

struct Cursor<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl Cursor<'_> {
    fn take(&mut self, count: u64) -> Option<&[u8]> {
        let count = usize::try_from(count).ok()?;
        let end = self.at.checked_add(count)?;
        let taken = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(taken)
    }

    fn number(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}

fn parse_changes(payload: &[u8]) -> Option<Vec<ArchivedChange>> {
    let mut cursor = Cursor { bytes: payload, at: 0 };
    let count = cursor.number()?;
    let mut changes = Vec::with_capacity(usize::try_from(count.min(1 << 16)).ok()?);
    for _ in 0..count {
        let key_length = cursor.number()?;
        let key = cursor.take(key_length)?.to_vec();
        let value = match cursor.take(1)?[0] {
            1 => {
                let length = cursor.number()?;
                Some(cursor.take(length)?.to_vec())
            }
            0 => None,
            _ => return None,
        };
        changes.push((key, value));
    }
    (cursor.at == payload.len()).then_some(changes)
}

/// Every record of one segment file, in order, stopping at the first that is cut short or
/// fails its checksum (a torn tail: the writer stopped while appending it).
fn records(path: &Path) -> Result<Vec<(u8, Vec<u8>)>> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|mut file| file.read_to_end(&mut bytes))
        .map_err(|error| archive_error(format!("cannot read {}: {error}", path.display())))?;
    if bytes.get(..MAGIC.len()) != Some(&MAGIC[..]) {
        return Err(Error::new(ErrorKind::Corrupt, format!("{} is not a change archive segment", path.display())));
    }
    let mut cursor = Cursor { bytes: &bytes, at: MAGIC.len() };
    let mut out = Vec::new();
    while cursor.at < bytes.len() {
        let Some(length) = cursor.number().filter(|length| *length <= MOST_RECORD_BYTES) else {
            break;
        };
        let Some(checksum) = cursor.take(4).and_then(|found| found.try_into().ok()).map(u32::from_le_bytes) else {
            break;
        };
        let Some(checked) = cursor.take(length + 1) else {
            break;
        };
        if eggdb_format::crc32c(checked) != checksum {
            break;
        }
        out.push((checked[0], checked[1..].to_vec()));
    }
    Ok(out)
}

/// Reads a change archive directory into its events, in commit order. A commit whose changes
/// were written but never confirmed (the process stopped between them) is left out.
pub fn read_change_archive(directory: &Path) -> Result<Vec<ArchivedEvent>> {
    let mut files = Vec::new();
    for number in segment_numbers(directory)? {
        files.push(directory.join(segment_name(number)));
    }
    events_of(&files, directory)
}

/// Reads one file in the change archive's format, such as a differential backup, into its
/// events.
pub fn read_change_file(path: &Path) -> Result<Vec<ArchivedEvent>> {
    events_of(&[path.to_path_buf()], path)
}

fn events_of(files: &[PathBuf], shown: &Path) -> Result<Vec<ArchivedEvent>> {
    let directory = shown;
    let mut events = Vec::new();
    let mut pending: Option<Vec<ArchivedChange>> = None;
    for file in files {
        for (kind, payload) in records(file)? {
            let stamp = || payload.get(..8).and_then(|bytes| bytes.try_into().ok()).map(u64::from_le_bytes).map(CommitStamp::from_bits);
            match kind {
                PENDING => {
                    pending = Some(parse_changes(&payload).ok_or_else(|| {
                        Error::new(ErrorKind::Corrupt, format!("a commit in the change archive {} is damaged", directory.display()))
                    })?);
                }
                COMMITTED => {
                    if let (Some(changes), Some(stamp)) = (pending.take(), stamp()) {
                        events.push(ArchivedEvent::Commit(stamp, changes));
                    }
                }
                ABORTED => pending = None,
                BASE => events.extend(stamp().map(ArchivedEvent::Base)),
                GAP => events.extend(stamp().map(ArchivedEvent::Gap)),
                _ => {}
            }
        }
    }
    Ok(events)
}

fn last_known(events: &[ArchivedEvent]) -> Option<CommitStamp> {
    events.iter().rev().map(|event| match event {
        ArchivedEvent::Base(stamp) | ArchivedEvent::Gap(stamp) | ArchivedEvent::Commit(stamp, _) => *stamp,
    }).next()
}

fn unconfirmed(directory: &Path) -> Result<bool> {
    let Some(last) = segment_numbers(directory)?.last().copied() else {
        return Ok(false);
    };
    let mut open = false;
    for (kind, _) in records(&directory.join(segment_name(last)))? {
        match kind {
            PENDING => open = true,
            COMMITTED | ABORTED => open = false,
            _ => {}
        }
    }
    Ok(open)
}

/// Writes a differential backup in the change archive's format: the stamp it starts from,
/// then its changes in records of at most 64 MiB, each confirmed with the stamp it reaches.
pub(crate) struct DeltaWriter {
    file: File,
    stamp: CommitStamp,
    buffered: Vec<ArchivedChange>,
    buffered_bytes: usize,
    written: u64,
}

const DELTA_RECORD_BYTES: usize = 64 << 20;

impl DeltaWriter {
    pub(crate) fn create(path: &Path, base: CommitStamp, stamp: CommitStamp) -> Result<Self> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .map_err(|error| archive_error(format!("cannot create {}: {error}", path.display())))?;
        file.write_all(MAGIC)
            .and_then(|()| file.write_all(&record(BASE, &base.bits().to_le_bytes())))
            .map_err(|error| archive_error(format!("cannot write {}: {error}", path.display())))?;
        Ok(Self { file, stamp, buffered: Vec::new(), buffered_bytes: 0, written: 0 })
    }

    pub(crate) fn push(&mut self, key: Vec<u8>, value: Option<Vec<u8>>) -> Result<()> {
        self.buffered_bytes += key.len() + value.as_ref().map_or(0, Vec::len) + 17;
        self.buffered.push((key, value));
        if self.buffered_bytes >= DELTA_RECORD_BYTES {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.buffered.is_empty() {
            return Ok(());
        }
        let changes = std::mem::take(&mut self.buffered);
        self.written += changes.len() as u64;
        self.buffered_bytes = 0;
        self.file
            .write_all(&record(PENDING, &changes_payload(&changes)))
            .and_then(|()| self.file.write_all(&record(COMMITTED, &self.stamp.bits().to_le_bytes())))
            .map_err(|error| archive_error(format!("cannot write a differential backup: {error}")))
    }

    /// Writes what is left, syncs, and returns how many keys the delta changes.
    pub(crate) fn finish(mut self) -> Result<u64> {
        self.flush()?;
        if self.written == 0 {
            self.file
                .write_all(&record(PENDING, &changes_payload(&[])))
                .and_then(|()| self.file.write_all(&record(COMMITTED, &self.stamp.bits().to_le_bytes())))
                .map_err(|error| archive_error(format!("cannot write a differential backup: {error}")))?;
        }
        self.file.sync_all().map_err(|error| archive_error(format!("cannot sync a differential backup: {error}")))?;
        Ok(self.written)
    }
}

impl ChangeArchive {
    /// Opens the archive in `directory` for a database whose last commit is `last_commit`,
    /// settling what a stop left: a commit written but not confirmed is confirmed when the
    /// database holds it and dropped when it does not, and commits the archive missed are
    /// marked as a gap.
    pub(crate) fn open(directory: &Path, last_commit: CommitStamp) -> Result<Self> {
        std::fs::create_dir_all(directory)
            .map_err(|error| archive_error(format!("cannot create the change archive {}: {error}", directory.display())))?;
        let archive = Self { directory: directory.to_path_buf(), segment: Mutex::new(None), failed: AtomicBool::new(false) };
        let events = read_change_archive(directory)?;
        let known = last_known(&events);
        if unconfirmed(directory)? {
            if known.is_some_and(|known| last_commit.bits() > known.bits()) {
                archive.append(COMMITTED, &last_commit.bits().to_le_bytes())?;
            } else {
                archive.append(ABORTED, &[])?;
            }
            return Ok(archive);
        }
        match known {
            None => archive.append(BASE, &last_commit.bits().to_le_bytes())?,
            Some(known) if last_commit.bits() > known.bits() => archive.append(GAP, &last_commit.bits().to_le_bytes())?,
            Some(_) => {}
        }
        Ok(archive)
    }

    fn append(&self, kind: u8, payload: &[u8]) -> Result<()> {
        let bytes = record(kind, payload);
        let mut guard = self.segment.lock().map_err(|_| archive_error("the change archive failed"))?;
        let rotate = guard.as_ref().is_none_or(|segment| segment.bytes + bytes.len() as u64 > SEGMENT_BYTES && segment.bytes > MAGIC.len() as u64);
        if rotate {
            let next = match guard.as_ref() {
                Some(segment) => segment.number + 1,
                None => segment_numbers(&self.directory)?.last().map_or(0, |last| last + 1),
            };
            let path = self.directory.join(segment_name(next));
            let mut file = OpenOptions::new()
                .create_new(true)
                .append(true)
                .open(&path)
                .map_err(|error| archive_error(format!("cannot create {}: {error}", path.display())))?;
            file.write_all(MAGIC).and_then(|()| file.sync_all()).map_err(|error| archive_error(format!("cannot write {}: {error}", path.display())))?;
            *guard = Some(Segment { file, bytes: MAGIC.len() as u64, number: next });
        }
        let Some(segment) = guard.as_mut() else {
            return Err(archive_error("the change archive has no segment"));
        };
        let written = segment.file.write_all(&bytes).and_then(|()| segment.file.sync_data());
        if let Err(error) = written {
            self.failed.store(true, Ordering::Release);
            return Err(archive_error(format!("cannot append to the change archive {}: {error}", self.directory.display())));
        }
        segment.bytes += bytes.len() as u64;
        Ok(())
    }

    /// Writes a commit's changes before the commit. After a failed append the archive refuses
    /// every commit until the database is opened again, so it never misses one silently.
    pub(crate) fn pending(&self, changes: &[ArchivedChange]) -> Result<()> {
        if self.failed.load(Ordering::Acquire) {
            return Err(archive_error("the change archive failed earlier; open the database again"));
        }
        self.append(PENDING, &changes_payload(changes))
    }

    /// Confirms the commit whose changes [`ChangeArchive::pending`] wrote.
    pub(crate) fn committed(&self, stamp: CommitStamp) -> Result<()> {
        self.append(COMMITTED, &stamp.bits().to_le_bytes())
    }

    /// Drops the changes [`ChangeArchive::pending`] wrote, for a commit that failed.
    pub(crate) fn aborted(&self) -> Result<()> {
        self.append(ABORTED, &[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commits_read_back_and_an_unconfirmed_one_is_settled_on_open() {
        let directory = std::env::temp_dir().join(format!("eggdb-change-archive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let archive = ChangeArchive::open(&directory, CommitStamp::from_bits(10)).unwrap();
        archive.pending(&[(b"a".to_vec(), Some(b"1".to_vec())), (b"b".to_vec(), None)]).unwrap();
        archive.committed(CommitStamp::from_bits(11)).unwrap();
        archive.pending(&[(b"c".to_vec(), Some(b"3".to_vec()))]).unwrap();
        drop(archive);
        let reopened = ChangeArchive::open(&directory, CommitStamp::from_bits(12)).unwrap();
        drop(reopened);
        let events = read_change_archive(&directory).unwrap();
        assert_eq!(
            events,
            vec![
                ArchivedEvent::Base(CommitStamp::from_bits(10)),
                ArchivedEvent::Commit(CommitStamp::from_bits(11), vec![(b"a".to_vec(), Some(b"1".to_vec())), (b"b".to_vec(), None)]),
                ArchivedEvent::Commit(CommitStamp::from_bits(12), vec![(b"c".to_vec(), Some(b"3".to_vec()))]),
            ]
        );
        let _ = std::fs::remove_dir_all(&directory);
    }
}
