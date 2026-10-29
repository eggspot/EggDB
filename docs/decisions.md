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
- [D87](decisions/D087.md) The embedded Rust API: `Database`, two transaction types, double-ended scans and errors with a kind
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
- [D98](decisions/D098.md) The SQL core, part 1: a storage-agnostic `eggdb-sql` crate, SQL in a reserved key prefix, order-preserving primary keys, PostgreSQL's errors and blocks
- [D99](decisions/D099.md) SQL is slower than SQLite for now: gated, scheduled and not waived; the binary grows by the SQL core (superseded in part by D101)
- [D100](decisions/D100.md) Joins and GROUP BY in the SQL core: left-to-right joins by key lookup, hash table or nested loop, grouping in a hash table, a 1 GiB bound per statement, and the join's gap to SQLite gated and scheduled (superseded in part by D101)
- [D101](decisions/D101.md) Benchmarks are reported on pull requests and only a slowdown of more than 2x blocks; one final `check` job is the required check and auto-merge is allowed
- [D102](decisions/D102.md) Every listener binds only to loopback or a local socket until EggDB's own TLS exists (owner rule, an exception to D22)
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
- [D117](decisions/D117.md) `COPY` streams through a client channel into the `INSERT` path in one transaction, scripts carry their data inline, lines up to 1 GiB, and no server files or programs
- [D118](decisions/D118.md) `eggdb restore` reads a `pg_dump` plain dump as a stream, one transaction per statement, a new file built aside, and a report of what was skipped
- [D119](decisions/D119.md) The built-in MCP server, part 1: `eggdb mcp` over stdio, read-only by opening the file read-only for each call, one statement per call, a time, row and byte limit per call, results marked as data
- [D120](decisions/D120.md) The MCP audit log: a sidecar `<file>.mcp-audit.jsonl`, one JSON line per call written and synced before the result is sent, opened anew for each record, not tamper-evident
- [D121](decisions/D121.md) The built-in MCP server, part 2: `write_sql` under `--allow-write`, destructive statements confirmed by the user through elicitation or `confirm: true`, the watchdog never ending a write that began to commit
- [D122](decisions/D122.md) SQL Server's TDS protocol and a first Transact-SQL dialect: TDS 7.4 without encryption, SQL logins, names folded to lower case with `dbo` as `public`, SQL Server's batch rules and error numbers (superseded in part by D123)
- [D123](decisions/D123.md) SQL Server's types, conversions and functions in the Transact-SQL dialect: types mapped onto the engine's, SQL Server's conversion rules and styles, no file-format change
- [D124](decisions/D124.md) Transact-SQL views and CHECK constraints in the file: SQL Server is dialect code 4 in the view and CHECK records, with no new format version
- [D125](decisions/D125.md) Restoring a SQL Server script: SSMS Generate Scripts and sqlpackage scripts through `eggdb restore --dialect sqlserver`, sharing item 35's restore runner
- [D126](decisions/D126.md) Oracle's types and semantics in the Oracle dialect: exact `NUMBER` values, datetime and number format models, NLS session settings and `CURRENT_SCHEMA`, Oracle's implicit conversions and date arithmetic, and `MERGE` joined directly in `merge.rs`, with no file-format change
- [D127](decisions/D127.md) Restoring Oracle schemas from SQL*Plus-style scripts and SQLcl CSV: item 35's runner with a SQL*Plus reader, users as schemas, disabled constraints dropped and re-added, includes only through the caller's opener, no file-format change
- [D128](decisions/D128.md) The SQL psql's `\d` family needs: linear-time regular expressions of our own, arrays as values of expressions only, set-returning functions and `VALUES` lists as intermediate tables of the statement
- [D129](decisions/D129.md) PostgreSQL's catalog for psql's `\d` family: catalog relations built from the stored definitions once per statement, OIDs from hashes of names, PostgreSQL's display form of constraints and defaults worked out from the stored text
- [D130](decisions/D130.md) PostgreSQL's string, date/time and JSON functions: back references and lookaround by a bounded, memoized backtracking search, PostgreSQL's template patterns in their own module, whole rows only as `FROM` item names
- [D131](decisions/D131.md) Java and Python apps on PostgreSQL's protocol: `information_schema` built per statement from the catalog snapshot with its domains as their base types, `pg_settings` and the driver functions, aggregate `ORDER BY`, lock-step set-returning functions and ranking window functions as select-list items
