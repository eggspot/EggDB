use crate::{Database, Error, ErrorKind, Result};
use eggdb_replication::{Apply, Batch, Change, Mode, Primary, ReplicaId};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

pub(crate) const POSITION_KEY: &[u8] = b"\xffCOL\x00\x7freplication";
const DEFAULT_RETAINED_BATCHES: usize = 100_000;

fn replication_error(message: impl Into<Box<str>>) -> Error {
    Error::new(ErrorKind::Other, message)
}

pub(crate) fn encode_position(term: u64, sequence: u64) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&term.to_le_bytes());
    bytes[8..].copy_from_slice(&sequence.to_le_bytes());
    bytes
}

fn decode_position(bytes: &[u8]) -> Option<(u64, u64)> {
    let term = u64::from_le_bytes(bytes.get(..8)?.try_into().ok()?);
    let sequence = u64::from_le_bytes(bytes.get(8..16)?.try_into().ok()?);
    Some((term, sequence))
}

/// A database's side as a replication primary (D1024): every commit becomes a numbered
/// [`Batch`] kept in memory until the replicas acknowledge it, at most 100,000 of them.
pub struct ReplicationPrimary {
    state: Mutex<Primary>,
    more: Condvar,
    acknowledged: Condvar,
    retain: usize,
}

impl std::fmt::Debug for ReplicationPrimary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplicationPrimary").finish_non_exhaustive()
    }
}

impl ReplicationPrimary {
    fn lock(&self) -> Result<MutexGuard<'_, Primary>> {
        self.state.lock().map_err(|_| replication_error("the replication state failed"))
    }

    /// The primary's term.
    pub fn term(&self) -> Result<u64> {
        Ok(self.lock()?.term())
    }

    /// The batches a replica that applied up to `applied` needs next, at most `limit`; `Err`
    /// with the oldest batch still kept when the replica needs one already forgotten.
    pub fn batches_after(&self, applied: u64, limit: usize) -> Result<std::result::Result<Vec<Batch>, u64>> {
        Ok(self.lock()?.batches_after(applied, limit))
    }

    /// Waits up to `timeout` for a commit after `applied`; returns whether one came.
    pub fn wait_for_more(&self, applied: u64, timeout: Duration) -> Result<bool> {
        let guard = self.lock()?;
        let (guard, _) = self
            .more
            .wait_timeout_while(guard, timeout, |primary| primary.last() <= applied)
            .map_err(|_| replication_error("the replication state failed"))?;
        Ok(guard.last() > applied)
    }

    /// Starts shipping to `replica`, which has applied up to `applied`.
    pub fn add_replica(&self, replica: ReplicaId, applied: u64) -> Result<()> {
        self.lock()?.add_replica(replica, applied);
        Ok(())
    }

    /// Stops keeping batches for `replica`.
    pub fn remove_replica(&self, replica: ReplicaId) -> Result<()> {
        self.lock()?.remove_replica(replica);
        Ok(())
    }

    /// Records that `replica` applied everything up to `sequence`.
    pub fn acknowledge(&self, replica: ReplicaId, sequence: u64) -> Result<()> {
        self.lock()?.acknowledge(replica, sequence);
        self.acknowledged.notify_all();
        Ok(())
    }

    /// Waits up to `timeout` for the commit at `sequence` to be durable under the primary's
    /// mode: at once when asynchronous, once enough replicas acknowledged it when synchronous.
    pub fn wait_durable(&self, sequence: u64, timeout: Duration) -> Result<bool> {
        let guard = self.lock()?;
        let (guard, _) = self
            .acknowledged
            .wait_timeout_while(guard, timeout, |primary| !primary.is_durable(sequence))
            .map_err(|_| replication_error("the replication state failed"))?;
        Ok(guard.is_durable(sequence))
    }

    pub(crate) fn next_position(&self) -> Result<[u8; 16]> {
        let primary = self.lock()?;
        Ok(encode_position(primary.term(), primary.last() + 1))
    }

    pub(crate) fn committed(&self, changes: &[crate::change_archive::ArchivedChange]) -> Result<u64> {
        let changes = changes
            .iter()
            .map(|(key, value)| match value {
                Some(value) => Change::Put { collection: String::new(), key: key.clone(), value: value.clone() },
                None => Change::Delete { collection: String::new(), key: key.clone() },
            })
            .collect();
        let mut primary = self.lock()?;
        let (batch, _) = primary.commit(changes);
        primary.retain_at_most(self.retain);
        drop(primary);
        self.more.notify_all();
        Ok(batch.sequence)
    }
}

/// Applies the batches a replica receives to its database, one write transaction per batch.
#[derive(Debug, Clone, Copy)]
pub struct DatabaseApply<'d>(pub &'d Database);

impl Apply for DatabaseApply<'_> {
    fn apply(&mut self, batch: &Batch) -> std::result::Result<(), String> {
        self.0.apply_batch(batch).map_err(|error| error.to_string())
    }
}

impl Database {
    /// Starts capturing every commit of this database as a replication batch for replicas to
    /// follow (D1024), continuing the term and sequence its last replicated commit recorded.
    /// Commits pay for the capture only once this was called.
    pub fn start_primary(&self, mode: Mode) -> Result<std::sync::Arc<ReplicationPrimary>> {
        let (term, sequence) = self.replication_position()?.unwrap_or((1, 0));
        let primary = std::sync::Arc::new(ReplicationPrimary {
            state: Mutex::new(Primary::new(term.max(1), sequence, mode)),
            more: Condvar::new(),
            acknowledged: Condvar::new(),
            retain: DEFAULT_RETAINED_BATCHES,
        });
        self.set_primary(std::sync::Arc::clone(&primary))?;
        Ok(primary)
    }

    /// The replication term and sequence of the last batch this database made as a primary or
    /// applied as a replica, kept in the same commit as the batch; `None` before the first.
    pub fn replication_position(&self) -> Result<Option<(u64, u64)>> {
        let read = self.begin_read()?;
        Ok(read.get_raw(POSITION_KEY)?.as_deref().and_then(decode_position))
    }

    /// Applies a batch a primary shipped, all of its changes in one write transaction, as a
    /// replica does. Changes outside the default key space are refused.
    pub fn apply_batch(&self, batch: &Batch) -> Result<()> {
        let mut transaction = self.begin_write()?;
        transaction.note_expiring_keys();
        for change in &batch.changes {
            match change {
                Change::Put { collection, key, value } if collection.is_empty() => transaction.put_raw(key, value)?,
                Change::Delete { collection, key } if collection.is_empty() => {
                    transaction.delete_raw(key)?;
                }
                _ => return Err(replication_error("a replicated change names a collection; EggDB ships raw keys")),
            }
        }
        transaction.put_raw(POSITION_KEY, &encode_position(batch.term, batch.sequence))?;
        transaction.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_replication::Replica;

    #[test]
    fn a_replica_follows_the_primary_s_commits() {
        let primary_db = Database::in_memory().unwrap();
        let primary = primary_db.start_primary(Mode::Asynchronous).unwrap();
        primary_db.put("a", "1").unwrap();
        primary_db.put("b", "2").unwrap();
        primary_db.delete("a").unwrap();
        let replica_db = Database::in_memory().unwrap();
        let mut replica = Replica::new(7, 0, 0, 0, DatabaseApply(&replica_db), 16);
        for batch in primary.batches_after(0, 100).unwrap().unwrap() {
            let applied = replica.receive(&batch).unwrap();
            primary.acknowledge(7, applied).unwrap();
        }
        assert_eq!(replica_db.get("a").unwrap(), None);
        assert_eq!(replica_db.get("b").unwrap(), Some(b"2".to_vec()));
        assert_eq!(replica_db.replication_position().unwrap(), Some((1, 3)));
        assert_eq!(primary_db.replication_position().unwrap(), Some((1, 3)));
    }
}
