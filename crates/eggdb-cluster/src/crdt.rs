//! Conflict-free replicated data types (D225's resolution policies): a positive-negative counter
//! and an observed-remove set, as Shapiro, Preguiça, Baquero and Zawirski define them. Their
//! states merge by a join that is commutative, associative and idempotent, so replicas that have
//! seen the same writes in any order hold the same state.

use crate::NodeId;
use std::collections::{BTreeMap, BTreeSet};

/// Which data type a key's value is, when concurrent versions are merged instead of chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrdtKind {
    /// A counter that goes up and down: [`PnCounter`].
    Counter,
    /// A set whose removals only remove what they saw: [`OrSet`].
    Set,
}

/// A positive-negative counter: each node's increments and decrements, kept apart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PnCounter {
    added: BTreeMap<NodeId, u64>,
    taken: BTreeMap<NodeId, u64>,
}

impl PnCounter {
    /// Adds `amount` (negative to subtract) as `node`.
    pub fn change(&mut self, node: NodeId, amount: i64) {
        let side = if amount >= 0 {
            &mut self.added
        } else {
            &mut self.taken
        };
        let entry = side.entry(node).or_insert(0);
        *entry = entry.saturating_add(amount.unsigned_abs());
    }

    /// The counter's value.
    pub fn value(&self) -> i128 {
        let up: i128 = self.added.values().map(|count| i128::from(*count)).sum();
        let down: i128 = self.taken.values().map(|count| i128::from(*count)).sum();
        up - down
    }

    /// Takes in every change `other` has seen.
    pub fn merge(&mut self, other: &Self) {
        for (node, count) in &other.added {
            let mine = self.added.entry(*node).or_insert(0);
            *mine = (*mine).max(*count);
        }
        for (node, count) in &other.taken {
            let mine = self.taken.entry(*node).or_insert(0);
            *mine = (*mine).max(*count);
        }
    }

    /// The state as bytes: for each side, a count and then node and total pairs.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for side in [&self.added, &self.taken] {
            out.extend_from_slice(&(side.len() as u64).to_le_bytes());
            for (node, count) in side {
                out.extend_from_slice(&node.to_le_bytes());
                out.extend_from_slice(&count.to_le_bytes());
            }
        }
        out
    }

    /// Reads a state [`PnCounter::encode`] wrote; `None` for anything else.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut reader = Reader { bytes, at: 0 };
        let mut sides = [BTreeMap::new(), BTreeMap::new()];
        for side in &mut sides {
            let count = reader.u64()?;
            for _ in 0..count {
                let node = reader.u64()?;
                let total = reader.u64()?;
                side.insert(node, total);
            }
        }
        if reader.at != bytes.len() {
            return None;
        }
        let [added, taken] = sides;
        Some(Self { added, taken })
    }
}

/// A tag that makes one addition unique: the node that added and its count of additions.
pub type Tag = (NodeId, u64);

/// An observed-remove set: each element with the tags of its additions and of the removed ones,
/// so a removal takes away only the additions it saw, and a concurrent addition survives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrSet {
    added: BTreeMap<Vec<u8>, BTreeSet<Tag>>,
    removed: BTreeMap<Vec<u8>, BTreeSet<Tag>>,
    clock: BTreeMap<NodeId, u64>,
}

impl OrSet {
    /// Adds `element` as `node`.
    pub fn add(&mut self, node: NodeId, element: &[u8]) {
        let counter = self.clock.entry(node).or_insert(0);
        *counter += 1;
        self.added
            .entry(element.to_vec())
            .or_default()
            .insert((node, *counter));
    }

    /// Removes `element`: every addition of it this replica has seen.
    pub fn remove(&mut self, element: &[u8]) {
        if let Some(tags) = self.added.get(element) {
            self.removed
                .entry(element.to_vec())
                .or_default()
                .extend(tags.iter().copied());
        }
    }

    /// Whether `element` is in the set.
    pub fn contains(&self, element: &[u8]) -> bool {
        let removed = self.removed.get(element);
        self.added.get(element).is_some_and(|tags| {
            tags.iter()
                .any(|tag| removed.is_none_or(|removed| !removed.contains(tag)))
        })
    }

    /// The elements in the set, in byte order.
    pub fn elements(&self) -> Vec<Vec<u8>> {
        self.added
            .keys()
            .filter(|element| self.contains(element))
            .cloned()
            .collect()
    }

    /// Takes in every addition and removal `other` has seen.
    pub fn merge(&mut self, other: &Self) {
        for (element, tags) in &other.added {
            self.added
                .entry(element.clone())
                .or_default()
                .extend(tags.iter().copied());
        }
        for (element, tags) in &other.removed {
            self.removed
                .entry(element.clone())
                .or_default()
                .extend(tags.iter().copied());
        }
        for (node, counter) in &other.clock {
            let mine = self.clock.entry(*node).or_insert(0);
            *mine = (*mine).max(*counter);
        }
    }

    /// The state as bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for map in [&self.added, &self.removed] {
            out.extend_from_slice(&(map.len() as u64).to_le_bytes());
            for (element, tags) in map {
                out.extend_from_slice(&(element.len() as u64).to_le_bytes());
                out.extend_from_slice(element);
                out.extend_from_slice(&(tags.len() as u64).to_le_bytes());
                for (node, counter) in tags {
                    out.extend_from_slice(&node.to_le_bytes());
                    out.extend_from_slice(&counter.to_le_bytes());
                }
            }
        }
        out.extend_from_slice(&(self.clock.len() as u64).to_le_bytes());
        for (node, counter) in &self.clock {
            out.extend_from_slice(&node.to_le_bytes());
            out.extend_from_slice(&counter.to_le_bytes());
        }
        out
    }

    /// Reads a state [`OrSet::encode`] wrote; `None` for anything else.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut reader = Reader { bytes, at: 0 };
        let mut maps = [BTreeMap::new(), BTreeMap::new()];
        for map in &mut maps {
            let elements = reader.u64()?;
            for _ in 0..elements {
                let length = usize::try_from(reader.u64()?).ok()?;
                let element = reader.take(length)?.to_vec();
                let tags = reader.u64()?;
                let mut set = BTreeSet::new();
                for _ in 0..tags {
                    set.insert((reader.u64()?, reader.u64()?));
                }
                map.insert(element, set);
            }
        }
        let mut clock = BTreeMap::new();
        for _ in 0..reader.u64()? {
            clock.insert(reader.u64()?, reader.u64()?);
        }
        if reader.at != bytes.len() {
            return None;
        }
        let [added, removed] = maps;
        Some(Self {
            added,
            removed,
            clock,
        })
    }
}

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    fn take(&mut self, length: usize) -> Option<&'b [u8]> {
        let end = self.at.checked_add(length)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}

/// Merges two encoded states of `kind`; a state that doesn't read, or a deletion, counts as the
/// empty state, so the other side's writes are kept.
pub fn merged(kind: CrdtKind, left: Option<&[u8]>, right: Option<&[u8]>) -> Vec<u8> {
    match kind {
        CrdtKind::Counter => {
            let mut state = left.and_then(PnCounter::decode).unwrap_or_default();
            state.merge(&right.and_then(PnCounter::decode).unwrap_or_default());
            state.encode()
        }
        CrdtKind::Set => {
            let mut state = left.and_then(OrSet::decode).unwrap_or_default();
            state.merge(&right.and_then(OrSet::decode).unwrap_or_default());
            state.encode()
        }
    }
}
