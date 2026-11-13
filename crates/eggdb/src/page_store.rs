//! A database file whose pages live in object storage (D18, D226, D1099): the file is cut into
//! 1 MiB chunks, each kept as an object under `chunks/` and named by its index and version, and
//! a `manifest` object lists the version of every chunk and the file's size. A local cache file
//! holds the chunks read so far, fetched the first time a page in them is read, so a node starts
//! without downloading the database. Each sync uploads the chunks changed since the last one and
//! then a new manifest, which makes them the file's state at once; because the write-ahead log
//! lives inside the file, every durable commit ships the log chunks it filled, segment by
//! segment. Old chunk versions are removed once the manifest no longer names them.

use eggdb_platform::{Error, ErrorKind, File, FileLock, FileSystem, OpenMode, OsFileSystem, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The bytes of one chunk.
pub const CHUNK_BYTES: u64 = 1 << 20;

const MANIFEST: &str = "manifest";
const MANIFEST_HEADER: &str = "eggdb-pages 1";

/// How long a chunk version no manifest names any more is kept for followers still reading an
/// older manifest (D1101): 15 minutes.
pub const RETIRED_KEPT_MS: u64 = 15 * 60 * 1000;

/// Where a page-store file keeps its objects: an S3 bucket's prefix, or anything that stores
/// whole objects by name.
pub trait ObjectStore: Send + Sync {
    /// The object called `name`, or `None` when there is none.
    fn get(&self, name: &str) -> std::result::Result<Option<Vec<u8>>, String>;
    /// Stores `bytes` as the object `name`, replacing one already there.
    fn put(&self, name: &str, bytes: &[u8]) -> std::result::Result<(), String>;
    /// Removes the object `name`; removing one that is not there is not an error.
    fn delete(&self, name: &str) -> std::result::Result<(), String>;
}

/// Objects held in memory, for tests and for trying the format out.
#[derive(Debug, Default)]
pub struct MemoryObjects {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemoryObjects {
    /// The names of the objects held, in order.
    pub fn names(&self) -> Vec<String> {
        self.objects
            .lock()
            .map(|objects| objects.keys().cloned().collect())
            .unwrap_or_default()
    }
}

impl ObjectStore for MemoryObjects {
    fn get(&self, name: &str) -> std::result::Result<Option<Vec<u8>>, String> {
        let objects = self.objects.lock().map_err(|_| "the objects' lock is poisoned".to_owned())?;
        Ok(objects.get(name).cloned())
    }

    fn put(&self, name: &str, bytes: &[u8]) -> std::result::Result<(), String> {
        let mut objects = self.objects.lock().map_err(|_| "the objects' lock is poisoned".to_owned())?;
        objects.insert(name.to_owned(), bytes.to_vec());
        Ok(())
    }

    fn delete(&self, name: &str) -> std::result::Result<(), String> {
        let mut objects = self.objects.lock().map_err(|_| "the objects' lock is poisoned".to_owned())?;
        objects.remove(name);
        Ok(())
    }
}

impl<T: ObjectStore + ?Sized> ObjectStore for std::sync::Arc<T> {
    fn get(&self, name: &str) -> std::result::Result<Option<Vec<u8>>, String> {
        (**self).get(name)
    }

    fn put(&self, name: &str, bytes: &[u8]) -> std::result::Result<(), String> {
        (**self).put(name, bytes)
    }

    fn delete(&self, name: &str) -> std::result::Result<(), String> {
        (**self).delete(name)
    }
}

/// What the manifest records: the file's size, its generation, and each chunk's version.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    /// The file's size in bytes.
    pub size: u64,
    /// One more for every sync that changed something.
    pub generation: u64,
    /// Each stored chunk's index and the version it was last written at.
    pub chunks: BTreeMap<u64, u64>,
    /// Chunk objects no longer named, with when they were retired, in milliseconds since 1970:
    /// kept for [`RETIRED_KEPT_MS`], then removed.
    pub retired: Vec<(String, u64)>,
}

impl Manifest {
    /// The manifest as text: a header, `size`, `generation`, then one `chunk <index> <version>`
    /// line per chunk.
    pub fn encode(&self) -> String {
        let mut out = format!("{MANIFEST_HEADER}\nsize {}\ngeneration {}\n", self.size, self.generation);
        for (index, version) in &self.chunks {
            out.push_str(&format!("chunk {index} {version}\n"));
        }
        for (name, at) in &self.retired {
            out.push_str(&format!("retired {name} {at}\n"));
        }
        out
    }

    /// Reads what [`Manifest::encode`] wrote, or `None` when it is not a manifest.
    pub fn decode(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()? != MANIFEST_HEADER {
            return None;
        }
        let mut manifest = Manifest::default();
        for line in lines {
            let mut words = line.split(' ');
            match (words.next()?, words.next(), words.next(), words.next()) {
                ("size", Some(size), None, None) => manifest.size = size.parse().ok()?,
                ("generation", Some(generation), None, None) => manifest.generation = generation.parse().ok()?,
                ("chunk", Some(index), Some(version), None) => {
                    manifest.chunks.insert(index.parse().ok()?, version.parse().ok()?);
                }
                ("retired", Some(name), Some(at), None) if name.starts_with("chunks/") => {
                    manifest.retired.push((name.to_owned(), at.parse().ok()?));
                }
                _ => return None,
            }
        }
        Some(manifest)
    }
}

fn chunk_name(index: u64, version: u64) -> String {
    format!("chunks/{index:016x}-{version:016x}")
}

fn failed(message: impl std::fmt::Display) -> Error {
    Error::from(std::io::Error::other(format!("object storage: {message}")))
}

#[derive(Debug, Default)]
struct State {
    manifest: Manifest,
    size: u64,
    present: BTreeMap<u64, u64>,
    dirty: BTreeSet<u64>,
    files: BTreeMap<u64, eggdb_platform::OsFile>,
    last_used: BTreeMap<u64, u64>,
    tick: u64,
}

/// A database file over an [`ObjectStore`], with a local cache directory holding one file per
/// chunk read or written so far: the warm tier between the page cache in memory and the cold
/// chunks only object storage holds (D1103).
pub struct PageStoreFile<O: ObjectStore> {
    objects: O,
    directory: PathBuf,
    lock: eggdb_platform::OsFile,
    disk_budget: Option<u64>,
    state: Mutex<State>,
}

impl<O: ObjectStore> std::fmt::Debug for PageStoreFile<O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageStoreFile").field("directory", &self.directory).finish_non_exhaustive()
    }
}

fn local_name(index: u64, version: u64) -> String {
    format!("{index:016x}-{version:016x}")
}

fn parse_local_name(name: &str) -> Option<(u64, u64)> {
    let (index, version) = name.split_once('-')?;
    Some((u64::from_str_radix(index, 16).ok()?, u64::from_str_radix(version, 16).ok()?))
}

fn whole(length: u64) -> Result<usize> {
    usize::try_from(length).map_err(|_| Error::new(ErrorKind::InvalidInput))
}

impl<O: ObjectStore> PageStoreFile<O> {
    /// Opens the file kept in `objects`, an empty one when it has no manifest yet, caching its
    /// chunks as files in the directory `cache` (created when missing), with no limit on the
    /// disk they take. Chunk files already there at the manifest's versions are used again;
    /// others are removed.
    pub fn open(objects: O, cache: &Path) -> Result<Self> {
        Self::open_with_budget(objects, cache, None)
    }

    /// Opens as [`PageStoreFile::open`] does, keeping at most `disk_budget` bytes of chunks on
    /// disk when given: past it, the chunks read longest ago leave the disk and are fetched
    /// from object storage again when next read. Chunks changed since the last sync stay until
    /// they are uploaded.
    pub fn open_with_budget(objects: O, cache: &Path, disk_budget: Option<u64>) -> Result<Self> {
        let manifest = match objects.get(MANIFEST).map_err(failed)? {
            Some(bytes) => std::str::from_utf8(&bytes)
                .ok()
                .and_then(Manifest::decode)
                .ok_or_else(|| failed("the manifest is damaged"))?,
            None => Manifest::default(),
        };
        let fs = OsFileSystem::new();
        fs.create_dir_all(cache)?;
        let lock_path = cache.join("lock");
        let lock = match fs.open(&lock_path, OpenMode::ReadWrite) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => fs.open(&lock_path, OpenMode::CreateNew)?,
            Err(error) => return Err(error),
        };
        let mut state = State { size: manifest.size, ..State::default() };
        if let Ok(entries) = std::fs::read_dir(cache) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some((index, version)) = parse_local_name(&name) else {
                    continue;
                };
                if manifest.chunks.get(&index) == Some(&version) {
                    state.present.insert(index, version);
                } else {
                    let _ = fs.remove_file(&entry.path());
                }
            }
        }
        state.manifest = manifest;
        Ok(Self { objects, directory: cache.to_path_buf(), lock, disk_budget, state: Mutex::new(state) })
    }

    /// The manifest as of the last sync.
    pub fn manifest(&self) -> Manifest {
        self.lock().map(|state| state.manifest.clone()).unwrap_or_default()
    }

    /// The chunks held on disk now.
    pub fn chunks_on_disk(&self) -> usize {
        self.lock().map(|state| state.present.len()).unwrap_or(0)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, State>> {
        self.state.lock().map_err(|_| failed("the page store's lock is poisoned"))
    }

    fn chunk_length(size: u64, index: u64) -> u64 {
        size.saturating_sub(index * CHUNK_BYTES).min(CHUNK_BYTES)
    }

    fn local_path(&self, index: u64, version: u64) -> PathBuf {
        self.directory.join(local_name(index, version))
    }

    fn chunk_file<'s>(&self, state: &'s mut State, index: u64) -> Result<&'s eggdb_platform::OsFile> {
        if !state.files.contains_key(&index) {
            let version = state.present.get(&index).copied().unwrap_or(0);
            let path = self.local_path(index, version);
            let fs = OsFileSystem::new();
            let file = match fs.open(&path, OpenMode::ReadWrite) {
                Ok(file) => file,
                Err(error) if error.kind() == ErrorKind::NotFound => fs.open(&path, OpenMode::CreateNew)?,
                Err(error) => return Err(error),
            };
            state.files.insert(index, file);
        }
        state.files.get(&index).ok_or_else(|| Error::new(ErrorKind::NotFound))
    }

    fn touch(state: &mut State, index: u64) {
        state.tick += 1;
        let tick = state.tick;
        state.last_used.insert(index, tick);
    }

    fn ensure(&self, state: &mut State, index: u64) -> Result<()> {
        Self::touch(state, index);
        if state.present.contains_key(&index) || state.dirty.contains(&index) {
            return Ok(());
        }
        let length = Self::chunk_length(state.size.max(state.manifest.size), index);
        let version = state.manifest.chunks.get(&index).copied();
        let mut bytes = match version {
            Some(version) => self
                .objects
                .get(&chunk_name(index, version))
                .map_err(failed)?
                .ok_or_else(|| failed(format!("chunk {index} at version {version} is missing")))?,
            None => Vec::new(),
        };
        bytes.resize(whole(length)?, 0);
        state.present.insert(index, version.unwrap_or(0));
        state.files.remove(&index);
        let file = self.chunk_file(state, index)?;
        file.set_size(0)?;
        file.write_all_at(&bytes, 0)?;
        self.evict(state)
    }

    fn evict(&self, state: &mut State) -> Result<()> {
        let Some(budget) = self.disk_budget else {
            return Ok(());
        };
        let mut held: u64 = state.present.len() as u64 * CHUNK_BYTES;
        while held > budget {
            let coldest = state
                .present
                .keys()
                .filter(|index| !state.dirty.contains(index))
                .min_by_key(|index| state.last_used.get(index).copied().unwrap_or(0))
                .copied();
            let Some(coldest) = coldest else {
                return Ok(());
            };
            let version = state.present.remove(&coldest).unwrap_or(0);
            state.files.remove(&coldest);
            state.last_used.remove(&coldest);
            let _ = OsFileSystem::new().remove_file(&self.local_path(coldest, version));
            held = held.saturating_sub(CHUNK_BYTES);
        }
        Ok(())
    }

    fn rename_local(&self, state: &mut State, index: u64, from: u64, to: u64) {
        state.files.remove(&index);
        let _ = std::fs::rename(self.local_path(index, from), self.local_path(index, to));
    }
}

impl<O: ObjectStore> File for PageStoreFile<O> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        let mut state = self.lock()?;
        let end = offset.saturating_add(buf.len() as u64).min(state.size);
        if offset >= end {
            return Ok(0);
        }
        let mut at = offset;
        while at < end {
            let index = at / CHUNK_BYTES;
            self.ensure(&mut state, index)?;
            let within = at - index * CHUNK_BYTES;
            let take = (CHUNK_BYTES - within).min(end - at);
            let from = whole(at - offset)?;
            let to = from + whole(take)?;
            let file = self.chunk_file(&mut state, index)?;
            let read = file.read_at(&mut buf[from..to], within)?;
            buf[from + read..to].fill(0);
            at += take;
        }
        whole(end - offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut state = self.lock()?;
        let end = offset.checked_add(buf.len() as u64).ok_or_else(|| Error::new(ErrorKind::InvalidInput))?;
        let mut at = offset;
        while at < end {
            let index = at / CHUNK_BYTES;
            let start = index * CHUNK_BYTES;
            let covers_whole = at == start && end >= start + CHUNK_BYTES;
            if !covers_whole && start < state.size {
                self.ensure(&mut state, index)?;
            } else {
                Self::touch(&mut state, index);
                state.present.entry(index).or_insert(0);
            }
            state.dirty.insert(index);
            let within = at - start;
            let take = (CHUNK_BYTES - within).min(end - at);
            let from = whole(at - offset)?;
            let file = self.chunk_file(&mut state, index)?;
            file.write_all_at(&buf[from..from + whole(take)?], within)?;
            at += take;
        }
        state.size = state.size.max(end);
        Ok(buf.len())
    }

    fn size(&self) -> Result<u64> {
        Ok(self.lock()?.size)
    }

    fn set_size(&self, size: u64) -> Result<()> {
        let mut state = self.lock()?;
        if size < state.size && size % CHUNK_BYTES != 0 {
            let last = size / CHUNK_BYTES;
            self.ensure(&mut state, last)?;
            state.dirty.insert(last);
            self.chunk_file(&mut state, last)?.set_size(size % CHUNK_BYTES)?;
        }
        if size > state.size {
            for index in state.size / CHUNK_BYTES..size.div_ceil(CHUNK_BYTES) {
                state.dirty.insert(index);
                state.present.entry(index).or_insert(0);
            }
        }
        let dropped: Vec<(u64, u64)> = state
            .present
            .iter()
            .filter(|(index, _)| **index * CHUNK_BYTES >= size)
            .map(|(index, version)| (*index, *version))
            .collect();
        for (index, version) in dropped {
            state.present.remove(&index);
            state.files.remove(&index);
            let _ = OsFileSystem::new().remove_file(&self.local_path(index, version));
        }
        state.dirty.retain(|index| index * CHUNK_BYTES < size);
        state.size = size;
        Ok(())
    }

    fn is_uncached(&self) -> bool {
        false
    }

    fn try_lock(&self, lock: FileLock) -> Result<()> {
        self.lock.try_lock(lock)
    }

    fn unlock(&self) -> Result<()> {
        self.lock.unlock()
    }

    fn sync_data(&self) -> Result<()> {
        let mut state = self.lock()?;
        let shrunk = state.manifest.chunks.keys().any(|index| index * CHUNK_BYTES >= state.size);
        if state.dirty.is_empty() && state.size == state.manifest.size && !shrunk {
            return Ok(());
        }
        let generation = state.manifest.generation + 1;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX));
        let mut next = Manifest {
            size: state.size,
            generation,
            chunks: state.manifest.chunks.clone(),
            retired: state.manifest.retired.clone(),
        };
        next.chunks.retain(|index, _| index * CHUNK_BYTES < state.size);
        let dirty: Vec<u64> = state.dirty.iter().copied().collect();
        for index in &dirty {
            let length = Self::chunk_length(state.size, *index);
            let mut bytes = vec![0u8; whole(length)?];
            let file = self.chunk_file(&mut state, *index)?;
            let read = file.read_at(&mut bytes, 0)?;
            bytes[read..].fill(0);
            file.sync_data()?;
            self.objects.put(&chunk_name(*index, generation), &bytes).map_err(failed)?;
            next.chunks.insert(*index, generation);
        }
        let superseded: Vec<String> = state
            .manifest
            .chunks
            .iter()
            .filter(|(index, version)| next.chunks.get(index) != Some(version))
            .map(|(index, version)| chunk_name(*index, *version))
            .collect();
        next.retired.extend(superseded.into_iter().map(|name| (name, now)));
        let expired: Vec<String> = next
            .retired
            .iter()
            .filter(|(_, at)| now.saturating_sub(*at) >= RETIRED_KEPT_MS)
            .map(|(name, _)| name.clone())
            .collect();
        next.retired.retain(|(_, at)| now.saturating_sub(*at) < RETIRED_KEPT_MS);
        self.objects.put(MANIFEST, next.encode().as_bytes()).map_err(failed)?;
        for index in &dirty {
            let old = state.present.get(index).copied().unwrap_or(0);
            if old != generation {
                self.rename_local(&mut state, *index, old, generation);
            }
            state.present.insert(*index, generation);
        }
        state.dirty.clear();
        state.manifest = next;
        self.evict(&mut state)?;
        drop(state);
        for name in expired {
            let _ = self.objects.delete(&name);
        }
        Ok(())
    }

    fn sync_all(&self) -> Result<()> {
        self.sync_data()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn fresh(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("eggdb-page-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn manifests_read_back_and_damaged_ones_are_refused() {
        let manifest = Manifest {
            size: 5,
            generation: 2,
            chunks: BTreeMap::from([(0, 2), (3, 1)]),
            retired: vec![("chunks/0000000000000000-0000000000000001".to_owned(), 99)],
        };
        assert_eq!(Manifest::decode(&manifest.encode()), Some(manifest));
        assert_eq!(Manifest::decode("something else\n"), None);
        assert_eq!(Manifest::decode("eggdb-pages 1\nsize x\n"), None);
    }

    #[test]
    fn a_sync_uploads_only_changed_chunks_and_a_new_node_reads_them_on_demand() {
        let objects = Arc::new(MemoryObjects::default());
        let file = PageStoreFile::open(Arc::clone(&objects), &fresh("first")).unwrap();
        file.write_all_at(&vec![7u8; 3 * CHUNK_BYTES as usize], 0).unwrap();
        file.sync_data().unwrap();
        assert_eq!(file.manifest().chunks.len(), 3);
        file.write_all_at(b"changed", CHUNK_BYTES + 10).unwrap();
        file.sync_data().unwrap();
        let manifest = file.manifest();
        assert_eq!(manifest.chunks[&0], 1);
        assert_eq!(manifest.chunks[&1], 2);
        assert_eq!(objects.names().iter().filter(|name| name.starts_with("chunks/")).count(), 4);
        assert_eq!(manifest.retired.len(), 1);
        drop(file);
        let reader = PageStoreFile::open(Arc::clone(&objects), &fresh("second")).unwrap();
        assert_eq!(reader.size().unwrap(), 3 * CHUNK_BYTES);
        let mut read = [0u8; 7];
        reader.read_exact_at(&mut read, CHUNK_BYTES + 10).unwrap();
        assert_eq!(&read, b"changed");
        assert_eq!(reader.chunks_on_disk(), 1);
        reader.set_size(CHUNK_BYTES / 2).unwrap();
        reader.sync_data().unwrap();
        assert_eq!(reader.manifest().chunks.keys().copied().collect::<Vec<_>>(), vec![0]);
        assert_eq!(reader.manifest().retired.len(), 4);
    }

    #[test]
    fn a_disk_budget_sends_the_coldest_chunks_back_to_object_storage() {
        let objects = Arc::new(MemoryObjects::default());
        let writer = PageStoreFile::open(Arc::clone(&objects), &fresh("tier-writer")).unwrap();
        for index in 0..6u64 {
            writer.write_all_at(&vec![index as u8; CHUNK_BYTES as usize], index * CHUNK_BYTES).unwrap();
        }
        writer.sync_data().unwrap();
        drop(writer);
        let tiered = PageStoreFile::open_with_budget(Arc::clone(&objects), &fresh("tier-reader"), Some(2 * CHUNK_BYTES)).unwrap();
        let mut byte = [0u8; 1];
        for index in [0u64, 1, 2, 0, 3] {
            tiered.read_exact_at(&mut byte, index * CHUNK_BYTES).unwrap();
            assert_eq!(byte[0], index as u8);
            assert!(tiered.chunks_on_disk() <= 2);
        }
        let held = tiered.lock().unwrap().present.keys().copied().collect::<Vec<_>>();
        assert_eq!(held, vec![0, 3]);
        tiered.read_exact_at(&mut byte, 1 + CHUNK_BYTES).unwrap();
        assert_eq!(byte[0], 1);
    }
}
