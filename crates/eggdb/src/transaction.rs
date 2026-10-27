use crate::database::Database;
use crate::error::{Error, Result};
use crate::scan::{KeyRange, Reader, Scan};
use crate::writer_slot::WriterTurn;
use eggdb_platform::OsFile;
use eggdb_storage::{BTree, CommitStamp};

/// A consistent, read-only view of a [`Database`]: the last commit before
/// [`Database::begin_read`], unchanged by any commit after it, for as long as it lives.
///
/// Any number of read transactions can be open at once, on any thread, alongside the write
/// transaction; none of them waits for another. Dropping it ends it. A read transaction that
/// lives across many commits keeps the pages it still needs in the snapshot memory (see
/// [`Options::snapshot_memory_bytes`](crate::Options::snapshot_memory_bytes)); if the writer
/// needs that memory, it expires, and its reads fail with
/// [`ErrorKind::SnapshotTooOld`](crate::ErrorKind::SnapshotTooOld).
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let path = std::env::temp_dir().join(format!("eggdb-doc-read-tx-{}.db", std::process::id()));
/// let db = eggdb::Database::open(&path)?;
/// db.put("color", "red")?;
/// let before = db.begin_read()?;
/// db.put("color", "blue")?;
/// assert_eq!(before.get("color")?, Some(b"red".to_vec()));
/// assert_eq!(db.get("color")?, Some(b"blue".to_vec()));
/// # drop(before);
/// # drop(db);
/// # std::fs::remove_file(&path)?;
/// # Ok(()) }
/// ```
pub struct ReadTransaction<'db> {
    inner: eggdb_storage::ReadTransaction<'db, OsFile>,
    tree: Option<BTree>,
}

impl<'db> ReadTransaction<'db> {
    pub(crate) fn begin(database: &'db Database) -> Result<Self> {
        let inner = database.store().begin_read().map_err(Error::storage)?;
        let tree = database.tree_at(inner.catalog_root(), |root| inner.open_tree(root))?;
        Ok(Self { inner, tree })
    }

    /// The value stored under `key`, or `None` when the key is absent.
    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        let Some(tree) = &self.tree else {
            return Ok(None);
        };
        self.inner.get(tree, key.as_ref()).map_err(Error::storage)
    }

    /// Calls `read` with the value stored under `key` where it lies, without copying it out,
    /// and returns what `read` returns, or `None` when the key is absent. The database is held
    /// while `read` runs, so `read` must not use the same database: beginning, using or
    /// dropping one of its transactions inside it deadlocks.
    pub fn get_with<R>(
        &self,
        key: impl AsRef<[u8]>,
        read: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>> {
        let Some(tree) = &self.tree else {
            return Ok(None);
        };
        self.inner
            .get_with(tree, key.as_ref(), read)
            .map_err(Error::storage)
    }

    /// The entries with keys in `range`, in key order; `.rev()` gives them largest key first.
    /// See [`Scan`] and [`KeyRange`].
    pub fn scan(&self, range: impl KeyRange) -> Scan<'_> {
        Scan::between(Reader::Snapshot(&self.inner), self.tree, &range)
    }

    /// The entries whose keys start with `prefix`, in key order; `.rev()` gives them largest
    /// key first. The empty prefix scans every entry.
    pub fn scan_prefix(&self, prefix: impl AsRef<[u8]>) -> Scan<'_> {
        Scan::prefix(Reader::Snapshot(&self.inner), self.tree, prefix.as_ref())
    }

    /// The stamp of the commit this transaction sees, or [`CommitStamp::NONE`] when nothing
    /// was committed to the database before it began.
    pub fn stamp(&self) -> CommitStamp {
        self.inner.stamp()
    }
}

impl std::fmt::Debug for ReadTransaction<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadTransaction")
            .field("stamp", &self.stamp())
            .finish_non_exhaustive()
    }
}

/// The one open write transaction of a [`Database`], from [`Database::begin_write`]. It sees
/// the last commit and its own changes; nobody else sees them before
/// [`WriteTransaction::commit`].
///
/// **Ending it.** [`WriteTransaction::commit`] makes every change visible and durable as the
/// database's [`Durability`](crate::Durability) promises, atomically: after a crash, the
/// database holds all of them or none. [`WriteTransaction::rollback`] drops them, and so
/// does dropping the transaction, for example when `?` returns early.
///
/// **Failures.** A key or value over the size limits fails with
/// [`ErrorKind::TooLarge`](crate::ErrorKind::TooLarge) and changes nothing, and the
/// transaction stays open. Any other failure of a change, such as
/// [`ErrorKind::CacheFull`](crate::ErrorKind::CacheFull) when the transaction outgrows the
/// page cache, rolls the whole transaction back at once: every later call fails with
/// [`ErrorKind::TransactionFailed`](crate::ErrorKind::TransactionFailed), and the next writer
/// can begin.
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let path = std::env::temp_dir().join(format!("eggdb-doc-write-tx-{}.db", std::process::id()));
/// let db = eggdb::Database::open(&path)?;
/// let mut tx = db.begin_write()?;
/// tx.put("a", "1")?;
/// tx.put("b", "2")?;
/// assert!(tx.delete("a")?);
/// assert_eq!(tx.get("b")?, Some(b"2".to_vec()));
/// tx.commit()?;
/// assert_eq!(db.get("a")?, None);
/// # drop(db);
/// # std::fs::remove_file(&path)?;
/// # Ok(()) }
/// ```
pub struct WriteTransaction<'db> {
    database: &'db Database,
    inner: eggdb_storage::WriteTransaction<'db, OsFile>,
    tree: Option<BTree>,
    created_tree: bool,
    turn: WriterTurn<'db>,
}

impl<'db> WriteTransaction<'db> {
    pub(crate) fn begin(database: &'db Database, turn: WriterTurn<'db>) -> Result<Self> {
        let inner = database.store().begin_write().map_err(Error::storage)?;
        let tree = database.tree_at(inner.catalog_root(), |root| inner.open_tree(root))?;
        Ok(Self {
            database,
            inner,
            tree,
            created_tree: false,
            turn,
        })
    }

    /// The value stored under `key`, this transaction's changes included, or `None` when the
    /// key is absent.
    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        let Some(tree) = &self.tree else {
            return Ok(None);
        };
        self.inner.get(tree, key.as_ref()).map_err(Error::storage)
    }

    /// Calls `read` with the value stored under `key`, this transaction's changes included,
    /// where it lies, and returns what `read` returns, or `None` when the key is absent. As
    /// with [`ReadTransaction::get_with`], `read` must not use the same database.
    pub fn get_with<R>(
        &self,
        key: impl AsRef<[u8]>,
        read: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>> {
        let Some(tree) = &self.tree else {
            return Ok(None);
        };
        self.inner
            .get_with(tree, key.as_ref(), read)
            .map_err(Error::storage)
    }

    /// The entries with keys in `range`, this transaction's changes included, in key order;
    /// `.rev()` gives them largest key first. See [`Scan`] and [`KeyRange`].
    pub fn scan(&self, range: impl KeyRange) -> Scan<'_> {
        Scan::between(Reader::Writer(&self.inner), self.tree, &range)
    }

    /// The entries whose keys start with `prefix`, this transaction's changes included, in key
    /// order; `.rev()` gives them largest key first. The empty prefix scans every entry.
    pub fn scan_prefix(&self, prefix: impl AsRef<[u8]>) -> Scan<'_> {
        Scan::prefix(Reader::Writer(&self.inner), self.tree, prefix.as_ref())
    }

    /// Stores `value` under `key`, replacing the value there. Any byte string is a key, the
    /// empty one included, up to [`Database::max_key_bytes`], and any byte string is a value,
    /// up to [`Database::max_value_bytes`].
    ///
    /// A value too large to sit beside its key in a B+tree leaf is stored on overflow pages
    /// of its own. Each of them stays in the page cache until the commit, like every page a
    /// write transaction changes, so a value also needs about its size in cache: one larger
    /// than [`Options::cache_bytes`](crate::Options::cache_bytes) allows fails with
    /// [`ErrorKind::TooLarge`](crate::ErrorKind::TooLarge) and changes nothing.
    pub fn put(&mut self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()> {
        self.put_bytes(key.as_ref(), value.as_ref())
    }

    fn put_bytes(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let tree = match self.tree {
            Some(tree) => tree,
            None => {
                self.inner.check_fits(key, value).map_err(Error::storage)?;
                self.create_tree()?
            }
        };
        self.inner.put(&tree, key, value).map_err(Error::storage)
    }

    /// Stores `value` under `key` only when `key` is absent, and returns whether it stored
    /// it; a key that is there keeps its value. It finds the place for the key once, where a
    /// [`get`](Self::get) followed by a [`put`](Self::put) searches twice. Limits and failures
    /// are those of [`put`](Self::put).
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-put-if-absent-{}.db", std::process::id()));
    /// let db = eggdb::Database::open(&path)?;
    /// let mut tx = db.begin_write()?;
    /// assert!(tx.put_if_absent("a", "1")?);
    /// assert!(!tx.put_if_absent("a", "2")?);
    /// assert_eq!(tx.get("a")?, Some(b"1".to_vec()));
    /// tx.commit()?;
    /// # drop(db);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(()) }
    /// ```
    pub fn put_if_absent(
        &mut self,
        key: impl AsRef<[u8]>,
        value: impl AsRef<[u8]>,
    ) -> Result<bool> {
        let (key, value) = (key.as_ref(), value.as_ref());
        let tree = match self.tree {
            Some(tree) => tree,
            None => {
                self.inner.check_fits(key, value).map_err(Error::storage)?;
                self.create_tree()?
            }
        };
        self.inner
            .put_if_absent(&tree, key, value)
            .map_err(Error::storage)
    }

    /// Removes `key` and its value, and returns whether the key was there.
    pub fn delete(&mut self, key: impl AsRef<[u8]>) -> Result<bool> {
        let Some(tree) = self.tree else {
            return Ok(false);
        };
        self.inner
            .delete(&tree, key.as_ref())
            .map_err(Error::storage)
    }

    /// Commits every change atomically and returns the commit's stamp, which is above every
    /// earlier commit's. Read transactions that begin afterwards see the changes.
    ///
    /// On an error nothing is committed, except after a failed write or sync of the file:
    /// then the commit may or may not have reached the disk, and the database refuses
    /// everything with [`ErrorKind::NeedsReopen`](crate::ErrorKind::NeedsReopen) until it is
    /// opened again.
    pub fn commit(self) -> Result<CommitStamp> {
        let Self {
            database,
            inner,
            tree,
            created_tree,
            turn,
        } = self;
        let stamp = inner.commit().map_err(Error::storage)?;
        if let Some(tree) = tree.filter(|_| created_tree) {
            database.remember_tree(tree);
        }
        drop(turn);
        Ok(stamp)
    }

    /// Drops every change of the transaction. Dropping the transaction does the same.
    pub fn rollback(self) -> Result<()> {
        self.inner.rollback().map_err(Error::storage)
    }

    fn create_tree(&mut self) -> Result<BTree> {
        let tree = self.inner.create_tree().map_err(Error::storage)?;
        self.inner.set_catalog_root(tree.root());
        self.tree = Some(tree);
        self.created_tree = true;
        Ok(tree)
    }
}

impl std::fmt::Debug for WriteTransaction<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteTransaction")
            .field("began_after", &self.inner.began_after())
            .finish_non_exhaustive()
    }
}
