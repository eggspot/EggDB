//! `eggdb-cql-compare`: EggDB's Cassandra-compatible CQL native protocol measured against
//! Apache Cassandra and ScyllaDB on the same machine, through the same client of this crate's own
//! (D7, D218).
//!
//! Run with `cargo run --release --manifest-path crates/eggdb-cql-compare/Cargo.toml --
//! [--results <file>]`, with `eggdb serve --cql-listen` at `EGGDB_CQL_ADDRESS`
//! (`127.0.0.1:19042`, user `eggdb` with `EGGDB_PASSWORD`), Cassandra at `CASSANDRA_ADDRESS`
//! (`127.0.0.1:9042`) and ScyllaDB at `SCYLLA_ADDRESS` (`127.0.0.1:9142`). A server that is not
//! running is reported and left out. The ratios are reported, not gated.

mod client;
mod workloads;

use client::Client;
use std::process::ExitCode;
use std::time::{Duration, Instant};
use workloads::Measured;

const ROWS: usize = 20_000;
const BATCHED_ROWS: usize = 20_000;
const READS: usize = 20_000;
const READY_WAIT: Duration = Duration::from_secs(240);

fn address(variable: &str, default: &str) -> String {
    std::env::var(variable).unwrap_or_else(|_| default.to_owned())
}

fn connect_when_ready(address: &str, login: Option<(&str, &str)>) -> Option<Client> {
    let started = Instant::now();
    loop {
        match Client::connect(address, login) {
            Ok(client) => return Some(client),
            Err(error) if started.elapsed() > READY_WAIT => {
                println!("eggdb-cql-compare: {address} did not answer ({error}); left out");
                return None;
            }
            Err(_) => std::thread::sleep(Duration::from_secs(2)),
        }
    }
}

fn run_all(client: &mut Client) -> Result<[Measured; 3], String> {
    workloads::prepare_schema(client)?;
    let inserted = workloads::inserts(client, ROWS)?;
    let batched = workloads::batches(client, ROWS, BATCHED_ROWS)?;
    let read = workloads::point_reads(client, ROWS + BATCHED_ROWS, READS)?;
    let held = workloads::counted_rows(client)?;
    if held != (ROWS + BATCHED_ROWS) as u64 {
        return Err(format!("the table holds {held} rows, not {}", ROWS + BATCHED_ROWS));
    }
    Ok([inserted, batched, read])
}

fn report(name: &str, eggdb: &Measured, other: &Measured, other_name: &str, lines: &mut Vec<String>) {
    let ratio = eggdb.per_second() / other.per_second().max(1e-9);
    let line = format!(
        "{name}: EggDB {:.0} ops/s (p50 {} us, p99 {} us per window of 64), {other_name} {:.0} ops/s (p50 {} us, p99 {} us), EggDB/{other_name} {ratio:.2}",
        eggdb.per_second(),
        eggdb.percentile(50.0),
        eggdb.percentile(99.0),
        other.per_second(),
        other.percentile(50.0),
        other.percentile(99.0),
    );
    println!("{line}");
    lines.push(line);
}

fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1);
    let mut results = None;
    while let Some(argument) = arguments.next() {
        match (argument.as_str(), arguments.next()) {
            ("--results", Some(path)) => results = Some(path),
            _ => {
                eprintln!("usage: eggdb-cql-compare [--results <file>]");
                return ExitCode::from(2);
            }
        }
    }
    let password = address("EGGDB_PASSWORD", "compare");
    let Some(mut eggdb) = connect_when_ready(&address("EGGDB_CQL_ADDRESS", "127.0.0.1:19042"), Some(("eggdb", &password))) else {
        return ExitCode::SUCCESS;
    };
    let eggdb_measured = match run_all(&mut eggdb) {
        Ok(measured) => measured,
        Err(message) => {
            eprintln!("eggdb-cql-compare: EggDB: {message}");
            return ExitCode::from(1);
        }
    };
    let names = ["cql.insert (prepared, one row each)", "cql.batch (unlogged, 50 rows each)", "cql.select (prepared point reads)"];
    let mut lines = Vec::new();
    for (other_name, variable, default) in [("Cassandra", "CASSANDRA_ADDRESS", "127.0.0.1:9042"), ("ScyllaDB", "SCYLLA_ADDRESS", "127.0.0.1:9142")] {
        let Some(mut other) = connect_when_ready(&address(variable, default), Some(("cassandra", "cassandra"))) else {
            continue;
        };
        let other_measured = match run_all(&mut other) {
            Ok(measured) => measured,
            Err(message) => {
                eprintln!("eggdb-cql-compare: {other_name}: {message}");
                return ExitCode::from(1);
            }
        };
        for (index, name) in names.iter().enumerate() {
            report(&format!("{name} against {other_name}"), &eggdb_measured[index], &other_measured[index], other_name, &mut lines);
        }
    }
    if let Some(path) = results
        && let Err(error) = std::fs::write(&path, lines.join("\n") + "\n")
    {
        eprintln!("eggdb-cql-compare: cannot write {path}: {error}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
