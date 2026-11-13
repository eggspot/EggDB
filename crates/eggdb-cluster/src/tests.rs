use crate::transaction::{Coordinator, Decision, Participant, Vote, recover};
use crate::{Placement, ShardMap, ShardMove, Strategy, rebalance, route};
use std::collections::BTreeMap;

fn placement(nodes: &[(u64, &str)]) -> Placement {
    Placement {
        nodes: nodes
            .iter()
            .map(|(node, region)| (*node, (*region).to_owned()))
            .collect(),
    }
}

#[test]
fn keys_route_by_each_strategy() {
    let hash = Strategy::Hash { shards: 8 };
    let mut counts = [0u32; 8];
    for key in 0..8_000u32 {
        counts[route(&hash, &key.to_be_bytes()).unwrap() as usize] += 1;
    }
    assert!(
        counts.iter().all(|count| (700..1_300).contains(count)),
        "{counts:?}"
    );
    let range = Strategy::Range {
        bounds: vec![b"g".to_vec(), b"p".to_vec()],
    };
    assert_eq!(
        [
            route(&range, b"apple"),
            route(&range, b"grape"),
            route(&range, b"zebra")
        ],
        [Some(0), Some(1), Some(2)]
    );
    let time = Strategy::Time {
        bucket_ms: 86_400_000,
    };
    assert_eq!(
        route(&time, &(86_400_000u64 * 3 + 5).to_be_bytes()),
        Some(3)
    );
    assert_eq!(route(&time, b"short"), None);
    let region = Strategy::Region {
        regions: vec!["eu".to_owned(), "us".to_owned()],
    };
    assert_eq!(route(&region, b"us\0order-1"), Some(1));
    assert_eq!(route(&region, b"ap\0order-1"), None);
}

#[test]
fn rebalancing_evens_out_load_and_keeps_regions() {
    let mut map = ShardMap::new(
        Strategy::Hash { shards: 12 },
        &placement(&[(1, "eu"), (2, "eu")]),
    );
    let grown = placement(&[(1, "eu"), (2, "eu"), (3, "eu"), (4, "eu")]);
    let moves = rebalance(&map, &grown);
    assert_eq!(moves.len(), 6);
    for (shard, _, to) in moves {
        map.shards.get_mut(&shard).unwrap().owner = to;
    }
    let mut load: BTreeMap<u64, usize> = BTreeMap::new();
    for shard in map.shards.values() {
        *load.entry(shard.owner).or_default() += 1;
    }
    assert!(load.values().all(|count| *count == 3), "{load:?}");
    let regions = placement(&[(1, "eu"), (2, "us"), (3, "eu")]);
    let pinned = ShardMap::new(
        Strategy::Region {
            regions: vec!["eu".to_owned(), "us".to_owned()],
        },
        &regions,
    );
    let without_us = placement(&[(1, "eu"), (3, "eu")]);
    assert!(
        rebalance(&pinned, &without_us).is_empty(),
        "a pinned shard never leaves its region"
    );
    let shrunk = placement(&[(1, "eu"), (2, "us")]);
    let moved = rebalance(&pinned, &shrunk);
    assert!(moved.iter().all(|(_, _, to)| shrunk.nodes.contains_key(to)));
}

#[test]
fn an_online_move_cuts_over_only_when_caught_up_and_refuses_stale_epochs() {
    let mut map = ShardMap::new(
        Strategy::Hash { shards: 4 },
        &placement(&[(1, "eu"), (2, "eu")]),
    );
    let key = b"customer:7";
    let (owner, epoch) = map.owner(key).unwrap();
    let shard = route(&map.strategy, key).unwrap();
    let target = if owner == 1 { 2 } else { 1 };
    let mut moving = ShardMove::start(&map, shard, target).unwrap();
    moving.record_write();
    assert!(!moving.cut_over(&mut map));
    moving.copied();
    moving.record_write();
    assert!(!moving.cut_over(&mut map));
    moving.caught_up(2);
    assert!(moving.cut_over(&mut map));
    assert_eq!(map.owner(key).unwrap().0, target);
    assert!(!map.accepts(epoch));
    assert!(map.accepts(map.epoch));
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn balance(participants: &BTreeMap<u32, Participant>, account: u32) -> i64 {
    participants[&(account % 3)]
        .committed(&account.to_be_bytes())
        .map_or(0, |bytes| {
            i64::from_be_bytes(bytes.as_slice().try_into().unwrap())
        })
}

#[test]
fn transfers_across_shards_keep_the_total_through_conflicts_and_coordinator_crashes() {
    for seed in 1..=20u64 {
        let mut rng = Rng(seed * 7919);
        let mut participants: BTreeMap<u32, Participant> = (0..3)
            .map(|shard| (shard, Participant::default()))
            .collect();
        let mut coordinator = Coordinator::default();
        let accounts = 9u32;
        let setup = coordinator.begin();
        for account in 0..accounts {
            let shard = participants.get_mut(&(account % 3)).unwrap();
            assert_eq!(
                shard.prepare(
                    setup,
                    vec![(
                        account.to_be_bytes().to_vec(),
                        Some(100i64.to_be_bytes().to_vec())
                    )]
                ),
                Vote::Yes
            );
        }
        coordinator.decide(setup, &[Vote::Yes]);
        recover(&mut coordinator, &mut participants);
        let mut pending: Vec<(u64, u32, u32)> = Vec::new();
        for _ in 0..300 {
            let from = (rng.next() % u64::from(accounts)) as u32;
            let to = (rng.next() % u64::from(accounts)) as u32;
            if from == to {
                continue;
            }
            let transaction = coordinator.begin();
            let reads = (
                participants
                    .get_mut(&(from % 3))
                    .unwrap()
                    .read(transaction, &from.to_be_bytes()),
                participants
                    .get_mut(&(to % 3))
                    .unwrap()
                    .read(transaction, &to.to_be_bytes()),
            );
            let (Ok(Some(from_bytes)), Ok(Some(to_bytes))) = reads else {
                for participant in participants.values_mut() {
                    participant.decide(transaction, Decision::Abort);
                }
                continue;
            };
            let amount = (rng.next() % 30) as i64;
            let from_balance =
                i64::from_be_bytes(from_bytes.as_slice().try_into().unwrap()) - amount;
            let to_balance = i64::from_be_bytes(to_bytes.as_slice().try_into().unwrap()) + amount;
            let votes = [
                participants.get_mut(&(from % 3)).unwrap().prepare(
                    transaction,
                    vec![(
                        from.to_be_bytes().to_vec(),
                        Some(from_balance.to_be_bytes().to_vec()),
                    )],
                ),
                participants.get_mut(&(to % 3)).unwrap().prepare(
                    transaction,
                    vec![(
                        to.to_be_bytes().to_vec(),
                        Some(to_balance.to_be_bytes().to_vec()),
                    )],
                ),
            ];
            match rng.next() % 10 {
                0 => pending.push((transaction, from, to)),
                1 => {
                    coordinator.decide(transaction, &votes);
                    pending.push((transaction, from, to));
                }
                _ => {
                    let decision = coordinator.decide(transaction, &votes);
                    for participant in participants.values_mut() {
                        participant.decide(transaction, decision);
                    }
                }
            }
            if rng.next().is_multiple_of(7) {
                recover(&mut coordinator, &mut participants);
                pending.clear();
            }
        }
        recover(&mut coordinator, &mut participants);
        let total: i64 = (0..accounts)
            .map(|account| balance(&participants, account))
            .sum();
        assert_eq!(total, 100 * i64::from(accounts), "seed {seed}");
        assert!(
            participants
                .values()
                .all(|participant| participant.in_doubt().is_empty())
        );
    }
}

mod leaderless {
    use super::Rng;
    use crate::quorum::{Quorum, QuorumError, Resolution, VersionVector};
    use std::collections::BTreeMap;

    const NODES: [u64; 5] = [1, 2, 3, 4, 5];

    fn converge(quorum: &mut Quorum) {
        for node in NODES {
            quorum.set_reachable(node, true);
        }
        quorum.hand_off();
        quorum.anti_entropy();
    }

    fn simulate(seed: u64, resolution: Resolution) {
        let mut rng = Rng(seed);
        let mut quorum = Quorum::new(&NODES, 3, 2, 2, resolution).unwrap();
        let keys: Vec<Vec<u8>> = (0..6u8).map(|key| vec![b'k', key]).collect();
        let mut acknowledged: BTreeMap<Vec<u8>, Vec<VersionVector>> = BTreeMap::new();
        let mut now = 1_000;
        for _ in 0..400 {
            now += rng.next() % 3;
            match rng.next() % 10 {
                0 => {
                    let node = NODES[(rng.next() % 5) as usize];
                    quorum.set_reachable(node, rng.next().is_multiple_of(2));
                }
                1 => quorum.hand_off(),
                2 | 3 => {
                    let key = &keys[(rng.next() % 6) as usize];
                    let _ = quorum.get(key);
                }
                _ => {
                    let key = keys[(rng.next() % 6) as usize].clone();
                    let coordinator = NODES[(rng.next() % 5) as usize];
                    let context = quorum
                        .get(&key)
                        .map(|read| read.context)
                        .unwrap_or_default();
                    let value =
                        (!rng.next().is_multiple_of(8)).then(|| rng.next().to_le_bytes().to_vec());
                    if let Ok(version) = quorum.put(coordinator, &key, value, &context, now) {
                        acknowledged.entry(key).or_default().push(version.vector);
                    }
                }
            }
        }
        converge(&mut quorum);
        for key in &keys {
            let homes: Vec<u64> = quorum.order(key).into_iter().take(3).collect();
            let first = quorum.replica(homes[0]).unwrap().versions(key).to_vec();
            for home in &homes[1..] {
                assert_eq!(
                    quorum.replica(*home).unwrap().versions(key),
                    first.as_slice(),
                    "seed {seed}: replicas of {key:?} differ after convergence"
                );
            }
            for written in acknowledged.get(key).into_iter().flatten() {
                assert!(
                    first.iter().any(|version| version.vector.descends(written)),
                    "seed {seed}: an acknowledged write of {key:?} was neither kept nor superseded"
                );
            }
            if resolution == Resolution::LastWriterWins {
                assert!(
                    first.len() <= 1,
                    "seed {seed}: last writer wins left siblings"
                );
            }
        }
    }

    #[test]
    fn replicas_converge_and_keep_acknowledged_writes_through_partitions() {
        for seed in 1..=40 {
            simulate(seed, Resolution::LastWriterWins);
            simulate(seed, Resolution::KeepSiblings);
        }
    }

    #[test]
    fn membership_changes_move_keys_and_tombstones_are_collected_once_everywhere() {
        let mut quorum = Quorum::new(&NODES, 3, 2, 2, Resolution::LastWriterWins).unwrap();
        let empty = VersionVector::default();
        for at in 0..40u32 {
            quorum
                .put(1, &at.to_be_bytes(), Some(at.to_be_bytes().to_vec()), &empty, u64::from(at))
                .unwrap();
        }
        assert_eq!(quorum.add_node(99), Ok(1));
        for at in 0..40u32 {
            let key = at.to_be_bytes();
            for home in quorum.order(&key).into_iter().take(3) {
                assert!(!quorum.replica(home).unwrap().versions(&key).is_empty(), "{at} on {home}");
            }
        }
        assert_eq!(quorum.remove_node(1), Ok(2));
        for at in 0..40u32 {
            assert_eq!(quorum.get(&at.to_be_bytes()).unwrap().values, [Some(at.to_be_bytes().to_vec())]);
        }
        let key = 7u32.to_be_bytes();
        let read = quorum.get(&key).unwrap();
        quorum.put(2, &key, None, &read.context, 1_000).unwrap();
        quorum.anti_entropy();
        assert_eq!(quorum.collect_tombstones(), 1);
        assert!(quorum.get(&key).unwrap().values.is_empty());
    }

    #[test]
    fn concurrent_writes_become_siblings_until_a_write_supersedes_them() {
        let mut quorum = Quorum::new(&NODES, 3, 2, 2, Resolution::KeepSiblings).unwrap();
        let key = b"cart";
        let empty = VersionVector::default();
        quorum
            .put(1, key, Some(b"eggs".to_vec()), &empty, 10)
            .unwrap();
        quorum
            .put(2, key, Some(b"milk".to_vec()), &empty, 11)
            .unwrap();
        let read = quorum.get(key).unwrap();
        assert_eq!(
            read.values,
            [Some(b"eggs".to_vec()), Some(b"milk".to_vec())]
        );
        quorum
            .put(3, key, Some(b"eggs,milk".to_vec()), &read.context, 12)
            .unwrap();
        assert_eq!(
            quorum.get(key).unwrap().values,
            [Some(b"eggs,milk".to_vec())]
        );
        let mut lww = Quorum::new(&NODES, 3, 2, 2, Resolution::LastWriterWins).unwrap();
        lww.put(1, key, Some(b"eggs".to_vec()), &empty, 10).unwrap();
        lww.put(2, key, Some(b"milk".to_vec()), &empty, 11).unwrap();
        assert_eq!(lww.get(key).unwrap().values, [Some(b"milk".to_vec())]);
    }

    #[test]
    fn quorums_refuse_when_too_few_replicas_answer() {
        assert_eq!(
            Quorum::new(&NODES, 6, 1, 1, Resolution::LastWriterWins).err(),
            Some(QuorumError::BadConfiguration)
        );
        let mut quorum = Quorum::new(&NODES, 3, 2, 2, Resolution::LastWriterWins).unwrap();
        for node in [2, 3, 4, 5] {
            quorum.set_reachable(node, false);
        }
        let key = b"k";
        assert!(matches!(
            quorum.put(1, key, Some(vec![1]), &VersionVector::default(), 1),
            Err(QuorumError::NotEnoughReplicas { .. })
        ));
        assert!(matches!(
            quorum.get(key),
            Err(QuorumError::NotEnoughReplicas { .. })
        ));
        assert_eq!(
            quorum.put(2, key, None, &VersionVector::default(), 1).err(),
            Some(QuorumError::UnknownCoordinator(2))
        );
    }
}

#[test]
fn a_presumed_abort_stands_against_a_later_commit_and_no_votes_never_commit() {
    let mut coordinator = Coordinator::default();
    let early = coordinator.begin();
    assert_eq!(coordinator.outcome(early), Decision::Abort);
    assert_eq!(
        coordinator.decide(early, &[Vote::Yes, Vote::Yes]),
        Decision::Abort
    );
    let empty = coordinator.begin();
    assert_eq!(coordinator.decide(empty, &[]), Decision::Abort);
    let normal = coordinator.begin();
    assert_eq!(coordinator.decide(normal, &[Vote::Yes]), Decision::Commit);
    assert_eq!(coordinator.decide(normal, &[Vote::No]), Decision::Commit);
    assert_eq!(coordinator.outcome(normal), Decision::Commit);
}

#[cfg(test)]
mod crdt_tests {
    use crate::crdt::{CrdtKind, OrSet, PnCounter, merged};

    #[test]
    fn counters_merge_to_the_sum_of_every_replicas_changes() {
        let mut a = PnCounter::default();
        let mut b = PnCounter::default();
        a.change(1, 5);
        b.change(2, 3);
        b.change(2, -1);
        let state = merged(CrdtKind::Counter, Some(&a.encode()), Some(&b.encode()));
        let again = merged(CrdtKind::Counter, Some(&state), Some(&b.encode()));
        assert_eq!(PnCounter::decode(&state).unwrap().value(), 7);
        assert_eq!(state, again);
    }

    #[test]
    fn a_concurrent_add_survives_a_remove_that_did_not_see_it() {
        let mut a = OrSet::default();
        a.add(1, b"milk");
        let mut b = a.clone();
        b.remove(b"milk");
        a.add(1, b"milk");
        let state = merged(CrdtKind::Set, Some(&a.encode()), Some(&b.encode()));
        let set = OrSet::decode(&state).unwrap();
        assert!(set.contains(b"milk"));
        let mut c = set.clone();
        c.remove(b"milk");
        assert!(!c.contains(b"milk"));
        assert!(OrSet::decode(b"junk").is_none());
    }
}

#[cfg(test)]
mod merkle_tests {
    use crate::merkle::MerkleTree;

    #[test]
    fn trees_differ_only_at_the_leaves_holding_a_changed_key() {
        let keys: Vec<Vec<u8>> = (0..500u32).map(|at| at.to_be_bytes().to_vec()).collect();
        let entries = |changed: Option<usize>| -> Vec<(Vec<u8>, u128)> {
            keys.iter()
                .enumerate()
                .map(|(at, key)| (key.clone(), if Some(at) == changed { 7 } else { 1 }))
                .collect()
        };
        let same = entries(None);
        let other = entries(Some(42));
        let left = MerkleTree::build(8, same.iter().map(|(key, value)| (key.as_slice(), *value)));
        let again = MerkleTree::build(8, same.iter().map(|(key, value)| (key.as_slice(), *value)));
        let right = MerkleTree::build(8, other.iter().map(|(key, value)| (key.as_slice(), *value)));
        assert_eq!(left.root(), again.root());
        assert!(left.differing_leaves(&again).is_empty());
        assert_eq!(left.differing_leaves(&right), vec![MerkleTree::leaf_of(&keys[42], 8)]);
    }
}

#[test]
fn a_shard_map_reads_back_from_its_stored_form_for_every_strategy() {
    let nodes = placement(&[(1, "eu"), (2, "us"), (3, "eu")]);
    let strategies = [
        Strategy::Hash { shards: 6 },
        Strategy::Range {
            bounds: vec![b"g".to_vec(), b"p".to_vec()],
        },
        Strategy::Time { bucket_ms: 3_600_000 },
        Strategy::Region {
            regions: vec!["eu".to_owned(), "us".to_owned()],
        },
    ];
    for strategy in strategies {
        let mut map = ShardMap::new(strategy, &nodes);
        map.epoch = 42;
        let bytes = map.encode();
        assert_eq!(ShardMap::decode(&bytes), Some(map.clone()));
        for cut in 0..bytes.len() {
            assert_eq!(ShardMap::decode(&bytes[..cut]), None);
        }
        let mut newer = bytes.clone();
        newer[0] = 2;
        assert_eq!(ShardMap::decode(&newer), None);
        let mut longer = bytes;
        longer.push(0);
        assert_eq!(ShardMap::decode(&longer), None);
    }
    let mut huge = vec![1u8];
    huge.extend_from_slice(&1u64.to_be_bytes());
    huge.push(1);
    huge.extend_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(ShardMap::decode(&huge), None);
}

#[test]
fn any_decision_log_settles_in_doubt_transactions() {
    use crate::transaction::{DecisionLog, decision_from, recover_from};
    let mut log = Coordinator::default();
    let first = DecisionLog::begin(&mut log).unwrap();
    let second = DecisionLog::begin(&mut log).unwrap();
    assert_ne!(first, second);
    assert_eq!(decision_from(&[]), Decision::Abort);
    assert_eq!(decision_from(&[Vote::Yes, Vote::No]), Decision::Abort);
    assert_eq!(
        DecisionLog::decide(&mut log, first, &[Vote::Yes, Vote::Yes]).unwrap(),
        Decision::Commit
    );
    let mut participants: BTreeMap<u32, Participant> = BTreeMap::new();
    for shard in 0..2u32 {
        let participant = participants.entry(shard).or_default();
        assert_eq!(
            participant.prepare(first, vec![(vec![shard as u8], Some(b"one".to_vec()))]),
            Vote::Yes
        );
        assert_eq!(
            participant.prepare(second, vec![(vec![10 + shard as u8], Some(b"two".to_vec()))]),
            Vote::Yes
        );
    }
    recover_from(&mut log, &mut participants).unwrap();
    for (shard, participant) in &participants {
        assert!(participant.in_doubt().is_empty());
        assert_eq!(participant.committed(&[*shard as u8]), Some(&b"one".to_vec()));
        assert_eq!(participant.committed(&[10 + *shard as u8]), None);
    }
    assert_eq!(
        DecisionLog::decide(&mut log, second, &[Vote::Yes]).unwrap(),
        Decision::Abort
    );
}
