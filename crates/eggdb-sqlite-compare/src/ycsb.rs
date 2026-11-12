use crate::baseline::Metric;
use crate::entries::{key, value};
use crate::measure;
use crate::stores::{Stores, failed};
use crate::workloads::{RatioGate, paired_metrics, prepare};
use crate::ycsb_mix::{
    Driver, Mix, Operation, RECORDS, WORKLOAD_A, WORKLOAD_B, WORKLOAD_C, WORKLOAD_D, WORKLOAD_E,
    WORKLOAD_F,
};
use eggdb::{Database, Durability};
use rusqlite::{Connection, Statement};
use std::hint::black_box;

pub(crate) const A: &str = "sqlite_compare.ycsb_a";
pub(crate) const B: &str = "sqlite_compare.ycsb_b";
pub(crate) const C: &str = "sqlite_compare.ycsb_c";
pub(crate) const D: &str = "sqlite_compare.ycsb_d";
pub(crate) const E: &str = "sqlite_compare.ycsb_e";
pub(crate) const F: &str = "sqlite_compare.ycsb_f";

fn eggdb_operations(db: &Database, driver: &mut Driver, count: u64) -> Result<usize, String> {
    let mut folded = 0;
    for _ in 0..count {
        match driver.next() {
            (Operation::Read, index) => {
                let found = db.get(key(index)).map_err(failed("EggDB read"))?;
                folded ^= found.map_or(0, |found| found.len());
            }
            (Operation::Update | Operation::Insert, index) => {
                db.put(key(index), value(index ^ 1))
                    .map_err(failed("EggDB update"))?;
            }
            (Operation::ReadModifyWrite, index) => {
                db.write(|tx| {
                    let current = tx.get(key(index))?;
                    let mut changed = current.unwrap_or_default();
                    changed.push(1);
                    changed.truncate(16);
                    tx.put(key(index), changed)
                })
                .map_err(failed("EggDB read-modify-write"))?;
            }
            (Operation::Scan(length), index) => {
                let tx = db.begin_read().map_err(failed("EggDB begin read"))?;
                let mut scan = tx.scan(key(index).to_vec()..);
                for _ in 0..length {
                    let Some((found, _)) = scan.next_entry().map_err(failed("EggDB scan"))? else {
                        break;
                    };
                    folded ^= found.len();
                }
            }
        }
    }
    Ok(folded)
}

struct SqliteStatements<'c> {
    read: Statement<'c>,
    upsert: Statement<'c>,
    scan: Statement<'c>,
    begin: Statement<'c>,
    commit: Statement<'c>,
}

impl<'c> SqliteStatements<'c> {
    fn new(sqlite: &'c Connection) -> Result<Self, String> {
        Ok(Self {
            read: prepare(sqlite, "SELECT v FROM kv WHERE k = ?1")?,
            upsert: prepare(
                sqlite,
                "INSERT INTO kv(k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            )?,
            scan: prepare(sqlite, "SELECT k FROM kv WHERE k >= ?1 ORDER BY k LIMIT ?2")?,
            begin: prepare(sqlite, "BEGIN IMMEDIATE")?,
            commit: prepare(sqlite, "COMMIT")?,
        })
    }

    fn read(&mut self, index: u64) -> Result<Option<Vec<u8>>, String> {
        let mut rows = self
            .read
            .query([&key(index)[..]])
            .map_err(failed("SQLite read"))?;
        match rows.next().map_err(failed("SQLite read"))? {
            Some(row) => Ok(Some(row.get(0).map_err(failed("SQLite read"))?)),
            None => Ok(None),
        }
    }

    fn write(&mut self, index: u64, written: &[u8]) -> Result<(), String> {
        self.upsert
            .execute((&key(index)[..], written))
            .map_err(failed("SQLite write"))?;
        Ok(())
    }

    fn operations(&mut self, driver: &mut Driver, count: u64) -> Result<usize, String> {
        let mut folded = 0;
        for _ in 0..count {
            match driver.next() {
                (Operation::Read, index) => {
                    folded ^= self.read(index)?.map_or(0, |found| found.len());
                }
                (Operation::Update | Operation::Insert, index) => {
                    self.write(index, &value(index ^ 1))?;
                }
                (Operation::ReadModifyWrite, index) => {
                    self.begin.execute([]).map_err(failed("SQLite begin"))?;
                    let mut changed = self.read(index)?.unwrap_or_default();
                    changed.push(1);
                    changed.truncate(16);
                    self.write(index, &changed)?;
                    self.commit.execute([]).map_err(failed("SQLite commit"))?;
                }
                (Operation::Scan(length), index) => {
                    let mut rows = self
                        .scan
                        .query((&key(index)[..], length as i64))
                        .map_err(failed("SQLite scan"))?;
                    while let Some(row) = rows.next().map_err(failed("SQLite scan"))? {
                        let found: Vec<u8> = row.get(0).map_err(failed("SQLite scan"))?;
                        folded ^= found.len();
                    }
                }
            }
        }
        Ok(folded)
    }
}

fn workload(benchmark: &'static str, mix: Mix) -> Result<Vec<Metric>, String> {
    let stores = Stores::open(benchmark, Durability::Normal, RECORDS)?;
    let mut statements = SqliteStatements::new(&stores.sqlite)?;
    let mut eggdb_driver = Driver::new(mix, 1);
    let mut sqlite_driver = Driver::new(mix, 1);
    let measured = measure::paired(
        &mut |count| {
            black_box(eggdb_operations(&stores.eggdb, &mut eggdb_driver, count)?);
            Ok(())
        },
        &mut |count| {
            black_box(statements.operations(&mut sqlite_driver, count)?);
            Ok(())
        },
    )?;
    Ok(paired_metrics(
        benchmark,
        measured,
        RatioGate::Reported,
        ["ns_per_operation", "sqlite_ns_per_operation"],
    ))
}

pub(crate) fn a() -> Result<Vec<Metric>, String> {
    workload(A, WORKLOAD_A)
}

pub(crate) fn b() -> Result<Vec<Metric>, String> {
    workload(B, WORKLOAD_B)
}

pub(crate) fn c() -> Result<Vec<Metric>, String> {
    workload(C, WORKLOAD_C)
}

pub(crate) fn d() -> Result<Vec<Metric>, String> {
    workload(D, WORKLOAD_D)
}

pub(crate) fn e() -> Result<Vec<Metric>, String> {
    workload(E, WORKLOAD_E)
}

pub(crate) fn f() -> Result<Vec<Metric>, String> {
    workload(F, WORKLOAD_F)
}
