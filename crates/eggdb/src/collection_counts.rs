use crate::error::{Error, ErrorKind, Result};
use std::collections::BTreeMap;

const COUNT_RECORD_VERSION: u8 = 1;
const COUNT_RECORD_BYTES: usize = 25;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct EntryCount {
    pub(crate) entries: u64,
    pub(crate) key_bytes: u64,
    pub(crate) value_bytes: u64,
}

impl EntryCount {
    pub(crate) fn record(&self) -> [u8; COUNT_RECORD_BYTES] {
        let mut record = [0u8; COUNT_RECORD_BYTES];
        record[0] = COUNT_RECORD_VERSION;
        record[1..9].copy_from_slice(&self.entries.to_be_bytes());
        record[9..17].copy_from_slice(&self.key_bytes.to_be_bytes());
        record[17..25].copy_from_slice(&self.value_bytes.to_be_bytes());
        record
    }

    pub(crate) fn read(bytes: &[u8]) -> Result<Self> {
        match bytes.split_first() {
            Some((&COUNT_RECORD_VERSION, rest)) if rest.len() == COUNT_RECORD_BYTES - 1 => {
                let field = |at: usize| {
                    rest.get(at..at + 8)
                        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
                        .map_or(0, u64::from_be_bytes)
                };
                Ok(Self {
                    entries: field(0),
                    key_bytes: field(8),
                    value_bytes: field(16),
                })
            }
            Some((&version, _)) if version > COUNT_RECORD_VERSION => Err(Error::new(
                ErrorKind::Incompatible,
                format!(
                    "a collection's entry count is in version {version} of its record, written by a newer EggDB; this release reads version {COUNT_RECORD_VERSION}. Upgrade eggdb from https://github.com/eggspot/EggDB/releases"
                ),
            )),
            _ => Err(Error::new(
                ErrorKind::Corrupt,
                format!(
                    "a collection's entry count is damaged: its record is {} bytes, not a version byte and three 8-byte counts; restore from a backup or run `eggdb check`",
                    bytes.len()
                ),
            )),
        }
    }

    pub(crate) fn add(&mut self, key_bytes: u64, value_bytes: u64) {
        self.entries = self.entries.saturating_add(1);
        self.key_bytes = self.key_bytes.saturating_add(key_bytes);
        self.value_bytes = self.value_bytes.saturating_add(value_bytes);
    }

    fn change(&mut self, key_bytes: u64, before: Option<u64>, after: Option<u64>) {
        match (before, after) {
            (None, Some(_)) => {
                self.entries = self.entries.saturating_add(1);
                self.key_bytes = self.key_bytes.saturating_add(key_bytes);
            }
            (Some(_), None) => {
                self.entries = self.entries.saturating_sub(1);
                self.key_bytes = self.key_bytes.saturating_sub(key_bytes);
            }
            _ => {}
        }
        self.value_bytes = self
            .value_bytes
            .saturating_sub(before.unwrap_or(0))
            .saturating_add(after.unwrap_or(0));
    }
}

#[derive(Debug, Clone, Copy)]
enum Slot {
    Kept { count: EntryCount, changed: bool },
    NotKept,
}

#[derive(Debug, Default)]
pub(crate) struct KeptCounts {
    slots: BTreeMap<u32, Slot>,
}

impl KeptCounts {
    pub(crate) fn knows(&self, number: u32) -> bool {
        self.slots.contains_key(&number)
    }

    pub(crate) fn kept(&self, number: u32) -> Option<EntryCount> {
        match self.slots.get(&number) {
            Some(Slot::Kept { count, .. }) => Some(*count),
            _ => None,
        }
    }

    pub(crate) fn learn(&mut self, number: u32, record: Option<&[u8]>) -> Result<()> {
        let slot = match record {
            Some(bytes) => Slot::Kept {
                count: EntryCount::read(bytes)?,
                changed: false,
            },
            None => Slot::NotKept,
        };
        self.slots.insert(number, slot);
        Ok(())
    }

    pub(crate) fn change(
        &mut self,
        number: u32,
        key_bytes: u64,
        before: Option<u64>,
        after: Option<u64>,
    ) {
        if let Some(Slot::Kept { count, changed }) = self.slots.get_mut(&number) {
            count.change(key_bytes, before, after);
            *changed = true;
        }
    }

    pub(crate) fn take_changed(&mut self) -> Vec<(u32, EntryCount)> {
        let mut taken = Vec::new();
        for (number, slot) in &mut self.slots {
            if let Slot::Kept { count, changed } = slot
                && *changed
            {
                *changed = false;
                taken.push((*number, *count));
            }
        }
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::{EntryCount, KeptCounts};
    use crate::collection::count_key;
    use crate::{Database, ErrorKind};

    fn shown(db: &Database) -> Vec<(String, u64, u64, u64)> {
        db.collection_stats()
            .unwrap()
            .into_iter()
            .map(|one| (one.name, one.entries, one.key_bytes, one.value_bytes))
            .collect()
    }

    fn forget_every_count(db: &Database, numbers: &[u32]) {
        let mut tx = db.begin_write().unwrap();
        for number in numbers {
            assert!(tx.delete_raw(&count_key(*number)).unwrap());
        }
        tx.commit().unwrap();
    }

    #[test]
    fn a_collection_without_a_count_is_counted_once_and_kept_from_then_on() {
        let db = Database::in_memory().unwrap();
        db.create_collection("old").unwrap();
        db.create_collection("new").unwrap();
        db.write(|tx| {
            let mut old = tx.collection("old")?;
            for n in 0..700u32 {
                old.put(n.to_be_bytes(), "vv")?;
            }
            tx.collection("new")?.put("k", "v")?;
            Ok(())
        })
        .unwrap();
        forget_every_count(&db, &[1]);
        assert!(db.has_uncounted_collections().unwrap());
        db.collection("old").put("extra", "e").unwrap();
        assert!(db.has_uncounted_collections().unwrap());
        let read = db.begin_read().unwrap();
        assert_eq!(read.collection_stats().unwrap()[1].entries, 701);
        drop(read);
        assert_eq!(
            shown(&db),
            [
                ("new".to_owned(), 1, 1, 1),
                ("old".to_owned(), 701, 700 * 4 + 5, 700 * 2 + 1)
            ]
        );
        assert!(!db.has_uncounted_collections().unwrap());
        db.collection("old").delete("extra").unwrap();
        assert_eq!(shown(&db)[1], ("old".to_owned(), 700, 700 * 4, 700 * 2));
    }

    #[test]
    fn a_journaled_drop_and_put_undone_in_reverse_restore_the_count() {
        let db = Database::in_memory().unwrap();
        db.create_collection("c").unwrap();
        db.write(|tx| {
            let mut c = tx.collection("c")?;
            for n in 0..600u32 {
                c.put(n.to_be_bytes(), "v")?;
            }
            Ok(())
        })
        .unwrap();
        let mut tx = db.begin_write().unwrap();
        let (done, changes) = tx.journaled(|tx| -> crate::Result<()> {
            tx.collection("c")?.put("one more", "value")?;
            assert!(tx.drop_collection("c")?);
            assert!(tx.create_collection("fresh")?);
            tx.collection("fresh")?.put("f", "v")?;
            Ok(())
        });
        done.unwrap();
        for (key, previous) in changes.iter().rev() {
            tx.restore_stored(key, previous.as_deref()).unwrap();
        }
        tx.commit().unwrap();
        assert_eq!(shown(&db), [("c".to_owned(), 600, 600 * 4, 600)]);
        db.collection("c").put("after", "a").unwrap();
        assert_eq!(shown(&db), [("c".to_owned(), 601, 600 * 4 + 5, 601)]);
    }

    fn count(entries: u64, key_bytes: u64, value_bytes: u64) -> EntryCount {
        EntryCount {
            entries,
            key_bytes,
            value_bytes,
        }
    }

    #[test]
    fn a_count_record_matches_its_test_vector_and_round_trips() {
        let record = count(1, 0x0203, u64::MAX).record();
        let mut expected = vec![1u8];
        expected.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]);
        expected.extend_from_slice(&[0, 0, 0, 0, 0, 0, 2, 3]);
        expected.extend_from_slice(&[0xff; 8]);
        assert_eq!(record.as_slice(), expected.as_slice());
        assert_eq!(
            EntryCount::read(&record).ok(),
            Some(count(1, 0x0203, u64::MAX))
        );
    }

    #[test]
    fn a_damaged_or_newer_count_record_is_an_error_not_a_count() {
        let record = count(3, 4, 5).record();
        let damaged: [&[u8]; 4] = [&[], &[1], &record[..24], &[0; 25]];
        for bytes in damaged {
            let error = EntryCount::read(bytes).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Corrupt, "{bytes:?}");
        }
        let mut longer = record.to_vec();
        longer.push(0);
        assert_eq!(
            EntryCount::read(&longer).unwrap_err().kind(),
            ErrorKind::Corrupt
        );
        let mut newer = record;
        newer[0] = 2;
        assert_eq!(
            EntryCount::read(&newer).unwrap_err().kind(),
            ErrorKind::Incompatible
        );
    }

    #[test]
    fn changes_add_replace_and_remove_entries_and_their_bytes() {
        let mut counts = KeptCounts::default();
        counts.learn(7, Some(&count(0, 0, 0).record())).unwrap();
        counts.learn(8, None).unwrap();
        counts.change(7, 2, None, Some(10));
        counts.change(7, 3, None, Some(1));
        counts.change(7, 2, Some(10), Some(4));
        counts.change(7, 3, Some(1), None);
        counts.change(8, 5, None, Some(5));
        assert_eq!(counts.kept(7), Some(count(1, 2, 4)));
        assert_eq!(counts.kept(8), None);
        assert!(counts.knows(8) && !counts.knows(9));
        assert_eq!(counts.take_changed(), [(7, count(1, 2, 4))]);
        assert_eq!(counts.take_changed(), []);
    }

    #[test]
    fn a_count_never_wraps_below_zero() {
        let mut counts = KeptCounts::default();
        counts.learn(1, Some(&count(0, 0, 0).record())).unwrap();
        counts.change(1, 9, Some(9), None);
        assert_eq!(counts.kept(1), Some(count(0, 0, 0)));
    }
}
