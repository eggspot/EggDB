//! Collections: named key spaces in one file beside the default one, each its own ordered map,
//! through the embedded API, across reopens, rollbacks and drops, and against a model.

mod common;

use common::TempPath;
use eggdb::{Database, ErrorKind, Options};
use std::collections::BTreeMap;

type Entries = BTreeMap<Vec<u8>, Vec<u8>>;

const RESERVED: &[u8] = b"\xffCOL\x00";

fn scan_all(db: &Database, collection: Option<&str>) -> Entries {
    db.read(|tx| match collection {
        Some(name) => tx.collection(name)?.scan(..).collect(),
        None => tx.scan(..).collect(),
    })
    .unwrap()
}

fn scan_all_reversed(db: &Database, collection: Option<&str>) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = db
        .read(|tx| match collection {
            Some(name) => tx.collection(name)?.scan(..).rev().collect(),
            None => tx.scan(..).rev().collect(),
        })
        .unwrap();
    entries.reverse();
    entries
}

fn entries(pairs: &[(&str, &str)]) -> Entries {
    pairs
        .iter()
        .map(|(key, value)| (key.as_bytes().to_vec(), value.as_bytes().to_vec()))
        .collect()
}

#[test]
fn two_collections_and_the_default_key_space_each_see_only_their_own_keys_after_a_reopen() {
    let file = TempPath::new("collections-reopen");
    let db = Database::open(file.path()).unwrap();
    assert!(db.create_collection("users").unwrap());
    assert!(db.create_collection("sessions").unwrap());
    assert!(!db.create_collection("users").unwrap());
    db.write(|tx| {
        tx.put("k", "default")?;
        tx.put("a", "default a")?;
        let mut users = tx.collection("users")?;
        users.put("k", "users")?;
        users.put("u:1", "Ada")?;
        users.put("u:2", "Grace")?;
        let mut sessions = tx.collection("sessions")?;
        sessions.put("k", "sessions")?;
        sessions.put("", "the empty key")?;
        Ok(())
    })
    .unwrap();
    db.close().unwrap();

    let db = Database::open(file.path()).unwrap();
    let default = entries(&[("a", "default a"), ("k", "default")]);
    let users = entries(&[("k", "users"), ("u:1", "Ada"), ("u:2", "Grace")]);
    let sessions = entries(&[("", "the empty key"), ("k", "sessions")]);
    for (collection, expected) in [
        (None, &default),
        (Some("users"), &users),
        (Some("sessions"), &sessions),
    ] {
        assert_eq!(&scan_all(&db, collection), expected, "{collection:?}");
        assert!(
            scan_all_reversed(&db, collection)
                .into_iter()
                .eq(expected.clone()),
            "{collection:?}"
        );
    }
    assert_eq!(db.get("k").unwrap(), Some(b"default".to_vec()));
    assert_eq!(
        db.collection("users").get("k").unwrap(),
        Some(b"users".to_vec())
    );
    assert_eq!(
        db.collection("sessions").get("k").unwrap(),
        Some(b"sessions".to_vec())
    );
    assert_eq!(db.collection("users").get("a").unwrap(), None);
    assert_eq!(db.collection_names().unwrap(), ["sessions", "users"]);
    let report = db.check().unwrap();
    assert_eq!(report.leaked_pages, 0);
    db.close().unwrap();
}

#[test]
fn scans_inside_a_collection_take_ranges_prefixes_and_both_ends() {
    let file = TempPath::new("collections-scans");
    let db = Database::open(file.path()).unwrap();
    db.create_collection("c").unwrap();
    db.write(|tx| {
        for key in ["apple", "apricot", "banana", "cherry", "date"] {
            tx.put(key, "default")?;
            tx.collection("c")?.put(key, key.to_uppercase())?;
        }
        Ok(())
    })
    .unwrap();
    let tx = db.begin_read().unwrap();
    let c = tx.collection("c").unwrap();
    let keys = |scan: eggdb::Scan<'_>| -> Vec<Vec<u8>> {
        scan.map(|entry| entry.map(|(key, _)| key))
            .collect::<eggdb::Result<_>>()
            .unwrap()
    };
    assert_eq!(keys(c.scan_prefix("ap")), [b"apple".to_vec(), b"apricot".to_vec()]);
    assert_eq!(keys(c.scan("b".."d")), [b"banana".to_vec(), b"cherry".to_vec()]);
    assert_eq!(keys(c.scan(..="banana")).len(), 3);
    assert_eq!(keys(c.scan("c"..)), [b"cherry".to_vec(), b"date".to_vec()]);
    assert_eq!(keys(c.scan("z".."a")), Vec::<Vec<u8>>::new());
    assert_eq!(keys(c.scan_prefix("zz")), Vec::<Vec<u8>>::new());
    let mut both = c.scan(..);
    assert_eq!(both.next().unwrap().unwrap().0, b"apple");
    assert_eq!(both.next_back().unwrap().unwrap().0, b"date");
    assert_eq!(both.next_entry().unwrap().map(|(key, _)| key.to_vec()), Some(b"apricot".to_vec()));
    assert_eq!(
        both.next_back_key_and_value_len().unwrap().map(|(key, len)| (key.to_vec(), len)),
        Some((b"cherry".to_vec(), 6))
    );
    assert_eq!(both.next().unwrap().unwrap(), (b"banana".to_vec(), b"BANANA".to_vec()));
    assert!(both.next().is_none());
    assert_eq!(
        c.get_with("cherry", |value| value.len()).unwrap(),
        Some(6)
    );
    drop(both);
    drop(c);
    assert_eq!(keys(tx.scan(..)).len(), 5);
}

#[test]
fn the_default_key_space_scans_around_the_collections_keys_in_order() {
    let file = TempPath::new("collections-around");
    let db = Database::open(file.path()).unwrap();
    db.create_collection("c").unwrap();
    let around: [&[u8]; 6] = [
        b"a",
        b"\xff",
        b"\xffCOL",
        b"\xffCOL\x01",
        b"\xffD",
        b"\xffSQL\x00x",
    ];
    db.write(|tx| {
        for key in around {
            tx.put(key, "v")?;
        }
        let mut c = tx.collection("c")?;
        for n in 0..50u8 {
            c.put([n], "inside")?;
        }
        Ok(())
    })
    .unwrap();
    let expected: Vec<Vec<u8>> = around.iter().map(|key| key.to_vec()).collect();
    let tx = db.begin_read().unwrap();
    let forward: Vec<Vec<u8>> = tx.scan(..).map(|entry| entry.unwrap().0).collect();
    assert_eq!(forward, expected);
    let backward: Vec<Vec<u8>> = tx.scan(..).rev().map(|entry| entry.unwrap().0).collect();
    assert_eq!(backward, expected.iter().rev().cloned().collect::<Vec<_>>());
    let mut mixed = tx.scan(..);
    let mut seen = Vec::new();
    loop {
        match (mixed.next(), mixed.next_back()) {
            (Some(front), Some(back)) => {
                seen.push(front.unwrap().0);
                seen.push(back.unwrap().0);
            }
            (Some(front), None) => seen.push(front.unwrap().0),
            (None, _) => break,
        }
    }
    seen.sort();
    assert_eq!(seen, expected);
    let from_inside: Vec<Vec<u8>> = tx
        .scan(b"\xffCOL\x00\x03".to_vec()..)
        .map(|entry| entry.unwrap().0)
        .collect();
    assert_eq!(from_inside, expected[3..].to_vec());
    let up_to_inside: Vec<Vec<u8>> = tx
        .scan(..b"\xffCOL\x00\x03".to_vec())
        .map(|entry| entry.unwrap().0)
        .collect();
    assert_eq!(up_to_inside, expected[..3].to_vec());
    assert_eq!(tx.scan_prefix(b"\xffC").count(), 2);
    assert_eq!(tx.scan_prefix(b"\xff").count(), 5);
    assert_eq!(tx.scan_prefix(RESERVED).count(), 0);
    assert_eq!(tx.scan_prefix(b"\xffSQL").count(), 1);
    assert_eq!(tx.collection("c").unwrap().scan(..).count(), 50);
}

#[test]
fn the_default_key_space_refuses_and_hides_the_reserved_prefix() {
    let file = TempPath::new("collections-reserved");
    let db = Database::open(file.path()).unwrap();
    db.create_collection("c").unwrap();
    db.collection("c").put("k", "v").unwrap();
    let reserved_key = [RESERVED, b"\x02c".as_slice()].concat();
    let error = db.put(&reserved_key, "overwrite").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ReservedKey);
    assert!(error.to_string().contains("collection"), "{error}");
    let mut tx = db.begin_write().unwrap();
    assert_eq!(
        tx.put_if_absent(RESERVED, "x").unwrap_err().kind(),
        ErrorKind::ReservedKey
    );
    assert!(!tx.delete(&reserved_key).unwrap());
    assert_eq!(tx.get(&reserved_key).unwrap(), None);
    tx.put("still open", "yes").unwrap();
    tx.commit().unwrap();
    assert_eq!(db.get(&reserved_key).unwrap(), None);
    assert_eq!(db.read(|tx| tx.get_with(&reserved_key, |v| v.len())).unwrap(), None);
    assert_eq!(db.collection("c").get("k").unwrap(), Some(b"v".to_vec()));
    assert_eq!(db.collection_names().unwrap(), ["c"]);
}

#[test]
fn a_rolled_back_create_leaves_no_collection_and_a_drop_removes_every_key() {
    let file = TempPath::new("collections-drop");
    let db = Database::open(file.path()).unwrap();
    let mut tx = db.begin_write().unwrap();
    assert!(tx.create_collection("gone").unwrap());
    tx.collection("gone").unwrap().put("k", "v").unwrap();
    tx.rollback().unwrap();
    assert_eq!(db.collection_names().unwrap(), Vec::<String>::new());
    assert_eq!(
        db.collection("gone").get("k").unwrap_err().kind(),
        ErrorKind::CollectionNotFound
    );

    db.put("kept", "default").unwrap();
    db.create_collection("big").unwrap();
    db.create_collection("other").unwrap();
    db.write(|tx| {
        let mut big = tx.collection("big")?;
        for n in 0..2000u32 {
            big.put(n.to_be_bytes(), vec![1u8; 100])?;
        }
        tx.collection("other")?.put("o", "other")?;
        Ok(())
    })
    .unwrap();
    assert_eq!(db.check().unwrap().entries, 1 + 2000 + 1 + 1 + 2);
    assert!(db.drop_collection("big").unwrap());
    assert!(!db.drop_collection("big").unwrap());
    assert_eq!(db.collection_names().unwrap(), ["other"]);
    assert_eq!(db.check().unwrap().entries, 1 + 1 + 1 + 1);
    assert_eq!(
        db.collection("big").put("k", "v").unwrap_err().kind(),
        ErrorKind::CollectionNotFound
    );

    assert!(db.create_collection("big").unwrap());
    assert_eq!(scan_all(&db, Some("big")), Entries::new());
    assert_eq!(
        db.collection("other").get("o").unwrap(),
        Some(b"other".to_vec())
    );
    assert_eq!(db.get("kept").unwrap(), Some(b"default".to_vec()));
    assert_eq!(db.check().unwrap().leaked_pages, 0);
}

#[test]
fn names_are_checked_and_missing_collections_are_errors() {
    let file = TempPath::new("collections-names");
    let db = Database::open(file.path()).unwrap();
    let long = "n".repeat(129);
    for bad in ["", "has space", "line\nbreak", long.as_str()] {
        assert_eq!(
            db.create_collection(bad).unwrap_err().kind(),
            ErrorKind::InvalidCollectionName,
            "{bad:?}"
        );
        assert_eq!(
            db.begin_read().unwrap().collection(bad).unwrap_err().kind(),
            ErrorKind::InvalidCollectionName
        );
    }
    assert!(db.create_collection(&"n".repeat(128)).unwrap());
    assert!(db.create_collection("名前").unwrap());
    let error = db.begin_read().unwrap().collection("missing").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::CollectionNotFound);
    assert!(error.to_string().contains("\"missing\""), "{error}");
    let io: std::io::Error = error.into();
    assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
    assert_eq!(
        db.collection("missing").delete("k").unwrap_err().kind(),
        ErrorKind::CollectionNotFound
    );
}

#[test]
fn a_collection_key_is_ten_bytes_shorter_than_a_default_key() {
    let file = TempPath::new("collections-long-key");
    let db = Database::open(file.path()).unwrap();
    db.create_collection("c").unwrap();
    let longest = db.max_collection_key_bytes();
    assert_eq!(longest, db.max_key_bytes() - 10);
    let c = db.collection("c");
    c.put(vec![b'k'; longest], "fits").unwrap();
    let error = c.put(vec![b'k'; longest + 1], "too long").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TooLarge);
    assert!(error.to_string().contains("\"c\""), "{error}");
    let mut tx = db.begin_write().unwrap();
    let mut open = tx.collection("c").unwrap();
    assert_eq!(
        open.put(vec![b'k'; longest + 1], "x").unwrap_err().kind(),
        ErrorKind::TooLarge
    );
    assert!(open.put_if_absent("new", "1").unwrap());
    assert!(!open.put_if_absent("new", "2").unwrap());
    tx.commit().unwrap();
    assert_eq!(c.get("new").unwrap(), Some(b"1".to_vec()));
    assert_eq!(c.get(vec![b'k'; longest]).unwrap(), Some(b"fits".to_vec()));
}

#[test]
fn a_read_transaction_keeps_its_snapshot_of_collections() {
    let file = TempPath::new("collections-snapshot");
    let db = Database::open(file.path()).unwrap();
    db.create_collection("c").unwrap();
    db.collection("c").put("k", "old").unwrap();
    let before = db.begin_read().unwrap();
    db.collection("c").put("k", "new").unwrap();
    db.drop_collection("c").unwrap();
    assert_eq!(
        before.collection("c").unwrap().get("k").unwrap(),
        Some(b"old".to_vec())
    );
    assert_eq!(before.collection_names().unwrap(), ["c"]);
    drop(before);
    assert_eq!(db.collection_names().unwrap(), Vec::<String>::new());
}

#[test]
fn a_read_only_database_reads_collections_and_refuses_changes() {
    let file = TempPath::new("collections-read-only");
    let db = Database::open(file.path()).unwrap();
    db.create_collection("c").unwrap();
    db.collection("c").put("k", "v").unwrap();
    db.close().unwrap();
    let db = Database::open_with(file.path(), Options::new().read_only(true)).unwrap();
    assert_eq!(db.collection("c").get("k").unwrap(), Some(b"v".to_vec()));
    assert_eq!(
        db.create_collection("d").unwrap_err().kind(),
        ErrorKind::ReadOnly
    );
    assert_eq!(
        db.collection("c").put("k", "w").unwrap_err().kind(),
        ErrorKind::ReadOnly
    );
}

struct Lcg(u64);

impl Lcg {
    fn step(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.step() % bound
    }
}

#[test]
fn random_changes_to_three_key_spaces_agree_with_three_maps() {
    let file = TempPath::new("collections-model");
    let db = Database::open(file.path()).unwrap();
    let spaces: [Option<&str>; 3] = [None, Some("a"), Some("b")];
    db.create_collection("a").unwrap();
    db.create_collection("b").unwrap();
    let mut models: [Entries; 3] = Default::default();
    let mut random = Lcg(0x5eed_c011_ec71_0a5);
    for round in 0..40 {
        let mut tx = db.begin_write().unwrap();
        for _ in 0..50 {
            let space = random.below(3) as usize;
            let key = format!("k{:03}", random.below(200)).into_bytes();
            let delete = random.below(4) == 0;
            let value = format!("v{round}-{}", random.below(1000)).into_bytes();
            match (spaces[space], delete) {
                (None, false) => tx.put(&key, &value).unwrap(),
                (None, true) => {
                    assert_eq!(tx.delete(&key).unwrap(), models[space].contains_key(&key));
                }
                (Some(name), false) => tx.collection(name).unwrap().put(&key, &value).unwrap(),
                (Some(name), true) => {
                    let deleted = tx.collection(name).unwrap().delete(&key).unwrap();
                    assert_eq!(deleted, models[space].contains_key(&key));
                }
            }
            if delete {
                models[space].remove(&key);
            } else {
                models[space].insert(key, value);
            }
        }
        if random.below(5) == 0 {
            tx.rollback().unwrap();
            for (space, model) in spaces.iter().zip(models.iter_mut()) {
                *model = scan_all(&db, *space);
            }
        } else {
            tx.commit().unwrap();
        }
        for (space, model) in spaces.iter().zip(models.iter()) {
            assert_eq!(&scan_all(&db, *space), model, "round {round}, {space:?}");
            let low = format!("k{:03}", random.below(200)).into_bytes();
            let in_range: Vec<Vec<u8>> = model.range(low.clone()..).map(|(key, _)| key.clone()).collect();
            let scanned: Vec<Vec<u8>> = db
                .read(|tx| match space {
                    Some(name) => tx.collection(name)?.scan(low.clone()..).map(|e| e.map(|(k, _)| k)).collect(),
                    None => tx.scan(low.clone()..).map(|e| e.map(|(k, _)| k)).collect(),
                })
                .unwrap();
            assert_eq!(scanned, in_range, "round {round}, {space:?}");
        }
        if round % 10 == 9 {
            db.checkpoint().unwrap();
        }
    }
    drop(db);
    let db = Database::open(file.path()).unwrap();
    for (space, model) in spaces.iter().zip(models.iter()) {
        assert_eq!(&scan_all(&db, *space), model, "after reopening, {space:?}");
    }
    assert_eq!(db.check().unwrap().leaked_pages, 0);
}
