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

    fn store_id(&self) -> usize {
        (**self).store_id()
    }

    fn page_lookups(&self) -> Option<PageLookups> {
        (**self).page_lookups()
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

    fn store_id(&self) -> usize {
        (**self).store_id()
    }

    fn page_lookups(&self) -> Option<PageLookups> {
        (**self).page_lookups()
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

    fn committed_version(&self) -> Option<u64> {
        (**self).committed_version()
    }
}

pub(crate) trait ErasedStore {
    fn begin_boxed_read(&self) -> SqlResult<Box<dyn KeyValueRead + '_>>;

    fn begin_boxed_write(&self) -> SqlResult<Box<dyn BoxedWrite + '_>>;

    fn erased_page_lookups(&self) -> Option<PageLookups>;
}

impl<S: KeyValueStore> ErasedStore for S {
    fn begin_boxed_read(&self) -> SqlResult<Box<dyn KeyValueRead + '_>> {
        Ok(Box::new(self.begin_read()?))
    }

    fn begin_boxed_write(&self) -> SqlResult<Box<dyn BoxedWrite + '_>> {
        Ok(Box::new(self.begin_write()?))
    }

    fn erased_page_lookups(&self) -> Option<PageLookups> {
        self.page_lookups()
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
