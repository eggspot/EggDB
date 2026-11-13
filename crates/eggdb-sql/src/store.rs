use crate::error::{SqlError, SqlResult, SqlState};
use std::time::Duration;

/// Which way a [`KeyValueRead::scan`] walks its range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanDirection {
    /// From the smallest key up.
    Forward,
    /// From the largest key down.
    Backward,
}

/// Why the key-value store under SQL failed, so SQL can name the failure with a SQLSTATE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreFailure {
    /// Another writer holds the database (`55P03`).
    Busy,
    /// The database was opened read-only (`25006`).
    ReadOnly,
    /// A key or value is larger than the store keeps, or a transaction outgrew its memory
    /// (`54000`).
    TooLarge,
    /// The file is damaged (`XX001`).
    Corrupt,
    /// Reading, writing or syncing the file failed (`58030`).
    Io,
    /// Memory could not be had (`53200`).
    OutOfMemory,
    /// Anything else (`XX000`).
    Other,
}

impl SqlError {
    /// A failure of the key-value store, with its message, as a SQL error.
    pub fn store(failure: StoreFailure, message: impl Into<String>) -> Self {
        let state = match failure {
            StoreFailure::Busy => SqlState::LOCK_NOT_AVAILABLE,
            StoreFailure::ReadOnly => SqlState::READ_ONLY_SQL_TRANSACTION,
            StoreFailure::TooLarge => SqlState::PROGRAM_LIMIT_EXCEEDED,
            StoreFailure::Corrupt => SqlState::DATA_CORRUPTED,
            StoreFailure::Io => SqlState::IO_ERROR,
            StoreFailure::OutOfMemory => SqlState::OUT_OF_MEMORY,
            StoreFailure::Other => SqlState::INTERNAL_ERROR,
        };
        Self::new(state, message)
    }
}

fn no_key_value_writes() -> SqlError {
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "this store keeps no times to live or collections",
    )
}

/// What a [`KeyValueRead::scan`] calls with each entry, key then value; it returns whether to
/// go on.
pub type EntryVisitor<'v> = dyn FnMut(&[u8], &[u8]) -> SqlResult<bool> + 'v;

/// Reads of an ordered byte-string key-value store: a snapshot, or the writer's own view.
pub trait KeyValueRead {
    /// The value under `key`, or `None`.
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>>;

    /// Calls `read` with the value under `key` where it lies, when there is one; whether
    /// there was. The default copies the value out with [`KeyValueRead::get`].
    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        match self.get(key)? {
            Some(value) => read(&value).map(|()| true),
            None => Ok(false),
        }
    }

    /// Calls `visit` with each entry whose key is at least `start` and below `end`, in
    /// `direction`, until it returns `Ok(false)` or an error.
    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()>;

    /// The version of the committed state this read sees, when it sees only committed
    /// state: two reads of a store with the same version see the same entries. `None`, the
    /// default, when it may also see changes not yet committed, as the writer does, or when
    /// the store does not tell; SQL then checks what it cached against the store.
    fn committed_version(&self) -> Option<u64> {
        None
    }

    /// A handle that [`KeyValueStore::begin_read_from`] opens another read on this snapshot
    /// with, for PostgreSQL's `pg_export_snapshot()`; `None`, the default, when the store
    /// cannot share snapshots or this read is not a snapshot.
    fn snapshot_handle(&self) -> Option<SnapshotHandle> {
        None
    }

    /// The names of the store's collections, in byte order: named key spaces of their own
    /// beside the one this trait reads, which SQL never uses (D139). A store without
    /// collections, the default, has none.
    fn collection_names(&self) -> SqlResult<Vec<String>> {
        Ok(Vec::new())
    }

    /// Whether the store has a collection named `name`; by default it has none.
    fn has_collection(&self, _name: &str) -> SqlResult<bool> {
        Ok(false)
    }

    /// The value under `key` in the collection named `name`, or `None` when the key or the
    /// collection is absent.
    fn collection_get(&self, _name: &str, _key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        Ok(None)
    }

    /// Calls `visit` with each entry of the collection named `name` whose key is at least
    /// `start` and below `end`, as [`KeyValueRead::scan`] does; with none when there is no
    /// such collection.
    fn collection_scan(
        &self,
        _name: &str,
        _start: &[u8],
        _end: &[u8],
        _direction: ScanDirection,
        _visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        Ok(())
    }
}

/// The one write transaction of an ordered byte-string key-value store.
pub trait KeyValueWrite: KeyValueRead {
    /// Stores `value` under `key`, replacing any value there.
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()>;

    /// Stores `value` under `key` only when `key` is absent; whether it stored it. A key that
    /// is there keeps its value. The default reads the key and then puts it; a store that can
    /// do both in one search should.
    fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> SqlResult<bool> {
        if self.get(key)?.is_some() {
            return Ok(false);
        }
        self.put(key, value)?;
        Ok(true)
    }

    /// Removes `key`; whether it was there.
    fn delete(&mut self, key: &[u8]) -> SqlResult<bool>;

    /// Keeps every key that starts with `prefix` in an LSM tree of its own from now on, for a
    /// table created `WITH (storage = lsm)` (D1119); `false` when it already is. The keys read,
    /// scan and write as before. A store without LSM trees, the default, refuses with `0A000`.
    fn store_prefix_in_lsm_tree(&mut self, _prefix: &[u8]) -> SqlResult<bool> {
        Err(SqlError::not_supported("tables stored in an LSM tree on this store"))
    }

    /// Stores `value` under `key` with a time to live of `ttl` from now, replacing any value
    /// and time to live there, for the key-value tools (D328); SQL never calls it. A store
    /// without times to live, the default, refuses with `0A000`.
    fn put_with_expiry(&mut self, _key: &[u8], _value: &[u8], _ttl: Duration) -> SqlResult<()> {
        Err(no_key_value_writes())
    }

    /// Stores `value` under `key` in the collection named `name` (D139), with a time to live
    /// of `ttl` when there is one, replacing any value and time to live there; `false`, with
    /// nothing stored, when there is no such collection. A store without collections, the
    /// default, refuses with `0A000`.
    fn collection_put(
        &mut self,
        _name: &str,
        _key: &[u8],
        _value: &[u8],
        _ttl: Option<Duration>,
    ) -> SqlResult<bool> {
        Err(no_key_value_writes())
    }

    /// Removes `key` from the collection named `name`: `None` when there is no such
    /// collection, otherwise whether the key was there. A store without collections, the
    /// default, refuses with `0A000`.
    fn collection_delete(&mut self, _name: &str, _key: &[u8]) -> SqlResult<Option<bool>> {
        Err(no_key_value_writes())
    }

    /// Adds `by` to the counter under `key`, in the collection named `collection` when there is
    /// one, and returns the new count; a negative `by` subtracts. A counter is a decimal integer
    /// stored as text, from `i64::MIN` to `i64::MAX`, an absent key counts as 0, and the key
    /// keeps its time to live. `None`, with nothing changed, when there is no such collection.
    /// For the key-value tools (D346); SQL never calls it. A store without counters, the
    /// default, refuses with `0A000`.
    fn increment(
        &mut self,
        _collection: Option<&str>,
        _key: &[u8],
        _by: i64,
    ) -> SqlResult<Option<i64>> {
        Err(no_key_value_writes())
    }

    /// Stores `new` under `key`, or removes the key when `new` is `None`, only while its value
    /// is `expected` (`None`: the key is absent), in the collection named `collection` when
    /// there is one. Returns whether it swapped and the value that was there before, or `None`,
    /// with nothing changed, when there is no such collection. A value stored this way has no
    /// time to live. For the key-value tools (D346); SQL never calls it. A store without
    /// compare-and-set, the default, refuses with `0A000`.
    fn compare_and_set(
        &mut self,
        _collection: Option<&str>,
        _key: &[u8],
        _expected: Option<&[u8]>,
        _new: Option<&[u8]>,
    ) -> SqlResult<Option<(bool, Option<Vec<u8>>)>> {
        Err(no_key_value_writes())
    }

    /// Gives `key`, in the collection named `collection` when there is one, a time to live of
    /// `ttl` from now, replacing any it had; a `ttl` of zero removes the key. Returns whether
    /// the key was there (an absent key stays absent), or `None`, with nothing changed, when
    /// there is no such collection. For the key-value tools (D346); SQL never calls it. A store
    /// without times to live, the default, refuses with `0A000`.
    fn set_expiry(
        &mut self,
        _collection: Option<&str>,
        _key: &[u8],
        _ttl: Duration,
    ) -> SqlResult<Option<bool>> {
        Err(no_key_value_writes())
    }

    /// Removes the time to live of `key`, in the collection named `collection` when there is
    /// one, so it stays until it is deleted; a key without one is left as it is. Returns
    /// whether the key is there, or `None` when there is no such collection. For the key-value
    /// tools (D346); SQL never calls it. A store without times to live, the default, refuses
    /// with `0A000`.
    fn clear_expiry(&mut self, _collection: Option<&str>, _key: &[u8]) -> SqlResult<Option<bool>> {
        Err(no_key_value_writes())
    }

    /// Puts `key` back as it was before a write made outside SQL in the same transaction:
    /// holding `previous`, or absent when it is `None`. `key` and `previous` are what the store
    /// itself keeps, which can include keys [`put`](Self::put) refuses, such as the ones a
    /// store keeps for times to live; a store with such keys writes them directly. The default
    /// puts or deletes.
    fn restore(&mut self, key: &[u8], previous: Option<&[u8]>) -> SqlResult<()> {
        match previous {
            Some(value) => self.put(key, value),
            None => self.delete(key).map(|_| ()),
        }
    }

    /// Makes every change durable and visible, atomically.
    fn commit(self) -> SqlResult<()>
    where
        Self: Sized;

    /// Discards every change.
    fn rollback(self) -> SqlResult<()>
    where
        Self: Sized;
}

/// A snapshot of a store that another session may open while a read of it is still open,
/// as [`KeyValueRead::snapshot_handle`] gives it; what it holds is the store's own.
#[derive(Clone)]
pub struct SnapshotHandle(std::sync::Arc<dyn std::any::Any + Send + Sync>);

impl SnapshotHandle {
    /// A handle holding `token`, which only the store that made it reads.
    pub fn new<T: std::any::Any + Send + Sync>(token: T) -> Self {
        Self(std::sync::Arc::new(token))
    }

    /// The token, when it is a `T`.
    pub fn token<T: std::any::Any>(&self) -> Option<&T> {
        self.0.downcast_ref::<T>()
    }
}

impl std::fmt::Debug for SnapshotHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SnapshotHandle")
    }
}

/// How a transaction from [`KeyValueStore::begin_concurrent_write`] is checked when it commits
/// (D1123).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConcurrentIsolation {
    /// The commit fails when a key it writes changed since it began: first committer wins.
    Snapshot,
    /// The commit fails when anything it read changed since it began.
    Serializable,
}

/// A write transaction that runs beside other writers and is checked when it commits, from
/// [`KeyValueStore::begin_concurrent_write`] (D1123).
pub trait ConcurrentWrite: KeyValueWrite {
    /// Checks the transaction against the commits made since it began and commits it; a failed
    /// check answers [`SqlState::SERIALIZATION_FAILURE`](crate::SqlState::SERIALIZATION_FAILURE)
    /// and commits nothing.
    fn commit_concurrent(self: Box<Self>) -> SqlResult<()>;

    /// Discards every change.
    fn rollback_concurrent(self: Box<Self>) -> SqlResult<()>;
}

/// An ordered byte-string key-value store that SQL keeps its catalog and tables in: one writer
/// and snapshot readers.
pub trait KeyValueStore {
    /// A read-only snapshot.
    type Read<'s>: KeyValueRead
    where
        Self: 's;
    /// The write transaction.
    type Write<'s>: KeyValueWrite
    where
        Self: 's;

    /// Begins a snapshot of the last commit.
    fn begin_read(&self) -> SqlResult<Self::Read<'_>>;

    /// Begins the write transaction, waiting for another writer as the store does.
    fn begin_write(&self) -> SqlResult<Self::Write<'_>>;

    /// Begins a write transaction that runs beside the store's other writers and is checked
    /// when it commits, for an explicit transaction block at `REPEATABLE READ` or `SERIALIZABLE`
    /// (D1123). `None`, the default, when the store has only its one writer.
    fn begin_concurrent_write(
        &self,
        isolation: ConcurrentIsolation,
    ) -> SqlResult<Option<Box<dyn ConcurrentWrite + '_>>> {
        let _ = isolation;
        Ok(None)
    }

    /// Begins the write transaction, waiting at most `limit` for another writer, for
    /// PostgreSQL's `lock_timeout`. By default it waits as [`KeyValueStore::begin_write`] does.
    fn begin_write_waiting(&self, limit: std::time::Duration) -> SqlResult<Self::Write<'_>> {
        let _ = limit;
        self.begin_write()
    }

    /// Begins a read of the snapshot `handle` names, while a read of it is still open; `None`
    /// when the handle is not this store's or the store cannot share snapshots, the default.
    fn begin_read_from(&self, handle: &SnapshotHandle) -> SqlResult<Option<Self::Read<'_>>> {
        let _ = handle;
        Ok(None)
    }

    /// A number naming this store among the stores of the process, the same through every
    /// handle on it (a reference, an [`Arc`](std::sync::Arc)): sessions on one store share its
    /// advisory locks. By default the store's address.
    fn store_id(&self) -> usize {
        std::ptr::from_ref(self).addr()
    }

    /// The page lookups the calling thread has made so far, if the store counts them: the
    /// difference between two readings is what `EXPLAIN (ANALYZE, BUFFERS)` reports. By
    /// default the store does not count them.
    fn page_lookups(&self) -> Option<PageLookups> {
        None
    }

    /// Writes a copy of the store's last commit to the new file `path` for SQL's `BACKUP`
    /// (D1023): a copy-on-write clone where the file system can when `clone`. `None` when the
    /// store keeps no file to copy, the default.
    fn backup_to(&self, path: &std::path::Path, clone: bool) -> SqlResult<Option<StoreBackup>> {
        let _ = (path, clone);
        Ok(None)
    }
}

/// What [`KeyValueStore::backup_to`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StoreBackup {
    /// The commit stamp the copy holds.
    pub stamp: u64,
    /// The entries copied; 0 for a clone, which copies no entries.
    pub entries: u64,
    /// Whether the copy is a copy-on-write clone.
    pub instant: bool,
}

/// Page lookups a store's page cache answered from memory and from its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PageLookups {
    /// Lookups answered from memory: PostgreSQL's `shared hit`.
    pub hits: u64,
    /// Lookups that read the page from the file: PostgreSQL's `read`.
    pub reads: u64,
}

impl PageLookups {
    /// What was looked up since `earlier`, a reading taken before this one on the same thread.
    pub fn since(self, earlier: Self) -> Self {
        Self {
            hits: self.hits.saturating_sub(earlier.hits),
            reads: self.reads.saturating_sub(earlier.reads),
        }
    }
}

impl<S: KeyValueStore> KeyValueStore for &S {
    type Read<'s>
        = S::Read<'s>
    where
        Self: 's;
    type Write<'s>
        = S::Write<'s>
    where
        Self: 's;

    fn begin_read(&self) -> SqlResult<S::Read<'_>> {
        (**self).begin_read()
    }

    fn begin_write(&self) -> SqlResult<S::Write<'_>> {
        (**self).begin_write()
    }

    fn begin_concurrent_write(
        &self,
        isolation: ConcurrentIsolation,
    ) -> SqlResult<Option<Box<dyn ConcurrentWrite + '_>>> {
        (**self).begin_concurrent_write(isolation)
    }

    fn begin_write_waiting(&self, limit: std::time::Duration) -> SqlResult<S::Write<'_>> {
        (**self).begin_write_waiting(limit)
    }

    fn begin_read_from(&self, handle: &SnapshotHandle) -> SqlResult<Option<S::Read<'_>>> {
        (**self).begin_read_from(handle)
    }

    fn store_id(&self) -> usize {
        (**self).store_id()
    }

    fn page_lookups(&self) -> Option<PageLookups> {
        (**self).page_lookups()
    }

    fn backup_to(&self, path: &std::path::Path, clone: bool) -> SqlResult<Option<StoreBackup>> {
        (**self).backup_to(path, clone)
    }
}

impl<S: KeyValueStore> KeyValueStore for std::sync::Arc<S> {
    type Read<'s>
        = S::Read<'s>
    where
        Self: 's;
    type Write<'s>
        = S::Write<'s>
    where
        Self: 's;

    fn begin_read(&self) -> SqlResult<S::Read<'_>> {
        (**self).begin_read()
    }

    fn begin_write(&self) -> SqlResult<S::Write<'_>> {
        (**self).begin_write()
    }

    fn begin_concurrent_write(
        &self,
        isolation: ConcurrentIsolation,
    ) -> SqlResult<Option<Box<dyn ConcurrentWrite + '_>>> {
        (**self).begin_concurrent_write(isolation)
    }

    fn begin_write_waiting(&self, limit: std::time::Duration) -> SqlResult<S::Write<'_>> {
        (**self).begin_write_waiting(limit)
    }

    fn begin_read_from(&self, handle: &SnapshotHandle) -> SqlResult<Option<S::Read<'_>>> {
        (**self).begin_read_from(handle)
    }

    fn store_id(&self) -> usize {
        (**self).store_id()
    }

    fn page_lookups(&self) -> Option<PageLookups> {
        (**self).page_lookups()
    }

    fn backup_to(&self, path: &std::path::Path, clone: bool) -> SqlResult<Option<StoreBackup>> {
        (**self).backup_to(path, clone)
    }
}

pub(crate) trait BoxedWrite: KeyValueWrite {
    fn commit_boxed(self: Box<Self>) -> SqlResult<()>;

    fn rollback_boxed(self: Box<Self>) -> SqlResult<()>;
}

impl<W: KeyValueWrite> BoxedWrite for W {
    fn commit_boxed(self: Box<Self>) -> SqlResult<()> {
        (*self).commit()
    }

    fn rollback_boxed(self: Box<Self>) -> SqlResult<()> {
        (*self).rollback()
    }
}

impl KeyValueRead for Box<dyn BoxedWrite + '_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        (**self).get(key)
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        (**self).get_with(key, read)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        (**self).scan(start, end, direction, visit)
    }

    fn committed_version(&self) -> Option<u64> {
        (**self).committed_version()
    }
}

impl KeyValueWrite for Box<dyn BoxedWrite + '_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        (**self).put(key, value)
    }

    fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> SqlResult<bool> {
        (**self).put_if_absent(key, value)
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        (**self).delete(key)
    }

    fn put_with_expiry(&mut self, key: &[u8], value: &[u8], ttl: Duration) -> SqlResult<()> {
        (**self).put_with_expiry(key, value, ttl)
    }

    fn collection_put(
        &mut self,
        name: &str,
        key: &[u8],
        value: &[u8],
        ttl: Option<Duration>,
    ) -> SqlResult<bool> {
        (**self).collection_put(name, key, value, ttl)
    }

    fn collection_delete(&mut self, name: &str, key: &[u8]) -> SqlResult<Option<bool>> {
        (**self).collection_delete(name, key)
    }

    fn increment(
        &mut self,
        collection: Option<&str>,
        key: &[u8],
        by: i64,
    ) -> SqlResult<Option<i64>> {
        (**self).increment(collection, key, by)
    }

    fn compare_and_set(
        &mut self,
        collection: Option<&str>,
        key: &[u8],
        expected: Option<&[u8]>,
        new: Option<&[u8]>,
    ) -> SqlResult<Option<(bool, Option<Vec<u8>>)>> {
        (**self).compare_and_set(collection, key, expected, new)
    }

    fn set_expiry(
        &mut self,
        collection: Option<&str>,
        key: &[u8],
        ttl: Duration,
    ) -> SqlResult<Option<bool>> {
        (**self).set_expiry(collection, key, ttl)
    }

    fn clear_expiry(&mut self, collection: Option<&str>, key: &[u8]) -> SqlResult<Option<bool>> {
        (**self).clear_expiry(collection, key)
    }

    fn restore(&mut self, key: &[u8], previous: Option<&[u8]>) -> SqlResult<()> {
        (**self).restore(key, previous)
    }

    fn commit(self) -> SqlResult<()> {
        self.commit_boxed()
    }

    fn rollback(self) -> SqlResult<()> {
        self.rollback_boxed()
    }
}

impl KeyValueRead for Box<dyn KeyValueRead + '_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        (**self).get(key)
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        (**self).get_with(key, read)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        (**self).scan(start, end, direction, visit)
    }

    fn snapshot_handle(&self) -> Option<SnapshotHandle> {
        (**self).snapshot_handle()
    }

    fn committed_version(&self) -> Option<u64> {
        (**self).committed_version()
    }
}

fn lock_timeout() -> Option<std::time::Duration> {
    let value = crate::pg_catalog::statement_settings()
        .get("lock_timeout")
        .ok()?;
    let millis = crate::session::milliseconds_setting(&value)?;
    (millis > 0).then(|| std::time::Duration::from_millis(millis.unsigned_abs()))
}

pub(crate) struct ConcurrentBoxed<'s>(Box<dyn ConcurrentWrite + 's>);

impl KeyValueRead for ConcurrentBoxed<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        self.0.get(key)
    }

    fn get_with(&self, key: &[u8], read: &mut dyn FnMut(&[u8]) -> SqlResult<()>) -> SqlResult<bool> {
        self.0.get_with(key, read)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        self.0.scan(start, end, direction, visit)
    }
}

impl KeyValueWrite for ConcurrentBoxed<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        self.0.put(key, value)
    }

    fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> SqlResult<bool> {
        self.0.put_if_absent(key, value)
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        self.0.delete(key)
    }

    fn commit(self) -> SqlResult<()> {
        self.0.commit_concurrent()
    }

    fn rollback(self) -> SqlResult<()> {
        self.0.rollback_concurrent()
    }
}

pub(crate) trait ErasedStore {
    fn begin_boxed_read(&self) -> SqlResult<Box<dyn KeyValueRead + '_>>;

    fn begin_boxed_concurrent_write(
        &self,
        isolation: ConcurrentIsolation,
    ) -> SqlResult<Option<Box<dyn BoxedWrite + '_>>>;

    fn begin_boxed_write(&self) -> SqlResult<Box<dyn BoxedWrite + '_>>;

    fn erased_page_lookups(&self) -> Option<PageLookups>;

    fn begin_boxed_read_from(
        &self,
        handle: &SnapshotHandle,
    ) -> SqlResult<Option<Box<dyn KeyValueRead + '_>>>;

    fn erased_store_id(&self) -> usize;

    fn erased_backup_to(&self, path: &std::path::Path, clone: bool) -> SqlResult<Option<StoreBackup>>;
}

impl<S: KeyValueStore> ErasedStore for S {
    fn begin_boxed_concurrent_write(
        &self,
        isolation: ConcurrentIsolation,
    ) -> SqlResult<Option<Box<dyn BoxedWrite + '_>>> {
        Ok(self
            .begin_concurrent_write(isolation)?
            .map(|writer| Box::new(ConcurrentBoxed(writer)) as Box<dyn BoxedWrite + '_>))
    }

    fn erased_backup_to(&self, path: &std::path::Path, clone: bool) -> SqlResult<Option<StoreBackup>> {
        self.backup_to(path, clone)
    }

    fn begin_boxed_read(&self) -> SqlResult<Box<dyn KeyValueRead + '_>> {
        Ok(Box::new(self.begin_read()?))
    }

    fn begin_boxed_write(&self) -> SqlResult<Box<dyn BoxedWrite + '_>> {
        match lock_timeout() {
            Some(limit) => match self.begin_write_waiting(limit) {
                Ok(writer) => Ok(Box::new(writer)),
                Err(error) if error.state() == crate::error::SqlState::LOCK_NOT_AVAILABLE => {
                    Err(crate::error::SqlError::new(
                        crate::error::SqlState::LOCK_NOT_AVAILABLE,
                        "canceling statement due to lock timeout",
                    ))
                }
                Err(error) => Err(error),
            },
            None => Ok(Box::new(self.begin_write()?)),
        }
    }

    fn erased_page_lookups(&self) -> Option<PageLookups> {
        self.page_lookups()
    }

    fn begin_boxed_read_from(
        &self,
        handle: &SnapshotHandle,
    ) -> SqlResult<Option<Box<dyn KeyValueRead + '_>>> {
        Ok(self
            .begin_read_from(handle)?
            .map(|read| Box::new(read) as Box<dyn KeyValueRead + '_>))
    }

    fn erased_store_id(&self) -> usize {
        self.store_id()
    }
}

const DELETE_BATCH: usize = 1024;

pub(crate) fn delete_prefix(writer: &mut dyn KeyValueWrite, prefix: &[u8]) -> SqlResult<()> {
    let end = crate::encoding::prefix_end(prefix);
    loop {
        let mut keys = Vec::new();
        writer.scan(prefix, &end, ScanDirection::Forward, &mut |key, _| {
            keys.push(key.to_vec());
            Ok(keys.len() < DELETE_BATCH)
        })?;
        if keys.is_empty() {
            return Ok(());
        }
        for key in &keys {
            writer.delete(key)?;
        }
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
