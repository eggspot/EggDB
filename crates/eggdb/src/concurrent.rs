use crate::collection;
use crate::database::Database;
use crate::error::{Error, ErrorKind, Result};
use crate::scan::KeyRange;
use crate::transaction::ReadTransaction;
use eggdb_storage::CommitStamp;
use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use std::time::Duration;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const PREPARED_TAG: u8 = 0x09;
const PREPARED_RECORD_VERSION: u8 = 1;
const MAX_GID_BYTES: usize = 200;
const FNV_PRIME: u64 = 0x0100_0000_01b3;

/// How a [`ConcurrentTransaction`] checks, when it commits, the transactions that committed
/// while it ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Isolation {
    /// Snapshot isolation: the commit fails when a key it writes was changed by a transaction
    /// that committed after it began, so the first committer wins.
    Snapshot,
    /// Serializable: the commit fails when anything it read, keys and scanned ranges, was
    /// changed by a transaction that committed after it began, so the committed transactions
    /// give the same results as running one at a time in commit order.
    #[default]
    Serializable,
}

#[derive(Debug)]
struct ScannedRange {
    start: Bound<Vec<u8>>,
    end: Bound<Vec<u8>>,
    fingerprint: Fingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fingerprint {
    entries: u64,
    hash: u64,
}

impl Fingerprint {
    fn new() -> Self {
        Self {
            entries: 0,
            hash: FNV_OFFSET,
        }
    }

    fn add(&mut self, key: &[u8], value: &[u8]) {
        self.entries += 1;
        for part in [key, value] {
            for byte in (part.len() as u64).to_le_bytes().iter().chain(part) {
                self.hash = (self.hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME);
            }
        }
    }
}

fn owned(bound: Bound<&[u8]>) -> Bound<Vec<u8>> {
    bound.map(<[u8]>::to_vec)
}

fn within(key: &[u8], start: &Bound<Vec<u8>>, end: &Bound<Vec<u8>>) -> bool {
    let after_start = match start {
        Bound::Included(start) => key >= start.as_slice(),
        Bound::Excluded(start) => key > start.as_slice(),
        Bound::Unbounded => true,
    };
    let before_end = match end {
        Bound::Included(end) => key <= end.as_slice(),
        Bound::Excluded(end) => key < end.as_slice(),
        Bound::Unbounded => true,
    };
    after_start && before_end
}

fn conflict(what: &str) -> Error {
    Error::new(
        ErrorKind::Conflict,
        format!(
            "could not serialize access due to concurrent update: {what} changed after this transaction began; run it again"
        ),
    )
}

/// A write transaction that runs alongside others (see [`Database::begin_concurrent`]). It
/// reads a snapshot of the last commit before it began, keeps its own changes in memory until
/// [`commit`](Self::commit), and only then takes the database's writer, for as long as checking
/// and writing its changes take. A transaction that another one's commit invalidated fails
/// with [`ErrorKind::Conflict`](crate::ErrorKind::Conflict) and changes nothing; run it again.
pub struct ConcurrentTransaction<'db> {
    database: &'db Database,
    snapshot: ReadTransaction<'db>,
    isolation: Isolation,
    writes: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    expiring: HashMap<Vec<u8>, Duration>,
    reads: HashMap<Vec<u8>, Option<Vec<u8>>>,
    ranges: Vec<ScannedRange>,
    collection_writes: BTreeMap<(String, Vec<u8>), Option<(Vec<u8>, Option<Duration>)>>,
    collection_reads: HashMap<(String, Vec<u8>), Option<Vec<u8>>>,
}

impl std::fmt::Debug for ConcurrentTransaction<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConcurrentTransaction")
            .field("isolation", &self.isolation)
            .field("writes", &self.writes.len())
            .field("reads", &self.reads.len())
            .field("ranges", &self.ranges.len())
            .finish_non_exhaustive()
    }
}

impl Database {
    /// Begins a [`ConcurrentTransaction`]: any number of them may run at once, alongside
    /// readers and the ordinary write transaction, and they wait for the writer only while they
    /// commit.
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-concurrent-{}.db", std::process::id()));
    /// use eggdb::{ErrorKind, Isolation};
    /// let db = eggdb::Database::open(&path)?;
    /// db.put("balance", "10")?;
    /// let mut first = db.begin_concurrent(Isolation::Serializable)?;
    /// let mut second = db.begin_concurrent(Isolation::Serializable)?;
    /// first.get("balance")?;
    /// second.get("balance")?;
    /// first.put("balance", "15")?;
    /// second.put("balance", "7")?;
    /// first.commit()?;
    /// assert_eq!(second.commit().unwrap_err().kind(), ErrorKind::Conflict);
    /// assert_eq!(db.get("balance")?, Some(b"15".to_vec()));
    /// # drop(db);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(()) }
    /// ```
    pub fn begin_concurrent(&self, isolation: Isolation) -> Result<ConcurrentTransaction<'_>> {
        Ok(ConcurrentTransaction {
            database: self,
            snapshot: self.begin_read()?,
            isolation,
            writes: BTreeMap::new(),
            expiring: HashMap::new(),
            reads: HashMap::new(),
            ranges: Vec::new(),
            collection_writes: BTreeMap::new(),
            collection_reads: HashMap::new(),
        })
    }
}

impl<'db> ConcurrentTransaction<'db> {
    /// The isolation it commits with.
    pub fn isolation(&self) -> Isolation {
        self.isolation
    }

    /// The value stored under `key` in the default key space, this transaction's own changes
    /// included, or `None`.
    pub fn get(&mut self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        let key = key.as_ref();
        if let Some(written) = self.writes.get(key) {
            return Ok(written.clone());
        }
        if let Some(seen) = self.reads.get(key) {
            return Ok(seen.clone());
        }
        let found = self.snapshot.get(key)?;
        if self.isolation == Isolation::Serializable {
            self.reads.insert(key.to_vec(), found.clone());
        }
        Ok(found)
    }

    /// Stores `value` under `key`, in memory until the commit. Limits are those of
    /// [`WriteTransaction::put`](crate::WriteTransaction::put), checked here.
    pub fn put(&mut self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()> {
        let key = key.as_ref();
        let value = value.as_ref();
        collection::refuse_reserved(key)?;
        self.check_limits(key, value.len())?;
        self.writes.insert(key.to_vec(), Some(value.to_vec()));
        self.expiring.remove(key);
        Ok(())
    }

    /// Stores `value` under `key` with a time to live of `ttl` from the commit, as
    /// [`WriteTransaction::put_with_expiry`](crate::WriteTransaction::put_with_expiry) does,
    /// in memory until the commit (D1124). Its own reads see the value; the time runs from the
    /// commit, when the key is written.
    pub fn put_with_expiry(
        &mut self,
        key: impl AsRef<[u8]>,
        value: impl AsRef<[u8]>,
        ttl: Duration,
    ) -> Result<()> {
        let key = key.as_ref();
        self.put(key, value)?;
        self.expiring.insert(key.to_vec(), ttl);
        Ok(())
    }

    /// The names of the collections the snapshot has, in byte order.
    pub fn collection_names(&self) -> Result<Vec<String>> {
        self.snapshot.collection_names()
    }

    /// Whether the snapshot has a collection named `name`.
    pub fn has_collection(&self, name: &str) -> Result<bool> {
        match self.snapshot.collection(name) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == ErrorKind::CollectionNotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// The value under `key` in the collection named `name`, this transaction's own changes
    /// included, or `None` (D1124). Fails with
    /// [`ErrorKind::CollectionNotFound`](crate::ErrorKind::CollectionNotFound) when the snapshot
    /// has no such collection. Under [`Isolation::Serializable`] the read is checked at the
    /// commit.
    pub fn collection_get(&mut self, name: &str, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        let key = key.as_ref();
        let slot = (name.to_owned(), key.to_vec());
        if let Some(written) = self.collection_writes.get(&slot) {
            return Ok(written.as_ref().map(|(value, _)| value.clone()));
        }
        if let Some(seen) = self.collection_reads.get(&slot) {
            return Ok(seen.clone());
        }
        let found = self.snapshot.collection(name)?.get(key)?;
        if self.isolation == Isolation::Serializable {
            self.collection_reads.insert(slot, found.clone());
        }
        Ok(found)
    }

    /// Stores `value` under `key` in the collection named `name`, with a time to live when
    /// `ttl` is given, in memory until the commit (D1124). The collection must exist when the
    /// transaction commits.
    pub fn collection_put(
        &mut self,
        name: &str,
        key: impl AsRef<[u8]>,
        value: impl AsRef<[u8]>,
        ttl: Option<Duration>,
    ) -> Result<()> {
        let key = key.as_ref();
        let value = value.as_ref();
        self.check_limits(key, value.len())?;
        self.collection_writes
            .insert((name.to_owned(), key.to_vec()), Some((value.to_vec(), ttl)));
        Ok(())
    }

    /// Removes `key` from the collection named `name`, in memory until the commit, and returns
    /// whether it held a value (D1124).
    pub fn collection_delete(&mut self, name: &str, key: impl AsRef<[u8]>) -> Result<bool> {
        let key = key.as_ref();
        let existed = self.collection_get(name, key)?.is_some();
        self.collection_writes.insert((name.to_owned(), key.to_vec()), None);
        Ok(existed)
    }

    /// Removes `key`, in memory until the commit, and returns whether it held a value.
    pub fn delete(&mut self, key: impl AsRef<[u8]>) -> Result<bool> {
        let key = key.as_ref();
        if collection::is_reserved(key) {
            return Ok(false);
        }
        let existed = self.get(key)?.is_some();
        self.writes.insert(key.to_vec(), None);
        Ok(existed)
    }

    fn check_limits(&self, key: &[u8], value_bytes: usize) -> Result<()> {
        let max_key = self.database.max_key_bytes();
        if key.len() > max_key {
            return Err(Error::new(
                ErrorKind::TooLarge,
                format!("the key is {} bytes; the most is {max_key}", key.len()),
            ));
        }
        let max_value = self.database.max_value_bytes();
        if value_bytes > max_value {
            return Err(Error::new(
                ErrorKind::TooLarge,
                format!("the value is {value_bytes} bytes; the most is {max_value}"),
            ));
        }
        Ok(())
    }

    /// The entries of the default key space with keys in `range`, this transaction's own
    /// changes included, in key order. Under [`Isolation::Serializable`] the range is checked
    /// at the commit, so a key another transaction adds to it, changes or removes fails the
    /// commit.
    pub fn scan(&mut self, range: impl KeyRange) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let (start, end) = crate::scan::sealed::Bounds::bounds(&range);
        let (start, end) = (owned(start), owned(end));
        let mut fingerprint = Fingerprint::new();
        let mut merged: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        for entry in self.snapshot.scan((start.clone(), end.clone())) {
            let (key, value) = entry?;
            fingerprint.add(&key, &value);
            merged.insert(key, value);
        }
        for (key, written) in &self.writes {
            if !within(key, &start, &end) {
                continue;
            }
            match written {
                Some(value) => {
                    merged.insert(key.clone(), value.clone());
                }
                None => {
                    merged.remove(key);
                }
            }
        }
        if self.isolation == Isolation::Serializable {
            self.ranges.push(ScannedRange {
                start,
                end,
                fingerprint,
            });
        }
        Ok(merged.into_iter().collect())
    }

    /// Checks the transaction against the commits made since it began, and writes its changes
    /// in one commit when they pass. It waits for the database's writer as
    /// [`Database::begin_write`] does. A check that fails returns
    /// [`ErrorKind::Conflict`](crate::ErrorKind::Conflict) and writes nothing.
    pub fn commit(self) -> Result<CommitStamp> {
        let mut writer = self.database.begin_write()?;
        self.check(&mut writer)?;
        self.apply(&mut writer)?;
        writer.commit()
    }

    /// The first phase of a two-phase commit (D1127): checks the transaction as
    /// [`commit`](Self::commit) does and, when it passes, stores its changes durably under
    /// `gid` instead of applying them. [`Database::commit_prepared`] applies them later, from
    /// any connection and after a restart, and [`Database::rollback_prepared`] drops them. A
    /// `gid` already prepared, or longer than 200 bytes, fails with
    /// [`ErrorKind::Conflict`](crate::ErrorKind::Conflict) or
    /// [`ErrorKind::TooLarge`](crate::ErrorKind::TooLarge).
    pub fn prepare(self, gid: &str) -> Result<CommitStamp> {
        if gid.len() > MAX_GID_BYTES {
            return Err(Error::new(
                ErrorKind::TooLarge,
                format!("a transaction identifier is at most {MAX_GID_BYTES} bytes"),
            ));
        }
        let mut writer = self.database.begin_write()?;
        self.check(&mut writer)?;
        let key = prepared_key(gid);
        if writer.get_raw(&key)?.is_some() {
            return Err(Error::new(
                ErrorKind::Conflict,
                format!("transaction identifier \"{gid}\" is already in use"),
            ));
        }
        let record = self.prepared_record(writer.now().unix_micros());
        writer.put_raw(&key, &record)?;
        writer.commit()
    }

    fn check(&self, writer: &mut crate::WriteTransaction<'_>) -> Result<()> {
        match self.isolation {
            Isolation::Serializable => {
                for (key, seen) in &self.reads {
                    if writer.get(key)? != *seen {
                        return Err(conflict("a key it read"));
                    }
                }
                for ((name, key), seen) in &self.collection_reads {
                    if writer.collection(name)?.get(key)? != *seen {
                        return Err(conflict("a collection key it read"));
                    }
                }
                for range in &self.ranges {
                    let mut now = Fingerprint::new();
                    for entry in writer.scan((range.start.clone(), range.end.clone())) {
                        let (key, value) = entry?;
                        now.add(&key, &value);
                    }
                    if now != range.fingerprint {
                        return Err(conflict("a range it scanned"));
                    }
                }
            }
            Isolation::Snapshot => {
                for key in self.writes.keys() {
                    if writer.get(key)? != self.snapshot.get(key)? {
                        return Err(conflict("a key it writes"));
                    }
                }
                for (name, key) in self.collection_writes.keys() {
                    let before = match self.snapshot.collection(name) {
                        Ok(collection) => collection.get(key)?,
                        Err(error) if error.kind() == ErrorKind::CollectionNotFound => None,
                        Err(error) => return Err(error),
                    };
                    if writer.collection(name)?.get(key)? != before {
                        return Err(conflict("a collection key it writes"));
                    }
                }
            }
        }
        Ok(())
    }

    fn changes(&self) -> Vec<Change> {
        let mut changes = Vec::with_capacity(self.writes.len() + self.collection_writes.len());
        for (key, written) in &self.writes {
            changes.push(match written {
                Some(value) => Change::Put {
                    key: key.clone(),
                    value: value.clone(),
                    ttl: self.expiring.get(key).copied(),
                },
                None => Change::Delete { key: key.clone() },
            });
        }
        for ((name, key), written) in &self.collection_writes {
            changes.push(match written {
                Some((value, ttl)) => Change::CollectionPut {
                    name: name.clone(),
                    key: key.clone(),
                    value: value.clone(),
                    ttl: *ttl,
                },
                None => Change::CollectionDelete {
                    name: name.clone(),
                    key: key.clone(),
                },
            });
        }
        changes
    }

    fn apply(&self, writer: &mut crate::WriteTransaction<'_>) -> Result<()> {
        apply_changes(writer, &self.changes())
    }

    fn prepared_record(&self, prepared_at_micros: i64) -> Vec<u8> {
        encode_prepared(prepared_at_micros, &self.changes())
    }

    /// Gives up the transaction's changes. Dropping it does the same.
    pub fn rollback(self) {}
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Change {
    Put {
        key: Vec<u8>,
        value: Vec<u8>,
        ttl: Option<Duration>,
    },
    Delete {
        key: Vec<u8>,
    },
    CollectionPut {
        name: String,
        key: Vec<u8>,
        value: Vec<u8>,
        ttl: Option<Duration>,
    },
    CollectionDelete {
        name: String,
        key: Vec<u8>,
    },
}

fn apply_changes(writer: &mut crate::WriteTransaction<'_>, changes: &[Change]) -> Result<()> {
    for change in changes {
        match change {
            Change::Put { key, value, ttl: Some(ttl) } => writer.put_with_expiry(key, value, *ttl)?,
            Change::Put { key, value, ttl: None } => writer.put(key, value)?,
            Change::Delete { key } => {
                writer.delete(key)?;
            }
            Change::CollectionPut { name, key, value, ttl } => {
                let mut collection = writer.collection(name)?;
                match ttl {
                    Some(ttl) => collection.put_with_expiry(key, value, *ttl)?,
                    None => collection.put(key, value)?,
                }
            }
            Change::CollectionDelete { name, key } => {
                writer.collection(name)?.delete(key)?;
            }
        }
    }
    Ok(())
}

fn prepared_prefix() -> Vec<u8> {
    let mut key = crate::collection::COLLECTIONS_PREFIX.to_vec();
    key.push(PREPARED_TAG);
    key
}

fn prepared_key(gid: &str) -> Vec<u8> {
    let mut key = prepared_prefix();
    key.extend_from_slice(gid.as_bytes());
    key
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn put_ttl(out: &mut Vec<u8>, ttl: Option<Duration>) {
    let millis = ttl.map_or(u64::MAX, |ttl| u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX - 1));
    out.extend_from_slice(&millis.to_be_bytes());
}

fn encode_prepared(prepared_at_micros: i64, changes: &[Change]) -> Vec<u8> {
    let mut out = vec![PREPARED_RECORD_VERSION];
    out.extend_from_slice(&prepared_at_micros.to_be_bytes());
    out.extend_from_slice(&u32::try_from(changes.len()).unwrap_or(u32::MAX).to_be_bytes());
    for change in changes {
        match change {
            Change::Put { key, value, ttl } => {
                out.push(0);
                put_bytes(&mut out, key);
                put_bytes(&mut out, value);
                put_ttl(&mut out, *ttl);
            }
            Change::Delete { key } => {
                out.push(1);
                put_bytes(&mut out, key);
            }
            Change::CollectionPut { name, key, value, ttl } => {
                out.push(2);
                put_bytes(&mut out, name.as_bytes());
                put_bytes(&mut out, key);
                put_bytes(&mut out, value);
                put_ttl(&mut out, *ttl);
            }
            Change::CollectionDelete { name, key } => {
                out.push(3);
                put_bytes(&mut out, name.as_bytes());
                put_bytes(&mut out, key);
            }
        }
    }
    out
}

struct RecordReader<'r> {
    bytes: &'r [u8],
}

impl<'r> RecordReader<'r> {
    fn damaged() -> Error {
        Error::new(ErrorKind::Corrupt, "a prepared transaction's record is damaged")
    }

    fn take(&mut self, count: usize) -> Result<&'r [u8]> {
        if self.bytes.len() < count {
            return Err(Self::damaged());
        }
        let (taken, rest) = self.bytes.split_at(count);
        self.bytes = rest;
        Ok(taken)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn word(&mut self) -> Result<u32> {
        let bytes: [u8; 4] = self.take(4)?.try_into().map_err(|_| Self::damaged())?;
        Ok(u32::from_be_bytes(bytes))
    }

    fn long(&mut self) -> Result<u64> {
        let bytes: [u8; 8] = self.take(8)?.try_into().map_err(|_| Self::damaged())?;
        Ok(u64::from_be_bytes(bytes))
    }

    fn bytes(&mut self) -> Result<Vec<u8>> {
        let length = usize::try_from(self.word()?).map_err(|_| Self::damaged())?;
        Ok(self.take(length)?.to_vec())
    }

    fn text(&mut self) -> Result<String> {
        String::from_utf8(self.bytes()?).map_err(|_| Self::damaged())
    }

    fn ttl(&mut self) -> Result<Option<Duration>> {
        let millis = self.long()?;
        Ok((millis != u64::MAX).then(|| Duration::from_millis(millis)))
    }
}

fn decode_prepared(bytes: &[u8]) -> Result<(i64, Vec<Change>)> {
    let mut reader = RecordReader { bytes };
    if reader.byte()? != PREPARED_RECORD_VERSION {
        return Err(RecordReader::damaged());
    }
    let prepared_at = reader.long()? as i64;
    let count = reader.word()?;
    let mut changes = Vec::new();
    for _ in 0..count {
        changes.push(match reader.byte()? {
            0 => Change::Put {
                key: reader.bytes()?,
                value: reader.bytes()?,
                ttl: reader.ttl()?,
            },
            1 => Change::Delete { key: reader.bytes()? },
            2 => Change::CollectionPut {
                name: reader.text()?,
                key: reader.bytes()?,
                value: reader.bytes()?,
                ttl: reader.ttl()?,
            },
            3 => Change::CollectionDelete {
                name: reader.text()?,
                key: reader.bytes()?,
            },
            _ => return Err(RecordReader::damaged()),
        });
    }
    if !reader.bytes.is_empty() {
        return Err(RecordReader::damaged());
    }
    Ok((prepared_at, changes))
}

/// A transaction prepared for a two-phase commit and not yet committed or rolled back (D1127).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedTransaction {
    /// The identifier it was prepared under.
    pub gid: String,
    /// When it was prepared, in microseconds since the Unix epoch.
    pub prepared_at_micros: i64,
    /// How many changes it holds.
    pub changes: usize,
}

impl Database {
    /// The second phase of a two-phase commit (D1127): applies the changes prepared under
    /// `gid` (see [`ConcurrentTransaction::prepare`]) and forgets them, in one commit. `false`
    /// when no transaction is prepared under that identifier.
    pub fn commit_prepared(&self, gid: &str) -> Result<bool> {
        let mut writer = self.begin_write()?;
        let key = prepared_key(gid);
        let Some(record) = writer.get_raw(&key)? else {
            return Ok(false);
        };
        let (_, changes) = decode_prepared(&record)?;
        apply_changes(&mut writer, &changes)?;
        writer.delete_raw(&key)?;
        writer.commit()?;
        Ok(true)
    }

    /// Drops the changes prepared under `gid`; `false` when none are.
    pub fn rollback_prepared(&self, gid: &str) -> Result<bool> {
        let mut writer = self.begin_write()?;
        if !writer.delete_raw(&prepared_key(gid))? {
            return Ok(false);
        }
        writer.commit()?;
        Ok(true)
    }

    /// The transactions prepared and not yet resolved, in identifier order, as PostgreSQL's
    /// `pg_prepared_xacts` and MySQL's `XA RECOVER` list them.
    pub fn prepared_transactions(&self) -> Result<Vec<PreparedTransaction>> {
        let read = self.begin_read()?;
        let prefix = prepared_prefix();
        let mut end = prefix.clone();
        if let Some(last) = end.last_mut() {
            *last += 1;
        }
        let mut prepared = Vec::new();
        let mut scan = crate::scan::Scan::within_between(read.reader(), read.tree(), b"", &(prefix.clone()..end));
        while let Some((key, value)) = scan.next_entry()? {
            let gid = String::from_utf8_lossy(key.get(prefix.len()..).unwrap_or_default()).into_owned();
            let (prepared_at_micros, changes) = decode_prepared(value)?;
            prepared.push(PreparedTransaction {
                gid,
                prepared_at_micros,
                changes: changes.len(),
            });
        }
        Ok(prepared)
    }
}

#[cfg(test)]
mod prepared_tests {
    use super::{Change, decode_prepared, encode_prepared};
    use std::time::Duration;

    #[test]
    fn a_prepared_record_reads_back_every_kind_of_change() {
        let changes = vec![
            Change::Put { key: b"a".to_vec(), value: b"1".to_vec(), ttl: None },
            Change::Put { key: b"b".to_vec(), value: Vec::new(), ttl: Some(Duration::from_secs(5)) },
            Change::Delete { key: b"c".to_vec() },
            Change::CollectionPut { name: "carts".into(), key: b"k".to_vec(), value: b"v".to_vec(), ttl: None },
            Change::CollectionDelete { name: "carts".into(), key: b"gone".to_vec() },
        ];
        let record = encode_prepared(1_700_000_000_000_000, &changes);
        assert_eq!(decode_prepared(&record).unwrap(), (1_700_000_000_000_000, changes));
        assert!(decode_prepared(&record[..record.len() - 1]).is_err());
    }
}
