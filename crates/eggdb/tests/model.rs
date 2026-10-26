//! A model test of the embedded API against `std`'s `BTreeMap`: random one-step and closure
//! transactions with large values, rollbacks, every range form, prefixes, scans that take
//! entries from both ends, checks, checkpoints, closes, drops and read-only reopens. Every
//! read must match the map. `EGGDB_MODEL_TEST=1` runs more seeds and steps.

mod common;

use common::TempPath;
use eggdb::{Database, Durability, ErrorKind, Options, Scan};
use std::collections::BTreeMap;
use std::ops::Bound;

type Model = BTreeMap<Vec<u8>, Vec<u8>>;
type Entries = Vec<(Vec<u8>, Vec<u8>)>;

const KEY_BYTES: [u8; 5] = [0x00, b'a', b'b', b'c', 0xFF];

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u64() % bound as u64) as usize
        }
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next_u64() as u8).collect()
    }
}

struct Run {
    rng: Rng,
    file: TempPath,
    options: Options,
    db: Option<Database>,
    model: Model,
    context: String,
}

impl Run {
    fn new(seed: u64) -> Self {
        let mut rng = Rng(seed);
        let durability = [Durability::Normal, Durability::Off][rng.below(2)];
        let options = Options::new()
            .durability(durability)
            .cache_bytes(48 * 4096)
            .auto_checkpoint_log_bytes(Some(192 * 1024));
        let file = TempPath::new(&format!("model-{seed}"));
        let db = Database::open_with(file.path(), options.clone()).unwrap();
        Self {
            rng,
            file,
            options,
            db: Some(db),
            model: Model::new(),
            context: String::new(),
        }
    }

    fn db(&self) -> &Database {
        self.db.as_ref().unwrap()
    }

    fn key(&mut self) -> Vec<u8> {
        if !self.model.is_empty() && self.rng.below(3) == 0 {
            let index = self.rng.below(self.model.len());
            return self.model.keys().nth(index).unwrap().clone();
        }
        let len = if self.rng.below(30) == 0 {
            self.rng.below(512)
        } else {
            self.rng.below(5)
        };
        (0..len)
            .map(|_| KEY_BYTES[self.rng.below(KEY_BYTES.len())])
            .collect()
    }

    fn value(&mut self) -> Vec<u8> {
        let len = match self.rng.below(12) {
            0 => 700 + self.rng.below(100),
            1 => 4096 * (1 + self.rng.below(5)) + self.rng.below(4096),
            2 if self.rng.below(4) == 0 => 4096 * (10 + self.rng.below(20)),
            _ => self.rng.below(40),
        };
        self.rng.bytes(len)
    }

    fn step(&mut self) {
        match self.rng.below(20) {
            0..=7 => self.write_transaction(),
            8 => self.one_step_change(),
            9..=12 => self.verify_reads(),
            13 => self.verify_check(),
            14 => self.db().checkpoint().unwrap(),
            15 => self.db().sync().unwrap(),
            16 => self.reopen(true),
            17 => self.reopen(false),
            18 => self.read_only_reopen(),
            _ => self.oversized_changes_change_nothing(),
        }
    }

    fn write_transaction(&mut self) {
        let mut pending = self.model.clone();
        let changes: Vec<(Vec<u8>, Option<Vec<u8>>)> = (0..1 + self.rng.below(10))
            .map(|_| {
                let key = self.key();
                let value = (self.rng.below(4) != 0).then(|| self.value());
                (key, value)
            })
            .collect();
        let commit = self.rng.below(8) != 0;
        let too_long = vec![b'k'; self.db().max_key_bytes() + 1];
        let context = &self.context;
        let result = self.db.as_ref().unwrap().write(|tx| {
            for (key, value) in &changes {
                match value {
                    Some(value) => {
                        tx.put(key, value)?;
                        pending.insert(key.clone(), value.clone());
                    }
                    None => {
                        let found = tx.delete(key)?;
                        assert_eq!(found, pending.remove(key).is_some(), "{context}");
                    }
                }
            }
            let (key, _) = &changes[0];
            assert_eq!(tx.get(key)?.as_ref(), pending.get(key), "{context}");
            let scanned: Entries = tx.scan(..).collect::<Result<_, _>>()?;
            assert!(scanned == entries(&pending), "{context}: scan in a write");
            if commit {
                Ok(())
            } else {
                tx.put(&too_long, b"")
            }
        });
        match result {
            Ok(()) => self.model = pending,
            Err(error) if !commit && error.kind() == ErrorKind::TooLarge => {}
            Err(error) if error.kind() == ErrorKind::CacheFull => {}
            Err(error) => panic!("{}: write: {error}", self.context),
        }
    }

    fn one_step_change(&mut self) {
        let key = self.key();
        if self.rng.below(2) == 0 {
            let value = self.value();
            match self.db().put(&key, &value) {
                Ok(()) => {
                    self.model.insert(key, value);
                }
                Err(error) if error.kind() == ErrorKind::CacheFull => {}
                Err(error) => panic!("{}: put: {error}", self.context),
            }
        } else {
            let found = self.db().delete(&key).unwrap();
            assert_eq!(found, self.model.remove(&key).is_some(), "{}", self.context);
        }
    }

    fn oversized_changes_change_nothing(&mut self) {
        let max_key = self.db().max_key_bytes();
        let key = vec![b'k'; max_key + 1];
        let refused = self.db().put(&key, b"v").unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::TooLarge, "{}", self.context);
        let refused = self.db().put(b"big", vec![0; 64 * 4096]).unwrap_err();
        assert!(
            matches!(refused.kind(), ErrorKind::TooLarge | ErrorKind::CacheFull),
            "{}: {refused}",
            self.context
        );
    }

    fn bound(&mut self) -> Bound<Vec<u8>> {
        match self.rng.below(3) {
            0 => Bound::Unbounded,
            1 => Bound::Included(self.key()),
            _ => Bound::Excluded(self.key()),
        }
    }

    fn verify_reads(&mut self) {
        let context = self.context.clone();
        let db = self.db.take().unwrap();
        let tx = db.begin_read().unwrap();
        for _ in 0..8 {
            let key = self.key();
            assert_eq!(
                tx.get(&key).unwrap().as_ref(),
                self.model.get(&key),
                "{context}: get"
            );
        }
        let scanned: Entries = tx.scan(..).collect::<Result<_, _>>().unwrap();
        assert!(scanned == entries(&self.model), "{context}: scan");
        let mut backward: Entries = tx.scan(..).rev().collect::<Result<_, _>>().unwrap();
        backward.reverse();
        assert!(backward == entries(&self.model), "{context}: reverse scan");
        for _ in 0..3 {
            let (start, end) = (self.bound(), self.bound());
            let expected = in_range(&self.model, &start, &end);
            let scan = tx.scan((start.clone(), end.clone()));
            let found = self.drained_from_both_ends(scan, &context);
            assert!(found == expected, "{context}: scan {start:?}..{end:?}");
            let (low, high) = (self.key(), self.key());
            let expected = in_range(
                &self.model,
                &Bound::Included(low.clone()),
                &Bound::Excluded(high.clone()),
            );
            let found: Entries = tx.scan(low..high).collect::<Result<_, _>>().unwrap();
            assert!(found == expected, "{context}: half-open scan");
            let mut prefix = self.key();
            prefix.truncate(self.rng.below(3));
            let expected: Entries = entries(&self.model)
                .into_iter()
                .filter(|(key, _)| key.starts_with(&prefix))
                .collect();
            let found = self.drained_from_both_ends(tx.scan_prefix(&prefix), &context);
            assert!(found == expected, "{context}: prefix {prefix:?}");
        }
        drop(tx);
        self.db = Some(db);
    }

    fn drained_from_both_ends(&mut self, mut scan: Scan<'_>, context: &str) -> Entries {
        let (mut front, mut back) = (Vec::new(), Vec::new());
        loop {
            let entry = if self.rng.below(2) == 0 {
                scan.next().map(|entry| (true, entry))
            } else {
                scan.next_back().map(|entry| (false, entry))
            };
            match entry {
                None => break,
                Some((from_front, entry)) => {
                    let entry = entry.unwrap_or_else(|error| panic!("{context}: scan: {error}"));
                    if from_front {
                        front.push(entry);
                    } else {
                        back.push(entry);
                    }
                }
            }
        }
        assert!(
            scan.next().is_none() && scan.next_back().is_none(),
            "{context}"
        );
        back.reverse();
        front.extend(back);
        front
    }

    fn verify_check(&mut self) {
        let report = self
            .db()
            .check()
            .unwrap_or_else(|error| panic!("{}: check: {error}", self.context));
        assert_eq!(report.entries, self.model.len() as u64, "{}", self.context);
        assert_eq!(
            report.value_bytes,
            self.model
                .values()
                .map(|value| value.len() as u64)
                .sum::<u64>(),
            "{}",
            self.context
        );
        assert_eq!(report.leaked_pages, 0, "{}", self.context);
        let stats = self.db().stats().unwrap();
        assert_eq!(stats.free_pages, report.free_pages, "{}", self.context);
    }

    fn reopen(&mut self, close: bool) {
        let db = self.db.take().unwrap();
        if close {
            db.close().unwrap();
        } else {
            drop(db);
        }
        self.db = Some(Database::open_with(self.file.path(), self.options.clone()).unwrap());
        self.verify_check();
    }

    fn read_only_reopen(&mut self) {
        drop(self.db.take());
        let options = self.options.clone().read_only(true);
        self.db = Some(Database::open_with(self.file.path(), options).unwrap());
        self.verify_reads();
        assert_eq!(
            self.db().put(b"k", b"v").unwrap_err().kind(),
            ErrorKind::ReadOnly
        );
        drop(self.db.take());
        self.db = Some(Database::open_with(self.file.path(), self.options.clone()).unwrap());
    }
}

fn entries(model: &Model) -> Entries {
    model.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn in_range(model: &Model, start: &Bound<Vec<u8>>, end: &Bound<Vec<u8>>) -> Entries {
    let after_start = |key: &[u8]| match start {
        Bound::Unbounded => true,
        Bound::Included(start) => key >= start.as_slice(),
        Bound::Excluded(start) => key > start.as_slice(),
    };
    let before_end = |key: &[u8]| match end {
        Bound::Unbounded => true,
        Bound::Included(end) => key <= end.as_slice(),
        Bound::Excluded(end) => key < end.as_slice(),
    };
    entries(model)
        .into_iter()
        .filter(|(key, _)| after_start(key) && before_end(key))
        .collect()
}

#[test]
fn random_use_of_the_embedded_api_always_matches_an_in_memory_map() {
    let long = std::env::var_os("EGGDB_MODEL_TEST").is_some();
    let (seeds, steps) = if long { (0..32, 2000) } else { (0..4, 150) };
    for seed in seeds {
        let mut run = Run::new(seed);
        for step in 0..steps {
            run.context = format!("seed {seed}, step {step}");
            run.step();
        }
        run.verify_reads();
        run.verify_check();
        run.db.take().unwrap().close().unwrap();
    }
}
