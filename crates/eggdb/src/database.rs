use crate::error::{Error, Result};
use crate::expiry::Moment;
use crate::options::Options;
use crate::transaction::{ReadTransaction, WriteTransaction};
use crate::writer_slot::WriterSlot;
use eggdb_platform::{Clock, FileSystem, OsFile, OsFileSystem, OsMemory};
use eggdb_storage::{
    Access, BTree, CacheSize, CommitStamp, CopyDestination, CopyFailure, CreateOptions, Features,
    FileId, NO_PAGE, PageCache, PageFile, PageSize, StorageError, Store,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

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
    store: Store<OsFile>,
    tree: OnceLock<BTree>,
    writer: WriterSlot,
    path: PathBuf,
    page_size: PageSize,
    read_only: bool,
    write_timeout: Duration,
    clock: ClockHandle,
    expiring_keys: AtomicBool,
}

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

    fn open_file(path: &Path, options: &Options) -> Result<Self> {
        let new_file_page_size = options.new_file_page_size()?;
        options.check_cache_holds(new_file_page_size)?;
        let file = open_or_create(path, options, new_file_page_size)
            .map_err(|error| explain_open_error(error, options))?;
        let page_size = file.page_size();
        options.check_cache_holds(page_size)?;
        let mut cache = PageCache::new(
            file,
            &OsMemory::new(),
            CacheSize::Bytes(options.cache_bytes),
        )
        .map_err(Error::storage)?;
        if options.lock_cache {
            cache.lock_in_memory().map_err(Error::storage)?;
        }
        let clock = options.clock.clock();
        let store = Store::new(cache, Arc::clone(&clock));
        let features = store.superblock().map_err(Error::storage)?.features;
        let expiring_keys = features.read_only_compatible & Features::EXPIRING_KEYS != 0;
        let configured = store
            .set_durability(options.durability.to_storage())
            .and_then(|()| store.set_auto_checkpoint_bytes(options.auto_checkpoint_log_bytes))
            .and_then(|()| store.set_snapshot_memory(CacheSize::Bytes(options.snapshot_memory())));
        configured.map_err(Error::storage)?;
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
        })
    }

    /// Begins a read transaction, which sees the last commit before it began and nothing
    /// after it (see [`ReadTransaction`]).
    pub fn begin_read(&self) -> Result<ReadTransaction<'_>> {
        ReadTransaction::begin(self)
    }

    /// Begins the write transaction (see [`WriteTransaction`]). While another one is open, it
    /// waits for that one to end, up to [`Options::write_timeout`] (five seconds by default),
    /// and then fails with [`ErrorKind::WriterActive`](crate::ErrorKind::WriterActive). It
    /// fails at once when this thread began the open write transaction, since waiting could
    /// never end; that holds even after the transaction was sent to another thread, so the
    /// beginning thread stays refused until it ends. On a read-only database it fails with
    /// [`ErrorKind::ReadOnly`](crate::ErrorKind::ReadOnly).
    pub fn begin_write(&self) -> Result<WriteTransaction<'_>> {
        self.begin_write_within(self.write_timeout)
    }

    /// Begins the write transaction, or fails at once with
    /// [`ErrorKind::WriterActive`](crate::ErrorKind::WriterActive) while another one is open.
    pub fn try_begin_write(&self) -> Result<WriteTransaction<'_>> {
        self.begin_write_within(Duration::ZERO)
    }

    fn begin_write_within(&self, timeout: Duration) -> Result<WriteTransaction<'_>> {
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
        let copy = path.as_ref();
        let staging = staging_path(copy, BACKUP_STAGING_SUFFIX);
        let snapshot = self.store.begin_read().map_err(Error::storage)?;
        let destination = CopyDestination {
            path: copy,
            staging: &staging,
            replaces: false,
        };
        let report = snapshot
            .copy_to(
                &OsFileSystem::new(),
                &OsMemory::new(),
                Arc::clone(&self.clock.0),
                destination,
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

    pub(crate) fn store(&self) -> &Store<OsFile> {
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

fn open_or_create(
    path: &Path,
    options: &Options,
    page_size: PageSize,
) -> std::result::Result<PageFile<OsFile>, StorageError> {
    let fs = OsFileSystem::new();
    let access = if options.read_only {
        Access::ReadOnly
    } else {
        Access::ReadWrite
    };
    let opened = PageFile::open(&fs, path, access);
    if !(options.creates_if_missing() && is_kind(&opened, eggdb_platform::ErrorKind::NotFound)) {
        return opened;
    }
    let created = PageFile::create(
        &fs,
        path,
        CreateOptions {
            page_size,
            file_id: FileId::random(),
        },
    );
    if is_kind(&created, eggdb_platform::ErrorKind::AlreadyExists) {
        return PageFile::open(&fs, path, access);
    }
    created
}

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
