use crate::storage::{self, MemoryStorage};
use crate::{
    BrokenLog, Changes, Envelope, HardState, Message, Node, NodeId, Recovered, Role, Snapshot,
    Timing,
};
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
    storage: BTreeMap<NodeId, MemoryStorage>,
    members: Vec<NodeId>,
    down: BTreeSet<NodeId>,
    in_flight: Vec<Envelope>,
    leaders: BTreeMap<u64, NodeId>,
    applied: BTreeMap<NodeId, Vec<(u64, Vec<u8>)>>,
}

impl Cluster {
    fn new(size: u64, seed: u64) -> Self {
        let members: Vec<NodeId> = (1..=size).collect();
        Self {
            nodes: members
                .iter()
                .map(|id| {
                    (
                        *id,
                        Node::new(
                            *id,
                            &members,
                            Timing::default(),
                            seed.wrapping_mul(31).wrapping_add(*id),
                        ),
                    )
                })
                .collect(),
            storage: members
                .iter()
                .map(|id| (*id, MemoryStorage::new()))
                .collect(),
            members,
            down: BTreeSet::new(),
            in_flight: Vec::new(),
            leaders: BTreeMap::new(),
            applied: BTreeMap::new(),
        }
    }

    fn check_and_apply(&mut self) {
        for (id, node) in &mut self.nodes {
            if node.role() == Role::Leader
                && let Some(other) = self.leaders.insert(node.term(), *id)
            {
                assert_eq!(other, *id, "two leaders in term {}", node.term());
            }
            for entry in node.take_committed() {
                self.applied
                    .entry(*id)
                    .or_default()
                    .push((entry.index, entry.command));
            }
        }
        let mut by_index: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
        for entries in self.applied.values() {
            for (index, command) in entries {
                if let Some(known) = by_index.insert(*index, command.clone()) {
                    assert_eq!(
                        &known, command,
                        "two different commands committed at index {index}"
                    );
                }
            }
        }
    }

    fn revive(&mut self, id: NodeId) {
        self.down.remove(&id);
        let recovered = self.storage[&id].recovered();
        let node = Node::recover(id, &self.members, Timing::default(), id * 7 + 3, recovered)
            .expect("stored logs are contiguous");
        self.nodes.insert(id, node);
    }

    fn compact_all(&mut self) {
        for node in self.nodes.values_mut() {
            let through = node.applied_index().saturating_sub(3);
            node.compact(through, through.to_be_bytes().to_vec());
        }
    }

    fn round(&mut self, rng: &mut Rng, drop_percent: u64, partition: &BTreeSet<NodeId>) {
        let mut outgoing = Vec::new();
        for (id, node) in &mut self.nodes {
            if !self.down.contains(id) {
                outgoing.extend(node.tick());
                if let Some(storage) = self.storage.get_mut(id) {
                    storage.store(&node.take_changes());
                }
            }
        }
        let delivering = std::mem::take(&mut self.in_flight);
        for envelope in delivering.into_iter().chain(outgoing) {
            let crosses = partition.contains(&envelope.from) != partition.contains(&envelope.to);
            if crosses
                || self.down.contains(&envelope.to)
                || self.down.contains(&envelope.from)
                || rng.chance(drop_percent)
            {
                continue;
            }
            if rng.chance(10) {
                self.in_flight.push(envelope);
                continue;
            }
            if let Some(node) = self.nodes.get_mut(&envelope.to) {
                let replies = node.step(envelope.from, envelope.message);
                if let Some(storage) = self.storage.get_mut(&envelope.to) {
                    storage.store(&node.take_changes());
                }
                self.in_flight.extend(replies);
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
    let (index, outgoing) = cluster
        .nodes
        .get_mut(&leader)
        .unwrap()
        .propose(b"set x".to_vec())
        .unwrap();
    cluster.in_flight.extend(outgoing);
    for _ in 0..50 {
        cluster.round(&mut rng, 0, &none);
    }
    for node in cluster.nodes.values() {
        assert!(node.commit_index() >= index);
    }
    let follower = cluster
        .nodes
        .keys()
        .copied()
        .find(|id| *id != leader)
        .unwrap();
    assert_eq!(
        cluster
            .nodes
            .get_mut(&follower)
            .unwrap()
            .propose(Vec::new())
            .unwrap_err()
            .leader,
        Some(leader)
    );
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
            let partition: BTreeSet<NodeId> = if (step / 200) % 2 == 1 {
                [1, 2].into_iter().collect()
            } else {
                BTreeSet::new()
            };
            if rng.chance(1) {
                let victim = rng.next() % 5 + 1;
                if cluster.down.contains(&victim) {
                    cluster.revive(victim);
                } else if cluster.down.len() < 2 {
                    cluster.down.insert(victim);
                }
            }
            if rng.chance(20)
                && let Some(leader) = cluster.leader()
                && let Ok((_, outgoing)) = cluster
                    .nodes
                    .get_mut(&leader)
                    .unwrap()
                    .propose(proposed.to_be_bytes().to_vec())
            {
                proposed += 1;
                cluster.in_flight.extend(outgoing);
            }
            if step % 97 == 0 {
                cluster.compact_all();
            }
            cluster.round(&mut rng, 5, &partition);
        }
        let revived: Vec<NodeId> = cluster.down.iter().copied().collect();
        for id in revived {
            cluster.revive(id);
        }
        let none = BTreeSet::new();
        for _ in 0..300 {
            cluster.round(&mut rng, 0, &none);
        }
        let leader = cluster
            .leader()
            .unwrap_or_else(|| panic!("seed {seed}: no leader after healing"));
        let (index, outgoing) = cluster
            .nodes
            .get_mut(&leader)
            .unwrap()
            .propose(b"final".to_vec())
            .unwrap();
        cluster.in_flight.extend(outgoing);
        for _ in 0..200 {
            cluster.round(&mut rng, 0, &none);
        }
        assert!(
            cluster
                .nodes
                .values()
                .all(|node| node.commit_index() >= index),
            "seed {seed}: the final proposal did not commit everywhere"
        );
    }
}

fn entry(term: u64, index: u64) -> crate::Entry {
    crate::Entry {
        term,
        index,
        command: vec![index as u8],
    }
}

#[test]
fn a_follower_never_commits_past_what_it_shares_with_the_leader() {
    let mut follower = Node::new(2, &[1, 2, 3], Timing::default(), 5);
    follower.step(
        1,
        crate::Message::Append {
            term: 1,
            previous_index: 0,
            previous_term: 0,
            entries: vec![entry(1, 1), entry(1, 2), entry(1, 3)],
            commit: 0,
        },
    );
    follower.step(
        3,
        crate::Message::Append {
            term: 2,
            previous_index: 1,
            previous_term: 1,
            entries: Vec::new(),
            commit: 3,
        },
    );
    assert_eq!(follower.commit_index(), 1);
    let refused = follower.step(
        3,
        crate::Message::Append {
            term: 2,
            previous_index: 1,
            previous_term: 1,
            entries: vec![entry(2, 0), entry(2, 7)],
            commit: 3,
        },
    );
    assert_eq!(follower.log().len(), 3);
    assert!(refused.iter().any(|envelope| matches!(
        envelope.message,
        crate::Message::AppendReply { success: false, .. }
    )));
}

#[test]
fn a_follower_behind_the_compacted_log_receives_the_snapshot() {
    let mut cluster = Cluster::new(3, 11);
    let mut rng = Rng(5);
    let none = BTreeSet::new();
    for _ in 0..100 {
        cluster.round(&mut rng, 0, &none);
    }
    let leader = cluster.leader().expect("a leader is elected");
    let lagging = cluster
        .members
        .iter()
        .copied()
        .find(|id| *id != leader)
        .unwrap();
    cluster.down.insert(lagging);
    for value in 0..20u8 {
        let (_, outgoing) = cluster
            .nodes
            .get_mut(&leader)
            .unwrap()
            .propose(vec![value])
            .unwrap();
        cluster.in_flight.extend(outgoing);
        cluster.round(&mut rng, 0, &none);
    }
    for _ in 0..20 {
        cluster.round(&mut rng, 0, &none);
    }
    let node = cluster.nodes.get_mut(&leader).unwrap();
    let through = node.applied_index();
    assert!(through >= 20);
    assert!(node.compact(through, b"state".to_vec()));
    assert!(!node.compact(through, Vec::new()));
    cluster.revive(lagging);
    for _ in 0..60 {
        cluster.round(&mut rng, 0, &none);
    }
    let caught_up = &cluster.nodes[&lagging];
    assert!(caught_up.snapshot_position().0 >= through);
    assert!(caught_up.commit_index() >= through);
    assert_eq!(
        cluster.storage[&lagging]
            .recovered()
            .snapshot
            .map(|snapshot| snapshot.data),
        Some(b"state".to_vec())
    );
}

#[test]
fn an_installed_snapshot_replaces_the_log_and_is_reported_for_storage() {
    let mut follower = Node::new(2, &[1, 2, 3], Timing::default(), 5);
    follower.step(
        1,
        Message::Append {
            term: 1,
            previous_index: 0,
            previous_term: 0,
            entries: vec![entry(1, 1), entry(1, 2)],
            commit: 1,
        },
    );
    let first = follower.take_changes();
    assert_eq!(first.entries_from, Some(1));
    assert_eq!(first.entries.len(), 2);
    assert_eq!(
        first.hard_state,
        Some(HardState {
            term: 1,
            voted_for: None
        })
    );
    let replies = follower.step(
        1,
        Message::InstallSnapshot {
            term: 2,
            snapshot: Snapshot {
                index: 10,
                term: 2,
                data: b"all of it".to_vec(),
            },
        },
    );
    assert!(replies.iter().any(|envelope| matches!(
        envelope.message,
        Message::AppendReply {
            success: true,
            index: 10,
            ..
        }
    )));
    assert!(follower.log().is_empty());
    assert_eq!(follower.commit_index(), 10);
    assert!(follower.take_committed().is_empty());
    let changes = follower.take_changes();
    assert_eq!(
        changes.installed.as_ref().map(|snapshot| snapshot.index),
        Some(10)
    );
    assert_eq!(changes.entries_from, Some(11));
    let mut stored = MemoryStorage::new();
    stored.store(&first);
    stored.store(&changes);
    let recovered = stored.recovered();
    assert!(recovered.entries.is_empty());
    let revived = Node::recover(2, &[1, 2, 3], Timing::default(), 5, recovered).unwrap();
    assert_eq!(revived.snapshot_position(), (10, 2));
    assert_eq!(revived.term(), 2);
    follower.step(
        1,
        Message::Append {
            term: 2,
            previous_index: 9,
            previous_term: 1,
            entries: vec![entry(2, 10), entry(2, 11)],
            commit: 11,
        },
    );
    assert_eq!(follower.commit_index(), 11);
    assert_eq!(follower.take_committed().len(), 1);
}

#[test]
fn stored_state_reads_back_and_a_gap_in_the_log_is_refused() {
    let state = HardState {
        term: 9,
        voted_for: Some(u64::MAX),
    };
    assert_eq!(
        storage::decode_hard_state(&storage::encode_hard_state(&state)),
        Some(state)
    );
    assert_eq!(storage::decode_hard_state(&[0; 3]), None);
    let mut bad = storage::encode_hard_state(&state);
    bad[8] = 2;
    assert_eq!(storage::decode_hard_state(&bad), None);
    assert_eq!(
        storage::decode_snapshot_position(&storage::encode_snapshot_position(4, 2)),
        Some((4, 2))
    );
    let stored = entry(3, 5);
    assert_eq!(
        storage::decode_entry(5, &storage::encode_entry(&stored)),
        Some(stored)
    );
    assert_eq!(storage::decode_entry(5, &[1, 2]), None);
    let gap = Recovered {
        hard_state: HardState::default(),
        snapshot: None,
        entries: vec![entry(1, 1), entry(1, 3)],
    };
    assert_eq!(
        Node::recover(1, &[1], Timing::default(), 1, gap).unwrap_err(),
        BrokenLog {
            expected: 2,
            found: 3
        }
    );
    assert!(Changes::default().is_empty());
}
