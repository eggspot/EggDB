use crate::args;
use crate::database::EggdbDb;
use crate::status::{EGGDB_ERR_READ_TRANSACTION, Failure, guarded};
use eggdb::{Database, ReadTransaction, WriteTransaction};
use std::ffi::{c_char, c_int};
use std::sync::Arc;
use std::time::Duration;

pub(crate) enum Transaction {
    Read(ReadTransaction<'static>),
    Write(WriteTransaction<'static>),
}

/// A transaction (`eggdb_txn` in `eggdb.h`), from [`eggdb_begin_read`] or
/// [`eggdb_begin_write`], ended by [`eggdb_commit`] or [`eggdb_rollback`]. One call at a time
/// may use it, from any thread.
pub struct EggdbTxn {
    pub(crate) transaction: Transaction,
    database: Arc<Database>,
}

impl std::fmt::Debug for EggdbTxn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.transaction {
            Transaction::Read(_) => "read",
            Transaction::Write(_) => "write",
        };
        f.debug_struct("EggdbTxn")
            .field("kind", &kind)
            .field("database", &self.database.path())
            .finish()
    }
}

impl EggdbTxn {
    fn writer(&mut self) -> Result<&mut WriteTransaction<'static>, Failure> {
        match &mut self.transaction {
            Transaction::Write(transaction) => Ok(transaction),
            Transaction::Read(_) => Err(Failure::new(
                EGGDB_ERR_READ_TRANSACTION,
                "this is a read transaction, which cannot change data; begin a write transaction with eggdb_begin_write",
            )),
        }
    }

    pub(crate) fn check_collection(&mut self, collection: Option<&str>) -> Result<(), Failure> {
        let Some(name) = collection else {
            return Ok(());
        };
        match &mut self.transaction {
            Transaction::Read(transaction) => transaction.collection(name).map(|_| ())?,
            Transaction::Write(transaction) => transaction.collection(name).map(|_| ())?,
        }
        Ok(())
    }
}

pub(crate) fn shared_database(database: &Arc<Database>) -> (Arc<Database>, &'static Database) {
    let shared = Arc::clone(database);
    // SAFETY: the reference points into the Arc's allocation, which never moves and lives as
    // long as one clone does; every handle that keeps the reference also keeps the clone
    // returned beside it and drops the reference's users first (field order).
    let database: &'static Database = unsafe { &*Arc::as_ptr(&shared) };
    (shared, database)
}

unsafe fn begin(
    db: *mut EggdbDb,
    txn: *mut *mut EggdbTxn,
    start: fn(&'static Database) -> eggdb::Result<Transaction>,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one handle at `txn`.
        unsafe { args::write(txn, std::ptr::null_mut(), "txn") }?;
        // SAFETY: eggdb.h asks for a live handle from eggdb_open.
        let db = unsafe { args::handle(db, "db") }?;
        let (database, database_ref) = shared_database(&db.database);
        let transaction = start(database_ref)?;
        let handle = args::give(EggdbTxn {
            transaction,
            database,
        });
        // SAFETY: as for the first write to `txn`.
        unsafe { args::write(txn, handle, "txn") }
    })
}

/// Begins a read transaction, a snapshot of the last commit, and puts its handle in `*txn`
/// (NULL on failure). Any number can be open at once, beside the write transaction.
///
/// # Safety
/// `db` is a handle from `eggdb_open` and `txn` points to writable memory for one handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_begin_read(db: *mut EggdbDb, txn: *mut *mut EggdbTxn) -> c_int {
    // SAFETY: the caller's promises for `db` and `txn` are the ones `begin` needs.
    unsafe {
        begin(db, txn, |database| {
            database.begin_read().map(Transaction::Read)
        })
    }
}

/// Begins the write transaction and puts its handle in `*txn` (NULL on failure). While another
/// is open it waits up to five seconds, then fails with `EGGDB_ERR_WRITER_ACTIVE`; it fails at
/// once when this thread began the open one.
///
/// # Safety
/// `db` is a handle from `eggdb_open` and `txn` points to writable memory for one handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_begin_write(db: *mut EggdbDb, txn: *mut *mut EggdbTxn) -> c_int {
    // SAFETY: the caller's promises for `db` and `txn` are the ones `begin` needs.
    unsafe {
        begin(db, txn, |database| {
            database.begin_write().map(Transaction::Write)
        })
    }
}

/// Ends the transaction and frees its handle: a write transaction commits every change
/// atomically, a read transaction just ends. `*stamp` gets the commit's stamp (the snapshot's
/// for a read transaction) when it is not NULL. On a failure nothing is committed.
///
/// # Safety
/// `txn` is a handle from `eggdb_begin_read` or `eggdb_begin_write` not ended yet; it must not
/// be used afterwards, whatever the status. `stamp` is NULL or writable memory for a
/// `uint64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_commit(txn: *mut EggdbTxn, stamp: *mut u64) -> c_int {
    guarded(|| {
        // SAFETY: the caller gives the handle up with this call (eggdb.h).
        let Some(handle) = (unsafe { args::take(txn) }) else {
            return Err(Failure::invalid_argument("txn is NULL"));
        };
        let EggdbTxn {
            transaction,
            database,
        } = *handle;
        let committed = match transaction {
            Transaction::Read(transaction) => Ok(transaction.stamp()),
            Transaction::Write(transaction) => transaction.commit(),
        };
        drop(database);
        let committed = committed?;
        // SAFETY: eggdb.h asks for NULL or writable memory for one uint64_t.
        unsafe { args::write_if_wanted(stamp, committed.bits()) };
        Ok(())
    })
}

/// Ends the transaction without committing and frees its handle: a write transaction's changes
/// are dropped, a read transaction just ends. NULL does nothing.
///
/// # Safety
/// `txn` is NULL or a handle from `eggdb_begin_read` or `eggdb_begin_write` not ended yet; it
/// must not be used afterwards, whatever the status.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_rollback(txn: *mut EggdbTxn) -> c_int {
    guarded(|| {
        // SAFETY: the caller gives the handle up with this call (eggdb.h).
        let Some(handle) = (unsafe { args::take(txn) }) else {
            return Ok(());
        };
        let EggdbTxn {
            transaction,
            database,
        } = *handle;
        let rolled_back = match transaction {
            Transaction::Read(_ended) => Ok(()),
            Transaction::Write(transaction) => transaction.rollback(),
        };
        drop(database);
        rolled_back.map_err(Failure::from)
    })
}

/// Reads the value stored under `key` in `collection` (NULL: the default key space). When the
/// key is there, `*value` gets a buffer with its bytes, the caller's to free with `eggdb_free`
/// and `*value_len`, and is never NULL, even for an empty value; when it is absent or has
/// expired, `*value` is NULL and `*value_len` 0.
///
/// # Safety
/// `txn` is a live transaction handle, `collection` NULL or a NUL-terminated string, `key`
/// points to `key_len` bytes (or is anything when `key_len` is 0), and `value` and
/// `value_len` point to writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_get(
    txn: *mut EggdbTxn,
    collection: *const c_char,
    key: *const u8,
    key_len: usize,
    value: *mut *mut u8,
    value_len: *mut usize,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one pointer at `value`.
        unsafe { args::write(value, std::ptr::null_mut(), "value") }?;
        // SAFETY: eggdb.h asks for writable memory for one size_t at `value_len`.
        unsafe { args::write(value_len, 0, "value_len") }?;
        // SAFETY: eggdb.h asks for a live transaction handle used by this call alone.
        let txn = unsafe { args::handle_mut(txn, "txn") }?;
        // SAFETY: eggdb.h asks for NULL or a NUL-terminated collection name.
        let collection = unsafe { args::optional_text(collection, "collection") }?;
        // SAFETY: eggdb.h asks for `key_len` readable bytes at `key`.
        let key = unsafe { args::bytes(key, key_len, "key") }?;
        let found: Option<Box<[u8]>> = match (&mut txn.transaction, collection) {
            (Transaction::Read(transaction), None) => transaction.get_with(key, owned)?,
            (Transaction::Read(transaction), Some(name)) => {
                transaction.collection(name)?.get_with(key, owned)?
            }
            (Transaction::Write(transaction), None) => transaction.get_with(key, owned)?,
            (Transaction::Write(transaction), Some(name)) => {
                transaction.collection(name)?.get_with(key, owned)?
            }
        };
        if let Some(found) = found {
            let (data, len) = args::library_bytes(found);
            // SAFETY: as for the first write to `value`.
            unsafe { args::write(value, data, "value") }?;
            // SAFETY: as for the first write to `value_len`.
            unsafe { args::write(value_len, len, "value_len") }?;
        }
        Ok(())
    })
}

/// Stores `value` under `key` in `collection` (NULL: the default key space), replacing the
/// value there and any time to live.
///
/// # Safety
/// `txn` is a live write transaction handle, `collection` NULL or a NUL-terminated string, and
/// `key` and `value` point to `key_len` and `value_len` bytes (anything when the length is 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_put(
    txn: *mut EggdbTxn,
    collection: *const c_char,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live transaction handle used by this call alone.
        let txn = unsafe { args::handle_mut(txn, "txn") }?;
        // SAFETY: eggdb.h asks for NULL or a NUL-terminated collection name.
        let collection = unsafe { args::optional_text(collection, "collection") }?;
        // SAFETY: eggdb.h asks for `key_len` readable bytes at `key`.
        let key = unsafe { args::bytes(key, key_len, "key") }?;
        // SAFETY: eggdb.h asks for `value_len` readable bytes at `value`.
        let value = unsafe { args::bytes(value, value_len, "value") }?;
        let writer = txn.writer()?;
        match collection {
            None => writer.put(key, value)?,
            Some(name) => writer.collection(name)?.put(key, value)?,
        }
        Ok(())
    })
}

/// Stores `value` under `key` in `collection` (NULL: the default key space) with a time to
/// live of `ttl_ms` milliseconds, after which no read sees it and commits remove it.
///
/// # Safety
/// As for `eggdb_put`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_put_with_expiry(
    txn: *mut EggdbTxn,
    collection: *const c_char,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
    ttl_ms: u64,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live transaction handle used by this call alone.
        let txn = unsafe { args::handle_mut(txn, "txn") }?;
        // SAFETY: eggdb.h asks for NULL or a NUL-terminated collection name.
        let collection = unsafe { args::optional_text(collection, "collection") }?;
        // SAFETY: eggdb.h asks for `key_len` readable bytes at `key`.
        let key = unsafe { args::bytes(key, key_len, "key") }?;
        // SAFETY: eggdb.h asks for `value_len` readable bytes at `value`.
        let value = unsafe { args::bytes(value, value_len, "value") }?;
        let ttl = Duration::from_millis(ttl_ms);
        let writer = txn.writer()?;
        match collection {
            None => writer.put_with_expiry(key, value, ttl)?,
            Some(name) => writer.collection(name)?.put_with_expiry(key, value, ttl)?,
        }
        Ok(())
    })
}

/// Removes `key` and its value from `collection` (NULL: the default key space); `*deleted`
/// gets 1 when the key was there and 0 when it was not, when it is not NULL.
///
/// # Safety
/// `txn` is a live write transaction handle, `collection` NULL or a NUL-terminated string,
/// `key` points to `key_len` bytes, and `deleted` is NULL or writable memory for an `int`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_delete(
    txn: *mut EggdbTxn,
    collection: *const c_char,
    key: *const u8,
    key_len: usize,
    deleted: *mut c_int,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live transaction handle used by this call alone.
        let txn = unsafe { args::handle_mut(txn, "txn") }?;
        // SAFETY: eggdb.h asks for NULL or a NUL-terminated collection name.
        let collection = unsafe { args::optional_text(collection, "collection") }?;
        // SAFETY: eggdb.h asks for `key_len` readable bytes at `key`.
        let key = unsafe { args::bytes(key, key_len, "key") }?;
        let writer = txn.writer()?;
        let was_there = match collection {
            None => writer.delete(key)?,
            Some(name) => writer.collection(name)?.delete(key)?,
        };
        // SAFETY: eggdb.h asks for NULL or writable memory for one int.
        unsafe { args::write_if_wanted(deleted, args::flag(was_there)) };
        Ok(())
    })
}

/// Gives `key` in `collection` (NULL: the default key space) a time to live of `ttl_ms`
/// milliseconds from now, replacing any it had; 0 removes the key. `*found` gets 1 when the key
/// was there and 0 when it was not (an absent key stays absent), when it is not NULL.
///
/// # Safety
/// As for `eggdb_delete`, with `found` in place of `deleted`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_set_expiry(
    txn: *mut EggdbTxn,
    collection: *const c_char,
    key: *const u8,
    key_len: usize,
    ttl_ms: u64,
    found: *mut c_int,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live transaction handle used by this call alone.
        let txn = unsafe { args::handle_mut(txn, "txn") }?;
        // SAFETY: eggdb.h asks for NULL or a NUL-terminated collection name.
        let collection = unsafe { args::optional_text(collection, "collection") }?;
        // SAFETY: eggdb.h asks for `key_len` readable bytes at `key`.
        let key = unsafe { args::bytes(key, key_len, "key") }?;
        let ttl = Duration::from_millis(ttl_ms);
        let writer = txn.writer()?;
        let was_there = match collection {
            None => writer.set_expiry(key, ttl)?,
            Some(name) => writer.collection(name)?.set_expiry(key, ttl)?,
        };
        // SAFETY: eggdb.h asks for NULL or writable memory for one int.
        unsafe { args::write_if_wanted(found, args::flag(was_there)) };
        Ok(())
    })
}

/// Removes the time to live of `key` in `collection` (NULL: the default key space), so it no
/// longer expires; `*cleared` gets 1 when it had one and 0 when not, when it is not NULL.
///
/// # Safety
/// As for `eggdb_delete`, with `cleared` in place of `deleted`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_clear_expiry(
    txn: *mut EggdbTxn,
    collection: *const c_char,
    key: *const u8,
    key_len: usize,
    cleared: *mut c_int,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live transaction handle used by this call alone.
        let txn = unsafe { args::handle_mut(txn, "txn") }?;
        // SAFETY: eggdb.h asks for NULL or a NUL-terminated collection name.
        let collection = unsafe { args::optional_text(collection, "collection") }?;
        // SAFETY: eggdb.h asks for `key_len` readable bytes at `key`.
        let key = unsafe { args::bytes(key, key_len, "key") }?;
        let writer = txn.writer()?;
        let had_one = match collection {
            None => writer.clear_expiry(key)?,
            Some(name) => writer.collection(name)?.clear_expiry(key)?,
        };
        // SAFETY: eggdb.h asks for NULL or writable memory for one int.
        unsafe { args::write_if_wanted(cleared, args::flag(had_one)) };
        Ok(())
    })
}

fn owned(value: &[u8]) -> Box<[u8]> {
    value.into()
}

fn whole_millis(left: Duration) -> i64 {
    let millis = left.as_nanos().div_ceil(1_000_000);
    i64::try_from(millis).unwrap_or(i64::MAX)
}

/// Puts in `*ttl_ms` how many milliseconds `key` in `collection` (NULL: the default key space)
/// has left before it expires, rounded up, or -1 when it has no time to live or is absent.
///
/// # Safety
/// `txn` is a live transaction handle, `collection` NULL or a NUL-terminated string, `key`
/// points to `key_len` bytes, and `ttl_ms` points to writable memory for an `int64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_time_to_live(
    txn: *mut EggdbTxn,
    collection: *const c_char,
    key: *const u8,
    key_len: usize,
    ttl_ms: *mut i64,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one int64_t at `ttl_ms`.
        unsafe { args::write(ttl_ms, -1, "ttl_ms") }?;
        // SAFETY: eggdb.h asks for a live transaction handle used by this call alone.
        let txn = unsafe { args::handle_mut(txn, "txn") }?;
        // SAFETY: eggdb.h asks for NULL or a NUL-terminated collection name.
        let collection = unsafe { args::optional_text(collection, "collection") }?;
        // SAFETY: eggdb.h asks for `key_len` readable bytes at `key`.
        let key = unsafe { args::bytes(key, key_len, "key") }?;
        let left = match (&mut txn.transaction, collection) {
            (Transaction::Read(transaction), None) => transaction.time_to_live(key)?,
            (Transaction::Read(transaction), Some(name)) => {
                transaction.collection(name)?.time_to_live(key)?
            }
            (Transaction::Write(transaction), None) => transaction.time_to_live(key)?,
            (Transaction::Write(transaction), Some(name)) => {
                transaction.collection(name)?.time_to_live(key)?
            }
        };
        if let Some(left) = left {
            // SAFETY: as for the first write to `ttl_ms`.
            unsafe { args::write(ttl_ms, whole_millis(left), "ttl_ms") }?;
        }
        Ok(())
    })
}

/// Adds `by` (negative: subtracts) to the counter under `key` in `collection` (NULL: the
/// default key space), a decimal integer stored as text where an absent key counts as 0, and
/// puts the new count in `*count` when it is not NULL. A value that is not a counter, or a sum
/// outside `int64_t`, fails with `EGGDB_ERR_NOT_AN_INTEGER` and changes nothing.
///
/// # Safety
/// `txn` is a live write transaction handle, `collection` NULL or a NUL-terminated string,
/// `key` points to `key_len` bytes, and `count` is NULL or writable memory for an `int64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_increment(
    txn: *mut EggdbTxn,
    collection: *const c_char,
    key: *const u8,
    key_len: usize,
    by: i64,
    count: *mut i64,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live transaction handle used by this call alone.
        let txn = unsafe { args::handle_mut(txn, "txn") }?;
        // SAFETY: eggdb.h asks for NULL or a NUL-terminated collection name.
        let collection = unsafe { args::optional_text(collection, "collection") }?;
        // SAFETY: eggdb.h asks for `key_len` readable bytes at `key`.
        let key = unsafe { args::bytes(key, key_len, "key") }?;
        let writer = txn.writer()?;
        let counted = match collection {
            None => writer.increment(key, by)?,
            Some(name) => writer.collection(name)?.increment(key, by)?,
        };
        // SAFETY: eggdb.h asks for NULL or writable memory for one int64_t.
        unsafe { args::write_if_wanted(count, counted) };
        Ok(())
    })
}

/// Stores `new_value` under `key` in `collection` (NULL: the default key space), or removes the
/// key when `new_value` is NULL, only when the value there now is `expected` (NULL: the key is
/// absent); `*swapped` gets 1 when it did and 0 when not, when it is not NULL. An empty value
/// is a non-NULL pointer with a length of 0.
///
/// # Safety
/// `txn` is a live write transaction handle, `collection` NULL or a NUL-terminated string,
/// `key` points to `key_len` bytes, `expected` and `new_value` are NULL (with a length of 0) or
/// point to their lengths in bytes, and `swapped` is NULL or writable memory for an `int`.
#[allow(clippy::too_many_arguments)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_compare_and_set(
    txn: *mut EggdbTxn,
    collection: *const c_char,
    key: *const u8,
    key_len: usize,
    expected: *const u8,
    expected_len: usize,
    new_value: *const u8,
    new_value_len: usize,
    swapped: *mut c_int,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live transaction handle used by this call alone.
        let txn = unsafe { args::handle_mut(txn, "txn") }?;
        // SAFETY: eggdb.h asks for NULL or a NUL-terminated collection name.
        let collection = unsafe { args::optional_text(collection, "collection") }?;
        // SAFETY: eggdb.h asks for `key_len` readable bytes at `key`.
        let key = unsafe { args::bytes(key, key_len, "key") }?;
        // SAFETY: eggdb.h asks for NULL or `expected_len` readable bytes at `expected`.
        let expected = unsafe { args::optional_bytes(expected, expected_len, "expected") }?;
        // SAFETY: eggdb.h asks for NULL or `new_value_len` readable bytes at `new_value`.
        let new_value = unsafe { args::optional_bytes(new_value, new_value_len, "new_value") }?;
        let writer = txn.writer()?;
        let changed = match collection {
            None => writer.compare_and_set(key, expected, new_value)?,
            Some(name) => writer
                .collection(name)?
                .compare_and_set(key, expected, new_value)?,
        };
        // SAFETY: eggdb.h asks for NULL or writable memory for one int.
        unsafe { args::write_if_wanted(swapped, args::flag(changed)) };
        Ok(())
    })
}

/// Creates the collection `name`, a named key space of its own in the same file; `*created`
/// gets 1 when it was made and 0 when it was already there, when it is not NULL. Names are 1 to
/// 128 bytes of UTF-8 without whitespace or control characters.
///
/// # Safety
/// `txn` is a live write transaction handle, `name` a NUL-terminated string, and `created`
/// NULL or writable memory for an `int`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_create_collection(
    txn: *mut EggdbTxn,
    name: *const c_char,
    created: *mut c_int,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live transaction handle used by this call alone.
        let txn = unsafe { args::handle_mut(txn, "txn") }?;
        // SAFETY: eggdb.h asks for a NUL-terminated collection name.
        let name = unsafe { args::text(name, "name") }?;
        let made = txn.writer()?.create_collection(name)?;
        // SAFETY: eggdb.h asks for NULL or writable memory for one int.
        unsafe { args::write_if_wanted(created, args::flag(made)) };
        Ok(())
    })
}

/// Drops the collection `name` with every key in it; `*dropped` gets 1 when it was there and 0
/// when not, when it is not NULL.
///
/// # Safety
/// As for `eggdb_create_collection`, with `dropped` in place of `created`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_drop_collection(
    txn: *mut EggdbTxn,
    name: *const c_char,
    dropped: *mut c_int,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live transaction handle used by this call alone.
        let txn = unsafe { args::handle_mut(txn, "txn") }?;
        // SAFETY: eggdb.h asks for a NUL-terminated collection name.
        let name = unsafe { args::text(name, "name") }?;
        let was_there = txn.writer()?.drop_collection(name)?;
        // SAFETY: eggdb.h asks for NULL or writable memory for one int.
        unsafe { args::write_if_wanted(dropped, args::flag(was_there)) };
        Ok(())
    })
}

/// Lists the collections in name order: `*names` gets a buffer holding each name followed by a
/// NUL byte, the caller's to free with `eggdb_free` and `*names_len`, and `*count` the number of
/// names.
///
/// # Safety
/// `txn` is a live transaction handle, and `names`, `names_len` and `count` point to writable
/// memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_collection_names(
    txn: *mut EggdbTxn,
    names: *mut *mut u8,
    names_len: *mut usize,
    count: *mut usize,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one pointer at `names`.
        unsafe { args::write(names, std::ptr::null_mut(), "names") }?;
        // SAFETY: eggdb.h asks for writable memory for one size_t at `names_len`.
        unsafe { args::write(names_len, 0, "names_len") }?;
        // SAFETY: eggdb.h asks for writable memory for one size_t at `count`.
        unsafe { args::write(count, 0, "count") }?;
        // SAFETY: eggdb.h asks for a live transaction handle used by this call alone.
        let txn = unsafe { args::handle_mut(txn, "txn") }?;
        let listed = match &txn.transaction {
            Transaction::Read(transaction) => transaction.collection_names()?,
            Transaction::Write(transaction) => transaction.collection_names()?,
        };
        let mut joined = Vec::with_capacity(listed.iter().map(|name| name.len() + 1).sum());
        for name in &listed {
            joined.extend_from_slice(name.as_bytes());
            joined.push(0);
        }
        let (data, len) = args::library_bytes(joined.into_boxed_slice());
        // SAFETY: as for the first write to `names`.
        unsafe { args::write(names, data, "names") }?;
        // SAFETY: as for the first write to `names_len`.
        unsafe { args::write(names_len, len, "names_len") }?;
        // SAFETY: as for the first write to `count`.
        unsafe { args::write(count, listed.len(), "count") }
    })
}
