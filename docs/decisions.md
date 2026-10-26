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
The simulator checks every operation with four torn-write seeds each. Writes to free pages are
refused. Overwriting a page of the committed state in place isn't undone by a crash: the log
(item 4) makes that safe, by logging each change before its page is written. The cost is that a
commit with no free page grows the file by the pages its list needs. The log and checkpoints
(items 4 and 7) replace this with logged allocation.

**A failed write or sync** stops the file taking changes until it is opened again (D59).

**Opening checks before trusting the file:**
- the file's real length against `page_count`;
- the free list: every entry in range and listed once, no loops, and the count matches.

A damaged list refuses the open with a readable error. A seeded test rewrites list pages with
valid checksums and checks this.

**File locks.** `File::try_lock` takes a shared or exclusive lock without waiting:
- **Unix, Linux included:** through the standard library's `flock`-based locks. This refines
  D77, which named open-file-description locks for Linux. Both kinds belong to the open file
  description, and `flock` is what the standard library provides.
- **Windows:** our own `LockFileEx` on one byte at offset 2^62, past any data. The standard
  library's whole-file lock is mandatory on Windows: it would block reads through the second,
  overlapped handle the I/O completion port uses (D69), and backup tools.
- **Simulator:** shared and exclusive counts per file, released when a handle drops or the
  machine crashes.

A handle takes one lock for its life, and a writer opens with an exclusive lock, so a second
writer anywhere is refused with "locked". Only on WASI, which has no file locks, does the file
open unlocked. Anywhere else, a refused or unsupported lock refuses the open. That includes
Solaris, where the standard library has no file locks yet: there a database can't be opened
until the platform layer gets its own `fcntl` locks.

**File identity.** A new file's 16-byte identity comes from the standard library's randomly
seeded hasher (`RandomState`), which draws on the operating system's randomness. That needs no
cryptography: the identity only has to differ between files, not resist guessing. On targets
without operating-system randomness the hasher's keys are predictable. Identities there may
repeat between files, which weakens only the "page from another database" check, never
correctness.

**Review fixes before merge:**
- A crafted generation of 2^64 − 1 refuses the commit instead of wrapping around and losing
  every later commit.
- A commit checks the new superblock before writing anything.
- Free-list entries naming the catalog root or a log segment are refused.

## D79 The page cache: one region of frames, clock eviction, latched page handles and no-steal — Decided by research (build loop, 2026-10-26); refines D77 and D78
`eggdb-storage`'s `PageCache` is the buffer manager over the page file (D78). Every structure
of the engine reads and changes pages through it.

**Memory: one region, allocated once.** The cache takes its frames, one page each, as a single
`MemoryRegion` from the platform layer's `Memory` when it is created, and never allocates
again on a page read. The size is set at creation, in pages or bytes. The region comes first,
so a size the memory can't hold fails there. The page table and the per-frame state follow,
reserved fallibly, so a failure is an out-of-memory error rather than an abort.
- **Cost at creation.** The region is lazily zeroed by the operating system, so its pages cost
  nothing until they are used. The side tables, about 60 to 95 bytes per frame, are written
  at creation. That is 16 to 24 MB for a 1 GiB cache of 4 KiB pages. Making them lazy too is
  left until startup is measured (D14).
- One region can move to huge pages later, and a hit is one index calculation.
- The alternative, one aligned buffer per frame behind a `RefCell`, needed no `unsafe`, but it
  touched every frame at creation and ruled out huge pages. It stays the fallback behind the
  same API.
- The `unsafe` code is confined to one private module: a pointer to the region, taken once,
  and slices built from it only under the frame's latch.

**Page table:** our own open-addressing table with linear probing, sized at twice the frames
rounded up to a power of two, with Fibonacci hashing and backward-shift deletion. It never
allocates after creation.

**Eviction: a clock (second chance).** The hand skips pinned frames and changed frames, and
clears the reference bit of the others before taking one on its next pass. After two full
sweeps without a frame, the request fails with `CacheFull`, which counts the pinned and the
changed frames and says what to do.

**Handles and latches.** `read` returns a `PageRef` and `write` and `allocate` return a
`PageMut`. Each frame's latch counts readers or marks one writer. A conflicting request fails
with `PageBorrowed`: it never waits and never panics. Several pages can be pinned at once,
because these methods take the cache by shared reference. `commit` and `rollback` take it
exclusively, so the borrow checker guarantees no handle outlives them.

**No-steal.** A changed page is never evicted. `commit` first checks everything that can
refuse it without writing: the update and the next superblock (a new no-I/O prepare step of
`PageFile::commit`), every changed page still in use, and no changed page still held by a
leaked handle. Only then does it write the changed pages in page order through
`PageFile::write_page`, and finish the page file's commit. A refused commit writes nothing,
leaves the changes in memory to commit again or roll back, and no longer stops the file taking
changes. Only a failed write or sync does that. `rollback` drops them and
calls the new `PageFile::rollback`, which undoes the allocations and frees since the last
commit. Every cached page is in use: reading a free page is refused, and freeing a page drops
it from memory with any change to it. Pages are verified by checksum when read in. A failure
leaves the frame empty and returns the error, naming the page.

**Snapshot readers (D77) will fit here.** `write` is the one place where a committed page first
becomes changed. That is where the writer will copy the committed image into the bounded
before-image store. Readers on other threads will read from that store or straight from the
file, which under no-steal always holds the committed image. They will need a `Sync` store and
page file, not shared frames. Concurrent access to frames later turns the latch counts into
atomic state words (issue #30). Once the log exists (item 4), changed pages whose commit record
is durable may be written and evicted before a checkpoint.

**Statistics:** frames, cached pages, changed pages, hits, misses, evictions and pages written,
for `eggdb stats` (item 13).

## D80 Benchmarks: a harness of our own, counts gated exactly, times gated as ratios — Decided by research (build loop, 2026-10-26); refines D7, partly addresses issue #28
`crates/eggdb-bench` is a plain binary that uses only the standard library and is never
published. `cargo run --release -p eggdb-bench` runs every benchmark and checks it against
`benchmarks/baseline.txt`. Each line of the baseline is `<system> <benchmark> <metric>
<value>`.
- **Counts gate exactly**: file operations per operation (from the simulator), evictions and
  heap allocations per operation (from a counting allocator in the binary). They are the same
  on every machine, so their lines are under `any`. A count that differs from its line in
  either direction fails, and so does a missing count line. A count that falls, such as
  evictions per miss, can mean the benchmark stopped measuring what it should, so a lower
  count is updated deliberately, never accepted silently.
- **Times gate only as ratios**: the hot path against a reference workload of the same shape,
  run in the same process. Fifteen samples of at least 20 ms each alternate between the two,
  and the gate is the median ratio. It fails more than 30% above the line for this system,
  after one retry of that benchmark. The page-cache hit's reference is a standard `HashMap`
  lookup plus a byte read from an array of page-sized frames. A reference made only of
  CRC-32C would track the processor's CRC instructions, not memory and hashing. A missing
  ratio line only warns and prints the line to add, so a new system's line can be committed
  from its first CI run.
- **Nanoseconds per operation are reported**, never gated. This covers the cache miss, which is
  dominated by a system call that a CPU-bound reference doesn't track.
- `--update` rewrites this system's lines and the shared count lines, and is refused when `CI`
  is set. A pull request that raises a gated line says why (D7).
- **CI:** a `bench` job on Linux runs it in parallel with the others. It is not a required
  check and has no path filter, since it takes under a minute. Results go to the job summary.

Adding a benchmark means writing a function that returns its metrics, listing it in
`BENCHMARKS`, and committing its lines. The comparison against SQLite and the size, startup and
memory baselines are v0.2.0 item 17. Issue #28's ports of M1's platform probes are still open.

## D81 The write-ahead log: whole-page images in segments inside the file, redo on open, three durability modes — Decided by research (build loop, 2026-10-26); refines D77, D78 and D79
`eggdb-storage`'s log is the chain of segments [format.md](format.md) specifies, inside the
database file. Every commit goes through it, which makes overwriting committed pages in place
crash-safe.

**What a commit logs.** A whole image of every page it writes, the new free-list pages
included, then a commit record carrying the superblock fields a commit changes: the page count,
the free list, the catalog root, the clock's high-water mark and the feature flags. Only after
the log is durable are the pages written in place, without a sync.
- Whole images need no page-LSN comparison on replay, and repair torn pages. The B+tree's
  smaller records (items 5 and 6) can come later behind the same framing.
- Carrying the superblock fields in the commit record means a commit costs one sync, where
  D78's commit cost two. The header slots are written only when the log is created, and later
  by checkpoints (item 7).
- The free list is rewritten, and logged, only when a commit allocated or freed a page.

**Opening.** Replay runs from `checkpoint_lsn`: committed images are written in place, the last
commit record's fields replace the header's, and the free list is checked after that. A
read-only open serves those pages from the log instead. Opening for writing first sets the
file to the replayed page count and always syncs, so no page is written in place from log
bytes that are only in the operating system's cache, no later batch record claims durability
for them, and stale records past the end are gone. The replayed feature flags are checked
before anything is written.

**Records are bound to their position.** A record's checksum seed mixes in the file's
identity, the segment's sequence and the record's own LSN, so a misdirected or duplicated
block write can't replay an old image after a newer one. A reader checks a record's type and
the length that type requires before checksumming it, and the corruption check looks only at
blocks that start with a batch record, so scanning costs at most one pass over the log.

**Decided by research** (a research subagent, recorded in PR 35):
1. *Torn tail or corruption.* Each flush starts with a batch record carrying the LSN known
   durable when it was written. A bad record is corruption only when a later batch in its
   segment proves it had been synced. The naive "any valid record after it" rule calls an
   ordinary torn multi-block flush corruption, and "stop at the first bad record" silently
   drops durable commits. Damage in the very last synced flush still looks like a torn tail,
   as in PostgreSQL. Damage near a segment's end, proven only by a later segment, is also read
   as a torn tail, since the link to that segment is past the damage.
2. *Resuming after replay.* Appends continue at the block after the last commit. If anything
   after it in that segment is not zero, the next flush links to a fresh segment with a higher
   sequence, so a record of a flush that never completed can never be read as part of the log.
   Rejected: a fresh segment on every open (wastes a segment per open), and zeroing the rest
   (an extra sync and rewrites).
3. *When the log is created.* At the first change, not at creation, so a file that is never
   changed stays 8 KiB (D6, D14). No feature flag is set for it, because no release before this
   one wrote files: v0.1.0 only prints its version, and format.md lets the format change until
   v0.2.0 ships. A file PR 34's code wrote, with no log, opens unchanged and gets its log at its
   first change; a test builds such a file and checks it.
4. *Durability `Normal`.* A commit writes its records without a sync and doesn't write its
   pages in place, because a page reaching the disk before its record could be torn with
   nothing to rebuild it from. The file keeps a map from each such page to its image in the
   log, reads go there, and the pages are written in place after the next sync: an explicit
   `sync`, a change of mode, or once 1 MiB of log or 1024 pages wait.
5. *Segment size.* A record never spans segments, so a segment must hold one page image: at
   least one page plus 8 KiB once the file has a log. The default stays 64 KiB of pages, but at
   least two pages, so 64 KiB pages get 128 KiB segments. A smaller value in a file without a
   log is replaced when the log is created.
6. *A replayed page count the file can't prove* (raised by the security review of PR 35, where
   a crafted 72 KiB file opened for writing grew to 256 TiB). A commit record's page count is
   accepted only up to the file's real length in pages plus the number of committed page
   images, and never below the header's; past that the open fails with "the log is damaged".
   Every page a commit adds is logged as an image, and each image lies inside the real file, so
   a valid log can claim at most that many pages past the end, and a file opened at most
   doubles. Refusing beats clamping, because a clamp could leave the catalog root or the free
   list pointing past the end. A link record's page count must be exactly the end of the
   segment it names. A commit that adds more pages than it logs images (pages allocated and
   never written, or allocated and freed in the same transaction) syncs its new size before
   writing its records, in every mode. `Full` is no exception: a power loss before its log sync
   can keep the commit record, which may lie in an existing segment, and lose the size change,
   and the open would then refuse the file instead of falling back to the previous commit (found
   by the second review of PR 35).

**Durable syncs on Apple's systems.** The pinned standard library (1.99.1) implements both
`File::sync_data` and `File::sync_all` with `fcntl(F_FULLFSYNC)` on Apple targets, as its
source shows, so the platform layer needs nothing new.

**Group commit.** A sync covers every commit written before it, and syncing a log already
durable to a given LSN does nothing. With v0.2.0's one writer, commits under `Normal` share the
next `sync`. That "sync up to this LSN" step is where a leader will sync for several waiting
writers later.

**Left for item 7:** checkpoints that write the superblock and move `log_head_page`,
recycling segments through `recycled_segments`, and truncating the log. Until then the log
grows by one segment per 64 KiB of commits, and opening replays it all. Recycled segments will
also need their stale bytes handled, since cutting the file only removes stale bytes past the
end. A failed sync stops the file taking changes (D59).

## D82 The B+tree: slotted pages with fence keys and prefix truncation, a fixed root, and pages checked once per read from the file — Decided by research (build loop, 2026-10-26); refines D77 and D79
`eggdb-storage`'s `BTree` is v0.2.0's index (D77), over the page cache (D79), with its changes
committed through the log (D81). [format.md](format.md) "B+tree pages" specifies the pages byte
for byte.

**Pages.** Leaves (type 3) and inner pages (type 2) share one slotted layout: a 32-byte tree
header, the fence keys, a sorted array of two-byte slots searched by binary search, and cells
packed from the end of the page. Each page stores its low and high fence keys, and their
common prefix only once; its keys are stored without that prefix. Leaf splits use the
shortest separator between the halves. A `level` byte (0 for leaves) makes every child exactly
one level below its parent, which bounds every walk to 48 steps and rules out cycles without
remembering visited pages. Small key-head hints in the slots, as LeanStore and Umbra use, are
left out until a benchmark shows binary search is the cost.

**Decided by research** (a research subagent, recorded in PR 36):
1. *Size limits.* A key is at most an eighth of a page (512 bytes with 4 KiB pages, LMDB's
   default too), and a key and value together at most `3 × page_size / 16 − 18` bytes, so four
   of the largest entries plus two of the longest fence keys fill a leaf exactly. That follows
   the common rule of at least four entries per page (SQLite's local payload), and stays
   between PostgreSQL's third of a page and InnoDB's limits. A sixteenth of a page is too small
   for composite SQL keys, and a quarter leaves no room for fence keys. Keys stay inside the
   tree after item 11; only values move to overflow pages, so the value limit becomes a spill
   threshold. Keys up to 512 bytes work with every page size.
2. *A fixed root.* A root split moves the root's entries into two new pages, and the root
   becomes an inner page one level up, as in SQLite and InnoDB. The catalog is never rewritten
   on a split, and latch coupling (M3) never chases a moving root.
3. *Checking pages.* A page is checked in full (layout, slots, non-overlapping cells that
   account for every byte, key order within the fence keys, limits) when the cache reads it
   from the file, not on every use: the cache keeps a "contents checked" bit per frame that it
   clears on every read-in, as SQLite checks a page once when loading it. Every step down the
   tree still checks the child's level and that the key lies within its fence keys, cheaply,
   and `BTree::check` (for `eggdb check`) also checks each child's fence keys against its
   parent's bounds. Siblings' ranges never overlap and are never empty, so that check also
   refuses a page reached twice, without a set of visited pages. When frames are shared between threads
   (issue #30) the bit becomes part of the atomic state word.
4. *Split points.* A split of the rightmost leaf after an insert at its end keeps every old
   entry on the left, and of the leftmost leaf after an insert at its start keeps them all on
   the right, so sequential and reverse loads fill pages (SQLite's quick balance; PostgreSQL's
   nbtree splits the rightmost page unevenly too). Other splits divide the bytes evenly, and
   fall back to any point where both halves fit, which always exists with these limits. An
   inner split's fallback may leave a page with no separators and only its rightmost child,
   which the format allows. A
   fill factor below 100% for sequential loads waits for a benchmark that needs it.
5. *Prefix truncation now.* It changes the page layout, so it is built before v0.2.0 ships
   rather than behind a layout version later.

**Errors.** Over-limit keys and entries are refused before anything changes. Any other error
during a put, such as a full cache in the middle of a split, can leave the transaction half
changed, and the caller rolls it back; v0.2.0's transactions (item 9) do that for it.

**Left for item 6:** delete, merges, and cursors and range scans. Removing a slot and counting
its cell as `garbage` already exists for replacing values, and the fence keys let a cursor move
between leaves through their parents, so leaves have no sibling pointers.

## D83 B+tree delete merges under-filled pages only when they fit, and cursors re-descend by fence key holding no page between calls — Decided by research (build loop, 2026-10-26); refines D82
`BTree::delete`, `BTree::scan` and `BTree::scan_prefix` complete v0.2.0's item 6 (D76) over the
pages D82 specifies. The page layout does not change; [format.md](format.md) "B+tree pages"
states the rules a reader can rely on.

**Delete.** A delete finds the key, returns `false` and changes nothing when it is absent, and
otherwise removes its slot, counting the cell as `garbage` as a replaced value does. A key longer
than the key limit is absent by definition.

**Decided by research** (a research subagent, recorded in PR 37):
1. *Merge only, never borrow.* A page left with less than a quarter of the page in use is
   merged with an adjacent page under the same parent, the left one first and then the right,
   when the merged page fits. The merged size is computed from the two pages before anything
   changes, with the prefix of the joined fence keys, which is shorter than either page's when
   the joined range is wider, so keys can grow in a merge. The merged entries go into the right
   page, the parent loses one separator, and the left page goes back to the free list. A merged
   page still under a quarter full is merged again with its new neighbours, and the parent is
   checked the same way, up to the root. Borrowing entries from a full neighbour would change
   the parent's separator, which can be longer than the old one and split the parent in the
   middle of a delete. Without it a delete never allocates or splits a page, so it cannot fail
   for lack of space, and the code stays small. InnoDB merges the same way (its
   `MERGE_THRESHOLD`); PostgreSQL's nbtree only removes empty pages, which reclaims less.
2. *A quarter.* A split leaves two pages about half full, so a threshold of a half would merge
   them again after a few deletes and split them after a few puts. A quarter gives hysteresis;
   SQLite rebalances below about a third. A page that cannot merge, such as an empty leaf
   beside a full one whose keys share a long prefix, stays as it is: the format allows pages
   with no entries.
3. *Root collapse.* A root left as an inner page with no separators takes over its only
   child's entries, re-encoded under the root's own page number, and the child is freed; this
   repeats while it applies. The root's page number never changes (D82), and `BTree::check`
   now refuses an inner root without a separator.
4. *Cursors hold no page between calls.* A `Cursor` copies the entries in range of one leaf at
   a time into a buffer it reuses, so steady scanning allocates nothing, and pins no page
   between calls. A pinned leaf would make a put or delete on it from the same thread fail with
   `PageBorrowed`, and would hold a frame of the fixed cache budget; with no sibling pointers a
   pin saves nothing across leaves anyway. Changes made between calls are seen in leaves the
   cursor has not read yet; the leaf it is reading stays as it was copied. Snapshot reads come
   with transactions (item 9).
5. *Moving between leaves.* Going up, the cursor looks up the leaf holding the current leaf's
   high fence key from the root, and takes its entries at or above that key. Going down, it
   looks up the leaf holding the keys just below the current leaf's low fence key: each inner
   page picks the child whose range ends at or above that key, and each page checks that the key
   lies above its low fence key and at or below its high one. A scan with no upper bound going
   down follows the rightmost children and checks that none has a high fence key. Every leaf
   reached lies strictly past the last one, so a damaged file can make a cursor fail but never
   loop, and every page is checked as for `get`.

**Cost.** One descent per leaf: three page reads per leaf of a three-level tree, 3,993 page
reads per 100,000 entries in the `btree.scan` benchmark, and 16 heap allocations for a whole
scan.
A delete of every key of a 100,000-entry tree in random order leaves the single root page.
Holding the leaf's path (as LMDB's cursor stack does) to save the descents waits for a benchmark
that shows them to be the cost.

## D84 Checkpoints at the log's append position, recycled segments, and segment sequences never used twice — Decided by research (build loop, 2026-10-26); refines D77 and D81
`PageFile::checkpoint` and `PageCache::checkpoint` complete v0.2.0's item 7 (D76): recovery
replays only the log written since the last checkpoint, and the log reuses its segments
instead of growing the file. [format.md](format.md) "Checkpoints" and "Segment sequences"
specify it.

**A checkpoint** syncs the log, writes the pages that durability `Normal` left only in the log
to their places, syncs, and writes the next superblock with `checkpoint_lsn` at the log's
current append position and `log_head_page` at the current segment, then syncs. It writes no
log record and no new segment. Every earlier segment of the log, and every unused recycled one,
becomes reusable: the seven with the lowest page numbers go to `recycled_segments`, and any
others join the free list; a checkpoint that frees segments that way writes its superblock to
both header slots. The pages in the page cache stay valid, since a checkpoint writes committed
pages unchanged. Changes not yet committed refuse it with `UncommittedChanges`, and a commit
whose catalog root or free-list head would be a page of the log is refused before anything is
written (a later checkpoint could not write such a header).

**Automatic checkpoints.** After a commit, a checkpoint runs once the log since the last one
reaches `auto_checkpoint_bytes` (1 MiB by default, `None` turns it off) or goes through more
segments than `recycled_segments` holds. The second rule keeps the file from growing in steady
state: with seven recycled slots, a log that never needs an eighth segment always finds one.

**Decided by research** (a research subagent, recorded in this PR):
1. *Sequence numbers are never used twice.* Recycling exposed a hazard the old rule ("one more
   than the previous segment's") could not handle: a flush cut off by a crash can leave its
   records in a recycled segment, and after recovery the next writer gives the same segment
   the same sequence and LSN, so if its link survives and its own header and records are torn
   away, recovery would read the old records, a stale commit record included, as new. A test
   builds exactly that file. The fix: a link names the next segment's sequence, which must be
   above the previous one's, and the header's new `log_sequence_reserved` bounds every sequence
   used. A writer that opens the file starts above it, and raises it, writing the header and
   syncing, before it writes a higher sequence. Rejected: a random salt per segment (SQLite's
   WAL salts; needs an entropy source, which `no_std` targets and deterministic simulation
   lack), zeroing recycled segments before reuse (a write of a whole segment per reuse),
   deriving the next sequence from the recycled segments' headers (a torn header hides a used
   sequence), and keeping recycled segments out of use until the next checkpoint (the same
   sequence and LSN pairs come back from the same tail). The raise is lazy: the first segment
   a writer starts after opening costs one header write and sync, and checkpoints raise it for
   free.
2. *The checkpoint position* is the writer's append position, which is mid-segment and, right
   after a recovery that found a cut-off flush, not block-aligned; the writer then still links
   to a fresh segment at its next flush, so the cut-off records are never read. The format
   therefore drops the block alignment of `checkpoint_lsn` and keeps "not in the last 9 bytes
   of a block". When the last flush ended with a link, the position is the start of the next
   segment, whose header is already durable, so the segment before it is recycled.
3. *Segments beyond seven* go to the free list at the checkpoint, written in place to pages
   free in the committed state before the header, as D78's commits did before the log. The
   alternative, freeing them with a logged commit after the header, leaks them on a crash in
   between, and nothing finds such a leak.
4. *A failed automatic checkpoint* does not fail the commit, which is already durable as its
   durability promises: the commit returns `Ok`, and the file refuses every later change with
   `NeedsReopen` (D59), so a caller never retries a commit that happened. Triggering at the
   start of the next commit was rejected (it charges an unrelated commit and leaves the log
   long while idle), and so was leaving triggering to the caller.

5. *A damaged newest header slot* (raised by the security review of PR 38). Recycling
   overwrites the log an older slot would replay, so falling back to the older slot after the
   newest was damaged could open a state that never existed. Two rules close it: after a
   checkpoint the writer starts above the previous header's reservation, and the checkpoint
   reserves from the larger of the old reservation and the highest sequence used, so any
   segment reused afterwards carries a sequence the older slot never reserved and its open is
   refused; and a checkpoint that gives segments to the free list writes its superblock to both
   slots, so no slot names a log running through pages that may become data. The alternative,
   keeping released segments out of reuse until the next checkpoint, needed state the header
   has no room for. A test damages the newest slot after every third commit of a workload with
   checkpoints, reuse and freed segments, and another after freed segments are overwritten;
   each open is refused or shows the latest commit. The guarantee is therefore: a damaged
   newest slot means a replay to the newest commit or a refused open, except after two double
   faults, each a crash followed later by that damage (format.md, "Damage to the newest header
   slot"; issue #39):
   - a crash between the two superblock writes of a freeing checkpoint, then damage to the
     newest slot after the freed segments were reused;
   - a crash in the first flush into a recycled segment (its new header block lost while later
     blocks persist, no later batch record to prove durability), then damage to the newest
     slot before the next header write (the first commit after reopen writes one): the older
     slot then opens with 0 commits replayed over pages that are newer, a mixed state;
     open-for-write could close the window by writing the newest superblock to the other slot
     after a recovery that found a cut-off flush.
   Each checkpoint consumes 65,536 sequence numbers, so about 2^48 checkpoints fit before the
   reservation is exhausted and opening for writing is refused.
6. *Recovery writes nothing before the free list is checked* (raised by the general review of
   PR 38). Opening now checks the replayed free list, reading replayed pages from the log,
   before it resizes the file, syncs or writes any page in place, so a forged log naming a data
   page as the free-list head is refused with the file unchanged.

**Hostile logs.** A link must name a sequence above its segment's and at most the reservation,
and either an unused entry of `recycled_segments` (page count 0) or a new segment at or past the
header's page count that ends at the page count it names and overlaps no recycled segment. A
segment header found with a sequence above the reservation, like a link above it, refuses the
open, since only a header older than the log can produce one. Replayed images may not overwrite
the log's segments or any recycled segment. The superblock refuses overlapping log segments and
a reservation below `log_head_sequence`.

**Known limit.** If the newest slot is damaged after it was synced, the older slot is chosen
(D77) and replays its own log as far as that log links, which can run past its checkpoint up
to the newest commit. The rules in item 5 make that open either reach the newest commit or be
refused, except after the two double faults item 5 names (a crash between a freeing
checkpoint's two header writes, or a crash in the first flush into a recycled segment, each
followed later by damage to the newest slot; issue #39). That is the class of damage after a
sync that Alagappan et al. (FAST'18) describe; detection beyond it is best-effort.

**Cost.** A checkpoint under `Full` is three file operations and two syncs (benchmark
`log.checkpoint`). Opening a file after 1,000 one-page commits takes 148 file operations and
replays 8.8 MB of log at 1.8 times the cost of computing CRC-32C over it; after a checkpoint
the same open takes 10 operations and replays nothing (about 85 µs against 1.5 ms on this Mac,
`log.recovery`). Commits pay for checkpoints: per 1,000 one-page commits, `Full` syncs 1,040
times instead of 1,000 and `Normal` 60 instead of 8, since `Normal` now syncs at checkpoints as
D77 planned; file operations fall because recycled segments need no file growth.
`page_cache.commit` (100 new pages after a reopen) rises from 110 to 115 operations: the first
segment after the reopen raises the reservation (a header write and a sync) and the commit's
eight segments trigger an automatic checkpoint (a sync, a header write and a sync). A checkpoint
that frees segments to the free list costs one more header write and sync. Larger
segments or more recycled slots would space checkpoints out under `Normal`, and wait for a
benchmark that needs it.

**Review fixes** (from a review of the code merged since the last one): a leaf page's values
are no longer readable or writable as child pointers, which a short value could turn into a
panic, and a leaf split allocates its right page before overwriting the left one. After a
commit whose automatic checkpoint failed, the page cache drops its pages, so every later read
is refused with `NeedsReopen` as after a failed commit.

## D85 Crash tests across the whole storage stack: a sweep that fails every I/O operation of a B+tree workload — Decided (build loop, 2026-10-26); completes v0.2.0 item 8, refines D9
The per-layer crash tests (page file, log, checkpoints, page cache, B+tree) each prove one layer.
v0.2.0's item 8 (D76) adds a sweep over all of them together, a test-only harness in
`crates/eggdb-storage/src/crash_sweep_tests.rs`, described in [testing.md](testing.md):

- **The workload** is seeded and deterministic, and is generated before it runs, with the model
  (an ordered map) after each commit: puts with overwrites, large values and long keys, a growing
  phase that splits the root and a shrinking phase that merges pages and collapses it, rolled-back
  transactions, explicit checkpoints and syncs, and automatic checkpoints every 128 KiB of log, so
  the log wraps through its recycled segments and frees extra ones. A 24-frame cache makes pages
  leave memory and come back from the file. Each commit stamps its number in `clock_high_water`,
  so the recovered commit is known exactly.
- **Each I/O operation is failed in turn** (`fail_operation`), and the run ends with a reopen and no
  power loss, or a power loss that drops unsynced writes or tears them. The reopened tree must
  pass `BTree::check` and scan, forward and in reverse, to exactly one commit's contents.
- **What it asserts is what format.md promises, and nothing weaker:** at most the last commit
  attempted; at least every acknowledged commit under `Full`, and in every mode when there was no
  power loss; under `Normal`, every commit acknowledged before the last successful sync; under
  `Off` after a power loss nothing beyond no panic, since the format promises nothing there. A
  failed commit, checkpoint or sync leaves the handle refusing commits with `NeedsReopen` and
  syncs with an error (D59). Every injected fault must come back as an error.
- **Double crashes and continuing.** From a sample of crash points, every operation of the
  following recovery is failed in turn and followed by a torn power loss, and the next open must
  recover the same commit; and more transactions, a checkpoint and a commit run on the recovered
  file before a last power loss, which must keep that commit.
- **Size.** CI gives every operation one of the four endings in rotation (about 15 s of a debug
  build for 2,478 crash points and 4,331 crashes during recovery); `EGGDB_CRASH_SWEEP=1` gives every
  operation all six endings (torn seeds 0 to 3): 14,868 crash points and 34,134 crashes during
  recovery, about 100 s of a debug build on an Apple M-series Mac. An environment variable rather
  than a feature keeps the exhaustive run a local command with no build change.

**What it does not prove:** `Off` after a power loss, damage to bytes after they were synced
(format.md's limits, issue #39), several writers and snapshot readers (item 9), and real disks:
the simulator tears unsynced writes into 512-byte pieces, which is not every behavior of real
hardware. **Result:** the sweep found no lost acknowledged commit, no mix of two commits, no
failed check, no panic and no swallowed error in the stack as merged through D84.

## D86 Transactions: one writer and snapshot readers over the page cache, before-images in memory, hybrid-clock commit stamps — Decided by research (build loop, 2026-10-26); completes v0.2.0 item 9, refines D77, D79 and D82
`eggdb-storage`'s `Store` owns the page cache (D79) and hands out transactions:
`begin_write` gives the one `WriteTransaction`, `begin_read` any number of `ReadTransaction`s.
The file format does not change.

**Snapshots by before-images (D77).** The cache keeps a store of committed page images in memory,
tagged with the commit that superseded them. A reader registers the number of the last commit
when it begins (an in-memory count, not the stamp), and reads a page from the image with the
smallest tag above that number, or from the cache when there is none.
- **When an image is kept.** Only while a reader is registered, so a store without readers pays
  nothing. The image is copied when the writer first changes a committed page in a transaction
  (`PageCache::write`, where D79 said it would go), and when it frees one (from the frame, or from
  the file when the page is not cached). Pages the transaction allocated are never visible to a
  reader, so they need none. An image is skipped when the page's newest kept image shows it last
  changed after every live reader began: no reader can see the version it would hold. So a reader
  that lives across many commits of one page keeps one image of it, not one per commit.
- **Freed pages are reused at once.** A page a reader may still need is kept as an image when it is
  freed, so the free list hands it out as before. That keeps the change inside the cache. The
  alternative, holding freed pages out of the free list until no reader can see them, needs new
  state in the page file and its free-list rules, for no gain while images are in memory anyway.
- **A reader that begins inside a write transaction** copies, from the file, the committed image
  of every page the transaction changed or freed before any reader existed. Under no-steal the
  file, or the log for `Normal`'s unwritten pages, still holds that image.
- **Checkpoints, recycled segments and reused pages** never touch what a reader sees: readers read
  committed pages, which a checkpoint writes unchanged, or images, which never reach the file.
- **Rollback** drops the images of the transaction. A failed commit drops them all, and every
  reader then gets `NeedsReopen`, as the cache does (D59).
- **Images are dropped** as soon as no registered reader is older than their tag.

**Decided by research** (a research subagent, recorded in PR 41):
1. *Threading.* The store is `Sync`: its state sits behind one `std::sync::Mutex`, and each
   operation of a transaction (a get, a cursor step, a put, a commit) takes it for that operation
   only. Readers on other threads never wait for a write transaction to end, and the writer never
   waits for readers. A commit holds the store while it writes and syncs, which readers wait out;
   shared frames with atomic state words (issue #30) remove that later. A single-threaded store
   was rejected because readers could never overlap the writer in time, and a read-write lock
   gains nothing, since cache reads change the cache's state (reference bits, loads, counters).
   A poisoned lock is reported as `NeedsReopen`.
2. *A second writer* fails at once with `WriterActive`, like the cache's `PageBorrowed`, rather
   than waiting: waiting would deadlock a thread that already holds the writer, and a timed wait
   can be built on top later (item 10).
3. *Snapshot memory.* The images may take as many pages as the page cache holds, by default, so a
   transaction's own images always fit (no-steal bounds a transaction by the cache);
   `Store::set_snapshot_memory` changes it, in the cache's units. When the writer needs more, the
   images of the oldest tag are dropped, and every reader older than that tag fails with
   `SnapshotTooOld` from its next read on, as Oracle's "snapshot too old" does; the writer is never
   held back. Unbounded memory was rejected, since one forgotten reader would grow it without
   limit, and so was LMDB's approach of keeping old pages in the file, which grows the file.
4. *Commit stamps.* A new stamp is `max(wall clock stamp, last stamp + 1)`, the wall clock in
   microseconds since 2026-01-01 with a counter of 0 (format.md, "Hybrid-clock stamps"). More than
   4,096 commits in a microsecond carry into the microseconds, a clock set back or before 2026
   counts up from the last stamp, and the stamp after `2^64 − 1` is refused with
   `ClockExhausted` rather than wrapping. There is no limit on how far ahead of the last stamp the
   wall clock may jump: such a limit bounds the uncertainty between nodes, and a single file has
   no peers; it comes with replication (D18). A reader's stamp is the stamp of the last commit it
   sees. A commit writes its stamp as the commit record's stamp and as `clock_high_water`, so the
   stamp survives a reopen with no extra write; a failed commit may use up a stamp.
5. *A change that fails half-way* (a put whose split finds the cache full, a merge that fails)
   rolls the whole transaction back at once and ends it: later calls fail with
   `TransactionFailed`, and the store takes a new writer straight away. That is LMDB's rule after
   an error; rolling back only the failed statement needs savepoints, which belong with SQL. Errors
   raised before anything changes, `KeyTooLarge` and `EntryTooLarge`, leave the transaction open,
   and reads never end it. A commit that fails rolls back too, except after a failed write or sync
   (D59).
6. *Names.* `Store`, `ReadTransaction`, `WriteTransaction`, `CommitStamp`, in the codebase's
   unabbreviated style; `Database` stays for item 10's embedded API.

**Follow-ups from items 6 to 8, taken here:**
- `PageCache::write` clears the "contents checked" bit (D82), so bytes written through a page
  handle are checked again before the tree trusts them; the tree's own changes go through a
  crate-private write that keeps the bit.
- A cursor checks that each page's fence keys equal the bounds its parent gives it, as
  `BTree::check` does, so a damaged leaf whose fence keys reach past its parent's makes a scan
  fail instead of skipping entries. The comparison runs byte by byte: slice equality calls
  `memcmp` per piece, which cost a reverse scan a quarter of its time for keys this short.
- `PageFile::prepare_commit` keeps the free list's head and count when the list did not change,
  whatever the update set them to, as `PageFile::commit` documents.
- Not taken: issue #39 (writing the newest superblock to the other header slot after a recovery
  that found a cut-off flush). Transactions do not touch the header write path, and that change
  needs its own crash tests and security review (D35).

**Cost.** A read transaction's begin, point get and end costs about what a B+tree get through the
cache does (`transaction.read`, ratio 1.0, 0 allocations); a one-put write transaction costs 1.035
times a put and commit through the cache (`transaction.write`, same 3,203 file operations and
1,040 syncs per 1,000 commits). The B+tree and cache benchmarks are unchanged within their noise.

**Left for later, and known limits:**
- A reader whose snapshot needs a page while every frame is pinned or changed fails with
  `CacheFull` like any read; readers of an older snapshot could read straight from the file
  instead. A cursor's descent pins a parent and its child at once (two frames, for the fence
  check), so snapshot scans on a tiny cache reach `CacheFull` a little sooner.
- Commit stamps are unique among durable commits only. Under `Normal` or `Off` a commit lost to a
  power loss never reached `clock_high_water`, so after the reopen its stamp can be handed out
  again when the wall clock is behind it.
- A damaged `clock_high_water` near `2^64 − 1` makes every commit fail with `ClockExhausted`;
  there is no repair tool yet (`eggdb check`, item 13).
- Dropping or beginning a transaction inside a `get_with` callback deadlocks, since the callback
  runs while the store is held; the rustdoc says so.
- Dropping a version's images walks every kept image (`BTreeMap::retain`), so expiring or pruning
  costs the number of images times the versions dropped; an index by version comes when a
  benchmark shows it.
- Transactions larger than the cache wait for M3, which also replaces the images with versions in
  the leaves.

**Review fix before merge:** a write transaction that expired snapshots (its images outgrew the
snapshot memory) and was then rolled back, by `rollback`, a drop, a failed change or a refused
commit, left the expiry mark above the last commit, so every new reader failed with
`SnapshotTooOld` until the next commit. A rollback now lowers the mark to the last commit: the
cache holds exactly that commit again, so readers of it need no images, while older readers whose
images were dropped stay expired. Readers of the last commit that had expired work again.

## D87 The embedded Rust API: `Database`, two transaction types, double-ended scans and errors with a kind — Decided by research (build loop, 2026-10-26); completes v0.2.0 item 10, refines D11, D41 and D86
The `eggdb` crate is the public face of the engine: `eggdb::Database` over `eggdb-storage`'s
`Store`, with `ReadTransaction`, `WriteTransaction`, `Scan`, `Options`, `Durability`, `Error`,
`ErrorKind`, `Stats` and `CheckReport`; `CommitStamp` is the storage crate's, re-exported. No
other storage type is part of the API, and the engine core still depends on no protocol or
platform choice: the `eggdb` crate picks the operating system's file system, memory and clock.
The guide is [embedded.md](embedded.md).

**Names** follow the business language: a database, transactions, keys and values, `get`, `put`,
`delete`, `scan`, `scan_prefix`, `check`, `stats`, `sync`, `checkpoint`, `close`. Keys and values
are anything that is bytes (`impl AsRef<[u8]>`). Option names carry their units
(`cache_bytes`, `snapshot_memory_bytes`, `auto_checkpoint_log_bytes`, `page_size_bytes`). The
transaction types keep the storage crate's names, which users never see side by side, and
`Store` stays the storage crate's word.

**The default key space.** Transactions read and change one ordered map, whose B+tree root is the
superblock's `catalog_root`, as format.md says for files before the catalog tree. A database
that never held a key has no tree: reads find nothing, scans are empty, a delete returns
`false`, and the first `put` creates the tree in its own transaction, so a rollback leaves the
file without one. The root never moves (D82), so the database remembers the tree once it is
committed and a transaction's begin costs no page read.

**Decided by research** (a research subagent, recorded in PR 42):
1. *No in-memory database in v0.2.0.* The only in-memory file system is the platform layer's
   `SimFileSystem`, a fault simulator that keeps unsynced writes for power-loss tests, and a
   `Database` generic over the file type would put a type parameter in every user's signatures
   (a default type parameter does not help `Database::open`'s inference). `Database` stays
   concrete; an in-memory file can be added behind it later without a change to its type.
2. *Closing.* `close(self)` writes a checkpoint, so the next open replays nothing, and returns
   any error; the file is closed either way. Dropping does no I/O, since a drop can't report an
   error and must not stall an exit or an unwind: no commit is lost by a drop, and the next open
   replays at most the log since the last checkpoint (about 1 MiB by default). `sync` and
   `checkpoint` are methods of their own, for a database shared through an `Arc` that is never
   closed. Rejected: a drop that syncs and checkpoints, which hides failures.
3. *A second writer waits* up to `Options::write_timeout` (5 s by default) and then fails with
   `WriterActive`; `try_begin_write` fails at once. LMDB and redb wait without a limit, and
   SQLite applications almost always set a busy timeout; failing at once only would put a retry
   loop in every threaded application, and waiting forever turns a bug into a silent hang. The
   database keeps a writer slot (a mutex and a condition variable) taken before
   `Store::begin_write`, so the store never sees a second writer. The slot records the holding
   thread, and a thread that asks again while it holds the writer is refused at once, which
   keeps D86's "no deadlock in the same thread"; a transaction moved to another thread is
   refused the same way on the thread that began it, and waits elsewhere. `checkpoint` takes the
   slot too, so a checkpoint never meets uncommitted changes.
4. *Cache size.* 16 MiB by default. No-steal (D79) makes the cache a hard bound on a write
   transaction's size, so a larger default lets bulk loads go further; the frames are reserved
   as virtual memory and become resident only as they are used, and the per-frame arrays cost
   about 200 KB at this size. The snapshot memory defaults to the cache's size (D86), so the
   worst case is twice the cache. Rejected: 64 MiB (address space on 32-bit and
   no-overcommit systems), and SQLite's 2 MiB (too small a transaction bound without spilling).
5. *Scans.* `scan(range)` takes any of Rust's range forms over byte keys, through a sealed
   `KeyRange` trait, because `RangeBounds<K>` can't infer the key type of `..`, and
   `RangeBounds<[u8]>` is not implemented for `Range<&[u8]>`. `Scan` is an iterator of owned
   `(Vec<u8>, Vec<u8>)`: the cursor copies a leaf at a time into a buffer it reuses and returns
   slices valid until its next call, which `Iterator` can't lend. `Scan::next_entry` lends them
   instead, without the two copies. `Scan` is double-ended, as `BTreeMap::range` is: each end
   gets a cursor only when it is first used, each keeps its last key in a reused buffer, and the
   scan ends where they meet. After an error it yields nothing more (`FusedIterator`), so a loop
   that skips errors can't spin. Rejected: a `ScanOrder` parameter or a `scan_reverse` method.
6. *Closures and one-step calls.* `read` and `write` take closures that return
   `eggdb::Result`, which keeps type inference working in closures that use `?`; a generic
   `E: From<Error>` needs annotations in most of them. `get`, `put` and `delete` on the database
   are one-step transactions, as SQLite's autocommit is; the docs say a loop of `put`s commits
   and syncs every time. A `delete` of an absent key commits nothing.
7. *Errors.* `Error` is opaque and `Send + Sync`: `kind()` gives a non-exhaustive `ErrorKind`,
   `Display` a message for people and models (D11) that names the file for open errors and says
   what to do, and `source()` the storage error, whose type is not part of the stable API. It
   converts into a `std::io::Error` of the closest kind. Storage errors map as: file-system
   errors to `NotFound`, `PermissionDenied`, `Locked`, `OutOfMemory` or `Io`; a missing magic
   to `NotADatabase`; a newer version or features to `Incompatible`; every other format error,
   a cut-off file and a damaged free list or page reference to `Corrupt`; a full file and an
   exhausted clock to `DatabaseFull` (the file is valid and reads still work);
   `UncommittedChanges` to `WriterActive`; a cache too small to `InvalidOption`.
8. *Options* is a by-value builder, checked when the database opens and never in its setters, and
   an option EggDB refuses fails before any file is created. `Durability` is the crate's own enum,
   with SQLite's names, so its docs talk about the database and not the page file.
9. *Collections later, without a break.* Named key spaces arrive as `tx.collection(name)` views
   with the same method names; the transactions' own methods keep meaning the default key space.
   When the catalog tree arrives, the existing tree becomes its entry under the reserved empty
   name, behind an incompatible feature flag, so older releases refuse such files. Rejected: a
   `Collection` type now, which would be API with nothing behind it.

**Storage change.** `ReadTransaction::check` checks a tree as of a snapshot: `BTree::check` now
reads through any page source, the cache or a snapshot. `Database::check` uses it, so a check
sees one commit while writers continue. It holds the store for the whole walk.

**Cost** (D80; ratios against the same operation through the raw `Store` on the same file
system, so they measure only this layer; Linux lines are the median of five CI runs):
`database.read`, a read transaction's begin, a point get returning the value and its end, 1.023
on this Mac and 1.006 on Linux, with exactly one allocation (the returned value, as for the
store); `database.write`, a one-put write transaction under `Off`, 1.009 and 0.997;
`database.open`, a read-only open of a checkpointed file with the default 16 MiB cache, 1.004
and 1.002, about 32 µs on this Mac (D14's start-up time). The write benchmark uses `Off` so that
it measures the layer and not the disk. The first version of the writer slot woke waiters on
every commit, which on Linux is a system call (the standard library's condition variable has no
waiter count), and cost `database.write` 1.078 there; the slot now counts its waiters.

**Known limit.** The lock belongs to the open file, and a child process shares its parent's open
files until it starts its own program, so while another thread starts a child, a database just
closed can stay locked for that moment (the tests met it: the cross-process lock test runs in a
binary of its own). Avoiding it needs the application to start its child processes without
inheriting the file, which the platform layer does not offer yet.

**Left for later:** a lending scan in both directions, a writer wait with a fairness order,
collections, an in-memory database, values larger than a page (item 11), and the `eggdb` crate's
publication to crates.io, which needs the owner's account (CLAUDE.md, "Releases"). Also from the
independent review: no `database.scan` benchmark yet, and `Scan` copies its last key on every
step; `snapshot_memory_bytes` is not range-checked; the comparison against SQLite is item 17.

## D88 Large values on chains of overflow pages, a 1 GiB value limit, and a check that accounts for every page — Decided by research (build loop, 2026-10-26); completes v0.2.0 item 11, refines D77, D81, D82, D84 and D87
A value too long to sit in a B+tree leaf beside its key, longer together with it than
`3 × page_size / 16 − 18` bytes (D82's entry limit, now the spill threshold), is stored whole on
a chain of overflow pages (type 4, layout 1), and its leaf cell holds the key and a 16-byte
reference: the value's length and the chain's first page. Keys stay in the leaves, with the
same limit as before. [format.md](format.md) "Overflow pages" specifies the bytes.

**Chains.** Each overflow page carries the next page, the chain's first page, its index in the
chain and the number of the value's bytes it holds; the rest of its last page is zero. A reader
walks exactly as many pages as the length implies, so a chain cut short, lengthened or bent
into a cycle fails the `first`, `index`, `bytes_here` or `next` rules instead of looping, with
no set of visited pages, and the per-page byte count catches a reference whose length is off
by less than a page. Chains are written once: a put that replaces a large value, and a delete,
free the old chain (after reading it whole and validating every page, so a crafted chain can
never make a delete free a live page), and a new value gets a new chain. Freed pages go
through the transaction system as tree pages do: to the free list at the commit, reused only
after it (D78), kept as before-images while a reader may need them (D86), all dropped by a
rollback.

**Reads.** `get` copies the value whole; `get_with` lends an inline value where it lies and
copies a large one into a buffer first. A cursor copies one leaf's entries at a time as before,
but a large value ends the batch: the next batch looks the leaf up again from the root just past
that key, so the cursor's buffer holds at most one leaf of small entries and one large value,
and a buffer grown past a megabyte is released when its batch is done. A streaming read of one
value, piece by piece, is a later item.

**Freeing is fail-safe.** A put that replaces a large value, and a delete, read the old chain
whole and check every page before freeing any of it, and do so before the new chain is written;
the freed frames also make room for the new chain, so a value that fits the cache can replace one
as large. A chain whose pages this transaction wrote is freed only by the key that wrote it (the
cache remembers each new chain's first page and key until the commit or rollback), so a crafted
reference naming another key's new chain is refused. A chain that passes its rules is well formed
for its reference; it does not prove that no other live reference names it. A reference to a
chain that breaks the rules (a wrong type, `first`, `index` or length), or to a chain another key
wrote in this transaction, therefore makes the put or delete of its key fail, with the
transaction rolled back. A shared reference, or one naming freed pages, left by earlier commits
passes them: deleting or replacing one key frees pages the other still names, and a later value
of the same length can be read through the stale reference until `check` runs, which alone finds
it. Only a damaged or crafted file holds such a reference; an ordinary workload cannot create
one.

**Hostile files (D9).** Every chain page is read through the cache, so it must pass its
checksum and lie inside the file, outside the free list and the log. Before reserving memory
for a value, a reader checks the length against 1 GiB and against the pages the file holds
(the file's real length was checked at open), then reserves fallibly, so a crafted length
fails as damage, never as an allocation the file could not back. A deterministic mutation test
(2,400 cases, about 10 s of a debug build) flips bits, rewrites every chain field and the
reference's fields with checksums fixed, swaps chain pages, changes page types and layouts,
flags inline values as references and truncates the file; every case must end in a clean
error or a passing check, with no panic, no hang and no out-of-memory error, a length past the
file must be refused before any allocation, and a passing check must mean every get and scan
works.

**Check.** `BTree::check` reads every chain whole and remembers every page it reaches, tree
and overflow pages alike, in a set of one bit per page of the file, so a page reached twice,
by two values or by a value and the tree, is refused. The new `Store::check` checks the last
commit's tree that way and then accounts for every page of the file: each data page must be
exactly one of reachable, free, a free-list page or a log page (live or recycled). A page with
two uses fails with `PageUsedTwice`; a page with none is counted as leaked, since it does no harm
but is never reused. It reads the committed free list (the free pages, plus those an open
writer has taken from it), so it runs beside an open write transaction. `Database::check`
uses it, and `CheckReport` gains `overflow_values`, `overflow_pages`, `free_pages`,
`log_pages` and `leaked_pages`. The crash sweep now requires no leaked page in every recovered
state.

**Decided by research** (a research subagent, recorded in PR 43):
1. *A linked chain, not a directory of pages.* Chains are written once, so they never hold
   stale versions; a directory page would cost an extra page for every value that just spills.
   LMDB-style contiguous runs need extent allocation the free list doesn't have. Freeing a chain
   reads it, as SQLite does; a directory would avoid that read and allow prefetching, and is
   left until a benchmark shows the read matters.
2. *The whole value goes out, with no prefix kept in the leaf.* LMDB and redb do the same;
   SQLite keeps a local prefix to fill overflow pages fully. The cost is real for values just
   over the threshold: with 4 KiB pages a value of 751 bytes to 4 KiB takes a whole overflow
   page and one more page read. A kept prefix is a later kind of reference, behind the
   reference's length bits and a feature flag. A reader accepts any length from 1 to the limit,
   not only lengths above the threshold, so a later release can change the threshold.
3. *1 GiB per value* (`BTree::MAX_VALUE_BYTES`, `Database::max_value_bytes`), stored as a `u64`
   so the limit can rise later behind a feature flag. It matches SQLite's default (10^9 bytes)
   and PostgreSQL's field limit; Redis allows 512 MB. A value is read and written whole, so it
   takes its size in memory while used, twice that as an owned scan item.
4. *A value the cache can't hold is refused up front.* No-steal (D79) keeps every page of a
   write transaction in the cache, so a value whose overflow pages and leaf need more pages
   than the cache has frames fails with `ValueExceedsCache` (`ErrorKind::TooLarge`) before
   anything changes, and the transaction stays open; with the default 16 MiB cache that is a
   value above about 15.8 MiB. A transaction that runs out of frames later, after other
   changes, still fails with `CacheFull` and rolls back (D86). The static check is
   deterministic; one against the cache's current dirty pages was rejected as racy in spirit.
5. *No feature flag and no version bump.* No release before v0.2.0 wrote a database file and
   format.md lets the format change until v0.2.0 ships, as D81 used for the log. From the
   v0.2.0 sample file on, a change to this layout needs a new layout behind a flag.
6. *Names.* `EntryTooLarge` is replaced by `ValueTooLarge` and `ValueExceedsCache`;
   `BTree::max_entry_bytes` becomes `max_inline_entry_bytes`, since it no longer limits what a
   user can store; `Database::max_entry_bytes` (never released) becomes `max_value_bytes`, and
   the inline threshold is not part of the embedded API.
7. *Whole-file accounting now*, not with the item 12 fuzzer or the item 13 CLI: format.md
   already promised it, overflow pages add new ways to leak or share a page, and the shared-chain
   check needs the same set of pages. It costs one bit per page: 32 KiB per GiB of file.

**Log segments for large commits** (found by this item's test that rewrites 100 KiB values: the
file grew by a log segment every few commits, forever). A commit's flush that runs past the
last unused recycled segment appends a segment at the end of the file, and the checkpoint after
it gives one of the older segments to the free list, so the file grew by a segment while free
pages piled up. A commit now also checkpoints after it when a flush as long as its own would
not fit in the rest of the current segment and the unused recycled ones. That changes only when
checkpoints run, not the format or recovery, and leaves the gated counts of one-page commits
unchanged: 100 KiB values rewritten 80 times keep the file at 263 pages. A commit whose flush
is longer than the seven recycled segments (about 448 KiB of log with the default segments)
still appends segments at the end of the file every time, and they pile up in the free list;
taking log segments from free space, or writing a commit's newly allocated pages in place
without logging them, is left for later (issue #44).

**Cost** (D80, this Mac; the Linux lines are the median of four CI runs):
- `large_value.put`, a one-put transaction of a 100 KiB value through `Database` under
  `Durability::Off`: about 52 µs, 9.8 times writing the value twice with two `pwrite`s, the
  floor of logging it and writing it in place (9.3 on Linux). The rest is computing CRC-32C over every page
  twice (page checksum and log record), copying the value into 26 frames and the log buffer, a
  write per page in place, and reading the old chain to free it. Gated counts on the simulator
  under `Full`: 3,299 file operations and 166 syncs per 100 puts, 26 pages per value, and a
  file of 263 pages and no leaked page after 500 puts rewriting four keys.
- `large_value.get`: about 3.1 µs, 2.4 times copying 100 KiB into a new `Vec` (1.9 on Linux),
  with exactly one allocation (the value).
- Existing hot paths: `btree.get`, `btree.put` and `btree.scan_reverse` unchanged; `btree.delete`,
  `btree.scan` and `transaction.read` measured 3 to 4% slower than main on this Mac in
  alternating runs, within the run-to-run spread (main's own `btree.scan` ranged 4.96 to 5.29);
  the large-value paths of `get` and the cursor are kept out of line. On Linux CI,
  `transaction.read` reads 1.12 to 1.19 against its line of 1.004, because its reference,
  `btree.get` through the cache, got faster there (1.63 to 1.71 against `BTreeMap`, line 1.85);
  the scans there spread as widely as on main (main's last five runs: 3.4 to 5.9).
- The crash sweep (CI) takes about 29 s of a debug build, up from about 20 s, and the
  exhaustive run (`EGGDB_CRASH_SWEEP=1`) gives 27,048 crash points and 43,992 crashes during
  recovery in 195 s.

**Review fixes before merge** (security review of PR 43): put used to write the new chain before
freeing the old one, so a crafted old reference naming the page one past the file's end matched
the new chain and freed it, and a crafted reference could free a chain another key wrote in the
same transaction; both now fail as above, with regression tests on crafted files. The mutation
test now requires a file that passed the check before a commit to pass it after. Freeing first
also fixed an overwrite at the cache's limit that `check_fits` accepted but that ran out of
frames. A chain that leads to the leaf being changed is reported as damage, not as a borrowed
page.

**API renames for the v0.2.0 notes:** `StorageError::EntryTooLarge` became `ValueTooLarge` and
`ValueExceedsCache`, `BTree::max_entry_bytes` became `max_inline_entry_bytes`, and
`Database::max_entry_bytes` became `max_value_bytes`; none was in a release.

**Left for later:** a streaming read of one value; reading a large value's pages without
setting the clock's reference bit, so one scan doesn't flush the cache; coalescing consecutive
page writes; an inline prefix for values just over the threshold; log segments from free space
for commits longer than the recycled segments; shrinking the file when its last pages are free.

## D89 A model test against an in-memory map, a seeded fuzzer for hostile files, and the older header slot made a copy when a file opens for writing — Decided by research (build loop, 2026-10-26); completes v0.2.0 item 12, refines D9, D84 and D85
Item 12 tests the whole storage engine two more ways, and closes issue #39 on the way.

**A model test** (`crates/eggdb-storage/src/model_tests.rs`). Seeded random steps drive a
`Store` on the simulated file system and compare every read with a `BTreeMap` of the committed
state: write transactions of up to forty puts and deletes with values from empty to sixteen
overflow pages and keys from empty to the longest allowed, in-transaction gets and scans,
commits, rollbacks, drops, transactions that fill the cache, a reader holding its snapshot
across a commit, gets, full, range and prefix scans in both orders, `check` with its counts,
checkpoints, syncs, reopens, read-only opens, power losses and injected I/O faults, some during
recovery. After every reopen the store must hold exactly one commit at or after the last durable
one, or the one whose commit reported a failure. The embedded API has a smaller twin on real
files (`crates/eggdb/tests/model.rs`) with every range form and scans drained from both ends.
CI runs 12 seeds of 250 steps; `EGGDB_MODEL_TEST=1` runs 256 seeds of 3,000 steps. The last long
run, in 268 s of a release build on this Mac: 373,101 commits, 90,117 rollbacks, 15,948
transactions that filled the cache, 45,950 reopens, 23,003 read-only opens, 35,097 power losses,
38,539 injected faults, 25,615 commits lost as their durability mode allows, 165,857 checks and
919,920 scans, with no difference from the model.

**A fuzzer for hostile files** (`crates/eggdb-storage/tests/hostile_files.rs`). No outside fuzzing
tool or crate (D1): a seeded generator damages five valid files with one to three of 22
mutations (raw bit flips, truncation, extension, zeroed pages and ranges, swapped and copied
pages, superblock fields with fixed checksums, tree, overflow and free-list pages and page
headers rewritten with valid page checksums, log bytes and blocks), then opens each file
read-only and for writing, reads, scans, checks, writes, commits, checkpoints and reopens.
Errors are the expected outcome. A panic, a case over a minute, a scan longer than the file can
hold, a single allocation over twice the file plus 16 MiB, or a file that passes `check` and then
fails a read, a commit's check or a reopen, fails the test with the seed and case that reproduce
it. It runs as an integration test so it can install its own global allocator, which records the
largest allocation per thread; cases run on every core. CI runs 800 cases (about 27 s on a
GitHub runner);
`EGGDB_FUZZ_CASES` and `EGGDB_FUZZ_SEED` run campaigns. The last ones, in release builds on this
Mac: 150,000 cases from seed 7 in 171 s (112,901 files opened, 60,825 passed the check) and
40,000 from the default seed in 47 s, with no failure.

What they found: nothing in the engine. The earlier layer-by-layer mutation tests (D82, D84,
D88) had already made every decoder return errors; the failures while building the fuzzer were
in the harness itself (mutations applied to a file another mutation had already truncated).

**Issue #39: the older header slot.** Two double faults, each a crash and later damage to the
newest header slot after it was synced, could open the older slot over pages newer than its log
explains (format.md, "Damage to the newest header slot"): a crash between a freeing
checkpoint's two superblock writes, and a crash in the first flush into a recycled segment.
Opening a file that has a log for writing now writes the chosen superblock, with `generation`
raised by one, to the other slot unless that slot already holds a copy (every field equal but
`generation`). The write goes out before the sync recovery already does before it writes
replayed pages in place, so it costs one write and no sync, only on a read-write open whose
other slot differs (in practice the first open after a checkpoint or a crash); read-only opens,
and so every gated benchmark count, are unchanged. A copy names exactly what the newest
superblock names, all of it synced when that superblock was, so a torn copy only damages the
older slot. New crash tests sweep every operation of a freeing checkpoint and of the first
commit into a recycled segment, reopen for writing, damage the newest slot, and require the
older slot to open the same state; without the fix the first one opens a mixed state. The first
window is now closed; the second stays open from the crash to the next open for writing.

**A benchmark finding.** With the comparison and the copy written inline in `PageFile::open`, the
gated `database.open` ratio (a read-only `Database` open against a read-only `Store` open, which
never run that code) read 1.38 to 1.39 in six CI runs on one kind of Linux runner and 1.007 on
the other, while `main` read 1.007 to 1.017 on both; a branch with only `page_file.rs` and
`format.rs` taken back from `main` read 1.017. The cause is code generation, not work: both opens
run the same steps. The check and the copy now live in two `#[inline(never)]` helpers, so
`open`'s own code is as on `main`. On the slow kind of runner the ratio then read 0.73 to 0.93
in three runs, with `Database` opens back at `main`'s 36 to 42 µs and the bench's own `Store`
open reading 45 to 57 µs: this ratio is sensitive to code layout on that kind of runner, in
either direction, and a later change to the open path may need the same care.

Decided by research (a Sonnet research subagent, 2026-10-26), since it is a cost-against-safety
choice the owner would otherwise make:
- *Which trigger?* Options were a copy whenever the older slot isn't one (as issue #39
  proposed), a copy only on narrow signals (a damaged slot, a recovery scan that stopped at
  non-zero bytes, a free list written by the newest checkpoint), or a copy deferred to the first
  commit. Answer: whenever the older slot isn't a copy. The narrow signals rest on an argument
  that they cover every state of both windows, and the scan's signal can miss a cut-off flush
  whose link record was lost; the deferred copy leaves the second window open for a session
  that opens for writing and never commits. With the write placed before the open's existing
  sync, the cost is one file operation and no sync, which D6 accepts with this reason.

Issue #44 (commits longer than the recycled segments grow the file) is not part of this item:
each of its fixes changes the log and recovery and needs its own crash tests and review.

## D90 The `eggdb` command: command first, byte arguments, formats chosen by the terminal, exit codes 0 to 5, terminal detection in the platform layer — Decided by research (build loop, 2026-10-26); completes v0.2.0 item 13, refines D6, D8, D9 and D15
Item 13 builds the `eggdb` command from Claude Fable's design, `docs/ux/cli.md` (D15), which
is the record of every choice: command first (`eggdb <command> <file> …`), files created only by
`create` or `put --create`, byte arguments (text, `0x` hex, `@file`, `@-`, `=` escape), the
`text|json|hex|raw` formats with `text` on a terminal and `json` in a pipe (`raw` for `get`), the
`check` and `stats` reports, exit codes 0 done, 1 not found, 2 usage, 3 database error,
4 damaged, 5 busy, errors as `eggdb: what: why. what to do` with a `try:` line, colour only on a
terminal with `NO_COLOR` unset and `TERM` not `dumb`, and the names `export`, `import`, `shell`,
`backup`, `compact` and `ui` reserved. The user guide is `docs/cli.md`.

**Where the code lives.** The command is the `eggdb` binary's own modules (`crates/eggdb/src/cli/`),
not the library's API: parsing, byte arguments, rendering, reports, output and the commands are
separate modules with unit tests, and `cli::run` takes the arguments, the three streams and what
the terminal is, so tests run it in the process as well as through the real binary. The parser
is hand-written (D1). Whether a stream is a terminal and whether it takes colour is a new
platform-layer module, `eggdb_platform::terminal` (D8): `std::io::IsTerminal` on every target,
the `NO_COLOR` and `TERM` rules, and on Windows the console's virtual-terminal mode turned on
through `GetConsoleMode` and `SetConsoleMode`, our own declarations, with colour only when the
console accepts it. `eggdb ui` (item 14) extends the same module with raw mode and window size.

**Safety (D9).** No input reaches a panic: arguments are parsed into values or usage errors,
`print!` is never used, so a closed pipe ends the command with exit 0 instead of a panic, and
reading `@file` or stdin stops at 1 GiB and a byte. Tests run 20,000 random argument lists
through the parser, 600 random commands in the process against a real file, a file that is not
a database and a missing one, and 150 random commands through the real binary, all of which
must exit with a designed code and never print a panic or a debug dump. The binary's tests also
cover every command, every exit code and its message, the formats a pipe gets, a 20 MiB value
through stdin, a busy file in both directions (in its own test binary, so no other test's child
process holds the file's lock), a closed pipe, a page damaged on disk and a file damaged where
opening reads it.

**Size (D6).** The release binary on macOS (AArch64) grows from 302,448 to 651,104 bytes. Until
now the binary only printed its version, so the linker dropped the whole storage engine; a build
of the same binary that only opens a database, writes, reads, scans, checks and prints its stats
is 568,176 bytes, so the engine accounts for about 266 KB and the command itself (parser,
formats, reports, help text) for about 83 KB. Startup for `eggdb stats` is under 10 ms.

Decided by research (a Sonnet research subagent, 2026-10-26), where the design could not be
built as written or was silent:
- *Ctrl-C:* keep the operating system's default (shells report 130) rather than install a
  signal handler per OS: `put` is atomic, `check` cannot stop half-way, and a handler would buy
  nothing until `eggdb ui` needs signals anyway. Output goes out in blocks of 64 KiB and more,
  which a pipe does not take atomically, so a killed `scan` can leave a partial last line;
  scripts that read a stopped scan drop a final line without a newline.
- *The damaged report's "what was checked" line:* the library reports no progress, and a B+tree
  check is not a left-to-right prefix, so the line says that every page read before the damage
  passed and the check stopped there; damage found while opening says so, with no commit. The
  library's closing advice is dropped from the `damaged:` line, which the `what to do` line
  replaces.
- *`--version`:* prints the crate version, so it reads 0.1.0 until the v0.2.0 bump.
- *Colour on Windows:* turn on the console's virtual-terminal mode and use colour only if that
  works, rather than no colour on Windows or escape codes an old console prints as text.

The smaller choices the design left open (usage error layout, `hex` and `raw` only for `get`
and `scan`, where not-found messages go per format, `@file` errors as usage errors) are listed in
`docs/ux/cli.md` section 13.

## D91 `eggdb ui` part 1: a raw terminal in the platform layer, an `eggdb-ui` crate behind a `Source` trait, a read-only browser — Decided (build loop, 2026-10-26); completes v0.2.0 item 14, refines D6, D8, D9, D15 and D90
Item 14 builds the read-only half of Claude Fable's design for `eggdb ui`, `docs/ux/ui.md`
(D15), which records the layout, keys, colours and testing contract. What the build decided
beyond the design:

**The terminal in the platform layer (D8).** `eggdb_platform::terminal::RawTerminal` takes the
terminal behind stdin and stdout and gives it back on drop, on a panic (a hook installed once)
and on `SIGTERM`, `SIGHUP`, `SIGQUIT` and a `SIGINT` sent by another process, whose handlers
only write the restore bytes, call `tcsetattr` and `_exit(128 + signal)` (the shell convention;
the design's `130 + n` was a slip). A restore marks the terminal given back only after
`tcsetattr` returns, so a signal arriving in the middle of one restores it again rather than
exiting raw. On Unix the saved `termios` is an opaque, 16-byte-aligned 256-byte buffer made raw by
`cfmakeraw` and kept in atomics for the signal handler, so no per-OS struct layout is
declared; only `TIOCGWINSZ`, `SIGWINCH` and `nfds_t` differ per OS. A resize sets an atomic
flag from `SIGWINCH`, and the input wait polls stdin in slices of at most 100 ms and checks
the flag, instead of a self-pipe. Autowrap is off while the screen is drawn, so writing the
bottom-right cell never scrolls. On Windows the console gets virtual-terminal input and output
and the UTF-8 output code page; key records are turned back into the bytes the terminal sent,
so one decoder serves every system; `Ctrl-C` and `Ctrl-Break` console events restore the
console before the default handler ends the process, like closing it. Systems without a
terminal (WASI, `no_std`) get `Unsupported`.

**Nothing in the data reaches the terminal raw (D9).** Keys and values are shown escaped:
controls as `\xNN` or `^X`, and bidi overrides and isolates (U+202A–U+202E, U+2066–U+2069),
the other invisible Unicode format characters and the line and paragraph separators as
`\u{202e}`, so a key such as `user\u{202e}gnp.exe` cannot reorder or hide what the screen
shows. The frame, the one place every cell is written, replaces any such character that gets
past the escaping with `?`, and the renderer refuses them too. Zero-width joiners and variation
selectors stay, so emoji sequences and scripts that need them still render. A paste is cut at
1 MiB and the rest of it is dropped up to its end marker (or the next pause in input, if the
marker never comes) rather than arriving as keys.

**The UI is its own crate and never sees `eggdb::Database`.** The `eggdb` binary lives in the
`eggdb` package, so a UI crate depending on `eggdb` would be a package cycle. `eggdb-ui` reads
through a `Source` trait (summary, a window of keys from a start in either direction, a value,
a count in steps) that the CLI implements on `Database` with lending scans; tests implement it
on a `BTreeMap`. The library's `Scan` gains `next_back_entry`, the lending reverse step, and
`next_key_and_value_len` and `next_back_key_and_value_len`, which lend a key and the length of
its value without reading the value: a large value's length comes from its reference in the
leaf (the storage `Cursor::next_key_and_value_len`), so listing and counting keys never reads
an overflow page. The key list and the count use them, and the count checks its 20 ms budget
after every key.

**The count runs in steps, not on a thread.** The design's worker thread is replaced by steps
of 10,000 keys that the event loop runs whenever no key is waiting. The screen is just as
honest (`1,204+ keys` until it is done), keys stay responsive, the source needs no `Send`, and
tests are deterministic (`Ui::run_until_idle`).

**Read-only in this part.** `eggdb ui` opens the file read-only, so it works beside another
reader; `e`, `a`, `d` and `c` say so in the status line. Item 15 adds editing, `--read-only`,
the check screen, the message modal, expiring toasts and the remaining scenarios.

**Tests.** The fake-terminal scenarios that apply to browsing (1 to 9, 17, 19 to 22 and the
40 × 10 run) compare plain-text golden screens in `crates/eggdb-ui/tests/screens/`
(`UPDATE_SCREENS=1` rewrites them); the decoder is fuzzed with random bytes split at random
points (never a panic, and split points never change the events, D9); the JSON printer is
fuzzed and refuses nesting deeper than 512 rather than overflowing the stack; the binary is
run with a piped stdin, which must exit 2 with `eggdb ui needs a terminal`.

**Size (D6).** The release binary grows from 651,088 to 750,688 bytes (+99,600, +15.3%) on
aarch64 macOS: the terminal UI, which the command did not have before. The library is
unchanged apart from `next_back_entry` and the key-and-length steps.

## D92 `eggdb ui` part 2: read-write by default, one confirmed commit per change, the check on a worker — Decided (build loop, 2026-10-27); completes v0.2.0 item 15, refines D9, D15 and D91
Item 15 builds the editing half of Claude Fable's design for `eggdb ui`, `docs/ux/ui.md` (D15).
The details the design left open are listed in that file's "Decided while building part 2";
the decisions that shape the code:

**Read-write by default, `--read-only` to refuse writes.** `eggdb ui <file>` opens the file for
writing (never creating it), so it excludes every other open of the file meanwhile, as `put`
does; `--read-only` opens it read-only, which refuses `e`, `a` and `d` with a toast and works
beside another reader. A `Locked` open for writing names `--read-only` as the way to browse.

**No transaction outlives a key press.** An editor holds only text. Confirming shows a modal
with the key and the sizes; `Enter` (for a delete, `d` again) calls `Source::write`, which is
the whole write transaction: `try_begin_write`, `put` or `delete`, `commit`, durable before the
screen redraws. A cancelled edit or a closed modal never reaches the database. The screen then
re-reads its window from the same anchor, keeps or moves the selection as the design says, and
adjusts a finished key count by one (a running count starts over).

**Failures by kind.** `Source` errors carry the engine's `ErrorKind` (a `SourceError`), shown
as `Error (<Kind>): <message>` with a `Try:` line where the UI knows the fix. `WriterActive`
opens a modal whose `r` waits up to the write timeout with `begin_write` on the UI thread, after
the waiting message is drawn; no worker thread, because inside one process the only other
writer is the embedding program, and the wait is bounded by the timeout. `TooLarge` and
`CacheFull` keep the editor; `NeedsReopen` reopens the file through `Source::reopen` and keeps
the edit; `Corrupt` offers the check.

**The check runs on a worker thread.** `Database::check` reads every page and cannot run in
steps, so the CLI's source shares the database through an `Arc` and runs it on a thread;
the UI polls `Source::check_outcome` every 200 ms and calls nothing else on the database
until it ends, so the screen keeps drawing and no read waits behind the check. Quitting during
a check leaves the thread to the process exit and closes the file without a checkpoint, which
loses nothing (D87). The key count, by contrast, stays in steps on the UI thread (D91).

**Bounded editing (D9).** The editor holds at most 4 MiB of text, so a value above that (or
one the viewer did not load, above 64 MiB) is refused at `e` with a pointer to
`eggdb put <file> <key> @file`; typing and pasting past the bound are refused. Typed text is
the escape spelling and decodes strictly, a bad escape refused at the cursor; a paste is taken
literally, its invisible and control characters spelled as escapes, so neither path can put a
raw bidi override or control on the screen. Pasted line breaks (`\r\n` or a lone `\r`, as
terminals send them) are stored as `\n`. A paste of more than 1 MiB is never cut, however its
bytes are split across reads: the input decoder turns it into one `Event::PasteTooLarge` and
drops its bytes up to the end marker, keeping only a possible partial marker, and the UI
refuses it with a message. The escape wait (25 ms) no longer ends a paste: only its end marker
does, or 1 s with no input (`InputDecoder::end_paste`, called by the terminal), so a pause
inside a paste over ssh never turns its tail into keys, and a terminal that never sends the
marker cannot swallow keys, `Ctrl-C` included, for longer than that second. Undo and redo keep
the newest 16 MiB of changes.

**A confirmation is guarded by being seen and by quiet input.** A burst of keys (typed ahead,
or a paste in a terminal without bracketed paste, which a pty delivers in many reads) could
open a confirmation and answer it before anyone saw it. Every modal therefore takes keys only
after `Ui::presented` reports a frame that shows it (not the too-small screen nor the help
screen; a frame that hides it disarms it), followed by `Ui::input_quiet`: the run loop waits
300 ms after that frame and calls it only when no input came and no bracketed paste is open
(`Terminal::paste_open`). Keys arriving earlier are dropped and restart the wait; a deliberate
key after reading the question works, and nobody reads one faster than 300 ms. `Ui::process`,
the tests' step, treats the end of its queued events as quiet. `Ctrl-C` is the one exception,
handled before any modal: it never confirms a change; it quits at once, or, with an unsaved
edit, asks first and quits on a second `Ctrl-C`.

What this guarantees, exactly: with bracketed paste (the normal case) pasted bytes are never
keys, whatever pauses the paste has, unless the terminal passes a raw `ESC[201~` through. Without it, a paste or typed-ahead keys cannot answer a
question while they arrive less than 300 ms apart; a paste that pauses longer (a slow link
delivering it in bursts), or one carrying a raw `ESC[201~` that the terminal passes through
unsanitised, can still answer a question that appeared meanwhile. The two timeouts do not
contradict each other: 1 s is how long a bracketed paste may pause before its missing end
marker is given up, and the 300 ms window does not run at all while such a paste is open.

**Toasts and messages.** A toast clears at the next key or after 8 s (`Ui::tick_at` takes the
time so tests can expire one); a cut or multi-line toast ends with `(m)`, and `m` opens the last
message in a modal that scrolls.

**Tests.** Fake-terminal scenarios 10 to 16, 18, 23 and 24 with golden screens, plus error
modals, reopen, toast expiry, quitting with an unsaved edit, an editor surviving a terminal too
small to draw it, and editing at 40 × 10; with real files: a confirmed edit, add and delete
persist after reopening and a cancelled edit does not, a read-only session leaves the file byte
for byte unchanged, a writer held on another thread is reported and waited for, the worker
check reports a damaged file as `Corrupt`, and 200 random adds and deletes through the UI agree
with a scan of the reopened file.

**Size (D6).** The release binary grows from 750,704 to 866,912 bytes (+116,208, +15.5%) on
aarch64 macOS: the editors, modals, check screen and error texts the UI did not have. The
library is unchanged.

## D93 `eggdb export` and `eggdb import`: the eggdb-jsonl format, a checked pass before writing, batched commits and new files built aside — Decided by research (build loop, 2026-10-27); completes v0.2.0 item 16, refines D9, D13, D15 and D90
Item 16 lets a database move between files, versions and machines as text: `eggdb export <file>
[<out> | -]` writes it as JSON lines and `eggdb import <file> [<in> | -]` loads such a file. The
user guide is `docs/cli.md`, section "`export` and `import`".

**The format, eggdb-jsonl version 1.** UTF-8 text, one JSON object per line, each line ended by
`\n`:
- a header first: `{"format":"eggdb-jsonl","version":1,"eggdb":"0.2.0","page_size":4096,
  "exported_at":"2026-10-27T09:14:07.123456Z"}`; `format` and `version` are required, the other
  three informational (`page_size` is the page size a new file gets on import);
- one line per entry, in ascending byte order of the keys: `{"key":…,"value":…}`;
- a trailer last: `{"entries":N}`, the number of entry lines, so a cut file is refused.

A key or value is written as a JSON string (`"key"`, `"value"`) when its bytes are UTF-8 without
control characters other than tab, line feed and carriage return, and otherwise as lowercase hex
(`"key_hex"`, `"value_hex"`). This is the field-name tag `scan --format json` already uses, with
a stricter rule for text, so an escaped string is never more than twice its bytes. Hex rather
than base64 because the CLI speaks hex everywhere else (`0x` arguments, `--format hex`,
`key_hex`); a later version may add a base64 tag without breaking this one. Import reads both
spellings and any JSON a tool might write for them: whitespace between tokens, fields in any
order, every JSON escape including surrogate pairs, hex of either case. It refuses what is not
exactly that: an unknown or repeated field, both spellings of one field, a raw control character
or a lone surrogate in a string, invalid UTF-8 in a text field, an odd or non-hex digit, a
number with a sign, fraction, exponent or leading zero, an empty line, a second header, a
version other than 1 (above 1: "from a newer EggDB; upgrade"), a page size that is not one, a
missing or wrong trailer count, and anything after the trailer. Every refusal names its line.

**Bounded memory (D9).** Import never holds a line: a reader of our own decodes straight from a
64 KiB buffer into the key and value, so the bytes it holds are the decoded entry, and it stops
a key at 8 KiB (the longest key any page size stores) and a value at 1 GiB while decoding, before
taking more memory. A line is also capped at twice the largest entry plus 64 KiB, about 2 GiB,
so endless whitespace is refused with its line number. Export reads through one read
transaction with the lending scan, and writes each value through a 256 KiB buffer in 8 KiB
pieces of hex or escaped text; a value is held whole only because the engine reads it whole.
So the largest entry costs its own size in each direction, plus the page cache import needs to
commit it (below).

**Writing, and what a failure leaves.** One write transaction cannot hold a large import, since
a transaction keeps every page it changes in the page cache until it commits. Import therefore:
- with a regular file to read, opens it once, checks the whole export (syntax, header, trailer
  count, key order and sizes against the target's page size, and with the default
  `--on-conflict error`, every key against those already in the target) and writes nothing when
  it finds a problem, then rewinds the same handle to write; the export can then no longer stop
  the writing, but an I/O error, a full disk, a `--cache` too small for a batch or an entry,
  Ctrl-C, or another program taking the file while it is reopened with a larger cache (only
  without the checked pass) can, after some batches were committed;
- commits in batches of at most 4 MiB of keys and values and 2 MiB of pages' worth of entries
  (512 entries with 4 KiB pages, 32 with 64 KiB), with `Durability::Normal` and a sync when the
  file closes, in a 32 MiB page cache; an entry above 4 MiB is a commit of its own, and the
  cache grows to fit it (the file is reopened with a larger cache, as `put` sizes its cache to
  its value), unless `--cache` was given;
- from stdin, or a path that is not a regular file (a named pipe, `/dev/stdin`, a shell's
  `<(…)`), which can be read only once, reads in one pass, stops at a bad line after the batches
  before it and says how many entries were committed, so the same export can be imported again with
  `--on-conflict skip`; `--dry-run` checks an export without writing anything;
- with `--create` and no file at the path, builds the database as `<file>.importing`, syncs it,
  renames it into place only after the whole import succeeded and syncs the directory, so a
  failed or interrupted restore never leaves a half-filled `<file>`; a `<file>.importing` left
  by a crash is refused, never reused or removed.

**Keys in ascending order.** Import requires each key to be above the one before, which
`eggdb export` always writes. It finds a key repeated in the export with no memory beyond the
previous key, lets the checked pass see every conflict before writing, and leaves room for a
faster append path later; accepting any order could come later as an option, while going from
loose to strict would break files people already have. A key equal to the previous one follows
`--on-conflict` like a key already in the target: `error` (the default) stops, `overwrite` keeps
the later value, `skip` the earlier. The `imported` count is of keys stored, so a repeated key
counts once under `overwrite`.

**The command line.** As Claude Fable's design sketched them (`docs/ux/cli.md`, section 2): the
second argument is the export file, `-` or nothing meaning stdout for `export` and stdin for
`import`; `import` with neither a file nor a pipe is a usage error. `export` never replaces a
file (like `create`) and removes the partial file when it fails. `import` makes a file only with
`--create`, like `put --create`, with the export's page size unless `--page-size` says
otherwise; `--page-size` on an existing file is a usage error. Errors are exit 3 (an unreadable
export file is exit 2, like an unreadable `@file`); a closed pipe ends an export with exit 0.
The summary goes to stdout in `text` or `json`; an export to stdout prints its summary to stderr
only on a terminal, and both commands show a progress line on stderr, redrawn at most once a
second, only when stderr is a terminal.

**Tests.** Unit tests of the format: the exact bytes of an export, 300 random exports of random
bytes and text (quotes, backslashes, line breaks, control and C1 characters, emoji) read back
through a reader that returns one to nine bytes per call and `Interrupted` errors, values of
3 MiB crossing every buffer boundary, the other spellings import accepts, 49 refusals each with
its line and message, the key and line limits, and a seeded fuzzer that mutates valid exports
(bit flips, bytes replaced, cut, deleted, JSON tokens inserted, lines moved, noise) or makes
random bytes, through the trickling reader, and requires a refusal with a line or entries within
the limits, never a panic, an input error or a case over 5 s: 4,000 cases in CI,
`EGGDB_FUZZ_CASES` and `EGGDB_FUZZ_SEED` for campaigns (2,000,000 cases in 5.7 s of a release
build on this Mac, no failure). In-process command tests: every kind of entry (text, binary, the
empty key, an empty value, 5 MiB of binary, 3.7 MiB of text) through a file and through a pipe,
an empty database, the text and JSON reports, the three conflict policies, keys repeated and out
of order, 1,500 entries from stdin with a bad last line (1,024 committed, `check` passes), a
failed import into a new file leaving nothing, a stale `.importing` refused, 4,000-byte keys
keeping 64 KiB pages and refused with 4 KiB ones, the dry run, every usage and file error, a
closed pipe, a named pipe given as the export path (read once, no hang, on Unix), a dangling
symbolic link as the target, and 150 mutated exports imported into new and existing files (exit 0 or 3, `check`
passes, nothing left behind, an existing file unchanged when nothing was imported). Through the
real binary: the acceptance check (text, binary, empty key, empty value and a 6 MiB value put
with the CLI, exported, imported into a fresh file from the file and from a pipe, `scan`
identical, `check` passing on all three), `/dev/stdin` given as the export path, the help and
random arguments with the new words.

**Known limitations.** The rename that puts a new file in place checks that nothing is at the
path just before it, but is not a no-replace rename, so a file created at that path in the
instant between the check and the rename would be replaced; a no-replace rename needs a
per-platform call (`renameat2`, `renamex_np`, `MoveFileEx` without `REPLACE_EXISTING`) in the
platform layer and is left for later. An interrupted import into a new file leaves
`<file>.importing`, which the next import refuses until it is removed. `export` syncs the new
file and its directory.

**Size (D6).** The release binary grows from 866,912 to 933,312 bytes (+66,400, +7.7%) on
aarch64 macOS: about 34 KB is the format reader and writer and the two commands, the rest their
help and messages and the standard library's file creation, rename and clock code they use. The
library is unchanged, and no engine path changed, so the gated benchmarks are unaffected.

Decided by research (a research subagent, 2026-10-27), where the brief and the design differed
or were silent:
- *`--out`/`--from` or a second argument?* The second argument, as the design's grammar has it;
  `--from` already means "first key" on `scan`, and two spellings were not wanted.
- *Replace an existing export file?* No, like `create`; `-` and the shell's `>` replace files.
- *Create a missing database?* Only with `--create`, like `put --create`, so a typo in the path
  never becomes a new file.
- *Atomic or batched?* Batched, with the checked pass before writing from a file, `--dry-run`,
  and new files built aside and renamed; atomic imports would need a page cache as large as the
  import.
- *Key order?* Required ascending, as above.
- *Hex or base64?* Hex with the `_hex` field tag, as above.

## D94 Benchmarks against SQLite in a crate of its own, and the binary's size, start time and peak memory gated — Decided by research (build loop, 2026-10-27); completes v0.2.0 item 17, refines D1, D6, D7, D8, D14 and D80
Item 17 measures the embedded key-value store against SQLite on the same machine and commits the
results, with the `eggdb` binary's size, start time and peak memory, as baseline lines CI gates.
The method, the settings and the current numbers are in [benchmarks.md](benchmarks.md).

**A crate of its own (D1).** `crates/eggdb-sqlite-compare` has its own `[workspace]` and its own
committed `Cargo.lock`, is excluded from the main workspace and never published, so
`cargo test --workspace`, `cargo clippy --workspace` and the release build never compile SQLite
and the main `Cargo.lock` never lists it. It depends on `eggdb` by path and on `rusqlite` pinned
to `=0.41.0` with `default-features = false, features = ["bundled"]`, which compiles SQLite 3.54.1
into the benchmark. The licences, read with `cargo metadata`, are all permissive: `rusqlite`
0.41.0 and `libsqlite3-sys` 0.39.1 MIT; `bitflags` 2.13.2, `fallible-iterator` 0.3.0,
`fallible-streaming-iterator` 0.1.9 and `smallvec` 1.16.4 MIT or Apache-2.0; the build-only `cc`
1.10.0, `find-msvc-tools` 0.1.15, `shlex` 2.0.1, `pkg-config` 0.3.34 and `vcpkg` 0.2.15 MIT or
Apache-2.0; SQLite itself is in the public domain. D1 allows such a crate for benchmarks only; a
CI step in the `check` job fails when `cargo tree -p eggdb -e normal,build` lists any package
but our own `eggdb` crates. The harness is shared, not copied: the compare crate includes
`eggdb-bench`'s baseline, measurement, run-loop and key modules by `#[path]`.

**Fair settings.** Both stores hold the same 16-byte keys and values (100,000 rows preloaded, or
1,000 for the durable commit) in files in the same temporary directory. SQLite runs in WAL mode
with `locking_mode = EXCLUSIVE` (EggDB's writer holds an exclusive lock on its file for as long
as it is open), 4 KiB pages, a 64 MiB cache (`cache_size = -65536`, EggDB's `cache_bytes`),
`mmap_size = 0` (EggDB reads through its own cache), `wal_autocheckpoint = 256` pages (EggDB
checkpoints after 1 MiB of log), `fullfsync` and `checkpoint_fullfsync` on Apple's systems (EggDB
syncs with `F_FULLFSYNC` there, D81), a `kv(k BLOB PRIMARY KEY, v BLOB) WITHOUT ROWID, STRICT`
table, statements prepared once and reused, and `synchronous` mapped from EggDB's durability:
`Full` to `FULL`, `Normal` to `NORMAL`, `Off` to `OFF`. A unit test reads every one of these
settings back, and others check that both stores scan the same entries in the same order and
hold the same contents after the same puts. *The `NORMAL` caveat:* both modes promise that a
power loss can lose the latest commits and never corrupts the file, but SQLite in WAL with
`NORMAL` syncs only at checkpoints, while EggDB's `Normal` also syncs once a megabyte of log or a
thousand pages wait, so EggDB syncs more often for the same promise.

**Workloads** (samples alternate between EggDB and SQLite in one process, D80):
- `get`: a point get returning the value, a read transaction each (`Database::get`, and the
  prepared `SELECT v FROM kv WHERE k = ?1` in autocommit);
- `scan`: ascending entries without copying them (`scan(..)`, and the prepared `SELECT k, v FROM
  kv ORDER BY k`), per entry;
- `batch_put`: overwrites of random existing keys, 1,000 per write transaction (`BEGIN
  IMMEDIATE` … `COMMIT` around a prepared upsert), per put, with `Off` and `synchronous = OFF`;
- `batch_put_normal`: the same with `Normal` and `NORMAL`, reported only;
- `durable_commit`: one put per transaction waiting for the disk (`Full` and `FULL`), per commit,
  reported only.

The batched workloads time whole batches, so a sample never commits a partial one; slow workloads
calibrate from one iteration instead of 64. On a CI runner the `Normal` batches are dominated by
checkpoint syncs (their ratio moved from 1.08 to 1.69 between two runs), so the gated batch never
waits for the disk, as `database.write` does (D87).

**Gates.** `ratio_to_sqlite` lines in `benchmarks/baseline.txt`, EggDB's median time over
SQLite's, so below 1 means EggDB is faster. They fail 50% above the line after one retry, where
D80's ratios fail at 30%: seven CI runs on different runners gave point gets 0.60 to 0.83 and
scans 0.35 to 0.52, because two engines respond differently to each runner's processor, while a
ratio of EggDB to itself does not move that way. A missing line only warns, as in D80, so the
Linux lines are the median of five CI runs and macOS lines are measured on this Mac. Gates now
carry their own tolerance (`Gate::Ceiling`), and `--update` replaces only the lines of the
benchmark families it measured and keeps every other line's text, so `eggdb-bench` and the
compare crate each update their own lines of the one file.

**Footprint (D6, D14).** `eggdb-bench` builds the `eggdb` binary (`cargo build --release -p
eggdb`) and gates, for each system:
- `footprint.binary stripped_release_bytes`, its size, 1% above the line (D6 lets it only shrink;
  the 1% leaves room for toolchain noise, and the release notes list every target's size, musl's
  included, D75);
- `footprint.start` (`eggdb --version`) and `footprint.open_and_get` (`eggdb get` of one key from
  a closed 10,000-entry file): the median start time over 20 runs as a ratio to starting
  `eggdb-bench --exit-at-once`, a Rust program that exits at once, 50% above the line, since a
  process start depends on the machine; and the median peak resident memory of 20 more runs, 10%
  above the line. The research suggested 5%, but the `--version` peak on Linux runners ranged
  from 2,471,936 to about 2,610,000 bytes (+5.6%) with the same binary, so 5% would fail on the
  runner alone; 10% is still about 250 KB on a 2.5 MB peak.

The peak is measured from outside, in the platform layer (D8):
`eggdb_platform::process::run_with_peak_memory` runs a command and returns its exit code and peak.
On Apple's systems and the BSDs it is `ru_maxrss` from `wait4` (bytes on Apple's systems,
kibibytes elsewhere); on Windows the peak working set from `K32GetProcessMemoryInfo`. On Linux and
Android `ru_maxrss` cannot be used: `exec` folds the old address space's high-water mark into it,
and a spawned child's old address space is its parent's, so the first CI run reported the
benchmark's own 145 MB for every `eggdb` run. There the child is traced (`PTRACE_TRACEME` before
`exec`, `PTRACE_O_TRACEEXIT`) and its `VmHWM` read from `/proc/<pid>/status` when it stops at its
exit, which counts only its own address space; a test runs a child from a parent holding 256 MiB
and requires a peak under 64 MiB. A child that starts another program in its place (`sh -c 'exec
…'`) is followed through that `exec` (`PTRACE_O_TRACEEXEC`) to its exit, and its peak is then the
final program's alone, since `exec` starts a new address space; if tracing fails part-way, the
child is killed and reaped before the error returns. Start times are measured in runs that are
not traced.

**Size (D6).** This item changes no code in the `eggdb` binary: 933,312 bytes on aarch64 macOS,
1,133,296 on x86-64 Linux (glibc). Against v0.1.0's 302,448 bytes on macOS, which only printed
its version, the growth is the storage engine and embedded API (about 266 KB, D90), the
command-line tool (about 83 KB, D90), `eggdb ui` (+99,600 bytes in D91 and +116,208 in D92) and
`export`/`import` (+66,400, D93), each stated with its reason when it landed.

**What SQLite still wins, and why this item merges anyway.** Batched random overwrites: EggDB
takes about 1.35 times SQLite's time on this Mac (3.07 against 2.28 µs per put without syncs,
25.0 against 21.0 µs in `Normal`) and about 1.09 times on the Linux runners, in both modes. Both
engines log whole pages, so the gap is in EggDB's own write path (page images per commit,
checkpoints writing in place). It is a measured, avoidable shortfall, not the unavoidable,
minimised price of correctness or security that D7's research exception covers. The gate does not
hold it where it is either: the `ratio_to_sqlite` line fails only 50% above its value, so the gap
can grow to about 1.63 on Linux and 2.03 on this Mac before CI fails. This item may still merge
because it only adds measurement: it changes no hot path and no engine code, the slower write
path landed earlier (D81, D86, D87), and blocking the measurement would leave the gap ungated
altogether. D7 now binds the follow-up, issue #52: before v0.2.0 ships, either make the write
path at least as fast as SQLite's on this workload, or run a proper D7 research that shows the
remaining cost is unavoidable and minimised, and record it as a decision.

Decided by research (a research subagent, 2026-10-27, before the build), where the brief left a
choice:
- *Where does SQLite live?* A separate crate with its own workspace (above), rather than a
  dev-dependency of `eggdb-bench`, so no main-workspace command ever compiles it or lists it in
  the main lock file.
- *Which settings?* WAL with matching page size, cache, checkpoint threshold and sync mode, no
  mmap, `fullfsync` where EggDB uses `F_FULLFSYNC`, a `WITHOUT ROWID, STRICT` table and prepared
  statements; the exclusive locking mode was added during the build, as EggDB's writer also holds
  its file exclusively.
- *What to gate?* Point gets, scans and batched puts, gated as ratios like D80; the durable
  commit reported, since it measures the disk. CI's `compare` job runs on Linux only, in
  parallel with the others, with its own cache and a 15-minute timeout; other systems run it by
  hand.
- *How to measure memory?* The child's own peak through the platform layer (VmHWM on Linux,
  `ru_maxrss` on Apple's systems, the peak working set on Windows), with 1% for size, 5% for memory (10% in the build, above)
  and a wide band for start time.
- *A result where SQLite wins?* State it, gate it, and put the fix on the roadmap. As built, the
  gate's 50% band does not stop it growing, and D7 still applies to the fix (issue #52, above).

Decided during the build (not by the research): the 50% band for the SQLite ratios, the `Off`
mode for the gated batch, tracing on Linux for the peak and the 10% band for memory, each for
the reason given above.

## D95 A file lock is released when its handle drops, even while a child process is starting — Decided (build loop, 2026-10-27); supersedes D87's known limit, refines D8 and D9
**Cause.** `cargo test -p eggdb --bin eggdb` failed about one run in six on macOS (and nearly
always on macOS CI once more tests started child processes) with exit code 5, "another program
has it open", right after a command had closed the same database. Every file the platform layer
opens is already close-on-exec (the standard library opens files with `O_CLOEXEC`, sockets with
`SOCK_CLOEXEC` or `FD_CLOEXEC`, epoll and io_uring descriptors close-on-exec, kqueues are not
inherited; the standard library's Windows handles are not inheritable). That is not enough: a child process gets a copy
of every descriptor when it is created, and close-on-exec only closes the copies when it starts
its own program. On the BSDs, macOS and Linux the lock (`flock`) belongs to the open file that
every copy shares, and closing a descriptor released it only when the last copy closed. A test
thread that started `mkfifo` while another closed a database therefore left that database locked
until the child reached `exec`. Measured on this Mac with a loop that opens, locks, closes and
relocks a file while another thread starts `/usr/bin/true`: 109 of 20,000 relocks refused through
`posix_spawn`, 474 through `fork` and `exec`, none without children.

**Fix.** `OsFile` unlocks a lock it holds (`flock(LOCK_UN)` through the standard library's
`File::unlock`) before closing the descriptor, which releases the lock for every copy at once, as
Windows already did with `UnlockFileEx`. Rejected: POSIX record locks (`fcntl(F_SETLK)`), which a
child never inherits but which do not conflict between two handles in one process and are all
dropped when any descriptor of the file closes; retrying a refused lock, which only hides the
race. The lock contract (`File::try_lock`) now says so, and a test forks a child that waits
before starting its program while the handle drops, at the platform layer and through
`Database::open`, both failing before the fix. The tests' workaround, `tests/cli.rs` taking turns
between starting a child and opening a database, is removed.

## D96 Release engineering for v0.2.0: a smoke test of every release binary, sample files frozen at the tag, guides checked by the tests — Decided (build loop, 2026-10-27; the sample-file rule decided by research); completes v0.2.0 item 18, refines D9, D20, D75 and D76
**The smoke test is a shell script run against the shipped binary.** `scripts/smoke-test.sh
<binary>` drives a built `eggdb` through every command, a 3 MB value, an export and import round
trip, the sample files, `eggdb ui --help` and the refusal without a terminal. Bash is on every
runner, Git Bash included, and the script tests the exact file that is packaged, which a Rust
integration test (built for the host, in debug) would not. The release workflow runs it on each
target its runner can execute (six of eight; `x86_64-apple-darwin` and `aarch64-pc-windows-msvc`
are built only), and CI runs it against the debug binary on Linux, macOS and Windows. The
release workflow's old check expected `eggdb` alone to print `EggDB <version>`, which stopped
being true when the command landed (D90, it prints the help), so the first tag would have
failed; it now checks `eggdb version`.

**`eggdb ui` in a real terminal through `script(1)`.** Where `script` gives the binary a
pseudo-terminal (util-linux on Linux, BSD on macOS; `stty` sets 80 × 24 inside it), the smoke
test types keys with pauses longer than the confirmation guard (D92): move down, delete with the
confirmation, quit; the file must then lack the key and pass `check`. Windows runners have no
such tool, and driving a console pseudo-terminal (ConPTY) would need Windows code of our own in
the platform layer, so on Windows the UI is covered by its fake-terminal tests and the refusal
without a terminal; a ConPTY driver is a later item.

**Sample files, frozen at the tag (decided by research).** `crates/eggdb/tests/data` holds
`v0.2.0.db` (checkpointed, made with the `eggdb` command), `v0.2.0-log-to-replay.db` (three
commits left in the log, made through the embedded API by an ignored test that refuses to
overwrite it) and `v0.2.0.jsonl` (its export); [testing.md](testing.md) lists how each was made.
`sample_files.rs` pins each by its size and an FNV-1a fingerprint and checks that it opens
read-only and for writing, holds exactly the expected entries, passes `check` with the expected
counts, replays its log, takes commits, reopens, reads through the command and imports. The rule:
the v0.2.0 samples are frozen at the v0.2.0 tag, not before. Until the tag, a change to what the
engine writes on disk (issue #52's fix could be one) may replace them only with a numbered
decision approving it, regenerated the documented way in the same pull request with their pinned
sizes and fingerprints updated. From the tag on they are never regenerated or edited; a later
format change adds samples beside them, and every later release must still read them. *Why:*
D20's promise is about what v0.2.0 writes, so samples that a pre-release format change made stale
would guard nothing, while freezing them today would force such a change into a format bump for
files nobody has. `.gitattributes` keeps the samples byte-exact and the script's line endings LF
on Windows checkouts.

**The guides are checked by the tests.** The Rust examples of `docs/embedded.md` and the README
are complete programs compiled as `no_run` doctests (`EmbeddedGuide` and `Readme`, which exist
only under `cfg(doctest)` in `crates/eggdb/src/lib.rs`); they open `app.db` in the working
directory, which a test must not litter, so they compile without running, and the behaviour
they show is tested elsewhere. The UI screens in `docs/cli.md` must equal the fake terminal's
golden frames (`the_cli_guide_shows_the_screens_as_the_ui_draws_them`).

**The correctness statement** is [correctness.md](correctness.md), linked from the README and the
release notes: the promises, the test behind each, the platforms CI runs, and what is not tested
(real power loss on hardware, a real process killed mid-commit, more than one writer, `Off` after
a power loss, an outside audit, network file systems, large files and long runs, CPUs outside
CI).
