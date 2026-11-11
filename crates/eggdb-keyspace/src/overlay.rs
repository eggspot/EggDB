use crate::{Entries, EntryVisitor, KeysRead, KeysWrite, Keyspace};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// Changes staged for one database: keys put (`Some`) or deleted (`None`), after an optional
/// clear of everything.
#[derive(Debug, Clone, Default)]
pub struct StagedDatabase {
    cleared: bool,
    entries: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
}

/// Every database's staged changes.
pub type StagedChanges = BTreeMap<u32, StagedDatabase>;

/// A keyspace that stages its writes over a base keyspace instead of committing them, so a
/// multi-statement transaction reads its own writes and changes the base only at
/// [`Overlay::commit`]. Reads see the base's committed keys as they are when read, with the
/// staged changes on top.
pub struct Overlay<'k> {
    base: &'k (dyn Keyspace + Sync),
    staged: &'k Mutex<StagedChanges>,
}

impl std::fmt::Debug for Overlay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Overlay").finish_non_exhaustive()
    }
}

struct Merged<'a> {
    base: &'a dyn KeysRead,
    staged: &'a StagedDatabase,
}

impl KeysRead for Merged<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        match self.staged.entries.get(key) {
            Some(value) => Ok(value.clone()),
            None if self.staged.cleared => Ok(None),
            None => self.base.get(key),
        }
    }

    fn keys(&self) -> Result<Vec<Vec<u8>>, String> {
        let mut keys: BTreeSet<Vec<u8>> = if self.staged.cleared {
            BTreeSet::new()
        } else {
            self.base.keys()?.into_iter().collect()
        };
        for (key, value) in &self.staged.entries {
            if value.is_some() {
                keys.insert(key.clone());
            } else {
                keys.remove(key);
            }
        }
        Ok(keys.into_iter().collect())
    }

    fn scan(&self, prefix: &[u8]) -> Result<Entries, String> {
        let mut found: BTreeMap<Vec<u8>, Vec<u8>> = if self.staged.cleared {
            BTreeMap::new()
        } else {
            self.base.scan(prefix)?.into_iter().collect()
        };
        for (key, value) in self
            .staged
            .entries
            .range(prefix.to_vec()..)
            .take_while(|(key, _)| key.starts_with(prefix))
        {
            match value {
                Some(value) => found.insert(key.clone(), value.clone()),
                None => found.remove(key),
            };
        }
        Ok(found.into_iter().collect())
    }

    fn visit_prefix(&self, prefix: &[u8], visit: &mut EntryVisitor<'_>) -> Result<(), String> {
        for (key, value) in self.scan(prefix)? {
            if !visit(&key, &value)? {
                break;
            }
        }
        Ok(())
    }
}

struct MergedWriter<'a> {
    base: &'a dyn KeysRead,
    staged: &'a mut StagedDatabase,
}

impl MergedWriter<'_> {
    fn view(&self) -> Merged<'_> {
        Merged {
            base: self.base,
            staged: &*self.staged,
        }
    }
}

impl KeysRead for MergedWriter<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        self.view().get(key)
    }

    fn keys(&self) -> Result<Vec<Vec<u8>>, String> {
        self.view().keys()
    }

    fn scan(&self, prefix: &[u8]) -> Result<Entries, String> {
        self.view().scan(prefix)
    }

    fn visit_prefix(&self, prefix: &[u8], visit: &mut EntryVisitor<'_>) -> Result<(), String> {
        self.view().visit_prefix(prefix, visit)
    }
}

impl KeysWrite for MergedWriter<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), String> {
        self.staged
            .entries
            .insert(key.to_vec(), Some(value.to_vec()));
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> Result<bool, String> {
        let existed = self.get(key)?.is_some();
        self.staged.entries.insert(key.to_vec(), None);
        Ok(existed)
    }

    fn clear(&mut self) -> Result<(), String> {
        self.staged.cleared = true;
        self.staged.entries.clear();
        Ok(())
    }
}

impl<'k> Overlay<'k> {
    /// An overlay on `base` keeping its staged changes in `staged`, which outlives each
    /// command of the transaction.
    pub fn new(base: &'k (dyn Keyspace + Sync), staged: &'k Mutex<StagedChanges>) -> Self {
        Self { base, staged }
    }

    fn staged_of(&self, db: u32) -> Result<StagedDatabase, String> {
        Ok(self
            .staged
            .lock()
            .map_err(|_| "the transaction failed".to_owned())?
            .get(&db)
            .cloned()
            .unwrap_or_default())
    }

    /// Writes every staged change to the base, one write transaction per database, and
    /// forgets them.
    pub fn commit(&self) -> Result<(), String> {
        let staged = std::mem::take(
            &mut *self
                .staged
                .lock()
                .map_err(|_| "the transaction failed".to_owned())?,
        );
        for (db, changes) in staged {
            self.base.write(db, &mut |keys| {
                if changes.cleared {
                    keys.clear()?;
                }
                for (key, value) in &changes.entries {
                    match value {
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

impl Keyspace for Overlay<'_> {
    fn read(
        &self,
        db: u32,
        body: &mut dyn FnMut(&dyn KeysRead) -> Result<(), String>,
    ) -> Result<(), String> {
        let staged = self.staged_of(db)?;
        self.base.read(db, &mut |keys| {
            body(&Merged {
                base: keys,
                staged: &staged,
            })
        })
    }

    fn write(
        &self,
        db: u32,
        body: &mut dyn FnMut(&mut dyn KeysWrite) -> Result<(), String>,
    ) -> Result<(), String> {
        let before = self.staged_of(db)?;
        let mut after = None;
        self.base.read(db, &mut |keys| {
            let mut working = before.clone();
            body(&mut MergedWriter {
                base: keys,
                staged: &mut working,
            })?;
            after = Some(working);
            Ok(())
        })?;
        if let Some(after) = after {
            self.staged
                .lock()
                .map_err(|_| "the transaction failed".to_owned())?
                .insert(db, after);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryKeyspace;

    fn value(keyspace: &dyn Keyspace, key: &[u8]) -> Option<Vec<u8>> {
        let mut found = None;
        keyspace
            .read(0, &mut |keys| {
                found = keys.get(key)?;
                Ok(())
            })
            .expect("the read runs");
        found
    }

    #[test]
    fn staged_writes_are_seen_through_the_overlay_and_reach_the_base_at_commit() {
        let base = MemoryKeyspace::default();
        base.write(0, &mut |keys| {
            keys.put(b"a", b"1")?;
            keys.put(b"b", b"2")
        })
        .expect("the base is written");
        let staged = Mutex::new(StagedChanges::new());
        let overlay = Overlay::new(&base, &staged);
        overlay
            .write(0, &mut |keys| {
                keys.put(b"c", b"3")?;
                assert!(keys.delete(b"a")?);
                Ok(())
            })
            .expect("the overlay stages");
        assert_eq!(value(&overlay, b"c"), Some(b"3".to_vec()));
        assert_eq!(value(&overlay, b"a"), None);
        assert_eq!(value(&base, b"a"), Some(b"1".to_vec()), "the base is untouched");
        let mut listed = Vec::new();
        overlay
            .read(0, &mut |keys| {
                listed = keys.scan(b"")?;
                Ok(())
            })
            .expect("the overlay scans");
        assert_eq!(
            listed,
            vec![(b"b".to_vec(), b"2".to_vec()), (b"c".to_vec(), b"3".to_vec())]
        );
        overlay.commit().expect("the overlay commits");
        assert_eq!(value(&base, b"a"), None);
        assert_eq!(value(&base, b"c"), Some(b"3".to_vec()));
        assert!(staged.lock().expect("unpoisoned").is_empty());
    }
}
