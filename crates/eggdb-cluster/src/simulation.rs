//! A seeded simulation of rebalancing under load (D1045): clients keep writing while shards
//! move between nodes, nodes join and are decommissioned, nodes fail and come back, partitions
//! cut nodes off, and node clocks drift apart. It checks the properties the online moves of
//! D212 and D1040 promise: no acknowledged write is lost, no node accepts a write for a shard
//! the map does not give it, and once the faults heal the shards end up within one of even.

use crate::clock::{HybridClock, Timestamp};
use crate::{NodeId, Placement, ShardId, ShardMap, ShardMove, Strategy, rebalance, route};
use std::collections::{BTreeMap, BTreeSet};

/// What the simulation varies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimulationConfig {
    /// The seed of every random choice.
    pub seed: u64,
    /// The nodes at the start.
    pub nodes: u32,
    /// The hash shards.
    pub shards: u32,
    /// The steps of load and faults before the faults heal.
    pub steps: u32,
    /// The keys clients write.
    pub keys: u32,
    /// How far node clocks drift from real time, at most, in milliseconds.
    pub clock_skew_ms: u64,
    /// The clock offset beyond which a node refuses another node's timestamps.
    pub max_offset_ms: u64,
}

impl Default for SimulationConfig {
    fn default() -> Self {
        Self {
            seed: 1,
            nodes: 4,
            shards: 16,
            steps: 2_000,
            keys: 200,
            clock_skew_ms: 300,
            max_offset_ms: 250,
        }
    }
}

/// What happened, and the properties checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SimulationReport {
    /// Writes a node accepted and the client was told about.
    pub writes_acknowledged: u64,
    /// Writes refused because the client routed with an old epoch, or the owner was down or
    /// cut off.
    pub writes_refused: u64,
    /// Moves that cut over.
    pub moves_completed: u64,
    /// Moves given up because a node failed, was cut off, or its clock was too far off.
    pub moves_abandoned: u64,
    /// Moves given up because the new owner refused the old owner's timestamps.
    pub clock_refusals: u64,
    /// Acknowledged writes whose value the shard's owner did not hold at the end: must be 0.
    pub lost_writes: u64,
    /// Writes a node accepted for a shard it did not own: must be 0.
    pub writes_by_non_owners: u64,
    /// The most shards any node owns minus the fewest, at the end.
    pub final_imbalance: usize,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, limit: u64) -> u64 {
        self.next() % limit.max(1)
    }

    fn chance(&mut self, per_thousand: u64) -> bool {
        self.below(1_000) < per_thousand
    }
}

struct SimNode {
    up: bool,
    view: ShardMap,
    clock: HybridClock,
    offset_ms: i64,
    data: BTreeMap<ShardId, BTreeMap<Vec<u8>, (u64, Timestamp)>>,
}

struct Moving {
    plan: ShardMove,
    pending: Vec<Vec<u8>>,
}

struct World {
    rng: Rng,
    config: SimulationConfig,
    map: ShardMap,
    nodes: BTreeMap<NodeId, SimNode>,
    members: BTreeSet<NodeId>,
    cut_off: BTreeSet<NodeId>,
    moves: BTreeMap<ShardId, Moving>,
    client_view: ShardMap,
    acknowledged: BTreeMap<Vec<u8>, u64>,
    wall_ms: u64,
    report: SimulationReport,
}

impl World {
    fn reachable(&self, node: NodeId) -> bool {
        self.nodes.get(&node).is_some_and(|state| state.up) && !self.cut_off.contains(&node)
    }

    fn node_wall(&self, node: NodeId) -> u64 {
        let offset = self.nodes.get(&node).map_or(0, |state| state.offset_ms);
        self.wall_ms.saturating_add_signed(offset)
    }

    fn placement(&self) -> Placement {
        Placement {
            nodes: self.members.iter().map(|node| (*node, "region".to_owned())).collect(),
        }
    }

    fn write(&mut self, step: u64) {
        let key = format!("key {}", self.rng.below(u64::from(self.config.keys))).into_bytes();
        let Some(shard) = route(&self.client_view.strategy, &key) else {
            return;
        };
        let Some((owner, epoch)) = self.client_view.owner(&key) else {
            return;
        };
        let wall = self.node_wall(owner);
        let reachable = self.reachable(owner);
        let authoritative = self.map.shards.get(&shard).map(|found| found.owner);
        let Some(node) = self.nodes.get_mut(&owner) else {
            self.report.writes_refused += 1;
            return;
        };
        let owns = node.view.accepts(epoch) && node.view.shards.get(&shard).is_some_and(|found| found.owner == owner);
        if !reachable || !owns {
            self.report.writes_refused += 1;
            self.client_view = self.map.clone();
            return;
        }
        if authoritative != Some(owner) {
            self.report.writes_by_non_owners += 1;
        }
        let stamp = node.clock.now(wall);
        node.data.entry(shard).or_default().insert(key.clone(), (step, stamp));
        self.acknowledged.insert(key.clone(), step);
        self.report.writes_acknowledged += 1;
        if let Some(moving) = self.moves.get_mut(&shard)
            && moving.plan.from == owner
        {
            moving.plan.record_write();
            moving.pending.push(key);
        }
    }

    fn abandon(&mut self, shard: ShardId) {
        if let Some(moving) = self.moves.remove(&shard) {
            if let Some(target) = self.nodes.get_mut(&moving.plan.to) {
                target.data.remove(&shard);
            }
            self.report.moves_abandoned += 1;
        }
    }

    fn take_in(&mut self, to: NodeId, from: NodeId, shard: ShardId, keys: Option<Vec<Vec<u8>>>) -> bool {
        let source: BTreeMap<Vec<u8>, (u64, Timestamp)> = self
            .nodes
            .get(&from)
            .and_then(|node| node.data.get(&shard))
            .cloned()
            .unwrap_or_default();
        let wall = self.node_wall(to);
        let Some(target) = self.nodes.get_mut(&to) else {
            return false;
        };
        let copied: Vec<(Vec<u8>, (u64, Timestamp))> = match keys {
            None => source.into_iter().collect(),
            Some(keys) => keys
                .into_iter()
                .filter_map(|key| source.get(&key).map(|entry| (key, *entry)))
                .collect(),
        };
        for (key, (value, stamp)) in copied {
            if target.clock.observe(stamp, wall).is_err() {
                return false;
            }
            target.data.entry(shard).or_default().insert(key, (value, stamp));
        }
        true
    }

    fn advance_moves(&mut self) {
        let shards: Vec<ShardId> = self.moves.keys().copied().collect();
        for shard in shards {
            let Some((from, to, step)) = self.moves.get(&shard).map(|moving| (moving.plan.from, moving.plan.to, moving.plan.step)) else {
                continue;
            };
            if !self.reachable(from) || !self.reachable(to) {
                if self.rng.chance(50) {
                    self.abandon(shard);
                }
                continue;
            }
            match step {
                crate::MoveStep::Copying => {
                    if !self.take_in(to, from, shard, None) {
                        self.report.clock_refusals += 1;
                        self.abandon(shard);
                        continue;
                    }
                    if let Some(moving) = self.moves.get_mut(&shard) {
                        moving.plan.copied();
                    }
                }
                crate::MoveStep::CatchingUp => {
                    let pending = self.moves.get_mut(&shard).map(|moving| std::mem::take(&mut moving.pending)).unwrap_or_default();
                    let count = pending.len() as u64;
                    if !self.take_in(to, from, shard, Some(pending)) {
                        self.report.clock_refusals += 1;
                        self.abandon(shard);
                        continue;
                    }
                    let mut map = self.map.clone();
                    let cut = match self.moves.get_mut(&shard) {
                        Some(moving) => {
                            moving.plan.caught_up(count);
                            moving.plan.cut_over(&mut map)
                        }
                        None => false,
                    };
                    if cut {
                        self.map = map;
                        for node in [from, to] {
                            if let Some(state) = self.nodes.get_mut(&node) {
                                state.view = self.map.clone();
                            }
                        }
                        if let Some(old) = self.nodes.get_mut(&from) {
                            old.data.remove(&shard);
                        }
                        self.moves.remove(&shard);
                        self.report.moves_completed += 1;
                    }
                }
                crate::MoveStep::Done => {
                    self.moves.remove(&shard);
                }
            }
        }
    }

    fn plan_moves(&mut self) {
        for (shard, _, to) in rebalance(&self.map, &self.placement()) {
            if self.moves.contains_key(&shard) || !self.reachable(to) {
                continue;
            }
            if let Some(plan) = ShardMove::start(&self.map, shard, to) {
                self.moves.insert(shard, Moving { plan, pending: Vec::new() });
            }
        }
    }

    fn gossip(&mut self) {
        let map = self.map.clone();
        let reachable: Vec<NodeId> = self.nodes.keys().copied().filter(|node| self.reachable(*node)).collect();
        for node in reachable {
            if let Some(state) = self.nodes.get_mut(&node) {
                state.view = map.clone();
            }
        }
    }

    fn faults(&mut self) {
        let ids: Vec<NodeId> = self.nodes.keys().copied().collect();
        if self.rng.chance(4) {
            let victim = ids[self.rng.below(ids.len() as u64) as usize];
            let down = self.nodes.values().filter(|state| !state.up).count();
            if let Some(state) = self.nodes.get_mut(&victim) {
                if state.up && down + 1 < ids.len() {
                    state.up = false;
                } else {
                    state.up = true;
                }
            }
        }
        if self.rng.chance(3) {
            if self.cut_off.is_empty() {
                let victim = ids[self.rng.below(ids.len() as u64) as usize];
                self.cut_off.insert(victim);
            } else {
                self.cut_off.clear();
            }
        }
        if self.rng.chance(5) {
            let victim = ids[self.rng.below(ids.len() as u64) as usize];
            let skew = self.config.clock_skew_ms as i64;
            let offset = self.rng.below(2 * self.config.clock_skew_ms + 1) as i64 - skew;
            if let Some(state) = self.nodes.get_mut(&victim) {
                state.offset_ms = offset;
            }
        }
        if self.rng.chance(2) {
            let id = self.nodes.keys().max().copied().unwrap_or(0) + 1;
            self.add_node(id);
        }
        if self.rng.chance(1) && self.members.len() > 2 {
            let leaving = *self.members.iter().nth(self.rng.below(self.members.len() as u64) as usize).unwrap_or(&0);
            self.members.remove(&leaving);
        }
    }

    fn add_node(&mut self, id: NodeId) {
        self.nodes.insert(
            id,
            SimNode {
                up: true,
                view: self.map.clone(),
                clock: HybridClock::new(self.config.max_offset_ms),
                offset_ms: 0,
                data: BTreeMap::new(),
            },
        );
        self.members.insert(id);
    }

    fn heal(&mut self) {
        self.cut_off.clear();
        for state in self.nodes.values_mut() {
            state.up = true;
            state.offset_ms = 0;
        }
        self.gossip();
    }

    fn check(&mut self) {
        for (key, value) in &self.acknowledged {
            let Some(shard) = route(&self.map.strategy, key) else {
                continue;
            };
            let owner = self.map.shards.get(&shard).map(|found| found.owner);
            let held = owner
                .and_then(|owner| self.nodes.get(&owner))
                .and_then(|node| node.data.get(&shard))
                .and_then(|data| data.get(key))
                .map(|(held, _)| *held);
            if held != Some(*value) {
                self.report.lost_writes += 1;
            }
        }
        let mut load: BTreeMap<NodeId, usize> = self.members.iter().map(|node| (*node, 0)).collect();
        for shard in self.map.shards.values() {
            *load.entry(shard.owner).or_insert(0) += 1;
        }
        let most = load.values().copied().max().unwrap_or(0);
        let fewest = load.values().copied().min().unwrap_or(0);
        self.report.final_imbalance = most - fewest;
    }
}

/// Runs the simulation and reports what it saw.
pub fn simulate_rebalancing(config: SimulationConfig) -> SimulationReport {
    let initial: Placement = Placement {
        nodes: (1..=u64::from(config.nodes.max(1))).map(|node| (node, "region".to_owned())).collect(),
    };
    let map = ShardMap::new(Strategy::Hash { shards: config.shards.max(1) }, &initial);
    let mut world = World {
        rng: Rng(config.seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1),
        config,
        map: map.clone(),
        nodes: BTreeMap::new(),
        members: BTreeSet::new(),
        cut_off: BTreeSet::new(),
        moves: BTreeMap::new(),
        client_view: map,
        acknowledged: BTreeMap::new(),
        wall_ms: 1_000_000,
        report: SimulationReport::default(),
    };
    for node in initial.nodes.keys() {
        world.add_node(*node);
    }
    for step in 0..u64::from(config.steps) {
        world.wall_ms += 1 + world.rng.below(3);
        world.faults();
        for _ in 0..4 {
            world.write(step);
        }
        if step % 5 == 0 {
            world.gossip();
        }
        if step % 25 == 0 {
            world.plan_moves();
        }
        world.advance_moves();
    }
    world.heal();
    for round in 0..400u64 {
        world.wall_ms += 2;
        world.write(u64::from(config.steps) + round);
        world.plan_moves();
        world.advance_moves();
        world.advance_moves();
        world.gossip();
        if world.moves.is_empty() && rebalance(&world.map, &world.placement()).is_empty() {
            break;
        }
    }
    world.check();
    world.report
}
