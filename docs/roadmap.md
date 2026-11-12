# EggDB roadmap

Each milestone ships as a release when it is merged (CLAUDE.md, "Releases"), except in the week
before v0.2.0, when releases are batched into it (D76). The order puts the core first and then
the models and protocols that most applications use (D5). The build loop may refine it through
research, recorded in docs/decisions/ (indexed in docs/decisions.md).

## Milestones
What is still to build for each milestone is listed in [What is left, per milestone](#what-is-left-per-milestone).

| # | Milestone | Status |
|---|-----------|--------|
| M0 | Repo skeleton: workspace, pinned toolchain, CI, docs | **done** |
| M1 | Platform layer: files, direct and async I/O, memory, threads, clocks, behind one interface (D8) | **done**, released as v0.1.0 (D75): files, clocks, the crash simulator (D59), direct I/O with aligned buffers (D60), memory with huge pages and its simulator (D61), CPU sets, pinning and cache padding (D62), named, pinned thread starts (D63), the first review (D64), the CPU topology (D65) and asynchronous file I/O through a completion queue with a worker pool and its simulator (D66) and io_uring behind it, with backends chosen automatically or by request (D67) done, the second review (D68), I/O completion ports on Windows on request (D69), networking's first part: TCP through a completion queue with a portable poller and a simulated network (D70), and epoll and kqueue behind it, with connects that never block (D71), and timeouts: a timed reap and cancel, with simulated time, latency and partitions (D72), and `poll(2)` on the BSDs and illumos (D73), and fewer system calls per connection and per wait: one-shot interest kept between waits, sockets made ready in one call, one accept woken per connection (D74), and the wrap-up review. The rest of the platform work (the "Missing" column below) continues alongside later milestones |
| M2 | Storage engine: our own versioned, checksummed page format, buffer management, write-ahead log ready to become a consensus log, hybrid logical clock timestamps, tenant and shard keys in the key layout (D20), checkpoints, crash recovery, partitioning; the B-tree/LSM/hybrid choice made by research with benchmarks | **in progress**, toward v0.2.0 (D76): the design and the file format's spec (D77), CRC-32C, the superblock in two header slots, page checksums, the page file with crash-safe commits, a free list and file locks (D78), the page cache with a fixed memory budget, clock eviction, pinned pages and no-steal commits (D79), benchmarks gated against a committed baseline (D80), the write-ahead log inside the file with redo on open and three durability modes (D81), the B+tree with get, put and splits up to the root, fence keys and prefix truncation (D82), delete with merges and cursors for range, prefix and reverse scans (D83), checkpoints, recovery from the last checkpoint and recycled log segments (D84), the crash sweep (D85), transactions with one writer, snapshot readers and hybrid-clock commit stamps (D86), large values on overflow pages and a check that accounts for every page (D88), and values streamed in and out a page at a time (`get_reader`, `put_reader`, `eggdb get -F raw`, `eggdb put @file`, D167), and declarative partitioning by range, list and hash with routed writes and pruned reads, in PostgreSQL's, MySQL's and Oracle's syntax (D169), rows that move between partitions on `UPDATE`, expressions in partition keys and indexes on the parent reaching every partition (D316), and an LSM tree made of B+trees with merges in bounded steps in the storage engine (D170; choosing it per table comes next), and large values compressed with our own LZ4-format codec (`Options::compress_values`, D171) |
| M3 | Transactions: MVCC snapshots, serializable isolation, deadlock-free locking, durability modes | **in progress**: concurrent write transactions in the embedded API, optimistic over the single writer, with serializable and snapshot isolation checked at commit (`begin_concurrent`, D172), savepoints in every dialect and row and table lock clauses (D173), key-value writes and SQL in one transaction (`WriteTransaction::sql`, D174) and in one `BEGIN` block of `eggdb shell` (D266); next: two-phase commit, SQL sessions on them with each dialect's isolation levels and errors, row versions so changes larger than memory spill |
| M4 | Embedded key-value API and the single-file database (D3); first SQLite and Redis comparisons and the first size, memory and startup budgets (D6); the first release binary with its download and install times measured (D14) | **in progress**, toward v0.2.0 (D76): the embedded Rust API (D87), values up to 1 GiB (D88), the first SQLite comparison and the size, memory and start-time baselines (D94); databases in memory (`Database::in_memory`, `:memory:`, D175); one-line installers checked against `SHA256SUMS` and timed in CI (D176); a Redis comparison (`eggdb-redis-compare`, D177) |
| M5 | SQL: parser, planner, optimizer and executor; SQLite-compatible embedded SQL and the SQLite file format; the stable C API and the first official drivers (Rust, C#, Java, Node.js, Python, Go) in embedded mode (D16); the `eggdb` CLI client (D15) | **in progress**: `ANALYZE` row counts and `approx_count_distinct` (D178), batched integer aggregates as the first vectorized operator (D179), SQLite files read by our own reader and queried as copies in memory (D180), SQLite's C API as `libsqlite3` (D181), the first official drivers over the C API: Python's DB-API, .NET's ADO.NET, Java's JDBC, Node.js and Go's `database/sql` (D182); next: column statistics and joins ordered by cost |
| M6a | Server and the PostgreSQL wire protocol and SQL dialect, with authentication (SCRAM), TLS and roles from the start, since the server never listens without authentication (D22, D23, D45); PostgreSQL tools (psql, pgAdmin, DBeaver) working (D12); restoring plain-SQL `pg_dump` output (D13) | **started** (item 21): the PostgreSQL protocol v3 over loopback with SCRAM-SHA-256, simple and extended query; implicit transactions per `Query` message and per pipeline up to `Sync`, portals that end with their transaction, and `CancelRequest` (item 45, D135); TLS 1.3 and 1.2 of our own on every listener, with `SSLRequest`, direct TLS, SCRAM-SHA-256-PLUS and listeners beyond loopback only with TLS (part 4.M6a-1, D161); stored roles (D183, under M6b) and `LISTEN`/`NOTIFY` between sessions of `eggdb serve`; users on a shared machine can no longer be locked out by idle sockets or have the server's memory exhausted: logins take the slot of idle ones and get 5 seconds, idle sessions close after an hour (`--idle-timeout`), and PostgreSQL statements, portals and every SQL listener's buffered bytes are capped under one memory budget ([issue #79](https://github.com/eggspot/EggDB/issues/79), D254); suspended portals' rows, RESP, MongoDB, CQL and memcached hold against that budget and `--max-connection-memory` sets its size (D284); users can now keep Kafka, MQTT, AMQP 0-9-1 and 1.0, NATS, STOMP, Bolt and HTTP clients and RESP pub/sub queues within that one budget, and print connection options and roles in debug output without their secrets (D307); Next: measure replies (RESP `EXEC`, single large values, a Bolt record, HTTP response bodies) before they are built |
| M6b | The rest of security: LDAP, Kerberos, OpenID Connect, client certificates, row- and column-level security, masking (D22) | **started**: stored roles and table privileges (D183), several databases per server (D184), created and dropped while serving with `CREATE DATABASE` and `DROP DATABASE` over every protocol (D270), refusing a `USE` that would leave a temporary table or session lock behind (D294), LDAP and OpenID Connect logins per user (D185), row-level security policies (D186), column privileges and evaluated `WITH CHECK` policies (D285), encryption at rest (D187); left: Kerberos, client certificates, masking |
| M6c | Operations: metrics, logs, slow queries, statement statistics, `EXPLAIN ANALYZE` (D25) | **started**: Prometheus metrics from `eggdb serve --metrics-listen` over our own HTTP crate (D188); statement statistics as `pg_stat_statements`, the slow-statement log (`--log-slow-statements`) and `EXPLAIN (ANALYZE, BUFFERS)` (D286); users can now read `(actual time=a..b rows=n loops=1)` on the top node, scans, a join's top, sorts and `Append` branches in `EXPLAIN ANALYZE`, and run Oracle's `EXPLAIN PLAN` with `DBMS_XPLAN.DISPLAY` and SQL Server's showplan options, also through `eggdb explain --dialect oracle|sqlserver` and MCP's `explain_query` on a read-only file (D312); users can now see subqueries as `InitPlan`/`SubPlan` nodes with their loops in `EXPLAIN [ANALYZE]` and use MySQL's `EXPLAIN FORMAT=TREE` and `EXPLAIN ANALYZE` (D327); users can now read MySQL's `EXPLAIN FORMAT=JSON`, `(cost=... rows=...)` estimates with `Aggregate`, `Group aggregate` and `Stream results` in the tree, and explain (and `EXPLAIN ANALYZE`) multi-table `UPDATE`/`DELETE` and `TABLE t` (D345); left: logs (OTLP), per-node buffers, times on the steps inside a join, wait events, `pg_stat_activity` for every session |
| M6d | The built-in MCP server: schema discovery, querying, agent permissions, prompt-injection safety (D11, D45) | **in progress**, brought forward to v0.2.0 (owner, 2026-10-28): `eggdb mcp` over stdio, read-only, with an audit log (item 49a, D119, D120); writes with `--allow-write` and the user's confirmation (item 49b-1, D121); `--tables`, a gap between calls and engine-wide value and row limits (item 49b-2, D156); confirmation for every statement that can change many rows ([issue #80](https://github.com/eggspot/EggDB/issues/80), D255), `--tables` with `--allow-write` (item 49b-3, D257), column lists and referenced-table inserts (item 49b-4, D281), writers that wait for a busy file and an engine-side deadline and row cap (D258), busy waits for every file command and set-returning functions stopped by `LIMIT` (D287), and writes to tables listed with columns, inserts into a listed table whose foreign key points at an unlisted one and column lists on views (D306); key writes with `put` and `del` and `--mcp-tables` on `eggdb serve` (D328); counters, compare-and-set and times to live as tools (`incr`, `cas`, `expire`, `persist`) and `expires_at` in `put`'s result (D346); next: the rest in [What is left](#what-is-left-per-milestone), M6d |
| M6e | The first GUI client screens on the desktop and in the browser, designed by Claude Fable (D15) | **started**: our own GUI toolkit `eggdb-gui` (software rasterizer, TrueType reader, PNG encoder) drawing the main window from Fable's `docs/ux/gui.md`, tested offscreen (D191); users can now open the connect screen in native windows on Linux (X11), Windows (Win32, per-monitor DPI) and macOS (Cocoa, Retina) with the separate `eggdb-gui` binary, in light, dark and high contrast with a keyboard focus ring, with its focus, names and roles exposed to VoiceOver, Narrator and Orca (NSAccessibility, UI Automation, AT-SPI2; not yet checked by hand), and see it in a browser on a canvas drawn by the same toolkit ([issue #29](https://github.com/eggspot/EggDB/issues/29)'s first slice, D256); users can now open a typed, browsed (macOS and Windows file dialogs) or dropped `.eggdb` file from the connect card, or connect to a server from its `Connect to a server` segment (its `URL`, in libpq form or as `eggdb://`, filling the fields, with `Use TLS`), see the open's progress and stop it with `Esc`, read the `Error`/`Try:` lines when it fails (a server's on the field they are about), and land in the main window on it (D274); users can now write SQL in the main window's query tab with the dialect pill (PostgreSQL, MySQL, SQL Server, Oracle, SQLite, re-highlighting the same text), run the statement under the caret with `Ctrl/Cmd+Enter` or all of them with `Ctrl/Cmd+Shift+Enter` without the window blocking, stop it with `Esc`, read the summary (`3 rows · 0.4 ms · 1.2 KB`) and the engine's errors inline (squiggle, `Error 42P01: … · Ln 1, Col 15`, `Messages`), and move, select and copy (TSV, with header, as `INSERT`) in a virtualized grid that keeps 100,000 rows with a banner (D275); users can now browse the database in the sidebar's tree (tables with their columns and indexes, views, indexes, sequences and key spaces with their counts), filter it, move through it with the arrows, `Home`/`End`, `*` and type-ahead, open a table with `Enter` as `SELECT * FROM <name> LIMIT 100;` in a new query tab, insert a name with `Shift+Enter`, and read the inspector's columns, indexes, DDL and sample rows, docked from 1440 px and an overlay with `Ctrl/Cmd+I` below (D276); users can now press `Ctrl/Cmd+E` to see the statement under the caret's plan as a tree in the results' `Plan` segment (cost and rows per node when the engine gives them), `Ctrl/Cmd+Shift+E` to measure it (own time, rows and buffers per node, writes rolled back), spot hot nodes by their `warning`/`error` borders and printed share, walk and select nodes from the keyboard with the node's table highlighted in the editor, and read the text form with `Alt+T` (D277); screen readers now read the connect card's and the main window's values (with the caret and selection), states (checked, expanded, selected, invalid, read-only), shortcuts and the grid's and trees' rows, columns and levels, and press buttons, tick checkboxes, expand tree rows, move the focus and type values through UI Automation's patterns, AT-SPI2's `Action`, `Text`, `EditableText`, `Selection` and `Table`, NSAccessibility and ARIA (D278); users can now point and click: hover and pressed fills, a click focuses inputs, the editor, grid cells and tree rows with the caret at the click, a drag selects, a double-click selects a word or opens a tree row, a release outside a button cancels it, the wheel scrolls the grid and the tree, `Ctrl/Cmd+C`, `X` and `V` use the system clipboard on macOS, Windows and X11 (a paste over 1 MB is refused), the caret blinks and greys when the window is inactive, and the window relayouts on every resize (D279); users can now pick a familiar layout (works like SQL Server Management Studio, pgAdmin 4, DBeaver, DataGrip, MySQL Workbench, MongoDB Compass or Redis Insight) from the connect card's `Where are you coming from?`, `Ctrl/Cmd+K L` or the status bar's `Works like` item, and the main window lays out with that layout's regions, run button, result view and tree names and keys (`F5` executes in SSMS's), kept in the client's settings for the next start (D309); users can now open more query tabs (`Ctrl/Cmd+T`, the strip's `+`), move between them (`Ctrl+Tab`), close them (`Ctrl/Cmd+W`, middle-click, asking first when one runs, holds a transaction or text), and run each on its own session kept between runs, so a `BEGIN` outlasts its run, with the open transaction shown on the tab and in the status bar (D310); users can now open a cell's full value with `Enter`, `F2` or a double-click (JSON pretty-printed, blobs in full hex, copied whole), right-click the grid or press `Shift+F10` or the menu key for `Copy`, `Copy with header`, `Copy as INSERT`, `Copy as CSV`, `Copy as JSON`, `Open value` and the select items, resize columns by dragging a header edge, fit one with a double-click on it or `Alt+Enter` and step it with `Alt+Left`/`Alt+Right`, widths kept across runs with the same columns, and page and drag the grid's scrollbars, with the editor's showing its position (D310); users can now see each table's triggers in the tree's `Triggers` group (events as right text, the action in the inspector, `Triggers (n)` in a table's), right-click a tree row or press `Shift+F10` for `Copy name`, `Show DDL`, `Refresh` and `Drop…` (the statement in the tab's dialect, run in its own session after a confirm dialog), press `Enter` on a key space for a tab of its first 100 keys, values and sizes, drag or key the sidebar, results and inspector splitters with the sizes kept per layout, collapse the sidebar to a 36 px rail with its drawer with `Ctrl/Cmd+B`, use windows from 800 px wide with the docked inspector closing under 1440, and see hover and pressed fills fade over 80 ms, changing at once when the system's reduce-motion setting (macOS, Windows, X11; read at start and when the window gains the focus) or `--reduced-motion` asks (D311); users can now explain in Oracle's dialect (`EXPLAIN PLAN FOR` with `DBMS_XPLAN.DISPLAY`, rolled back) and SQL Server's (`SET SHOWPLAN_ALL`, and `SET STATISTICS PROFILE` rolled back with `Ctrl/Cmd+Shift+E`) in the plan view, hover a node, read its full text in a tooltip after 600 ms or with `Ctrl/Cmd+Shift+Space`, and click a node to select it, and see the own time and share of the top node, scans, a join's top, sorts and `Append` branches from EggDB's own `Ctrl/Cmd+Shift+E` (D312); users can now click, drag and double-click in the browser card as on the desktop, copy and paste there through the async clipboard when the browser sends no clipboard event, copy and paste text larger than one X11 request (`INCR`), and see the drag-over ring and drop files on Windows (an OLE drop target) and on X11 (XDND) (D313); users can now right-click the editor or press `Shift+F10` for `Cut`, `Copy`, `Paste`, `Select all`, `Run statement`, `Explain`, `Toggle comment` and `Format statement`, filter a keys tab by a key prefix and page it 100 keys at a time with `Next 100` and `Back`, browse a RESP server's keys over `redis://` with each key's type, and see `Show DDL` move the focus to the DDL box (D329); left: checking the screen readers by hand (VoiceOver, Narrator, Orca and a browser screen reader), UI Automation's Text pattern, IME, Wayland, streaming rows into the grid, document and other model groups, times in the plan view for the steps inside a join (scans, a join's top, sorts and `Append` branches have them, D312), the remaining screens; Next for the familiar layouts (D309): each layout's own region contents (pgAdmin's Query Tool toolbar, DBeaver's run column, Compass's filter bar and document cards, Redis Insight's result cards and CLI, Workbench's output rows, SSMS's `F4` properties), the hidden extra regions, per-layout splitter memory, `View > Layout` and the palette, `F7` in the platform layer, comment/format/new-tab keys, the connect screen's `Coming from another client?` row, the picker in the browser build and dialog/list box/option accessibility roles; Next for the query tabs (D310): the `Transaction open` item's `COMMIT`/`ROLLBACK` popover and its click on another tab's item, disabled rows in the locked dialect menu, a double-click on the strip adding a tab, the tab strip in the familiar layouts' own tab rows, naming the tab that holds a file's write transaction in another tab's `55P03` error (`Query 1 holds the write transaction; COMMIT or ROLLBACK it first`; a file's tabs share one session thread, so the second writer fails at once instead of waiting), a session advisory lock (`pg_advisory_lock`) wanted by a second tab of a file while another holds it, which waits until `Stop` because the holder cannot run meanwhile (run each tab's session on a thread of its own, or answer `55P03` at once), and a Fable design for the 400-tab cap's message; Next for the tree, splitters and motion (D311): the whole value in a keys tab's value view (its cell keeps the first 64 bytes), `alertdialog` and `menu`/`menuitem` roles for the Drop dialog and the row menu, the results grid growing into a taller results region, the results and inspector splitters as `Tab` stops, the resize cursor, the rail's hover, tooltips and accessibility roles (and the splitters' and drawer's), the drawer returning focus to where it was, splitters in the familiar layouts, hover fills that fade on tree rows, grid rows, rail items, status bar pills, dialog buttons and menus' highlighted rows, redrawing at the display's rate, the OS's reduced-motion change notifications, the browser's `prefers-reduced-motion`, Wayland's settings portal and `View > Reduce motion`; Next for the platform layer (D313): dropping files on the browser card (opened in memory), touch gestures beyond a tap and a drag, `INCR` for `PRIMARY`, XDND's `XdndProxy`, actions other than copy and a landing property per drag, dragging out of the client (`DoDragDrop`, an XDND source), Wayland's data device and a hand check on real desktops; Next for the grid (D310): the value view's partial text selection and the column's SQL type in its header, the menu, dialog and scroll bar accessibility roles in the platform layer (the popovers expose groups and buttons now), the scrollbars hiding 800 ms after the last scroll and fading (they stay shown, the reduced-motion look), the editor's own scroll offset with a draggable thumb and a horizontal scrollbar, and the column-resize pointer cursor |
| M6f | The official drivers over the network and their main ORMs (D16, D17) | **started**: the C API's SQL session over the network through `eggdb_sql_connect` and a `postgresql://` URL, which every driver built on it inherits (D192); left: the per-language drivers' own network tests and their ORMs |
| M7 | Caching: in-memory tables with expiry and eviction, pub/sub, streams; Redis RESP | **started**: RESP2/RESP3 with typed values and expiry (`eggdb serve --resp-listen`, D193), memcached's text and meta protocol on loopback (D194); users can now restore Redis 7's multi-part AOF by pointing `eggdb restore` at `appendonlydir/`, replayed in the manifest's order with hostile manifests and RDB preambles refused (D13, D193); users can now run transactions (`MULTI`/`EXEC`/`WATCH`), pub/sub, streams without consumer groups, and `maxmemory`-style eviction under `--resp-max-memory` and `--resp-eviction` (D193 parts 4.M7-2 and 4.M7-3), with pub/sub queues held against the server's memory budget (D307). Users can now block on lists, sorted sets and streams (`BLPOP`, `BLMOVE`, `BZPOPMIN`, `BLMPOP`, `XREAD BLOCK` and the rest, served first-come first-served) and use `LMOVE`, `LMPOP`, `ZPOPMIN` and `ZMPOP` (D347). Next: consumer groups, scripting, ACL users and cluster mode, in [What is left](#what-is-left-per-milestone), M7 |
| M8 | Full-text search: analyzers, inverted indexes, BM25 ranking; Elasticsearch/OpenSearch REST | **started**: an Elasticsearch/OpenSearch-compatible REST API on the key-value store (`eggdb serve --es-listen`, D196) |
| M9 | Documents and JSON; the MongoDB wire protocol | **started**: BSON, a document store with MongoDB's query and update operators and aggregation pipeline, the MongoDB wire protocol (`eggdb serve --mongo-listen`) and `mongodump` restores (D197); users can now update array elements with `$`, `$[]` and `$[<identifier>]` with `arrayFilters` (nested too), and send pipeline-style updates (`$set`/`$addFields`, `$project`, `$unset`, `$replaceRoot`/`$replaceWith`) in `update` and `findAndModify`, with MongoDB's error codes (D349) |
| M10 | The other SQL dialects, each as itself (D10): MySQL/MariaDB, Microsoft SQL Server (T-SQL, TDS) and Oracle (SQL, PL/SQL) | **started**: restoring `mysqldump` output (D198), MySQL stored procedures and functions (D199), `pg_cron` jobs run by `eggdb serve` (D200) |
| M11 | Columnar storage and analytics; time series | **started**: columnar encodings and integer chunks (D201), `time_bucket`, Parquet import and export (D202) |
| M12 | Vectors (approximate nearest neighbours) and geospatial | **started**: HNSW vector indexes with pgvector-style types and operators (D203), geometry functions (D204), built-in embeddings and hybrid ranking (D205) |
| M13 | Graphs | **started**: property graphs with Cypher and graph algorithms (D206), the Bolt protocol (`eggdb serve --bolt-listen`, D207), algorithms as `CALL` procedures with communities, triangles and similarity (D248), Cypher's named paths, shortest paths, `MERGE` actions, `FOREACH`, `UNION` and comprehensions (D333), label predicates, `CALL {}` subqueries and shortest paths as expressions (D351) |
| M14 | Backup, point-in-time recovery and verified restores (D24); replication and high availability: read replicas (synchronous, asynchronous, cascading), failover, consistency per request (D18) | **started**: continuous archiving and point-in-time restore (D208), dumps in every dialect's plain SQL (`eggdb export --sql`), read replicas and failover (D209), change streams through a transactional outbox to webhooks and queues (D210, D217), materialized views |
| M15 | Sharding (hash, range, time, region) with online resharding, distributed SQL with consensus and serializable distributed transactions, multi-region clusters (D4, D18) | **started**: Raft consensus checked by deterministic simulation (D211), shard maps with online moves and two-phase commit across shards (D212) |
| M16 | Messaging and queues (D33), in order: Kafka, AMQP 0-9-1 (RabbitMQ), MQTT, SQS, NATS, AMQP 1.0, STOMP, with transactional outbox and inbox; wide-column (CQL), objects (S3), InfluxDB line protocol | **started**: Kafka (D213), AMQP 0-9-1 and 1.0 on durable queues (D214, D217), MQTT (D215), SQS and NATS (D216), STOMP and the outbox relay (D217), Cassandra CQL on SQL tables (D218), S3-compatible objects (D219), InfluxDB line protocol and Prometheus remote write with PromQL (D220); syslog over UDP and TCP into SQL (D315); heart-beats for STOMP and AMQP 0-9-1, and syslog idle limits and dropped-message counts (D352); DynamoDB-compatible tables and queries (D241) |
| M17 | Every platform: Windows, macOS, BSDs, Android, iOS, EggOS, WebAssembly, `no_std` (D8); packages and cloud deployments | **started**: EggDB in the browser as a WebAssembly (WASI) module with OPFS persistence and a playground page (D221); Android through a Kotlin wrapper over JNI and the C API, iOS through an XCFramework (D222); the file format without the standard library, with pages on raw flash, built for Cortex-M (D223); tests on 32-bit x86 and ARM and big-endian s390x in CI; the image on GHCR for amd64 and arm64 with AWS, Azure and Google Cloud templates (D224) |
| M18 | The other ways of scaling (D18): multi-primary, leaderless quorums, storage separated from compute, serverless, tiered storage, edge copies, offline-first sync, federation | **started**: leaderless quorums with version vectors, last-writer-wins or siblings, read repair, hinted handoff and anti-entropy (D225); archives in S3-compatible buckets and nodes that start from them (D226); read-only edge copies cached by their SHA-256 (D227); offline-first sync of key-value data (D228); federation through `eggdb_foreign` over allowed EggDB, PostgreSQL and Parquet sources (D229) |

## Next: v0.2.0, the first user release (2026-11-01, D76)
An embedded, crash-safe, transactional key-value store in one file, with a Rust API, the
`eggdb` command-line tool and `eggdb ui`, a terminal UI. It brings forward M2's storage
engine, a single-writer slice of M3, M4's embedded API and the one-shot commands of M5's CLI,
and the first part of SQL (item 19) if it is merged before the freeze, served to psql and
PostgreSQL drivers on loopback (item 21). No TLS, other protocols, compression or concurrent
writers yet. Feature freeze
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
| 18 | Release engineering: CLI smoke tests on every target, a committed v0.2.0 sample file CI keeps reading, the README quickstart, `docs/embedded.md`, `docs/cli.md`, release notes, the correctness statement | **done**: `scripts/smoke-test.sh` runs every command, an export and import round trip, the sample files and a scripted `eggdb ui` session in a pseudo-terminal against each release binary its runner can execute, and against the debug binary in CI on Linux, macOS and Windows; two v0.2.0 sample files and an export, pinned by their bytes and read, checked, replayed and written to by `sample_files.rs`; the README quickstart, the guides' Rust examples compiled by `cargo test` and the UI screens in [cli.md](cli.md) checked against the UI's own frames; [correctness.md](correctness.md) and [releases/v0.2.0.md](releases/v0.2.0.md) (D96); users can now follow the README, `eggdb help` and the v0.2.0 notes on macOS, Windows and Linux as written: help names every dialect and protocol, and every release archive is unpacked and smoke tested with psql, mysql and sqlcmd where the runner has them, as the notes' "What was tested" lists ([issue #81](https://github.com/eggspot/EggDB/issues/81)) |
| 17b | Batched random overwrites at least as fast as SQLite's, required by D7 from item 17 ([issue #52](https://github.com/eggspot/EggDB/issues/52)) | **done**: a flush longer than the recycled log segments takes its new segments first and ends in a recycled one, and the checkpoint after it cuts the new ones off the file instead of freeing them, so long commits no longer grow the file (issue #44) and EggDB takes 0.69 times SQLite's time on a Mac and 0.61 times on Linux; `Normal` stays about 1.1 by two header syncs that correctness needs, decided by research ([benchmarks.md](benchmarks.md), D97) |
| 19 | SQL core: a catalog and tables on the key-value engine (row encoding, primary keys, secondary indexes), the PostgreSQL dialect first: CREATE/DROP TABLE, INSERT, SELECT with WHERE, ORDER BY, LIMIT, inner and left joins, GROUP BY with aggregates, UPDATE, DELETE, BEGIN/COMMIT/ROLLBACK, the common types; run with `eggdb sql` | **part 1 done**: `eggdb sql` runs SQL from `-c`, a script or stdin with psql's output and PostgreSQL's errors and exit codes; the `eggdb-sql` crate over any ordered key-value store with tables, primary keys and versioned rows under a reserved key prefix, `CREATE`/`DROP TABLE`, `INSERT` with `DEFAULT` and `RETURNING`, `SELECT` with `WHERE`, `ORDER BY`, `LIMIT` and `OFFSET`, `UPDATE`, `DELETE`, transaction blocks with PostgreSQL's aborted state, the integer, float, boolean and text types, three-valued logic and `0A000` for the rest ([sql.md](sql.md), D98) |
| 19b | Users can join tables (inner and left joins) and summarise them (`GROUP BY` with `count`, `sum`, `avg`, `min`, `max`, `HAVING`, `DISTINCT`) | **done**: inner, left and cross joins with `ON`, `USING` and comma joins, aliases, qualified names and `t.*`, by key lookup, hash table or nested loop, decoding only the columns a statement names and applying each `WHERE` part as soon as its tables are joined; `GROUP BY` with `count`, `sum`, `avg`, `min`, `max` (with `DISTINCT`), `HAVING`, grouping by a primary key, `SELECT DISTINCT`, PostgreSQL's errors (`42702`, `42712`, `42803`, ...), a 1 GiB bound on what a statement holds (`53200`), a differential test against SQLite; `RIGHT`/`FULL` joins, subqueries and the other aggregates answer `0A000` ([sql.md](sql.md), D100) |
| 19c | Users can create secondary indexes (`CREATE INDEX`, `UNIQUE`) that speed up `WHERE` and `ORDER BY` | **done**: `CREATE [UNIQUE] INDEX`, `DROP INDEX` and `UNIQUE` columns and constraints in the PostgreSQL, MySQL (`KEY`, `UNIQUE KEY`, `DROP INDEX ... ON`), SQLite and Oracle dialects; one entry per row beside the rows on the key-value engine, written in the row's transaction by every change, filled in bounded batches by `CREATE INDEX`, uniqueness across the statement with many NULLs allowed, table definitions in format version 3 only for tables with indexes; ranges on a leading part of a key for `=`, `IN`, `IS NULL`, `<`, `<=`, `>`, `>=`, `BETWEEN` and `LIKE 'prefix%'`, `ORDER BY` from a key read forward or backward with `LIMIT` stopping early, join lookups through an index; `EXPLAIN` in PostgreSQL's and MySQL's style and SQLite's `EXPLAIN QUERY PLAN`; `SHOW INDEX`, `PRAGMA index_list`/`index_info`, `USER_INDEXES`/`USER_IND_COLUMNS` and `.schema`; each dialect's duplicate-key errors; a check of every index against its rows, a differential test against full scans, a killed-process test and a fuzzer for index statements and hostile definitions; indexed lookups at 1.9 times SQLite's time on a Mac, gated ([sql.md](sql.md#indexes), D109) |
| 19d | Users can store `numeric`, `date`, `timestamp` and `json`/`jsonb` values and use `serial` and identity columns over `smallint`, `integer` and `bigint` | **done**: exact `numeric(p,s)` (EggDB's own decimal, 1,000 digits each side of the point, PostgreSQL's scales and rounding, `numeric field overflow`, `NaN`, `round`/`trunc`/`ceil`/`floor`/`abs`, exact `sum` and `avg`), `date`, `timestamp(p)` and `timestamptz(p)` over PostgreSQL's ranges with the session's `TimeZone` (UTC and fixed offsets), `now()`, `current_date`, `extract`, `date_part`, `date_trunc` and date arithmetic by days, `json` and normalized `jsonb` with `->`, `->>`, `#>`, `#>>`, `@>`, `<@` and the type and length functions, `serial`, `bigserial`, `smallserial` and identity columns; MySQL's `DECIMAL`, `DATE`, `DATETIME`, `TIMESTAMP`, `JSON` and clock functions and Oracle's `NUMBER(p,s)`, `DATE`, `TIMESTAMP`, `JSON`, `SYSDATE` and `SYSTIMESTAMP`; the PostgreSQL protocol's OIDs and text and binary forms (numeric in base 10,000) both ways and MySQL's `NEWDECIMAL`, `DATE`, `DATETIME`, `TIMESTAMP` and `JSON` columns with packed binary dates; table definitions in format version 4 only for tables with the new types; order-preserving index keys for every new type; a differential test of the decimal arithmetic, an indexed-versus-full-scan test over the new keys, fuzzing of the new syntax in the four dialects and of hostile rows, keys and wire values; pg8000 and PyMySQL round trips in CI ([sql.md](sql.md), D111) |
| 19e | Users get SQL point `SELECT`s, bulk `INSERT`s and joins at least as fast as SQLite's (required by D7 before v0.6.0, D99, D100) | **in progress**: joins by primary key now take 0.99 times SQLite's time on a Mac and 1.40 on Linux (from 3.5 and 3.9), `GROUP BY` 0.94 on a Mac and 0.86 on Linux, gated; the Linux join is not at parity, D7 not met there, and it must reach 1.0 before v0.6.0 after Linux profiles of the hash build's allocations, `Value` clones and the probe loop (D100); the Linux point-select line moves from a low sample of 0.85 to 0.91 after an A/B against main showed no added cost (D100); from 1.95 to 1.23 times SQLite's time for point selects (1.25 to 0.85 on Linux) and from 3.16 to 0.89 for bulk inserts (2.63 to 0.83 on Linux), now faster than SQLite (cached definitions by commit, borrowed tokens, streamed rows, single-descent `put_if_absent`, splits that move entries, append splits at the rightmost leaf without a descent, a direct `INSERT ... VALUES` path, D99); Users can now repeat a `SELECT` with other constants without it being parsed again (a per-session plan cache keyed by the statement's shape), look rows up through a view by its table's key (`WHERE` conditions pushed into views and derived tables), and run correlated `IN`, `= ANY`, `<> ALL` and aggregate subqueries once instead of once per outer value (D155; lines `sql_view_point_select`, `sql_correlated_in`, `sql_scalar_aggregate`); users' joins by key now skip copying the joined table's values that nothing reads after the join (hash joins store only the columns the query still needs, D100); users' joins now do less per row: one cancel check per level through a watch taken once per statement, no dialect lookup for comparisons on fixed-type columns, joined rows projected without an intermediate copy, and index-lookup joins with reused key buffers (D100 note), though CI measured the Linux join at 1.66 times SQLite's time on this batch (1.50 on main), so next: the `perf` profiles in the weekly performance pass; left: the Linux join's `perf` profiles (D100) and the 1.0 check of every line on Linux and macOS in CI's `compare` job; first, the weekly pass finds which change moved point selects and bulk inserts above SQLite's time on both systems (1.67 and 1.24 times on Linux, 2.34 and 1.31 on macOS on `main` after Batch 5, D100's v0.4.0 note) |
| 20 | `eggdb shell`: an interactive prompt with history and tab completion for key-value commands and SQL; a query screen in `eggdb ui`; readable errors in each dialect's style; a correctness suite from each database's documented behaviour | **done** for PostgreSQL (D105): `eggdb shell` with line editing, a history file, `Ctrl-R`, completion of commands, tables, columns, keywords and keys, `.tables`/`.schema`/`.mode`/`.timer`, `BEGIN` blocks across prompts and a batch mode (`-c`, stdin, `--keep-going`); the `s` query screen in `eggdb ui`; a 186-case suite from the PostgreSQL manual, 12 marked as known differences (sql.md); each later dialect (22 to 25) adds its suite; users can now mix key-value writes and SQL in one `BEGIN` block: `put`, `get`, `del`, `incr` and `decr` join the block's transaction, so `COMMIT` keeps both and `ROLLBACK` drops both (D266); `expire`, `persist`, `cas`, `scan` and `ttl` run in the block too, seeing its own writes, with times to live kept by `COMMIT` and dropped by `ROLLBACK` (D283). Key-value writes run while a `SAVEPOINT` is set and `ROLLBACK TO SAVEPOINT` undoes them with the SQL changes, times to live and collection entries included (D319) |
| 21 | The PostgreSQL wire protocol v3: psql and drivers connect (simple and extended query, SCRAM-SHA-256 with our own SHA-256, HMAC and PBKDF2 checked against published vectors, SSLRequest answered no), CI connects with psql and a common driver; every listener binds only to loopback or a local socket, without TLS yet, and requires authentication (recorded when it lands) | **done**: `eggdb serve` on TCP loopback only (D102), the `eggdb-pgwire` crate with SCRAM-SHA-256 over our own `eggdb-crypto` (FIPS 180-4, RFC 4231, RFC 7914 and RFC 7677 vectors), the simple and extended query protocols with text and binary parameters and results, PostgreSQL's tags, errors, statuses and reported parameters, `SET`/`SHOW`, a thread per connection up to 64, a graceful stop, hostile-message fuzzing; CI runs psql (create, insert, join) and the pg8000 driver (D103, D104). Built since: TLS, Unix sockets (D267), `pg_catalog` for `\d` and GUI tools (items 37 and 3.19–3.22), `COPY` (item 34), `LISTEN`/`NOTIFY`, cancel (item 45) and stored roles (D183) |
| 22 | The MySQL/MariaDB dialect and client/server protocol | **done**: `eggdb serve --mysql-listen` on TCP loopback only (D102) beside the PostgreSQL listener, the `eggdb-mysqlwire` crate with `caching_sha2_password` (fast path) and `mysql_native_password` over our own SHA-256 and SHA-1 (FIPS 180 and known-answer vectors), `COM_QUERY` with several statements, prepared statements with binary parameters and rows, MySQL's errors, status flags and session variables, hostile-packet fuzzing; the MySQL dialect in `eggdb sql`, `eggdb shell` (`--dialect mysql`) and the server: backticks, `LIMIT a, b`, `AUTO_INCREMENT`, `INSERT IGNORE`, `REPLACE`, `ON DUPLICATE KEY UPDATE`, MySQL's types as the engine's, `SHOW`/`DESCRIBE`/`USE`/`SET`, MySQL's transactions and a 118-case suite from the reference manual, 3 marked as known differences (sql.md); CI runs the `mysql` client and the PyMySQL driver (D106). Built since: TLS, dates and times, `DECIMAL`, `information_schema` for GUI tools and `CREATE USER` (D183). Next: collations beyond `utf8mb4_0900_ai_ci` (M5), `LOAD DATA` (M10) and MySQL logins for stored roles (M6b), in [What is left](#what-is-left-per-milestone) |
| 23 | The SQL Server T-SQL dialect and the TDS protocol | split into 23a and 23b (owner, 2026-10-27) |
| 24 | The SQLite dialect through `eggdb sql --dialect sqlite` and the embedded API | **done**: `--dialect sqlite` in `eggdb sql` and `eggdb shell`, `eggdb::sql` in the embedded API; SQLite's lexical rules and parameters, type affinity, `INTEGER PRIMARY KEY` row ids and `AUTOINCREMENT`, `INSERT OR ...`, `REPLACE`, upserts with `excluded`, `LIMIT m, n`, `WITHOUT ROWID` and `STRICT` accepted, `sqlite_schema`, `PRAGMA table_info`/`table_list`/`foreign_keys`/`user_version`/`journal_mode`/`synchronous`, SQLite's transactions, scalar functions and errors with result codes, and a 155-case suite from sqlite.org's documentation, 3 marked as known differences (sql.md, D107). Built since: blobs and dynamic typing in `BLOB`, `TEXT`, `NUMERIC`, `REAL` and untyped columns with keys and indexes on them (D153, D305), dates and times, indexes and `UNIQUE` (item 19c), and reading SQLite's file format (D180). Next: `INTEGER` columns as dynamic (D305) and writing SQLite files (D180), in [What is left](#what-is-left-per-milestone), M5 |
| 25 | The Oracle dialect (`DUAL`, `FETCH FIRST`/`ROWNUM`, `NVL`, `VARCHAR2`) through `--dialect oracle`; no Oracle wire protocol (D10) | **done**: `--dialect oracle` in `eggdb sql` and `eggdb shell`, `Dialect::Oracle` in the embedded API; names folded to upper case on the shared catalog, the empty string as NULL, `DUAL`, `ROWNUM` in `WHERE`, `OFFSET`/`FETCH FIRST`, `VARCHAR2`, `NUMBER(p,s)`, `CLOB`, `BINARY_DOUBLE` and identity columns onto the engine's types, Oracle's string, numeric and conversion functions with `TO_CHAR` format models, `USER_TABLES`/`ALL_TABLES`/`USER_TAB_COLUMNS`, implicit transactions with DDL commits, `TRUNCATE TABLE`, `ORA-` errors and a 164-case suite from the SQL Language Reference, 8 marked as known differences (sql.md, D108). Built since: dates, sequences, `MERGE`, set operators, subqueries, `LISTAGG` (with its analytic form, D300), `ROWNUM` in the select list and exact `NUMBER`; no Oracle wire protocol (D10) |
| 30 | Users can write subqueries (scalar, `IN`, `EXISTS`, in `FROM`), `WITH` queries, `UNION`/`INTERSECT`/`EXCEPT`, and `RIGHT`/`FULL` joins; check: the PostgreSQL manual's cases and a differential run against the `postgres` container | **done** (D112): scalar, `IN`, `EXISTS` and `ANY`/`ALL` subqueries, correlated ones run per distinct outer value with key and index lookups, derived tables with column lists, `WITH` and `WITH RECURSIVE`, `UNION`/`INTERSECT`/`EXCEPT` (`MINUS` in Oracle) with `ALL`, parentheses and `ORDER BY`/`LIMIT` on the result, `RIGHT` and `FULL` joins, `greatest`/`least`, standalone subqueries in `INSERT`, `UPDATE` and `DELETE`, in all four dialects with their errors; 116 new correctness cases, a seeded differential test against join forms and a reference in Rust with and without indexes, fuzzing in every dialect. The differential run against the `postgres` container waits for that container in CI. Built since, in item 30b (D151): `LATERAL`, `FULL JOIN ... USING`, correlated subqueries at any depth, in grouped select lists and in `UPDATE`/`DELETE`, data-modifying `WITH`, and `VALUES` as a query in PostgreSQL. Next: `VALUES` as a query in the other dialects (M5) |
| 31 | Users can store `time`, `interval`, `bytea`, `uuid` and `char(n)`, sent to clients in text and binary form; check: every type round-trips through a standard driver as PostgreSQL prints it | **done** (D113): `time(p)`, `timetz`, `interval` with fields and precision (PostgreSQL and ISO 8601 input, `postgres` output, equality by 30-day months, arithmetic with month-end clamping, `justify_*`, `age`, `extract`), `bytea` (hex and escape, `encode`/`decode`, `get_byte`, `substring`, `||`), `uuid` with `gen_random_uuid()`, blank-padded `character(n)`; PostgreSQL's wire protocol in text and binary with their OIDs, MySQL's `TIME` and `BLOB`; MySQL's `TIME` and binary types and Oracle's `CHAR`, `RAW` and `BLOB` map to them; table definitions in format version 5; 37 new PostgreSQL correctness cases and more in the other dialects, a differential test with and without indexes, pg8000 and PyMySQL round trips. Built since: infinite intervals, `AT TIME ZONE` (item 31b), `make_interval`, MySQL's `TIME` values beyond a day (`-838:59:59` to `838:59:59`), Oracle's `INTERVAL` types and SQLite `BLOB` columns (D153) |
| 31b | Users can now use named time zones (`SET TimeZone = 'Europe/Paris'`, `AT TIME ZONE 'America/New_York'`, `timestamptz` input and output with each region's daylight saving time and history), PostgreSQL 17's infinite intervals, MySQL's `TIME` past a day and below zero with `time_zone` applied, Oracle's region names in `TIME_ZONE`, `TZR`, `FROM_TZ` and `TZ_OFFSET`, and SQLite `BLOB` columns that hold every storage class with `x'...'` literals; check: PostgreSQL, MySQL, Oracle and SQLite suite cases, DST edges against the published rules, the `pg-functions` differential job | **written** (D153): the IANA 2026e time zone database (its public-domain `tzdata.zi` source, embedded and compiled per region on first use by our own compiler into transitions plus each region's continuing rules), `TimeZone` validated (`22023`), a gap taking the offset from before and a repeated hour the offset after, calendar `timestamptz ± interval`, region names in input and `to_char`'s `TZ` abbreviation, `pg_timezone_names`; `infinity`/`-infinity` intervals; `TIME` from `-838:59:59` to `838:59:59` stored as before; `SqlType::Dynamic` for SQLite `BLOB` columns in table-definition version 10. Users can now write time zone abbreviations (`2024-01-01 10:00 PST`, PostgreSQL's `Default` set, listed in `pg_timezone_abbrevs`), POSIX rule strings in `TimeZone` and `AT TIME ZONE` (`EST5EDT,M3.2.0,M11.1.0`, `<+03>-3`), and `infinity`/`-infinity` dates and time stamps with PostgreSQL 17's arithmetic, stored as PostgreSQL's sentinels with no format change (D271). Users can now use SQL Server's `AT TIME ZONE` with the Windows zone names of `sys.time_zone_info`, get infinite dates refused as out of range by SQL Server's and Oracle's functions, and keep `Infinity` in `numeric`, which `extract` of an infinite value returns (D290). Users can now keep dynamic typing in `TEXT`, `NUMERIC`, `REAL` and untyped columns and key and index every dynamic column (D305). Not yet: dynamic typing for `INTEGER` columns |
| 32 | Users can use sequences (`CREATE SEQUENCE`, `nextval`, `currval`, `setval`, `OWNED BY`) and `ALTER TABLE` (columns, primary key, unique, check and enforced foreign keys, defaults, `NOT NULL`); secondary indexes are done in 19c; check: the manual's foreign-key and check cases | **done** (D114): sequences as catalog objects (`CREATE`/`ALTER`/`DROP SEQUENCE` with `AS`, `START`, `INCREMENT`, `MINVALUE`/`MAXVALUE`, `CACHE`, `CYCLE`, `OWNED BY`, `RESTART`), `nextval`/`currval`/`lastval`/`setval`, `::regclass` and `pg_catalog.` calls in PostgreSQL, `seq.NEXTVAL`/`CURRVAL` in Oracle, non-transactional and durable with the transaction that used them; `CHECK` and `FOREIGN KEY` (`CASCADE`, `SET NULL`, `SET DEFAULT`, `RESTRICT`, `NO ACTION`, self-references, multi-column) enforced in every dialect with its errors, SQLite's after `PRAGMA foreign_keys = ON`; `ALTER TABLE` `ADD`/`DROP`/`RENAME COLUMN`, `RENAME TO`, `SET`/`DROP DEFAULT`, `SET`/`DROP NOT NULL`, `TYPE`, `ADD`/`DROP CONSTRAINT`, `OWNER TO`, MySQL's `MODIFY`/`CHANGE`/`DROP FOREIGN KEY`, Oracle's `ADD (...)`/`MODIFY (...)`, SQLite's limited `ALTER`; `DROP TABLE ... CASCADE`; table definitions in format version 6 only for tables with constraints; the `pg_dump` forms run as one psql-style script; 120 new correctness cases, a model test of referential actions with and without an index, a killed-process test, format and fuzz tests. Users can now add constraints `NOT VALID` and validate or rename them later, move MySQL columns with `FIRST`/`AFTER` in `ADD`, `MODIFY` and `CHANGE`, see foreign keys and checks in MySQL's `SHOW CREATE TABLE`, read a SQLite table's `CREATE TABLE` text as written in `sqlite_schema` and the shell's `.schema`, kept across `ADD COLUMN` and `RENAME TO`, with indexes', views' and triggers' text as written (D294), read a sequence as a one-row relation (`SELECT last_value, is_called FROM s`, which `pg_dump` sends) and query the `pg_sequences` view (D106, D114, D152, D273). Built since: deferred constraint checks and identity options (item 35b), and `ADD INDEX`, `RENAME INDEX` and `DROP INDEX` in MySQL's `ALTER TABLE` (D350). Next, in [What is left](#what-is-left-per-milestone): MariaDB sequences (M10), SQLite's stored text after `RENAME COLUMN`/`DROP COLUMN` (M5), `pg_sequences.last_value` for identity columns (M6a) |
| 33 | Users can use schemas (`CREATE SCHEMA`, `search_path`), views and `COMMENT ON`; check: a view over a join answers as on PostgreSQL | **done** (D116): `CREATE`/`DROP SCHEMA` (`IF [NOT] EXISTS`, `CASCADE`/`RESTRICT`), `ALTER SCHEMA ... RENAME TO`, schema-qualified names everywhere, `ALTER TABLE ... SET SCHEMA`, `search_path` with `SET`, `RESET`, `SHOW`, `set_config`, `current_schema()` and `current_schemas()`; `CREATE [OR REPLACE] VIEW [(columns)]`, `DROP VIEW`, `ALTER VIEW ... RENAME TO`/`SET SCHEMA`, views over joins, subqueries, set operations and other views, expanded at use with depth, size and cycle limits, `2BP01` for tables, columns and views that views use; `COMMENT ON` tables, views, sequences, columns, indexes, constraints and schemas; the `pg_dump` forms (owners, `GRANT`/`REVOKE`, `ALTER DEFAULT PRIVILEGES`, `CREATE EXTENSION plpgsql`, parenthesized joins in view text) run as one psql-style script; views in MySQL (`ALGORITHM`, `DEFINER`, `SQL SECURITY`, `SHOW CREATE VIEW`, `COMMENT` options), SQLite (`IF NOT EXISTS`, `main.`) and Oracle (`FORCE`, `WITH READ ONLY`, `USER_VIEWS`, `USER_TAB_COMMENTS`, `USER_COL_COMMENTS`); tables in schemas in format version 7, views in version 8; 144 new correctness cases, a seeded test of views against their inline queries with and without indexes, a killed-process test, format and fuzz tests. Built since: MySQL's `CREATE DATABASE` and `USE` of other databases (D270), updatable views and `WITH CHECK OPTION` (item 30b), materialized and temporary views, recursive views (D299), roles and privileges (D183) and `obj_description` (item 37). Next: renaming what a view uses (M5, [What is left](#what-is-left-per-milestone)) |
| 33b | Users can create and run triggers in SQLite, MySQL, SQL Server and PostgreSQL syntax (row and statement level, `BEFORE`/`AFTER`/`INSTEAD OF`, `UPDATE OF`, `WHEN`, `REFERENCING`); check: each dialect's documented trigger examples, a killed process keeps a trigger's changes with its statement, an SSMS script's trigger restores | **written** (D157): users can now create and run triggers: SQLite's (`NEW`/`OLD`, `WHEN`, `RAISE(IGNORE)`/`RAISE(ABORT, ...)`, `INSTEAD OF` on views), MySQL's (`SET NEW.col`, `BEGIN ... END` with `DECLARE`, `IF`, `WHILE`, `SIGNAL`, `FOLLOWS`/`PRECEDES`, upserts firing `UPDATE` triggers, `SHOW TRIGGERS`), SQL Server's (`AFTER` and `INSTEAD OF` on the batch interpreter with `inserted`/`deleted` and `UPDATE(col)`, `ENABLE`/`DISABLE TRIGGER`, `sys.triggers`), and PostgreSQL's `CREATE TRIGGER ... EXECUTE FUNCTION`, which runs once PL/pgSQL functions arrive (3.14); triggers show in `sqlite_schema`, `pg_trigger`, `pg_get_triggerdef` and `information_schema.triggers`, fire inside their statement's transaction, nest at most 32 deep, and SQL Server restores run them instead of reporting them; users' row triggers now fire for rows that cascading foreign keys and MySQL's `REPLACE` change or remove, each dialect as its own database does, so `eggdb_cdc`, `eggdb_tenancy` and `eggdb_retention` work on tables with cascades (D260); next: a PL/SQL cascade trigger test, transition tables on cascades, statement triggers on self-referencing tables checked against PostgreSQL |
| 33c | Users can create and call PostgreSQL functions and procedures in `LANGUAGE sql` and PL/pgSQL (`CREATE [OR REPLACE] FUNCTION/PROCEDURE`, `CALL`, `DO`, `RETURNS TABLE`/`SETOF`, `RAISE`, exceptions, cursors, `PERFORM`, `EXECUTE`), PL/pgSQL trigger functions, and `\df` shows them; check: PL/pgSQL manual examples, the `pg_dump` fixture with functions and triggers restores and its triggers run | **written** (D158, batch part 3.14): users can now create PostgreSQL functions and procedures in `LANGUAGE sql` (`AS`, `RETURN`, `BEGIN ATOMIC`) and PL/pgSQL with overloads, `IN`/`OUT`/`INOUT`/`VARIADIC` parameters, defaults, `RETURNS TABLE`/`SETOF`, `STRICT`, volatility and `SET`; drop, rename and move them; call them with `SELECT f(...)`, in any expression, as a `FROM` item (`WITH ORDINALITY`), with `CALL` (`INOUT` results) and named arguments, and run `DO` blocks. PL/pgSQL covers declarations (`%TYPE`, `%ROWTYPE`, `record`, `CONSTANT`, `ALIAS`, cursors), blocks with labels, `IF`, `CASE`, every loop form, `EXIT`/`CONTINUE`, `SELECT INTO [STRICT]`, `PERFORM`, dynamic `EXECUTE ... USING`, `GET DIAGNOSTICS`, `FOUND`, `RETURN NEXT`/`QUERY`, `RAISE` with notices sent to the client, `ASSERT`, exception handlers that undo their block's writes, and cursors; bodies are checked at `CREATE` unless `check_function_bodies` is off; trigger functions run PostgreSQL row and statement triggers (also on `TRUNCATE`) with `NEW`, `OLD` and `TG_*`; routines show in `pg_proc`, `pg_language`, `pg_get_functiondef`, `information_schema.routines`/`parameters` and psql's `\df`; `pg_dump` restores create and run their `sql`/`plpgsql` functions, procedures and triggers; calls nest at most 100 deep and loops can be cancelled (D45). Users can now `COMMIT` and `ROLLBACK` inside procedures and `DO` blocks called outside a transaction block, read the refcursors functions and procedures return with `FETCH` as Npgsql and psycopg do, and use SQL `DECLARE CURSOR [WITH HOLD]`, `FETCH`, `MOVE` and `CLOSE` (D262); users can write functions and procedures with polymorphic parameters and results (`anyelement`, `anyarray`, `anynonarray`, `anyenum`, `anycompatible` and their arrays), resolved on each call, and `AFTER ... FOR EACH STATEMENT` triggers that read `REFERENCING OLD TABLE`/`NEW TABLE` transition tables (D264). A `COMMIT` or `ROLLBACK` inside a procedure now closes the cursors other than `WITH HOLD` and releases transaction-level advisory locks, a procedure with a `SET` clause cannot commit, `refcursor` is a type of its own (OID 1790 on the wire) and `pg_typeof` names any value's type (D280). Users can now use `SET LOCAL` and `set_config(..., true)` (ended at the transaction's end, also at a procedure's `COMMIT`), get a `SET` undone by its transaction's `ROLLBACK` or `ROLLBACK TO SAVEPOINT`, run `SET`, `SET LOCAL`, `RESET` and `set_config` inside routines, have a routine's own `SET` clause (and `SET ... FROM CURRENT`) applied while it runs, `search_path` and `TimeZone` included, and use `refcursor[]` arrays (D301); a `COMMIT` that fails its deferred checks now undoes the transaction's settings with its data, `SET ... TO DEFAULT` takes the connection's value as `RESET` does, and a routine running `SET` in a loop costs linear time (D301, review round 1). Not yet: writes from functions called per row, user-defined `SETOF` functions in a select list, `$0`, transition tables in row-level triggers, `regprocedure`; users can now run `RESET ALL` and `SET eggdb.dialect` inside routines, change a routine's settings with `ALTER FUNCTION ... SET`/`RESET`, and read with `current_setting` what a routine set earlier in the same statement (D318); next: `refcursor` and `refcursor[]` table columns kept as their own types (needs a file format change, D35) |
| 33d | Users can create and run Transact-SQL stored procedures and functions (scalar, inline and multi-statement table-valued), `EXEC` them with output parameters in batches and over RPC, and SSMS restores run them; check: the Transact-SQL reference's examples, SqlClient calls a procedure with output parameters | **written** (D159, batch part 3.15): users can now `CREATE [OR ALTER]`, `ALTER` and `DROP [IF EXISTS]` Transact-SQL procedures and functions, checked at `CREATE` with the batch compiler; `EXEC [@status =] procedure` with positional, named, `DEFAULT` and `OUTPUT` arguments runs the procedure inside the caller's batch (result sets, messages, `TRY ... CATCH` across calls, `RETURN` status, `INSERT ... EXEC`, `@@NESTLEVEL`, at most 32 levels, SQL Server's error numbers), also from triggers; scalar functions are called with their schema in any expression and table-valued ones as a `FROM` item, with side effects refused as SQL Server refuses them (Msg 443, 444, 557); over TDS, RPC calls of a procedure return its rows, `RETURNSTATUS` and `OUTPUT` values, and SqlClient's `DeriveParameters` lists its parameters; procedures and functions show in `sys.objects`, `sys.procedures`, `sys.parameters`, `sys.sql_modules`, `INFORMATION_SCHEMA.ROUTINES`/`PARAMETERS`, `OBJECT_DEFINITION`, `sp_helptext`, `sp_stored_procedures` and `sp_sproc_columns`; SSMS and sqlpackage scripts restore their procedures and functions. Users can now read `ERROR_PROCEDURE()` and `@@PROCID` (`OBJECT_NAME(@@PROCID)`), `CROSS`/`OUTER APPLY` table-valued functions that read the outer row, create temporary `#name` procedures, and see the procedure name in TDS error tokens (D159). Not yet: CLR, numbered procedures (`proc;2`), temporary functions |
| 33e | Users can run PL/SQL: anonymous blocks, procedures, functions, packages (specification and body), `%TYPE`/`%ROWTYPE`, cursors, exceptions, `RAISE_APPLICATION_ERROR`, `DBMS_OUTPUT` and Oracle triggers, and Oracle restores run PL/SQL instead of listing it; check: PL/SQL Language Reference examples, HR's `hr_code.sql` (procedures and triggers) runs on restore | **written** (D160, batch part 3.16): users can now run PL/SQL in Oracle's dialect through `eggdb sql`, `eggdb shell`, the embedded API, PostgreSQL and TDS connections that chose Oracle and restores: anonymous blocks; `CREATE [OR REPLACE]`, `ALTER ... COMPILE` and `DROP` of procedures, functions, packages and package bodies, compiled at `CREATE` (`ORA-06550` with the `PLS-` message when they do not compile); `CALL` and `EXECUTE`; declarations with `%TYPE`, `%ROWTYPE`, records, subtypes, cursors, `REF CURSOR`, exceptions with `EXCEPTION_INIT` and nested subprograms; `IF`, `CASE`, every loop, labels, `SELECT INTO`, DML, cursor attributes on cursors and `SQL`, `EXECUTE IMMEDIATE`, `COMMIT`/`ROLLBACK`; associative arrays, nested tables and varrays with their methods, `BULK COLLECT` and `FORALL ... SAVE EXCEPTIONS`; predefined and user exceptions, `SQLCODE`/`SQLERRM`, `RAISE_APPLICATION_ERROR` and `ORA-06512` lines per unit; functions in any SQL expression with named arguments; packages with per-session state, initialization, overloads and private members; `DBMS_OUTPUT` printed by `eggdb sql` after `SET SERVEROUTPUT ON`, by the Oracle shell and read through `Session::take_server_output`; Oracle triggers (`BEFORE`/`AFTER`/`INSTEAD OF`, row and statement level, `:NEW`/`:OLD`, `WHEN`, `INSERTING`/`UPDATING`/`DELETING`, `ALTER TRIGGER`); `USER_SOURCE`, `USER_PROCEDURES`, `USER_ARGUMENTS`, `USER_TRIGGERS`, `USER_ERRORS` and PL/SQL objects in `USER_OBJECTS`; HR's installer creates and runs its procedures and triggers on restore. Users can now use `GOTO` and labels, pipelined functions read with `TABLE(f(...))`, `TABLE(v)` of PL/SQL collections, and `MEMBER OF`, `SUBMULTISET OF`, `CARDINALITY` and the `MULTISET` operators in SQL (D160). Not yet: autonomous transactions (refused while there is one writer at a time), `TABLE()` correlated to an earlier `FROM` item or in a join, `IS A SET`, `IS EMPTY`, `SET()`, `MULTISET(subquery)`, schema-level collection types, object types, compound triggers, Oracle-supplied packages other than `DBMS_OUTPUT` |
| 30b | Users can write the rest of PostgreSQL's query language: `LATERAL`, `FULL JOIN ... USING`, correlated subqueries at any depth, in grouped queries and in `UPDATE`/`DELETE`, data-modifying `WITH`, window functions with frames, `FILTER` and ordered-set aggregates, updatable views with `WITH CHECK OPTION`, and temporary tables and views; check: the PostgreSQL manual's cases and the differential job against `postgres:17` (`scripts/pg-queries/cases.sql`) | **written** (D151): users can now write window functions anywhere in a select list and in `ORDER BY` (`row_number`, `rank`, `dense_rank`, `percent_rank`, `cume_dist`, `ntile`, `lag`, `lead`, `first_value`, `last_value`, `nth_value` and every aggregate) with `PARTITION BY`, `ORDER BY`, `ROWS`/`RANGE`/`GROUPS` frames, `EXCLUDE` and named windows (`WINDOW w AS (...)`, `OVER w`, `OVER (w ...)`); `FILTER (WHERE ...)` on every aggregate; `percentile_cont`, `percentile_disc` and `mode() WITHIN GROUP (ORDER BY ...)`; `LATERAL` subqueries and functions (and Transact-SQL's `CROSS APPLY`/`OUTER APPLY`); `FULL JOIN ... USING`; correlated subqueries that name any query around them, stand in grouped select lists and `HAVING`, and name the changed row in `UPDATE` and `DELETE`; `WITH` before `INSERT`, `UPDATE` and `DELETE` and data-modifying `WITH ... (INSERT/UPDATE/DELETE ... RETURNING)`; `INSERT`, `UPDATE` and `DELETE` through automatically updatable views, with `WITH [LOCAL | CASCADED] CHECK OPTION` (two new view-definition flags, D35); and temporary tables and views (`CREATE TEMP TABLE ... ON COMMIT ...`, MySQL's `TEMPORARY`, SQLite's `temp.`, Transact-SQL's `#name`) kept in the session's memory |
| 34 | Users can bulk-load and unload data with `COPY ... FROM STDIN`/`TO STDOUT` (text and CSV, psql's `\copy`) and `eggdb import --csv`; check: a million rows in and out byte for byte | **done** (D117): `COPY table [(columns)] FROM STDIN` and `COPY {table [(columns)] \| (query)} TO STDOUT` in the PostgreSQL dialect with the manual's text format (escapes, `\N`, `\.`, `\r\n` and `\r` lines) and CSV format (quotes, escapes, multi-line fields), the options `FORMAT`, `DELIMITER`, `NULL`, `DEFAULT`, `HEADER [MATCH]`, `QUOTE`, `ESCAPE`, `FORCE_QUOTE`, `FORCE_NOT_NULL`, `FORCE_NULL`, `ENCODING 'UTF8'`, `FREEZE` and `ON_ERROR stop`, and the older `WITH CSV HEADER ...` forms; rows through the `INSERT ... VALUES` path in one transaction (constraints, defaults, serial, indexes; foreign keys checked in batches of 4,096), streamed with one line in memory at a time and lines up to 1 GiB, errors with PostgreSQL's SQLSTATEs and a `CONTEXT` line; server files and programs refused with `42501`; the copy sub-protocol in `eggdb-pgwire` (`CopyInResponse`, `CopyOutResponse`, `CopyData`, `CopyDone`, `CopyFail`, from the simple and the extended protocol, errors mid-copy, disconnects); scripts with inline `COPY` data in `eggdb sql` and `eggdb shell` (pg_dump's data sections), `COPY ... TO STDOUT` printing rows, the shell's `\copy` to and from files, `eggdb import --csv --table [--header] [--delimiter] [--null]`; 57 new correctness cases from the manual's COPY page, 33 tests (reader and writer round trips in both formats, a model test against `INSERT` with and without indexes, rollback and a killed process, hostile statements, data and protocol messages), a million rows in and out byte for byte through the embedded API and through psql's `\copy` in the smoke test, and `sql_copy` benchmarks. Not yet: the binary format, `ON_ERROR ignore`, `COPY ... WHERE`, other encodings (row 34b) |
| 34b | Users can now copy in PostgreSQL's binary format (`COPY ... (FORMAT binary)`, `COPY BINARY`), skip rows with bad values with `ON_ERROR ignore` and `LOG_VERBOSITY`, load only some rows with `COPY ... FROM STDIN WHERE`, and copy data in `LATIN1`, `LATIN9`, `WIN1252` and UTF-16; check: a binary round trip with psql's `\copy` and psycopg 3 against PostgreSQL 17 byte for byte, the manual's COPY cases | **written** (D154): the binary file layout both ways (signature, flags and header extension, field counts and lengths, trailer; `CopyInResponse`/`CopyOutResponse` with binary format codes) with each value in the binary form EggDB already sent over the protocol, now one codec in `eggdb-sql` (`encode_pg_binary`, `decode_pg_binary_text`) shared by `eggdb-pgwire` and `COPY`, plus one-dimensional arrays and enums; PostgreSQL's binary-format errors and option refusals; `ON_ERROR ignore` skipping rows whose values their type refuses, with PostgreSQL 17's notices (`LOG_VERBOSITY default`, `verbose`, and 18's `silent`); `WHERE` tested on each row after its defaults, with aggregates, window functions and subqueries refused; `ENCODING` converting client data to and from UTF-8 for `LATIN1`, `LATIN9`, `WIN1252` and `UTF16`/`UTF16LE`/`UTF16BE` (`22P05` for characters without an equivalent), PostgreSQL's other encodings `0A000`; the two differing suite cases now match; unit tests of the reader, writer and converters, hostile binary data, a socket test, CI's `pg-apps` job with psycopg 3's binary `write_row`/`rows()` and psql's binary `\copy` and `od` of the bytes against PostgreSQL 17, and `sql_copy.from_stdin_binary`/`sql_copy.to_stdout_binary` benchmarks as ratios to the text format. Not yet: multidimensional arrays and composite or range columns in the binary format, a domain `CHECK` failure skipped by `ON_ERROR ignore` |
| 35 | Users can restore a `pg_dump` into EggDB (`eggdb restore dump.sql`, or `psql -f` against `eggdb serve`), with a restore report for what can't be created yet; check: a fixture dumped from PostgreSQL 16 and 17 restores and every table matches | **done** (D118): `eggdb restore [--create] <file> [<dump.sql> \| -]` reads `pg_dump`'s plain output (and `pg_restore -f`'s) as a stream of lines, splits statements as psql does (quotes, `E''`, dollar quotes, comments, parentheses, `BEGIN ATOMIC`), runs each in its own transaction with `COPY` data inline, handles the `SET` preamble, `set_config('search_path', '', false)`, `\connect`, `\restrict`/`\unrestrict` (keys checked), `CREATE EXTENSION plpgsql`, `OWNER TO`, `GRANT`/`REVOKE`, identity columns, sequences and `setval`, constraints, indexes, views and comments, `--inserts` and `--column-inserts`; skips functions, procedures, triggers, rules, policies, types, domains, materialized views, partitioned and inherited tables (with their rows counted), other extensions and the rest, lists consequences, and stops at a real error with the dump's line (`--keep-going` lists them all); `--create` builds the file as `<file>.restoring` and renames it only on success; a report in text or JSON (`--report <path>`) designed by Claude Fable (docs/ux/cli.md section 16); custom, directory and tar archives and compressed dumps refused with the exact `pg_restore -f` or decompress command; the engine gained `= ANY (ARRAY[...])`/`<> ALL`, `ALTER COLUMN ... ADD GENERATED AS IDENTITY (SEQUENCE NAME ...)`, `OVERRIDING SYSTEM VALUE`, `CREATE UNLOGGED TABLE`, and `setval('public.s')` under an empty search path; [migrate.md](migrate.md) is the guide; checks: dumps from PostgreSQL 16 (plain, `--inserts`, `--column-inserts`) and 18 (plain with `\restrict`, custom through `pg_restore -f`) committed with PostgreSQL's own sorted `COPY` output and matched byte for byte in `cargo test`, CI's `pg-restore` job with the `postgres:16` and `postgres:17` containers and about 50,000 rows (every form by `eggdb restore`, the `--inserts` one by `psql -f` against `eggdb serve`, every table and sequence compared, a skipped enum-typed identity table), a fuzzed dump reader, cut dumps refused, a killed-process test, and the smoke test both ways. Not yet: the skipped objects, `pg_dump --clean`, `pg_dumpall`, large objects, encodings other than UTF-8 |
| 35b | Users can restore and use what `pg_dump` restores skipped: enum, composite and range types (`CREATE TYPE`), domains, array columns, deferred foreign keys (`DEFERRABLE INITIALLY DEFERRED`, `SET CONSTRAINTS`), `NOT VALID` and `VALIDATE CONSTRAINT`, `RENAME CONSTRAINT`, identity options, `pg_dump --clean` and `pg_dumpall` scripts (globals into the report); check: CI `pg-restore` with the fixture grown by enum, domain, array, composite, range and deferred-key tables compares every table with PostgreSQL | **written** (D152, batch part 3.08): users can now create and use enums (`ALTER TYPE ... ADD VALUE`, `enum_range`), composite types with `ROW(...)` and `(value).field`, the built-in and their own range types with their operators, domains with `NOT NULL`, checks and defaults (`ALTER DOMAIN` in full), and one-dimensional array columns; foreign keys checked at `COMMIT`; checks and keys added `NOT VALID` and validated later; constraints renamed; identity `START WITH`/`INCREMENT BY` kept and changed with `ALTER COLUMN ... SET`; `information_schema._pg_expandarray` for pgjdbc's `getPrimaryKeys`; types in `pg_type`, `pg_enum`, `pg_range` and `information_schema.columns`; table-definition version 10 and type records (D35); unit and session tests, restore tests (`--clean` over itself, `pg_dumpall`), the committed fixtures' enum table restored, the `pg-restore` fixture grown |
| 36 | Users can use string, date/time and JSON functions (`regexp_*`, `substring(... FROM regex)`, `SIMILAR TO`, `split_part`, `date_trunc`, `extract`, `to_char`, `->`, `->>`; the `~` operators and their own linear-time regular expressions arrived in 37a, D128, which leaves back references, lookaround and embedded options to this item); check: the manual's cases and the differential run | **done** (D130): users can now use `SIMILAR TO`, `substring(s FROM re)` and `substring(s SIMILAR re ESCAPE e)`, `regexp_match`, `regexp_matches`, `regexp_replace` (with start and occurrence), `regexp_split_to_table`/`_to_array`, `regexp_like`, `regexp_count`, `regexp_instr` and `regexp_substr` with PostgreSQL's flags, and back references, lookahead and lookbehind, the constraint escapes and embedded options in every regular expression (a backtracking search bounded by the step budget, the linear-time automaton kept for the rest); `split_part`, `concat`, `concat_ws`, `replace`, `position`/`strpos`, `left`, `right`, `lpad`, `rpad`, `repeat`, `initcap`, `translate`, `reverse`, `chr`, `ascii`, `md5`, `starts_with`, `format`, `overlay`, `substr`, `quote_literal`, `quote_nullable`; `to_char`, `to_date`, `to_timestamp` (text and epoch) and `to_number` with PostgreSQL's template patterns; `make_date`, `make_time`, `make_timestamp(tz)`, `make_interval` (named arguments too), `date_bin`, `isfinite` and `AT TIME ZONE` with UTC, fixed offsets and intervals; `to_json(b)`, `row_to_json` (of a `FROM` item or `row(...)`), `json(b)_build_object`/`_array`, `json(b)_agg`, `json(b)_object_agg`, `jsonb_set`, `jsonb_insert`, `json(b)_strip_nulls`, `jsonb_pretty`, `json(b)_extract_path(_text)`, `json(b)_each(_text)`, `json(b)_array_elements(_text)`, `json(b)_object_keys` and `jsonb`'s `?`, `?|`, `?&`, `-`, `#-` and `||`; 45 new PostgreSQL correctness cases (and the five cases that were marked as differing now pass), unit tests per module and CI's `pg-functions` job, which runs every case in `scripts/pg-functions/cases.sql` through psql on the `postgres:17` container and on `eggdb serve` and compares the bytes. Users can now query `jsonb` with SQL/JSON path (`jsonb_path_exists`, `_match`, `_query`, `_query_array`, `_query_first`, `@?`, `@@`; lax and strict modes, filters, `like_regex`, `starts with`, arithmetic, item methods, variables), turn JSON into rows with `json(b)_to_record(set)` and a column definition list, and truncate intervals with `date_trunc` (D272). Users can now call `JSON_EXISTS`, `JSON_VALUE`, `JSON_QUERY` (`PASSING`, `RETURNING`, wrappers, quotes, `ON EMPTY`/`ON ERROR`) and `JSON_TABLE` (ordinality, `EXISTS`, `NESTED PATH`, `ON ERROR`) in PostgreSQL's and Oracle's dialects, `json(b)_populate_record(set)` with `NULL::table` or a composite type, and jsonpath's `datetime()`, `date()`, `time()`, `time_tz()`, `timestamp()` and `timestamp_tz()` (D289). Users can now use `IS [NOT] JSON [VALUE | SCALAR | ARRAY | OBJECT] [WITH UNIQUE KEYS]`, `JSON()`, `JSON_SCALAR`, `JSON_SERIALIZE`, jsonpath's `.decimal(p, s)`, `json(b)_populate_record(set)` with a non-NULL base row, `scale()`, `min_scale()` and `trim_scale()`, and the `citext` versions of `regexp_match(es)`, `regexp_replace`, `regexp_split_to_*`, `replace`, `split_part`, `strpos` and `translate` (D302), the last four folding case over all of Unicode as citext's `=` does (review round 1). Not yet: `json_populate_record` in a select list and with a column alias list, `JSON_TABLE`'s `PLAN` clause, Oracle's `IS JSON` and `JSON_SERIALIZE` options, a `jsonpath` type of its own, arrays as `to_record` column types; next: a Unicode case-insensitive mode in the regular expression engine so citext's `regexp_*` and `~*` fold non-ASCII letters (today the `i` flag folds ASCII letters only), and `IS JSON` past the JSON parser's 256 levels (false today, true in PostgreSQL) |
| 37 | Users can browse a database with psql's `\d`, `\dt`, `\di`, `\dv`, `\ds`, `\dn`, `\l` and `\du` (`pg_catalog`); check: psql 17's `\d` output matches PostgreSQL's for the fixture | **done** (D128, D129): part a (#78) brought the SQL psql's queries use (our own regular expressions with `~`, `OPERATOR(...)` and `COLLATE`, arrays in expressions, `generate_series` and `unnest` with `WITH ORDINALITY`, `VALUES`, `string_agg`, `trim`, `quote_ident`); part b adds the catalog: 24 `pg_catalog` relations with PostgreSQL 16's columns, built once per statement from the stored tables, views, sequences, schemas and comments, read-only; the `oid`, `name`, `"char"`, `xid`, `pg_node_tree`, `aclitem`, `int2vector`, `oidvector` and `reg*` types in expressions; `format_type`, `pg_get_constraintdef`, `pg_get_indexdef`, `pg_get_expr`, `pg_table_is_visible` and the other functions psql calls; OIDs from hashes of names; `CHECK` constraints and defaults in PostgreSQL's display form; CI's `pg-catalog` job compares psql 17's output with PostgreSQL 16's byte for byte, and cargo tests run psql 16's, 17's and 18's recorded queries. Users can now run psql's `\d+`, `\dt+` and the other `+` forms, `\df+`, `\dT`, `\dD`, `\dp` (privileges shown as `aclitem`), `\dx`, `\dd`, `\db`, `\dC`, `\dy`, `\dRp` and `\dRs`, and catalog tables answer `tableoid` (D129). Not yet: `tableoid` and the other system columns on user tables, `pg_cast` rows for `\dC`, composite types' `typrelid`, these commands in CI's byte-for-byte `pg-catalog` job, view columns of catalog types, composite row types, multidimensional arrays and slices, more than one set-returning function in a select list ([What is left](#what-is-left-per-milestone), M5 and M6a); built since: storing arrays (item 35b), `LATERAL` (item 30b) and ordered aggregates; `information_schema` is item 38 |
| 37c | Users see an index's method and sort order, unique indexes made by `CREATE UNIQUE INDEX`, and `GENERATED ALWAYS` identities in psql's `\d` as in PostgreSQL: the table definition stores them (D35) | **done** (D146): users can now see `USING hash`, `DESC`, `NULLS FIRST` and `NULLS LAST` indexes, unique indexes that are not constraints and `generated always as identity` / `generated by default as identity` columns (made by `CREATE TABLE` or restored from `pg_dump`'s `ALTER TABLE ... ADD GENERATED`, with `Sequence for identity column:`) in psql's `\d` exactly as PostgreSQL shows them, in `pg_index`, `pg_am`, `pg_attribute`, `pg_constraint`, `pg_depend`, `pg_get_indexdef` and `information_schema.columns`; `GENERATED ALWAYS` refuses given values (`428C9`) unless `OVERRIDING SYSTEM VALUE`, `OVERRIDING USER VALUE` works, `hash` indexes follow PostgreSQL's limits; table definitions in format version 9 only for tables that use them; MySQL's `SHOW INDEX`, Oracle's `DESCEND`, SQL Server's `is_descending_key` and `.schema` show the order; CI's `pg-catalog` fixture gains the cases. Not yet: Oracle's `GENERATED ALWAYS` refusal, `ALTER COLUMN ... SET GENERATED` and `DROP IDENTITY`, descending entries on disk, re-recorded psql query data with the new fixture |
| 38 | Users can run Java and Python apps unchanged (`information_schema`, every isolation level); check: pgjdbc CRUD and metadata, psycopg 3 and SQLAlchemy Core reflection | **done** (D131): users can now run pgjdbc, psycopg 3 and SQLAlchemy Core apps unchanged: `information_schema`'s `schemata`, `tables`, `columns`, `views`, `sequences`, `table_constraints`, `key_column_usage`, `referential_constraints`, `constraint_column_usage`, `check_constraints` and the empty `routines` and `parameters` with PostgreSQL 17's columns (its domains as their base types); `SET SESSION CHARACTERISTICS AS TRANSACTION` and `SET TRANSACTION` with every isolation level; `pg_settings`, `pg_opclass`, `pg_enum`, `pg_get_serial_sequence`, `to_regclass`, `to_regtype`, `generate_subscripts` and `regproc` for `array_in`; `array_agg`, `bool_and`, `bool_or`, `every` and `ORDER BY` inside aggregate calls; several set-returning functions in one select list in lock-step; `row_number()`, `rank()` and `dense_rank()` over `PARTITION BY`/`ORDER BY` windows; CI's `pg-apps` job compares a pgjdbc program and a psycopg 3 and SQLAlchemy program on EggDB and PostgreSQL 17 byte for byte. Users can now run pgjdbc's `getPrimaryKeys` and `getIndexInfo` unchanged and use PostgreSQL's integer `&` and `\|` operators (D131). Users can now use PostgreSQL's integer `#` and prefix `~` operators (D288); window frames and the other window functions came with item 30b (batch part 3.07, D151) |
| 23a | Users can connect with go-sqlcmd and run T-SQL batches: TDS 7.4 from [MS-TDS] with encryption off, SQL logins, and the T-SQL dialect (`TOP`, `IDENTITY`, `[brackets]`, `GETDATE()`) | done: `eggdb serve --tds-listen`, `--dialect sqlserver` (D122) |
| 23b | Users can run .NET and Java apps through Microsoft.Data.SqlClient and mssql-jdbc (RPC, prepared statements, transactions, TDS types); check: parameterised CRUD and a transaction from C# and Java | **done** (D132): users can now run .NET and Java apps through Microsoft.Data.SqlClient and mssql-jdbc: RPC requests by name or id for `sp_executesql`, `sp_prepare`, `sp_prepexec`, `sp_execute` and `sp_unprepare` (prepared handles per connection), parameters bound by name or position to their declarations with the drivers' TDS types (`INTN`, `BITN`, `DECIMALN`, `FLTN`, `MONEYN`, `DATETIMN`, `DATEN`, `TIMEN`, `DATETIME2N`, `DATETIMEOFFSETN`, `GUID`, `NVARCHAR`/`VARCHAR`/`VARBINARY` with PLP `max`, `NTEXT`/`TEXT`/`IMAGE`), output parameters and the return status, transaction manager requests (begin, commit, rollback, chained begin; save points and distributed transactions are 40517) with the transaction descriptor `ENVCHANGE`, the connection-reset flag of pooled connections, and `SET IMPLICIT_TRANSACTIONS ON` with single-statement `IF` for mssql-jdbc's `setAutoCommit(false)`; CI's `tds-apps` job runs a C# SqlClient program and a Java mssql-jdbc program (parameterized CRUD, prepared statements, a committed and a rolled-back transaction, an error, a reused pooled connection) against `eggdb serve --tds-listen`. Save points over transaction manager requests came with D138 and D173 (D291), and output parameters assigned by statements with item 47. Next: distributed transactions |
| 39 | Users can use SQL Server's types and functions (`decimal`, `money`, `datetime2`, `uniqueidentifier`, `nvarchar(max)`, `ISNULL`, `CONVERT`, `DATEADD`, `OFFSET ... FETCH`, `OUTPUT`, ...); check: cases from the Transact-SQL reference | done: types mapped onto the engine's, SQL Server's conversion rules and styles, the new TDS types (D123) |
| 39b | Users can use the rest of SQL Server's common functions (`SUBSTRING`, `LEFT`, `RIGHT`, `REPLACE`, `STUFF`, `REPLICATE`, `CONCAT`, `DATALENGTH`, `IIF`, `DATEPART`, `EOMONTH`, `TRY_CONVERT`, `ROUND`, ...), the other `CONVERT` styles and SQL Server's `SUM`, `AVG` and decimal division types; check: cases from the Transact-SQL reference | **written**: users can now use `SUBSTRING`, `LEFT`, `RIGHT`, `REPLACE`, `STUFF`, `REPLICATE` and `SPACE` (cut at 8,000 bytes unless `max`), `REVERSE`, `TRIM`/`LTRIM`/`RTRIM`, `CONCAT`, `CONCAT_WS`, `DATALENGTH`, `ASCII`, `CHAR`, `UNICODE`, `NCHAR`, `IIF`, `CHOOSE`, `DATEPART`, `DATENAME`, `YEAR`, `MONTH`, `DAY`, `EOMONTH`, `DATEDIFF_BIG`, `TRY_CAST`, `TRY_CONVERT`, `FORMAT` (.NET number, date and time formats, `en-US` and invariant), `ROUND`, `CEILING`, `FLOOR`, `COUNT_BIG`, `STRING_AGG ... WITHIN GROUP`, `STRING_SPLIT` (with `ordinal`), `@@IDENTITY`, every documented `CONVERT` style but Hijri (dates, `float`, `money`, binary), and SQL Server's `SUM`/`AVG` result types and decimal division precision and scale; 50 cases from the Transact-SQL reference in the SQL Server suite |
| 40a | Users can keep views and `CHECK` constraints written in Transact-SQL: the SQL Server dialect in the file format (D35) | done: dialect code 4 in the view and CHECK records, no new format version (D124); Transact-SQL `DEFAULT` expressions beyond constants and `IDENTITY` seeds other than (1, 1) moved to 40b |
| 40b | Users can keep any `DEFAULT` expression and `IDENTITY(seed, increment)` written in Transact-SQL (two new default tags in the table definition, D35) | **written** (D147): users can now keep any Transact-SQL `DEFAULT` expression (kept as written in table-definition version 9 and run as Transact-SQL from every dialect, inline or through `ADD DEFAULT ... FOR`) and `IDENTITY(seed, increment)` with any seed and increment, negative ones included, shown in `sys.identity_columns` and `sp_help`; an SSMS script with them restores without known differences for them, and inserts after reopening get SQL Server's next values |
| 40 | Users can restore an SSMS Generate Scripts or sqlpackage script (`eggdb restore --dialect sqlserver`, or `sqlcmd -i` against `eggdb serve`); check: a fixture script restores and its queries match the expected outputs | done: UTF-16 and UTF-8 scripts, `GO` and sqlcmd commands, statements found by the Transact-SQL parser, the restore runner shared with item 35 and item 43, known differences in the report, `.bak`, BACPAC and `.mdf` routed by content, and the engine syntax SSMS writes (D125); the follow-ups are item 40-T; SSMS's existence guards restore since item 47 |
| 40-T | Users can restore the rest of what SSMS and sqlpackage scripts hold: big batches without `GO`, the identity shapes and types EggDB lacked, `XACT_ABORT ON`, UTF-16 big-endian files, sequences and descriptions, and run `GO` in `eggdb sql`; check: a fixture per case in `crates/eggdb/tests/all/restore_sqlserver.rs` | **written** (D148): users can now restore a batch of any size without `GO` (cut at `;` and at lines starting `INSERT`, `UPDATE` or `DELETE`, outside `BEGIN ... END` and `CASE`), a heap or a table whose `IDENTITY` is not the first primary-key column (the property not kept, its `SET IDENTITY_INSERT` ignored), `numeric(p ≤ 18, 0) IDENTITY` as `bigint`, alias types (`CREATE TYPE ... FROM`) as their base types, `xml` columns as `nvarchar(max)`, UTF-16 big-endian scripts, `CREATE`, `ALTER` and `DROP SEQUENCE` with `NEXT VALUE FOR` (also as a default), `MS_Description` extended properties as comments (shown in `sys.extended_properties`), and values built with `CHAR(13) + CHAR(10)`; `SET XACT_ABORT ON` rolls back the transaction and ends the batch at a run-time error (an uncommittable transaction inside `TRY`, `XACT_STATE()` -1, Msg 3930 and 3998); `eggdb sql --dialect sqlserver` runs a script batch by batch at its `GO` lines |
| 41 | Users can browse a database from metadata tools (`sys.*` views, `INFORMATION_SCHEMA`, `sp_tables`, `sp_columns`, `sp_help`, ...); check: mssql-jdbc and SqlClient metadata lists are right for the fixture | **written** (D133): users can now browse a database from SQL Server metadata tools: `sys.objects`, `sys.tables`, `sys.views`, `sys.columns`, `sys.types`, `sys.schemas`, `sys.indexes`, `sys.index_columns`, `sys.foreign_keys`, `sys.foreign_key_columns`, `sys.key_constraints`, `sys.default_constraints`, `sys.check_constraints`, `sys.identity_columns`, `sys.sequences`, `sys.databases` and `sys.sql_modules` (and empty `sys.procedures`, `sys.parameters`, `sys.triggers`, `sys.extended_properties`), `INFORMATION_SCHEMA` with SQL Server's columns and values, `SERVERPROPERTY`, `DATABASEPROPERTYEX`, `DB_NAME`, `DB_ID`, `OBJECT_ID`, `OBJECT_NAME`, `OBJECT_SCHEMA_NAME`, `SCHEMA_NAME`, `SCHEMA_ID`, `TYPE_NAME`, `TYPE_ID`, `COLUMNPROPERTY`, `OBJECTPROPERTY(EX)`, `COL_NAME`, `COL_LENGTH`, `SYSTEM_USER`, `CURRENT_USER` and more `@@` values, and the catalog procedures `sp_tables`, `sp_columns(_100)`, `sp_pkeys`, `sp_fkeys`, `sp_statistics(_100)`, `sp_special_columns`, `sp_datatype_info(_100)`, `sp_stored_procedures`, `sp_sproc_columns`, `sp_help`, `sp_helpindex`, `sp_databases` and SqlClient's `sp_columns_managed`, `sp_indexes_managed`, `sp_indexcolumns_managed`, by `EXEC`, as the first statement of a batch and by RPC; table variables filled by `INSERT ... EXEC` and `COLLATE` in expressions, as mssql-jdbc's `getColumns` and `getImportedKeys` send them. CI's `tds-apps` job restores the sqlpackage fixture and compares mssql-jdbc's `getTables`, `getColumns`, `getPrimaryKeys`, `getImportedKeys`, `getIndexInfo`, `getTypeInfo`, `getCatalogs`, `getUserName` and SqlClient's `GetSchema("Tables" \| "Columns" \| "Indexes" \| "IndexColumns" \| "ForeignKeys")` with expected outputs. Users can now list databases with SqlClient's `GetSchema("Databases")` through the compatibility view `sysdatabases` (also `dbo.` and `master..`) (D291). Next: SSMS's own queries (3.21), procedures and triggers (3.15), mssql-jdbc's `getSchemas()` |
| 42 | Users can use Oracle's types and semantics (`NUMBER(p,s)`, `DATE` with time, `TIMESTAMP`, `CLOB`/`BLOB`/`RAW`, `TO_DATE`, `NLS_DATE_FORMAT`, `SYSDATE`, `DECODE`, sequences, `MERGE`); check: cases from Oracle's SQL Language Reference | done: exact `NUMBER` values, datetime and number format models, `ALTER SESSION` with the NLS formats and `CURRENT_SCHEMA`, Oracle's implicit conversions and date arithmetic, `ADD_MONTHS`, `LAST_DAY`, `TRUNC(date)` and `MERGE` with `DELETE WHERE` (D126) |
| 42b | Users can use the rest of Oracle's datetime and number functions and elements (`MONTHS_BETWEEN`, `NEXT_DAY`, `ROUND(date)`, `TO_TIMESTAMP_TZ` and zone elements, AD/BC, input `J`/`DDD`/`DAY`, `DS`/`DL`, `EEEE`/`RN`/`L`, `INTERVAL` types, `TIME_ZONE`, `LISTAGG`, `FETCH ... PERCENT`); check: cases from Oracle's SQL Language Reference | **written** (D149): users can now use `MONTHS_BETWEEN`, `NEXT_DAY`, `ROUND(date)` and `TRUNC(date)` with every unit, `TO_TIMESTAMP_TZ`, `FROM_TZ`, `SYS_EXTRACT_UTC`, `TZ_OFFSET`, the zone elements `TZH`/`TZM`/`TZR`/`TZD`, AD/BC, `SYYYY`, `Y,YYY`, `RM`, `J`/`DDD`/`D`/`DAY`/`DY`/`SSSSS` as input with Oracle's conflict errors, `DS`/`DL`/`TS`, `CC`, `Q`, `WW`, `IW`, `YEAR` and the `TH`/`SP` suffixes; the number elements `EEEE`, `RN`, `L`, `C`, `U`, `V`, `B`, `PR`, trailing `S`, `X` and `TM`; `INTERVAL YEAR TO MONTH` and `INTERVAL DAY TO SECOND` columns, literals, `NUMTODSINTERVAL`/`NUMTOYMINTERVAL`/`TO_DSINTERVAL`/`TO_YMINTERVAL`, interval arithmetic and `TIMESTAMP - TIMESTAMP` as an interval; `ALTER SESSION SET TIME_ZONE`, `SESSIONTIMEZONE`, `DBTIMEZONE`; `LISTAGG` with `DISTINCT`, `WITHIN GROUP` and `ON OVERFLOW`; `FETCH ... PERCENT` and `WITH TIES`. Users can now set `NLS_DATE_LANGUAGE` (German, French, Spanish, Italian, Portuguese, Dutch), `NLS_CURRENCY`, `NLS_ISO_CURRENCY` and `NLS_NUMERIC_CHARACTERS` and see them in `TO_CHAR`, `TO_DATE`, `TO_NUMBER` and `NEXT_DAY` (D126). Not yet: offsets kept in `TIMESTAMP WITH TIME ZONE` (a file-format change that needs a decision first), the NLS decimal character in implicit number-to-text conversions, `NLS_TERRITORY` other than `AMERICA`, `LISTAGG (DISTINCT ...) OVER`, `TO_DATE(... DEFAULT ... ON CONVERSION ERROR)` ([sql.md](sql.md#the-oracle-dialect)) |
| 43 | Users can restore Oracle schemas from scripts (SQL Developer and SQLcl exports, `impdp SQLFILE=`, CSV), with SQL*Plus commands handled and PL/SQL in the restore report; check: Oracle's HR sample scripts run unchanged | done: `eggdb restore --dialect oracle` reads SQL*Plus-style scripts (`;` and `/`, `DEFINE` and `&` substitution, `@`/`@@`/`START` includes, `CONNECT`), runs SQL-only anonymous blocks as one transaction, lists PL/SQL, makes users into schemas, drops and re-adds disabled constraints, accepts Oracle's storage clauses, and `eggdb import --dialect oracle --csv` loads SQLcl CSV; HR's installer restores unchanged with the documented counts (D127); next, 43b: `USING INDEX` takeover by another name, column-level disabled constraints, SQL*Plus commands in `eggdb sql` |
| 43b | Users can restore Oracle scripts whose `USING INDEX` names another index, with constraints disabled inside a column and identity columns with their options, and run SQL*Plus scripts in `eggdb sql --dialect oracle`; check: fixtures in `crates/eggdb/tests/all/restore_oracle.rs` and a SQL*Plus script through `eggdb sql` | **written** (D150): users can now restore Oracle scripts whose `ADD CONSTRAINT ... USING INDEX name` names another index (taken over on the same columns, `ORA-01418`/`ORA-14196` otherwise), with `CHECK`, `REFERENCES`, `UNIQUE` and `PRIMARY KEY` written `DISABLE` inside a column (remembered and added at their `ENABLE`, as table-level ones), with `ALTER TABLE ... ADD (...)` holding disabled constraints, and with identity columns kept: `GENERATED ALWAYS` recorded and enforced (`ORA-32795`, `ORA-32796`), `START WITH` and `INCREMENT BY` kept, the other options accepted, `user_tab_identity_cols` listing them, and an identity EggDB cannot keep rewritten out as before; and run SQL*Plus scripts in `eggdb sql --dialect oracle` (`DEFINE` and `&` substitution, `--define`, `@`/`@@`/`START`, `PROMPT`, `CONNECT`, `EXIT [ROLLBACK]`, notices for `SPOOL`, `HOST` and the commands with no effect) |
| 44 | Users can run Oracle SQL from apps over PostgreSQL or TDS drivers, with the dialect chosen per connection and Oracle's data dictionary; check: psql and JDBC run HR's queries | **written** (D134): users can now run Oracle SQL from apps through PostgreSQL and TDS drivers: the dialect chosen per connection with `eggdb.dialect` (`options=-c eggdb.dialect=oracle` in the startup packet, `SET eggdb.dialect = oracle` in PostgreSQL's dialect, `SET eggdb.dialect oracle` in Transact-SQL, back with `= postgresql`/`= sqlserver`/`DEFAULT`), taking effect from the next statement of the same query or batch, with the connection's own transaction rules and `$1` placeholders; Oracle's data dictionary (`USER_`/`ALL_`/`DBA_` `TABLES`, `TAB_COLUMNS`, `TAB_COLS`, `CONSTRAINTS`, `CONS_COLUMNS`, `INDEXES`, `IND_COLUMNS`, `SEQUENCES`, `VIEWS`, `OBJECTS`, `TAB_COMMENTS`, `COL_COMMENTS`, `USERS`, the `TABS`/`COLS`/`IND`/`SEQ`/`OBJ` synonyms), `V$VERSION`, `NLS_SESSION_PARAMETERS`, `NLS_DATABASE_PARAMETERS` and `SYS_CONTEXT('USERENV', ...)`, as catalog relations that join, group and filter like tables; the session user's schema of its own (`hr` for the user `hr`) as its `USER_` objects and its unqualified names. CI's `oracle-apps` job restores HR's installer and runs HR's queries and `SELECT table_name FROM user_tables` through psql and pgjdbc against expected outputs. Users can now run psql's backslash commands and pgjdbc's `DatabaseMetaData` while the connection reads Oracle SQL, bind `@name` TDS parameters in Oracle SQL, and see `NOT NULL` constraints in `USER_CONSTRAINTS` under generated `SYS_C` names (D269). Users can now call PL/SQL procedures by name over TDS RPC (mssql-jdbc's `{call p(?, ?)}`, SqlClient's `StoredProcedure`), by position or by name, with `OUT`/`IN OUT` parameters returned as return values, call them with Oracle's `CALL` over the PostgreSQL protocol with `OUT` parameters as one row, and use mssql-jdbc's `DatabaseMetaData` (`getTables`, `getColumns`, `getPrimaryKeys`, `getProcedures`, `getProcedureColumns`) while the connection reads Oracle SQL (D295). Next: named `NOT NULL` constraints (a file-format change) |
| 45 | Users can run .NET apps on PostgreSQL through Npgsql (type loading, pipelined extended queries, `CancelRequest`); check: Npgsql CRUD, a batch and a cancelled query | **written** (D135): users can now run .NET apps through Npgsql unchanged: its type-loading queries (`pg_proc` with the array functions, an empty `pg_range`, arrays found by `array_recv`), the statements of one `Query` message and of an extended-protocol pipeline up to `Sync` committing or rolling back together outside `BEGIN` (an implicit transaction opened by the first write, turned into a block by a `BEGIN` among them), portals that end with their transaction, `DISCARD ALL`/`PLANS`/`TEMP`/`SEQUENCES` and `RESET` back to the settings sent when connecting (pooled connections keep their connection string's settings), `CancelRequest` with a random secret key per connection stopping the running statement at its next row with `57014`, and `pg_sleep`; socket tests for pipelines, portals, `DISCARD ALL` and cancels, and CI's `pg-apps` job runs an Npgsql program (CRUD over pooled connections, `int[]`, transactions, a batch, a batch that fails half way and leaves nothing, a cancelled `pg_sleep`) on EggDB and PostgreSQL 17 and compares the bytes. Users can now keep prepared statements across Npgsql's connection reset (`SET SESSION AUTHORIZATION DEFAULT`, `RESET ALL`, `CLOSE ALL`, `UNLISTEN *`, `pg_advisory_unlock_all()`, `DISCARD SEQUENCES`/`TEMP` in one query), bind array parameters in text and binary, and see `void` (OID 2278) as the type of functions that return nothing (D265). Users can now get PostgreSQL's `void` results from user functions (empty from PL/pgSQL, NULL from SQL-language ones) and bind binary array parameters with any lower bound (D288). Next: the reset and an `int[]` parameter in CI's `pg-apps` Npgsql program |
| 46 | Users can load data with `bcp in` and `SqlBulkCopy` (TDS bulk load); check: `bcp` character-format files load and rows compare | **written** (D136): users can now load data with `SqlBulkCopy` and `bcp in` over TDS and with `eggdb import --dialect sqlserver` from `bcp -c` files: `SET FMTONLY ON` (queries return their declared columns and no rows, other statements but `SET` do not run), `sp_tablecollations_100`, `EXEC ..procedure`, `INSERT BULK table [(column type [COLLATE c], ...)] [WITH (KEEP_NULLS, TABLOCK, CHECK_CONSTRAINTS, FIRE_TRIGGERS, KEEPIDENTITY, ORDER(...), ROWS_PER_BATCH, KILOBYTES_PER_BATCH)]` waiting for its rows, bulk load messages (COLMETADATA, ROW, NBCROW, DONE from [MS-TDS] 2.2.6.1, every RPC parameter type plus `text`/`ntext`/`image`) read a row at a time as they arrive (rows up to 64 MiB, any number of them), loaded in one statement through the `INSERT` path (defaults for NULLs without `KEEP_NULLS`, identity kept when named, constraints, indexes, foreign keys in batches of 4,096) with errors 2627/515/4804/4815/4816 and the connection kept; `Session::execute_bulk_load` with `BulkRows` for embedders; bcp's character format (`-t`/`-r` terminators with bcp's escapes, empty fields as NULL, NUL as the empty string, `\r\n`, `--header` as `-F 2`, `--keep-nulls` as `-k`, UTF-8); decimal columns described with length 17 as SQL Server does; 16 tests (bcp reader, bulk reader split at every size and cut at every byte, session loads, FMTONLY, sockets with a 300-row message across packets and bad data, CLI), bulk data in the TDS fuzzer, a smoke-test step, and SqlBulkCopy (batches, `KeepNulls`, `UseInternalTransaction`, `TableLock`, a failing load) and `eggdb import` loading the committed `crates/eggdb/tests/data/sqlserver/bcp/items.dat` compared row by row with the file in CI's `tds-apps` job (`bcp` itself stays out of CI under its license). Users can now import bcp native (`-n`), Unicode (`-w`) and format-file (`-f`, non-XML and XML) data with `eggdb import --dialect sqlserver --native`/`--unicode`/`--format-file`, embedders write bcp files with `write_bcp_table`, and `BULK INSERT` and `OPENROWSET(BULK ...)` parse with SQL Server's options and are refused (4834) because EggDB never reads server files from SQL (D136). Not yet: native `varchar`/`char` columns as bcp writes them (`SQLCHAR`, which needs the declared `varchar`/`nvarchar` type, D133), a smoke-test step for `--native`, skipping constraints without `CHECK_CONSTRAINTS` (needs an untrusted constraint state, D291) |
| 47 | Users can run T-SQL batch logic (`DECLARE`, `IF`, `WHILE`, `TRY`/`CATCH`, `THROW`, `PRINT`, `sp_executesql`); check: cases from the Transact-SQL reference, a sqlcmd-style script with TRY/CATCH and a loop over TDS, SSMS existence guards restore | **written** (D137): users can now run Transact-SQL batch logic: `DECLARE @v type [= value]` (a variable exists for the whole batch, assignments convert to its type), `SET @v = ...` and `+= -= *= /= %=`, `SELECT @v = ... [FROM ...]` (last row; `SELECT @s += x` accumulates), `IF ... ELSE` with any condition (`EXISTS`, subqueries, `OBJECT_ID`), `BEGIN ... END`, `WHILE` with `BREAK` and `CONTINUE`, `RETURN`, `BEGIN TRY ... END TRY BEGIN CATCH ... END CATCH` with `ERROR_NUMBER()`/`ERROR_SEVERITY()`/`ERROR_STATE()`/`ERROR_LINE()`/`ERROR_MESSAGE()`/`ERROR_PROCEDURE()`, `THROW` (and `THROW;` in a `CATCH`), `RAISERROR` with `printf`-style arguments and `WITH SETERROR`/`NOWAIT`/`LOG`, `PRINT` (as TDS `INFO` tokens, error states sent too), `@@ERROR`, `XACT_STATE()`, dynamic SQL with `EXEC (...)` and `sp_executesql` (its own scope, parameters, `OUTPUT`, 32 levels), table variables with `INSERT ... VALUES / SELECT / EXEC`, `UPDATE` and `DELETE`; the whole batch compiled before it runs (errors 134, 135, 136, 137, 141, 178, 10704), run by one interpreter over the parser's statements (D137); `sp_executesql` `OUTPUT` parameters over TDS return what the batch assigned, `EXEC sp_unprepare n` in a batch releases the handle, and a stopping TDS server cancels running batches; `eggdb restore` runs SSMS's existence guards (`IF [NOT] EXISTS (...)`, `IF OBJECT_ID(...) IS [NOT] NULL` around one statement, and `EXEC sp_executesql N'CREATE VIEW ...'`); unit, session, correctness-suite and socket tests, the TDS fuzzer seeded with batch logic, and CI's `tds-apps` job runs `scripts/tds-apps/batch-logic.sql` batch by batch through SqlClient. Users can now run T-SQL cursors (`DECLARE`/`OPEN`/`FETCH`/`CLOSE`/`DEALLOCATE`, `@@FETCH_STATUS`, `CURSOR_STATUS`), `GOTO`, `WAITFOR DELAY`/`TIME` and `SELECT @s = @s + x FROM t`, and a TDS attention stops a running batch at once (D137). Not yet: cursor variables (`DECLARE @c CURSOR`), `FOR UPDATE` with `WHERE CURRENT OF`, `sp_cursor*` server cursors over RPC, global cursors dropped by `sp_reset_connection` |
| 48 | Stretch: users can run an unchanged SQLAlchemy ORM app and Flyway migrations, and an EF Core app over SQL Server's protocol | **written** (D138): users can now run an unchanged SQLAlchemy 2.0 ORM app (models and relationships, `create_all`/`drop_all`, a session's multi-row inserts through `INSERT ... SELECT ... RETURNING`, queries, updates, rollback, cascading deletes) and Flyway 10 `migrate`/`info`/`validate` over PostgreSQL's protocol, and an EF Core 8 app over SQL Server's (`Database.Migrate()`, `SaveChanges` batches with `MERGE ... OUTPUT` and `OUTPUT 1`, LINQ with `OFFSET ... FETCH`, `EXISTS`, `GROUP BY` and `OPENJSON` for `Contains`, a rolled-back transaction with EF's save point): `INSERT ... SELECT` in every dialect, PostgreSQL's advisory lock functions (session and transaction level, exclusive and shared) in one table per process, `set_config` with parameters, `SET ROLE` to the session's user, Transact-SQL `MERGE` (`WHEN MATCHED [AND] THEN UPDATE`/`DELETE`, `WHEN NOT MATCHED [BY TARGET] THEN INSERT`, `OUTPUT inserted./deleted./source columns`), `(VALUES ...) AS t (columns)` in Transact-SQL, `OPENJSON(json [, path]) WITH (...)`, `SAVE TRANSACTION` over SQL and transaction manager requests; session, unit and SQL Server suite tests, and CI's `pg-apps` (SQLAlchemy ORM and Flyway compared byte for byte with PostgreSQL 17) and `tds-apps` (EF Core against expected output) jobs. Users can now run EF Core 9's migration lock (`EXEC @result = sp_getapplock`/`sp_releaseapplock` with SQL Server's return codes and lock modes, `APPLOCK_MODE`, `APPLOCK_TEST`), and rolling back to a save point over TDS keeps what came before it (D138, D173). Users can now run `MERGE ... WHEN NOT MATCHED BY SOURCE`, `OPENJSON` without `WITH`, and get `sp_getapplock`'s -2 and -3 (D291). Not yet: Flyway's default two-connection lock (needs concurrent writers, 4.M3-1), EF Core 9 in CI's `tds-apps` job |
| 49 | Users can now let AI agents read and, when they allow it, change an EggDB database through its built-in MCP server; check: the built `eggdb mcp` driven by a scripted client: a describe, the same query in PostgreSQL and Oracle SQL, refused writes and `COMMIT; DROP`, the user's writer between calls, and `eggdb audit` listing every call | **49a done** (D119, D120): `eggdb mcp <file>` speaks MCP over stdio (the `initialize` versions 2025-11-25 and 2025-06-18) with `list_tables`, `describe_table`, `run_sql` (one statement, in any dialect), `explain_query`, `sample_rows`, `kv_get`, `kv_scan` and the `eggdb://schema` resource; read-only by opening the file read-only for each call; a time, row and byte limit per call and a watchdog; results marked as data and sent as one ASCII line; every call recorded in `<file>.mcp-audit.jsonl` before its result is sent, shown by `eggdb audit`; a fuzzer of hostile messages, the smoke test and a whole-call benchmark ([mcp.md](mcp.md)). **49b-1 done** (D121): `--allow-write` adds `write_sql` (one statement per call, its own transaction, at most 256 MiB of change; `run_sql` stays read-only); destructive statements run only after the user confirms them through `elicitation/create` or, when the client cannot ask, the agent's `confirm: true`; the watchdog never ends a write that began to commit; `call_started` records and `call_id`s pair a write with its outcome in the audit. **49b-2 done** (D156): users can now limit an agent to some tables with `eggdb mcp --tables a,b`: every other table, view and sequence reads as missing below the SQL engine, so it is refused directly, through a view and in `pg_catalog`, `information_schema`, SQL Server's `sys` views and Oracle's dictionary, and `kv_get`/`kv_scan` are withdrawn (`limited_to_tables`); names are checked at start; read-only servers only. `--call-gap` (100 ms by default) gives the user's writers the file between an agent's calls. Every SQL session now has EggDB's own limits of 1 GiB for one value and 2 GiB for one row. Users who give an agent `--allow-write` are now asked before any statement that can change or delete many rows: a `DELETE` or `UPDATE` runs unasked only when it pins a primary or unique key, and upserts and `MERGE` always ask ([issue #80](https://github.com/eggspot/EggDB/issues/80), D255). **49b-3 done** (D257): users can now let an agent write only some tables with `eggdb mcp --allow-write --tables a,b`: other tables read as missing to every statement, trigger and routine, any change outside the list (new names, schemas, types, routines, roles, triggers) is refused with 42501, and a listed table tied to an unlisted one by a foreign key or view cannot change where it could reach it. Users' writers now wait up to five seconds for a file an agent's call holds (`--busy-timeout`, `Options::busy_timeout`), and an agent's calls stop inside the engine at the time limit and at `max_rows` + 1 rows, with every growing value builder checking the value limit (D258). **49b-4 done** (D281): users can now show an agent only some columns of a table (`--tables 'orders(id,total)'`; the other columns are unknown everywhere and that table is read-only to the agent), an agent can insert into and change other columns of a listed table that an unlisted one references (deletes and changes to the referenced columns stay refused), and a new row-security policy, default, check or generated expression that calls a user-defined routine or holds a subquery is refused; a listed table's existing definitions that call a user routine stay allowed, since every call runs under the same filter (decided by research, D281). `restore` and `import` now wait for a busy file like every other command, Oracle's `upper`, `lower` and `initcap` refuse only results really over the value limit and SQLite's check it too, and a set-returning function alone in `FROM` stops at a literal outer `LIMIT` instead of building every row (D287). **49b-5 done** (D306): users can now let an agent insert rows into a table listed with columns (the hidden columns take their defaults) and change its listed columns (the hidden values, their indexes and checks are kept, and a write whose outcome would test a hidden value is refused whatever that value), insert into and change a listed table whose foreign key points at an unlisted one (the writer checks the key without showing that table's rows), and list a view with some of its columns; joined `UPDATE`/`DELETE`, `MERGE`, `OUTPUT ... INTO` and `SELECT ... INTO` find unlisted tables missing too. **49b-6 done** (D328): users can now let an agent store keys (`put`, with a time to live and a collection) and delete them (`del`, after their confirmation every time), and limit `eggdb serve`'s MCP endpoint to some tables with `--mcp-tables`. Next: the 2026-07-28 protocol era; deleting rows of a table listed with columns; inserts whose hidden column takes a generated value |
| 26 | Release extra: collections, named key spaces in one file, in the API, the CLI, the shell and the UI | **written** (D139): users can now keep several named key spaces in one file beside the default one: `Database::create_collection`, `drop_collection`, `collection_names` and `collection(name)` for one-step `get`/`put`/`delete`, and `ReadTransaction::collection`/`WriteTransaction::collection` with the default key space's methods (`get`, `get_with`, `scan`, `scan_prefix`, `put`, `put_if_absent`, `delete`), several collections and the default key space changed in one transaction; collections as key prefixes in the file's one tree (`0xff 'C' 'O' 'L' 0x00`, a catalog of names to never-reused numbers, no feature flag), which the default key space neither shows nor accepts (`ErrorKind::ReservedKey`), missing ones and bad names as `CollectionNotFound` and `InvalidCollectionName`; `eggdb collection list\|add\|delete` and `-C`/`--collection` on `get`, `put` (`--create` makes it), `del`, `scan`, `export` and `import`; eggdb-jsonl version 2 tagging entries with their collection and keeping empty ones as collection lines (written only for files with collections; `export -C` writes version 1, `import -C` loads everything into one collection); `.collection` in `eggdb shell` with the collection in the prompt; in `eggdb ui` the `C` picker (which also adds collections), the sidebar from 110 columns, `Keys in <name>` and the collection in the confirmations, designed by Claude Fable ([ux/ui.md](ux/ui.md), [ux/cli.md](ux/cli.md)); API, model, sample-file, CLI, shell, UI-scenario and smoke tests. Users can now rename a collection in one commit (`Database::rename_collection`, `eggdb collection move`, alias `rename`, refused with `AlreadyExists` when the name is taken), count each collection's entries and bytes (`Database::collection_stats`, `eggdb stats --collections`), read collections from MCP's `kv_get` and `kv_scan` with a `collection` argument (withdrawn with the tools under `--tables` and agent roles), and drop a collection of any size in steps of 512 keys behind a `0x06` mark that hides it from the first commit and that later commits and `Database::finish_collection_drops` finish after a crash (D292). Each collection keeps its entry count up to date on every write, so `eggdb collection list` shows counts and `stats --collections` is instant; `eggdb shell` and `eggdb ui` rename collections, and a long delete shows its progress (D320). Not yet: SQL tables in a collection (D98's move of SQL into a collection named `sql` is not done) |
| 3.17–3.18 | Users can now copy a running PostgreSQL (logical replication) or SQL Server (change tracking) database into EggDB and keep it in step until they switch over, with `eggdb follow` | **written** (D162): our own PostgreSQL client (TLS, SCRAM-SHA-256-PLUS, `COPY`, copy-both) and `pgoutput` reader, our own TDS client, a temporary slot's snapshot copied with schema, keys, indexes, foreign keys and sequences, each source transaction applied as one transaction, change tracking polled for SQL Server, preconditions checked before the copy (Fable's design in ux/cli.md section 30); CI's `pg-follow` job follows PostgreSQL 17 under writes and compares ([cli.md](cli.md), [migrate.md](migrate.md)). Users can now keep EggDB in step with a SQL Server database through its CDC change tables as well as change tracking, chosen per table or forced with `--changes`, and a follow stops with a clear message when CDC cleanup removes changes it has not applied (D261; CI's `ss-follow` job); users can stop and resume `eggdb follow` without copying again, it retries a lost connection for `--reconnect-for` seconds, and columns added, dropped, renamed or retyped on a PostgreSQL source follow into the file, while SQL Server column changes stop it with a clear message (D263). SQL Server follows now read CDC capture instances of some columns (the rest named), use net changes, retry a connection lost in the middle of a poll without losing or repeating a change, drop columns dropped on the source, and switch to a second capture instance after a column is added (D282; CI's `ss-follow` job). From PostgreSQL 14, `eggdb follow` reads `pgoutput` protocol 2: large transactions stream before they commit and are kept in memory or a spool file until their commit (dropped at abort, savepoint rollbacks cut out), tuples arrive in binary when every followed type is readable, and a table created or published on the source during a follow is created and copied from its own snapshot (D296; CI's `pg-follow` job). Not yet: `streaming 'parallel'` and two-phase messages, binary arrays, domains, enums and ranges, SQL Server column type changes |
| 3.19–3.22 | Users can now browse EggDB in pgAdmin, DBeaver and SSMS, and rely on SQLAlchemy's own dialect suite | **written** (D163): pgAdmin's statistics views, extension catalogs and administration functions (`pg_catalog/monitoring.rs`), MySQL's `information_schema` and `SHOW ENGINES`/`CHARACTER SET`/`COLLATION`/`PLUGINS`/`PROCESSLIST` for DBeaver and MySQL tools (`mysql_catalog.rs`), SSMS's server views, three-part names and permission functions, the `sqlalchemy-suite` CI job with a pass count that may only rise, and [compatibility.md](compatibility.md). Users can now run SQLAlchemy's reflection suite (`ComponentReflectionTest`), whose setup errors were 237 of the 250 counted on `main`: its setup now creates `test_schema` and `test_schema_2`, and materialized views are created inside a transaction block and in any schema; the suite runs to its end and the committed minimum is 96, main's count (D308). Next: `relkind` `m` for materialized views in `pg_class`, `nextval` of a `SERIAL` column's sequence, two identity columns in one table and identity `MAXVALUE`/`MINVALUE`/`CYCLE`, `CREATE TABLE t ()`, the requested isolation level in `SHOW transaction_isolation`, psycopg's named (server-side) cursors, a bound frame offset (`ROWS BETWEEN $1 PRECEDING`) and multidimensional arrays |
| 27 | Release extra: expiring keys (TTL) and atomic counters, in the API, the CLI, the shell and the UI | **written** (D140): users can now give keys a time to live in the default key space and in collections (`WriteTransaction::put_with_expiry`, `set_expiry`, `clear_expiry`, `time_to_live`, with one-step versions on `Database` and `Collection`), never read an expired key (gets, scans and writes treat it as absent, as of the transaction's time, never earlier than the last commit's stamp), have expired keys removed by every commit (up to 32) and by `Database::remove_expired`, and change values atomically with counters (`increment`, decimal text from `i64::MIN` to `i64::MAX`, `ErrorKind::NotAnInteger`) and compare-and-set (`compare_and_set`); `Options::clock` takes a `SimClock` for tests; the first time to live sets the read-only-compatible `EXPIRING_KEYS` feature bit; `eggdb put --ttl`, `ttl`, `expire`, `persist`, `incr`, `decr` and `cas` with `-C`, the same words in `eggdb shell`, and in `eggdb ui` the time left in the key list and viewer, `t` to set or clear it and `+`/`-` for counters (Fable's design in ux/cli.md section 21 and ux/ui.md); simulated-clock tests across a crash and a reopen, threads that increment together, CLI, shell, UI scenario and sample-file tests, seeded fuzzers, a smoke step ([embedded.md](embedded.md), [cli.md](cli.md), [format.md](format.md)) |
| 28 | Release extra: online backup and compaction (`eggdb backup`, `eggdb compact`) | **written** (D141): users can now back up a database while it works (`Database::backup_to`, `eggdb backup <file> <copy>`, `backup <copy>` in `eggdb shell`): a copy of one commit, rebuilt entry by entry with every collection, SQL table, time to live and feature flag, built as `<copy>.backing-up`, synced, checked and renamed, so a crash never leaves part of a copy and a backup never replaces a file; and compact a file (`Database::compact`, `Database::compact_file`, `eggdb compact <file>`): checked, rebuilt as `<file>.compacting` without free or leaked pages and renamed over the file atomically while readers in other programs keep working; reports in text and JSON (Fable's design in ux/cli.md section 22), the reserved-command message retired; a fault-injection sweep that fails every operation of a backup and of a compaction followed by a reopen or a power loss, a backup taken while a writer commits, CLI and smoke-test steps ([embedded.md](embedded.md), [cli.md](cli.md), [correctness.md](correctness.md)) |
| 29 | Release extra: a C API with `eggdb.h` and a Python ctypes example | **written** (D142): users can now embed EggDB from any language that calls C, through `eggdb.h` and the `eggdb-capi` library (shared and static): `eggdb_open` with options and `eggdb_close`, read and write transactions, `eggdb_get`/`eggdb_put`/`eggdb_delete` in the default key space or a collection, scans read in batches (range, prefix, reverse, resumable in a later transaction), collections, times to live, counters and compare-and-set, SQL sessions in every dialect with `eggdb_sql_execute` and `eggdb_sql_execute_with_parameters` (results as typed values with their printed text, read in place with the inline `eggdb_result_value`), backup and compaction; a status from every call with the message and SQLSTATE per thread, opaque handles, values the caller frees with `eggdb_free` and batches the handle owns, every misuse answered with a status; `examples/python/eggdb_ctypes.py` wraps it with `ctypes` alone; Rust tests of every function, a test that checks `eggdb.h` against the code, a C program and a C++ compile of the header with warnings as errors plus the Python example in CI (Linux and macOS; Python on Windows), and `eggdb-capi` archives with each release ([c-api.md](c-api.md)). Users embedding EggDB from C can now prepare a statement once and run it many times with typed and binary parameters, cancel a running statement from another thread, and bulk-load rows with `COPY FROM STDIN` (D268). Users embedding EggDB from C can now step through a statement's rows one at a time with typed getters (`eggdb_stmt_step`, `eggdb_stmt_row`, `eggdb_stmt_value_*`), read a `COPY ... TO STDOUT` in pieces and feed a `COPY ... FROM STDIN` piece by piece (`eggdb_copy`), and use prepared statements, stepping, cancelling and `COPY` on a session from `eggdb_sql_connect`, where rows and copy data stream over the connection (D293). Not yet: a parsed plan kept in a prepared statement, rows and copy data streamed out of the engine itself in the process, typed parameters declared to a server at prepare, the new calls in the per-language drivers (D16), async calls, the remaining open options |
| 50 | The Egg command model with EggOS (owner, 2026-10-28): the same verbs, JSON answers, errors with shared categories and safety fields for people and agents, in three slices (1: the commands agents use; 2: every command and new CLI twins `table list\|show\|sample`, `explain`, `sql --read-only`; 3: paging and previews) | **slice 1 written** (D143): users and agents can now get JSON (`--json`, the same as `--format json`) from `get`, `scan`, `check`, `stats`, `export` and, new, `sql` (one line per statement, the MCP result without its envelope); errors with a code, a category shared with EggOS, the message, the usage line and the `try:` command as `fix_command`, one JSON line on stderr whenever JSON was asked for (a pipe alone keeps text); `eggdb help --json` and `eggdb help <command> --json` from the registry; MCP errors with `category` (and `fix_command` for `damaged`); and MCP `instructions` generated to teach the effects, the session's grant and the categories. The new `eggdb-commands` crate holds the command and tool definitions, the CLI's help and the MCP `tools/list` are read from it (byte for byte as shipped), and the registry, parity (each tool against its CLI twin, by value and by error code and category, through the built binaries), help and doc tests guard it ([ux/command-model.md](ux/command-model.md), [ux/cli.md](ux/cli.md) section 23). **Slice 2 written** (D144): users can now ask every command for `help --json` (all 33 commands are in the registry; `help table --json` gives one line per verb), get `eggdb version --json`, and use the MCP tools' new CLI twins: `eggdb table list|show|sample <file> [<table>]` (tables and views, one in full with its row count, columns, keys and indexes, or its first rows), `eggdb explain <file> -c <sql>` (the plan, in the dialect's EXPLAIN form) and `eggdb sql --read-only` (beside other readers; a write fails with SQLSTATE 25006, `read_only` in JSON), each returning the same values as its tool, checked by the parity test through the built binaries; a missing table is `table_not_found` (exit 1, `try: eggdb table list <file>`); the catalog code is shared with the tools in `eggdb_sql::relations`; every example of a reading command runs as a test ([ux/cli.md](ux/cli.md) section 24, Claude Fable's design). **Slice 3 written** (D145): users and agents can now read a table a page at a time with `eggdb row list <file> <table> [-n <count>] [--after <cursor>]` and the new MCP tool `row_list` (`limit`, `after`; answers `{items, truncated, truncated_by, next_cursor}`), with a cursor that names the position after the last row, so pages neither repeat nor skip rows while a writer commits between them (tested over a 10,000-row table); page `eggdb table list` the same way; preview a restore or a CSV load exactly with `eggdb restore --dry-run` and `eggdb import --csv --dry-run`, the real run on a scratch copy that is then removed, so the file and its sequences never change; and read each tool's confirmation check in `help --json` (`confirm_when`), from which the MCP server now picks its confirmation flow ([ux/cli.md](ux/cli.md) section 25, Claude Fable's design) |

**Scope** (owner, 2026-10-27, D110): v0.2.0 brings PostgreSQL, SQL Server and Oracle databases
into EggDB by restoring the dumps and scripts their own tools produce; EggDB never runs, wraps or
proxies another engine.

**Next**, in the owner's work order of 2026-10-27: 19c (done), 19d (done), then items 30 to 38, 23a and 23b, 39 to 48, and the
release extras 26 to 29, as far as the freeze on 2026-10-30 23:59 allows. Items 22 (MySQL) and
24 (SQLite) are done, and so is the core of 25 (Oracle); the rest of Oracle is folded into items
42 to 44. The cut
order and what is never cut are in D76.

## Where we are
The platform layer, the storage engine with its write-ahead log, transactions and the embedded
key-value API, the `eggdb` command and its UI exist, and v0.1.0 is released. The batch of
2026-10-29 to 2026-11-01 gave every milestone a first part: SQL in five dialects with their
procedural languages, servers for the PostgreSQL, MySQL, TDS, RESP, MongoDB, Elasticsearch, CQL,
Kafka, AMQP, MQTT, NATS, STOMP, SQS, S3, DynamoDB and Bolt protocols, and first pieces of every
data model and way of scaling. Most of them are first parts: [What is left, per milestone](#what-is-left-per-milestone)
lists what is still to build for each. Measured sizes and budgets are in D115 and D253, checkpoints in D168.

| Area | Status | Missing |
|------|--------|---------|
| Platform layer (M1) | files with positional, direct and asynchronous I/O and shared or exclusive file locks (a completion queue on io_uring on Linux, I/O completion ports on Windows on request, and a worker pool elsewhere), aligned buffers, directory sync, clocks, whether a standard stream is a terminal and takes colour, memory (page sizes, explicit and transparent huge pages), CPU sets, thread pinning on Linux and Windows and pinned thread starts, cache padding, CPU topology (cores, packages, NUMA nodes, last-level caches) on Linux, Android and Windows with counts on Apple's systems, the same error for the same wrong path on every OS, deterministic crash, fault, memory, CPU and I/O-ordering simulation; TCP listeners and connections through a completion queue that waits in epoll on Linux and Android, in kqueue on Apple's systems and in `poll(2)` on FreeBSD, NetBSD, illumos and Solaris (a portable poller elsewhere), with a simulated network of hosts (ordered bounded streams, seeded delivery, latency, partitions, resets, injected faults) on a simulated clock; retransmission timeouts across long cuts, one-way cuts and per-host latency and bandwidth in that simulation, `NotConnected` after a reset, and no readiness call in a reap already satisfied (D314); timed reaps and cancel on the network queue; sockets through io_uring on Linux and I/O completion ports on Windows, on request (D165); UDP sockets on every OS and in the simulated network (D315); a memory allocator of our own with per-thread caches and a budget, opt-in (D166); running a child process and reading its own peak memory | IOCP as Windows' automatic choice (after a benchmark), batched io_uring submission, timed reaps and cancel on the file I/O queue, non-blocking connects and CI runners on the BSDs and illumos, OpenBSD and DragonFly, Windows named pipes for the mysql client (local sockets are written on Unix systems, part 4.M1-1, D164, and on Windows 10 version 1803 and later through its own `AF_UNIX` sockets with an owner-only access list, D267), per-device alignment, NUMA memory placement, performance and efficiency cores, pinning and topology on the BSDs, file locks on Solaris (D78), and the M1 wrap-up review's follow-ups: a guard against completion packets after an immediate IOCP success, and tests that cancel a pending connect and mix port and worker operations on IOCP |
| Storage and transactions (M2, M3) | the design (D77), the file format's spec ([format.md](format.md)), CRC-32C with the processor's instructions, the superblock in two header slots, page headers and their checksums, the page file: crash-safe commits, growth, a free list, rollback, one writer or many readers by file lock; the page cache: a fixed budget allocated once, clock eviction, pinned page handles, changes kept until commit, statistics; a benchmark harness with counts gated exactly and times gated as ratios (D80); the write-ahead log: segments inside the file, checksummed records, commits that log whole pages before writing them in place, group commit, the Full, Normal and Off durability modes, redo on open that tells a torn tail from damage (D81); the B+tree: get, put, splits up to the root, fence keys, prefix truncation, every page checked when read from the file, and a whole-tree check (D82), delete with merges and root collapse, and cursors for range, prefix and reverse scans (D83); checkpoints, recovery from the last checkpoint and log segments recycled with never-reused sequence numbers (D84); a crash sweep across the whole stack (D85, [testing.md](testing.md)); transactions: one writer, snapshot readers on any thread through before-images, atomic commit, rollback, hybrid-clock commit stamps (D86); the embedded Rust API, `eggdb::Database` ([embedded.md](embedded.md), D87); values up to 1 GiB on chains of overflow pages, and a check that accounts for every page of the file (D88); a model test against an in-memory map and a fuzzer for hostile files (D89); long commits that no longer grow the file, and batched random overwrites faster than SQLite's (D97) | the rest of the v0.2.0 list above, then MVCC, serializable isolation, partitioning and the LSM bake-off; streaming reads of large values; a checkpoint that restarts the log without header syncs, so `Normal` commits sync as rarely as SQLite's (D97) |
| Models (D2) | key-value; relational: tables, indexes, joins, aggregates, views, routines and triggers (D98 to D163); documents (D111, D197); full-text search (D195, D196); time series and columnar (D201, D202); vectors and geospatial (D203 to D205); graphs and the Bolt protocol (D206, D207); wide-column, objects and queues (D213 to D219) | what is left for each model is listed in [What is left, per milestone](#what-is-left-per-milestone) |
| SQL dialects (D10, D47) | PostgreSQL, first parts: `CREATE`/`DROP TABLE`, `INSERT`, `SELECT` with `WHERE`, inner, left, right, full and cross joins, subqueries, `WITH` and set operations in all four dialects (D112), `GROUP BY` with aggregates, `HAVING`, `DISTINCT`, `ORDER BY` and `LIMIT`, `UPDATE`, `DELETE`, transaction blocks, integer, float, boolean and text types, through `eggdb sql` (D98, D100); exact `numeric`, `date`, `timestamp`, `timestamptz`, `json`, `jsonb`, `serial` and identity columns in all four dialects and both protocols (D111); `CREATE [UNIQUE] INDEX`, `DROP INDEX` and `UNIQUE` in every dialect, and `EXPLAIN` in PostgreSQL's and MySQL's style and SQLite's `EXPLAIN QUERY PLAN` (D109), Oracle's `EXPLAIN PLAN` into `PLAN_TABLE` with `DBMS_XPLAN.DISPLAY`, and SQL Server's `SET SHOWPLAN_TEXT`, `SHOWPLAN_ALL` and `STATISTICS PROFILE` (D312); MySQL, first parts on the same tables, with MySQL's errors (D106); SQLite, first parts on the same tables, with SQLite's errors (D107); Oracle, first parts on the same tables, with `ORA-` errors (D108); SQL Server's Transact-SQL, first parts on the same tables (`[brackets]`, `TOP`, `IDENTITY`, `GETDATE()`, batches without semicolons), with SQL Server's error numbers (D122) | the rest of PostgreSQL, MySQL/MariaDB, SQLite and Oracle, T-SQL, then GoogleSQL, Snowflake, Spark, ClickHouse, Trino, Db2, Sybase |
| Protocols (D5) | PostgreSQL v3 (D103), MySQL's client/server protocol (D106) and SQL Server's TDS 7.4 for go-sqlcmd (D122) and for Microsoft.Data.SqlClient and mssql-jdbc with RPC, prepared statements and transaction manager requests (D132) and bulk loads from `bcp in` and `SqlBulkCopy` (D136), over TLS 1.3 and 1.2 of our own on any address, or on loopback without TLS (D102, D161) | RESP, MongoDB, Elasticsearch, CQL, Kafka, AMQP, MQTT, NATS, STOMP, SQS, S3, DynamoDB and Bolt have first parts (D193 to D219); left: their full command sets, and one shared listener crate for the protocol servers (D122) |
| Platforms (D8) | Linux (glibc, and musl on x86-64), macOS and Windows built and tested in CI, and release builds for x86-64 and AArch64 on each, run where the runner can (D75); the BSDs, illumos, Solaris, Android, iOS and WASI type-checked in CI | first parts of the browser build and the mobile libraries (D221, D222); left: EggOS, `no_std`, CI runners on the BSDs |
| Scale (D4, D18) | first parts: replication and failover (D209), change streams (D210), Raft (D211), shard maps and two-phase commit (D212), leaderless quorums (D225) | what is left for each way of scaling is listed in [What is left, per milestone](#what-is-left-per-milestone) (M14, M15, M18) |
| Build and tests (D335) | one integration-test program per package (`tests/all`), checked in CI by `scripts/check-test-layout.sh`; debug and test builds keep line tables only; a clean `cargo test --workspace --no-run` builds 72 programs in 2.7 GB instead of 153 in 4.7 GB (5.6 GB with full debug information) | nothing |

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
behavior (D19). Users can now `CREATE EXTENSION citext` and `hstore`: `citext` compares,
orders, groups, indexes and matches `LIKE` through one case fold (D233's), and `hstore` brings its
operators, functions and PostgreSQL's text and binary forms (D259); SQLAlchemy's test setup now
creates both. Users can now use `citext[]` and `hstore[]` columns, hstore's `%%`, `#=` and
`populate_record`, and case-insensitive regular expressions on `citext` (D288). Next for
PostgreSQL's types: `hstore_to_matrix` and `%#` of a non-empty hstore (multidimensional arrays),
the `citext` versions of the regexp and string functions, and the part's second, security-only
review (D35).
Users can now read a sequence as a one-row relation (`SELECT last_value, is_called FROM s`, which
`pg_dump` sends) and query the `pg_sequences` view (item 32).
Users' row triggers now fire for rows that cascading foreign keys and MySQL's `REPLACE` change or
remove, so tables captured by `eggdb_cdc` and isolated by `eggdb_tenancy` can have cascades
(D260, item 33b).
Users can now change and delete rows through joins in every dialect that has them: PostgreSQL's
`UPDATE ... FROM` and `DELETE ... USING`, SQLite's `UPDATE ... FROM`, MySQL's multiple-table
`UPDATE t1 JOIN t2 ... SET t1.a = ..., t2.b = ...` and `DELETE t1, t2 FROM ...`, and Transact-SQL's
`UPDATE ... FROM` and `DELETE ... FROM ... JOIN` with `OUTPUT`, with `RETURNING` reading the joined
tables (D297). Next for joined `UPDATE` and `DELETE`: subqueries in their `SET` and `RETURNING`
clauses, a view, a partitioned table or a table with row-level security as their target,
`EXPLAIN` of them, SQLite's refusal of `FROM` tables in `RETURNING` of `UPDATE ... FROM`, and
Transact-SQL `OUTPUT deleted.` of a joined `UPDATE` (Msg 40517 today).
Users can now summarise at several levels in one query in every dialect that has it:
`GROUP BY ROLLUP`, `CUBE`, `GROUPING SETS` and `()` in PostgreSQL (with `GROUP BY DISTINCT`),
SQL Server (also `WITH ROLLUP` and `WITH CUBE`) and Oracle, MySQL's `WITH ROLLUP`, `GROUPING()`
everywhere but SQLite, `GROUPING_ID()` and Oracle's `GROUP_ID()`, in one pass over the rows
(D298). Next for grouping sets: SQL Server's and Oracle's one-argument limit for `GROUPING`
and their own error numbers for grouping-set mistakes, grouping sets in a query with window
functions checked against each manual, `GROUPING` of an outer query's keys inside a subquery
(`42803` today); MySQL's, SQLite's and SQL Server's NULL ordering is built (D321, D340).
Users can now keep one row per key with PostgreSQL's `DISTINCT ON (...)` in `ORDER BY` order,
join by every shared column name with `NATURAL [INNER | LEFT | RIGHT | FULL] JOIN` (PostgreSQL,
MySQL, SQLite, Oracle), name their own escape character with `LIKE ... ESCAPE` in every dialect
(and SQLite's `like(pattern, text, escape)`), match SQLite `GLOB` character classes and patterns
from columns, and create PostgreSQL `RECURSIVE` views (D299). Next for these: `DISTINCT ON` an
expression outside the select list of a grouped query or beside a window function, `NATURAL`
joins with a `LATERAL` item, SQLite's own error for an `ESCAPE` computed at run time that is
not one character (a constant one is refused as SQLite refuses it),
Oracle's `ORA-01424` for an escape character before anything but `%`, `_` or itself,
Transact-SQL's `[...]` classes in `LIKE`, SQLite's "no match" (not `22025`) for a `LIKE` pattern
ending in its escape character, `\` as an ordinary character in SQLite and Transact-SQL `LIKE`
without `ESCAPE`, and a `NATURAL` join's column-privilege check limited to the join's own group
(not items before a comma).
Users can now keep tied rows with PostgreSQL's `FETCH FIRST n ROWS WITH TIES` (and `FETCH FIRST n
ROWS ONLY` as `LIMIT`), Transact-SQL's `TOP n PERCENT` and `TOP n [PERCENT] WITH TIES` with SQL
Server's errors 1031 and 1062, and join each partition's values on every row with Transact-SQL's
`STRING_AGG ... OVER (PARTITION BY ...)` and Oracle's analytic `LISTAGG ... OVER (PARTITION BY
...)`, in `WITHIN GROUP` order (D300). Next for these: `TOP ... PERCENT` in `UPDATE`, `DELETE` and
`INSERT`, `LISTAGG (DISTINCT ...) OVER` and Msg 1006 for a plain Transact-SQL `TOP (NULL)` (`TOP (NULL) PERCENT` is built).
Users can now write Transact-SQL `MERGE` with two `WHEN MATCHED` clauses (one `UPDATE`, one
`DELETE`), `$action` in its `OUTPUT` and `MERGE TOP (n) [PERCENT]`; send the rows of `OUTPUT` on
`INSERT`, `UPDATE`, `DELETE` and `MERGE` into a table variable or a table with `OUTPUT ... INTO`
(with a second `OUTPUT` to the client), in one change with the statement; and create a table from
a query with `SELECT ... INTO new_table` in Transact-SQL (`#temporary` included) and PostgreSQL
(`INTO [TEMPORARY] [TABLE]`) (D303). Next for these: `deleted.` in a Transact-SQL `UPDATE`'s
`OUTPUT`, `CREATE TABLE ... AS`, `SELECT ... INTO` keeping the source columns' identity (their
`NOT NULL` is kept, D342), `DELETE` triggers fired by `MERGE`'s deletes, SQL Server's refusal of an `OUTPUT ...
INTO` target with triggers, checks or foreign keys, PostgreSQL's `MERGE`, describing a prepared
`OUTPUT ... INTO` that still returns rows, `SELECT ... INTO` deriving a computed column's
`decimal(p, s)` and a literal's own type as SQL Server does, dropping a procedure's `#temporary`
tables when it returns, and "The MERGE statement conflicted with the REFERENCE constraint" for a
`MERGE` delete that hits a referencing row.
Users can now walk hierarchies in Oracle SQL with `START WITH ... CONNECT BY [NOCYCLE] PRIOR
...`, `LEVEL`, `SYS_CONNECT_BY_PATH`, `CONNECT_BY_ROOT`, `CONNECT_BY_ISLEAF`, `CONNECT_BY_ISCYCLE`
and `ORDER SIBLINGS BY` (with `ORA-01436` for a loop and `SELECT LEVEL FROM dual CONNECT BY LEVEL
<= n` for counting), and fill several tables from one query with `INSERT ALL` and `INSERT FIRST
WHEN ... THEN INTO ... ELSE INTO ...` (D304). Next for these: `CONNECT BY` over a join (with the
join's conditions in `WHERE` applied first), `PRIOR` beside a subquery, `ORDER SIBLINGS BY` a
pseudo-column, a sequence's `NEXTVAL` advancing once per source row of a multitable insert, and a
benchmark of a deep hierarchy against a recursive `WITH` on SQLite.
Users can now keep SQLite's dynamic typing in columns of `TEXT`, `NUMERIC` and `REAL` affinity
and in columns without a declared type: each value keeps the storage class the affinity gives it
(`'3.0e+5'` in a `NUMERIC` column is the integer `300000`, `5` in a `TEXT` column the text `'5'`),
a string compared with a `NUMERIC` column takes its affinity, and these columns and `BLOB` ones
can be primary keys, `UNIQUE` and indexed, ordered as SQLite orders values (numbers by exact
value, then text, then blobs) by a class-ordered key encoding in table-definition version 13
(D305). Next for these: dynamic typing for `INTEGER` columns (text in an `INTEGER` column, reals
kept as reals), an integer compared with a `TEXT` column turned into text first, a `TEXT` or
untyped column's values turned into numbers when compared with a `NUMERIC` or `REAL` column,
`CAST(x AS NUMERIC)` giving integers, view columns keeping their source column's affinity,
per-value classes for SQLite columns in the C API's column kinds, and a benchmark of keyed
lookups on `TEXT` keys against SQLite. `PRAGMA table_info` shows the declared type as written
(D305).

More tracks that run alongside the milestones, each starting where its dependencies land:
- **The project (D21):** contributing guide, code of conduct, security policy, the stability
  promise, and signed and reproducible releases. Started: `CONTRIBUTING.md`,
  `CODE_OF_CONDUCT.md`, [stability.md](stability.md), a CI check that two builds are identical,
  and final size, memory and startup targets per build profile, checked by the weekly pass
  (D230, D354).
- **Security and compliance (D22, D23, D26):** with the server first, deepened every release.
  Started: hash-chained audit records and `eggdb audit --verify` (D231).
- **Operations (D24, D25):** backups and monitoring as soon as there is data to protect and a
  server to watch.
- **Programmability (D27):** each dialect's procedural language with that dialect, WebAssembly
  functions and extensions after the server. Started: our own WebAssembly interpreter with
  fuel, memory and call-depth limits (D232).
- **Time travel, derived data and change streams (D28):** after transactions (M3) and the server
  (M6).
- **Interchange and lakehouse (D29):** after columnar storage (M11). Started: Apache Arrow IPC streams from `eggdb export --arrow` (D240).
- **Text and locales (D30):** collation with SQL (M5), analyzers with full-text search (M8).
  Started: Unicode normalization, case folding and case- and accent-insensitive comparison from
  Unicode's own data, with `normalize()` in SQL (D233).
- **AI (D31):** after vectors (M12) and the MCP server (M6).
- **Relational completeness and multi-tenancy (D32):** across the SQL milestones, tracked as a
  conformance list. Started: tenants isolated by row with quotas, through `eggdb_tenancy`
  (D234).

From the second gap review (D37–D45):
- **M6 is split** into M6a to M6f (table above), with security in the first piece (D45).
- **Generated APIs and realtime (D37)** follow the server. Started: a REST API generated from the schema in PostgREST's
  conventions (D235).
- **The extra models (D38)** follow the SQL dialects. Started: append-only ledger tables chained by SHA-256 (D236).
- **Observability storage (D39)** follows time series (M11).
- **Engine internals (D40)** run from M1 (I/O) and M5 (the optimizer).
- **Embedded detail (D41)** runs from M4.
- **The console and notebooks (D42)** run with the client track. Started: ER diagrams in Mermaid and DOT from SQL (D237).
- **Legal checks (D43)** come before each release.
- **Public proof (D44)** runs from the first benchmarks. Started: a reproduction script with
  energy where measurable, and a weekly compatibility farm (D252).

More tracks (D46), and any further missing capability is added here without asking the owner:
- **Business intelligence:** our own dashboards, reports, semantic layer, scheduling, sharing and
  embedding. Started: scheduled reports and alerts to webhooks through `eggdb_bi`
  (D238).
- **ETL and ELT pipelines:** our own connectors, transformations, orchestration and change data
  capture. Started: [connectors.md](connectors.md) and a compatibility job
  running dbt against EggDB (D239).
- **Big-data compatibility:** a Spark connector, an Iceberg REST catalog, federated SQL and Kafka
  Connect.

From the third gap review (D47–D58), each after its foundations:
- **Dialects and APIs (D47–D49)** follow the SQL and protocol milestones. Started: KQL
  pipelines translated to SQL by `eggdb sql --kql` (D242).
- **Industry standards (D50)** follow the messaging work. Started: FIX, HL7 v2 and EDIFACT
  messages parsed into `jsonb` by SQL functions (D243).
- **Catalog and master data (D51)** follow the console. Started: a glossary, quality rules and
  lineage with OpenLineage events in `CREATE EXTENSION eggdb_catalog` (D244).
- **Sharing and privacy (D52)** follow branching (D28). Started: FF1 format-preserving tokens,
  Laplace noise and a k-anonymity check (D245).
- **Retention and sovereignty (D53)** follow backups and compliance. Started: write-once
  retention, legal holds and waiting erasure in `CREATE EXTENSION eggdb_retention` (D246).
- **Coordination (D54)** follows consensus (M15). Started: leases with fencing tokens, leader
  election and OpenFeature-style flags in `CREATE EXTENSION eggdb_coordination` (D247).
- **Streaming (D55)** follows the messaging work (M16).
- **Content (D56)** follows full-text search. Started: `extract_text` for Office and
  OpenDocument files on our own DEFLATE and ZIP (D249).
- **Science and GIS (D57)** follow analytics. Started: map tiles, Web Mercator and geohashes
  (D250).
- **Hyperscale operations (D58)** follow clusters. Started: resumable batch copies with verified
  differences in `CREATE EXTENSION eggdb_migration` (D251).

## What is left, per milestone
What each milestone still has to build: the vision (CLAUDE.md, D1 to D58) compared with what is
built (the tables above, D59 to D334 and PR #86), with every "Left:", "Next:" and "Not yet" line
of the decisions. Work from the tracks is listed under the milestone it builds on. Left out: D45's
"not planned" list, Oracle's own wire protocol (D10), and publishing to package registries and
other release steps. Items that need money, accounts or a paid license (X12, D50) are in the last
list, [Needs the owner](#needs-the-owner-money-or-accounts-d58). Each bullet is one buildable
item; "Check:" names what proves it. API compatibility with Google, Amazon, Snowflake,
Databricks, Microsoft, Splunk, SAP or IBM services comes after D58's legal check.

### M1 Platform layer
- I/O completion ports as Windows' automatic choice, and io_uring and IOCP sockets as the default
  network backends, once the benchmarks show it (D67, D69, D165). Check: the network benchmarks.
- Batched io_uring submission, and timed reaps and cancel on the file I/O queue (D72, Where we are).
  Check: simulator tests of a timed reap and a cancel on files.
- kqueue changes folded into the waiting `kevent` call, so a wait costs no change call (D74).
  Check: system calls per wait, measured on macOS and FreeBSD. **Code written, unbuilt** (code-only mode, 2026-11-10).
- Acknowledgements that take latency to return, and packet loss, duplication and reordering
  below the stream level in simulation (D314). Check: simulator tests of each.
- The keep-alive timing set on each accepted socket on macOS and NetBSD: macOS copies only
  `SO_KEEPALIVE`, the interval and the count from the listener, and NetBSD only `SO_KEEPALIVE`,
  none of the timing (D336); with the shared listener crate (D122). **Code written, unbuilt** (code-only mode, 2026-11-10). `eggdb_platform::accept_tcp` copies the
  listener's timing and every server's accept loop uses it.
- Non-blocking connects in the `poll(2)` backend on the BSDs and illumos (D73). **Code written, unbuilt** (code-only mode, 2026-11-10).
- OpenBSD and DragonFly on the `poll(2)` backend, with CI runners for them (D73). **Code written, unbuilt** (code-only mode, 2026-11-10, D356): VM jobs with the
  systems' packaged Rust, since rustup has no toolchain for them.
- Solaris: file locks, and sends that never raise `SIGPIPE` (D73, D78). File locks: **Code written, unbuilt** (code-only mode, 2026-11-10). `SIGPIPE`: left.
- Datagrams in the network completion queue (`NetOp`) and batched receives, for QUIC (D315).
- Windows named pipes, for the mysql client and SQL Server clients on Windows (D267, Where we are).
- Per-device alignment for direct I/O (Where we are). **Code written, unbuilt** (code-only mode, 2026-11-10). Uncached opens are refused when the device's
  alignment does not divide 4096 (`statx` on Linux, the logical sector size on Windows).
- NUMA memory placement and work placed by NUMA node, so one machine is used fully (D18, D65,
  PR #86). Placement: **Code written, unbuilt** (code-only mode, 2026-11-10, D356): `MemoryRegion::place_on_numa_node` on Linux. Left: the page cache
  and the shards using it.
- Performance and efficiency cores on Windows and macOS, and hot work placed on performance cores
  (D65, PR #86). **Code written, unbuilt** (code-only mode, 2026-11-10, D356): Windows' efficiency classes, macOS's `hw.perflevel0` count,
  `ThreadOptions::hot`. Left: marking the engine's hot threads.
- Thread pinning and CPU topology on the BSDs (Where we are). **Code written, unbuilt** (code-only mode, 2026-11-10). FreeBSD, DragonFly and NetBSD; OpenBSD has
  no affinity call.
- EggDB's own allocator as the default build, once the benchmarks show it (D166).
  Check: the benchmark lines with and without it.
- The M1 review's IOCP follow-ups: a guard against packets after an immediate success, and tests
  that cancel a pending connect and mix port and worker operations (Where we are). The guard: **Code written, unbuilt** (code-only mode, 2026-11-10). The tests: left.
- `NotConnected` after a reset checked against Windows' sockets in the contract test (D314).
- Optional accelerators behind build features: first CXL memory placed as a NUMA node for the
  page cache, with no `mmap`, on D65 and the NUMA item above (D40, D45). Check: cache benchmarks.
- Persistent memory and GPUs, each after a decision that fits D60 and D77 (no `mmap`, the log in
  the file) (D40, D45).

### M2 Storage engine
- Users can choose the LSM tree per table with `WITH (storage = lsm)`, kept in the catalog (D170).
  Check: the B-tree and LSM bake-off in the benchmarks, recorded as a decision.
- Dirty pages spill before commit, so a write needn't fit the cache (D86, D167). Check: a 1 GiB
  `put_reader` peaks near 32 MiB, the 16 MiB default cache plus snapshots (`eggdb/src/options.rs`).
- A Zstandard-class codec of our own (RFC 8878) for cold data (D171). Check: RFC test vectors and
  a fuzzer. **Code written, unbuilt** (code-only mode, 2026-11-10, D357): decoder, a simple encoder,
  overflow layout 3; Huffman-coded literals in the encoder: **code written, unbuilt** (code-only mode,
  2026-11-12, D638); FSE-compressed Huffman weights: **code written, unbuilt** (code-only mode,
  2026-11-12, D639); a long-running fuzzer (`eggdb-fuzz zstd`, with LZ4 frames, SQLite files and Arrow
  streams): **code written, unbuilt** (code-only mode, 2026-11-12, D662).
- Compressed leaf pages and log records, with compression chosen per column and page (D40, D171).
- Compression options in the CLI and `eggdb serve` (D171). **Code written, unbuilt** (code-only
  mode, 2026-11-10, D357): `--compress lz4|zstd|off`.
- Partitioning's leftovers from D316: Oracle's `DISABLE ROW MOVEMENT` (ORA-14402), the source's
  delete and the target's insert row triggers of a moved row (and the target's `INSERT`
  statement triggers, which fire for a moved row where PostgreSQL fires only the `UPDATE` ones;
  **code written, unbuilt**, code-only mode, 2026-11-10, D368),
  user-defined `IMMUTABLE` functions and operator classes in keys, unique indexes on expression
  keys (MySQL's: **code written, unbuilt**, code-only mode, 2026-11-10, D367; PostgreSQL refuses
  them too), `relispartition` and `pg_partition_tree` for partition indexes (**code written, unbuilt**,
  code-only mode, 2026-11-10, D365), attaching a partition's primary key to its parent's
  (**code written, unbuilt**, code-only mode, 2026-11-10, D366), MySQL's allowed partition
  functions and integer-only keys, and
  `42883` for an unknown function in a PostgreSQL key (D316). MySQL's functions and integer-only keys and
  `42883`: **code written, unbuilt** (code-only mode, 2026-11-10, D359). Oracle's `DISABLE ROW
  MOVEMENT` and `ORA-14402`: **code written, unbuilt** (code-only mode, 2026-11-10, D361).
- Oracle's interval partitioning and SQL Server's partition schemes (D169); interval
  partitioning: **code written, unbuilt** (code-only mode, 2026-11-10, D362; left: an `UPDATE` that
  moves a row above the transition, `SET INTERVAL`); MySQL's
  `SUBPARTITION BY`, `LOAD DATA ... PARTITION`, `INSERT IGNORE` skipping rows outside a
  `PARTITION (...)` set with a warning, and ids for rows written straight into a partition of a
  table with an identity column (D337). `SUBPARTITION BY` (MySQL and Oracle's composite
  partitioning): **code written, unbuilt** (code-only mode, 2026-11-10, D361); left: subpartitions
  in the maintenance `ALTER`s and Oracle's `SUBPARTITION TEMPLATE`. `INSERT IGNORE` skipping rows outside a `PARTITION (...)`
  set: **code written, unbuilt** (code-only mode, 2026-11-10, D360). Ids for rows written straight
  into a partition: **code written, unbuilt** (code-only mode, 2026-11-10, D363). `LOAD DATA ...
  PARTITION` waits for `LOAD DATA` itself (M10).
- An identity or `AUTO_INCREMENT` column anywhere in the primary key, as MySQL allows, with table
  definition version 15 and its D35 review (D337). **Code written, unbuilt** (code-only mode,
  2026-11-10, D364).
- MySQL's and Oracle's partition `ALTER`s (add, drop, split, exchange) (D169). **Code written, unbuilt**
  (code-only mode, 2026-11-10, D358). `COALESCE PARTITION`, `MERGE PARTITIONS` and `MODIFY PARTITION
  ... ADD | DROP VALUES`: **code written, unbuilt** (code-only mode, 2026-11-10, D360); `REBUILD`, `ANALYZE`,
  `CHECK`, `OPTIMIZE` and `REPAIR`: **code written, unbuilt** (code-only mode, 2026-11-12, D629);
  Oracle's `MERGE PARTITIONS a TO b` range form: **code written, unbuilt** (code-only mode, 2026-11-12, D630).
- Online upgrades with a tested downgrade path to the previous minor version (D20, D35). Check:
  the upgrade, downgrade and mixed-version file tests in CI.
- Online schema changes and reindexing that block neither reads nor writes (D20). Check: a
  concurrent workload test across an `ALTER TABLE` and a reindex.
- Learned indexes, first over the LSM tree's sealed runs, after the per-table LSM switch above,
  with a D35 review if they are stored (D40, D170). Check: point gets with and without them.

### M3 Transactions
- Users can run SQL sessions on concurrent transactions, with each dialect's isolation levels and
  errors (D172, D173). Check: Flyway's default two-connection lock (D138).
- Users can choose the isolation level per database, table or request, from strict serializable
  to read committed with per-statement snapshots (D18, D173).
- Row versions in the storage engine (MVCC), so changes larger than memory spill (D172).
- Collections and expiring keys inside concurrent transactions (D172).
- Waiting on a predicate lock instead of failing at commit (D172).
- Row locks that hold other writers: `FOR UPDATE`/`FOR SHARE` with `NOWAIT` and `SKIP LOCKED`
  across concurrent sessions (D32, D173).
- Deadlocks between PostgreSQL advisory locks and row locks detected with `40P01` (D173); SQL
  Server's application locks already detect cycles among themselves (D291), the start to build on.
- Two-phase commit and XA: `PREPARE TRANSACTION`, `COMMIT PREPARED`, MySQL's `XA` and TDS's
  distributed transactions (D32, D173).
- A failing Oracle statement trigger or SQL Server `AFTER` trigger undoes only its own
  statement, as those databases do (D317). Oracle's: **code written, unbuilt** (code-only mode,
  2026-11-10, D372); SQL Server keeps rolling the transaction back, as it does (D372).
- A failing statement inside a MySQL `CALL` keeps the procedure's statements before it (D317).
  **Code written, unbuilt** (code-only mode, 2026-11-10, D370).
- SQL Server rolls back the whole transaction for a batch-aborting error that fails before
  writing, and dooms the transaction for such an error inside `TRY` (D317). **Code written,
  unbuilt** (code-only mode, 2026-11-10, D371).
- SQLite's conflict clauses of table constraints (D317), and `UPDATE OR FAIL | IGNORE | REPLACE`
  with `FROM` or on a view (D338). `UPDATE OR ...` with `FROM` and on a view: **code written,
  unbuilt** (code-only mode, 2026-11-10, D373).
- A benchmark of a bulk write inside a transaction in MySQL's, SQLite's, Oracle's and SQL
  Server's dialects, measuring statement-level rollback's extra read per key (D317).
- PostgreSQL's exported snapshots: `pg_export_snapshot()` and `SET TRANSACTION SNAPSHOT` (D318).
  **Code written, unbuilt** (code-only mode, 2026-11-10, D369). Left: exporting from a read-write
  block.
- The locking clause kept in the statement's tree, so `FOR UPDATE` in a read-only transaction is
  found exactly: a column named `share` after `FOR`, `FOR UPDATE OF` naming only temporary
  tables, and an outer clause over a `WITH` query, which PostgreSQL does not lock (D338).
- `ROLLBACK TO SAVEPOINT` undoes key-value writes made through `eggdb serve`'s RESP and wire
  sessions once those share a block with SQL (D319).
- The savepoint undo log bounded as a whole against the session's memory budget, or spilled to
  disk: each statement's before-images stop at 1 GiB with `53200` (D317), but the log keeps every
  statement's and key-value write's entries until `RELEASE`, `ROLLBACK TO` or `COMMIT` (D266,
  D307, D319).

### M4 Embedded key-value and the single file
- A benchmark against SQLite's `:memory:` in the compare job (D175).
- `ATTACH` of in-memory, EggDB and SQLite files, in every dialect that has it (D175, sql.md).
- SQL tables kept in their own collection, named `sql` (D139: "SQL in collections is later work").
- Collections in the C API and the language drivers beyond D139 (D292).
- SQLite's `speedtest1` and the standard embedded benchmarks on the `embedded` profile (D41).
  Check: the compare job, gated.
- On-disk overhead per row and per index entry, measured with a target beside the others (D6,
  D354).
- The `server` profile split of the binary, with its own size, idle-memory and first-start targets
  (D230, D354), and `serve`'s time to its first line in `scripts/footprint.sh`. Check:
  `benchmarks/footprint.txt`.
- YCSB workloads against SQLite and Redis (D7).
- A published branch-coverage target for the core, rising every release toward SQLite's (D41).
- Published descriptions of the CI runners the benchmarks use, a public results page, conformance
  and benchmark dashboards, and the farm's matrix of every driver and ORM version (D32, D252).
- Joules per query per workload and idle wake-ups, reported and then gated (D44, D252).

### M5 SQL engine, SQLite, C API and embedded drivers
- Column statistics (`pg_stats`, histograms, correlation) gathered by `ANALYZE` (D40, D178). **Code written,
  unbuilt** (code-only mode, 2026-11-12, D786). Left: extended statistics (`CREATE STATISTICS`), array element
  statistics, and the cost-based optimizer using the histograms for ranges.
- A cost-based optimizer that orders joins by cost (D40, D178). Check: TPC-H's joins, and the
  Linux join at 1.0 times SQLite's (D100).
- Adaptive re-planning during execution (D40).
- Point `SELECT`s as fast as SQLite's everywhere (item 19e, D99). Check: the compare job.
- SQL joins by key at 1.0 times SQLite's on Linux: measured by CI's Linux `compare` job after
  the per-row pass, then `linux sqlite_compare.sql_join` (1.396) lowered in
  `benchmarks/baseline.txt`, with the Linux join's `perf` profiles (item 19e, D7, D100).
- The 1.0 check of every compare line on Linux and macOS in CI's `compare` job (item 19e, D7).
- Sorts, joins and groups that spill to disk instead of failing at 1 GiB, and a top-N for
  `WITH TIES`, `PERCENT` and `DISTINCT ON` limits, which still sort every row (D100, D341).
- A resumable executor that streams rows, so portals, the C API and `COPY TO` send rows as they
  come (D284, D287, D293).
- Set-returning functions stopped by a `LIMIT` given as a parameter or an expression, joined with
  other tables, or filtered as their rows arrive (D287).
- Vectorized scans, filters and projections over rows and columnar chunks, float and numeric
  kernels and grouped aggregates, with SIMD chosen at runtime (D40, D179, D201).
- A query compiler for hot prepared statements' expressions, compiled to a flat program, after
  parsed plans are kept in prepared statements (below) (D40). Check: TPC-H Q1 and Q6 times.
- `TABLESAMPLE`, t-digest and the other `APPROX_*` functions (D40). `TABLESAMPLE`: **code written, unbuilt** (code-only
  mode, 2026-11-11, D541); the `APPROX_PERCENTILE` family, exact: **code written, unbuilt** (D542); t-digest
  sketches for `APPROX_PERCENTILE_CONT`: **code written, unbuilt** (code-only mode, 2026-11-12, D620).
- TPC-C against PostgreSQL and MySQL (D7).
- Partial, expression and covering (`INCLUDE`) indexes, `CREATE INDEX CONCURRENTLY` and `NULLS NOT
  DISTINCT` (D32).
  `CONCURRENTLY` and storage parameters: **code written, unbuilt** (code-only mode, 2026-11-11, D543).
- GIN, GiST, SP-GiST, BRIN and bitmap index methods, and Oracle's bitmap, function-based and
  reverse-key indexes (D32, sql.md).
- Oracle's `STATS_*`, `CORR_S` and `CORR_K` functions and its 38-digit `NUMBER` results for the
  statistical aggregates (`CORR_S` and `CORR_K` coefficients and `STATS_MODE`: **code written,
  unbuilt**, code-only mode, 2026-11-10, D374, D375); `stddev`, `variance`, the hypothetical-set aggregates, `regr_*`,
  `covar_samp`, `covar_pop` and `corr` are built (D321, D340).
- Grouping sets: SQL Server's and Oracle's one-argument `GROUPING` and their own error numbers
  for grouping-set mistakes, grouping sets beside window functions checked against each manual,
  and `GROUPING` of outer references in a subquery; `ROLLUP`, `CUBE`, `GROUPING SETS` and
  `WITH ROLLUP` are built (D298). SQL Server's and Oracle's one-argument `GROUPING` and their error numbers: **code
  written, unbuilt** (code-only mode, 2026-11-12, D847).
- An index read in place of the sort when a nullable column whose NULLs must move is not the
  first `ORDER BY` column after the ones fixed by `=`, or a later `ORDER BY` column may hold
  NULLs (D341 reads the first such column in two ranges only).
- `DISTINCT ON` an expression outside the select list of a grouped query or beside a window
  function, `NATURAL` joins with a `LATERAL` item, SQLite's error for a wrong `ESCAPE` computed
  at run time and Oracle's for a wrong `ESCAPE`, and Transact-SQL's `[...]` classes in `LIKE` (D299). Transact-SQL's
  `[...]` classes: **code written, unbuilt** (code-only mode, 2026-11-12, D918).
- Joined `UPDATE` and `DELETE` with subqueries in `SET` and `RETURNING`, a view, a partitioned
  table or a table with row-level security as the target, `EXPLAIN` of them, SQLite's refusal
  of `FROM` tables in `RETURNING`, and `OUTPUT deleted.` of a joined Transact-SQL `UPDATE`
  (D297).
- PostgreSQL's `EXECUTE` of the extended protocol's named prepared statements (they and SQL
  `PREPARE` are kept apart), `USING`, `WITH (...)` and `TABLESPACE` in a plain `CREATE TABLE`, and
  `EXPLAIN TABLE t` (D342). `WITH (...)`, `USING` and `TABLESPACE` in a plain `CREATE TABLE`:
  **code written, unbuilt** (code-only mode, 2026-11-10, D386); `EXPLAIN TABLE t`: the parser
  already routes it, a correctness case added (unbuilt).
- SQL Server's `SELECT ... INTO` transferring the identity property (needs an identity counter
  outside the primary key, a file format change) and keeping `NOT NULL` through inner joins and
  of non-null expressions (D342).
- A correlated `(a, b) IN (SELECT ...)` looked up by key instead of scanned (D323).
- A data-modifying `WITH` query that resolves conflicts (sql.md); `CREATE RECURSIVE VIEW` is
  built (D299).
- Multidimensional arrays and array slices (D19, D288).
- Generated columns' rest (D322): SQL Server's `sys.computed_columns` rows, the generation clause
  in SQLite's, Oracle's and MySQL's `CREATE TABLE` text and `SHOW CREATE TABLE`, `ALTER COLUMN
  ... SET EXPRESSION`/`DROP EXPRESSION`, MySQL `MODIFY` of a generated column, virtual columns
  computed on read (then SQL Server's non-persisted computed columns over `GETDATE()` or
  `NEWID()`), generated columns in partitioned tables, user-defined `IMMUTABLE` functions and
  PostgreSQL's stable casts checked in generation expressions. `ALTER COLUMN ... SET EXPRESSION`/`DROP EXPRESSION`: **code written, unbuilt**
  (code-only mode, 2026-11-10, D385).
- The rest of `ALTER TABLE`: `ALTER COLUMN ... TYPE ... USING`, adding a serial or identity
  column, renaming what a view uses (D32). Adding a serial column: **code written, unbuilt**
  (code-only mode, 2026-11-10, D387); `TYPE ... USING`: **code written, unbuilt** (D388).
- A published SQL:2023 conformance list, first Part 2's feature IDs (E011, F031, ...) with each
  one's status and test (D32). Check: a CI test that every feature marked done has a case. Started: [sql-conformance.md](sql-conformance.md) and
  its test: **code written, unbuilt** (code-only mode, 2026-11-12, D578).
- The Unicode Collation Algorithm with CLDR tailorings (`COLLATE "de-DE"`), with MySQL's and SQL
  Server's collation names mapped onto it (D30, D233). Check: the collation conformance test.
- Collation keys in indexes, and locale-aware `upper` and `lower` (D233).
- Every legacy encoding the supported databases accept, listed per dialect, on the wire and in
  `COPY` (D30). PostgreSQL's single-byte encodings in `COPY ... ENCODING`: **code written, unbuilt** (code-only mode,
  2026-11-12, D830); the single-byte encodings as `client_encoding` on the PostgreSQL wire: **code written, unbuilt**
  (code-only mode, 2026-11-12, D866). Left: the multi-byte East Asian encodings and the other dialects' lists.
- WebAssembly functions: floating point and the rest of the 1.0 instruction set (D232). Check: the
  official spec test suite in CI. Floating point, `call_indirect` with a function table, the start function,
  non-trapping float-to-integer conversions and `memory.copy`/`memory.fill`: **code written, unbuilt** (code-only
  mode, 2026-11-12, D792); multi-value results and typed blocks: **code written, unbuilt** (code-only mode,
  2026-11-12, D861); imported functions answered by the host: **code written, unbuilt** (code-only mode, 2026-11-12,
  D908). Left: the spec test suite in CI.
- `CREATE FUNCTION ... LANGUAGE wasm`, with type mapping and host functions behind permissions
  (D27, D232). Scalar functions over numbers and booleans (`AS '\x<module>', '<export>'`): **code written,
  unbuilt** (code-only mode, 2026-11-12, D793); text and bytea through the module's memory: **code written, unbuilt**
  (code-only mode, 2026-11-12, D862); the side-effect-free host functions `eggdb.now_micros` and `env.abort`: **code
  written, unbuilt** (code-only mode, 2026-11-12, D908). Left: set-returning functions, host functions with effects
  behind permissions, and per-role limits.
- An extension API for types, index methods, functions and protocols, loaded as sandboxed
  WebAssembly with CPU, memory, time and output limits per role (D27, D45).
- A native-code compiler for hot WebAssembly functions (D27, D232). Check: benchmarks against
  other runtimes.
- Event triggers: PostgreSQL's event triggers, SQL Server's DDL triggers and Oracle's DDL and
  database event triggers (D27). PostgreSQL's (`ddl_command_start`, `ddl_command_end`, `sql_drop`): **code written,
  unbuilt** (code-only mode, 2026-11-12, D787); `login`: **code written, unbuilt** (code-only mode, 2026-11-12, D863).
  `table_rewrite` firing: **code written, unbuilt** (code-only mode, 2026-11-12, D864). SQL Server's DDL triggers
  with `EVENTDATA()`: **code written, unbuilt** (code-only mode, 2026-11-12, D872). Oracle's DDL triggers and `AFTER
  LOGON` and `AFTER SERVERERROR` triggers: **code written, unbuilt** (code-only mode, 2026-11-12, D873). SQL Server's
  `LOGON` triggers and Oracle's `BEFORE LOGOFF`: **code written, unbuilt** (code-only mode, 2026-11-12, D907). Left:
  SQL Server's server-level DDL events (logins, endpoints, databases), and Oracle's `STARTUP`, `SHUTDOWN` and other
  database events.
- Users can write SQLite files: the B-tree writer, the rollback journal and the WAL (D180).
  Check: files EggDB writes pass `PRAGMA integrity_check`. The b-tree writer for whole new files
  and `VACUUM INTO`: **code written, unbuilt** (code-only mode, 2026-11-12, D631). Left: editing a
  file in place with the rollback journal and the WAL.
- `WITHOUT ROWID` tables and UTF-16 databases in SQLite files (D180). Reading them (and importing them with
  `eggdb restore`): **code written, unbuilt** (code-only mode, 2026-11-12, D788); writing `WITHOUT ROWID` tables (the
  writer and `VACUUM INTO`): **code written, unbuilt** (D789). Left: writing UTF-16 files, which only editing a
  UTF-16 file in place would need.
- `eggdb restore` from an SQLite file into an EggDB file (D13, D180). **Code written, unbuilt** (code-only mode, 2026-11-12,
  D568).
- SQLite's dynamic typing for `INTEGER` columns (text kept as text, reals as reals), an integer
  compared with a `TEXT` column turned into text first, a `TEXT` or untyped column's values
  turned into numbers when compared with a `NUMERIC` or `REAL` column, `CAST(x AS NUMERIC)`
  giving integers, view columns keeping their source's affinity, and per-value classes in the C
  API's column kinds; the other affinities, untyped
  columns and keys on every dynamic column are built (D305). Check: keyed lookups on `TEXT` keys
  benchmarked against SQLite.
- SQLite's `VACUUM` in SQL, blob, JSON and math functions, `group_concat`, `total` and
  temporary triggers (PR #86, sql.md); `group_concat`, `total` and MySQL's `GROUP_CONCAT`:
  **code written, unbuilt** (code-only mode, 2026-11-10, D376); the math functions, `unhex`,
  `zeroblob` and `randomblob`: **code written, unbuilt** (D377); the reading and building JSON
  functions: **code written, unbuilt** (D378), the editing ones (D379), `->`/`->>` (D380) and
  `json_group_array`/`object` (D381), `json_each`/`json_tree` (D382), temporary triggers
  (D383) and `VACUUM`/`REINDEX` accepted (D384), and `VACUUM INTO`: **code written, unbuilt** (code-only mode, 2026-11-12, D631); `GLOB` classes and temporary views are built (D299,
  D151). Check: the SQLite suite.
- SQLite's own text in `sqlite_schema` after `RENAME COLUMN`, `DROP COLUMN` and renamed
  references in views and triggers (D273, D294). Tables' and indexes' text after `RENAME COLUMN` and
  `DROP COLUMN`: **code written, unbuilt** (code-only mode, 2026-11-12, D642). Left: renamed references in
  views and triggers.
- sqllogictest run with its pass rate published and a stated minimum (D10, D45). The runner
  (`eggdb-sqllogictest DIR --minimum PERCENT`): **code written, unbuilt** (code-only mode, 2026-11-12, D664).
  Left: the CI job over sqlite.org's files, the published rate and the stated minimum.
- `libsqlite3`: `sqlite3_create_function`, `sqlite3_backup_*` and the rest of the interface, with
  rows streamed per step (D181). Check: the sqlite3 shell and Python's `sqlite3` module in CI. Scalar
  `sqlite3_create_function[_v2]` with the `sqlite3_value_*` and `sqlite3_result_*` calls, and the status and
  statement calls (`sqlite3_errstr`, `sqlite3_get_autocommit`, `sqlite3_complete`, `sqlite3_interrupt`,
  `sqlite3_sql`, `sqlite3_column_decltype`, `sqlite3_bind_parameter_name`/`index` and others): **code written,
  unbuilt** (code-only mode, 2026-11-12, D790); named `:name`, `@name` and `$name` parameters: **code written,
  unbuilt** (code-only mode, 2026-11-12, D874). Left: user aggregates and window functions, collations,
  `sqlite3_backup_*`, and rows streamed per step.
- The C API: parsed plans kept in prepared statements, binary result formats, typed `Parse` on
  server sessions and the remaining open options, `check` and `stats` (D142, D268, D293).
- Asynchronous C API calls, completed by one callback per batch (D17, D293).
- Every language driver using the new C API calls (prepared handles, stepping, `COPY`) (D293).
- Each driver benchmarked in embedded mode against SQLite's binding in its language, as D17 lists
  them (D17, D182). Check: gated in CI.
- JDBC's `DatabaseMetaData`, and a JNI binding for Java before 22 (D16, D182). `DatabaseMetaData`: **code written,
  unbuilt** (code-only mode, 2026-11-12, D791). Left: the JNI binding.
- The Python driver through the CPython C API with buffer-protocol batches (D16, D17).
- SQL*Plus commands in `eggdb shell`, and `DESCRIBE`, `VARIABLE`/`PRINT`, `SPOOL` and `COLUMN`
  formatting (D150). `DESCRIBE`, `PROMPT` and `SPOOL`: **code written, unbuilt** (code-only mode,
  2026-11-11, D544); `VARIABLE`/`PRINT` with `EXEC :x := …`: **code written, unbuilt** (code-only mode,
  2026-11-12, D622); `COLUMN` formatting: **code written, unbuilt** (code-only mode, 2026-11-12, D633); wrapped
  text columns: **code written, unbuilt** (code-only mode, 2026-11-12, D685); output binds assigned inside PL/SQL
  blocks: **code written, unbuilt** (code-only mode, 2026-11-12, D686).

### M6a Server and the PostgreSQL protocol
- TLS on every listener beyond PostgreSQL, MySQL and TDS, so each may leave loopback once its own
  authentication (under its milestone) is in (D22, D161). Check: `tls-clients` per protocol.
- Replies measured before they are built (RESP's `EXEC`, single large values, a Bolt record, an
  HTTP response body), a Bolt result's rows waiting for `PULL` and the queues' stored messages
  counted, and AMQP deliveries that never fit refused instead of offered again each round; every
  listener's reads and RESP's pub/sub queues already hold the server's memory budget (D284, D307).
- One shared listener crate for the protocol servers (D122).
- Connections served from an event loop over the completion queue instead of a thread per
  connection (D103, D165). Check: a many-connections benchmark.
- TLS session resumption, and the processor's AES instructions (D161).
- HTTP/2, gRPC and QUIC as shared crates of our own (D34).
- Elasticsearch search and scroll hits sent as a stream, like the REST API's rows (D325). Search:
  **code written, unbuilt** (code-only mode, 2026-11-11, D463); scroll pages are whole responses of
  `size` hits (D467).
- S3 `GetObject` of a large object sent as a stream (D325, D188). **Code written, unbuilt** (code-only
  mode, 2026-11-11, D462).
- Prometheus query results sent as a stream (D325). **Code written, unbuilt** (code-only mode,
  2026-11-11, D464).
- Large HTTP request bodies handed to the handler as they arrive (D325).
- PostgreSQL's function call protocol (D103). **Code written, unbuilt** (code-only mode, 2026-11-11,
  D461).
- PostgreSQL's network address, geometric, bit string, `money`, `xml` and multirange types (D19). `xml` read as
  `text`: **code written, unbuilt** (code-only mode, 2026-11-12, D697).
- `tsvector`, `tsquery` and `jsonpath` as types of their own (D195, D289).
- `refcursor` and `refcursor[]` table columns kept as their own types, a file format change with
  its security review; `refcursor[]` values are built (D35, D301).
- `regprocedure`, transition tables in row-level triggers, and `pg_sequences.last_value` for
  identity columns (D158, D264, sql.md). `regprocedure`: **code written, unbuilt** (code-only
  mode, 2026-11-10, D390); `pg_sequences.last_value` for identity columns: **code written,
  unbuilt** (D391).
- Partial unique indexes and indexes on expressions, and their `ON CONFLICT` inference (the
  predicate implying the index's, `ON CONFLICT (lower(email))`); `COLLATE` and operator classes on
  conflict target columns; `DO UPDATE ... WHERE` (D324). `DO UPDATE ... WHERE`: **code written,
  unbuilt** (code-only mode, 2026-11-11, D466).
- SQL/JSON: `json_populate_record` in a select list and with a column alias list, a table's own
  row type as its base row, and `JSON_TABLE`'s `PLAN` clause; a non-NULL base row is built (D289,
  D302).
- `IS JSON` on a `json` value holding `\u0000`; PostgreSQL's `JSON_SERIALIZE`, `JSON_SCALAR`,
  `JSON()`, `IS JSON` and `.decimal()` are built (D302). **Code written, unbuilt** (code-only mode,
  2026-11-11, D552).
- `DateStyle` and `IntervalStyle` beyond ISO and `postgres`, and the special date and time inputs
  (D19, sql.md). `IntervalStyle`: **code written, unbuilt** (code-only mode, 2026-11-10, D389);
  the special date and time inputs: **code written, unbuilt** (code-only mode, 2026-11-11, D465);
  `DateStyle`: **code written, unbuilt** (code-only mode, 2026-11-11, D554).
- `hstore_to_matrix` and `%#` of a non-empty hstore, which need multidimensional arrays, and the
  citext part's security-only second review (D35, D288); citext's regexp and string functions
  are built (D302).
- `pg_dump --clean`, `pg_dumpall`'s roles and several databases, large objects and dumps in
  encodings other than UTF-8 (D118). `pg_dumpall`'s roles and privileges with
  `eggdb restore --roles`: **code written, unbuilt** (code-only mode, 2026-11-12, D848).
- Composite and range columns in `COPY`'s binary format, and `LOG_VERBOSITY verbose` notices sent
  before a `REJECT_LIMIT` error (D154, D324). Ranges and composites written in binary, built-in ranges read: **code written,
  unbuilt** (code-only mode, 2026-11-12, D828); the `LOG_VERBOSITY` notices before the error: **code written,
  unbuilt** (code-only mode, 2026-11-12, D829); composites and user-defined ranges read in binary: **code written,
  unbuilt** (code-only mode, 2026-11-12, D856).
- The catalog: `tableoid` and the other system columns, `pg_cast` rows, composite types'
  `typrelid`, `information_schema.routines` and `parameters`, OIDs kept across renames (D129, D131).
- pgAdmin, DBeaver and SSMS run in CI, moving from "expected to work" to supported (D12, D163).
- SQLAlchemy's dialect suite: `relkind` `m` for materialized views in `pg_class`, `nextval` of a
  `SERIAL` column's sequence, two identity columns in one table and identity
  `MAXVALUE`/`MINVALUE`/`CYCLE`, zero-column tables, the requested level in `SHOW
  transaction_isolation`, psycopg's named cursors and a bound frame offset; then the minimum in
  `scripts/sqlalchemy-suite/minimum-passes.txt` raised from 96 to 1160 after CI measured 1174 passes (D308); next: the 272 failures.
  The requested level in `SHOW transaction_isolation`: **code written, unbuilt** (code-only mode,
  2026-11-11, D555); `relkind` `m`: **code written, unbuilt** (code-only mode, 2026-11-11, D556);
  zero-column tables: **code written, unbuilt** (code-only mode, 2026-11-11, D557); psycopg's named
  cursors: **code written, unbuilt** (code-only mode, 2026-11-11, D558).
- Temporary tables and session locks carried across `USE` instead of refusing it (D294).
- PostgreSQL's regression suite run from outside the repo, with its pass rate published and a
  stated minimum, as for every protocol and dialect (D10, D45).
- Generated REST: JWT sign-in with roles per request, embedded resources, `or=`/`and=` groups,
  `POST /rpc/<function>`, upserts and an OpenAPI document (D235). `or=`/`and=` groups: **code written,
  unbuilt** (code-only mode, 2026-11-11, D456); `/rpc/<function>`, upserts and the OpenAPI
  document: **code written, unbuilt** (code-only mode, 2026-11-11, D457); JWT sign-in with roles per
  request: **code written, unbuilt** (code-only mode, 2026-11-11, D458); embedded resources (one level):
  **code written, unbuilt** (code-only mode, 2026-11-11, D459).
- A GraphQL API generated from the schema, in Hasura's style (D37). Queries, aggregates, relationships from
  foreign keys and mutations over `POST /graphql`: **code written, unbuilt** (code-only mode, 2026-11-12, D875).
  Introspection (`__schema`, `__type`): **code written, unbuilt** (code-only mode, 2026-11-12, D876); subscriptions
  over Server-Sent Events (D878); nested inserts one relationship deep: **code written, unbuilt** (code-only mode,
  2026-11-12, D909). Left: nested inserts deeper than one relationship, and `on_conflict` with them.
- gRPC and OData APIs generated from the schema, OData for Excel, Power BI and SAP (D37). The OData v4 service
  (`$metadata`, entity sets with `$filter`/`$select`/`$orderby`/`$top`/`$skip`/`$count`, keys, writes): **code
  written, unbuilt** (code-only mode, 2026-11-12, D877); navigation properties from foreign keys and `$expand`:
  **code written, unbuilt** (code-only mode, 2026-11-12, D910). Left: gRPC, OData's batches, and options inside
  `$expand`.
- Realtime subscriptions over WebSocket and Server-Sent Events on live queries (D37). Live queries over
  Server-Sent Events for REST reads and GraphQL subscriptions: **code written, unbuilt** (code-only mode,
  2026-11-12, D878). Left: WebSocket transports and change-driven invalidation.
- End-user sign-in for apps: sign-up, password, magic link, social login and OpenID Connect, with
  JWT sessions mapped onto row-level security (D37). **Code written, unbuilt** (code-only mode, 2026-11-12, D771), in
  `eggdb_app_auth`; the OAuth redirect flow and fetching providers' key sets are left.
- WebAuthn and passkeys for app sign-in (D54). **Code written, unbuilt** (code-only mode, 2026-11-12, D772), ES256
  passkeys without attestation checks.
- Files in object storage transformed on request (resize, crop, convert) through WebAssembly
  extensions and our own image codecs (D37).
- Rate limits on every generated API (D25, D37). **Code written, unbuilt** (code-only mode, 2026-11-11,
  D460).

### M6b The rest of security
- Kerberos/GSSAPI logins (D22, D185).
- TLS client certificates, requested and verified by the TLS layer (D22, D161, D185). Check:
  psql with `sslcert` in the `tls-clients` job. **Code written, unbuilt** (code-only mode, 2026-11-12, D803): TLS 1.3 only. Left: TLS 1.2's
  CertificateRequest, a `serve` option in place of `EGGDB_TLS_CLIENT_CA`, and the `tls-clients` check.
- LDAP search-and-bind, StartTLS and LDAPS (D185). **Code written, unbuilt** (code-only mode, 2026-11-12, D802).
  Left: a directory in CI.
- PostgreSQL 18's `OAUTHBEARER` mechanism (D185). **Code written, unbuilt** (code-only mode, 2026-11-12, D801).
  Left: libpq's device flow checked against EggDB in CI.
- SAML, API keys and multi-factor sign-in for interactive tools (D22). API keys (`CREATE EXTENSION eggdb_api_keys`, the `apikey` rule): **code written,
  unbuilt** (code-only mode, 2026-11-12, D820). Left: SAML and multi-factor sign-in.
- External authentication rules on the MySQL and TDS listeners, and rules by address or database
  (D185). Rules by address or database (`database=`, `address=`) on the PostgreSQL listener: **code written,
  unbuilt** (code-only mode, 2026-11-12, D821); the rule file on the MySQL listener: **code written, unbuilt**
  (code-only mode, 2026-11-12, D825); on the TDS listener: **code written, unbuilt** (code-only mode, 2026-11-12,
  D826).
- MySQL and TDS logins for stored roles (D183). MySQL: **code written, unbuilt** (code-only mode, 2026-11-12, D823);
  TDS: **code written, unbuilt** (code-only mode, 2026-11-12, D824).
- `SET ROLE`, object owners, `WITH GRANT OPTION`, and schema, sequence and routine privileges
  (D183). `SET ROLE`, `SET ROLE NONE` and `RESET ROLE`: **code written, unbuilt** (code-only mode, 2026-11-12,
  D827).
- Schema, type, parameter (`GRANT SET ON PARAMETER`) and tablespace grants stored, so the
  inquiry functions and `pg_namespace.nspacl` follow them; `information_schema.role_routine_grants`
  and `role_udt_grants` (D326, D344).
- `FORCE ROW LEVEL SECURITY` taking effect once object owners land, and roles with `BYPASSRLS`
  (D326).
- SQL Server's column `GRANT` and `DENY`, column privileges on sequences, `MERGE` and `ON CONFLICT
  DO UPDATE` under row security, and row security through updatable views (D285). `GRANT` and `REVOKE` with columns: **code written,
  unbuilt** (code-only mode, 2026-11-12, D586); `DENY` on tables: **code written, unbuilt** (code-only mode,
  2026-11-12, D621).
- Column grants that follow a column renamed by MySQL's `CHANGE COLUMN` (D285). **Code written,
  unbuilt** (code-only mode, 2026-11-11, D484).
- Oracle VPD and SQL Server's security predicates (D186). SQL Server's security policies with filter predicates: **code written, unbuilt**
  (code-only mode, 2026-11-12, D822); block predicates `AFTER INSERT` and `AFTER UPDATE`: **code written, unbuilt**
  (code-only mode, 2026-11-12, D850); Oracle VPD static policies through `DBMS_RLS`: **code written, unbuilt**
  (code-only mode, 2026-11-12, D851); `BEFORE UPDATE` and `BEFORE DELETE` block predicates and predicates with no
  operation: **code written, unbuilt** (code-only mode, 2026-11-12, D871). Left: VPD's dynamic and context-sensitive
  policies, column-level VPD and policy groups.
- Dynamic data masking: SQL Server's `MASKED WITH` and Oracle's redaction, after its decision
  (D22, D285).
- Redis ACLs, MongoDB roles, Elasticsearch roles and Cassandra roles mapped onto the one role
  model (D22). Check: the same user has the same rights over every protocol.
- Authenticated page encryption, with a tag per page (D187).
- Encrypting an existing file and rotating keys online (D23, D187).
- KMS, HSM and Vault-compatible key sources (D23, D187).
- Per-column and per-tenant keys (D23, D187, D234).
- A mode that uses FIPS-approved algorithms only (D23). In TLS (`EGGDB_FIPS=1`, `fips_only`): **code written, unbuilt**
  (code-only mode, 2026-11-12, D812); logins (MySQL's `mysql_native_password` switched to `caching_sha2_password`) and
  `serve --fips`: **code written, unbuilt** (code-only mode, 2026-11-12, D855). Left: a validated-module claim, which
  needs a CMVP certificate.
- A path to post-quantum key exchange in TLS (D23). SHA-3 and ML-KEM-768 in `eggdb-crypto`: **code written, unbuilt**
  (code-only mode, 2026-11-12, D804); the X25519MLKEM768 hybrid group, preferred in TLS 1.3 on both sides: **code written,
  unbuilt** (code-only mode, 2026-11-12, D805). Left: NIST's ML-KEM known-answer vectors and a constant-time review.
- Server events (logins, privilege and schema changes, chosen tables' reads) in the hash-chained
  audit log, with anchored digests and retention (D26, D231). Logins over the PostgreSQL protocol and schema and privilege changes from
  every protocol, with `EGGDB_AUDIT=1`: **code written, unbuilt** (code-only mode, 2026-11-12, D815); MySQL and TDS
  logins: **code written, unbuilt** (code-only mode, 2026-11-12, D816); chosen tables' reads (`EGGDB_AUDIT_TABLES`):
  **code written, unbuilt** (code-only mode, 2026-11-12, D817); anchored digests (`EGGDB_AUDIT_ANCHOR`): **code
  written, unbuilt** (code-only mode, 2026-11-12, D818); rotation and retention (`EGGDB_AUDIT_ROTATE_MB`,
  `EGGDB_AUDIT_KEEP_DAYS`): **code written, unbuilt** (code-only mode, 2026-11-12, D819); digests signed with the
  server's key (`EGGDB_AUDIT_SIGNING_KEY`, `EGGDB_AUDIT_PUBLIC_KEY`): **code written, unbuilt** (code-only mode,
  2026-11-12, D867).
- Crypto-shredding: per-subject keys whose deletion makes data unreadable in the file, history
  and every backup, with holds that override it (D26, D53, D231, D246). **Code written, unbuilt** (code-only mode,
  2026-11-12, D773), in `eggdb_shredding`, for values sealed with it.
- Classification and tagging of personal data (D26). **Code written, unbuilt** (code-only mode, 2026-11-12, D748).
- Evidence reports for SOC 2, HIPAA and PCI DSS audits (D26). **Code written, unbuilt** (code-only mode, 2026-11-12,
  D761), for what SQL can read; the audit log's server events and encryption settings are left.
- Ledger tables: `TRUNCATE` and `DROP` refused (**code written, unbuilt**, code-only mode,
  2026-11-10, D392), digests published outside the database and checked
  against backups, Merkle proofs, SQL Server's `WITH (LEDGER = ON)` (D38, D236).
- A ledger on a table that already has rows (D236). **Code written, unbuilt** (code-only mode, 2026-11-12, D754).
- Retention: holds over some rows, engine-level WORM, records schedules and eDiscovery export in
  the EDRM style (D53, D246). Holds over some rows, records schedules and the EDRM export: **code written, unbuilt**
  (code-only mode, 2026-11-12, D762). Left: engine-level WORM.
- Sovereignty: customer-held keys kept in-country, limits on operator access and logs of
  cross-border transfers (D53, D246).
- Delta Sharing server and client after D58's legal check, and zero-copy shares between
  organizations on branches with entitlements (D52, D245).
- Marketplace listings of shared data inside EggDB, with no payments and no external marketplace
  account (D52). **Code written, unbuilt** (code-only mode, 2026-11-12, D763).
- Clean rooms: query-output policies and private set intersection (D52, D245). **Code written, unbuilt** (code-only
  mode, 2026-11-12, D759).
- Differential privacy: privacy budgets, DP aggregates, the snapping mechanism and l-diversity
  (D52, D245). Budgets and DP count, sum and average: **code written, unbuilt** (code-only mode, 2026-11-12, D748).
  The snapping mechanism and l-diversity: **code written, unbuilt** (code-only mode, 2026-11-12, D758).
- Blind-index search on encrypted columns (D52). **Code written, unbuilt** (code-only mode, 2026-11-12, D752).
- Synthetic data, and masked copies for non-production use (D52). Masked copies (`eggdb_privacy.masked_copy`): **code
  written, unbuilt** (code-only mode, 2026-11-12, D748). Synthetic data (`eggdb_privacy.synthetic_copy`): **code written,
  unbuilt** (code-only mode, 2026-11-12, D757).
- A secrets store with dynamic, short-lived credentials in Vault's style (D54). **Code written, unbuilt** (code-only mode,
  2026-11-12, D751), in `eggdb_secrets`.
- An LDAP v3 server, SCIM, and the published Active Directory protocols (MS-ADTS) after D58's
  legal check (D54).
- Isolation of tenants by schema or by database, beside isolation by row (D32, D234). By schema: **code written,
  unbuilt** (code-only mode, 2026-11-12, D764). Left: by database.
- SECURITY.md's scope: denial of service from hostile input, sandbox escapes and protocol attacks
  (D45).

### M6c Operations
- OTLP export of EggDB's own metrics, logs and traces (D25, D188). Metrics over OTLP/HTTP JSON: **code written,
  unbuilt** (code-only mode, 2026-11-12, D567); logs and slow statements as traces: **code written, unbuilt**
  (code-only mode, 2026-11-12, D666); protobuf: **code written, unbuilt** (code-only mode, 2026-11-12, D688); TLS:
  **code written, unbuilt** (code-only mode, 2026-11-12, D868).
- Live activity, locks and wait events: `pg_stat_activity` for every session,
  `sys.dm_exec_requests` and `SHOW PROCESSLIST` across sessions (D25, D188). `pg_stat_activity` and MySQL's process
  list across sessions: **code written, unbuilt** (code-only mode, 2026-11-11, D488); `sys.dm_exec_sessions` and
  `sys.dm_exec_requests` across sessions: **code written, unbuilt** (code-only mode, 2026-11-11, D489). `pg_cancel_backend` and
  `pg_terminate_backend`: **code written, unbuilt** (code-only mode, 2026-11-11, D490).
- Statement statistics in the other dialects' views: `performance_schema`'s digest summary and
  `sys.dm_exec_query_stats` (D25, D286). `sys.dm_exec_query_stats`: **code
  written, unbuilt** (code-only mode, 2026-11-11, D489); `performance_schema.events_statements_summary_by_digest`:
  **code written, unbuilt** (code-only mode, 2026-11-12, D813).
- SQLite's `.scanstats on|off` in `eggdb sql`'s shell: after each `SELECT` the shell prints its
  `QUERY PLAN` with `--SCAN t (loops=1 rows=3)` lines (D327). **Code written, unbuilt** (code-only mode, 2026-11-12, D814), as a second
  result in `QUERY PLAN`'s table form.
- MySQL's `EXPLAIN FORMAT=JSON`: `key_length` (**code written, unbuilt**, code-only mode, 2026-11-12, D663),
  `used_columns`, `data_read_per_join`, subqueries
  (`attached_subqueries`), conditions in MySQL's `` `db`.`t`.`c` `` spelling, format version 2;
  `EXPLAIN FORMAT=TREE|JSON` of `INSERT` and `REPLACE`; measured steps under a multi-table
  write's `Update`/`Delete` iterator (D345).
- Subqueries in `EXPLAIN` of `UPDATE` and `DELETE`, and of derived tables, `WITH` and set
  operations (D327).
- Subqueries in Oracle's, SQL Server's, SQLite's and MySQL's tabular plans (D327).
- An index used for a comparison with an `InitPlan`'s `$k` (D327).
- Times on each iterator inside a subplan in `EXPLAIN ANALYZE` (D327).
- Times on the steps inside a join (a counter per level in `JoinedRows::extend`, inner `loops` =
  outer rows), on an `UPDATE` or `DELETE`'s scan and on aggregates; per-operator rows in `SET
  STATISTICS PROFILE`; `DISPLAY_CURSOR`, predicates in `PLAN_TABLE`, and `SHOWPLAN_*` alone in
  its batch (D312).
- `EXPLAIN` with costs, conditions and formats other than text; per-node buffers, `I/O Timings`
  and planning time (D286, sql.md). PostgreSQL's `FORMAT JSON` and `YAML`: **code written, unbuilt**
  (code-only mode, 2026-11-11, D531); `XML`: **code written, unbuilt** (code-only mode, 2026-11-12, D628); measured
  plans in JSON, YAML and XML: **code written, unbuilt** (code-only mode, 2026-11-12, D870). Left: costs.
- `lock_timeout`, and memory, CPU and I/O limits per user, role, tenant and query (D25, PR #86).
  `lock_timeout` on the writer wait: **code written, unbuilt** (code-only mode, 2026-11-10, D393).
- Quotas by bytes and requests per tenant, and admission control (D25, D234).
- An index and plan advisor, and automatic tuning of indexes and configuration (D25, D31). The index advisor
  (`eggdb_advisor`): **code written, unbuilt** (code-only mode, 2026-11-12, D780). Left: plan advice and configuration
  tuning.
- An admin API, with the same actions as MCP tools (D11, D25). `GET /admin/tools` and `POST /admin/tools/<name>` beside `/mcp`: **code written,
  unbuilt** (code-only mode, 2026-11-12, D840).
- A Kubernetes operator in Rust (D25, D45).
- A Terraform provider in Rust on Terraform's published plugin protocol (D25, D46).
- A metering API (usage per tenant, database and query) with hooks for billing, and chargeback
  and showback reports on it (D44, D58). **Code written, unbuilt** (code-only mode, 2026-11-12, D760), for SQL
  statements; storage and the other protocols are left.
- Column-level lineage captured from queries (D51, D244).
- The Apache Atlas and OpenMetadata APIs (D51).
- A schema registry and data contracts (D51). **Code written, unbuilt** (code-only mode, 2026-11-12, D753).
- Master data: entity resolution, match and merge, and golden records (D51). **Code written, unbuilt** (code-only mode,
  2026-11-12, D749).
- Data profiling and data-quality rules at scale (D42, D51). Profiling (`eggdb_catalog.profile`): **code written,
  unbuilt** (code-only mode, 2026-11-12, D749). Left: rules at scale.

### M6d The built-in MCP server
- OAuth for MCP over HTTP, as the MCP specification describes for remote servers; TLS comes with
  the M6a item for every listener (D189). OAuth access tokens (JWTs) and the protected-resource metadata: **code written, unbuilt**
  (code-only mode, 2026-11-12, D841). Left: TLS on the MCP listener.
- Writes with confirmation over HTTP (D189, D190). **Code written, unbuilt** (code-only mode, 2026-11-11, D522); the user
  is asked over HTTP once event streams carry server requests.
- Server-sent event streams (D189). Answers as SSE when the client asks: **code written, unbuilt**
  (code-only mode, 2026-11-11, D487); progress streams wait for tools that report progress.
- Per-agent time limits and quotas, and the agent's role in each audit record (D190). **Code written,
  unbuilt** (code-only mode, 2026-11-11, D491) for `eggdb serve`'s MCP endpoint.
- Table resources beyond the schema document (D190). **Code written, unbuilt** (code-only mode, 2026-11-12, D569).
- The 2026-07-28 protocol revision (D190).
- `eggdb mcp --tables`: deleting rows of a table listed with columns, inserts whose hidden column
  takes a generated value, unique indexes and foreign keys over a hidden column checked without
  an oracle, and checking a listed child's keys against a hidden column of a listed parent
  (refused with 42501 for now); writes to tables listed with columns, foreign keys to unlisted
  tables and column lists on views (whose query text no catalog shows) are built (D306).
- MCP key writes for agents acting as a role, through key-level grants (D328).
- A per-session key allow-list, so the MCP key-value tools work under `--tables` and
  `--mcp-tables` (D328).
- Backup, restore and administration tools (D11).
- Tools that query documents, search indexes, vectors, graphs and keys in their own query
  languages (D10, D11).
- Text-to-SQL in every dialect through the MCP server (D31, D205). `eggdb_ai.text_to_sql`: **code written, unbuilt**
  (code-only mode, 2026-11-12, D779). Left: an MCP tool of its own.

### M6e The GUI client and the console
- Issue #29 (the client track) is the GUI bullets below; it closes when they are done (#29).
- A hand check with VoiceOver, Narrator, Orca and a browser screen reader on every screen (D278).
- UI Automation's Text and Selection patterns, NSAccessibility rows and live regions, AT-SPI2's
  text attributes, and selection announcements (D278).
- IME input, and Wayland windows (D256).
- The main window in the browser, with the wheel and files dropped on the browser card opened in
  memory; the browser card's pointer and async clipboard are built (D278, D279, D313).
- Touch gestures beyond a tap and a drag in the browser; `INCR` for the `PRIMARY` selection;
  XDND's `XdndProxy` and actions other than copy; dragging out of the client (`DoDragDrop`, an
  XDND source); Wayland's data device; X11's `INCR`, XDND drops and Windows' OLE drop target are
  built (D274, D279, D313).
- The full-value view of a RESP aggregate (`HGETALL`, `LRANGE`, `SMEMBERS`, `ZRANGE …
  WITHSCORES`, `XRANGE` shown as JSON) (D329).
- The dimmed looks of the RESP keys tab's count, `–` and `(gone)` cells (D329). **Code written,
  unbuilt** (code-only mode, 2026-11-11, D549).
- RESP over TLS in the client (`rediss://`, shown as `RESP · TLS`) (D329).
- A RESP connection opens without an empty `Query 1` tab (D329). **Code written, unbuilt** (code-only
  mode, 2026-11-11, D547).
- Caret movement in the keys tab's prefix field, and `Tab` stops on `Back` and `Next 100` (D329). Caret
  movement: **code written, unbuilt** (code-only mode, 2026-11-11, D548); the `Tab` stops: **code
  written, unbuilt** (code-only mode, 2026-11-11, D550).
- The editor keeps its focused look while its menu is open (D329). **Code written, unbuilt** (code-only
  mode, 2026-11-11, D548).
- `Paste` in the browser reads the clipboard (D329).
- `menu`, `menuitem` and `searchbox` roles in the platform layer's accessibility (D329). **Code written,
  unbuilt** (code-only mode, 2026-11-11, D551).
- `Stop` interrupting a RESP reconnect's TCP connect itself, which now takes effect when that
  connect ends (D329).
- Rows streamed into the grid with pages fetched on scroll (D275).
- The value view's text selection and SQL type names; menu, dialog and scroll bar accessibility
  roles; scrollbars that hide and fade; the editor's draggable and horizontal scrollbars; the
  column-resize cursor (D310).
- The splitters' leftovers: the results grid filling a taller results region, the results and
  inspector splitters as `Tab` stops, the resize cursor, splitters in the familiar layouts, and the
  rail's hover, tooltips and accessibility roles with the splitters' and the drawer's (D311).
- Motion: hover fills that fade on tree rows, grid rows, rail items, status bar pills, dialog
  buttons and menus; redrawing at the display's rate; the spinner and the sweeping bar; the OS's
  reduced-motion change notifications, the browser's `prefers-reduced-motion`, Wayland's settings
  portal and `View > Reduce motion` (D311).
- Showing in the plan view the join-step times and Oracle's measured plans (`DISPLAY_CURSOR`)
  once M6c's `EXPLAIN` builds them, and the plan tooltip in the accessibility tree (D277, D312).
- Connecting over the MySQL and TDS protocols, recent connections in the sidebar, the password
  field's `Show` button, and `verify-ca`/`verify-full` certificate checks in the GUI (D274);
  RESP connections are built (D329).
- Every data model in the object browser: documents, search, series, graphs, vectors and queues,
  each opened in its own form (D15, D276).
- Completion and error help with `Fix` buttons in the query editor; `Format statement` is built
  (D15, D275, D329).
- Editing data in the grid (D15).
- Import, export, backup and restore screens, with import wizards (D13, D15, D42).
- Users and permissions screens (D15).
- Monitoring screens: server status, sessions, locks and slow queries (D15, D25, D237).
- AI help in the client through the MCP server (D11, D15).
- The familiar layouts' own region contents (pgAdmin's Query Tool toolbar, DBeaver's run column,
  Compass's filter bar and document cards, Redis Insight's result cards, Workbench's output rows,
  DataGrip's Services header, SSMS's `F4` properties), their hidden regions and toggles, splitter
  positions per layout, `View > Layout`, the connect screen's `Coming from another client?` row,
  the picker in the browser build, its accessibility roles and golden frames; the picker, the
  kept choice, the regions and each layout's keys are built (D309). Check: each layout's spec
  tests in `docs/ux/`.
- GPU-accelerated drawing on the desktop (D15).
- Hinting, kerning, right-to-left and complex scripts, and CFF outlines (D191).
- The GUI and its docs localized, first English and Arabic (mirrored), after right-to-left text
  above and with a pinned Arabic font (D42). Check: golden frames of the connect card in Arabic.
- The client's speed measured like the engine's: opening a database, running a query and
  scrolling a large result (D15).
- Small screens, remembered window size and position, and the OS's theme (D15, D256). Remembered size: **code written,
  unbuilt** (code-only mode, 2026-11-12, D570). Left: position, small screens and the OS's theme.
- The `eggdb gui` command in the command model (D191). **Code written, unbuilt** (code-only mode, 2026-11-12, D571).
- The interactive ER view, with diagrams across schemas, views, indexes and checks (D237).
- An admin web console served by the server (D42).
- SQL notebooks with charts (D42).
- A visual query builder (D42).
- Console screens for the catalog, lineage and master data (D42, D244).
- Onboarding: a migration guide per source database, sample datasets and tutorials (D42).
- A documentation site, generated as static pages by our own Rust tool (D32).
- A research check of whether EggOS's GUI and `eggdb-gui` can share one toolkit, recorded as a
  new decision that refines D15 (D15).

### M6f Network drivers, ORMs and connectors
- The MySQL and TDS protocols as backends of the C API's network session (D192).
- Connection pooling, prepared statement reuse, binary result formats, cancel and `COPY` in the
  network drivers (D192).
- Each driver's tests run against `eggdb serve` over the network, Python, .NET, Java, Node.js and
  Go first (D182, D192). Check: the drivers' CI jobs with a `postgresql://` URL.
- Each driver benchmarked over the network against the best driver in its language, as D17 lists
  them, and gated (D17).
- A research decision on the PostgreSQL protocol or an EggDB-native one for the official
  drivers, and a pure-language server path where benchmarks need one (D17).
- Every model and dialect and the MCP tools in the drivers, first the dialect per connection and
  an ordered key-value API in FoundationDB's style (D16, D48). Check: driver tests, both modes.
- The EF Core provider, with Dapper and NHibernate checked (D16, D182). Check: EF Core's
  provider specification tests.
- The Hibernate dialect, R2DBC, and jOOQ, Spring Data and Exposed checked (D16, D182).
- The SQLAlchemy dialect, the Django backend and asyncio support for Python (D16, D182).
- Prisma, TypeORM, Drizzle, Sequelize and Knex adapters for Node.js (D16, D182).
- The GORM dialect, with sqlc checked (D16, D182).
- A PDO driver with Laravel/Eloquent and Doctrine (D16, D182).
- ActiveRecord and Sequel for Ruby (D16, D182).
- The Swift and Dart/Flutter drivers with their mobile frameworks (D16).
- An ODBC driver (D12, D46).
- The `compatibility` job with DataGrip, MySQL Workbench, Redis Insight, MongoDB Compass, Kibana,
  Grafana, Metabase, Superset, Power BI, Airbyte, Liquibase and Alembic (D12, D239).
- Our own connectors in each tool's plugin system: a Power BI connector, and Superset, Metabase
  and Grafana data source plugins (D46, D239).
- A dbt adapter, an Airbyte source and destination, a Debezium connector and Airflow operators
  (D46, D239).
- Spark, Flink, Trino and Presto connectors (D46, D239).

### M7 Caching and Redis
- Per-element storage for large lists, sets, hashes and sorted sets (D193).
- Lua scripting (`EVAL`) and Redis Functions on our own Lua interpreter (D27, D193). The interpreter's lexer
  and parser (`eggdb-lua`): **code written, unbuilt** (code-only mode, 2026-11-12, D667). The interpreter with
  its step budget and the base, `table` and `math` libraries: **code written, unbuilt** (code-only mode, 2026-11-12,
  D668). The `string` library with Lua patterns: **code written, unbuilt** (code-only mode, 2026-11-12, D669).
  `cjson`, `bit` and `cmsgpack`: **code written, unbuilt** (code-only mode, 2026-11-12, D670). `EVAL`, `EVALSHA`,
  `EVAL_RO`, `EVALSHA_RO` and `SCRIPT` in the RESP server: **code written, unbuilt** (code-only mode, 2026-11-12,
  D671). Redis Functions (`FUNCTION`, `FCALL`, `FCALL_RO`): **code written, unbuilt** (code-only mode,
  2026-11-12, D672); keeping libraries across restarts, `FUNCTION DUMP` and `RESTORE`: **code written, unbuilt**
  (code-only mode, 2026-11-12, D799).
- Stream consumer groups (`XGROUP`, `XREADGROUP` with `BLOCK`, `XACK`, `XPENDING`, `XCLAIM`,
  `XAUTOCLAIM`, `XINFO`) as their own security-critical format part: group state kept with the
  stream in a new stored value kind 6, with pinned test vectors, truncation and byte-flip tests
  and `docs/format.md`'s RESP value section (D35, D193, D347). **Code written,
  unbuilt** (code-only mode, 2026-11-11, D512).
- Geo searches over the geohash ranges of the nine neighbouring cells instead of every member,
  and RESP3 double replies for `GEOPOS`/`WITHCOORD` coordinates (D330). RESP3 doubles: **code
  written, unbuilt** (code-only mode, 2026-11-10, D394). Nine cells: **code written, unbuilt** (code-only
  mode, 2026-11-12, D590).
- Keyspace notifications, the rest of `CLIENT` (`LIST`, `KILL`, ...), the rest of `INFO`
  (it has only `blocked_clients`, D347), `OBJECT` and `DEBUG` (D12, D193). `OBJECT`: **code
  written, unbuilt** (code-only mode, 2026-11-10, D395); `INFO`'s sections: **code written,
  unbuilt** (D396); `DEBUG OBJECT | SLEEP | SET-ACTIVE-EXPIRE | HELP`: **code written, unbuilt**
  (D397). Keyspace notifications and `CLIENT LIST | INFO | KILL | ID`: **code written, unbuilt** (code-only
  mode, 2026-11-12, D591).
- Cluster mode: slots, the cluster commands and sharded pub/sub (`SSUBSCRIBE`) (D193). One node holding every slot, the
  cluster commands and sharded pub/sub: **code written, unbuilt** (code-only mode, 2026-11-11, D565).
  Left: several nodes, `MOVED`/`ASK` and resharding (with M15).
- The rest of Redis's commands, tracked per command (D5, D193). Check: Redis client libraries'
  own test suites. `GETEX`, `SETRANGE`, `MSETNX`, `LCS`,
  `TOUCH`, `EXPIRETIME`, `RANDOMKEY`, `HSTRLEN`, `HINCRBYFLOAT`, `HRANDFIELD`, `HSCAN`, `LPOS`, `SMISMEMBER`,
  `SRANDMEMBER`, `SMOVE`, `SINTERCARD`, the set `...STORE` commands, `SSCAN`, `ZMSCORE`, `ZRANDMEMBER`,
  `ZSCAN`, `ZUNION`/`ZINTER`/`ZDIFF` and their stores, `ZINTERCARD`: **code written, unbuilt** (code-only
  mode, 2026-11-11, D485). The reverse and lexicographic sorted-set ranges and removals, `ZREVRANK`, `SPOP`,
  `LINSERT`, `LPUSHX`/`RPUSHX`, `TIME`, `LASTSAVE` and `CONFIG GET`: **code written, unbuilt** (code-only mode,
  2026-11-11, D486).
- Memory-only collections in the engine, with expiry and eviction (D2, D193).
- Benchmarks with redis-benchmark and memtier against Redis, Valkey and Dragonfly, gated (D7,
  D177).
- memcached's binary protocol and SASL (D194). **Code written, unbuilt** (code-only mode,
  2026-11-10, D398).
- The rest of memcached's meta flags, `stats` details and LRU eviction for its items (D194). Meta
  flags and `stats` counts: **code written, unbuilt** (code-only mode, 2026-11-11, D474); LRU eviction:
  **code written, unbuilt** (code-only mode, 2026-11-12, D592).

### M8 Full-text search
- An inverted index in the file, with BM25 in SQL served from it (D195). Check: a benchmark
  against SQLite FTS5 and Elasticsearch.
- Weights (`setweight`) and `ts_headline` (D195). **Code written, unbuilt** (code-only mode,
  2026-11-11, D469).
- Stemmers for every major language, and word segmentation for Chinese, Japanese, Korean and Thai
  (D30). German (`german` configuration): **code written, unbuilt** (code-only mode, 2026-11-12, D649);
  Spanish (`spanish`): **code written, unbuilt** (code-only mode, 2026-11-12, D650); Swedish, Norwegian
  and Danish: **code written, unbuilt** (code-only mode, 2026-11-12, D651); Italian: **code written, unbuilt**
  (code-only mode, 2026-11-12, D652); Russian: **code written, unbuilt** (code-only mode, 2026-11-12, D653);
  Dutch: **code written, unbuilt** (code-only mode, 2026-11-12, D654); Portuguese: **code written, unbuilt**
  (code-only mode, 2026-11-12, D655); French: **code written, unbuilt** (code-only mode, 2026-11-12, D656);
  Finnish: **code written, unbuilt** (code-only mode, 2026-11-12, D673); Hungarian: **code written, unbuilt**
  (code-only mode, 2026-11-12, D674); Indonesian: **code written, unbuilt** (code-only mode, 2026-11-12, D687); Romanian: **code
  written, unbuilt** (code-only mode, 2026-11-12, D718). Left: Turkish and the others, and word segmentation.
- Synonyms, stop words, and fuzzy and phonetic matching (D30). Fuzzy and phonetic functions:
  **code written, unbuilt** (code-only mode, 2026-11-11, D470); Elasticsearch analyzers with stop words and synonyms:
  **code written, unbuilt** (code-only mode, 2026-11-12, D739). Left: synonyms and stop words in SQL text search
  configurations.
- MySQL's `MATCH ... AGAINST` with `FULLTEXT` keys, SQL Server's `CONTAINS` and `FREETEXT`, and
  SQLite's FTS5 (D10, D195). `MATCH ... AGAINST` without keys: **code written, unbuilt** (code-only
  mode, 2026-11-11, D513). `CONTAINS` and `FREETEXT` without full-text indexes: **code written,
  unbuilt** (D514). FTS5 without an index (`CREATE VIRTUAL TABLE ... USING fts5`, `MATCH`, `bm25()`, `highlight()`,
  `snippet()`): **code written, unbuilt** (code-only mode, 2026-11-12, D800). Left: `FULLTEXT` keys, full-text
  indexes (FTS5's included, with corpus-wide `bm25()` and the hidden `rank` column), and `CONTAINSTABLE`.
- Elasticsearch mappings applied to field types: `text` and `keyword`, dates, `ip`, geo, `nested`,
  `flattened`, `dense_vector`, `sparse_vector` and `completion` (D19, D196). `text`, `keyword`
  (with multi-fields and dynamic `.keyword`), dates and `ip` in `_search`: **code written, unbuilt**
  (code-only mode, 2026-11-11, D559); geo point queries: **code written, unbuilt** (code-only
  mode, 2026-11-11, D560); `dense_vector` with `knn` search: **code written, unbuilt** (code-only
  mode, 2026-11-11, D561); the `nested` query: **code written, unbuilt** (code-only mode,
  2026-11-11, D562); `completion` suggestions: **code written, unbuilt** (code-only mode,
  2026-11-11, D563); `flattened` and `sparse_vector`: **code written, unbuilt** (code-only mode,
  2026-11-11, D564).
- `wait_for_completion=false` tasks for the by-query APIs (D331). **Code written, unbuilt** (code-only
  mode, 2026-11-11, D473): the task finishes in the request and is kept for `_tasks`.
- `_rethrottle` and parallel `slices` for the by-query APIs (D331). **Code written, unbuilt** (code-only
  mode, 2026-11-11, D473): `slices` runs as one slice.
- `_update_by_query` and `_delete_by_query` over several indices or patterns (D331). **Code
  written, unbuilt** (code-only mode, 2026-11-10, D401), with `_search` and `_count` too.
- `_search/template` and `_msearch/template` with Mustache `{{var}}` and `{{#toJson}}` (D331,
  D348). `_search/template` and `_render/template`: **code written, unbuilt** (code-only mode,
  2026-11-10, D402), `_msearch/template` (D403); stored templates: **code written, unbuilt** (code-only mode,
  2026-11-12, D593).
- `if_seq_no`/`if_primary_term` and `retry_on_conflict` on `_update` (D348). **Code written,
  unbuilt** (code-only mode, 2026-11-10, D400), on indexing and deleting too.
- `_source` filtering on `GET _doc` (D331, D348). **Code written, unbuilt** (code-only mode,
  2026-11-10, D399), with `GET <index>/_source/<id>`.
- Painless beyond D331's update subset (conditions, loops, comparisons, more methods, stored
  scripts), `query_string` syntax and highlighting (D196, D331).
  `query_string`, `simple_query_string` and highlighting: **code written, unbuilt** (code-only
  mode, 2026-11-11, D515); stored scripts: **code written, unbuilt** (code-only mode, 2026-11-12, D593).
- Scroll (`scroll=1m`, `_search/scroll`, `DELETE _search/scroll`) with contexts bounded in count
  and memory and expired by their keep-alive, and point in time (`pit`) for `search_after`
  (D196, D348). Scroll: **code written, unbuilt** (code-only mode, 2026-11-11, D467); point in
  time: **code written, unbuilt** (code-only mode, 2026-11-11, D468).
- `_script`, `_geo_distance` and `nested` sorts, and refusing sorts on unmapped fields once
  mappings apply (D348).
  `_geo_distance`: **code written, unbuilt** (code-only mode, 2026-11-11, D518); `_script`, `nested` and
  unmapped fields: **code written, unbuilt** (code-only mode, 2026-11-12, D595).
- Nested and bucket aggregations beyond `terms` (D196). **Code written, unbuilt** (code-only mode,
  2026-11-11, D471); the `nested` aggregation: **code written, unbuilt** (code-only mode, 2026-11-11, D562).
- Aliases, index templates and the rest of `_cat` (D196). Aliases, `_cat/aliases`, `count` and `health`:
  **code written, unbuilt** (code-only mode, 2026-11-11, D472); index and component templates: **code
  written, unbuilt** (code-only mode, 2026-11-12, D594); legacy `_template`: **code written, unbuilt** (D626).
- Kibana and OpenSearch Dashboards working against EggDB (D12, D196). Check: `compatibility` job.
- A benchmark against OpenSearch (D7, D196).
- Enterprise search that respects each user's permissions (D56).
- Text extracted from PDF files, and from PST files after D58's legal check, and OCR through ONNX
  models (D56, D249). PDF text: **code written, unbuilt** (code-only mode, 2026-11-12, D714). Left: PST
  and OCR.

### M9 Documents and MongoDB
- Indexes used by queries, unique indexes and collations (D197). Unique indexes (by scanning): **code written,
  unbuilt** (code-only mode, 2026-11-11, D529); collations in `find`, `count` and `distinct`: **code written,
  unbuilt** (code-only mode, 2026-11-12, D738). Left: indexes in the file used by queries, and collations in writes
  and `aggregate`.
- `$$NOW` and `$$CLUSTER_TIME` in `aggregate`, and `$$CLUSTER_TIME` in pipeline-style updates
  (D349). **Code written, unbuilt** (code-only mode, 2026-11-10, D404).
- `$text`, `$expr`, `$jsonSchema`, `$where` and the projection operators (D197). `$expr`: **code
  written, unbuilt** (code-only mode, 2026-11-10, D406); the projection operators and
  expressions: **code written, unbuilt** (D408); `$jsonSchema` in filters: **code
  written, unbuilt** (D409); `$text` with a text index: **code written, unbuilt** (D530); text scores:
  **code written, unbuilt** (code-only mode, 2026-11-12, D597); `$where` with JavaScript expressions: **code written,
  unbuilt** (code-only mode, 2026-11-12, D719).
- `$facet`, `$bucket`, `$out`, `$merge` and `$lookup` with a pipeline (D197). `$facet`, `$bucket`
  and `$switch`: **code written, unbuilt** (code-only mode, 2026-11-10, D405); `$lookup` with `let` and
  `pipeline`: **code written, unbuilt** (D406); `$out` and `$merge`: **code written,
  unbuilt** (D407).
- Change streams and multi-document transactions, with the replica-set handshake (D197). Multi-document
  transactions with the replica-set handshake: **code written, unbuilt** (code-only mode, 2026-11-11,
  D566); change streams: **code written, unbuilt** (code-only mode, 2026-11-12, D588).
- `explain`, `renameCollection`, capped and time-series collections, and GridFS (D197). `renameCollection`:
  **code written, unbuilt** (code-only mode, 2026-11-10, D410); `explain`: **code written,
  unbuilt** (D411); capped and time-series collections: **code written, unbuilt** (D532). Tailable
  cursors: **code written, unbuilt** (code-only mode, 2026-11-12, D596). Left: GridFS checks; TTL indexes:
  **code written, unbuilt** (D533).
- SCRAM-SHA-1, other users and roles (D197). SCRAM-SHA-1: **code written, unbuilt** (code-only mode,
  2026-11-11, D475); users and built-in roles: **code written, unbuilt** (code-only mode, 2026-11-12, D737);
  user-defined roles: **code written, unbuilt** (code-only mode, 2026-11-12, D911).
- `OP_COMPRESSED` (D197). **Code written, unbuilt** (code-only mode, 2026-11-10, D412).
- `mongorestore` inputs: `--gzip`, `--archive`, unique indexes, and a smoke-test sample dump
  (D197) `--gzip` and `--archive`: **code written,
  unbuilt** (code-only mode, 2026-11-10, D413).
- Following a live MongoDB through its change streams for a switch with no downtime (D13).
- A benchmark against MongoDB (D7, D197). Check: YCSB.
- The Firestore API, after D58's legal check (D48).
- Cosmos DB's NoSQL API in gateway mode, after D58's legal check (D48).
- Couchbase SQL++, from its documentation only (D48).
- XML documents with XPath and XQuery (D38). Check: the W3C XQuery suite. An XML parser and XPath 1.0 with
  `xpath()`, `EXTRACTVALUE` and `EXISTSNODE`: **code written, unbuilt** (code-only mode, 2026-11-12, D690);
  `XMLTABLE`: **code written, unbuilt** (code-only mode, 2026-11-12, D692). Left: XQuery and the stored `xml` type.

### M10 The other SQL dialects
- MySQL's types: `BIGINT UNSIGNED` and unsigned ranges, `ENUM`, `SET`, `YEAR`, `BIT`, the spatial
  types, zero dates and the full `TIME` range (D19, sql.md). Columns of `BIGINT UNSIGNED`, `YEAR`, `BIT`,
  `ENUM` and `SET` accepted on the nearest stored type: **code written, unbuilt** (code-only mode, 2026-11-12,
  D646); `ENUM` members, unsigned ranges and `YEAR`'s range checked: **code written, unbuilt** (code-only mode,
  2026-11-12, D684); `SET` member checks: **code written, unbuilt** (code-only mode, 2026-11-12, D700); bit literals and `0x` literals: **code
  written, unbuilt** (code-only mode, 2026-11-12, D716). Left: `ENUM` ordering, `SET` member order, two-digit years, the
  spatial types, zero dates and the full `TIME` range.
- MySQL's semantics: `DECIMAL` division, booleans as integers, implicit string-to-number
  conversion, `sql_mode`, and `1 / 0` as `NULL` with a warning (D10, sql.md). `1 / 0` as `NULL` with
  warning 1365, and numbers as conditions: **code written, unbuilt** (code-only mode, 2026-11-11,
  D536, D513); string-to-number conversion in arithmetic: **code written, unbuilt** (D537).
- The rest of MySQL's date and time functions, `STR_TO_DATE` and `ON UPDATE CURRENT_TIMESTAMP`
  (sql.md). `DATE_ADD`/`DATE_SUB` with `INTERVAL`, `TIMESTAMPADD`/`TIMESTAMPDIFF`, `STR_TO_DATE`,
  `UNIX_TIMESTAMP`, `FROM_UNIXTIME` and the day-number, period and clock functions: **code written,
  unbuilt** (code-only mode, 2026-11-11, D415). `ON UPDATE CURRENT_TIMESTAMP`: **code written,
  unbuilt** (D416; table definition format 16); `date + INTERVAL n unit`: **code written, unbuilt**
  (D417); the `ON UPDATE` clause in `SHOW CREATE TABLE`: **code written, unbuilt** (code-only mode,
  2026-11-12, D598).
- MySQL's `WEEK()` with its modes and `DATE_FORMAT()`; `YEAR()`, `MONTH()`, `DAYOFWEEK()` and
  the other date-part functions and `SQRT()` are built (D350) (sql.md). `WEEK()`, `YEARWEEK()`,
  `WEEKOFYEAR()` and `DATE_FORMAT()`: **code written, unbuilt** (code-only mode, 2026-11-10, D414).
- Prefix indexes: a key on `name(10)` indexes the whole value until the index definition can
  record the prefix length (a format change); `ADD INDEX`, `RENAME INDEX` and `DROP INDEX` in
  `ALTER TABLE` are built (D350) (sql.md).
- MySQL stored programs: handlers, cursors, named conditions, `RESIGNAL`, `GET DIAGNOSTICS`,
  variables set in bodies, and MySQL's error numbers (D199). Handlers, cursors, named conditions, `RESIGNAL` and
  `GET DIAGNOSTICS`: **code written, unbuilt** (code-only mode, 2026-11-11, D418). Variables
  set in bodies and `SIGNAL ... SET MYSQL_ERRNO`: **code written, unbuilt** (D419); variables set by a stored
  function called outside `CALL`: **code written, unbuilt** (code-only mode, 2026-11-12, D657).
- `SHOW CREATE PROCEDURE` and `information_schema.ROUTINES` details, and compound bodies in
  triggers (D199). `SHOW CREATE PROCEDURE|FUNCTION`, `SHOW ... STATUS` and the `ROUTINES`
  and `PARAMETERS` views: **code written, unbuilt** (code-only mode, 2026-11-11, D420); handlers and cursors in
  trigger bodies: **code written, unbuilt** (code-only mode, 2026-11-12, D636). MySQL's `TRIGGERS` view: **code written, unbuilt** (code-only mode,
  2026-11-12, D618); `CASE`, loops and labels in trigger bodies: **code written, unbuilt** (D619).
- MySQL events (`CREATE EVENT`) on the job scheduler (D27, D200). **Code written, unbuilt** (code-only mode,
  2026-11-11, D421). Sub-minute runs: **code written, unbuilt** (code-only mode, 2026-11-12, D576).
  `event_scheduler`: **code written, unbuilt** (code-only
  mode, 2026-11-12, D574); `information_schema.EVENTS`: **code written, unbuilt** (code-only mode,
  2026-11-12, D575).
- `GET_LOCK` named locks, and `information_schema` and `SHOW` completeness (D198, D294). Named
  locks: **code written, unbuilt** (code-only mode, 2026-11-11, D422).
- `LOAD DATA`, and mysqldump's leftovers: deferred foreign keys, `/*M!` comments, `--tab`, `--xml`,
  mysqlpump and MySQL Shell dumps (D198). Check: a PyMySQL check of a restored file. `LOAD DATA
  LOCAL INFILE`: **code written, unbuilt** (code-only mode, 2026-11-11, D423); server-side files
  stay refused.
- MariaDB's sequences (D10, roadmap item 32). **Code written, unbuilt** (code-only mode, 2026-11-11, D424);
  reading a sequence as a table, `CREATE OR REPLACE SEQUENCE` and `SHOW CREATE SEQUENCE`: **code written,
  unbuilt** (code-only mode, 2026-11-11, D500, D501, D502).
- MySQL's `JSON_VALUE` and `JSON_TABLE` with MySQL's path language (D289). **Code written, unbuilt**
  (code-only mode, 2026-11-11, D425); MySQL's error numbers for them: **code written, unbuilt** (code-only
  mode, 2026-11-12, D599).
- Following a live MySQL through its binlog for a switch with no downtime (D13).
- MySQL's own test suite run from outside the repo, with its pass rate published (D1, D10, D45).
- MySQL and TDS clients refused an infinite `numeric` or date written by a PostgreSQL session, as
  the dialect's out-of-range error, instead of sent as text (D290). **Code written, unbuilt** (code-only mode,
  2026-11-11, D426).
- SQL Server's ranking and offset window functions (`ROW_NUMBER`, `RANK`, `DENSE_RANK`, `NTILE`,
  `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE`) with its errors 10753 and 4112; only aggregates take
  `OVER` in that dialect today (found while building D340). **Code written, unbuilt** (code-only
  mode, 2026-11-11, D427). `IGNORE NULLS` and `ORDER BY` through a named window: **code written, unbuilt** (code-only mode,
  2026-11-12, D600).
- SQL Server's `rowversion`, `sql_variant`, `xml` and `hierarchyid` (D19). Untyped `xml` with `value()`, `query()`
  and `exist()`: **code written, unbuilt** (code-only mode, 2026-11-12, D695); `nodes()` in `APPLY`: **code written,
  unbuilt** (code-only mode, 2026-11-12, D696); `hierarchyid` with its methods: **code written, unbuilt** (code-only
  mode, 2026-11-12, D797); `rowversion` kept by every write: **code written, unbuilt** (D798). Left: `sql_variant`,
  `@@DBTS`, and `hierarchyid` and `rowversion` as their own types in the catalogs.
- `datetimeoffset` keeping its offset, and the declared `varchar` or `nvarchar` kept (D19, D133).
- `deleted.` in a Transact-SQL `UPDATE`'s `OUTPUT`, `TOP ... PERCENT` in `UPDATE`, `DELETE` and
  `INSERT`, and global `##temp` tables; `TOP ... PERCENT`, `WITH TIES`, `OUTPUT ... INTO` and
  `SELECT ... INTO` are built (sql.md, D300, D303). `deleted.` in `UPDATE`'s `OUTPUT`: **code
  written, unbuilt** (code-only mode, 2026-11-11, D430). `TOP (n)` in `UPDATE` and `DELETE`: **code written,
  unbuilt** (code-only mode, 2026-11-12, D605); `TOP ... PERCENT` and `INSERT TOP` in DML: **code written,
  unbuilt** (code-only mode, 2026-11-12, D632); `##temp` tables: **code written, unbuilt** (code-only mode, 2026-11-12,
  D891).
- `MERGE`: `DELETE` triggers fired by its deletes (by-source deletes too), SQL Server's refusal of
  an `OUTPUT ... INTO` target with triggers, checks or foreign keys, `SELECT ... INTO` keeping the
  source's identity (`NOT NULL` is built, D342), describing a prepared `OUTPUT ... INTO` that still returns
  rows, and PostgreSQL's `MERGE`; two `WHEN MATCHED` clauses, `$action`, `TOP` and `OUTPUT ...
  INTO` are built (D303). PostgreSQL's `MERGE` (`WHEN [NOT] MATCHED [BY SOURCE|TARGET] [AND ...] THEN
  UPDATE | DELETE | INSERT | DO NOTHING`): **code written, unbuilt** (code-only mode, 2026-11-12, D837).
- SQL Server's application locks choosing one victim when two waiters close a cycle (PR #91). **Code
  written, unbuilt** (code-only mode, 2026-11-11, D448).
- Cursor variables, `WHERE CURRENT OF` on `FOR UPDATE` cursors (in PostgreSQL too), `sp_cursor*`
  server cursors and cursors reset by `sp_reset_connection` (D291). Cursor variables (`DECLARE @c CURSOR`, `SET @c = CURSOR ...
  FOR`): **code written, unbuilt** (code-only mode, 2026-11-12, D849).
- `INSERT ... EXEC` of dynamic SQL, `EXEC AS`, three-part names and other databases in a batch
  (sql.md). `INSERT ... EXEC` of a procedure into a table or a temporary table: **code written, unbuilt**
  (code-only mode, 2026-11-12, D648).
- Numbered procedures and temporary functions (roadmap item 33d). Numbered procedures: **code written, unbuilt**
  (code-only mode, 2026-11-12, D643); temporary functions are refused, as SQL Server refuses them.
- SQL Server Agent jobs on the job scheduler (D27, D200). **Code written, unbuilt** (code-only mode, 2026-11-11, D523).
  The `msdb` views: **code written, unbuilt** (code-only mode, 2026-11-12, D623); step flow options and retries:
  **code written, unbuilt** (D624); operators and alerts: **code written, unbuilt** (code-only mode, 2026-11-12, D635).
  Sending the notifications (Database Mail, `sp_send_dbmail`): **code written, unbuilt** (code-only mode, 2026-11-12,
  D785).
- Identity on any column, `NEXT VALUE FOR` returning the sequence's type and its `OVER` form,
  `sp_sequence_get_range`, and extended properties beyond `MS_Description` (D148). `sp_sequence_get_range`:
  **code written, unbuilt** (code-only mode, 2026-11-11, D449).
- bcp native files with `varchar` columns, and untrusted constraints for bulk loads (D291).
- EF Core 9 in the `tds-apps` job, and mssql-jdbc's `getSchemas()` (D291).
- Oracle's `(+)` outer joins, `RETURNING ... INTO`, synonyms, database links and `ROWID` (sql.md). `(+)`
  outer joins in queries: **code written, unbuilt** (code-only mode, 2026-11-11, D432). Synonyms
  for relations: **code written, unbuilt** (D433). `RETURNING ... INTO :bind` in `eggdb shell`: **code written,
  unbuilt** (code-only mode, 2026-11-12, D689). Left: database links and `ROWID`.
- `CONNECT BY` over a join (the join's conditions in `WHERE` applied first), `PRIOR` beside a
  subquery, `ORDER SIBLINGS BY` a pseudo-column, and a sequence's `NEXTVAL` advancing once per
  source row of `INSERT ALL`; `CONNECT BY`, `INSERT ALL` and `INSERT FIRST` are built (D304).
  Check: a deep hierarchy against a recursive `WITH` on SQLite.
- Oracle's XML functions (the SQL/XML publishing functions: **code written, unbuilt**, D454;
  `EXTRACTVALUE` and `EXISTSNODE`: **code written, unbuilt**, D690; `XMLTABLE`: **code written, unbuilt**, D692;
  `XMLQUERY` and `XMLEXISTS` over XPath 1.0: **code written, unbuilt**, D693; `XMLType` left) and `TO_DATE(... DEFAULT ... ON
  CONVERSION ERROR)` (sql.md; the
  conversion functions' `DEFAULT ... ON CONVERSION ERROR`: **code written, unbuilt**, code-only
  mode, 2026-11-11, D431; `CAST ... DEFAULT` and `VALIDATE_CONVERSION`: **code written, unbuilt**, code-only
  mode, 2026-11-12, D603);
  `DISTINCT` in window aggregates (`LISTAGG (DISTINCT ...) OVER`, `COUNT(DISTINCT x) OVER`) and
  `KEEP` on `LISTAGG`; the rest of Oracle's regex syntax (`\z`, `[=e=]`, `[.ch.]`, `[^...]`
  matching a newline without `n`) (D332). The regex syntax: **code written, unbuilt** (code-only mode,
  2026-11-11, D452). `COUNT(DISTINCT x) OVER` and the other
  aggregates: **code written, unbuilt** (D453); `LISTAGG (DISTINCT) OVER`: **code written, unbuilt**
  (code-only mode, 2026-11-12, D604); `KEEP` on `LISTAGG` is not in Oracle's reference (D604). Window functions, `RATIO_TO_REPORT`, `KEEP` and
  `REGEXP_*` are built (D332). Check: Oracle-dialect tests of each.
- Oracle's types: `TIMESTAMP WITH TIME ZONE` keeping its offset, `BFILE`, `UROWID`, `XMLType`,
  object types and schema-level collection types (D19).
- `UTL_RAW`, `DBMS_LOB` and the other Oracle-supplied packages (sql.md). `UTL_RAW`'s and `DBMS_LOB`'s
  functions: **code written, unbuilt** (code-only mode, 2026-11-11, D434); `DBMS_LOB`'s locator procedures:
  **code written, unbuilt** (code-only mode, 2026-11-12, D658); `DBMS_RANDOM`, `DBMS_ASSERT` and `UTL_MATCH`:
  **code written, unbuilt** (code-only mode, 2026-11-12, D659); `DBMS_CRYPTO`'s hashes and MACs and
  `DBMS_UTILITY.GET_TIME`: **code written, unbuilt** (code-only mode, 2026-11-12, D660); `DBMS_CRYPTO.ENCRYPT` and
  `DECRYPT` with AES: **code written, unbuilt** (code-only mode, 2026-11-12, D661); `UTL_ENCODE`, `UTL_I18N` and
  `UTL_URL`: **code written, unbuilt** (code-only mode, 2026-11-12, D732); `UTL_COMPRESS`: **code written, unbuilt**
  (code-only mode, 2026-11-12, D733); `DBMS_APPLICATION_INFO`, `DBMS_SESSION`, `DBMS_LOCK.SLEEP` and `DBMS_STATS`:
  **code written, unbuilt** (code-only mode, 2026-11-12, D857); application contexts (`CREATE CONTEXT`,
  `DBMS_SESSION.SET_CONTEXT`): **code written, unbuilt** (code-only mode, 2026-11-12, D858); `DBMS_METADATA.GET_DDL`
  for tables and views: **code written, unbuilt** (code-only mode, 2026-11-12, D859). Left: `DBMS_SQL`, the other
  `DBMS_METADATA` object types, `UTL_FILE` and the rest.
- PL/SQL: autonomous transactions (after concurrent writers, M3), compound triggers, the
  mutating-table check, `IS A SET`, `IS EMPTY`, `MULTISET`, `MULTISET (subquery)`, `SET()` and
  `TABLE()` correlated in a join (roadmap item 33e). `IS A SET` and `IS EMPTY`: **code written, unbuilt** (code-only mode,
  2026-11-11, D455).
- `DBMS_SCHEDULER` jobs on the job scheduler (D27, D200). **Code written, unbuilt** (code-only mode, 2026-11-11, D524).
  Programs and named schedules: **code written, unbuilt** (code-only mode, 2026-11-12, D625); chains:
  **code written, unbuilt** (code-only mode, 2026-11-12, D634); the jobs' dictionary views:
  **code written, unbuilt** (code-only mode, 2026-11-12, D577).
- pg_cron: sub-minute schedules, `$`, `cron.alter_job` and time zones per job (D200). Sub-minute
  schedules, `$` and `cron.alter_job`: **code written, unbuilt** (code-only mode, 2026-11-11,
  D429); time zones per job: **code written, unbuilt** (code-only mode, 2026-11-12, D601).
- pg_cron: schedules checked at `cron.schedule`, a missed minute run after a restart, and jobs run
  as their `username` (D200). Jobs as their `username`: **code written, unbuilt** (D429); schedules checked
  at `cron.schedule` and the restart minute: **code written, unbuilt** (code-only mode, 2026-11-12, D602).
- NLS: languages beyond the seven, `NLS_TERRITORY` other than `AMERICA`, the NLS decimal character
  (roadmap item 42b). Six more languages and 13 more territories: **code written, unbuilt**
  (code-only mode, 2026-11-11, D451); the decimal character was already built.
- Named `NOT NULL` constraints kept, overloaded package procedures described over RPC, and
  `sp_sproc_columns` rows for PL/SQL functions (D295).
- Oracle's refusal of values for `GENERATED ALWAYS` identity columns, `ALTER COLUMN ... SET
  GENERATED` and `DROP IDENTITY` (roadmap item 37c). **Code written, unbuilt** (code-only mode, 2026-11-11, D428):
  the refusals were already there; Oracle's `MODIFY` forms added. Left: `ON NULL`.
- Oracle's `IS JSON` (lax syntax, `STRICT`, `TYPE (...)`), `JSON_SERIALIZE`'s `PRETTY`, `ASCII`
  and `TRUNCATE`, and `JSON_SCALAR` in Oracle's dialect (D302). **Code written, unbuilt** (code-only
  mode, 2026-11-11, D450).
- GoogleSQL, after D58's legal check: first BigQuery's dialect over the PostgreSQL protocol, then
  Spanner's API (D47, D48). Check: a correctness suite from the GoogleSQL reference.
- Snowflake SQL, after D58's legal check: first `VARIANT`, `FLATTEN` and its functions over the
  PostgreSQL protocol, then its SQL REST API (D47). Check: a suite from Snowflake's reference.
- Databricks/Spark SQL, after D58's legal check: first the Statement Execution REST API, then
  HiveServer2 Thrift (D47). Check: Beeline over Thrift and a suite from Spark's SQL reference.
- ClickHouse SQL: first its HTTP interface with its formats (`TabSeparated`, `JSONEachRow`), then
  the native protocol (D47). Check: clickhouse-connect and `clickhouse client` in CI.
  HTTP interface and its text formats: **code written, unbuilt** (code-only mode, 2026-11-11,
  D510). Left: ClickHouse's SQL dialect and the native protocol.
- Trino/Presto SQL: first the client REST protocol (`/v1/statement`) with Trino's types and
  functions (D47). Check: the Trino CLI and JDBC driver against EggDB in CI.
  The client REST protocol: **code written, unbuilt** (code-only mode, 2026-11-11, D511). Left:
  Trino's SQL dialect, types' functions and TLS for stock clients.
- IBM Db2 SQL and SQL PL, after D58's legal check: first DRDA's connect and SQL execution for Db2
  LUW clients, then Informix and Db2 for z/OS (D47). Check: a suite from Db2's SQL reference.
- Sybase/SAP ASE SQL, after D58's legal check: first ASE's differences on the SQL Server dialect
  over TDS 7+ (D47). Check: a suite from ASE's reference manual.
- Redshift (after D58's legal check), Vertica, Greenplum and Teradata SQL over the PostgreSQL
  protocol, Redshift first (D47). Check: a correctness suite per dialect from its own reference.
- SAP HANA SQL, after D34's and D58's legal checks: first over the PostgreSQL protocol, then its
  SQL Command Network Protocol (D47). Check: a suite from SAP's SQL reference.
- `QUALIFY` in the dialects that have it (D240). **Code written, unbuilt** (code-only mode, 2026-11-12, D890), as a rewrite to a
  derived table, accepted in every dialect.

### M11 Columnar analytics and time series
- Columnar table storage: row groups of chunks, deletes as bitmaps, `CREATE TABLE ... USING
  columnar` (D201).
- Float and string chunks, and compression beyond the encodings (D201).
- ClickBench-style queries and TPC-H against DuckDB and ClickHouse in the compare job (D7, D201).
- Analytical replicas: columnar copies kept in step with row tables (HTAP) (D18).
- Gap filling, `first` and `last`, and hypertables partitioned by time automatically (D202,
  PR #86). `first` and `last`: **code written, unbuilt** (code-only mode, 2026-11-11, D435).
- Retention, downsampling and continuous aggregates for time series and observability data (D39,
  D202, D220). Check: a TSBS benchmark. TimescaleDB's `first` and `last` aggregates: **code written,
  unbuilt** (code-only mode, 2026-11-12, D647); retention and continuous-aggregate policies (`CREATE EXTENSION
  timescaledb`): **code written, unbuilt** (code-only mode, 2026-11-12, D736). Left: incremental refresh.
- ASOF and window joins (D50, D243). ASOF joins: **code written, unbuilt** (code-only mode, 2026-11-11,
  D447); the `ASOF JOIN ... USING (keys, time)` form: **code written, unbuilt** (code-only mode, 2026-11-12, D637).
  Left: window joins.
- Bitemporal tables (D50). **Code written, unbuilt** (code-only mode, 2026-11-12, D755).
- Order-book and double-entry ledger primitives, with microsecond tail-latency targets in the
  benchmarks (D50). The primitives (`CREATE EXTENSION eggdb_finance`): **code written, unbuilt** (code-only mode,
  2026-11-12, D756). Left: the latency targets.
- Parquet: nested columns, GZIP (on D249's DEFLATE) and ZSTD, statistics and dictionaries when
  writing, several row groups, streaming (D202). Check: a benchmark against DuckDB's reader.
  Statistics and several row groups when writing: **code written, unbuilt** (code-only mode,
  2026-11-12, D572); GZIP and ZSTD pages, read and written: **code written, unbuilt** (code-only mode, 2026-11-12,
  D734); nested columns read (lists, maps and structs of plain values, as JSON text): **code written, unbuilt**
  (code-only mode, 2026-11-12, D838). Left: deeper nesting (lists of structs and the like), writing nested columns,
  and streaming.
- `read_parquet` and the other file readers in SQL, after D202's decision on which files a
  server's SQL may open (D29, D202). The rule (embedded sessions read local files, servers none) and `read_parquet`
  with a column definition list: **code written, unbuilt** (code-only mode, 2026-11-12, D703); `read_csv`: **code
  written, unbuilt** (code-only mode, 2026-11-12, D704); `read_arrow`: **code written, unbuilt** (code-only mode,
  2026-11-12, D705); the file's own column list: **code written, unbuilt** (code-only mode, 2026-11-12, D706).
- Arrow IPC reading, the `.arrow` file format, several batches, dictionaries and nested types
  (D240). Reading streams and `.arrow` files with several batches: **code written, unbuilt**
  (code-only mode, 2026-11-12, D573), and writing `.arrow` files too; reading dictionary-encoded columns:
  **code written, unbuilt** (code-only mode, 2026-11-12, D640); reading compressed batches (`LZ4_FRAME`,
  `ZSTD`): **code written, unbuilt** (code-only mode, 2026-11-12, D641); list, large list and struct columns read as
  JSON text: **code written, unbuilt** (code-only mode, 2026-11-12, D835); maps, fixed-size lists and unions: **code
  written, unbuilt** (code-only mode, 2026-11-12, D860); dictionary-encoded values inside nested columns:
  **code written, unbuilt** (code-only mode, 2026-11-12, D897).
- Arrow Flight SQL and ADBC (D29).
- NDJSON, ORC and Avro (D29). NDJSON: **code written, unbuilt** (code-only mode, 2026-11-11, D505).
  Avro: **code written, unbuilt** (code-only mode, 2026-11-11, D509). ORC (`read_orc`, flat columns): **code
  written, unbuilt** (code-only mode, 2026-11-12, D735); ORC list, map and struct columns as JSON text: **code written,
  unbuilt** (code-only mode, 2026-11-12, D836); ORC unions: **code written, unbuilt** (code-only mode, 2026-11-12, D896).
- Apache Iceberg and Hudi tables, and Delta Lake after D58's legal check, read and written, with
  an Iceberg REST catalog (D29, D46).
- Excel files read and written as tables: `.xlsx` (OOXML), and `.xls` (MS-XLS) after D58's
  legal check (D49). `.xlsx`: **code written, unbuilt** (code-only mode, 2026-11-11, D506).
- Mainframe loaders: COBOL copybooks, EBCDIC, packed decimal, and VSAM and IMS exports (D49, D242).
  Copybooks, EBCDIC and packed decimal: **code written, unbuilt** (code-only mode, 2026-11-11,
  D508); VSAM and sequential unloads in RECFM V, VB and VBS (block descriptor words, spanned records): **code
  written, unbuilt** (code-only mode, 2026-11-12, D839). Left: IMS exports.
- dBase, ISAM and Btrieve readers where public specifications exist (D49). dBase: **code written,
  unbuilt** (code-only mode, 2026-11-11, D507).
- KQL, after D58's legal check: `join`, `union`, `let`, `parse`, `mv-expand`, `make-series`,
  `render`, percentiles and a Kusto-compatible REST endpoint (D49, D242).
- Splunk's SPL, with HTTP Event Collector ingest, after D58's legal check (D49).
- OLAP cubes and semantic models with MDX, DAX and XMLA, after D34's legal check (D38).
- A semantic layer of metrics and dimensions, defined once and used by BI and OLAP (D46, D238). Models, dimensions and
  metrics in `eggdb_bi`: **code written, unbuilt** (code-only mode, 2026-11-12, D745). Left: OLAP use.
- BI dashboards and charts, designed by Claude Fable (D46, D238).
- Interactive reports with filters and drill-down, in CSV, HTML and PDF (D46, D238). **Code written, unbuilt**
  (code-only mode, 2026-11-12, D746): `eggdb_bi.render_report` with filters. Left: charts.
- Reports sent to `https://` targets and by email through the operator's mail server, shared and
  embedded in other applications (D46, D238). `https://` and e-mail delivery: **code written, unbuilt** (code-only
  mode, 2026-11-12, D747). Left: sharing and embedding.
- Spreadsheet-style tables with formula columns, and a grid view of them in the client (D38). The tables and formula
  columns (`eggdb_sheets`): **code written, unbuilt** (code-only mode, 2026-11-12, D765). Left: the client's grid view.
- Arrays: SQL/MDA, Zarr, HDF5 and NetCDF (D57). NetCDF classic files (`read_netcdf`): **code written, unbuilt**
  (code-only mode, 2026-11-12, D709). Zarr v2 and v3 arrays (`read_zarr`, raw, zlib, gzip and zstd chunks): **code
  written, unbuilt** (code-only mode, 2026-11-12, D715). Zarr v3 sharding and Blosc 1 chunks (BloscLZ, LZ4, Snappy, zlib,
  zstd, byte shuffle): **code written, unbuilt** (code-only mode, 2026-11-12, D806); Blosc bit shuffle: **code written,
  unbuilt** (code-only mode, 2026-11-12, D852). Left: SQL/MDA, Blosc2, HDF5 and NetCDF-4.
- Genomics: VCF, BAM and CRAM, and htsget (D57). VCF files in SQL (`read_vcf`, gzip and BGZF read): **code written,
  unbuilt** (code-only mode, 2026-11-12, D707); BAM (`read_bam`): **code written, unbuilt** (code-only mode,
  2026-11-12, D708). Left: CRAM and htsget.

### M12 Vectors, geospatial and AI
- A stored `vector(n)` type with its dimension checked, binary send and receive and a catalog
  entry (D203). **Code written, unbuilt** (code-only mode, 2026-11-12, D796). Left: the dimension kept as a type modifier
  (`atttypmod`, shown as `vector(3)`) rather than a check, and pgvector's own 22000 error for a wrong dimension.
- `CREATE INDEX ... USING hnsw` and `ivfflat`, kept in the file and used by `ORDER BY ... LIMIT`
  (D203).
- `halfvec`, `sparsevec`, `int8` and bit vectors, and vector arithmetic (D19, D203). Bit distances,
  `binary_quantize`, `subvector` and `sparsevec` text input: **code written, unbuilt** (code-only
  mode, 2026-11-11, D437). Left: the stored types and arithmetic.
- An ANN benchmark (recall and queries per second) against pgvector (D203).
- A stored geometry type (hex EWKB, the binary wire format, a catalog entry) (D204). **Code written, unbuilt** (code-only mode, 2026-11-12,
  D845); typmods (`geometry(Point, 4326)`) as checks: **code written, unbuilt** (code-only mode, 2026-11-12, D846).
  Left: `geography`, Z and M subtypes, and the spatial functions returning `geometry`.
- R-tree spatial indexes, with MySQL's `SPATIAL` keys (D204, sql.md).
- Z and M coordinates, and geography on the WGS 84 spheroid (D204).
- `ST_Transform` between any projections, overlay operations (union, intersection, buffer) and
  validity checks (D204, D250). Check: the OGC test suite cases. Validity checks (rings and
  self-crossings) and 22 more functions: **code written, unbuilt** (code-only mode, 2026-11-11,
  D438); World Mercator and UTM projections: **code written, unbuilt** (code-only mode, 2026-11-12, D741); linear
  referencing and line intersections: **code written, unbuilt** (code-only mode, 2026-11-12, D883). Left: area
  overlays, full validity, other projections and datum shifts.
- SQL Server's `geometry` and `geography` types with their methods (D19, D204).
- Vector tiles (`ST_AsMVT`, `ST_AsMVTGeom`), and H3 and S2 indexes (D57, D250). `ST_AsMVTGeom`:
  **code written, unbuilt** (code-only mode, 2026-11-11, D520); `ST_AsMVT`: **code written, unbuilt** (code-only
  mode, 2026-11-12, D608); S2 cell ids: **code written, unbuilt** (code-only mode, 2026-11-12, D609). Left: H3.
- Raster, GeoTIFF and Cloud-Optimized GeoTIFF (D57). GeoTIFF headers (`geotiff_info`): **code written, unbuilt**
  (code-only mode, 2026-11-12, D710); pixels (`read_geotiff`): **code written, unbuilt** (code-only mode, 2026-11-12,
  D711). Left: raster types and operations.
- Topology and routing at pgRouting's level (D57). Routing (`pgr_dijkstra`, `pgr_drivingDistance`,
  `pgr_connectedComponents`): **code written, unbuilt** (code-only mode, 2026-11-12, D782); topology
  (`pgr_createTopology`, `pgr_createVerticesTable`): **code written, unbuilt** (code-only mode, 2026-11-12, D831).
  `pgr_aStar`, `pgr_dijkstraCost` and `pgr_bdDijkstra`: **code written, unbuilt** (code-only mode, 2026-11-12, D832).
  Many-to-many forms and `pgr_dijkstraCostMatrix`: **code written, unbuilt** (code-only mode, 2026-11-12, D865).
  `pgr_TSP`, `pgr_TSPeuclidean` and `pgr_analyzeGraph`: **code written, unbuilt** (code-only mode, 2026-11-12, D882).
  `pgr_nodeNetwork`: **code written, unbuilt** (code-only mode, 2026-11-12, D883). Spanning trees (`pgr_kruskal`,
  `pgr_prim`) and maximum flows (`pgr_maxFlow`, `pgr_pushRelabel`, `pgr_edmondsKarp`, `pgr_boykovKolmogorov`): **code
  written, unbuilt** (code-only mode, 2026-11-12, D905). Left: the rest of pgRouting's functions (the DFS and BFS
  forms, k shortest paths, turn restrictions, contraction and others).
- The OGC APIs (D57). OGC API - Features (Core, GeoJSON) on `--rest-listen`: **code written, unbuilt** (code-only
  mode, 2026-11-12, D781). CQL2 filters (text and JSON) and queryables (Part 3): **code written, unbuilt**
  (code-only mode, 2026-11-12, D879); CRS (Part 2): **code written, unbuilt** (code-only mode, 2026-11-12, D880).
  Tiles with vector tiles on `WebMercatorQuad`: **code written, unbuilt** (code-only mode, 2026-11-12, D881);
  `WorldCRS84Quad`: **code written, unbuilt** (code-only mode, 2026-11-12, D906). Left: Maps, and the other tile matrix
  sets of OGC 17-083r4 (the UTM and polar ones, `EuropeanETRS89_LAEAQuad`, `CanadianNAD83_LCC`).
- An ONNX runtime with `embed(model, text)` on local models (D31, D205).
- Opt-in remote embedding and language-model providers, with keys from the secrets store (D31,
  D205). **Code written, unbuilt** (code-only mode, 2026-11-12, D777).
- Reranking models in hybrid search (D31). Remote rerank models (`eggdb_remote_rerank`): **code written, unbuilt**
  (code-only mode, 2026-11-12, D778). Left: local models.
- A semantic cache (D31). **Code written, unbuilt** (code-only mode, 2026-11-12, D740).
- Agent memory as a built-in model: first a memory table per agent (text, embedding, time)
  recalled by hybrid search and offered as MCP tools (D31, D205). Check: an MCP recall test. The memory table and
  hybrid recall (`eggdb_ai`): **code written, unbuilt** (code-only mode, 2026-11-12, D740). Left: the MCP tools.
- In-database training, first linear and logistic regression over a query's rows with predictions
  in SQL (D31). Check: fits compared with a reference on a public dataset. **Code written,
  unbuilt** (code-only mode, 2026-11-11, D439).
- A feature store with low-latency online feature serving (D31, D55). **Code written, unbuilt** (code-only mode,
  2026-11-12, D744): feature views, an online table and point-in-time reads in `eggdb_features`.
- An end-to-end retrieval benchmark (D205).

### M13 Graphs
- SQL/PGQ and GQL, and graph views over tables (D10, D206).
- Cypher path predicates pushed into the shortest-path search (D333). **Code written, unbuilt** (code-only mode, 2026-11-11,
  D534), with the list predicates `all`, `any`, `none` and `single`.
- GQL quantified paths and `SHORTEST k` (D333). `SHORTEST k`, `ANY` and `ALL SHORTEST`, and quantifiers on one
  relationship: **code written, unbuilt** (code-only mode, 2026-11-11, D535); path modes: **code written, unbuilt**
  (code-only mode, 2026-11-12, D726); quantified path patterns of one relationship: **code written, unbuilt**
  (code-only mode, 2026-11-12, D727); groups of several relationships: **code written, unbuilt** (code-only mode,
  2026-11-12, D884); `WHERE` inside a group: **code written, unbuilt** (code-only mode, 2026-11-12, D898).
- Cypher's `CALL (a, b) { ... }` scope form, `CALL { ... } IN TRANSACTIONS`, and the label-expression
  operators `&`, `!` and `%` (D351). The label-expression operators: **code
  written, unbuilt** (code-only mode, 2026-11-11, D478); the scope form and `IN TRANSACTIONS` (in one
  transaction, `ON ERROR FAIL` only): **code written, unbuilt** (code-only mode, 2026-11-11, D479).
- Indexes on labels and properties, and uniqueness constraints (D206). Schema commands, uniqueness and
  existence constraints: **code written, unbuilt** (code-only mode, 2026-11-11, D503); label and property indexes
  used by `MATCH`: **code written, unbuilt** (code-only mode, 2026-11-12, D728); `WHERE` equalities using them: **code
  written, unbuilt** (code-only mode, 2026-11-12, D729); range conditions and `STARTS WITH` reading them: **code
  written, unbuilt** (code-only mode, 2026-11-12, D885). Left: an order-preserving index encoding read as key ranges.
- Bolt: open transactions across messages, `LOGON` (5.1 and later), routing tables and bookmarks
  (D207). `LOGON`, routing tables and bookmarks: **code written, unbuilt** (code-only mode, 2026-11-11, D504);
  open transactions across messages: **code written, unbuilt** (code-only mode, 2026-11-12, D606).
- Bolt's temporal and spatial PackStream types (D207). **Code written, unbuilt** (code-only mode, 2026-11-12, D720),
  with Cypher's temporal and point values; their arithmetic, `duration.between` and `point.distance`: **code written,
  unbuilt** (code-only mode, 2026-11-12, D721).
- Apache TinkerPop Gremlin, and Cosmos DB's Gremlin API after D58's legal check (D48).
- Louvain, Leiden, betweenness and closeness centrality, weighted shortest paths, k-core and graph
  embeddings (D55, D248). Louvain, betweenness, closeness, weighted shortest paths and k-core:
  **code written, unbuilt** (code-only mode, 2026-11-11, D436), with FastRP embeddings; Leiden: **code
  written, unbuilt** (code-only mode, 2026-11-12, D607). Left: learned embeddings.
- The graph algorithms as procedures in SQL/PGQ (D248).
- RDF triples and quads with SPARQL 1.1 (D38). Check: the W3C SPARQL suite. The quad store and its readers (N-Triples,
  N-Quads, Turtle, TriG) in `eggdb-rdf`: **code written, unbuilt** (code-only mode, 2026-11-12, D886); the SPARQL
  1.1 query and update reader: **code written, unbuilt** (code-only mode, 2026-11-12, D887); queries evaluated with
  the four result formats: **code written, unbuilt** (code-only mode, 2026-11-12, D888); updates, the SPARQL protocol
  and Graph Store protocol, and `eggdb serve --sparql-listen`: **code written, unbuilt** (code-only mode, 2026-11-12,
  D889). SPARQL in the CLI (`eggdb sql --sparql`): **code written, unbuilt** (code-only mode, 2026-11-12, D922). Left: the W3C
  SPARQL suite in CI, `SERVICE`, and RDF in the MCP tools. `LOAD` from allowed URLs: **code written, unbuilt**
  (code-only mode, 2026-11-12, D923).
- The LDBC benchmark against Neo4j (D7, D206).

### M14 Backup, point-in-time recovery and replication
- Continuous log shipping and replay to any commit (D24, D208). Check: the simulator test of
  restoring to a stamp.
- Incremental and parallel backups, encrypted, with retention (D24, D208).
- Instant snapshots, and copy-on-write clones and branches of a whole database (D24, D28).
- `eggdb dump`: one logical file of the whole database (schemas, rows, collections, roles,
  routines), where `eggdb export`'s eggdb-jsonl copies raw key-value entries (D24, D93). **Code written, unbuilt**
  (code-only mode, 2026-11-12, D899) as `eggdb export --dump`, which `eggdb restore` loads.
- `eggdb export --sql` with views, sequences, comments and every constraint (D24, PR #86). **Code written,
  unbuilt** for the PostgreSQL dialect (code-only mode, 2026-11-11, D446). Roles, memberships, table grants, routines and triggers
  in the PostgreSQL dump: **code written, unbuilt** (code-only mode, 2026-11-12, D842).
- Immutable (WORM) backups that ransomware can't alter (D53). Copies archived into S3-compatible buckets under Object
  Lock (`eggdb backup --lock-days`): **code written, unbuilt** (code-only mode, 2026-11-12, D919). Left: a locked
  manifest, and WORM for local archives.
- Backups through `eggdb serve`'s clients, compaction of a file `serve` holds, and progress
  reports (D141).
- Compaction's lock window closed: the platform layer tells whether a file it locked is still
  linked at its path (D141).
- Tenant-aware backup and restore (D234). Logical, per tenant (`eggdb_tenancy.backup_tenant`, `restore_tenant`):
  **code written, unbuilt** (code-only mode, 2026-11-12, D784).
- Replication wired into the engine: the commit hook and its change log, a transport with TLS and
  authentication, and `eggdb serve --replica-of` (D209).
- Initial copy of a replica from a snapshot archive (D208, D209).
- Automatic failure detection, fencing of an old primary, and planned switchover (D18, D209).
- Delayed replicas, and standbys in another region fed by log shipping (D18).
- Replica freshness chosen per request: linearizable, read-your-writes, causal, bounded staleness
  or eventual (D18).
- Jepsen-style checks against real processes (D9, D209).
- Logical replication to other EggDB clusters, across versions, for upgrades without downtime
  (D18).
- PostgreSQL's replication protocol served, so other systems can follow EggDB (D18, D103).
- Change streams to Kafka topics, AMQP exchanges, MQTT and NATS, and `https://` webhooks;
  `--cdc-queue` already feeds AMQP, STOMP and SQS queues (D28, D210, D217).
  Kafka topics (`--cdc-topic`): **code written, unbuilt** (code-only mode, 2026-11-11, D527).
- Retries with backoff and a dead-letter table, Debezium's `schema` section and transaction
  metadata (D210). Check: a consumer test that receives every commit of a workload.
  Retries with backoff and the dead-letter table: **code written, unbuilt** (code-only mode,
  2026-11-11, D528); Debezium's `schema` section: **code written, unbuilt** (code-only mode, 2026-11-12,
  D610). Left: transaction metadata (needs a transaction id per transaction).
- Change capture without triggers (D210).
- System-versioned temporal tables (SQL:2011) with `AS OF` over kept history, and SQL Server's
  temporal tables (D28). `FOR SYSTEM_TIME AS OF | FROM ... TO | BETWEEN ... AND | CONTAINED IN | ALL` over the tables
  `eggdb_temporal` keeps (D755): **code written, unbuilt** (code-only mode, 2026-11-12, D794); SQL Server's and
  MariaDB's `CREATE`/`ALTER` syntax for system versioning: **code written, unbuilt** (D795); SQL Server's
  `SYSTEM_VERSIONING = ON` in `ALTER TABLE` over a declared period, and `HISTORY_RETENTION_PERIOD`: **code written,
  unbuilt** (code-only mode, 2026-11-12, D900). Left: invisible (`HIDDEN`) period columns.
- Materialized views kept up to date incrementally at commit, and continuous queries (D28,
  PR #86).
- `eggdb follow` of `pgoutput`'s parallel streaming and two-phase messages, binary arrays,
  domains, enums, ranges, network and geometric types, and tables renamed, moved or recreated
  during a follow (D296).
- `eggdb follow` of SQL Server CDC: column type changes, capture instances keyed by a unique index
  with net changes, a table dropped and re-added between polls, and a lost connection during the
  first copy (D282).

### M15 Sharding and distributed SQL
- Raft's term, vote and log persisted in the file's consensus area, snapshots and log compaction
  (D20, D211).
- Membership changes by joint consensus, pre-vote, and leader leases for reads (D211).
- The network transport and `eggdb serve --cluster` (D211, D212).
- The shard map and decision log replicated on Raft (D212).
- The data copy of online moves over the replication protocol (D209, D212).
- A distributed SQL planner that pushes work to shards (D212).
- Snapshot reads across shards by timestamp, and serializable distributed transactions (D18,
  D212).
- Global indexes (D18).
- Multi-region clusters with latency-aware placement (D4, D212).
- Rebalancing simulated under load, with partitions, clock skew and node failures (D18, D212).
- Chaos testing of clusters (D44, D252).
- The tenant prefix in the key layout, so tenants move online between shards (D20, D234).
- Databases per tenant moved and rebalanced online (D18, D32).
- Rolling upgrades with mixed versions in a cluster (D20).
- MPP distributed analytics with workload queues (D58).
- Resumable petabyte-scale migrations: Merkle range comparison and parallel ranges across nodes
  (D58, D251).
- The etcd v3 gRPC API and the ZooKeeper protocol (D54, D247). The ZooKeeper protocol (`eggdb-zookeeper`, `--zookeeper-listen`):
  **code written, unbuilt** (code-only mode, 2026-11-12, D742). Left: the etcd v3 API (after HTTP/2 and gRPC).
- Watches and service discovery on change streams (D54, D247). **Code written, unbuilt** (code-only mode, 2026-11-12,
  D743), with notifications on `eggdb_services` and the ZooKeeper watches of D742.
- JsonLogic targeting and fractional rollouts for feature flags (D247). **Code written, unbuilt** (code-only
  mode, 2026-11-11, D440).
- Benchmarks at scale: TPC-C and YCSB on a cluster (D7, D18).

### M16 Messaging, wide-column, objects and observability
- Kafka consumer groups (D33, D213). **Code written, unbuilt** (code-only mode, 2026-11-11, D525). Static
  membership: **code written, unbuilt** (code-only mode, 2026-11-12, D589). Left: the new rebalance
  protocol. The group listing APIs: **code written,
  unbuilt** (code-only mode, 2026-11-11, D545).
- Kafka transactions and idempotent producers (D33, D213). Idempotent producers: **code written,
  unbuilt** (code-only mode, 2026-11-11, D546); transactions: **code written, unbuilt** (code-only mode,
  2026-11-12, D694); transaction timeouts: **code written, unbuilt** (code-only mode, 2026-11-12, D717).
- Kafka's flexible request versions, SASL, and topic creation and deletion APIs (D213). Topic creation and
  deletion: **code written, unbuilt** (code-only mode, 2026-11-11, D492). SASL/PLAIN: **code written, unbuilt** (code-only mode,
  2026-11-11, D493).
- Partitions per topic, retention and compaction (D33, D213). Partitions per topic (D492) and retention:
  **code written, unbuilt** (code-only mode, 2026-11-11, D494); compaction: **code written, unbuilt** (code-only
  mode, 2026-11-12, D616).
- Kafka Connect's REST and worker protocol (D46).
- A benchmark against Kafka (D33, D213).
- AMQP 0-9-1: durable exchanges and bindings, headers exchanges, exclusive and auto-delete queues
  (D33, D214). Headers exchanges and exclusive and auto-delete queues: **code written, unbuilt**
  (code-only mode, 2026-11-11, D482).
- AMQP 0-9-1 `tx` transactions (D33, D214). **Code written, unbuilt** (code-only mode, 2026-11-11, D483).
- Dead-letter exchanges, message and queue TTL, delayed messages and priorities (D33, D214). Dead-letter
  exchanges, message TTL and delayed messages: **code written, unbuilt** (code-only mode,
  2026-11-11, D516); `x-death`: **code written, unbuilt** (code-only mode, 2026-11-12, D613). Queue TTL
  (`x-expires`): **code written, unbuilt** (code-only mode, 2026-11-12, D614); priorities: **code written,
  unbuilt** (code-only mode, 2026-11-12, D615), within a delivery window.
- Replicated (quorum-style) queues (D33).
- A benchmark against RabbitMQ (D33, D214).
- MQTT persistent sessions, and QoS 1 and 2 redelivery to subscribers (D33, D215). **Code written,
  unbuilt** (code-only mode, 2026-11-11, D526); sessions kept in memory. Kept in the file: **code
  written, unbuilt** (code-only mode, 2026-11-12, D581); flow control: **code written, unbuilt**
  (code-only mode, 2026-11-12, D582).
- MQTT 5's topic aliases, shared subscriptions, message expiry and reason strings (D33, D215). Topic
  aliases, message expiry and reason strings: **code written, unbuilt** (code-only mode, 2026-11-11, D480);
  shared subscriptions: **code written, unbuilt** (code-only mode, 2026-11-11, D481).
- MQTT over WebSockets (D215). **Code written, unbuilt** (code-only mode, 2026-11-11, D517).
- A benchmark against Mosquitto and EMQX (D33, D215).
- SQS, after D58's legal check: FIFO queues, attributes, batches, long polling, dead-letter
  queues and a non-zero `ChangeMessageVisibility` (D33, D216).
- NATS JetStream: durable streams and consumers (D33, D216). **Code written, unbuilt** (code-only mode, 2026-11-12,
  D722). Key-value bucket reads and writes (per-subject limits, rollups, direct gets): **code written, unbuilt**
  (code-only mode, 2026-11-12, D723); idle heartbeats, timed redelivery and pull timeouts for watches: **code
  written, unbuilt** (code-only mode, 2026-11-12, D724); object stores (stream names and lists by subject, checked
  through an object store's chunk, metadata-rollup and purge flow): **code written, unbuilt** (code-only mode,
  2026-11-12, D810); mirrors and sources within one server: **code written, unbuilt** (code-only mode, 2026-11-12,
  D811). Left: sources from other domains and servers.
- NATS no-responders, token and NKey authentication, clusters and leaf nodes (D216). No-responders and
  token authentication: **code written, unbuilt** (code-only mode, 2026-11-11, D476); NKeys: **code written,
  unbuilt** (code-only mode, 2026-11-12, D617).
- Benchmarks against ElasticMQ and nats-server (D216).
- AMQP 1.0: annotations and properties end to end, transactions, dynamic nodes, link resumption,
  `amqp:modified`, and the `idle-time-out` enforced and heartbeats sent (D217, D352). Dynamic nodes: **code
  written, unbuilt** (code-only mode, 2026-11-11, D538); the idle time-out enforced: **code
  written, unbuilt** (D539); `amqp:modified` and `released`: **code written, unbuilt** (code-only mode,
  2026-11-12, D627).
- JMS clients over AMQP 1.0, and Azure Service Bus-style use after D58's legal check (D33).
- STOMP `/exchange/` destinations and selectors (D217). **Code written, unbuilt** (code-only mode,
  2026-11-11, D477); selectors on `/queue/` subscriptions are not written.
- A dead-letter queue for rejected messages, and an in-process inbox for embedded consumers
  (D217). The dead-letter queue: **code written, unbuilt** (code-only mode, 2026-11-11, D540); the in-process inbox
  (`eggdb::inbox`): **code written, unbuilt** (code-only mode, 2026-11-12, D776).
- CQL collections, tuples, user-defined types and counters (D19, D218). Collections and tuples, whole values and list
  appends: **code written, unbuilt** (code-only mode, 2026-11-12, D730). Adding to and removing from lists, sets and
  maps, prepends, `m[k] = v` and `l[i] = v`, counter decrements, `CONTAINS` and `CONTAINS KEY`: **code written,
  unbuilt** (code-only mode, 2026-11-12, D807); `DELETE m[k]` and `DELETE l[i]`: **code written, unbuilt** (code-only
  mode, 2026-11-12, D808); user-defined types (`CREATE TYPE`, `DROP TYPE`, `{field: value}` values): **code written,
  unbuilt** (code-only mode, 2026-11-12, D809); `ALTER TYPE ... ADD | RENAME`: **code written, unbuilt** (code-only
  mode, 2026-11-12, D843); `system_schema.types`: **code written, unbuilt** (code-only mode, 2026-11-12, D844); field
  selection (`home.street`): **code written, unbuilt** (code-only mode, 2026-11-12, D853); updating one field: **code
  written, unbuilt** (code-only mode, 2026-11-12, D854).
- CQL TTL, `USING TIMESTAMP`, and lightweight transactions beyond `IF NOT EXISTS` (D218). Conditional `UPDATE` and `DELETE`: **code written,
  unbuilt** (code-only mode, 2026-11-12, D587).
- CQL paging, protocol v5 framing and compression (D218). Paging: **code written, unbuilt** (code-only mode,
  2026-11-11, D519).
- CQL secondary indexes, materialized views, `ALTER TABLE`, schema events and prepared statements
  shared between connections (D218). Prepared statements shared between connections:
  **code written, unbuilt** (code-only mode, 2026-11-12, D583). `ALTER TABLE`: **code written, unbuilt** (code-only
  mode, 2026-11-12, D584). Secondary indexes: **code written, unbuilt** (code-only
  mode, 2026-11-12, D585).
- A benchmark against ScyllaDB and Cassandra (D7, D218).
- DynamoDB, after D58's legal check: secondary indexes, filter and projection expressions,
  batches, transactions, streams, TTL, all conditions, and PartiQL (D48, D241).
- The Bigtable and HBase APIs, and Cosmos DB's Table API, after D58's legal check (D48).
- S3, after D58's legal check: versioning, object lock, ACLs and bucket policies, tagging,
  lifecycle rules and server-side encryption headers (D219).
- S3, after D58's legal check: `aws-chunked` uploads, `ListParts`, `ListMultipartUploads`,
  virtual-hosted buckets and conditional requests (D219).
- S3, after D58's legal check: large objects streamed, and listing without loading a bucket's
  descriptions (D219). Check: a benchmark against MinIO.
- InfluxQL and Flux, and points with the same series and time deduplicated (D220). InfluxQL's core:
  **code written, unbuilt** (code-only mode, 2026-11-11, D521); `fill()` and `GROUP BY time()` offsets: **code
  written, unbuilt** (code-only mode, 2026-11-12, D675); `GROUP BY *`, `SLIMIT`, several measurements and
  `distinct()`: **code written, unbuilt** (code-only mode, 2026-11-12, D676); math between fields: **code written,
  unbuilt** (code-only mode, 2026-11-12, D677); deduplication: **code written, unbuilt** (code-only mode, 2026-11-12,
  D678); `INTO`: **code written, unbuilt** (code-only mode, 2026-11-12, D679); subqueries: **code written, unbuilt**
  (code-only mode, 2026-11-12, D680); `tz()`: **code written, unbuilt** (code-only mode, 2026-11-12, D681);
  Flux's core: **code written, unbuilt** (code-only mode, 2026-11-12, D682); `SHOW SERIES`, `DROP` and `DELETE`:
  **code written, unbuilt** (code-only mode, 2026-11-12, D712); Flux's `map`, `pivot` and row expressions: **code
  written, unbuilt** (code-only mode, 2026-11-12, D713); variables, several results and `join`: **code written,
  unbuilt** (code-only mode, 2026-11-12, D912). Left: the rest of Flux (`join.*` and other packages, functions
  defined in the query) and pushing its windows into SQL.
- Remote write 2.0, native histograms and exemplars (D220). Remote write 2.0 with native histograms kept as
  `_count` and `_sum`: **code written, unbuilt** (code-only mode, 2026-11-12, D665); histogram buckets and exemplars:
  **code written, unbuilt** (code-only mode, 2026-11-12, D683).
- PromQL's remaining functions and modifiers: the `@` modifier and negative offsets, `limitk`
  and `limit_ratio`, `clamp*`, `predict_linear`, `deriv`, `resets`, `changes`, `absent*`,
  `sort*`, date, trigonometric and the other `*_over_time` functions, string results, named
  capture groups in `label_replace`, a query timeout (a subquery over many series at a fine
  resolution, such as `max_over_time(rate({__name__=~".+"}[1m])[127d:1s])`, stays under the step
  cap but can hold a thread for hours), and label matchers pushed down into the SQL read (D334). The functions (`clamp*`, `predict_linear`, `deriv`, `resets`,
  `changes`, `absent*`, `sort*`, date, trigonometric and the other `*_over_time`): **code written,
  unbuilt** (code-only mode, 2026-11-11, D441). The `@` modifier, negative offsets, `limitk` and `limit_ratio`:
  **code written, unbuilt** (code-only mode, 2026-11-12, D611). Named capture groups and the query timeout:
  **code written, unbuilt** (code-only mode, 2026-11-12, D612).
- OTLP ingest of metrics, logs and traces (D39). OTLP/HTTP in JSON and protobuf: **code written,
  unbuilt** (code-only mode, 2026-11-11, D498); gRPC is not written.
- Loki's push API, and Jaeger and Zipkin traces (D39). Loki's push API: **code written, unbuilt**
  (code-only mode, 2026-11-11, D495); Zipkin traces: **code written, unbuilt** (code-only mode,
  2026-11-11, D497).
- Syslog over TLS (RFC 5425) and beyond loopback, the sender's address as a column, and rsyslog
  and syslog-ng in CI (D315). The sender's address: **code written,
  unbuilt** (code-only mode, 2026-11-11, D499); syslog over TLS and beyond loopback: **code written, unbuilt**
  (code-only mode, 2026-11-12, D783). Left: rsyslog and syslog-ng in CI.
- LogQL and TraceQL, with the Loki and Tempo query HTTP APIs Grafana uses (D39). LogQL log queries with Loki's
  query and label APIs: **code written, unbuilt** (code-only mode, 2026-11-11, D496).
- Telegraf and Prometheus in CI, and benchmarks against InfluxDB and Prometheus (D220).
- Streaming SQL: event time, watermarks, and tumbling, hopping and session windows (D55). **Code written, unbuilt**
  (code-only mode, 2026-11-12, D766), in `eggdb_streams`.
- Stream-stream joins and late data (D55). **Code written, unbuilt** (code-only mode, 2026-11-12, D767), in
  `eggdb_streams`.
- `MATCH_RECOGNIZE` for complex event processing (D55). The matcher and `eggdb_streams.match_recognize`: **code
  written, unbuilt** (code-only mode, 2026-11-12, D768). Left: the clause in the parser, and `MEASURES`.
- ETL connectors, first scheduled copies from PostgreSQL, MySQL and SQL Server and from CSV and
  Parquet in object storage (D46, D229). Check: a CI copy from PostgreSQL 17, compared by row. Scheduled copies
  (`eggdb_etl`) from PostgreSQL, SQL Server and local CSV and Parquet files: **code written, unbuilt** (code-only
  mode, 2026-11-12, D769); CSV and Parquet objects in S3-compatible storage: **code written, unbuilt** (code-only
  mode, 2026-11-12, D913); MySQL: **code written, unbuilt** (code-only mode, 2026-11-12, D914).
- Transformations in SQL and WebAssembly, first SQL models with `ref()` dependencies, tests and
  lineage in `eggdb_catalog` (D46, D244). Check: a model graph run in dependency order. SQL models (`eggdb_pipelines`): **code written, unbuilt** (code-only mode, 2026-11-12, D750).
- Pipeline orchestration on the job scheduler, first dependencies, retries with backoff and
  backfills (D46, D200). Check: a test that fails a step and sees it retried. **Code written, unbuilt** (code-only mode, 2026-11-12, D750), in `eggdb_pipelines`.
- FIX and EDIFACT dictionaries, HL7 v2 message building and ACKs, and FHIR validation (D50, D243). HL7
  building and ACKs, FIX building and name dictionaries: **code written, unbuilt** (code-only mode,
  2026-11-11, D445). FHIR structural validation (`fhir_validate`): **code written, unbuilt** (code-only mode,
  2026-11-12, D834). Left: full dictionaries, and FHIR validation against StructureDefinitions and profiles.
- ISO 20022 and DICOM (D50). DICOM files as DICOM's JSON model (`dicom_parse`): **code written, unbuilt** (code-only
  mode, 2026-11-12, D698); ISO 20022 messages as JSON (`iso20022_parse`): **code written, unbuilt** (code-only mode,
  2026-11-12, D699). Left: schema validation.
- IoT models: W3C Web of Things, and DTDL after D58's legal check (D57). Web of Things (`eggdb_iot`): **code written,
  unbuilt** (code-only mode, 2026-11-12, D774). Left: DTDL.
- OPC UA, Modbus and Sparkplug ingest (D57). Sparkplug B payloads decoded in SQL (`sparkplug_decode`): **code written,
  unbuilt** (code-only mode, 2026-11-12, D701). Modbus frames decoded in SQL (`modbus_decode`): **code written,
  unbuilt** (code-only mode, 2026-11-12, D775). OPC UA binary Variants and DataValues decoded in SQL (`opcua_decode`):
  **code written, unbuilt** (code-only mode, 2026-11-12, D833). Left: ingest as messages arrive, and an OPC UA client
  (secure channel, sessions, subscriptions).
- Email stores with IMAP, JMAP and SMTP (D56). The store and an SMTP receiver (`eggdb-mail`, `--smtp-listen`): **code
  written, unbuilt** (code-only mode, 2026-11-12, D892); IMAP (`--imap-listen`): **code written, unbuilt** (code-only
  mode, 2026-11-12, D893); JMAP (`--jmap-listen`): **code written, unbuilt** (code-only mode, 2026-11-12, D894); MIME
  parts and attachments: **code written, unbuilt** (code-only mode, 2026-11-12, D902); IMAP `IDLE`: **code written,
  unbuilt** (code-only mode, 2026-11-12, D903); JMAP uploads and `Email/import`: **code written, unbuilt** (code-only
  mode, 2026-11-12, D904). Left: JMAP submission and push, and TLS.
- CalDAV, CardDAV and WebDAV (D56). iCalendar and vCard read as jCal and jCard (`icalendar_parse`,
  `vcard_parse`): **code written, unbuilt** (code-only mode, 2026-11-12, D702); the WebDAV, CalDAV and CardDAV server
  (`eggdb-dav`, `--dav-listen`): **code written, unbuilt** (code-only mode, 2026-11-12, D895); `time-range` filters
  over recurring events: **code written, unbuilt** (code-only mode, 2026-11-12, D901). Left: `expand` in
  `calendar-data`, time zones by `TZID`, scheduling and ACLs.
- Digital asset management: EXIF and XMP metadata, and renditions with our own royalty-free
  codecs (D56). EXIF and XMP (`image_metadata`): **code written, unbuilt** (code-only mode, 2026-11-12, D731). Left:
  renditions.

### M17 Every platform
- EggOS as a target, with its own backend in the platform layer (D8).
- The B-tree, page cache, log and transactions without `std`, on an embedded platform backend
  with a fixed-frame cache (D223).
- Wear levelling, the header slots on flash, and power-loss tests on raw flash (D41, D223).
- A QEMU Cortex-M run in CI (D223).
- A published memory floor and size budgets per microcontroller profile (D41, D230).
- A fixed-memory mode that never allocates beyond a set budget (D41).
- Write-amplification and flash-wear budgets, and battery-aware background work (D41).
- Correct behavior under iOS background limits and Android Doze (D41).
- The browser build in a worker with OPFS synchronous access handles, a database shared between
  tabs and a real `poll_oneoff` sleep (D221).
- `wasm32-unknown-unknown` without WASI, and size budgets for the module (D6, D221).
- An IndexedDB-compatible API in the browser build (D48).
- Android emulator runs, a Gradle module and Android Archive, and the 32-bit ABIs in CI (D222).
- iOS Simulator runs, and a Swift package once a release publishes the XCFramework (D222).
- Prepared statements and transactions in the mobile bindings, benchmarked against SQLite's
  Android and iOS bindings (D17, D222).
- The rest of the crates tested on i686, armv7 and s390x under QEMU (PR #86).
- Builds and tests on riscv64, powerpc64le and loongarch64 under QEMU (D8). Check: the
  emulated-CPU CI job on `main`.
- The container image's size and pull time measured and kept from growing (D14, D224).

### M18 The other ways of scaling
- Leaderless quorums over the network, wired under `eggdb serve` (D225).
- Merkle trees for anti-entropy (D225). **Code written, unbuilt** (code-only mode, 2026-11-11, D443).
- CRDT types (counters, sets) as resolution policies (D18, D225). **Code written, unbuilt** (code-only mode,
  2026-11-11, D442).
- Multi-primary SQL rows with conflict rules per table: last writer wins, application rules or
  CRDTs (D18, D225).
- Tombstones collected once every replica has seen them, and membership changes by epoch (D225). **Code
  written, unbuilt** (code-only mode, 2026-11-11, D444).
- Page-level remote storage: the page cache reads missing pages from object storage, and the log
  ships segment by segment (D18, D226).
- `https://` object storage endpoints, multipart copies of large archives, and uploads without a
  staging file (D226). Check: a simulator test of restoring under failures. `https://` endpoints, through the
  shared HTTP client's TLS: **code written, unbuilt** (code-only mode, 2026-11-12, D920); multipart uploads of large
  archives: **code written, unbuilt** (code-only mode, 2026-11-12, D921). Left: uploads without a staging file, and
  the simulator test.
- Stateless compute nodes over shared storage, scaling apart from storage (D18).
- Serverless: `eggdb serve` archives and exits when idle, and starts again through
  `--restore-from` (D226); research decides what wakes it (D18). Check: a timed cold start.
- Tiered storage: hot, warm and cold pages by age across memory, disk and object storage (D18,
  D227).
- An edge server on an edge copy, refreshed as its manifest grows, with cache limits and eviction
  (D227).
- Placement rules per table, for tiers, edges and regions (D18, D227).
- Data pinned to regions, and residency policies per row (D18, D53).
- Offline-first sync over HTTP with authentication per user, as the client sync API apps use (D37,
  D228).
- Sync of SQL tables through change capture (D210, D228).
- Sync log compaction, partial sync, per-field merges and CRDTs, and end-to-end encryption (D228).
- CouchDB's replication protocol (D48).
- Foreign tables with declared columns and filter pushdown (D29, D229). `CREATE SERVER` and `CREATE FOREIGN TABLE`
  with declared columns, as views over `eggdb_foreign`: **code written, unbuilt** (code-only mode, 2026-11-12, D916).
  `IMPORT FOREIGN SCHEMA`: **code written, unbuilt** (code-only mode, 2026-11-12, D917). Left: filter pushdown.
- Foreign sources: MySQL, SQL Server, HTTP, S3 and Parquet over object storage (D29, D229). SQL Server: **code
  written, unbuilt** (code-only mode, 2026-11-12, D769); CSV and Parquet objects in S3-compatible storage: **code
  written, unbuilt** (code-only mode, 2026-11-12, D913); MySQL: **code written, unbuilt** (code-only mode, 2026-11-12,
  D914); `http://` CSV, Parquet and JSON: **code written, unbuilt** (code-only mode, 2026-11-12, D915).
- Credentials per source, permission per role, and a timeout and row limit per call (D229). **Code written, unbuilt**
  (code-only mode, 2026-11-12, D770).
- Joins across servers planned as one distributed query, and federated SQL across EggDB clusters
  in Trino's style (D18, D46, D229).
- A fleet control plane: inventory, upgrades in canary waves and placement by hardware class
  (D58).
- Tenant lifecycle APIs, and databases per user at millions of tenants (D58).

### Needs the owner (money or accounts, D58)
- A Tableau connector, and Tableau in the `compatibility` job (D46, D239).
- A Looker data source (D46).
- A Fivetran source and destination (D46).
- Dataverse's Web API and SharePoint lists as sources and targets (D49).
- An Excel data source plugin, which needs paid Excel to test (D46, D239).
- ECS, Azure Kubernetes Service and private-networking variants of the cloud templates, and a
  managed-disk option on Azure, which need paid cloud accounts to test (D224).
- ASC X12 support, once its paid license is bought (D50, D58).
- Dedicated, pinned benchmark hardware with published descriptions (D44, D58).
- Community: GitHub Discussions, a release blog and roadmap voting (D44).
