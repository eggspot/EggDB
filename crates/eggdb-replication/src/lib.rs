//! EggDB's replication protocol (D209). A primary numbers each commit's key-value changes as a
//! [`Batch`] in its current term and ships batches to replicas, which apply them strictly in
//! order and acknowledge what they applied. In synchronous mode a commit is reported durable
//! only once enough replicas acknowledged it; in asynchronous mode at once. A replica can feed
//! another replica (cascading). When the primary is lost, [`elect`] picks the replica holding
//! the most, which starts a new term; batches of an older term are refused afterwards. A
//! [`ReadToken`] lets a client read from any replica that has applied its own writes.
//!
//! The protocol is independent of storage: the engine supplies the changes of each commit and
//! applies received ones through [`Apply`].

mod wire;

pub use wire::{decode_batch, encode_batch};

use std::collections::{BTreeMap, VecDeque};

/// One change to a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Store `value` under `key` in `collection` (empty for the default key space).
    Put {
        /// The collection.
        collection: String,
        /// The key.
        key: Vec<u8>,
        /// The value.
        value: Vec<u8>,
    },
    /// Remove `key` from `collection`.
    Delete {
        /// The collection.
        collection: String,
        /// The key.
        key: Vec<u8>,
    },
}

/// The changes of one commit, numbered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    /// The term of the primary that made it.
    pub term: u64,
    /// Its place in the history, from 1.
    pub sequence: u64,
    /// The changes, in the order the commit made them.
    pub changes: Vec<Change>,
}

/// How commits wait for replicas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A commit is durable once this many replicas have acknowledged it.
    Synchronous {
        /// The acknowledgements needed.
        acknowledgements: usize,
    },
    /// A commit is durable on the primary alone; replicas follow when they can.
    Asynchronous,
}

/// A replica's identity.
pub type ReplicaId = u32;

/// What a client keeps after a write so a replica serves it only after applying that write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReadToken {
    /// The sequence of the write.
    pub sequence: u64,
}

/// Applies received changes to storage, all of a batch in one transaction.
pub trait Apply {
    /// Applies `batch`; a failure applies nothing of it.
    fn apply(&mut self, batch: &Batch) -> Result<(), String>;
}

/// Why a replica refused a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The batch comes from a primary of an older term.
    StaleTerm {
        /// The term the replica follows.
        current: u64,
    },
    /// Batches before it are missing: send from `next`.
    Gap {
        /// The sequence the replica needs next.
        next: u64,
    },
    /// Applying it failed.
    Failed(String),
}

/// The primary's side.
#[derive(Debug, Clone)]
pub struct Primary {
    term: u64,
    last: u64,
    mode: Mode,
    retained: VecDeque<Batch>,
    acknowledged: BTreeMap<ReplicaId, u64>,
}

impl Primary {
    /// A primary for `term` whose history ends at `last` (0 for a new database).
    pub fn new(term: u64, last: u64, mode: Mode) -> Self {
        Self { term, last, mode, retained: VecDeque::new(), acknowledged: BTreeMap::new() }
    }

    /// The term.
    pub fn term(&self) -> u64 {
        self.term
    }

    /// Records a commit's changes; returns its batch and the token to read it back with.
    pub fn commit(&mut self, changes: Vec<Change>) -> (Batch, ReadToken) {
        self.last += 1;
        let batch = Batch { term: self.term, sequence: self.last, changes };
        self.retained.push_back(batch.clone());
        (batch, ReadToken { sequence: self.last })
    }

    /// The batches a replica that has applied up to `applied` still needs, at most `limit`.
    pub fn batches_after(&self, applied: u64, limit: usize) -> Result<Vec<Batch>, u64> {
        match self.retained.front() {
            Some(first) if first.sequence > applied + 1 => Err(first.sequence),
            _ => Ok(self.retained.iter().filter(|batch| batch.sequence > applied).take(limit).cloned().collect()),
        }
    }

    /// Starts shipping to `replica`, which has applied up to `applied`; batches it still needs
    /// are kept until it acknowledges them.
    pub fn add_replica(&mut self, replica: ReplicaId, applied: u64) {
        self.acknowledged.entry(replica).or_insert(applied.min(self.last));
    }

    /// Stops keeping batches for `replica`.
    pub fn remove_replica(&mut self, replica: ReplicaId) {
        self.acknowledged.remove(&replica);
    }

    /// Records that `replica` applied everything up to `sequence`, and forgets batches every
    /// known replica has.
    pub fn acknowledge(&mut self, replica: ReplicaId, sequence: u64) {
        let entry = self.acknowledged.entry(replica).or_insert(0);
        *entry = (*entry).max(sequence.min(self.last));
        let everyone = self.acknowledged.values().copied().min().unwrap_or(0);
        while self.retained.front().is_some_and(|batch| batch.sequence <= everyone) {
            self.retained.pop_front();
        }
    }

    /// Whether the commit at `sequence` is durable under the mode.
    pub fn is_durable(&self, sequence: u64) -> bool {
        match self.mode {
            Mode::Asynchronous => sequence <= self.last,
            Mode::Synchronous { acknowledgements } => {
                self.acknowledged.values().filter(|applied| **applied >= sequence).count() >= acknowledgements
            }
        }
    }
}

/// A replica's side.
#[derive(Debug, Clone)]
pub struct Replica<A: Apply> {
    id: ReplicaId,
    term: u64,
    applied: u64,
    storage: A,
    retained: VecDeque<Batch>,
    retain: usize,
}

impl<A: Apply> Replica<A> {
    /// A replica that has applied up to `applied` in `term`, keeping its last `retain` batches
    /// to feed cascading replicas.
    pub fn new(id: ReplicaId, term: u64, applied: u64, storage: A, retain: usize) -> Self {
        Self { id, term, applied, storage, retained: VecDeque::new(), retain }
    }

    /// Its id.
    pub fn id(&self) -> ReplicaId {
        self.id
    }

    /// The last sequence applied.
    pub fn applied(&self) -> u64 {
        self.applied
    }

    /// The term followed.
    pub fn term(&self) -> u64 {
        self.term
    }

    /// The storage.
    pub fn storage(&self) -> &A {
        &self.storage
    }

    /// Applies `batch` when it is the next one of the current or a newer term.
    pub fn receive(&mut self, batch: &Batch) -> Result<u64, Refusal> {
        if batch.term < self.term {
            return Err(Refusal::StaleTerm { current: self.term });
        }
        if batch.sequence <= self.applied {
            return Ok(self.applied);
        }
        if batch.sequence != self.applied + 1 {
            return Err(Refusal::Gap { next: self.applied + 1 });
        }
        self.storage.apply(batch).map_err(Refusal::Failed)?;
        self.term = batch.term;
        self.applied = batch.sequence;
        self.retained.push_back(batch.clone());
        while self.retained.len() > self.retain {
            self.retained.pop_front();
        }
        Ok(self.applied)
    }

    /// Joins a new term announced by an elected primary, refusing older primaries after.
    pub fn follow_term(&mut self, term: u64) {
        self.term = self.term.max(term);
    }

    /// The batches a cascading replica that applied up to `applied` needs from this one.
    pub fn batches_after(&self, applied: u64) -> Result<Vec<Batch>, u64> {
        match self.retained.front() {
            Some(first) if first.sequence > applied + 1 => Err(first.sequence),
            None if applied < self.applied => Err(self.applied),
            _ => Ok(self.retained.iter().filter(|batch| batch.sequence > applied).cloned().collect()),
        }
    }

    /// Whether this replica may answer a read that needs `token`.
    pub fn can_serve(&self, token: ReadToken) -> bool {
        self.applied >= token.sequence
    }

    /// Turns this replica into the primary of `term`.
    pub fn promote(self, term: u64, mode: Mode) -> (Primary, A) {
        (Primary::new(term.max(self.term + 1), self.applied, mode), self.storage)
    }
}

/// What failover knows of a reachable replica.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    /// The replica.
    pub id: ReplicaId,
    /// Its term.
    pub term: u64,
    /// Its last applied sequence.
    pub applied: u64,
}

/// The replica to promote: the one with the newest term and then the most applied (the lower id
/// on a tie, so every node picks the same one).
pub fn elect(candidates: &[Candidate]) -> Option<ReplicaId> {
    candidates.iter().max_by(|a, b| (a.term, a.applied, std::cmp::Reverse(a.id)).cmp(&(b.term, b.applied, std::cmp::Reverse(b.id)))).map(|candidate| candidate.id)
}

#[cfg(test)]
mod tests;
