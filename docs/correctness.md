# Correctness statement

What EggDB v0.2.0 promises about your data, how each promise is tested, and what is not tested
yet. Correct and safe comes before fast (D9), so this page says plainly where the evidence
stops. The details of every test are in [testing.md](testing.md); this page is the summary.

## The promises
- **A commit is atomic.** After any crash, a database holds every change of a commit or none of
  them, never part of one, and never a mix of two commits.
- **A commit is durable once it returns,** under `Durability::Full` (the default and what the
  `eggdb` command uses): a crash of the process or a power loss after that loses nothing.
  `Normal` can lose the latest commits on a power loss but never corrupts the file, and `Off`
  promises nothing after a power loss ([embedded.md](embedded.md), "Durability").
- **Damage is reported, not served.** Every page and every log record carries a CRC-32C
  checksum that is checked when it is read; a page that fails it is an error (`Corrupt`, exit
  4), never data. A checksum catches every damage of a few bits and all but about one in four
  billion of the rest. `eggdb check` and `Database::check` read every page and account for every page of the
  file.
- **Hostile input never crashes or hangs EggDB.** A damaged or crafted file, a hostile export
  and any command-line argument produce an error, not a panic or a hang, and a hostile file
  never makes EggDB allocate more than twice the file's size plus 16 MiB at once.
- **Files stay readable.** Every later release opens the files v0.2.0 writes (D20, D96).

## How each one is tested
| What | How | Where |
|------|-----|-------|
| Crash at any moment | A seeded workload through the page cache, the log and the file, in each durability mode, fails every one of its I/O operations in turn and ends in a reopen, a power loss that drops unsynced writes, or one that tears them into 512-byte pieces; the reopened file must hold exactly one commit, no older than promised, with every page accounted for. A sample of those points also crashes during recovery, and writes after it. | [testing.md](testing.md), "The whole stack" (D85) |
| Transactions under crashes | The same sweep through write and read transactions: atomic commits, rollbacks, snapshot readers that keep their view, and a handle that refuses work after a failed sync instead of retrying it as a success. | testing.md (D86) |
| Injected faults | The simulated file system fails any operation on request; every layer (page file, log, checkpoints, page cache, B+tree) has its own crash and fault tests. | testing.md, "Each layer" |
| Behaves like a map | Seeded random transactions, rollbacks, large values, scans, checks, reopens, power losses and faults compared with Rust's `BTreeMap`, through the storage engine and through the embedded API on real files. | testing.md, "Against a model" (D89) |
| Hostile files | A seeded fuzzer damages five kinds of valid files 22 ways (bit flips, cuts, swapped pages, rewritten fields with valid checksums, damaged logs), opens each read-only and for writing, reads, scans, checks and writes; 800 cases in every CI run. | testing.md, "Hostile files" (D89) |
| Hostile exports and arguments | A fuzzer for the `eggdb import` reader (4,000 cases per run), mutated exports imported through the command's entry point, random arguments to the real binary, and a fuzzed terminal-input decoder for `eggdb ui`. | testing.md, "Hostile exports" (D93); D90, D91 |
| Subqueries, set operations and outer joins | A seeded differential test builds random tables with NULLs, twice (with and without indexes), and requires `EXISTS`, `IN`, `= ANY`, a join and a `DISTINCT` join to agree; `NOT EXISTS` with an anti-join; `NOT IN` (three-valued, NULLs on either side) with `NOT (= ANY)` and a reference written in the test; correlated `NOT IN`, `< ANY`, `> ALL`, `<> ALL`, a scalar `count` subquery against a grouped left join; `UNION [ALL]`, `INTERSECT [ALL]`, `EXCEPT [ALL]` against a multiset reference; `FULL` joins against a reference and a `RIGHT JOIN ... UNION ALL ... NOT EXISTS` form. Bounds: 32 levels of subqueries and derived tables, 1,024 `SELECT`s, a 50,000-item `IN` list and 20,000-row subqueries in linear time, recursion stopped after its step limit, a doubling recursive string stopped by the memory limit; a fuzzer of subquery, `WITH`, set-operation and join fragments in every dialect; dialect spellings (`MINUS`, `FULL JOIN`, `ANY`, `INTERSECT ALL`, parentheses) checked to stay in their dialects; column types over the PostgreSQL protocol. | `crates/eggdb-sql/src/subquery_tests.rs`, `crates/eggdb-pgwire/tests/protocol.rs` (D112) |
| Schemas, views and comments | A seeded test creates eight kinds of views (filters, inner and outer joins, groups, `IN` subqueries, `UNION`, `DISTINCT`, `*`) over random tables, with and without indexes, and requires each to answer as its query written inline, also joined; view chains of 31 and 32 levels run and a 33rd is refused, a fan-out of views stops at 1,024 `SELECT`s, self-reference directly and through others is refused; a killed `eggdb sql` leaves every schema with both its table and its view or neither; damaged view, schema, comment and dependency records are errors, never panics; version 7 and 8 records cut at every length and damaged at every byte; a fuzzer of schema, view and comment fragments in all four dialects; the `pg_dump` forms as one psql-style script over the PostgreSQL protocol. | `crates/eggdb-sql/src/schema_tests.rs`, `crates/eggdb-sql/src/encoding/catalog.rs`, `crates/eggdb/tests/sql.rs`, `crates/eggdb-pgwire/tests/protocol.rs` (D116) |
| Secondary indexes | Every index is checked against its rows (`Session::check_indexes`); a seeded differential test runs random queries and changes on tables with and without three indexes and requires the same results; a killed `eggdb sql` never leaves an index that disagrees with its rows; the store counts entries read to prove an indexed lookup reads only its range; a fuzzer of index statements in every dialect and of damaged table definitions and index entries; the version 3 table definition pinned byte for byte, cut at every length and damaged in every marker; the same differential test over `numeric`, date and time stamp keys (D111) and over `interval`, `character(n)`, `bytea`, `uuid`, `time` and `timetz` keys (D113), whose equal values (`'1 mon'` and `'30 days'`, `'ab'` and `'ab  '`) must share one key; version 4 and 5 definitions, rows and keys pinned, cut and fuzzed. | `crates/eggdb-sql/src/index_tests.rs`, `crates/eggdb-sql/src/encoding.rs`, `crates/eggdb/tests/sql.rs` (D109) |
| `check` finds damage | Crafted pages and chains that must fail `check`, and a check after every case of the tests above. | testing.md |
| Files from v0.2.0 stay readable | Two sample files written by v0.2.0, one checkpointed and one with three commits still in its log, and a v0.2.0 export, are committed and pinned by their bytes. CI opens, reads, scans, checks, replays, writes to and imports them on every change. | `crates/eggdb/tests/sample_files.rs`, testing.md, "Files from released versions" (D96) |
| The released binaries work | A smoke test runs every command, an export and import round trip, the sample files and a scripted `eggdb ui` session in a pseudo-terminal against each release binary on its own platform. | `scripts/smoke-test.sh`, testing.md "The release smoke test" (D96) |
| The Oracle dialect | 254 cases from Oracle's SQL Language Reference (23ai), never Oracle's software, with Oracle's error numbers: `DUAL`, upper-case names on tables the other dialects share, the empty string as NULL, `ROWNUM` and `FETCH FIRST`, identity columns, the dictionary views, implicit transactions and DDL commits, and Oracle's functions and format models, `NUMBER(p,s)`, `DATE` and `TIMESTAMP`, subqueries, subquery factoring, set operators with `MINUS` and outer joins, sequences with `NEXTVAL` and `CURRVAL`, constraints and `ALTER TABLE`; 5 marked as known differences (sql.md); a seeded fuzzer of 25,000 hostile statements built from Oracle fragments (bind variables, `q'...'` literals, `ROWNUM` bounds, huge `LPAD`/`RPAD`/`REPLACE` results, overflowing arithmetic); a check that Oracle's functions stay in Oracle's dialect and the others' out of it; `ORA-` errors through `eggdb sql`. | `crates/eggdb-sql/correctness/oracle.test`, `crates/eggdb-sql/src/tests.rs`, `crates/eggdb/tests/sql.rs` (D108) |
| The SQLite dialect | 234 cases from sqlite.org's documentation of SQLite 3.46 (datatype3, lang_*, lang_corefunc, the PRAGMA and result-code pages, compound selects, subqueries and `WITH`, foreign keys behind `PRAGMA foreign_keys`, `CHECK` and the limited `ALTER TABLE`), never SQLite's source, with SQLite's extended result codes, 4 marked as known differences (sql.md); a seeded fuzzer of 25,000 hostile statements built from SQLite fragments (parameters, bracket identifiers, PRAGMAs, upserts, `printf` formats); parameter numbering; SQLite-style errors through `eggdb sql`. | `crates/eggdb-sql/correctness/sqlite.test`, `crates/eggdb-sql/src/tests.rs`, `crates/eggdb/tests/sql.rs` (D107) |
| The SQL Server dialect and protocol | 78 cases from the Transact-SQL reference's documented behaviour (`TOP`, `IDENTITY` with inline and table-level primary keys, `[brackets]` and folded names, `N'...'`, Transact-SQL's operators only, `GETDATE()` and its kin, `@@` functions, nested `BEGIN TRAN`, `SET` options, `USE`, batches without semicolons) with SQL Server's error numbers after the SQLSTATE; unit tests of the lexer, the statement boundaries, the batch rules and the error translation (no PostgreSQL wording reaches a SQL Server client); every TDS parser (PRELOGIN, LOGIN7 with the password de-obfuscated, SQL batches with ALL_HEADERS), truncated and inconsistent messages refused; the PRELOGIN and LOGIN7 examples of [MS-TDS] section 4; a client written in the tests speaks TDS over a real loopback socket: encryption refused for clients that need it and TLS closed, TDS 7.4 only, wrong passwords, unknown users and databases, integrated logins refused, packet sizes from 512 to 32,767 with results and batches across packets, every TDS type and NVARCHAR sizing in UTF-16 units, statement-terminating and batch-ending errors, transactions and their ENVCHANGEs, `NOCOUNT`, attention, RPC refused, the connection limit and a graceful stop; a seeded fuzzer (20,000 mutated messages through the parsers, 200 over sockets before and after login); and in CI, Microsoft's go-sqlcmd against the real binary. | `crates/eggdb-sql/correctness/sqlserver.test`, `crates/eggdb-tdswire/tests/`, `scripts/smoke-test.sh` (D122) |
| The MySQL dialect and protocol | 223 cases from the MySQL 8.0 Reference Manual's documented behaviour (with `DECIMAL`, `DATE`, `DATETIME`, `TIMESTAMP`, `JSON`, `TIME` and `BLOB`, subqueries, `WITH` and set operations, foreign keys, `CHECK` and `ALTER TABLE`), with MySQL's error numbers and SQLSTATEs, 4 marked as known differences (sql.md); SHA-1 against FIPS 180's vectors, block boundaries and split updates, and both authentication methods against known-answer vectors; every packet parser, truncated and inconsistent payloads refused; a client written in the tests speaks the protocol over a real loopback socket: the handshake's capabilities, both authentication methods and the switch between them, wrong passwords, unknown users and databases, text queries with several statements and their flags, transactions' status, prepared statements with binary parameters, rows (packed dates) and long data, unknown commands, the connection limit and a graceful stop; a seeded fuzzer (20,000 mutated payloads through the parsers and password checks, 80 over sockets before and after authentication), oversized packets, truncated handshakes and wrong sequence ids; and in CI, the `mysql` client and the PyMySQL driver (with `DECIMAL`, `DATE`, `DATETIME` and `JSON` values) against the real binary. | `crates/eggdb-sql/correctness/mysql.test`, `crates/eggdb-mysqlwire/tests/`, `crates/eggdb-crypto/tests/vectors.rs`, `scripts/smoke-test.sh` (D106) |
| The PostgreSQL protocol | The cryptography against its published vectors (FIPS 180-4 and NIST for SHA-256, RFC 4231 for HMAC, RFC 7914 for PBKDF2, RFC 7677's SCRAM exchange); every message codec, truncated and inconsistent messages refused; a client written in the tests speaks the protocol over a real loopback socket: startup and encryption requests, SCRAM with right and wrong passwords and unknown users, simple and extended queries with text and binary parameters (numeric in base 10,000, dates, time stamps, `jsonb`, `time`, `timetz`, `interval`, `bytea`, `uuid`, `bpchar`) and results, errors with SQLSTATE and position, failed blocks until `ROLLBACK`, row limits, two connections and a waiting writer, the connection limit and the cap on logins in progress, the authentication timeout and a graceful stop (also against clients trickling a byte every 100 ms), finished portals that never rerun, and log lines that client-supplied names cannot break; a seeded fuzzer of mutated messages (20,000 cases per run through the parsers and SCRAM, 80 over sockets before and after authentication); `eggdb serve` as a process (password sources, loopback refusal, `SIGTERM`); and in CI, psql (create, insert, join, errors, a wrong password) and the pg8000 driver (parameterized queries, `numeric`, `date`, `timestamp`, `timestamptz`, `jsonb`, `bytea`, `uuid`, `time`, `interval` and `char(n)` round trips) against the real binary. | `crates/eggdb-crypto/tests/vectors.rs`, `crates/eggdb-pgwire/tests/`, `crates/eggdb/tests/serve.rs`, `scripts/smoke-test.sh`, testing.md "Hostile protocol messages" (D103) |
| SQL behaves as PostgreSQL documents | A table-driven suite of 499 cases written from the PostgreSQL manual's documented behaviour (never PostgreSQL's source), run through the session API that `eggdb sql`, `eggdb shell` and `eggdb serve` use: integer ranges and overflow (`22003`), division by zero (`22012`), floating point with NaN ordering and infinities, casts and their errors (`22P02`), NULL and three-valued logic, comparisons, string operators and functions, `ORDER BY` with `NULLS FIRST`/`LAST`, `LIMIT`/`OFFSET` and their errors, `GROUP BY`/`HAVING` and grouping errors (`42803`), inner, left and cross joins, `UPDATE`/`DELETE` row counts and `RETURNING`, transaction blocks (a failed statement aborts the block with `25P02` until `ROLLBACK`; `COMMIT` of a failed block rolls back), and DDL errors (`42P01`, `42703`, `42P07`, `23505`, `23502`), exact `numeric` with its scales and rounding, `serial` and identity columns, dates and time stamps with their ranges and functions, and `json`/`jsonb` operators, subqueries (correlated and not, `IN`/`NOT IN` with NULLs, `EXISTS`, `ANY`/`ALL`, derived tables), `WITH` and `WITH RECURSIVE`, set operations with `ALL`, `RIGHT` and `FULL` joins, `CASE`, `coalesce`, `nullif`, `greatest` and `least`, `time`, `timetz` and `interval` input, output and arithmetic, `bytea`, `uuid` and `character(n)`, check and foreign-key constraints with their referential actions, `ALTER TABLE` and sequences with their functions and limits, schemas and `search_path`, views (over joins, groups, set operations and other views, with their `2BP01`, `42P16` and `42P17` refusals) and `COMMENT ON`. 6 cases EggDB does not match yet are marked and listed in [sql.md](sql.md) "Known differences"; the suite fails when one of them starts to match, so the list stays true. | `crates/eggdb-sql/correctness/postgresql.test`, testing.md "SQL against the manual" (D105) |
| One writer, locked files | Two processes opening one file; `get` and `put` refused with exit 5 while another program has the file open for writing, and `get` working beside a reader; locks released when a handle closes, even while a child process starts. | `crates/eggdb/tests/lock_across_processes.rs`, `cli_busy.rs` (D95) |

## Where it runs in CI
- **Every test** runs on Linux x86-64 (glibc and musl), macOS on Apple silicon and Windows
  x86-64, on every pull request.
- **The release smoke test** runs on those three and on Linux AArch64 (glibc and musl), each
  against the release binary built for it.
- **Built, not run:** macOS x86-64 and Windows AArch64 (the runners can't execute them), so
  only their build is checked.
- **Compiled and linted, never run:** FreeBSD, NetBSD, illumos, Solaris, Android, iOS and
  WebAssembly (WASI).
- **`eggdb ui` in a real terminal:** the smoke test drives it on Linux and macOS through
  `script(1)`. Windows has no such tool on its runners, so there it is tested through the UI's
  fake terminal and the refusal without a terminal only.

## What is not tested, or not promised
- **Real power loss on real hardware.** Crashes and power losses are simulated: unsynced writes
  are dropped or torn into 512-byte pieces. EggDB relies on the operating system's sync
  (`fsync`, `F_FULLFSYNC` on Apple's systems, `FlushFileBuffers` on Windows through Rust's
  standard library) and on the disk honouring it. No test pulls the plug on a machine, and a
  disk or a virtual machine that acknowledges writes it hasn't stored can lose commits.
- **A real process killed in the middle of a commit.** That is covered by the simulator, which
  stops at every I/O operation; no test kills a running `eggdb` process during a write.
- **The PostgreSQL protocol's limits.** No TLS, so `eggdb serve` listens on loopback only
  (D102); no query cancellation, `COPY`, `LISTEN`/`NOTIFY` or catalog queries, and psql's `\d`
  commands fail with an error. A multi-statement `Query` and an extended-protocol pipeline do
  not run in one implicit transaction: outside `BEGIN`, each statement commits on its own. Only
  psql and pg8000 are run in CI; other drivers and GUI tools are not tested yet. The SCRAM code
  had no external security audit; D35's second, security-focused review is done by a review
  agent, not a person. Timing side channels are limited to constant-time comparisons of the
  proof and nonce; PBKDF2 and HMAC are not hardened against cache-timing attacks.
- **SQL beyond the suite.** The suite covers the statements and types EggDB has; it is not
  PostgreSQL's own regression suite, which we cannot use (D1), and it is checked against the
  manual, not against a running PostgreSQL. Dates and times, `numeric`, JSON, subqueries,
  window functions and the catalog are not in it because EggDB does not have them yet. The
  other dialects get their suites with items 22 to 25 and 23a (MySQL's, SQLite's, Oracle's and SQL Server's exist). Next for the suite: its expected rows
  assume byte-order text comparison (PostgreSQL's `C` collation; a database created with
  another collation sorts `'Z' < 'a'` differently), and a few `ORDER BY` cases with ties expect
  the primary-key order EggDB gives, which PostgreSQL does not promise; next: tie-breaking
  columns in those cases, and collation-dependent cases once collations exist (D30).
- **More than one writer.** v0.2.0 has one writer at a time by design (D76); concurrent writers
  come with M3.
- **`Off` after a power loss**, which promises nothing.
- **No outside audit.** No Jepsen-style test or independent review outside the project has
  checked EggDB yet; every test here is our own.
- **Damage to synced bytes** is detected by checksums, not repaired: EggDB has no repair yet, so
  restore from a backup (`eggdb export`, or a copy of the closed file).
- **Network and shared file systems** (NFS, SMB) and their locks.
- **Large files and long runs.** The tests use small files; the largest value written end to end
  is 20 MiB, against a limit of 1 GiB, and there is no soak test of days or of databases of many
  gigabytes yet.
- **CPUs and systems outside CI**: 32-bit and big-endian CPUs are not built or run.
- **No network listener exists in v0.2.0**, so nothing here covers one. When the first one
  lands, it listens on loopback only and without TLS until EggDB's own TLS is built (D22,
  D23), and this page will say how it is tested.

A problem with your data? Keep the file unchanged, run `eggdb check` on a copy, and open an
issue at https://github.com/eggspot/EggDB/issues with its output.
