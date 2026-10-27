use crate::error::{SqlError, SqlResult, SqlState};

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

/// What a [`KeyValueRead::scan`] calls with each entry, key then value; it returns whether to
/// go on.
pub type EntryVisitor<'v> = dyn FnMut(&[u8], &[u8]) -> SqlResult<bool> + 'v;

/// Reads of an ordered byte-string key-value store: a snapshot, or the writer's own view.
pub trait KeyValueRead {
    /// The value under `key`, or `None`.
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>>;

    /// Calls `visit` with each entry whose key is at least `start` and below `end`, in
    /// `direction`, until it returns `Ok(false)` or an error.
    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()>;
}

/// The one write transaction of an ordered byte-string key-value store.
pub trait KeyValueWrite: KeyValueRead {
    /// Stores `value` under `key`, replacing any value there.
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()>;

    /// Removes `key`; whether it was there.
    fn delete(&mut self, key: &[u8]) -> SqlResult<bool>;

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
}
