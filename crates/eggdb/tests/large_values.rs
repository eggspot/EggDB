//! Values too large for a B+tree leaf, stored on overflow pages, through the embedded API.

mod common;

use common::TempPath;
use eggdb::{Database, Durability, ErrorKind, Options};
use std::collections::BTreeMap;

fn patterned(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|index| (index as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn assert_holds(db: &Database, model: &BTreeMap<Vec<u8>, Vec<u8>>) {
    let tx = db.begin_read().unwrap();
    for (key, value) in model {
        assert_eq!(tx.get(key).unwrap().as_ref(), Some(value));
    }
    let forward: Vec<_> = tx.scan(..).map(Result::unwrap).collect();
    let expected: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert!(forward == expected);
    let mut backward: Vec<_> = tx.scan(..).rev().map(Result::unwrap).collect();
    backward.reverse();
    assert!(backward == expected);
    let mut lent = tx.scan(..);
    let mut lent_bytes = 0;
    while let Some((_, value)) = lent.next_entry().unwrap() {
        lent_bytes += value.len();
    }
    assert_eq!(lent_bytes, model.values().map(Vec::len).sum::<usize>());
    let report = db.check().unwrap();
    assert_eq!(report.entries, model.len() as u64);
    assert_eq!(report.value_bytes, lent_bytes as u64);
    assert_eq!(report.leaked_pages, 0, "{report:?}");
}

#[test]
fn large_values_round_trip_through_puts_scans_and_a_reopen() {
    let file = TempPath::new("large-values");
    let db = Database::open(file.path()).unwrap();
    let mut model = BTreeMap::new();
    db.write(|tx| {
        for (index, len) in [0, 743, 744, 4056, 4057, 100_000, 1 << 20]
            .into_iter()
            .enumerate()
        {
            let key = format!("value:{index}").into_bytes();
            let value = patterned(len, index as u8);
            tx.put(&key, &value)?;
            model.insert(key, value);
        }
        Ok(())
    })
    .unwrap();
    assert_holds(&db, &model);
    let report = db.check().unwrap();
    assert_eq!(report.overflow_values, 5);
    db.close().unwrap();
    let db = Database::open_with(file.path(), Options::new().read_only(true)).unwrap();
    assert_holds(&db, &model);
}

#[test]
fn replacing_and_deleting_large_values_frees_their_pages_and_a_rollback_keeps_them() {
    let file = TempPath::new("large-replace");
    let db = Database::open(file.path()).unwrap();
    let mut model = BTreeMap::new();
    for (round, len) in [10, 50_000, 200_000, 30, 120_000].into_iter().enumerate() {
        let value = patterned(len, round as u8);
        db.put("key", &value).unwrap();
        model.insert(b"key".to_vec(), value);
        assert_holds(&db, &model);
    }
    let before = db.check().unwrap();
    let refused = db.write(|tx| {
        tx.put("key", patterned(300_000, 9))?;
        tx.put("other", patterned(40_000, 8))?;
        tx.delete("key")?;
        tx.put(vec![0; 513], "refused, so the closure returns the error")
    });
    assert_eq!(refused.unwrap_err().kind(), ErrorKind::TooLarge);
    assert_holds(&db, &model);
    assert_eq!(db.check().unwrap().free_pages, before.free_pages);
    assert!(db.delete("key").unwrap());
    model.clear();
    let report = db.check().unwrap();
    assert_eq!((report.overflow_values, report.overflow_pages), (0, 0));
    assert!(report.free_pages >= before.overflow_pages, "{report:?}");
    assert_holds(&db, &model);
}

#[test]
fn a_reader_sees_the_large_value_of_its_snapshot_while_the_writer_replaces_it() {
    let file = TempPath::new("large-snapshot");
    let db = Database::open(file.path()).unwrap();
    let old = patterned(300_000, 1);
    db.put("doc", &old).unwrap();
    db.put("gone", patterned(80_000, 2)).unwrap();
    let reader = db.begin_read().unwrap();
    db.put("doc", patterned(310_000, 3)).unwrap();
    assert!(db.delete("gone").unwrap());
    db.put("reuses", patterned(380_000, 4)).unwrap();
    assert_eq!(reader.get("doc").unwrap(), Some(old.clone()));
    assert_eq!(reader.get("gone").unwrap(), Some(patterned(80_000, 2)));
    let seen: Vec<_> = reader.scan(..).map(Result::unwrap).collect();
    assert_eq!(seen.len(), 2);
    drop(reader);
    assert_eq!(db.get("doc").unwrap(), Some(patterned(310_000, 3)));
    assert_eq!(db.get("gone").unwrap(), None);
    assert_eq!(db.check().unwrap().leaked_pages, 0);
}

#[test]
fn rewriting_large_values_keeps_the_file_size_steady() {
    let file = TempPath::new("large-reuse");
    let options = Options::new().durability(Durability::Off);
    let db = Database::open_with(file.path(), options).unwrap();
    let mut sizes = Vec::new();
    for round in 0..48u8 {
        db.put([round % 4], patterned(100 << 10, round)).unwrap();
        sizes.push(db.stats().unwrap().file_bytes);
    }
    for round in 0..4u8 {
        assert!(db.delete([round]).unwrap());
    }
    for round in 0..16u8 {
        db.put([b'n', round % 4], patterned((100 << 10) - 1000, round))
            .unwrap();
        sizes.push(db.stats().unwrap().file_bytes);
    }
    let settled = sizes[16];
    assert!(sizes[16..].iter().all(|&size| size == settled), "{sizes:?}");
    assert_eq!(db.check().unwrap().leaked_pages, 0);
}

#[test]
fn a_value_larger_than_the_cache_is_refused_and_the_write_continues() {
    let file = TempPath::new("large-cache");
    let db = Database::open_with(file.path(), Options::new().cache_bytes(1 << 20)).unwrap();
    let mut tx = db.begin_write().unwrap();
    tx.put("small", "1").unwrap();
    let error = tx.put("big", vec![0; 2 << 20]).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TooLarge);
    assert!(error.to_string().contains("larger cache"), "{error}");
    tx.put("fits", vec![1; 512 << 10]).unwrap();
    tx.commit().unwrap();
    assert_eq!(db.get("fits").unwrap(), Some(vec![1; 512 << 10]));
    assert_eq!(db.get("big").unwrap(), None);
}
