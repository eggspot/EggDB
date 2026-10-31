use crate::transaction::{Coordinator, Decision, Participant, Vote, recover};
use crate::{Placement, ShardMap, ShardMove, Strategy, rebalance, route};
use std::collections::BTreeMap;

fn placement(nodes: &[(u64, &str)]) -> Placement {
    Placement { nodes: nodes.iter().map(|(node, region)| (*node, (*region).to_owned())).collect() }
}

#[test]
fn keys_route_by_each_strategy() {
    let hash = Strategy::Hash { shards: 8 };
    let mut counts = [0u32; 8];
    for key in 0..8_000u32 {
        counts[route(&hash, &key.to_be_bytes()).unwrap() as usize] += 1;
    }
    assert!(counts.iter().all(|count| (700..1_300).contains(count)), "{counts:?}");
    let range = Strategy::Range { bounds: vec![b"g".to_vec(), b"p".to_vec()] };
    assert_eq!([route(&range, b"apple"), route(&range, b"grape"), route(&range, b"zebra")], [Some(0), Some(1), Some(2)]);
    let time = Strategy::Time { bucket_ms: 86_400_000 };
    assert_eq!(route(&time, &(86_400_000u64 * 3 + 5).to_be_bytes()), Some(3));
    assert_eq!(route(&time, b"short"), None);
    let region = Strategy::Region { regions: vec!["eu".to_owned(), "us".to_owned()] };
    assert_eq!(route(&region, b"us\0order-1"), Some(1));
    assert_eq!(route(&region, b"ap\0order-1"), None);
}

#[test]
fn rebalancing_evens_out_load_and_keeps_regions() {
    let mut map = ShardMap::new(Strategy::Hash { shards: 12 }, &placement(&[(1, "eu"), (2, "eu")]));
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
    let pinned = ShardMap::new(Strategy::Region { regions: vec!["eu".to_owned(), "us".to_owned()] }, &regions);
    let without_us = placement(&[(1, "eu"), (3, "eu")]);
    assert!(rebalance(&pinned, &without_us).is_empty(), "a pinned shard never leaves its region");
    let shrunk = placement(&[(1, "eu"), (2, "us")]);
    let moved = rebalance(&pinned, &shrunk);
    assert!(moved.iter().all(|(_, _, to)| shrunk.nodes.contains_key(to)));
}

#[test]
fn an_online_move_cuts_over_only_when_caught_up_and_refuses_stale_epochs() {
    let mut map = ShardMap::new(Strategy::Hash { shards: 4 }, &placement(&[(1, "eu"), (2, "eu")]));
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
    participants[&(account % 3)].committed(&account.to_be_bytes()).map_or(0, |bytes| i64::from_be_bytes(bytes.as_slice().try_into().unwrap()))
}

#[test]
fn transfers_across_shards_keep_the_total_through_conflicts_and_coordinator_crashes() {
    for seed in 1..=20u64 {
        let mut rng = Rng(seed * 7919);
        let mut participants: BTreeMap<u32, Participant> = (0..3).map(|shard| (shard, Participant::default())).collect();
        let mut coordinator = Coordinator::default();
        let accounts = 9u32;
        let setup = coordinator.begin();
        for account in 0..accounts {
            let shard = participants.get_mut(&(account % 3)).unwrap();
            assert_eq!(shard.prepare(setup, vec![(account.to_be_bytes().to_vec(), Some(100i64.to_be_bytes().to_vec()))]), Vote::Yes);
        }
        coordinator.decide(setup, &[Vote::Yes]);
        recover(&coordinator, &mut participants);
        let mut pending: Vec<(u64, u32, u32)> = Vec::new();
        for _ in 0..300 {
            let from = (rng.next() % u64::from(accounts)) as u32;
            let to = (rng.next() % u64::from(accounts)) as u32;
            if from == to {
                continue;
            }
            let transaction = coordinator.begin();
            let reads = (
                participants.get_mut(&(from % 3)).unwrap().read(transaction, &from.to_be_bytes()),
                participants.get_mut(&(to % 3)).unwrap().read(transaction, &to.to_be_bytes()),
            );
            let (Ok(Some(from_bytes)), Ok(Some(to_bytes))) = reads else {
                for participant in participants.values_mut() {
                    participant.decide(transaction, Decision::Abort);
                }
                continue;
            };
            let amount = (rng.next() % 30) as i64;
            let from_balance = i64::from_be_bytes(from_bytes.as_slice().try_into().unwrap()) - amount;
            let to_balance = i64::from_be_bytes(to_bytes.as_slice().try_into().unwrap()) + amount;
            let votes = [
                participants.get_mut(&(from % 3)).unwrap().prepare(transaction, vec![(from.to_be_bytes().to_vec(), Some(from_balance.to_be_bytes().to_vec()))]),
                participants.get_mut(&(to % 3)).unwrap().prepare(transaction, vec![(to.to_be_bytes().to_vec(), Some(to_balance.to_be_bytes().to_vec()))]),
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
            if rng.next() % 7 == 0 {
                recover(&coordinator, &mut participants);
                pending.clear();
            }
        }
        recover(&coordinator, &mut participants);
        let total: i64 = (0..accounts).map(|account| balance(&participants, account)).sum();
        assert_eq!(total, 100 * i64::from(accounts), "seed {seed}");
        assert!(participants.values().all(|participant| participant.in_doubt().is_empty()));
    }
}
