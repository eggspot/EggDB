# EggDB: Claude Code instructions

EggDB is one database for every kind of data, written from scratch in Rust: caching, relational SQL,
full-text search and every other kind of database people run today, in a single lightweight engine.
It is the fastest and the lightest, it scales from a tiny embedded file (better than SQLite) to an
enterprise cluster with billions of billions of rows, and it runs on every OS, every CPU and every
cloud. Open source under MIT (public repo `eggspot/EggDB`), a sibling of EggOS.

## IMPORTANT: notes for every task
Check each task against these before starting and again before opening the PR.
1. **All in one.** One engine covers every kind of database that exists: key-value and caching (with
   expiry, eviction, pub/sub, streams and queues), relational SQL with ACID transactions, full-text
   search, documents and JSON, time series, columnar analytics, graphs, vectors for AI, geospatial,
   wide-column, blobs and objects. They share one storage engine, one transaction system and one
   query layer, so a single query or transaction can span models. Never build a model as a
   separate database bolted on the side. (D2)
2. **We support everything people already use.** Applications switch to EggDB without code
   changes: EggDB speaks the existing wire protocols and query languages (PostgreSQL and MySQL
   protocols and SQL, Redis RESP, MongoDB, Elasticsearch/OpenSearch, Cassandra CQL, InfluxDB line
   protocol, Kafka, S3, SQLite's embedded API and file format, and more; order in D5). Behavior
   matches the original closely enough that its own client libraries and test suites pass. (D5)
3. **Single.** One binary that is both the server and an embeddable library (in-process, like
   SQLite). One process, one data directory (or one file when embedded), one config, one backup.
   No helper services to install. (D3)
4. **Lightweight.** The binary size, idle memory, startup time and on-disk overhead are measured
   for every release and may only shrink. Features the user doesn't use cost nothing at runtime.
   Any growth needs a stated reason and is handled like a performance regression. (D6)
5. **The fastest, with the best performance.** Every hot path gets a benchmark when it is written,
   compared with the best existing database for that workload (SQLite, PostgreSQL, MySQL, DuckDB,
   ClickHouse, Redis/Valkey/Dragonfly, Elasticsearch, MongoDB, Cassandra/ScyllaDB and others) on the
   same hardware. A slower result blocks the PR until it is fixed, or until a research subagent
   concludes the cost is the unavoidable, minimised price of correctness or security (D7).
6. **Every scale.** From a microcontroller and a single small file (smaller and faster than SQLite)
   to a distributed cluster of many machines across regions holding billions of billions of rows
   (exabytes), with replication, sharding, distributed transactions and online growth. The same
   engine and the same data format at every size; never write code that assumes one size. (D4)
7. **Everywhere.** Every OS (Linux, Windows, macOS, the BSDs, Android, iOS, EggOS, the browser
   through WebAssembly, `no_std` embedded), every CPU architecture, every cloud (AWS, Azure, Google
   Cloud and the rest), Kubernetes, containers and bare metal. Platform code lives behind one
   platform layer, never mixed into the engine. (D8)
8. **Correct and safe first, then fast.** Data is never lost or corrupted: crash safety is tested,
   not assumed (fault injection, deterministic simulation, fuzzing every parser and wire
   protocol), and hostile input never crashes or hangs the server. (D9)

## Code
- **Everything written by us, in Rust, from scratch.** No code from other databases (SQLite,
  PostgreSQL, RocksDB, Redis, Lucene and so on). Implement from public specifications, papers and
  protocol documentation (clean room). No third-party crates in what we ship; a research subagent may
  approve a permissive (MIT, Apache-2.0, BSD) crate for tests, benchmarks or build tooling only.
  Never add GPL, AGPL, SSPL or other copyleft code. (D1)
- `unsafe` only where unavoidable, each block with a `// SAFETY:` comment explaining why it is
  sound (clippy denies undocumented unsafe blocks). One unsafe operation per block.
- **No other comments in code.** Public items get rustdoc (`///`, `//!`), which `cargo doc` builds
  in CI. Everything else is explained by names, small functions, types and tests (global rule
  "Code comments: none, the code explains itself").
- Release builds are what we measure and ship (LTO, one codegen unit).

## Layout
- `crates/eggdb`: the library and the `eggdb` binary. New subsystems become workspace crates
  under `crates/` when they have their own boundary (storage, transactions, query, protocols,
  platform layer); the engine core never depends on a protocol or a platform.
- `docs/decisions.md`: numbered, append-only decisions. `docs/roadmap.md`: milestones and status.

## Commands
- `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`,
  `cargo doc --workspace --no-deps`. CI runs exactly these (job `check`, the required status check).
- EggDB is public, so CI goes for build time, not billed minutes (global CI rule).

## Pull requests: merge them yourself
The owner doesn't merge PRs by hand. After opening a PR, in the same session:
1. Review it twice yourself (global PR workflow), then launch an independent review subagent with the
   PR, the diff and this file, and ask for MERGE or BLOCK. Fix every blocking issue and ask again.
2. When the verdict is MERGE, both passes are clean and CI is green, squash-merge it
   (`gh pr merge <n> --squash --delete-branch`), then confirm every workflow the merge started
   succeeded.

An open question is never a reason to leave a PR unmerged: answer it with a research subagent,
record the answer in the PR under "Decided by research" (and in docs/decisions.md for architecture),
and merge. Only credentials, money, new accounts and going against an owner rule wait for the owner.
A PR that changes the license is never merged.

## Releases
Semantic versioning, tags `vMAJOR.MINOR.PATCH`. Until 1.0, each finished milestone bumps MINOR and
fixes in between bump PATCH. Cut the release as soon as a milestone is merged and `main` is green:
version bump PR, tag, and a GitHub Release with notes and binaries for the platforms that build.
Publishing to package registries (crates.io and others) needs accounts and tokens, so the owner sets
those up first.

## Owner decisions
The decisions above are recorded in docs/decisions.md (D1 to D9). Record every further
architecture decision there as a numbered entry; a later entry may supersede an earlier one.
