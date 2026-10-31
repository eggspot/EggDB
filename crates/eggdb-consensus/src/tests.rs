use crate::{Envelope, Node, NodeId, Role, Timing};
use std::collections::{BTreeMap, BTreeSet};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

struct Cluster {
    nodes: BTreeMap<NodeId, Node>,
    down: BTreeSet<NodeId>,
    in_flight: Vec<Envelope>,
    leaders: BTreeMap<u64, NodeId>,
    applied: BTreeMap<NodeId, Vec<(u64, Vec<u8>)>>,
}

impl Cluster {
    fn new(size: u64, seed: u64) -> Self {
        let members: Vec<NodeId> = (1..=size).collect();
        Self {
            nodes: members.iter().map(|id| (*id, Node::new(*id, &members, Timing::default(), seed.wrapping_mul(31).wrapping_add(*id)))).collect(),
            down: BTreeSet::new(),
            in_flight: Vec::new(),
            leaders: BTreeMap::new(),
            applied: BTreeMap::new(),
        }
    }

    fn check_and_apply(&mut self) {
        for (id, node) in &mut self.nodes {
            if node.role() == Role::Leader {
                if let Some(other) = self.leaders.insert(node.term(), *id) {
                    assert_eq!(other, *id, "two leaders in term {}", node.term());
                }
            }
            for entry in node.take_committed() {
                self.applied.entry(*id).or_default().push((entry.index, entry.command));
            }
        }
        let mut by_index: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        for entries in self.applied.values() {
            for (index, command) in entries {
                if let Some(known) = by_index.insert(*index, command.clone()) {
                    assert_eq!(&known, command, "two different commands committed at index {index}");
                }
            }
        }
    }

    fn round(&mut self, rng: &mut Rng, drop_percent: u64, partition: &BTreeSet<NodeId>) {
        let mut outgoing = Vec::new();
        for (id, node) in &mut self.nodes {
            if !self.down.contains(id) {
                outgoing.extend(node.tick());
            }
        }
        let delivering = std::mem::take(&mut self.in_flight);
        for envelope in delivering.into_iter().chain(outgoing) {
            let crosses = partition.contains(&envelope.from) != partition.contains(&envelope.to);
            if crosses || self.down.contains(&envelope.to) || self.down.contains(&envelope.from) || rng.chance(drop_percent) {
                continue;
            }
            if rng.chance(10) {
                self.in_flight.push(envelope);
                continue;
            }
            if let Some(node) = self.nodes.get_mut(&envelope.to) {
                self.in_flight.extend(node.step(envelope.from, envelope.message));
            }
        }
        self.check_and_apply();
    }

    fn leader(&self) -> Option<NodeId> {
        self.nodes
            .iter()
            .filter(|(id, node)| !self.down.contains(id) && node.role() == Role::Leader)
            .max_by_key(|(_, node)| node.term())
            .map(|(id, _)| *id)
    }
}

#[test]
fn a_cluster_elects_one_leader_and_commits_proposals() {
    let mut cluster = Cluster::new(3, 7);
    let mut rng = Rng(99);
    let none = BTreeSet::new();
    for _ in 0..100 {
        cluster.round(&mut rng, 0, &none);
    }
    let leader = cluster.leader().expect("a leader is elected");
    let (index, outgoing) = cluster.nodes.get_mut(&leader).unwrap().propose(b"set x".to_vec()).unwrap();
    cluster.in_flight.extend(outgoing);
    for _ in 0..50 {
        cluster.round(&mut rng, 0, &none);
    }
    for node in cluster.nodes.values() {
        assert!(node.commit_index() >= index);
    }
    let follower = cluster.nodes.keys().copied().find(|id| *id != leader).unwrap();
    assert_eq!(cluster.nodes.get_mut(&follower).unwrap().propose(Vec::new()).unwrap_err().leader, Some(leader));
}

#[test]
fn a_single_node_leads_itself() {
    let mut node = Node::new(1, &[1], Timing::default(), 3);
    for _ in 0..40 {
        node.tick();
    }
    assert_eq!(node.role(), Role::Leader);
    let (index, outgoing) = node.propose(b"a".to_vec()).unwrap();
    assert!(outgoing.is_empty());
    assert_eq!(node.commit_index(), index);
}

#[test]
fn partitions_crashes_and_drops_never_break_safety_and_the_cluster_recovers() {
    for seed in 1..=30u64 {
        let mut cluster = Cluster::new(5, seed);
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let mut proposed = 0u64;
        for step in 0..1_500u64 {
            let partition: BTreeSet<NodeId> = if (step / 200) % 2 == 1 { [1, 2].into_iter().collect() } else { BTreeSet::new() };
            if rng.chance(1) {
                let victim = rng.next() % 5 + 1;
                if cluster.down.contains(&victim) {
                    cluster.down.remove(&victim);
                    cluster.nodes.get_mut(&victim).unwrap().restart();
                } else if cluster.down.len() < 2 {
                    cluster.down.insert(victim);
                }
            }
            if rng.chance(20)
                && let Some(leader) = cluster.leader()
                && let Ok((_, outgoing)) = cluster.nodes.get_mut(&leader).unwrap().propose(proposed.to_be_bytes().to_vec())
            {
                proposed += 1;
                cluster.in_flight.extend(outgoing);
            }
            cluster.round(&mut rng, 5, &partition);
        }
        let revived: Vec<NodeId> = cluster.down.iter().copied().collect();
        for id in revived {
            cluster.down.remove(&id);
            cluster.nodes.get_mut(&id).unwrap().restart();
        }
        let none = BTreeSet::new();
        for _ in 0..300 {
            cluster.round(&mut rng, 0, &none);
        }
        let leader = cluster.leader().unwrap_or_else(|| panic!("seed {seed}: no leader after healing"));
        let (index, outgoing) = cluster.nodes.get_mut(&leader).unwrap().propose(b"final".to_vec()).unwrap();
        cluster.in_flight.extend(outgoing);
        for _ in 0..200 {
            cluster.round(&mut rng, 0, &none);
        }
        assert!(cluster.nodes.values().all(|node| node.commit_index() >= index), "seed {seed}: the final proposal did not commit everywhere");
    }
}
