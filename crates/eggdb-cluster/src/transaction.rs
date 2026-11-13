//! Transactions across shards with two-phase commit and strict two-phase locking: a
//! [`Participant`] per shard locks every key a transaction reads or writes until the decision,
//! votes in [`Participant::prepare`], and applies or discards on the decision. The
//! [`Coordinator`] records each decision before sending it, so a participant left prepared by a
//! coordinator crash learns the outcome from its log, and a transaction with no recorded
//! decision is aborted (presumed abort). Holding every lock until the decision makes the
//! transactions serializable; a lock held by another transaction makes the newcomer abort and
//! retry, so there is no deadlock.
//!
//! Every committed write is kept as a version stamped with its transaction's commit
//! [`Timestamp`] (D1042): a participant prepares at a timestamp from its hybrid clock, the
//! coordinator commits at the largest prepare timestamp, and [`Participant::read_at`] reads a
//! consistent snapshot of any shard at any timestamp, waiting only for a transaction prepared
//! at or before it whose decision is not known yet. [`snapshot_read`] reads keys of several
//! shards at one timestamp, so a read across shards sees every distributed transaction whole
//! or not at all.

use crate::ShardId;
use crate::clock::Timestamp;
use std::collections::{BTreeMap, BTreeSet};

/// A transaction's identity.
pub type TransactionId = u64;

type Writes = Vec<(Vec<u8>, Option<Vec<u8>>)>;

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
    versions: BTreeMap<Vec<u8>, Vec<(Timestamp, Option<Vec<u8>>)>>,
    locks: BTreeMap<Vec<u8>, TransactionId>,
    prepared: BTreeMap<TransactionId, (Timestamp, Writes)>,
    last_commit: Timestamp,
    read_floor: Timestamp,
}

/// What a read at a timestamp found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadAt {
    /// The value as of the timestamp (`None` when the key did not exist then).
    Value(Option<Vec<u8>>),
    /// A transaction prepared at or before the timestamp writes the key and is not decided
    /// yet; the read waits for its decision.
    Wait(TransactionId),
}

impl Participant {
    /// The committed value of `key`, locking it for `transaction`.
    pub fn read(
        &mut self,
        transaction: TransactionId,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, Conflict> {
        self.lock(transaction, key)?;
        Ok(self.committed(key).cloned())
    }

    /// The latest committed value of `key`, without a transaction.
    pub fn committed(&self, key: &[u8]) -> Option<&Vec<u8>> {
        self.versions
            .get(key)
            .and_then(|versions| versions.last())
            .and_then(|(_, value)| value.as_ref())
    }

    /// The value of `key` as of `at`: the newest version committed at or before it. A
    /// transaction prepared at or before `at` that writes the key may still commit at a
    /// timestamp at or before `at`, so the read waits for its decision instead. The read
    /// raises this shard's floor to `at`, so nothing prepares here at or before it afterwards
    /// and the answer never changes.
    pub fn read_at(&mut self, key: &[u8], at: Timestamp) -> ReadAt {
        self.read_floor = self.read_floor.max(at);
        if let Some((transaction, _)) = self.prepared.iter().find(|(_, (prepared_at, writes))| {
            *prepared_at <= at && writes.iter().any(|(written, _)| written.as_slice() == key)
        }) {
            return ReadAt::Wait(*transaction);
        }
        ReadAt::Value(
            self.versions
                .get(key)
                .and_then(|versions| versions.iter().rev().find(|(stamp, _)| *stamp <= at))
                .and_then(|(_, value)| value.clone()),
        )
    }

    /// The timestamp of the last commit applied here.
    pub fn last_commit(&self) -> Timestamp {
        self.last_commit
    }

    /// Drops versions no snapshot at or after `oldest` can read: for each key, every version
    /// older than the newest one at or before `oldest`.
    pub fn collect_garbage(&mut self, oldest: Timestamp) {
        for versions in self.versions.values_mut() {
            let keep_from = versions
                .iter()
                .rposition(|(stamp, _)| *stamp <= oldest)
                .unwrap_or(0);
            versions.drain(..keep_from);
        }
        self.versions.retain(|_, versions| {
            !(versions.len() == 1 && versions[0].1.is_none() && versions[0].0 <= oldest)
        });
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
    pub fn prepare(
        &mut self,
        transaction: TransactionId,
        writes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    ) -> Vote {
        let at = self.last_commit.next();
        self.prepare_at(transaction, writes, at)
    }

    /// Prepares `writes` at the timestamp `at` from this shard's clock, as [`Self::prepare`]
    /// does; the transaction will commit at `at` or later.
    pub fn prepare_at(
        &mut self,
        transaction: TransactionId,
        writes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        at: Timestamp,
    ) -> Vote {
        let at = at.max(self.read_floor.next());
        if writes.iter().any(|(key, _)| {
            self.locks
                .get(key)
                .is_some_and(|holder| *holder != transaction)
        }) {
            self.release(transaction);
            return Vote::No;
        }
        for (key, _) in &writes {
            self.locks.insert(key.clone(), transaction);
        }
        let entry = self
            .prepared
            .entry(transaction)
            .or_insert_with(|| (at, Vec::new()));
        entry.0 = entry.0.max(at);
        entry.1.extend(writes);
        Vote::Yes
    }

    /// The timestamp `transaction` was prepared at here, if it is prepared.
    pub fn prepared_at(&self, transaction: TransactionId) -> Option<Timestamp> {
        self.prepared.get(&transaction).map(|(at, _)| *at)
    }

    fn release(&mut self, transaction: TransactionId) {
        self.locks.retain(|_, holder| *holder != transaction);
    }

    /// Applies or discards `transaction` and releases its locks; repeating it does nothing.
    /// A commit is stamped after this shard's last commit and the transaction's prepare.
    pub fn decide(&mut self, transaction: TransactionId, decision: Decision) {
        let at = self
            .prepared_at(transaction)
            .unwrap_or_default()
            .max(self.last_commit.next());
        self.decide_at(transaction, decision, at);
    }

    /// Applies `transaction` with the commit timestamp `at` the coordinator chose (at least
    /// its prepare timestamp on every shard), or discards it, and releases its locks.
    pub fn decide_at(&mut self, transaction: TransactionId, decision: Decision, at: Timestamp) {
        if let Some((prepared_at, writes)) = self.prepared.remove(&transaction)
            && decision == Decision::Commit
        {
            let at = at.max(prepared_at);
            for (key, value) in writes {
                let versions = self.versions.entry(key).or_default();
                let position = versions.partition_point(|(stamp, _)| *stamp <= at);
                versions.insert(position, (at, value));
            }
            self.last_commit = self.last_commit.max(at);
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

    /// Records the decision from the votes, before anyone is told: commit only if there are
    /// votes and every participant voted yes. A decision already recorded, including an abort
    /// presumed for a participant in doubt, stands and is returned instead.
    pub fn decide(&mut self, transaction: TransactionId, votes: &[Vote]) -> Decision {
        *self
            .decisions
            .entry(transaction)
            .or_insert(decision_from(votes))
    }

    /// The outcome of `transaction` for a participant in doubt: the recorded decision, or an
    /// abort, recorded now so that a later [`Coordinator::decide`] cannot commit it.
    pub fn outcome(&mut self, transaction: TransactionId) -> Decision {
        *self.decisions.entry(transaction).or_insert(Decision::Abort)
    }
}

/// Where a coordinator records its decisions before telling anyone: in memory for tests
/// ([`Coordinator`]), or durably in the cluster's replicated database.
pub trait DecisionLog {
    /// A new transaction id, never handed out before.
    fn begin(&mut self) -> Result<TransactionId, String>;

    /// Records and returns the decision from the votes, as [`Coordinator::decide`] does: a
    /// decision already recorded stands.
    fn decide(&mut self, transaction: TransactionId, votes: &[Vote]) -> Result<Decision, String>;

    /// The recorded decision, or an abort recorded now, as [`Coordinator::outcome`] does.
    fn outcome(&mut self, transaction: TransactionId) -> Result<Decision, String>;
}

impl DecisionLog for Coordinator {
    fn begin(&mut self) -> Result<TransactionId, String> {
        Ok(Coordinator::begin(self))
    }

    fn decide(&mut self, transaction: TransactionId, votes: &[Vote]) -> Result<Decision, String> {
        Ok(Coordinator::decide(self, transaction, votes))
    }

    fn outcome(&mut self, transaction: TransactionId) -> Result<Decision, String> {
        Ok(Coordinator::outcome(self, transaction))
    }
}

/// The decision a fresh vote count gives: commit only when there are votes and all are yes.
pub fn decision_from(votes: &[Vote]) -> Decision {
    if !votes.is_empty() && votes.iter().all(|vote| *vote == Vote::Yes) {
        Decision::Commit
    } else {
        Decision::Abort
    }
}

/// Settles every in-doubt transaction of `participants` from `log`, as [`recover`] does with
/// an in-memory coordinator.
pub fn recover_from(
    log: &mut impl DecisionLog,
    participants: &mut BTreeMap<ShardId, Participant>,
) -> Result<(), String> {
    for participant in participants.values_mut() {
        let doubts: BTreeSet<TransactionId> = participant.in_doubt().into_iter().collect();
        for transaction in doubts {
            participant.decide(transaction, log.outcome(transaction)?);
        }
    }
    Ok(())
}

/// Settles every in-doubt transaction of `participants` from the coordinator's record, as a
/// restarted participant or coordinator does.
pub fn recover(coordinator: &mut Coordinator, participants: &mut BTreeMap<ShardId, Participant>) {
    for participant in participants.values_mut() {
        let doubts: BTreeSet<TransactionId> = participant.in_doubt().into_iter().collect();
        for transaction in doubts {
            participant.decide(transaction, coordinator.outcome(transaction));
        }
    }
}

/// The commit timestamp of a transaction prepared at these timestamps: the largest, so the
/// commit is after every prepare and after every read its locks protected.
pub fn commit_timestamp(prepared: &[Timestamp]) -> Timestamp {
    prepared.iter().copied().max().unwrap_or_default()
}

/// Reads `keys` (with the shard holding each) at the one timestamp `at`, as one snapshot
/// across shards; the first transaction a read has to wait for when one is still undecided.
pub fn snapshot_read(
    participants: &mut BTreeMap<ShardId, Participant>,
    keys: &[(ShardId, Vec<u8>)],
    at: Timestamp,
) -> Result<Vec<Option<Vec<u8>>>, TransactionId> {
    keys.iter()
        .map(|(shard, key)| match participants.get_mut(shard).map(|participant| participant.read_at(key, at)) {
            Some(ReadAt::Value(value)) => Ok(value),
            Some(ReadAt::Wait(transaction)) => Err(transaction),
            None => Ok(None),
        })
        .collect()
}
