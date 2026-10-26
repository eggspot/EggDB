use crate::error::{Error, Result};
use crate::options::Options;
use crate::transaction::{ReadTransaction, WriteTransaction};
use crate::writer_slot::WriterSlot;
use eggdb_platform::{OsFile, OsFileSystem, OsMemory, SystemClock};
use eggdb_storage::{
    Access, BTree, CacheSize, CommitStamp, CreateOptions, FileId, NO_PAGE, PageCache, PageFile,
    PageSize, StorageError, Store,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

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
    /// The file's size as of the last commit: `page_count × page_size`.
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
        let cache = PageCache::new(
            file,
            &OsMemory::new(),
            CacheSize::Bytes(options.cache_bytes),
        )
        .map_err(Error::storage)?;
        let store = Store::new(cache, Arc::new(SystemClock::new()));
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

fn is_kind<T>(
    result: &std::result::Result<T, StorageError>,
    kind: eggdb_platform::ErrorKind,
) -> bool {
    matches!(result, Err(StorageError::Io(error)) if error.kind() == kind)
}
