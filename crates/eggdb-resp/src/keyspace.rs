use std::collections::BTreeMap;
use std::sync::Mutex;

/// Reads one database's keys within a transaction.
pub trait KeysRead {
    /// The stored bytes of `key`.
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String>;
    /// Every key, in byte order.
    fn keys(&self) -> Result<Vec<Vec<u8>>, String>;
}

/// Changes one database's keys within a transaction.
pub trait KeysWrite: KeysRead {
    /// Stores `value` under `key`.
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), String>;
    /// Removes `key`; returns whether it was there.
    fn delete(&mut self, key: &[u8]) -> Result<bool, String>;
    /// Removes every key.
    fn clear(&mut self) -> Result<(), String>;
}

/// Where the keys of the RESP server's databases (0 to 15) live: each command runs in one
/// read or write transaction, so it is atomic. A write whose body fails changes nothing.
pub trait Keyspace: Sync {
    /// Runs `body` on a snapshot of database `db`.
    fn read(&self, db: u32, body: &mut dyn FnMut(&dyn KeysRead) -> Result<(), String>) -> Result<(), String>;
    /// Runs `body` in a write transaction on database `db`, committing when it succeeds.
    fn write(&self, db: u32, body: &mut dyn FnMut(&mut dyn KeysWrite) -> Result<(), String>) -> Result<(), String>;
}

/// A keyspace in memory, for tests and for servers that keep no file.
#[derive(Debug, Default)]
pub struct MemoryKeyspace {
    databases: Mutex<BTreeMap<u32, BTreeMap<Vec<u8>, Vec<u8>>>>,
}

struct MemoryKeys<'a> {
    map: &'a mut BTreeMap<Vec<u8>, Vec<u8>>,
}

impl KeysRead for MemoryKeys<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        Ok(self.map.get(key).cloned())
    }

    fn keys(&self) -> Result<Vec<Vec<u8>>, String> {
        Ok(self.map.keys().cloned().collect())
    }
}

impl KeysWrite for MemoryKeys<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), String> {
        self.map.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> Result<bool, String> {
        Ok(self.map.remove(key).is_some())
    }

    fn clear(&mut self) -> Result<(), String> {
        self.map.clear();
        Ok(())
    }
}

impl Keyspace for MemoryKeyspace {
    fn read(&self, db: u32, body: &mut dyn FnMut(&dyn KeysRead) -> Result<(), String>) -> Result<(), String> {
        let mut databases = self.databases.lock().map_err(|_| "the keyspace failed".to_owned())?;
        let mut snapshot = databases.entry(db).or_default().clone();
        body(&MemoryKeys { map: &mut snapshot })
    }

    fn write(&self, db: u32, body: &mut dyn FnMut(&mut dyn KeysWrite) -> Result<(), String>) -> Result<(), String> {
        let mut databases = self.databases.lock().map_err(|_| "the keyspace failed".to_owned())?;
        let current = databases.entry(db).or_default();
        let mut changed = current.clone();
        body(&mut MemoryKeys { map: &mut changed })?;
        *current = changed;
        Ok(())
    }
}
