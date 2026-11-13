//! SQL on a [`Database`]: [`Database::sql_store`] gives a [`Store`], and [`Store::session`]
//! runs SQL on it in a [`Dialect`]: PostgreSQL's, MySQL's, SQLite's or Oracle's, all on the same
//! tables.
//! The SQL lives in the database's own key space under [`RESERVED_KEY_PREFIX`], so its
//! transactions, crash safety and backups are the database's.
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let path = std::env::temp_dir().join(format!("eggdb-doc-sql-{}.db", std::process::id()));
//! use eggdb::Database;
//! use eggdb::sql::{Dialect, Value};
//!
//! let db = Database::open(&path)?;
//! let store = db.sql_store();
//! let mut sql = store.session(Dialect::Sqlite);
//! sql.execute("CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)", &mut |_| {})?;
//! sql.execute("INSERT INTO notes (body) VALUES ('first'), ('second')", &mut |_| {})?;
//! let mut rows = Vec::new();
//! sql.execute("SELECT id, body FROM notes WHERE id = last_insert_rowid()", &mut |result| {
//!     rows.extend(result.rows);
//! })?;
//! assert_eq!(rows, vec![vec![Value::BigInt(2), Value::Text("second".into())]]);
//! sql.close()?;
//! db.close()?;
//! # std::fs::remove_file(&path)?;
//! # Ok(()) }
//! ```

use crate::{Database, Error, ErrorKind, ReadTransaction, WriteTransaction};
use eggdb_sql::StoreFailure;
pub use eggdb_sql::{
    CancelSignal, FileReader, NativeFunction, NativeFunctions, NewFileWriter, ColumnSchema, CopyChannel, CopyFormat, CopyStart, Dialect, IndexCheck,
    IndexColumnOrder, IndexSchema, Interval, IntervalFields, Numeric, NumericModifier,
    OneStatementError, OneStatementRefusal, OutputColumn, Parameter, QueryResult,
    RESERVED_KEY_PREFIX, Session, SqlError, SqlResult, SqlState, SqlType, TableSchema,
    TransactionStatus, Value, ViewColumnSchema, ViewSchema, sqlite_result_code_name,
    with_native_functions,
};
use eggdb_sql::{EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection};
use std::ops::Bound;
use std::time::Duration;

/// A [`Database`] as the store SQL sessions run on; [`Database::sql_store`] makes one.
#[derive(Debug, Clone, Copy)]
pub struct Store<'db>(pub &'db Database);

impl<'db> Store<'db> {
    /// A SQL session on this database that reads `dialect`, outside any transaction block.
    pub fn session(&self, dialect: Dialect) -> Session<'_> {
        eggdb_sql::set_random_source(eggdb_platform::secure_random::fill);
        Session::with_dialect(self, dialect)
    }
}

/// Creates files on this machine for statements that write one, such as SQLite's `VACUUM
/// INTO`. Embedded use gives it to a session with [`Session::set_new_file_writer`]; servers
/// never do.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalFiles;

/// The [`LocalFiles`] writer, to hand to [`Session::set_new_file_writer`].
pub static LOCAL_FILES: LocalFiles = LocalFiles;

impl FileReader for LocalFiles {
    fn read_file(&self, path: &str, most_bytes: usize) -> Result<Vec<u8>, String> {
        let target = std::path::Path::new(path);
        let size = std::fs::metadata(target).map_err(|error| format!("cannot read {}: {error}", target.display()))?.len();
        if size > most_bytes as u64 {
            return Err(format!("{} is larger than {most_bytes} bytes", target.display()));
        }
        std::fs::read(target).map_err(|error| format!("cannot read {}: {error}", target.display()))
    }
}

impl NewFileWriter for LocalFiles {
    fn write_new_file(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        use std::io::Write;
        let target = std::path::Path::new(path);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    "output file already exists".to_owned()
                } else {
                    format!("cannot create {}: {error}", target.display())
                }
            })?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("cannot write {}: {error}", target.display()))
    }
}

impl Database {
    /// This database as a store for SQL: [`Store::session`] runs SQL on it in any dialect.
    pub fn sql_store(&self) -> Store<'_> {
        eggdb_sql::set_random_source(eggdb_platform::secure_random::fill);
        Store(self)
    }
}

const WRITER_BUSY: &str = "another session holds the database's one write transaction; retry once it commits or rolls back (EggDB runs one writer at a time)";

fn store_error(error: Error) -> SqlError {
    if error.kind() == ErrorKind::WriterActive {
        return SqlError::store(StoreFailure::Busy, WRITER_BUSY);
    }
    if error.kind() == ErrorKind::ReservedKey {
        return SqlError::new(SqlState::INVALID_PARAMETER_VALUE, error.to_string());
    }
    if error.kind() == ErrorKind::NotAnInteger {
        return SqlError::new(SqlState::DATA_EXCEPTION, error.to_string());
    }
    if error.kind() == ErrorKind::Conflict {
        return SqlError::new(
            SqlState::SERIALIZATION_FAILURE,
            format!("could not serialize access due to a concurrent commit: {error}"),
        )
        .with_hint("The transaction might succeed if retried.")
        .with_vendor_code(1213)
        .with_oracle_code(8177);
    }
    let failure = match error.kind() {
        ErrorKind::Locked | ErrorKind::WriterActive => StoreFailure::Busy,
        ErrorKind::ReadOnly => StoreFailure::ReadOnly,
        ErrorKind::TooLarge | ErrorKind::CacheFull | ErrorKind::DatabaseFull => {
            StoreFailure::TooLarge
        }
        ErrorKind::Corrupt => StoreFailure::Corrupt,
        ErrorKind::Io | ErrorKind::NeedsReopen => StoreFailure::Io,
        ErrorKind::OutOfMemory => StoreFailure::OutOfMemory,
        _ => StoreFailure::Other,
    };
    SqlError::store(failure, error.to_string())
}

fn scan_through(
    mut scan: crate::Scan<'_>,
    direction: ScanDirection,
    visit: &mut EntryVisitor<'_>,
) -> SqlResult<()> {
    loop {
        let entry = match direction {
            ScanDirection::Forward => scan.next_entry(),
            ScanDirection::Backward => scan.next_back_entry(),
        }
        .map_err(store_error)?;
        let Some((key, value)) = entry else {
            return Ok(());
        };
        if !visit(key, value)? {
            return Ok(());
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SnapshotOf {
    database: usize,
    token: eggdb_storage::SnapshotToken,
}

fn read_from<'db>(
    database: &'db Database,
    handle: &eggdb_sql::SnapshotHandle,
) -> SqlResult<Option<SqlSnapshot<'db>>> {
    let Some(snapshot) = handle.token::<SnapshotOf>() else {
        return Ok(None);
    };
    if snapshot.database != std::ptr::from_ref(database).addr() {
        return Ok(None);
    }
    ReadTransaction::begin_from(database, snapshot.token)
        .map(|read| Some(SqlSnapshot(read)))
        .map_err(store_error)
}

/// A snapshot that SQL reads, as [`Store`] gives it.
#[derive(Debug)]
pub struct SqlSnapshot<'db>(ReadTransaction<'db>);

/// The write transaction that SQL writes through, as [`Store`] gives it.
#[derive(Debug)]
pub struct SqlWriter<'db>(WriteTransaction<'db>);

impl KeyValueRead for SqlSnapshot<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        self.0.get(key).map_err(store_error)
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        match self.0.get_with(key, read).map_err(store_error)? {
            Some(outcome) => outcome.map(|()| true),
            None => Ok(false),
        }
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        let range = (Bound::Included(start), Bound::Excluded(end));
        scan_through(self.0.scan(range), direction, visit)
    }

    fn committed_version(&self) -> Option<u64> {
        Some(self.0.stamp().bits())
    }

    fn snapshot_handle(&self) -> Option<eggdb_sql::SnapshotHandle> {
        Some(eggdb_sql::SnapshotHandle::new(SnapshotOf {
            database: self.0.database_address(),
            token: self.0.snapshot_token(),
        }))
    }

    fn collection_names(&self) -> SqlResult<Vec<String>> {
        self.0.collection_names().map_err(store_error)
    }

    fn has_collection(&self, name: &str) -> SqlResult<bool> {
        Ok(collection_of(&self.0, name)?.is_some())
    }

    fn collection_get(&self, name: &str, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        match collection_of(&self.0, name)? {
            Some(collection) => collection.get(key).map_err(store_error),
            None => Ok(None),
        }
    }

    fn collection_scan(
        &self,
        name: &str,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        let Some(collection) = collection_of(&self.0, name)? else {
            return Ok(());
        };
        let range = (Bound::Included(start), Bound::Excluded(end));
        scan_through(collection.scan(range), direction, visit)
    }
}

fn collection_of<'t>(
    transaction: &'t ReadTransaction<'_>,
    name: &str,
) -> SqlResult<Option<crate::ReadCollection<'t>>> {
    match transaction.collection(name) {
        Ok(collection) => Ok(Some(collection)),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::CollectionNotFound | ErrorKind::InvalidCollectionName
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(store_error(error)),
    }
}

fn put_in_collection(
    transaction: &mut WriteTransaction<'_>,
    name: &str,
    key: &[u8],
    value: &[u8],
    ttl: Option<Duration>,
) -> SqlResult<bool> {
    let Some(mut collection) = writable_collection(transaction, name)? else {
        return Ok(false);
    };
    match ttl {
        Some(ttl) => collection.put_with_expiry(key, value, ttl),
        None => collection.put(key, value),
    }
    .map_err(store_error)?;
    Ok(true)
}

fn delete_from_collection(
    transaction: &mut WriteTransaction<'_>,
    name: &str,
    key: &[u8],
) -> SqlResult<Option<bool>> {
    let Some(mut collection) = writable_collection(transaction, name)? else {
        return Ok(None);
    };
    collection.delete(key).map(Some).map_err(store_error)
}

fn increment_in(
    transaction: &mut WriteTransaction<'_>,
    collection: Option<&str>,
    key: &[u8],
    by: i64,
) -> SqlResult<Option<i64>> {
    let Some(name) = collection else {
        return transaction
            .increment(key, by)
            .map(Some)
            .map_err(store_error);
    };
    let Some(mut named) = writable_collection(transaction, name)? else {
        return Ok(None);
    };
    named.increment(key, by).map(Some).map_err(store_error)
}

fn compare_and_set_in(
    transaction: &mut WriteTransaction<'_>,
    collection: Option<&str>,
    key: &[u8],
    expected: Option<&[u8]>,
    new: Option<&[u8]>,
) -> SqlResult<Option<(bool, Option<Vec<u8>>)>> {
    let Some(name) = collection else {
        let found = transaction.get(key).map_err(store_error)?;
        let swapped = transaction
            .compare_and_set(key, expected, new)
            .map_err(store_error)?;
        return Ok(Some((swapped, found)));
    };
    let Some(mut named) = writable_collection(transaction, name)? else {
        return Ok(None);
    };
    let found = named.get(key).map_err(store_error)?;
    let swapped = named
        .compare_and_set(key, expected, new)
        .map_err(store_error)?;
    Ok(Some((swapped, found)))
}

fn set_expiry_in(
    transaction: &mut WriteTransaction<'_>,
    collection: Option<&str>,
    key: &[u8],
    ttl: Duration,
) -> SqlResult<Option<bool>> {
    let Some(name) = collection else {
        return transaction
            .set_expiry(key, ttl)
            .map(Some)
            .map_err(store_error);
    };
    let Some(mut named) = writable_collection(transaction, name)? else {
        return Ok(None);
    };
    named.set_expiry(key, ttl).map(Some).map_err(store_error)
}

fn clear_expiry_in(
    transaction: &mut WriteTransaction<'_>,
    collection: Option<&str>,
    key: &[u8],
) -> SqlResult<Option<bool>> {
    let Some(name) = collection else {
        let present = transaction
            .get_with(key, |_| ())
            .map_err(store_error)?
            .is_some();
        transaction.clear_expiry(key).map_err(store_error)?;
        return Ok(Some(present));
    };
    let Some(mut named) = writable_collection(transaction, name)? else {
        return Ok(None);
    };
    let present = named.get_with(key, |_| ()).map_err(store_error)?.is_some();
    named.clear_expiry(key).map_err(store_error)?;
    Ok(Some(present))
}

fn writable_collection<'t, 'db>(
    transaction: &'t mut WriteTransaction<'db>,
    name: &str,
) -> SqlResult<Option<crate::WriteCollection<'t, 'db>>> {
    match transaction.collection(name) {
        Ok(collection) => Ok(Some(collection)),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::CollectionNotFound | ErrorKind::InvalidCollectionName
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(store_error(error)),
    }
}

impl KeyValueRead for SqlWriter<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        self.0.get(key).map_err(store_error)
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        match self.0.get_with(key, read).map_err(store_error)? {
            Some(outcome) => outcome.map(|()| true),
            None => Ok(false),
        }
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        let range = (Bound::Included(start), Bound::Excluded(end));
        scan_through(self.0.scan(range), direction, visit)
    }
}

impl KeyValueWrite for SqlWriter<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        self.0.put(key, value).map_err(store_error)
    }

    fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> SqlResult<bool> {
        self.0.put_if_absent(key, value).map_err(store_error)
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        self.0.delete(key).map_err(store_error)
    }

    fn store_prefix_in_lsm_tree(&mut self, prefix: &[u8]) -> SqlResult<bool> {
        self.0.store_prefix_in_lsm_tree(prefix).map_err(store_error)
    }

    fn put_with_expiry(&mut self, key: &[u8], value: &[u8], ttl: Duration) -> SqlResult<()> {
        self.0.put_with_expiry(key, value, ttl).map_err(store_error)
    }

    fn collection_put(
        &mut self,
        name: &str,
        key: &[u8],
        value: &[u8],
        ttl: Option<Duration>,
    ) -> SqlResult<bool> {
        put_in_collection(&mut self.0, name, key, value, ttl)
    }

    fn collection_delete(&mut self, name: &str, key: &[u8]) -> SqlResult<Option<bool>> {
        delete_from_collection(&mut self.0, name, key)
    }

    fn increment(
        &mut self,
        collection: Option<&str>,
        key: &[u8],
        by: i64,
    ) -> SqlResult<Option<i64>> {
        increment_in(&mut self.0, collection, key, by)
    }

    fn compare_and_set(
        &mut self,
        collection: Option<&str>,
        key: &[u8],
        expected: Option<&[u8]>,
        new: Option<&[u8]>,
    ) -> SqlResult<Option<(bool, Option<Vec<u8>>)>> {
        compare_and_set_in(&mut self.0, collection, key, expected, new)
    }

    fn set_expiry(
        &mut self,
        collection: Option<&str>,
        key: &[u8],
        ttl: Duration,
    ) -> SqlResult<Option<bool>> {
        set_expiry_in(&mut self.0, collection, key, ttl)
    }

    fn clear_expiry(&mut self, collection: Option<&str>, key: &[u8]) -> SqlResult<Option<bool>> {
        clear_expiry_in(&mut self.0, collection, key)
    }

    fn commit(self) -> SqlResult<()> {
        self.0.commit().map(|_| ()).map_err(store_error)
    }

    fn rollback(self) -> SqlResult<()> {
        self.0.rollback().map_err(store_error)
    }
}

impl KeyValueStore for Store<'_> {
    type Read<'s>
        = SqlSnapshot<'s>
    where
        Self: 's;
    type Write<'s>
        = SqlWriter<'s>
    where
        Self: 's;

    fn begin_read(&self) -> SqlResult<SqlSnapshot<'_>> {
        self.0.begin_read().map(SqlSnapshot).map_err(store_error)
    }

    fn begin_read_from(
        &self,
        handle: &eggdb_sql::SnapshotHandle,
    ) -> SqlResult<Option<SqlSnapshot<'_>>> {
        read_from(&self.0, handle)
    }

    fn begin_write(&self) -> SqlResult<SqlWriter<'_>> {
        self.0.begin_write().map(SqlWriter).map_err(store_error)
    }

    fn begin_concurrent_write(
        &self,
        isolation: eggdb_sql::ConcurrentIsolation,
    ) -> SqlResult<Option<Box<dyn eggdb_sql::ConcurrentWrite + '_>>> {
        concurrent_writer(&self.0, isolation)
    }

    fn begin_write_waiting(&self, limit: Duration) -> SqlResult<SqlWriter<'_>> {
        self.0
            .begin_write_within(limit)
            .map(SqlWriter)
            .map_err(store_error)
    }

    fn page_lookups(&self) -> Option<eggdb_sql::PageLookups> {
        Some(this_threads_page_lookups())
    }

    fn backup_to(&self, path: &std::path::Path, clone: bool) -> SqlResult<Option<eggdb_sql::StoreBackup>> {
        let failed = |error: crate::Error| SqlError::new(SqlState::IO_ERROR, format!("BACKUP: {error}"));
        if clone {
            let report = self.0.clone_to(path).map_err(failed)?;
            return Ok(Some(eggdb_sql::StoreBackup { stamp: report.stamp.bits(), entries: 0, instant: report.instant }));
        }
        let report = self.0.backup_to(path).map_err(failed)?;
        Ok(Some(eggdb_sql::StoreBackup { stamp: report.stamp.bits(), entries: report.entries, instant: false }))
    }
}

fn concurrent_writer(
    database: &Database,
    isolation: eggdb_sql::ConcurrentIsolation,
) -> SqlResult<Option<Box<dyn eggdb_sql::ConcurrentWrite + '_>>> {
    let isolation = match isolation {
        eggdb_sql::ConcurrentIsolation::Snapshot => crate::Isolation::Snapshot,
        eggdb_sql::ConcurrentIsolation::Serializable => crate::Isolation::Serializable,
    };
    let transaction = database.begin_concurrent(isolation).map_err(store_error)?;
    Ok(Some(Box::new(SqlConcurrentWriter(std::cell::RefCell::new(transaction)))))
}

/// A write transaction that SQL runs beside other writers for an explicit `REPEATABLE READ` or
/// `SERIALIZABLE` block (D1123): a [`crate::ConcurrentTransaction`], checked when it commits.
/// Collections, keys with a time to live and counters are refused inside it for now.
#[derive(Debug)]
pub struct SqlConcurrentWriter<'db>(std::cell::RefCell<crate::ConcurrentTransaction<'db>>);

impl KeyValueRead for SqlConcurrentWriter<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        self.0.borrow_mut().get(key).map_err(store_error)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        let entries = self
            .0
            .borrow_mut()
            .scan((Bound::Included(start), Bound::Excluded(end)))
            .map_err(store_error)?;
        let ordered: Box<dyn Iterator<Item = &(Vec<u8>, Vec<u8>)>> = match direction {
            ScanDirection::Forward => Box::new(entries.iter()),
            ScanDirection::Backward => Box::new(entries.iter().rev()),
        };
        for (key, value) in ordered {
            if !visit(key, value)? {
                break;
            }
        }
        Ok(())
    }
}

impl KeyValueWrite for SqlConcurrentWriter<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        self.0.get_mut().put(key, value).map_err(store_error)
    }

    fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> SqlResult<bool> {
        let transaction = self.0.get_mut();
        if transaction.get(key).map_err(store_error)?.is_some() {
            return Ok(false);
        }
        transaction.put(key, value).map_err(store_error)?;
        Ok(true)
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        self.0.get_mut().delete(key).map_err(store_error)
    }

    fn commit(self) -> SqlResult<()> {
        self.0.into_inner().commit().map(|_| ()).map_err(store_error)
    }

    fn rollback(self) -> SqlResult<()> {
        self.0.into_inner().rollback();
        Ok(())
    }
}

impl eggdb_sql::ConcurrentWrite for SqlConcurrentWriter<'_> {
    fn commit_concurrent(self: Box<Self>) -> SqlResult<()> {
        (*self).commit()
    }

    fn rollback_concurrent(self: Box<Self>) -> SqlResult<()> {
        (*self).rollback()
    }
}

/// A [`Database`] owned as the store SQL sessions run on, for a program that opens the
/// database for one piece of work and closes it after, such as one call of `eggdb mcp`.
/// [`OwnedStore::close`] gives the database back closed; [`Store`] borrows one instead.
#[derive(Debug)]
pub struct OwnedStore(pub Database);

impl OwnedStore {
    /// Closes the database, as [`Database::close`] does.
    pub fn close(self) -> crate::Result<()> {
        self.0.close()
    }
}

impl KeyValueStore for OwnedStore {
    type Read<'s>
        = SqlSnapshot<'s>
    where
        Self: 's;
    type Write<'s>
        = SqlWriter<'s>
    where
        Self: 's;

    fn begin_read(&self) -> SqlResult<SqlSnapshot<'_>> {
        self.0.begin_read().map(SqlSnapshot).map_err(store_error)
    }

    fn begin_read_from(
        &self,
        handle: &eggdb_sql::SnapshotHandle,
    ) -> SqlResult<Option<SqlSnapshot<'_>>> {
        read_from(&self.0, handle)
    }

    fn begin_write(&self) -> SqlResult<SqlWriter<'_>> {
        self.0.begin_write().map(SqlWriter).map_err(store_error)
    }

    fn begin_concurrent_write(
        &self,
        isolation: eggdb_sql::ConcurrentIsolation,
    ) -> SqlResult<Option<Box<dyn eggdb_sql::ConcurrentWrite + '_>>> {
        concurrent_writer(&self.0, isolation)
    }

    fn begin_write_waiting(&self, limit: Duration) -> SqlResult<SqlWriter<'_>> {
        self.0
            .begin_write_within(limit)
            .map(SqlWriter)
            .map_err(store_error)
    }

    fn page_lookups(&self) -> Option<eggdb_sql::PageLookups> {
        Some(this_threads_page_lookups())
    }
}

fn this_threads_page_lookups() -> eggdb_sql::PageLookups {
    let lookups = eggdb_storage::thread_page_lookups();
    eggdb_sql::PageLookups {
        hits: lookups.hits,
        reads: lookups.reads,
    }
}

struct TransactionStore<'t, 'db> {
    transaction: std::cell::RefCell<&'t mut WriteTransaction<'db>>,
}

/// A read of a write transaction that runs SQL inside it (see [`WriteTransaction::sql`]).
pub struct TransactionReader<'s, 't, 'db>(std::cell::Ref<'s, &'t mut WriteTransaction<'db>>);

/// A statement's writes into a write transaction that runs SQL inside it (see
/// [`WriteTransaction::sql`]): kept with the values they replace, so a failed statement undoes
/// its own writes and leaves the transaction's other changes.
pub struct TransactionWriter<'s, 't, 'db> {
    transaction: std::cell::RefMut<'s, &'t mut WriteTransaction<'db>>,
    undo: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl std::fmt::Debug for TransactionReader<'_, '_, '_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TransactionReader")
    }
}

impl std::fmt::Debug for TransactionWriter<'_, '_, '_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransactionWriter")
            .field("undo_entries", &self.undo.len())
            .finish_non_exhaustive()
    }
}

fn in_use() -> SqlError {
    SqlError::store(
        StoreFailure::Busy,
        "the write transaction is already in use by this SQL statement",
    )
}

impl KeyValueRead for TransactionReader<'_, '_, '_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        self.0.get(key).map_err(store_error)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        let range = (Bound::Included(start), Bound::Excluded(end));
        scan_through(self.0.scan(range), direction, visit)
    }
}

impl KeyValueRead for TransactionWriter<'_, '_, '_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        self.transaction.get(key).map_err(store_error)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        let range = (Bound::Included(start), Bound::Excluded(end));
        scan_through(self.transaction.scan(range), direction, visit)
    }
}

impl KeyValueWrite for TransactionWriter<'_, '_, '_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        let previous = self.transaction.get(key).map_err(store_error)?;
        self.transaction.put(key, value).map_err(store_error)?;
        self.undo.push((key.to_vec(), previous));
        Ok(())
    }

    fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> SqlResult<bool> {
        let inserted = self
            .transaction
            .put_if_absent(key, value)
            .map_err(store_error)?;
        if inserted {
            self.undo.push((key.to_vec(), None));
        }
        Ok(inserted)
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        let previous = self.transaction.get(key).map_err(store_error)?;
        let deleted = self.transaction.delete(key).map_err(store_error)?;
        if deleted {
            self.undo.push((key.to_vec(), previous));
        }
        Ok(deleted)
    }

    fn commit(self) -> SqlResult<()> {
        Ok(())
    }

    fn rollback(mut self) -> SqlResult<()> {
        while let Some((key, previous)) = self.undo.pop() {
            match previous {
                Some(value) => self.transaction.put(&key, &value),
                None => self.transaction.delete(&key).map(|_| ()),
            }
            .map_err(store_error)?;
        }
        Ok(())
    }
}

impl<'t, 'db> KeyValueStore for TransactionStore<'t, 'db> {
    type Read<'s>
        = TransactionReader<'s, 't, 'db>
    where
        Self: 's;
    type Write<'s>
        = TransactionWriter<'s, 't, 'db>
    where
        Self: 's;

    fn begin_read(&self) -> SqlResult<TransactionReader<'_, 't, 'db>> {
        self.transaction
            .try_borrow()
            .map(TransactionReader)
            .map_err(|_| in_use())
    }

    fn begin_write(&self) -> SqlResult<TransactionWriter<'_, 't, 'db>> {
        self.transaction
            .try_borrow_mut()
            .map(|transaction| TransactionWriter {
                transaction,
                undo: Vec::new(),
            })
            .map_err(|_| in_use())
    }
}

/// A [`Database`] as the store SQL sessions run on, whose open write transaction key-value code
/// also reaches between statements. While a session's block (`BEGIN` to `COMMIT` or `ROLLBACK`)
/// holds the database's one write transaction, [`SharedWriteStore::with_open_transaction`]
/// writes into that same transaction, so the block's commit keeps the key-value writes and the
/// SQL changes together and its rollback drops both.
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let path = std::env::temp_dir().join(format!("eggdb-doc-shared-{}.db", std::process::id()));
/// use eggdb::sql::{Dialect, SharedWriteStore};
/// let db = eggdb::Database::open(&path)?;
/// let store = SharedWriteStore::new(&db);
/// let mut sql = store.session(Dialect::PostgreSql);
/// sql.execute("CREATE TABLE orders (id integer PRIMARY KEY)", &mut |_| {})?;
/// sql.execute("BEGIN; INSERT INTO orders VALUES (7)", &mut |_| {})?;
/// store.with_open_transaction(|tx| tx.put("last_order", "7")).transpose()?;
/// sql.execute("ROLLBACK", &mut |_| {})?;
/// assert_eq!(db.get("last_order")?, None);
/// sql.close()?;
/// # drop(store);
/// # drop(db);
/// # std::fs::remove_file(&path)?;
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct SharedWriteStore<'db> {
    db: &'db Database,
    open: std::cell::RefCell<Option<WriteTransaction<'db>>>,
}

impl<'db> SharedWriteStore<'db> {
    /// A store over `db` with no write transaction open.
    pub fn new(db: &'db Database) -> Self {
        eggdb_sql::set_random_source(eggdb_platform::secure_random::fill);
        Self {
            db,
            open: std::cell::RefCell::new(None),
        }
    }

    /// A SQL session on this store that reads `dialect`, outside any transaction block.
    pub fn session(&self, dialect: Dialect) -> Session<'_> {
        Session::with_dialect(self, dialect)
    }

    /// Runs `work` on the write transaction a SQL session holds open on this store, as a block
    /// does from `BEGIN` to its end, and returns what it returned; `None`, without running it,
    /// when no write transaction is open.
    pub fn with_open_transaction<T>(
        &self,
        work: impl FnOnce(&mut WriteTransaction<'db>) -> T,
    ) -> Option<T> {
        let mut open = self.open.try_borrow_mut().ok()?;
        open.as_mut().map(work)
    }

    /// Runs `work` as [`with_open_transaction`](Self::with_open_transaction) does, for
    /// `session`, the session on this store whose block holds the transaction. While that block
    /// has a `SAVEPOINT` set, every change `work` makes, its times to live and collection
    /// entries included, goes into the session's savepoint undo log: `ROLLBACK TO SAVEPOINT`
    /// undoes it together with the statements' writes since the savepoint, and
    /// `RELEASE SAVEPOINT` keeps it in the block.
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-undoable-{}.db", std::process::id()));
    /// use eggdb::sql::{Dialect, SharedWriteStore};
    /// let db = eggdb::Database::open(&path)?;
    /// let store = SharedWriteStore::new(&db);
    /// let mut sql = store.session(Dialect::PostgreSql);
    /// sql.execute("BEGIN", &mut |_| {})?;
    /// store.with_open_transaction_undoable(&mut sql, |tx| tx.put("a", "1")).transpose()?;
    /// sql.execute("SAVEPOINT s", &mut |_| {})?;
    /// store.with_open_transaction_undoable(&mut sql, |tx| tx.put("b", "2")).transpose()?;
    /// store.with_open_transaction_undoable(&mut sql, |tx| tx.delete("a")).transpose()?;
    /// sql.execute("ROLLBACK TO SAVEPOINT s", &mut |_| {})?;
    /// sql.execute("COMMIT", &mut |_| {})?;
    /// assert_eq!(db.get("a")?, Some(b"1".to_vec()));
    /// assert_eq!(db.get("b")?, None);
    /// sql.close()?;
    /// # drop(store);
    /// # drop(db);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(()) }
    /// ```
    pub fn with_open_transaction_undoable<T>(
        &self,
        session: &mut Session<'_>,
        work: impl FnOnce(&mut WriteTransaction<'db>) -> T,
    ) -> Option<T> {
        if !session.records_savepoint_undo() {
            return self.with_open_transaction(work);
        }
        let (result, changes) =
            self.with_open_transaction(|transaction| transaction.journaled(work))?;
        let recorded = session.record_savepoint_undo(changes);
        debug_assert!(
            recorded,
            "the session stopped taking savepoint undo during the work"
        );
        Some(result)
    }
}

/// The write transaction SQL writes through, as [`SharedWriteStore`] gives it: the transaction
/// stays in the store, where [`SharedWriteStore::with_open_transaction`] reaches it between
/// statements. Dropping it without a commit rolls the transaction back.
#[derive(Debug)]
pub struct SharedWriter<'s, 'db> {
    open: &'s std::cell::RefCell<Option<WriteTransaction<'db>>>,
}

fn shared_transaction_ended() -> SqlError {
    SqlError::store(
        StoreFailure::Other,
        "the shared write transaction has already ended",
    )
}

impl<'db> SharedWriter<'_, 'db> {
    fn reading<T>(
        &self,
        work: impl FnOnce(&WriteTransaction<'db>) -> SqlResult<T>,
    ) -> SqlResult<T> {
        let open = self.open.try_borrow().map_err(|_| in_use())?;
        work(open.as_ref().ok_or_else(shared_transaction_ended)?)
    }

    fn writing<T>(
        &self,
        work: impl FnOnce(&mut WriteTransaction<'db>) -> SqlResult<T>,
    ) -> SqlResult<T> {
        let mut open = self.open.try_borrow_mut().map_err(|_| in_use())?;
        work(open.as_mut().ok_or_else(shared_transaction_ended)?)
    }

    fn taken(&self) -> SqlResult<WriteTransaction<'db>> {
        self.open
            .try_borrow_mut()
            .map_err(|_| in_use())?
            .take()
            .ok_or_else(shared_transaction_ended)
    }
}

impl Drop for SharedWriter<'_, '_> {
    fn drop(&mut self) {
        if let Ok(mut open) = self.open.try_borrow_mut()
            && let Some(transaction) = open.take()
        {
            let _ = transaction.rollback();
        }
    }
}

impl KeyValueRead for SharedWriter<'_, '_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        self.reading(|transaction| transaction.get(key).map_err(store_error))
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        self.reading(
            |transaction| match transaction.get_with(key, read).map_err(store_error)? {
                Some(outcome) => outcome.map(|()| true),
                None => Ok(false),
            },
        )
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        let range = (Bound::Included(start), Bound::Excluded(end));
        self.reading(|transaction| scan_through(transaction.scan(range), direction, visit))
    }
}

impl KeyValueWrite for SharedWriter<'_, '_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        self.writing(|transaction| transaction.put(key, value).map_err(store_error))
    }

    fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> SqlResult<bool> {
        self.writing(|transaction| transaction.put_if_absent(key, value).map_err(store_error))
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        self.writing(|transaction| transaction.delete(key).map_err(store_error))
    }

    fn store_prefix_in_lsm_tree(&mut self, prefix: &[u8]) -> SqlResult<bool> {
        self.writing(|transaction| {
            transaction
                .store_prefix_in_lsm_tree(prefix)
                .map_err(store_error)
        })
    }

    fn put_with_expiry(&mut self, key: &[u8], value: &[u8], ttl: Duration) -> SqlResult<()> {
        self.writing(|transaction| {
            transaction
                .put_with_expiry(key, value, ttl)
                .map_err(store_error)
        })
    }

    fn collection_put(
        &mut self,
        name: &str,
        key: &[u8],
        value: &[u8],
        ttl: Option<Duration>,
    ) -> SqlResult<bool> {
        self.writing(|transaction| put_in_collection(transaction, name, key, value, ttl))
    }

    fn collection_delete(&mut self, name: &str, key: &[u8]) -> SqlResult<Option<bool>> {
        self.writing(|transaction| delete_from_collection(transaction, name, key))
    }

    fn increment(
        &mut self,
        collection: Option<&str>,
        key: &[u8],
        by: i64,
    ) -> SqlResult<Option<i64>> {
        self.writing(|transaction| increment_in(transaction, collection, key, by))
    }

    fn compare_and_set(
        &mut self,
        collection: Option<&str>,
        key: &[u8],
        expected: Option<&[u8]>,
        new: Option<&[u8]>,
    ) -> SqlResult<Option<(bool, Option<Vec<u8>>)>> {
        self.writing(|transaction| compare_and_set_in(transaction, collection, key, expected, new))
    }

    fn set_expiry(
        &mut self,
        collection: Option<&str>,
        key: &[u8],
        ttl: Duration,
    ) -> SqlResult<Option<bool>> {
        self.writing(|transaction| set_expiry_in(transaction, collection, key, ttl))
    }

    fn clear_expiry(&mut self, collection: Option<&str>, key: &[u8]) -> SqlResult<Option<bool>> {
        self.writing(|transaction| clear_expiry_in(transaction, collection, key))
    }

    fn restore(&mut self, key: &[u8], previous: Option<&[u8]>) -> SqlResult<()> {
        self.writing(|transaction| {
            transaction
                .restore_stored(key, previous)
                .map_err(store_error)
        })
    }

    fn commit(self) -> SqlResult<()> {
        self.taken()?.commit().map(|_| ()).map_err(store_error)
    }

    fn rollback(self) -> SqlResult<()> {
        self.taken()?.rollback().map_err(store_error)
    }
}

impl<'db> KeyValueStore for SharedWriteStore<'db> {
    type Read<'s>
        = SqlSnapshot<'s>
    where
        Self: 's;
    type Write<'s>
        = SharedWriter<'s, 'db>
    where
        Self: 's;

    fn begin_read(&self) -> SqlResult<SqlSnapshot<'_>> {
        self.db.begin_read().map(SqlSnapshot).map_err(store_error)
    }

    fn begin_write(&self) -> SqlResult<SharedWriter<'_, 'db>> {
        let mut open = self.open.try_borrow_mut().map_err(|_| in_use())?;
        if open.is_some() {
            return Err(SqlError::store(StoreFailure::Busy, WRITER_BUSY));
        }
        *open = Some(self.db.begin_write().map_err(store_error)?);
        Ok(SharedWriter { open: &self.open })
    }
}

impl<'db> WriteTransaction<'db> {
    /// Runs the SQL statements in `text`, in `dialect`, inside this transaction, and returns
    /// each statement's result. Their changes become part of this transaction, so one commit
    /// keeps key-value writes and SQL changes together and one rollback drops both. A statement
    /// that fails undoes its own writes, and the error comes back; the transaction stays open
    /// with the changes made before it. Transaction control in `text` (`BEGIN`, `COMMIT`) has no
    /// effect beyond its statements: this transaction's commit is the only one.
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-tx-sql-{}.db", std::process::id()));
    /// use eggdb::sql::Dialect;
    /// let db = eggdb::Database::open(&path)?;
    /// db.sql_store()
    ///     .session(Dialect::PostgreSql)
    ///     .execute("CREATE TABLE orders (id integer PRIMARY KEY)", &mut |_| {})?;
    /// let mut tx = db.begin_write()?;
    /// tx.put("last_order", "7")?;
    /// tx.sql(Dialect::PostgreSql, "INSERT INTO orders VALUES (7)")?;
    /// tx.rollback()?;
    /// assert_eq!(db.get("last_order")?, None);
    /// # drop(db);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(()) }
    /// ```
    pub fn sql(&mut self, dialect: Dialect, text: &str) -> SqlResult<Vec<QueryResult>> {
        eggdb_sql::set_random_source(eggdb_platform::secure_random::fill);
        let store = TransactionStore {
            transaction: std::cell::RefCell::new(self),
        };
        let mut session = Session::with_dialect(&store, dialect);
        let mut results = Vec::new();
        let outcome = session.execute(text, &mut |result| results.push(result));
        let closed = session.close();
        outcome?;
        closed?;
        Ok(results)
    }
}

/// The lines `eggdb sql` prints for `error`, raised by `text`: PostgreSQL's `ERROR:`,
/// `LINE n:` with a `^` under the position, `DETAIL:`, `HINT:`, `CONTEXT:` and `SQLSTATE:`,
/// or the vendor's own form for SQLite, Oracle, SQL Server and MySQL errors. The GUI's
/// `Messages` and the CLI show the same lines.
pub fn error_lines(error: &SqlError, text: &str) -> Vec<String> {
    if error.vendor() == Some(Dialect::Sqlite) {
        let code = error.vendor_code().unwrap_or(1);
        let name = sqlite_result_code_name(code).unwrap_or("SQLITE_ERROR");
        return vec![
            format!("Error: {}", error.message()),
            format!("Result code: {name} ({code})"),
        ];
    }
    if error.vendor() == Some(Dialect::Oracle) || error.vendor() == Some(Dialect::SqlServer) {
        return vec![error.to_string()];
    }
    if let Some(code) = error.vendor_code() {
        return vec![format!(
            "ERROR {code} ({}): {}",
            error.state(),
            error.message()
        )];
    }
    let mut lines = vec![format!("ERROR:  {}", error.message())];
    if let Some(position) = error.position().filter(|&position| position <= text.len()) {
        let (number, line, column) = line_of(text, position);
        let prefix = format!("LINE {number}: ");
        lines.push(format!("{prefix}{line}"));
        lines.push(format!("{}^", " ".repeat(prefix.chars().count() + column)));
    }
    if let Some(detail) = error.detail() {
        lines.push(format!("DETAIL:  {detail}"));
    }
    if let Some(hint) = error.hint() {
        lines.push(format!("HINT:  {hint}"));
    }
    if let Some(context) = error.context() {
        lines.push(format!("CONTEXT:  {context}"));
    }
    lines.push(format!("SQLSTATE:  {}", error.state()));
    lines
}

fn line_of(text: &str, position: usize) -> (usize, &str, usize) {
    let mut position = position;
    while !text.is_char_boundary(position) {
        position -= 1;
    }
    let before = &text[..position];
    let number = before.matches('\n').count() + 1;
    let start = before.rfind('\n').map_or(0, |index| index + 1);
    let end = text[position..]
        .find('\n')
        .map_or(text.len(), |index| position + index);
    let line = text[start..end].trim_end_matches('\r');
    let column = text[start..position].chars().count();
    (number, line, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_position_names_its_line_and_column() {
        let text = "SELECT 1;\nSELECT * FROM nope;";
        assert_eq!(line_of(text, 24), (2, "SELECT * FROM nope;", 14));
        assert_eq!(line_of(text, 0), (1, "SELECT 1;", 0));
        assert_eq!(line_of("é x", 3), (1, "é x", 2));
    }

    #[test]
    fn the_shared_writer_writes_times_to_live_and_collection_entries()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let db = Database::in_memory()?;
        db.create_collection("sessions")?;
        db.collection("sessions").put("s:1", "Ada")?;
        let store = SharedWriteStore::new(&db);
        let mut writer = store.begin_write()?;
        writer.put_with_expiry(b"timed", b"t", Duration::from_secs(3600))?;
        assert!(writer.collection_put("sessions", b"s:2", b"Grace", None)?);
        assert!(!writer.collection_put("nowhere", b"k", b"v", None)?);
        assert_eq!(writer.collection_delete("sessions", b"s:1")?, Some(true));
        assert_eq!(writer.collection_delete("nowhere", b"k")?, None);
        writer.commit()?;
        assert!(db.time_to_live("timed")?.is_some());
        assert_eq!(
            db.collection("sessions").get("s:2")?,
            Some(b"Grace".to_vec())
        );
        assert_eq!(db.collection("sessions").get("s:1")?, None);
        Ok(())
    }

    fn counts_swaps_and_expires(
        writer: &mut impl KeyValueWrite,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        assert_eq!(writer.increment(None, b"hits", 5)?, Some(5));
        assert_eq!(writer.increment(None, b"hits", -7)?, Some(-2));
        assert_eq!(writer.increment(Some("sessions"), b"n", 1)?, Some(1));
        assert_eq!(writer.increment(Some("nowhere"), b"n", 1)?, None);
        let not_a_counter = writer.increment(None, b"name", 1).unwrap_err();
        assert_eq!(not_a_counter.state(), SqlState::DATA_EXCEPTION);
        assert_eq!(
            writer.compare_and_set(None, b"lock", None, Some(b"me".as_slice()))?,
            Some((true, None))
        );
        assert_eq!(
            writer.compare_and_set(None, b"lock", Some(b"you".as_slice()), None)?,
            Some((false, Some(b"me".to_vec())))
        );
        assert_eq!(
            writer.compare_and_set(Some("sessions"), b"s:1", Some(b"Ada".as_slice()), None)?,
            Some((true, Some(b"Ada".to_vec())))
        );
        assert_eq!(
            writer.compare_and_set(Some("nowhere"), b"k", None, None)?,
            None
        );
        assert_eq!(
            writer.set_expiry(None, b"lock", Duration::from_secs(60))?,
            Some(true)
        );
        assert_eq!(
            writer.set_expiry(None, b"absent", Duration::from_secs(60))?,
            Some(false)
        );
        assert_eq!(
            writer.set_expiry(None, b"name", Duration::ZERO)?,
            Some(true)
        );
        assert_eq!(writer.get(b"name")?, None);
        assert_eq!(
            writer.set_expiry(Some("sessions"), b"n", Duration::from_secs(60))?,
            Some(true)
        );
        assert_eq!(writer.clear_expiry(Some("sessions"), b"n")?, Some(true));
        assert_eq!(writer.clear_expiry(None, b"absent")?, Some(false));
        assert_eq!(writer.clear_expiry(Some("nowhere"), b"k")?, None);
        Ok(())
    }

    #[test]
    fn both_writers_count_swap_and_set_times_to_live_for_the_key_value_tools()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        for shared in [false, true] {
            let db = Database::in_memory()?;
            db.create_collection("sessions")?;
            db.collection("sessions").put("s:1", "Ada")?;
            db.put("name", "Ada")?;
            if shared {
                let store = SharedWriteStore::new(&db);
                let mut writer = store.begin_write()?;
                counts_swaps_and_expires(&mut writer)?;
                writer.commit()?;
            } else {
                let store = db.sql_store();
                let mut writer = store.begin_write()?;
                counts_swaps_and_expires(&mut writer)?;
                writer.commit()?;
            }
            assert_eq!(db.get("hits")?, Some(b"-2".to_vec()));
            assert!(db.time_to_live("lock")?.is_some());
            assert_eq!(db.get("name")?, None);
            let sessions = db.collection("sessions");
            assert_eq!(sessions.get("s:1")?, None);
            assert_eq!(sessions.get("n")?, Some(b"1".to_vec()));
            assert_eq!(sessions.time_to_live("n")?, None);
        }
        Ok(())
    }

    #[test]
    fn a_key_the_engine_keeps_is_never_counted_swapped_or_expired()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let db = Database::in_memory()?;
        let store = db.sql_store();
        let mut writer = store.begin_write()?;
        let mut kept = crate::collection::COLLECTIONS_PREFIX.to_vec();
        kept.push(0);
        let refused = writer.increment(None, &kept, 1).unwrap_err();
        assert_eq!(refused.state(), SqlState::INVALID_PARAMETER_VALUE);
        assert!(
            writer
                .compare_and_set(None, &kept, None, Some(b"x".as_slice()))
                .is_err()
        );
        assert_eq!(writer.set_expiry(None, &kept, Duration::ZERO)?, Some(false));
        writer.rollback()?;
        Ok(())
    }

    fn undone_after_a_savepoint(
        db: &Database,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let store = SharedWriteStore::new(db);
        let mut sql = store.session(Dialect::PostgreSql);
        sql.execute("BEGIN; SAVEPOINT s", &mut |_| {})?;
        store
            .with_open_transaction_undoable(&mut sql, |tx| -> crate::Result<()> {
                tx.clear_expiry("timed")?;
                tx.put_with_expiry("fresh", "f", std::time::Duration::from_secs(60))?;
                tx.put("plain", "changed")?;
                let mut sessions = tx.collection("sessions")?;
                sessions.delete("s:1")?;
                sessions.put_with_expiry("s:2", "Grace", std::time::Duration::from_secs(60))?;
                Ok(())
            })
            .transpose()?;
        sql.execute("ROLLBACK TO SAVEPOINT s", &mut |_| {})?;
        store
            .with_open_transaction_undoable(&mut sql, |tx| tx.put("after", "kept"))
            .transpose()?;
        sql.execute("RELEASE SAVEPOINT s; COMMIT", &mut |_| {})?;
        sql.close()?;
        Ok(())
    }

    #[test]
    fn a_savepoint_rollback_puts_back_times_to_live_and_collection_entries()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let path = std::env::temp_dir().join(format!(
            "eggdb-sql-savepoint-undo-{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let db = Database::open(&path)?;
        db.create_collection("sessions")?;
        db.collection("sessions").put("s:1", "Ada")?;
        db.put_with_expiry("timed", "t", std::time::Duration::from_secs(3600))?;
        db.put("plain", "p")?;
        undone_after_a_savepoint(&db)?;
        assert!(db.time_to_live("timed")?.is_some());
        assert_eq!(db.get("fresh")?, None);
        assert_eq!(db.get("plain")?, Some(b"p".to_vec()));
        assert_eq!(db.get("after")?, Some(b"kept".to_vec()));
        let sessions = db.collection("sessions");
        assert_eq!(sessions.get("s:1")?, Some(b"Ada".to_vec()));
        assert_eq!(sessions.get("s:2")?, None);
        assert_eq!(sessions.time_to_live("s:2")?, None);
        drop(sessions);
        let counted: Vec<(String, u64, u64, u64)> = db
            .collection_stats()?
            .into_iter()
            .map(|one| (one.name, one.entries, one.key_bytes, one.value_bytes))
            .collect();
        assert_eq!(counted, [("sessions".to_owned(), 1, 3, 3)]);
        drop(db);
        std::fs::remove_file(&path)?;
        Ok(())
    }
}
