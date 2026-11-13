//! The Raft state a database file keeps for its node of a cluster: stored, read back after a
//! reopen, cut back by a rewritten suffix and dropped below a snapshot.

use crate::common::TempPath;
use eggdb::Database;
use eggdb::consensus::{Changes, Entry, EntryKind, HardState, Membership, Node, Snapshot, Timing};

fn entry(term: u64, index: u64) -> Entry {
    Entry {
        term,
        index,
        kind: EntryKind::Command,
        command: format!("command {index}").into_bytes(),
    }
}

#[test]
fn the_term_vote_and_log_survive_a_reopen() {
    let file = TempPath::new("consensus-reopen");
    let db = Database::open(file.path()).unwrap();
    assert_eq!(db.consensus_state().unwrap().entries, Vec::new());
    db.save_consensus(&Changes {
        hard_state: Some(HardState {
            term: 3,
            voted_for: Some(2),
        }),
        entries_from: Some(1),
        entries: (1..=5).map(|index| entry(3, index)).collect(),
        ..Changes::default()
    })
    .unwrap();
    db.save_consensus(&Changes {
        entries_from: Some(4),
        entries: vec![entry(4, 4)],
        ..Changes::default()
    })
    .unwrap();
    drop(db);
    let db = Database::open(file.path()).unwrap();
    let recovered = db.consensus_state().unwrap();
    assert_eq!(
        recovered.hard_state,
        HardState {
            term: 3,
            voted_for: Some(2)
        }
    );
    assert_eq!(
        recovered
            .entries
            .iter()
            .map(|entry| (entry.index, entry.term))
            .collect::<Vec<_>>(),
        vec![(1, 3), (2, 3), (3, 3), (4, 4)]
    );
    assert_eq!(db.begin_read().unwrap().scan_prefix(b"").count(), 0);
    let node = Node::recover(1, &[1, 2, 3], Timing::default(), 1, recovered).unwrap();
    assert_eq!(node.term(), 3);
    assert_eq!(node.log().len(), 4);
}

#[test]
fn a_snapshot_drops_the_entries_it_covers() {
    let file = TempPath::new("consensus-snapshot");
    let db = Database::open(file.path()).unwrap();
    db.save_consensus(&Changes {
        entries_from: Some(1),
        entries: (1..=6).map(|index| entry(1, index)).collect(),
        ..Changes::default()
    })
    .unwrap();
    db.save_consensus(&Changes {
        compacted: Some(Snapshot {
            index: 4,
            term: 1,
            membership: Membership::new(&[1, 2, 3]),
            data: Vec::new(),
        }),
        ..Changes::default()
    })
    .unwrap();
    let recovered = db.consensus_state().unwrap();
    assert_eq!(
        recovered.snapshot.as_ref().map(|snapshot| (snapshot.index, snapshot.term)),
        Some((4, 1))
    );
    assert_eq!(
        recovered
            .entries
            .iter()
            .map(|entry| entry.index)
            .collect::<Vec<_>>(),
        vec![5, 6]
    );
    assert_eq!(
        recovered.snapshot.as_ref().map(|snapshot| snapshot.membership.clone()),
        Some(Membership::new(&[1, 2, 3]))
    );
    let node = Node::recover(1, &[1], Timing::default(), 1, recovered).unwrap();
    assert_eq!(node.snapshot_position(), (4, 1));
    assert_eq!(node.membership(), &Membership::new(&[1, 2, 3]));
    assert_eq!(node.commit_index(), 4);
}

#[test]
fn the_shard_map_is_published_only_over_the_epoch_it_was_based_on() {
    use eggdb::cluster::{Placement, ShardMap, Strategy};
    let file = TempPath::new("cluster-map");
    let db = Database::open(file.path()).unwrap();
    assert_eq!(db.shard_map().unwrap(), None);
    let placement = Placement {
        nodes: [(1, "eu".to_owned()), (2, "eu".to_owned())].into_iter().collect(),
    };
    let first = ShardMap::new(Strategy::Hash { shards: 4 }, &placement);
    assert!(!db.publish_shard_map(&first, Some(1)).unwrap());
    assert!(db.publish_shard_map(&first, None).unwrap());
    let mut moved = first.clone();
    moved.shards.get_mut(&0).unwrap().owner = 2;
    moved.epoch = first.epoch + 1;
    assert!(!db.publish_shard_map(&moved, None).unwrap());
    assert!(db.publish_shard_map(&moved, Some(first.epoch)).unwrap());
    assert!(!db.publish_shard_map(&moved, Some(first.epoch)).unwrap());
    drop(db);
    let db = Database::open(file.path()).unwrap();
    assert_eq!(db.shard_map().unwrap(), Some(moved));
    db.fence("a follower");
    assert!(db.publish_shard_map(&first, Some(2)).is_err());
}

#[test]
fn decisions_are_recorded_once_and_outlive_the_coordinator() {
    use eggdb::cluster::transaction::{Decision, DecisionLog, Vote};
    let file = TempPath::new("cluster-decisions");
    let db = Database::open(file.path()).unwrap();
    let mut log = db.decision_log();
    let first = log.begin().unwrap();
    let second = log.begin().unwrap();
    assert_eq!(second, first + 1);
    assert_eq!(log.decide(first, &[Vote::Yes, Vote::Yes]).unwrap(), Decision::Commit);
    assert_eq!(log.decide(first, &[Vote::No]).unwrap(), Decision::Commit);
    assert_eq!(log.outcome(second).unwrap(), Decision::Abort);
    assert_eq!(log.decide(second, &[Vote::Yes]).unwrap(), Decision::Abort);
    drop(db);
    let db = Database::open(file.path()).unwrap();
    let log = db.decision_log();
    assert_eq!(log.recorded(first).unwrap(), Some(Decision::Commit));
    log.forget(first).unwrap();
    assert_eq!(log.recorded(first).unwrap(), None);
    let mut log = db.decision_log();
    assert_eq!(log.begin().unwrap(), second + 1);
}

#[test]
fn a_shard_is_copied_page_by_page_and_caught_up_from_batches() {
    use eggdb::cluster::{Placement, ShardMap, Strategy, route};
    use eggdb::{ReplicationBatch, decode_shard_changes, encode_shard_changes, shard_changes};
    let source_file = TempPath::new("shard-source");
    let target_file = TempPath::new("shard-target");
    let source = Database::open(source_file.path()).unwrap();
    let target = Database::open(target_file.path()).unwrap();
    let placement = Placement {
        nodes: [(1, "eu".to_owned()), (2, "eu".to_owned())].into_iter().collect(),
    };
    let map = ShardMap::new(Strategy::Hash { shards: 4 }, &placement);
    for index in 0..500u32 {
        source.put(format!("key {index}"), format!("value {index}")).unwrap();
    }
    let shard = route(&map.strategy, b"key 7").unwrap();
    let mut after: Option<Vec<u8>> = None;
    let mut copied = 0;
    loop {
        let page = source.shard_page(&map, shard, after.as_deref(), 64).unwrap();
        assert!(page.entries.iter().all(|(key, _)| route(&map.strategy, key) == Some(shard)));
        let changes: Vec<eggdb::ShardChange> = page
            .entries
            .into_iter()
            .map(|(key, value)| (key, Some(value)))
            .collect();
        copied += changes.len();
        let wire = encode_shard_changes(&changes);
        target
            .apply_shard_changes(&decode_shard_changes(&wire).unwrap())
            .unwrap();
        match page.next_after {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    let expected = (0..500u32)
        .filter(|index| route(&map.strategy, format!("key {index}").as_bytes()) == Some(shard))
        .count();
    assert_eq!(copied, expected);
    assert_eq!(target.get(b"key 7").unwrap(), Some(b"value 7".to_vec()));
    let batch = ReplicationBatch {
        term: 1,
        sequence: 9,
        previous_term: 1,
        changes: vec![
            eggdb::ReplicationChange::Put {
                collection: String::new(),
                key: b"key 7".to_vec(),
                value: b"changed".to_vec(),
            },
            eggdb::ReplicationChange::Put {
                collection: String::new(),
                key: b"\xffCOL\x00\x7fraft\x00state".to_vec(),
                value: b"x".to_vec(),
            },
        ],
    };
    let caught_up = shard_changes(&batch, &map, shard);
    assert_eq!(caught_up, vec![(b"key 7".to_vec(), Some(b"changed".to_vec()))]);
    target.apply_shard_changes(&caught_up).unwrap();
    assert_eq!(target.get(b"key 7").unwrap(), Some(b"changed".to_vec()));
    assert!(
        target
            .apply_shard_changes(&[(b"\xffCOL\x00\x7fraft\x00state".to_vec(), None)])
            .is_err()
    );
    assert_eq!(decode_shard_changes(&[9, 0, 0, 0]), None);
}
