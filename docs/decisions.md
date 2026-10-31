# Architecture decisions

The index of the numbered, append-only decisions. Each decision is a file of its own in
[decisions/](decisions/), named `D<number>.md` with the number zero-padded to three digits
(`decisions/D007.md`), so a reader opens only the decisions it needs. The first line of a file is
its full title with its status.

## Rules

- **Append-only.** A decision is never rewritten or deleted. A later decision may supersede an
  earlier one: say so in both, in the later decision's text and as a short note after the status
  in the header of the earlier one (for example "superseded in part by D101"), and in this index.
- **Status** is **Decided** (owner), **Decided by research**, or **Decided by the build loop**
  (engineering). The owner's decisions fix the goal. The concrete lists inside them (protocol
  order, benchmark names, budgets, permission details) were filled in when the project was set
  up, and research may refine them with a new entry.
- **Numbering.** The next free number is the highest number in this index plus one. Check the
  index and `ls docs/decisions` on the latest `main` before taking it, since pull requests open at
  the same time may take the same number: whichever merges second renumbers.
- **A new decision** goes in its own file `docs/decisions/D<number>.md`, with a header line
  `## D<number> <title> — <status> (<who>, <date>); <relations to other decisions>`, and gets
  one line in the list below: its number as a link to the file, and its title, plus any
  supersession note. Other documents refer to a decision by its number ("D7"), or link to its
  file.

## Decisions

- [D1](decisions/D001.md) Everything written by us, in Rust, from scratch
- [D2](decisions/D002.md) All in one: every kind of database in one engine
- [D3](decisions/D003.md) Single: one binary, server and embedded library
- [D4](decisions/D004.md) Every scale, one engine
- [D5](decisions/D005.md) We support everything people already use
- [D6](decisions/D006.md) Lightweight: size, memory and startup budgets that only shrink
- [D7](decisions/D007.md) The fastest database, with the best performance (superseded in part by D101)
- [D8](decisions/D008.md) Everywhere: every OS, CPU and cloud
- [D9](decisions/D009.md) Correct and safe first
- [D10](decisions/D010.md) Every query language and concept, each SQL dialect as itself
- [D11](decisions/D011.md) AI-native, with a built-in MCP server
- [D12](decisions/D012.md) Works with every tool people already use
- [D13](decisions/D013.md) Migrating from another database is restoring its backup (for `pg_dump`, applied by D118)
- [D14](decisions/D014.md) One second to download, one second to install
- [D15](decisions/D015.md) The EggDB client: our own design by Claude Fable, plus familiar layouts
- [D16](decisions/D016.md) Official connectors for every language, working with existing ORMs
- [D17](decisions/D017.md) Driver speed: batched crossings and benchmarks in every language
- [D18](decisions/D018.md) Every way of scaling, not only a primary with replicas
- [D19](decisions/D019.md) Every data type, with each database's exact behavior
- [D20](decisions/D020.md) Our own file format: versioned, checksummed, ready for scale and upgrades
- [D21](decisions/D021.md) Project governance, security policy and stability promise
- [D22](decisions/D022.md) Security model: authentication, authorization, row and column security
- [D23](decisions/D023.md) Cryptography, TLS and key management
- [D24](decisions/D024.md) Backup, point-in-time recovery and verified restores
- [D25](decisions/D025.md) Observability and operations
- [D26](decisions/D026.md) Audit, compliance and data governance
- [D27](decisions/D027.md) Programmability: procedures, functions, triggers, extensions, jobs, events
- [D28](decisions/D028.md) Time travel, branching, derived data and change streams out
- [D29](decisions/D029.md) Data interchange, lakehouse tables and external data
- [D30](decisions/D030.md) Text, locales and internationalization
- [D31](decisions/D031.md) AI and machine learning in the database
- [D32](decisions/D032.md) Relational completeness and multi-tenancy
- [D33](decisions/D033.md) Messaging and queues: RabbitMQ, MQTT, NATS, SQS and Kafka
- [D34](decisions/D034.md) Engineering guardrails: build profiles, performance targets, legal checks
- [D35](decisions/D035.md) Owner answers: our own cryptography, security-critical code merges itself, the name is open
- [D36](decisions/D036.md) License MIT OR Apache-2.0; no paid external audit
- [D37](decisions/D037.md) Auto-APIs, realtime and the app backend
- [D38](decisions/D038.md) The remaining data models and their query languages
- [D39](decisions/D039.md) Observability data: metrics, logs and traces as a store
- [D40](decisions/D040.md) Engine internals, stated
- [D41](decisions/D041.md) Embedded and edge, in detail
- [D42](decisions/D042.md) Beyond the client: admin console, notebooks and onboarding
- [D43](decisions/D043.md) Legal, export and privacy
- [D44](decisions/D044.md) Public proof, community, cost and energy
- [D45](decisions/D045.md) Fixes to weak spots
- [D46](decisions/D046.md) Nothing missing: we build every missing piece ourselves, without asking
- [D47](decisions/D047.md) More SQL dialects and analytics protocols
- [D48](decisions/D048.md) Other NoSQL, key-value and graph APIs
- [D49](decisions/D049.md) Microsoft, Splunk and legacy sources
- [D50](decisions/D050.md) Finance and industry standards
- [D51](decisions/D051.md) Data catalog, lineage, master data and quality
- [D52](decisions/D052.md) Data sharing, clean rooms and privacy technology
- [D53](decisions/D053.md) Retention, legal hold, WORM and sovereignty
- [D54](decisions/D054.md) Coordination, configuration, secrets and directory
- [D55](decisions/D055.md) Real-time decisioning: streams, events and graph algorithms
- [D56](decisions/D056.md) Content and collaboration
- [D57](decisions/D057.md) Science, geo and IoT
- [D58](decisions/D058.md) Hyperscale operations
- [D59](decisions/D059.md) The platform layer's first shape: files, clocks and a crash simulator
- [D60](decisions/D060.md) Direct I/O: an explicit uncached open that fails instead of falling back
- [D61](decisions/D061.md) Memory: OS page sizes, explicit and transparent huge pages, and a simulated twin
- [D62](decisions/D062.md) CPUs: the available set, pinning given or refused, and padding against false sharing
- [D63](decisions/D063.md) Starting threads: pinned before the work runs, a refused pin is a spawn error
- [D64](decisions/D064.md) First platform review: syncs need a writable handle, the simulator stays bounded and fast
- [D65](decisions/D065.md) CPU topology: our own machine-wide numbers, the whole machine, read once when asked
- [D66](decisions/D066.md) Asynchronous file I/O: a completion queue, owned buffers, a worker pool first
- [D67](decisions/D067.md) io_uring behind the queue, and choosing a backend: automatic by default, given or refused when asked
- [D68](decisions/D068.md) Second platform review: wrong paths answered alike, overlapping operations refused in simulation, Windows topology grouped as Linux does
- [D69](decisions/D069.md) IOCP behind the queue: a second overlapped handle per queue, uncached files through the port, on request only
- [D70](decisions/D070.md) Networking: a completion queue of accepts, connects, receives and sends, a portable poller first, a simulated network
- [D71](decisions/D071.md) Readiness backends for the network queue: epoll and kqueue, interest only while an operation waits
- [D72](decisions/D072.md) Timeouts in the network queue: a timed reap and cancel, with simulated time, latency and partitions
- [D73](decisions/D073.md) `poll(2)` for the network queue on the BSDs and illumos, with blocking connects there for now
- [D74](decisions/D074.md) Fewer system calls per connection and per wait: one-shot interest, sockets ready in one call, one accept per connection
- [D75](decisions/D075.md) Releases: v0.1.0 first, built and published by a tag-triggered workflow, static Linux binaries by default
- [D76](decisions/D076.md) The first user release: v0.2.0 on 2026-11-01, an embedded key-value store in one file
- [D77](decisions/D077.md) The storage engine's design and file format, version 1
- [D78](decisions/D078.md) The page file: commits that never overwrite committed pages, and file locks through the platform layer
- [D79](decisions/D079.md) The page cache: one region of frames, clock eviction, latched page handles and no-steal
- [D80](decisions/D080.md) Benchmarks: a harness of our own, counts gated exactly, times gated as ratios (superseded in part by D101)
- [D81](decisions/D081.md) The write-ahead log: whole-page images in segments inside the file, redo on open, three durability modes
- [D82](decisions/D082.md) The B+tree: slotted pages with fence keys and prefix truncation, a fixed root, and pages checked once per read from the file
- [D83](decisions/D083.md) B+tree delete merges under-filled pages only when they fit, and cursors re-descend by fence key holding no page between calls
- [D84](decisions/D084.md) Checkpoints at the log's append position, recycled segments, and segment sequences never used twice
- [D85](decisions/D085.md) Crash tests across the whole storage stack: a sweep that fails every I/O operation of a B+tree workload
- [D86](decisions/D086.md) Transactions: one writer and snapshot readers over the page cache, before-images in memory, hybrid-clock commit stamps
- [D87](decisions/D087.md) The embedded Rust API: `Database`, two transaction types, double-ended scans and errors with a kind (superseded in part by D139: collections as key prefixes)
- [D88](decisions/D088.md) Large values on chains of overflow pages, a 1 GiB value limit, and a check that accounts for every page
- [D89](decisions/D089.md) A model test against an in-memory map, a seeded fuzzer for hostile files, and the older header slot made a copy when a file opens for writing
- [D90](decisions/D090.md) The `eggdb` command: command first, byte arguments, formats chosen by the terminal, exit codes 0 to 5, terminal detection in the platform layer
- [D91](decisions/D091.md) `eggdb ui` part 1: a raw terminal in the platform layer, an `eggdb-ui` crate behind a `Source` trait, a read-only browser
- [D92](decisions/D092.md) `eggdb ui` part 2: read-write by default, one confirmed commit per change, the check on a worker
- [D93](decisions/D093.md) `eggdb export` and `eggdb import`: the eggdb-jsonl format, a checked pass before writing, batched commits and new files built aside
- [D94](decisions/D094.md) Benchmarks against SQLite in a crate of its own, and the binary's size, start time and peak memory gated (superseded in part by D101)
- [D95](decisions/D095.md) A file lock is released when its handle drops, even while a child process is starting
- [D96](decisions/D096.md) Release engineering for v0.2.0: a smoke test of every release binary, sample files frozen at the tag, guides checked by the tests
- [D97](decisions/D097.md) A flush longer than the recycled log segments ends in one of them, and the checkpoint after it cuts its new segments off the file
- [D98](decisions/D098.md) The SQL core, part 1: a storage-agnostic `eggdb-sql` crate, SQL in a reserved key prefix, order-preserving primary keys, PostgreSQL's errors and blocks (superseded in part by D139: SQL stays in its prefix)
- [D99](decisions/D099.md) SQL is slower than SQLite for now: gated, scheduled and not waived; the binary grows by the SQL core (superseded in part by D101)
- [D100](decisions/D100.md) Joins and GROUP BY in the SQL core: left-to-right joins by key lookup, hash table or nested loop, grouping in a hash table, a 1 GiB bound per statement, and the join's gap to SQLite gated and scheduled (superseded in part by D101)
- [D101](decisions/D101.md) Benchmarks are reported on pull requests and only a slowdown of more than 2x blocks; one final `check` job is the required check and auto-merge is allowed
- [D102](decisions/D102.md) Every listener binds only to loopback or a local socket until EggDB's own TLS exists (owner rule, an exception to D22); superseded in part by D161
- [D103](decisions/D103.md) The PostgreSQL server: an `eggdb-pgwire` crate, a thread per connection, SCRAM-SHA-256 with our own `eggdb-crypto`, one user whose verifier is derived at startup
- [D104](decisions/D104.md) SQL takes `$n` parameters as text bound at parse time, describes statements without running them, and keeps PostgreSQL's run-time parameters
- [D105](decisions/D105.md) `eggdb shell` and the query screen reuse the UI's terminal layer and the `sql` command's printers; the correctness suite is a table-driven file per dialect
- [D106](decisions/D106.md) MySQL protocol: caching_sha2_password fast-auth, native as second; the MySQL dialect in the SQL core, MySQL's errors and transactions, `AUTO_INCREMENT` in table definition version 2
- [D107](decisions/D107.md) The SQLite dialect: affinity mapped onto the engine's types, `INTEGER PRIMARY KEY` as the row-id column, SQLite's errors with extended result codes, `eggdb::sql` for SQL from the embedded API
- [D108](decisions/D108.md) The Oracle dialect: names that swap case on the shared catalog, the empty string as NULL, `ROWNUM` as a source-row limit, implicit transactions and `ORA-` errors; no Oracle wire protocol
- [D109](decisions/D109.md) Secondary indexes: entries beside the rows on the key-value engine, table definitions in format version 3, a planner of fixed rules, `EXPLAIN` in each dialect's style
- [D110](decisions/D110.md) v0.2.0 scope: bring PostgreSQL, SQL Server and Oracle databases into EggDB by restoring their own dumps and scripts; no other engine run, wrapped or proxied
- [D111](decisions/D111.md) Exact numeric (own decimal, 1,000 digits each side), PostgreSQL's dates and time stamps, `json` checked and `jsonb` normalized as text, serial and identity on the auto-increment column, table definitions in format version 4
- [D112](decisions/D112.md) Subqueries, `WITH` and set operations as in-memory intermediate tables of the statement, correlated subqueries run per distinct outer value with their outer references as constants, PostgreSQL's `UNION` type rules, `RIGHT`/`FULL` joins with matched flags, nesting and recursion limits
- [D113](decisions/D113.md) `time`, `timetz`, `interval` (months, days, microseconds; equal by 30-day months), `bytea`, `uuid` and blank-padded `character(n)`; table definitions in format version 5; `gen_random_uuid()` from a random source the embedding program registers
- [D114](decisions/D114.md) Sequences as catalog objects saved with the writer and re-saved after a rollback; `CHECK` and `FOREIGN KEY` constraints in table definition format version 6; statement-level foreign keys through a reverse-reference key space; `ALTER TABLE` by rewriting rows in bounded batches
- [D115](decisions/D115.md) The binary's growth past twice its first baseline is the price of the all-in-one SQL features: the `footprint.binary` lines move to the measured size and the weekly pass of 2026-10-31 shrinks it
- [D116](decisions/D116.md) Schemas as name prefixes (`public` keeps today's bytes) with tables in other schemas in format version 7, `search_path` resolved per statement, views as format version 8 records in the table key space expanded at use with depth, size and cycle limits, view dependencies and comments in key spaces of their own
- [D117](decisions/D117.md) `COPY` streams through a client channel into the `INSERT` path in one transaction, scripts carry their data inline, lines up to 1 GiB, and no server files or programs (superseded in part by D154)
- [D118](decisions/D118.md) `eggdb restore` reads a `pg_dump` plain dump as a stream, one transaction per statement, a new file built aside, and a report of what was skipped (`OVERRIDING` superseded by D146)
- [D119](decisions/D119.md) The built-in MCP server, part 1: `eggdb mcp` over stdio, read-only by opening the file read-only for each call, one statement per call, a time, row and byte limit per call, results marked as data
- [D120](decisions/D120.md) The MCP audit log: a sidecar `<file>.mcp-audit.jsonl`, one JSON line per call written and synced before the result is sent, opened anew for each record, not tamper-evident
- [D121](decisions/D121.md) The built-in MCP server, part 2: `write_sql` under `--allow-write`, destructive statements confirmed by the user through elicitation or `confirm: true`, the watchdog never ending a write that began to commit
- [D122](decisions/D122.md) SQL Server's TDS protocol and a first Transact-SQL dialect: TDS 7.4 without encryption, SQL logins, names folded to lower case with `dbo` as `public`, SQL Server's batch rules and error numbers (superseded in part by D123 and D132)
- [D123](decisions/D123.md) SQL Server's types, conversions and functions in the Transact-SQL dialect: types mapped onto the engine's, SQL Server's conversion rules and styles, no file-format change
- [D124](decisions/D124.md) Transact-SQL views and CHECK constraints in the file: SQL Server is dialect code 4 in the view and CHECK records, with no new format version
- [D125](decisions/D125.md) Restoring a SQL Server script: SSMS Generate Scripts and sqlpackage scripts through `eggdb restore --dialect sqlserver`, sharing item 35's restore runner
- [D126](decisions/D126.md) Oracle's types and semantics in the Oracle dialect: exact `NUMBER` values, datetime and number format models, NLS session settings and `CURRENT_SCHEMA`, Oracle's implicit conversions and date arithmetic, and `MERGE` joined directly in `merge.rs`, with no file-format change
- [D127](decisions/D127.md) Restoring Oracle schemas from SQL*Plus-style scripts and SQLcl CSV: item 35's runner with a SQL*Plus reader, users as schemas, disabled constraints dropped and re-added, includes only through the caller's opener, no file-format change (superseded in part by D134)
- [D128](decisions/D128.md) The SQL psql's `\d` family needs: linear-time regular expressions of our own, arrays as values of expressions only, set-returning functions and `VALUES` lists as intermediate tables of the statement
- [D129](decisions/D129.md) PostgreSQL's catalog for psql's `\d` family: catalog relations built from the stored definitions once per statement, OIDs from hashes of names, PostgreSQL's display form of constraints and defaults worked out from the stored text (what it shows of indexes and identities superseded in part by D146)
- [D130](decisions/D130.md) PostgreSQL's string, date/time and JSON functions: back references and lookaround by a bounded, memoized backtracking search, PostgreSQL's template patterns in their own module, whole rows only as `FROM` item names
- [D131](decisions/D131.md) Java and Python apps on PostgreSQL's protocol: `information_schema` built per statement from the catalog snapshot with its domains as their base types, `pg_settings` and the driver functions, aggregate `ORDER BY`, lock-step set-returning functions and ranking window functions as select-list items
- [D132](decisions/D132.md) .NET and Java apps over TDS: RPC calls of the system procedures with parameters bound by name as typed values, prepared handles per connection, transaction manager requests as Transact-SQL, connection resets, and the implicit transactions and single-statement `IF` mssql-jdbc sends (the single-statement `IF` and the echoed `OUTPUT` values superseded by D137)
- [D133](decisions/D133.md) SQL Server's metadata for tools: `sys` views and `INFORMATION_SCHEMA` built per statement from the catalog snapshot with object ids from hashes of names, the catalog procedures answered in Rust by `EXEC` and RPC, table variables only for `INSERT ... EXEC`, engine types shown as the most common SQL Server type
- [D134](decisions/D134.md) Oracle SQL from apps over PostgreSQL and TDS drivers: the dialect chosen per connection with `eggdb.dialect`, transactions kept by the connection's protocol, Oracle's data dictionary as catalog relations built per statement, the session user's own schema (supersedes in part D127)
- [D135](decisions/D135.md) .NET apps on PostgreSQL's protocol through Npgsql: implicit transactions per `Query` message and per pipeline up to `Sync`, opened lazily by the first write; portals that end with their transaction; `DISCARD` and `RESET` back to the connection's startup settings; cancellation by a per-session signal registered under the backend key; `pg_proc` and `pg_range` for Npgsql's type loading
- [D136](decisions/D136.md) Bulk loads from `bcp in` and `SqlBulkCopy`: `INSERT BULK` waits in the session for rows a server streams from the bulk load message, loaded in one statement through the `INSERT` path; `SET FMTONLY` describes declared types; bcp's character format read in the engine
- [D137](decisions/D137.md) Transact-SQL batch logic: the batch compiled once into a program of jumps over the parser's statements, each statement parsed again right before it runs with variables as constants; `TRY` blocks as instruction ranges; dynamic SQL as nested batches with their own scope; `INFO` messages and raised errors with their numbers and states (supersedes the single-statement `IF` of D132)
- [D138](decisions/D138.md) ORMs and migration tools unchanged: `INSERT ... SELECT` through the `INSERT` path, advisory locks in one table per process keyed by store, Flyway with its session-level lock while there is one writer, Transact-SQL `MERGE` with `OUTPUT` on the `MERGE` executor, `SAVE TRANSACTION` recorded but not rolled back to, `OPENJSON ... WITH` as a derived table, EF Core 8
- [D139](decisions/D139.md) Collections: named key spaces as key prefixes in the file's one tree, with a catalog of names to never-reused numbers, hidden from and refused by the default key space; eggdb-jsonl version 2; the CLI, the shell and the UI as Claude Fable designed them (supersedes in part D87 and D98)
- [D140](decisions/D140.md) Expiring keys and atomic counters: deadlines as two records under the reserved prefix, by key and by deadline; time from the database's clock, never earlier than the last commit; commits sweep 32 expired keys; a read-only-compatible `EXPIRING_KEYS` feature bit; counters as decimal text changed by the one writer; compare-and-set; the CLI, the shell and the UI as Claude Fable designed them
- [D141](decisions/D141.md) Online backup and compaction: a logical copy of one snapshot, rebuilt entry by entry into a staging file, synced, checked and renamed into place, carrying the feature flags and the clock; compaction as that copy renamed over the file after a check; `eggdb backup` and `eggdb compact` as Claude Fable designed them
- [D142](decisions/D142.md) The C API: one header `eggdb.h` and the `eggdb-capi` library; opaque handles, a status from every call with the message per thread, buffers the caller frees for single values and batches the handle owns for scans and SQL results, scans that outlive their transaction, SQL values typed with their printed text, parameters as text; shipped beside each release
- [D143](decisions/D143.md) EggDB's command model: one registry crate, `eggdb-commands`, of command and tool definitions that the CLI, its help and the MCP server read; the kept names; error codes with the shared categories and per-command exit codes; JSON errors only when JSON is asked for; `restore` keeps "bring back"
- [D144](decisions/D144.md) Command model slice 2: every command in the registry, with `table list|show|sample`, `explain` and `sql --read-only` as the MCP tools' CLI twins sharing `eggdb_sql::relations` with them; `table_not_found` for the CLI with parity by category; SQLSTATE 25006 is `read_only`; `version` and `help` keep text in a pipe
- [D145](decisions/D145.md) Command model slice 3: paging by a cursor over what is stored (`row list` and the tool `row_list`, `table list`), never by offset; dry runs only where exact, as the real run on a scratch copy (`restore`, `import --csv`); confirmation checks read from the registry
- [D146](decisions/D146.md) Index methods and sort order, unique indexes that are not constraints and identity columns in the table definition: format version 9 only for tables that use them, entries still stored ascending, `GENERATED ALWAYS` enforced in PostgreSQL's dialect (supersedes in part D118 and D129)
- [D147](decisions/D147.md) Transact-SQL defaults kept as written and `IDENTITY` seeds and increments: two new default tags in table-definition version 9, the next identity number from the furthest stored one (supersedes in part D123 and D125)
- [D148](decisions/D148.md) SQL Server restore follow-ups: large batches cut by a scanner, identity shapes, alias types and `xml` rewritten by the restore, `XACT_ABORT` with uncommittable transactions, Transact-SQL sequences, `MS_Description` as comments, `GO` in `eggdb sql` (extends D125)
- [D149](decisions/D149.md) The rest of Oracle's datetime and number functions: Oracle's `INTERVAL` types on the engine's `interval`, `TIMESTAMP - TIMESTAMP` as an interval, the full datetime and number format models with Oracle's conflict errors, `ROUND`/`TRUNC` date units, `TIME_ZONE` as the session's time zone, `LISTAGG` over the array aggregate, `FETCH ... PERCENT` and `WITH TIES` in the row window (supersedes in part D113 and D126)
- [D150](decisions/D150.md) Oracle restore follow-ups: a key takes over an index named in `USING INDEX`, constraints disabled inside a column move to the table level, Oracle's identities recorded and enforced with their options (`ORA-32795`), SQL*Plus scripts in `eggdb sql` through the public `SqlPlusScript` (supersedes in part D127 and D146)
- [D151](decisions/D151.md) The rest of the query language: window functions with frames and named windows in the select-list expansion, `FILTER` and ordered-set aggregates, `LATERAL` per distinct set of named values joined back on hidden keys, `FULL JOIN ... USING` as `coalesce`, correlated subqueries at any depth, in grouped queries and in `UPDATE`/`DELETE`, data-modifying `WITH`, updatable views with check options (two view-definition flags, D35), and temporary tables and views in the session's memory (supersedes in part D112, D116 and D131)
- [D152](decisions/D152.md) User-defined types, domains, stored arrays and deferred constraints: types as catalog records with stable OIDs loaded per statement, one `Defined` type and value in the engine, domains stored as their base type with a per-column domain, table-definition version 10 (D35), foreign keys deferred to `COMMIT`, `NOT VALID`, `RENAME CONSTRAINT`, identity options and `pg_dump --clean`
- [D153](decisions/D153.md) Named time zones: the IANA source embedded and compiled per region on first use, one `TimeZone` for every dialect, PostgreSQL's rules for skipped and repeated local times; infinite intervals as PostgreSQL 17's sentinels; MySQL's full `TIME` range in `time`; SQLite `BLOB` columns as a `Dynamic` type in table-definition version 10 (supersedes in part D107, D111 and D113)
- [D154](decisions/D154.md) `COPY`'s binary format on one PostgreSQL binary codec in `eggdb-sql` shared with `eggdb-pgwire`, `ON_ERROR ignore` for type-input errors only, `COPY FROM ... WHERE` tested after each row's defaults, and client encodings (`LATIN1`, `LATIN9`, `WIN1252`, UTF-16) converted at the copy channel (supersedes in part D117)
- [D155](decisions/D155.md) A per-session plan cache keyed by statement shape (constants left out, byte offsets kept), conditions pushed into views and derived tables, and `IN`, `= ANY`, `<> ALL` and scalar-aggregate subqueries decorrelated by key (item 19e)
- [D156](decisions/D156.md) `eggdb mcp --tables` as a filter on what SQL reads from the store (hidden reads as missing in every dialect and catalog), the key-value tools withdrawn under it, read-only servers only for now, `--call-gap` between calls, and EggDB's own limits of 1 GiB per value and 2 GiB per row for every session (item 49b-2)
- [D157](decisions/D157.md) Triggers: one record per table or view in the file, firing inside the statement (a statement whose triggers run commits with them), each dialect's own order and recursion rules, bodies kept as source and run in the dialect that wrote them, at most 32 deep and 1024 per table (part 3.13)
- [D158](decisions/D158.md) Routines: one record per name holding every overload as source (`0x0C`), PL/pgSQL compiled to a checked statement tree at `CREATE` and on first use, run by an interpreter over the calling statement's writer (exception blocks undone key by key), calls in row expressions read-only, limits on depth (100), size and time; user routines in `pg_proc` and `information_schema`
- [D159](decisions/D159.md) Transact-SQL procedures and functions on the shared routine store (one routine per name, no format change), bodies compiled by the batch compiler at `CREATE`, procedures run inside the caller's batch on the session (also for RPC), functions run read-only over the calling statement's writer, 32 levels (part 3.15)
- [D160](decisions/D160.md) PL/SQL on the shared routine store: compiled to PL/pgSQL's statement tree and run by the same interpreter in Oracle's semantics (a handled exception undoes only its statement, `COMMIT` applied when the statement ends), packages as their own records (`0x0D`) with per-session state, collections as record values, units refused at `CREATE` when they do not compile (part 3.16)
- [D161](decisions/D161.md) EggDB's own TLS 1.3 and 1.2 (`eggdb-tls` on `eggdb-crypto`) for every listener: listeners beyond loopback only with TLS, which other machines must use; ChaCha20-Poly1305 first; SCRAM-SHA-256-PLUS; self-signed P-256 certificates; supersedes part of D102
- [D162](decisions/D162.md) Live copies with `eggdb follow`: PostgreSQL through `pgoutput` logical replication from a temporary slot's snapshot, SQL Server through change tracking polls, with our own PostgreSQL and TDS clients
- [D163](decisions/D163.md) The compatibility list (`docs/compatibility.md`): supported means a CI job runs or replays the tool; expected to work means every known catalog query is answered; pgAdmin's statistics views and administration functions
- [D164](decisions/D164.md) Unix domain sockets in the platform layer (`local_socket`), a `Local` server connection beside TCP and TLS, no TLS on local sockets; `eggdb serve --socket-dir` and `--mysql-socket`
- [D165](decisions/D165.md) Sockets on io_uring and I/O completion ports, on request: `NetBackend::IoUring` (batched submission, kernel cancel, Linux 5.11) and `NetBackend::Iocp` (AcceptEx, ConnectEx, one port per socket), a `net.round_trip` benchmark
- [D166](decisions/D166.md) EggDB's own memory allocator (`EggAllocator`): 40 size classes, per-thread caches, a budget, the system allocator underneath; opt-in (`--features egg-allocator`) until `allocator.small_blocks` says otherwise
- [D167](decisions/D167.md) Large values streamed a page at a time (`get_reader`, `put_reader`, `eggdb get -F raw`, `eggdb put @file`); errors roll back; the cache limit stays until M3 spills dirty pages
- [D168](decisions/D168.md) A checkpoint that cuts segments off writes one header slot; the copy to the other slot waits until a data or free-list page is about to use the cut space (three syncs per long `Normal` commit instead of four)
- [D169](decisions/D169.md) Declarative partitioning: partitions are tables linked by a catalog record (`0x0E`), routed writes, reads expanded into a pruned `UNION ALL`, MySQL's and Oracle's named partitions
- [D170](decisions/D170.md) An LSM tree made of B+trees (delta, runs, a manifest), sealed deltas without copying and merges in bounded steps, in the storage engine first; `lsm.random_put` against a B+tree
- [D171](decisions/D171.md) Compressed values: our own LZ4-format codec, overflow chains of layout 2 behind the incompatible feature `COMPRESSED_VALUES`, `Options::compress_values`
- [D172](decisions/D172.md) Concurrent writers as optimistic transactions over the single-writer engine (`begin_concurrent`): serializable by read and range validation, snapshot isolation by write validation, `ErrorKind::Conflict`
- [D173](decisions/D173.md) Savepoints by an undo log of previous values in every dialect; row and table lock clauses accepted over the one writer; isolation levels accepted, blocks serializable
- [D174](decisions/D174.md) SQL inside an embedded write transaction (`WriteTransaction::sql`) through a store that borrows it, with each statement's writes undone when it fails
- [D175](decisions/D175.md) Databases in memory (`Database::in_memory`, `:memory:`) on the platform layer's memory file through `DatabaseFile`; `PageFile::create_in`/`open_in`
- [D176](decisions/D176.md) One-line installers (`install.sh`, `install.ps1`) that check `SHA256SUMS`, run and timed by CI's `installers` job on three systems
- [D177](decisions/D177.md) `eggdb-redis-compare` in its own workspace with our own RESP client; set and get against a Redis-compatible server, reported not gated, CI job `redis-compare`
- [D178](decisions/D178.md) `ANALYZE` row counts in a catalog record (`0x10`) shown as `reltuples`; `approx_count_distinct` by a HyperLogLog sketch
- [D179](decisions/D179.md) Vectorized execution starts with batched ungrouped integer aggregates (count, sum, min, max) through SIMD-friendly lane kernels
- [D180](decisions/D180.md) Our own reader of SQLite's file format (`eggdb-sqlite-format`, WAL included), and SQLite files opened by the command line as copies in memory
- [D181](decisions/D181.md) SQLite's C API as `libsqlite3` (`eggdb-sqlite3`) with our own `sqlite3.h`, on the SQLite dialect
- [D182](decisions/D182.md) Official drivers as thin layers over the C API: Python DB-API through ctypes and a .NET ADO.NET provider through P/Invoke, tested in CI
- [D183](decisions/D183.md) Roles, users and table privileges stored in the database (SCRAM verifiers in `pg_authid`'s form, grants per table and grantee), checked where names are resolved; embedded sessions and the server's own user stay superusers
- [D184](decisions/D184.md) Several databases per server: `eggdb serve <directory>` serves each `*.eggdb` file by its stem, chosen by the startup packet's database name and listed in `pg_database`
- [D185](decisions/D185.md) External authentication in `eggdb-auth`: `eggdb serve --auth-rules` picks SCRAM, LDAP simple bind, OpenID Connect tokens (JWT over our own RSA and P-256) or reject per user; clear-text methods only over TLS or the local socket
- [D186](decisions/D186.md) Row-level security as PostgreSQL's policies (records per table, `USING` applied where names are resolved like a view, default deny); checked roles only; new rows only under `WITH CHECK (true)` until checks are evaluated
- [D187](decisions/D187.md) Encryption at rest: data pages AES-256-CTR encrypted after their header (counter from LSN and page number, key by HKDF from the user's 32-byte key), `PAGE_ENCRYPTED` page flag, incompatible feature `ENCRYPTED_PAGES`, key check at superblock offset 232; `Options::encryption_key` and `--key-file`
- [D188](decisions/D188.md) Our own HTTP/1.1 crate `eggdb-http` (bounded, GET/HEAD), process-wide session counters in `eggdb_sql::activity`, and `eggdb serve --metrics-listen` serving Prometheus `/metrics`
- [D189](decisions/D189.md) MCP over Streamable HTTP inside `eggdb serve` (`--mcp-listen`, `--mcp-token-file`): one MCP server per session id, Bearer token, `Origin` refused, read-only and loopback-only until TLS
- [D190](decisions/D190.md) Agents as stored roles: `ServerConfig::agent_role` runs MCP calls as a role, picked per token in `--mcp-token-file`, so grants, row security and `LOGIN` apply; key-value tools withdrawn for roles; MCP prompts
- [D191](decisions/D191.md) Our own GUI toolkit `eggdb-gui`: software rasterizer, our own TrueType reader over the system's fonts (listed by the platform layer), PNG encoder, the main window from Fable's `docs/ux/gui.md`, tested offscreen with a box typeface
- [D192](decisions/D192.md) Drivers over the network: `eggdb_sql_connect` gives the C API's SQL session a remote backend through `eggdb-pgwire`'s client (extended protocol for parameters, values typed by OID), so every driver takes a `postgresql://` URL
- [D193](decisions/D193.md) The RESP protocol (RESP2/RESP3) in our own `eggdb-resp`, from the public specification: each RESP database a collection `resp<n>`, each command one engine transaction, typed values with expiry; `eggdb serve --resp-listen`
- [D194](decisions/D194.md) memcached's text and basic meta protocol in our own `eggdb-memcached`, over `eggdb_resp::Keyspace` in the collection `memcached0`; `eggdb serve --memcached-listen`, loopback only
- [D195](decisions/D195.md) Full-text search in our own `eggdb-search` (English Porter2 stemmer from the published description, tsvector/tsquery text forms, phrases, ts_rank and BM25); PostgreSQL's `@@` and text-search functions first, inverted index next
- [D196](decisions/D196.md) A REST API compatible with Elasticsearch and OpenSearch in our own `eggdb-esrest`, from their public API documentation, over `eggdb_resp::Keyspace` in the collection `es0`; BM25 from `eggdb-search`; `eggdb serve --es-listen`, basic auth, loopback only
- [D197](decisions/D197.md) Documents and the MongoDB wire protocol in our own `eggdb-bson`, `eggdb-documents` and `eggdb-mongowire` from the public specifications; collections in `mongo0` through the key-value interface, now its own `eggdb-keyspace`; SCRAM moved to `eggdb-auth`; `eggdb serve --mongo-listen`, loopback only
