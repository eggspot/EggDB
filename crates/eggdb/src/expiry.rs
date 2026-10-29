use crate::collection::COLLECTIONS_PREFIX;
use crate::database::Database;
use crate::error::{Error, ErrorKind, Result};
use crate::scan::{Reader, Scan};
use crate::transaction::{ReadTransaction, WriteTransaction};
use eggdb_storage::BTree;
use std::time::Duration;

const DEADLINE_TAG: u8 = 0x04;
const BY_DEADLINE_TAG: u8 = 0x05;
const RECORD_VERSION: u8 = 1;
const RECORD_BYTES: usize = 9;
const DEADLINE_BYTES: usize = 8;
const SWEEP_KEYS_PER_COMMIT: usize = 32;
const SWEEP_KEYS_PER_BATCH: usize = 1024;
const LONGEST_COUNTER_TEXT: usize = 20;
const SHOWN_VALUE_BYTES: usize = 32;

pub(crate) const EXPIRY_KEY_OVERHEAD_BYTES: usize = COLLECTIONS_PREFIX.len() + 1 + DEADLINE_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Moment(u64);

impl Moment {
    pub(crate) const fn from_unix_micros(micros: u64) -> Self {
        Self(micros)
    }

    pub(crate) const fn unix_micros(self) -> u64 {
        self.0
    }

    pub(crate) fn after(self, ttl: Duration) -> Self {
        let micros = u64::try_from(ttl.as_micros()).unwrap_or(u64::MAX).max(1);
        Self(self.0.saturating_add(micros))
    }

    pub(crate) fn has_reached(self, deadline: Self) -> bool {
        self.0 >= deadline.0
    }

    pub(crate) fn until(self, deadline: Self) -> Option<Duration> {
        deadline
            .0
            .checked_sub(self.0)
            .filter(|&micros| micros > 0)
            .map(Duration::from_micros)
    }
}

fn tagged(tag: u8, parts: &[&[u8]]) -> Vec<u8> {
    let length = parts.iter().map(|part| part.len()).sum::<usize>();
    let mut key = Vec::with_capacity(COLLECTIONS_PREFIX.len() + 1 + length);
    key.extend_from_slice(COLLECTIONS_PREFIX);
    key.push(tag);
    for part in parts {
        key.extend_from_slice(part);
    }
    key
}

pub(crate) fn deadline_key(stored: &[u8]) -> Vec<u8> {
    tagged(DEADLINE_TAG, &[stored])
}

pub(crate) fn by_deadline_key(deadline: Moment, stored: &[u8]) -> Vec<u8> {
    tagged(BY_DEADLINE_TAG, &[&deadline.0.to_be_bytes(), stored])
}

pub(crate) fn by_deadline_prefix() -> Vec<u8> {
    tagged(BY_DEADLINE_TAG, &[])
}

pub(crate) fn record(deadline: Moment) -> [u8; RECORD_BYTES] {
    let mut bytes = [0u8; RECORD_BYTES];
    bytes[0] = RECORD_VERSION;
    bytes[1..].copy_from_slice(&deadline.0.to_be_bytes());
    bytes
}

pub(crate) fn read_record(bytes: &[u8]) -> Result<Moment> {
    match bytes {
        [RECORD_VERSION, deadline @ ..] if deadline.len() == DEADLINE_BYTES => {
            let mut be = [0u8; DEADLINE_BYTES];
            be.copy_from_slice(deadline);
            Ok(Moment(u64::from_be_bytes(be)))
        }
        [version, ..] if *version > RECORD_VERSION => Err(Error::new(
            ErrorKind::Incompatible,
            format!(
                "a key's time to live is in version {version} of its record, written by a newer EggDB; this release reads version {RECORD_VERSION}. Upgrade eggdb from https://github.com/eggspot/EggDB/releases"
            ),
        )),
        _ => Err(Error::new(
            ErrorKind::Corrupt,
            format!(
                "a key's time to live is damaged: its record is {} bytes, not a version byte and an 8-byte deadline; restore from a backup or run `eggdb check`",
                bytes.len()
            ),
        )),
    }
}

fn split_by_deadline(rest: &[u8]) -> Result<(Moment, &[u8])> {
    match rest.split_first_chunk::<DEADLINE_BYTES>() {
        Some((deadline, stored)) => Ok((Moment(u64::from_be_bytes(*deadline)), stored)),
        None => Err(Error::new(
            ErrorKind::Corrupt,
            "an entry of the index of keys by deadline is shorter than its 8-byte deadline; restore from a backup or run `eggdb check`",
        )),
    }
}

pub(crate) fn deadline_in(reader: Reader<'_>, tree: &BTree, stored: &[u8]) -> Result<Option<Moment>> {
    reader
        .get(tree, &deadline_key(stored))?
        .map(|bytes| read_record(&bytes))
        .transpose()
}

pub(crate) fn has_expired(
    reader: Reader<'_>,
    tree: &BTree,
    stored: &[u8],
    now: Moment,
) -> Result<bool> {
    Ok(deadline_in(reader, tree, stored)?.is_some_and(|deadline| now.has_reached(deadline)))
}

pub(crate) fn time_left(
    reader: Reader<'_>,
    tree: Option<BTree>,
    stored: &[u8],
    now: Option<Moment>,
) -> Result<Option<Duration>> {
    let (Some(tree), Some(now)) = (tree, now) else {
        return Ok(None);
    };
    Ok(deadline_in(reader, &tree, stored)?.and_then(|deadline| now.until(deadline)))
}

pub(crate) fn parse_counter(value: &[u8]) -> Result<i64> {
    let digits = value.strip_prefix(b"-").unwrap_or(value);
    let well_formed = !digits.is_empty()
        && value.len() <= LONGEST_COUNTER_TEXT
        && digits.iter().all(u8::is_ascii_digit)
        && (digits.len() == 1 || digits[0] != b'0')
        && value != b"-0";
    let parsed = std::str::from_utf8(value)
        .ok()
        .filter(|_| well_formed)
        .and_then(|text| text.parse::<i64>().ok());
    parsed.ok_or_else(|| {
        Error::new(
            ErrorKind::NotAnInteger,
            format!(
                "the value {} is not a counter: a counter is a decimal integer from {} to {}, written as text without spaces, a plus sign or leading zeros",
                shown_value(value),
                i64::MIN,
                i64::MAX
            ),
        )
    })
}

fn shown_value(value: &[u8]) -> String {
    let cut = &value[..value.len().min(SHOWN_VALUE_BYTES)];
    let text: String = String::from_utf8_lossy(cut)
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    if cut.len() < value.len() {
        format!("\"{text}…\" ({} bytes)", value.len())
    } else {
        format!("\"{text}\"")
    }
}

fn overflow(current: i64, by: i64) -> Error {
    Error::new(
        ErrorKind::NotAnInteger,
        format!(
            "adding {by} to the counter {current} would leave the range of a counter, {} to {}; nothing changed",
            i64::MIN,
            i64::MAX
        ),
    )
}

impl WriteTransaction<'_> {
    pub(crate) fn deadline_of(&self, stored: &[u8]) -> Result<Option<Moment>> {
        match (self.tree(), self.expiry_now()) {
            (Some(tree), Some(_)) => deadline_in(self.reader(), &tree, stored),
            _ => Ok(None),
        }
    }

    pub(crate) fn is_expired(&self, stored: &[u8]) -> Result<bool> {
        let Some(now) = self.expiry_now() else {
            return Ok(false);
        };
        Ok(self
            .deadline_of(stored)?
            .is_some_and(|deadline| now.has_reached(deadline)))
    }

    pub(crate) fn get_live(&self, stored: &[u8]) -> Result<Option<Vec<u8>>> {
        let value = self.get_raw(stored)?;
        if value.is_some() && self.is_expired(stored)? {
            return Ok(None);
        }
        Ok(value)
    }

    pub(crate) fn get_live_with<R>(
        &self,
        stored: &[u8],
        read: impl FnOnce(&[u8]) -> R,
    ) -> Result<Option<R>> {
        let Some(tree) = self.tree() else {
            return Ok(None);
        };
        if self.is_expired(stored)? {
            return Ok(None);
        }
        self.reader().get_with(&tree, stored, read)
    }

    pub(crate) fn put_live(&mut self, stored: &[u8], value: &[u8]) -> Result<()> {
        self.put_raw(stored, value)?;
        self.clear_deadline(stored)?;
        Ok(())
    }

    pub(crate) fn put_if_absent_live(&mut self, stored: &[u8], value: &[u8]) -> Result<bool> {
        if self.is_expired(stored)? {
            self.remove_stored(stored)?;
        }
        self.put_if_absent_raw(stored, value)
    }

    pub(crate) fn delete_live(&mut self, stored: &[u8]) -> Result<bool> {
        let expired = self.is_expired(stored)?;
        self.clear_deadline(stored)?;
        let deleted = self.delete_raw(stored)?;
        Ok(deleted && !expired)
    }

    pub(crate) fn remove_stored(&mut self, stored: &[u8]) -> Result<()> {
        self.clear_deadline(stored)?;
        self.delete_raw(stored)?;
        Ok(())
    }

    fn clear_deadline(&mut self, stored: &[u8]) -> Result<bool> {
        let Some(deadline) = self.deadline_of(stored)? else {
            return Ok(false);
        };
        self.delete_raw(&deadline_key(stored))?;
        self.delete_raw(&by_deadline_key(deadline, stored))?;
        Ok(true)
    }

    fn check_expiry_fits(&self, stored: &[u8], shown_key_bytes: usize) -> Result<()> {
        let longest = self.max_key_bytes();
        if stored.len().saturating_add(EXPIRY_KEY_OVERHEAD_BYTES) <= longest {
            return Ok(());
        }
        let most = longest
            .saturating_sub(EXPIRY_KEY_OVERHEAD_BYTES)
            .saturating_sub(stored.len().saturating_sub(shown_key_bytes));
        Err(Error::new(
            ErrorKind::TooLarge,
            format!(
                "a key of {shown_key_bytes} bytes is too long for a time to live: a key with one is at most {most} bytes with this page size, {EXPIRY_KEY_OVERHEAD_BYTES} fewer than without, since its deadline is kept in a key of its own; nothing changed"
            ),
        ))
    }

    fn set_deadline(&mut self, stored: &[u8], deadline: Moment) -> Result<()> {
        self.clear_deadline(stored)?;
        self.note_expiring_keys();
        self.put_raw(&deadline_key(stored), &record(deadline))?;
        self.put_raw(&by_deadline_key(deadline, stored), b"")
    }

    pub(crate) fn put_expiring(
        &mut self,
        stored: &[u8],
        shown_key_bytes: usize,
        value: &[u8],
        ttl: Duration,
    ) -> Result<()> {
        if ttl.is_zero() {
            return self.remove_stored(stored);
        }
        self.check_expiry_fits(stored, shown_key_bytes)?;
        self.put_raw(stored, value)?;
        let deadline = self.now().after(ttl);
        self.set_deadline(stored, deadline)
    }

    pub(crate) fn expire_stored(
        &mut self,
        stored: &[u8],
        shown_key_bytes: usize,
        ttl: Duration,
    ) -> Result<bool> {
        if self.get_live_with(stored, |_| ())?.is_none() {
            if self.get_raw_with(stored)? {
                self.remove_stored(stored)?;
            }
            return Ok(false);
        }
        if ttl.is_zero() {
            self.remove_stored(stored)?;
            return Ok(true);
        }
        self.check_expiry_fits(stored, shown_key_bytes)?;
        let deadline = self.now().after(ttl);
        self.set_deadline(stored, deadline)?;
        Ok(true)
    }

    fn get_raw_with(&self, stored: &[u8]) -> Result<bool> {
        let Some(tree) = self.tree() else {
            return Ok(false);
        };
        Ok(self.reader().get_with(&tree, stored, |_| ())?.is_some())
    }

    pub(crate) fn persist_stored(&mut self, stored: &[u8]) -> Result<bool> {
        if self.is_expired(stored)? {
            self.remove_stored(stored)?;
            return Ok(false);
        }
        self.clear_deadline(stored)
    }

    pub(crate) fn time_to_live_stored(&self, stored: &[u8]) -> Result<Option<Duration>> {
        time_left(self.reader(), self.tree(), stored, self.expiry_now())
    }

    pub(crate) fn increment_stored(&mut self, stored: &[u8], by: i64) -> Result<i64> {
        let current = match self.get_live(stored)? {
            Some(value) => parse_counter(&value)?,
            None => {
                self.clear_deadline(stored)?;
                0
            }
        };
        let next = current.checked_add(by).ok_or_else(|| overflow(current, by))?;
        self.put_raw(stored, next.to_string().as_bytes())?;
        Ok(next)
    }

    pub(crate) fn compare_and_set_stored(
        &mut self,
        stored: &[u8],
        expected: Option<&[u8]>,
        new: Option<&[u8]>,
    ) -> Result<bool> {
        let current = self.get_live(stored)?;
        if current.as_deref() != expected {
            return Ok(false);
        }
        match new {
            Some(value) => self.put_live(stored, value)?,
            None => self.remove_stored(stored)?,
        }
        Ok(true)
    }

    pub(crate) fn remove_expired_keys(&mut self, limit: usize) -> Result<usize> {
        let (Some(tree), Some(now)) = (self.tree(), self.expiry_now()) else {
            return Ok(0);
        };
        let mut due = Vec::new();
        {
            let prefix = by_deadline_prefix();
            let mut scan = Scan::within_prefix(self.reader(), Some(tree), &prefix, b"");
            while due.len() < limit {
                let Some((rest, _)) = scan.next_key_and_value_len()? else {
                    break;
                };
                let (deadline, stored) = split_by_deadline(rest)?;
                if !now.has_reached(deadline) {
                    break;
                }
                due.push((deadline, stored.to_vec()));
            }
        }
        for (deadline, stored) in &due {
            if self.deadline_of(stored)? == Some(*deadline) {
                self.delete_raw(stored)?;
                self.delete_raw(&deadline_key(stored))?;
            }
            self.delete_raw(&by_deadline_key(*deadline, stored))?;
        }
        Ok(due.len())
    }

    pub(crate) fn sweep_before_commit(&mut self) -> Result<()> {
        self.remove_expired_keys(SWEEP_KEYS_PER_COMMIT).map(drop)
    }

    /// Gives `key` the `value` and a time to live of `ttl`: the key expires once `ttl` has
    /// passed by the database's [clock](crate::Options::clock), and from then on no read sees
    /// it and the next commits remove it. A later [`put`](Self::put) of the key replaces the
    /// value and removes the time to live; [`increment`](Self::increment) keeps it. A `ttl` of
    /// zero removes the key.
    ///
    /// A key with a time to live is at most [`Database::max_key_bytes`] less 14 bytes long,
    /// since its deadline is kept in a key of its own; a longer one fails with
    /// [`ErrorKind::TooLarge`] and changes nothing. Other limits and failures are those of
    /// [`put`](Self::put).
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-put-with-expiry-{}.db", std::process::id()));
    /// use std::time::Duration;
    ///
    /// let db = eggdb::Database::open(&path)?;
    /// db.write(|tx| {
    ///     tx.put_with_expiry("session:1", "alice", Duration::from_secs(3600))?;
    ///     assert!(tx.time_to_live("session:1")?.is_some());
    ///     tx.put("session:1", "bob")?;
    ///     assert_eq!(tx.time_to_live("session:1")?, None);
    ///     Ok(())
    /// })?;
    /// # drop(db);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(()) }
    /// ```
    pub fn put_with_expiry(
        &mut self,
        key: impl AsRef<[u8]>,
        value: impl AsRef<[u8]>,
        ttl: Duration,
    ) -> Result<()> {
        let key = key.as_ref();
        crate::collection::refuse_reserved(key)?;
        self.put_expiring(key, key.len(), value.as_ref(), ttl)
    }

    /// Gives the key a time to live of `ttl` from now, replacing any it had, and returns
    /// whether the key was there; an absent key stays absent. A `ttl` of zero removes the key.
    /// Keys longer than [`put_with_expiry`](Self::put_with_expiry) allows fail with
    /// [`ErrorKind::TooLarge`].
    pub fn set_expiry(&mut self, key: impl AsRef<[u8]>, ttl: Duration) -> Result<bool> {
        let key = key.as_ref();
        if crate::collection::is_reserved(key) {
            return Ok(false);
        }
        self.expire_stored(key, key.len(), ttl)
    }

    /// Removes the key's time to live, so it no longer expires, and returns whether it had
    /// one. An absent key has none.
    pub fn clear_expiry(&mut self, key: impl AsRef<[u8]>) -> Result<bool> {
        let key = key.as_ref();
        if crate::collection::is_reserved(key) {
            return Ok(false);
        }
        self.persist_stored(key)
    }

    /// How long the key has left before it expires, this transaction's changes included, or
    /// `None` when it has no time to live or is absent. Time is the database's
    /// [clock](crate::Options::clock) when the transaction began.
    pub fn time_to_live(&self, key: impl AsRef<[u8]>) -> Result<Option<Duration>> {
        let key = key.as_ref();
        if crate::collection::is_reserved(key) {
            return Ok(None);
        }
        self.time_to_live_stored(key)
    }

    /// Adds `by` to the counter stored under `key` and returns the new count; a negative `by`
    /// subtracts. A counter is a decimal integer stored as text, as `"42"` or `"-7"`, from
    /// `i64::MIN` to `i64::MAX`, and an absent key counts as 0. A value that is not a counter,
    /// or a sum outside that range, fails with [`ErrorKind::NotAnInteger`] and changes nothing.
    /// The key keeps its time to live.
    ///
    /// The read and the write are one step of the one write transaction, so increments from
    /// any number of threads never lose one another's counts.
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-increment-{}.db", std::process::id()));
    /// let db = eggdb::Database::open(&path)?;
    /// let count = db.write(|tx| {
    ///     tx.increment("visits", 1)?;
    ///     tx.increment("visits", 10)
    /// })?;
    /// assert_eq!(count, 11);
    /// assert_eq!(db.get("visits")?, Some(b"11".to_vec()));
    /// # drop(db);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(()) }
    /// ```
    pub fn increment(&mut self, key: impl AsRef<[u8]>, by: i64) -> Result<i64> {
        let key = key.as_ref();
        crate::collection::refuse_reserved(key)?;
        self.increment_stored(key, by)
    }

    /// Stores `new` under `key`, or removes the key when `new` is `None`, only when the value
    /// there now is `expected` (`None`: the key is absent), and returns whether it did. A value
    /// stored this way has no time to live, as after [`put`](Self::put).
    ///
    /// ```
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let path = std::env::temp_dir().join(format!("eggdb-doc-cas-{}.db", std::process::id()));
    /// let db = eggdb::Database::open(&path)?;
    /// db.write(|tx| {
    ///     assert!(tx.compare_and_set("lock", None, Some(b"me".as_slice()))?);
    ///     assert!(!tx.compare_and_set("lock", None, Some(b"you".as_slice()))?);
    ///     assert!(tx.compare_and_set("lock", Some(b"me".as_slice()), None)?);
    ///     Ok(())
    /// })?;
    /// assert_eq!(db.get("lock")?, None);
    /// # drop(db);
    /// # std::fs::remove_file(&path)?;
    /// # Ok(()) }
    /// ```
    pub fn compare_and_set(
        &mut self,
        key: impl AsRef<[u8]>,
        expected: Option<&[u8]>,
        new: Option<&[u8]>,
    ) -> Result<bool> {
        let key = key.as_ref();
        if new.is_some() {
            crate::collection::refuse_reserved(key)?;
        } else if crate::collection::is_reserved(key) {
            return Ok(expected.is_none());
        }
        self.compare_and_set_stored(key, expected, new)
    }
}

impl ReadTransaction<'_> {
    /// How long the key has left before it expires, or `None` when it has no time to live or
    /// is absent. Time is the database's [clock](crate::Options::clock) when the transaction
    /// began, so a key that expires while the transaction lives stays readable in it.
    pub fn time_to_live(&self, key: impl AsRef<[u8]>) -> Result<Option<Duration>> {
        let key = key.as_ref();
        if crate::collection::is_reserved(key) {
            return Ok(None);
        }
        time_left(self.reader(), self.tree(), key, self.expiry_now())
    }
}

impl Database {
    /// Stores `value` under `key` with a time to live of `ttl`, and commits (see
    /// [`WriteTransaction::put_with_expiry`]).
    pub fn put_with_expiry(
        &self,
        key: impl AsRef<[u8]>,
        value: impl AsRef<[u8]>,
        ttl: Duration,
    ) -> Result<()> {
        self.write(|transaction| transaction.put_with_expiry(key, value, ttl))
    }

    /// Gives the key a time to live of `ttl` from now and commits, and returns whether the key
    /// was there (see [`WriteTransaction::set_expiry`]). Nothing is committed when it was
    /// absent.
    pub fn set_expiry(&self, key: impl AsRef<[u8]>, ttl: Duration) -> Result<bool> {
        self.write_if(|transaction| transaction.set_expiry(key, ttl))
    }

    /// Removes the key's time to live and commits, and returns whether it had one (see
    /// [`WriteTransaction::clear_expiry`]). Nothing is committed when it had none.
    pub fn clear_expiry(&self, key: impl AsRef<[u8]>) -> Result<bool> {
        self.write_if(|transaction| transaction.clear_expiry(key))
    }

    /// How long the key has left before it expires as of the last commit, or `None` when it has
    /// no time to live or is absent.
    pub fn time_to_live(&self, key: impl AsRef<[u8]>) -> Result<Option<Duration>> {
        self.begin_read()?.time_to_live(key)
    }

    /// Adds `by` to the counter under `key`, commits, and returns the new count (see
    /// [`WriteTransaction::increment`]).
    pub fn increment(&self, key: impl AsRef<[u8]>, by: i64) -> Result<i64> {
        self.write(|transaction| transaction.increment(key, by))
    }

    /// Stores `new` under `key` (or removes it, for `None`) only when its value is `expected`,
    /// commits, and returns whether it did (see [`WriteTransaction::compare_and_set`]).
    /// Nothing is committed when the value was not `expected`.
    pub fn compare_and_set(
        &self,
        key: impl AsRef<[u8]>,
        expected: Option<&[u8]>,
        new: Option<&[u8]>,
    ) -> Result<bool> {
        self.write_if(|transaction| transaction.compare_and_set(key, expected, new))
    }

    /// Removes every key whose time to live has run out, in the default key space and in every
    /// collection, in commits of up to 1,024 keys, and returns how many it removed. Reads never
    /// see such keys anyway, and every commit removes up to 32 of them by itself; this frees
    /// their space at once, for example before a backup.
    pub fn remove_expired(&self) -> Result<u64> {
        let mut removed = 0u64;
        loop {
            let mut transaction = self.begin_write()?;
            transaction.skip_sweep_on_commit();
            let batch = transaction.remove_expired_keys(SWEEP_KEYS_PER_BATCH)?;
            if batch == 0 {
                transaction.rollback()?;
                return Ok(removed);
            }
            transaction.commit()?;
            removed += batch as u64;
            if batch < SWEEP_KEYS_PER_BATCH {
                return Ok(removed);
            }
        }
    }

    pub(crate) fn write_if(
        &self,
        change: impl FnOnce(&mut WriteTransaction<'_>) -> Result<bool>,
    ) -> Result<bool> {
        let mut transaction = self.begin_write()?;
        if change(&mut transaction)? {
            transaction.commit()?;
            Ok(true)
        } else {
            transaction.rollback()?;
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EXPIRY_KEY_OVERHEAD_BYTES, Moment, by_deadline_key, by_deadline_prefix, deadline_key,
        parse_counter, read_record, record, split_by_deadline,
    };
    use crate::ErrorKind;
    use crate::collection::is_reserved;
    use std::time::Duration;

    #[test]
    fn the_key_layout_matches_its_test_vectors() {
        assert_eq!(deadline_key(b"k"), b"\xffCOL\x00\x04k");
        assert_eq!(
            by_deadline_key(Moment::from_unix_micros(0x0102_0304_0506_0708), b"k"),
            b"\xffCOL\x00\x05\x01\x02\x03\x04\x05\x06\x07\x08k"
        );
        assert_eq!(by_deadline_prefix(), b"\xffCOL\x00\x05");
        assert_eq!(
            record(Moment::from_unix_micros(0x0102_0304_0506_0708)),
            [1, 1, 2, 3, 4, 5, 6, 7, 8]
        );
        assert_eq!(EXPIRY_KEY_OVERHEAD_BYTES, 14);
        assert_eq!(
            by_deadline_key(Moment::from_unix_micros(1), b"").len(),
            EXPIRY_KEY_OVERHEAD_BYTES
        );
        assert!(is_reserved(&deadline_key(b"")) && is_reserved(&by_deadline_prefix()));
    }

    #[test]
    fn the_index_sorts_by_deadline_before_key() {
        let early = by_deadline_key(Moment::from_unix_micros(5), b"zzz");
        let late = by_deadline_key(Moment::from_unix_micros(6), b"aaa");
        let later = by_deadline_key(Moment::from_unix_micros(256), b"");
        assert!(early < late && late < later);
        let prefix = by_deadline_prefix();
        let (deadline, stored) = split_by_deadline(&early[prefix.len()..]).unwrap();
        assert_eq!((deadline, stored), (Moment::from_unix_micros(5), b"zzz".as_slice()));
        let short = split_by_deadline(&[0; 7]).unwrap_err();
        assert_eq!(short.kind(), ErrorKind::Corrupt);
    }

    #[test]
    fn records_round_trip_and_damage_is_an_error() {
        for micros in [0, 1, u64::MAX] {
            let moment = Moment::from_unix_micros(micros);
            assert_eq!(read_record(&record(moment)).ok(), Some(moment));
        }
        let damaged_records: [&[u8]; 4] = [&[], &[1], &[1, 0, 0, 0, 0, 0, 0, 0], &[0; 9]];
        for damaged in damaged_records {
            assert_eq!(read_record(damaged).unwrap_err().kind(), ErrorKind::Corrupt);
        }
        let newer = read_record(&[2, 0, 0, 0, 0, 0, 0, 0, 1]).unwrap_err();
        assert_eq!(newer.kind(), ErrorKind::Incompatible);
    }

    #[test]
    fn a_moment_reaches_its_deadline_exactly_and_never_overflows() {
        let now = Moment::from_unix_micros(1_000);
        let deadline = now.after(Duration::from_millis(2));
        assert_eq!(deadline, Moment::from_unix_micros(3_000));
        assert!(!Moment::from_unix_micros(2_999).has_reached(deadline));
        assert!(Moment::from_unix_micros(3_000).has_reached(deadline));
        assert_eq!(now.until(deadline), Some(Duration::from_micros(2_000)));
        assert_eq!(deadline.until(deadline), None);
        assert_eq!(now.after(Duration::from_nanos(1)), Moment::from_unix_micros(1_001));
        assert_eq!(now.after(Duration::MAX), Moment::from_unix_micros(u64::MAX));
    }

    #[test]
    fn counters_are_decimal_integers_written_plainly() {
        for (text, value) in [
            ("0", 0),
            ("42", 42),
            ("-7", -7),
            ("9223372036854775807", i64::MAX),
            ("-9223372036854775808", i64::MIN),
        ] {
            assert_eq!(parse_counter(text.as_bytes()).ok(), Some(value), "{text}");
        }
        for bad in [
            "", "-", "+1", "01", "-0", " 1", "1 ", "1.5", "0x10", "9223372036854775808",
            "-9223372036854775809", "１",
        ] {
            let error = parse_counter(bad.as_bytes()).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::NotAnInteger, "{bad:?}");
        }
        let long = parse_counter(&[b'x'; 100]).unwrap_err().to_string();
        assert!(long.contains("(100 bytes)"), "{long}");
    }

    #[test]
    fn hostile_values_and_records_are_errors_never_panics() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let alphabet = b"0123456789-+ x\x00\xff";
        for _ in 0..20_000 {
            let length = (next() % 24) as usize;
            let value: Vec<u8> = (0..length)
                .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
                .collect();
            if let Ok(count) = parse_counter(&value) {
                assert_eq!(count.to_string().as_bytes(), value.as_slice());
            }
            let record_bytes: Vec<u8> = (0..(next() % 12)).map(|_| next() as u8).collect();
            let _ = read_record(&record_bytes);
            let _ = split_by_deadline(&record_bytes);
        }
    }
}
