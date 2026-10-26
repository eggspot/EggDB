# Architecture decisions

Numbered, append-only. A later decision may supersede an earlier one; say so in both entries.
Status is **Decided** (owner), **Decided by research**, or **Decided by the build loop** (engineering).
The owner's decisions fix the goal. The concrete lists inside them (protocol order, benchmark names,
budgets, permission details) were filled in when the project was set up, and research may refine
them with a new entry.

## D1 Everything written by us, in Rust, from scratch — Decided (owner, 2026-10-23); per-language driver layers excepted by D16
EggDB is our own code in Rust. The only code in other languages is listed in D16:
- the per-language driver and ORM layers;
- the minimal HTML and JavaScript that loads the browser build.

No code from other databases: we implement storage, transactions, query processing and every
protocol from public specifications, papers and documentation (clean room). What we ship has no third-party crates; Rust's standard library and our own declarations of
operating-system APIs are fine. Permissive crates (MIT, Apache-2.0, BSD) may be used for tests,
benchmarks and build tooling only, when a research subagent approves them. Licensed MIT OR Apache-2.0 (D36); no
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

## D4 Every scale, one engine — Decided (owner, 2026-10-23); the ways of scaling listed in D18
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

## D12 Works with every tool people already use — Decided (owner, 2026-10-23)
EggDB is compatible with the query languages (D10) and also with the tools built around other
databases. They work against EggDB without changes:
- **drivers and connectors:** JDBC, ODBC, ADO.NET, libpq, and each language's native clients;
- **ORMs:** EF Core, Hibernate, Prisma, SQLAlchemy, Django, ActiveRecord, GORM, TypeORM;
- **GUI and admin clients:** pgAdmin, DBeaver, DataGrip, SQL Server Management Studio, Azure
  Data Studio, MySQL Workbench, Redis Insight, MongoDB Compass, Kibana;
- **BI and monitoring tools:** Grafana, Metabase, Power BI, Tableau, Superset;
- **migration and schema tools:** Flyway, Liquibase, Alembic.

That means answering each tool's catalog and metadata queries (`information_schema`, `pg_catalog`,
`sys.*`, MySQL's `SHOW` commands, the Redis `INFO` and `CLIENT` commands, and so on) the way it
expects. The proof is each tool connecting, browsing and running its usual workflows in our
compatibility tests. Coverage is tracked per tool alongside the dialect coverage (D10).

## D13 Migrating from another database is restoring its backup — Decided (owner, 2026-10-23)
Moving to EggDB means restoring the backup or dump the old database already produces. Every format
is handled from public specifications and documentation only (D1), so each one is in one of two
groups.

**Restored directly**, because the format is publicly specified:
- plain SQL dumps from every dialect (D10), including `pg_dump`'s plain format, `mysqldump` and
  SQL Server's generated scripts;
- SQLite database files, opened as they are (the file format is published);
- MongoDB `mongodump` output (BSON, with its published specification);
- Redis AOF (RESP commands), and Redis RDB from its published format description.

**Through a documented route**, because there is no public specification of the format:
- SQL Server `.bak` (only its outer tape container is published);
- `pg_dump`'s custom, directory and tar formats;
- `pg_basebackup` archives;
- Elasticsearch and OpenSearch snapshots;
- Oracle Data Pump.

The documented route is the source's own export to a documented format (plain SQL, CSV, `bcp`, the
`_bulk` format), or the live replication feed below. These formats are never reverse-engineered, and
nobody reads the source database's code to learn them. A research subagent may move a format to
the first group only when it finds a lawful public specification, recorded here.

For a switch with no downtime, EggDB can also follow a running database through its documented
replication or change stream (PostgreSQL logical replication, the MySQL binlog, SQL Server change
data capture, MongoDB change streams, Redis replication), then take over. Each restored or
followed source is tested by restoring real backups made by the original and comparing every row,
index and object.

## D14 One second to download, one second to install — Decided (owner, 2026-10-23)
EggDB is one small, self-contained file with no runtime dependencies, so getting it is as fast as
downloading that file:
- a one-line install command, and the usual package managers (Homebrew, apt, dnf, winget,
  Chocolatey, Docker, crates.io), each shipping that same binary in its own package format;
- no installer wizard, no service to configure first, sensible defaults;
- the first start takes well under a second.

Every release measures and publishes:
- the download size;
- the time from starting the download to a running database on a typical connection and machine;
- the first-start time.

The numbers may only go down, like D6. The target is about one second to download and one second
to install; concrete budgets are set from measurements once the first release ships. Package
registries that need accounts or tokens wait for the owner (CLAUDE.md, "Releases").

## D15 The EggDB client: our own design by Claude Fable, plus familiar layouts — Decided (owner, 2026-10-23)
EggDB ships a client with excellent UI and UX for everyone who works with the data:
- a **GUI**, written in Rust, that runs as a desktop app on every OS (D8) and in the browser
  (WebAssembly, also served by the EggDB server);
- a **CLI** that is at least as good as psql, sqlcmd and mysql.

It covers the whole job:
- connections;
- schema and object browsing for every data model (D2);
- a query editor for every dialect (D10), with completion, formatting and error help;
- result grids that stay fast on huge results;
- editing data;
- explain plans shown visually;
- import, export, backup and restore (D13);
- users and permissions;
- monitoring;
- AI help through the built-in MCP server (D11).

**Our own design.** The client's main design is EggDB's own, created by Claude Fable: every visual
design task (layouts, flows, the design system, icons, motion, each screen) goes to an agent on the
`fable` model. The design is recorded in `docs/ux/` and then implemented.

**Familiar layouts.** Beside our own design, the client offers layouts that work like the clients
people already use:
- SQL Server Management Studio;
- pgAdmin;
- DBeaver;
- DataGrip;
- MySQL Workbench;
- Oracle SQL Developer;
- MongoDB Compass;
- Redis Insight.

Each one keeps that client's arrangement, shortcuts and workflows, so someone switching keeps their
habits.

They are clean-room (D1). We match layout, behavior and shortcuts from using the products and from
their public documentation. We never copy another product's code, artwork, icons, screenshots or
documentation images. We don't read the source of the ones that are source-available. A layout may
be labelled by the product it resembles ("works like SQL Server Management Studio"), with a note
that the names are their owners' trademarks and imply no endorsement. Each layout has a
specification in `docs/ux/` with the shortcuts and workflows it must match, and tests that check
them.

**Usable by everyone.** The client follows accessibility standards (keyboard-first, screen readers,
contrast) and works on small screens. Its speed is measured like the engine's (D7): opening a
database, running a query and scrolling a large result.

**The desktop GUI is a must-have, from the start.** It is not a later add-on to the CLI or the browser
client. One UI codebase in Rust draws the same screens both ways:
- natively on the desktop (Windows, macOS, Linux and the rest of D8), GPU-accelerated;
- in the browser through WebAssembly.

The GUI toolkit is our own (D1): rendering, text layout and shaping, input, accessibility and
platform windows. That is a large piece of work, so it is its own track. It starts now, alongside
the engine, and doesn't wait for the server. A research subagent orders its steps, but never drops
the desktop. EggOS also builds its own Rust GUI (its D16), so research checks whether the two can
share one toolkit, and records the answer here.

## D16 Official connectors for every language, working with existing ORMs — Decided (owner, 2026-10-23)
EggDB has official drivers for every widely used language. Each one fits that language's standard
database interface and its popular ORMs and frameworks:

| Language | Standard interface | ORMs and frameworks |
|---|---|---|
| C# / .NET | ADO.NET | EF Core, Dapper, NHibernate |
| Java, Kotlin | JDBC, R2DBC | Hibernate/JPA, jOOQ, Spring Data, Exposed |
| Node.js, TypeScript | | Prisma, TypeORM, Drizzle, Sequelize, Knex |
| Python | DB-API | SQLAlchemy, Django ORM, asyncio |
| Go | `database/sql` | GORM, sqlc |
| Rust | the native crate | |
| PHP | PDO | Laravel/Eloquent, Doctrine |
| Ruby | | ActiveRecord, Sequel |
| Swift, Dart/Flutter | | the mobile frameworks |
| C, C++ | a stable C API | |

More languages follow as people ask for them.

**How they work.** Every driver works both against a server and in embedded mode (D3), and exposes
every data model and dialect (D2, D10). One Rust client core does the protocol, pooling, types and
embedded engine. Each language's driver is a thin, idiomatic layer over it through the C API, so
behavior and speed are the same everywhere. A runtime that can't load native code (the browser,
for example) gets the same core compiled to WebAssembly.

**What is Rust and what isn't.** The database, the client core and everything performance-critical
are Rust (D1). The per-language layer is written in that language, because a framework can only
load code in its own language. An EF Core provider is a .NET assembly that implements EF Core's
interfaces in C#: its type mappings, its translation of LINQ into EggDB SQL and its migrations SQL
generator. A JDBC driver or Hibernate dialect is Java, and a SQLAlchemy dialect is Python. These
layers hold no protocol or storage logic. They call the Rust core through the C API:
- P/Invoke in .NET;
- the Foreign Function and Memory API in Java 22 and later, with a JNI binding for older Java;
- N-API in Node.js;
- the CPython C API in Python.

They keep crossings into the core few and batched, so the binding never becomes the bottleneck.
They are written by us, clean room (D1), and follow each language's conventions and the
no-comments rule (public API doc comments such as C# XML docs are kept).

**The exceptions, named.**
- A runtime that can load neither native code nor WebAssembly may get a protocol implementation
  written in its own language. D17 adds the second and last case: a pure-language server-mode
  path, when benchmarks show it is needed. These two are the only places protocol logic lives
  outside Rust, and both must pass the same protocol test suite as the Rust core.
- The browser build is loaded by the minimal HTML and JavaScript a browser requires, and nothing
  more.

**Frameworks are dependencies, never part of what we ship.** An ORM framework (EF Core, Hibernate,
SQLAlchemy and the others) is a dependency the user's application already has, declared in the
package metadata. We never copy, bundle or modify it. Our provider is our own MIT OR Apache-2.0 code that
implements the framework's public interfaces, whatever the framework's own license (Hibernate's
LGPL included).

**Packaging.** Each language package carries the native library for every platform it supports (D8):
runtime-specific assets in NuGet, classifier JARs in Maven, prebuilt binaries in npm, wheels on PyPI.
Installing the driver is all the user does.

**Existing drivers.** Once EggDB's PostgreSQL protocol lands (M6), the existing PostgreSQL drivers and
ORM providers (for example Npgsql with its EF Core provider) work against it, and later the other
compatible databases' drivers too (D12). The official drivers add what only EggDB has: embedded
mode, every model, every dialect and the MCP tools.

**Proof and publishing.** Each driver is proven by its ORM's own test suite or official provider
tests running against EggDB in CI. Publishing to the package registries (NuGet, Maven Central, npm,
PyPI, crates.io, Packagist, RubyGems, pub.dev and others) needs accounts and tokens, which wait for
the owner.

## D17 Driver speed: batched crossings and benchmarks in every language — Decided (owner, 2026-10-23); refines D16
Every official driver (D16) must be at least as fast as the best existing driver in its language,
and should beat it (D7). The rules below apply to every language, not one.

**Crossing between a language and the Rust core is batched, never per value.** A call from another
language into Rust costs roughly ten nanoseconds (.NET P/Invoke, Java's FFM API) to tens of
nanoseconds (Java's JNI, Node.js N-API, Go's cgo, which also ties up an OS thread). Creating values
in the host language (JavaScript, Python objects) often costs more than the call itself. So the C
API and every driver follow these rules:
1. **No per-value crossings.** Language bindings make one call per operation: open, prepare,
   execute, fetch the next batch, close. C and C++ users, who call the C API directly, get
   inline accessors that read values from the batch in their own memory, with no call per value.
2. **Results in batches, in shared memory.** The Rust core writes thousands of rows at a time
   into a buffer with a compact binary layout. The language reads values straight from that memory
   without copying, and releases the whole batch with one call. The tools are `Span<T>` and
   pointers in .NET, `MemorySegment` in Java 22+ (a direct `ByteBuffer` with JNI on older Java),
   an external `ArrayBuffer` with `DataView` in Node.js (one copy per batch where external buffers
   aren't allowed, as in Electron), the buffer protocol (`memoryview`) in Python, and unsafe slices
   in Go.
3. **Lazy values.** Text stays UTF-8 in the buffer. A host-language string or object is created
   only when the application reads that value, with the platform's fast decoder. Bulk APIs
   (`fetchall`, data frames, Arrow, streaming readers) take the whole batch at once.
4. **Parameters in one buffer.** All of a command's parameters are packed into one buffer and
   passed with the execute call.
5. **Prepared statements are handles.** SQL text crosses once. After that, only a small handle and
   the parameter buffer cross.
6. **Async without blocked threads, where the language has async.** Network waiting happens in the
   Rust core. Completion comes back through one callback per batch, which completes the language's
   own task or promise: `[UnmanagedCallersOnly]` in .NET, an upcall stub in Java, a thread-safe
   function in Node.js, an event-loop callback in Python. Languages without async (PHP, for
   example) and Go's cgo calls block one thread per call in progress, and the batching keeps
   those calls few. Tiny hot calls skip the runtime's thread-state switch where the runtime
   supports it (`SuppressGCTransition` in .NET, the FFM linker's critical option on recent Java).
7. **Measured, and gated** (below).

**Benchmarks per language, in both modes.** Each driver runs the same workload: many small queries,
large result sets, bulk inserts, prepared statements and concurrent connections.

- **Embedded mode, gated from the first driver (M5).** Compared against SQLite's binding in that
  language, on the same data:
  - `Microsoft.Data.Sqlite` (.NET), the SQLite JDBC driver (Java), `better-sqlite3` (Node.js);
  - Python's `sqlite3`, Go's SQLite drivers, `rusqlite` (Rust), SQLite's own C API (C, C++).
- **Server mode, gated once EggDB's PostgreSQL protocol lands (M6).** Compared against the best
  existing driver in that language, connected to the same EggDB server over the PostgreSQL
  protocol:
  - Npgsql (.NET), PgJDBC (Java), `pg` and `postgres` (Node.js), psycopg and asyncpg (Python);
  - pgx (Go), `tokio-postgres` (Rust), PDO pgsql (PHP), libpq (C, C++), the `pg` gem (Ruby);
  - PostgresNIO (Swift), `postgres` (Dart).

  The official driver uses whichever protocol it ships with. A research subagent decides at M6
  whether that is the PostgreSQL protocol or an EggDB-native one, and records it here.

**How the comparison is run.** Baseline drivers are installed in the benchmark environment only,
never vendored or shipped, whatever their license (D1). Each comparison is a ratio:
- our driver's throughput, and its p50 and p99 latency;
- against the baseline driver on the same runner in the same job.

A ratio worse than the baseline by more than D7's noise threshold is a performance regression. The
full suites run on main and on a schedule; pull requests run a short version (CI rules).

**If the Rust core is slower over the network in some language,** by that ratio, after the
batching rules above have been applied, a research subagent decides with benchmarks whether that
language gets a server-mode path in the language itself. Pure-language drivers are popular for
exactly this (pgx in Go, Npgsql in .NET), and some ecosystems prefer them: many Go programs are
built without cgo. The path is bounded:
- it covers the client side of server mode only: connection, protocol and results;
- it is our own clean-room code and passes the same protocol test suite as the Rust core;
- it is recorded here, and retired if the Rust core later matches it.

Embedded mode always uses the Rust core, since only the core can run the database inside the
application. For Go, that means embedded mode needs cgo, and cgo-free builds get server mode only.

## D18 Every way of scaling, not only a primary with replicas — Decided (owner, 2026-10-23); refines D4
EggDB supports every way people scale databases. A deployment picks the ones it needs, combines
them, and changes them later without exporting data or stopping:
- **Up:** one machine used fully. Every core, NUMA-aware memory, fast storage and large memory,
  with no global locks on hot paths.
- **Read replicas:** synchronous or asynchronous, cascading, delayed.
- **Multi-primary (active-active):** writes accepted in several places, with conflict handling
  chosen per table: last writer wins, application rules, or CRDT types.
- **Leaderless quorums:** Dynamo-style reads and writes with tunable consistency per request.
- **Sharding:** by hash, range, time or region, with online resharding and rebalancing.
- **Distributed SQL:** consensus-replicated ranges (Raft, Paxos), serializable distributed
  transactions, and global indexes.
- **Storage separated from compute:** stateless compute nodes over shared or object storage, which
  scale independently.
- **Serverless:** scale to zero and back on demand.
- **Tiered storage:** hot, warm and cold data across memory, local disk and object storage
  (S3-compatible), queried the same way.
- **Geo and edge:** data pinned to regions for latency and data-residency laws, and read copies at
  the edge.
- **Offline-first sync:** embedded EggDB on phones, browsers and devices, syncing with a server or
  cluster when connected (D3, D4).
- **Federation:** one query across separate EggDB clusters, and across other databases through the
  protocols EggDB speaks.
- **Partitioning within one node:** by range, list, hash or time, with partition pruning, distinct
  from sharding across nodes.
- **Logical replication and change data capture:** to other EggDB clusters (including across
  versions, for upgrades without downtime) and to other systems.
- **Standby and disaster recovery:** log shipping and asynchronous standbys in another region, with
  planned switchover and failover.
- **Analytical replicas (HTAP):** columnar copies kept in step with transactional data (M11), so
  analytics never slow down transactions.
- **Tenancy at scale:** many tenants per cluster, from shared tables to a database per tenant, moved
  and rebalanced online.

**Consistency per request.** Consistency has two axes, and both are chosen per database, table or
request:
- **Transaction isolation:** strict serializable, serializable, snapshot, repeatable read, read
  committed.
- **Replica freshness:** linearizable reads, read-your-writes, causal, bounded staleness, eventual.

The trade-off is explicit, and the defaults are the safe ends: serializable isolation and reads
that see the latest committed data.

**Proof.** Every way of scaling is proven like crash safety (D9): deterministic simulation of
partitions, clock skew and node failures, Jepsen-style consistency checks, and benchmarks at scale.
The roadmap brings them in order of use: partitioning with the storage engine, replicas,
standbys and high availability (M14), then sharding and distributed SQL (M15), then the rest
(M18, which builds on M15's consensus and M17's platforms for offline-first sync).

## D19 Every data type, with each database's exact behavior — Decided (owner, 2026-10-23); refines D10
EggDB supports every data type of every database it is compatible with (D5, D10). Each type keeps
the behavior it has in its home database: precision, range, overflow, rounding, string collation,
time-zone handling, comparison and sort order, and conversions. That includes:

- **PostgreSQL:**
  - the numeric, character, binary, date/time and interval types;
  - boolean, enums, arrays, ranges and multiranges, composite types and domains;
  - JSON and JSONB, XML, UUID, money;
  - network addresses, geometric types, bit strings, full-text `tsvector`/`tsquery`.
- **SQL Server:** `datetimeoffset`, `datetime2`, `smalldatetime`, `money`, `uniqueidentifier`,
  `hierarchyid`, `sql_variant`, `geography`/`geometry`, `rowversion`, `xml`, `nvarchar(max)` and the
  rest.
- **MySQL and MariaDB:** unsigned integers, `ENUM`, `SET`, `YEAR`, `BIT`, spatial types, `JSON`.
- **Oracle:** `NUMBER`, `BINARY_FLOAT`/`BINARY_DOUBLE`, `VARCHAR2`/`NVARCHAR2`, `DATE`,
  `TIMESTAMP WITH (LOCAL) TIME ZONE`, the `INTERVAL` types, `CLOB`/`BLOB`/`RAW`/`BFILE`,
  `ROWID`/`UROWID`, `JSON`, `XMLType`, object types, `VARRAY` and nested tables.
- **SQLite:** dynamic typing with type affinity.
- **Redis:** strings, lists, sets, sorted sets, hashes, streams, bitmaps, HyperLogLog, geo.
- **MongoDB:** BSON's types (ObjectId, Decimal128, dates, binary subtypes, regular expressions).
- **Elasticsearch and OpenSearch:** `text` and `keyword`, numeric, date and `date_nanos`, `boolean`,
  `ip`, `geo_point` and `geo_shape`, `nested` and `object`, `flattened`, `dense_vector`,
  `sparse_vector`, `completion`.
- **Cassandra:** scalars (`varint`, `decimal`, `timeuuid`, `duration`, `inet` and the rest), the
  collections, tuples, user-defined types and counters.
- **The other models of M16:** InfluxDB line-protocol fields and tags, Kafka records (key, value,
  headers, timestamp), S3 objects and their metadata.
- **Across models:** vectors (dense float32/float16/int8/binary, sparse), geospatial (OGC simple
  features, geography on the sphere), time series, graph nodes and edges, arbitrary-precision
  decimals, and large objects.
- **User-defined types:** user-defined types, domains and casts.

**One type system underneath.** It stores, indexes and compares all of these. Each dialect's type
is a precise mapping onto it, not an approximation, so a value written through one protocol reads
back exactly through another where both can represent it. Where they can't, the conversion is
explicit and documented, never silently lossy.

**Proof and progress.** Each type is proven by the original database's own type tests, where they can
lawfully be run (D1, D10), and by our round-trip and comparison tests across protocols. A type is
done when those pass. The roadmap's support matrix tracks the done types per dialect and model, as a
coverage figure alongside D10's.

## D20 Our own file format: versioned, checksummed, ready for scale and upgrades — Decided by research (gap review, 2026-10-23); refines D4 and D5
**The format.** EggDB stores data in its own format, never in another database's. SQLite files are a
compatibility format: EggDB opens, attaches, imports and exports them (D5, D13), but its native
storage is its own, because SQLite's layout can't hold MVCC, columnar or vector data.

**Properties.** Every page, log record and file:
- is checksummed;
- carries a format version;
- is readable by every later release (backward compatible), and by the previous release where a
  rolling upgrade needs it.

**Upgrades and schema changes.** Upgrades run online:
- one node at a time in a cluster, with mixed versions working together during the upgrade;
- a tested downgrade path to the previous minor version.

Schema changes and reindexing run online too, without blocking reads or writes.

**Built for distribution from the start.** The core is designed for distribution from M2 and M3,
not retrofitted at M14:
- hybrid logical clock timestamps on every commit;
- a write-ahead log that can become a consensus-replicated log;
- tenant and shard keys in the key layout, costing zero bytes when unused, so a small embedded
  database stays smaller than SQLite (D4, D6).

That way, scaling (D18) never forces a format break.

## D21 Project governance, security policy and stability promise — Decided by research (gap review, 2026-10-23)
**The project files.** The repo gets:
- CONTRIBUTING.md, with the clean-room rules (D1) and the Developer Certificate of Origin (sign-off
  on every commit, no CLA);
- CODE_OF_CONDUCT.md (the Contributor Covenant);
- SECURITY.md: private reports through GitHub's private vulnerability reporting, a response
  target, coordinated disclosure, and CVEs for confirmed issues;
- a short governance note: the owner decides, and the decision log is public.

**A stability promise, in semantic versioning, for:**
- the C API and the language drivers;
- the wire protocols and SQL;
- the file format (D20);
- configuration.

Deprecations are announced one minor release ahead and removed only in a major release. After
1.0, there are release channels (stable, beta, nightly) and an LTS line.

**Telemetry is off.** EggDB sends nothing anywhere unless the user turns it on.

**Supply chain.** Releases are signed, with a software bill of materials, build provenance (SLSA)
and reproducible builds.

**Owner items.** These wait for the owner, and the build loop lists them in its report:
- any bug bounty (money); the name needs no policy (D35);
- who answers security reports, and the response time SECURITY.md promises.

Security-critical code merges like everything else (D35).

## D22 Security model: authentication, authorization, row and column security — Decided by research (gap review, 2026-10-23)
**Authentication:**
- password (SCRAM-SHA-256), TLS client certificates;
- LDAP, Kerberos/GSSAPI, OpenID Connect/JWT, SAML;
- API keys, and multi-factor for interactive tools.

**Authorization.** One role-based and attribute-based model underneath. Every protocol's own
permission system maps onto it:
- PostgreSQL and SQL Server roles and grants;
- MySQL privileges;
- Redis ACLs;
- MongoDB roles;
- Elasticsearch roles.

So a user's rights are the same whichever protocol they use.

**Finer control:**
- row-level security;
- column permissions;
- dynamic data masking.

D11's agent permissions sit on this same model.

**Defaults.** Secure by default: no open network listener without authentication, TLS on, least
privilege. A server without this can't hold real data, so authentication, TLS and roles ship with the
server (M6).

## D23 Cryptography, TLS and key management — Decided by research (gap review, 2026-10-23); the library question answered by the owner in D35
**What we need:**
- TLS 1.3 (1.2 for older clients);
- encryption at rest: per page, per column, and with per-tenant keys;
- key management with rotation, through external KMS, HSM and Vault integration;
- a mode that uses FIPS-approved algorithms only;
- a path to post-quantum key exchange.

**How it's built, under D1.** All cryptography is our own code, written in constant time, checked
against standard test vectors (Wycheproof and the NIST vectors), and fuzzed.

**Owner decision (D35, D36).** All cryptography is our own; no library exception, no paid external
audit and no formal FIPS 140 validation. The code is reviewed in the open, by the stricter
automated checks of D35 and by anyone who reads it.

## D24 Backup, point-in-time recovery and verified restores — Decided by research (gap review, 2026-10-23); complements D13
EggDB's own backups:
- continuous log archiving to local disk or object storage (S3-compatible);
- point-in-time recovery to any moment;
- instant snapshots and copy-on-write clones;
- incremental and parallel backups, encrypted with D23's keys;
- `eggdb dump` and `eggdb restore` for logical exports in EggDB's format and in the plain-SQL dumps of
  every dialect (D13 is the import side).

Every backup kind is proven by automated restore tests that restore and compare. A backup is only
counted when it has been restored.

## D25 Observability and operations — Decided by research (gap review, 2026-10-23)
**What to see:**
- metrics, logs and traces through OpenTelemetry, plus a Prometheus endpoint;
- a slow-query log;
- statement statistics (a `pg_stat_statements` equivalent, in every dialect's own views);
- wait events, live activity and locks;
- `EXPLAIN ANALYZE` in every dialect.

**What to control:**
- resource governance: memory, CPU and I/O limits per user, role, tenant and query;
- quotas, admission control and query timeouts;
- an index and plan advisor.

**How to run it:**
- an admin API (and MCP tools, D11);
- a Kubernetes operator, written in Rust, with its Helm chart and manifests (YAML configuration,
  not code);
- a Terraform provider in Rust, speaking Terraform's published plugin protocol;
- the GUI's monitoring screens (D15).

If a Rust Terraform provider proves impractical, writing it in Go is an exception to D1, and that
is the owner's decision.

## D26 Audit, compliance and data governance — Decided by research (gap review, 2026-10-23)
**Audit.** A tamper-evident audit log of logins, permission changes, schema changes and chosen data
access.

**Data governance:**
- classification and tagging of personal data;
- retention policies;
- verified erasure for GDPR and similar laws, reaching history, time travel and backups (D24, D28)
  through crypto-shredding: data under per-subject keys that are destroyed on request;
- data residency through geo pinning (D18).

**Compliance.** Built-in reports give the evidence SOC 2, HIPAA and PCI DSS audits ask for. Being
certified is the deploying organization's job, and EggDB provides the controls and evidence. The
FIPS mode is covered in D23.

## D27 Programmability: procedures, functions, triggers, extensions, jobs, events — Decided by research (gap review, 2026-10-23)
**Each dialect's own procedural language runs as itself (D10):**
- PL/pgSQL;
- T-SQL;
- MySQL stored programs;
- PL/SQL;
- Redis Lua and Functions;
- MongoDB server-side expressions.

**User-defined functions in any language** run sandboxed in WebAssembly inside the engine: one safe,
fast route for C, Rust, Go, C#, JavaScript, Python and more.
- **The runtime is ours.** It is our own Rust code (D1): an interpreter first, a compiler for speed
  later.
- **Compilers stay outside.** The compilers and language runtimes that users compile their
  functions with stay outside EggDB's binary.

**Extensions.** An extension API for new types, index methods, functions and protocols. It is the
role PostGIS or pgvector play for PostgreSQL. Extensions are loaded as sandboxed WebAssembly or as
our own optional built-in modules, and cost nothing when unused (D6).

**Also:**
- triggers and event triggers;
- scheduled jobs (`pg_cron` and SQL Server Agent equivalents);
- notifications (`LISTEN`/`NOTIFY` and its equivalents).

## D28 Time travel, branching, derived data and change streams out — Decided by research (gap review, 2026-10-23)
**History and branching:**
- system-versioned temporal tables (SQL:2011) and `AS OF` queries over history;
- copy-on-write branches and clones of a whole database, for development, testing and safe
  migrations.

**Derived data:**
- incrementally maintained materialized views;
- continuous queries.

**Change streams out** (D18 covers change streams in):
- a change feed in a Debezium-compatible format to Kafka and the messaging protocols of D33;
- webhooks;
- the transactional outbox pattern, built in.

## D29 Data interchange, lakehouse tables and external data — Decided by research (gap review, 2026-10-23)
**Bulk paths:** fast bulk load and export (`COPY` and every dialect's equivalent).

**Formats:**
- CSV;
- NDJSON;
- Parquet, ORC and Avro;
- Apache Arrow, with Arrow Flight SQL and ADBC for zero-copy results.

**Lakehouse:** read and write Apache Iceberg, Delta Lake and Hudi tables.

**External data** (the role of foreign data wrappers), queried in place and joined with local data:
- tables over files;
- object storage;
- HTTP;
- other databases.

Every format comes from its public specification (D1).

## D30 Text, locales and internationalization — Decided by research (gap review, 2026-10-23)
**Text handling:**
- Unicode collation (the Unicode Collation Algorithm and CLDR-based locale tailorings);
- normalization;
- every legacy encoding the supported databases accept;
- the IANA time zone database, updated with each release.

**Full-text search in every major language:**
- stemmers;
- word segmentation for Chinese, Japanese, Korean and Thai;
- synonyms, stop words, fuzzy and phonetic matching.

**Built ourselves (D1).** We build these from the published Unicode, CLDR and IANA data, our own
code reading the public data files, without ICU's code. Their correctness is checked against each
dialect's collation behavior (D19).

**Data licences.** Only data under permissive terms is used (the Unicode license, IANA's public
domain, or our own). Word lists and dictionaries for segmentation and stemming must be permissive
too. Where none exist for a language, we build our own, never using share-alike or restricted ones.
"Every legacy encoding" means the encodings the supported databases themselves accept, listed per
dialect.

## D31 AI and machine learning in the database — Decided by research (gap review, 2026-10-23); extends D11
Beyond vectors and MCP:
- embedding generation and model inference from SQL (ONNX models, and local or remote language
  models through any provider);
- hybrid search that combines vector, full-text and filters, with reranking;
- semantic caching;
- agent memory as a built-in model;
- in-database training for common models;
- a feature store;
- text-to-SQL in every dialect;
- automatic tuning (indexes, configuration).

It stays vendor-neutral (D11): no dependence on one model provider. Remote calls are off by default
and permission-checked (D22).

## D32 Relational completeness and multi-tenancy — Decided by research (gap review, 2026-10-23)
**Relational features.** The full relational feature set of the supported dialects:
- every index type (B-tree, hash, GIN, GiST, SP-GiST, BRIN, bitmap, partial, expression,
  covering);
- declarative partitioning (D18);
- two-phase commit and XA;
- advisory locks, sequences and identity, generated columns;
- cursors, savepoints, window functions, recursive queries;
- the SQL:2023 features, tracked as a conformance list.

**Multi-tenancy:**
- isolation by row, schema or database;
- per-tenant quotas (D25) and keys (D23);
- moving tenants online (D18).

**For developers:**
- a documentation site, generated as static pages by our own Rust tool;
- a browser playground (WebAssembly, D8, with D16's minimal loader);
- migration tooling;
- public conformance and benchmark dashboards for every dialect, protocol, type and driver.

## D33 Messaging and queues: RabbitMQ, MQTT, NATS, SQS and Kafka — Decided (owner, 2026-10-23); protocol list and order by research; refines D2 and D5
D2 promised queues and streams, and D5 named Kafka. EggDB also speaks the messaging protocols
applications use. Each is implemented from its public specification or documentation (D1):
- **AMQP 0-9-1**, RabbitMQ's protocol: exchanges (direct, topic, fanout, headers), bindings, routing
  keys, acknowledgements, prefetch and priorities. Also RabbitMQ's documented extensions to it:
  dead-letter exchanges, message and queue TTL, delayed messages, publisher confirms, and
  replicated (quorum-style) queues;
- **AMQP 1.0**, the OASIS standard, which also serves JMS clients and Azure Service Bus-style use;
- **MQTT 3.1.1 and 5**, for IoT devices: QoS 0, 1 and 2, retained messages, last will, and MQTT 5's
  shared subscriptions, message expiry and reason codes;
- **STOMP**;
- the **NATS** protocol, with JetStream-style persistence;
- the **Amazon SQS** API (standard and FIFO queues);
- the **Kafka** protocol (D5): topics, partitions, consumer groups, compaction, transactions;
- **Redis** lists, pub/sub and streams (D2).

**Why in a database.** Queues live in the same storage and transactions as the data (D2). An
application can update rows and publish or consume messages in one transaction: the outbox and
inbox patterns built in, with exactly-once processing within EggDB. The same scaling (D18), security
(D22) and observability (D25) cover messaging.

**Order.** The protocols come in order of use:
1. Kafka;
2. AMQP 0-9-1 (RabbitMQ);
3. MQTT;
4. the SQS API;
5. NATS;
6. AMQP 1.0;
7. STOMP.

Research may refine the order.

**Proof.** Each protocol is proven by its clients' own test suites and conformance tools where they
can lawfully be run, and benchmarked against RabbitMQ, Mosquitto/EMQX, NATS and Kafka (D7).

## D34 Engineering guardrails: build profiles, performance targets, legal checks — Decided by research (gap review, 2026-10-23); refines D6 and D7
**Build profiles.** "All in one" and "lightweight" are reconciled by compile-time build profiles:
- `embedded`: the core and the embedded API;
- `server`: adds protocols, security and operations;
- `full`: adds every model, protocol, the GUI and MCP.

Each profile is still one binary (D3). D6's and D14's budgets are measured per profile:
- the headline numbers (about one second to download and to install, the SQLite comparison) are
  for `embedded` and `server`;
- `full` is measured and kept as small as possible.

Unused features cost nothing at runtime in any profile.

**Shared foundations, all our own Rust (D1):** compression codecs, HTTP/1.1 and HTTP/2, gRPC, QUIC,
and the TLS of D23. Each is built once in the core and used by every protocol that needs it.

**Performance targets per workload.** D7 is applied per workload, with a target (the best existing
database for that workload) and a measured gap. The CI gate is the committed baseline: a PR that
makes any workload slower than its baseline is blocked (D7's rule). A workload that hasn't reached its target yet doesn't block unrelated work; closing the
gap is roadmap work, tracked on the benchmark dashboard (D32).

**Legal checks.** Protocol compatibility with products under non-permissive or restrictive terms gets
a research check of the law and public case history before each protocol milestone, recorded here.
That includes Redis's recent versions, MongoDB's SSPL, Elasticsearch's licenses, RabbitMQ's MPL,
EMQX's BSL, the AWS terms behind the SQS API, and Microsoft's Analysis Services languages (MDX,
DAX, XMLA, D38). Anything unclear goes to the owner.

**CI.** EggDB is a public repository, so the global CI rule's build-time aim applies: parallel jobs
are fine (CLAUDE.md, "Commands").

## D35 Owner answers: our own cryptography, security-critical code merges itself, the name is open — Decided (owner, 2026-10-23)
The owner answered three questions from the gap review:

1. **All cryptography is written by us** (D1, D23): TLS, the ciphers and hashes, key exchange,
   signatures and encryption at rest. There is no exception for an outside library.
2. **Security-critical changes merge themselves**, like every other change: cryptography,
   authentication and authorization, and the file format. No human approval is needed. Because
   nothing stops them, the automated checks are stricter for these changes:
   - **Two independent reviews instead of one:** the usual independent review, plus a second
     review focused only on security (side channels and constant-time behavior, memory safety,
     input handling, protocol downgrade, key handling).
   - **Test vectors, fuzzing and differential tests** in CI for every change: the standard vectors
     (Wycheproof, NIST), a fuzz run, and comparison against a reference implementation used in the
     test environment only (D1).
   - **A format or compatibility test:** a file-format change passes the upgrade, downgrade and
     mixed-version tests of D20 before merging.

3. **The name is as open as the code.** There is no trademark policy and no registration. Anyone may
   use the name EggDB however they want, including forks and products built on it. We still say
   "compatible with" for other products' names (D1).

Still open for the owner, since it costs money: any bug bounty (D21). The external audit and FIPS
validation were answered in D36.

## D36 License MIT OR Apache-2.0; no paid external audit — Decided (owner, 2026-10-23); refines D1, D23 and D35
EggDB is licensed under either the MIT License or the Apache License 2.0, at the user's option
(`LICENSE-MIT`, `LICENSE-APACHE`). This is the same as Rust and most of its ecosystem. It is free
forever under both: anyone may use, change, sell and ship it. MIT keeps it simple, and Apache-2.0
adds an express patent grant. Contributions are accepted under the same terms, with the DCO
sign-off (D21). The owner applied the same change to the other eggspot open-source projects.

There is no paid external security audit and no formal FIPS 140 validation. The project's
cryptography and security code is reviewed in the open, and by the stricter automated checks of
D35. A finding from anyone is handled through SECURITY.md.

## D37 Auto-APIs, realtime and the app backend — Decided by research (second gap review, 2026-10-23)
Most new applications reach their database through generated APIs, not drivers. EggDB generates
them from the schema, with no code to write:
- **REST**, in the style of PostgREST;
- **GraphQL**, in the style of Hasura;
- **gRPC** and **OData**, the last for Excel, Power BI and SAP;
- **realtime:** WebSocket and Server-Sent Events subscriptions on live queries, for changes to rows
  and query results;
- **a client sync API** for apps, built on D18's offline-first sync.

**End-user authentication for applications**, distinct from database roles (D22): sign-up,
password, magic link, social login and OpenID Connect. Users get JWT sessions that map onto
row-level security, so an app can talk to EggDB directly and safely.

**Files** stored in EggDB's object storage (D2) can be transformed on request (resize, crop,
convert) through sandboxed WebAssembly extensions (D27). The image codecs are our own code (D1). Every API is permission-checked by D22 and
rate-limited by D25.

## D38 The remaining data models and their query languages — Decided by research (second gap review, 2026-10-23); refines D2 and D10
These close "every kind of database":
- **Ledger tables:** append-only, tamper-evident history with Merkle proofs that a row was or wasn't
  changed. It is not a blockchain and has no consensus among strangers.
- **RDF** triples and quads, with **SPARQL** 1.1.
- **XML documents**, with **XPath** and **XQuery** (D19 has the type).
- **OLAP cubes** and semantic models, with **MDX**, **DAX** and **XMLA**, since Excel and Power BI
  speak them.
- **Spreadsheet-style tables:** formula columns, and a grid view in the client (D15).
- **Key-value with secondary indexes:** this comes from the relational core, and is stated here so
  key-value users find it.

Each is a front-end over the one core (D2, D10). Each is proven by its standard's own test suites
where they exist and can lawfully be run (the W3C SPARQL and XQuery suites, for example). Otherwise
it is proven by our own tests built from the public documentation and its examples. That is the
case for DAX, which has no public suite.

**MDX, DAX and XMLA** are implemented from Microsoft's public documentation, which D1 allows. Before
their milestone they get D34's legal check and D43's patent check, with anything unclear going to
the owner.

## D39 Observability data: metrics, logs and traces as a store — Decided by research (second gap review, 2026-10-23)
D25 covers EggDB's own telemetry. EggDB is also a store for everyone else's, so Grafana (D12) can
use it in place of Prometheus, Loki and Tempo.

**Ingest:**
- the OpenTelemetry protocol (OTLP) for metrics, logs and traces;
- Prometheus remote write;
- Loki's push API;
- syslog;
- Jaeger and Zipkin traces.

**Query:** PromQL, LogQL, TraceQL and SQL, all over the same data. EggDB also serves the Prometheus,
Loki and Tempo query HTTP APIs that Grafana uses. Loki and Tempo are AGPL, so their APIs and
behavior come from their public documentation only, never their source (D1). Retention, downsampling and tiered
storage (D18) keep it cheap.

## D40 Engine internals, stated — Decided by research (second gap review, 2026-10-23); refines D7
**Planning:** a cost-based optimizer with statistics, histograms and correlation, and adaptive
re-planning during execution.

**Execution:** vectorized, with SIMD chosen at runtime for the machine. A query compiler is added
later where it pays.

**I/O:** asynchronous, through io_uring and IOCP in the platform layer (M1). There is an epoll path
where io_uring is unavailable, and kqueue for network I/O with a thread pool for files on the BSDs
and macOS, since kqueue has no asynchronous file I/O.

**Approximate answers:** `TABLESAMPLE`, sketches (HyperLogLog, t-digest) and `APPROX_*` functions.

**Indexes and storage:** adaptive and learned indexes where benchmarks justify them, and
compression chosen per column and page.

**Accelerators:** GPUs, persistent memory and CXL memory are optional, behind build features, and
never required (D6, D34).

## D41 Embedded and edge, in detail — Decided by research (second gap review, 2026-10-23); refines D3, D4 and D6
**Small devices:**
- a published memory floor for `no_std` builds;
- a fixed-memory mode that never allocates beyond a set budget.

**Flash and power:**
- write-amplification and flash-wear budgets;
- power-loss safety on raw flash;
- battery-aware background work: few wake-ups, deferred maintenance.

**Phones and browsers:**
- correct behavior under iOS background limits and Android Doze;
- browser persistence through the Origin Private File System.

**Better than SQLite, defined and published:**
- smaller and faster on SQLite's own workloads (its `speedtest1` and the standard embedded
  benchmarks), measured on the `embedded` profile (D34);
- a published branch-coverage target for the core, rising every release toward SQLite's;
- a promise that today's files stay readable for decades (D20).

## D42 Beyond the client: admin console, notebooks and onboarding — Decided by research (second gap review, 2026-10-23); extends D15 and D32
Alongside the client (D15), designed by Claude Fable like every visual surface:
- an **admin web console** served by the server;
- **SQL notebooks** with charts;
- a **visual query builder** and **ER diagrams**;
- **import wizards**;
- **data profiling and data-quality rules**.

The GUI and documentation are **localized**, including right-to-left scripts. Onboarding includes:
- a migration guide per source database (D13);
- sample datasets;
- step-by-step tutorials.

## D43 Legal, export and privacy — Decided by research (second gap review, 2026-10-23); refines D34
- **Cryptography export:** the cryptography is our own (D35), so before the first release research
  records what export rules call for, for publicly available encryption source (for example the US
  rules for publicly available source code). Any filing or notification is the owner's to make.
- **Patents:** the clean room (D1) covers copyright, not patents. Users who choose the Apache-2.0
  option (D36) get its patent grant from our contributors; the MIT option has none. Research checks known patents before building patent-heavy features,
  and records the result here.
- **Benchmark clauses:** some commercial databases' licenses (Oracle, SQL Server and others)
  restrict publishing benchmark results. Before D32's dashboards name such a product, research checks
  the clause and records what may be published.
- **Privacy:** the project's own sites and services publish a privacy statement. Telemetry stays
  off (D21).

## D44 Public proof, community, cost and energy — Decided by research (second gap review, 2026-10-23); extends D7, D21 and D32
**Public proof:**
- a reproducible benchmark repository that anyone can rerun: scripts, data and pinned hardware
  descriptions;
- a compatibility farm that runs every supported tool, driver and ORM version (D12, D16) weekly
  and publishes the matrix;
- chaos testing for clusters (D18).

**Cost and energy:**
- a metering API (usage per tenant, database and query) with hooks for anyone's billing;
- energy in the lightweight budgets (D6): joules per query and idle wake-ups. These are reported
  first, and gated once a repeatable measurement method is fixed and recorded here.

**Community:** on GitHub Discussions, with a release blog and public roadmap voting. Anything that
needs an account elsewhere is the owner's call.

## D45 Fixes to weak spots — Decided by research (second gap review, 2026-10-23)
**Split M6.** The roadmap splits M6 into M6a to M6f, in order:
- **M6a:** the server and the PostgreSQL protocol. Authentication, TLS and roles ship in this first
  piece, because the server never listens without authentication (D22).
- **M6b:** the rest of security: more authentication methods, and row and column security.
- **M6c:** operations.
- **M6d:** MCP.
- **M6e:** the first GUI screens.
- **M6f:** the network drivers and their ORMs.

"M6" elsewhere means all six.

**Order of our own foundations.** Being our own (D1) has no fallback, so the order goes by need:
1. the file format (D20);
2. TLS and cryptography (D23), in step with the server;
3. the WebAssembly runtime (D27), before functions and extensions;
4. the GUI toolkit (D15), its own track;
5. the operator and the Terraform provider (D25), after the server is stable.

**Compatibility thresholds.** Each protocol and dialect publishes the pass rate of the original's
test suites, which must not fall between releases. Its first release needs a stated minimum pass
rate, recorded here.

**Prompt injection (D11).** Data returned to agents through MCP is marked as data, never
instructions. Agent-readable errors never echo raw stored text as if it were guidance. Tools that
write or delete stay behind D11's permissions and confirmation.

**Limits for functions and extensions (D27).** Every WebAssembly function and extension runs with CPU,
memory, time and output limits, set per role.

**SECURITY.md** gains:
- the scope: denial of service from hostile input, sandbox escapes, protocol attacks;
- the release signing key, once releases exist;
- the supported-versions policy after 1.0.

**Not planned without the owner.** These are business decisions or against the goals:
- a managed cloud service, billing and paid certification (D44's metering hooks are ready if the
  owner wants them);
- a bug bounty;
- accounts on proprietary community platforms;
- reverse-engineered formats or protocols (D10, D13);
- blockchain consensus (D38's ledger is tamper-evident tables only);
- any hard dependency on a GPU or vendor SDK (D40).

## D46 Nothing missing: we build every missing piece ourselves, without asking — Decided (owner, 2026-10-24); strengthens D2
The owner: *"we are all in one, everything missing we will build our own, don't need to ask me. It's
our important point."*

**The standing rule.** When a capability people need from a data platform is missing from EggDB, the
build loop and every session add it to the roadmap and build it ourselves (D1). They don't ask the
owner first. Research decides how and in what order, and records it here. The things that still
go to the owner are:
- anything that needs money, accounts or a license change (D21);
- legal and patent findings that research can't settle, and any legal filing (D34, D38, D43);
- the items on D45's "not planned without the owner" list.

**Added now under this rule:**

**Business intelligence**, our own complete application, not only connectors to others' (D12).
It is designed by Claude Fable (D15) and runs in the client and the admin console (D42). It
provides:
- drag-and-drop dashboards and charts;
- interactive reports with filters and drill-down;
- a semantic layer of metrics and dimensions defined once and reused everywhere, which also serves
  D38's OLAP cubes;
- scheduled and emailed reports (sent through the operator's own mail server), sharing, embedding
  in other applications, and alerts;
- row-level security inherited from D22.

It competes with Power BI, Tableau, Looker, Metabase and Superset.

**ETL and ELT pipelines**, our own tool:
- connectors to read from and write to other databases, files, object storage, APIs and the
  messaging protocols (D29, D33);
- transformations in SQL and in any language through WebAssembly (D27), with models, tests and
  lineage in the style of dbt;
- orchestration: schedules, dependencies, retries, backfills, monitoring (D25);
- change data capture in and out (D18, D28).

It competes with Airflow, dbt, Fivetran and Airbyte.

**Big-data ecosystem compatibility:**
- a Spark connector, which is a Java driver layer under D16: a thin JVM data source over the Rust
  core, with no database logic of its own;
- an Apache Iceberg REST catalog, so Spark, Trino, Flink, DuckDB and Snowflake can use EggDB's
  lakehouse tables (D29);
- federated SQL over many sources, in the style of Trino and Presto (D29);
- Kafka Connect compatibility (D33): EggDB implements the Connect REST and worker protocol in Rust,
  so existing Connect deployments and tools work with it. Our connectors are Rust.

**Our own connector for every ecosystem** (owner, 2026-10-24): *"when it comes to any kind of
existing connector or things we need to support, we build our own support too, so users can use them
as they want."* Wherever an existing tool has its own plugin or connector system, we write and
maintain an EggDB connector for it, so people keep using the tools they have:
- **BI:** Power BI and Tableau connectors, Excel, Looker, Superset, Metabase and Grafana data
  source plugins;
- **pipelines:** a dbt adapter, Airbyte and Fivetran sources and destinations, a Debezium connector,
  Airflow operators;
- **big data:** Spark, Flink, Trino and Presto connectors, an Iceberg REST catalog;
- **infrastructure:** a Terraform provider (D25), Kubernetes and Helm (D25);
- **the drivers:** JDBC, ODBC, ADO.NET and the other drivers (D16, D17);
- **anything else people ask for.**

Each connector is our own code. Where the host tool requires another language, the connector is a
thin layer in that language over the Rust core, under D16's rules, holding no database logic. Each
one is tested against the host tool's own connector test suite or certification kit where one
exists and can be run, and is tracked in the compatibility farm (D44). Publishing or certifying them
in marketplaces and registries (Microsoft's and Tableau's connector programs, the Grafana plugin
catalog, package registries) needs accounts, so that waits for the owner (D16, D46). The connectors
work without it.

**Order.** They follow their foundations:
- BI after the console and analytics (M11, D42);
- ETL after the messaging, interchange and change-stream work (D28, D29, D33);
- the big-data pieces after the lakehouse tables (D29).

## D47 More SQL dialects and analytics protocols — Decided by research (third gap review, 2026-10-24); extends D10 under D46
More dialects, each as itself (D10):

**Cloud warehouses**
- GoogleSQL (BigQuery, Spanner);
- Snowflake SQL, through Snowflake's documented SQL REST API (its driver wire protocol isn't
  documented);
- Databricks/Spark SQL, through the HiveServer2 Thrift protocol and the Statement Execution REST
  API.

**Other analytics engines**
- ClickHouse SQL, through its HTTP and native protocols;
- Trino/Presto SQL, through the Trino client REST protocol.

**IBM and SAP**
- IBM Db2 SQL and SQL PL, through DRDA (an Open Group standard), which also reaches Informix and
  Db2 for z/OS;
- Sybase/SAP ASE: its SQL, reached through the PostgreSQL protocol and through TDS 7+ (MS-TDS).
  TDS 5.0 itself is implemented only if a lawful public specification is found, as D10 handles
  Oracle's protocol.

**Reached through the PostgreSQL protocol**
- Redshift, Vertica and Greenplum;
- Teradata SQL, whose own wire protocol isn't public (D1, D10).

**SAP HANA.** SAP publishes the HANA SQL Command Network Protocol reference, so HANA's own protocol is
implemented from it, after D34's legal check.

**Vendor drivers.** Teradata's and Vertica's own vendor drivers won't connect over the PostgreSQL
protocol. Users of those reach EggDB through PostgreSQL drivers, or through EggDB's own drivers
(D16).

The order follows use, decided by research. Each dialect has its pass rate tracked (D45).

## D48 Other NoSQL, key-value and graph APIs — Decided by research (third gap review, 2026-10-24); extends D5 under D46
**Cloud NoSQL APIs**
- the DynamoDB JSON API with PartiQL;
- Firestore, Bigtable and HBase, and Spanner's API, from their public protocol definitions;
- Cosmos DB's NoSQL, Gremlin and Table APIs, in the documented gateway mode.

**Key-value**
- the memcached protocol;
- an ordered, transactional key-value API in the style of FoundationDB, LevelDB and RocksDB;
- an IndexedDB-compatible API for the browser build.

**Replication and graph**
- CouchDB replication;
- Couchbase SQL++, from its documentation only, since Couchbase is under BSL (D1);
- Neo4j's Bolt protocol, alongside Cypher (D10);
- Apache TinkerPop Gremlin.

FoundationDB's own wire protocol and Apple's CloudKit aren't implemented: CloudKit is a hosted Apple
service with no public server protocol. Their data comes in through documented exports (D13). Core
Data's stores are SQLite files, which EggDB already opens (D5).

## D49 Microsoft, Splunk and legacy sources — Decided by research (third gap review, 2026-10-24); under D46
**Query languages and ingest**
- KQL, with the Kusto REST API, for Azure Data Explorer and Sentinel-style queries;
- Splunk's SPL, with its HTTP Event Collector ingest.

**Microsoft business data**
- Dataverse's Web API and SharePoint lists, as sources and targets;
- Excel files, read and written as tables (`.xlsx` from the OOXML standard, `.xls` from the
  published MS-XLS specification).

**Legacy and mainframe**
- mainframe data loaders: COBOL copybooks, EBCDIC, packed decimal;
- dBase, ISAM and Btrieve readers where public specifications exist.

**Documented routes only** (D1, D13): Access `.accdb`/`.mdb`, FileMaker and IMS go through the vendor's
own export, ODBC or JDBC.

## D50 Finance and industry standards — Decided by research (third gap review, 2026-10-24); under D46
**Financial tick data** (the kdb+ use case)
- ASOF and window joins;
- bitemporal tables;
- order-book primitives;
- double-entry ledger primitives;
- microsecond tail-latency targets in D7's benchmarks.

**Industry formats for ingest and export**
- FIX and ISO 20022 (finance);
- FHIR and HL7 v2 (healthcare);
- EDIFACT (trade);
- DICOM (medical imaging).

All of these are built from their freely available standards. ASC X12 needs a paid license to
use its specifications, so X12 support waits for the owner (D46). Formal certifications (SWIFT
membership, FHIR conformance, DICOM conformance statements) wait for the owner too.

## D51 Data catalog, lineage, master data and quality — Decided by research (third gap review, 2026-10-24); extends D42 under D46
**Catalog and lineage**
- a data catalog with a business glossary and stewardship;
- lineage through OpenLineage;
- the Apache Atlas and OpenMetadata APIs, so tools built on them connect. Microsoft Purview reaches
  EggDB through its own scanners and connectors (D46), not through an Atlas server.

**Contracts**
- a schema registry and data contracts.

**Master data management**
- entity resolution;
- match and merge;
- golden records.

**Data quality**
- rules at scale, built on D42's profiling.

## D52 Data sharing, clean rooms and privacy technology — Decided by research (third gap review, 2026-10-24); under D46
**Sharing**
- a Delta Sharing server and client;
- zero-copy sharing between organizations, through D28's branches, with entitlements;
- marketplace listings, with no payments (payments are the owner's, D45).

**Clean rooms**
- query-output policies;
- private set intersection.

**Privacy technology**
- differential privacy and k-anonymity checks;
- tokenization and format-preserving encryption (NIST FF1);
- blind-index search on encrypted columns;
- synthetic data, and masked copies for non-production use.

**Not required, not committed**
- confidential computing: optional attestation only, never required (D45);
- homomorphic encryption and multi-party computation: a research track only.

## D53 Retention, legal hold, WORM and sovereignty — Decided by research (third gap review, 2026-10-24); refines D26 under D46
**Legal hold**
- a legal hold overrides D26's crypto-shredding: erasure requests for held data wait until the
  hold is lifted, and are recorded.

**Retention and records**
- immutable (WORM) retention and backups that ransomware can't alter;
- records-management schedules;
- eDiscovery export in the EDRM style.

**Sovereignty**
- residency policies per row;
- customer-held keys kept in-country;
- limits on operator access;
- logs of cross-border transfers.

**WORM retention and erasure.** Data under a WORM retention period can't be altered, even by
erasure. A GDPR erasure request on it is recorded and carried out when the retention period ends,
the same as under a legal hold. The retention rules say which law takes precedence; that is the
deploying organization's legal call.

**Compliance claims** (SEC 17a-4, FINRA, HIPAA, national sovereignty laws) need counsel and paid
assessments, so they stay with the owner. EggDB provides the controls (D26).

## D54 Coordination, configuration, secrets and directory — Decided by research (third gap review, 2026-10-24); under D46
**Coordination**
- distributed locks and leases with fencing tokens;
- leader election;
- watches and service discovery;
- the etcd v3 and ZooKeeper protocols.

**Configuration and secrets**
- a configuration store with OpenFeature feature flags;
- a secrets store with dynamic, short-lived credentials, in the style of Vault. Vault and Consul are
  under BUSL, so their APIs come from their documentation only (D1).

**Directory**
- an LDAP v3 server and SCIM;
- the published Active Directory protocol specifications (MS-ADTS).

**App sign-in**
- WebAuthn and passkeys for D37's end-user authentication.

## D55 Real-time decisioning: streams, events and graph algorithms — Decided by research (third gap review, 2026-10-24); under D46
**Streaming SQL in the style of Flink**
- event time and watermarks;
- tumbling, hopping and session windows;
- stream-stream joins and late data;
- `MATCH_RECOGNIZE` for complex event processing.

**Graph algorithms** for fraud detection and recommendations
- PageRank;
- community detection;
- paths, similarity and the rest.

**Online feature serving**
- low latency, for D31's feature store.

## D56 Content and collaboration — Decided by research (third gap review, 2026-10-24); under D46
**Text extraction for search**
- from OOXML, ODF, PDF and PST files;
- OCR through ONNX models (D31).

**Email, calendar and file stores**
- email stores with IMAP, JMAP and SMTP;
- CalDAV, CardDAV and WebDAV.

**Assets and search**
- digital asset management: EXIF and XMP metadata, renditions;
- enterprise search that respects each user's permissions (D22).

Media codecs are our own (D1), and only royalty-free ones.

## D57 Science, geo and IoT — Decided by research (third gap review, 2026-10-24); extends D19 under D46
**Arrays**
- SQL/MDA (ISO 9075-15);
- Zarr, HDF5 and NetCDF.

**GIS at PostGIS parity**, from PostGIS's and pgRouting's documentation only, since both are GPL (D1)
- raster, topology and routing;
- GeoTIFF and Cloud-Optimized GeoTIFF;
- the OGC APIs;
- vector tiles;
- H3 and S2 indexes.

Map and coordinate datasets are used only under compatible licenses (D30's rule).

**Genomics**
- VCF, BAM and CRAM;
- htsget.

**IoT and digital twins**
- DTDL and W3C Web of Things models;
- OPC UA, Modbus and Sparkplug ingest.

## D58 Hyperscale operations — Decided by research (third gap review, 2026-10-24); extends D25 and D44 under D46
**Fleet control plane** for thousands of clusters. This is software operators run themselves, not
a hosted service (D45)
- inventory;
- upgrades in canary waves;
- placement by hardware class.

**Cost and tenancy**
- chargeback and showback, built on D44's metering;
- tenant lifecycle APIs;
- databases per user at millions of tenants.

**Analytics and migration**
- MPP distributed analytics with workload queues;
- resumable petabyte-scale migrations with verified differences.

**Owner items from this review** (D46):
- **Legal checks:** before building, D34's and D43's legal and patent checks cover API
  compatibility with every outside vendor's service named in D47–D57, with unclear results going
  to the owner. That means Google, Amazon, Snowflake, Databricks, Microsoft (Cosmos DB, Dataverse,
  Kusto, the MS-XLS, MS-ADTS and PST formats, DTDL), Splunk, SAP and IBM.
- **Licenses:** ASC X12's paid license (D50).
- **Accounts and money:**
  - paid cloud accounts and hardware for compatibility testing (free official emulators run from
    outside the repo);
  - push-notification credentials;
  - marketplace listings;
  - country-level legal opinions.

## D59 The platform layer's first shape: files, clocks and a crash simulator — Decided by the build loop (2026-10-24); refines D8 and D9
**One crate, std only.** `crates/eggdb-platform` holds every operating-system call the engine makes.
The engine sees traits, never an OS API:
- `FileSystem` (open, remove, rename, create directories, sync a directory, exists) and `File`
  (positional `read_at`/`write_at`, `size`, `set_size`, `sync_data`, `sync_all`). Positional I/O
  needs no cursor, so one handle is shared between threads. Unix uses `pread`/`pwrite` through
  std, Windows uses `seek_read`/`seek_write`, and any other target falls back to seek and read
  under a lock.
- `Clock`: a monotonic clock for timeouts and a wall clock in Unix nanoseconds for hybrid logical
  clocks (D20), which may jump in either direction.
- Its own `Error` with an `ErrorKind`, so the engine never matches on `std::io::ErrorKind` and a
  later `no_std` build (D8) can keep the same interface.

**Durability follows POSIX's weakest promise.** File contents are durable only after
`sync_data`/`sync_all`; creating, renaming and removing a file only after the parent directory is
synced. Engine code is written against that rule on every OS. Windows flushes the directory
through a handle opened with backup semantics; targets with no way to flush a directory (WASI)
only check that it exists. A rename across directories is durable only once both directories
are synced, and the simulator lets either half survive alone, which is stricter than journaling
file systems.

**A sync failure is fatal to the file.** After a failed `fsync` some kernels drop the dirty pages
and report success on the next try (PostgreSQL's 2018 "fsyncgate"). So the engine never retries a
failed sync: it stops using the file and recovers from the last durable state. The simulator
therefore does not need to model retries after a failed sync.

**Deterministic simulation from day one (D9).** `SimFileSystem` keeps what the program sees and
what would survive a power loss. `crash(DropUnsynced)` keeps only the durable state;
`crash(Torn { seed })` lets each 512-byte piece of each unsynced write, and each unsynced size
change, survive or not, which models torn and reordered writes. Every operation is numbered, and
any of them can fail on purpose, by number or at a seeded random rate. A failed operation changes
nothing, so a test can walk every crash and failure point of a protocol reproducibly. `SimClock`
moves only when a test moves it, and its wall clock can jump back. Files above 1 GiB are refused
with an error rather than exhausting memory, so fuzz targets can drive the simulator safely.

**Still to come in M1:** direct I/O that bypasses the page cache (`O_DIRECT`, `F_NOCACHE`,
`FILE_FLAG_NO_BUFFERING`) with aligned buffers; asynchronous I/O as D40 sets it (io_uring and IOCP, an epoll path, a file thread pool where kqueue has no file I/O) behind the
same traits; memory (page size, aligned and huge-page allocation); threads and CPU topology; and
networking. Each needs our own OS API declarations (D1), and each gets the same simulated twin.

## D60 Direct I/O: an explicit uncached open that fails instead of falling back — Decided by research (build loop, 2026-10-24); refines D59
The engine caches pages itself (D40), so the platform layer can open a file that bypasses the
operating system's cache: `FileSystem::open_uncached`, used with `AlignedBuffer`.

**How each OS does it:**
- Linux and Android: `O_DIRECT` through std's `custom_flags`. Its value differs by architecture
  (from the kernel's uapi `fcntl.h` headers): `0o40000` on x86, x86_64, riscv64, s390x and
  loongarch64; `0o200000` on arm and aarch64; `0o400000` on powerpc and powerpc64; `0x8000` on
  mips. Any other
  architecture answers `Unsupported` rather than risk a wrong flag, so the build still works
  everywhere (D8).
- macOS and iOS: `fcntl(F_NOCACHE = 48, 1)` right after open, through our own variadic
  declaration of `fcntl`; on Apple arm64 a fixed-argument declaration of a variadic function is
  undefined behaviour.
- Windows: `FILE_FLAG_NO_BUFFERING` (`0x20000000`) through std's `custom_flags`.
- Everything else: `Unsupported`.

**It fails instead of silently falling back.** When the platform or file system refuses (for
example `EINVAL` from a file system without `O_DIRECT`), `open_uncached` returns
`ErrorKind::Unsupported`. Only a refusal counts: a raw `EINVAL` from the open on Linux, or a
failed `F_NOCACHE` on Apple; other errors keep their kind. The operating system creates the file
before refusing, so a refused `CreateNew` removes the file it just made, so that falling back to
`open(CreateNew)` works, and a refused `Create` may leave an empty file; the simulator does the
same. That matches the public documentation of databases that rely on direct
I/O: TigerBeetle refuses to start without it outside a development flag; ScyllaDB checks at
startup; RocksDB's `use_direct_reads` makes the open fail. Falling back is the engine's choice,
made later as a setting (`auto`, `required`, `off`), so the speed trade-off stays visible (D7)
and the platform layer stays honest (D9). There is no "really bypasses the cache" query, because
nobody can know: older ZFS accepts `O_DIRECT` and still caches, NFS treats it as a hint, and
macOS's `F_NOCACHE` is a hint. A handle reports only what was asked for (`File::is_uncached`).

**Alignment is a fixed 4096 bytes for now**, for buffer addresses, offsets and lengths. That is the
largest logical block size common devices need (Linux requires the device's logical block size,
usually 512 or 4096; Windows requires the volume's sector size). Asking the device (`statx` with
`STATX_DIOALIGN` on Linux 6.1+, `FileStorageInfo` on Windows) to raise the alignment where needed
is an M1 follow-up. `AlignedBuffer` gives zeroed memory, so padding never leaks old bytes.

**The simulator enforces the rules.** Uncached simulated handles reject unaligned buffers, offsets
and lengths with `InvalidInput`, so a mistake that would be `EINVAL` on Linux or Windows shows up on
every OS, including macOS, where misaligned I/O quietly goes through the cache.
`SimFileSystem::refuse_uncached` makes the open fail, to test the engine's fallback.

**Rules for the engine:**
- Direct I/O is not durable by itself: the drive's write cache and the file's size and extents
  still need `sync_data` (D59's rule is unchanged).
- Don't mix cached and uncached handles, or memory maps, on the same file.
- Never change a buffer while I/O on it is in flight.
- Preallocate files instead of growing them write by write.
- Read and write whole pages. The default `read_exact_at` loop is only safe at page boundaries:
  after a short read inside the file, the next offset is unaligned and fails.

## D61 Memory: OS page sizes, explicit and transparent huge pages, and a simulated twin — Decided by research (build loop, 2026-10-24); refines D59 and D60
The engine's buffer pool and arenas take memory from the platform layer's `Memory` trait, never
from an OS call: `OsMemory` for the machine, `SimMemory` for tests. A "page" here always means the
operating system's page, never a database page.

**The page size is asked for, never assumed.** It is 4 KiB on most x86 machines, 16 KiB on Apple
arm64 and newer Android devices, and 64 KiB on ppc64le and some aarch64 distributions. Unix uses
`getpagesize()` (exported by glibc, musl, bionic, Apple's libc and the BSDs; `sysconf`'s
`_SC_PAGESIZE` has a different value on each of them), Windows uses `GetSystemInfo`, WebAssembly
uses its 64 KiB page. The huge page size comes from `/proc/meminfo` on Linux and
`GetLargePageMinimum` on Windows; other systems report none.

**A region is a whole number of pages, and the caller says so.** `allocate(len, mode)` returns a
zeroed, page-aligned `MemoryRegion` of exactly `len` bytes, and a `len` that is zero or not a
multiple of the mode's page size fails with `InvalidInput`, as `AlignedBuffer` does. Rounding up
silently would turn 2 MiB + 1 byte into 4 MiB of huge pages without anyone seeing it.

**Three page modes (`PageMode`):**
- `Normal`: anonymous private mappings (`mmap` on Linux, Android, Apple's systems and the BSDs;
  `VirtualAlloc` on Windows). Other systems use the heap with page alignment.
- `Huge`: pages the administrator reserved. Linux `MAP_HUGETLB` (`0x40000`, `0x80000` on mips),
  Windows `MEM_LARGE_PAGES` after switching on `SeLockMemoryPrivilege` once with
  `AdjustTokenPrivileges` (holding the privilege is not enough). They are never swapped out.
- `TransparentHuge`: normal pages with `madvise(MADV_HUGEPAGE)` on Linux, over a region that starts
  on a huge-page boundary (the mapping is made one huge page larger and trimmed). Many
  distributions run transparent huge pages in `madvise` mode and reserve no explicit huge pages,
  so without this hint a buffer pool would get none at all.

**Huge pages are given or refused, never faked (as D60 for direct I/O).** "Not possible here"
(no huge page size, a kernel without them, a missing privilege: `EINVAL`, `ENOSYS`,
`ERROR_PRIVILEGE_NOT_HELD`, `ERROR_NOT_ALL_ASSIGNED`) is `Unsupported`; "configured but exhausted"
(`ENOMEM`, `ERROR_NO_SYSTEM_RESOURCES`, `ERROR_COMMITMENT_LIMIT`) is `OutOfMemory`. Linux reports
an empty huge page pool, its default, as `ENOMEM`, so that is `OutOfMemory` too. Both keep the
operating system's error. Falling back is the engine's choice, made later as a setting like
PostgreSQL's `huge_pages = try | on | off` and Oracle's `USE_LARGE_PAGES`, which also reports what
it actually got (PostgreSQL 17's `huge_pages_status`). Where the system turns transparent huge
pages on for everything (RHEL's default), even `Normal` regions may land on huge pages.

**ABI details that are easy to get wrong:**
- `mmap`'s offset is `off_t`, whose width varies: the platform layer calls `mmap64` with a 64-bit
  offset on glibc and bionic, and `mmap` with a 64-bit offset on musl, Apple's systems and the
  BSDs, where `off_t` is always 64 bits.
- `MAP_ANONYMOUS` is `0x20` on Linux, `0x800` on mips and `0x1000` on Apple's systems and the
  BSDs. illumos, Haiku and AIX use other values, so the mapping path is limited to an allowlist of
  systems, and the rest use the heap.
- The architecture lists include mips32r6 and mips64r6; D60's `O_DIRECT` list missed them and now
  has them.

**The simulator (D9).** `SimMemory` follows the same rules over heap allocations:
- normal and huge page sizes that tests set, so code that assumes 4 KiB pages fails on every
  machine;
- a byte budget and a separate huge page pool, each failing with `OutOfMemory`;
- huge and transparent huge pages that can each be switched off (`Unsupported`);
- numbered allocations that fail on purpose by number or at a seeded rate (`InjectedFault`);
- live and peak byte counters, which also catch regions a test leaks.

**Follow-ups, each when the engine needs it:** locking pages in memory (`mlock`,
`VirtualLock`, `RLIMIT_MEMLOCK`); NUMA placement (`mbind`, `VirtualAllocExNuma`); reserving
address space and committing it later; returning memory (`MADV_DONTNEED`, `MADV_FREE`,
`MEM_RESET`); keeping data pages out of core dumps (`MADV_DONTDUMP`); huge pages other than the
default size (`MAP_HUGE_1GB`); sizing the buffer pool from cgroup v2's `memory.max`; one
operation counter shared by every simulator component; taking the transparent huge page size
from `/sys/kernel/mm/transparent_hugepage/hpage_pmd_size`, which can differ from the explicit
default (`default_hugepagesz=1G`) or exist without it.

## D62 CPUs: the available set, pinning given or refused, and padding against false sharing — Decided by research (build loop, 2026-10-24); refines D59
The engine's thread-per-core shards (D40) learn about processors from the platform layer's `Cpus`
trait: `OsCpus` for the machine, `SimCpus` for tests.

**What the first piece holds:**
- `CpuSet`, a set of `CpuId`s written in Linux's list format (`0-3,8-11`), parsed with errors as
  values (D9) and covered by a seeded random-input test. A set holds CPUs below 65,536, more than
  any OS supports. Windows numbers a CPU as its processor group times 64 plus its number in the
  group.
- `available()`: the CPUs the process may run on, read once when `OsCpus` is created, before any
  thread is pinned. On Linux and Android it is `sched_getaffinity`, retried with a larger mask
  while the kernel answers `EINVAL`, so it includes the cgroup cpuset. On Windows it is every
  active processor in every group (Windows 11 and Server 2022 run a process across all groups).
  Elsewhere it is the first `parallelism()` CPUs.
- `parallelism()`: how many threads can run at once, from `std::thread::available_parallelism`,
  which on Linux also respects cgroup CPU quotas. The engine sizes its shards from this, not from
  the core count, as ScyllaDB's Seastar does with `--smp`.
- `pin_current_thread(&CpuSet)`: `sched_setaffinity` on Linux and Android, `SetThreadGroupAffinity`
  on Windows (`SetThreadAffinityMask` reaches only the primary group). An empty set, one outside
  `available()` (the kernel would silently narrow it to the cpuset), or one spanning several
  Windows groups is `InvalidInput`.
- `CachePadded<T>` and `CACHE_LINE_PADDING`: 128 bytes on x86_64, aarch64, arm64ec and powerpc64 (adjacent
  lines are fetched in pairs on x86; Apple's aarch64 lines are 128 bytes), 256 on s390x, 32 on
  arm and mips, 64 elsewhere. It is a constant because a layout cannot depend on a value read at
  run time.

**Pinning is given or refused, never faked (as D60 and D61).** Apple's `THREAD_AFFINITY_POLICY` is
only a hint and is not supported on Apple silicon, so macOS and iOS answer `Unsupported`, as do the
BSDs and WebAssembly for now. Pinning stays optional for the engine: Seastar turns it off when a
machine is overprovisioned, and so will a later setting.

**The simulator (D9).** `SimCpus` reports an available set and a parallelism the test chooses
(standing in for a cpuset or a container quota), records each thread's pins instead of applying
them, refuses pinning on demand, and fails numbered pin requests by number or at a seeded rate. It
still runs real threads. A deterministic single-threaded scheduler, in the style of FoundationDB's
simulation and TigerBeetle's VOPR, needs every unit of concurrency to be a task the simulator
drives, so it comes with asynchronous I/O and the per-core event loops, not here.

**Follow-ups, in order:** reading the topology (cores, packages, NUMA nodes and the measured cache
line, from sysfs, `GetLogicalProcessorInformationEx` and `sysctlbyname`, with everything unknown as
`None`) and spawning threads with a name, a stack size and a pin; then thread priorities and
quality of service; performance and efficiency cores; pinning on FreeBSD (`cpuset_setaffinity`);
Windows CPU Sets and sets across groups; the process affinity mask on Windows; interrupt affinity;
and a real fuzz harness for every parser, decided once for the whole project.

## D63 Starting threads: pinned before the work runs, a refused pin is a spawn error — Decided by the build loop (2026-10-24); refines D62
Engine threads start through `ThreadOptions` (name, stack size, CPUs) and its `spawn`, never
`std::thread` directly, so every shard thread is named for debuggers and profilers and pinned the
same way on every OS. The new thread pins itself through `Cpus::pin_current_thread` and reports
the outcome to `spawn` before it runs any of the caller's code. When the pin is refused, the work
never runs, the thread ends, and `spawn` returns the pin's error: a shard never runs on CPUs it
did not ask for without knowing it, in the spirit of D60–D62's "given or refused, never faked".
With `SimCpus` the pin is recorded, so tests can check where each thread was meant to run. A
thread name with a NUL byte, on which std would panic, is `InvalidInput` (D9).

## D64 First platform review: syncs need a writable handle, the simulator stays bounded and fast — Decided by the build loop (2026-10-24); refines D59, D61 and D62
The build loop's first review of M1 (D59–D63) fixed what it found and settled these rules:
- **Syncing needs a writable handle, on every OS.** Windows' `FlushFileBuffers` refuses a
  handle without write access, while Linux and macOS accept one. So `sync_data` and `sync_all`
  fail with `PermissionDenied` through an `OpenMode::Read` handle everywhere, and the simulator
  does the same, so recovery code that syncs a file it opened read-only fails on every machine,
  not only on Windows.
- **Writes, size changes and syncs through a read-only handle are `PermissionDenied`** on the
  operating system as in the simulator; before, the operating system's `EBADF` arrived as
  `Other`.
- **An empty write changes nothing, at any offset.** `OsFile` returns 0 without calling the
  operating system (Windows leaves a zero-length write's effect to the file system), and the
  simulator no longer rejects an empty write beyond its largest file size.
- **The simulator's memory is bounded by unsynced changes too.** Each file's unsynced writes
  and size changes, kept for a later torn crash, may hold at most
  `SimFileSystem::MAX_UNSYNCED_BYTES` (1 GiB), counting each change's bookkeeping as well as its
  bytes so that tiny writes are bounded too, and a write past it fails with `InvalidInput` until
  the file is synced, as D59's 1 GiB file limit does. Before, overwriting one page without
  syncing grew memory without limit, which a fuzz target could hit. `limit_unsynced_bytes`
  lowers it for tests.
- **The simulator's cost follows the work, not the data it holds.** A sync replays only the
  changes since the last one (a full copy only when they add up to more than the file), and a
  file is freed by keeping counts of its visible and durable names instead of rebuilding a set
  of every name on each close, removal, rename and directory sync. On one probe in release
  builds, 4,000 appends of 4 KiB with a sync after each went from 509 ms to 5.7 ms, and 20,000
  open-and-drop cycles among 20,000 files from 15.0 s to 17 ms.
- **Paths are compared without `.` components**, so `./dir/f` and `dir/f` are one file in the
  simulator as on the operating system. Renaming a file onto itself, or onto another name of
  the same file, changes nothing, as POSIX says; renaming onto the root is `InvalidInput`.
- **A rename across directories with only the target synced survives under both names**, which
  then share one file; with only the source synced it survives under neither. This is the
  "either half survives alone" of D59, now pinned by a test. It is the simulator's deliberately
  pessimistic choice, not what a journaling file system does (none turns a rename into a hard
  link), so engine code must not depend on either outcome.
- **A refused uncached `CreateNew` keeps its own error.** Removing the file it may have created
  is best effort: if the operating system refused before creating it, the removal's `NotFound`
  no longer replaces `Unsupported`. Between the refusal and the removal another process could
  create the same name and lose it; the engine opens its files in its own data directory, so
  that race needs a second writer there, which the single-process rule (D3) already excludes.
- `OsCpus` never reports a parallelism above the number of available CPUs; Windows enables the
  lock-memory privilege once per process, as D61 says; and the other Unix systems (illumos,
  Haiku, AIX, Hurd, Solaris) ask `getpagesize()` instead of assuming 4 KiB; targets built on
  newlib (ESP-IDF, Horizon, Vita) may not export it and keep 4 KiB.

**Still open, as follow-ups:** the error kinds for a path that runs through a regular file
(`create_dir_all("file/sub")`, `exists("file/x")`, `sync_dir` on a file) differ between the
operating system and the simulator and need one decided kind each; `sync_dir` still scans every
name; Windows reports active processors as the low bits of each group, which the topology work
replaces with the group masks.

## D65 CPU topology: our own machine-wide numbers, the whole machine, read once when asked — Decided by research (build loop, 2026-10-24); refines D62
`Cpus::topology` tells the engine's thread-per-core shards (D40) where each online CPU sits, so
they can take one shard per physical core before using hardware siblings, keep a shard's memory
on its own NUMA node, and avoid sharing work across the split last-level caches of AMD's chiplet
designs. It returns a `CpuTopology`:
- a `CpuLocation` per online CPU: its `CoreId`, `PackageId`, `NumaNodeId` and last-level cache
  `CacheId`, each `None` when the operating system does not say;
- the counts of physical cores, logical CPUs and packages, and the cache line size it reports;
- helpers the engine needs first: `core_cpus`, `numa_node_cpus`, `numa_nodes` and
  `one_cpu_per_core`.

**Decided by research:**
- **Identifiers are our own, dense and machine-wide**, numbered in the order of each group's
  lowest CPU, as hwloc's logical indexes are: Linux numbers a core only within its package and
  marks an unknown package `-1`, and Windows has no core numbers at all. NUMA nodes keep the
  operating system's number, since memory placement (`mbind`, `VirtualAllocExNuma`) takes it.
- **It describes the whole machine, not only `available()`**, so the engine sees when a sibling
  hardware thread belongs to another cpuset; the engine intersects the two.
- **It is read on first use and cached** (shared by clones of `OsCpus`), so an embedded program
  that never shards never pays for walking sysfs at startup (D6, D14).
- **What each OS gives:**
  - Linux and Android: sysfs under `/sys/devices/system`: `cpu/online`, then
    `topology/core_cpus_list` (or the older `thread_siblings_list`) and `package_cpus_list` (or
    `core_siblings_list`); `node/online` and `node/nodeN/cpulist`; and `cache/indexN/` with
    `level`, `type`, `shared_cpu_list` and `coherency_line_size`.
    - CPUs are grouped by these lists, never by `core_id` and `physical_package_id`.
    - No `node` directory (a kernel without NUMA, common on Android) means NUMA is unknown,
      not node 0.
    - Lists are cut down to online CPUs before CPUs are grouped, so lists that differ only in
      offline CPUs name one group.
    - The reader takes a file-reading function, so its tests run on every OS against a fake
      tree.
  - Windows: one `GetLogicalProcessorInformationEx(RelationAll)` call, retried while it answers
    `ERROR_INSUFFICIENT_BUFFER` (122): one consistent snapshot, with NUMA nodes in every
    processor group since Windows Server 2022.
    - The records are parsed as bytes, with explicit offsets for both the 64-bit and the 32-bit
      `GROUP_AFFINITY` layouts. A group count of 0 (before Windows 10 20H2) means one mask.
      Records of the newer NUMA kind (`RelationNumaNodeEx`, 6) are read as NUMA records.
    - A 32-bit process on a machine with more than 32 processors in a group gets an unknown
      topology, since its masks fold processors onto each other.
  - macOS and iOS: `sysctlbyname` for `hw.physicalcpu`, `hw.logicalcpu`, `hw.packages` and
    `hw.cachelinesize`. There is no per-CPU mapping there, and pinning is refused anyway (D62).
    The top-level `hw.l2cachesize` describes the efficiency cluster on Apple silicon, so it is
    not used.
  - Everything else: unknown.
- **Contradictions are dropped, never guessed.**
  - A CPU listed in two groups (cores, packages, caches or NUMA nodes) makes that kind unknown.
  - NUMA nodes that leave an online CPU out are unknown, on Linux and Windows alike.
  - A core whose CPUs disagree about their package, node or cache keeps only the cores.
  - A cache line that is not a power of two is unknown.
  - A malformed or truncated record or file makes the field, or the whole topology, unknown.
  - Seeded random-input tests cover both parsers.
- **The simulator starts unknown.** `SimCpus` answers `CpuTopology::unknown()` until a test
  sets one, so engine code meets the `None` answers it must handle.
  - `CpuTopology::symmetric` builds test machines with siblings numbered `Adjacent` (Windows,
    most arm64) or `Spread` (Linux on most x86), so code that assumes one numbering fails.
  - A topology that places CPUs must place every available one.
- **The measured cache line is information, not layout.** `CACHE_LINE_PADDING` stays a
  compile-time constant (D62); a contract test checks that the measured line is never longer.

**Follow-ups:** performance and efficiency cores (Windows `EfficiencyClass`, Linux
`cpu_capacity`, Apple's `hw.perflevelN`); dies and clusters; NUMA distances and memory-only
nodes; FreeBSD's `kern.sched.topology_spec`; the BSDs and WebAssembly; re-reading after a CPU is
hot-plugged; using the processor-group masks for Windows' `available()` (D64); dropping only the
contradicting kind instead of every group kind; and `CACHE_LINE_PADDING` on 32-bit arm, which is
32 bytes although Cortex-A cores running AArch32 report 64-byte lines.

## D66 Asynchronous file I/O: a completion queue, owned buffers, a worker pool first — Decided by research (build loop, 2026-10-24); refines D40 and D59
The engine's per-core event loops (D40) do file I/O through the platform layer's `IoQueue`,
created by `FileSystem::io_queue(IoQueueOptions)`: `OsIoQueue` for the machine, `SimIoQueue` for
tests.

**Decided by research:**
- **A completion queue, not futures.** The caller submits an `IoOp` (read, write, `sync_data`,
  `sync_all`) with a `u64` tag and later reaps `IoCompletion`s, in any order. That is the shape of
  io_uring's submission and completion rings and of IOCP, it needs no executor (std has none),
  and it is the only shape a single-threaded simulator can make deterministic. A future-based
  layer can sit on top later if the engine wants one.
- **An operation owns what the operating system uses.** The file is an `Arc` of the handle and
  the buffer an owned `IoBuffer` (heap memory, or an `AlignedBuffer` for uncached handles), both
  handed back in the completion, so nothing can be freed or changed while the kernel or a worker
  still uses it. Registered files and buffers (io_uring's fixed files and buffers, slices of a
  buffer pool's `MemoryRegion`) are an optimisation behind the same interface, with their own
  decision on aliasing.
- **Short transfers are reported, not hidden**, as `File::read_at` and `write_at` do; the caller
  resubmits the rest.
- **Every queue is bounded (D9).** `capacity` (default 128, at most 32,768, Linux's largest
  io_uring) counts operations submitted and not yet reaped, so a completion ring sized to it never
  overflows. A submit beyond it fails at once with the new `ErrorKind::QueueFull` and hands the
  operation back in a `RejectedOp`. Waiting for more completions than are in flight is
  `InvalidInput` instead of a wait that never ends.
- **Workers are small and never lose a completion.** Each pool thread has a 64 KiB stack, and an
  operation that panics on one (release builds abort instead) completes with `Other`, so a
  `reap` never waits for a completion that cannot come.
- **Dropping a queue waits for every operation in flight**, since the operating system may still
  be reading or writing their buffers (Microsoft documents that an `OVERLAPPED` is in use until
  its I/O completes, even after `CancelIoEx`).
- **The first operating-system backend is a pool of worker threads** (`eggdb-io-<n>`, 4 by
  default, never more than the capacity, at most 256) running the existing positional `File`
  calls. It works on every OS, it stays the file path on macOS and the BSDs (kqueue has no
  asynchronous file I/O), and it is the fallback when io_uring is refused at run time. With 0
  threads, as on WebAssembly, operations run on the caller's thread inside `reap`. On one probe
  (release build, macOS, 4 KiB random reads from the page cache, queue depth 32), a direct
  `read_at` took 545 ns, the inline queue 501 ns, and the pool 787 ns with one thread and 835 ns
  with four: the pool's price is the handoff between threads, paid back only when the device is
  slow. io_uring on Linux and IOCP on Windows come next, chosen inside `OsIoQueue`, so the engine
  never picks a backend.

**The durability rule for the engine.** No backend orders operations in flight: io_uring_enter(2)
says completions are unordered and a queued fsync need not cover a write queued beside it. So a
sync is only guaranteed to cover the writes whose successful completions were reaped before the
sync was submitted; it may cover more, but nothing else is promised. Overlapping writes, or a
read and a write of the same range, are never in flight at once; a failed write leaves its range
unknown; and a failed sync is still fatal to the file (D59). io_uring's links and drains stay out
of the interface, since the pool and IOCP cannot honour them; an explicit ordering flag may come
later if benchmarks justify it.

**The simulator (D9).** `SimIoQueue` runs no threads. Each operation is pending until the
simulation performs it on its `SimFile`, then done until it is reaped. Every submit and reap
performs a random number of pending operations in a random order, and a reap performs more until
its minimum is met, all drawn from `SimFileSystem::seed_io_queues` and the queue's creation
number, so a run repeats exactly and different seeds reach every completion order. Operations go
through the `SimFile` methods, so the file system's numbering and injected faults cover them.
Across a crash, a done write is an unsynced change the crash mode keeps or loses, a done sync
made its writes durable although nobody reaped it, and a pending operation never happened: it
completes with `StaleHandle`. `short_transfers` makes reads and writes stop early at a seeded
rate, in whole aligned pages on uncached handles.

**Follow-ups, in order:** io_uring on Linux, with our own declarations of its system calls and
ring layout and a run-time probe that falls back to the pool; IOCP on Windows (syncs still go to
the pool, as `FlushFileBuffers` has no overlapped form); registered files and buffers; an epoll
path for networking where io_uring is missing; and the simulator refusing overlapping writes in
flight, so the engine's rule above is checked rather than trusted.

## D67 io_uring behind the queue, and choosing a backend: automatic by default, given or refused when asked — Decided by research (build loop, 2026-10-24); refines D60, D61 and D66
`OsIoQueue` now runs on io_uring on Linux, through our own declarations of
`io_uring_setup` and `io_uring_enter` (system calls 425 and 426 on every architecture in the
allowlist: x86_64, x86, aarch64, arm, riscv64, powerpc64, s390x and loongarch64; not x32) and of
the ring layout from io_uring(7) and io_uring_setup(2). The ring is mapped once
(`IORING_FEAT_SINGLE_MMAP`), and the kernel must report `IORING_FEAT_RW_CUR_POS`, which arrived
with `IORING_OP_READ` and `IORING_OP_WRITE` in Linux 5.6; an older kernel is a refusal. Syncs use
`IORING_OP_FSYNC`, with `IORING_FSYNC_DATASYNC` for `SyncData`. Android is left out for now:
apps run under bionic's seccomp allowlist, and where `io_uring_setup` is not on it the filter
may kill the process with `SIGSYS` instead of returning an error, so an automatic probe there is
unsafe until the allowlist is checked per Android version.

**Decided by research, on choosing a backend:**
- **Automatic by default, as D66 said.** Without a request, a queue takes io_uring where it is
  built and falls back on any refusal (`ENOSYS` from an old kernel, `EPERM` from a seccomp
  filter such as Docker's default or from `kernel.io_uring_disabled`, missing features) to the thread
  pool, or to running inline with `threads(0)`. The engine never has to pick.
- **An explicit request is given or refused, never substituted (as D60 and D61).**
  `IoQueueOptions::backend(IoBackend)` asks for `Inline`, `ThreadPool`, `IoUring` or `Iocp`. A
  refused io_uring is `Unsupported` with the kernel's error, except `ENOMEM` (the locked-memory
  limit on kernels before 5.12), which is `OutOfMemory`. `Iocp` is refused until it is built,
  and `ThreadPool` with no threads is `InvalidInput`. This matches PostgreSQL's
  `huge_pages = try | on`; PostgreSQL 18's `io_method` defaults to its worker processes, and
  TigerBeetle refuses to start without io_uring, so both choices exist in practice, and the
  explicit request serves the engine setting that will come later.
- **The queue reports what it got**: `OsIoQueue::backend()`, using the same `IoBackend` enum,
  for logs and the operations views (D25). The simulator has no backend: it refuses what every
  machine refuses (`Iocp` until it is built, `ThreadPool` with no threads), so code that passes
  in simulation does not fail on a real machine for those, and runs every operation its own
  way.
- **`threads` only sizes the pool.** io_uring starts no threads of ours, so `threads(0)` still
  takes io_uring where it is offered; running inline in submission order is
  `backend(IoBackend::Inline)`.

**What the io_uring backend answers itself, to behave as `OsFile` does:** writes and syncs
through a read-only handle are `PermissionDenied` (D64; the kernel would say `EBADF`), an empty
write is 0 without the kernel, and offsets beyond `i64::MAX` are `InvalidInput`, since io_uring
reads an offset of -1 as "the file's current position". Transfers are capped at Linux's
0x7fff_f000 bytes and reported as short.

**One thread per queue.** Linux cancels the io_uring operations of a thread that exits
(`ECANCELED`), so a queue is submitted to and reaped from by one thread that outlives its
operations, as a per-core event loop is.

**Safety of the buffers.** Each operation in the kernel keeps its file and buffer in a slot of
the ring until its completion is read, the completion ring is twice the submission ring and
never holds more than the queue's capacity, and dropping the ring waits for every operation in
the kernel. If waiting fails, the ring leaks its buffers and mappings rather than free memory the
kernel may still write.

**Tests.** The contract tests run on every backend the machine has, with io_uring requested
explicitly on Linux so a broken probe fails instead of quietly using the pool; GitHub's Ubuntu
runners allow io_uring. A test-only switch refuses the setup to check the automatic fallback.

**Follow-ups:** batching submissions (one `io_uring_enter` per submit today); registered files
and buffers; `IORING_SETUP_SINGLE_ISSUER` and `DEFER_TASKRUN` for thread-per-core rings; IOCP on
Windows; measuring io_uring against the pool on Linux with a committed baseline once the storage
engine's benchmarks exist.

## D68 Second platform review: wrong paths answered alike, overlapping operations refused in simulation, Windows topology grouped as Linux does — Decided by research (build loop, 2026-10-24); refines D64, D65 and D66
The build loop's second review of M1 covered what merged after D64 (D65–D67) and closed two of
their follow-ups.

**Wrong paths, one answer everywhere (closes D64's follow-up).** Decided by research, with no new
`ErrorKind` (one can still be added, since the enum is `#[non_exhaustive]`):
- **A path that runs through a regular file (`f/x`) is `NotFound`**: POSIX says such a path
  cannot be resolved, Windows already answers `ERROR_PATH_NOT_FOUND`, and Python, Java and
  Rust's `Path::exists` treat it as missing. `ENOTDIR` (Rust's `NotADirectory`) now maps to
  `NotFound`, and `exists("f/x")` is `Ok(false)` instead of an error on Unix.
- **A directory where a file is needed, or a file where a directory is needed, is
  `InvalidInput`**, as D64 already said for a rename onto a directory: opening a directory
  (which Unix allows read-only, so `OsFileSystem` checks the opened handle with one `fstat`),
  removing or renaming one, and `sync_dir` on a regular file (Unix and Windows would sync the
  file and succeed). `EISDIR` maps to `InvalidInput`.
- **Creating a name that is taken stays `AlreadyExists`**: `CreateNew` on a directory, and
  `create_dir_all` when the path or a parent is a regular file.
- The extra checks run at `open` (one `fstat`), on `rename`'s source (one `stat`) and in
  `sync_dir` (one `fstat` beside an `fsync`), and otherwise only on error paths: Windows
  answers a rename onto `f/x` with an error Rust reads as `InvalidInput`, so a failed rename
  looks at the ancestors of both paths and calls one that runs through a file `NotFound`.
  Reads and writes are untouched.
  Telling a missing database from a corrupt layout needs only `NotFound` against the rest, and
  initialising over a stray file still stops at `AlreadyExists`.

**The simulator checks the engine's I/O rule (closes D66's last follow-up).** `SimIoQueue`
refuses, with `InvalidInput`, a submit whose read or write overlaps a write still in flight in
the same queue on the same file, through any handle opened since the last crash (or a write
overlapping a read or write in flight); a completion that was performed but not yet reaped is
still in flight. Empty transfers overlap nothing. No operating system enforces the rule, so
engine code that breaks it now fails in tests instead of racing on a machine. The check scans
the queue's operations in flight on each submit, which is cheap at the default capacity of 128;
an interval index replaces it if simulations run full queues of thousands. Overlaps between two
queues (two shards writing one file) are not caught yet.

**Windows topology grouped as Linux does (refines D65).** The Windows parser now cuts package
and cache groups down to the CPUs the core records place before numbering them, so identifiers
stay dense, and drops repeated identical groups (in `number_groups`, shared with Linux), so a
repeated record no longer makes a kind unknown. Each CPU's last-level cache is the
highest-level data or unified cache that contains it, as on Linux, instead of the machine's
highest level: on a chip whose small cores have no L3, those cores keep their L2 group, and a
CPU with no cache at all makes the kind unknown on both systems. Both parsers' random-input
tests now check that every kind is numbered from 0 without gaps and that the counts match.

**Reviewed and found sound:** the io_uring ring (slots freed only on completion, the tail
rolled back only when the kernel did not consume the entry, the completion ring never
overfilled, `Drop` waiting or leaking), the thread pool's shutdown, and the parsers' bounds
against huge ranges and malformed records.

**Still open:** catching overlapping operations across queues in the simulator; the largest
reported data cache line can exceed `CACHE_LINE_PADDING` on arm64 chips with 256-byte lines
(Fujitsu A64FX), as on 32-bit arm (D65); and `sync_dir` in the simulator still scans every name
(D64).

## D69 IOCP behind the queue: a second overlapped handle per queue, uncached files through the port, on request only — Decided by research (build loop, 2026-10-25); refines D66 and D67
`OsIoQueue` now runs on Windows' I/O completion ports when `IoBackend::Iocp` is asked for,
through our own declarations of `CreateIoCompletionPort`, `ReOpenFile`,
`SetFileCompletionNotificationModes`, `ReadFile`, `WriteFile`, `GetOverlappedResult`,
`GetQueuedCompletionStatusEx` and `PostQueuedCompletionStatus`.

**Decided by research:**
- **`OsFile` stays synchronous; each queue reopens the file for itself.** A handle opened with
  `FILE_FLAG_OVERLAPPED` cannot go through std's positional reads and writes (std waits on the
  handle and aborts if the I/O is still pending), and a file object joins one completion port
  for its whole life, while one `Arc<OsFile>` may be used by several queues (one per core). So
  the first time a queue starts a read or write of a file, it calls `ReOpenFile` (same access,
  sharing read, write and delete, `FILE_FLAG_OVERLAPPED | FILE_FLAG_NO_BUFFERING`), which makes
  a new file object, binds it to its own port and sets `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS |
  FILE_SKIP_SET_EVENT_ON_HANDLE`. The file keeps those handles in a small list keyed by the
  queue's number (from a counter, never reused), so a handle closes with its file at the
  latest, and a closed file's handle can never be matched to another file, which keying by the
  `Arc`'s address would allow. A queue remembers the files it bound weakly and closes its
  handles when it is dropped, after its operations finished.
- **Only uncached files go through the port.** Microsoft documents that cached reads that hit
  the cache, writes that extend a file, and compressed or encrypted files complete
  synchronously, and that `FILE_FLAG_NO_BUFFERING` is the way to get truly asynchronous
  requests; a cached read that misses can block the caller's thread. Reads and writes of cached
  files therefore go to the queue's worker threads, as do all syncs, since `FlushFileBuffers`
  has no overlapped form. Workers wake the port with an empty packet, so `reap` waits in one
  place; with `threads(0)` those operations run inline inside `reap`.
- **On request only.** The automatic choice on Windows stays the thread pool until a committed
  Windows benchmark (D7) shows the port is no slower; an explicit `Iocp` is given on Windows and
  refused with `Unsupported` elsewhere. The simulation accepts it, as it accepts `IoUring`.
- **One packet per operation that started.** An operation that finishes at once is completed
  from its own structure (the skip flag means no packet follows), one that is pending completes
  from its packet, and one that fails at once completes with that error and no packet;
  `ERROR_INVALID_USER_BUFFER` and `ERROR_NOT_ENOUGH_MEMORY`, documented as too many requests
  outstanding, hand the operation back with `QueueFull` while the port has operations in flight
  (otherwise they complete with that error, since reaping would free nothing). A Windows test submits refused
  operations between good ones to pin that no stray packet arrives.
- **Answered without the system, as `OsFile` and io_uring answer them:** empty reads and writes
  transfer 0, writes through a read-only handle are `PermissionDenied`, and offsets beyond
  `i64::MAX` are `InvalidInput`, since Windows reads the top offsets as "append" and "current
  position". Transfers are capped at 1 GiB (sector-aligned, under `ReadFile`'s 32-bit length)
  and reported as short; a read at the end of the file (`ERROR_HANDLE_EOF`) transfers 0.
- **Refusals.** A port the system will not make is `Unsupported` (`OutOfMemory` for
  `ERROR_NOT_ENOUGH_MEMORY`); a file `ReOpenFile` or the binding refuses hands the operation
  back with `Unsupported` for `ERROR_INVALID_PARAMETER`, `ERROR_NOT_SUPPORTED` and
  `ERROR_INVALID_FUNCTION`, and the system's own kind otherwise.
- **Dropping a queue waits without cancelling.** `CancelIoEx` could tear a page that was half
  written, so the queue waits for every packet; if waiting fails it leaks the operations' memory
  rather than free what the system may still write, as D67's ring does.

Each operation in flight is a heap allocation that begins with its `OVERLAPPED` and owns its
file and buffer; it is freed only when its packet is read, or at once when no packet will come.
The completion key is the queue's number; packets are read 64 at a time with
`GetQueuedCompletionStatusEx`. While workers have operations, `reap` waits on the port at most
10 ms at a time and then checks them, so a wake packet the system failed to post cannot hang it.
Every wait that returns a full batch is followed by another without waiting, so wake packets
whose completions were already taken from the workers never pile up beyond one queue's worth.

**Follow-ups:** guarding `OsFile`'s own positional reads and writes on Windows against offsets
beyond `i64::MAX`, which the workers' path still passes to the system; checking that every file
system the engine runs on honours `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS` (network redirectors in
particular), since a packet after an immediate success would name freed memory; measuring the
port against the pool on Windows once the storage engine's
benchmarks exist, then deciding the automatic choice; Windows 11's `IoRing` API; registered
buffers (`SetFileIoOverlappedRange`); and D67's one-thread-per-queue rule checked against how
Windows treats I/O issued by a thread that exits.

## D70 Networking: a completion queue of accepts, connects, receives and sends, a portable poller first, a simulated network — Decided by research (build loop, 2026-10-25); refines D40, D59 and D66
The platform layer now has a `Network` (`OsNetwork`, and `SimNetwork`'s `SimHost`s) with TCP
listeners and connections, and a `NetQueue` of `NetOp`s (`Accept`, `Connect`, `Receive`,
`Send`) reaped as `NetCompletion`s, each operation owning its listener, connection and
`IoBuffer` until it completes.

**Decided by research:**
- **The completion shape, not blocking calls or a readiness poller.** D66's reasons hold for
  sockets too: a single-threaded simulator can only make a completion queue deterministic (a
  blocking receive would deadlock it, and `WouldBlock` without a poller is unusable),
  io_uring and IOCP both do sockets as completions, and TigerBeetle's I/O layer does accept,
  connect, receive and send the same way over io_uring, kqueue and IOCP. A readiness poller
  (epoll, kqueue) is an implementation detail inside a backend, not the interface.
- **A sibling queue for now.** `IoOp` is generic over a file and the simulated file queue is
  tied to `SimFileSystem`, so sockets get their own `NetQueue` with the same contract: a tag, a
  bounded capacity with `QueueFull` and `RejectedNetOp`, `reap(min)` refusing `min` above what
  is in flight, short transfers reported. Merging file and socket operations into one queue
  per core is decided when io_uring and IOCP get their socket backends.
- **`Address` is a non-exhaustive enum** with only `Tcp(SocketAddr)`, so Unix-domain sockets
  and others are added without breaking the traits.
- **A receive of 0 bytes means the peer ended its sending**, as in POSIX, so a receive into an
  empty buffer is `InvalidInput`; an empty send transfers 0 without the system. A connection
  has at most one send and one receive in flight, since two sends may interleave.
- **New error kinds**, mapped from the operating system's: `ConnectionRefused`,
  `ConnectionReset` (also for aborted connections and broken pipes, so the machine and the
  simulation report one set), `TimedOut`, `AddrInUse`, `AddrNotAvailable`, `NotConnected`,
  `HostUnreachable` (also for unreachable networks), and `Stalled` for the simulation.
- **The first OS backend is a portable poller** over std's non-blocking sockets: `reap` retries
  the operations that would block, backing off from 20 µs to 2 ms between rounds, and a
  connect waits inside `reap` (std has no non-blocking connect). It is correct everywhere std
  has sockets and is never the performance path; epoll, kqueue, io_uring and IOCP backends
  follow, each with its own decision. Connections turn Nagle's algorithm off, as PostgreSQL
  and Redis do for their small requests and answers.
- **The simulation (D9)**, modelled on FoundationDB's simulated network: one `SimNetwork`
  holds every host; each direction of a connection is an ordered byte stream through a bounded
  buffer (64 KiB); sent bytes are in transit and arrive a seeded amount at a time, so
  connections advance in a different order on every seed while each stays in order. Queues
  perform their operations only inside `reap`, in a seeded order, and when nothing left could
  finish what `reap` waits for, it fails with `Stalled` instead of hanging, as asking for more
  than is in flight is `InvalidInput`. Dropping a connection with bytes owed to it resets it;
  dropping it otherwise ends its sending; dropping a listener resets the connections it did not
  accept. Every listen and completed operation is numbered and can fail with `InjectedFault`
  by number or at random, changing nothing. Connections (16,384), backlogs (128 per listener),
  buffers and queues are bounded; a connect takes the lowest free ephemeral port, and maps are
  ordered, so a seed repeats exactly. `Stalled` looks at the queue being reaped only: a
  simulation that drives several queues reaps each with a `min` its own operations can meet, or
  0, in turn. A connection or listener from another `SimNetwork` is refused at submit.

**Follow-ups, in order:** time in the simulation, with timeouts on operations, partitions,
clogging and latency, so a stalled connect becomes `TimedOut`; `SO_REUSEPORT` and listen
options (std cannot set them before `bind`, so they need our own socket declarations); epoll
and kqueue backends, then sockets on io_uring and IOCP and the decision on one queue per
core; Unix-domain sockets; UDP (then QUIC, built by us); a resolver; TLS above `NetStream`
(D23).

## D71 Readiness backends for the network queue: epoll and kqueue, interest only while an operation waits — Decided by research (build loop, 2026-10-25); refines D67 and D70
`OsNetwork`'s queues now wait in the kernel instead of sleeping: `NetBackend::Epoll` on Linux and
Android, `NetBackend::Kqueue` on Apple's systems, and `NetBackend::Portable` (D70's poller)
elsewhere. `NetQueueOptions::backend` asks for one, given or refused with `Unsupported`, never
falling back, as D67 does for files; `OsNetQueue::backend` says which one runs. On a loopback
ping-pong of 8-byte messages through one queue on an Apple M-series Mac (release build), a round
trip took 13 µs with kqueue against 65 µs with the portable poller, which also burns its back-off
sleeps.

**Decided by research:**
- **Interest only while an operation waits, level-triggered.** Each operation is tried once when
  `reap` starts it; only one that would block registers its socket's direction (accept and
  receive read, connect and send write), and the registration is removed before the operation's
  completion leaves the queue; a failed connect's socket is closed only after that. The queue holds the socket's `Arc` while it is registered, so the
  descriptor can neither close nor be reused under it, and one socket may be used from several
  queues, each with its own epoll or kqueue. Edge-triggered registration for a socket's life
  (mio's way) needs the socket to deregister itself on drop and needs draining to `EAGAIN`, which
  one transfer per operation does not do. Most sends finish on the first try, so the extra
  `epoll_ctl` calls are paid only by operations that wait.
- **kqueue uses the same level-triggered `EV_ADD`/`EV_DELETE` path as epoll**, applied at once.
  Research preferred `EV_ONESHOT` with the changes passed in the waiting `kevent` call (one system
  call per round, as TigerBeetle does); sharing one interest-tracking path with epoll was chosen
  for now, and batching the changes into the wait is a later optimisation measured on its own.
- **Apple only for kqueue.** `struct kevent` differs on FreeBSD (`ext[4]`, versioned symbols),
  NetBSD and OpenBSD, and CI has no BSD runner; the BSDs and other Unixes keep the portable
  poller until a `poll(2)` backend, whose `struct pollfd` is the same everywhere.
- **Epoll only on the architectures io_uring already lists** (x86, x86_64, arm, aarch64,
  riscv64, powerpc64, s390x, loongarch64): socket and descriptor constants differ on mips, sparc
  and alpha, and `epoll_event` is packed on x86_64 only, which a compile-time size check pins.
- **Connects never block.** Readiness backends create the socket with our own `socket`
  declaration (close-on-exec), connect it non-blocking, treat `EINPROGRESS` or `EINTR` as
  pending, wait for it to become writable and then read `SO_ERROR` (std's `take_error`) and the
  peer address. The portable poller still connects with a blocking call, so on Windows a
  connect that gets no answer holds that queue until it times out, until IOCP sockets land.
- **No `SIGPIPE`.** std sends with `MSG_NOSIGNAL` on Linux, but on Apple's systems it sets
  `SO_NOSIGPIPE` only on sockets it creates itself, not on accepted ones, and a process that
  embeds EggDB (D3) has not ignored `SIGPIPE` as Rust programs do, so every stream sets
  `SO_NOSIGPIPE` there.
- **Errors:** `EINTR` from a wait is retried; an accept that finds the connection already
  aborted keeps waiting; `EPOLLERR` and `EPOLLHUP` wake both directions, and the operation itself
  reports the real outcome; a registration the kernel refuses (`ENOSPC`, `ENOMEM`) fails the
  operations waiting on that socket, not the queue. Each wait returns at most
  min(capacity, 1024) events, from a buffer allocated once.

**Review fixes in the simulation (D9):** a queue refuses a second send or a second receive on one
end of a connection while the first is in flight (`InvalidInput`), so a caller that breaks D70's
rule is caught in tests; the first send after the peer closed cleanly succeeds and resets the
connection, as over TCP, instead of failing at once; and empty sends and receives are numbered
for fault injection like every other operation. Refusing a host's queue the sockets of another
host is left for later: the simulation's own tests drive both ends of a connection from one
queue.

**Follow-ups, in order:** time in the simulation with timeouts, partitions and latency (D70);
`poll(2)` for the BSDs and other Unixes; non-blocking connect in the portable poller on Unix;
sockets on io_uring and IOCP, and one queue per core; batching kqueue changes into the wait;
the contract tests run once per backend; fewer system calls per connection (`SOCK_NONBLOCK` on
Linux, no second non-blocking and no-`SIGPIPE` call on sockets the queue created, waking one
pending accept per ready connection instead of all).

## D72 Timeouts in the network queue: a timed reap and cancel, with simulated time, latency and partitions — Decided by research (build loop, 2026-10-25); refines D70 and D71
A caller can now bound a wait and give up an operation. `NetQueue::reap_timeout(completions, min,
timeout)` waits at most `timeout` and, when time runs out first, returns the fewer than `min`
completions it appended (possibly none) as `Ok`, leaving the rest in flight; a zero timeout only
collects what has finished. `NetQueue::cancel(user_data)` gives up the operations in flight with
that tag: each still completes exactly once through a later reap, with the new
`ErrorKind::Canceled` and its listener, connection and buffer, and stays counted in `in_flight`
until then; an unknown tag is `NotFound`.

- **Why not a deadline inside each operation:** only io_uring has a per-operation timeout
  (`IORING_OP_LINK_TIMEOUT`); IOCP and the readiness backends would each need their own timer plus
  an internal cancel, and it would put timeout policy (connect, idle, request deadlines) in the
  platform layer when the engine needs one timer wheel across files, network and queries.
  FoundationDB and TigerBeetle both keep timeouts above the I/O layer. Cancel maps onto every
  backend: `IORING_OP_ASYNC_CANCEL`, `CancelIoEx`, deregistering in epoll and kqueue, and the
  simulator; it is also the only way to get an owned buffer (D66) back early.
- **Why `Ok` with fewer completions, not `Err(TimedOut)`:** running out of time is a normal
  outcome, and epoll, kqueue and `GetQueuedCompletionStatusEx` all report it as fewer events.
  `Err` stays for real failures.
- **Why a relative `Duration`, not a deadline on a `Clock`:** every backend's wait takes a
  relative timeout, absolute io_uring timers need Linux 6.12, and a deadline on a clock other
  than the queue's would be a silent bug. Each backend turns it into a deadline once, so an
  interrupted wait does not start over.
- **Spelling:** `Canceled`, as the repository writes American English and `ECANCELED` does.
- **OS backends:** epoll waits with the remaining time rounded up to a millisecond, kqueue with a
  `timespec`; an interrupted wait (`EINTR`) now returns to the queue's loop, which recomputes the
  time left. The portable poller sleeps at most the time left; its blocking connect can still
  overrun a timeout (D71). Cancel in the readiness queue drops the operation's interest, and a
  pending connect's socket is closed only after the kernel forgets it.

**Simulated time (D9).** `SimNetwork` keeps time on a `SimClock` (its own, or one shared through
`SimNetwork::with_clock`), which moves only inside `reap`: when no operation can finish and
nothing is due, the queue moves the clock to the next event (bytes or an end of sending arriving,
a handshake finishing, a connect timing out), or to the end of a timed reap's wait, whichever is
first. A plain `reap` fails with `Stalled` only when no event is left, as before. This is
FoundationDB's discrete-event approach: the same seed gives the same schedule and the same clock
readings.

- **Latency:** `SimNetwork::latency(least, most)` draws each send's one-way delay from the seed;
  bytes keep their order (a later send never arrives before an earlier one), an end of sending
  arrives after the bytes before it, and a connect takes a round trip. Zero, the default, draws
  nothing, so earlier seeded schedules are unchanged.
- **Partitions:** `SimNetwork::partition(a, b)` cuts two hosts apart until `heal(a, b)` or
  `heal_all()`. Bytes between them stay in transit, so receives wait; a connect across the cut
  fails with `TimedOut` after `SimNetwork::CONNECT_TIMEOUT` (127 s, Linux's six handshake retries
  doubling from one second), and one that heals in time completes. Connections already open are
  not reset by a cut, as TCP survives a short outage.
- Canceled operations are not numbered for fault injection, since they are not network
  operations that completed.

**Not yet:** a retransmission timeout that fails a connection whose bytes stay unacknowledged
across a long cut (Linux's `tcp_retries2`, about 15 minutes), one-way cuts, per-host latency and
bandwidth, and timed reaps and cancel on the file I/O queue (D66).

**Review fix (D71 follow-up):** the network contract tests now run once per operating-system
backend (the portable poller and the system's own) instead of only the automatic one, and half
of the simulated seeds run with latency.

## D73 `poll(2)` for the network queue on the BSDs and illumos, with blocking connects there for now — Decided by research (build loop, 2026-10-25); refines D71
`NetBackend::Poll` waits for readiness in `poll(2)`. It is the automatic backend on FreeBSD,
NetBSD, illumos and Solaris, which had only the portable poller, and can be requested wherever
epoll or kqueue is available (Linux, Android and Apple's systems); CI runs the contract tests on
it on Linux and macOS. It shares the readiness
queue with epoll and kqueue: a selector enum picks the system's own selector or `poll`, and the
queue's interest tracking (interest only while an operation waits, D71) is unchanged.

- **Why `poll(2)` and not each BSD's kqueue or illumos's event ports:** `struct pollfd` and the
  `POLL*` values are the same on every Unix, so one small declaration serves them all, while
  `struct kevent` differs on FreeBSD, NetBSD and OpenBSD and event ports are illumos-only. Each
  wait hands the kernel every waiting socket, so it costs O(sockets) per wait; native selectors
  per OS can follow when a workload needs them.
- **Verified from the public headers (FreeBSD and NetBSD under BSD, illumos under CDDL):**
  `pollfd` is `{int fd; short events; short revents}`, `POLLIN` 0x1, `POLLOUT` 0x4, `POLLERR`
  0x8, `POLLHUP` 0x10, `POLLNVAL` 0x20 and `EINTR` 4 everywhere; `nfds_t` is `unsigned long` on
  Linux (glibc and musl), illumos and Solaris and `unsigned int` on Android (bionic), Apple's
  systems, FreeBSD and NetBSD; `poll`
  is not a versioned symbol on NetBSD (only `pollts` is).
- **Connects still block on FreeBSD, NetBSD, illumos and Solaris.** A non-blocking connect needs
  our own `socket` and `connect` declarations with constants that differ in ways nothing would
  catch without a runner: `SOCK_STREAM` is 2 and `EINPROGRESS` 150 on illumos, whose
  `sockaddr_in6` is 32 bytes; `AF_INET6` is 28, 24 and 26; the BSDs' sockaddrs start with a
  length byte; NetBSD's `socket` is the symbol `__socket30`. Correctness first (D9): the poll
  backend uses std's blocking connect there, as the portable poller already did, and
  non-blocking connect follows once a VM runner (FreeBSD, NetBSD, OmniOS) tests it. On Linux and
  Apple's systems the poll backend connects without blocking, through the same code as epoll
  and kqueue.
- **CI:** a new `cross` job lints the platform layer for FreeBSD, NetBSD, illumos, Solaris,
  aarch64 Linux, Android, iOS and WebAssembly, which catches type and `cfg` errors though not wrong
  constants or link errors. It runs beside the other jobs, so the time to a green run is
  unchanged.
- **Measured:** on a loopback ping-pong of 8-byte messages through one queue (Apple M-series,
  release build), a round trip took 14.9 µs with `poll`, 13.5 µs with kqueue (13 µs before this
  change) and 70.7 µs with the portable poller, which those systems ran until now.

**Not yet:** OpenBSD and DragonFly (tier 3 targets, which cannot be linted here) still run the
portable poller; non-blocking connects and runners on the BSDs and illumos. On Solaris a send
to a peer that went away may still raise `SIGPIPE`, as std sends with `MSG_NOSIGNAL` on Linux,
the BSDs and illumos but not there, and the portable poller had the same gap; it needs
`SO_NOSIGPIPE`-like handling once Solaris has a runner.

## D74 Fewer system calls per connection and per wait: one-shot interest, sockets ready in one call, one accept per connection — Decided by research (build loop, 2026-10-26); refines D71 and D73

D71 registered a socket with epoll or kqueue only while an operation waited on it, so every
receive or send that would block cost two extra system calls on the per-message path: a register
before the wait and a deregister after it woke. The readiness queue now asks the selector for
**one-shot** interest and leaves what is left after it fires:

- **epoll** registers with `EPOLLONESHOT`. After an event the entry stays in the epoll set,
  disabled, and the next wait re-arms it with one `EPOLL_CTL_MOD`; the first wait on a socket
  adds it. A wait costs one `epoll_ctl` instead of two.
- **kqueue** adds each filter with `EV_ONESHOT`, which the kernel deletes when it delivers the
  event. A wait costs one `kevent` change instead of two.
- **`poll(2)`** keeps its own list (D73) and makes no system call for a change, so it is unchanged.
- **Descriptors are closed and reused behind the queue's back:** users hold the sockets and may
  drop them at any time. The selector remembers per descriptor what it has armed. Interest is
  armed only while an operation waits, and that operation holds the socket open, so an armed
  descriptor cannot be closed and reused. A cancel that leaves no waiter disarms at the next
  reap, before that reap hands the canceled operation back, so the socket is still open when it
  is disarmed (one call, off the hot path). What may be stale is only epoll's "already in the set" hint for a
  closed descriptor; a re-arm that gets `ENOENT` adds instead, and an add that gets `EEXIST`
  modifies. So the first wait on a socket that reuses an old descriptor number costs two calls
  (the failed modify and the add), as every wait did before, and each later wait one. A test cancels, drops and reconnects so descriptors are reused, and fails when a
  cancel leaves interest armed.
- **Why not keep interest for the socket's lifetime (research's option B, as libuv does) or
  edge-triggered registration (mio):** with level-triggered interest that outlives its waiter,
  a socket that is ready but unread wakes every wait until something deletes it. A descriptor
  shared with a child process through `fork` keeps its epoll registration after the parent
  closes it, and the registration then fires under a number the parent can no longer delete.
  One-shot interest fires at most once, so neither case can spin, and it saves half of the
  calls for none of that state. Edge-triggered registration needs the same close tracking plus a
  second path for `poll`.

**Sockets ready in one call:**
- On Linux and Android a connect creates its socket with `SOCK_NONBLOCK | SOCK_CLOEXEC`, and an
  accept uses `accept4` with the same flags. That saves the `ioctl` that made the socket
  non-blocking, which D71's connect also repeated after the connect finished.
- On Apple's systems the listener gets `SO_NOSIGPIPE` and `TCP_NODELAY` when it is made. An
  accepted socket inherits both from it, and inherits the listener's non-blocking mode, so an
  accept needs no call after `accept` (std's `accept` still sets close-on-exec). A test checks
  on macOS that accepted and connected sockets are non-blocking, have Nagle's algorithm off and
  suppress `SIGPIPE`, and on every OS that they are non-blocking without Nagle.
- Elsewhere an accepted or connected socket is set up as before, one option at a time.

Before, a queue-made connection on Linux took `socket`, `ioctl`, `connect`, `epoll_ctl` to add,
`epoll_wait`, `epoll_ctl` to delete, `getsockopt`, `getpeername`, `ioctl` and `setsockopt`.
Now it takes `socket`, `connect`, `epoll_ctl`, `epoll_wait`, `getsockopt`, `getpeername` and
`setsockopt`.

**One accept per connection:** when a listener becomes readable and several accepts wait, the
queue retries them one at a time in submission order and stops at the first that would block,
which puts the rest back in front without a call. Before, every waiting accept tried and all
but one got `EAGAIN`. A level-triggered listener reports readiness again for the next
connection, so stopping loses nothing. Only accepts share a direction of one socket in
practice, as D70 allows at most one receive and one send in flight per stream.

**Platform `cfg` lists:** the readiness module now chooses its backends with `cfg_select!`
(stable in the pinned toolchain) instead of repeating the list of platforms six times, without
a build script.

**Measured** on a loopback ping-pong of 8-byte messages through one queue, and a connect plus
accept with eight accepts waiting (Apple M-series, release build, several runs each): kqueue round trips
went from 14.5 µs to 14.3 µs and connect plus accept stayed at 24 to 27 µs, so the change is
within noise on macOS, where kqueue changes are cheap and loopback wake-ups dominate. It was
not measured on Linux (no Linux machine here; CI runs the tests there), so the Linux gain is the
saved calls counted above, not a timing. The probe also showed that thousands of
connections a second exhaust the ephemeral ports through `TIME_WAIT` (`EADDRNOTAVAIL`); that is
the operating system's limit, not the queue's.

**Not yet:** folding kqueue changes into the waiting `kevent` call, which would make a wait cost
no change call at all but needs per-change errors (`EV_ERROR` events or `EV_RECEIPT`); measuring
on Linux.

## D75 Releases: v0.1.0 first, built and published by a tag-triggered workflow, static Linux binaries by default — Decided by research (build loop, 2026-10-26); refines D6, D14, D21 and D43
**Numbering:** M0 was never tagged, so M1 is the first release, v0.1.0, and each later milestone
bumps MINOR from there. No tag is added retroactively for M0: it would publish a release with no
binary behind it.

**How a release is cut:** a pull request bumps the workspace version and adds
`docs/releases/vX.Y.Z.md`; after it merges and `main` is green, the tag `vX.Y.Z` is pushed.
`.github/workflows/release.yml` then:
- refuses a tag that doesn't name the workspace version;
- refuses a shipped binary that depends on any crate but our own (D1);
- builds `eggdb` for each target, runs it where the runner can, and packs it with the licenses
  and the README (`.tar.gz`, `.zip` on Windows);
- writes `SHA256SUMS`, attests build provenance for every file (signed through GitHub's
  Sigstore-backed attestations, verifiable with `gh attestation verify`), and publishes the
  GitHub Release with the notes file plus a table of binary and download sizes, the first
  numbers D6 and D14 track.

Pull requests that change the workflow or the notes run the builds without publishing, so the
pipeline is tested before a tag depends on it.

**Targets:** Linux on x86-64 and AArch64 twice: static `-linux-musl` builds, the default
download because only they are one self-contained file with no dependencies on every
distribution (D14), and `-linux-gnu` builds made on Ubuntu 22.04 so they need only glibc 2.35.
The glibc builds stay because musl's allocator is markedly slower under many threads, which D7
can't accept; a memory allocator of our own, now on the roadmap, is what closes that gap later.
macOS on Apple silicon and Intel, Windows on x86-64 and AArch64. A CI job runs the whole test
suite against musl so a release is never the first musl build.

**Stripped:** release builds strip symbols (`strip = true`), which took the binary from 375 KB to
302 KB on macOS. The cost is unnamed frames in a backtrace printed before an abort; crash
reports are symbolized from the build instead, once releases are reproducible.

**Legal check before v0.1.0 (D43):** the release contains no cryptography, so there is no
export classification to make yet; that research is due before the first release that ships
D23's cryptography. The shipped binary has no third-party code, and the workflow checks it.

**Not yet:** signing beyond the provenance attestation, a software bill of materials (empty
while nothing third-party ships), reproducible builds, package managers and registries (they
need the owner's accounts), and the startup, idle-memory and install-time measurements, which
start with the first build that serves data (M4).

## D76 The first user release: v0.2.0 on 2026-11-01, an embedded key-value store in one file — Decided (owner, 2026-10-26); refines D75 and the roadmap's order
v0.1.0 only prints its version. The owner put time to market first for one week: v0.2.0, on
2026-11-01, is the first release people can use. It is an embedded, crash-safe, transactional
key-value store in one file, with:
- a Rust API;
- an `eggdb` command-line tool;
- `eggdb ui`, a full-screen terminal UI for browsing and editing a database.

**Why a terminal UI:** a web UI needs a network listener, and D22 requires TLS on every
listener, while a desktop app needs a GUI toolkit, and D1 allows no third-party crates. The
desktop GUI stays a must-have of the client track (D15) after this release.

**Out of scope for v0.2.0:**
- a network server, because D22 requires TLS and D35 requires our own cryptography, which
  doesn't fit the week;
- SQL and the Redis and PostgreSQL protocols;
- compression;
- more than one writer at a time.

**The roadmap's own order, cut down:** M2's storage engine, a single-writer slice of M3's
transactions, M4's embedded API and the one-shot commands of M5's CLI, brought forward. The
must-have list, in order, is the roadmap's "Next: v0.2.0" section. Its cut order when behind is:
1. the nice-to-haves;
2. reverse scans;
3. merging under-filled pages on delete;
4. overflow pages (a documented value-size limit instead);
5. import (export stays);
6. editing in `eggdb ui` (it ships read-only).

Never cut: checksums, recovery, the crash tests, the hostile-file fuzzer, the sample-file
compatibility test, `eggdb check` and the correctness statement.

**Releases are held this week (an exception to D75's release on every milestone):** milestone
releases are batched into v0.2.0, and no other release is cut before it except a release
candidate. The feature freeze is 2026-10-30 23:59; after it only fixes, tests, release
engineering and docs merge. If crash safety isn't proven by the freeze, v0.2.0 still ships on
2026-11-01, as a GitHub pre-release labelled preview, "not for data you can't lose", rather
than move the date. The exception ends when v0.2.0 is out.

## D77 The storage engine's design and file format, version 1 — Decided by research (build loop, 2026-10-26); refines D20, D40 and D41 under D76
The full specification is [format.md](format.md). The research behind this entry is the M2 design
brief in issue #30 (from the papers it cites) and a research pass on what a one-file v0.2.0
needs.

**Index: an in-place B+tree**, with fence keys and prefix truncation. Neither an LSM nor a
copy-on-write tree:
- an in-place B+tree reads best and fits serializable snapshot isolation (M3);
- LeanStore (ICDE'18) and Umbra (CIDR'20) show it comes close to in-memory speed with a lean
  buffer manager;
- an LSM's compaction spikes and background wake-ups hurt embedded use (D41);
- a copy-on-write tree has no log to become a consensus log (D20).

Page types are reserved for immutable sorted runs, and an LSM or hybrid replaces the B+tree
only if it wins issue #30's pre-registered bake-off:
- it ingests more than twice as fast;
- with at most 1.5 times the read latency;
- and at most 1.3 times the space.

**Buffer: our own page cache** with a fixed memory budget. No `mmap`: Crotty, Leis and Pavlo,
CIDR'22.

**One file, with the log inside it.** The write-ahead log is a chain of fixed-size segments in
the database file. Not a sidecar, and not a fixed ring:
- a sidecar breaks D3's one file while the database is open, can be left behind or separated
  from its file, and needs directory syncs;
- a fixed ring fixes the log's size at creation and makes the empty file bigger.

Segments are recycled after checkpoints and reused before the file grows. One sync covers the
log and the data.

**Checksums: CRC-32C everywhere**, in hardware where the processor has it. They detect
accidental damage, not tampering. A crafted file must still fail safely, and tamper detection
waits for encryption at rest (D23). The page checksum
is seeded with the file's identity and the page number:
- a misdirected or foreign page fails like a damaged one;
- pages need not store their own number;
- the superblock checksum covers its magic and fields.

Measured on this Mac (Apple silicon, one stream): about 11.5 GB/s, 357 ns per 4 KiB page.
Splitting the work into three interleaved streams to go faster is a later optimisation, once
the benchmark harness (#28) exists.

**Header: two 4 KiB slots at bytes 0 and 4096, whatever the page size.** The slot is chosen by
the generation's parity. The newest intact slot wins. An intact slot that is refused for its
contents refuses the file, rather than falling back to an older state. That includes a
superblock in the wrong slot and two slots with the same generation: failing safe beats
guessing. A damaged other slot is reported to the caller, because damage after a sync looks
like a torn write. Recovery then checks the log head's sequence number, so an older header
can't silently open a newer file. Compatibility uses ext4's three feature sets: compatible,
read-only-compatible and incompatible. Page numbers keep 16 bits for a later volume number.

**Snapshot readers in v0.2.0: copies of pages in memory.** Before the writer first changes a
page that a live reader may still need, it keeps the committed image in memory. A reader
whose snapshot predates a page's current version reads that copy. Pages freed by a commit are
reused only once no reader can see them. A reader that falls further behind than the bounded
store allows gets a "snapshot too old" error.

Nothing of this reaches the file, so recovery never sees it. Reading old pages back from the
log (as SQLite's WAL mode does) was rejected, because our log isn't made of whole pages. M3
replaces the copies with versions in the leaves.

**Recovery: analysis and redo, no undo.** In v0.2.0 no page changed by an uncommitted
transaction is written to the file. A transaction larger than the page cache therefore fails
with a readable error until M3. The first change to a page after a checkpoint logs the whole
page, against torn pages.

**Hybrid-clock stamps:** 64 bits, 52 bits of microseconds since 2026-01-01T00:00:00Z and a
12-bit counter. That runs to about 2168, where a 1970 epoch would end in 2112. The high-water
mark is persisted so stamps never go backwards.

**Durability modes:** `Full` (the default, a sync per commit group), `Normal` (syncs at
checkpoints, never corrupts) and `Off`. The names and behaviour follow SQLite's `synchronous`
setting, so they are familiar. Apple's systems use `F_FULLFSYNC`.

**Locking:** an exclusive lock on the database file itself, never a lock file:
- open-file-description locks on Linux;
- `flock` on Apple's systems and the BSDs;
- `LockFileEx` on Windows, on a byte range past any data.

Classic POSIX record locks are avoided, because closing any descriptor of the file drops them.

**Keys:** each tree has a key scheme in the catalog. Scheme 0, raw bytes compared as unsigned,
is the only one in v0.2.0. Tenant and shard prefixes come as a later scheme behind an
incompatible feature flag. Prefix truncation makes them cost about nothing per entry (D20).

**Fuzzing:** our own deterministic, seeded fuzz loops run inside `cargo test`, with no crate.
Every decoder of file bytes returns errors as values.

## D78 The page file: commits that never overwrite committed pages, and file locks through the platform layer — Decided by the build loop (2026-10-26); refines D59 and D77
`eggdb-storage`'s `PageFile` is the database file seen as checksummed pages. Every later item
writes through it.

**Commits.** Changes become durable only at a commit:
1. Write the new free list.
2. Sync the file.
3. Write the next superblock to the slot the current one isn't in.
4. Sync again.

Until the log exists (item 4), a commit is the only way to make anything durable. Two rules
mean the committed state is never overwritten before the new superblock is durable:
- a page freed since the last commit is handed out only after the commit that frees it;
- the new free list goes to pages that are free in the committed state, or to new pages, never
  over the committed list.

A crash at any operation of a commit, under torn writes, opens the old state or the new one.
The simulator checks every operation with four torn-write seeds each. The cost is that a
commit with no free page grows the file by the pages its list needs. The log and checkpoints
(items 4 and 7) replace this with logged allocation.

**A failed write or sync** stops the file taking changes until it is opened again (D59).

**Opening checks before trusting the file:**
- the file's real length against `page_count`;
- the free list: every entry in range and listed once, no loops, and the count matches.

A damaged list refuses the open with a readable error. A seeded test rewrites list pages with
valid checksums and checks this.

**File locks.** `File::try_lock` takes a shared or exclusive lock without waiting:
- **Unix:** through the standard library's `flock`-based locks, whose locks belong to the open
  file description, as D77 asks.
- **Windows:** our own `LockFileEx` on one byte at offset 2^62, past any data. The standard
  library's whole-file lock is mandatory on Windows: it would block reads through the second,
  overlapped handle the I/O completion port uses (D69), and backup tools.
- **Simulator:** shared and exclusive counts per file, released when a handle drops or the
  machine crashes.

A handle takes one lock for its life, and a writer opens with an exclusive lock, so a second
writer anywhere is refused with "locked".

**File identity.** A new file's 16-byte identity comes from the standard library's randomly
seeded hasher (`RandomState`), which draws on the operating system's randomness. That needs no
cryptography: the identity only has to differ between files, not resist guessing.
