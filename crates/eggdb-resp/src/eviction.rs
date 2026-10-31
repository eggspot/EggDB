use crate::keyspace::{KeysRead, KeysWrite, Keyspace};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

const DATABASES: u32 = 16;
const OUT_OF_MEMORY: &str = "OOM command not allowed when used memory > 'maxmemory'.";

/// Which keys go when the data grows past the limit (the `maxmemory-policy` names).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictionPolicy {
    /// Refuse writes instead.
    NoEviction,
    /// The least recently used key.
    AllKeysLru,
    /// The least frequently used key.
    AllKeysLfu,
    /// Any key.
    AllKeysRandom,
    /// The least recently used key that has an expiry.
    VolatileLru,
    /// The least frequently used key that has an expiry.
    VolatileLfu,
    /// Any key that has an expiry.
    VolatileRandom,
    /// The key with an expiry that expires first.
    VolatileTtl,
}

impl EvictionPolicy {
    /// The policy named `name`, as `maxmemory-policy` spells it.
    pub fn named(name: &str) -> Option<Self> {
        Some(match name {
            "noeviction" => Self::NoEviction,
            "allkeys-lru" => Self::AllKeysLru,
            "allkeys-lfu" => Self::AllKeysLfu,
            "allkeys-random" => Self::AllKeysRandom,
            "volatile-lru" => Self::VolatileLru,
            "volatile-lfu" => Self::VolatileLfu,
            "volatile-random" => Self::VolatileRandom,
            "volatile-ttl" => Self::VolatileTtl,
            _ => return None,
        })
    }

    fn volatile_only(self) -> bool {
        matches!(self, Self::VolatileLru | Self::VolatileLfu | Self::VolatileRandom | Self::VolatileTtl)
    }
}

#[derive(Debug, Clone, Copy)]
struct Usage {
    bytes: u64,
    last_access: u64,
    uses: u64,
    expires_at_ms: Option<u64>,
}

fn expiry_of(stored: &[u8]) -> Option<u64> {
    if stored.first() != Some(&1) {
        return None;
    }
    let word: [u8; 8] = stored.get(2..10)?.try_into().ok()?;
    let at = u64::from_le_bytes(word);
    (at != 0).then_some(at)
}

/// A keyspace that keeps its stored bytes under a limit, evicting keys by a policy after each
/// write that crosses it (or refusing writes under `noeviction`), as `maxmemory` does. Sizes are
/// a key's bytes and its stored value's.
pub struct EvictingKeyspace<'k> {
    inner: &'k (dyn Keyspace + Sync),
    limit: u64,
    policy: EvictionPolicy,
    usage: Mutex<HashMap<(u32, Vec<u8>), Usage>>,
    total: AtomicU64,
    clock: AtomicU64,
    seed: AtomicU64,
}

impl std::fmt::Debug for EvictingKeyspace<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvictingKeyspace")
            .field("limit", &self.limit)
            .field("policy", &self.policy)
            .field("total", &self.total.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

struct Tracked<'a, 'k> {
    inner: &'a mut dyn KeysWrite,
    owner: &'a EvictingKeyspace<'k>,
    db: u32,
}

struct TrackedRead<'a, 'k> {
    inner: &'a dyn KeysRead,
    owner: &'a EvictingKeyspace<'k>,
    db: u32,
}

impl KeysRead for TrackedRead<'_, '_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        self.owner.touch(self.db, key);
        self.inner.get(key)
    }

    fn keys(&self) -> Result<Vec<Vec<u8>>, String> {
        self.inner.keys()
    }
}

impl KeysRead for Tracked<'_, '_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        self.owner.touch(self.db, key);
        self.inner.get(key)
    }

    fn keys(&self) -> Result<Vec<Vec<u8>>, String> {
        self.inner.keys()
    }
}

impl KeysWrite for Tracked<'_, '_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), String> {
        self.inner.put(key, value)?;
        self.owner.record(self.db, key, Some(value));
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> Result<bool, String> {
        let removed = self.inner.delete(key)?;
        self.owner.record(self.db, key, None);
        Ok(removed)
    }

    fn clear(&mut self) -> Result<(), String> {
        for key in self.inner.keys()? {
            self.owner.record(self.db, &key, None);
        }
        self.inner.clear()
    }
}

impl<'k> EvictingKeyspace<'k> {
    /// Wraps `inner` with a limit of `limit` bytes and `policy`, counting what it holds now.
    pub fn new(inner: &'k (dyn Keyspace + Sync), limit: u64, policy: EvictionPolicy) -> Result<Self, String> {
        let keyspace = Self {
            inner,
            limit,
            policy,
            usage: Mutex::new(HashMap::new()),
            total: AtomicU64::new(0),
            clock: AtomicU64::new(1),
            seed: AtomicU64::new(0x9e37_79b9_7f4a_7c15),
        };
        for db in 0..DATABASES {
            inner.read(db, &mut |keys| {
                for key in keys.keys()? {
                    if let Some(value) = keys.get(&key)? {
                        keyspace.record(db, &key, Some(&value));
                    }
                }
                Ok(())
            })?;
        }
        Ok(keyspace)
    }

    /// The stored bytes counted now.
    pub fn used_bytes(&self) -> u64 {
        self.total.load(Ordering::Acquire)
    }

    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed)
    }

    fn touch(&self, db: u32, key: &[u8]) {
        let now = self.tick();
        let mut usage = self.usage.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = usage.get_mut(&(db, key.to_vec())) {
            entry.last_access = now;
            entry.uses = entry.uses.saturating_add(1);
        }
    }

    fn record(&self, db: u32, key: &[u8], value: Option<&[u8]>) {
        let now = self.tick();
        let mut usage = self.usage.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = (db, key.to_vec());
        if let Some(old) = usage.remove(&slot) {
            self.total.fetch_sub(old.bytes, Ordering::AcqRel);
        }
        if let Some(value) = value {
            let bytes = (key.len() + value.len()) as u64;
            self.total.fetch_add(bytes, Ordering::AcqRel);
            usage.insert(slot, Usage { bytes, last_access: now, uses: 1, expires_at_ms: expiry_of(value) });
        }
    }

    fn random(&self) -> u64 {
        let mut value = self.seed.load(Ordering::Relaxed);
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.seed.store(value, Ordering::Relaxed);
        value
    }

    fn victim(&self) -> Option<(u32, Vec<u8>)> {
        let usage = self.usage.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let candidates = usage
            .iter()
            .filter(|(_, entry)| !self.policy.volatile_only() || entry.expires_at_ms.is_some());
        let chosen = match self.policy {
            EvictionPolicy::NoEviction => None,
            EvictionPolicy::AllKeysLru | EvictionPolicy::VolatileLru => {
                candidates.min_by_key(|(_, entry)| entry.last_access)
            }
            EvictionPolicy::AllKeysLfu | EvictionPolicy::VolatileLfu => {
                candidates.min_by_key(|(_, entry)| (entry.uses, entry.last_access))
            }
            EvictionPolicy::VolatileTtl => candidates.min_by_key(|(_, entry)| entry.expires_at_ms),
            EvictionPolicy::AllKeysRandom | EvictionPolicy::VolatileRandom => {
                let all: Vec<_> = candidates.collect();
                (!all.is_empty()).then(|| all[(self.random() % all.len() as u64) as usize])
            }
        };
        chosen.map(|(slot, _)| slot.clone())
    }

    fn evict(&self) -> Result<(), String> {
        while self.used_bytes() > self.limit {
            let Some((db, key)) = self.victim() else {
                return Ok(());
            };
            self.inner.write(db, &mut |keys| {
                keys.delete(&key)?;
                Ok(())
            })?;
            self.record(db, &key, None);
        }
        Ok(())
    }
}

impl Keyspace for EvictingKeyspace<'_> {
    fn read(&self, db: u32, body: &mut dyn FnMut(&dyn KeysRead) -> Result<(), String>) -> Result<(), String> {
        self.inner.read(db, &mut |keys| body(&TrackedRead { inner: keys, owner: self, db }))
    }

    fn write(&self, db: u32, body: &mut dyn FnMut(&mut dyn KeysWrite) -> Result<(), String>) -> Result<(), String> {
        if self.policy == EvictionPolicy::NoEviction && self.used_bytes() > self.limit {
            return Err(OUT_OF_MEMORY.to_owned());
        }
        self.inner.write(db, &mut |keys| body(&mut Tracked { inner: keys, owner: self, db }))?;
        self.evict()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Client, Context, execute};
    use crate::keyspace::MemoryKeyspace;

    fn run(keyspace: &dyn Keyspace, client: &mut Client, line: &str) -> crate::protocol::Reply {
        let arguments: Vec<Vec<u8>> = line.split(' ').map(|word| word.as_bytes().to_vec()).collect();
        execute(&Context { keyspace, password_sha256: None, now_ms: 1000 }, client, &arguments)
    }

    #[test]
    fn the_least_recently_used_key_goes_first_and_the_limit_holds() {
        let memory = MemoryKeyspace::default();
        let keyspace = EvictingKeyspace::new(&memory, 200, EvictionPolicy::AllKeysLru).unwrap();
        let mut client = Client { authenticated: true, ..Client::default() };
        for key in ["a", "b", "c", "d"] {
            run(&keyspace, &mut client, &format!("SET {key} {}", "x".repeat(40)));
            run(&keyspace, &mut client, "GET a");
        }
        assert!(keyspace.used_bytes() <= 200);
        assert_eq!(run(&keyspace, &mut client, "EXISTS a"), crate::protocol::Reply::Integer(1));
        assert_eq!(run(&keyspace, &mut client, "EXISTS b"), crate::protocol::Reply::Integer(0));
    }

    #[test]
    fn noeviction_refuses_writes_past_the_limit_and_volatile_policies_spare_persistent_keys() {
        let memory = MemoryKeyspace::default();
        let keyspace = EvictingKeyspace::new(&memory, 60, EvictionPolicy::NoEviction).unwrap();
        let mut client = Client { authenticated: true, ..Client::default() };
        run(&keyspace, &mut client, &format!("SET a {}", "x".repeat(80)));
        assert_eq!(
            run(&keyspace, &mut client, "SET b y"),
            crate::protocol::Reply::Error(OUT_OF_MEMORY.to_owned())
        );
        let memory = MemoryKeyspace::default();
        let keyspace = EvictingKeyspace::new(&memory, 100, EvictionPolicy::VolatileTtl).unwrap();
        run(&keyspace, &mut client, &format!("SET keep {}", "x".repeat(30)));
        run(&keyspace, &mut client, &format!("SET soon {} EX 10", "x".repeat(30)));
        run(&keyspace, &mut client, &format!("SET later {} EX 100", "x".repeat(30)));
        assert_eq!(run(&keyspace, &mut client, "EXISTS keep later"), crate::protocol::Reply::Integer(2));
        assert_eq!(run(&keyspace, &mut client, "EXISTS soon"), crate::protocol::Reply::Integer(0));
        assert_eq!(EvictionPolicy::named("allkeys-lfu"), Some(EvictionPolicy::AllKeysLfu));
    }
}
