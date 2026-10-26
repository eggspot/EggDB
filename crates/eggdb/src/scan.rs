use crate::error::{Error, Result};
use eggdb_platform::OsFile;
use eggdb_storage::{BTree, Cursor, ScanOrder, StorageError};
use std::iter::FusedIterator;
use std::ops::{Bound, Range, RangeFrom, RangeFull, RangeInclusive, RangeTo, RangeToInclusive};

/// A range of keys to [`scan`](crate::ReadTransaction::scan): any of Rust's range forms over
/// keys, such as `..`, `"a".."m"`, `b"a".to_vec()..`, `..="z"`, or a pair of
/// [`Bound`]s. Keys are anything that is bytes: `&[u8]`, `&[u8; N]`, `Vec<u8>`, `&str` and
/// `String`.
///
/// A range whose start lies above its end holds no keys. This trait is sealed: the range
/// forms above are all there is.
pub trait KeyRange: sealed::Bounds {}

impl<R: sealed::Bounds> KeyRange for R {}

pub(crate) mod sealed {
    use std::ops::Bound;

    pub trait Bounds {
        fn bounds(&self) -> (Bound<&[u8]>, Bound<&[u8]>);
    }
}

fn key_bounds<'k, K: AsRef<[u8]>>(
    start: Bound<&'k K>,
    end: Bound<&'k K>,
) -> (Bound<&'k [u8]>, Bound<&'k [u8]>) {
    (start.map(AsRef::as_ref), end.map(AsRef::as_ref))
}

impl sealed::Bounds for RangeFull {
    fn bounds(&self) -> (Bound<&[u8]>, Bound<&[u8]>) {
        (Bound::Unbounded, Bound::Unbounded)
    }
}

impl<K: AsRef<[u8]>> sealed::Bounds for Range<K> {
    fn bounds(&self) -> (Bound<&[u8]>, Bound<&[u8]>) {
        key_bounds(Bound::Included(&self.start), Bound::Excluded(&self.end))
    }
}

impl<K: AsRef<[u8]>> sealed::Bounds for RangeInclusive<K> {
    fn bounds(&self) -> (Bound<&[u8]>, Bound<&[u8]>) {
        key_bounds(Bound::Included(self.start()), Bound::Included(self.end()))
    }
}

impl<K: AsRef<[u8]>> sealed::Bounds for RangeFrom<K> {
    fn bounds(&self) -> (Bound<&[u8]>, Bound<&[u8]>) {
        key_bounds(Bound::Included(&self.start), Bound::Unbounded)
    }
}

impl<K: AsRef<[u8]>> sealed::Bounds for RangeTo<K> {
    fn bounds(&self) -> (Bound<&[u8]>, Bound<&[u8]>) {
        key_bounds(Bound::Unbounded, Bound::Excluded(&self.end))
    }
}

impl<K: AsRef<[u8]>> sealed::Bounds for RangeToInclusive<K> {
    fn bounds(&self) -> (Bound<&[u8]>, Bound<&[u8]>) {
        key_bounds(Bound::Unbounded, Bound::Included(&self.end))
    }
}

impl<K: AsRef<[u8]>> sealed::Bounds for (Bound<K>, Bound<K>) {
    fn bounds(&self) -> (Bound<&[u8]>, Bound<&[u8]>) {
        key_bounds(self.0.as_ref(), self.1.as_ref())
    }
}

#[derive(Clone, Copy)]
enum Wanted {
    Values,
    Lengths,
}

struct Lent<'a> {
    key: &'a [u8],
    value: &'a [u8],
    value_len: u64,
}

impl<'a> Lent<'a> {
    fn with_value((key, value): (&'a [u8], &'a [u8])) -> Self {
        Self {
            key,
            value,
            value_len: value.len() as u64,
        }
    }

    fn without_value((key, value_len): (&'a [u8], u64)) -> Self {
        Self {
            key,
            value: &[],
            value_len,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Reader<'t> {
    Snapshot(&'t eggdb_storage::ReadTransaction<'t, OsFile>),
    Writer(&'t eggdb_storage::WriteTransaction<'t, OsFile>),
}

impl Reader<'_> {
    fn lend(
        self,
        cursor: &mut Cursor,
        wanted: Wanted,
    ) -> std::result::Result<Option<Lent<'_>>, StorageError> {
        match (self, wanted) {
            (Self::Snapshot(transaction), Wanted::Values) => {
                Ok(transaction.next(cursor)?.map(Lent::with_value))
            }
            (Self::Writer(transaction), Wanted::Values) => {
                Ok(transaction.next(cursor)?.map(Lent::with_value))
            }
            (Self::Snapshot(transaction), Wanted::Lengths) => Ok(transaction
                .next_key_and_value_len(cursor)?
                .map(Lent::without_value)),
            (Self::Writer(transaction), Wanted::Lengths) => Ok(transaction
                .next_key_and_value_len(cursor)?
                .map(Lent::without_value)),
        }
    }
}

enum Keys {
    Between(Bound<Vec<u8>>, Bound<Vec<u8>>),
    Prefix(Vec<u8>),
}

impl Keys {
    fn cursor(&self, tree: &BTree, order: ScanOrder) -> Cursor {
        match self {
            Self::Between(start, end) => tree.scan(
                start.as_ref().map(Vec::as_slice),
                end.as_ref().map(Vec::as_slice),
                order,
            ),
            Self::Prefix(prefix) => tree.scan_prefix(prefix, order),
        }
    }
}

#[derive(Default)]
struct End {
    cursor: Option<Cursor>,
    returned_any: bool,
    last_key: Vec<u8>,
}

/// The entries of a [`scan`](crate::ReadTransaction::scan) or
/// [`scan_prefix`](crate::ReadTransaction::scan_prefix), in key order, as an iterator of
/// `Result<(key, value)>`.
///
/// It reads the transaction's view of the database as it goes: a read transaction's snapshot,
/// or a write transaction's committed data with its own changes. It borrows the transaction,
/// so a write transaction can't change anything while a scan of it is alive.
///
/// **Order.** Entries come smallest key first. [`Iterator::rev`] gives them largest first, as
/// for a [`std::collections::BTreeMap`] range, and the two ends can be mixed: the scan ends
/// when they meet. Each end reads the tree only once it is used.
///
/// **Cost.** Each item is a key and a value copied into two new `Vec<u8>`s. A loop that only
/// looks at the bytes can avoid those copies with [`Scan::next_entry`], which lends them
/// instead, and a loop that needs only keys and the sizes of their values can skip reading
/// values altogether with [`Scan::next_key_and_value_len`]. The scan underneath copies one leaf
/// page of entries at a time into a buffer it reuses, and holds no page of the database
/// between calls. A large value, stored on overflow
/// pages, is copied whole into that buffer and ends the leaf's batch, so the buffer holds at
/// most one large value; as an owned item it is copied once more, so it costs twice its size
/// in memory until the item is dropped.
///
/// **Errors.** A failure, such as a damaged page or an expired snapshot, is returned as one
/// `Err` item, after which the scan yields nothing more.
///
/// ```
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let path = std::env::temp_dir().join(format!("eggdb-doc-scan-{}.db", std::process::id()));
/// let db = eggdb::Database::open(&path)?;
/// db.write(|tx| {
///     for (key, value) in [("apple", "1"), ("banana", "2"), ("cherry", "3")] {
///         tx.put(key, value)?;
///     }
///     Ok(())
/// })?;
/// let tx = db.begin_read()?;
/// let keys: Vec<Vec<u8>> = tx
///     .scan("b"..)
///     .rev()
///     .map(|entry| entry.map(|(key, _)| key))
///     .collect::<Result<_, _>>()?;
/// assert_eq!(keys, [b"cherry".to_vec(), b"banana".to_vec()]);
/// # drop(tx);
/// # drop(db);
/// # std::fs::remove_file(&path)?;
/// # Ok(()) }
/// ```
pub struct Scan<'t> {
    reader: Reader<'t>,
    tree: Option<BTree>,
    keys: Keys,
    front: End,
    back: End,
    finished: bool,
}

impl<'t> Scan<'t> {
    pub(crate) fn between(reader: Reader<'t>, tree: Option<BTree>, range: &impl KeyRange) -> Self {
        let (start, end) = range.bounds();
        Self::new(
            reader,
            tree,
            Keys::Between(start.map(<[u8]>::to_vec), end.map(<[u8]>::to_vec)),
        )
    }

    pub(crate) fn prefix(reader: Reader<'t>, tree: Option<BTree>, prefix: &[u8]) -> Self {
        Self::new(reader, tree, Keys::Prefix(prefix.to_vec()))
    }

    fn new(reader: Reader<'t>, tree: Option<BTree>, keys: Keys) -> Self {
        Self {
            reader,
            finished: tree.is_none(),
            tree,
            keys,
            front: End::default(),
            back: End::default(),
        }
    }

    /// The next entry from the front, lent rather than copied: the key and value stay valid
    /// until the next call. `Ok(None)` once the scan is done. This is the scan without the
    /// two copies per entry that [`Iterator::next`] makes.
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-next-entry-{}.db", std::process::id()));
    /// let db = eggdb::Database::open(&path)?;
    /// db.put("a", "12")?;
    /// db.put("b", "345")?;
    /// let tx = db.begin_read()?;
    /// let mut scan = tx.scan(..);
    /// let mut value_bytes = 0;
    /// while let Some((_key, value)) = scan.next_entry()? {
    ///     value_bytes += value.len();
    /// }
    /// assert_eq!(value_bytes, 5);
    /// # drop(scan);
    /// # drop(tx);
    /// # drop(db);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(()) }
    /// ```
    pub fn next_entry(&mut self) -> Result<Option<(&[u8], &[u8])>> {
        Ok(self
            .step(ScanOrder::Ascending, Wanted::Values)?
            .map(|lent| (lent.key, lent.value)))
    }

    /// The next entry from the back, lent like [`Scan::next_entry`]: the reverse scan without
    /// copying keys and values.
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-next-back-entry-{}.db", std::process::id()));
    /// let db = eggdb::Database::open(&path)?;
    /// db.put("a", "12")?;
    /// db.put("b", "345")?;
    /// let tx = db.begin_read()?;
    /// let mut scan = tx.scan(..);
    /// let mut keys = Vec::new();
    /// while let Some((key, _value)) = scan.next_back_entry()? {
    ///     keys.push(key.to_vec());
    /// }
    /// assert_eq!(keys, [b"b".to_vec(), b"a".to_vec()]);
    /// # drop(scan);
    /// # drop(tx);
    /// # drop(db);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(()) }
    /// ```
    pub fn next_back_entry(&mut self) -> Result<Option<(&[u8], &[u8])>> {
        Ok(self
            .step(ScanOrder::Descending, Wanted::Values)?
            .map(|lent| (lent.key, lent.value)))
    }

    /// The next key from the front and the length in bytes of its value, which is not read:
    /// a large value's length comes from its reference, so none of its pages is touched. The
    /// key stays valid until the next call; `Ok(None)` once the scan is done. This is the step
    /// for listing or counting keys. It mixes freely with the other steps of the scan.
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-next-key-len-{}.db", std::process::id()));
    /// let db = eggdb::Database::open(&path)?;
    /// db.put("a", "12")?;
    /// db.put("b", vec![0u8; 1 << 20])?;
    /// let tx = db.begin_read()?;
    /// let mut scan = tx.scan(..);
    /// let mut lengths = Vec::new();
    /// while let Some((key, value_len)) = scan.next_key_and_value_len()? {
    ///     lengths.push((key.to_vec(), value_len));
    /// }
    /// assert_eq!(lengths, [(b"a".to_vec(), 2), (b"b".to_vec(), 1 << 20)]);
    /// # drop(scan);
    /// # drop(tx);
    /// # drop(db);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(()) }
    /// ```
    pub fn next_key_and_value_len(&mut self) -> Result<Option<(&[u8], u64)>> {
        Ok(self
            .step(ScanOrder::Ascending, Wanted::Lengths)?
            .map(|lent| (lent.key, lent.value_len)))
    }

    /// The next key from the back and its value's length, like
    /// [`Scan::next_key_and_value_len`]: the reverse key listing without reading values.
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-next-back-key-len-{}.db", std::process::id()));
    /// let db = eggdb::Database::open(&path)?;
    /// db.put("a", "12")?;
    /// db.put("b", "345")?;
    /// let tx = db.begin_read()?;
    /// let mut scan = tx.scan(..);
    /// let mut lengths = Vec::new();
    /// while let Some((key, value_len)) = scan.next_back_key_and_value_len()? {
    ///     lengths.push((key.to_vec(), value_len));
    /// }
    /// assert_eq!(lengths, [(b"b".to_vec(), 3), (b"a".to_vec(), 2)]);
    /// # drop(scan);
    /// # drop(tx);
    /// # drop(db);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(()) }
    /// ```
    pub fn next_back_key_and_value_len(&mut self) -> Result<Option<(&[u8], u64)>> {
        Ok(self
            .step(ScanOrder::Descending, Wanted::Lengths)?
            .map(|lent| (lent.key, lent.value_len)))
    }

    fn step(&mut self, order: ScanOrder, wanted: Wanted) -> Result<Option<Lent<'_>>> {
        let Some(tree) = self.tree.filter(|_| !self.finished) else {
            return Ok(None);
        };
        let (this, other) = match order {
            ScanOrder::Ascending => (&mut self.front, &self.back),
            ScanOrder::Descending => (&mut self.back, &self.front),
        };
        let keys = &self.keys;
        let cursor = this.cursor.get_or_insert_with(|| keys.cursor(&tree, order));
        let entry = match self.reader.lend(cursor, wanted) {
            Ok(Some(entry)) => entry,
            Ok(None) => {
                self.finished = true;
                return Ok(None);
            }
            Err(error) => {
                self.finished = true;
                return Err(Error::storage(error));
            }
        };
        let key = entry.key;
        let crossed = other.returned_any
            && match order {
                ScanOrder::Ascending => key >= other.last_key.as_slice(),
                ScanOrder::Descending => key <= other.last_key.as_slice(),
            };
        if crossed {
            self.finished = true;
            return Ok(None);
        }
        this.returned_any = true;
        this.last_key.clear();
        this.last_key.extend_from_slice(key);
        Ok(Some(entry))
    }

    fn step_owned(&mut self, order: ScanOrder) -> Option<Result<(Vec<u8>, Vec<u8>)>> {
        self.step(order, Wanted::Values)
            .map(|entry| entry.map(|lent| (lent.key.to_vec(), lent.value.to_vec())))
            .transpose()
    }
}

impl Iterator for Scan<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.step_owned(ScanOrder::Ascending)
    }
}

impl DoubleEndedIterator for Scan<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.step_owned(ScanOrder::Descending)
    }
}

impl FusedIterator for Scan<'_> {}

impl std::fmt::Debug for Scan<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scan")
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}
