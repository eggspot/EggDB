# EggDB roadmap

Each milestone ships as a release when it is merged (CLAUDE.md, "Releases"). The order puts the
core first and then the models and protocols that most applications use (D5). The build loop may
refine it through research, recorded in docs/decisions.md.

## Milestones
| # | Milestone | Status |
|---|-----------|--------|
| M0 | Repo skeleton: workspace, pinned toolchain, CI, docs | **done** |
| M1 | Platform layer: files, direct and async I/O, memory, threads, clocks, behind one interface (D8) | |
| M2 | Storage engine: page format, buffer management, write-ahead log, checkpoints, crash recovery; the B-tree/LSM/hybrid choice made by research with benchmarks | |
| M3 | Transactions: MVCC snapshots, serializable isolation, deadlock-free locking, durability modes | |
| M4 | Embedded key-value API and the single-file database (D3); first SQLite and Redis comparisons and the first size, memory and startup budgets (D6); the first release binary with its download and install times measured (D14) | |
| M5 | SQL: parser, planner, optimizer and executor; SQLite-compatible embedded SQL and the SQLite file format | |
| M6 | Server and the PostgreSQL wire protocol and SQL dialect; the built-in MCP server, with schema discovery, querying and agent permissions (D11); PostgreSQL tools (psql, pgAdmin, DBeaver, the main drivers and ORMs) working (D12); restoring `pg_dump` output (D13) | |
| M7 | Caching: in-memory tables with expiry and eviction, pub/sub, streams; Redis RESP | |
| M8 | Full-text search: analyzers, inverted indexes, BM25 ranking; Elasticsearch/OpenSearch REST | |
| M9 | Documents and JSON; the MongoDB wire protocol | |
| M10 | The other SQL dialects, each as itself (D10): MySQL/MariaDB, Microsoft SQL Server (T-SQL, TDS) and Oracle (SQL, PL/SQL) | |
| M11 | Columnar storage and analytics; time series | |
| M12 | Vectors (approximate nearest neighbours) and geospatial | |
| M13 | Graphs | |
| M14 | Replication and high availability | |
| M15 | Sharding, distributed transactions and multi-region clusters (D4) | |
| M16 | Wide-column (CQL), queues and streams (Kafka), objects (S3), InfluxDB line protocol | |
| M17 | Every platform: Windows, macOS, BSDs, Android, iOS, EggOS, WebAssembly, `no_std` (D8); packages and cloud deployments | |

Every protocol milestone also covers that database's tools (D12) and its backups and dumps (D13):
an application's whole toolchain and its data move over together.
