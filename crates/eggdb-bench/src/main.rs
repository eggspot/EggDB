//! `eggdb-bench`: EggDB's benchmarks, gated against `benchmarks/baseline.txt` (D7, D80).
//!
//! Run with `cargo run --release -p eggdb-bench`. Each benchmark reports metrics of three
//! kinds:
//! - counts (file operations, evictions and heap allocations per operation), the same on every
//!   machine, which fail when they differ from their baseline line for `any` system;
//! - ratios of a hot path's time to a reference workload run in the same process, which fail
//!   when the median of interleaved samples is more than 30% above the line for this system,
//!   after one retry;
//! - the footprint of the `eggdb` binary, which it builds first: its size (1% tolerance), its
//!   peak memory (5%) and its start time as a ratio to starting a program that exits at once
//!   (50%), for this system;
//! - times in nanoseconds per operation, reported and never gated.
//!
//! `--update` rewrites this system's lines and the shared count lines of the benchmark families
//! it measured, keeping the others (such as `eggdb-sqlite-compare`'s), `--baseline <path>`
//! reads another file, and the results are added to `$GITHUB_STEP_SUMMARY` when it is set.

mod allocations;
mod baseline;
mod bench_file;
mod btree;
mod checkpoint;
mod database;
mod entries;
mod footprint;
mod harness;
mod large_value;
mod log;
mod measure;
mod page_cache;
mod transaction;

use crate::allocations::CountingAllocator;
use crate::baseline::Metric;
use crate::harness::Benchmark;
use std::process::ExitCode;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator::new();

type Counted = fn(&CountingAllocator) -> Result<Vec<Metric>, String>;
type Uncounted = fn() -> Result<Vec<Metric>, String>;

const COUNTED: [(&str, Counted); 20] = [
    (page_cache::HIT, page_cache::hit),
    (page_cache::MISS, page_cache::miss),
    (page_cache::COMMIT, page_cache::commit),
    (log::COMMIT_FULL, log::commit_full),
    (log::COMMIT_NORMAL, log::commit_normal),
    (log::COMMIT_OFF, log::commit_off),
    (checkpoint::CHECKPOINT, checkpoint::checkpoint),
    (checkpoint::RECOVERY, checkpoint::recovery),
    (btree::GET, btree::get),
    (btree::PUT, btree::put),
    (btree::DELETE, btree::delete),
    (btree::SCAN, btree::scan_ascending),
    (btree::SCAN_REVERSE, btree::scan_descending),
    (transaction::READ, transaction::read),
    (transaction::WRITE, transaction::write),
    (database::OPEN, database::open),
    (database::READ, database::read),
    (database::WRITE, database::write),
    (large_value::PUT, large_value::put),
    (large_value::GET, large_value::get),
];

const FOOTPRINT: [(&str, Uncounted); 3] = [
    (footprint::BINARY, footprint::binary),
    (footprint::START, footprint::start),
    (footprint::OPEN_AND_GET, footprint::open_and_get),
];

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1).peekable();
    if args.peek().map(String::as_str) == Some(footprint::EXIT_AT_ONCE) {
        return ExitCode::SUCCESS;
    }
    let counted = COUNTED
        .into_iter()
        .map(|(name, benchmark)| -> Benchmark<'_> {
            (name, Box::new(move || benchmark(&ALLOCATOR)))
        });
    let footprint = FOOTPRINT
        .into_iter()
        .map(|(name, benchmark)| -> Benchmark<'_> { (name, Box::new(benchmark)) });
    let outcome = harness::run("Benchmarks", args, counted.chain(footprint).collect());
    harness::exit_code("eggdb-bench", outcome)
}
