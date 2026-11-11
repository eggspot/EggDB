//! The key-value transaction interface EggDB's key-value and document servers share: a server
//! runs each command as one read or write transaction on a numbered database of byte keys, and
//! the `eggdb` binary backs it with collections of the served file. [`MemoryKeyspace`] keeps the
//! keys in memory, for tests.

use std::collections::BTreeMap;
use std::sync::Mutex;

mod overlay;

pub use overlay::{Overlay, StagedChanges, StagedDatabase};

/// Keys and their stored bytes, in key order.
pub type Entries = Vec<(Vec<u8>, Vec<u8>)>;

/// Receives one key and its stored bytes from [`KeysRead::visit_prefix`]; returns whether to go on.
pub type EntryVisitor<'a> = dyn FnMut(&[u8], &[u8]) -> Result<bool, String> + 'a;

type Database = BTreeMap<Vec<u8>, Vec<u8>>;

/// Reads one database's keys within a transaction.
pub trait KeysRead {
    /// The stored bytes of `key`.
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String>;
    /// Every key, in byte order.
    fn keys(&self) -> Result<Vec<Vec<u8>>, String>;
    /// Calls `visit` with each key in byte order, one at a time, until it returns `false` or an
    /// error, so a caller that keeps only some keys never holds them all.
    fn visit_keys(
        &self,
        visit: &mut dyn FnMut(Vec<u8>) -> Result<bool, String>,
    ) -> Result<(), String> {
        for key in self.keys()? {
            if !visit(key)? {
                break;
            }
        }
        Ok(())
    }
    /// Every key starting with `prefix` and its stored bytes, in key order.
    fn scan(&self, prefix: &[u8]) -> Result<Entries, String> {
        let mut found = Vec::new();
        for key in self.keys()? {
            if key.starts_with(prefix)
                && let Some(value) = self.get(&key)?
            {
                found.push((key, value));
            }
        }
        Ok(found)
    }
    /// Calls `visit` with each key starting with `prefix` and its stored bytes, in key order,
    /// one entry at a time, until it returns `false` or an error, so a caller that needs only
    /// the first entries never holds the others.
    fn visit_prefix(&self, prefix: &[u8], visit: &mut EntryVisitor<'_>) -> Result<(), String> {
        self.visit_keys(&mut |key| {
            if !key.starts_with(prefix) {
                return Ok(key.as_slice() < prefix);
            }
            match self.get(&key)? {
                Some(value) => visit(&key, &value),
                None => Ok(true),
            }
        })
    }
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

/// Where a server's numbered databases of keys live: each command runs in one read or write
/// transaction, so it is atomic. A write whose body fails changes nothing.
pub trait Keyspace {
    /// Runs `body` on a snapshot of database `db`.
    fn read(
        &self,
        db: u32,
        body: &mut dyn FnMut(&dyn KeysRead) -> Result<(), String>,
    ) -> Result<(), String>;
    /// Runs `body` in a write transaction on database `db`, committing when it succeeds.
    fn write(
        &self,
        db: u32,
        body: &mut dyn FnMut(&mut dyn KeysWrite) -> Result<(), String>,
    ) -> Result<(), String>;
}

/// A keyspace in memory, for tests and for servers that keep no file.
#[derive(Debug, Default)]
pub struct MemoryKeyspace {
    databases: Mutex<BTreeMap<u32, Database>>,
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

    fn visit_keys(
        &self,
        visit: &mut dyn FnMut(Vec<u8>) -> Result<bool, String>,
    ) -> Result<(), String> {
        for key in self.map.keys() {
            if !visit(key.clone())? {
                break;
            }
        }
        Ok(())
    }

    fn scan(&self, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>, String> {
        Ok(self
            .map
            .range(prefix.to_vec()..)
            .take_while(|(key, _)| key.starts_with(prefix))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect())
    }

    fn visit_prefix(&self, prefix: &[u8], visit: &mut EntryVisitor<'_>) -> Result<(), String> {
        for (key, value) in self
            .map
            .range(prefix.to_vec()..)
            .take_while(|(key, _)| key.starts_with(prefix))
        {
            if !visit(key, value)? {
                break;
            }
        }
        Ok(())
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
    fn read(
        &self,
        db: u32,
        body: &mut dyn FnMut(&dyn KeysRead) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut databases = self
            .databases
            .lock()
            .map_err(|_| "the keyspace failed".to_owned())?;
        let mut snapshot = databases.entry(db).or_default().clone();
        body(&MemoryKeys { map: &mut snapshot })
    }

    fn write(
        &self,
        db: u32,
        body: &mut dyn FnMut(&mut dyn KeysWrite) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut databases = self
            .databases
            .lock()
            .map_err(|_| "the keyspace failed".to_owned())?;
        let current = databases.entry(db).or_default();
        let mut changed = current.clone();
        body(&mut MemoryKeys { map: &mut changed })?;
        *current = changed;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{KeysRead, Keyspace, MemoryKeyspace};
    use std::collections::BTreeMap;

    struct KeysOnly(BTreeMap<Vec<u8>, Vec<u8>>);

    impl KeysRead for KeysOnly {
        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
            Ok(self.0.get(key).cloned())
        }

        fn keys(&self) -> Result<Vec<Vec<u8>>, String> {
            Ok(self.0.keys().cloned().collect())
        }
    }

    fn entries() -> BTreeMap<Vec<u8>, Vec<u8>> {
        [&b"a"[..], b"b", b"b\x01", b"b\x02", b"b\x03", b"c"]
            .into_iter()
            .map(|key| (key.to_vec(), key.to_vec()))
            .collect()
    }

    fn first_two_under_b(keys: &dyn KeysRead) -> (Vec<Vec<u8>>, usize) {
        let mut seen = Vec::new();
        let mut calls = 0;
        keys.visit_prefix(b"b", &mut |key, value| {
            calls += 1;
            assert_eq!(key, value);
            seen.push(key.to_vec());
            Ok(seen.len() < 2)
        })
        .unwrap();
        (seen, calls)
    }

    #[test]
    fn visit_prefix_sees_only_the_prefix_and_stops_when_told() {
        let expected = vec![b"b".to_vec(), b"b\x01".to_vec()];
        assert_eq!(
            first_two_under_b(&KeysOnly(entries())),
            (expected.clone(), 2)
        );
        let keyspace = MemoryKeyspace::default();
        keyspace
            .write(0, &mut |keys| {
                for (key, value) in entries() {
                    keys.put(&key, &value)?;
                }
                Ok(())
            })
            .unwrap();
        keyspace
            .read(0, &mut |keys| {
                assert_eq!(first_two_under_b(keys), (expected.clone(), 2));
                Ok(())
            })
            .unwrap();
    }
}
