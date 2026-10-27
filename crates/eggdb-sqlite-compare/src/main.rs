//! `eggdb-sqlite-compare`: EggDB's embedded key-value store measured against SQLite on the same
//! machine, in the same process, with matching settings (D7, D94).
//!
//! Run with `cargo run --release --manifest-path crates/eggdb-sqlite-compare/Cargo.toml`. The
//! crate has a workspace of its own, so SQLite is never compiled by the main workspace's
//! commands and never reaches what EggDB ships. Both stores hold the same 16-byte keys and
//! values, and samples alternate between them; each workload reports EggDB's time, SQLite's
//! time and their ratio.
//!
//! Every ratio with a `ratio_to_sqlite` line in `benchmarks/baseline.txt` is compared with it and
//! reported in a table of changes. A ratio above its line (50% for point gets, ordered scans and
//! batched puts that never wait for the disk, 15% for the SQL workloads, whose gap to SQLite is
//! known and scheduled, D99) is reported as `SLOWER` without failing the run; one more than
//! twice its line fails it, after one retry (D101). Batched puts in the `Normal` mode and the
//! durable commit, one commit per put waiting for the disk, depend on the disk more than on
//! either engine and are reported only. `--update` rewrites this system's lines of these
//! benchmarks, `--baseline <path>` reads another file and `--results <path>` writes the
//! results for CI to keep per pull request, as `eggdb-bench` does.

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
mod sql_workloads;
mod stores;
mod workloads;

use crate::harness::Benchmark;

#[cfg(test)]
mod thread_allocations {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static MADE: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) struct ThreadCountingAllocator;

    pub(crate) fn made_on_this_thread() -> u64 {
        MADE.with(Cell::get)
    }

    fn record() {
        MADE.with(|made| made.set(made.get() + 1));
    }

    // SAFETY: every call goes unchanged to the system allocator, which meets `GlobalAlloc`'s
    // contract; counting the calls in a thread-local cell allocates nothing.
    unsafe impl GlobalAlloc for ThreadCountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            record();
            // SAFETY: the caller meets `alloc`'s contract, which `System.alloc` shares.
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            record();
            // SAFETY: the caller meets `alloc_zeroed`'s contract, which `System.alloc_zeroed`
            // shares.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            record();
            // SAFETY: the caller meets `realloc`'s contract, and `ptr` came from the system
            // allocator underneath this one.
            unsafe { System.realloc(ptr, layout, new_size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: the caller meets `dealloc`'s contract, and `ptr` came from the system
            // allocator underneath this one.
            unsafe { System.dealloc(ptr, layout) }
        }
    }
}

#[cfg(test)]
#[global_allocator]
static ALLOCATOR: thread_allocations::ThreadCountingAllocator =
    thread_allocations::ThreadCountingAllocator;
use std::process::ExitCode;

type Workload = fn() -> Result<Vec<crate::baseline::Metric>, String>;

const WORKLOADS: [(&str, Workload); 13] = [
    (workloads::GET, workloads::get),
    (workloads::SCAN, workloads::scan),
    (workloads::BATCH_PUT, workloads::batch_put),
    (workloads::BATCH_PUT_NORMAL, workloads::batch_put_normal),
    (workloads::DURABLE_COMMIT, workloads::durable_commit),
    (sql_workloads::POINT_SELECT, sql_workloads::point_select),
    (sql_workloads::INDEXED_LOOKUP, sql_workloads::indexed_lookup),
    (sql_workloads::BULK_INSERT, sql_workloads::bulk_insert),
    (sql_workloads::JOIN, sql_workloads::join),
    (sql_workloads::GROUP_BY, sql_workloads::group_by),
    (sql_workloads::IN_SUBQUERY, sql_workloads::in_subquery),
    (
        sql_workloads::CORRELATED_EXISTS,
        sql_workloads::correlated_exists,
    ),
    (sql_workloads::UNION, sql_workloads::union),
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
