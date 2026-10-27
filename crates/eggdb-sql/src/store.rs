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
