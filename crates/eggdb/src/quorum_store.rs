//! A leaderless quorum node's replica kept in the database (D1097): each key's versions in the
//! collection `eggdb.quorum`, and the hints held for unreachable nodes in `eggdb.quorum_hints`,
//! so a node's replica and hints survive a restart, are in its backups, and take part in its
//! transactions.

use crate::{Database, Result};
use eggdb_cluster::NodeId;
use eggdb_cluster::quorum::{Resolution, Version, reconcile};
use eggdb_cluster::quorum_net::ReplicaStore;
use eggdb_cluster::quorum_wire::{decode_version, decode_versions, encode_version, encode_versions};

const VERSIONS: &str = "eggdb.quorum";
const HINTS: &str = "eggdb.quorum_hints";
const NEXT_HINT: &[u8] = b"next";
const HINT_PREFIX: u8 = b'h';

/// A quorum node's replica and hints in a [`Database`].
#[derive(Debug)]
pub struct DatabaseReplicaStore<'db> {
    db: &'db Database,
}

impl<'db> DatabaseReplicaStore<'db> {
    /// Keeps the replica in `db`, creating its two collections when they are missing.
    pub fn new(db: &'db Database) -> Result<Self> {
        db.create_collection(VERSIONS)?;
        db.create_collection(HINTS)?;
        Ok(Self { db })
    }
}

fn text(error: crate::Error) -> String {
    error.to_string()
}

fn damaged(what: &str) -> String {
    format!("the quorum {what} kept in the database is damaged")
}

fn hint_key(id: u64) -> Vec<u8> {
    let mut key = vec![HINT_PREFIX];
    key.extend_from_slice(&id.to_be_bytes());
    key
}

impl ReplicaStore for DatabaseReplicaStore<'_> {
    fn versions(&self, key: &[u8]) -> std::result::Result<Vec<Version>, String> {
        let stored = self.db.read(|tx| tx.collection(VERSIONS)?.get(key)).map_err(text)?;
        match stored {
            Some(bytes) => decode_versions(&bytes).ok_or_else(|| damaged("versions")),
            None => Ok(Vec::new()),
        }
    }

    fn apply(&self, key: &[u8], version: &Version, resolution: Resolution) -> std::result::Result<bool, String> {
        let mut outcome = Ok(false);
        self.db
            .write(|tx| {
                let mut collection = tx.collection(VERSIONS)?;
                let mut held = match collection.get(key)? {
                    Some(bytes) => match decode_versions(&bytes) {
                        Some(held) => held,
                        None => {
                            outcome = Err(damaged("versions"));
                            return Ok(());
                        }
                    },
                    None => Vec::new(),
                };
                let changed = reconcile(&mut held, version, resolution);
                if changed {
                    collection.put(key, encode_versions(&held))?;
                }
                outcome = Ok(changed);
                Ok(())
            })
            .map_err(text)?;
        outcome
    }

    fn keys(&self) -> std::result::Result<Vec<Vec<u8>>, String> {
        self.db
            .read(|tx| {
                tx.collection(VERSIONS)?
                    .scan(..)
                    .map(|entry| entry.map(|(key, _)| key))
                    .collect::<Result<Vec<_>>>()
            })
            .map_err(text)
    }

    fn add_hint(&self, owner: NodeId, key: &[u8], version: &Version) -> std::result::Result<(), String> {
        self.db
            .write(|tx| {
                let mut hints = tx.collection(HINTS)?;
                let id = u64::try_from(hints.increment(NEXT_HINT, 1)?).unwrap_or(u64::MAX);
                let mut value = owner.to_be_bytes().to_vec();
                value.extend_from_slice(&u32::try_from(key.len()).unwrap_or(u32::MAX).to_be_bytes());
                value.extend_from_slice(key);
                value.extend_from_slice(&encode_version(version));
                hints.put(hint_key(id), value)
            })
            .map_err(text)
    }

    fn hints(&self) -> std::result::Result<Vec<(u64, NodeId, Vec<u8>, Version)>, String> {
        let entries = self
            .db
            .read(|tx| tx.collection(HINTS)?.scan_prefix([HINT_PREFIX]).collect::<Result<Vec<_>>>())
            .map_err(text)?;
        let mut hints = Vec::with_capacity(entries.len());
        for (key, value) in entries {
            let id = key
                .get(1..9)
                .and_then(|bytes| bytes.try_into().ok())
                .map(u64::from_be_bytes)
                .ok_or_else(|| damaged("hint"))?;
            let owner = value
                .get(..8)
                .and_then(|bytes| bytes.try_into().ok())
                .map(u64::from_be_bytes)
                .ok_or_else(|| damaged("hint"))?;
            let length = value
                .get(8..12)
                .and_then(|bytes| bytes.try_into().ok())
                .map(u32::from_be_bytes)
                .and_then(|length| usize::try_from(length).ok())
                .ok_or_else(|| damaged("hint"))?;
            let hinted = value.get(12..12 + length).ok_or_else(|| damaged("hint"))?.to_vec();
            let version = value
                .get(12 + length..)
                .and_then(decode_version)
                .ok_or_else(|| damaged("hint"))?;
            hints.push((id, owner, hinted, version));
        }
        Ok(hints)
    }

    fn drop_hint(&self, id: u64) -> std::result::Result<(), String> {
        self.db
            .write(|tx| tx.collection(HINTS)?.delete(hint_key(id)).map(|_| ()))
            .map_err(text)
    }
}
