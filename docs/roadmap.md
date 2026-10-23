# EggDB roadmap

Each milestone ships as a release when it is merged (CLAUDE.md, "Releases"). The order puts the
core first and then the models and protocols that most applications use (D5). The build loop may
refine it through research, recorded in docs/decisions.md.

## Milestones
| # | Milestone | Status |
|---|-----------|--------|
| M0 | Repo skeleton: workspace, pinned toolchain, CI, docs | **done** |
| M1 | Platform layer: files, direct and async I/O, memory, threads, clocks, behind one interface (D8) | **in progress**: files, clocks, the crash simulator (D59), direct I/O with aligned buffers (D60), memory with huge pages and its simulator (D61), CPU sets, pinning and cache padding (D62), named, pinned thread starts (D63), the first review (D64), the CPU topology (D65) and asynchronous file I/O through a completion queue with a worker pool and its simulator (D66) and io_uring behind it, with backends chosen automatically or by request (D67) done; next IOCP, then networking |
| M2 | Storage engine: our own versioned, checksummed page format, buffer management, write-ahead log ready to become a consensus log, hybrid logical clock timestamps, tenant and shard keys in the key layout (D20), checkpoints, crash recovery, partitioning; the B-tree/LSM/hybrid choice made by research with benchmarks | |
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

## Where we are
Rough progress toward the whole goal: **about 1%**. The skeleton, the decisions and the first piece
of the platform layer exist; there is no storage engine, transaction system or query layer yet.

| Area | Status | Missing |
|------|--------|---------|
| Platform layer (M1) | files with positional, direct and asynchronous I/O (a completion queue on io_uring on Linux and a worker pool elsewhere), aligned buffers, directory sync, clocks, memory (page sizes, explicit and transparent huge pages), CPU sets, thread pinning on Linux and Windows and pinned thread starts, cache padding, CPU topology (cores, packages, NUMA nodes, last-level caches) on Linux, Android and Windows with counts on Apple's systems, deterministic crash, fault, memory and CPU simulation | IOCP, batched io_uring submission, networking, per-device alignment, NUMA memory placement and locked memory, performance and efficiency cores, pinning and topology on the BSDs |
| Storage and transactions (M2, M3) | not started | everything |
| Models (D2) | none | key-value, relational, search, documents, time series, columnar, graphs, vectors, geospatial, wide-column, objects, queues |
| SQL dialects (D10, D47) | none (0%) | PostgreSQL, SQLite, MySQL/MariaDB, T-SQL, Oracle, then GoogleSQL, Snowflake, Spark, ClickHouse, Trino, Db2, Sybase |
| Protocols (D5) | none (0%) | PostgreSQL, RESP, MongoDB, Elasticsearch, CQL, Kafka, AMQP, MQTT, S3 and the rest |
| Platforms (D8) | Linux, macOS and Windows built and tested in CI; WASI type-checked locally | BSDs, Android, iOS, EggOS, browser WebAssembly, `no_std` |
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
