use crate::args;
use crate::status::{Failure, guarded};
use crate::transaction::{EggdbTxn, Transaction};
use eggdb::Scan;
use std::ffi::{c_char, c_int};
use std::ops::Bound;

/// `eggdb_scan_open` flag: the entries from the last key down to the first.
pub const EGGDB_SCAN_REVERSE: u32 = 1;
/// `eggdb_scan_open` flag: the range includes `end` itself.
pub const EGGDB_SCAN_END_INCLUSIVE: u32 = 2;
/// `eggdb_scan_open` flag: `start` is a prefix, and the scan holds the keys that begin with
/// it; `end` must be NULL.
pub const EGGDB_SCAN_PREFIX: u32 = 4;

const KNOWN_SCAN_FLAGS: u32 = EGGDB_SCAN_REVERSE | EGGDB_SCAN_END_INCLUSIVE | EGGDB_SCAN_PREFIX;
const DEFAULT_BATCH_ENTRIES: usize = 256;
const BATCH_BYTES: usize = 1 << 20;

/// One entry of a scan's batch (`eggdb_entry` in `eggdb.h`): a key and its value, lent by the
/// scan until its next call.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EggdbEntry {
    /// The key's first byte.
    pub key: *const u8,
    /// The key's length in bytes.
    pub key_len: usize,
    /// The value's first byte.
    pub value: *const u8,
    /// The value's length in bytes.
    pub value_len: usize,
}

#[derive(Debug, Clone, Copy)]
struct Span {
    start: usize,
    key_len: usize,
    value_len: usize,
}

#[derive(Debug, Default)]
struct EntryBatch {
    bytes: Vec<u8>,
    spans: Vec<Span>,
    entries: Vec<EggdbEntry>,
}

impl EntryBatch {
    fn clear(&mut self) {
        self.bytes.clear();
        self.spans.clear();
        self.entries.clear();
    }

    fn fill_from(&mut self, mut scan: Scan<'_>, reverse: bool, limit: usize) -> eggdb::Result<bool> {
        while self.spans.len() < limit {
            let entry = if reverse {
                scan.next_back_entry()?
            } else {
                scan.next_entry()?
            };
            let Some((key, value)) = entry else {
                return Ok(false);
            };
            self.spans.push(Span {
                start: self.bytes.len(),
                key_len: key.len(),
                value_len: value.len(),
            });
            self.bytes.extend_from_slice(key);
            self.bytes.extend_from_slice(value);
            if self.bytes.len() >= BATCH_BYTES {
                return Ok(true);
            }
        }
        Ok(true)
    }

    fn last_key(&self) -> Option<Vec<u8>> {
        let last = self.spans.last()?;
        self.bytes
            .get(last.start..last.start + last.key_len)
            .map(<[u8]>::to_vec)
    }

    fn publish(&mut self) -> (*const EggdbEntry, usize) {
        let base = self.bytes.as_ptr();
        self.entries.extend(self.spans.iter().map(|span| EggdbEntry {
            key: base.wrapping_add(span.start),
            key_len: span.key_len,
            value: base.wrapping_add(span.start + span.key_len),
            value_len: span.value_len,
        }));
        if self.entries.is_empty() {
            return (std::ptr::null(), 0);
        }
        (self.entries.as_ptr(), self.entries.len())
    }
}

/// A scan of a range of keys (`eggdb_scan` in `eggdb.h`), from [`eggdb_scan_open`], read in
/// batches with [`eggdb_scan_next`] and freed with [`eggdb_scan_free`]. It remembers where it
/// stopped, not a transaction: each batch is read in the transaction passed with it.
#[derive(Debug)]
pub struct EggdbScan {
    collection: Option<String>,
    start: Bound<Vec<u8>>,
    end: Bound<Vec<u8>>,
    reverse: bool,
    finished: bool,
    batch: EntryBatch,
}

fn after_prefix(prefix: &[u8]) -> Bound<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < u8::MAX {
            end.push(last + 1);
            return Bound::Excluded(end);
        }
    }
    Bound::Unbounded
}

impl EggdbScan {
    fn new(
        collection: Option<&str>,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
        flags: u32,
    ) -> Result<Self, Failure> {
        let unknown = flags & !KNOWN_SCAN_FLAGS;
        if unknown != 0 {
            return Err(Failure::invalid_argument(format!(
                "flags has unknown bits {unknown:#x}; use EGGDB_SCAN_REVERSE, EGGDB_SCAN_END_INCLUSIVE and EGGDB_SCAN_PREFIX"
            )));
        }
        let (start, end) = if flags & EGGDB_SCAN_PREFIX != 0 {
            if end.is_some() || flags & EGGDB_SCAN_END_INCLUSIVE != 0 {
                return Err(Failure::invalid_argument(
                    "a prefix scan takes its range from start alone; pass end as NULL and leave out EGGDB_SCAN_END_INCLUSIVE",
                ));
            }
            let prefix = start.unwrap_or_default();
            (Bound::Included(prefix.to_vec()), after_prefix(prefix))
        } else {
            let start = start.map_or(Bound::Unbounded, |key| Bound::Included(key.to_vec()));
            let end = match end {
                None => Bound::Unbounded,
                Some(key) if flags & EGGDB_SCAN_END_INCLUSIVE != 0 => Bound::Included(key.to_vec()),
                Some(key) => Bound::Excluded(key.to_vec()),
            };
            (start, end)
        };
        Ok(Self {
            collection: collection.map(str::to_owned),
            start,
            end,
            reverse: flags & EGGDB_SCAN_REVERSE != 0,
            finished: false,
            batch: EntryBatch::default(),
        })
    }

    fn next_batch(
        &mut self,
        transaction: &mut Transaction,
        max_entries: usize,
    ) -> Result<(*const EggdbEntry, usize), Failure> {
        self.batch.clear();
        if self.finished {
            return Ok(self.batch.publish());
        }
        let limit = if max_entries == 0 {
            DEFAULT_BATCH_ENTRIES
        } else {
            max_entries
        };
        let range = (
            self.start.as_ref().map(Vec::as_slice),
            self.end.as_ref().map(Vec::as_slice),
        );
        let reverse = self.reverse;
        let batch = &mut self.batch;
        let more = match (transaction, self.collection.as_deref()) {
            (Transaction::Read(transaction), None) => {
                batch.fill_from(transaction.scan(range), reverse, limit)?
            }
            (Transaction::Read(transaction), Some(name)) => {
                let collection = transaction.collection(name)?;
                batch.fill_from(collection.scan(range), reverse, limit)?
            }
            (Transaction::Write(transaction), None) => {
                batch.fill_from(transaction.scan(range), reverse, limit)?
            }
            (Transaction::Write(transaction), Some(name)) => {
                let collection = transaction.collection(name)?;
                batch.fill_from(collection.scan(range), reverse, limit)?
            }
        };
        if let Some(last) = self.batch.last_key() {
            if self.reverse {
                self.end = Bound::Excluded(last);
            } else {
                self.start = Bound::Excluded(last);
            }
        }
        self.finished = !more;
        Ok(self.batch.publish())
    }
}

/// Opens a scan of the keys of `collection` (NULL: the default key space) from `start` (NULL:
/// the first key) up to `end` (NULL: the last key; left out unless `EGGDB_SCAN_END_INCLUSIVE`),
/// or of the keys beginning with `start` with `EGGDB_SCAN_PREFIX`, in key order or, with
/// `EGGDB_SCAN_REVERSE`, the other way; its handle goes in `*scan` (NULL on failure). The range
/// is copied, and a missing collection fails here with `EGGDB_ERR_COLLECTION_NOT_FOUND`.
///
/// # Safety
/// `txn` is a live transaction handle, `collection` NULL or a NUL-terminated string, `start`
/// and `end` NULL (with a length of 0) or pointers to their lengths in bytes, and `scan` points
/// to writable memory for one handle.
#[allow(clippy::too_many_arguments)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_scan_open(
    txn: *mut EggdbTxn,
    collection: *const c_char,
    start: *const u8,
    start_len: usize,
    end: *const u8,
    end_len: usize,
    flags: u32,
    scan: *mut *mut EggdbScan,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one handle at `scan`.
        unsafe { args::write(scan, std::ptr::null_mut(), "scan") }?;
        // SAFETY: eggdb.h asks for a live transaction handle used by this call alone.
        let txn = unsafe { args::handle_mut(txn, "txn") }?;
        // SAFETY: eggdb.h asks for NULL or a NUL-terminated collection name.
        let collection = unsafe { args::optional_text(collection, "collection") }?;
        // SAFETY: eggdb.h asks for NULL or `start_len` readable bytes at `start`.
        let start = unsafe { args::optional_bytes(start, start_len, "start") }?;
        // SAFETY: eggdb.h asks for NULL or `end_len` readable bytes at `end`.
        let end = unsafe { args::optional_bytes(end, end_len, "end") }?;
        let opened = EggdbScan::new(collection, start, end, flags)?;
        txn.check_collection(collection)?;
        // SAFETY: as for the first write to `scan`.
        unsafe { args::write(scan, args::give(opened), "scan") }
    })
}

/// Reads the scan's next batch in `txn`: up to `max_entries` entries (0: 256), fewer once about
/// a mebibyte of keys and values is gathered, never none while entries remain. `*entries` gets
/// the batch and `*count` its length; a count of 0 means the scan is done. The batch belongs to
/// the scan and stays valid until its next call or `eggdb_scan_free`. Entries written in `txn`
/// after the scan stopped are seen when they lie ahead of it.
///
/// # Safety
/// `txn` is a live transaction handle and `scan` a handle from `eggdb_scan_open`, each used by
/// this call alone, and `entries` and `count` point to writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_scan_next(
    txn: *mut EggdbTxn,
    scan: *mut EggdbScan,
    max_entries: usize,
    entries: *mut *const EggdbEntry,
    count: *mut usize,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one pointer at `entries`.
        unsafe { args::write(entries, std::ptr::null(), "entries") }?;
        // SAFETY: eggdb.h asks for writable memory for one size_t at `count`.
        unsafe { args::write(count, 0, "count") }?;
        // SAFETY: eggdb.h asks for a live transaction handle used by this call alone.
        let txn = unsafe { args::handle_mut(txn, "txn") }?;
        // SAFETY: eggdb.h asks for a live scan handle used by this call alone.
        let scan = unsafe { args::handle_mut(scan, "scan") }?;
        let (batch, len) = scan.next_batch(&mut txn.transaction, max_entries)?;
        // SAFETY: as for the first write to `entries`.
        unsafe { args::write(entries, batch, "entries") }?;
        // SAFETY: as for the first write to `count`.
        unsafe { args::write(count, len, "count") }
    })
}

/// Frees a scan and its batch. NULL does nothing.
///
/// # Safety
/// `scan` is NULL or a handle from `eggdb_scan_open` not freed yet; it and its entries must not
/// be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_scan_free(scan: *mut EggdbScan) {
    // SAFETY: the caller gives the handle up with this call (eggdb.h).
    drop(unsafe { args::take(scan) });
}

#[cfg(test)]
mod tests {
    use super::{EGGDB_SCAN_END_INCLUSIVE, EGGDB_SCAN_PREFIX, EggdbScan, after_prefix};
    use std::ops::Bound;

    #[test]
    fn a_prefix_ends_before_the_next_prefix() {
        assert_eq!(after_prefix(b"ab"), Bound::Excluded(b"ac".to_vec()));
        assert_eq!(after_prefix(b"a\xff"), Bound::Excluded(b"b".to_vec()));
        assert_eq!(after_prefix(b"\xff\xff"), Bound::Unbounded);
        assert_eq!(after_prefix(b""), Bound::Unbounded);
    }

    #[test]
    fn a_prefix_scan_refuses_an_end() {
        let a: &[u8] = b"a";
        let b: &[u8] = b"b";
        assert!(EggdbScan::new(None, Some(a), Some(b), EGGDB_SCAN_PREFIX).is_err());
        let inclusive = EGGDB_SCAN_PREFIX | EGGDB_SCAN_END_INCLUSIVE;
        assert!(EggdbScan::new(None, Some(a), None, inclusive).is_err());
        assert!(EggdbScan::new(None, Some(a), None, 0x40).is_err());
    }

    #[test]
    fn bounds_follow_the_flags() {
        let a: &[u8] = b"a";
        let c: &[u8] = b"c";
        let scan = EggdbScan::new(None, Some(a), Some(c), EGGDB_SCAN_END_INCLUSIVE)
            .unwrap_or_else(|failure| panic!("{failure:?}"));
        assert_eq!(scan.start, Bound::Included(b"a".to_vec()));
        assert_eq!(scan.end, Bound::Included(b"c".to_vec()));
        let open = EggdbScan::new(None, None, None, 0).unwrap_or_else(|failure| panic!("{failure:?}"));
        assert_eq!(open.start, Bound::Unbounded);
        assert_eq!(open.end, Bound::Unbounded);
    }
}
