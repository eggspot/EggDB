//! Collections kept in memory only (D1367): named key spaces beside the file's collections that
//! never touch the file, with a time to live per key and eviction of the least recently used
//! keys under a byte limit, for caches that should not cost a write or survive a restart.

use crate::error::{Error, ErrorKind, Result};
use eggdb_platform::time::Instant;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

const ENTRY_OVERHEAD_BYTES: u64 = 64;

/// A collection kept in memory only. Its keys go when the database closes; a put past its byte
/// limit evicts the least recently used keys first, and an expired key reads as absent.
#[derive(Debug)]
pub struct MemoryCollection {
    limit_bytes: Option<u64>,
    inner: Mutex<Entries>,
}

#[derive(Debug, Default)]
struct Entries {
    by_key: HashMap<Vec<u8>, Entry>,
    by_use: BTreeMap<u64, Vec<u8>>,
    bytes: u64,
    tick: u64,
    evicted: u64,
}

#[derive(Debug)]
struct Entry {
    value: Vec<u8>,
    expires_at: Option<Instant>,
    used: u64,
}

fn footprint(key: &[u8], value: &[u8]) -> u64 {
    (key.len() + value.len()) as u64 + ENTRY_OVERHEAD_BYTES
}

impl Entries {
    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    fn remove(&mut self, key: &[u8]) -> Option<Entry> {
        let entry = self.by_key.remove(key)?;
        self.by_use.remove(&entry.used);
        self.bytes = self.bytes.saturating_sub(footprint(key, &entry.value));
        Some(entry)
    }

    fn live(&mut self, key: &[u8], now: Instant) -> bool {
        let expired = self
            .by_key
            .get(key)
            .is_some_and(|entry| entry.expires_at.is_some_and(|at| at <= now));
        if expired {
            self.remove(key);
        }
        self.by_key.contains_key(key)
    }

    fn touch(&mut self, key: &[u8]) {
        let tick = self.next_tick();
        if let Some(entry) = self.by_key.get_mut(key) {
            self.by_use.remove(&entry.used);
            entry.used = tick;
            self.by_use.insert(tick, key.to_vec());
        }
    }

    fn evict_to(&mut self, limit: u64, keep: &[u8]) {
        while self.bytes > limit {
            let Some(oldest) = self
                .by_use
                .values()
                .find(|candidate| candidate.as_slice() != keep)
                .cloned()
            else {
                break;
            };
            self.remove(&oldest);
            self.evicted += 1;
        }
    }
}

impl MemoryCollection {
    fn new(limit_bytes: Option<u64>) -> Self {
        Self {
            limit_bytes,
            inner: Mutex::new(Entries::default()),
        }
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, Entries> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The value under `key`, unless it is absent or expired; reading it makes it the most
    /// recently used.
    pub fn get(&self, key: impl AsRef<[u8]>) -> Option<Vec<u8>> {
        let key = key.as_ref();
        let mut entries = self.entries();
        if !entries.live(key, Instant::now()) {
            return None;
        }
        entries.touch(key);
        entries.by_key.get(key).map(|entry| entry.value.clone())
    }

    /// Stores `value` under `key`, living for `ttl` when there is one, replacing any value and
    /// time to live there; then evicts the least recently used keys while the collection holds
    /// more than its limit. A key and value larger than the limit alone fail with
    /// [`ErrorKind::TooLarge`], storing nothing.
    pub fn put(&self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>, ttl: Option<Duration>) -> Result<()> {
        let (key, value) = (key.as_ref(), value.as_ref());
        if let Some(limit) = self.limit_bytes
            && footprint(key, value) > limit
        {
            return Err(Error::new(
                ErrorKind::TooLarge,
                format!("{} bytes do not fit in this memory collection's limit of {limit} bytes", footprint(key, value)),
            ));
        }
        let now = Instant::now();
        let mut entries = self.entries();
        entries.remove(key);
        let tick = entries.next_tick();
        entries.bytes += footprint(key, value);
        entries.by_use.insert(tick, key.to_vec());
        entries.by_key.insert(
            key.to_vec(),
            Entry {
                value: value.to_vec(),
                expires_at: ttl.and_then(|ttl| now.checked_add(ttl)),
                used: tick,
            },
        );
        if let Some(limit) = self.limit_bytes {
            entries.evict_to(limit, key);
        }
        Ok(())
    }

    /// Removes `key`; whether it was there and not expired.
    pub fn delete(&self, key: impl AsRef<[u8]>) -> bool {
        let key = key.as_ref();
        let mut entries = self.entries();
        let live = entries.live(key, Instant::now());
        entries.remove(key);
        live
    }

    /// How long `key` still lives: `None` when it is absent or expired, `Some(None)` when it
    /// has no time to live.
    pub fn time_to_live(&self, key: impl AsRef<[u8]>) -> Option<Option<Duration>> {
        let key = key.as_ref();
        let now = Instant::now();
        let mut entries = self.entries();
        if !entries.live(key, now) {
            return None;
        }
        entries
            .by_key
            .get(key)
            .map(|entry| entry.expires_at.map(|at| at.saturating_duration_since(now)))
    }

    /// How many keys it holds, expired ones that were not read yet included.
    pub fn len(&self) -> usize {
        self.entries().by_key.len()
    }

    /// Whether it holds no key.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The bytes its keys and values take, each with a fixed overhead.
    pub fn bytes(&self) -> u64 {
        self.entries().bytes
    }

    /// How many keys its limit has evicted so far.
    pub fn evicted(&self) -> u64 {
        self.entries().evicted
    }

    /// Removes every expired key now, rather than when it is next read; how many went.
    pub fn remove_expired(&self) -> usize {
        let now = Instant::now();
        let mut entries = self.entries();
        let expired: Vec<Vec<u8>> = entries
            .by_key
            .iter()
            .filter(|(_, entry)| entry.expires_at.is_some_and(|at| at <= now))
            .map(|(key, _)| key.clone())
            .collect();
        for key in &expired {
            entries.remove(key);
        }
        expired.len()
    }
}

/// The memory collections of one open database.
#[derive(Debug, Default)]
pub(crate) struct MemoryCollections {
    by_name: Mutex<BTreeMap<String, Arc<MemoryCollection>>>,
}

impl MemoryCollections {
    pub(crate) fn create(&self, name: &str, limit_bytes: Option<u64>) -> bool {
        let mut by_name = self.by_name.lock().unwrap_or_else(PoisonError::into_inner);
        if by_name.contains_key(name) {
            return false;
        }
        by_name.insert(name.to_owned(), Arc::new(MemoryCollection::new(limit_bytes)));
        true
    }

    pub(crate) fn get(&self, name: &str) -> Option<Arc<MemoryCollection>> {
        self.by_name.lock().unwrap_or_else(PoisonError::into_inner).get(name).cloned()
    }

    pub(crate) fn drop_named(&self, name: &str) -> bool {
        self.by_name.lock().unwrap_or_else(PoisonError::into_inner).remove(name).is_some()
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.by_name.lock().unwrap_or_else(PoisonError::into_inner).keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::MemoryCollection;
    use std::time::Duration;

    #[test]
    fn values_expire_and_the_least_recently_used_are_evicted() {
        let cache = MemoryCollection::new(Some(3 * (64 + 2)));
        cache.put("a", "1", None).unwrap();
        cache.put("b", "2", None).unwrap();
        cache.put("c", "3", Some(Duration::ZERO)).unwrap();
        assert_eq!(cache.get("c"), None);
        cache.put("c", "3", None).unwrap();
        assert_eq!(cache.get("a"), Some(b"1".to_vec()));
        cache.put("d", "4", None).unwrap();
        assert_eq!(cache.get("b"), None, "b was the least recently used");
        assert_eq!(cache.get("a"), Some(b"1".to_vec()));
        assert_eq!(cache.evicted(), 1);
        assert_eq!(cache.len(), 3);
        assert!(cache.put("big", vec![0u8; 1000], None).is_err());
        assert_eq!(cache.time_to_live("a"), Some(None));
        cache.put("t", "5", Some(Duration::from_secs(60))).unwrap();
        assert!(cache.time_to_live("t").flatten().is_some_and(|left| left <= Duration::from_secs(60)));
        assert!(cache.delete("t"));
        assert!(!cache.delete("t"));
    }
}
