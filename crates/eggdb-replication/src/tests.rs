use crate::{Apply, Batch, Candidate, Change, Mode, Primary, ReadToken, Refusal, Replica, decode_batch, elect, encode_batch};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq)]
struct Memory {
    keys: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl Apply for Memory {
    fn apply(&mut self, batch: &Batch) -> Result<(), String> {
        for change in &batch.changes {
            match change {
                Change::Put { key, value, .. } => {
                    self.keys.insert(key.clone(), value.clone());
                }
                Change::Delete { key, .. } => {
                    self.keys.remove(key);
                }
            }
        }
        Ok(())
    }
}

fn put(key: u64) -> Vec<Change> {
    vec![Change::Put { collection: String::new(), key: key.to_be_bytes().to_vec(), value: vec![1] }]
}

#[test]
fn replicas_apply_in_order_and_refuse_gaps_and_stale_terms() {
    let mut primary = Primary::new(1, 0, Mode::Asynchronous);
    let mut replica = Replica::new(7, 1, 0, Memory::default(), 16);
    let (first, _) = primary.commit(put(1));
    let (second, token) = primary.commit(put(2));
    assert_eq!(replica.receive(&second), Err(Refusal::Gap { next: 1 }));
    assert_eq!(replica.receive(&first), Ok(1));
    assert!(!replica.can_serve(token));
    assert_eq!(replica.receive(&second), Ok(2));
    assert!(replica.can_serve(token));
    assert_eq!(replica.receive(&first), Ok(2));
    replica.follow_term(2);
    let stale = Batch { term: 1, sequence: 3, changes: put(3) };
    assert_eq!(replica.receive(&stale), Err(Refusal::StaleTerm { current: 2 }));
}

#[test]
fn synchronous_commits_wait_for_acknowledgements_and_logs_are_trimmed() {
    let mut primary = Primary::new(1, 0, Mode::Synchronous { acknowledgements: 2 });
    primary.add_replica(1, 0);
    primary.add_replica(2, 0);
    let (batch, _) = primary.commit(put(1));
    assert!(!primary.is_durable(batch.sequence));
    primary.acknowledge(1, 1);
    assert!(!primary.is_durable(1));
    primary.acknowledge(2, 1);
    assert!(primary.is_durable(1));
    assert_eq!(primary.batches_after(0, 10), Ok(Vec::new()));
    primary.commit(put(2));
    assert_eq!(primary.batches_after(1, 10).unwrap().len(), 1);
    assert_eq!(primary.batches_after(0, 10), Err(2));
}

#[test]
fn cascading_replicas_follow_another_replica() {
    let mut primary = Primary::new(1, 0, Mode::Asynchronous);
    let mut middle = Replica::new(1, 1, 0, Memory::default(), 8);
    let mut leaf = Replica::new(2, 1, 0, Memory::default(), 8);
    for key in 0..5 {
        let (batch, _) = primary.commit(put(key));
        middle.receive(&batch).unwrap();
    }
    for batch in middle.batches_after(leaf.applied()).unwrap() {
        leaf.receive(&batch).unwrap();
    }
    assert_eq!(leaf.storage(), middle.storage());
}

#[test]
fn election_prefers_the_newest_term_then_the_most_applied() {
    let candidates = [
        Candidate { id: 3, term: 1, applied: 90 },
        Candidate { id: 1, term: 2, applied: 10 },
        Candidate { id: 2, term: 2, applied: 10 },
    ];
    assert_eq!(elect(&candidates), Some(1));
    assert_eq!(elect(&[]), None);
}

#[test]
fn batches_cross_the_wire_and_damaged_bytes_are_refused() {
    let batch = Batch {
        term: 4,
        sequence: 9,
        changes: vec![
            Change::Put { collection: "resp0".to_owned(), key: b"k".to_vec(), value: b"v".to_vec() },
            Change::Delete { collection: String::new(), key: b"gone".to_vec() },
        ],
    };
    let bytes = encode_batch(&batch);
    assert_eq!(decode_batch(&bytes).unwrap(), batch);
    for cut in 0..bytes.len() {
        assert!(decode_batch(&bytes[..cut]).is_err());
    }
    let mut huge = bytes.clone();
    huge[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(decode_batch(&huge).is_err());
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

#[test]
fn simulated_partitions_never_lose_a_synchronous_commit_on_failover() {
    for seed in 1..=40u64 {
        let mut rng = Rng(seed * 0x9e37_79b9_7f4a_7c15);
        let mut primary = Primary::new(1, 0, Mode::Synchronous { acknowledgements: 1 });
        let mut replicas: Vec<Replica<Memory>> = (1..=2).map(|id| Replica::new(id, 1, 0, Memory::default(), 1_000)).collect();
        for replica in &replicas {
            primary.add_replica(replica.id(), 0);
        }
        let mut durable: Vec<u64> = Vec::new();
        let mut written: Vec<(u64, u64)> = Vec::new();
        for key in 0..200u64 {
            let (batch, token) = primary.commit(put(key));
            written.push((key, token.sequence));
            for replica in &mut replicas {
                let reachable = rng.next() % 4 != 0;
                if !reachable {
                    continue;
                }
                if let Ok(batches) = primary.batches_after(replica.applied(), 50) {
                    for pending in batches {
                        if replica.receive(&pending).is_err() {
                            break;
                        }
                    }
                }
                let applied = replica.applied();
                primary.acknowledge(replica.id(), applied);
            }
            if primary.is_durable(batch.sequence) {
                durable.push(key);
            }
        }
        for (key, sequence) in &written {
            if primary.is_durable(*sequence) && !durable.contains(key) {
                durable.push(*key);
            }
        }
        let candidates: Vec<Candidate> = replicas.iter().map(|replica| Candidate { id: replica.id(), term: replica.term(), applied: replica.applied() }).collect();
        let chosen = elect(&candidates).unwrap();
        let winner = replicas.into_iter().find(|replica| replica.id() == chosen).unwrap();
        for key in &durable {
            assert!(winner.storage().keys.contains_key(&key.to_be_bytes().to_vec()), "seed {seed}: durable key {key} lost");
        }
        let applied = winner.applied();
        let (new_primary, storage) = winner.promote(1, Mode::Asynchronous);
        assert_eq!(new_primary.term(), 2);
        assert_eq!(storage.keys.len() as u64, applied);
        assert!(ReadToken { sequence: applied } <= ReadToken { sequence: applied + 1 });
    }
}
