//! Mixed versions in one cluster during a rolling upgrade (D1049): every release supports a
//! range of cluster feature levels, the cluster runs at the highest level every member supports,
//! and the level only rises once the last member has been upgraded, so an upgraded node never
//! sends what an older one cannot read, and a node too old for the cluster's level is refused
//! before it joins.

use crate::NodeId;
use std::collections::BTreeMap;

/// The feature levels one release supports, inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelRange {
    /// The oldest level it still speaks.
    pub oldest: u32,
    /// The newest level it speaks.
    pub newest: u32,
}

/// Why a node may not join a cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinRefused {
    /// The cluster already runs at a level newer than this release speaks: an upgraded
    /// cluster cannot take a downgraded node back.
    TooOld {
        /// The cluster's level.
        cluster: u32,
        /// The newest level this release speaks.
        newest: u32,
    },
    /// The cluster still runs at a level older than this release speaks: upgrade in smaller
    /// steps.
    TooNew {
        /// The cluster's level.
        cluster: u32,
        /// The oldest level this release speaks.
        oldest: u32,
    },
}

impl std::fmt::Display for JoinRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooOld { cluster, newest } => write!(
                formatter,
                "the cluster runs at feature level {cluster}, and this release speaks levels up to {newest}: run a release at least as new as the cluster's"
            ),
            Self::TooNew { cluster, oldest } => write!(
                formatter,
                "the cluster runs at feature level {cluster}, and this release speaks levels from {oldest}: upgrade the cluster through a release in between first"
            ),
        }
    }
}

impl std::error::Error for JoinRefused {}

/// Whether a node of `range` may join a cluster running at `cluster`.
pub fn may_join(cluster: u32, range: LevelRange) -> Result<(), JoinRefused> {
    if cluster > range.newest {
        return Err(JoinRefused::TooOld { cluster, newest: range.newest });
    }
    if cluster < range.oldest {
        return Err(JoinRefused::TooNew { cluster, oldest: range.oldest });
    }
    Ok(())
}

/// The level the cluster should run at: the newest every member speaks, never below
/// `current` (levels only rise), and `current` itself while a member's range is not known yet
/// or no common level is newer.
pub fn negotiated_level(current: u32, members: &[NodeId], ranges: &BTreeMap<NodeId, LevelRange>) -> u32 {
    let mut common = u32::MAX;
    for member in members {
        let Some(range) = ranges.get(member) else {
            return current;
        };
        common = common.min(range.newest);
    }
    if common == u32::MAX { current } else { common.max(current) }
}

/// The order to upgrade a cluster's members one at a time: followers first, in id order, and
/// the leader last, so the cluster changes leader once.
pub fn upgrade_order(members: &[NodeId], leader: Option<NodeId>) -> Vec<NodeId> {
    let mut order: Vec<NodeId> = members.iter().copied().filter(|member| Some(*member) != leader).collect();
    order.sort_unstable();
    if let Some(leader) = leader.filter(|leader| members.contains(leader)) {
        order.push(leader);
    }
    order
}
