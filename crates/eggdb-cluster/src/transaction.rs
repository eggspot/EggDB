//! Transactions across shards with two-phase commit and strict two-phase locking: a
//! [`Participant`] per shard locks every key a transaction reads or writes until the decision,
//! votes in [`Participant::prepare`], and applies or discards on the decision. The
//! [`Coordinator`] records each decision before sending it, so a participant left prepared by a
//! coordinator crash learns the outcome from its log, and a transaction with no recorded
//! decision is aborted (presumed abort). Holding every lock until the decision makes the
//! transactions serializable; a lock held by another transaction makes the newcomer abort and
//! retry, so there is no deadlock.

use crate::ShardId;
use std::collections::{BTreeMap, BTreeSet};

/// A transaction's identity.
pub type TransactionId = u64;

/// A participant's vote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vote {
    /// Prepared: the writes are durable and the locks held.
    Yes,
    /// Refused: a key is locked by another transaction.
    No,
}

/// The outcome of a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Every participant applies.
    Commit,
    /// Every participant discards.
    Abort,
}

/// A key is held by another transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conflict {
    /// The transaction holding it.
    pub holder: TransactionId,
}

/// One shard's side of two-phase commit.
#[derive(Debug, Clone, Default)]
pub struct Participant {
    data: BTreeMap<Vec<u8>, Vec<u8>>,
    locks: BTreeMap<Vec<u8>, TransactionId>,
    prepared: BTreeMap<TransactionId, Vec<(Vec<u8>, Option<Vec<u8>>)>>,
}

impl Participant {
    /// The committed value of `key`, locking it for `transaction`.
    pub fn read(&mut self, transaction: TransactionId, key: &[u8]) -> Result<Option<Vec<u8>>, Conflict> {
        self.lock(transaction, key)?;
        Ok(self.data.get(key).cloned())
    }

    /// The committed value of `key`, without a transaction.
    pub fn committed(&self, key: &[u8]) -> Option<&Vec<u8>> {
        self.data.get(key)
    }

    fn lock(&mut self, transaction: TransactionId, key: &[u8]) -> Result<(), Conflict> {
        match self.locks.get(key) {
            Some(holder) if *holder != transaction => Err(Conflict { holder: *holder }),
            _ => {
                self.locks.insert(key.to_vec(), transaction);
                Ok(())
            }
        }
    }

    /// Prepares `writes` (a `None` value deletes): locks every key and keeps the writes until
    /// the decision.
    pub fn prepare(&mut self, transaction: TransactionId, writes: Vec<(Vec<u8>, Option<Vec<u8>>)>) -> Vote {
        if writes.iter().any(|(key, _)| self.locks.get(key).is_some_and(|holder| *holder != transaction)) {
            self.release(transaction);
            return Vote::No;
        }
        for (key, _) in &writes {
            self.locks.insert(key.clone(), transaction);
        }
        self.prepared.entry(transaction).or_default().extend(writes);
        Vote::Yes
    }

    fn release(&mut self, transaction: TransactionId) {
        self.locks.retain(|_, holder| *holder != transaction);
    }

    /// Applies or discards `transaction` and releases its locks; repeating it does nothing.
    pub fn decide(&mut self, transaction: TransactionId, decision: Decision) {
        if let Some(writes) = self.prepared.remove(&transaction)
            && decision == Decision::Commit
        {
            for (key, value) in writes {
                match value {
                    Some(value) => {
                        self.data.insert(key, value);
                    }
                    None => {
                        self.data.remove(&key);
                    }
                }
            }
        }
        self.release(transaction);
    }

    /// The transactions prepared here and not yet decided, for recovery.
    pub fn in_doubt(&self) -> Vec<TransactionId> {
        self.prepared.keys().copied().collect()
    }
}

/// The coordinator's durable record of decisions.
#[derive(Debug, Clone, Default)]
pub struct Coordinator {
    decisions: BTreeMap<TransactionId, Decision>,
    next: TransactionId,
}

impl Coordinator {
    /// A new transaction id.
    pub fn begin(&mut self) -> TransactionId {
        self.next += 1;
        self.next
    }

    /// Records the decision from the votes, before anyone is told: commit only if every
    /// participant voted yes.
    pub fn decide(&mut self, transaction: TransactionId, votes: &[Vote]) -> Decision {
        let decision = if votes.iter().all(|vote| *vote == Vote::Yes) { Decision::Commit } else { Decision::Abort };
        self.decisions.insert(transaction, decision);
        decision
    }

    /// The outcome of `transaction` for a participant in doubt: the recorded decision, or abort
    /// when none was recorded.
    pub fn outcome(&self, transaction: TransactionId) -> Decision {
        self.decisions.get(&transaction).copied().unwrap_or(Decision::Abort)
    }
}

/// Settles every in-doubt transaction of `participants` from the coordinator's record, as a
/// restarted participant or coordinator does.
pub fn recover(coordinator: &Coordinator, participants: &mut BTreeMap<ShardId, Participant>) {
    for participant in participants.values_mut() {
        let doubts: BTreeSet<TransactionId> = participant.in_doubt().into_iter().collect();
        for transaction in doubts {
            participant.decide(transaction, coordinator.outcome(transaction));
        }
    }
}
