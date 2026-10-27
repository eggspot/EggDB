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
| M2 | Storage engine: our own versioned, checksummed page format, buffer management, write-ahead log ready to become a consensus log, hybrid logical clock timestamps, tenant and shard keys in the key layout (D20), checkpoints, crash recovery, partitioning; the B-tree/LSM/hybrid choice made by research with benchmarks | **in progress**, toward v0.2.0 (D76): the design and the file format's spec (D77), CRC-32C, the superblock in two header slots, page checksums, the page file with crash-safe commits, a free list and file locks (D78), the page cache with a fixed memory budget, clock eviction, pinned pages and no-steal commits (D79), benchmarks gated against a committed baseline (D80), the write-ahead log inside the file with redo on open and three durability modes (D81), the B+tree with get, put and splits up to the root, fence keys and prefix truncation (D82), delete with merges and cursors for range, prefix and reverse scans (D83), checkpoints, recovery from the last checkpoint and recycled log segments (D84), the crash sweep (D85), transactions with one writer, snapshot readers and hybrid-clock commit stamps (D86), large values on overflow pages and a check that accounts for every page (D88) |
| M3 | Transactions: MVCC snapshots, serializable isolation, deadlock-free locking, durability modes | |
| M4 | Embedded key-value API and the single-file database (D3); first SQLite and Redis comparisons and the first size, memory and startup budgets (D6); the first release binary with its download and install times measured (D14) | **in progress**, toward v0.2.0 (D76): the embedded Rust API (D87), values up to 1 GiB (D88), the first SQLite comparison and the size, memory and start-time baselines (D94) |
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
engine, a single-writer slice of M3, M4's embedded API and the one-shot commands of M5's CLI,
and the first part of SQL (item 19) if it is merged before the freeze. No network server, other
protocols, compression or concurrent writers yet. Feature freeze
2026-10-30 23:59.

| # | Must-have, in order | Status |
|---|---------------------|--------|
| 1 | Storage design decision by research and the spec in [format.md](format.md), with CRC-32C, the superblock in its two header slots and the page checksums (D77) | **done** |
| 2 | File layout: the two alternating header slots, the page allocator and free list, file growth, an exclusive file lock in the platform layer (D78) | **done** |
| 3 | Page cache with a fixed memory budget, eviction and pinning (D79), and the benchmark harness with a gated baseline (D80) | **done** |
| 4 | Write-ahead log: sequence numbers, checksums, group commit, durability modes (D81) | **done**: the log inside the file, commits through it, redo on open. Checkpoints, recycling segments and truncating the log stay with row 7 |
| 5 | B+tree: get, put and splits | **done**: slotted pages with fence keys and prefix truncation, a fixed root, pages checked when read from the file, `BTree::check` (D82) |
| 6 | B+tree: delete, and cursors for range, prefix and reverse scans | **done**: delete with merges of pages under a quarter full and root collapse, cursors that move between leaves through the root by fence key and hold no page between calls (D83) |
| 7 | Checkpoints, crash recovery and log truncation | **done**: checkpoints explicit and automatic (after 1 MiB of log, or before the log outgrows the seven recycled segments), recovery from the last checkpoint only, segments recycled with sequence numbers never used twice, extra segments given to the free list (D84) |
| 8 | Crash tests in the simulator: a crash at every I/O step, torn writes, injected faults | **done**: a sweep over the whole stack fails every I/O operation of a B+tree workload in each durability mode, ends with a reopen or a power loss, and checks the recovered tree holds exactly one commit, no older than promised; it also crashes during recovery and keeps writing after it ([testing.md](testing.md), D85) |
| 9 | Transactions: one writer with snapshot readers, atomic commit, rollback, hybrid-clock commit stamps | **done**: a `Store` with one write transaction at a time, committed atomically through the log or rolled back (explicitly, on drop, or by itself when a change fails half-way), read transactions on any thread reading snapshots kept by in-memory before-images, and hybrid-clock commit stamps that never go back (D86) |
| 10 | Embedded Rust API (`Database::open`, read and write transactions, get, put, delete, scan) | **done**: `eggdb::Database` with `Options`, read and write transactions, closures and one-step calls, double-ended scans over any key range or prefix, a second writer that waits with a timeout, `check`, `stats`, `close`, and errors with a kind to match on ([embedded.md](embedded.md), D87) |
| 11 | Large values on overflow pages, size limits, free-space reuse | **done**: values too large for a leaf on write-once chains of checksummed overflow pages, up to 1 GiB, freed through transactions and reused after the commit, read by get and scans, and a check that validates every chain and accounts for every page of the file (D88) |
| 12 | A model test against an in-memory map, and a fuzzer for hostile files | **done**: seeded random transactions, rollbacks, large values, scans, checks, reopens, power losses and injected faults compared with a `BTreeMap`, through the store on the simulator and through the embedded API; a seeded fuzzer that damages valid files 22 ways and requires errors, never a panic, a hang or an allocation beyond the file; an open for writing makes the older header slot a copy, closing issue #39 ([testing.md](testing.md), D89) |
| 13 | The `eggdb` CLI: `put`, `get`, `del`, `scan`, `check`, `stats` (designed by Claude Fable, D15) | **done**: `create`, `put`, `get`, `del`, `scan`, `check`, `stats`, `help` and `version` with a hand-written parser, byte arguments (text, hex, `@file`, stdin), `text`/`json`/`hex`/`raw` output chosen by whether stdout is a terminal, the check and stats reports, exit codes 0 to 5, colour only on terminals through the platform layer, reserved names for the commands to come ([cli.md](cli.md), [ux/cli.md](ux/cli.md), D90) |
| 14 | `eggdb ui`, part 1: the design in `docs/ux/`, our own terminal layer, the main screen | **done**: Claude Fable's design ([ux/ui.md](ux/ui.md)); the platform layer's raw terminal (raw mode, alternate screen, size and resizes, input with a timeout, restore on drop, panics and `SIGTERM`/`SIGHUP`/`SIGQUIT` or console close); the `eggdb-ui` crate with the `Terminal` trait, a fuzzed input decoder, a diffing ANSI renderer, a fake terminal with golden screens, and the read-only browser: windowed key list with paging and a prefix filter, text/JSON/hex viewer, help, small-terminal and mono/ASCII/plain builds; `eggdb ui <file>` ([cli.md](cli.md), D91) |
| 15 | `eggdb ui`, part 2: edit, add and delete keys (opening read-write, `--read-only`), the `check` screen, the message modal (`m`), toasts that expire, and the remaining fake-terminal scenarios (10 to 16, 18, 23, 24) | **done**: our own one-line and multi-line editors (escape spelling, literal pastes, undo and redo, a 4 MiB bound), every change confirmed in a modal and committed in one write transaction begun only then, failures shown by error kind (a busy writer waited for on `r`, reopen after `NeedsReopen`), the check on a worker thread, the message modal and toasts that expire after 8 s; `eggdb ui` opens read-write, `--read-only` refuses changes; scenarios with golden screens and real-file tests for persistence, read-only and a model of random edits ([cli.md](cli.md), [ux/ui.md](ux/ui.md), D92) |
| 16 | `eggdb export` and `import` (JSON lines) | **done**: the eggdb-jsonl format (a header, one line per entry with keys and values as text or hex, a trailer with the count), export from one snapshot with bounded memory, import with a strict streaming reader, the whole export checked before writing from a file, batched commits, `--on-conflict error\|overwrite\|skip`, `--dry-run`, and new files built aside and renamed into place; a seeded fuzzer for hostile exports ([cli.md](cli.md), D93) |
| 17 | Benchmarks against SQLite as the committed baseline, plus binary size, startup and memory | **done**: `crates/eggdb-sqlite-compare`, a crate with its own workspace that compares point gets, scans, batched puts and durable commits with SQLite 3.54.1 under matching settings, gated as `ratio_to_sqlite` lines in a parallel Linux `compare` CI job; the `eggdb` binary's size, start time and peak memory gated by `eggdb-bench`, the peak measured from outside through the platform layer; a CI check that only our own crates ship ([benchmarks.md](benchmarks.md), D94) |
| 18 | Release engineering: CLI smoke tests on every target, a committed v0.2.0 sample file CI keeps reading, the README quickstart, `docs/embedded.md`, `docs/cli.md`, release notes, the correctness statement | **done**: `scripts/smoke-test.sh` runs every command, an export and import round trip, the sample files and a scripted `eggdb ui` session in a pseudo-terminal against each release binary its runner can execute, and against the debug binary in CI on Linux, macOS and Windows; two v0.2.0 sample files and an export, pinned by their bytes and read, checked, replayed and written to by `sample_files.rs`; the README quickstart, the guides' Rust examples compiled by `cargo test` and the UI screens in [cli.md](cli.md) checked against the UI's own frames; [correctness.md](correctness.md) and [releases/v0.2.0.md](releases/v0.2.0.md) (D96) |
| 17b | Batched random overwrites at least as fast as SQLite's, required by D7 from item 17 ([issue #52](https://github.com/eggspot/EggDB/issues/52)) | **done**: a flush longer than the recycled log segments takes its new segments first and ends in a recycled one, and the checkpoint after it cuts the new ones off the file instead of freeing them, so long commits no longer grow the file (issue #44) and EggDB takes 0.69 times SQLite's time on a Mac and 0.61 times on Linux; `Normal` stays about 1.1 by two header syncs that correctness needs, decided by research ([benchmarks.md](benchmarks.md), D97) |
| 19 | SQL core: a catalog and tables on the key-value engine (row encoding, primary keys, secondary indexes), the PostgreSQL dialect first: CREATE/DROP TABLE, INSERT, SELECT with WHERE, ORDER BY, LIMIT, inner and left joins, GROUP BY with aggregates, UPDATE, DELETE, BEGIN/COMMIT/ROLLBACK, the common types; run with `eggdb sql` | **part 1 done**: `eggdb sql` runs SQL from `-c`, a script or stdin with psql's output and PostgreSQL's errors and exit codes; the `eggdb-sql` crate over any ordered key-value store with tables, primary keys and versioned rows under a reserved key prefix, `CREATE`/`DROP TABLE`, `INSERT` with `DEFAULT` and `RETURNING`, `SELECT` with `WHERE`, `ORDER BY`, `LIMIT` and `OFFSET`, `UPDATE`, `DELETE`, transaction blocks with PostgreSQL's aborted state, the integer, float, boolean and text types, three-valued logic and `0A000` for the rest ([sql.md](sql.md), D98) |
| 19b | Users can join tables (inner and left joins) and summarise them (`GROUP BY` with `count`, `sum`, `avg`, `min`, `max`, `HAVING`, `DISTINCT`) | **done**: inner, left and cross joins with `ON`, `USING` and comma joins, aliases, qualified names and `t.*`, by key lookup, hash table or nested loop, decoding only the columns a statement names and applying each `WHERE` part as soon as its tables are joined; `GROUP BY` with `count`, `sum`, `avg`, `min`, `max` (with `DISTINCT`), `HAVING`, grouping by a primary key, `SELECT DISTINCT`, PostgreSQL's errors (`42702`, `42712`, `42803`, ...), a 1 GiB bound on what a statement holds (`53200`), a differential test against SQLite; `RIGHT`/`FULL` joins, subqueries and the other aggregates answer `0A000` ([sql.md](sql.md), D100) |
| 19c | Users can create secondary indexes (`CREATE INDEX`, `UNIQUE`) that speed up `WHERE` and `ORDER BY` | planned |
| 19d | Users can store `numeric`, `timestamp` and `json`/`jsonb` values and use `serial` and identity columns | planned |
| 19e | Users get SQL point `SELECT`s, bulk `INSERT`s and joins at least as fast as SQLite's (required by D7 before v0.3.0, D99, D100) | **in progress**: joins by primary key now take 0.99 times SQLite's time on a Mac and 1.32-1.43 on Linux (from 3.5 and 3.9), `GROUP BY` 0.94 on a Mac and 0.94-1.16 on Linux, gated; the Linux join is still to bring to parity (D100); point selects drifted from 0.85 to about 0.96 on Linux with the wider `SELECT` path, not yet explained by a profile, and their line is not loosened (D100); from 1.95 to 1.23 times SQLite's time for point selects (1.25 to 0.85 on Linux) and from 3.16 to 0.89 for bulk inserts (2.63 to 0.83 on Linux), now faster than SQLite (cached definitions by commit, borrowed tokens, streamed rows, single-descent `put_if_absent`, splits that move entries, append splits at the rightmost leaf without a descent, a direct `INSERT ... VALUES` path, D99); next, for point selects on macOS: an AST that borrows the SQL text, a plan cache |
| 20 | `eggdb shell`: an interactive prompt with history and tab completion for key-value commands and SQL; a query screen in `eggdb ui`; readable errors in each dialect's style; a correctness suite from each database's documented behaviour | planned |
| 21 | The PostgreSQL wire protocol v3: psql and drivers connect (simple and extended query, SCRAM-SHA-256 with our own SHA-256, HMAC and PBKDF2 checked against published vectors, SSLRequest answered no), CI connects with psql and a common driver; every listener binds only to loopback or a local socket, without TLS yet, and requires authentication (recorded when it lands) | planned |
| 22 | The MySQL/MariaDB dialect and client/server protocol | planned |
| 23 | The SQL Server T-SQL dialect and the TDS protocol | planned |
| 24 | The SQLite dialect through `eggdb sql --dialect sqlite` and the embedded API | planned |
| 25 | The Oracle dialect (`DUAL`, `FETCH FIRST`/`ROWNUM`, `NVL`, `VARCHAR2`) through `--dialect oracle`; no Oracle wire protocol (D10) | planned |
| 26 | Release extra: collections, named key spaces in one file, in the API, the CLI, the shell and the UI | planned |
| 27 | Release extra: expiring keys (TTL) and atomic counters | planned |
| 28 | Release extra: online backup and compaction (`eggdb backup`, `eggdb compact`) | planned |
| 29 | Release extra: a C API with `eggdb.h` and a Python ctypes example | planned |

**Next**: item 19's next part, secondary indexes (19c), then the rest of the
SQL block in the owner's order (19c to 25) and the release extras (26 to 29), as far as the freeze
on 2026-10-30 23:59 allows. The cut order and what is never cut are in D76.

## Where we are
Rough progress toward the whole goal: **about 3%**. The platform layer, the storage engine with
its write-ahead log, transactions and the embedded key-value API, the `eggdb` command and its UI
exist, v0.1.0 is released and v0.2.0 is being finished; the query layer has its first part, SQL
tables with primary keys, joins and `GROUP BY` in the PostgreSQL dialect through `eggdb sql`
(D98, D100). There is no network
server or second data model yet.

| Area | Status | Missing |
|------|--------|---------|
| Platform layer (M1) | files with positional, direct and asynchronous I/O and shared or exclusive file locks (a completion queue on io_uring on Linux, I/O completion ports on Windows on request, and a worker pool elsewhere), aligned buffers, directory sync, clocks, whether a standard stream is a terminal and takes colour, memory (page sizes, explicit and transparent huge pages), CPU sets, thread pinning on Linux and Windows and pinned thread starts, cache padding, CPU topology (cores, packages, NUMA nodes, last-level caches) on Linux, Android and Windows with counts on Apple's systems, the same error for the same wrong path on every OS, deterministic crash, fault, memory, CPU and I/O-ordering simulation; TCP listeners and connections through a completion queue that waits in epoll on Linux and Android, in kqueue on Apple's systems and in `poll(2)` on FreeBSD, NetBSD, illumos and Solaris (a portable poller elsewhere), with a simulated network of hosts (ordered bounded streams, seeded delivery, latency, partitions, resets, injected faults) on a simulated clock; timed reaps and cancel on the network queue; running a child process and reading its own peak memory | IOCP as Windows' automatic choice (after a benchmark), batched io_uring submission, retransmission timeouts and one-way cuts in simulation, timed reaps and cancel on the file I/O queue, sockets on io_uring and IOCP, non-blocking connects and CI runners on the BSDs and illumos, OpenBSD and DragonFly, Unix sockets and UDP, per-device alignment, NUMA memory placement and locked memory, performance and efficiency cores, pinning and topology on the BSDs, file locks on Solaris (D78), a memory allocator of our own (D75), and the M1 wrap-up review's follow-ups: a guard against completion packets after an immediate IOCP success, `NotConnected` from a reset stream's address and shutdown in simulation, tests that cancel a pending connect and mix port and worker operations on IOCP, and no readiness call in a reap that is already satisfied |
| Storage and transactions (M2, M3) | the design (D77), the file format's spec ([format.md](format.md)), CRC-32C with the processor's instructions, the superblock in two header slots, page headers and their checksums, the page file: crash-safe commits, growth, a free list, rollback, one writer or many readers by file lock; the page cache: a fixed budget allocated once, clock eviction, pinned page handles, changes kept until commit, statistics; a benchmark harness with counts gated exactly and times gated as ratios (D80); the write-ahead log: segments inside the file, checksummed records, commits that log whole pages before writing them in place, group commit, the Full, Normal and Off durability modes, redo on open that tells a torn tail from damage (D81); the B+tree: get, put, splits up to the root, fence keys, prefix truncation, every page checked when read from the file, and a whole-tree check (D82), delete with merges and root collapse, and cursors for range, prefix and reverse scans (D83); checkpoints, recovery from the last checkpoint and log segments recycled with never-reused sequence numbers (D84); a crash sweep across the whole stack (D85, [testing.md](testing.md)); transactions: one writer, snapshot readers on any thread through before-images, atomic commit, rollback, hybrid-clock commit stamps (D86); the embedded Rust API, `eggdb::Database` ([embedded.md](embedded.md), D87); values up to 1 GiB on chains of overflow pages, and a check that accounts for every page of the file (D88); a model test against an in-memory map and a fuzzer for hostile files (D89); long commits that no longer grow the file, and batched random overwrites faster than SQLite's (D97) | the rest of the v0.2.0 list above, then MVCC, serializable isolation, partitioning and the LSM bake-off; streaming reads of large values; a checkpoint that restarts the log without header syncs, so `Normal` commits sync as rarely as SQLite's (D97) |
| Models (D2) | key-value; relational, first part: tables with primary keys (D98) | relational joins, aggregates and indexes, search, documents, time series, columnar, graphs, vectors, geospatial, wide-column, objects, queues |
| SQL dialects (D10, D47) | PostgreSQL, first parts: `CREATE`/`DROP TABLE`, `INSERT`, `SELECT` with `WHERE`, inner, left and cross joins, `GROUP BY` with aggregates, `HAVING`, `DISTINCT`, `ORDER BY` and `LIMIT`, `UPDATE`, `DELETE`, transaction blocks, integer, float, boolean and text types, through `eggdb sql` (D98, D100) | the rest of PostgreSQL, SQLite, MySQL/MariaDB, T-SQL, Oracle, then GoogleSQL, Snowflake, Spark, ClickHouse, Trino, Db2, Sybase |
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
