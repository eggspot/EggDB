use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

const ITEM_OVERHEAD_BYTES: u64 = 48;

#[derive(Debug, Default)]
struct State {
    loaded: bool,
    used_bytes: u64,
    next_tick: u64,
    by_tick: BTreeMap<u64, Vec<u8>>,
    by_key: HashMap<Vec<u8>, (u64, u64)>,
}

impl State {
    fn forget(&mut self, key: &[u8]) {
        if let Some((tick, bytes)) = self.by_key.remove(key) {
            self.by_tick.remove(&tick);
            self.used_bytes = self.used_bytes.saturating_sub(bytes);
        }
    }

    fn bump(&mut self, key: &[u8]) {
        if let Some((tick, _)) = self.by_key.get_mut(key) {
            self.by_tick.remove(tick);
            self.next_tick += 1;
            *tick = self.next_tick;
            self.by_tick.insert(self.next_tick, key.to_vec());
        }
    }

    fn place(&mut self, key: &[u8], bytes: u64) {
        self.forget(key);
        self.next_tick += 1;
        self.by_tick.insert(self.next_tick, key.to_vec());
        self.by_key.insert(key.to_vec(), (self.next_tick, bytes));
        self.used_bytes = self.used_bytes.saturating_add(bytes);
    }
}

/// The bytes one stored item counts for against the limit: its key, its stored bytes and
/// memcached's per-item header.
pub fn item_bytes(key: &[u8], stored: &[u8]) -> u64 {
    (key.len() + stored.len()) as u64 + ITEM_OVERHEAD_BYTES
}

/// memcached's `-m`: the most bytes the items may take, and their order of last use, so a
/// write past the limit evicts the least recently used items. A limit of 0 turns it off.
#[derive(Debug, Default)]
pub struct Lru {
    limit_bytes: u64,
    state: Mutex<State>,
    evictions: AtomicU64,
}

impl Lru {
    /// Items limited to `limit_bytes` in all; 0 for no limit.
    pub fn with_limit(limit_bytes: u64) -> Self {
        Self {
            limit_bytes,
            ..Self::default()
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The limit, 0 for none.
    pub fn limit_bytes(&self) -> u64 {
        self.limit_bytes
    }

    /// Whether a limit is set.
    pub fn is_on(&self) -> bool {
        self.limit_bytes > 0
    }

    /// How many items were evicted since the server started.
    pub fn evictions(&self) -> u64 {
        self.evictions.load(Ordering::Relaxed)
    }

    /// The items' bytes and count as the limit counts them.
    pub fn usage(&self) -> (u64, usize) {
        let state = self.locked();
        (state.used_bytes, state.by_key.len())
    }

    pub(crate) fn is_loaded(&self) -> bool {
        self.locked().loaded
    }

    pub(crate) fn load(&self, items: Vec<(Vec<u8>, u64)>) {
        let mut state = self.locked();
        if state.loaded {
            return;
        }
        for (key, bytes) in items {
            if !state.by_key.contains_key(&key) {
                state.place(&key, bytes);
            }
        }
        state.loaded = true;
    }

    pub(crate) fn note_read(&self, hits: &[Vec<u8>]) {
        if !self.is_on() || hits.is_empty() {
            return;
        }
        let mut state = self.locked();
        for key in hits {
            state.bump(key);
        }
    }

    pub(crate) fn note_written(&self, changes: &[Change]) {
        if !self.is_on() || changes.is_empty() {
            return;
        }
        let mut state = self.locked();
        for change in changes {
            match change {
                Change::Put(key, bytes) => state.place(key, *bytes),
                Change::Deleted(key) => state.forget(key),
                Change::Cleared => {
                    state.by_key.clear();
                    state.by_tick.clear();
                    state.used_bytes = 0;
                }
            }
        }
    }

    pub(crate) fn note_evicted(&self, count: usize) {
        self.evictions.fetch_add(count as u64, Ordering::Relaxed);
    }

    /// The least recently used keys to evict so the items fit the limit again, oldest first,
    /// never one of `keep`.
    pub(crate) fn victims(&self, keep: &[Vec<u8>]) -> Vec<Vec<u8>> {
        if !self.is_on() {
            return Vec::new();
        }
        let state = self.locked();
        let mut over = state.used_bytes.saturating_sub(self.limit_bytes);
        let mut chosen = Vec::new();
        for key in state.by_tick.values() {
            if over == 0 {
                break;
            }
            if keep.contains(key) {
                continue;
            }
            let bytes = state.by_key.get(key).map_or(0, |(_, bytes)| *bytes);
            over = over.saturating_sub(bytes);
            chosen.push(key.clone());
        }
        chosen
    }
}

/// One change a write made to the items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Change {
    Put(Vec<u8>, u64),
    Deleted(Vec<u8>),
    Cleared,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_least_recently_used_items_are_chosen_past_the_limit() {
        let lru = Lru::with_limit(3 * (ITEM_OVERHEAD_BYTES + 2));
        lru.load(Vec::new());
        for key in [b"a", b"b", b"c"] {
            lru.note_written(&[Change::Put(key.to_vec(), ITEM_OVERHEAD_BYTES + 2)]);
        }
        assert!(lru.victims(&[]).is_empty());
        lru.note_read(&[b"a".to_vec()]);
        lru.note_written(&[Change::Put(b"d".to_vec(), ITEM_OVERHEAD_BYTES + 2)]);
        assert_eq!(lru.victims(&[b"d".to_vec()]), vec![b"b".to_vec()]);
        lru.note_written(&[Change::Deleted(b"b".to_vec())]);
        assert!(lru.victims(&[]).is_empty());
        lru.note_written(&[Change::Cleared]);
        assert_eq!(lru.usage(), (0, 0));
        assert!(Lru::default().victims(&[]).is_empty());
    }
}
