# Architecture decisions

Numbered, append-only. A later decision may supersede an earlier one; say so in both entries.
Status is **Decided** (owner), **Decided by research**, or **Decided by the build loop** (engineering).

## D1 Everything written by us, in Rust, from scratch — Decided (owner, 2026-10-23)
EggDB is our own code in Rust. No code from other databases: we implement storage, transactions,
query processing and every protocol from public specifications, papers and documentation (clean
room). What we ship has no third-party crates; permissive crates may be used for tests, benchmarks
and build tooling only, when a research subagent approves them. Licensed MIT; no copyleft code
(GPL, AGPL, SSPL, BSL) ever.

## D2 All in one: every kind of database in one engine — Decided (owner, 2026-10-23)
One engine covers every kind of database in use today:
- key-value and caching, with expiry, eviction, pub/sub, streams and queues;
- relational SQL with ACID transactions;
- full-text search;
- documents and JSON;
- time series and columnar analytics;
- graphs, vectors for AI, geospatial and wide-column data;
- blobs and objects.

They share one storage engine, one transaction system and one query layer, so a query or a
transaction can span models (join a table with a search result and a vector match, in one
snapshot). New models are added to that core, never as a separate database beside it.

## D3 Single: one binary, server and embedded library — Decided (owner, 2026-10-23)
EggDB is one binary that runs as a server and is also a library linked into an application
(in-process, like SQLite). One process, one data directory, or one file when embedded, one config,
one backup, no helper services.

## D4 Every scale, one engine — Decided (owner, 2026-10-23)
EggDB runs from a microcontroller and a single small file (smaller and faster than SQLite) to a
distributed cluster holding billions of billions of rows across many machines and regions. The
cluster side covers replication, sharding, distributed transactions and online growth. It is the
same engine and the same data format at every size, so an embedded database can grow into a
cluster without an export.

## D5 We support everything people already use — Decided (owner, 2026-10-23); order decided by research
Applications switch to EggDB without code changes, because EggDB speaks the wire protocols and
query languages they already use. The order below follows what most applications use and what
reuses the most of the core, and a research subagent may refine it:
1. SQLite's embedded API and file format, and PostgreSQL's wire protocol and SQL;
2. Redis RESP, for caching;
3. MySQL's protocol and SQL;
4. Elasticsearch/OpenSearch REST, for search;
5. MongoDB's wire protocol;
6. then Cassandra CQL, InfluxDB line protocol, Kafka, S3 and others.

"Supported" means the original's own client libraries and conformance or regression tests pass
against EggDB.

## D6 Lightweight: size, memory and startup budgets that only shrink — Decided (owner, 2026-10-23)
Every release measures and publishes:
- the binary size;
- idle memory;
- startup time;
- on-disk overhead per row and per index entry.

The numbers may only go down. Features that aren't used cost nothing at runtime. Concrete budgets
are set once the first embedded build works, from measurements against SQLite, Redis and
PostgreSQL on the same hardware.

## D7 The fastest database, with the best performance — Decided (owner, 2026-10-23)
For each workload, EggDB must beat the best existing database for it on the same hardware:
- SQLite for embedded;
- PostgreSQL and MySQL for transactions;
- DuckDB and ClickHouse for analytics;
- Redis, Valkey and Dragonfly for caching;
- Elasticsearch for search;
- MongoDB for documents;
- Cassandra and ScyllaDB for wide-column.

Measured with the standard benchmarks (TPC-C, TPC-H, YCSB, redis-benchmark, the ClickBench and
search suites) and with micro-benchmarks for every hot path, gated in CI against a committed
baseline.

A slower result blocks the PR until it is fixed. The one exception is when a research subagent
concludes the cost is the unavoidable price of correctness or security: already minimised, and
compared with how the other databases pay for the same thing. Then the PR records the reasoning
here and updates exactly the affected baseline lines. Nobody waits for the owner, who can overrule
later.

## D8 Everywhere: every OS, CPU and cloud — Decided (owner, 2026-10-23)
EggDB runs on every OS:
- Linux, Windows and macOS;
- the BSDs, Android and iOS;
- EggOS;
- the browser, through WebAssembly;
- `no_std` embedded targets.

It also runs on every CPU architecture and every cloud, with Kubernetes, containers and bare
metal. Operating-system and hardware code lives behind one platform layer (files, I/O, memory,
threads, clocks, networking), never in the engine. CI builds and tests on as many of these as it
can.

## D9 Correct and safe first — Decided (owner, 2026-10-23)
Committed data is never lost or corrupted. Crash safety, recovery and replication are tested
continuously: fault injection, deterministic simulation of disks, networks and clocks, and
fuzzing of every parser, file format and wire protocol. Hostile input never crashes or hangs the
server, and every resource a client can make it use has a limit.
