//! A cluster's voting members, and the joint configuration of Raft's section 6 that moves
//! from one set of voters to another without two majorities that do not overlap.

use crate::NodeId;
use std::collections::BTreeSet;

/// The voters whose majority decides elections and commitment. During a change it is a
/// joint configuration: a majority of `voters` and a majority of `outgoing` are both needed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Membership {
    /// The voters of the new configuration, sorted.
    pub voters: Vec<NodeId>,
    /// The voters of the old configuration while a change is in progress, sorted; empty
    /// otherwise.
    pub outgoing: Vec<NodeId>,
}

fn sorted(ids: &[NodeId]) -> Vec<NodeId> {
    let set: BTreeSet<NodeId> = ids.iter().copied().collect();
    set.into_iter().collect()
}

fn majority_of(group: &[NodeId], granted: &BTreeSet<NodeId>) -> bool {
    group.iter().filter(|member| granted.contains(member)).count() > group.len() / 2
}

impl Membership {
    /// A configuration of these voters, outside any change.
    pub fn new(voters: &[NodeId]) -> Self {
        Self {
            voters: sorted(voters),
            outgoing: Vec::new(),
        }
    }

    /// The joint configuration that moves from `from` to the voters `to`.
    pub fn joint(from: &Self, to: &[NodeId]) -> Self {
        Self {
            voters: sorted(to),
            outgoing: from.voters.clone(),
        }
    }

    /// Whether a change is in progress.
    pub fn is_joint(&self) -> bool {
        !self.outgoing.is_empty()
    }

    /// Whether `id` votes in either half.
    pub fn contains(&self, id: NodeId) -> bool {
        self.voters.contains(&id) || self.outgoing.contains(&id)
    }

    /// Every voter of either half.
    pub fn members(&self) -> BTreeSet<NodeId> {
        self.voters
            .iter()
            .chain(&self.outgoing)
            .copied()
            .collect()
    }

    /// Whether `granted` holds a majority of the voters, and of the outgoing voters during a
    /// change.
    pub fn has_quorum(&self, granted: &BTreeSet<NodeId>) -> bool {
        majority_of(&self.voters, granted)
            && (self.outgoing.is_empty() || majority_of(&self.outgoing, granted))
    }

    /// The largest value a quorum has reached, given each member's value.
    pub(crate) fn quorum_value(&self, value_of: impl Fn(NodeId) -> u64) -> u64 {
        let reached = |group: &[NodeId]| {
            if group.is_empty() {
                return u64::MAX;
            }
            let mut values: Vec<u64> = group.iter().map(|member| value_of(*member)).collect();
            values.sort_unstable_by(|a, b| b.cmp(a));
            values[group.len() / 2]
        };
        if self.voters.is_empty() {
            return 0;
        }
        reached(&self.voters).min(reached(&self.outgoing))
    }

    /// The configuration as a log entry carries it: the voter count (4 bytes, big-endian) and
    /// the voters (8 bytes each), then the same for the outgoing voters.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(8 + 8 * (self.voters.len() + self.outgoing.len()));
        for group in [&self.voters, &self.outgoing] {
            bytes.extend_from_slice(&(group.len() as u32).to_be_bytes());
            for id in group {
                bytes.extend_from_slice(&id.to_be_bytes());
            }
        }
        bytes
    }

    /// Reads [`Self::encode`]'s bytes; `None` when they do not have its layout.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut rest = bytes;
        let mut group = || -> Option<Vec<NodeId>> {
            let (count, tail) = rest.split_first_chunk::<4>()?;
            let count = usize::try_from(u32::from_be_bytes(*count)).ok()?;
            if count > tail.len() / 8 {
                return None;
            }
            let (ids, tail) = tail.split_at(count * 8);
            rest = tail;
            Some(
                ids.as_chunks::<8>()
                    .0
                    .iter()
                    .map(|id| NodeId::from_be_bytes(*id))
                    .collect(),
            )
        };
        let voters = group()?;
        let outgoing = group()?;
        if !rest.is_empty() {
            return None;
        }
        Some(Self {
            voters: sorted(&voters),
            outgoing: sorted(&outgoing),
        })
    }
}
