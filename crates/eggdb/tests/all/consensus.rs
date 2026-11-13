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
