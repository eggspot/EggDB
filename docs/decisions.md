# Architecture decisions

Numbered, append-only. A later decision may supersede an earlier one; say so in both entries.
Status is **Decided** (owner), **Decided by research**, or **Decided by the build loop** (engineering).
The owner's decisions fix the goal. The concrete lists inside them (protocol order, benchmark names,
budgets, permission details) were filled in when the project was set up, and research may refine
them with a new entry.

## D1 Everything written by us, in Rust, from scratch — Decided (owner, 2026-10-23)
EggDB is our own code in Rust. No code from other databases: we implement storage, transactions,
query processing and every protocol from public specifications, papers and documentation (clean
room). What we ship has no third-party crates; Rust's standard library and our own declarations of
operating-system APIs are fine. Permissive crates (MIT, Apache-2.0, BSD) may be used for tests,
benchmarks and build tooling only, when a research subagent approves them. Licensed MIT; no
copyleft or source-available code ever: GPL, LGPL, AGPL, MPL, SSPL, BSL and the like.

Clean room includes not reading the source of copyleft or source-available databases (MySQL, MariaDB,
MongoDB, and any other under those licenses) while working on EggDB. Their public documentation,
protocol specifications and published papers are fine. Test suites under those licenses may be
run against EggDB from outside the repo, but are never copied into it. Product names such as
PostgreSQL, Oracle, SQL Server, MySQL, Redis, MongoDB and Elasticsearch are their owners'
trademarks; we say "compatible with", never imply endorsement.

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
reuses the most of the core (the roadmap follows the same order), and a research subagent may
refine it:
1. SQLite's embedded API and file format, and PostgreSQL's wire protocol and SQL;
2. Redis RESP, for caching;
3. Elasticsearch/OpenSearch REST, for search;
4. MongoDB's wire protocol;
5. the other SQL dialects (D10): MySQL's protocol and SQL, Microsoft SQL Server's TDS protocol
   (publicly documented as MS-TDS) and T-SQL, and Oracle's SQL and PL/SQL;
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

## D10 Every query language and concept, each SQL dialect as itself — Decided (owner, 2026-10-23)
EggDB runs each SQL dialect as itself:
- PostgreSQL;
- Microsoft SQL Server (T-SQL);
- MySQL and MariaDB;
- Oracle (SQL and PL/SQL);
- SQLite.

Each comes with its data types, built-in functions, procedural language, system catalogs and
behavior, so a query or stored procedure written for one of them runs on EggDB unchanged.

The other query languages people use sit beside SQL:
- Redis commands;
- the MongoDB query language;
- the Elasticsearch query DSL;
- CQL;
- graph queries (SQL/PGQ, GQL, Cypher);
- vector and full-text search;
- time-series queries.

A session chooses its dialect by wire protocol, connection setting or per statement. Every dialect
reads and writes the same data in the same transactions (D2). Dialects are front-ends over one
query core (one logical plan, one optimizer, one executor), never separate engines.

Proof of support is the original's own regression and conformance tests where they can lawfully
be run (PostgreSQL's regression suite, sqllogictest; MySQL's GPL test suite only from outside the
repo, D1), plus our own tests. Support is tracked per dialect as a coverage figure that grows every
release, with the parts that can't be matched recorded and explained, so no dialect blocks the
others while it is incomplete.

Oracle's own wire protocol (Oracle Net / TNS) has no lawful public specification. Until one exists,
Oracle SQL and PL/SQL are reached over our PostgreSQL and TDS protocols (and the embedded API), and
the Oracle protocol itself is not implemented.

## D11 AI-native, with a built-in MCP server — Decided (owner, 2026-10-23)
Any AI agent, framework or model can use EggDB as easily as a person can.

**The MCP server.** EggDB includes an MCP server (Model Context Protocol, implemented by us from
the public spec, over stdio for embedded use and over the network for the server). Its tools and
resources cover:
- schema and data discovery;
- queries in any dialect;
- safe writes;
- explain plans;
- sampling;
- backups and restores;
- administration.

**Readable by models.** Error messages, explain output and metadata are written to be read by
models as well as people: precise, structured and with the fix when there is one.

**Vectors and embeddings** are first-class (D2), so retrieval for AI needs no second database.

**Permissions.** Agents act only with what they are granted: per agent, per database and per
table, read-only by default and revocable. Destructive actions need confirmation, and every agent
action is logged where the owner can see it. It is vendor-neutral, with no dependence on one
model provider.
