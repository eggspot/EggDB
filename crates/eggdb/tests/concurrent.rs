//! Concurrent transactions: optimistic writers checked at commit, under snapshot isolation and
//! serializable isolation, against a checker that replays the committed ones in commit order.

mod common;

use common::TempPath;
use eggdb::{Database, ErrorKind, Isolation};
use std::collections::BTreeMap;

fn key(index: usize) -> Vec<u8> {
    format!("account:{index:02}").into_bytes()
}

fn number(value: Option<Vec<u8>>) -> i64 {
    value
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| text.parse().ok())
        .unwrap_or(0)
}

#[test]
fn the_first_committer_wins_a_write_write_conflict_in_both_isolations() {
    for isolation in [Isolation::Snapshot, Isolation::Serializable] {
        let file = TempPath::new("concurrent-ww");
        let db = Database::open(file.path()).unwrap();
        db.put("x", "1").unwrap();
        let mut first = db.begin_concurrent(isolation).unwrap();
        let mut second = db.begin_concurrent(isolation).unwrap();
        let seen = number(first.get("x").unwrap());
        first.put("x", (seen + 1).to_string()).unwrap();
        let seen = number(second.get("x").unwrap());
        second.put("x", (seen + 10).to_string()).unwrap();
        first.commit().unwrap();
        let error = second.commit().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Conflict, "{isolation:?}");
        assert_eq!(db.get("x").unwrap(), Some(b"2".to_vec()));
    }
}

#[test]
fn serializable_refuses_write_skew_that_snapshot_isolation_allows() {
    for (isolation, both_commit) in [(Isolation::Snapshot, true), (Isolation::Serializable, false)] {
        let file = TempPath::new("concurrent-skew");
        let db = Database::open(file.path()).unwrap();
        db.put("on_call:alice", "yes").unwrap();
        db.put("on_call:bob", "yes").unwrap();
        let mut alice = db.begin_concurrent(isolation).unwrap();
        let mut bob = db.begin_concurrent(isolation).unwrap();
        let on_call = |tx: &mut eggdb::ConcurrentTransaction<'_>| {
            tx.scan("on_call:".to_owned().."on_call;".to_owned())
                .unwrap()
                .iter()
                .filter(|(_, value)| value == b"yes")
                .count()
        };
        assert_eq!(on_call(&mut alice), 2);
        assert_eq!(on_call(&mut bob), 2);
        alice.put("on_call:alice", "no").unwrap();
        bob.put("on_call:bob", "no").unwrap();
        alice.commit().unwrap();
        assert_eq!(bob.commit().is_ok(), both_commit, "{isolation:?}");
    }
}

#[test]
fn a_phantom_in_a_scanned_range_fails_a_serializable_commit() {
    let file = TempPath::new("concurrent-phantom");
    let db = Database::open(file.path()).unwrap();
    let mut reader = db.begin_concurrent(Isolation::Serializable).unwrap();
    assert!(reader.scan("order:".to_owned().."order;".to_owned()).unwrap().is_empty());
    reader.put("summary", "0 orders").unwrap();
    db.put("order:1", "new").unwrap();
    assert_eq!(reader.commit().unwrap_err().kind(), ErrorKind::Conflict);
    assert_eq!(db.get("summary").unwrap(), None);
}

#[derive(Debug, Clone)]
struct Recorded {
    reads: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    writes: Vec<(Vec<u8>, Vec<u8>)>,
}

#[test]
fn committed_transfers_match_running_them_one_at_a_time_in_commit_order() {
    let file = TempPath::new("concurrent-model");
    let db = Database::open(file.path()).unwrap();
    for index in 0..8 {
        db.put(key(index), "100").unwrap();
    }
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = |bound: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % bound as u64) as usize
    };
    let mut committed: Vec<Recorded> = Vec::new();
    let mut conflicts = 0;
    for _ in 0..200 {
        let mut open = Vec::new();
        for _ in 0..3 {
            let mut tx = db.begin_concurrent(Isolation::Serializable).unwrap();
            let (from, to) = (next(8), next(8));
            let amount = next(30) as i64;
            let from_balance = tx.get(key(from)).unwrap();
            let to_balance = tx.get(key(to)).unwrap();
            let mut recorded = Recorded {
                reads: vec![(key(from), from_balance.clone()), (key(to), to_balance.clone())],
                writes: Vec::new(),
            };
            if from != to && number(from_balance.clone()) >= amount {
                let from_value = (number(from_balance) - amount).to_string().into_bytes();
                let to_value = (number(to_balance) + amount).to_string().into_bytes();
                tx.put(key(from), &from_value).unwrap();
                tx.put(key(to), &to_value).unwrap();
                recorded.writes = vec![(key(from), from_value), (key(to), to_value)];
            }
            open.push((tx, recorded));
        }
        while !open.is_empty() {
            let (tx, recorded) = open.remove(next(open.len()));
            match tx.commit() {
                Ok(_) => committed.push(recorded),
                Err(error) => {
                    assert_eq!(error.kind(), ErrorKind::Conflict);
                    conflicts += 1;
                }
            }
        }
    }
    assert!(conflicts > 0, "some transactions overlapped");
    let mut serial: BTreeMap<Vec<u8>, Vec<u8>> =
        (0..8).map(|index| (key(index), b"100".to_vec())).collect();
    for recorded in &committed {
        for (key, seen) in &recorded.reads {
            assert_eq!(serial.get(key), seen.as_ref(), "a committed read differs from serial order");
        }
        for (key, value) in &recorded.writes {
            serial.insert(key.clone(), value.clone());
        }
    }
    let total: i64 = (0..8).map(|index| number(db.get(key(index)).unwrap())).sum();
    assert_eq!(total, 800);
    for (key, value) in serial {
        assert_eq!(db.get(&key).unwrap(), Some(value));
    }
}

#[test]
fn transactions_on_many_threads_keep_every_transfer() {
    let file = TempPath::new("concurrent-threads");
    let db = Database::open(file.path()).unwrap();
    for index in 0..4 {
        db.put(key(index), "1000").unwrap();
    }
    std::thread::scope(|scope| {
        for thread in 0..4 {
            let db = &db;
            scope.spawn(move || {
                let mut done = 0;
                while done < 50 {
                    let mut tx = db.begin_concurrent(Isolation::Serializable).unwrap();
                    let (from, to) = (thread, (thread + 1) % 4);
                    let from_balance = number(tx.get(key(from)).unwrap());
                    let to_balance = number(tx.get(key(to)).unwrap());
                    tx.put(key(from), (from_balance - 1).to_string()).unwrap();
                    tx.put(key(to), (to_balance + 1).to_string()).unwrap();
                    match tx.commit() {
                        Ok(_) => done += 1,
                        Err(error) => assert_eq!(error.kind(), ErrorKind::Conflict),
                    }
                }
            });
        }
    });
    let total: i64 = (0..4).map(|index| number(db.get(key(index)).unwrap())).sum();
    assert_eq!(total, 4000);
}
