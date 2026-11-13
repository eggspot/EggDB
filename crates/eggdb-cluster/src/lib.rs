//! EggDB's cluster layer (D212). A [`ShardMap`] splits the key space into shards by hash, by key
//! range, by time bucket or by region, and places each shard on a node; every change to the map
//! raises its epoch, and a node refuses requests routed with an older epoch, so a client never
//! writes to a shard's old home after a move. [`rebalance`] plans the fewest moves that even out
//! the shards per node (keeping region-pinned shards in their region), and [`ShardMove`] moves
//! one shard online: copy, catch up, then cut over in one epoch step. [`transaction`] commits
//! changes that span shards with two-phase commit, recording each decision in a
//! [`transaction::DecisionLog`]. [`ShardMap::encode`] gives the map's stored form, so the map and
//! the decisions live in the cluster's Raft-replicated database (D1039). [`quorum`] replicates
//! keys without a leader: any node takes writes, quorums decide success, and replicas converge.

pub mod clock;
mod codec;
pub mod crdt;
pub mod global_index;
pub mod merkle;
pub mod quorum;
pub mod regions;
mod shards;
pub mod transaction;

pub use shards::{MoveStep, Placement, Shard, ShardMap, ShardMove, Strategy, rebalance, route};

/// A node's identity.
pub type NodeId = u64;

/// A shard's identity.
pub type ShardId = u32;

#[cfg(test)]
mod tests;
