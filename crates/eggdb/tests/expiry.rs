//! Expiring keys and atomic counters through the embedded API, on a simulated clock: keys that
//! expire exactly at their deadline across reopens and crashes, in the default key space and in
//! collections, increments from many threads that never lose a count, and compare-and-set.

mod common;

use common::TempPath;
use eggdb::{Database, Durability, ErrorKind, Options, SimClock};
use std::sync::Arc;
use std::time::Duration;

const OCTOBER_2026_NANOS: u64 = 1_792_000_000_000_000_000;
const MICRO: Duration = Duration::from_micros(1);
const TTL: Duration = Duration::from_secs(30);

fn clock() -> Arc<SimClock> {
    Arc::new(SimClock::new(OCTOBER_2026_NANOS))
}

fn open(file: &TempPath, clock: &Arc<SimClock>) -> Database {
    Database::open_with(file.path(), Options::new().clock(clock.clone())).unwrap()
}

fn keys(db: &Database, collection: Option<&str>) -> Vec<Vec<u8>> {
    db.read(|tx| {
        let scan = match collection {
            Some(name) => tx.collection(name)?.scan(..).collect::<eggdb::Result<Vec<_>>>()?,
            None => tx.scan(..).collect::<eggdb::Result<Vec<_>>>()?,
        };
        Ok(scan.into_iter().map(|(key, _)| key).collect())
    })
    .unwrap()
}

fn keys_reversed(db: &Database, collection: Option<&str>) -> Vec<Vec<u8>> {
    let mut found: Vec<Vec<u8>> = db
        .read(|tx| {
            let scan = match collection {
                Some(name) => tx
                    .collection(name)?
                    .scan(..)
                    .rev()
                    .collect::<eggdb::Result<Vec<_>>>()?,
                None => tx.scan(..).rev().collect::<eggdb::Result<Vec<_>>>()?,
            };
            Ok(scan.into_iter().map(|(key, _)| key).collect())
        })
        .unwrap();
    found.reverse();
    found
}

fn owned(names: &[&str]) -> Vec<Vec<u8>> {
    names.iter().map(|name| name.as_bytes().to_vec()).collect()
}

#[test]
fn a_key_expires_exactly_at_its_deadline_across_a_crash_and_a_reopen() {
    let file = TempPath::new("expiry-deadline");
    let clock = clock();
    let db = open(&file, &clock);
    db.create_collection("sessions").unwrap();
    db.write(|tx| {
        tx.put("a", "kept")?;
        tx.put_with_expiry("b", "expiring", TTL)?;
        tx.put("c", "kept")?;
        let mut sessions = tx.collection("sessions")?;
        sessions.put_with_expiry("s1", "alice", TTL)?;
        sessions.put("s2", "bob")?;
        Ok(())
    })
    .unwrap();
    clock.advance(TTL - MICRO);
    assert_eq!(db.get("b").unwrap(), Some(b"expiring".to_vec()));
    assert_eq!(db.time_to_live("b").unwrap(), Some(MICRO));
    assert_eq!(keys(&db, None), owned(&["a", "b", "c"]));
    assert_eq!(keys(&db, Some("sessions")), owned(&["s1", "s2"]));
    drop(db);

    let db = open(&file, &clock);
    assert_eq!(db.collection("sessions").get("s1").unwrap(), Some(b"alice".to_vec()));
    assert_eq!(db.collection("sessions").time_to_live("s1").unwrap(), Some(MICRO));
    clock.advance(MICRO);
    assert_eq!(db.get("b").unwrap(), None);
    assert_eq!(db.get_with_len("b"), None);
    assert_eq!(db.time_to_live("b").unwrap(), None);
    assert_eq!(db.collection("sessions").get("s1").unwrap(), None);
    for (collection, expected) in [(None, &["a", "c"][..]), (Some("sessions"), &["s2"][..])] {
        assert_eq!(keys(&db, collection), owned(expected));
        assert_eq!(keys_reversed(&db, collection), owned(expected));
    }
    let before_sweep = db.check().unwrap().entries;
    db.put("d", "a commit sweeps").unwrap();
    assert_eq!(db.check().unwrap().entries, before_sweep + 1 - 6);
    db.close().unwrap();

    let db = open(&file, &clock);
    assert_eq!(keys(&db, None), owned(&["a", "c", "d"]));
    assert_eq!(keys(&db, Some("sessions")), owned(&["s2"]));
    db.close().unwrap();
}

trait LengthOf {
    fn get_with_len(&self, key: &str) -> Option<usize>;
}

impl LengthOf for Database {
    fn get_with_len(&self, key: &str) -> Option<usize> {
        self.read(|tx| tx.get_with(key, <[u8]>::len)).unwrap()
    }
}

#[test]
fn a_wall_clock_set_back_after_a_commit_never_brings_an_expired_key_back() {
    let file = TempPath::new("expiry-clock-back");
    let clock = clock();
    let db = open(&file, &clock);
    db.write(|tx| {
        for index in 0..40 {
            tx.put_with_expiry(format!("k{index:02}"), "v", Duration::from_secs(10))?;
        }
        Ok(())
    })
    .unwrap();
    clock.advance(Duration::from_secs(20));
    db.put("other", "x").unwrap();
    clock.set_unix_time_nanos(OCTOBER_2026_NANOS);
    assert_eq!(keys(&db, None), owned(&["other"]));
    assert_eq!(db.get("k39").unwrap(), None);
    drop(db);
    let db = open(&file, &clock);
    assert_eq!(db.get("k39").unwrap(), None);
    assert_eq!(db.remove_expired().unwrap(), 8);
    assert_eq!(db.check().unwrap().entries, 1);
    db.close().unwrap();
}

#[test]
fn a_reader_keeps_the_time_its_transaction_began_with() {
    let file = TempPath::new("expiry-snapshot");
    let clock = clock();
    let db = open(&file, &clock);
    db.put_with_expiry("k", "v", TTL).unwrap();
    let before = db.begin_read().unwrap();
    clock.advance(TTL);
    assert_eq!(before.get("k").unwrap(), Some(b"v".to_vec()));
    assert_eq!(before.scan(..).count(), 1);
    assert_eq!(db.get("k").unwrap(), None);
    drop(before);
    db.close().unwrap();
}

#[test]
fn a_read_only_open_hides_expired_keys_without_removing_them() {
    let file = TempPath::new("expiry-read-only");
    let clock = clock();
    let db = open(&file, &clock);
    db.put_with_expiry("k", "v", TTL).unwrap();
    db.put("kept", "v").unwrap();
    db.close().unwrap();
    clock.advance(TTL);
    let options = Options::new().read_only(true).clock(clock.clone());
    let reader = Database::open_with(file.path(), options).unwrap();
    assert_eq!(reader.get("k").unwrap(), None);
    assert_eq!(keys(&reader, None), owned(&["kept"]));
    assert_eq!(reader.check().unwrap().entries, 4);
    assert_eq!(reader.remove_expired().unwrap_err().kind(), ErrorKind::ReadOnly);
}

#[test]
fn put_replaces_a_time_to_live_and_increment_keeps_it() {
    let file = TempPath::new("expiry-put-increment");
    let clock = clock();
    let db = open(&file, &clock);
    db.put_with_expiry("put", "v", TTL).unwrap();
    db.put("put", "w").unwrap();
    assert_eq!(db.time_to_live("put").unwrap(), None);
    db.put_with_expiry("count", "5", TTL).unwrap();
    assert_eq!(db.increment("count", 3).unwrap(), 8);
    assert_eq!(db.time_to_live("count").unwrap(), Some(TTL));
    clock.advance(TTL);
    assert_eq!(db.get("put").unwrap(), Some(b"w".to_vec()));
    assert_eq!(db.get("count").unwrap(), None);
    assert_eq!(db.increment("count", 1).unwrap(), 1);
    assert_eq!(db.time_to_live("count").unwrap(), None);
    clock.advance(TTL);
    assert_eq!(db.get("count").unwrap(), Some(b"1".to_vec()));
    db.close().unwrap();
}

#[test]
fn set_and_clear_expiry_act_only_on_keys_that_are_there() {
    let file = TempPath::new("expiry-set-clear");
    let clock = clock();
    let db = open(&file, &clock);
    assert!(!db.set_expiry("absent", TTL).unwrap());
    assert_eq!(db.get("absent").unwrap(), None);
    db.put("k", "v").unwrap();
    assert!(!db.clear_expiry("k").unwrap());
    assert!(db.set_expiry("k", TTL).unwrap());
    assert!(db.set_expiry("k", 2 * TTL).unwrap());
    assert_eq!(db.time_to_live("k").unwrap(), Some(2 * TTL));
    assert!(db.clear_expiry("k").unwrap());
    assert_eq!(db.time_to_live("k").unwrap(), None);
    clock.advance(4 * TTL);
    assert_eq!(db.get("k").unwrap(), Some(b"v".to_vec()));
    assert!(db.set_expiry("k", TTL).unwrap());
    clock.advance(TTL);
    assert!(!db.set_expiry("k", TTL).unwrap());
    assert!(!db.clear_expiry("k").unwrap());
    assert_eq!(db.get("k").unwrap(), None);
    db.put("zero", "v").unwrap();
    assert!(db.set_expiry("zero", Duration::ZERO).unwrap());
    assert_eq!(db.get("zero").unwrap(), None);
    db.put_with_expiry("zero", "v", Duration::ZERO).unwrap();
    assert_eq!(db.get("zero").unwrap(), None);
    assert_eq!(db.check().unwrap().entries, 0);
    db.close().unwrap();
}

#[test]
fn expired_keys_are_absent_to_every_write() {
    let file = TempPath::new("expiry-writes");
    let clock = clock();
    let db = open(&file, &clock);
    db.write(|tx| {
        tx.put_with_expiry("a", "old", TTL)?;
        tx.put_with_expiry("b", "old", TTL)?;
        tx.put_with_expiry("c", "old", TTL)?;
        Ok(())
    })
    .unwrap();
    clock.advance(TTL);
    db.write(|tx| {
        assert!(tx.put_if_absent("a", "new")?);
        assert_eq!(tx.time_to_live("a")?, None);
        assert!(!tx.delete("b")?);
        assert!(tx.compare_and_set("c", None, Some(b"new".as_slice()))?);
        Ok(())
    })
    .unwrap();
    clock.advance(4 * TTL);
    assert_eq!(keys(&db, None), owned(&["a", "c"]));
    assert_eq!(db.check().unwrap().entries, 2);
    db.close().unwrap();
}

#[test]
fn increments_from_many_threads_sum_exactly() {
    let file = TempPath::new("expiry-threads");
    let options = Options::new()
        .durability(Durability::Normal)
        .write_timeout(Duration::from_secs(60));
    let db = Database::open_with(file.path(), options).unwrap();
    db.create_collection("stats").unwrap();
    std::thread::scope(|scope| {
        for thread in 0..8 {
            let db = &db;
            scope.spawn(move || {
                for _ in 0..100 {
                    db.increment("hits", 1).unwrap();
                    db.collection("stats").increment("hits", 2).unwrap();
                    if thread % 2 == 0 {
                        db.increment("balance", -3).unwrap();
                    }
                }
            });
        }
    });
    assert_eq!(db.get("hits").unwrap(), Some(b"800".to_vec()));
    assert_eq!(db.collection("stats").get("hits").unwrap(), Some(b"1600".to_vec()));
    assert_eq!(db.get("balance").unwrap(), Some(b"-1200".to_vec()));
    db.close().unwrap();
}

#[test]
fn a_value_that_is_not_a_counter_or_an_overflow_changes_nothing() {
    let file = TempPath::new("expiry-not-a-counter");
    let db = Database::open(file.path()).unwrap();
    db.put("name", "Ada").unwrap();
    db.put("big", i64::MAX.to_string()).unwrap();
    let mut tx = db.begin_write().unwrap();
    assert_eq!(tx.increment("name", 1).unwrap_err().kind(), ErrorKind::NotAnInteger);
    assert_eq!(tx.increment("big", 1).unwrap_err().kind(), ErrorKind::NotAnInteger);
    assert_eq!(tx.increment("big", -1).unwrap(), i64::MAX - 1);
    tx.commit().unwrap();
    assert_eq!(db.get("name").unwrap(), Some(b"Ada".to_vec()));
    assert_eq!(db.get("big").unwrap(), Some((i64::MAX - 1).to_string().into_bytes()));
    assert_eq!(db.increment("fresh", i64::MIN).unwrap(), i64::MIN);
    assert_eq!(db.increment("fresh", -1).unwrap_err().kind(), ErrorKind::NotAnInteger);
    db.close().unwrap();
}

#[test]
fn compare_and_set_changes_only_the_value_it_expects() {
    let file = TempPath::new("expiry-cas");
    let db = Database::open(file.path()).unwrap();
    let some = |text: &'static str| Some(text.as_bytes());
    assert!(db.compare_and_set("lock", None, some("me")).unwrap());
    assert!(!db.compare_and_set("lock", None, some("you")).unwrap());
    assert!(!db.compare_and_set("lock", some("you"), some("you")).unwrap());
    assert_eq!(db.get("lock").unwrap(), Some(b"me".to_vec()));
    let stamp = db.stats().unwrap().last_commit;
    assert!(!db.compare_and_set("lock", some("other"), None).unwrap());
    assert_eq!(db.stats().unwrap().last_commit, stamp);
    assert!(db.compare_and_set("lock", some("me"), some("again")).unwrap());
    assert!(db.compare_and_set("lock", some("again"), None).unwrap());
    assert_eq!(db.get("lock").unwrap(), None);
    db.create_collection("locks").unwrap();
    let locks = db.collection("locks");
    assert!(locks.compare_and_set("l", None, some("a")).unwrap());
    assert!(!locks.compare_and_set("l", some("b"), some("c")).unwrap());
    assert_eq!(locks.get("l").unwrap(), Some(b"a".to_vec()));
    assert_eq!(db.get("l").unwrap(), None);
    assert_eq!(
        db.compare_and_set(b"\xffCOL\x00x", None, some("v")).unwrap_err().kind(),
        ErrorKind::ReservedKey
    );
    db.close().unwrap();
}

#[test]
fn a_key_with_a_time_to_live_is_fourteen_bytes_shorter() {
    let file = TempPath::new("expiry-key-length");
    let db = Database::open(file.path()).unwrap();
    let longest = db.max_key_bytes() - 14;
    db.put_with_expiry(vec![b'k'; longest], "v", TTL).unwrap();
    let error = db
        .put_with_expiry(vec![b'l'; longest + 1], "v", TTL)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TooLarge);
    assert_eq!(db.get(vec![b'l'; longest + 1]).unwrap(), None);
    db.put(vec![b'm'; longest + 1], "v").unwrap();
    assert_eq!(
        db.set_expiry(vec![b'm'; longest + 1], TTL).unwrap_err().kind(),
        ErrorKind::TooLarge
    );
    db.create_collection("c").unwrap();
    let in_collection = db.max_collection_key_bytes() - 14;
    db.collection("c")
        .put_with_expiry(vec![b'k'; in_collection], "v", TTL)
        .unwrap();
    let error = db
        .collection("c")
        .put_with_expiry(vec![b'k'; in_collection + 1], "v", TTL)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TooLarge);
    assert_eq!(
        db.put_with_expiry(b"\xffCOL\x00\x04x", "v", TTL).unwrap_err().kind(),
        ErrorKind::ReservedKey
    );
    db.close().unwrap();
}

#[test]
fn dropping_a_collection_removes_the_times_to_live_of_its_keys() {
    let file = TempPath::new("expiry-drop-collection");
    let clock = clock();
    let db = open(&file, &clock);
    db.create_collection("c").unwrap();
    db.collection("c").put_with_expiry("k", "v", TTL).unwrap();
    db.put_with_expiry("k", "default", 2 * TTL).unwrap();
    assert!(db.drop_collection("c").unwrap());
    assert_eq!(db.check().unwrap().entries, 1 + 3);
    db.create_collection("c").unwrap();
    db.collection("c").put("k", "permanent").unwrap();
    clock.advance(TTL);
    assert_eq!(db.collection("c").get("k").unwrap(), Some(b"permanent".to_vec()));
    assert_eq!(db.get("k").unwrap(), Some(b"default".to_vec()));
    clock.advance(TTL);
    assert_eq!(db.get("k").unwrap(), None);
    assert_eq!(db.remove_expired().unwrap(), 1);
    db.close().unwrap();
}
