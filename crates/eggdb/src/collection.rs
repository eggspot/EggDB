use crate::database::Database;
use crate::error::{Error, ErrorKind, Result};
use crate::scan::{KeyRange, Reader, Scan};
use crate::transaction::{ReadTransaction, WriteTransaction};
use eggdb_storage::BTree;

pub(crate) const COLLECTIONS_PREFIX: &[u8] = b"\xffCOL\x00";
pub(crate) const COLLECTIONS_END: &[u8] = b"\xffCOL\x01";
const CATALOG_TAG: u8 = 0x01;
const NAME_TAG: u8 = 0x02;
const ENTRY_TAG: u8 = 0x03;
const RECORD_VERSION: u8 = 1;
const RECORD_BYTES: usize = 5;
const FIRST_NUMBER: u32 = 1;
const DROP_BATCH_KEYS: usize = 256;
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
        "keys that start with the bytes 0xff 'C' 'O' 'L' 0x00 hold EggDB's collections, so the default key space refuses them; store the key in a collection, or choose another first byte",
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
        let mut transaction = self.database.begin_write()?;
        if transaction.collection(&self.name)?.delete(key)? {
            transaction.commit()?;
            Ok(true)
        } else {
            transaction.rollback()?;
            Ok(false)
        }
    }
}

/// A collection as a [`ReadTransaction`] sees it, from [`ReadTransaction::collection`]: the
/// same snapshot as the transaction, with the default key space's reading methods.
pub struct ReadCollection<'t> {
    reader: Reader<'t>,
    tree: Option<BTree>,
    base: Vec<u8>,
    name: Box<str>,
}

impl ReadCollection<'_> {
    /// The collection's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The value stored under `key` in the collection, or `None` when the key is absent.
    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        let Some(tree) = &self.tree else {
            return Ok(None);
        };
        self.reader.get(tree, &[self.base.as_slice(), key.as_ref()].concat())
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
        self.reader
            .get_with(tree, &[self.base.as_slice(), key.as_ref()].concat(), read)
    }

    /// The collection's entries with keys in `range`, in key order; `.rev()` gives them
    /// largest key first. See [`Scan`] and [`KeyRange`].
    pub fn scan(&self, range: impl KeyRange) -> Scan<'_> {
        Scan::within_between(self.reader, self.tree, &self.base, &range)
    }

    /// The collection's entries whose keys start with `prefix`, in key order; `.rev()` gives
    /// them largest key first. The empty prefix scans the whole collection.
    pub fn scan_prefix(&self, prefix: impl AsRef<[u8]>) -> Scan<'_> {
        Scan::within_prefix(self.reader, self.tree, &self.base, prefix.as_ref())
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
    /// `None` when the key is absent.
    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        self.transaction.get_raw(&self.key(key.as_ref()))
    }

    /// Calls `read` with the value stored under `key` where it lies, as
    /// [`WriteTransaction::get_with`] does, or returns `None` when the key is absent.
    pub fn get_with<R>(
        &self,
        key: impl AsRef<[u8]>,
        read: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>> {
        let Some(tree) = self.transaction.tree() else {
            return Ok(None);
        };
        self.transaction
            .reader()
            .get_with(&tree, &self.key(key.as_ref()), read)
    }

    /// The collection's entries with keys in `range`, this transaction's changes included, in
    /// key order; `.rev()` gives them largest key first.
    pub fn scan(&self, range: impl KeyRange) -> Scan<'_> {
        Scan::within_between(
            self.transaction.reader(),
            self.transaction.tree(),
            &self.base,
            &range,
        )
    }

    /// The collection's entries whose keys start with `prefix`, this transaction's changes
    /// included, in key order; `.rev()` gives them largest key first.
    pub fn scan_prefix(&self, prefix: impl AsRef<[u8]>) -> Scan<'_> {
        Scan::within_prefix(
            self.transaction.reader(),
            self.transaction.tree(),
            &self.base,
            prefix.as_ref(),
        )
    }

    /// Stores `value` under `key` in the collection, replacing the value there. Limits and
    /// failures are those of [`WriteTransaction::put`], with keys ten bytes shorter.
    pub fn put(&mut self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()> {
        let key = self.checked_key(key.as_ref())?;
        self.transaction.put_raw(&key, value.as_ref())
    }

    /// Stores `value` under `key` in the collection only when `key` is absent, and returns
    /// whether it stored it, as [`WriteTransaction::put_if_absent`] does.
    pub fn put_if_absent(
        &mut self,
        key: impl AsRef<[u8]>,
        value: impl AsRef<[u8]>,
    ) -> Result<bool> {
        let key = self.checked_key(key.as_ref())?;
        self.transaction.put_if_absent_raw(&key, value.as_ref())
    }

    /// Removes `key` and its value from the collection, and returns whether the key was there.
    pub fn delete(&mut self, key: impl AsRef<[u8]>) -> Result<bool> {
        self.transaction.delete_raw(&self.key(key.as_ref()))
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

impl ReadTransaction<'_> {
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
        Ok(true)
    }

    /// Removes the collection named `name` and every key in it, and returns whether it was
    /// there. The keys go in this transaction, so a collection with more changed pages than the
    /// page cache holds fails with [`ErrorKind::CacheFull`](crate::ErrorKind::CacheFull); open
    /// the database with a larger [`Options::cache_bytes`](crate::Options::cache_bytes).
    pub fn drop_collection(&mut self, name: &str) -> Result<bool> {
        check_collection_name(name)?;
        let key = name_key(name);
        let Some(existing) = self.get_raw(&key)? else {
            return Ok(false);
        };
        let number = read_record(&existing, &format!("the collection \"{name}\""))?;
        let base = entries_prefix(number);
        loop {
            let keys = self.first_keys_under(&base)?;
            if keys.is_empty() {
                break;
            }
            for key in &keys {
                self.delete_raw(key)?;
            }
        }
        self.delete_raw(&key)?;
        Ok(true)
    }

    fn first_keys_under(&self, base: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut scan = Scan::within_prefix(self.reader(), self.tree(), base, b"");
        let mut keys = Vec::new();
        while keys.len() < DROP_BATCH_KEYS {
            match scan.next_key_and_value_len()? {
                Some((key, _)) => keys.push([base, key].concat()),
                None => break,
            }
        }
        Ok(keys)
    }
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

    /// Removes the collection named `name` and every key in it, and commits, and returns
    /// whether it was there (see [`WriteTransaction::drop_collection`]).
    pub fn drop_collection(&self, name: &str) -> Result<bool> {
        let mut transaction = self.begin_write()?;
        if transaction.drop_collection(name)? {
            transaction.commit()?;
            Ok(true)
        } else {
            transaction.rollback()?;
            Ok(false)
        }
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
        check_collection_name, entries_prefix, is_reserved, name_key, names_prefix, read_record,
        record,
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
        let damaged_records: [&[u8]; 5] = [&[], &[1], &[1, 0, 0, 0], &[1, 0, 0, 0, 0, 0], &[0, 0, 0, 0, 1]];
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
        for good in ["users", "a", "日本", "with-dash_and.dot:colon", longest.as_str()] {
            assert!(check_collection_name(good).is_ok(), "{good}");
        }
        let too_long = "x".repeat(129);
        for bad in ["", "two words", "tab\there", "nul\0", too_long.as_str()] {
            let error = check_collection_name(bad).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidCollectionName, "{bad:?}");
        }
    }
}
