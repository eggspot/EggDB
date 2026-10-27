//! `eggdb-sqlite-compare`: EggDB's embedded key-value store measured against SQLite on the same
//! machine, in the same process, with matching settings (D7, D94).
//!
//! Run with `cargo run --release --manifest-path crates/eggdb-sqlite-compare/Cargo.toml`. The
//! crate has a workspace of its own, so SQLite is never compiled by the main workspace's
//! commands and never reaches what EggDB ships. Both stores hold the same 16-byte keys and
//! values, and samples alternate between them; each workload reports EggDB's time, SQLite's
//! time and their ratio.
//!
//! The ratios of point gets, ordered scans and batched puts that never wait for the disk are
//! gated against the `ratio_to_sqlite` lines of `benchmarks/baseline.txt`: they fail 50% above
//! the line after one retry. The SQL workloads run the same SQL text on both, without prepared
//! statements on either side; their ratios fail 15% above the line, since their gap to SQLite is
//! known and scheduled (D99) and may only shrink. `eggdb-bench` gates its ratios the same way at 30% (D80), but two
//! engines respond differently to each CI runner's processor, so their ratio moves more between
//! runs. Batched puts in the `Normal` mode and the durable commit, one commit per put waiting for
//! the disk, depend on the disk more than on either engine and are reported only. `--update`
//! rewrites this system's lines of these benchmarks and `--baseline <path>` reads another file.

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
#[path = "../../eggdb/src/cli/sql_store.rs"]
mod sql_store;
mod sql_workloads;
mod stores;
mod workloads;

use crate::harness::Benchmark;
use std::process::ExitCode;

type Workload = fn() -> Result<Vec<crate::baseline::Metric>, String>;

const WORKLOADS: [(&str, Workload); 9] = [
    (workloads::GET, workloads::get),
    (workloads::SCAN, workloads::scan),
    (workloads::BATCH_PUT, workloads::batch_put),
    (workloads::BATCH_PUT_NORMAL, workloads::batch_put_normal),
    (workloads::DURABLE_COMMIT, workloads::durable_commit),
    (sql_workloads::POINT_SELECT, sql_workloads::point_select),
    (sql_workloads::BULK_INSERT, sql_workloads::bulk_insert),
    (sql_workloads::JOIN, sql_workloads::join),
    (sql_workloads::GROUP_BY, sql_workloads::group_by),
];

fn main() -> ExitCode {
    let title = format!("EggDB against SQLite {}", rusqlite::version());
    println!("{title}");
    let workloads = WORKLOADS
        .into_iter()
        .map(|(name, workload)| -> Benchmark<'_> { (name, Box::new(workload)) })
        .collect();
    let outcome = harness::run(&title, std::env::args().skip(1), workloads);
    harness::exit_code("eggdb-sqlite-compare", outcome)
}
