use crate::baseline::Metric;
use crate::entries::{key, value};
use crate::measure;
use crate::resp::RespClient;
use eggdb::{Database, Durability, Options};

pub(crate) const SET: &str = "redis_compare.set";
pub(crate) const GET: &str = "redis_compare.get";

const ENTRIES: u64 = 100_000;
const PIPELINE: u64 = 100;

fn temporary_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("eggdb-redis-compare-{name}-{}.db", std::process::id()))
}

fn database(name: &str) -> Result<Database, String> {
    let path = temporary_path(name);
    let _ = std::fs::remove_file(&path);
    Database::open_with(&path, Options::new().durability(Durability::Normal))
        .map_err(|error| format!("opening EggDB: {error}"))
}

fn eggdb_sets(db: &Database, first: &mut u64, iterations: u64) -> Result<(), String> {
    for _ in 0..iterations.div_ceil(PIPELINE) {
        db.write(|tx| {
            for _ in 0..PIPELINE {
                tx.put(key(*first % ENTRIES), value(*first))?;
                *first += 1;
            }
            Ok(())
        })
        .map_err(|error| format!("EggDB put: {error}"))?;
    }
    Ok(())
}

fn server_sets(client: &mut RespClient, first: &mut u64, iterations: u64) -> Result<(), String> {
    for _ in 0..iterations.div_ceil(PIPELINE) {
        for _ in 0..PIPELINE {
            client.queue(&[
                b"SET".as_slice(),
                key(*first % ENTRIES).as_slice(),
                value(*first).as_slice(),
            ]);
            *first += 1;
        }
        client.send().map_err(|error| format!("sending: {error}"))?;
        for _ in 0..PIPELINE {
            client.reply()?;
        }
    }
    Ok(())
}

fn eggdb_gets(db: &Database, first: &mut u64, iterations: u64) -> Result<(), String> {
    for _ in 0..iterations {
        db.get(key(*first % ENTRIES))
            .map_err(|error| format!("EggDB get: {error}"))?
            .ok_or("EggDB lost a stored key")?;
        *first += 1;
    }
    Ok(())
}

fn server_gets(client: &mut RespClient, first: &mut u64, iterations: u64) -> Result<(), String> {
    for _ in 0..iterations.div_ceil(PIPELINE) {
        for _ in 0..PIPELINE {
            client.queue(&[b"GET".as_slice(), key(*first % ENTRIES).as_slice()]);
            *first += 1;
        }
        client.send().map_err(|error| format!("sending: {error}"))?;
        for _ in 0..PIPELINE {
            client.reply()?.ok_or("the server lost a stored key")?;
        }
    }
    Ok(())
}

pub(crate) fn set(address: &str) -> Result<Vec<Metric>, String> {
    let db = database("set")?;
    let mut client = RespClient::connect(address).map_err(|error| error.to_string())?;
    let (mut ours, mut theirs) = (0u64, 0u64);
    let measured = measure::paired(
        &mut |iterations| eggdb_sets(&db, &mut ours, iterations),
        &mut |iterations| server_sets(&mut client, &mut theirs, iterations),
    )?;
    drop(db);
    let _ = std::fs::remove_file(temporary_path("set"));
    Ok(vec![
        Metric::reported(SET, "ratio_to_redis", measured.ratio),
        Metric::reported(SET, "eggdb_ns_per_set", measured.subject_ns),
        Metric::reported(SET, "redis_ns_per_set", measured.reference_ns),
    ])
}

pub(crate) fn get(address: &str) -> Result<Vec<Metric>, String> {
    let db = database("get")?;
    let mut client = RespClient::connect(address).map_err(|error| error.to_string())?;
    let mut filled = 0;
    eggdb_sets(&db, &mut filled, ENTRIES)?;
    let mut filled = 0;
    server_sets(&mut client, &mut filled, ENTRIES)?;
    let (mut ours, mut theirs) = (0u64, 0u64);
    let measured = measure::paired(
        &mut |iterations| eggdb_gets(&db, &mut ours, iterations),
        &mut |iterations| server_gets(&mut client, &mut theirs, iterations),
    )?;
    drop(db);
    let _ = std::fs::remove_file(temporary_path("get"));
    Ok(vec![
        Metric::reported(GET, "ratio_to_redis", measured.ratio),
        Metric::reported(GET, "eggdb_ns_per_get", measured.subject_ns),
        Metric::reported(GET, "redis_ns_per_get", measured.reference_ns),
    ])
}
