//! `eggdb-mongo-compare`: EggDB's MongoDB-compatible wire protocol measured against MongoDB on
//! the same machine (D7, D197).
//!
//! Run with `cargo run --release --manifest-path crates/eggdb-mongo-compare/Cargo.toml`, with
//! `eggdb serve --mongo-listen` at `EGGDB_MONGO_ADDRESS` (`127.0.0.1:27018` by default, user
//! `eggdb` with the password in `EGGDB_PASSWORD`) and MongoDB at `MONGODB_ADDRESS`
//! (`127.0.0.1:27017`, without access control). Both servers get the same `OP_MSG` commands
//! from one client of this crate's own: batched inserts, range finds, a `$group` aggregation and
//! YCSB workloads A to F over 100,000 records. The ratios are reported, not gated. Without both
//! servers the program says so and exits successfully.

#[path = "../../eggdb-bench/src/baseline.rs"]
#[allow(dead_code)]
mod baseline;
mod client;
#[path = "../../eggdb-bench/src/harness.rs"]
mod harness;
#[path = "../../eggdb-bench/src/measure.rs"]
#[allow(dead_code)]
mod measure;
mod workloads;
#[path = "../../eggdb-sqlite-compare/src/ycsb_mix.rs"]
#[allow(dead_code)]
mod ycsb_mix;

use crate::client::MongoClient;
use crate::harness::Benchmark;
use std::cell::RefCell;
use std::process::ExitCode;

fn connected(address: &str, credentials: Option<(&str, &str)>) -> Result<MongoClient, String> {
    let mut client = MongoClient::connect(address)?;
    if let Some((user, password)) = credentials {
        client.authenticate(user, password)?;
    }
    Ok(client)
}

fn main() -> ExitCode {
    let eggdb_address = std::env::var("EGGDB_MONGO_ADDRESS").unwrap_or_else(|_| "127.0.0.1:27018".to_owned());
    let mongodb_address = std::env::var("MONGODB_ADDRESS").unwrap_or_else(|_| "127.0.0.1:27017".to_owned());
    let password = std::env::var("EGGDB_PASSWORD").unwrap_or_default();
    let eggdb = match connected(&eggdb_address, Some(("eggdb", &password))) {
        Ok(client) => client,
        Err(reason) => {
            println!("eggdb-mongo-compare: no EggDB at {eggdb_address} ({reason}); nothing measured");
            return ExitCode::SUCCESS;
        }
    };
    let mongodb = match connected(&mongodb_address, None) {
        Ok(client) => client,
        Err(reason) => {
            println!("eggdb-mongo-compare: no MongoDB at {mongodb_address} ({reason}); nothing measured");
            return ExitCode::SUCCESS;
        }
    };
    let title = format!("EggDB at {eggdb_address} against MongoDB at {mongodb_address}");
    println!("{title}");
    let servers = RefCell::new(workloads::Servers {
        eggdb,
        mongodb,
        records_loaded: false,
    });
    let servers = &servers;
    let mut benchmarks: Vec<Benchmark<'_>> = vec![
        (workloads::INSERT, Box::new(move || workloads::inserts(&mut servers.borrow_mut()))),
        (workloads::FIND_RANGE, Box::new(move || workloads::find_range(&mut servers.borrow_mut()))),
        (workloads::GROUP, Box::new(move || workloads::group(&mut servers.borrow_mut()))),
    ];
    for (name, mix) in workloads::YCSB {
        benchmarks.push((name, Box::new(move || workloads::ycsb(&mut servers.borrow_mut(), name, mix))));
    }
    let outcome = harness::run(&title, std::env::args().skip(1), benchmarks);
    harness::exit_code("eggdb-mongo-compare", outcome)
}
