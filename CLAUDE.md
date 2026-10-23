# EggDB: Claude Code instructions

EggDB is one database for every kind of data, written from scratch in Rust: caching, relational SQL,
full-text search and every other kind of database people run today, in a single lightweight engine.
It is the fastest and the lightest, it scales from a tiny embedded file (better than SQLite) to an
enterprise cluster with billions of billions of rows, and it runs on every OS, every CPU and every
cloud. Open source under MIT OR Apache-2.0 (public repo `eggspot/EggDB`), a sibling of EggOS.

## IMPORTANT: notes for every task
Check each task against these before starting and again before opening the PR.
1. **All in one.** One engine covers every kind of database that exists: key-value and caching (with
   expiry, eviction, pub/sub, streams and queues), relational SQL with ACID transactions, full-text
   search, documents and JSON, time series, columnar analytics, graphs, vectors for AI, geospatial,
   wide-column, blobs and objects. They share one storage engine, one transaction system and one
   query layer, so a single query or transaction can span models. Never build a model as a
   separate database bolted on the side. **Nothing missing:** when a capability people need from a
   data platform is missing, add it to the roadmap and build it ourselves, without asking the
   owner. That includes our own BI application, ETL pipelines and big-data connectors. What still
   goes to the owner: money, accounts and license changes, unsettled legal or patent findings, and
   D45's "not planned" list. (D2, D46)
2. **We support everything people already use.** Applications switch to EggDB without code
   changes: EggDB speaks the existing wire protocols and query languages (PostgreSQL and MySQL
   protocols and SQL, Redis RESP, MongoDB, Elasticsearch/OpenSearch, Cassandra CQL, InfluxDB line
   protocol, Kafka, S3, SQLite's embedded API and file format, and more; order in D5). Behavior
   matches the original closely enough that its own client libraries and test suites pass;
   coverage is tracked per protocol and dialect and grows every release. Oracle's own wire
   protocol is not implemented until a lawful public specification exists (D10). (D5)
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
   (exabytes). Every way of scaling, not only a primary with replicas; D18 has the full list:
   - scaling up and partitioning;
   - replicas and standbys;
   - multi-primary and leaderless quorums;
   - sharding and distributed SQL;
   - storage separated from compute, and serverless;
   - tiered storage, geo pinning and edge copies;
   - offline-first sync, federation and analytical replicas.

   Each deployment picks the ones it needs, and can change without exporting data. The same engine
   and the same data format at every size; never write code that assumes one size or one topology.
   (D4, D18)
7. **Everywhere.** Every OS (Linux, Windows, macOS, the BSDs, Android, iOS, EggOS, the browser
   through WebAssembly, `no_std` embedded), every CPU architecture, every cloud (AWS, Azure, Google
   Cloud and the rest), Kubernetes, containers and bare metal. Platform code lives behind one
   platform layer, never mixed into the engine. (D8)
8. **Correct and safe first, then fast.** Data is never lost or corrupted: crash safety is tested,
   not assumed (fault injection, deterministic simulation, fuzzing every parser and wire
   protocol), and hostile input never crashes or hangs the server. (D9)
9. **Every query language and concept.** Each SQL dialect runs as itself: PostgreSQL, Microsoft SQL
   Server (T-SQL), MySQL/MariaDB, Oracle (SQL and PL/SQL) and SQLite. That covers their data
   types, functions, procedural languages, system catalogs and quirks, so a query written for one
   of them runs on EggDB unchanged. Next to SQL come the other ways people query data: the
   Redis, MongoDB, Elasticsearch and CQL query languages, graph queries (SQL/PGQ, GQL, Cypher),
   vector and full-text search, and time-series queries. A session picks its dialect (by protocol,
   connection setting or per statement), and every dialect works on the same data. (D10)
10. **AI-native, with MCP built in.** Any AI agent can use EggDB as easily as a person:
    - a built-in MCP server (Model Context Protocol, implemented by us from the public spec)
      exposes schema discovery, querying in any dialect, safe writes, explain plans, data
      sampling, backups and administration as MCP tools and resources;
    - errors and explain output are written to be read by models as well as people;
    - vector search and embeddings are first-class (D2).

    Agents act only with the permissions they are granted (per agent, per database and table,
    read-only by default, revocable). Destructive actions need confirmation, and every agent
    action is logged where the owner can see it. It must work well with any agent, framework and
    model, not one vendor's. (D11)
11. **Works with every tool people already use.** Drivers, ORMs, GUI clients, admin, BI, backup and
    monitoring tools built for other databases work with EggDB unchanged. That includes JDBC,
    ODBC, ADO.NET, libpq, EF Core, Hibernate, Prisma, SQLAlchemy, pgAdmin, DBeaver, DataGrip,
    SQL Server Management Studio, MySQL Workbench, Redis Insight, MongoDB Compass, Kibana,
    Grafana and Metabase. Each one's catalog queries, metadata calls and protocol details are
    answered the way it expects. (D12)
12. **Migrating is restoring a backup.** Moving to EggDB means pointing it at the backup or dump
    the old database already makes. EggDB restores formats with a public specification directly:
    - SQL dumps from any of the dialects, including `pg_dump` plain format and `mysqldump`;
    - SQLite files;
    - `mongodump` BSON;
    - Redis AOF, and Redis RDB where its format is publicly documented.

    Formats without one (SQL Server `.bak`, `pg_dump`'s custom format, `pg_basebackup`,
    Elasticsearch snapshots, Oracle Data Pump) go through the source's own documented export or
    its replication feed, and are never reverse-engineered. EggDB can also follow a live database
    through its replication or change stream for a switch with no downtime. (D13)
13. **One second to download, one second to install.** EggDB is one small, self-contained file
    with no dependencies. Installing it means downloading that file (a one-line command or the
    usual package manager) and it runs; the first start takes well under a second. Download
    size, install time and first-start time are measured for every release and may only
    shrink. (D14)
14. **A SQL client people love, designed by Claude Fable.** EggDB ships its own client for working
    with data: a GUI that runs as a desktop app on every OS and in the browser, from one Rust
    codebase on our own GUI toolkit (the desktop GUI is a must-have, built from the start), and a
    CLI. Any task that designs something a user sees (layouts, flows, the design system, icons,
    motion, the query editor, result grids, schema browser, explain views) gets its design from
    Claude Fable: delegate it to an agent on the `fable` model (Agent tool, `model: "fable"`),
    record the design in `docs/ux/`, then implement it. Next to EggDB's own design, the client
    offers familiar layouts that work like the clients people already know (SQL Server Management
    Studio, pgAdmin, DBeaver, DataGrip, MySQL Workbench, MongoDB Compass, Redis Insight), so nobody
    has to relearn their habits. Familiar means the same layout, shortcuts and workflows, never
    their code, artwork, icons or screenshots. A layout may be labelled "works like <product>", with
    a trademark note. (D15)
15. **Connectors for every language, working with their ORMs.** Official EggDB drivers for C#/.NET,
    Java and Kotlin, Node.js and TypeScript, Python, Go, Rust, PHP, Ruby, Swift, Dart, C and C++
    (and more as people ask), each fitting its language's standard database interface and its
    popular ORMs. That covers ADO.NET and EF Core, JDBC and Hibernate, Prisma, TypeORM, Drizzle
    and Sequelize, DB-API, SQLAlchemy and Django, `database/sql` and GORM, PDO with Laravel and
    Doctrine, and ActiveRecord. They work both against a server and in embedded mode. The engine
    and one client core are Rust. Each language's driver and ORM provider (an EF Core provider is
    C#, a Hibernate dialect is Java) is a thin layer in that language calling the Rust core
    through the C API, with no protocol or storage logic of its own (the two exceptions are in
    D16 and D17). Crossings are batched, never per value. Every driver is benchmarked against
    SQLite's binding in its language (embedded mode, from M5) and against the best driver in its
    language (server mode, from M6), and is gated in CI. (D17) Existing drivers keep
    working too, through the protocols EggDB speaks (D12). (D16)
16. **Every data type.** Every data type of every database EggDB is compatible with, with its exact
    behavior: precision, range, overflow, rounding, collation, time zones and comparison. That
    covers every SQL dialect's types (D10), the types of Redis, MongoDB, Elasticsearch and
    Cassandra, vectors, geospatial, time series, graphs, and user-defined types. One type
    system underneath stores and indexes them all. (D19)
17. **Everything a production database needs, from the start.** That means:
    - our own versioned, checksummed file format with online upgrades, designed for distribution
      from day one (D20);
    - a public security policy and stability promise (D21);
    - authentication, roles, and row and column security on by default (D22);
    - cryptography and key management (D23);
    - backups with point-in-time recovery (D24);
    - observability and operations (D25);
    - audit and compliance (D26);
    - procedures, triggers, functions, extensions and jobs (D27);
    - time travel, branching and change streams (D28);
    - Parquet, Arrow and lakehouse tables (D29);
    - every language's text (D30);
    - AI in the database (D31);
    - the full relational feature set and multi-tenancy (D32);
    - messaging and queues: RabbitMQ (AMQP), MQTT, NATS, SQS and Kafka (D33).

    Build profiles and per-workload targets keep all of it lightweight and fast (D34).
18. **And the rest of what applications need** (D37–D45):
    - generated REST, GraphQL, gRPC and OData APIs, realtime subscriptions and end-user auth for apps
      (D37);
    - ledger, RDF/SPARQL, XML/XQuery, OLAP (MDX, DAX) and spreadsheet-style data (D38);
    - metrics, logs and traces as a store with PromQL, LogQL and TraceQL (D39);
    - the engine internals stated (D40);
    - embedded and edge in detail (D41);
    - an admin console, notebooks and onboarding (D42);
    - legal, export and privacy (D43);
    - public proof, community, metering and energy (D44);
    - the weak-spot fixes, including prompt-injection safety and limits for functions (D45).

## Code
- **Everything written by us, in Rust, from scratch.** The only exceptions are the per-language
  driver and ORM layers and the minimal HTML/JS that loads the browser build (D16). No code from other databases (SQLite,
  PostgreSQL, RocksDB, Redis, Lucene and so on). Implement from public specifications, papers and
  protocol documentation (clean room), and never read the source of copyleft or source-available
  databases (MySQL, MariaDB, MongoDB and others) while working on EggDB. No third-party crates in
  what we ship (Rust's standard library and our own OS API declarations are fine); a research
  subagent may approve a permissive (MIT, Apache-2.0, BSD) crate for tests, benchmarks or build
  tooling only. Never add copyleft or source-available code (GPL, LGPL, AGPL, MPL, SSPL, BSL).
  Other products' names are trademarks: say "compatible with". (D1)
- **No panics on input.** Release builds abort on panic for speed, so nothing a client, a file or
  the network sends may reach a panic: parsing and validation return errors as values, and fuzzing
  checks it (D9).
- `unsafe` only where unavoidable, each block with a `// SAFETY:` comment explaining why it is
  sound (clippy denies undocumented unsafe blocks). One unsafe operation per block.
- **No other comments in code.** Public items get rustdoc (`///`, `//!`), which `cargo doc` builds
  in CI. Everything else is explained by names, small functions, types and tests. When you change
  a file, remove the comments it has that this rule doesn't allow.
- Release builds are what we measure and ship (LTO, one codegen unit).

## Layout
- `crates/eggdb`: the library and the `eggdb` binary. New subsystems become workspace crates
  under `crates/` when they have their own boundary (storage, transactions, query, protocols,
  platform layer); the engine core never depends on a protocol or a platform.
- `docs/decisions.md`: numbered, append-only decisions. `docs/roadmap.md`: milestones and status.

## Commands
- `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`,
  `cargo doc --workspace --no-deps`. CI runs exactly these (job `check`, the required status check).
- EggDB is public, so standard GitHub runners are free: CI aims for the shortest time from push to
  the first failure and to a green run (parallel jobs are fine when they shorten it), with no larger
  runners, superseded PR runs cancelled, and a timeout on every job.

## Pull requests: merge them yourself
The owner doesn't merge PRs by hand. After opening a PR, in the same session:
1. Review it twice yourself (global PR workflow), then launch an independent review subagent with the
   PR, the diff and this file, and ask for MERGE or BLOCK. Fix every blocking issue and ask again.
2. When the verdict is MERGE, both passes are clean and CI is green, squash-merge it
   (`gh pr merge <n> --squash --delete-branch`), then confirm every workflow the merge started
   succeeded.

Security-critical PRs (cryptography, authentication and authorization, the file format) merge the
same way, but need a second independent review focused only on security, plus test vectors,
fuzzing and the format compatibility tests in CI (D35).

An open question is never a reason to leave a PR unmerged: answer it with a research subagent,
record the answer in the PR under "Decided by research" (and in docs/decisions.md for architecture),
and merge. Only credentials, money, new accounts and going against an owner rule wait for the owner.
A PR that changes the license is merged only on the owner's explicit instruction (D36).

## Releases
Semantic versioning, tags `vMAJOR.MINOR.PATCH`. Until 1.0, each finished milestone bumps MINOR and
fixes in between bump PATCH. Cut the release as soon as a milestone is merged and `main` is green:
version bump PR, tag, and a GitHub Release with notes and binaries for the platforms that build.
Publishing to package registries (crates.io and others) needs accounts and tokens, so the owner sets
those up first.

## Owner decisions
The decisions above are recorded in docs/decisions.md (D1 to D46). Record every further
architecture decision there as a numbered entry; a later entry may supersede an earlier one.
