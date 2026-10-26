use crate::baseline::Metric;
use crate::entries::{key, value};
use crate::measure;
use crate::stores::{Stores, failed};
use eggdb::{Database, Durability};
use rusqlite::{Connection, Statement};
use std::hint::black_box;

pub(crate) const GET: &str = "sqlite_compare.get";
pub(crate) const SCAN: &str = "sqlite_compare.scan";
pub(crate) const BATCH_PUT: &str = "sqlite_compare.batch_put";
pub(crate) const DURABLE_COMMIT: &str = "sqlite_compare.durable_commit";

pub(crate) const ENTRIES: u64 = 100_000;
const DURABLE_ENTRIES: u64 = 1000;
const PUTS_PER_BATCH: u64 = 1000;
const RATIO: &str = "ratio_to_sqlite";

const SELECT_VALUE: &str = "SELECT v FROM kv WHERE k = ?1";
const SELECT_IN_ORDER: &str = "SELECT k, v FROM kv ORDER BY k";
const UPSERT: &str =
    "INSERT INTO kv(k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v";

pub(crate) fn eggdb_get(
    db: &Database,
    first: u64,
    count: u64,
    entries: u64,
) -> Result<usize, String> {
    let mut folded = 0;
    for index in first..first + count {
        let found = db
            .get(key(index % entries))
            .map_err(failed("EggDB get"))?
            .ok_or("EggDB lost a stored key")?;
        folded ^= found.len();
    }
    Ok(folded)
}

pub(crate) fn sqlite_get(
    select: &mut Statement<'_>,
    first: u64,
    count: u64,
    entries: u64,
) -> Result<usize, String> {
    let mut folded = 0;
    for index in first..first + count {
        let found: Vec<u8> = select
            .query_row([&key(index % entries)[..]], |row| row.get(0))
            .map_err(failed("SQLite get"))?;
        folded ^= found.len();
    }
    Ok(folded)
}

pub(crate) fn get() -> Result<Vec<Metric>, String> {
    let stores = Stores::open("get", Durability::Normal, ENTRIES)?;
    let mut select = prepare(&stores.sqlite, SELECT_VALUE)?;
    black_box(eggdb_get(&stores.eggdb, 0, ENTRIES, ENTRIES)?);
    black_box(sqlite_get(&mut select, 0, ENTRIES, ENTRIES)?);
    let (mut eggdb_next, mut sqlite_next) = (0, 0);
    let measured = measure::paired(
        &mut |count| {
            black_box(eggdb_get(&stores.eggdb, eggdb_next, count, ENTRIES)?);
            eggdb_next += count;
            Ok(())
        },
        &mut |count| {
            black_box(sqlite_get(&mut select, sqlite_next, count, ENTRIES)?);
            sqlite_next += count;
            Ok(())
        },
    )?;
    Ok(paired_metrics(
        GET,
        measured,
        RatioGate::Gated,
        ["ns_per_get", "sqlite_ns_per_get"],
    ))
}

pub(crate) fn eggdb_scan(
    db: &Database,
    count: u64,
    mut visit: impl FnMut(&[u8], &[u8]),
) -> Result<(), String> {
    let mut left = count;
    while left > 0 {
        let tx = db.begin_read().map_err(failed("EggDB begin read"))?;
        let mut scan = tx.scan(..);
        let mut visited_any = false;
        while left > 0 {
            let Some((key, value)) = scan.next_entry().map_err(failed("EggDB scan"))? else {
                break;
            };
            visit(key, value);
            visited_any = true;
            left -= 1;
        }
        if !visited_any {
            return Err("EggDB scanned an empty table".into());
        }
    }
    Ok(())
}

pub(crate) fn sqlite_scan(
    select: &mut Statement<'_>,
    count: u64,
    mut visit: impl FnMut(&[u8], &[u8]),
) -> Result<(), String> {
    let mut left = count;
    while left > 0 {
        let mut rows = select.query([]).map_err(failed("SQLite scan"))?;
        let mut visited_any = false;
        while left > 0 {
            let Some(row) = rows.next().map_err(failed("SQLite scan"))? else {
                break;
            };
            let key = row.get_ref(0).map_err(failed("SQLite key"))?;
            let value = row.get_ref(1).map_err(failed("SQLite value"))?;
            visit(
                key.as_blob().map_err(failed("SQLite key"))?,
                value.as_blob().map_err(failed("SQLite value"))?,
            );
            visited_any = true;
            left -= 1;
        }
        if !visited_any {
            return Err("SQLite scanned an empty table".into());
        }
    }
    Ok(())
}

pub(crate) fn scan() -> Result<Vec<Metric>, String> {
    let stores = Stores::open("scan", Durability::Normal, ENTRIES)?;
    let mut select = prepare(&stores.sqlite, SELECT_IN_ORDER)?;
    let mut folded = 0;
    let mut fold = |key: &[u8], value: &[u8]| folded ^= key.len() ^ value.len();
    eggdb_scan(&stores.eggdb, ENTRIES, &mut fold)?;
    sqlite_scan(&mut select, ENTRIES, &mut fold)?;
    let measured = measure::paired(
        &mut |count| {
            eggdb_scan(&stores.eggdb, count, |key, value| {
                black_box((key, value));
            })
        },
        &mut |count| {
            sqlite_scan(&mut select, count, |key, value| {
                black_box((key, value));
            })
        },
    )?;
    black_box(folded);
    Ok(paired_metrics(
        SCAN,
        measured,
        RatioGate::Gated,
        ["ns_per_entry", "sqlite_ns_per_entry"],
    ))
}

pub(crate) fn eggdb_batch_put(
    db: &Database,
    first: u64,
    count: u64,
    entries: u64,
) -> Result<(), String> {
    let mut next = first;
    let end = first + count;
    while next < end {
        let batch_end = end.min(next + PUTS_PER_BATCH);
        let mut tx = db.begin_write().map_err(failed("EggDB begin write"))?;
        for index in next..batch_end {
            tx.put(key(index % entries), value(index))
                .map_err(failed("EggDB put"))?;
        }
        tx.commit().map_err(failed("EggDB commit"))?;
        next = batch_end;
    }
    Ok(())
}

pub(crate) struct SqliteWriter<'c> {
    begin: Statement<'c>,
    upsert: Statement<'c>,
    commit: Statement<'c>,
}

impl<'c> SqliteWriter<'c> {
    pub(crate) fn new(sqlite: &'c Connection) -> Result<Self, String> {
        Ok(Self {
            begin: prepare(sqlite, "BEGIN IMMEDIATE")?,
            upsert: prepare(sqlite, UPSERT)?,
            commit: prepare(sqlite, "COMMIT")?,
        })
    }

    pub(crate) fn batch_put(&mut self, first: u64, count: u64, entries: u64) -> Result<(), String> {
        let mut next = first;
        let end = first + count;
        while next < end {
            let batch_end = end.min(next + PUTS_PER_BATCH);
            self.begin.execute([]).map_err(failed("SQLite begin"))?;
            for index in next..batch_end {
                self.upsert(index, entries)?;
            }
            self.commit.execute([]).map_err(failed("SQLite commit"))?;
            next = batch_end;
        }
        Ok(())
    }

    pub(crate) fn commit_each(
        &mut self,
        first: u64,
        count: u64,
        entries: u64,
    ) -> Result<(), String> {
        for index in first..first + count {
            self.upsert(index, entries)?;
        }
        Ok(())
    }

    fn upsert(&mut self, index: u64, entries: u64) -> Result<(), String> {
        self.upsert
            .execute((&key(index % entries)[..], &value(index)[..]))
            .map_err(failed("SQLite put"))?;
        Ok(())
    }
}

pub(crate) fn batch_put() -> Result<Vec<Metric>, String> {
    let stores = Stores::open("batch-put", Durability::Normal, ENTRIES)?;
    let mut writer = SqliteWriter::new(&stores.sqlite)?;
    let (mut eggdb_next, mut sqlite_next) = (ENTRIES, ENTRIES);
    let per_batch = measure::paired_calibrated_from(
        1,
        &mut measure::timed(&mut |batches| {
            let count = batches * PUTS_PER_BATCH;
            eggdb_batch_put(&stores.eggdb, eggdb_next, count, ENTRIES)?;
            eggdb_next += count;
            Ok(())
        }),
        &mut measure::timed(&mut |batches| {
            let count = batches * PUTS_PER_BATCH;
            writer.batch_put(sqlite_next, count, ENTRIES)?;
            sqlite_next += count;
            Ok(())
        }),
    )?;
    let per_put = measure::Paired {
        subject_ns: per_batch.subject_ns / PUTS_PER_BATCH as f64,
        reference_ns: per_batch.reference_ns / PUTS_PER_BATCH as f64,
        ratio: per_batch.ratio,
    };
    Ok(paired_metrics(
        BATCH_PUT,
        per_put,
        RatioGate::Gated,
        ["ns_per_put", "sqlite_ns_per_put"],
    ))
}

pub(crate) fn eggdb_commit_each(
    db: &Database,
    first: u64,
    count: u64,
    entries: u64,
) -> Result<(), String> {
    for index in first..first + count {
        db.put(key(index % entries), value(index))
            .map_err(failed("EggDB put"))?;
    }
    Ok(())
}

pub(crate) fn durable_commit() -> Result<Vec<Metric>, String> {
    let stores = Stores::open("durable-commit", Durability::Full, DURABLE_ENTRIES)?;
    let mut writer = SqliteWriter::new(&stores.sqlite)?;
    let (mut eggdb_next, mut sqlite_next) = (DURABLE_ENTRIES, DURABLE_ENTRIES);
    let measured = measure::paired_calibrated_from(
        1,
        &mut measure::timed(&mut |count| {
            eggdb_commit_each(&stores.eggdb, eggdb_next, count, DURABLE_ENTRIES)?;
            eggdb_next += count;
            Ok(())
        }),
        &mut measure::timed(&mut |count| {
            writer.commit_each(sqlite_next, count, DURABLE_ENTRIES)?;
            sqlite_next += count;
            Ok(())
        }),
    )?;
    Ok(paired_metrics(
        DURABLE_COMMIT,
        measured,
        RatioGate::Reported,
        ["ns_per_commit", "sqlite_ns_per_commit"],
    ))
}

fn prepare<'c>(sqlite: &'c Connection, sql: &str) -> Result<Statement<'c>, String> {
    sqlite
        .prepare(sql)
        .map_err(failed("prepare a SQLite statement"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RatioGate {
    Gated,
    Reported,
}

fn paired_metrics(
    benchmark: &'static str,
    measured: measure::Paired,
    gate: RatioGate,
    [eggdb_ns, sqlite_ns]: [&'static str; 2],
) -> Vec<Metric> {
    let ratio = match gate {
        RatioGate::Gated => Metric::ratio(benchmark, RATIO, measured.ratio),
        RatioGate::Reported => Metric::reported(benchmark, RATIO, measured.ratio),
    };
    vec![
        ratio,
        Metric::reported(benchmark, eggdb_ns, measured.subject_ns),
        Metric::reported(benchmark, sqlite_ns, measured.reference_ns),
    ]
}

#[cfg(test)]
mod tests {
    use super::{
        SELECT_IN_ORDER, SELECT_VALUE, SqliteWriter, eggdb_batch_put, eggdb_commit_each, eggdb_get,
        eggdb_scan, prepare, sqlite_get, sqlite_scan,
    };
    use crate::stores::Stores;
    use eggdb::Durability;

    const ENTRIES: u64 = 2000;

    fn contents(stores: &Stores) -> [Vec<(Vec<u8>, Vec<u8>)>; 2] {
        let mut eggdb = Vec::new();
        eggdb_scan(&stores.eggdb, ENTRIES, |key, value| {
            eggdb.push((key.to_vec(), value.to_vec()));
        })
        .unwrap();
        let mut select = prepare(&stores.sqlite, SELECT_IN_ORDER).unwrap();
        let mut sqlite = Vec::new();
        sqlite_scan(&mut select, ENTRIES, |key, value| {
            sqlite.push((key.to_vec(), value.to_vec()));
        })
        .unwrap();
        [eggdb, sqlite]
    }

    #[test]
    fn both_stores_scan_the_same_entries_in_the_same_order() {
        let stores = Stores::open("test-scan", Durability::Off, ENTRIES).unwrap();
        let [eggdb, sqlite] = contents(&stores);
        assert_eq!(eggdb.len(), ENTRIES as usize);
        assert!(eggdb.is_sorted());
        assert_eq!(eggdb, sqlite);
    }

    #[test]
    fn a_scan_longer_than_the_table_starts_again_from_the_first_key() {
        let stores = Stores::open("test-wrap", Durability::Off, 3).unwrap();
        let mut eggdb = Vec::new();
        eggdb_scan(&stores.eggdb, 7, |key, _| eggdb.push(key.to_vec())).unwrap();
        let mut select = prepare(&stores.sqlite, SELECT_IN_ORDER).unwrap();
        let mut sqlite = Vec::new();
        sqlite_scan(&mut select, 7, |key, _| sqlite.push(key.to_vec())).unwrap();
        assert_eq!(eggdb.len(), 7);
        assert_eq!(eggdb[0], eggdb[3]);
        assert_eq!(eggdb, sqlite);
    }

    #[test]
    fn both_stores_find_every_key_with_the_same_values() {
        let stores = Stores::open("test-get", Durability::Off, ENTRIES).unwrap();
        let mut select = prepare(&stores.sqlite, SELECT_VALUE).unwrap();
        let eggdb = eggdb_get(&stores.eggdb, 0, 2 * ENTRIES, ENTRIES).unwrap();
        let sqlite = sqlite_get(&mut select, 0, 2 * ENTRIES, ENTRIES).unwrap();
        assert_eq!(eggdb, sqlite);
    }

    #[test]
    fn batched_and_durable_puts_leave_both_stores_with_the_same_contents() {
        for durability in [Durability::Normal, Durability::Full] {
            let stores = Stores::open("test-put", durability, ENTRIES).unwrap();
            let mut writer = SqliteWriter::new(&stores.sqlite).unwrap();
            eggdb_batch_put(&stores.eggdb, ENTRIES, 2500, ENTRIES).unwrap();
            writer.batch_put(ENTRIES, 2500, ENTRIES).unwrap();
            eggdb_commit_each(&stores.eggdb, 3 * ENTRIES, 20, ENTRIES).unwrap();
            writer.commit_each(3 * ENTRIES, 20, ENTRIES).unwrap();
            drop(writer);
            let [eggdb, sqlite] = contents(&stores);
            assert_eq!(eggdb.len(), ENTRIES as usize);
            assert_eq!(eggdb, sqlite);
        }
    }
}
