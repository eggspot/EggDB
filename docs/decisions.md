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
habits. These are clean-room (D1): we match layout, behavior and shortcuts, and never copy another
product's code, artwork, icons, names or trademarks.

**Usable by everyone.** The client follows accessibility standards (keyboard-first, screen readers,
contrast) and works on small screens. Its speed is measured like the engine's (D7): opening a
database, running a query and scrolling a large result.

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
behavior and speed are the same everywhere. A language or runtime that can't load native code (the
browser, for example) gets the same core compiled to WebAssembly, or a pure implementation of the
protocol when that is the only way.

Existing drivers for the databases EggDB is compatible with keep working too (D12). The official
drivers add what only EggDB has: embedded mode, every model and the MCP tools.

**Proof and publishing.** Each driver is proven by its ORM's own test suite or official provider
tests running against EggDB in CI. Publishing to the package registries (NuGet, Maven Central, npm,
PyPI, crates.io, Packagist, RubyGems, pub.dev and others) needs accounts and tokens, which wait for
the owner.
