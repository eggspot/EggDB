# EggDB roadmap

Each milestone ships as a release when it is merged (CLAUDE.md, "Releases"), except in the week
before v0.2.0, when releases are batched into it (D76). The order puts the core first and then
the models and protocols that most applications use (D5). The build loop may refine it through
research, recorded in docs/decisions.md.

## Milestones
| # | Milestone | Status |
|---|-----------|--------|
| M0 | Repo skeleton: workspace, pinned toolchain, CI, docs | **done** |
| M1 | Platform layer: files, direct and async I/O, memory, threads, clocks, behind one interface (D8) | **done**, released as v0.1.0 (D75): files, clocks, the crash simulator (D59), direct I/O with aligned buffers (D60), memory with huge pages and its simulator (D61), CPU sets, pinning and cache padding (D62), named, pinned thread starts (D63), the first review (D64), the CPU topology (D65) and asynchronous file I/O through a completion queue with a worker pool and its simulator (D66) and io_uring behind it, with backends chosen automatically or by request (D67) done, the second review (D68), I/O completion ports on Windows on request (D69), networking's first part: TCP through a completion queue with a portable poller and a simulated network (D70), and epoll and kqueue behind it, with connects that never block (D71), and timeouts: a timed reap and cancel, with simulated time, latency and partitions (D72), and `poll(2)` on the BSDs and illumos (D73), and fewer system calls per connection and per wait: one-shot interest kept between waits, sockets made ready in one call, one accept woken per connection (D74), and the wrap-up review. The rest of the platform work (the "Missing" column below) continues alongside later milestones |
| M2 | Storage engine: our own versioned, checksummed page format, buffer management, write-ahead log ready to become a consensus log, hybrid logical clock timestamps, tenant and shard keys in the key layout (D20), checkpoints, crash recovery, partitioning; the B-tree/LSM/hybrid choice made by research with benchmarks | **in progress**, toward v0.2.0 (D76): the design and the file format's spec (D77), CRC-32C, the superblock in two header slots, page checksums, the page file with crash-safe commits, a free list and file locks (D78), the page cache with a fixed memory budget, clock eviction, pinned pages and no-steal commits (D79), benchmarks gated against a committed baseline (D80), the write-ahead log inside the file with redo on open and three durability modes (D81), the B+tree with get, put and splits up to the root, fence keys and prefix truncation (D82), delete with merges and cursors for range, prefix and reverse scans (D83) |
| M3 | Transactions: MVCC snapshots, serializable isolation, deadlock-free locking, durability modes | |
| M4 | Embedded key-value API and the single-file database (D3); first SQLite and Redis comparisons and the first size, memory and startup budgets (D6); the first release binary with its download and install times measured (D14) | |
| M5 | SQL: parser, planner, optimizer and executor; SQLite-compatible embedded SQL and the SQLite file format; the stable C API and the first official drivers (Rust, C#, Java, Node.js, Python, Go) in embedded mode (D16); the `eggdb` CLI client (D15) | |
| M6a | Server and the PostgreSQL wire protocol and SQL dialect, with authentication (SCRAM), TLS and roles from the start, since the server never listens without authentication (D22, D23, D45); PostgreSQL tools (psql, pgAdmin, DBeaver) working (D12); restoring plain-SQL `pg_dump` output (D13) | |
| M6b | The rest of security: LDAP, Kerberos, OpenID Connect, client certificates, row- and column-level security, masking (D22) | |
| M6c | Operations: metrics, logs, slow queries, statement statistics, `EXPLAIN ANALYZE` (D25) | |
| M6d | The built-in MCP server: schema discovery, querying, agent permissions, prompt-injection safety (D11, D45) | |
| M6e | The first GUI client screens on the desktop and in the browser, designed by Claude Fable (D15) | |
| M6f | The official drivers over the network and their main ORMs (D16, D17) | |
| M7 | Caching: in-memory tables with expiry and eviction, pub/sub, streams; Redis RESP | |
| M8 | Full-text search: analyzers, inverted indexes, BM25 ranking; Elasticsearch/OpenSearch REST | |
| M9 | Documents and JSON; the MongoDB wire protocol | |
| M10 | The other SQL dialects, each as itself (D10): MySQL/MariaDB, Microsoft SQL Server (T-SQL, TDS) and Oracle (SQL, PL/SQL) | |
| M11 | Columnar storage and analytics; time series | |
| M12 | Vectors (approximate nearest neighbours) and geospatial | |
| M13 | Graphs | |
| M14 | Backup, point-in-time recovery and verified restores (D24); replication and high availability: read replicas (synchronous, asynchronous, cascading), failover, consistency per request (D18) | |
| M15 | Sharding (hash, range, time, region) with online resharding, distributed SQL with consensus and serializable distributed transactions, multi-region clusters (D4, D18) | |
| M16 | Messaging and queues (D33), in order: Kafka, AMQP 0-9-1 (RabbitMQ), MQTT, SQS, NATS, AMQP 1.0, STOMP, with transactional outbox and inbox; wide-column (CQL), objects (S3), InfluxDB line protocol | |
| M17 | Every platform: Windows, macOS, BSDs, Android, iOS, EggOS, WebAssembly, `no_std` (D8); packages and cloud deployments | |
| M18 | The other ways of scaling (D18): multi-primary, leaderless quorums, storage separated from compute, serverless, tiered storage, edge copies, offline-first sync, federation | |

## Next: v0.2.0, the first user release (2026-11-01, D76)
An embedded, crash-safe, transactional key-value store in one file, with a Rust API, the
`eggdb` command-line tool and `eggdb ui`, a terminal UI. It brings forward M2's storage
engine, a single-writer slice of M3, M4's embedded API and the one-shot commands of M5's CLI.
No network server, SQL, other protocols, compression or concurrent writers yet. Feature freeze
2026-10-30 23:59.

| # | Must-have, in order | Status |
|---|---------------------|--------|
| 1 | Storage design decision by research and the spec in [format.md](format.md), with CRC-32C, the superblock in its two header slots and the page checksums (D77) | **done** |
| 2 | File layout: the two alternating header slots, the page allocator and free list, file growth, an exclusive file lock in the platform layer (D78) | **done** |
| 3 | Page cache with a fixed memory budget, eviction and pinning (D79), and the benchmark harness with a gated baseline (D80) | **done** |
| 4 | Write-ahead log: sequence numbers, checksums, group commit, durability modes (D81) | **done**: the log inside the file, commits through it, redo on open. Checkpoints, recycling segments and truncating the log stay with row 7 |
| 5 | B+tree: get, put and splits | **done**: slotted pages with fence keys and prefix truncation, a fixed root, pages checked when read from the file, `BTree::check` (D82) |
| 6 | B+tree: delete, and cursors for range, prefix and reverse scans | **done**: delete with merges of pages under a quarter full and root collapse, cursors that move between leaves through the root by fence key and hold no page between calls (D83) |
| 7 | Checkpoints, crash recovery and log truncation | **next** |
| 8 | Crash tests in the simulator: a crash at every I/O step, torn writes, injected faults | |
| 9 | Transactions: one writer with snapshot readers, atomic commit, rollback, hybrid-clock commit stamps | |
| 10 | Embedded Rust API (`Database::open`, read and write transactions, get, put, delete, scan) | |
| 11 | Large values on overflow pages, size limits, free-space reuse | |
| 12 | A model test against an in-memory map, and a fuzzer for hostile files | |
| 13 | The `eggdb` CLI: `put`, `get`, `del`, `scan`, `check`, `stats` (designed by Claude Fable, D15) | |
| 14 | `eggdb ui`, part 1: the design in `docs/ux/`, our own terminal layer, the main screen | |
| 15 | `eggdb ui`, part 2: edit, add and delete keys, `check`, readable errors, help, fake-terminal tests | |
| 16 | `eggdb export` and `import` (JSON lines) | |
| 17 | Benchmarks against SQLite as the committed baseline, plus binary size, startup and memory | |
| 18 | Release engineering: CLI smoke tests on every target, a committed v0.2.0 sample file CI keeps reading, the README quickstart, `docs/embedded.md`, `docs/cli.md`, release notes, the correctness statement | |

Nice-to-have once the must-haves are merged, before the freeze: an interactive `eggdb shell`,
online `eggdb backup`, a C API with a header. The cut order and what is never cut are in D76.

## Where we are
Rough progress toward the whole goal: **about 2%**. The skeleton, the decisions and the platform
layer exist, and v0.1.0 is the first release; there is no storage engine, transaction system or
query layer yet.

| Area | Status | Missing |
|------|--------|---------|
| Platform layer (M1) | files with positional, direct and asynchronous I/O and shared or exclusive file locks (a completion queue on io_uring on Linux, I/O completion ports on Windows on request, and a worker pool elsewhere), aligned buffers, directory sync, clocks, memory (page sizes, explicit and transparent huge pages), CPU sets, thread pinning on Linux and Windows and pinned thread starts, cache padding, CPU topology (cores, packages, NUMA nodes, last-level caches) on Linux, Android and Windows with counts on Apple's systems, the same error for the same wrong path on every OS, deterministic crash, fault, memory, CPU and I/O-ordering simulation; TCP listeners and connections through a completion queue that waits in epoll on Linux and Android, in kqueue on Apple's systems and in `poll(2)` on FreeBSD, NetBSD, illumos and Solaris (a portable poller elsewhere), with a simulated network of hosts (ordered bounded streams, seeded delivery, latency, partitions, resets, injected faults) on a simulated clock; timed reaps and cancel on the network queue | IOCP as Windows' automatic choice (after a benchmark), batched io_uring submission, retransmission timeouts and one-way cuts in simulation, timed reaps and cancel on the file I/O queue, sockets on io_uring and IOCP, non-blocking connects and CI runners on the BSDs and illumos, OpenBSD and DragonFly, Unix sockets and UDP, per-device alignment, NUMA memory placement and locked memory, performance and efficiency cores, pinning and topology on the BSDs, file locks on Solaris (D78), a memory allocator of our own (D75), and the M1 wrap-up review's follow-ups: a guard against completion packets after an immediate IOCP success, `NotConnected` from a reset stream's address and shutdown in simulation, tests that cancel a pending connect and mix port and worker operations on IOCP, and no readiness call in a reap that is already satisfied |
| Storage and transactions (M2, M3) | the design (D77), the file format's spec ([format.md](format.md)), CRC-32C with the processor's instructions, the superblock in two header slots, page headers and their checksums, the page file: crash-safe commits, growth, a free list, rollback, one writer or many readers by file lock; the page cache: a fixed budget allocated once, clock eviction, pinned page handles, changes kept until commit, statistics; a benchmark harness with counts gated exactly and times gated as ratios (D80); the write-ahead log: segments inside the file, checksummed records, commits that log whole pages before writing them in place, group commit, the Full, Normal and Off durability modes, redo on open that tells a torn tail from damage (D81); the B+tree: get, put, splits up to the root, fence keys, prefix truncation, every page checked when read from the file, and a whole-tree check (D82), delete with merges and root collapse, and cursors for range, prefix and reverse scans (D83) | checkpoints and log truncation, crash tests, transactions (the v0.2.0 list above), then MVCC, serializable isolation, partitioning and the LSM bake-off |
| Models (D2) | none | key-value, relational, search, documents, time series, columnar, graphs, vectors, geospatial, wide-column, objects, queues |
| SQL dialects (D10, D47) | none (0%) | PostgreSQL, SQLite, MySQL/MariaDB, T-SQL, Oracle, then GoogleSQL, Snowflake, Spark, ClickHouse, Trino, Db2, Sybase |
| Protocols (D5) | none (0%) | PostgreSQL, RESP, MongoDB, Elasticsearch, CQL, Kafka, AMQP, MQTT, S3 and the rest |
| Platforms (D8) | Linux (glibc, and musl on x86-64), macOS and Windows built and tested in CI, and release builds for x86-64 and AArch64 on each, run where the runner can (D75); the BSDs, illumos, Solaris, Android, iOS and WASI type-checked in CI | BSDs, Android, iOS, EggOS, browser WebAssembly, `no_std` |
| Scale (D4, D18) | none | a single file first (M4), then replicas, sharding and the rest of D18 |

Every protocol milestone also covers that database's tools (D12) and its backups and dumps (D13):
an application's whole toolchain and its data move over together.

Tracks that run alongside the milestones:
- **The client (D15).** It starts now, not at M5:
  - our own Rust GUI toolkit, for the desktop and the browser from one codebase;
  - Claude Fable's design in `docs/ux/`;
  - the CLI;
  - the familiar layouts;
  - each new model and dialect added to the GUI and CLI as it lands.

  The desktop GUI is a must-have.
- **The drivers (D16):** each language and ORM added and proven by its own test suite.

Every milestone that adds a model or dialect also brings all of its data types, with their exact
behavior (D19).

More tracks that run alongside the milestones, each starting where its dependencies land:
- **The project (D21):** contributing guide, code of conduct, security policy, the stability
  promise, and signed and reproducible releases.
- **Security and compliance (D22, D23, D26):** with the server first, deepened every release.
- **Operations (D24, D25):** backups and monitoring as soon as there is data to protect and a
  server to watch.
- **Programmability (D27):** each dialect's procedural language with that dialect, WebAssembly
  functions and extensions after the server.
- **Time travel, derived data and change streams (D28):** after transactions (M3) and the server
  (M6).
- **Interchange and lakehouse (D29):** after columnar storage (M11).
- **Text and locales (D30):** collation with SQL (M5), analyzers with full-text search (M8).
- **AI (D31):** after vectors (M12) and the MCP server (M6).
- **Relational completeness and multi-tenancy (D32):** across the SQL milestones, tracked as a
  conformance list.

From the second gap review (D37–D45):
- **M6 is split** into M6a to M6f (table above), with security in the first piece (D45).
- **Generated APIs and realtime (D37)** follow the server.
- **The extra models (D38)** follow the SQL dialects.
- **Observability storage (D39)** follows time series (M11).
- **Engine internals (D40)** run from M1 (I/O) and M5 (the optimizer).
- **Embedded detail (D41)** runs from M4.
- **The console and notebooks (D42)** run with the client track.
- **Legal checks (D43)** come before each release.
- **Public proof (D44)** runs from the first benchmarks.

More tracks (D46), and any further missing capability is added here without asking the owner:
- **Business intelligence:** our own dashboards, reports, semantic layer, scheduling, sharing and
  embedding.
- **ETL and ELT pipelines:** our own connectors, transformations, orchestration and change data
  capture.
- **Big-data compatibility:** a Spark connector, an Iceberg REST catalog, federated SQL and Kafka
  Connect.

From the third gap review (D47–D58), each after its foundations:
- **Dialects and APIs (D47–D49)** follow the SQL and protocol milestones.
- **Industry standards (D50)** follow the messaging work.
- **Catalog and master data (D51)** follow the console.
- **Sharing and privacy (D52)** follow branching (D28).
- **Retention and sovereignty (D53)** follow backups and compliance.
- **Coordination (D54)** follows consensus (M15).
- **Streaming (D55)** follows the messaging work (M16).
- **Content (D56)** follows full-text search.
- **Science and GIS (D57)** follow analytics.
- **Hyperscale operations (D58)** follow clusters.
