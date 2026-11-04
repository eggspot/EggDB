use crate::collection_counts::EntryCount;
use crate::database::Database;
use crate::error::{Error, ErrorKind, Result};
use crate::expiry::{self, Moment};
use crate::scan::{KeyRange, Reader, Scan};
use crate::transaction::{ReadTransaction, WriteTransaction};
use eggdb_storage::BTree;
use std::time::Duration;

pub(crate) const COLLECTIONS_PREFIX: &[u8] = b"\xffCOL\x00";
pub(crate) const COLLECTIONS_END: &[u8] = b"\xffCOL\x01";
const CATALOG_TAG: u8 = 0x01;
const NAME_TAG: u8 = 0x02;
const ENTRY_TAG: u8 = 0x03;
const DROPPING_TAG: u8 = 0x06;
const COUNT_TAG: u8 = 0x07;
const RECORD_VERSION: u8 = 1;
const RECORD_BYTES: usize = 5;
const FIRST_NUMBER: u32 = 1;
const DROP_STEP_KEYS: usize = 512;
const DROPPED_KEYS_PER_COMMIT: usize = 32;

pub(crate) const DROPS_UNKNOWN: u8 = 0;
const DROPS_NONE: u8 = 1;
const DROPS_PENDING: u8 = 2;
pub(crate) const ENTRY_PREFIX_BYTES: usize = COLLECTIONS_PREFIX.len() + 1 + 4;

pub(crate) const MAX_COLLECTION_NAME_BYTES: usize = 128;

pub(crate) fn is_reserved(key: &[u8]) -> bool {
    key.starts_with(COLLECTIONS_PREFIX)
}

pub(crate) fn refuse_reserved(key: &[u8]) -> Result<()> {
    if !is_reserved(key) {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::ReservedKey,
        "keys that start with the bytes 0xff 'C' 'O' 'L' 0x00 hold EggDB's collections and keys' times to live, so the default key space refuses them; store the key in a collection, or choose another first byte",
    ))
}

pub(crate) fn check_collection_name(name: &str) -> Result<()> {
    let problem = if name.is_empty() {
        "is empty"
    } else if name.len() > MAX_COLLECTION_NAME_BYTES {
        "is longer than 128 bytes"
    } else if name.chars().any(|c| c.is_whitespace() || c.is_control()) {
        "has whitespace or a control character"
    } else {
        return Ok(());
    };
    let shown: String = name
        .chars()
        .take(MAX_COLLECTION_NAME_BYTES)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    Err(Error::new(
        ErrorKind::InvalidCollectionName,
        format!(
            "the collection name \"{shown}\" {problem}; a name is 1 to {MAX_COLLECTION_NAME_BYTES} bytes of text without whitespace or control characters"
        ),
    ))
}

fn tagged(tag: u8, rest: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(COLLECTIONS_PREFIX.len() + 1 + rest.len());
    key.extend_from_slice(COLLECTIONS_PREFIX);
    key.push(tag);
    key.extend_from_slice(rest);
    key
}

fn catalog_key() -> Vec<u8> {
    tagged(CATALOG_TAG, b"")
}

fn names_prefix() -> Vec<u8> {
    tagged(NAME_TAG, b"")
}

fn name_key(name: &str) -> Vec<u8> {
    tagged(NAME_TAG, name.as_bytes())
}

fn entries_prefix(number: u32) -> Vec<u8> {
    tagged(ENTRY_TAG, &number.to_be_bytes())
}

pub(crate) fn count_key(number: u32) -> Vec<u8> {
    tagged(COUNT_TAG, &number.to_be_bytes())
}

pub(crate) fn entry_number(key: &[u8]) -> Option<(u32, u64)> {
    let rest = key
        .strip_prefix(COLLECTIONS_PREFIX)?
        .strip_prefix(&[ENTRY_TAG])?;
    let (number, own_key) = rest.split_first_chunk::<4>()?;
    Some((u32::from_be_bytes(*number), own_key.len() as u64))
}

pub(crate) fn counted_number(key: &[u8]) -> Option<u32> {
    let rest = key
        .strip_prefix(COLLECTIONS_PREFIX)?
        .strip_prefix(&[COUNT_TAG])?;
    let number: [u8; 4] = rest.try_into().ok()?;
    Some(u32::from_be_bytes(number))
}

fn droppings_prefix() -> Vec<u8> {
    tagged(DROPPING_TAG, b"")
}

fn dropping_key(number: u32) -> Vec<u8> {
    tagged(DROPPING_TAG, &number.to_be_bytes())
}

fn dropping_record(name: &str) -> Vec<u8> {
    let mut record = Vec::with_capacity(1 + name.len());
    record.push(RECORD_VERSION);
    record.extend_from_slice(name.as_bytes());
    record
}

fn read_dropping_number(key: &[u8]) -> Result<u32> {
    let number: [u8; 4] = key.try_into().map_err(|_| {
        Error::new(
            ErrorKind::Corrupt,
            format!(
                "a dropped collection's marker is damaged: its key holds {} bytes after the tag, not a 4-byte number; restore from a backup or run `eggdb check`",
                key.len()
            ),
        )
    })?;
    Ok(u32::from_be_bytes(number))
}

fn record(number: u32) -> [u8; RECORD_BYTES] {
    let [a, b, c, d] = number.to_be_bytes();
    [RECORD_VERSION, a, b, c, d]
}

fn read_record(bytes: &[u8], what: &str) -> Result<u32> {
    match bytes {
        [RECORD_VERSION, a, b, c, d] => Ok(u32::from_be_bytes([*a, *b, *c, *d])),
        [version, ..] if *version > RECORD_VERSION => Err(Error::new(
            ErrorKind::Incompatible,
            format!(
                "{what} is in version {version} of its record, written by a newer EggDB; this release reads version {RECORD_VERSION}. Upgrade eggdb from https://github.com/eggspot/EggDB/releases"
            ),
        )),
        _ => Err(Error::new(
            ErrorKind::Corrupt,
            format!(
                "{what} is damaged: its record is {} bytes, not a version byte and a 4-byte number; restore from a backup or run `eggdb check`",
                bytes.len()
            ),
        )),
    }
}

fn not_found(name: &str) -> Error {
    Error::new(
        ErrorKind::CollectionNotFound,
        format!(
            "there is no collection named \"{name}\"; create it with create_collection (or `eggdb collection add <file> {name}`)"
        ),
    )
}

fn collection_number(record: Option<Vec<u8>>, name: &str) -> Result<u32> {
    match record {
        Some(bytes) => read_record(&bytes, &format!("the collection \"{name}\"")),
        None => Err(not_found(name)),
    }
}

fn names_in(mut scan: Scan<'_>) -> Result<Vec<String>> {
    let mut names = Vec::new();
    while let Some((name, _)) = scan.next_key_and_value_len()? {
        let name = std::str::from_utf8(name).map_err(|_| {
            Error::new(
                ErrorKind::Corrupt,
                "a collection's name in the catalog is not UTF-8 text; restore from a backup or run `eggdb check`",
            )
        })?;
        names.push(name.to_owned());
    }
    Ok(names)
}

/// One named collection of a [`Database`], from [`Database::collection`]: its own ordered map
/// from byte-string keys to byte-string values, beside the default key space and every other
/// collection in the same file. Each method is a transaction of one step, as
/// [`Database::get`], [`Database::put`] and [`Database::delete`] are for the default key
/// space; for several steps in one transaction, use [`ReadTransaction::collection`] and
/// [`WriteTransaction::collection`].
///
/// The collection must exist: create it with [`Database::create_collection`]. Every method
/// fails with [`ErrorKind::CollectionNotFound`] while it doesn't.
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let path = std::env::temp_dir().join(format!("eggdb-doc-collection-{}.db", std::process::id()));
/// let db = eggdb::Database::open(&path)?;
/// db.create_collection("users")?;
/// let users = db.collection("users");
/// users.put("1", "Ada")?;
/// db.put("1", "not a user")?;
/// assert_eq!(users.get("1")?, Some(b"Ada".to_vec()));
/// assert_eq!(db.get("1")?, Some(b"not a user".to_vec()));
/// assert_eq!(db.collection_names()?, ["users"]);
/// # drop(db);
/// # std::fs::remove_file(&path)?;
/// # Ok(()) }
/// ```
#[derive(Debug, Clone)]
pub struct Collection<'db> {
    database: &'db Database,
    name: Box<str>,
}

impl<'db> Collection<'db> {
    pub(crate) fn new(database: &'db Database, name: &str) -> Self {
        Self {
            database,
            name: name.into(),
        }
    }

    /// The collection's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The value stored under `key` in the collection as of the last commit, or `None`.
    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        self.database
            .read(|transaction| transaction.collection(&self.name)?.get(key))
    }

    /// Stores `value` under `key` in the collection and commits.
    pub fn put(&self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()> {
        self.database
            .write(|transaction| transaction.collection(&self.name)?.put(key, value))
    }

    /// Removes `key` from the collection and commits, and returns whether the key was there.
    /// Nothing is committed when it was absent.
    pub fn delete(&self, key: impl AsRef<[u8]>) -> Result<bool> {
        self.database
            .write_if(|transaction| transaction.collection(&self.name)?.delete(key))
    }

    /// Stores `value` under `key` in the collection with a time to live of `ttl`, and commits
    /// (see [`WriteTransaction::put_with_expiry`]).
    pub fn put_with_expiry(
        &self,
        key: impl AsRef<[u8]>,
        value: impl AsRef<[u8]>,
        ttl: Duration,
    ) -> Result<()> {
        self.database.write(|transaction| {
            transaction
                .collection(&self.name)?
                .put_with_expiry(key, value, ttl)
        })
    }

    /// Gives `key` in the collection a time to live of `ttl` and commits, and returns whether the
    /// key was there (see [`WriteTransaction::set_expiry`]).
    pub fn set_expiry(&self, key: impl AsRef<[u8]>, ttl: Duration) -> Result<bool> {
        self.database
            .write_if(|transaction| transaction.collection(&self.name)?.set_expiry(key, ttl))
    }

    /// Removes the time to live of `key` in the collection and commits, and returns whether it
    /// had one (see [`WriteTransaction::clear_expiry`]).
    pub fn clear_expiry(&self, key: impl AsRef<[u8]>) -> Result<bool> {
        self.database
            .write_if(|transaction| transaction.collection(&self.name)?.clear_expiry(key))
    }

    /// How long `key` in the collection has left before it expires as of the last commit, or
    /// `None` when it has no time to live or is absent.
    pub fn time_to_live(&self, key: impl AsRef<[u8]>) -> Result<Option<Duration>> {
        self.database
            .read(|transaction| transaction.collection(&self.name)?.time_to_live(key))
    }

    /// Adds `by` to the counter under `key` in the collection, commits, and returns the new
    /// count (see [`WriteTransaction::increment`]).
    pub fn increment(&self, key: impl AsRef<[u8]>, by: i64) -> Result<i64> {
        self.database
            .write(|transaction| transaction.collection(&self.name)?.increment(key, by))
    }

    /// Stores `new` under `key` in the collection (or removes it, for `None`) only when its
    /// value is `expected`, commits, and returns whether it did (see
    /// [`WriteTransaction::compare_and_set`]).
    pub fn compare_and_set(
        &self,
        key: impl AsRef<[u8]>,
        expected: Option<&[u8]>,
        new: Option<&[u8]>,
    ) -> Result<bool> {
        self.database.write_if(|transaction| {
            transaction
                .collection(&self.name)?
                .compare_and_set(key, expected, new)
        })
    }
}

/// A collection as a [`ReadTransaction`] sees it, from [`ReadTransaction::collection`]: the
/// same snapshot as the transaction, with the default key space's reading methods.
pub struct ReadCollection<'t> {
    reader: Reader<'t>,
    tree: Option<BTree>,
    base: Vec<u8>,
    name: Box<str>,
    expiry_now: Option<Moment>,
}

impl ReadCollection<'_> {
    /// The collection's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The value stored under `key` in the collection, or `None` when the key is absent or has
    /// expired.
    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        self.get_with(key, <[u8]>::to_vec)
    }

    fn has_expired(&self, tree: &BTree, stored: &[u8]) -> Result<bool> {
        match self.expiry_now {
            Some(now) => expiry::has_expired(self.reader, tree, stored, now),
            None => Ok(false),
        }
    }

    /// How long `key` in the collection has left before it expires, or `None` when it has no
    /// time to live or is absent, as [`ReadTransaction::time_to_live`] says.
    pub fn time_to_live(&self, key: impl AsRef<[u8]>) -> Result<Option<Duration>> {
        let stored = [self.base.as_slice(), key.as_ref()].concat();
        expiry::time_left(self.reader, self.tree, &stored, self.expiry_now)
    }

    /// Calls `read` with the value stored under `key` where it lies, as
    /// [`ReadTransaction::get_with`] does, or returns `None` when the key is absent.
    pub fn get_with<R>(
        &self,
        key: impl AsRef<[u8]>,
        read: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>> {
        let Some(tree) = &self.tree else {
            return Ok(None);
        };
        let stored = [self.base.as_slice(), key.as_ref()].concat();
        if self.has_expired(tree, &stored)? {
            return Ok(None);
        }
        self.reader.get_with(tree, &stored, read)
    }

    /// The collection's entries with keys in `range`, in key order; `.rev()` gives them
    /// largest key first. Expired keys are left out. See [`Scan`] and [`KeyRange`].
    pub fn scan(&self, range: impl KeyRange) -> Scan<'_> {
        Scan::within_between(self.reader, self.tree, &self.base, &range)
            .hiding_expired(self.expiry_now)
    }

    /// The collection's entries whose keys start with `prefix`, in key order; `.rev()` gives
    /// them largest key first. The empty prefix scans the whole collection. Expired keys are
    /// left out.
    pub fn scan_prefix(&self, prefix: impl AsRef<[u8]>) -> Scan<'_> {
        Scan::within_prefix(self.reader, self.tree, &self.base, prefix.as_ref())
            .hiding_expired(self.expiry_now)
    }
}

impl std::fmt::Debug for ReadCollection<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadCollection")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// A collection as the [`WriteTransaction`] sees and changes it, from
/// [`WriteTransaction::collection`]: the transaction's own changes included, with the default
/// key space's methods. Its changes commit or roll back with the transaction.
///
/// A key in a collection is at most [`Database::max_collection_key_bytes`] long, ten bytes less
/// than in the default key space; a longer one fails with
/// [`ErrorKind::TooLarge`](crate::ErrorKind::TooLarge) and changes nothing.
pub struct WriteCollection<'t, 'db> {
    transaction: &'t mut WriteTransaction<'db>,
    base: Vec<u8>,
    name: Box<str>,
}

impl WriteCollection<'_, '_> {
    /// The collection's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The value stored under `key` in the collection, this transaction's changes included, or
    /// `None` when the key is absent or has expired.
    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        self.transaction.get_live(&self.key(key.as_ref()))
    }

    /// Calls `read` with the value stored under `key` where it lies, as
    /// [`WriteTransaction::get_with`] does, or returns `None` when the key is absent.
    pub fn get_with<R>(
        &self,
        key: impl AsRef<[u8]>,
        read: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>> {
        self.transaction
            .get_live_with(&self.key(key.as_ref()), read)
    }

    /// The collection's entries with keys in `range`, this transaction's changes included, in
    /// key order; `.rev()` gives them largest key first. Expired keys are left out.
    pub fn scan(&self, range: impl KeyRange) -> Scan<'_> {
        Scan::within_between(
            self.transaction.reader(),
            self.transaction.tree(),
            &self.base,
            &range,
        )
        .hiding_expired(self.transaction.expiry_now())
    }

    /// The collection's entries whose keys start with `prefix`, this transaction's changes
    /// included, in key order; `.rev()` gives them largest key first. Expired keys are left
    /// out.
    pub fn scan_prefix(&self, prefix: impl AsRef<[u8]>) -> Scan<'_> {
        Scan::within_prefix(
            self.transaction.reader(),
            self.transaction.tree(),
            &self.base,
            prefix.as_ref(),
        )
        .hiding_expired(self.transaction.expiry_now())
    }

    /// Stores `value` under `key` in the collection, replacing the value there and any time to
    /// live. Limits and failures are those of [`WriteTransaction::put`], with keys ten bytes
    /// shorter.
    pub fn put(&mut self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()> {
        let key = self.checked_key(key.as_ref())?;
        self.transaction.put_live(&key, value.as_ref())
    }

    /// Stores `value` under `key` in the collection with a time to live of `ttl`, as
    /// [`WriteTransaction::put_with_expiry`] does; such a key is at most
    /// [`Database::max_collection_key_bytes`] less 14 bytes long.
    pub fn put_with_expiry(
        &mut self,
        key: impl AsRef<[u8]>,
        value: impl AsRef<[u8]>,
        ttl: Duration,
    ) -> Result<()> {
        let key = key.as_ref();
        let stored = self.checked_key(key)?;
        self.transaction
            .put_expiring(&stored, key.len(), value.as_ref(), ttl)
    }

    /// Gives `key` in the collection a time to live of `ttl` from now, and returns whether the
    /// key was there, as [`WriteTransaction::set_expiry`] does.
    pub fn set_expiry(&mut self, key: impl AsRef<[u8]>, ttl: Duration) -> Result<bool> {
        let key = key.as_ref();
        let stored = self.key(key);
        self.transaction.expire_stored(&stored, key.len(), ttl)
    }

    /// Removes the time to live of `key` in the collection, and returns whether it had one.
    pub fn clear_expiry(&mut self, key: impl AsRef<[u8]>) -> Result<bool> {
        let stored = self.key(key.as_ref());
        self.transaction.persist_stored(&stored)
    }

    /// How long `key` in the collection has left before it expires, this transaction's changes
    /// included, or `None` when it has no time to live or is absent.
    pub fn time_to_live(&self, key: impl AsRef<[u8]>) -> Result<Option<Duration>> {
        self.transaction
            .time_to_live_stored(&self.key(key.as_ref()))
    }

    /// Adds `by` to the counter under `key` in the collection and returns the new count, as
    /// [`WriteTransaction::increment`] does.
    pub fn increment(&mut self, key: impl AsRef<[u8]>, by: i64) -> Result<i64> {
        let stored = self.checked_key(key.as_ref())?;
        self.transaction.increment_stored(&stored, by)
    }

    /// Stores `new` under `key` in the collection (or removes it, for `None`) only when its
    /// value is `expected` (`None`: absent), and returns whether it did, as
    /// [`WriteTransaction::compare_and_set`] does.
    pub fn compare_and_set(
        &mut self,
        key: impl AsRef<[u8]>,
        expected: Option<&[u8]>,
        new: Option<&[u8]>,
    ) -> Result<bool> {
        let stored = match new {
            Some(_) => self.checked_key(key.as_ref())?,
            None => self.key(key.as_ref()),
        };
        self.transaction
            .compare_and_set_stored(&stored, expected, new)
    }

    /// Stores `value` under `key` in the collection only when `key` is absent, and returns
    /// whether it stored it, as [`WriteTransaction::put_if_absent`] does.
    pub fn put_if_absent(
        &mut self,
        key: impl AsRef<[u8]>,
        value: impl AsRef<[u8]>,
    ) -> Result<bool> {
        let key = self.checked_key(key.as_ref())?;
        self.transaction.put_if_absent_live(&key, value.as_ref())
    }

    /// Removes `key` and its value from the collection, and returns whether the key was there;
    /// an expired key was not.
    pub fn delete(&mut self, key: impl AsRef<[u8]>) -> Result<bool> {
        self.transaction.delete_live(&self.key(key.as_ref()))
    }

    fn key(&self, key: &[u8]) -> Vec<u8> {
        [self.base.as_slice(), key].concat()
    }

    fn checked_key(&self, key: &[u8]) -> Result<Vec<u8>> {
        let max = self
            .transaction
            .max_key_bytes()
            .saturating_sub(ENTRY_PREFIX_BYTES);
        if key.len() > max {
            return Err(Error::new(
                ErrorKind::TooLarge,
                format!(
                    "a key of {} bytes is too long for the collection \"{}\": a collection stores keys up to {max} bytes with this page size, ten fewer than the default key space",
                    key.len(),
                    self.name
                ),
            ));
        }
        Ok(self.key(key))
    }
}

impl std::fmt::Debug for WriteCollection<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteCollection")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// What [`Database::collection_stats`] counts in one collection: its entries and their bytes,
/// as the collection keeps them up to date on every write. Keys whose time to live has run out
/// count until the expiry sweep removes them, as a key's own deletion would.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct CollectionStats {
    /// The collection's name.
    pub name: String,
    /// How many keys it holds.
    pub entries: u64,
    /// The bytes of its keys together, as the collection's own keys (without the ten bytes
    /// that say which collection a key is in).
    pub key_bytes: u64,
    /// The bytes of its values together.
    pub value_bytes: u64,
}

/// How far [`Database::drop_collection_with_progress`] has come, after each of its commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct DropProgress {
    /// The keys removed so far, those of earlier unfinished drops included.
    pub removed: u64,
    /// The keys the collection held when the drop began, as its kept count says, or `None`
    /// for a collection without one (written by a release before counts were kept).
    pub entries: Option<u64>,
}

struct KeptRecord {
    name: String,
    number: u32,
    count: Option<EntryCount>,
}

impl KeptRecord {
    fn stats(&self, reader: Reader<'_>, tree: Option<BTree>) -> Result<CollectionStats> {
        let count = match self.count {
            Some(count) => count,
            None => counted_by_scan(reader, tree, self.number)?,
        };
        Ok(CollectionStats {
            name: self.name.clone(),
            entries: count.entries,
            key_bytes: count.key_bytes,
            value_bytes: count.value_bytes,
        })
    }
}

fn counted_by_scan(reader: Reader<'_>, tree: Option<BTree>, number: u32) -> Result<EntryCount> {
    let mut scan = Scan::within_prefix(reader, tree, &entries_prefix(number), b"");
    let mut count = EntryCount::default();
    while let Some((key, value_bytes)) = scan.next_key_and_value_len()? {
        count.add(key.len() as u64, value_bytes);
    }
    Ok(count)
}

fn kept_records(reader: Reader<'_>, tree: Option<BTree>) -> Result<Vec<KeptRecord>> {
    let names = names_in(Scan::within_prefix(reader, tree, &names_prefix(), b""))?;
    let Some(found) = tree else {
        return Ok(Vec::new());
    };
    names
        .into_iter()
        .map(|name| {
            let number = collection_number(reader.get(&found, &name_key(&name))?, &name)?;
            let count = reader
                .get(&found, &count_key(number))?
                .map(|record| EntryCount::read(&record))
                .transpose()?;
            Ok(KeptRecord {
                name,
                number,
                count,
            })
        })
        .collect()
}

fn stats_of(
    records: &[KeptRecord],
    reader: Reader<'_>,
    tree: Option<BTree>,
) -> Result<Vec<CollectionStats>> {
    records
        .iter()
        .map(|record| record.stats(reader, tree))
        .collect()
}

impl ReadTransaction<'_> {
    /// Each collection's entries and their bytes, as this transaction sees them, in the byte
    /// order of the names (see [`CollectionStats`]). Each collection keeps its count, so this
    /// reads one record per collection; a collection written by a release before counts were
    /// kept is counted by reading its keys.
    pub fn collection_stats(&self) -> Result<Vec<CollectionStats>> {
        let records = kept_records(self.reader(), self.tree())?;
        stats_of(&records, self.reader(), self.tree())
    }

    pub(crate) fn check_collection_counts(&self) -> Result<()> {
        let names = names_in(Scan::within_prefix(
            self.reader(),
            self.tree(),
            &names_prefix(),
            b"",
        ))?;
        let Some(found) = self.tree() else {
            return Ok(());
        };
        for name in names {
            let number = collection_number(self.reader().get(&found, &name_key(&name))?, &name)?;
            let Some(record) = self.reader().get(&found, &count_key(number))? else {
                continue;
            };
            let kept = EntryCount::read(&record).map_err(|error| {
                Error::new(error.kind(), format!("the collection \"{name}\": {error}"))
            })?;
            let held = counted_by_scan(self.reader(), self.tree(), number)?;
            if kept != held {
                return Err(Error::new(
                    ErrorKind::Corrupt,
                    format!(
                        "the collection \"{name}\" keeps a count of {} entries ({} key bytes, {} value bytes) but holds {} ({} key bytes, {} value bytes); restore from a backup, or delete the collection with `eggdb collection delete`",
                        kept.entries,
                        kept.key_bytes,
                        kept.value_bytes,
                        held.entries,
                        held.key_bytes,
                        held.value_bytes
                    ),
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn has_uncounted_collections(&self) -> Result<bool> {
        let records = kept_records(self.reader(), self.tree())?;
        Ok(records.iter().any(|record| record.count.is_none()))
    }

    /// The collection named `name`, as this transaction sees it (see [`ReadCollection`]). It
    /// fails with [`ErrorKind::CollectionNotFound`] when the snapshot has no such collection,
    /// and with [`ErrorKind::InvalidCollectionName`] for a name no collection can have.
    pub fn collection(&self, name: &str) -> Result<ReadCollection<'_>> {
        check_collection_name(name)?;
        let number = collection_number(self.get_raw(&name_key(name))?, name)?;
        Ok(ReadCollection {
            reader: self.reader(),
            tree: self.tree(),
            base: entries_prefix(number),
            name: name.into(),
            expiry_now: self.expiry_now(),
        })
    }

    /// The names of the database's collections, in byte order.
    pub fn collection_names(&self) -> Result<Vec<String>> {
        names_in(Scan::within_prefix(
            self.reader(),
            self.tree(),
            &names_prefix(),
            b"",
        ))
    }
}

impl<'db> WriteTransaction<'db> {
    /// The collection named `name`, as this transaction sees and changes it (see
    /// [`WriteCollection`]). It fails with [`ErrorKind::CollectionNotFound`] when there is no
    /// such collection, created by this transaction or before it, and with
    /// [`ErrorKind::InvalidCollectionName`] for a name no collection can have.
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-write-collection-{}.db", std::process::id()));
    /// let db = eggdb::Database::open(&path)?;
    /// db.write(|tx| {
    ///     tx.create_collection("sessions")?;
    ///     let mut sessions = tx.collection("sessions")?;
    ///     sessions.put("s1", "alice")?;
    ///     sessions.put("s2", "bob")?;
    ///     assert_eq!(sessions.scan(..).count(), 2);
    ///     Ok(())
    /// })?;
    /// let tx = db.begin_read()?;
    /// assert_eq!(tx.collection("sessions")?.get("s2")?, Some(b"bob".to_vec()));
    /// assert_eq!(tx.scan(..).count(), 0);
    /// # drop(tx);
    /// # drop(db);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(()) }
    /// ```
    pub fn collection(&mut self, name: &str) -> Result<WriteCollection<'_, 'db>> {
        check_collection_name(name)?;
        let number = collection_number(self.get_raw(&name_key(name))?, name)?;
        Ok(WriteCollection {
            transaction: self,
            base: entries_prefix(number),
            name: name.into(),
        })
    }

    /// The names of the database's collections, this transaction's changes included, in byte
    /// order.
    pub fn collection_names(&self) -> Result<Vec<String>> {
        names_in(Scan::within_prefix(
            self.reader(),
            self.tree(),
            &names_prefix(),
            b"",
        ))
    }

    /// Creates an empty collection named `name`, and returns whether it created it: `false`
    /// when the collection already exists, which is left as it is. A name no collection can have
    /// fails with [`ErrorKind::InvalidCollectionName`].
    pub fn create_collection(&mut self, name: &str) -> Result<bool> {
        check_collection_name(name)?;
        let key = name_key(name);
        if let Some(existing) = self.get_raw(&key)? {
            read_record(&existing, &format!("the collection \"{name}\""))?;
            return Ok(false);
        }
        let catalog = catalog_key();
        let number = match self.get_raw(&catalog)? {
            Some(bytes) => read_record(&bytes, "the catalog of collections")?,
            None => FIRST_NUMBER,
        };
        let next = number.checked_add(1).ok_or_else(|| {
            Error::new(
                ErrorKind::DatabaseFull,
                "this database has created as many collections as it can number (4,294,967,295); export it and import the export into a new file to start the numbers again",
            )
        })?;
        self.put_raw(&catalog, &record(next))?;
        self.put_raw(&key, &record(number))?;
        self.put_raw(&count_key(number), &EntryCount::default().record())?;
        Ok(true)
    }

    /// Gives the collection named `name` the name `new_name`, with every key in it and their
    /// times to live, in this transaction: no transaction ever sees it under both names or
    /// under neither. It fails with [`ErrorKind::CollectionNotFound`] when there is no
    /// collection named `name`, with [`ErrorKind::AlreadyExists`] when a collection is already
    /// named `new_name` (`name` itself included), which is left as it is, and with
    /// [`ErrorKind::InvalidCollectionName`] for a name no collection can have. It changes only
    /// the collection's name record, so it takes the same time for a collection of any size.
    pub fn rename_collection(&mut self, name: &str, new_name: &str) -> Result<()> {
        check_collection_name(name)?;
        check_collection_name(new_name)?;
        let key = name_key(name);
        let number = collection_number(self.get_raw(&key)?, name)?;
        let new_key = name_key(new_name);
        if self.get_raw(&new_key)?.is_some() {
            return Err(Error::new(
                ErrorKind::AlreadyExists,
                format!(
                    "cannot rename the collection \"{name}\" to \"{new_name}\": a collection named \"{new_name}\" is already there, and a rename never replaces one; choose another name, or drop that collection first"
                ),
            ));
        }
        self.delete_raw(&key)?;
        self.put_raw(&new_key, &record(number))?;
        Ok(())
    }

    /// Removes the collection named `name` and every key in it, and returns whether it was
    /// there. The collection is gone at once: from this transaction on, it has no name and no
    /// read sees its keys, and the name is free for a new collection, which starts empty.
    /// The keys go in steps, so a collection of any size drops within a small page cache: this
    /// transaction removes up to 512 of them, and a mark left in the file has later commits
    /// remove the rest, up to 32 each, until
    /// [`Database::finish_collection_drops`] or [`Database::drop_collection`] removes them
    /// all. A crash between steps leaves the mark, so those commits carry on where the steps
    /// stopped; keys of a dropped collection are never read again in the meantime.
    pub fn drop_collection(&mut self, name: &str) -> Result<bool> {
        Ok(self.drop_collection_step(name)?.is_some())
    }

    fn drop_collection_step(&mut self, name: &str) -> Result<Option<FirstDropStep>> {
        check_collection_name(name)?;
        let key = name_key(name);
        let Some(existing) = self.get_raw(&key)? else {
            return Ok(None);
        };
        let number = read_record(&existing, &format!("the collection \"{name}\""))?;
        let entries = match self.kept_count(number) {
            Ok(count) => count.map(|count| count.entries),
            Err(error) if error.kind() == ErrorKind::Corrupt => None,
            Err(error) => return Err(error),
        };
        self.delete_raw(&key)?;
        self.delete_raw(&count_key(number))?;
        self.put_raw(&dropping_key(number), &dropping_record(name))?;
        self.database().note_collection_drops(DROPS_PENDING);
        let step = self.remove_dropped_entries(DROP_STEP_KEYS)?;
        Ok(Some(FirstDropStep {
            removed: step.entries as u64,
            entries,
        }))
    }

    pub(crate) fn keep_collection_counts(&mut self) -> Result<Vec<CollectionStats>> {
        let records = kept_records(self.reader(), self.tree())?;
        let mut stats = Vec::with_capacity(records.len());
        for record in &records {
            let one = match self.kept_count(record.number)? {
                Some(count) => CollectionStats {
                    name: record.name.clone(),
                    entries: count.entries,
                    key_bytes: count.key_bytes,
                    value_bytes: count.value_bytes,
                },
                None => {
                    let counted = record.stats(self.reader(), self.tree())?;
                    let count = EntryCount {
                        entries: counted.entries,
                        key_bytes: counted.key_bytes,
                        value_bytes: counted.value_bytes,
                    };
                    self.put_raw(&count_key(record.number), &count.record())?;
                    counted
                }
            };
            stats.push(one);
        }
        Ok(stats)
    }

    pub(crate) fn finish_drops_before_commit(&mut self) -> Result<()> {
        if self.database().collection_drops() == DROPS_NONE {
            return Ok(());
        }
        self.remove_dropped_entries(DROPPED_KEYS_PER_COMMIT)
            .map(drop)
    }

    fn remove_dropped_entries(&mut self, limit: usize) -> Result<DropStep> {
        let mut step = DropStep::default();
        while step.entries < limit {
            let Some(number) = self.first_dropping_number()? else {
                if step == DropStep::default() {
                    self.database().note_collection_drops(DROPS_NONE);
                }
                return Ok(step);
            };
            let wanted = limit - step.entries;
            let keys = self.first_keys_under(&entries_prefix(number), wanted)?;
            for key in &keys {
                self.remove_stored(key)?;
            }
            step.entries += keys.len();
            if keys.len() < wanted {
                self.delete_raw(&dropping_key(number))?;
                step.finished += 1;
            }
        }
        Ok(step)
    }

    fn first_dropping_number(&self) -> Result<Option<u32>> {
        let mut scan = Scan::within_prefix(self.reader(), self.tree(), &droppings_prefix(), b"");
        match scan.next_key_and_value_len()? {
            Some((key, _)) => read_dropping_number(key).map(Some),
            None => Ok(None),
        }
    }

    fn first_keys_under(&self, base: &[u8], limit: usize) -> Result<Vec<Vec<u8>>> {
        let mut scan = Scan::within_prefix(self.reader(), self.tree(), base, b"");
        let mut keys = Vec::new();
        while keys.len() < limit {
            match scan.next_key_and_value_len()? {
                Some((key, _)) => keys.push([base, key].concat()),
                None => break,
            }
        }
        Ok(keys)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct DropStep {
    entries: usize,
    finished: usize,
}

struct FirstDropStep {
    removed: u64,
    entries: Option<u64>,
}

impl Database {
    /// The collection named `name`, for transactions of one step (see [`Collection`]). Nothing
    /// is read until it is used.
    pub fn collection(&self, name: &str) -> Collection<'_> {
        Collection::new(self, name)
    }

    /// Creates an empty collection named `name` and commits, and returns whether it created it:
    /// `false` when it already exists, and then nothing is committed. Names are 1 to 128 bytes
    /// of text without whitespace or control characters; another fails with
    /// [`ErrorKind::InvalidCollectionName`].
    pub fn create_collection(&self, name: &str) -> Result<bool> {
        let mut transaction = self.begin_write()?;
        if transaction.create_collection(name)? {
            transaction.commit()?;
            Ok(true)
        } else {
            transaction.rollback()?;
            Ok(false)
        }
    }

    /// Removes the collection named `name` and every key in it, and returns whether it was
    /// there. The first commit removes its name, so no read sees the collection from then on,
    /// and up to 512 of its keys; later commits of up to 512 keys each remove the rest, so a
    /// collection of millions of keys never needs one huge transaction, and each step's write
    /// leaves read transactions working as any commit does. It also finishes every earlier
    /// drop that a crash or [`WriteTransaction::drop_collection`] left unfinished (see
    /// [`Database::finish_collection_drops`]). An error in the first commit changes nothing;
    /// after one in a later step the collection stays dropped, and the next commits, or
    /// [`Database::finish_collection_drops`], remove its remaining keys.
    pub fn drop_collection(&self, name: &str) -> Result<bool> {
        self.drop_collection_with_progress(name, |_| {})
    }

    /// Drops the collection named `name` as [`Database::drop_collection`] does, and calls
    /// `progress` after each of its commits with the keys removed so far and, when the
    /// collection kept a count, how many it held: a caller can show how far a long drop has
    /// come. `progress` is not called when there is no such collection.
    pub fn drop_collection_with_progress(
        &self,
        name: &str,
        mut progress: impl FnMut(DropProgress),
    ) -> Result<bool> {
        let mut transaction = self.begin_write()?;
        transaction.skip_sweep_on_commit();
        let Some(first) = transaction.drop_collection_step(name)? else {
            transaction.rollback()?;
            return Ok(false);
        };
        transaction.commit()?;
        let mut shown = DropProgress {
            removed: first.removed,
            entries: first.entries,
        };
        progress(shown);
        self.finish_drops_reporting(|removed| {
            shown.removed = first.removed.saturating_add(removed);
            progress(shown);
        })?;
        Ok(true)
    }

    /// Removes every key left of collections that were dropped, in commits of up to 512 keys,
    /// and returns how many it removed: none when no drop is unfinished. Reads never see those
    /// keys, and every commit removes up to 32 of them by itself; this frees their space at
    /// once, for example after a crash in the middle of [`Database::drop_collection`].
    pub fn finish_collection_drops(&self) -> Result<u64> {
        self.finish_drops_reporting(|_| {})
    }

    fn finish_drops_reporting(&self, mut removed_so_far: impl FnMut(u64)) -> Result<u64> {
        let mut removed = 0u64;
        loop {
            let mut transaction = self.begin_write()?;
            transaction.skip_sweep_on_commit();
            let step = transaction.remove_dropped_entries(DROP_STEP_KEYS)?;
            if step.entries == 0 && step.finished == 0 {
                transaction.rollback()?;
                return Ok(removed);
            }
            transaction.commit()?;
            removed += step.entries as u64;
            if step.entries > 0 {
                removed_so_far(removed);
            }
        }
    }

    /// Gives the collection named `name` the name `new_name` and commits (see
    /// [`WriteTransaction::rename_collection`]): atomic, and refused with
    /// [`ErrorKind::AlreadyExists`] when a collection already has the new name.
    pub fn rename_collection(&self, name: &str, new_name: &str) -> Result<()> {
        let mut transaction = self.begin_write()?;
        transaction.rename_collection(name, new_name)?;
        transaction.commit()?;
        Ok(())
    }

    /// Each collection's entries and their bytes as of the last commit, in the byte order of
    /// the names (see [`CollectionStats`]). Each collection keeps its count up to date on
    /// every write, so this reads one record per collection. A collection written by a release
    /// before counts were kept is counted once by reading its keys, and on a database open for
    /// writing whose writer is free that count is committed and kept from then on.
    pub fn collection_stats(&self) -> Result<Vec<CollectionStats>> {
        let read = self.begin_read()?;
        let records = kept_records(read.reader(), read.tree())?;
        let all_kept = records.iter().all(|record| record.count.is_some());
        if all_kept || self.is_read_only() {
            return stats_of(&records, read.reader(), read.tree());
        }
        drop(read);
        match self.try_begin_write() {
            Ok(mut transaction) => {
                let stats = transaction.keep_collection_counts()?;
                transaction.commit()?;
                Ok(stats)
            }
            Err(error) if error.kind() == ErrorKind::WriterActive => {
                self.begin_read()?.collection_stats()
            }
            Err(error) => Err(error),
        }
    }

    /// Whether a collection made by a release before counts were kept has no count yet, as
    /// of the last commit. [`Database::collection_stats`] counts such a collection by reading
    /// its keys and, on a database open for writing, keeps the count.
    pub fn has_uncounted_collections(&self) -> Result<bool> {
        self.begin_read()?.has_uncounted_collections()
    }

    /// The names of the database's collections as of the last commit, in byte order.
    pub fn collection_names(&self) -> Result<Vec<String>> {
        self.begin_read()?.collection_names()
    }

    /// The longest key a collection stores, in bytes: [`Database::max_key_bytes`] less the ten
    /// bytes that say which collection a key is in.
    pub fn max_collection_key_bytes(&self) -> usize {
        self.max_key_bytes().saturating_sub(ENTRY_PREFIX_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        COLLECTIONS_END, COLLECTIONS_PREFIX, ENTRY_PREFIX_BYTES, catalog_key,
        check_collection_name, dropping_key, dropping_record, droppings_prefix, entries_prefix,
        is_reserved, name_key, names_prefix, read_dropping_number, read_record, record,
    };
    use crate::ErrorKind;
    use crate::scan::keys_after_prefix;
    use std::ops::Bound;

    #[test]
    fn the_hidden_range_is_exactly_the_reserved_prefix() {
        assert_eq!(
            keys_after_prefix(COLLECTIONS_PREFIX),
            Bound::Excluded(COLLECTIONS_END.to_vec())
        );
        assert!(is_reserved(&entries_prefix(7)));
        assert!(is_reserved(&name_key("users")));
        assert!(!is_reserved(b"\xffCOL"));
        assert!(!is_reserved(b"\xffSQL\x00"));
        assert_eq!(entries_prefix(1).len(), ENTRY_PREFIX_BYTES);
    }

    #[test]
    fn the_key_layout_matches_its_test_vectors() {
        assert_eq!(catalog_key(), b"\xffCOL\x00\x01");
        assert_eq!(names_prefix(), b"\xffCOL\x00\x02");
        assert_eq!(name_key("users"), b"\xffCOL\x00\x02users");
        assert_eq!(entries_prefix(1), b"\xffCOL\x00\x03\x00\x00\x00\x01");
        assert_eq!(
            entries_prefix(0x0102_0304),
            b"\xffCOL\x00\x03\x01\x02\x03\x04"
        );
        assert_eq!(record(1), [1, 0, 0, 0, 1]);
        assert_eq!(record(0x0102_0304), [1, 1, 2, 3, 4]);
        assert_eq!(droppings_prefix(), b"\xffCOL\x00\x06");
        assert_eq!(dropping_key(1), b"\xffCOL\x00\x06\x00\x00\x00\x01");
        assert_eq!(dropping_record("cache"), b"\x01cache");
    }

    #[test]
    fn a_dropping_mark_sorts_after_every_entry_and_names_its_number() {
        assert!(dropping_key(1) > entries_prefix(u32::MAX));
        assert!(is_reserved(&dropping_key(7)));
        let tag = droppings_prefix().len();
        assert_eq!(read_dropping_number(&dropping_key(7)[tag..]).ok(), Some(7));
        assert_eq!(
            read_dropping_number(&dropping_key(u32::MAX)[tag..]).ok(),
            Some(u32::MAX)
        );
        let damaged_keys: [&[u8]; 3] = [b"", b"\x00\x00\x01", b"\x00\x00\x00\x01\x00"];
        for damaged in damaged_keys {
            let error = read_dropping_number(damaged).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Corrupt, "{damaged:?}");
        }
    }

    #[test]
    fn entry_prefixes_of_different_collections_never_overlap_and_sort_by_number() {
        let (one, two, big) = (entries_prefix(1), entries_prefix(2), entries_prefix(256));
        assert!(!two.starts_with(&one) && !one.starts_with(&two));
        assert!(one < two && two < big);
    }

    #[test]
    fn records_round_trip_and_damage_is_an_error() {
        assert_eq!(read_record(&record(42), "x").ok(), Some(42));
        assert_eq!(read_record(&record(u32::MAX), "x").ok(), Some(u32::MAX));
        let damaged_records: [&[u8]; 5] = [
            &[],
            &[1],
            &[1, 0, 0, 0],
            &[1, 0, 0, 0, 0, 0],
            &[0, 0, 0, 0, 1],
        ];
        for damaged in damaged_records {
            let error = read_record(damaged, "x").unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Corrupt, "{damaged:?}");
        }
        let newer = read_record(&[2, 0, 0, 0, 1], "x").unwrap_err();
        assert_eq!(newer.kind(), ErrorKind::Incompatible);
    }

    #[test]
    fn names_are_text_without_whitespace_or_control_characters() {
        let longest = "x".repeat(128);
        for good in [
            "users",
            "a",
            "日本",
            "with-dash_and.dot:colon",
            longest.as_str(),
        ] {
            assert!(check_collection_name(good).is_ok(), "{good}");
        }
        let too_long = "x".repeat(129);
        for bad in ["", "two words", "tab\there", "nul\0", too_long.as_str()] {
            let error = check_collection_name(bad).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidCollectionName, "{bad:?}");
        }
    }
}
