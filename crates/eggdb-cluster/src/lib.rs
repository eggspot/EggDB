//! EggDB's cluster layer (D212). A [`ShardMap`] splits the key space into shards by hash, by key
//! range, by time bucket or by region, and places each shard on a node; every change to the map
//! raises its epoch, and a node refuses requests routed with an older epoch, so a client never
//! writes to a shard's old home after a move. [`rebalance`] plans the fewest moves that even out
//! the shards per node (keeping region-pinned shards in their region), and [`ShardMove`] moves
//! one shard online: copy, catch up, then cut over in one epoch step. [`transaction`] commits
//! changes that span shards with two-phase commit.

mod shards;
pub mod transaction;

pub use shards::{MoveStep, Placement, Shard, ShardMap, ShardMove, Strategy, rebalance, route};

/// A node's identity.
pub type NodeId = u64;

/// A shard's identity.
pub type ShardId = u32;

#[cfg(test)]
mod tests;
