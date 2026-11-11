use crate::item::{Item, expiry_of};
use crate::lru::{Change, Lru, item_bytes};
use eggdb_keyspace::{KeysRead, KeysWrite, Keyspace};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_KEY_BYTES: usize = 250;
pub(crate) const MAX_ITEM_BYTES: usize = 1 << 20;

/// What the server shares between connections.
pub struct Shared<'k> {
    /// Where items live (database 0 of the keyspace).
    pub keyspace: &'k (dyn Keyspace + Sync),
    /// The next CAS value.
    pub next_cas: AtomicU64,
    /// What `stats` reports.
    pub counters: Counters,
    /// The items' memory limit and order of last use (`-m`).
    pub lru: Lru,
}

/// The counts `stats` reports, since the server started or `stats reset`.
#[derive(Debug)]
pub struct Counters {
    pub(crate) started: std::time::Instant,
    pub(crate) curr_connections: AtomicU64,
    pub(crate) total_connections: AtomicU64,
    pub(crate) cmd_get: AtomicU64,
    pub(crate) cmd_set: AtomicU64,
    pub(crate) cmd_touch: AtomicU64,
    pub(crate) cmd_flush: AtomicU64,
    pub(crate) get_hits: AtomicU64,
    pub(crate) get_misses: AtomicU64,
    pub(crate) delete_hits: AtomicU64,
    pub(crate) delete_misses: AtomicU64,
    pub(crate) incr_hits: AtomicU64,
    pub(crate) incr_misses: AtomicU64,
    pub(crate) decr_hits: AtomicU64,
    pub(crate) decr_misses: AtomicU64,
    pub(crate) cas_hits: AtomicU64,
    pub(crate) cas_misses: AtomicU64,
    pub(crate) cas_badval: AtomicU64,
    pub(crate) touch_hits: AtomicU64,
    pub(crate) touch_misses: AtomicU64,
}

impl Default for Counters {
    fn default() -> Self {
        Self {
            started: std::time::Instant::now(),
            curr_connections: AtomicU64::new(0),
            total_connections: AtomicU64::new(0),
            cmd_get: AtomicU64::new(0),
            cmd_set: AtomicU64::new(0),
            cmd_touch: AtomicU64::new(0),
            cmd_flush: AtomicU64::new(0),
            get_hits: AtomicU64::new(0),
            get_misses: AtomicU64::new(0),
            delete_hits: AtomicU64::new(0),
            delete_misses: AtomicU64::new(0),
            incr_hits: AtomicU64::new(0),
            incr_misses: AtomicU64::new(0),
            decr_hits: AtomicU64::new(0),
            decr_misses: AtomicU64::new(0),
            cas_hits: AtomicU64::new(0),
            cas_misses: AtomicU64::new(0),
            cas_badval: AtomicU64::new(0),
            touch_hits: AtomicU64::new(0),
            touch_misses: AtomicU64::new(0),
        }
    }
}

impl Counters {
    fn resettable(&self) -> [(&'static str, &AtomicU64); 18] {
        [
            ("total_connections", &self.total_connections),
            ("cmd_get", &self.cmd_get),
            ("cmd_set", &self.cmd_set),
            ("cmd_flush", &self.cmd_flush),
            ("cmd_touch", &self.cmd_touch),
            ("get_hits", &self.get_hits),
            ("get_misses", &self.get_misses),
            ("delete_misses", &self.delete_misses),
            ("delete_hits", &self.delete_hits),
            ("incr_misses", &self.incr_misses),
            ("incr_hits", &self.incr_hits),
            ("decr_misses", &self.decr_misses),
            ("decr_hits", &self.decr_hits),
            ("cas_misses", &self.cas_misses),
            ("cas_hits", &self.cas_hits),
            ("cas_badval", &self.cas_badval),
            ("touch_hits", &self.touch_hits),
            ("touch_misses", &self.touch_misses),
        ]
    }

    /// The `stats` reply: `STAT <name> <value>` lines and `END`.
    pub(crate) fn report(&self, version: &str) -> Vec<u8> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let mut out = format!(
            "STAT pid {}\r\nSTAT uptime {}\r\nSTAT time {now}\r\nSTAT version {version}\r\nSTAT pointer_size 64\r\nSTAT curr_connections {}\r\n",
            std::process::id(),
            self.started.elapsed().as_secs(),
            self.curr_connections.load(Ordering::Relaxed),
        );
        for (name, counter) in self.resettable() {
            out.push_str(&format!("STAT {name} {}\r\n", counter.load(Ordering::Relaxed)));
        }
        out.push_str("END\r\n");
        out.into_bytes()
    }

    /// `stats reset`: every count back to zero.
    pub(crate) fn reset(&self) {
        for (_, counter) in self.resettable() {
            counter.store(0, Ordering::Relaxed);
        }
    }
}

impl std::fmt::Debug for Shared<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

impl Shared<'_> {
    fn cas(&self) -> u64 {
        self.next_cas.fetch_add(1, Ordering::Relaxed)
    }

    /// The `stats` reply: the counters, then the items' bytes, count, limit and evictions.
    pub(crate) fn stats(&self, version: &str) -> Vec<u8> {
        let mut out = self.counters.report(version);
        out.truncate(out.len().saturating_sub(b"END\r\n".len()));
        if self.lru.is_on() {
            load_lru(self);
        }
        let (bytes, items) = self.lru.usage();
        out.extend(
            format!(
                "STAT bytes {bytes}\r\nSTAT curr_items {items}\r\nSTAT limit_maxbytes {}\r\nSTAT evictions {}\r\nEND\r\n",
                self.lru.limit_bytes(),
                self.lru.evictions()
            )
            .into_bytes(),
        );
        out
    }
}

struct RecordingRead<'k> {
    inner: &'k dyn KeysRead,
    hits: std::cell::RefCell<Vec<Vec<u8>>>,
}

impl KeysRead for RecordingRead<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let found = self.inner.get(key)?;
        if found.is_some()
            && let Ok(mut hits) = self.hits.try_borrow_mut()
        {
            hits.push(key.to_vec());
        }
        Ok(found)
    }

    fn keys(&self) -> Result<Vec<Vec<u8>>, String> {
        self.inner.keys()
    }

    fn visit_keys(&self, visit: &mut dyn FnMut(Vec<u8>) -> Result<bool, String>) -> Result<(), String> {
        self.inner.visit_keys(visit)
    }
}

struct RecordingWrite<'k> {
    inner: &'k mut dyn KeysWrite,
    hits: std::cell::RefCell<Vec<Vec<u8>>>,
    changes: Vec<Change>,
}

impl KeysRead for RecordingWrite<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let found = self.inner.get(key)?;
        if found.is_some()
            && let Ok(mut hits) = self.hits.try_borrow_mut()
        {
            hits.push(key.to_vec());
        }
        Ok(found)
    }

    fn keys(&self) -> Result<Vec<Vec<u8>>, String> {
        self.inner.keys()
    }

    fn visit_keys(&self, visit: &mut dyn FnMut(Vec<u8>) -> Result<bool, String>) -> Result<(), String> {
        self.inner.visit_keys(visit)
    }
}

impl KeysWrite for RecordingWrite<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), String> {
        self.inner.put(key, value)?;
        self.changes.push(Change::Put(key.to_vec(), item_bytes(key, value)));
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> Result<bool, String> {
        let deleted = self.inner.delete(key)?;
        if deleted {
            self.changes.push(Change::Deleted(key.to_vec()));
        }
        Ok(deleted)
    }

    fn clear(&mut self) -> Result<(), String> {
        self.inner.clear()?;
        self.changes.push(Change::Cleared);
        Ok(())
    }
}

fn load_lru(shared: &Shared<'_>) {
    if shared.lru.is_loaded() {
        return;
    }
    let mut items = Vec::new();
    let scanned = shared.keyspace.read(0, &mut |keys| {
        items.clear();
        keys.visit_prefix(b"", &mut |key, value| {
            items.push((key.to_vec(), item_bytes(key, value)));
            Ok(true)
        })
    });
    if scanned.is_ok() {
        shared.lru.load(items);
    }
}

fn evict(shared: &Shared<'_>, keep: &[Vec<u8>]) {
    let victims = shared.lru.victims(keep);
    if victims.is_empty() {
        return;
    }
    let mut evicted = Vec::new();
    let outcome = shared.keyspace.write(0, &mut |keys| {
        evicted.clear();
        for key in &victims {
            if keys.delete(key)? {
                evicted.push(Change::Deleted(key.clone()));
            }
        }
        Ok(())
    });
    if outcome.is_ok() {
        shared.lru.note_evicted(evicted.len());
        shared.lru.note_written(&evicted);
    }
}

fn valid_key(key: &[u8]) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY_BYTES
        && key.iter().all(|byte| *byte > b' ' && *byte != 0x7f)
}

fn load(keys: &dyn KeysRead, key: &[u8], now: u64) -> Result<Option<Item>, String> {
    Ok(keys
        .get(key)?
        .and_then(|bytes| Item::decode(&bytes))
        .filter(|item| !item.expired(now)))
}

fn store(keys: &mut dyn KeysWrite, key: &[u8], item: &Item) -> Result<(), String> {
    keys.put(key, &item.encode())
}

fn read<T>(
    shared: &Shared<'_>,
    body: impl FnOnce(&dyn KeysRead) -> Result<T, String>,
) -> Result<T, String> {
    let mut body = Some(body);
    let mut out = None;
    let mut hits = Vec::new();
    shared.keyspace.read(0, &mut |keys| {
        if let Some(body) = body.take() {
            let recording = RecordingRead {
                inner: keys,
                hits: std::cell::RefCell::default(),
            };
            out = Some(body(&recording)?);
            hits = recording.hits.into_inner();
        }
        Ok(())
    })?;
    if shared.lru.is_on() {
        load_lru(shared);
        shared.lru.note_read(&hits);
    }
    out.ok_or_else(|| "the read did not run".to_owned())
}

fn write<T>(
    shared: &Shared<'_>,
    body: impl FnOnce(&mut dyn KeysWrite) -> Result<T, String>,
) -> Result<T, String> {
    if shared.lru.is_on() {
        load_lru(shared);
    }
    let mut body = Some(body);
    let mut out = None;
    let mut hits = Vec::new();
    let mut changes = Vec::new();
    shared.keyspace.write(0, &mut |keys| {
        if let Some(body) = body.take() {
            let mut recording = RecordingWrite {
                inner: keys,
                hits: std::cell::RefCell::default(),
                changes: Vec::new(),
            };
            out = Some(body(&mut recording)?);
            hits = recording.hits.into_inner();
            changes = recording.changes;
        }
        Ok(())
    })?;
    if shared.lru.is_on() {
        shared.lru.note_read(&hits);
        shared.lru.note_written(&changes);
        let written: Vec<Vec<u8>> = changes
            .iter()
            .filter_map(|change| match change {
                Change::Put(key, _) => Some(key.clone()),
                _ => None,
            })
            .collect();
        if !written.is_empty() {
            evict(shared, &written);
        }
    }
    out.ok_or_else(|| "the write did not run".to_owned())
}

/// A storage command's mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `set`.
    Set,
    /// `add`: only when absent.
    Add,
    /// `replace`: only when present.
    Replace,
    /// `append`.
    Append,
    /// `prepend`.
    Prepend,
    /// `cas` with the value the client read.
    Cas(u64),
}

/// The outcome of a storage command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    /// `STORED`.
    Stored,
    /// `NOT_STORED`.
    NotStored,
    /// `EXISTS`: the CAS value differs.
    Exists,
    /// `NOT_FOUND`.
    NotFound,
}

/// Runs `set`, `add`, `replace`, `append`, `prepend` or `cas`.
pub fn storage(
    shared: &Shared<'_>,
    mode: Mode,
    key: &[u8],
    flags: u32,
    exptime: i64,
    data: Vec<u8>,
    now: u64,
) -> Result<Stored, String> {
    let cas = shared.cas();
    write(shared, |keys| {
        let existing = load(keys, key, now)?;
        let item = match (mode, existing) {
            (Mode::Add, Some(_)) | (Mode::Replace | Mode::Append | Mode::Prepend, None) => {
                return Ok(Stored::NotStored);
            }
            (Mode::Cas(_), None) => return Ok(Stored::NotFound),
            (Mode::Cas(expected), Some(existing)) if existing.cas != expected => {
                return Ok(Stored::Exists);
            }
            (Mode::Append, Some(mut existing)) => {
                existing.data.extend_from_slice(&data);
                existing.cas = cas;
                existing
            }
            (Mode::Prepend, Some(mut existing)) => {
                let mut joined = data;
                joined.extend_from_slice(&existing.data);
                existing.data = joined;
                existing.cas = cas;
                existing
            }
            _ => Item {
                data,
                flags,
                cas,
                expires_at_ms: expiry_of(exptime, now),
            },
        };
        if item.data.len() > MAX_ITEM_BYTES {
            return Err("SERVER_ERROR object too large for cache".to_owned());
        }
        if item.expired(now) {
            keys.delete(key)?;
        } else {
            store(keys, key, &item)?;
        }
        Ok(Stored::Stored)
    })
}

/// The items of `keys` that exist, with their keys.
pub fn get(
    shared: &Shared<'_>,
    keys_wanted: &[Vec<u8>],
    touch: Option<i64>,
    now: u64,
) -> Result<Vec<(Vec<u8>, Item)>, String> {
    let mut found = Vec::new();
    get_each(shared, keys_wanted, touch, now, &mut |key, item| {
        found.push((key.to_vec(), item.clone()));
        true
    })?;
    Ok(found)
}

/// The reply to a read that `accept` refused to take one more item into.
pub const OUT_OF_MEMORY_READING: &str = "SERVER_ERROR out of memory";

/// Hands each item of `keys` that exists to `accept`, one at a time, so the caller can stop
/// before taking more than it may hold: when `accept` returns false the read stops with
/// [`OUT_OF_MEMORY_READING`] and a touch changes nothing. With `touch`, each item's new expiry
/// is set before `accept` sees it.
pub fn get_each(
    shared: &Shared<'_>,
    keys_wanted: &[Vec<u8>],
    touch: Option<i64>,
    now: u64,
    accept: &mut dyn FnMut(&[u8], &Item) -> bool,
) -> Result<(), String> {
    match touch {
        None => read(shared, |keys| {
            for key in keys_wanted {
                if let Some(item) = load(keys, key, now)?
                    && !accept(key, &item)
                {
                    return Err(OUT_OF_MEMORY_READING.to_owned());
                }
            }
            Ok(())
        }),
        Some(exptime) => write(shared, |keys| {
            for key in keys_wanted {
                let Some(mut item) = load(keys, key, now)? else {
                    continue;
                };
                item.expires_at_ms = expiry_of(exptime, now);
                if !accept(key, &item) {
                    return Err(OUT_OF_MEMORY_READING.to_owned());
                }
                store(keys, key, &item)?;
            }
            Ok(())
        }),
    }
}

/// Deletes `key`; returns whether it was there.
pub fn delete(shared: &Shared<'_>, key: &[u8], now: u64) -> Result<bool, String> {
    write(shared, |keys| {
        let found = load(keys, key, now)?.is_some();
        if found {
            keys.delete(key)?;
        }
        Ok(found)
    })
}

/// Deletes `key` when its CAS value is `compare` (or always without one): `None` when it does
/// not exist, `Some(false)` when the CAS value differs.
pub fn delete_if(
    shared: &Shared<'_>,
    key: &[u8],
    compare: Option<u64>,
    now: u64,
) -> Result<Option<bool>, String> {
    write(shared, |keys| {
        let Some(item) = load(keys, key, now)? else {
            return Ok(None);
        };
        if compare.is_some_and(|expected| expected != item.cas) {
            return Ok(Some(false));
        }
        keys.delete(key)?;
        Ok(Some(true))
    })
}

/// A meta arithmetic (`ma`) request.
#[derive(Debug, Clone, Copy)]
pub struct Arithmetic {
    /// How much to add or subtract.
    pub delta: u64,
    /// Whether to add.
    pub increment: bool,
    /// Create a missing item with this `exptime` and initial value.
    pub vivify: Option<(i64, u64)>,
    /// Change only an item with this CAS value.
    pub compare: Option<u64>,
    /// A new `exptime` for the item.
    pub touch: Option<i64>,
}

/// Runs a meta arithmetic request: `None` when the item does not exist (and is not created),
/// `Some(Err(()))` when its CAS value differs, else the item after the change.
pub fn arithmetic_with(
    shared: &Shared<'_>,
    key: &[u8],
    request: Arithmetic,
    now: u64,
) -> Result<Option<Result<Item, ()>>, String> {
    let cas = shared.cas();
    write(shared, |keys| {
        let item = match (load(keys, key, now)?, request.vivify) {
            (Some(mut item), _) => {
                if request.compare.is_some_and(|expected| expected != item.cas) {
                    return Ok(Some(Err(())));
                }
                let current: u64 = std::str::from_utf8(&item.data)
                    .ok()
                    .and_then(|text| text.trim().parse().ok())
                    .ok_or_else(|| {
                        "CLIENT_ERROR cannot increment or decrement non-numeric value".to_owned()
                    })?;
                let next = if request.increment {
                    current.wrapping_add(request.delta)
                } else {
                    current.saturating_sub(request.delta)
                };
                item.data = next.to_string().into_bytes();
                item.cas = cas;
                if let Some(exptime) = request.touch {
                    item.expires_at_ms = expiry_of(exptime, now);
                }
                item
            }
            (None, Some((exptime, initial))) => Item {
                data: initial.to_string().into_bytes(),
                flags: 0,
                cas,
                expires_at_ms: expiry_of(exptime, now),
            },
            (None, None) => return Ok(None),
        };
        store(keys, key, &item)?;
        Ok(Some(Ok(item)))
    })
}

/// Adds or subtracts `delta` from a decimal item; `None` when it does not exist.
pub fn arithmetic(
    shared: &Shared<'_>,
    key: &[u8],
    delta: u64,
    increment: bool,
    now: u64,
) -> Result<Option<u64>, String> {
    let cas = shared.cas();
    write(shared, |keys| {
        let Some(mut item) = load(keys, key, now)? else {
            return Ok(None);
        };
        let current: u64 = std::str::from_utf8(&item.data)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .ok_or_else(|| {
                "CLIENT_ERROR cannot increment or decrement non-numeric value".to_owned()
            })?;
        let next = if increment {
            current.wrapping_add(delta)
        } else {
            current.saturating_sub(delta)
        };
        item.data = next.to_string().into_bytes();
        item.cas = cas;
        store(keys, key, &item)?;
        Ok(Some(next))
    })
}

/// Sets a new expiry on `key`; returns whether it exists.
pub fn touch(shared: &Shared<'_>, key: &[u8], exptime: i64, now: u64) -> Result<bool, String> {
    write(shared, |keys| {
        let Some(mut item) = load(keys, key, now)? else {
            return Ok(false);
        };
        item.expires_at_ms = expiry_of(exptime, now);
        store(keys, key, &item)?;
        Ok(true)
    })
}

/// Removes every item.
pub fn flush_all(shared: &Shared<'_>) -> Result<(), String> {
    write(shared, |keys| keys.clear())
}

pub(crate) fn check_key(key: &[u8]) -> Result<(), String> {
    if valid_key(key) {
        Ok(())
    } else {
        Err("CLIENT_ERROR bad command line format".to_owned())
    }
}
