use crate::collection;
use crate::database::Database;
use crate::error::{Error, Result};
use crate::expiry::{self, Moment};
use crate::scan::{KeyRange, Reader, Scan};
use crate::writer_slot::WriterTurn;
use eggdb_platform::OsFile;
use eggdb_storage::{BTree, CommitStamp, Features};

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
    expiry_now: Option<Moment>,
}

impl<'db> ReadTransaction<'db> {
    pub(crate) fn begin(database: &'db Database) -> Result<Self> {
        let inner = database.store().begin_read().map_err(Error::storage)?;
        let tree = database.tree_at(inner.catalog_root(), |root| inner.open_tree(root))?;
        let expiry_now = database
            .has_expiring_keys()
            .then(|| database.now_after(inner.stamp()));
        Ok(Self {
            inner,
            tree,
            expiry_now,
        })
    }

    /// The value stored under `key` in the default key space, or `None` when the key is
    /// absent or has expired. Keys of [collections](Self::collection) are not in the default
    /// key space.
    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        let key = key.as_ref();
        if collection::is_reserved(key) {
            return Ok(None);
        }
        self.get_live(key)
    }

    pub(crate) fn get_raw(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let Some(tree) = &self.tree else {
            return Ok(None);
        };
        self.reader().get(tree, key)
    }

    pub(crate) fn get_live(&self, stored: &[u8]) -> Result<Option<Vec<u8>>> {
        let value = self.get_raw(stored)?;
        if value.is_some() && self.has_expired(stored)? {
            return Ok(None);
        }
        Ok(value)
    }

    pub(crate) fn get_live_with<R>(
        &self,
        stored: &[u8],
        read: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>> {
        let Some(tree) = &self.tree else {
            return Ok(None);
        };
        if self.has_expired(stored)? {
            return Ok(None);
        }
        self.reader().get_with(tree, stored, read)
    }

    fn has_expired(&self, stored: &[u8]) -> Result<bool> {
        match (&self.tree, self.expiry_now) {
            (Some(tree), Some(now)) => expiry::has_expired(self.reader(), tree, stored, now),
            _ => Ok(false),
        }
    }

    pub(crate) fn reader(&self) -> Reader<'_> {
        Reader::Snapshot(&self.inner)
    }

    pub(crate) fn tree(&self) -> Option<BTree> {
        self.tree
    }

    pub(crate) fn expiry_now(&self) -> Option<Moment> {
        self.expiry_now
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
        let key = key.as_ref();
        if collection::is_reserved(key) {
            return Ok(None);
        }
        self.get_live_with(key, read)
    }

    /// The entries of the default key space with keys in `range`, in key order; `.rev()`
    /// gives them largest key first. Expired keys are left out. See [`Scan`] and
    /// [`KeyRange`].
    pub fn scan(&self, range: impl KeyRange) -> Scan<'_> {
        Scan::between(Reader::Snapshot(&self.inner), self.tree, &range)
            .hiding_expired(self.expiry_now)
    }

    /// The entries of the default key space whose keys start with `prefix`, in key order;
    /// `.rev()` gives them largest key first. The empty prefix scans every entry. Expired keys
    /// are left out.
    pub fn scan_prefix(&self, prefix: impl AsRef<[u8]>) -> Scan<'_> {
        Scan::prefix(Reader::Snapshot(&self.inner), self.tree, prefix.as_ref())
            .hiding_expired(self.expiry_now)
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
    now: Moment,
    adds_expiring_keys: bool,
    sweeps_on_commit: bool,
    turn: WriterTurn<'db>,
}

impl<'db> WriteTransaction<'db> {
    pub(crate) fn begin(database: &'db Database, turn: WriterTurn<'db>) -> Result<Self> {
        let inner = database.store().begin_write().map_err(Error::storage)?;
        let tree = database.tree_at(inner.catalog_root(), |root| inner.open_tree(root))?;
        let now = database.now_after(inner.began_after());
        Ok(Self {
            database,
            inner,
            tree,
            created_tree: false,
            now,
            adds_expiring_keys: false,
            sweeps_on_commit: true,
            turn,
        })
    }

    /// The value stored under `key` in the default key space, this transaction's changes
    /// included, or `None` when the key is absent or has expired.
    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        let key = key.as_ref();
        if collection::is_reserved(key) {
            return Ok(None);
        }
        self.get_live(key)
    }

    pub(crate) fn get_raw(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let Some(tree) = &self.tree else {
            return Ok(None);
        };
        self.reader().get(tree, key)
    }

    pub(crate) fn reader(&self) -> Reader<'_> {
        Reader::Writer(&self.inner)
    }

    pub(crate) fn tree(&self) -> Option<BTree> {
        self.tree
    }

    pub(crate) fn max_key_bytes(&self) -> usize {
        self.database.max_key_bytes()
    }

    pub(crate) fn now(&self) -> Moment {
        self.now
    }

    pub(crate) fn expiry_now(&self) -> Option<Moment> {
        self.database.has_expiring_keys().then_some(self.now)
    }

    pub(crate) fn note_expiring_keys(&mut self) {
        self.adds_expiring_keys = true;
        self.database.note_expiring_keys();
    }

    pub(crate) fn skip_sweep_on_commit(&mut self) {
        self.sweeps_on_commit = false;
    }

    /// Calls `read` with the value stored under `key`, this transaction's changes included,
    /// where it lies, and returns what `read` returns, or `None` when the key is absent. As
    /// with [`ReadTransaction::get_with`], `read` must not use the same database.
    pub fn get_with<R>(
        &self,
        key: impl AsRef<[u8]>,
        read: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>> {
        let key = key.as_ref();
        if collection::is_reserved(key) {
            return Ok(None);
        }
        self.get_live_with(key, read)
    }

    /// The entries of the default key space with keys in `range`, this transaction's changes
    /// included, in key order; `.rev()` gives them largest key first. Expired keys are left
    /// out. See [`Scan`] and [`KeyRange`].
    pub fn scan(&self, range: impl KeyRange) -> Scan<'_> {
        Scan::between(Reader::Writer(&self.inner), self.tree, &range)
            .hiding_expired(self.expiry_now())
    }

    /// The entries of the default key space whose keys start with `prefix`, this transaction's
    /// changes included, in key order; `.rev()` gives them largest key first. The empty prefix
    /// scans every entry. Expired keys are left out.
    pub fn scan_prefix(&self, prefix: impl AsRef<[u8]>) -> Scan<'_> {
        Scan::prefix(Reader::Writer(&self.inner), self.tree, prefix.as_ref())
            .hiding_expired(self.expiry_now())
    }

    /// Stores `value` under `key` in the default key space, replacing the value there. Any byte
    /// string is a key, the empty one included, up to [`Database::max_key_bytes`], except those
    /// that start with the bytes EggDB keeps for [collections](Self::collection), `0xff C O L
    /// 0x00`, which fail with [`ErrorKind::ReservedKey`](crate::ErrorKind::ReservedKey). Any
    /// byte string is a value, up to [`Database::max_value_bytes`]. The key loses any time to
    /// live it had (see [`put_with_expiry`](Self::put_with_expiry)).
    ///
    /// A value too large to sit beside its key in a B+tree leaf is stored on overflow pages
    /// of its own. Each of them stays in the page cache until the commit, like every page a
    /// write transaction changes, so a value also needs about its size in cache: one larger
    /// than [`Options::cache_bytes`](crate::Options::cache_bytes) allows fails with
    /// [`ErrorKind::TooLarge`](crate::ErrorKind::TooLarge) and changes nothing.
    pub fn put(&mut self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()> {
        let key = key.as_ref();
        collection::refuse_reserved(key)?;
        self.put_live(key, value.as_ref())
    }

    pub(crate) fn put_raw(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let tree = match self.tree {
            Some(tree) => tree,
            None => {
                self.inner.check_fits(key, value).map_err(Error::storage)?;
                self.create_tree()?
            }
        };
        self.inner.put(&tree, key, value).map_err(Error::storage)
    }

    /// Stores `value` under `key` only when `key` is absent or has expired, and returns whether
    /// it stored it; a key that is there keeps its value and its time to live. It finds the place for the key once, where a
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
        let key = key.as_ref();
        collection::refuse_reserved(key)?;
        self.put_if_absent_live(key, value.as_ref())
    }

    pub(crate) fn put_if_absent_raw(&mut self, key: &[u8], value: &[u8]) -> Result<bool> {
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

    /// Removes `key` and its value from the default key space, and returns whether the key was
    /// there; an expired key was not, though what is left of it goes too.
    pub fn delete(&mut self, key: impl AsRef<[u8]>) -> Result<bool> {
        let key = key.as_ref();
        if collection::is_reserved(key) {
            return Ok(false);
        }
        self.delete_live(key)
    }

    pub(crate) fn delete_raw(&mut self, key: &[u8]) -> Result<bool> {
        let Some(tree) = self.tree else {
            return Ok(false);
        };
        self.inner.delete(&tree, key).map_err(Error::storage)
    }

    /// Commits every change atomically and returns the commit's stamp, which is above every
    /// earlier commit's. Read transactions that begin afterwards see the changes. In a
    /// database that has keys with a time to live, the commit also removes up to 32 keys
    /// whose time has run out.
    ///
    /// On an error nothing is committed, except after a failed write or sync of the file:
    /// then the commit may or may not have reached the disk, and the database refuses
    /// everything with [`ErrorKind::NeedsReopen`](crate::ErrorKind::NeedsReopen) until it is
    /// opened again.
    pub fn commit(mut self) -> Result<CommitStamp> {
        if self.sweeps_on_commit && self.database.has_expiring_keys() {
            self.sweep_before_commit()?;
        }
        if self.adds_expiring_keys {
            self.inner.add_features(Features {
                read_only_compatible: Features::EXPIRING_KEYS,
                ..Features::default()
            });
        }
        let Self {
            database,
            inner,
            tree,
            created_tree,
            turn,
            ..
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
