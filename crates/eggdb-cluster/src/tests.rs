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

#[test]
fn snapshot_reads_across_shards_see_every_transfer_whole() {
    use crate::clock::{HybridClock, Timestamp};
    use crate::transaction::{TransactionId, commit_timestamp, snapshot_read};
    const ACCOUNTS: u32 = 6;
    const SHARDS: u32 = 3;
    let key = |account: u32| account.to_be_bytes().to_vec();
    let amount_of = |bytes: &[u8]| i64::from_be_bytes(bytes.try_into().unwrap());
    for seed in 1..=20u64 {
        let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut participants: BTreeMap<u32, Participant> =
            (0..SHARDS).map(|shard| (shard, Participant::default())).collect();
        let mut clocks: Vec<HybridClock> = (0..SHARDS).map(|_| HybridClock::new(500)).collect();
        let mut wall = 1_000u64;
        let mut prepares = Vec::new();
        for shard in 0..SHARDS {
            let at = clocks[shard as usize].now(wall);
            let writes = (0..ACCOUNTS)
                .filter(|account| account % SHARDS == shard)
                .map(|account| (key(account), Some(100i64.to_be_bytes().to_vec())))
                .collect();
            participants.get_mut(&shard).unwrap().prepare_at(1, writes, at);
            prepares.push(at);
        }
        let setup_at = commit_timestamp(&prepares);
        for participant in participants.values_mut() {
            participant.decide_at(1, Decision::Commit, setup_at);
        }
        let mut pending: Vec<(TransactionId, Vec<u32>, Vec<Timestamp>)> = Vec::new();
        let mut transaction: TransactionId = 1;
        let mut remembered: Vec<(Timestamp, Vec<Option<Vec<u8>>>)> = Vec::new();
        let keys: Vec<(u32, Vec<u8>)> = (0..ACCOUNTS).map(|account| (account % SHARDS, key(account))).collect();
        let settle = |participants: &mut BTreeMap<u32, Participant>,
                          clocks: &mut Vec<HybridClock>,
                          wall: u64,
                          (id, shards, prepared): (TransactionId, Vec<u32>, Vec<Timestamp>)| {
            let at = commit_timestamp(&prepared);
            for shard in shards {
                let _ = clocks[shard as usize].observe(at, wall);
                participants.get_mut(&shard).unwrap().decide_at(id, Decision::Commit, at);
            }
        };
        for _ in 0..400 {
            wall += next() % 3;
            match next() % 3 {
                0 => {
                    let from = (next() % u64::from(ACCOUNTS)) as u32;
                    let to = (next() % u64::from(ACCOUNTS)) as u32;
                    if from == to {
                        continue;
                    }
                    transaction += 1;
                    let mut balances = Vec::new();
                    let mut refused = false;
                    for account in [from, to] {
                        match participants.get_mut(&(account % SHARDS)).unwrap().read(transaction, &key(account)) {
                            Ok(value) => balances.push(amount_of(value.as_deref().unwrap())),
                            Err(_) => refused = true,
                        }
                    }
                    if refused {
                        for participant in participants.values_mut() {
                            participant.decide(transaction, Decision::Abort);
                        }
                        continue;
                    }
                    let amount = (next() % 10) as i64 + 1;
                    let mut by_shard: BTreeMap<u32, Vec<(Vec<u8>, Option<Vec<u8>>)>> = BTreeMap::new();
                    by_shard.entry(from % SHARDS).or_default().push((key(from), Some((balances[0] - amount).to_be_bytes().to_vec())));
                    by_shard.entry(to % SHARDS).or_default().push((key(to), Some((balances[1] + amount).to_be_bytes().to_vec())));
                    let mut prepared = Vec::new();
                    for (shard, writes) in &by_shard {
                        let at = clocks[*shard as usize].now(wall);
                        let participant = participants.get_mut(shard).unwrap();
                        assert_eq!(participant.prepare_at(transaction, writes.clone(), at), Vote::Yes);
                        prepared.push(participant.prepared_at(transaction).unwrap());
                    }
                    pending.push((transaction, by_shard.keys().copied().collect(), prepared));
                }
                1 => {
                    if !pending.is_empty() {
                        let oldest = pending.remove(0);
                        settle(&mut participants, &mut clocks, wall, oldest);
                    }
                }
                _ => {
                    let at = Timestamp::new(wall.saturating_sub(next() % 5), 0);
                    let values = loop {
                        match snapshot_read(&mut participants, &keys, at) {
                            Ok(values) => break values,
                            Err(waiting) => {
                                let position = pending
                                    .iter()
                                    .position(|(id, _, _)| *id == waiting)
                                    .expect("a read waits only for a prepared transaction");
                                let undecided = pending.remove(position);
                                settle(&mut participants, &mut clocks, wall, undecided);
                            }
                        }
                    };
                    let present: Vec<i64> = values.iter().flatten().map(|bytes| amount_of(bytes.as_slice())).collect();
                    assert!(
                        present.is_empty() || (present.len() == ACCOUNTS as usize && present.iter().sum::<i64>() == 600),
                        "seed {seed}: a snapshot at {at:?} saw a transfer in part: {present:?}"
                    );
                    remembered.push((at, values));
                }
            }
        }
        for (at, values) in remembered {
            assert_eq!(
                snapshot_read(&mut participants, &keys, at),
                Ok(values),
                "seed {seed}: a snapshot at {at:?} changed after it was read"
            );
        }
    }
}

#[test]
fn hybrid_clocks_stay_ahead_of_what_they_saw_and_refuse_far_futures() {
    use crate::clock::{HybridClock, Timestamp};
    let mut clock = HybridClock::new(100);
    let first = clock.now(1_000);
    assert_eq!(first, Timestamp::new(1_000, 0));
    let second = clock.now(1_000);
    assert_eq!(second, Timestamp::new(1_000, 1));
    let seen = clock.observe(Timestamp::new(1_050, 7), 1_001).unwrap();
    assert_eq!(seen, Timestamp::new(1_050, 8));
    assert!(clock.now(1_002) > seen);
    assert_eq!(clock.observe(Timestamp::new(5_000, 0), 1_002).unwrap_err().ahead_ms, 3_998);
    assert_eq!(Timestamp::new(7, 9).physical_ms(), 7);
    assert_eq!(Timestamp::new(7, 9).logical(), 9);
}

#[test]
fn a_global_index_changes_with_its_rows_and_never_disagrees_in_a_snapshot() {
    use crate::clock::HybridClock;
    use crate::global_index::GlobalIndex;
    use crate::transaction::{commit_timestamp, snapshot_read};
    const ROWS: u32 = 8;
    const ROW_SHARDS: u32 = 3;
    const INDEX_SHARD_BASE: u32 = 10;
    let index = GlobalIndex { name: "by_color".to_owned(), shards: 2 };
    let colors: [&[u8]; 3] = [b"red", b"green", b"blue"];
    let row_key = |row: u32| row.to_be_bytes().to_vec();
    let mut participants: BTreeMap<u32, Participant> = (0..ROW_SHARDS)
        .chain(INDEX_SHARD_BASE..INDEX_SHARD_BASE + index.shards)
        .map(|shard| (shard, Participant::default()))
        .collect();
    let mut clock = HybridClock::new(1_000);
    let mut wall = 5_000u64;
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut transaction = 0u64;
    let mut snapshots = Vec::new();
    for step in 0..300u32 {
        wall += 1;
        let row = (next() % u64::from(ROWS)) as u32;
        let shard = row % ROW_SHARDS;
        transaction += 1;
        let old = participants
            .get_mut(&shard)
            .unwrap()
            .read(transaction, &row_key(row))
            .unwrap();
        let new: Option<Vec<u8>> = if next() % 5 == 0 {
            None
        } else {
            Some(colors[(next() % 3) as usize].to_vec())
        };
        let mut writes: BTreeMap<u32, Vec<(Vec<u8>, Option<Vec<u8>>)>> = BTreeMap::new();
        writes.entry(shard).or_default().push((row_key(row), new.clone()));
        for (index_shard, key, value) in index.entry_changes(&row_key(row), old.as_deref(), new.as_deref()) {
            writes.entry(INDEX_SHARD_BASE + index_shard).or_default().push((key, value));
        }
        let mut prepared = Vec::new();
        for (target, changes) in &writes {
            let at = clock.now(wall);
            let participant = participants.get_mut(target).unwrap();
            assert_eq!(participant.prepare_at(transaction, changes.clone(), at), Vote::Yes);
            prepared.push(participant.prepared_at(transaction).unwrap());
        }
        let at = commit_timestamp(&prepared);
        for target in writes.keys() {
            participants.get_mut(target).unwrap().decide_at(transaction, Decision::Commit, at);
        }
        if step % 10 == 0 {
            snapshots.push(clock.now(wall));
        }
    }
    for at in snapshots {
        let keys: Vec<(u32, Vec<u8>)> = (0..ROWS).map(|row| (row % ROW_SHARDS, row_key(row))).collect();
        let rows = snapshot_read(&mut participants, &keys, at).unwrap();
        let mut entries = Vec::new();
        for index_shard in 0..index.shards {
            entries.extend(
                participants
                    .get_mut(&(INDEX_SHARD_BASE + index_shard))
                    .unwrap()
                    .scan_at(b"\xffgix\x00", at)
                    .unwrap(),
            );
        }
        let present: Vec<(u32, Vec<u8>)> = rows
            .iter()
            .enumerate()
            .filter_map(|(row, value)| Some((row as u32, value.clone()?)))
            .collect();
        assert_eq!(entries.len(), present.len(), "at {at:?}: {entries:?} for {present:?}");
        for (row, color) in present {
            let shard = INDEX_SHARD_BASE + index.shard_of(&color);
            let found = participants
                .get_mut(&shard)
                .unwrap()
                .scan_at(&index.value_prefix(&color), at)
                .unwrap();
            assert!(found.iter().any(|(_, primary_key)| *primary_key == row_key(row)));
        }
    }
}

fn world() -> (Placement, crate::regions::Latencies) {
    let mut latencies = crate::regions::Latencies::new();
    latencies.set("eu", "us", 80);
    latencies.set("eu", "ap", 160);
    latencies.set("us", "ap", 120);
    (
        placement(&[(1, "eu"), (2, "eu"), (3, "us"), (4, "us"), (5, "ap")]),
        latencies,
    )
}

#[test]
fn shards_move_toward_the_region_their_requests_come_from() {
    use crate::regions::{Traffic, place_by_latency};
    let (nodes, latencies) = world();
    let map = ShardMap::new(Strategy::Hash { shards: 5 }, &nodes);
    let mut traffic: Traffic = BTreeMap::new();
    for shard in map.shards.keys() {
        traffic.insert(*shard, [("ap".to_owned(), 900u64), ("eu".to_owned(), 100u64)].into_iter().collect());
    }
    let moves = place_by_latency(&map, &nodes, &traffic, &latencies);
    let ap_owned = map.shards.values().filter(|shard| nodes.nodes[&shard.owner] == "ap").count();
    assert_eq!(moves.len(), map.shards.len() - ap_owned);
    assert!(moves.iter().all(|(_, _, to)| nodes.nodes[to] == "ap"));
    let quiet: Traffic = BTreeMap::new();
    assert!(place_by_latency(&map, &nodes, &quiet, &latencies).is_empty());
    let pinned = ShardMap::new(
        Strategy::Region {
            regions: vec!["eu".to_owned(), "us".to_owned()],
        },
        &nodes,
    );
    let mut toward_ap: Traffic = BTreeMap::new();
    for shard in pinned.shards.keys() {
        toward_ap.insert(*shard, [("ap".to_owned(), 1_000u64)].into_iter().collect());
    }
    assert!(place_by_latency(&pinned, &nodes, &toward_ap, &latencies).is_empty());
}

#[test]
fn voters_spread_over_regions_survive_losing_one_and_commit_at_the_second_nearest() {
    use crate::regions::{choose_voters, quorum_latency, survives_region_loss};
    let (nodes, latencies) = world();
    let voters = choose_voters(&nodes, "eu", 3, &latencies).unwrap();
    let regions: Vec<String> = voters.iter().map(|voter| nodes.nodes[voter].clone()).collect();
    assert_eq!(regions, vec!["eu".to_owned(), "us".to_owned(), "ap".to_owned()]);
    assert!(survives_region_loss(&nodes, &voters));
    assert_eq!(quorum_latency("eu", &regions, &latencies), 80);
    let crowded = choose_voters(&nodes, "eu", 5, &latencies).unwrap();
    assert_eq!(crowded.len(), 5);
    assert!(!survives_region_loss(&nodes, &[1, 2, 3]));
    assert_eq!(choose_voters(&nodes, "eu", 6, &latencies), None);
    assert_eq!(latencies.between("eu", "mars"), 160);
}

#[test]
fn rebalancing_under_load_and_faults_loses_nothing_and_ends_even() {
    use crate::simulation::{SimulationConfig, simulate_rebalancing};
    let mut moved = 0;
    let mut refused_clocks = 0;
    for seed in 1..=25u64 {
        let report = simulate_rebalancing(SimulationConfig {
            seed,
            ..SimulationConfig::default()
        });
        assert_eq!(report.lost_writes, 0, "seed {seed}: {report:?}");
        assert_eq!(report.writes_by_non_owners, 0, "seed {seed}: {report:?}");
        assert!(report.final_imbalance <= 1, "seed {seed}: {report:?}");
        assert!(report.writes_acknowledged > 0, "seed {seed}: {report:?}");
        moved += report.moves_completed;
        refused_clocks += report.clock_refusals;
    }
    assert!(moved > 0);
    assert!(refused_clocks > 0, "the clock skew never exceeded the allowed offset");
}

#[test]
fn tenants_route_as_a_unit_and_move_to_a_shard_of_their_own() {
    use crate::tenants::{split_tenant_key, tenant_key, tenant_prefix};
    let nodes = placement(&[(1, "eu"), (2, "eu")]);
    let mut map = ShardMap::new(
        Strategy::Tenant {
            shards: 4,
            placed: BTreeMap::new(),
        },
        &nodes,
    );
    let first = tenant_key("acme", b"order 1").unwrap();
    let second = tenant_key("acme", b"order 2").unwrap();
    assert_eq!(route(&map.strategy, &first), route(&map.strategy, &second));
    assert_eq!(route(&map.strategy, b"plain key"), None);
    assert_eq!(split_tenant_key(&first), Some(("acme", &b"order 1"[..])));
    assert!(first.starts_with(&tenant_prefix("acme").unwrap()));
    assert!(!tenant_key("acmex", b"").unwrap().starts_with(&tenant_prefix("acme").unwrap()));
    assert_eq!(tenant_prefix(""), None);
    assert_eq!(tenant_prefix(&"x".repeat(256)), None);
    let epoch = map.epoch;
    assert!(map.place_tenant("acme", 9, 2));
    assert_eq!(map.epoch, epoch + 1);
    assert_eq!(map.tenant_shard("acme"), Some(9));
    assert_eq!(map.owner(&first), Some((2, map.epoch)));
    assert_eq!(ShardMap::decode(&map.encode()), Some(map.clone()));
    let mut hashed = ShardMap::new(Strategy::Hash { shards: 2 }, &nodes);
    assert!(!hashed.place_tenant("acme", 1, 1));
    assert_eq!(hashed.tenant_shard("acme"), None);
}

#[test]
fn whole_databases_move_from_the_fullest_server_while_the_gap_narrows() {
    use crate::databases::plan_database_moves;
    let servers: BTreeMap<String, Vec<(String, u64)>> = [
        (
            "a".to_owned(),
            vec![("shop".to_owned(), 100), ("blog".to_owned(), 50), ("wiki".to_owned(), 10)],
        ),
        ("b".to_owned(), Vec::new()),
        ("c".to_owned(), vec![("crm".to_owned(), 70)]),
    ]
    .into_iter()
    .collect();
    let moves = plan_database_moves(&servers);
    assert_eq!(moves[0], ("shop".to_owned(), "a".to_owned(), "b".to_owned()));
    let mut held: BTreeMap<String, u64> = servers
        .iter()
        .map(|(server, databases)| (server.clone(), databases.iter().map(|(_, size)| size).sum()))
        .collect();
    let sizes: BTreeMap<String, u64> = servers.values().flatten().cloned().collect();
    for (database, from, to) in &moves {
        *held.get_mut(from).unwrap() -= sizes[database];
        *held.get_mut(to).unwrap() += sizes[database];
    }
    let most = held.values().max().unwrap();
    let fewest = held.values().min().unwrap();
    assert!(most - fewest <= 100, "{held:?}");
    assert!(plan_database_moves(&BTreeMap::new()).is_empty());
    let even: BTreeMap<String, Vec<(String, u64)>> = [
        ("a".to_owned(), vec![("x".to_owned(), 10)]),
        ("b".to_owned(), vec![("y".to_owned(), 10)]),
    ]
    .into_iter()
    .collect();
    assert!(plan_database_moves(&even).is_empty());
}

#[test]
fn a_rolling_upgrade_raises_the_level_only_when_every_member_speaks_it() {
    use crate::versions::{JoinRefused, LevelRange, may_join, negotiated_level, upgrade_order};
    let old = LevelRange { oldest: 1, newest: 1 };
    let new = LevelRange { oldest: 1, newest: 2 };
    let members = [1, 2, 3];
    let mut ranges: BTreeMap<u64, LevelRange> = [(1, old), (2, old), (3, old)].into_iter().collect();
    assert_eq!(negotiated_level(1, &members, &ranges), 1);
    for node in upgrade_order(&members, Some(2)) {
        assert_eq!(may_join(negotiated_level(1, &members, &ranges), new), Ok(()));
        ranges.insert(node, new);
        let level = negotiated_level(1, &members, &ranges);
        assert_eq!(level, if node == 2 { 2 } else { 1 }, "after upgrading node {node}");
    }
    assert_eq!(upgrade_order(&members, Some(2)), vec![1, 3, 2]);
    assert_eq!(may_join(2, old), Err(JoinRefused::TooOld { cluster: 2, newest: 1 }));
    assert_eq!(
        may_join(1, LevelRange { oldest: 2, newest: 3 }),
        Err(JoinRefused::TooNew { cluster: 1, oldest: 2 })
    );
    ranges.remove(&3);
    assert_eq!(negotiated_level(2, &members, &ranges), 2);
    assert_eq!(negotiated_level(1, &members, &ranges), 1);
}
