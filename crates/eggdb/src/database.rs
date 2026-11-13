use crate::error::{Error, ErrorKind, Result};
use crate::expiry::Moment;
use crate::options::Options;
use crate::transaction::{ReadTransaction, WriteTransaction};
use crate::writer_slot::WriterSlot;
use eggdb_platform::{Clock, DatabaseFile, FileSystem, OpenMode, OsFileSystem, OsMemory, PowerSource};
use eggdb_storage::{
    Access, BTree, CacheSize, CommitStamp, CopyDestination, CopyFailure, CreateOptions, Features,
    FileId, NO_PAGE, PageCache, PageFile, PageSize, StorageError, Store,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

const FIRST_BUSY_PAUSE: Duration = Duration::from_millis(1);
const LONGEST_BUSY_PAUSE: Duration = Duration::from_millis(50);

const NANOS_PER_MICRO: u64 = 1_000;
const BACKUP_STAGING_SUFFIX: &str = ".backing-up";
const COMPACTION_STAGING_SUFFIX: &str = ".compacting";

/// An EggDB database in one file, opened in this process: an ordered map from byte-string keys
/// to byte-string values, with transactions.
///
/// **Transactions.** Every read and change happens in a transaction. One write transaction
/// at a time ([`Database::begin_write`]) changes the database and commits atomically; any
/// number of read transactions ([`Database::begin_read`]) each see a snapshot of the last
/// commit before they began. [`Database::read`] and [`Database::write`] run a closure in a
/// transaction, and [`Database::get`], [`Database::put`] and [`Database::delete`] are one-step
/// transactions of their own.
///
/// **Threads.** A `Database` is `Send` and `Sync`: share it between threads by reference or
/// through an [`Arc`]. Readers never wait for the writer, nor the writer for readers. A
/// second writer waits for the first to end, up to [`Options::write_timeout`].
///
/// **One file, one user.** The file is locked while the database is open: an open for
/// writing excludes every other open of the file, in this process or another, and a
/// read-only open excludes writers. A second open fails at once with
/// [`ErrorKind::Locked`](crate::ErrorKind::Locked).
///
/// **Closing.** [`Database::close`] writes a checkpoint, so the next open has no log to
/// replay, and reports any error. Dropping the database closes the file without writing
/// anything: no commit is lost by a drop, and the next open replays the log written since
/// the last checkpoint, at most about a megabyte by default. Under
/// [`Durability::Normal`](crate::Durability::Normal), commits since the last sync stay
/// exposed to a power loss until the next open syncs them, as that mode allows.
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let path = std::env::temp_dir().join(format!("eggdb-doc-database-{}.db", std::process::id()));
/// use eggdb::Database;
///
/// let db = Database::open(&path)?;
/// db.write(|tx| {
///     tx.put("user:1", "Ada")?;
///     tx.put("user:2", "Grace")?;
///     Ok(())
/// })?;
/// let names: Vec<Vec<u8>> = db.read(|tx| {
///     tx.scan_prefix("user:")
///         .map(|entry| entry.map(|(_, name)| name))
///         .collect()
/// })?;
/// assert_eq!(names, [b"Ada".to_vec(), b"Grace".to_vec()]);
/// db.close()?;
/// # std::fs::remove_file(&path)?;
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct Database {
    store: Store<DatabaseFile>,
    tree: OnceLock<BTree>,
    writer: WriterSlot,
    path: PathBuf,
    page_size: PageSize,
    read_only: bool,
    write_timeout: Duration,
    clock: ClockHandle,
    expiring_keys: AtomicBool,
    collection_drops: AtomicU8,
    change_archive: Option<crate::change_archive::ChangeArchive>,
    encrypted: bool,
    primary: OnceLock<Arc<crate::replication::ReplicationPrimary>>,
    fence: std::sync::Mutex<Option<String>>,
    row_capture: OnceLock<fn(&[u8]) -> bool>,
    auto_checkpoint_log_bytes: Option<u64>,
    power_source: std::sync::Mutex<PowerSource>,
}

/// How much more log a commit leaves before its automatic checkpoint while the machine runs on
/// a battery (D1087).
const BATTERY_CHECKPOINT_STRETCH: u64 = 4;

struct ClockHandle(Arc<dyn Clock>);

impl std::fmt::Debug for ClockHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Clock")
    }
}

/// What [`Database::stats`] reports: the file's size and free space, the last commit, and
/// what the page cache and the log have done since the database was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Stats {
    /// The page size, in bytes.
    pub page_size: usize,
    /// The pages in the file as of the last commit, the header's and the log's pages
    /// included.
    pub page_count: u64,
    /// Pages in the file that are free for later commits to reuse.
    pub free_pages: u64,
    /// The file's size as of the last commit: `page_count × page_size`. The file on disk can be
    /// longer by the log space of its longest transaction, which later ones reuse and the next
    /// open for writing gives back.
    pub file_bytes: u64,
    /// The stamp of the last commit, or [`CommitStamp::NONE`] when nothing was ever committed.
    pub last_commit: CommitStamp,
    /// How many pages the page cache holds at most.
    pub cache_pages: usize,
    /// How many pages it holds now.
    pub cached_pages: usize,
    /// Page lookups answered from the cache since the database was opened.
    pub cache_hits: u64,
    /// Page lookups that read the page from the file since the database was opened.
    pub cache_misses: u64,
    /// Bytes of write-ahead log since the last checkpoint, which the next open would replay.
    pub log_bytes_since_checkpoint: u64,
    /// Whether the file has held keys with a time to live: reads then look up the deadline of
    /// each key they return, and commits remove keys whose time has run out.
    pub expiring_keys: bool,
}

/// What [`Database::check`] found in a database whose every page it read passed, and what
/// each page of the file is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct CheckReport {
    /// The stamp of the commit that was checked.
    pub stamp: CommitStamp,
    /// How many keys the database holds.
    pub entries: u64,
    /// The bytes of every key together.
    pub key_bytes: u64,
    /// The bytes of every value together.
    pub value_bytes: u64,
    /// The B+tree's levels from its root to its leaves; 0 for a database that never held a
    /// key.
    pub tree_height: u32,
    /// The B+tree's pages, inner and leaf.
    pub tree_pages: u64,
    /// Values too large for a leaf, stored on overflow pages.
    pub overflow_values: u64,
    /// The overflow pages holding them.
    pub overflow_pages: u64,
    /// Pages free for later commits to reuse, as [`Stats::free_pages`] counts them.
    pub free_pages: u64,
    /// Pages of the write-ahead log, the segments kept for reuse included.
    pub log_pages: u64,
    /// Pages in use by nothing at all: neither the tree, its values, the free list nor the
    /// log. They do no harm, but the file lost track of them and never reuses them; a healthy
    /// database has none.
    pub leaked_pages: u64,
}

/// What [`Database::clone_to`] made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct CloneReport {
    /// The stamp of the commit the clone holds.
    pub stamp: CommitStamp,
    /// Whether the file system cloned the file copy-on-write; otherwise the clone is a copy
    /// [`Database::backup_to`] wrote.
    pub instant: bool,
}

/// What [`Database::backup_differential_to`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct DifferentialReport {
    /// The stamp of the base copy's commit the differential starts from.
    pub base: CommitStamp,
    /// The stamp of the commit the differential reaches.
    pub stamp: CommitStamp,
    /// The keys it changes or deletes.
    pub changes: u64,
}

/// What [`Database::backup_to`] wrote: a new database file holding one commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct BackupReport {
    /// The stamp of the commit the copy holds: the last commit before the backup began.
    pub stamp: CommitStamp,
    /// The entries copied, counted as [`CheckReport::entries`] counts them: every key of the
    /// default key space, of the collections and of SQL tables, with the records EggDB keeps
    /// beside them, such as keys' deadlines.
    pub entries: u64,
    /// The bytes of every key copied.
    pub key_bytes: u64,
    /// The bytes of every value copied.
    pub value_bytes: u64,
    /// The copy's pages, as [`Stats::page_count`] counts them.
    pub page_count: u64,
    /// The copy's size: `page_count × page_size`.
    pub file_bytes: u64,
}

/// What [`Database::compact`] did: the file before, and the compacted file that replaced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct CompactReport {
    /// The stamp of the commit the compacted file holds: the file's last commit.
    pub stamp: CommitStamp,
    /// The entries the compacted file holds, counted as [`CheckReport::entries`] counts them.
    pub entries: u64,
    /// The file's pages before, as [`Stats::page_count`] counts them.
    pub pages_before: u64,
    /// The free pages it had, as [`Stats::free_pages`] counts them.
    pub free_pages_before: u64,
    /// The pages it had lost track of, as [`CheckReport::leaked_pages`] counts them.
    pub leaked_pages_before: u64,
    /// The compacted file's pages.
    pub pages_after: u64,
    /// The file's size before: `pages_before × page_size`.
    pub bytes_before: u64,
    /// The compacted file's size: `pages_after × page_size`.
    pub bytes_after: u64,
}

impl Database {
    /// Opens the database file at `path`, creating an empty one if no file is there, with the
    /// default [`Options`].
    ///
    /// Opening recovers the file: it replays every commit its write-ahead log holds since the
    /// last checkpoint, so a crash at any moment leaves exactly the commits that were durable.
    /// It fails with [`ErrorKind::Locked`](crate::ErrorKind::Locked) when the file is open
    /// elsewhere, [`ErrorKind::NotADatabase`](crate::ErrorKind::NotADatabase) for another kind
    /// of file, and [`ErrorKind::Corrupt`](crate::ErrorKind::Corrupt) for a damaged one.
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-open-{}.db", std::process::id()));
    /// let db = eggdb::Database::open(&path)?;
    /// db.put("k", "v")?;
    /// drop(db);
    /// let reopened = eggdb::Database::open(&path)?;
    /// assert_eq!(reopened.get("k")?, Some(b"v".to_vec()));
    /// # drop(reopened);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(()) }
    /// ```
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, Options::new())
    }

    /// Opens the database file at `path` as `options` say (see [`Options`]).
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-open-with-{}.db", std::process::id()));
    /// use eggdb::{Database, ErrorKind, Options};
    ///
    /// Database::open(&path)?.put("k", "v")?;
    /// let reader = Database::open_with(&path, Options::new().read_only(true))?;
    /// assert_eq!(reader.get("k")?, Some(b"v".to_vec()));
    /// assert_eq!(reader.put("k", "w").unwrap_err().kind(), ErrorKind::ReadOnly);
    /// # drop(reader);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(()) }
    /// ```
    pub fn open_with(path: impl AsRef<Path>, options: Options) -> Result<Self> {
        let path = path.as_ref();
        Self::open_file(path, &options).map_err(|error| error.opening(path))
    }

    /// Creates a database that lives in memory and is gone once it is dropped: nothing is
    /// written to disk. [`Database::backup_to`] still writes a copy to a file.
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let db = eggdb::Database::in_memory()?;
    /// db.put("k", "v")?;
    /// assert_eq!(db.get("k")?, Some(b"v".to_vec()));
    /// # Ok(()) }
    /// ```
    pub fn in_memory() -> Result<Self> {
        Self::in_memory_with(Options::new())
    }

    /// Creates a database in memory with `options` (see [`Database::in_memory`]). A read-only
    /// database in memory would stay empty, so [`Options::read_only`] is refused.
    pub fn in_memory_with(options: Options) -> Result<Self> {
        if options.read_only {
            return Err(Error::new(
                ErrorKind::InvalidOption,
                "a database in memory starts empty, so it can't be opened read-only",
            ));
        }
        let page_size = options.new_file_page_size()?;
        options.check_cache_holds(page_size)?;
        let file = DatabaseFile::memory().map_err(|error| Error::storage(error.into()))?;
        let created = PageFile::create_in(
            file,
            CreateOptions {
                page_size,
                file_id: FileId::random(),
            },
        )
        .map_err(Error::storage)?;
        Self::with_page_file(created, Path::new(MEMORY_NAME), &options)
    }

    /// Whether the database lives in memory (see [`Database::in_memory`]).
    pub fn is_in_memory(&self) -> bool {
        self.path == Path::new(MEMORY_NAME)
    }

    fn open_file(path: &Path, options: &Options) -> Result<Self> {
        let new_file_page_size = options.new_file_page_size()?;
        options.check_cache_holds(new_file_page_size)?;
        let file = open_waiting_while_busy(path, options, new_file_page_size)?;
        Self::with_page_file(file, path, options)
    }

    fn with_page_file(
        mut file: PageFile<DatabaseFile>,
        path: &Path,
        options: &Options,
    ) -> Result<Self> {
        match &options.encryption_key {
            Some(key) => file.use_page_key(&key.0).map_err(Error::storage)?,
            None if file.is_encrypted() => {
                return Err(Error::storage(eggdb_storage::StorageError::NeedsKey));
            }
            None => {}
        }
        let page_size = file.page_size();
        options.check_cache_holds(page_size)?;
        let mut cache = PageCache::new(
            file,
            &OsMemory::new(),
            CacheSize::Bytes(options.cache_bytes),
        )
        .map_err(Error::storage)?;
        if let Some(node) = options.cache_numa_node {
            cache
                .place_on_numa_node(eggdb_platform::NumaNodeId::new(node))
                .map_err(Error::storage)?;
        }
        if options.lock_cache {
            cache.lock_in_memory().map_err(Error::storage)?;
        }
        cache.set_value_compression(options.value_compression);
        let clock = options.clock.clock();
        let store = Store::new(cache, Arc::clone(&clock));
        let features = store.superblock().map_err(Error::storage)?.features;
        let expiring_keys = features.read_only_compatible & Features::EXPIRING_KEYS != 0;
        let configured = store
            .set_durability(options.durability.to_storage())
            .and_then(|()| store.set_auto_checkpoint_bytes(options.auto_checkpoint_log_bytes))
            .and_then(|()| store.set_snapshot_memory(CacheSize::Bytes(options.snapshot_memory())));
        configured.map_err(Error::storage)?;
        let change_archive = match &options.change_archive {
            Some(directory) if !options.read_only => {
                let last_commit = store.last_commit().map_err(Error::storage)?;
                Some(crate::change_archive::ChangeArchive::open(directory, last_commit)?)
            }
            _ => None,
        };
        Ok(Self {
            store,
            tree: OnceLock::new(),
            writer: WriterSlot::default(),
            path: path.to_path_buf(),
            page_size,
            read_only: options.read_only,
            write_timeout: options.write_timeout,
            clock: ClockHandle(clock),
            expiring_keys: AtomicBool::new(expiring_keys),
            collection_drops: AtomicU8::new(crate::collection::DROPS_UNKNOWN),
            change_archive,
            encrypted: options.encryption_key.is_some(),
            primary: OnceLock::new(),
            fence: std::sync::Mutex::new(None),
            row_capture: OnceLock::new(),
            auto_checkpoint_log_bytes: options.auto_checkpoint_log_bytes,
            power_source: std::sync::Mutex::new(PowerSource::Mains),
        })
    }

    /// Tells the database what the machine runs on, so background work costs less battery
    /// (D41, D1087). On [`PowerSource::Battery`] a commit leaves four times
    /// [`Options::auto_checkpoint_log_bytes`] of log before its automatic checkpoint, so the
    /// same pages are written to their places less often; on [`PowerSource::LowBattery`]
    /// commits checkpoint only when the log would outgrow the segments it reuses, and
    /// [`Database::checkpoint`] and closing still checkpoint at once. Nothing is less durable:
    /// a commit is on the disk when it returns either way, and only the next open's replay is
    /// longer. [`PowerSource::Mains`], the state after opening, restores the configured
    /// checkpoints. `eggdb_platform::power_source` reads it from the system.
    pub fn set_power_source(&self, source: PowerSource) -> Result<()> {
        let auto_checkpoint = match source {
            PowerSource::Mains => self.auto_checkpoint_log_bytes,
            PowerSource::Battery => self
                .auto_checkpoint_log_bytes
                .map(|bytes| bytes.saturating_mul(BATTERY_CHECKPOINT_STRETCH)),
            _ => None,
        };
        let mut current = self
            .power_source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.read_only {
            *current = source;
            return Ok(());
        }
        self.store
            .set_auto_checkpoint_bytes(auto_checkpoint)
            .map_err(Error::storage)?;
        *current = source;
        Ok(())
    }

    /// Gives the database file up while the app is suspended (D41, D1088). iOS suspends an
    /// app soon after it leaves the screen and ends one that is suspended while holding a lock
    /// on a file in a shared container; Android's Doze and app standby stop its work at any
    /// moment. Call this when the app goes to the background
    /// (`applicationDidEnterBackground`, `onStop`): it waits for the write transaction, as
    /// [`Database::begin_write`] does, writes a checkpoint, syncs, and releases the file's
    /// lock, so nothing is left to replay and no lock is held while the app sleeps. Until
    /// [`Database::resume`], new transactions fail with
    /// [`ErrorKind::Suspended`](crate::ErrorKind::Suspended); a read transaction already open
    /// reads what the cache holds and fails with it on the first page the cache must read.
    /// Suspending twice does nothing.
    pub fn suspend(&self) -> Result<()> {
        let _turn = self.writer.take(self.write_timeout)?;
        self.store.suspend().map_err(Error::storage)
    }

    /// Takes the database file back after [`Database::suspend`], when the app returns to the
    /// foreground. Fails with [`ErrorKind::Locked`] while another program, an app extension
    /// sharing the file, holds it; call it again later. When another program changed the file
    /// meanwhile, it fails with [`ErrorKind::NeedsReopen`], and the database must be opened
    /// again to see the changes. Does nothing when the database is not suspended.
    pub fn resume(&self) -> Result<()> {
        self.store.resume().map_err(Error::storage)
    }

    /// Whether [`Database::suspend`] gave the file up.
    pub fn is_suspended(&self) -> bool {
        self.store.is_suspended().unwrap_or(false)
    }

    /// What [`Database::set_power_source`] last set; [`PowerSource::Mains`] after opening.
    pub fn power_source(&self) -> PowerSource {
        *self
            .power_source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Captures every commit's changes to the keys `filter` accepts, with their stored bytes
    /// before and after the commit, as one capture record written in the same commit (D1030),
    /// for change data capture without triggers; `eggdb_sql::cdc` passes SQL table rows and
    /// delivers the records.
    pub fn capture_rows(&self, filter: fn(&[u8]) -> bool) -> Result<()> {
        self.row_capture
            .set(filter)
            .map_err(|_| Error::new(ErrorKind::InvalidOption, "the database already captures changes"))
    }

    pub(crate) fn row_capture(&self) -> Option<fn(&[u8]) -> bool> {
        self.row_capture.get().copied()
    }

    /// Refuses every write transaction from now on with `reason` (D1026), as a replica, an old
    /// primary that lost its lease, or a primary handing over in a switchover is; transactions
    /// already open finish. Batches a replica applies still go through.
    pub fn fence(&self, reason: impl Into<String>) {
        if let Ok(mut fence) = self.fence.lock() {
            *fence = Some(reason.into());
        }
    }

    /// Takes the fence away: write transactions begin again.
    pub fn unfence(&self) {
        if let Ok(mut fence) = self.fence.lock() {
            *fence = None;
        }
    }

    /// Fences the database with `reason` and waits until the write transaction open at that
    /// moment, if any, has finished, so no write commits after this returns (D1048): the
    /// freeze of a database moved to another server.
    pub fn fence_and_drain(&self, reason: impl Into<String>) -> Result<()> {
        self.fence(reason);
        let drained = self.begin_write_unfenced(self.write_timeout)?;
        drop(drained);
        Ok(())
    }

    /// Why writes are refused, while the database is fenced.
    pub fn fenced(&self) -> Option<String> {
        self.fence.lock().ok().and_then(|fence| fence.clone())
    }

    pub(crate) fn write_timeout(&self) -> Duration {
        self.write_timeout
    }

    pub(crate) fn primary(&self) -> Option<&Arc<crate::replication::ReplicationPrimary>> {
        self.primary.get().filter(|primary| !primary.is_retired())
    }

    pub(crate) fn set_primary(&self, primary: Arc<crate::replication::ReplicationPrimary>) -> Result<()> {
        self.primary
            .set(primary)
            .map_err(|_| Error::new(ErrorKind::InvalidOption, "the database already replicates as a primary"))
    }

    pub(crate) fn change_archive(&self) -> Option<&crate::change_archive::ChangeArchive> {
        self.change_archive.as_ref()
    }

    /// Begins a read transaction, which sees the last commit before it began and nothing
    /// after it (see [`ReadTransaction`]).
    pub fn begin_read(&self) -> Result<ReadTransaction<'_>> {
        ReadTransaction::begin(self)
    }

    /// Begins the write transaction (see [`WriteTransaction`]). While another one is open, it
    /// waits for that one to end, up to [`Options::write_timeout`] (five seconds by default),
    /// and then fails with [`ErrorKind::WriterActive`](crate::ErrorKind::WriterActive).
    /// Waiting writers are served in arrival order. It fails at once when this thread began
    /// the open write transaction, since waiting could never end; that holds even after the
    /// transaction was sent to another thread, so the beginning thread stays refused until it
    /// ends. On a read-only database it fails with
    /// [`ErrorKind::ReadOnly`](crate::ErrorKind::ReadOnly).
    pub fn begin_write(&self) -> Result<WriteTransaction<'_>> {
        self.begin_write_within(self.write_timeout)
    }

    /// Begins the write transaction, or fails at once with
    /// [`ErrorKind::WriterActive`](crate::ErrorKind::WriterActive) while another one is open.
    pub fn try_begin_write(&self) -> Result<WriteTransaction<'_>> {
        self.begin_write_within(Duration::ZERO)
    }

    pub(crate) fn begin_write_within(&self, timeout: Duration) -> Result<WriteTransaction<'_>> {
        if self.read_only {
            return Err(Error::storage(StorageError::ReadOnly));
        }
        if let Some(reason) = self.fenced() {
            return Err(Error::new(ErrorKind::ReadOnly, reason));
        }
        self.begin_write_unfenced(timeout)
    }

    pub(crate) fn begin_write_unfenced(&self, timeout: Duration) -> Result<WriteTransaction<'_>> {
        if self.read_only {
            return Err(Error::storage(StorageError::ReadOnly));
        }
        let turn = self.writer.take(timeout)?;
        WriteTransaction::begin(self, turn)
    }

    /// Runs `read` in a new read transaction and returns what it returns.
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-read-{}.db", std::process::id()));
    /// let db = eggdb::Database::open(&path)?;
    /// db.put("a", "1")?;
    /// db.put("b", "2")?;
    /// let both = db.read(|tx| Ok((tx.get("a")?, tx.get("b")?)))?;
    /// assert_eq!(both, (Some(b"1".to_vec()), Some(b"2".to_vec())));
    /// # drop(db);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(()) }
    /// ```
    pub fn read<T>(&self, read: impl FnOnce(&ReadTransaction<'_>) -> Result<T>) -> Result<T> {
        read(&self.begin_read()?)
    }

    /// Runs `write` in a new write transaction, which [`Database::begin_write`] begins, and
    /// commits it when `write` returns `Ok`; when it returns `Err`, rolls it back and returns
    /// the error. To return an error type of your own, use [`Database::begin_write`] and
    /// [`WriteTransaction::commit`] instead.
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-write-{}.db", std::process::id()));
    /// let db = eggdb::Database::open(&path)?;
    /// db.put("balance:alice", "10")?;
    /// let refused = db.write(|tx| {
    ///     tx.put("balance:alice", "0")?;
    ///     tx.put("balance:bob", "10")?;
    ///     tx.put(vec![0; 4096], "")?;
    ///     Ok(())
    /// });
    /// assert_eq!(refused.unwrap_err().kind(), eggdb::ErrorKind::TooLarge);
    /// assert_eq!(db.get("balance:alice")?, Some(b"10".to_vec()));
    /// assert_eq!(db.get("balance:bob")?, None);
    /// # drop(db);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(()) }
    /// ```
    pub fn write<T>(
        &self,
        write: impl FnOnce(&mut WriteTransaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut transaction = self.begin_write()?;
        let value = write(&mut transaction)?;
        transaction.commit()?;
        Ok(value)
    }

    /// The value stored under `key` as of the last commit, or `None`: a read transaction of
    /// one step.
    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        self.begin_read()?.get(key)
    }

    /// Stores `value` under `key` and commits: a write transaction of one step. Each call
    /// commits, and under [`Durability::Full`](crate::Durability::Full) waits for the disk, so
    /// many changes go faster together in one [`Database::write`].
    pub fn put(&self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()> {
        self.write(|transaction| transaction.put(key, value))
    }

    /// Removes `key` and commits, and returns whether the key was there: a write transaction
    /// of one step. Nothing is committed when the key was absent.
    pub fn delete(&self, key: impl AsRef<[u8]>) -> Result<bool> {
        let mut transaction = self.begin_write()?;
        if transaction.delete(key)? {
            transaction.commit()?;
            Ok(true)
        } else {
            transaction.rollback()?;
            Ok(false)
        }
    }

    /// Makes every commit so far durable with one sync, for databases opened with
    /// [`Durability::Normal`](crate::Durability::Normal); with
    /// [`Durability::Full`](crate::Durability::Full) commits already are. A read-only database
    /// fails with [`ErrorKind::ReadOnly`](crate::ErrorKind::ReadOnly).
    pub fn sync(&self) -> Result<()> {
        self.store.sync().map_err(Error::storage)
    }

    /// Writes a checkpoint: syncs every commit, writes the pages the write-ahead log holds to
    /// their places in the file, and lets the log start over, so the next open replays
    /// nothing. Commits write checkpoints by themselves (see
    /// [`Options::auto_checkpoint_log_bytes`]). An open write transaction is waited for, as
    /// [`Database::begin_write`] waits; read transactions are unaffected.
    pub fn checkpoint(&self) -> Result<()> {
        if self.read_only {
            return Err(Error::storage(StorageError::ReadOnly));
        }
        let _turn = self.writer.take(self.write_timeout)?;
        self.store.checkpoint().map_err(Error::storage)
    }

    /// Checks every page of the database's B+tree as of the last commit, how the pages fit
    /// together, and every large value's chain of overflow pages, and counts what it holds.
    /// Then it accounts for every page of the file: each must be exactly one of a page of the
    /// tree or of a value, a free page, or a page of the log. A damaged page, or a page with
    /// two uses, fails with [`ErrorKind::Corrupt`](crate::ErrorKind::Corrupt), with a message
    /// that says what is damaged; a page with no use is counted in
    /// [`CheckReport::leaked_pages`]. The free list was checked when the database was opened.
    /// Last, each collection's kept entry count is read and compared with its entries; a
    /// damaged count, or one that differs, fails with `Corrupt` naming the collection.
    ///
    /// The check reads the whole tree and every value, and holds the database while it does:
    /// other transactions' operations wait until it ends. An open write transaction's changes
    /// are not seen. Its memory is one bit per page of the file, beside one leaf at a time.
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-check-{}.db", std::process::id()));
    /// let db = eggdb::Database::open(&path)?;
    /// db.put("a", "1")?;
    /// db.put("bc", "23")?;
    /// let report = db.check()?;
    /// assert_eq!((report.entries, report.key_bytes, report.value_bytes), (2, 3, 3));
    /// # drop(db);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(()) }
    /// ```
    pub fn check(&self) -> Result<CheckReport> {
        let file = self.store.check().map_err(Error::storage)?;
        self.begin_read()?.check_collection_counts()?;
        let tree = file.tree.unwrap_or_default();
        Ok(CheckReport {
            stamp: file.stamp,
            entries: tree.entries,
            key_bytes: tree.key_bytes,
            value_bytes: tree.value_bytes,
            tree_height: tree.height,
            tree_pages: tree.inner_pages + tree.leaf_pages,
            overflow_values: tree.overflow_values,
            overflow_pages: tree.overflow_pages,
            free_pages: file.free_pages,
            log_pages: file.log_pages,
            leaked_pages: file.leaked_pages,
        })
    }

    /// The file's size and free space, the last commit, and what the page cache and the log
    /// have done since the database was opened. It reads no page.
    pub fn stats(&self) -> Result<Stats> {
        let superblock = self.store.superblock().map_err(Error::storage)?;
        let cache = self.store.cache_stats().map_err(Error::storage)?;
        let log = self.store.log_stats().map_err(Error::storage)?;
        let last_commit = self.store.last_commit().map_err(Error::storage)?;
        let page_size = self.page_size.bytes();
        Ok(Stats {
            page_size,
            page_count: superblock.page_count,
            free_pages: superblock.free_page_count,
            file_bytes: superblock.page_count.saturating_mul(page_size as u64),
            last_commit,
            cache_pages: cache.frames,
            cached_pages: cache.cached_pages,
            cache_hits: cache.hits,
            cache_misses: cache.misses,
            log_bytes_since_checkpoint: log.bytes_since_checkpoint,
            expiring_keys: self.has_expiring_keys(),
        })
    }

    /// Closes the database: writes a checkpoint, so the next open replays nothing, and closes
    /// the file, which lets other opens of it in. The file is closed even when the checkpoint
    /// fails, and the error says why; the next open then recovers the file as after a crash. A
    /// read-only database just closes. Dropping the database closes it without the checkpoint.
    pub fn close(self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        self.store.checkpoint().map_err(Error::storage)
    }

    /// Writes a differential backup of the last commit to the new file `path` (D1020): every
    /// key whose stored value differs from `base`'s, a copy of this database taken earlier
    /// (such as by [`Database::backup_to`]), and every key `base` has that this database no
    /// longer has, in the change archive's format. [`replay_events`](crate::replay_events) of
    /// [`read_change_file`](crate::read_change_file) applied to a restored `base` gives this
    /// commit's state.
    pub fn backup_differential_to(&self, base: &Database, path: impl AsRef<Path>) -> Result<DifferentialReport> {
        let live = self.begin_read()?;
        let old = base.begin_read()?;
        let report = DifferentialReport { base: old.stamp(), stamp: live.stamp(), changes: 0 };
        let mut writer = crate::change_archive::DeltaWriter::create(path.as_ref(), report.base, report.stamp)?;
        let mut ours = live.raw_scan();
        let mut theirs = old.raw_scan();
        let mut mine = ours.next().transpose()?;
        let mut other = theirs.next().transpose()?;
        loop {
            match (mine.take(), other.take()) {
                (None, None) => break,
                (Some((key, value)), None) => {
                    writer.push(key, Some(value))?;
                    mine = ours.next().transpose()?;
                }
                (None, Some((key, _))) => {
                    writer.push(key, None)?;
                    other = theirs.next().transpose()?;
                }
                (Some((key, value)), Some((old_key, old_value))) => match key.cmp(&old_key) {
                    std::cmp::Ordering::Less => {
                        writer.push(key, Some(value))?;
                        mine = ours.next().transpose()?;
                        other = Some((old_key, old_value));
                    }
                    std::cmp::Ordering::Greater => {
                        writer.push(old_key, None)?;
                        other = theirs.next().transpose()?;
                        mine = Some((key, value));
                    }
                    std::cmp::Ordering::Equal => {
                        if value != old_value {
                            writer.push(key, Some(value))?;
                        }
                        mine = ours.next().transpose()?;
                        other = theirs.next().transpose()?;
                    }
                },
            }
        }
        let changes = writer.finish()?;
        Ok(DifferentialReport { changes, ..report })
    }

    /// Makes a clone of the database as of its last commit in the new file `path` (D1021): a
    /// copy-on-write clone of the file where the file system makes them (APFS, Btrfs, XFS),
    /// which takes no time and shares the file's blocks until either side changes, and an
    /// ordinary copy ([`Database::backup_to`]) elsewhere, for an encrypted database and for one
    /// in memory. While the file is cloned, the write transaction waits, so the clone holds
    /// whole commits; opening it replays its log as after a stop. A clone is a branch: a database
    /// of its own from that commit on.
    pub fn clone_to(&self, path: impl AsRef<Path>) -> Result<CloneReport> {
        let path = path.as_ref();
        if !self.encrypted && !self.is_in_memory() {
            let turn = if self.read_only { None } else { Some(self.writer.take(self.write_timeout)?) };
            if turn.is_some() {
                self.store.sync().map_err(Error::storage)?;
            }
            let stamp = self.store.last_commit().map_err(Error::storage)?;
            let cloned = eggdb_platform::clone_file(&self.path, path).map_err(|error| {
                Error::new(ErrorKind::Io, format!("cannot clone {} to {}: {error}", self.path.display(), path.display()))
            })?;
            drop(turn);
            if cloned {
                return Ok(CloneReport { stamp, instant: true });
            }
        }
        let report = self.backup_to(path)?;
        Ok(CloneReport { stamp: report.stamp, instant: false })
    }

    /// Writes a copy of the database's last commit to a new file at `path`, while readers and
    /// the writer keep working: the backup reads one snapshot, as a read transaction does, so
    /// commits made while it runs are not in the copy, and it never waits for them.
    ///
    /// The copy is rebuilt key by key, with every collection, SQL table and time to live, so
    /// it holds no free or leaked pages and is often smaller than the file. It is built as
    /// `<path>.backing-up` beside `path`, then synced and checked: its entries must be
    /// exactly the snapshot's. Only then is it renamed to `path`, so a crash never leaves part
    /// of a copy at `path`. After an error the `.backing-up` file is removed, except when
    /// only the rename failed: then it is the complete copy, and the error says so. Its
    /// commits stamp above the copied one, so keys that had expired stay expired.
    ///
    /// A file at `path` fails with [`ErrorKind::AlreadyExists`](crate::ErrorKind::AlreadyExists),
    /// since a backup never replaces a file, and so does a `<path>.backing-up` left by a
    /// backup that did not finish, which the backup leaves alone. A writer that changes more
    /// pages than the snapshot memory holds while the backup runs makes it fail with
    /// [`ErrorKind::SnapshotTooOld`](crate::ErrorKind::SnapshotTooOld) (see
    /// [`Options::snapshot_memory_bytes`]). The backup takes a page cache of its own, 16 MiB
    /// or the size of the largest value.
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-backup-{}.db", std::process::id()));
    /// # let copy = std::env::temp_dir().join(format!("eggdb-doc-backup-{}-copy.db", std::process::id()));
    /// let db = eggdb::Database::open(&path)?;
    /// db.put("k", "v")?;
    /// let report = db.backup_to(&copy)?;
    /// assert_eq!(report.entries, 1);
    /// db.put("k", "w")?;
    /// assert_eq!(eggdb::Database::open(&copy)?.get("k")?, Some(b"v".to_vec()));
    /// # drop(db);
    /// # std::fs::remove_file(&path)?;
    /// # std::fs::remove_file(&copy)?;
    /// # Ok(()) }
    /// ```
    pub fn backup_to(&self, path: impl AsRef<Path>) -> Result<BackupReport> {
        self.backup_to_reporting(path, &mut |_| {})
    }

    /// Writes a copy as [`Database::backup_to`] does, calling `progress` with the entries
    /// copied so far and the entries the copy will hold after each batch (D1023).
    pub fn backup_to_reporting(&self, path: impl AsRef<Path>, progress: &mut dyn FnMut(u64, u64)) -> Result<BackupReport> {
        let copy = path.as_ref();
        let staging = staging_path(copy, BACKUP_STAGING_SUFFIX);
        let snapshot = self.store.begin_read().map_err(Error::storage)?;
        let destination = CopyDestination {
            path: copy,
            staging: &staging,
            replaces: false,
        };
        let report = snapshot
            .copy_to_reporting(
                &OsFileSystem::new(),
                &OsMemory::new(),
                Arc::clone(&self.clock.0),
                destination,
                &mut |step| progress(step.entries, step.total_entries),
            )
            .map_err(|failure| self.backup_failure(failure, destination))?;
        Ok(BackupReport {
            stamp: report.stamp,
            entries: report.entries,
            key_bytes: report.key_bytes,
            value_bytes: report.value_bytes,
            page_count: report.page_count,
            file_bytes: self.bytes_of(report.page_count),
        })
    }

    /// Opens the database file at `path` read-only and compacts it (see
    /// [`Database::compact`]), so programs reading the file keep working throughout.
    pub fn compact_file(path: impl AsRef<Path>) -> Result<CompactReport> {
        let options = Options::new().read_only(true);
        Self::open_with(path, options)?.compact()
    }

    /// Rewrites the database's file without free or leaked pages and puts the rewritten file
    /// in its place atomically, then closes the database.
    ///
    /// The file is first checked as [`Database::check`] checks it, so a damaged file fails
    /// with [`ErrorKind::Corrupt`](crate::ErrorKind::Corrupt) and is left as it is. Then it is
    /// rebuilt key by key as [`Database::backup_to`] builds a copy, as `<path>.compacting`
    /// beside it, synced and checked, and renamed over the file. A crash at any moment leaves
    /// the old file or the compacted one, each whole; a `<path>.compacting` left by a crash
    /// fails the next compaction with
    /// [`ErrorKind::AlreadyExists`](crate::ErrorKind::AlreadyExists) until it is removed.
    ///
    /// Opened read-only, as [`Database::compact_file`] opens it, the database shares the file
    /// with programs reading it, which keep reading the file they opened until they open it
    /// again; a program that has it open for writing makes the open fail with
    /// [`ErrorKind::Locked`](crate::ErrorKind::Locked). Opened for writing, nobody else has
    /// the file, and taking the database by value ends its use, so no commit can go to the
    /// file that was replaced.
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-compact-{}.db", std::process::id()));
    /// let db = eggdb::Database::open(&path)?;
    /// db.write(|tx| {
    ///     for i in 0..2000 {
    ///         tx.put(format!("key{i:04}"), [7u8; 100])?;
    ///     }
    ///     Ok(())
    /// })?;
    /// db.write(|tx| {
    ///     for i in 10..2000 {
    ///         tx.delete(format!("key{i:04}"))?;
    ///     }
    ///     Ok(())
    /// })?;
    /// let report = db.compact()?;
    /// assert!(report.pages_after < report.pages_before);
    /// assert_eq!(eggdb::Database::open(&path)?.get("key0003")?, Some(vec![7u8; 100]));
    /// # std::fs::remove_file(&path)?;
    /// # Ok(()) }
    /// ```
    pub fn compact(self) -> Result<CompactReport> {
        if self.is_in_memory() {
            return Err(Error::new(
                ErrorKind::InvalidOption,
                "a database in memory has no file to compact",
            ));
        }
        let staging = staging_path(&self.path, COMPACTION_STAGING_SUFFIX);
        let before = self.store.superblock().map_err(Error::storage)?;
        let checked = self
            .store
            .check()
            .map_err(|error| self.compaction_check_failure(error))?;
        let snapshot = self.store.begin_read().map_err(Error::storage)?;
        let destination = CopyDestination {
            path: &self.path,
            staging: &staging,
            replaces: true,
        };
        let report = snapshot
            .copy_to(
                &OsFileSystem::new(),
                &OsMemory::new(),
                Arc::clone(&self.clock.0),
                destination,
            )
            .map_err(|failure| self.compaction_failure(failure, destination))?;
        drop(snapshot);
        Ok(CompactReport {
            stamp: report.stamp,
            entries: report.entries,
            pages_before: before.page_count,
            free_pages_before: checked.free_pages,
            leaked_pages_before: checked.leaked_pages,
            pages_after: report.page_count,
            bytes_before: self.bytes_of(before.page_count),
            bytes_after: self.bytes_of(report.page_count),
        })
    }

    fn backup_failure(&self, failure: CopyFailure, destination: CopyDestination<'_>) -> Error {
        let file = self.path.display();
        let copy = destination.path.display();
        let staging = destination.staging.display();
        match failure {
            CopyFailure::NotCopied(error) if is_already_exists(&error) => {
                let message = if staging_exists(destination) {
                    format!(
                        "cannot back up {file} to {copy}: {staging} exists, left by a backup that did not finish or one still running. Remove it and back up again."
                    )
                } else {
                    format!(
                        "cannot back up {file} to {copy}: {copy} already exists, and a backup never replaces a file. Choose another name, or remove it first."
                    )
                };
                Error::storage(error).explained(message)
            }
            CopyFailure::NotCopied(error) => {
                let message = format!(
                    "cannot back up {file} to {copy}: {error}. Nothing is left behind: {copy} does not exist and {staging} was removed."
                );
                Error::storage(error).explained(message)
            }
            CopyFailure::NotRenamed(error) => {
                let message = format!(
                    "cannot rename {staging} to {copy}: {error}. The copy is complete and verified; rename it yourself."
                );
                Error::storage(error).explained(message)
            }
            CopyFailure::RenameNotDurable(error) => {
                let message = format!(
                    "backed up {file} to {copy}, but cannot sync its directory, so a power loss could still undo the rename: {error}"
                );
                Error::storage(error).explained(message)
            }
        }
    }

    fn compaction_check_failure(&self, error: StorageError) -> Error {
        let file = self.path.display();
        let message = format!("cannot compact {file}: {error}. {file} is unchanged.");
        Error::storage(error).explained(message)
    }

    fn compaction_failure(&self, failure: CopyFailure, destination: CopyDestination<'_>) -> Error {
        let file = self.path.display();
        let staging = destination.staging.display();
        let message = match &failure {
            CopyFailure::NotCopied(error) if is_already_exists(error) => format!(
                "cannot compact {file}: {staging} exists, left by a compaction that did not finish or one still running. Remove it and compact again."
            ),
            CopyFailure::NotCopied(error) => format!(
                "cannot compact {file}: {error}. {file} is unchanged and {staging} was removed."
            ),
            CopyFailure::NotRenamed(error) => format!(
                "cannot rename {staging} over {file}: {error}. {file} is unchanged; the compacted file is {staging}, complete and verified."
            ),
            CopyFailure::RenameNotDurable(error) => format!(
                "compacted {file}, but cannot sync its directory, so a power loss could still undo the rename: {error}"
            ),
        };
        Error::storage(failure.into_error()).explained(message)
    }

    fn bytes_of(&self, pages: u64) -> u64 {
        pages.saturating_mul(self.page_size.bytes() as u64)
    }

    /// The path the database was opened at.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the database was opened read-only.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// The database file's page size in bytes, fixed when the file was created.
    pub fn page_size(&self) -> usize {
        self.page_size.bytes()
    }

    /// The longest key this database stores, in bytes: an eighth of a page, 512 bytes with
    /// the default 4 KiB pages. A longer key fails with
    /// [`ErrorKind::TooLarge`](crate::ErrorKind::TooLarge).
    pub fn max_key_bytes(&self) -> usize {
        BTree::max_key_bytes(self.page_size)
    }

    /// The longest value this database stores, in bytes: 1 GiB, whatever the page size. A
    /// longer one fails with [`ErrorKind::TooLarge`](crate::ErrorKind::TooLarge).
    ///
    /// A value is read and written whole, so it takes its size in memory while it is used,
    /// and twice that when a [`Scan`](crate::Scan) returns it as an owned item. A value that
    /// does not fit in a B+tree leaf beside its key (750 bytes for the key and value together
    /// with the default 4 KiB pages) goes to overflow pages, which a write transaction keeps
    /// in the page cache until it commits, so the cache bounds a value too (see
    /// [`Options::cache_bytes`]).
    pub fn max_value_bytes(&self) -> usize {
        BTree::MAX_VALUE_BYTES
    }

    pub(crate) fn store(&self) -> &Store<DatabaseFile> {
        &self.store
    }

    pub(crate) fn tree_at(
        &self,
        root: u64,
        open: impl FnOnce(u64) -> std::result::Result<BTree, StorageError>,
    ) -> Result<Option<BTree>> {
        if root == NO_PAGE {
            return Ok(None);
        }
        if let Some(tree) = self.tree.get().filter(|tree| tree.root() == root) {
            return Ok(Some(*tree));
        }
        let tree = open(root).map_err(Error::storage)?;
        self.remember_tree(tree);
        Ok(Some(tree))
    }

    pub(crate) fn remember_tree(&self, tree: BTree) {
        let _ = self.tree.set(tree);
    }

    pub(crate) fn has_expiring_keys(&self) -> bool {
        self.expiring_keys.load(Ordering::Acquire)
    }

    pub(crate) fn note_expiring_keys(&self) {
        self.expiring_keys.store(true, Ordering::Release);
    }

    pub(crate) fn collection_drops(&self) -> u8 {
        self.collection_drops.load(Ordering::Acquire)
    }

    pub(crate) fn note_collection_drops(&self, state: u8) {
        self.collection_drops.store(state, Ordering::Release);
    }

    pub(crate) fn now_after(&self, last_commit: CommitStamp) -> Moment {
        let wall = self.clock.0.unix_time_nanos() / NANOS_PER_MICRO;
        let committed = if last_commit == CommitStamp::NONE {
            0
        } else {
            last_commit.unix_micros()
        };
        Moment::from_unix_micros(wall.max(committed))
    }
}

fn open_waiting_while_busy(
    path: &Path,
    options: &Options,
    page_size: PageSize,
) -> Result<PageFile<DatabaseFile>> {
    let open = || {
        open_or_create(path, options, page_size).map_err(|error| explain_open_error(error, options))
    };
    let first = open();
    let busy = matches!(&first, Err(error) if error.kind() == ErrorKind::Locked);
    if !busy || options.busy_timeout.is_zero() {
        return first;
    }
    let give_up = std::time::Instant::now().checked_add(options.busy_timeout);
    let mut pause = FIRST_BUSY_PAUSE;
    loop {
        let remaining = match give_up {
            Some(give_up) => give_up.saturating_duration_since(std::time::Instant::now()),
            None => LONGEST_BUSY_PAUSE,
        };
        if remaining.is_zero() {
            return open();
        }
        std::thread::sleep(pause.min(remaining));
        pause = pause.saturating_mul(2).min(LONGEST_BUSY_PAUSE);
        match open() {
            Err(error) if error.kind() == ErrorKind::Locked => {}
            opened => return opened,
        }
    }
}

const REOPENS_AFTER_REPLACEMENT: usize = 8;

fn open_or_create(
    path: &Path,
    options: &Options,
    page_size: PageSize,
) -> std::result::Result<PageFile<DatabaseFile>, StorageError> {
    let fs = OsFileSystem::new();
    let access = if options.read_only {
        Access::ReadOnly
    } else {
        Access::ReadWrite
    };
    let open = || -> std::result::Result<PageFile<DatabaseFile>, StorageError> {
        let mode = match access {
            Access::ReadOnly => OpenMode::Read,
            Access::ReadWrite => OpenMode::ReadWrite,
        };
        for _ in 0..REOPENS_AFTER_REPLACEMENT {
            let file = fs.open(path, mode)?;
            let opened = file.identity().ok();
            let page_file = PageFile::open_in(DatabaseFile::Os(file), access)?;
            let still_there = match opened {
                Some(opened) => eggdb_platform::file_identity_at(path).ok() == Some(opened),
                None => true,
            };
            if still_there {
                return Ok(page_file);
            }
        }
        Err(StorageError::Locked)
    };
    let opened = open();
    if !(options.creates_if_missing() && is_kind(&opened, eggdb_platform::ErrorKind::NotFound)) {
        return opened;
    }
    let created = fs
        .open(path, OpenMode::CreateNew)
        .map_err(StorageError::from)
        .and_then(|file| {
            PageFile::create_in(
                DatabaseFile::Os(file),
                CreateOptions {
                    page_size,
                    file_id: FileId::random(),
                },
            )
        });
    if is_kind(&created, eggdb_platform::ErrorKind::AlreadyExists) {
        return open();
    }
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let created = created?;
    fs.sync_dir(directory)?;
    Ok(created)
}

const MEMORY_NAME: &str = ":memory:";

fn explain_open_error(error: StorageError, options: &Options) -> Error {
    let missing =
        matches!(&error, StorageError::Io(io) if io.kind() == eggdb_platform::ErrorKind::NotFound);
    let error = Error::storage(error);
    if !missing {
        return error;
    }
    error.explained(if options.creates_if_missing() {
        "its directory does not exist, so the file can't be created there"
    } else if options.read_only {
        "the file does not exist, and a read-only open never creates one"
    } else {
        "the file does not exist; open it with `Options::create_if_missing(true)` to create it"
    })
}

fn staging_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

fn is_already_exists(error: &StorageError) -> bool {
    matches!(error, StorageError::Io(io) if io.kind() == eggdb_platform::ErrorKind::AlreadyExists)
}

fn staging_exists(destination: CopyDestination<'_>) -> bool {
    OsFileSystem::new()
        .exists(destination.staging)
        .unwrap_or(false)
}

fn is_kind<T>(
    result: &std::result::Result<T, StorageError>,
    kind: eggdb_platform::ErrorKind,
) -> bool {
    matches!(result, Err(StorageError::Io(error)) if error.kind() == kind)
}
