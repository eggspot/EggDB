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
    CancelSignal, ColumnSchema, CopyChannel, CopyFormat, CopyStart, Dialect, IndexCheck,
    IndexColumnOrder, IndexSchema, Interval, IntervalFields, Numeric, NumericModifier,
    OneStatementError, OneStatementRefusal, OutputColumn, Parameter, QueryResult,
    RESERVED_KEY_PREFIX, Session, SqlError, SqlResult, SqlState, SqlType, TableSchema,
    TransactionStatus, Value, ViewColumnSchema, ViewSchema, sqlite_result_code_name,
};
use eggdb_sql::{EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection};
use std::ops::Bound;

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

    fn begin_write(&self) -> SqlResult<SqlWriter<'_>> {
        self.0.begin_write().map(SqlWriter).map_err(store_error)
    }

    fn page_lookups(&self) -> Option<eggdb_sql::PageLookups> {
        Some(this_threads_page_lookups())
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

    fn begin_write(&self) -> SqlResult<SqlWriter<'_>> {
        self.0.begin_write().map(SqlWriter).map_err(store_error)
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
}
