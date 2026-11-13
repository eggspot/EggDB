//! Latency-aware placement for clusters that span regions (D1044): where each shard's owner
//! should live given where its requests come from, which voters a replication group should
//! have so a region can fail without losing the quorum, and what a commit then waits for.

use crate::{NodeId, Placement, ShardId, ShardMap};
use std::collections::{BTreeMap, BTreeSet};

/// Round-trip times between regions, in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Latencies {
    between: BTreeMap<(String, String), u32>,
}

impl Latencies {
    /// No measurements yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the round-trip time between two regions, in both directions.
    pub fn set(&mut self, a: &str, b: &str, milliseconds: u32) {
        self.between.insert((a.to_owned(), b.to_owned()), milliseconds);
        self.between.insert((b.to_owned(), a.to_owned()), milliseconds);
    }

    /// The round-trip time between two regions: 0 within one, and the largest measured one
    /// for a pair never measured, so an unknown path is never preferred.
    pub fn between(&self, a: &str, b: &str) -> u32 {
        if a == b {
            return 0;
        }
        self.between
            .get(&(a.to_owned(), b.to_owned()))
            .copied()
            .unwrap_or_else(|| self.between.values().copied().max().unwrap_or(u32::MAX / 4))
    }
}

/// Requests per region for each shard, as the routers counted them.
pub type Traffic = BTreeMap<ShardId, BTreeMap<String, u64>>;

fn regions_of(placement: &Placement) -> BTreeSet<String> {
    placement.nodes.values().cloned().collect()
}

/// The region whose nodes serve `requests` with the least total latency, weighting each
/// region's requests by its round-trip time to the candidate; ties go to the first region.
pub fn best_region(requests: &BTreeMap<String, u64>, regions: &BTreeSet<String>, latencies: &Latencies) -> Option<String> {
    regions
        .iter()
        .min_by_key(|candidate| {
            requests
                .iter()
                .map(|(from, count)| u128::from(*count) * u128::from(latencies.between(from, candidate)))
                .sum::<u128>()
        })
        .cloned()
}

/// The moves that put each shard's owner in the region nearest its traffic (D1044), as
/// `(shard, from, to)`: a pinned shard stays in its region, a shard without counted traffic
/// stays where it is, and the new owner is the node of that region owning the fewest shards.
pub fn place_by_latency(map: &ShardMap, placement: &Placement, traffic: &Traffic, latencies: &Latencies) -> Vec<(ShardId, NodeId, NodeId)> {
    let regions = regions_of(placement);
    let mut load: BTreeMap<NodeId, usize> = placement.nodes.keys().map(|node| (*node, 0)).collect();
    for shard in map.shards.values() {
        if let Some(count) = load.get_mut(&shard.owner) {
            *count += 1;
        }
    }
    let mut moves = Vec::new();
    for shard in map.shards.values() {
        if shard.region.is_some() {
            continue;
        }
        let Some(requests) = traffic.get(&shard.id).filter(|requests| requests.values().any(|count| *count > 0)) else {
            continue;
        };
        let Some(target) = best_region(requests, &regions, latencies) else {
            continue;
        };
        if placement.nodes.get(&shard.owner) == Some(&target) {
            continue;
        }
        let Some(to) = placement
            .nodes
            .iter()
            .filter(|(_, region)| **region == target)
            .map(|(node, _)| *node)
            .min_by_key(|node| (load.get(node).copied().unwrap_or(0), *node))
        else {
            continue;
        };
        if let Some(count) = load.get_mut(&shard.owner) {
            *count = count.saturating_sub(1);
        }
        *load.entry(to).or_insert(0) += 1;
        moves.push((shard.id, shard.owner, to));
    }
    moves
}

/// What a commit led from `leader_region` waits for with voters in `voter_regions` (the
/// leader's own included): the round trip to the farthest voter of the fastest majority.
pub fn quorum_latency(leader_region: &str, voter_regions: &[String], latencies: &Latencies) -> u32 {
    let mut times: Vec<u32> = voter_regions.iter().map(|region| latencies.between(leader_region, region)).collect();
    times.sort_unstable();
    let majority = voter_regions.len() / 2 + 1;
    times.get(majority.saturating_sub(1)).copied().unwrap_or(0)
}

/// Voters for a replication group of `count` members led from `home`: one node in each of the
/// regions nearest `home` (the home region first), wrapping to second nodes only when there are
/// fewer regions than voters, so losing any one region keeps a majority whenever `count`
/// regions exist. `None` when the placement has fewer nodes than voters.
pub fn choose_voters(placement: &Placement, home: &str, count: usize, latencies: &Latencies) -> Option<Vec<NodeId>> {
    if placement.nodes.len() < count {
        return None;
    }
    let mut regions: Vec<String> = regions_of(placement).into_iter().collect();
    regions.sort_by_key(|region| (latencies.between(home, region), region.clone()));
    let mut by_region: BTreeMap<&String, Vec<NodeId>> = BTreeMap::new();
    for (node, region) in &placement.nodes {
        by_region.entry(region).or_default().push(*node);
    }
    let mut chosen = Vec::with_capacity(count);
    let mut round = 0;
    while chosen.len() < count {
        let mut added = false;
        for region in &regions {
            if chosen.len() == count {
                break;
            }
            if let Some(node) = by_region.get(region).and_then(|nodes| nodes.get(round)) {
                chosen.push(*node);
                added = true;
            }
        }
        if !added {
            return None;
        }
        round += 1;
    }
    Some(chosen)
}

/// Whether losing every node of any one region leaves `voters` a majority.
pub fn survives_region_loss(placement: &Placement, voters: &[NodeId]) -> bool {
    let majority = voters.len() / 2 + 1;
    regions_of(placement).iter().all(|region| {
        voters
            .iter()
            .filter(|voter| placement.nodes.get(voter) != Some(region))
            .count()
            >= majority
    })
}
