use crate::baseline::Metric;
use crate::entries::{key, value};
use crate::measure;
use crate::resp::RespClient;
use crate::ycsb_mix::{
    Driver, Mix, Operation, RECORDS, WORKLOAD_A, WORKLOAD_B, WORKLOAD_C, WORKLOAD_D, WORKLOAD_F,
};
use eggdb::{Database, Durability, Options};
use std::hint::black_box;

pub(crate) const A: &str = "redis_compare.ycsb_a";
pub(crate) const B: &str = "redis_compare.ycsb_b";
pub(crate) const C: &str = "redis_compare.ycsb_c";
pub(crate) const D: &str = "redis_compare.ycsb_d";
pub(crate) const F: &str = "redis_compare.ycsb_f";

const FILL_BATCH: u64 = 1000;

fn temporary_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "eggdb-redis-compare-ycsb-{name}-{}.db",
        std::process::id()
    ))
}

fn filled_database(name: &str) -> Result<Database, String> {
    let path = temporary_path(name);
    let _ = std::fs::remove_file(&path);
    let db = Database::open_with(&path, Options::new().durability(Durability::Normal))
        .map_err(|error| format!("opening EggDB: {error}"))?;
    for first in (0..RECORDS).step_by(FILL_BATCH as usize) {
        db.write(|tx| {
            for index in first..RECORDS.min(first + FILL_BATCH) {
                tx.put(key(index), value(index))?;
            }
            Ok(())
        })
        .map_err(|error| format!("filling EggDB: {error}"))?;
    }
    Ok(db)
}

fn fill_server(client: &mut RespClient) -> Result<(), String> {
    for first in (0..RECORDS).step_by(FILL_BATCH as usize) {
        let last = RECORDS.min(first + FILL_BATCH);
        for index in first..last {
            client.queue(&[
                b"SET".as_slice(),
                key(index).as_slice(),
                value(index).as_slice(),
            ]);
        }
        client.send().map_err(|error| format!("sending: {error}"))?;
        for _ in first..last {
            client.reply()?;
        }
    }
    Ok(())
}

fn eggdb_operations(db: &Database, driver: &mut Driver, count: u64) -> Result<usize, String> {
    let failed = |error: eggdb::Error| format!("EggDB: {error}");
    let mut folded = 0;
    for _ in 0..count {
        match driver.next() {
            (Operation::Read | Operation::Scan(_), index) => {
                folded ^= db.get(key(index)).map_err(failed)?.map_or(0, |found| found.len());
            }
            (Operation::Update | Operation::Insert, index) => {
                db.put(key(index), value(index ^ 1)).map_err(failed)?;
            }
            (Operation::ReadModifyWrite, index) => {
                let mut changed = db.get(key(index)).map_err(failed)?.unwrap_or_default();
                changed.push(1);
                changed.truncate(16);
                db.put(key(index), changed).map_err(failed)?;
            }
        }
    }
    Ok(folded)
}

fn server_get(client: &mut RespClient, index: u64) -> Result<Option<Vec<u8>>, String> {
    client.queue(&[b"GET".as_slice(), key(index).as_slice()]);
    client.send().map_err(|error| format!("sending: {error}"))?;
    client.reply()
}

fn server_set(client: &mut RespClient, index: u64, written: &[u8]) -> Result<(), String> {
    client.queue(&[b"SET".as_slice(), key(index).as_slice(), written]);
    client.send().map_err(|error| format!("sending: {error}"))?;
    client.reply().map(|_| ())
}

fn server_operations(
    client: &mut RespClient,
    driver: &mut Driver,
    count: u64,
) -> Result<usize, String> {
    let mut folded = 0;
    for _ in 0..count {
        match driver.next() {
            (Operation::Read | Operation::Scan(_), index) => {
                folded ^= server_get(client, index)?.map_or(0, |found| found.len());
            }
            (Operation::Update | Operation::Insert, index) => {
                server_set(client, index, &value(index ^ 1))?;
            }
            (Operation::ReadModifyWrite, index) => {
                let mut changed = server_get(client, index)?.unwrap_or_default();
                changed.push(1);
                changed.truncate(16);
                server_set(client, index, &changed)?;
            }
        }
    }
    Ok(folded)
}

fn workload(benchmark: &'static str, address: &str, mix: Mix) -> Result<Vec<Metric>, String> {
    let db = filled_database(benchmark)?;
    let mut client = RespClient::connect(address).map_err(|error| error.to_string())?;
    fill_server(&mut client)?;
    let mut ours = Driver::new(mix, 1);
    let mut theirs = Driver::new(mix, 1);
    let measured = measure::paired(
        &mut |count| {
            black_box(eggdb_operations(&db, &mut ours, count)?);
            Ok(())
        },
        &mut |count| {
            black_box(server_operations(&mut client, &mut theirs, count)?);
            Ok(())
        },
    )?;
    drop(db);
    let _ = std::fs::remove_file(temporary_path(benchmark));
    Ok(vec![
        Metric::reported(benchmark, "ratio_to_redis", measured.ratio),
        Metric::reported(benchmark, "eggdb_ns_per_operation", measured.subject_ns),
        Metric::reported(benchmark, "redis_ns_per_operation", measured.reference_ns),
    ])
}

pub(crate) fn a(address: &str) -> Result<Vec<Metric>, String> {
    workload(A, address, WORKLOAD_A)
}

pub(crate) fn b(address: &str) -> Result<Vec<Metric>, String> {
    workload(B, address, WORKLOAD_B)
}

pub(crate) fn c(address: &str) -> Result<Vec<Metric>, String> {
    workload(C, address, WORKLOAD_C)
}

pub(crate) fn d(address: &str) -> Result<Vec<Metric>, String> {
    workload(D, address, WORKLOAD_D)
}

pub(crate) fn f(address: &str) -> Result<Vec<Metric>, String> {
    workload(F, address, WORKLOAD_F)
}
