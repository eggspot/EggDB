use crate::{Database, Error, ErrorKind, Result};
use eggdb_cluster::{ShardId, ShardMap, route};
use eggdb_replication::{Batch, Change};
use std::ops::Bound;

/// One key's state in a shard copy: its value, or `None` when it was deleted.
pub type ShardChange = (Vec<u8>, Option<Vec<u8>>);

/// One page of a shard's keys, read from one snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShardPage {
    /// The shard's keys among those scanned, with their values.
    pub entries: Vec<(Vec<u8>, Vec<u8>)>,
    /// The last key scanned, to continue after; `None` once the key space is done.
    pub next_after: Option<Vec<u8>>,
}

fn reserved(key: &[u8]) -> bool {
    key.first() == Some(&0xFF) && !key.starts_with(eggdb_cluster::tenants::TENANT_PREFIX)
}

/// The changes of `batch` to `tenant`'s keys, in the batch's order: what the new home of a
/// moving tenant applies to catch up (D1047).
pub fn tenant_changes(batch: &Batch, tenant: &str) -> Vec<ShardChange> {
    let Some(prefix) = eggdb_cluster::tenants::tenant_prefix(tenant) else {
        return Vec::new();
    };
    batch
        .changes
        .iter()
        .filter_map(|change| match change {
            Change::Put {
                collection,
                key,
                value,
            } if collection.is_empty() => Some((key.clone(), Some(value.clone()))),
            Change::Delete { collection, key } if collection.is_empty() => Some((key.clone(), None)),
            _ => None,
        })
        .filter(|(key, _)| key.starts_with(&prefix))
        .collect()
}

/// The changes of `batch` to keys of `shard` under `map`, in the batch's order: what the new
/// owner of a moving shard applies to catch up (D1040).
pub fn shard_changes(batch: &Batch, map: &ShardMap, shard: ShardId) -> Vec<ShardChange> {
    batch
        .changes
        .iter()
        .filter_map(|change| match change {
            Change::Put {
                collection,
                key,
                value,
            } if collection.is_empty() => Some((key.clone(), Some(value.clone()))),
            Change::Delete { collection, key } if collection.is_empty() => Some((key.clone(), None)),
            _ => None,
        })
        .filter(|(key, _)| !reserved(key) && route(&map.strategy, key) == Some(shard))
        .collect()
}

impl Database {
    /// Up to `scanned` keys of the default key space after `after`, from one snapshot, with
    /// those of `shard` under `map` returned (D1040). A move copies a shard page by page while
    /// the old owner keeps serving; changes made meanwhile come after, from the replication
    /// batches, so pages read at different times still converge.
    pub fn shard_page(
        &self,
        map: &ShardMap,
        shard: ShardId,
        after: Option<&[u8]>,
        scanned: usize,
    ) -> Result<ShardPage> {
        let read = self.begin_read()?;
        let range: (Bound<Vec<u8>>, Bound<Vec<u8>>) = match after {
            Some(after) => (Bound::Excluded(after.to_vec()), Bound::Unbounded),
            None => (Bound::Unbounded, Bound::Unbounded),
        };
        let mut page = ShardPage::default();
        let mut count = 0;
        for item in read.scan(range) {
            let (key, value) = item?;
            count += 1;
            let last = key.clone();
            if !reserved(&key) && route(&map.strategy, &key) == Some(shard) {
                page.entries.push((key, value));
            }
            if count >= scanned.max(1) {
                page.next_after = Some(last);
                break;
            }
        }
        Ok(page)
    }

    /// Writes a shard copy's page or catch-up changes in one write transaction. Only keys of
    /// the default key space are taken; a reserved key is refused.
    pub fn apply_shard_changes(&self, changes: &[ShardChange]) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let mut transaction = self.begin_write()?;
        for (key, value) in changes {
            if reserved(key) {
                return Err(Error::new(
                    ErrorKind::Other,
                    "a shard copy may not write the engine's reserved keys",
                ));
            }
            match value {
                Some(value) => transaction.put_raw(key, value)?,
                None => {
                    transaction.delete_raw(key)?;
                }
            }
        }
        transaction.commit()?;
        Ok(())
    }
}

/// Changes as they cross the wire in a shard copy: a count (4 bytes, little-endian), then each
/// key's length (4) and bytes, 1 and the value's length (4) and bytes, or 0 for a deletion.
pub fn encode_shard_changes(changes: &[ShardChange]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(changes.len() as u32).to_le_bytes());
    for (key, value) in changes {
        out.extend_from_slice(&(key.len() as u32).to_le_bytes());
        out.extend_from_slice(key);
        match value {
            Some(value) => {
                out.push(1);
                out.extend_from_slice(&(value.len() as u32).to_le_bytes());
                out.extend_from_slice(value);
            }
            None => out.push(0),
        }
    }
    out
}

/// Reads [`encode_shard_changes`]'s bytes; `None` when they do not have its layout.
pub fn decode_shard_changes(bytes: &[u8]) -> Option<Vec<ShardChange>> {
    let mut rest = bytes;
    let mut take = |count: usize| -> Option<&[u8]> {
        let (taken, tail) = rest.split_at_checked(count)?;
        rest = tail;
        Some(taken)
    };
    let length = |bytes: &[u8]| -> Option<usize> {
        usize::try_from(u32::from_le_bytes(bytes.try_into().ok()?)).ok()
    };
    let count = length(take(4)?)?;
    if count > bytes.len() / 5 {
        return None;
    }
    let mut changes = Vec::with_capacity(count);
    for _ in 0..count {
        let key_length = length(take(4)?)?;
        let key = take(key_length)?.to_vec();
        let value = match take(1)?[0] {
            0 => None,
            1 => {
                let value_length = length(take(4)?)?;
                Some(take(value_length)?.to_vec())
            }
            _ => return None,
        };
        changes.push((key, value));
    }
    rest.is_empty().then_some(changes)
}
