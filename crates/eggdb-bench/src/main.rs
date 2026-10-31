//! `eggdb-bench`: EggDB's benchmarks, compared with `benchmarks/baseline.txt` (D7, D80, D101).
//!
//! Run with `cargo run --release -p eggdb-bench`. Each benchmark reports metrics of four
//! kinds:
//! - counts (file operations, evictions and heap allocations per operation), the same on every
//!   machine, compared with their baseline line for `any` system;
//! - ratios of a hot path's time to a reference workload run in the same process, the median of
//!   interleaved samples, compared with the line for this system;
//! - the footprint of the `eggdb` binary, which it builds first: its size (1% tolerance), its
//!   peak memory (10%) and its start time as a ratio to starting a program that exits at once
//!   (50%), for this system;
//! - times in nanoseconds per operation, reported and never compared.
//!
//! A count or ratio above its line is printed in a table of changes and reported as `SLOWER`
//! without failing the run, and one more than twice its line fails it after one retry for
//! ratios (D101). A baseline line missing for a count also fails it.
//!
//! `--update` rewrites this system's lines and the shared count lines of the benchmark families
//! it measured, keeping the others (such as `eggdb-sqlite-compare`'s), `--baseline <path>`
//! reads another file, `--results <path>` writes one line per metric (`<os> <benchmark>
//! <metric> <value> <baseline> <change> <verdict>`) for CI to keep per pull request, and the
//! table is added to `$GITHUB_STEP_SUMMARY` when it is set. `--only <family>` runs only the
//! benchmarks whose names start with `<family>.`, such as `sql_copy`, so `--update` can add one
//! family's lines without measuring the rest again.

mod allocations;
mod baseline;
mod bench_file;
mod btree;
mod checkpoint;
mod copy;
mod database;
mod entries;
mod footprint;
mod harness;
mod large_value;
mod log;
mod mcp;
mod measure;
mod net;
mod page_cache;
mod tls;
mod transaction;

use crate::allocations::CountingAllocator;
use crate::baseline::Metric;
use crate::harness::Benchmark;
use std::process::ExitCode;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator::new();

type Counted = fn(&CountingAllocator) -> Result<Vec<Metric>, String>;
type Uncounted = fn() -> Result<Vec<Metric>, String>;

const COUNTED: [(&str, Counted); 32] = [
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
    (database::READ_EXPIRING, database::read_expiring),
    (database::INCREMENT, database::increment),
    (database::BACKUP, database::backup),
    (large_value::PUT, large_value::put),
    (large_value::GET, large_value::get),
    (copy::FROM, copy::from_stdin),
    (copy::TO, copy::to_stdout),
    (copy::FROM_BINARY, copy::from_stdin_binary),
    (copy::TO_BINARY, copy::to_stdout_binary),
    (mcp::TOOLS_CALL, mcp::tools_call),
    (mcp::WRITE_CALL, mcp::write_call),
    (tls::RECORD_SEAL, tls::record_seal),
    (tls::HANDSHAKE, tls::handshake),
    (net::ROUND_TRIP, net::round_trip),
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
