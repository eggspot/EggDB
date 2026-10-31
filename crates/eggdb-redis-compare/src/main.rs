//! `eggdb-redis-compare`: EggDB's key-value store measured against a Redis-compatible server
//! (Redis or Valkey) on the same machine (D7).
//!
//! Run with `cargo run --release --manifest-path crates/eggdb-redis-compare/Cargo.toml`, with a
//! server listening at `REDIS_ADDRESS` (`127.0.0.1:6379` by default). The crate has a workspace
//! of its own and talks to the server through a small RESP client of its own, so nothing of it
//! reaches what EggDB ships. EggDB runs embedded with `Durability::Normal`, the server with its
//! default persistence, and both hold the same 16-byte keys and values. Requests to the server
//! are pipelined 100 at a time, so its numbers include loopback round trips that an embedded
//! store does not pay; the ratios are reported, not gated. Without a server the program says
//! so and exits successfully.

#[path = "../../eggdb-bench/src/baseline.rs"]
#[allow(dead_code)]
mod baseline;
#[path = "../../eggdb-bench/src/bench_file.rs"]
mod bench_file;
#[path = "../../eggdb-bench/src/entries.rs"]
mod entries;
#[path = "../../eggdb-bench/src/harness.rs"]
mod harness;
#[path = "../../eggdb-bench/src/measure.rs"]
#[allow(dead_code)]
mod measure;
mod resp;
mod workloads;

use crate::harness::Benchmark;
use std::process::ExitCode;

fn main() -> ExitCode {
    let address = std::env::var("REDIS_ADDRESS").unwrap_or_else(|_| "127.0.0.1:6379".to_owned());
    if resp::RespClient::connect(&address).is_err() {
        println!("eggdb-redis-compare: no Redis-compatible server at {address}; nothing measured");
        return ExitCode::SUCCESS;
    }
    let title = format!("EggDB against the Redis-compatible server at {address}");
    println!("{title}");
    let set_address = address.clone();
    let workloads: Vec<Benchmark<'_>> = vec![
        (
            workloads::SET,
            Box::new(move || workloads::set(&set_address)),
        ),
        (workloads::GET, Box::new(move || workloads::get(&address))),
    ];
    let outcome = harness::run(&title, std::env::args().skip(1), workloads);
    harness::exit_code("eggdb-redis-compare", outcome)
}
