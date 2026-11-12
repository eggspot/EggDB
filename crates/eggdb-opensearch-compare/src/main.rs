//! `eggdb-opensearch-compare`: EggDB's Elasticsearch-compatible API measured against
//! OpenSearch on the same machine (D7, D196).
//!
//! Run with `cargo run --release --manifest-path crates/eggdb-opensearch-compare/Cargo.toml`,
//! with `eggdb serve --es-listen` at `EGGDB_ES_URL` (`http://127.0.0.1:59200` by default, user
//! `eggdb` with the password in `EGGDB_PASSWORD`) and OpenSearch at `OPENSEARCH_URL`
//! (`http://127.0.0.1:9200`, its security plugin off). Both servers get the same requests over
//! HTTP on loopback: bulk indexing, document reads, `match` searches and a `terms`
//! aggregation over the same documents. The ratios are reported, not gated. Without both servers
//! the program says so and exits successfully.

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

use crate::client::Server;
use crate::harness::Benchmark;
use std::process::ExitCode;

fn main() -> ExitCode {
    let eggdb_url = std::env::var("EGGDB_ES_URL").unwrap_or_else(|_| "http://127.0.0.1:59200".to_owned());
    let opensearch_url = std::env::var("OPENSEARCH_URL").unwrap_or_else(|_| "http://127.0.0.1:9200".to_owned());
    let password = std::env::var("EGGDB_PASSWORD").ok();
    let eggdb = match Server::reachable(&eggdb_url, password.as_deref().map(|password| ("eggdb", password))) {
        Ok(server) => server,
        Err(reason) => {
            println!("eggdb-opensearch-compare: no EggDB at {eggdb_url} ({reason}); nothing measured");
            return ExitCode::SUCCESS;
        }
    };
    let opensearch = match Server::reachable(&opensearch_url, None) {
        Ok(server) => server,
        Err(reason) => {
            println!("eggdb-opensearch-compare: no OpenSearch at {opensearch_url} ({reason}); nothing measured");
            return ExitCode::SUCCESS;
        }
    };
    let title = format!("EggDB at {eggdb_url} against OpenSearch at {opensearch_url}");
    println!("{title}");
    let servers = workloads::Servers {
        eggdb,
        opensearch,
        read_index_loaded: std::cell::Cell::new(false),
    };
    let runs: [(&'static str, fn(&workloads::Servers) -> Result<Vec<baseline::Metric>, String>); 4] = [
        (workloads::BULK, workloads::bulk),
        (workloads::GET, workloads::get),
        (workloads::MATCH, workloads::search_match),
        (workloads::TERMS, workloads::terms_aggregation),
    ];
    let benchmarks: Vec<Benchmark<'_>> = runs
        .into_iter()
        .map(|(name, run)| {
            let servers = &servers;
            let benchmark: Benchmark<'_> = (name, Box::new(move || run(servers)));
            benchmark
        })
        .collect();
    let outcome = harness::run(&title, std::env::args().skip(1), benchmarks);
    harness::exit_code("eggdb-opensearch-compare", outcome)
}
