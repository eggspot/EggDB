use crate::error::{Error, ErrorKind, Result};
use crate::fs::{File, FileSystem, OpenMode, advance};
use crate::rng::SplitMix64;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
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

    /// Simulates a power loss followed by a restart. Afterwards the file system holds only what
    /// was durable, plus whatever `mode` lets survive, and every handle opened before the crash
    /// fails with [`ErrorKind::StaleHandle`].
    pub fn crash(&self, mode: CrashMode) {
        lock(&self.shared).crash(mode);
    }
}

impl FileSystem for SimFileSystem {
    type File = SimFile;

    fn open(&self, path: &Path, mode: OpenMode) -> Result<SimFile> {
        let mut state = begin(&self.shared)?;
        if state.dirs.contains(path) || is_root(path) {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
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
                state.names.insert(path.to_path_buf(), inode);
                inode
            }
        };
        Ok(SimFile {
            shared: Arc::clone(&self.shared),
            inode,
            generation: state.generation,
            writable: mode.is_writable(),
        })
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        let mut state = begin(&self.shared)?;
        state
            .names
            .remove(path)
            .map(|_| ())
            .ok_or_else(|| Error::new(ErrorKind::NotFound))
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let mut state = begin(&self.shared)?;
        if !state.names.contains_key(from) || !state.dir_exists(parent(to)) {
            return Err(Error::new(ErrorKind::NotFound));
        }
        if state.dirs.contains(to) {
            return Err(Error::new(ErrorKind::InvalidInput));
        }
        if let Some(inode) = state.names.remove(from) {
            state.names.insert(to.to_path_buf(), inode);
        }
        Ok(())
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
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
        let mut state = begin(&self.shared)?;
        if !state.dir_exists(path) {
            return Err(Error::new(ErrorKind::NotFound));
        }
        state.sync_dir(path);
        Ok(())
    }

    fn exists(&self, path: &Path) -> Result<bool> {
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
}

impl SimFile {
    fn with_inode<T>(&self, operation: impl FnOnce(&mut Inode) -> Result<T>) -> Result<T> {
        let mut state = begin(&self.shared)?;
        if state.generation != self.generation {
            return Err(Error::new(ErrorKind::StaleHandle));
        }
        let inode = state
            .inodes
            .get_mut(&self.inode)
            .ok_or_else(|| Error::new(ErrorKind::StaleHandle))?;
        operation(inode)
    }

    fn require_writable(&self) -> Result<()> {
        if self.writable {
            Ok(())
        } else {
            Err(Error::new(ErrorKind::PermissionDenied))
        }
    }
}

impl File for SimFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.with_inode(|inode| {
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
        self.with_inode(|inode| {
            self.require_writable()?;
            let start = checked_size(offset)?;
            checked_size(advance(offset, buf.len())?)?;
            write_into(&mut inode.contents, start, buf);
            inode.unsynced.push(Change::Write {
                offset,
                bytes: buf.to_vec(),
            });
            Ok(buf.len())
        })
    }

    fn size(&self) -> Result<u64> {
        self.with_inode(|inode| advance(0, inode.contents.len()))
    }

    fn set_size(&self, size: u64) -> Result<()> {
        self.with_inode(|inode| {
            self.require_writable()?;
            let new_len = checked_size(size)?;
            inode.contents.resize(new_len, 0);
            inode.unsynced.push(Change::Resize(size));
            Ok(())
        })
    }

    fn sync_data(&self) -> Result<()> {
        self.with_inode(|inode| {
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
        self.durable.clone_from(&self.contents);
        self.unsynced.clear();
    }

    fn crash(&mut self, rng: Option<&mut SplitMix64>) {
        let mut survived = std::mem::take(&mut self.durable);
        if let Some(rng) = rng {
            for change in &self.unsynced {
                apply_torn(&mut survived, change, rng);
            }
        }
        self.contents = survived;
        self.make_durable();
    }
}

impl State {
    fn dir_exists(&self, path: &Path) -> bool {
        is_root(path) || self.dirs.contains(path)
    }

    fn sync_dir(&mut self, dir: &Path) {
        self.durable_names.retain(|name, _| parent(name) != dir);
        let synced_names = self
            .names
            .iter()
            .filter(|(name, _)| parent(name) == dir)
            .map(|(name, inode)| (name.clone(), *inode));
        self.durable_names.extend(synced_names);
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
        let referenced: BTreeSet<u64> = self.names.values().copied().collect();
        self.inodes.retain(|inode, _| referenced.contains(inode));
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
    use super::{CrashMode, SimFileSystem, sector_pieces};
    use crate::{ErrorKind, File, FileSystem, OpenMode};
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
}
