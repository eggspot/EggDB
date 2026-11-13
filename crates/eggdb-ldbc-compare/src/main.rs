//! `eggdb-ldbc-compare`: EggDB's property graph measured against Neo4j on the same machine, over
//! Bolt, with data in the shape of the LDBC Social Network Benchmark's core and reads and updates
//! modelled on its Interactive workload (D7, D206, D1015).
//!
//! Run with `cargo run --release --manifest-path crates/eggdb-ldbc-compare/Cargo.toml`, with
//! `eggdb serve --bolt-listen` at `EGGDB_BOLT_ADDRESS` (`127.0.0.1:7688` by default, user `eggdb`
//! with the password in `EGGDB_PASSWORD`) and Neo4j at `NEO4J_ADDRESS` (`127.0.0.1:7687`, user
//! `neo4j` with the password in `NEO4J_PASSWORD`). Both servers load the same generated network
//! (`LDBC_PERSONS` persons, 2,000 by default) through the same client of this crate's own, then
//! run the same parameterised queries. The ratios are reported, not gated. Without both servers
//! the program says so and exits successfully. This is not an audited LDBC run: the data comes
//! from this crate's generator, not LDBC's Datagen, and only a subset of the queries runs.

#[path = "../../eggdb-bench/src/baseline.rs"]
#[allow(dead_code)]
mod baseline;
mod bolt;
#[path = "../../eggdb-bench/src/harness.rs"]
mod harness;
#[path = "../../eggdb-bench/src/measure.rs"]
#[allow(dead_code)]
mod measure;
mod social;
mod workloads;

use crate::bolt::BoltClient;
use crate::harness::Benchmark;
use crate::social::Scale;
use std::cell::RefCell;
use std::process::ExitCode;

const DEFAULT_PERSONS: u64 = 2_000;

fn main() -> ExitCode {
    let eggdb_address = std::env::var("EGGDB_BOLT_ADDRESS").unwrap_or_else(|_| "127.0.0.1:7688".to_owned());
    let neo4j_address = std::env::var("NEO4J_ADDRESS").unwrap_or_else(|_| "127.0.0.1:7687".to_owned());
    let eggdb_password = std::env::var("EGGDB_PASSWORD").unwrap_or_default();
    let neo4j_password = std::env::var("NEO4J_PASSWORD").unwrap_or_default();
    let persons = std::env::var("LDBC_PERSONS").ok().and_then(|text| text.parse().ok()).unwrap_or(DEFAULT_PERSONS).max(10);
    let eggdb = match BoltClient::connect(&eggdb_address, "eggdb", &eggdb_password) {
        Ok(client) => client,
        Err(reason) => {
            println!("eggdb-ldbc-compare: no EggDB at {eggdb_address} ({reason}); nothing measured");
            return ExitCode::SUCCESS;
        }
    };
    let neo4j = match BoltClient::connect(&neo4j_address, "neo4j", &neo4j_password) {
        Ok(client) => client,
        Err(reason) => {
            println!("eggdb-ldbc-compare: no Neo4j at {neo4j_address} ({reason}); nothing measured");
            return ExitCode::SUCCESS;
        }
    };
    let title = format!("EggDB at {eggdb_address} against Neo4j at {neo4j_address}, {persons} persons");
    println!("{title}");
    let servers = RefCell::new(workloads::Servers {
        eggdb,
        neo4j,
        scale: Scale { persons, friends: 20, posts: 5 },
        loaded: false,
    });
    let servers = &servers;
    let mut benchmarks: Vec<Benchmark<'_>> = vec![(workloads::LOAD, Box::new(move || servers.borrow_mut().measure_load()))];
    for name in workloads::READS {
        benchmarks.push((name, Box::new(move || servers.borrow_mut().measure(name))));
    }
    let outcome = harness::run(&title, std::env::args().skip(1), benchmarks);
    harness::exit_code("eggdb-ldbc-compare", outcome)
}
