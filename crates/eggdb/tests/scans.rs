//! Scans through the embedded API, checked against `std`'s `BTreeMap` in every range form and both directions.

mod common;

use common::TempPath;
use eggdb::{Database, Options};
use std::collections::BTreeMap;
use std::ops::Bound;

type Entries = Vec<(Vec<u8>, Vec<u8>)>;

fn filled(file: &TempPath, model: &BTreeMap<Vec<u8>, Vec<u8>>) -> Database {
    let db = Database::open_with(file.path(), Options::new().cache_bytes(64 << 20)).unwrap();
    db.write(|tx| {
        for (key, value) in model {
            tx.put(key, value)?;
        }
        Ok(())
    })
    .unwrap();
    db
}

fn numbered(count: u32) -> BTreeMap<Vec<u8>, Vec<u8>> {
    (0..count)
        .map(|index| {
            let key = format!("key:{:06}", index * 3).into_bytes();
            let value = format!("value {index}").into_bytes();
            (key, value)
        })
        .collect()
}

fn owned(entries: impl Iterator<Item = (Vec<u8>, Vec<u8>)>) -> Entries {
    entries.collect()
}

fn model_range(
    model: &BTreeMap<Vec<u8>, Vec<u8>>,
    start: Bound<&[u8]>,
    end: Bound<&[u8]>,
) -> Entries {
    let start = start.map(<[u8]>::to_vec);
    let end = end.map(<[u8]>::to_vec);
    owned(
        model
            .range((start, end))
            .map(|(key, value)| (key.clone(), value.clone())),
    )
}

fn collected(scan: eggdb::Scan<'_>) -> Entries {
    scan.collect::<Result<_, _>>().unwrap()
}

#[test]
fn every_range_form_scans_the_same_keys_as_a_btreemap_in_both_directions() {
    let file = TempPath::new("ranges");
    let model = numbered(3000);
    let db = filled(&file, &model);
    let tx = db.begin_read().unwrap();
    let a: &[u8] = b"key:001000";
    let b: &[u8] = b"key:004501";
    let cases: Vec<(Entries, Entries)> = vec![
        (
            collected(tx.scan(..)),
            model_range(&model, Bound::Unbounded, Bound::Unbounded),
        ),
        (
            collected(tx.scan(a..b)),
            model_range(&model, Bound::Included(a), Bound::Excluded(b)),
        ),
        (
            collected(tx.scan(a..=b"key:004500".as_slice())),
            model_range(
                &model,
                Bound::Included(a),
                Bound::Included(b"key:004500".as_slice()),
            ),
        ),
        (
            collected(tx.scan(b"key:008000".to_vec()..)),
            model_range(
                &model,
                Bound::Included(b"key:008000".as_slice()),
                Bound::Unbounded,
            ),
        ),
        (
            collected(tx.scan(.."key:000300")),
            model_range(
                &model,
                Bound::Unbounded,
                Bound::Excluded(b"key:000300".as_slice()),
            ),
        ),
        (
            collected(tx.scan(..=String::from("key:000300"))),
            model_range(
                &model,
                Bound::Unbounded,
                Bound::Included(b"key:000300".as_slice()),
            ),
        ),
        (
            collected(tx.scan((Bound::Excluded(a), Bound::Excluded(b)))),
            model_range(&model, Bound::Excluded(a), Bound::Excluded(b)),
        ),
    ];
    for (index, (scanned, expected)) in cases.iter().enumerate() {
        assert!(!expected.is_empty(), "case {index}");
        assert_eq!(scanned, expected, "case {index}");
    }
    let reversed: Vec<Entries> = vec![
        collected_rev(tx.scan(..)),
        collected_rev(tx.scan(a..b)),
        collected_rev(tx.scan(a..=b"key:004500".as_slice())),
        collected_rev(tx.scan(b"key:008000".to_vec()..)),
        collected_rev(tx.scan(.."key:000300")),
        collected_rev(tx.scan(..=String::from("key:000300"))),
        collected_rev(tx.scan((Bound::Excluded(a), Bound::Excluded(b)))),
    ];
    for (index, (reversed, (_, expected))) in reversed.into_iter().zip(&cases).enumerate() {
        let mut expected = expected.clone();
        expected.reverse();
        assert_eq!(reversed, expected, "reversed case {index}");
    }
}

fn collected_rev(scan: eggdb::Scan<'_>) -> Entries {
    scan.rev().collect::<Result<_, _>>().unwrap()
}

#[test]
fn a_range_with_its_start_above_its_end_holds_nothing() {
    let file = TempPath::new("empty-ranges");
    let db = filled(&file, &numbered(100));
    let tx = db.begin_read().unwrap();
    assert_eq!(tx.scan("key:000200".."key:000100").count(), 0);
    assert_eq!(tx.scan("key:000200".."key:000100").rev().count(), 0);
    assert_eq!(tx.scan("key:000030".."key:000030").count(), 0);
    let point = Bound::Excluded("key:000030");
    assert_eq!(tx.scan((point, point)).count(), 0);
    assert_eq!(tx.scan("key:000030"..="key:000030").count(), 1);
}

#[test]
fn both_ends_can_be_read_until_they_meet() {
    let file = TempPath::new("both-ends");
    let model = numbered(2000);
    let db = filled(&file, &model);
    let tx = db.begin_read().unwrap();
    let mut scan = tx.scan(..);
    let mut seen = Vec::new();
    loop {
        let front = scan.next().map(Result::unwrap);
        let back = scan.next_back().map(Result::unwrap);
        if front.is_none() && back.is_none() {
            break;
        }
        seen.extend(front);
        seen.extend(back);
    }
    seen.sort();
    assert_eq!(
        seen,
        model_range(&model, Bound::Unbounded, Bound::Unbounded)
    );
    assert!(scan.next().is_none());
    assert!(scan.next_back().is_none());

    for taken_from_front in [0, 1, 999, 1000, 1999, 2000] {
        let mut scan = tx.scan(..);
        let front: Entries = scan
            .by_ref()
            .take(taken_from_front)
            .map(Result::unwrap)
            .collect();
        let back: Entries = scan.rev().map(Result::unwrap).collect();
        assert_eq!(front.len() + back.len(), 2000, "{taken_from_front}");
        let mut all = front;
        all.extend(back.into_iter().rev());
        assert_eq!(all, model_range(&model, Bound::Unbounded, Bound::Unbounded));
    }
}

#[test]
fn prefix_scans_find_exactly_the_keys_that_start_with_the_prefix() {
    let file = TempPath::new("prefix");
    let mut model = BTreeMap::new();
    for key in [
        &b""[..],
        b"a",
        b"ab",
        b"ab\x00",
        b"ab\xff",
        b"ab\xff\xff",
        b"ac",
        b"b",
        b"\xff",
        b"\xff\xff",
        b"\xff\xff\x00",
    ] {
        model.insert(key.to_vec(), key.to_vec());
    }
    for index in 0..500u32 {
        let mut key = b"ab".to_vec();
        key.extend(index.to_be_bytes());
        model.insert(key.clone(), key);
    }
    let db = filled(&file, &model);
    let tx = db.begin_read().unwrap();
    for prefix in [
        &b""[..],
        b"a",
        b"ab",
        b"ab\xff",
        b"ac",
        b"\xff",
        b"\xff\xff",
        b"zz",
    ] {
        let expected: Entries = owned(
            model
                .iter()
                .filter(|(key, _)| key.starts_with(prefix))
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        assert_eq!(collected(tx.scan_prefix(prefix)), expected, "{prefix:?}");
        let mut reversed = expected;
        reversed.reverse();
        assert_eq!(
            collected_rev(tx.scan_prefix(prefix)),
            reversed,
            "{prefix:?}"
        );
    }
}

#[test]
fn a_write_transaction_scans_its_own_changes() {
    let file = TempPath::new("write-scan");
    let db = filled(&file, &numbered(10));
    let mut tx = db.begin_write().unwrap();
    tx.put("key:000003", "changed").unwrap();
    tx.delete("key:000006").unwrap();
    tx.put("key:000004", "added").unwrap();
    let keys: Vec<Vec<u8>> = tx
        .scan("key:000003"..="key:000009")
        .map(|entry| entry.unwrap().0)
        .collect();
    assert_eq!(
        keys,
        [
            b"key:000003".to_vec(),
            b"key:000004".to_vec(),
            b"key:000009".to_vec()
        ]
    );
    assert_eq!(
        tx.scan_prefix("key:000003").next().unwrap().unwrap().1,
        b"changed".to_vec()
    );
    tx.rollback().unwrap();
    assert_eq!(db.begin_read().unwrap().scan(..).count(), 10);
}

#[test]
fn next_entry_lends_the_same_entries_without_copies() {
    let file = TempPath::new("next-entry");
    let model = numbered(1500);
    let db = filled(&file, &model);
    let tx = db.begin_read().unwrap();
    let mut scan = tx.scan("key:000300"..);
    let mut lent = Vec::new();
    while let Some((key, value)) = scan.next_entry().unwrap() {
        lent.push((key.to_vec(), value.to_vec()));
    }
    assert_eq!(scan.next_entry().unwrap(), None);
    assert_eq!(
        lent,
        model_range(
            &model,
            Bound::Included(b"key:000300".as_slice()),
            Bound::Unbounded
        )
    );
}

#[test]
fn key_and_length_steps_list_every_key_with_its_value_size_from_both_ends() {
    let file = TempPath::new("next-key-len");
    let mut model = numbered(600);
    for n in [3u32, 250, 599] {
        model.insert(format!("key:{n:06}").into_bytes(), vec![n as u8; 3 << 20]);
    }
    let db = filled(&file, &model);
    let tx = db.begin_read().unwrap();
    let expected: Vec<(Vec<u8>, u64)> = model
        .iter()
        .map(|(key, value)| (key.clone(), value.len() as u64))
        .collect();
    let mut scan = tx.scan(..);
    let mut forward = Vec::new();
    while let Some((key, len)) = scan.next_key_and_value_len().unwrap() {
        forward.push((key.to_vec(), len));
    }
    assert_eq!(forward, expected);
    let mut scan = tx.scan(..);
    let mut backward = Vec::new();
    while let Some((key, len)) = scan.next_back_key_and_value_len().unwrap() {
        backward.push((key.to_vec(), len));
    }
    backward.reverse();
    assert_eq!(backward, expected);
    let mut scan = tx.scan(..);
    let mut mixed = Vec::new();
    for turn in 0.. {
        let step = match turn % 4 {
            0 => scan.next_key_and_value_len().unwrap(),
            1 => scan.next_back_key_and_value_len().unwrap(),
            2 => scan
                .next_entry()
                .unwrap()
                .map(|(key, value)| (key, value.len() as u64)),
            _ => scan
                .next_back_entry()
                .unwrap()
                .map(|(key, value)| (key, value.len() as u64)),
        };
        let Some((key, len)) = step else {
            break;
        };
        mixed.push((key.to_vec(), len));
    }
    mixed.sort();
    assert_eq!(mixed, expected);
}
