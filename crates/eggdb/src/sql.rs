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
    ColumnSchema, CopyChannel, CopyFormat, CopyStart, Dialect, IndexCheck, IndexColumnOrder,
    IndexSchema, Interval, IntervalFields, Numeric, NumericModifier, OneStatementError, OneStatementRefusal, OutputColumn,
    Parameter, QueryResult, RESERVED_KEY_PREFIX, Session, SqlError, SqlResult, SqlState, SqlType,
    TableSchema, TransactionStatus, Value, ViewColumnSchema, ViewSchema, sqlite_result_code_name,
};
use eggdb_sql::{EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection};
use std::ops::Bound;

/// A [`Database`] as the store SQL sessions run on; [`Database::sql_store`] makes one.
#[derive(Debug, Clone, Copy)]
pub struct Store<'db>(pub &'db Database);

impl<'db> Store<'db> {
    /// A SQL session on this database that reads `dialect`, outside any transaction block.
    pub fn session(&self, dialect: Dialect) -> Session<'_, Store<'db>> {
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
