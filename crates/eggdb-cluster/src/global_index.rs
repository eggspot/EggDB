//! Global secondary indexes over sharded data (D1043): an index entry lives on the shard its
//! indexed value hashes to, not on its row's shard, so a lookup by value asks one shard
//! instead of all of them. Entries change in the same distributed transaction as their row
//! ([`entry_changes`] gives the writes to add to it), so with D1042's snapshots a read never
//! sees an index entry without its row or a row without its entry.

use crate::{ShardId, Strategy, route};

/// A global index: its name and how many shards its entries are spread over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalIndex {
    /// The index's name, which prefixes its entries' keys.
    pub name: String,
    /// The number of shards its entries hash over.
    pub shards: u32,
}

/// One write to an index shard: the shard, the entry's key, and its value (the row's primary
/// key) or `None` to delete it.
pub type EntryChange = (ShardId, Vec<u8>, Option<Vec<u8>>);

fn put_part(key: &mut Vec<u8>, part: &[u8]) {
    key.extend_from_slice(&(part.len() as u32).to_be_bytes());
    key.extend_from_slice(part);
}

impl GlobalIndex {
    /// The shard that holds the entries for `value`.
    pub fn shard_of(&self, value: &[u8]) -> ShardId {
        route(&Strategy::Hash { shards: self.shards.max(1) }, value).unwrap_or(0)
    }

    /// The prefix every entry for `value` starts with: the index's name and the value, each
    /// length-prefixed, so no value's entries run into another's.
    pub fn value_prefix(&self, value: &[u8]) -> Vec<u8> {
        let mut key = b"\xffgix\x00".to_vec();
        put_part(&mut key, self.name.as_bytes());
        put_part(&mut key, value);
        key
    }

    /// The key of the entry for the row with `primary_key` and the indexed `value`.
    pub fn entry_key(&self, value: &[u8], primary_key: &[u8]) -> Vec<u8> {
        let mut key = self.value_prefix(value);
        put_part(&mut key, primary_key);
        key
    }

    /// The index writes that go with changing a row's indexed value from `old` to `new`
    /// (`None` when the row did not exist, or no longer does): the old entry deleted and the
    /// new one written, nothing when the value did not change.
    pub fn entry_changes(&self, primary_key: &[u8], old: Option<&[u8]>, new: Option<&[u8]>) -> Vec<EntryChange> {
        if old == new {
            return Vec::new();
        }
        let mut changes = Vec::new();
        if let Some(old) = old {
            changes.push((self.shard_of(old), self.entry_key(old, primary_key), None));
        }
        if let Some(new) = new {
            changes.push((self.shard_of(new), self.entry_key(new, primary_key), Some(primary_key.to_vec())));
        }
        changes
    }
}
