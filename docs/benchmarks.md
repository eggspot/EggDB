# Benchmarks

EggDB measures itself against the best database for each workload on the same machine (D7). For
v0.2.0's embedded key-value store that database is SQLite, and this page says how they are
compared, with what settings, and what the numbers are today. It also covers the `eggdb`
binary's size, start time and peak memory, which may only shrink (D6, D14). The decision behind
all of it is D94; the harness is D80.

## Running them

```sh
cargo run --release -p eggdb-bench
cargo run --release --manifest-path crates/eggdb-sqlite-compare/Cargo.toml
```

The first runs EggDB's own benchmarks and the footprint of the `eggdb` binary, which it builds
first. The second compares EggDB with SQLite; it is a crate with a workspace of its own, so SQLite
is compiled only by this command and never reaches what EggDB ships. Both check their results
against `benchmarks/baseline.txt`, print the lines a new system is missing, and take `--update`
to rewrite this system's lines of what they measured (refused in CI). In CI, the `bench` and
`compare` jobs run them on Linux on every pull request and push to `main`, and add the results to
the job summary.

## How EggDB is compared with SQLite

Both run in one process, on files in the same temporary directory, holding the same 16-byte keys
and values: 100,000 rows loaded before measuring (1,000 for the durable commit). Each workload
alternates fifteen samples of EggDB with fifteen of SQLite, each sample at least 20 ms long, and
takes the medians. The gated number is `ratio_to_sqlite`, EggDB's time over SQLite's: below 1,
EggDB is faster.

SQLite (3.54.1, compiled into the benchmark by `rusqlite` 0.41.0) gets the settings that match
EggDB's:

| Setting | SQLite | EggDB |
|---------|--------|-------|
| Journal | `journal_mode = WAL` | write-ahead log in the file |
| Lock | `locking_mode = EXCLUSIVE` | the writer locks its file while open |
| Page size | `page_size = 4096` | `page_size_bytes(4096)` |
| Cache | `cache_size = -65536` (64 MiB) | `cache_bytes(64 MiB)` |
| Memory-mapped reads | `mmap_size = 0` | none, reads go through the cache |
| Checkpoints | `wal_autocheckpoint = 256` pages (1 MiB) | after 1 MiB of log |
| Syncs on Apple's systems | `fullfsync = ON`, `checkpoint_fullfsync = ON` | `F_FULLFSYNC` |
| Durability | `synchronous` `FULL`, `NORMAL` or `OFF` | `Full`, `Normal` or `Off` |
| Table | `kv(k BLOB PRIMARY KEY, v BLOB) WITHOUT ROWID, STRICT` | the database's one key space |
| Statements | prepared once, reused | not applicable |

A test reads every SQLite setting back, and others check that both stores scan the same entries
in the same order and hold the same contents after the same puts.

`Normal` and `NORMAL` make the same promise, that a power loss can lose the latest commits but
never corrupts the file, but they keep it differently: SQLite in WAL mode with `NORMAL` syncs only
at checkpoints, while EggDB's `Normal` also syncs once a megabyte of log or a thousand pages wait.
EggDB therefore syncs more often for the same promise, which the `batch_put_normal` numbers
include.

| Workload | What one operation is | Durability | Gate |
|----------|-----------------------|------------|------|
| `sqlite_compare.get` | a point get returning the value, in its own read transaction | either | 50% above the line |
| `sqlite_compare.scan` | one entry of an ascending scan, not copied | either | 50% above the line |
| `sqlite_compare.batch_put` | one overwrite of a random existing key, in transactions of 1,000 | `Off` / `OFF` | 50% above the line |
| `sqlite_compare.batch_put_normal` | the same | `Normal` / `NORMAL` | reported |
| `sqlite_compare.durable_commit` | one put in its own transaction, waiting for the disk | `Full` / `FULL` | reported |

The band is wider than the 30% of EggDB's other ratios because two engines respond differently
to each CI runner's processor: seven runs gave point gets from 0.60 to 0.83 and scans from 0.35
to 0.52. Workloads that wait for the disk measure the disk more than either engine, so they are
reported, not gated.

## The `eggdb` binary

| Benchmark | Metric | Gate |
|-----------|--------|------|
| `footprint.binary` | size of the stripped release binary, in bytes | 1% above the line |
| `footprint.start` | `eggdb --version`: start time as a ratio to starting a Rust program that exits at once | 50% above the line |
| `footprint.start` | its peak resident memory, in bytes | 10% above the line |
| `footprint.open_and_get` | `eggdb get` of one key from a closed 10,000-entry file: the same ratio | 50% above the line |
| `footprint.open_and_get` | its peak resident memory, in bytes | 10% above the line |

Times and memory are medians of 20 runs each. Memory is measured from outside the process
through the platform layer: on Linux the child's `VmHWM` when it exits, on Apple's systems
`ru_maxrss`, on Windows the peak working set. The release notes also list the binary's size for
every target, the static musl builds included (D75).

## Current numbers

Measured on 2026-10-27. macOS is an Apple-silicon Mac (aarch64); Linux is the median of five runs
of GitHub's `ubuntu-latest` x86-64 runners, whose hardware varies between runs.

| | macOS ratio | macOS EggDB | macOS SQLite | Linux ratio |
|-|-------------|-------------|--------------|-------------|
| get | 0.74 | 279 ns | 375 ns | 0.69 |
| scan, per entry | 0.59 | 20.9 ns | 35.8 ns | 0.39 |
| batch put, `Off` | 1.35 | 3.07 µs | 2.28 µs | 1.09 |
| batch put, `Normal` | 1.18 | 25.0 µs | 21.0 µs | 1.09 |
| durable commit | about 1.0 | 4.2 ms | 4.1 ms | 0.92 |

| | macOS | Linux |
|-|-------|-------|
| Binary size | 933,312 bytes | 1,133,296 bytes |
| `eggdb --version` | 1.48 ms, 0.97 × a program that exits at once | 0.78 ms, 1.02 × |
| `eggdb get` on a 10,000-entry file | 1.28 ms, 1.05 × | 1.08 ms, 1.42 × |
| Peak memory, `--version` | 1,654,784 bytes | 2,484,224 bytes |
| Peak memory, `get` | 2,310,144 bytes | 5,103,616 bytes |

EggDB is faster than SQLite on point gets and scans on both systems. **SQLite is faster on
batched random overwrites**: EggDB takes about 1.35 times its time on this Mac without syncs and
1.18 times in `Normal`, and about 1.09 times in both modes on Linux. Both log whole pages, so the
gap is in EggDB's write path; making it faster is on the roadmap. A durable commit on either
engine is one disk flush (`F_FULLFSYNC` on the Mac), which takes nearly all of its time.

The binary is unchanged by this measurement; its growth since v0.1.0 (302,448 bytes on macOS,
when it only printed its version) is the storage engine, the command-line tool, `eggdb ui` and
`export`/`import`, each stated with its reason when it landed (D90 to D93).
