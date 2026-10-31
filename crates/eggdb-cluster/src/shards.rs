use crate::{NodeId, ShardId};
use std::collections::BTreeMap;

/// How keys are split into shards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Strategy {
    /// By a hash of the key, into this many equal hash ranges.
    Hash {
        /// The number of shards.
        shards: u32,
    },
    /// By key: shard `n` holds keys from `bounds[n - 1]` (inclusive) to `bounds[n]`.
    Range {
        /// The split points, ascending.
        bounds: Vec<Vec<u8>>,
    },
    /// By time: the key starts with a big-endian millisecond time, bucketed by `bucket_ms`.
    Time {
        /// The bucket width in milliseconds.
        bucket_ms: u64,
    },
    /// By region: the key starts with the region's name and a NUL.
    Region {
        /// The regions, one shard each, in shard order.
        regions: Vec<String>,
    },
}

/// One shard and where it lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shard {
    /// Its id.
    pub id: ShardId,
    /// The node that owns it.
    pub owner: NodeId,
    /// The region it must stay in, if pinned.
    pub region: Option<String>,
}

/// Where nodes are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// Each node with its region.
    pub nodes: BTreeMap<NodeId, String>,
}

/// The cluster's shard map, versioned by its epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardMap {
    /// The strategy.
    pub strategy: Strategy,
    /// The shards, by id.
    pub shards: BTreeMap<ShardId, Shard>,
    /// Raised by every change, so stale routes are refused.
    pub epoch: u64,
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3))
}

/// The shard that holds `key` under `strategy`, or `None` when the key does not fit it (a time
/// key shorter than 8 bytes, or a region the map does not know).
pub fn route(strategy: &Strategy, key: &[u8]) -> Option<ShardId> {
    match strategy {
        Strategy::Hash { shards } => {
            let shards = u64::from((*shards).max(1));
            Some(((fnv1a(key) as u128 * shards as u128) >> 64) as ShardId)
        }
        Strategy::Range { bounds } => Some(bounds.partition_point(|bound| bound.as_slice() <= key) as ShardId),
        Strategy::Time { bucket_ms } => {
            let time = u64::from_be_bytes(key.get(..8)?.try_into().ok()?);
            Some(u32::try_from(time / (*bucket_ms).max(1)).unwrap_or(u32::MAX))
        }
        Strategy::Region { regions } => {
            let name = key.split(|byte| *byte == 0).next()?;
            regions.iter().position(|region| region.as_bytes() == name).map(|at| at as ShardId)
        }
    }
}

impl ShardMap {
    /// A map whose shards are dealt round-robin to `nodes`; region shards go to a node in their
    /// region.
    pub fn new(strategy: Strategy, placement: &Placement) -> Self {
        let nodes: Vec<NodeId> = placement.nodes.keys().copied().collect();
        let count: u32 = match &strategy {
            Strategy::Hash { shards } => (*shards).max(1),
            Strategy::Range { bounds } => bounds.len() as u32 + 1,
            Strategy::Time { .. } => 0,
            Strategy::Region { regions } => regions.len() as u32,
        };
        let mut shards = BTreeMap::new();
        for id in 0..count {
            let region = match &strategy {
                Strategy::Region { regions } => regions.get(id as usize).cloned(),
                _ => None,
            };
            let candidates: Vec<NodeId> = match &region {
                Some(region) => placement.nodes.iter().filter(|(_, at)| *at == region).map(|(node, _)| *node).collect(),
                None => nodes.clone(),
            };
            let owner = candidates.get(id as usize % candidates.len().max(1)).copied().unwrap_or(0);
            shards.insert(id, Shard { id, owner, region });
        }
        Self { strategy, shards, epoch: 1 }
    }

    /// The node that owns `key`, with the epoch to send along.
    pub fn owner(&self, key: &[u8]) -> Option<(NodeId, u64)> {
        let shard = route(&self.strategy, key)?;
        self.shards.get(&shard).map(|shard| (shard.owner, self.epoch))
    }

    /// Adds a time shard on `owner` when a new bucket appears; returns its id.
    pub fn add_time_shard(&mut self, key: &[u8], owner: NodeId) -> Option<ShardId> {
        let id = route(&self.strategy, key)?;
        if !self.shards.contains_key(&id) {
            self.shards.insert(id, Shard { id, owner, region: None });
            self.epoch += 1;
        }
        Some(id)
    }

    /// Whether a request routed with `epoch` may be served: only the current epoch is.
    pub fn accepts(&self, epoch: u64) -> bool {
        epoch == self.epoch
    }

    fn reassign(&mut self, shard: ShardId, owner: NodeId) {
        if let Some(found) = self.shards.get_mut(&shard)
            && found.owner != owner
        {
            found.owner = owner;
            self.epoch += 1;
        }
    }
}

/// The fewest shard moves that leave every node within one shard of the others, first moving
/// shards off nodes no longer in `placement`, and never moving a region-pinned shard out of its
/// region; as `(shard, from, to)`.
pub fn rebalance(map: &ShardMap, placement: &Placement) -> Vec<(ShardId, NodeId, NodeId)> {
    let mut owner: BTreeMap<ShardId, NodeId> = map.shards.values().map(|shard| (shard.id, shard.owner)).collect();
    let eligible = |shard: ShardId, node: NodeId| {
        let region = map.shards.get(&shard).and_then(|shard| shard.region.as_ref());
        placement.nodes.get(&node).is_some_and(|at| region.is_none_or(|region| at == region))
    };
    let mut moves = Vec::new();
    for _ in 0..(map.shards.len() * 2 + placement.nodes.len() + 1) {
        let mut load: BTreeMap<NodeId, usize> = placement.nodes.keys().map(|node| (*node, 0)).collect();
        for node in owner.values() {
            if let Some(count) = load.get_mut(node) {
                *count += 1;
            }
        }
        let least = |shard: ShardId, load: &BTreeMap<NodeId, usize>| {
            load.iter().filter(|(node, _)| eligible(shard, **node)).min_by_key(|(node, count)| (**count, **node)).map(|(node, count)| (*node, *count))
        };
        let orphan = owner.iter().find(|(_, node)| !placement.nodes.contains_key(node)).map(|(shard, node)| (*shard, *node));
        let chosen = match orphan {
            Some((shard, from)) => least(shard, &load).map(|(to, _)| (shard, from, to)),
            None => {
                let mut by_load: Vec<(ShardId, NodeId, usize)> = owner.iter().map(|(shard, node)| (*shard, *node, load.get(node).copied().unwrap_or(0))).collect();
                by_load.sort_by_key(|(shard, node, count)| (std::cmp::Reverse(*count), *node, *shard));
                by_load.into_iter().find_map(|(shard, from, from_load)| {
                    least(shard, &load).filter(|(_, to_load)| to_load + 1 < from_load).map(|(to, _)| (shard, from, to))
                })
            }
        };
        let Some((shard, from, to)) = chosen else {
            break;
        };
        owner.insert(shard, to);
        moves.push((shard, from, to));
    }
    moves
}

/// The steps of moving one shard online.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveStep {
    /// The new owner is copying a snapshot; the old owner still serves.
    Copying,
    /// The new owner applies changes made since the snapshot.
    CatchingUp,
    /// The map points at the new owner; the old one refuses with the new epoch.
    Done,
}

/// One shard moving from one node to another while it keeps serving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardMove {
    /// The shard.
    pub shard: ShardId,
    /// Where it was.
    pub from: NodeId,
    /// Where it goes.
    pub to: NodeId,
    /// How far it got.
    pub step: MoveStep,
    /// Changes applied on the old owner and not yet on the new one.
    pub lag: u64,
}

impl ShardMove {
    /// Starts moving `shard` to `to`.
    pub fn start(map: &ShardMap, shard: ShardId, to: NodeId) -> Option<Self> {
        let from = map.shards.get(&shard)?.owner;
        Some(Self { shard, from, to, step: MoveStep::Copying, lag: 0 })
    }

    /// A write was served by the old owner during the move.
    pub fn record_write(&mut self) {
        if self.step != MoveStep::Done {
            self.lag += 1;
        }
    }

    /// The snapshot is copied.
    pub fn copied(&mut self) {
        if self.step == MoveStep::Copying {
            self.step = MoveStep::CatchingUp;
        }
    }

    /// The new owner applied `changes` more changes.
    pub fn caught_up(&mut self, changes: u64) {
        self.lag = self.lag.saturating_sub(changes);
    }

    /// Cuts over when nothing is left to apply: the map's epoch rises and the shard's owner
    /// changes in one step. Returns whether it cut over.
    pub fn cut_over(&mut self, map: &mut ShardMap) -> bool {
        if self.step != MoveStep::CatchingUp || self.lag > 0 {
            return false;
        }
        map.reassign(self.shard, self.to);
        self.step = MoveStep::Done;
        true
    }
}
