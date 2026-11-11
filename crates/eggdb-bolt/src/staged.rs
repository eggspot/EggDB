use eggdb_keyspace::{KeysRead, KeysWrite, Keyspace};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

/// The most bytes an explicit transaction may stage before it commits.
pub(crate) const MOST_STAGED_BYTES: usize = 64 << 20;

/// An explicit Bolt transaction's writes, held in memory until `COMMIT` applies them: per
/// database, each key's new bytes (`None` once deleted), and the databases it cleared.
#[derive(Debug, Default)]
pub(crate) struct StagedChanges {
    changes: RefCell<BTreeMap<u32, BTreeMap<Vec<u8>, Option<Vec<u8>>>>>,
    cleared: RefCell<BTreeSet<u32>>,
    bytes: std::cell::Cell<usize>,
}

impl StagedChanges {
    /// Whether the transaction has written anything.
    pub(crate) fn is_empty(&self) -> bool {
        self.changes.borrow().values().all(BTreeMap::is_empty) && self.cleared.borrow().is_empty()
    }

    /// Applies the staged writes to `base`, one write transaction per database touched.
    pub(crate) fn commit(&self, base: &dyn Keyspace) -> Result<(), String> {
        let cleared = self.cleared.borrow().clone();
        let changes = self.changes.borrow().clone();
        let databases: BTreeSet<u32> = changes.keys().copied().chain(cleared.iter().copied()).collect();
        for db in databases {
            let empty = BTreeMap::new();
            let written = changes.get(&db).unwrap_or(&empty);
            base.write(db, &mut |keys| {
                if cleared.contains(&db) {
                    keys.clear()?;
                }
                for (key, change) in written {
                    match change {
                        Some(value) => keys.put(key, value)?,
                        None => {
                            keys.delete(key)?;
                        }
                    }
                }
                Ok(())
            })?;
        }
        Ok(())
    }
}

/// The base keyspace as an explicit transaction sees it: its own staged writes over the
/// committed keys, with writes staged instead of applied.
pub(crate) struct Staged<'k> {
    pub(crate) base: &'k dyn Keyspace,
    pub(crate) staged: &'k StagedChanges,
}

struct Layered<'b> {
    base: &'b dyn KeysRead,
    changes: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    cleared: bool,
}

impl KeysRead for Layered<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        match self.changes.get(key) {
            Some(change) => Ok(change.clone()),
            None if self.cleared => Ok(None),
            None => self.base.get(key),
        }
    }

    fn keys(&self) -> Result<Vec<Vec<u8>>, String> {
        let mut keys: BTreeSet<Vec<u8>> = if self.cleared {
            BTreeSet::new()
        } else {
            self.base.keys()?.into_iter().collect()
        };
        for (key, change) in &self.changes {
            match change {
                Some(_) => {
                    keys.insert(key.clone());
                }
                None => {
                    keys.remove(key);
                }
            }
        }
        Ok(keys.into_iter().collect())
    }
}

impl KeysWrite for Layered<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), String> {
        self.changes.insert(key.to_vec(), Some(value.to_vec()));
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> Result<bool, String> {
        let existed = self.get(key)?.is_some();
        self.changes.insert(key.to_vec(), None);
        Ok(existed)
    }

    fn clear(&mut self) -> Result<(), String> {
        self.changes.clear();
        self.cleared = true;
        Ok(())
    }
}

fn change_bytes(changes: &BTreeMap<Vec<u8>, Option<Vec<u8>>>) -> usize {
    changes
        .iter()
        .map(|(key, value)| key.len() + value.as_ref().map_or(0, Vec::len) + 16)
        .sum()
}

impl Keyspace for Staged<'_> {
    fn read(
        &self,
        db: u32,
        body: &mut dyn FnMut(&dyn KeysRead) -> Result<(), String>,
    ) -> Result<(), String> {
        let changes = self.staged.changes.borrow().get(&db).cloned().unwrap_or_default();
        let cleared = self.staged.cleared.borrow().contains(&db);
        self.base.read(db, &mut |keys| {
            body(&Layered {
                base: keys,
                changes: changes.clone(),
                cleared,
            })
        })
    }

    fn write(
        &self,
        db: u32,
        body: &mut dyn FnMut(&mut dyn KeysWrite) -> Result<(), String>,
    ) -> Result<(), String> {
        let before = self.staged.changes.borrow().get(&db).cloned().unwrap_or_default();
        let was_cleared = self.staged.cleared.borrow().contains(&db);
        let mut outcome = None;
        self.base.read(db, &mut |keys| {
            let mut layered = Layered {
                base: keys,
                changes: before.clone(),
                cleared: was_cleared,
            };
            body(&mut layered)?;
            outcome = Some((layered.changes, layered.cleared));
            Ok(())
        })?;
        let Some((changes, cleared)) = outcome else {
            return Ok(());
        };
        let others: usize = self
            .staged
            .changes
            .borrow()
            .iter()
            .filter(|(other, _)| **other != db)
            .map(|(_, changes)| change_bytes(changes))
            .sum();
        let total = others + change_bytes(&changes);
        if total > MOST_STAGED_BYTES {
            return Err(format!(
                "the transaction would stage more than {MOST_STAGED_BYTES} bytes; commit in smaller transactions"
            ));
        }
        self.staged.bytes.set(total);
        self.staged.changes.borrow_mut().insert(db, changes);
        if cleared {
            self.staged.cleared.borrow_mut().insert(db);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_keyspace::MemoryKeyspace;

    fn value(keyspace: &dyn Keyspace, key: &[u8]) -> Option<Vec<u8>> {
        let mut found = None;
        keyspace
            .read(0, &mut |keys| {
                found = keys.get(key)?;
                Ok(())
            })
            .unwrap();
        found
    }

    #[test]
    fn staged_writes_are_seen_inside_and_applied_only_on_commit() {
        let base = MemoryKeyspace::default();
        base.write(0, &mut |keys| keys.put(b"a", b"1")).unwrap();
        let staged = StagedChanges::default();
        let inside = Staged { base: &base, staged: &staged };
        inside
            .write(0, &mut |keys| {
                keys.put(b"b", b"2")?;
                keys.delete(b"a")?;
                Ok(())
            })
            .unwrap();
        assert_eq!(value(&inside, b"b"), Some(b"2".to_vec()));
        assert_eq!(value(&inside, b"a"), None);
        assert_eq!(value(&base, b"a"), Some(b"1".to_vec()));
        assert_eq!(value(&base, b"b"), None);
        let failed = inside.write(0, &mut |keys| {
            keys.put(b"c", b"3")?;
            Err("refused".to_owned())
        });
        assert!(failed.is_err());
        assert_eq!(value(&inside, b"c"), None);
        assert!(!staged.is_empty());
        staged.commit(&base).unwrap();
        assert_eq!(value(&base, b"a"), None);
        assert_eq!(value(&base, b"b"), Some(b"2".to_vec()));
    }
}
