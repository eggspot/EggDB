use crate::aligned::AlignedBuffer;
use crate::error::{Error, ErrorKind, Result};
use crate::fs::{File, FileSystem, OpenMode, advance};
use crate::rng::SplitMix64;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// What a simulated crash does to the writes that were not yet synced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CrashMode {
    /// Every unsynced write and size change is lost.
    DropUnsynced,
    /// Each unsynced size change, and each
    /// [`SimFileSystem::TORN_WRITE_SECTOR_SIZE`]-aligned piece of each unsynced write, independently survives or is lost, chosen by a generator seeded with
    /// `seed`. Models torn and reordered writes.
    Torn {
        /// The seed that decides which pieces survive.
        seed: u64,
    },
}

/// An in-memory file system for deterministic tests (D9).
///
/// It keeps two states: what the program sees and what would survive a power loss. File contents
/// become durable through [`File::sync_data`] or [`File::sync_all`]; creating, renaming and
/// removing files and creating directories become durable through [`FileSystem::sync_dir`] on
/// the parent directory. [`SimFileSystem::crash`] throws away everything that is not durable.
///
/// Every operation through the file system or one of its files is numbered from zero, and any of
/// them can be made to fail with [`ErrorKind::InjectedFault`], either by number or at random
/// from a seed. A failed operation changes nothing. Clones share the same state.
#[derive(Debug, Clone, Default)]
pub struct SimFileSystem {
    shared: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct State {
    generation: u64,
    next_inode: u64,
    inodes: BTreeMap<u64, Inode>,
    names: BTreeMap<PathBuf, u64>,
    durable_names: BTreeMap<PathBuf, u64>,
    dirs: BTreeSet<PathBuf>,
    durable_dirs: BTreeSet<PathBuf>,
    operations: u64,
    failing_operations: BTreeSet<u64>,
    random_failures: Option<RandomFailures>,
    refuse_uncached: bool,
    unsynced_limit: Option<u64>,
}

#[derive(Debug)]
struct RandomFailures {
    rng: SplitMix64,
    per_million: u32,
}

#[derive(Debug, Default)]
struct Inode {
    contents: Vec<u8>,
    durable: Vec<u8>,
    unsynced: Vec<Change>,
    unsynced_bytes: u64,
    open_handles: usize,
    name_count: usize,
    durable_name_count: usize,
}

#[derive(Debug)]
enum Change {
    Write { offset: u64, bytes: Vec<u8> },
    Resize(u64),
}

impl SimFileSystem {
    /// The largest file the simulation holds; larger writes and sizes fail with
    /// [`ErrorKind::InvalidInput`] instead of exhausting memory.
    pub const MAX_FILE_SIZE: u64 = 1 << 30;

    /// The most memory one file's unsynced writes and size changes may hold for a later crash
    /// to tear, counting each change's bookkeeping as well as its bytes; a write or size change
    /// beyond it fails with [`ErrorKind::InvalidInput`] until the file is synced, so that
    /// overwriting the same bytes again and again, in pieces of any size, cannot exhaust memory
    /// either.
    pub const MAX_UNSYNCED_BYTES: u64 = 1 << 30;

    /// The unit in which [`CrashMode::Torn`] keeps or loses the parts of an unsynced write.
    pub const TORN_WRITE_SECTOR_SIZE: u64 = 512;

    /// Creates an empty file system.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many operations have been attempted so far, including failed ones.
    pub fn operation_count(&self) -> u64 {
        lock(&self.shared).operations
    }

    /// Makes the operation numbered `index` fail.
    pub fn fail_operation(&self, index: u64) {
        lock(&self.shared).failing_operations.insert(index);
    }

    /// Makes each later operation fail with probability `failures_per_million` in a million,
    /// drawn from a generator seeded with `seed`. A rate of zero stops random failures.
    pub fn fail_randomly(&self, seed: u64, failures_per_million: u32) {
        lock(&self.shared).random_failures = (failures_per_million > 0).then(|| RandomFailures {
            rng: SplitMix64::new(seed),
            per_million: failures_per_million,
        });
    }

    /// Makes [`FileSystem::open_uncached`] fail with [`ErrorKind::Unsupported`], as a file
    /// system that cannot bypass its cache does, or lets it succeed again.
    pub fn refuse_uncached(&self, refuse: bool) {
        lock(&self.shared).refuse_uncached = refuse;
    }

    /// Sets the most memory one file's unsynced writes and size changes may hold, capped at
    /// [`SimFileSystem::MAX_UNSYNCED_BYTES`], so a test reaches the limit cheaply.
    pub fn limit_unsynced_bytes(&self, bytes: u64) {
        lock(&self.shared).unsynced_limit = Some(bytes.min(Self::MAX_UNSYNCED_BYTES));
    }

    /// Simulates a power loss followed by a restart. Afterwards the file system holds only what
    /// was durable, plus whatever `mode` lets survive, and every handle opened before the crash
    /// fails with [`ErrorKind::StaleHandle`].
    pub fn crash(&self, mode: CrashMode) {
        lock(&self.shared).crash(mode);
    }
}

impl SimFileSystem {
    fn open_handle(&self, path: &Path, mode: OpenMode, uncached: bool) -> Result<SimFile> {
        let path = &normalized(path);
        let mut state = begin(&self.shared)?;
        if state.dirs.contains(path) || is_root(path) {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        let created = !state.names.contains_key(path);
        let inode = match (state.names.get(path).copied(), mode) {
            (Some(_), OpenMode::CreateNew) => return Err(Error::new(ErrorKind::AlreadyExists)),
            (Some(inode), _) => inode,
            (None, OpenMode::Read | OpenMode::ReadWrite) => {
                return Err(Error::new(ErrorKind::NotFound));
            }
            (None, OpenMode::Create | OpenMode::CreateNew) => {
                if !state.dir_exists(parent(path)) {
                    return Err(Error::new(ErrorKind::NotFound));
                }
                let inode = state.next_inode;
                state.next_inode += 1;
                state.inodes.insert(inode, Inode::default());
                state.link(path.to_path_buf(), inode);
                inode
            }
        };
        if uncached && state.refuse_uncached {
            if created && mode == OpenMode::CreateNew {
                state.unlink(path);
            }
            return Err(Error::new(ErrorKind::Unsupported));
        }
        if let Some(open) = state.inodes.get_mut(&inode) {
            open.open_handles += 1;
        }
        Ok(SimFile {
            shared: Arc::clone(&self.shared),
            inode,
            generation: state.generation,
            writable: mode.is_writable(),
            uncached,
        })
    }
}

impl FileSystem for SimFileSystem {
    type File = SimFile;

    fn open(&self, path: &Path, mode: OpenMode) -> Result<SimFile> {
        self.open_handle(path, mode, false)
    }

    fn open_uncached(&self, path: &Path, mode: OpenMode) -> Result<SimFile> {
        self.open_handle(path, mode, true)
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        let path = &normalized(path);
        let mut state = begin(&self.shared)?;
        if !state.names.contains_key(path) {
            return Err(Error::new(ErrorKind::NotFound));
        }
        state.unlink(path);
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let (from, to) = (&normalized(from), &normalized(to));
        let mut state = begin(&self.shared)?;
        if !state.names.contains_key(from) || !state.dir_exists(parent(to)) {
            return Err(Error::new(ErrorKind::NotFound));
        }
        if state.dirs.contains(to) || is_root(to) {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        if let Some(&inode) = state.names.get(from)
            && state.names.get(to) != Some(&inode)
        {
            state.link(to.to_path_buf(), inode);
            state.unlink(from);
        }
        Ok(())
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        let path = &normalized(path);
        let mut state = begin(&self.shared)?;
        let missing: Vec<&Path> = path
            .ancestors()
            .filter(|dir| !state.dir_exists(dir))
            .collect();
        if missing.iter().any(|dir| state.names.contains_key(*dir)) {
            return Err(Error::new(ErrorKind::AlreadyExists));
        }
        for dir in missing {
            state.dirs.insert(dir.to_path_buf());
        }
        Ok(())
    }

    fn sync_dir(&self, path: &Path) -> Result<()> {
        let path = &normalized(path);
        let mut state = begin(&self.shared)?;
        if !state.dir_exists(path) {
            return Err(Error::new(ErrorKind::NotFound));
        }
        state.sync_dir(path);
        Ok(())
    }

    fn exists(&self, path: &Path) -> Result<bool> {
        let path = &normalized(path);
        let state = begin(&self.shared)?;
        Ok(state.names.contains_key(path) || state.dir_exists(path))
    }
}

/// A file opened through [`SimFileSystem`].
#[derive(Debug)]
pub struct SimFile {
    shared: Arc<Mutex<State>>,
    inode: u64,
    generation: u64,
    writable: bool,
    uncached: bool,
}

impl SimFile {
    fn with_inode<T>(&self, operation: impl FnOnce(&mut Inode, u64) -> Result<T>) -> Result<T> {
        let mut state = begin(&self.shared)?;
        if state.generation != self.generation {
            return Err(Error::new(ErrorKind::StaleHandle));
        }
        let unsynced_limit = state
            .unsynced_limit
            .unwrap_or(SimFileSystem::MAX_UNSYNCED_BYTES);
        let inode = state
            .inodes
            .get_mut(&self.inode)
            .ok_or_else(|| Error::new(ErrorKind::StaleHandle))?;
        operation(inode, unsynced_limit)
    }

    fn require_alignment(&self, buf: &[u8], offset: u64) -> Result<()> {
        let alignment = AlignedBuffer::ALIGNMENT;
        let aligned = buf.is_empty()
            || (buf.as_ptr().addr().is_multiple_of(alignment)
                && buf.len().is_multiple_of(alignment)
                && u64::try_from(alignment)
                    .is_ok_and(|alignment| offset.is_multiple_of(alignment)));
        if self.uncached && !aligned {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        Ok(())
    }

    fn require_writable(&self) -> Result<()> {
        if self.writable {
            Ok(())
        } else {
            Err(Error::new(ErrorKind::PermissionDenied))
        }
    }
}

impl Drop for SimFile {
    fn drop(&mut self) {
        let mut state = lock(&self.shared);
        if state.generation != self.generation {
            return;
        }
        if let Some(inode) = state.inodes.get_mut(&self.inode) {
            inode.open_handles = inode.open_handles.saturating_sub(1);
        }
        state.free_if_unreachable(self.inode);
    }
}

impl File for SimFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.with_inode(|inode, _| {
            self.require_alignment(buf, offset)?;
            let Some(available) = usize::try_from(offset)
                .ok()
                .and_then(|start| inode.contents.get(start..))
            else {
                return Ok(0);
            };
            let read = available.len().min(buf.len());
            buf[..read].copy_from_slice(&available[..read]);
            Ok(read)
        })
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize> {
        self.with_inode(|inode, unsynced_limit| {
            self.require_writable()?;
            self.require_alignment(buf, offset)?;
            if buf.is_empty() {
                return Ok(0);
            }
            let start = checked_size(offset)?;
            checked_size(advance(offset, buf.len())?)?;
            inode.charge_unsynced(size_of::<Change>() + buf.len(), unsynced_limit)?;
            write_into(&mut inode.contents, start, buf);
            inode.unsynced.push(Change::Write {
                offset,
                bytes: buf.to_vec(),
            });
            Ok(buf.len())
        })
    }

    fn size(&self) -> Result<u64> {
        self.with_inode(|inode, _| advance(0, inode.contents.len()))
    }

    fn set_size(&self, size: u64) -> Result<()> {
        self.with_inode(|inode, unsynced_limit| {
            self.require_writable()?;
            let new_len = checked_size(size)?;
            inode.charge_unsynced(size_of::<Change>(), unsynced_limit)?;
            inode.contents.resize(new_len, 0);
            inode.unsynced.push(Change::Resize(size));
            Ok(())
        })
    }

    fn is_uncached(&self) -> bool {
        self.uncached
    }

    fn sync_data(&self) -> Result<()> {
        self.with_inode(|inode, _| {
            self.require_writable()?;
            inode.make_durable();
            Ok(())
        })
    }

    fn sync_all(&self) -> Result<()> {
        self.sync_data()
    }
}

impl Inode {
    fn make_durable(&mut self) {
        let replayed = advance(0, self.contents.len()).is_ok_and(|len| self.unsynced_bytes < len)
            && self
                .unsynced
                .iter()
                .all(|change| change.apply(&mut self.durable));
        if !replayed {
            self.durable.clone_from(&self.contents);
        }
        self.unsynced.clear();
        self.unsynced_bytes = 0;
    }

    fn charge_unsynced(&mut self, bytes: usize, limit: u64) -> Result<()> {
        let total = advance(self.unsynced_bytes, bytes)?;
        if total > limit {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        self.unsynced_bytes = total;
        Ok(())
    }

    fn is_unreachable(&self) -> bool {
        self.open_handles == 0 && self.name_count == 0 && self.durable_name_count == 0
    }

    fn crash(&mut self, rng: Option<&mut SplitMix64>) {
        let mut survived = std::mem::take(&mut self.durable);
        if let Some(rng) = rng {
            for change in &self.unsynced {
                apply_torn(&mut survived, change, rng);
            }
        }
        self.durable.clone_from(&survived);
        self.contents = survived;
        self.unsynced.clear();
        self.unsynced_bytes = 0;
        self.open_handles = 0;
    }
}

impl State {
    fn dir_exists(&self, path: &Path) -> bool {
        is_root(path) || self.dirs.contains(path)
    }

    fn link(&mut self, name: PathBuf, inode: u64) {
        if let Some(linked) = self.inodes.get_mut(&inode) {
            linked.name_count += 1;
        }
        if let Some(replaced) = self.names.insert(name, inode) {
            self.drop_name(replaced);
        }
    }

    fn unlink(&mut self, name: &Path) {
        if let Some(inode) = self.names.remove(name) {
            self.drop_name(inode);
        }
    }

    fn drop_name(&mut self, inode: u64) {
        if let Some(unlinked) = self.inodes.get_mut(&inode) {
            unlinked.name_count = unlinked.name_count.saturating_sub(1);
        }
        self.free_if_unreachable(inode);
    }

    fn sync_dir(&mut self, dir: &Path) {
        let mut forgotten = Vec::new();
        self.durable_names.retain(|name, inode| {
            let kept = parent(name) != dir;
            if !kept {
                forgotten.push(*inode);
            }
            kept
        });
        let synced: Vec<(PathBuf, u64)> = self
            .names
            .iter()
            .filter(|(name, _)| parent(name) == dir)
            .map(|(name, inode)| (name.clone(), *inode))
            .collect();
        for (name, inode) in synced {
            if let Some(durable) = self.inodes.get_mut(&inode) {
                durable.durable_name_count += 1;
            }
            self.durable_names.insert(name, inode);
        }
        for inode in forgotten {
            if let Some(durable) = self.inodes.get_mut(&inode) {
                durable.durable_name_count = durable.durable_name_count.saturating_sub(1);
            }
            self.free_if_unreachable(inode);
        }
        self.durable_dirs.retain(|child| parent(child) != dir);
        let synced_dirs = self
            .dirs
            .iter()
            .filter(|child| parent(child) == dir)
            .cloned();
        self.durable_dirs.extend(synced_dirs);
    }

    fn crash(&mut self, mode: CrashMode) {
        self.generation += 1;
        let mut rng = match mode {
            CrashMode::DropUnsynced => None,
            CrashMode::Torn { seed } => Some(SplitMix64::new(seed)),
        };
        for inode in self.inodes.values_mut() {
            inode.crash(rng.as_mut());
        }
        let mut surviving_dirs = BTreeSet::new();
        for dir in &self.durable_dirs {
            if is_root(parent(dir)) || surviving_dirs.contains(parent(dir)) {
                surviving_dirs.insert(dir.clone());
            }
        }
        self.durable_names
            .retain(|name, _| is_root(parent(name)) || surviving_dirs.contains(parent(name)));
        self.names.clone_from(&self.durable_names);
        self.dirs.clone_from(&surviving_dirs);
        self.durable_dirs = surviving_dirs;
        self.recount_names();
        self.inodes.retain(|_, inode| !inode.is_unreachable());
    }

    fn recount_names(&mut self) {
        for inode in self.inodes.values_mut() {
            inode.name_count = 0;
            inode.durable_name_count = 0;
        }
        for inode in self.names.values() {
            if let Some(named) = self.inodes.get_mut(inode) {
                named.name_count += 1;
            }
        }
        for inode in self.durable_names.values() {
            if let Some(named) = self.inodes.get_mut(inode) {
                named.durable_name_count += 1;
            }
        }
    }

    fn free_if_unreachable(&mut self, inode: u64) {
        if self.inodes.get(&inode).is_some_and(Inode::is_unreachable) {
            self.inodes.remove(&inode);
        }
    }

    #[cfg(test)]
    fn inode_count(&self) -> usize {
        self.inodes.len()
    }

    #[cfg(test)]
    fn snapshot(&self) -> String {
        let inodes: Vec<_> = self
            .inodes
            .iter()
            .map(|(id, inode)| {
                (
                    id,
                    &inode.contents,
                    &inode.durable,
                    inode.unsynced.len(),
                    inode.unsynced_bytes,
                    inode.open_handles,
                )
            })
            .collect();
        format!(
            "{:?} {:?} {:?} {:?} {inodes:?}",
            self.names, self.durable_names, self.dirs, self.durable_dirs
        )
    }

    #[cfg(test)]
    fn assert_names_counted(&self) {
        for (id, inode) in &self.inodes {
            let names = self.names.values().filter(|named| *named == id).count();
            let durable = self
                .durable_names
                .values()
                .filter(|named| *named == id)
                .count();
            assert_eq!(
                (inode.name_count, inode.durable_name_count),
                (names, durable),
                "inode {id}"
            );
            assert!(!inode.is_unreachable(), "inode {id} should have been freed");
        }
        for id in self.names.values().chain(self.durable_names.values()) {
            assert!(
                self.inodes.contains_key(id),
                "name points at freed inode {id}"
            );
        }
    }

    fn next_operation_fails(&mut self) -> bool {
        let index = self.operations;
        self.operations += 1;
        let chosen = self.failing_operations.remove(&index);
        let random = self
            .random_failures
            .as_mut()
            .is_some_and(|failures| failures.rng.one_in_million(failures.per_million));
        chosen || random
    }
}

impl Change {
    fn apply(&self, contents: &mut Vec<u8>) -> bool {
        match self {
            Change::Write { offset, bytes } => usize::try_from(*offset)
                .map(|start| write_into(contents, start, bytes))
                .is_ok(),
            Change::Resize(size) => usize::try_from(*size)
                .map(|size| contents.resize(size, 0))
                .is_ok(),
        }
    }
}

fn apply_torn(contents: &mut Vec<u8>, change: &Change, rng: &mut SplitMix64) {
    match change {
        Change::Resize(size) => {
            if rng.coin()
                && let Ok(size) = usize::try_from(*size)
            {
                contents.resize(size, 0);
            }
        }
        Change::Write { offset, bytes } => {
            for (piece_offset, piece) in sector_pieces(*offset, bytes) {
                if rng.coin()
                    && let Ok(start) = usize::try_from(piece_offset)
                {
                    write_into(contents, start, piece);
                }
            }
        }
    }
}

fn sector_pieces(mut offset: u64, mut bytes: &[u8]) -> Vec<(u64, &[u8])> {
    let mut pieces = Vec::new();
    while !bytes.is_empty() {
        let to_boundary =
            SimFileSystem::TORN_WRITE_SECTOR_SIZE - offset % SimFileSystem::TORN_WRITE_SECTOR_SIZE;
        let len = usize::try_from(to_boundary).map_or(bytes.len(), |len| len.min(bytes.len()));
        let (piece, rest) = bytes.split_at(len);
        pieces.push((offset, piece));
        offset = offset.saturating_add(to_boundary);
        bytes = rest;
    }
    pieces
}

fn write_into(contents: &mut Vec<u8>, start: usize, bytes: &[u8]) {
    let end = start + bytes.len();
    if contents.len() < end {
        contents.resize(end, 0);
    }
    contents[start..end].copy_from_slice(bytes);
}

fn checked_size(size: u64) -> Result<usize> {
    if size > SimFileSystem::MAX_FILE_SIZE {
        return Err(Error::new(ErrorKind::InvalidInput));
    }
    usize::try_from(size).map_err(|_| Error::new(ErrorKind::InvalidInput))
}

fn normalized(path: &Path) -> PathBuf {
    path.components()
        .filter(|component| *component != Component::CurDir)
        .collect()
}

fn parent(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new(""))
}

fn is_root(path: &Path) -> bool {
    path.parent().is_none()
}

fn lock(shared: &Mutex<State>) -> MutexGuard<'_, State> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

fn begin(shared: &Mutex<State>) -> Result<MutexGuard<'_, State>> {
    let mut state = lock(shared);
    if state.next_operation_fails() {
        return Err(Error::new(ErrorKind::InjectedFault));
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::{CrashMode, SimFile, SimFileSystem, lock, sector_pieces};
    use crate::rng::SplitMix64;
    use crate::{AlignedBuffer, ErrorKind, File, FileSystem, OpenMode, Result};
    use std::path::Path;

    fn read_all(file: &impl File) -> Vec<u8> {
        let size = usize::try_from(file.size().unwrap()).unwrap();
        let mut contents = vec![0; size];
        file.read_exact_at(&mut contents, 0).unwrap();
        contents
    }

    fn create_durable(fs: &SimFileSystem, path: &str, contents: &[u8]) {
        let file = fs.open(Path::new(path), OpenMode::CreateNew).unwrap();
        file.write_all_at(contents, 0).unwrap();
        file.sync_all().unwrap();
        fs.sync_dir(Path::new(path).parent().unwrap()).unwrap();
    }

    fn reopen(fs: &SimFileSystem, path: &str) -> Vec<u8> {
        read_all(&fs.open(Path::new(path), OpenMode::Read).unwrap())
    }

    #[test]
    fn synced_writes_survive_a_crash() {
        let fs = SimFileSystem::new();
        create_durable(&fs, "wal", b"committed");
        fs.crash(CrashMode::DropUnsynced);
        assert_eq!(reopen(&fs, "wal"), b"committed");
    }

    #[test]
    fn unsynced_writes_are_lost_in_a_crash() {
        let fs = SimFileSystem::new();
        create_durable(&fs, "wal", b"committed");
        let file = fs.open(Path::new("wal"), OpenMode::ReadWrite).unwrap();
        file.write_all_at(b"-pending", 9).unwrap();
        file.set_size(4).unwrap();
        fs.crash(CrashMode::DropUnsynced);
        assert_eq!(reopen(&fs, "wal"), b"committed");
    }

    #[test]
    fn a_new_file_needs_its_directory_synced_to_survive() {
        let fs = SimFileSystem::new();
        let file = fs.open(Path::new("new"), OpenMode::CreateNew).unwrap();
        file.write_all_at(b"data", 0).unwrap();
        file.sync_all().unwrap();
        fs.crash(CrashMode::DropUnsynced);
        assert!(!fs.exists(Path::new("new")).unwrap());
    }

    #[test]
    fn a_rename_needs_its_directory_synced_to_survive() {
        let fs = SimFileSystem::new();
        create_durable(&fs, "tmp", b"v2");
        fs.rename(Path::new("tmp"), Path::new("current")).unwrap();
        fs.crash(CrashMode::DropUnsynced);
        assert!(fs.exists(Path::new("tmp")).unwrap());
        assert!(!fs.exists(Path::new("current")).unwrap());

        fs.rename(Path::new("tmp"), Path::new("current")).unwrap();
        fs.sync_dir(Path::new("")).unwrap();
        fs.crash(CrashMode::DropUnsynced);
        assert!(!fs.exists(Path::new("tmp")).unwrap());
        assert_eq!(reopen(&fs, "current"), b"v2");
    }

    #[test]
    fn a_removal_needs_its_directory_synced_to_survive() {
        let fs = SimFileSystem::new();
        create_durable(&fs, "old", b"x");
        fs.remove_file(Path::new("old")).unwrap();
        fs.crash(CrashMode::DropUnsynced);
        assert_eq!(reopen(&fs, "old"), b"x");
    }

    #[test]
    fn files_in_a_directory_that_was_never_made_durable_vanish() {
        let fs = SimFileSystem::new();
        fs.create_dir_all(Path::new("data/db")).unwrap();
        create_durable(&fs, "data/db/pages", b"p");
        fs.crash(CrashMode::DropUnsynced);
        assert!(!fs.exists(Path::new("data")).unwrap());
        assert!(!fs.exists(Path::new("data/db/pages")).unwrap());
    }

    #[test]
    fn durable_directories_keep_their_durable_files() {
        let fs = SimFileSystem::new();
        fs.create_dir_all(Path::new("data/db")).unwrap();
        fs.sync_dir(Path::new("")).unwrap();
        fs.sync_dir(Path::new("data")).unwrap();
        create_durable(&fs, "data/db/pages", b"p");
        fs.crash(CrashMode::DropUnsynced);
        assert_eq!(reopen(&fs, "data/db/pages"), b"p");
    }

    #[test]
    fn handles_opened_before_a_crash_are_stale() {
        let fs = SimFileSystem::new();
        create_durable(&fs, "wal", b"x");
        let file = fs.open(Path::new("wal"), OpenMode::Read).unwrap();
        fs.crash(CrashMode::DropUnsynced);
        assert_eq!(file.size().unwrap_err().kind(), ErrorKind::StaleHandle);
    }

    #[test]
    fn torn_crashes_keep_some_sectors_and_are_reproducible() {
        let outcome = |seed| {
            let fs = SimFileSystem::new();
            create_durable(&fs, "pages", &[0; 4096]);
            let file = fs.open(Path::new("pages"), OpenMode::ReadWrite).unwrap();
            file.write_all_at(&[1; 4096], 0).unwrap();
            fs.crash(CrashMode::Torn { seed });
            reopen(&fs, "pages")
        };
        let torn = outcome(42);
        assert_eq!(torn, outcome(42));
        assert_eq!(torn.len(), 4096);
        let sector = usize::try_from(SimFileSystem::TORN_WRITE_SECTOR_SIZE).unwrap();
        for piece in torn.chunks(sector) {
            assert!(piece.iter().all(|&byte| byte == piece[0]));
        }
        assert!(torn.contains(&0) && torn.contains(&1));
    }

    #[test]
    fn sector_pieces_split_at_absolute_sector_boundaries() {
        let bytes = [7u8; 1100];
        let pieces: Vec<(u64, usize)> = sector_pieces(500, &bytes)
            .into_iter()
            .map(|(offset, piece)| (offset, piece.len()))
            .collect();
        assert_eq!(pieces, [(500, 12), (512, 512), (1024, 512), (1536, 64)]);
        assert!(sector_pieces(0, &[]).is_empty());
    }

    #[test]
    fn a_chosen_operation_fails_and_changes_nothing() {
        let fs = SimFileSystem::new();
        let file = fs.open(Path::new("f"), OpenMode::CreateNew).unwrap();
        fs.fail_operation(fs.operation_count());
        assert_eq!(
            file.write_at(b"lost", 0).unwrap_err().kind(),
            ErrorKind::InjectedFault
        );
        assert_eq!(file.size().unwrap(), 0);
        file.write_all_at(b"kept", 0).unwrap();
        assert_eq!(read_all(&file), b"kept");
    }

    #[test]
    fn a_failed_sync_makes_nothing_durable() {
        let fs = SimFileSystem::new();
        create_durable(&fs, "wal", b"old");
        let file = fs.open(Path::new("wal"), OpenMode::ReadWrite).unwrap();
        file.write_all_at(b"new", 0).unwrap();
        fs.fail_operation(fs.operation_count());
        assert_eq!(
            file.sync_data().unwrap_err().kind(),
            ErrorKind::InjectedFault
        );
        fs.crash(CrashMode::DropUnsynced);
        assert_eq!(reopen(&fs, "wal"), b"old");
    }

    #[test]
    fn writes_through_a_read_only_handle_are_numbered_operations() {
        let fs = SimFileSystem::new();
        create_durable(&fs, "f", b"x");
        let file = fs.open(Path::new("f"), OpenMode::Read).unwrap();
        let before = fs.operation_count();
        let error = file.write_at(b"y", 0).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::PermissionDenied);
        assert_eq!(fs.operation_count(), before + 1);
    }

    #[test]
    fn a_removed_file_is_freed_once_unnamed_and_closed() {
        let fs = SimFileSystem::new();
        create_durable(&fs, "segment", b"old log");
        let file = fs.open(Path::new("segment"), OpenMode::Read).unwrap();
        fs.remove_file(Path::new("segment")).unwrap();
        fs.sync_dir(Path::new("")).unwrap();
        assert_eq!(read_all(&file), b"old log");
        assert_eq!(super::lock(&fs.shared).inode_count(), 1);
        drop(file);
        assert_eq!(super::lock(&fs.shared).inode_count(), 0);
    }

    #[test]
    fn a_removed_file_stays_until_the_removal_is_durable() {
        let fs = SimFileSystem::new();
        create_durable(&fs, "segment", b"old log");
        fs.remove_file(Path::new("segment")).unwrap();
        assert_eq!(super::lock(&fs.shared).inode_count(), 1);
        fs.crash(CrashMode::DropUnsynced);
        assert_eq!(reopen(&fs, "segment"), b"old log");
    }

    #[test]
    fn uncached_handles_reject_unaligned_buffers_offsets_and_lengths() {
        let fs = SimFileSystem::new();
        let file = fs
            .open_uncached(Path::new("pages"), OpenMode::CreateNew)
            .unwrap();
        assert!(file.is_uncached());
        let page = AlignedBuffer::zeroed(AlignedBuffer::ALIGNMENT).unwrap();
        let unaligned_offset = file.write_at(&page, 512).unwrap_err();
        let short_length = file.write_at(&page[..512], 0).unwrap_err();
        let shifted_address = file.write_at(&page[1..], 0).unwrap_err();
        let mut small = [0; 16];
        let unaligned_read = file.read_at(&mut small, 0).unwrap_err();
        for error in [
            unaligned_offset,
            short_length,
            shifted_address,
            unaligned_read,
        ] {
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
        }
        file.write_all_at(&page, 4096).unwrap();
        assert_eq!(file.size().unwrap(), 8192);
    }

    #[test]
    fn cached_handles_accept_any_alignment() {
        let fs = SimFileSystem::new();
        let file = fs.open(Path::new("log"), OpenMode::CreateNew).unwrap();
        assert!(!file.is_uncached());
        file.write_all_at(b"abc", 3).unwrap();
    }

    #[test]
    fn a_refused_uncached_create_new_leaves_no_file_for_the_fallback() {
        let fs = SimFileSystem::new();
        fs.refuse_uncached(true);
        let error = fs
            .open_uncached(Path::new("pages"), OpenMode::CreateNew)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(!fs.exists(Path::new("pages")).unwrap());
        fs.open(Path::new("pages"), OpenMode::CreateNew).unwrap();
    }

    #[test]
    fn a_refused_uncached_create_may_leave_an_empty_file_like_the_os() {
        let fs = SimFileSystem::new();
        fs.refuse_uncached(true);
        let error = fs
            .open_uncached(Path::new("pages"), OpenMode::Create)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(fs.exists(Path::new("pages")).unwrap());
        let fallback = fs.open(Path::new("pages"), OpenMode::Create).unwrap();
        assert_eq!(fallback.size().unwrap(), 0);
        fs.refuse_uncached(false);
        assert!(
            fs.open_uncached(Path::new("pages"), OpenMode::Create)
                .is_ok()
        );
    }

    #[test]
    fn missing_paths_are_reported_before_a_refusal() {
        let fs = SimFileSystem::new();
        fs.refuse_uncached(true);
        let error = fs
            .open_uncached(Path::new("missing"), OpenMode::Read)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound);
    }

    #[test]
    fn random_failures_are_reproducible_from_the_seed() {
        let failures = |seed| {
            let fs = SimFileSystem::new();
            let file = fs.open(Path::new("f"), OpenMode::CreateNew).unwrap();
            fs.fail_randomly(seed, 250_000);
            let pattern: Vec<bool> = (0..200).map(|_| file.size().is_err()).collect();
            fs.fail_randomly(seed, 0);
            assert!(file.size().is_ok());
            pattern
        };
        let pattern = failures(9);
        assert_eq!(pattern, failures(9));
        let failed = pattern.iter().filter(|&&failed| failed).count();
        assert!((20..80).contains(&failed), "{failed} of 200 failed");
    }

    #[test]
    fn oversized_writes_are_rejected_instead_of_exhausting_memory() {
        let fs = SimFileSystem::new();
        let file = fs.open(Path::new("f"), OpenMode::CreateNew).unwrap();
        let error = file
            .write_at(b"x", SimFileSystem::MAX_FILE_SIZE)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        let error = file.set_size(u64::MAX).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        let error = file.write_at(b"x", u64::MAX).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn directories_cannot_be_opened_as_files_or_created_over_files() {
        let fs = SimFileSystem::new();
        fs.create_dir_all(Path::new("dir")).unwrap();
        let error = fs.open(Path::new("dir"), OpenMode::Create).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        fs.open(Path::new("file"), OpenMode::CreateNew).unwrap();
        let error = fs.create_dir_all(Path::new("file/sub")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::AlreadyExists);
    }

    #[test]
    fn unsynced_writes_stop_at_the_limit_until_the_file_is_synced() {
        let fs = SimFileSystem::new();
        fs.limit_unsynced_bytes(1100);
        let file = fs.open(Path::new("f"), OpenMode::CreateNew).unwrap();
        file.write_all_at(&[7; 512], 0).unwrap();
        file.write_all_at(&[7; 512], 0).unwrap();
        let error = file.write_at(&[8; 200], 0).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        let error = file.set_size(0).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert_eq!(read_all(&file), vec![7; 512]);
        file.sync_data().unwrap();
        file.write_all_at(&[8], 0).unwrap();
        file.set_size(1).unwrap();
        assert_eq!(read_all(&file), [8]);
    }

    #[test]
    fn tiny_unsynced_writes_count_their_bookkeeping_against_the_limit() {
        let fs = SimFileSystem::new();
        fs.limit_unsynced_bytes(64 * 1024);
        let file = fs.open(Path::new("f"), OpenMode::CreateNew).unwrap();
        let accepted = (0..10_000)
            .take_while(|_| file.write_at(&[1], 0).is_ok())
            .count();
        assert!((1..64 * 1024 / 32).contains(&accepted), "{accepted}");
    }

    #[test]
    fn renaming_onto_another_name_of_the_same_file_changes_nothing() {
        let fs = SimFileSystem::new();
        fs.create_dir_all(Path::new("a")).unwrap();
        fs.create_dir_all(Path::new("b")).unwrap();
        fs.sync_dir(Path::new("")).unwrap();
        create_durable(&fs, "a/x", b"one file");
        fs.rename(Path::new("a/x"), Path::new("b/x")).unwrap();
        fs.sync_dir(Path::new("b")).unwrap();
        fs.crash(CrashMode::DropUnsynced);
        fs.rename(Path::new("a/x"), Path::new("b/x")).unwrap();
        assert_eq!(reopen(&fs, "a/x"), b"one file");
        assert_eq!(reopen(&fs, "b/x"), b"one file");
        lock(&fs.shared).assert_names_counted();
    }

    #[test]
    fn files_cannot_be_renamed_onto_the_root() {
        let fs = SimFileSystem::new();
        fs.open(Path::new("f"), OpenMode::CreateNew).unwrap();
        for root in ["", "."] {
            let error = fs.rename(Path::new("f"), Path::new(root)).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
        }
        assert!(fs.exists(Path::new("f")).unwrap());
    }

    #[test]
    fn syncs_make_exactly_the_written_contents_durable() {
        let mut rng = SplitMix64::new(5);
        let fs = SimFileSystem::new();
        let file = fs.open(Path::new("f"), OpenMode::CreateNew).unwrap();
        let mut model: Vec<u8> = Vec::new();
        for step in 0..3000 {
            match rng.next_u64() % 8 {
                0 => {
                    file.sync_data().unwrap();
                    let state = lock(&fs.shared);
                    assert_eq!(state.inodes[&file.inode].durable, model, "step {step}");
                }
                1 => {
                    let size = rng.next_u64() % 6000;
                    file.set_size(size).unwrap();
                    model.resize(usize::try_from(size).unwrap(), 0);
                }
                _ => {
                    let offset = usize::try_from(rng.next_u64() % 5000).unwrap();
                    let len = usize::try_from(rng.next_u64() % 300 + 1).unwrap();
                    let bytes = vec![rng.next_u64().to_le_bytes()[0]; len];
                    file.write_all_at(&bytes, u64::try_from(offset).unwrap())
                        .unwrap();
                    if model.len() < offset + len {
                        model.resize(offset + len, 0);
                    }
                    model[offset..offset + len].copy_from_slice(&bytes);
                }
            }
        }
    }

    #[test]
    fn every_failed_operation_changes_nothing() {
        type Step = fn(&SimFileSystem, &mut Vec<SimFile>) -> Result<()>;
        let steps: [Step; 16] = [
            |fs, _| fs.create_dir_all(Path::new("d/e")),
            |fs, _| fs.sync_dir(Path::new("")),
            |fs, files| {
                fs.open(Path::new("d/a"), OpenMode::CreateNew)
                    .map(|file| files.push(file))
            },
            |_, files| with_file(files, 0, |file| file.write_all_at(b"hello", 0)),
            |_, files| with_file(files, 0, File::sync_data),
            |fs, _| fs.sync_dir(Path::new("d")),
            |fs, _| fs.rename(Path::new("d/a"), Path::new("d/e/b")),
            |fs, files| {
                fs.open(Path::new("d/c"), OpenMode::Create)
                    .map(|file| files.push(file))
            },
            |_, files| with_file(files, 1, |file| file.set_size(10)),
            |fs, _| fs.rename(Path::new("d/c"), Path::new("d/e/b")),
            |fs, _| fs.sync_dir(Path::new("d/e")),
            |fs, _| fs.remove_file(Path::new("d/e/b")),
            |_, files| {
                files.clear();
                Ok(())
            },
            |fs, _| fs.sync_dir(Path::new("d")),
            |fs, _| fs.exists(Path::new("d/a")).map(|_| ()),
            |fs, files| {
                fs.open_uncached(Path::new("d/u"), OpenMode::CreateNew)
                    .map(|file| files.push(file))
            },
        ];
        for failing in 0..steps.len() {
            let fs = SimFileSystem::new();
            let mut files = Vec::new();
            for (index, step) in steps.iter().enumerate() {
                let operations = fs.operation_count();
                if index == failing {
                    fs.fail_operation(operations);
                }
                let before = lock(&fs.shared).snapshot();
                let result = step(&fs, &mut files);
                if index == failing && fs.operation_count() > operations {
                    assert_eq!(result.unwrap_err().kind(), ErrorKind::InjectedFault);
                    assert_eq!(lock(&fs.shared).snapshot(), before, "step {index}");
                }
                lock(&fs.shared).assert_names_counted();
            }
            fs.crash(CrashMode::Torn { seed: 3 });
            lock(&fs.shared).assert_names_counted();
        }
    }

    fn with_file(
        files: &[SimFile],
        index: usize,
        operation: impl FnOnce(&SimFile) -> Result<()>,
    ) -> Result<()> {
        files
            .get(index)
            .map_or(Err(crate::Error::new(ErrorKind::NotFound)), operation)
    }

    #[test]
    fn a_rename_across_directories_needs_both_directories_synced() {
        let setup = || {
            let fs = SimFileSystem::new();
            fs.create_dir_all(Path::new("a")).unwrap();
            fs.create_dir_all(Path::new("b")).unwrap();
            fs.sync_dir(Path::new("")).unwrap();
            create_durable(&fs, "a/x", b"moved");
            fs.rename(Path::new("a/x"), Path::new("b/x")).unwrap();
            fs
        };
        let fs = setup();
        fs.sync_dir(Path::new("b")).unwrap();
        fs.crash(CrashMode::DropUnsynced);
        assert_eq!(reopen(&fs, "a/x"), b"moved");
        assert_eq!(reopen(&fs, "b/x"), b"moved");
        let through_a = fs.open(Path::new("a/x"), OpenMode::ReadWrite).unwrap();
        through_a.write_all_at(b"M", 0).unwrap();
        assert_eq!(reopen(&fs, "b/x"), b"Moved");
        let fs = setup();
        fs.sync_dir(Path::new("a")).unwrap();
        fs.crash(CrashMode::DropUnsynced);
        assert!(!fs.exists(Path::new("a/x")).unwrap());
        assert!(!fs.exists(Path::new("b/x")).unwrap());
    }

    #[test]
    fn torn_crashes_keep_or_lose_each_size_change_and_sector() {
        let mut sizes = std::collections::BTreeSet::new();
        for seed in 0..64 {
            let fs = SimFileSystem::new();
            create_durable(&fs, "f", b"12345678");
            let file = fs.open(Path::new("f"), OpenMode::ReadWrite).unwrap();
            file.write_all_at(&[9; 1024], 8).unwrap();
            file.set_size(4).unwrap();
            fs.crash(CrashMode::Torn { seed });
            let contents = reopen(&fs, "f");
            assert!(contents.starts_with(b"1234"), "seed {seed}");
            if contents.len() > 4 {
                assert!(contents.starts_with(b"12345678"), "seed {seed}");
            }
            sizes.insert(contents.len());
        }
        assert!(
            sizes.is_subset(&[4, 8, 512, 1024, 1032].into()),
            "{sizes:?}"
        );
        assert!(sizes.contains(&4) && sizes.contains(&1032), "{sizes:?}");
    }
}
