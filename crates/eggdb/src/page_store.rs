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
}

/// A database file over an [`ObjectStore`], with a local cache file of the chunks read or
/// written so far.
pub struct PageStoreFile<O: ObjectStore> {
    objects: O,
    cache: eggdb_platform::OsFile,
    present_path: PathBuf,
    state: Mutex<State>,
}

impl<O: ObjectStore> std::fmt::Debug for PageStoreFile<O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageStoreFile").field("present_path", &self.present_path).finish_non_exhaustive()
    }
}

impl<O: ObjectStore> PageStoreFile<O> {
    /// Opens the file kept in `objects`, an empty one when it has no manifest yet, caching its
    /// chunks in the file at `cache` (created when missing). Chunks the cache already holds at
    /// the manifest's versions, as `cache` plus `.chunks` records, are not fetched again.
    pub fn open(objects: O, cache: &Path) -> Result<Self> {
        let manifest = match objects.get(MANIFEST).map_err(failed)? {
            Some(bytes) => std::str::from_utf8(&bytes)
                .ok()
                .and_then(Manifest::decode)
                .ok_or_else(|| failed("the manifest is damaged"))?,
            None => Manifest::default(),
        };
        let fs = OsFileSystem::new();
        let file = match fs.open(cache, OpenMode::ReadWrite) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => fs.open(cache, OpenMode::CreateNew)?,
            Err(error) => return Err(error),
        };
        let mut present_path = cache.as_os_str().to_owned();
        present_path.push(".chunks");
        let present_path = PathBuf::from(present_path);
        let recorded = std::fs::read_to_string(&present_path).unwrap_or_default();
        let present: BTreeMap<u64, u64> = recorded
            .lines()
            .filter_map(|line| {
                let (index, version) = line.split_once(' ')?;
                Some((index.parse().ok()?, version.parse().ok()?))
            })
            .filter(|(index, version)| manifest.chunks.get(index) == Some(version))
            .collect();
        let size = manifest.size;
        Ok(Self {
            objects,
            cache: file,
            present_path,
            state: Mutex::new(State { manifest, size, present, dirty: BTreeSet::new() }),
        })
    }

    /// The manifest as of the last sync.
    pub fn manifest(&self) -> Manifest {
        self.lock().map(|state| state.manifest.clone()).unwrap_or_default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, State>> {
        self.state.lock().map_err(|_| failed("the page store's lock is poisoned"))
    }

    fn chunk_length(size: u64, index: u64) -> u64 {
        size.saturating_sub(index * CHUNK_BYTES).min(CHUNK_BYTES)
    }

    fn ensure(&self, state: &mut State, index: u64) -> Result<()> {
        if state.present.contains_key(&index) || state.dirty.contains(&index) {
            return Ok(());
        }
        let length = Self::chunk_length(state.size.max(state.manifest.size), index);
        let mut bytes = match state.manifest.chunks.get(&index) {
            Some(version) => self
                .objects
                .get(&chunk_name(index, *version))
                .map_err(failed)?
                .ok_or_else(|| failed(format!("chunk {index} at version {version} is missing")))?,
            None => Vec::new(),
        };
        bytes.resize(usize::try_from(length).map_err(|_| Error::new(ErrorKind::InvalidInput))?, 0);
        self.cache.write_all_at(&bytes, index * CHUNK_BYTES)?;
        let version = state.manifest.chunks.get(&index).copied().unwrap_or(0);
        state.present.insert(index, version);
        Ok(())
    }

    fn record_present(&self, state: &State) -> Result<()> {
        let text: String = state
            .present
            .iter()
            .map(|(index, version)| format!("{index} {version}\n"))
            .collect();
        std::fs::write(&self.present_path, text).map_err(Error::from)
    }
}

impl<O: ObjectStore> File for PageStoreFile<O> {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        let mut state = self.lock()?;
        let end = offset.saturating_add(buf.len() as u64).min(state.size);
        if offset >= end {
            return Ok(0);
        }
        for index in offset / CHUNK_BYTES..=(end - 1) / CHUNK_BYTES {
            self.ensure(&mut state, index)?;
        }
        let wanted = usize::try_from(end - offset).map_err(|_| Error::new(ErrorKind::InvalidInput))?;
        self.cache.read_exact_at(&mut buf[..wanted], offset)?;
        Ok(wanted)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut state = self.lock()?;
        let end = offset.checked_add(buf.len() as u64).ok_or_else(|| Error::new(ErrorKind::InvalidInput))?;
        for index in offset / CHUNK_BYTES..=(end - 1) / CHUNK_BYTES {
            let start = index * CHUNK_BYTES;
            let covers_whole = offset <= start && end >= start + CHUNK_BYTES;
            if !covers_whole && index * CHUNK_BYTES < state.size {
                self.ensure(&mut state, index)?;
            }
            state.dirty.insert(index);
        }
        self.cache.write_all_at(buf, offset)?;
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
        }
        if size > state.size {
            for index in state.size / CHUNK_BYTES..size.div_ceil(CHUNK_BYTES) {
                state.dirty.insert(index);
            }
        }
        state.dirty.retain(|index| index * CHUNK_BYTES < size);
        state.present.retain(|index, _| index * CHUNK_BYTES < size);
        self.cache.set_size(size)?;
        state.size = size;
        Ok(())
    }

    fn is_uncached(&self) -> bool {
        false
    }

    fn try_lock(&self, lock: FileLock) -> Result<()> {
        self.cache.try_lock(lock)
    }

    fn unlock(&self) -> Result<()> {
        self.cache.unlock()
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
            let mut bytes = vec![0u8; usize::try_from(length).map_err(|_| Error::new(ErrorKind::InvalidInput))?];
            self.cache.read_exact_at(&mut bytes, index * CHUNK_BYTES)?;
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
        self.cache.sync_data()?;
        self.objects.put(MANIFEST, next.encode().as_bytes()).map_err(failed)?;
        for index in &dirty {
            state.present.insert(*index, generation);
        }
        state.dirty.clear();
        state.manifest = next;
        self.record_present(&state)?;
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

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("eggdb-page-store-{name}-{}", std::process::id()))
    }

    fn fresh(name: &str) -> PathBuf {
        let path = scratch(name);
        let _ = std::fs::remove_file(&path);
        let mut present = path.as_os_str().to_owned();
        present.push(".chunks");
        let _ = std::fs::remove_file(PathBuf::from(present));
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
        let first_cache = fresh("first");
        let file = PageStoreFile::open(Arc::clone(&objects), &first_cache).unwrap();
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
        let second_cache = fresh("second");
        let reader = PageStoreFile::open(Arc::clone(&objects), &second_cache).unwrap();
        assert_eq!(reader.size().unwrap(), 3 * CHUNK_BYTES);
        let mut read = [0u8; 7];
        reader.read_exact_at(&mut read, CHUNK_BYTES + 10).unwrap();
        assert_eq!(&read, b"changed");
        assert_eq!(reader.lock().unwrap().present.len(), 1);
        reader.set_size(CHUNK_BYTES / 2).unwrap();
        reader.sync_data().unwrap();
        assert_eq!(reader.manifest().chunks.keys().copied().collect::<Vec<_>>(), vec![0]);
        assert_eq!(reader.manifest().retired.len(), 4);
    }
}
