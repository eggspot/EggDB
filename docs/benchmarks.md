# Benchmarks

EggDB measures itself against the best database for each workload on the same machine (D7). For
v0.2.0's embedded key-value store that database is SQLite, and this page says how they are
compared, with what settings, and what the numbers are today. It also covers the `eggdb`
binary's size, start time and peak memory, which may only shrink (D6, D14). The decision behind
all of it is D94; the harness is D80; how a slower result is handled is D101.

## Running them

```sh
cargo run --release -p eggdb-bench
cargo run --release --manifest-path crates/eggdb-sqlite-compare/Cargo.toml
```

The first runs EggDB's own benchmarks and the footprint of the `eggdb` binary, which it builds
first. The second compares EggDB with SQLite; it is a crate with a workspace of its own, so SQLite
is compiled only by this command and never reaches what EggDB ships. Both compare their results
with `benchmarks/baseline.txt`, print a table of the changes, print the lines a new system is
missing, and take `--update` to rewrite this system's lines of what they measured (refused in CI)
and `--results <path>` to write one line per metric. In CI, the `bench` job runs on Linux
and the `compare` job on Linux and macOS (D155), on every pull request and push to `main`; they add the table to the job summary and
upload the results files as artifacts.

## What blocks a pull request (D101)
Benchmarks report on pull requests; they block only when something is more than twice as slow as
its line. Each metric gets a verdict in the table:

| Verdict | Meaning | Fails the run |
|---------|---------|---------------|
| `ok` | within the line's tolerance (the Gate column below) | no |
| `SLOWER` | above the tolerance and at most twice the line | no, reported; the weekly performance pass (Saturdays) fixes it |
| `BLOCKED` | more than twice the line, after one more measurement for ratios; a count above a line of zero is always more than twice | yes, fixed in the same pull request |
| `improved` | below the line | no; the weekly pass lowers the line with `--update` |
| `reported only` | times in nanoseconds and the disk-bound workloads | no |

A count with no baseline line still fails: add it with `--update` in the pull request that adds the
benchmark. The Gate column below names the tolerance after which a result is `SLOWER`, not a
limit that fails the run. The results files are named
`benchmarks-pr-<number>-<head sha>` and `sqlite-compare-pr-<number>-<head sha>` (`push` in place of
`pr-<number>` on `main`) and kept 90 days, so a regression can be traced to the pull request that
caused it; each line is `<os> <benchmark> <metric> <value> <baseline> <change> <verdict>`.

## How EggDB is compared with SQLite

Both run in one process, on files in the same temporary directory, holding the same 16-byte keys
and values: 100,000 rows loaded before measuring (1,000 for the durable commit). Each workload
alternates fifteen samples of EggDB with fifteen of SQLite, each sample at least 20 ms long, and
takes the medians. The compared number is `ratio_to_sqlite`, EggDB's time over SQLite's: below 1,
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

| Workload | What one operation is | Durability | Gate (D101) |
|----------|-----------------------|------------|------|
| `sqlite_compare.get` | a point get returning the value, in its own read transaction | either | 50% above the line |
| `sqlite_compare.scan` | one entry of an ascending scan, not copied | either | 50% above the line |
| `sqlite_compare.batch_put` | one overwrite of a random existing key, in transactions of 1,000 | `Off` / `OFF` | 50% above the line |
| `sqlite_compare.batch_put_normal` | the same | `Normal` / `NORMAL` | reported |
| `sqlite_compare.durable_commit` | one put in its own transaction, waiting for the disk | `Full` / `FULL` | reported |
| `sqlite_compare.sql_point_select` | `SELECT name, score FROM people WHERE id = N` as SQL text, run each time (EggDB's plan cache skips the parse of a shape it has seen, D155; SQLite prepares each time), over 100,000 rows (`bigint PRIMARY KEY` in EggDB, `INTEGER PRIMARY KEY` in SQLite) | `Normal` / `NORMAL` | 15% above the line (D99) |
| `sqlite_compare.sql_indexed_lookup` | `SELECT id, score FROM people WHERE name = 'person N'` as SQL text over the same 100,000 rows with `CREATE INDEX people_name ON people (name)` in both | `Normal` / `NORMAL` | 50% above the line; 1.95 on a Mac when it was added, the Linux line follows its first CI run (D109) |
| `sqlite_compare.sql_bulk_insert` | one row of 100-row `INSERT ... VALUES` statements, 1,000 rows per `BEGIN ... COMMIT`, as SQL text | `Off` / `OFF` | 15% above the line (D99) |
| `sqlite_compare.sql_join` | `SELECT count(*), sum(o.amount) FROM orders o JOIN people p ON p.id = o.person_id WHERE p.score > 100` as SQL text: 10,000 orders, each joined to one of 10,000 people by primary key | `Normal` / `NORMAL` | 15% above the line (D100) |
| `sqlite_compare.sql_group_by` | `SELECT bucket, count(*), sum(amount), min(amount), max(amount) FROM orders GROUP BY bucket` as SQL text: 10,000 rows in 100 groups | `Normal` / `NORMAL` | 15% above the line (D100) |
| `sqlite_compare.sql_in_subquery` | `SELECT count(*), sum(amount) FROM orders WHERE person_id IN (SELECT id FROM people WHERE score > 500)` over the same tables: an uncorrelated subquery run once into a hash set | `Normal` / `NORMAL` | 15% above the line; 2.2 times SQLite's time on a Mac when added, the Linux line follows its first CI run (D112) |
| `sqlite_compare.sql_correlated_exists` | `SELECT count(*) FROM people p WHERE EXISTS (SELECT 1 FROM orders o WHERE o.person_id = p.id AND o.amount > 900)`: a correlated `EXISTS`, run once into a hash set of `person_id`s that each person looks up (D112) | `Normal` / `NORMAL` | 15% above the line; 6.4 times SQLite's time on a Mac when added (D112) |
| `sqlite_compare.sql_union` | `SELECT count(*) FROM (SELECT person_id FROM orders UNION SELECT id + 5000 FROM people) u`: a hashed `UNION` of 20,000 rows read through a derived table | `Normal` / `NORMAL` | 15% above the line; 4.4 times SQLite's time on a Mac when added (D112) |
| `sqlite_compare.sql_view_point_select` | `SELECT name, score FROM people_view WHERE id = N` through `CREATE VIEW people_view AS SELECT id, name, score FROM people`, over the 100,000 rows of `sql_point_select`: the condition is pushed into the view and read by key (D155) | `Normal` / `NORMAL` | 15% above the line (D155) |
| `sqlite_compare.sql_correlated_in` | `SELECT count(*) FROM people p WHERE 990 IN (SELECT o.amount FROM orders o WHERE o.person_id = p.id)`: a correlated `IN` run once and grouped by `person_id` (D155) | `Normal` / `NORMAL` | 15% above the line (D155) |
| `sqlite_compare.sql_scalar_aggregate` | `SELECT count(*) FROM people p WHERE (SELECT sum(o.amount) FROM orders o WHERE o.person_id = p.id) > 900`: a correlated aggregate run once `GROUP BY person_id` (D155) | `Normal` / `NORMAL` | 15% above the line (D155) |

The band is wider than the 30% of EggDB's other ratios because two engines respond differently
to each CI runner's processor: seven runs gave point gets from 0.60 to 0.83 and scans from 0.35
to 0.52. Workloads that wait for the disk measure the disk more than either engine, so they are
reported only.

## The `eggdb` binary

| Benchmark | Metric | Gate (D101) |
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

Measured on 2026-10-27, the batched puts again after D97 the same day. macOS is an
Apple-silicon Mac (aarch64), the median of five runs for the batched puts; Linux is the median
of five runs of GitHub's `ubuntu-latest` x86-64 runners, whose hardware varies between runs.

| | macOS ratio | macOS EggDB | macOS SQLite | Linux ratio |
|-|-------------|-------------|--------------|-------------|
| get | 0.74 | 279 ns | 375 ns | 0.69 |
| scan, per entry | 0.59 | 20.9 ns | 35.8 ns | 0.39 |
| batch put, `Off` | 0.69 | 1.58 µs | 2.28 µs | 0.61 |
| batch put, `Normal` | 1.13 | 24.0 µs | 21.3 µs | 1.07 |
| durable commit | about 1.0 | 4.2 ms | 4.1 ms | 0.92 |
| SQL point select | 1.23 | 1.27 µs | 1.04 µs | 0.85 |
| SQL bulk insert, per row | 0.89 | 0.31 µs | 0.34 µs | 0.83 |

| | macOS | Linux |
|-|-------|-------|
| Binary size | 1,198,032 bytes (933,312 before SQL, D99) | 1,461,576 bytes (1,133,296 before SQL) |
| `eggdb --version` | 1.48 ms, 0.97 × a program that exits at once | 0.78 ms, 1.02 × |
| `eggdb get` on a 10,000-entry file | 1.28 ms, 1.05 × | 1.08 ms, 1.42 × |
| Peak memory, `--version` | 1,654,784 bytes | 2,484,224 bytes |
| Peak memory, `get` | 2,310,144 bytes | 5,103,616 bytes |

EggDB is faster than SQLite on point gets, scans and batched random overwrites without syncs on
both systems. The overwrites used to take EggDB about 1.35 times SQLite's time on this Mac and
1.09 times on Linux: each commit's log was longer than the log segments EggDB keeps for reuse, so
every commit grew the file and its free list. A long commit's log now ends in a reused segment
and the checkpoint after it cuts the new ones off the file again (D97, issues #52 and #44). In
`Normal`, SQLite stays ahead by about 1.1 times because EggDB syncs four times per commit there
and SQLite twice; the two extra syncs make EggDB's header durable before log segments are reused
and keep its second header slot a copy, which D97 records as the price of correctness (D7). A
durable commit on either engine is one disk flush (`F_FULLFSYNC` on the Mac), which takes nearly
all of its time.

The binary is unchanged by this measurement; its growth since v0.1.0 (302,448 bytes on macOS,
when it only printed its version) is the storage engine, the command-line tool, `eggdb ui` and
`export`/`import`, each stated with its reason when it landed (D90 to D93).

## Footprint per build profile
`scripts/footprint.sh` builds the `embedded` artifacts (the C API library, and the WebAssembly
module when it has been built) and the `full` binary in release, strips them, and checks each
size against `benchmarks/footprint.txt` (D6, D34, D230). CI runs it on Linux and macOS; a size
over its budget fails until the budget is raised with a stated reason, and `--update` rewrites
the current system's lines.
