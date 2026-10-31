//! Leaderless replication with quorums (D225): every node takes writes for any key (multi-primary),
//! each key lives on the first `n` nodes of its rendezvous-hash order, a write succeeds once `w`
//! of them hold it and a read asks `r`. Versions carry a version vector, so a replica keeps a new
//! version only when it is not already superseded, and concurrent versions are reconciled by
//! last writer wins on a hybrid-clock stamp, or kept side by side as siblings for the
//! application to merge. Reads repair the replicas they found behind; writes to an unreachable
//! node go to the next node with a hint and are handed off when it returns; anti-entropy brings
//! every replica of a key to the same versions.

use crate::NodeId;
use std::collections::{BTreeMap, BTreeSet};

/// A hybrid logical clock stamp: physical milliseconds, a counter for events in the same
/// millisecond, and the writing node, so every stamp is distinct and totally ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Stamp {
    /// Milliseconds since 1970 as the clock read them, never behind a stamp already seen.
    pub millis: u64,
    /// Events within the same millisecond.
    pub counter: u32,
    /// The node that made the stamp.
    pub node: NodeId,
}

/// For each node, how many of its writes a version has seen.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct VersionVector(BTreeMap<NodeId, u64>);

impl VersionVector {
    /// Counts one more write by `node`.
    pub fn bump(&mut self, node: NodeId) {
        *self.0.entry(node).or_insert(0) += 1;
    }

    /// Whether this vector has seen every write `other` has.
    pub fn descends(&self, other: &Self) -> bool {
        other.0.iter().all(|(node, count)| self.0.get(node).is_some_and(|mine| mine >= count))
    }

    /// Takes in every write `other` has seen.
    pub fn merge(&mut self, other: &Self) {
        for (node, count) in &other.0 {
            let mine = self.0.entry(*node).or_insert(0);
            *mine = (*mine).max(*count);
        }
    }
}

/// One version of a key: its vector, its stamp, and its value (`None` once deleted).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version {
    /// The writes this version has seen.
    pub vector: VersionVector,
    /// When and where it was written.
    pub stamp: Stamp,
    /// The value, or `None` for a deletion.
    pub value: Option<Vec<u8>>,
}

/// How concurrent versions of one key are reconciled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// The version with the highest stamp wins.
    LastWriterWins,
    /// Concurrent versions are kept as siblings; a write that read them all replaces them.
    KeepSiblings,
}

/// One node's copy of the keys it replicates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Replica {
    versions: BTreeMap<Vec<u8>, Vec<Version>>,
}

impl Replica {
    /// Takes in `incoming` for `key`; returns whether it changed what the replica holds.
    pub fn apply(&mut self, key: &[u8], incoming: &Version, resolution: Resolution) -> bool {
        let held = self.versions.entry(key.to_vec()).or_default();
        if held.iter().any(|version| version == incoming || (version.vector.descends(&incoming.vector) && version.vector != incoming.vector)) {
            return false;
        }
        if held.iter().any(|version| version.vector == incoming.vector && version.stamp >= incoming.stamp) {
            return false;
        }
        held.retain(|version| !incoming.vector.descends(&version.vector));
        held.push(incoming.clone());
        if resolution == Resolution::LastWriterWins && held.len() > 1 {
            let mut merged = VersionVector::default();
            for version in held.iter() {
                merged.merge(&version.vector);
            }
            let winner = held.iter().max_by_key(|version| version.stamp).cloned().unwrap_or_else(|| incoming.clone());
            *held = vec![Version { vector: merged, ..winner }];
        }
        held.sort_by_key(|version| version.stamp);
        true
    }

    /// The versions held for `key`, oldest stamp first.
    pub fn versions(&self, key: &[u8]) -> &[Version] {
        self.versions.get(key).map_or(&[], Vec::as_slice)
    }
}

/// Why a quorum operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuorumError {
    /// `n`, `r` or `w` does not fit the nodes: zero, or more than there are.
    BadConfiguration,
    /// The coordinator is not a node of the cluster or cannot be reached.
    UnknownCoordinator(NodeId),
    /// Fewer replicas answered than the quorum needs.
    NotEnoughReplicas {
        /// How many answered.
        answered: usize,
        /// How many were needed.
        needed: usize,
    },
}

/// What a read found: the values of the surviving versions (one, or siblings) and the context
/// to write back with, which supersedes them all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadResult {
    /// The values, `None` for a deletion, in stamp order.
    pub values: Vec<Option<Vec<u8>>>,
    /// The merged vector of everything read.
    pub context: VersionVector,
}

fn rendezvous(key: &[u8], node: NodeId) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.iter().chain(node.to_le_bytes().iter()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash ^ (hash >> 29)
}

struct Hint {
    owner: NodeId,
    key: Vec<u8>,
    version: Version,
}

/// A leaderless cluster: its replicas, which of them can be reached, and its quorum sizes.
pub struct Quorum {
    replicas: BTreeMap<NodeId, Replica>,
    unreachable: BTreeSet<NodeId>,
    clocks: BTreeMap<NodeId, Stamp>,
    hints: BTreeMap<NodeId, Vec<Hint>>,
    n: usize,
    r: usize,
    w: usize,
    resolution: Resolution,
}

impl std::fmt::Debug for Quorum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Quorum").field("nodes", &self.replicas.len()).field("n", &self.n).field("r", &self.r).field("w", &self.w).finish_non_exhaustive()
    }
}

impl Quorum {
    /// A cluster of `nodes` keeping each key on `n` of them, writes needing `w` and reads `r`.
    pub fn new(nodes: &[NodeId], n: usize, r: usize, w: usize, resolution: Resolution) -> Result<Self, QuorumError> {
        if n == 0 || r == 0 || w == 0 || n > nodes.len() || r > n || w > n {
            return Err(QuorumError::BadConfiguration);
        }
        Ok(Self {
            replicas: nodes.iter().map(|node| (*node, Replica::default())).collect(),
            unreachable: BTreeSet::new(),
            clocks: BTreeMap::new(),
            hints: BTreeMap::new(),
            n,
            r,
            w,
            resolution,
        })
    }

    /// Marks `node` reachable or not, as a partition or a crash would.
    pub fn set_reachable(&mut self, node: NodeId, reachable: bool) {
        if reachable {
            self.unreachable.remove(&node);
        } else {
            self.unreachable.insert(node);
        }
    }

    fn reachable(&self, node: NodeId) -> bool {
        self.replicas.contains_key(&node) && !self.unreachable.contains(&node)
    }

    /// Every node in `key`'s order: its first `n` are the key's home replicas.
    pub fn order(&self, key: &[u8]) -> Vec<NodeId> {
        let mut nodes: Vec<NodeId> = self.replicas.keys().copied().collect();
        nodes.sort_by_key(|node| std::cmp::Reverse(rendezvous(key, *node)));
        nodes
    }

    /// The replica held by `node`.
    pub fn replica(&self, node: NodeId) -> Option<&Replica> {
        self.replicas.get(&node)
    }

    fn tick(&mut self, node: NodeId, now_ms: u64) -> Stamp {
        let last = self.clocks.get(&node).copied().unwrap_or_default();
        let stamp = if now_ms > last.millis { Stamp { millis: now_ms, counter: 0, node } } else { Stamp { millis: last.millis, counter: last.counter.saturating_add(1), node } };
        self.clocks.insert(node, stamp);
        stamp
    }

    fn observe(&mut self, node: NodeId, stamp: Stamp) {
        let clock = self.clocks.entry(node).or_default();
        if stamp.millis > clock.millis || (stamp.millis == clock.millis && stamp.counter > clock.counter) {
            *clock = Stamp { node, ..stamp };
        }
    }

    /// Writes `value` (`None` deletes) to `key` through `coordinator`, superseding what
    /// `context` read; succeeds once `w` replicas hold it, using fallback nodes with hints for
    /// unreachable home replicas.
    pub fn put(&mut self, coordinator: NodeId, key: &[u8], value: Option<Vec<u8>>, context: &VersionVector, now_ms: u64) -> Result<Version, QuorumError> {
        if !self.reachable(coordinator) {
            return Err(QuorumError::UnknownCoordinator(coordinator));
        }
        let stamp = self.tick(coordinator, now_ms);
        let mut vector = context.clone();
        vector.bump(coordinator);
        let version = Version { vector, stamp, value };
        let order = self.order(key);
        let homes: Vec<NodeId> = order.iter().take(self.n).copied().collect();
        let spares: Vec<NodeId> = order.iter().skip(self.n).copied().filter(|node| self.reachable(*node)).collect();
        let mut fallbacks = spares.into_iter();
        let mut stored = 0;
        for home in &homes {
            if self.reachable(*home) {
                self.store(*home, key, &version);
                stored += 1;
            } else if let Some(fallback) = fallbacks.next() {
                self.store(fallback, key, &version);
                self.hints.entry(fallback).or_default().push(Hint { owner: *home, key: key.to_vec(), version: version.clone() });
                stored += 1;
            }
        }
        if stored < self.w {
            return Err(QuorumError::NotEnoughReplicas { answered: stored, needed: self.w });
        }
        Ok(version)
    }

    fn store(&mut self, node: NodeId, key: &[u8], version: &Version) {
        let resolution = self.resolution;
        if let Some(replica) = self.replicas.get_mut(&node) {
            replica.apply(key, version, resolution);
        }
        self.observe(node, version.stamp);
    }

    /// Reads `key` from `r` reachable home replicas, reconciles what they hold, and repairs the
    /// ones that were behind.
    pub fn get(&mut self, key: &[u8]) -> Result<ReadResult, QuorumError> {
        let asked: Vec<NodeId> = self.order(key).into_iter().take(self.n).filter(|node| self.reachable(*node)).take(self.r).collect();
        if asked.len() < self.r {
            return Err(QuorumError::NotEnoughReplicas { answered: asked.len(), needed: self.r });
        }
        let mut merged = Replica::default();
        for node in &asked {
            let found: Vec<Version> = self.replicas[node].versions(key).to_vec();
            for version in &found {
                merged.apply(key, version, self.resolution);
            }
        }
        let reconciled: Vec<Version> = merged.versions(key).to_vec();
        for node in &asked {
            for version in &reconciled {
                self.store(*node, key, version);
            }
        }
        let mut context = VersionVector::default();
        for version in &reconciled {
            context.merge(&version.vector);
        }
        Ok(ReadResult { values: reconciled.into_iter().map(|version| version.value).collect(), context })
    }

    /// Hands the versions kept for unreachable home replicas to those now reachable.
    pub fn hand_off(&mut self) {
        let holders: Vec<NodeId> = self.hints.keys().copied().collect();
        for holder in holders {
            if !self.reachable(holder) {
                continue;
            }
            let hints = self.hints.remove(&holder).unwrap_or_default();
            let mut kept = Vec::new();
            for hint in hints {
                if self.reachable(hint.owner) {
                    self.store(hint.owner, &hint.key, &hint.version);
                } else {
                    kept.push(hint);
                }
            }
            if !kept.is_empty() {
                self.hints.insert(holder, kept);
            }
        }
    }

    /// Brings every reachable home replica of every key to the same versions.
    pub fn anti_entropy(&mut self) {
        let keys: BTreeSet<Vec<u8>> = self.replicas.values().flat_map(|replica| replica.versions.keys().cloned()).collect();
        for key in keys {
            let homes: Vec<NodeId> = self.order(&key).into_iter().take(self.n).filter(|node| self.reachable(*node)).collect();
            let mut merged = Replica::default();
            for node in &homes {
                let found: Vec<Version> = self.replicas[node].versions(&key).to_vec();
                for version in &found {
                    merged.apply(&key, version, self.resolution);
                }
            }
            let reconciled: Vec<Version> = merged.versions(&key).to_vec();
            for node in &homes {
                for version in &reconciled {
                    self.store(*node, &key, version);
                }
            }
        }
    }
}
