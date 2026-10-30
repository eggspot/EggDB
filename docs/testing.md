# What is tested

This page lists what the storage engine's tests prove about crashes and I/O faults, and what they
don't. It feeds the correctness statement, [correctness.md](correctness.md). Every test runs on the
simulated file system of `eggdb-platform` (D9), which numbers every file operation, can fail any
one of them, and can simulate a power loss that drops every unsynced write or tears unsynced
writes into 512-byte pieces that survive or not, chosen by a seed.

## The whole stack: the crash sweep (D85)
`crates/eggdb-storage/src/crash_sweep_tests.rs` runs a seeded B+tree workload through the page
cache, the log and the page file: puts, overwrites, values at the leaf's limit, values on
overflow pages (one to four pages, replaced and deleted like the rest) and long keys, a growing phase
that splits the root and a shrinking phase that merges pages and collapses it again, rolled-back
transactions, explicit checkpoints and syncs, and automatic checkpoints often enough that the log
wraps through its recycled segments and gives extra segments to the free list. It runs once
with each durability mode (`Full`, `Normal`, `Off`).

Each I/O operation of the run is failed in turn, and the run then ends in one of these ways:
- a reopen without a power loss (the fault was an I/O error, and the process went on or died);
- a power loss that drops every unsynced write;
- a power loss that tears unsynced writes (two seeds in CI, four in the exhaustive run).

Then the file is opened again, which runs recovery, and the test checks:
- the tree passes `BTree::check`, every chain of overflow pages included, and a forward and a
  reverse scan return exactly the contents of one commit, never a mix of two;
- every page of the file is accounted for, exactly once, as a page of the tree or of a value, a
  free page, a free-list page or a log page, and no page is leaked;
- that commit is at most the last one attempted, and at least every commit acknowledged before the
  fault under `Full`, or in any mode when there was no power loss, and every commit acknowledged
  before the last successful sync under `Normal`;
- after a failed commit, checkpoint or sync, the open handle refuses the next commit with
  `NeedsReopen` and the next sync with an error: a failed sync is never retried as a success;
- every injected fault is reported as an error, and nothing panics.

Two more checks run from a sample of those crash points:
- **A crash during recovery.** Each operation of the recovery is failed in turn, followed by a torn
  power loss; the next open must recover exactly the same commit.
- **Writing after recovery.** More transactions, a checkpoint and a commit run on the recovered
  file, then a torn power loss; the last commit must be there.

**Transactions (D86).** The same workloads also run through `Store` in `Full` and `Normal`: each
commit is a write transaction, each rolled-back step a dropped one, and a read transaction begun
before every commit must still read that commit's predecessor after it. Every I/O operation is
failed in turn with the rotating endings, and the reopened file must hold exactly one commit, with
the same floors as above; a failed commit, checkpoint or sync must leave the store refusing a new
writer with `NeedsReopen`. The commit stamps run from a simulated clock set before 2026, so each
stamp is its commit's number and the recovered stamp names the commit exactly.

CI gives every operation one ending, rotating through the four, and runs in about 30 seconds of a
debug build. Setting `EGGDB_CRASH_SWEEP=1` gives every operation every ending; the counts of the
last exhaustive run are in D88 (27,048 crash points and 43,992 crashes during recovery, about
195 s).

## Large values and hostile chains (D88)
`overflow_tests.rs` checks values at every size boundary (empty, the leaf's limit and one byte
past it, one byte short of a page of overflow, whole pages, many pages) with 4 KiB and 64 KiB
pages through a reopen; replacing values small to large and back, deletes and rollbacks, with
every page accounted for; a reader that keeps reading old large values while the writer
replaces, deletes and reuses their pages; scans in both directions over ranges mixing large and
small values, and the cursor releasing a buffer grown past a megabyte; rewriting 100 KiB values
eighty times without the file growing; a value too large for the cache refused with the
transaction left open; and hand-made damage: a flipped byte, a chain cut short, lengthened, bent
into a cycle or leading to another value or page type, references with impossible lengths and
pages, two references sharing a chain, a leaked page, and a page with two uses.

`overflow_mutation_tests.rs` damages a file holding sixteen large values in 2,400 seeded ways,
about 10 seconds of a debug build: bit flips, every chain field and both reference fields
rewritten with interesting values (zero, the file's end, other chains, tree pages, the largest
numbers) with checksums fixed, swapped chain pages, other page types and layouts, inline values
flagged as references, and truncated files. Each case must open, read, scan, check and, for a
quarter of them, write and commit without a panic, a hang or an out-of-memory error, a length
past the file must be refused before anything is allocated, and a check that passes must mean
every read works. Its mutations of references and chains are not repeated by the hostile-file fuzzer below.

## Against a model: random use of the store and the API (D89)
`crates/eggdb-storage/src/model_tests.rs` drives a `Store` on the simulator through seeded random
steps and compares every read with a `BTreeMap` of what was committed:
- write transactions of one to forty changes: puts of empty, small, leaf-limit and overflow
  values (one to sixteen pages), puts over existing keys, deletes of present and absent keys,
  keys from empty to the longest allowed with bytes 0x00 and 0xFF, keys and values over the
  limits refused with the transaction left open, and gets and range scans inside the
  transaction, which see its own changes;
- commits, explicit rollbacks, dropped transactions, and transactions that fill the cache and
  must then refuse every call while the store takes a new writer;
- a read transaction begun before a writer, which must still read its snapshot after the
  commit;
- gets, full scans in both orders, range scans with every kind of bound and prefix scans in
  both orders, and `check`, whose counts of entries, key and value bytes and overflow values
  must match the model, with no leaked page;
- checkpoints, syncs, reopens, read-only opens that must refuse a writer, power losses that drop
  or tear unsynced writes, and I/O faults injected into commits, checkpoints and syncs, some
  followed by a power loss and some by a fault during recovery.

After a reopen the store must hold exactly one commit at or after the last one that was durable
(every acknowledged commit under `Full`, or with no power loss; the last synced one under
`Normal`), or the commit whose failure was reported. A third of the seeds use a 40-page cache so
transactions run out of frames. CI runs 12 seeds of 250 steps (about 2 s of a debug build);
`EGGDB_MODEL_TEST=1` runs 256 seeds of 3,000 steps.

`crates/eggdb/tests/model.rs` does the same through the embedded API on real files: closure and
one-step transactions, rollbacks by an error from the closure, every range form, prefixes,
scans that take entries from both ends at random, `check` and `stats`, closes, drops and
read-only reopens.

## Hostile files (D89)
`crates/eggdb-storage/tests/hostile_files.rs` is a seeded fuzzer with no outside tools. It builds
five valid files (checkpointed; with a log to replay; with a log that wrapped through its
recycled segments and gave segments to the free list; cut off by a torn power loss; with 16 KiB
pages) and damages each case with one to three of 22 mutations:
- bit flips anywhere, in a leaf or in the log; truncation anywhere, at a page boundary or one
  byte short; garbage or zeros appended; a page or a range zeroed;
- pages swapped or copied over each other, raw or with their checksums fixed for their new
  place;
- a superblock field (version, page size, generation, features, page count, free list, catalog
  root, checkpoint position, log head and sequence, segment length, recycled segments,
  reservation) set to an interesting number with the checksum fixed, in the newest slot or in
  both, and raw header bytes;
- bytes, 16-bit and 64-bit fields of inner, leaf, overflow and free-list pages, their headers
  (type, layout, flags, `lsn`) and the free list's next page, count and entries, rewritten with
  valid page checksums;
- log bytes and log blocks copied over each other.

Each case opens the file read-only and then for writing, reads every key, scans both ways, by a
prefix and by a range, runs `check`, writes a small and a large value, deletes a key, commits,
checkpoints and reopens. Errors are expected; the test fails on a panic, on a case running more
than a minute, on a scan returning more entries than the file can hold, on one allocation larger
than twice the file plus 16 MiB (a global allocator records the largest per thread), and on a
file that passes `check` but fails a read, or fails `check` or a reopen after a commit. Cases run
on every core, and a failure names its seed and case. CI runs 800 cases (about 7 s of a debug
build); `EGGDB_FUZZ_CASES` and `EGGDB_FUZZ_SEED` run longer campaigns, and the counts of the last
one are in D89.

## Hostile exports (D93)
`eggdb import` reads files from anywhere, so its reader (`crates/eggdb/src/cli/jsonl.rs`) has a
seeded fuzzer of its own in `jsonl/tests.rs`: it mutates valid exports (bit flips, bytes
replaced, cut or deleted, JSON tokens and noise inserted, lines moved) or makes random bytes,
feeds them through a reader that returns one to nine bytes per call and `Interrupted` errors,
and requires either a refusal with a line number or entries within the key and value limits,
never a panic, an input error or a case over 5 s. CI runs 4,000 cases (under a second);
`EGGDB_FUZZ_CASES` and `EGGDB_FUZZ_SEED` run longer campaigns. The command itself is fuzzed in
`cli/transfer_tests.rs`: mutated exports imported into new and existing files must exit 0 or 3,
leave no `.importing` file, keep `check` passing, and change nothing when the message says
nothing was imported.

## Hostile protocol messages (D103)
`eggdb serve` reads bytes from any program on the machine, so the PostgreSQL protocol has a
seeded fuzzer of its own in `crates/eggdb-pgwire/tests/hostile_messages.rs`: it mutates valid
startup packets, `Query`, `Parse`, `Bind`, `Describe`, `Execute`, `Close`, `Sync` and SASL
messages (bit flips, cuts, noise, rewritten length and count fields, random message types) and
feeds them to every parser and to the SCRAM exchange, which must return an error or a value and
never panic (20,000 cases in CI); it then sends the same mutations over real sockets before and
after authentication (80 cases) and requires a well-behaved client to connect and query
afterwards. Oversized length fields are refused without allocating them. `EGGDB_FUZZ_CASES` and
`EGGDB_FUZZ_SEED` run longer campaigns. The message codecs, the SCRAM exchange (RFC 7677's
example and malformed variants) and the cryptography's published vectors have unit tests of
their own, and `crates/eggdb-pgwire/tests/protocol.rs` drives every protocol flow with a client
written in the test.

## Hostile MCP messages (D119)
`eggdb mcp` reads whatever an agent's client sends, so its JSON-RPC reader and dispatch have a
seeded fuzzer in `crates/eggdb-mcp/tests/hostile_messages.rs`: it mutates valid MCP messages
(byte flips, cuts, inserted JSON fragments such as `\u0000`, lone surrogates and huge numbers,
nesting past the 256-level limit, a parameter of every wrong type, a key given twice, batches and
random bytes) and requires every line that is a request to get exactly one answer that is one
line of valid JSON, no answer to notifications and responses, no panic, and the database
unchanged (25,000 cases in CI); a second run sends 2,000 of them through `serve` and counts the
answers in order. Messages over 4 MiB are refused without being buffered, and so are messages holding more than 100,000 JSON values, before they are parsed. `EGGDB_FUZZ_CASES` and
`EGGDB_FUZZ_SEED` run longer campaigns. A third run (D121) starts a server with `--allow-write`
and a client that can be asked, sends 400 destructive `write_sql` calls and answers each
question with mutated, cut, stray or garbled answers, cancels and pings: every line out must be
JSON and the statement must never run. `crates/eggdb-mcp/tests/server.rs` drives every tool and
rule over an in-memory store, `crates/eggdb-mcp/tests/writes.rs` the writes, both confirmation
paths, the wait for an answer and the commits that must never be reported failed, and
`crates/eggdb/tests/mcp.rs` drives the built binary as a child process, checking that every line
on its stdout is a JSON-RPC message.

## Expiring keys and counters (D140)
`crates/eggdb/tests/expiry.rs` runs the API on a `SimClock` (`Options::clock`): a key expires
exactly at its deadline, one microsecond after it is still readable, across a crash (a drop
without `close`) and a reopen, in the default key space and in a collection, in `get`, `get_with`
and scans both ways; a wall clock set back after a commit never brings an expired key back; a
read transaction keeps its time; a read-only open hides expired keys without removing them;
eight threads increment the same counters and the counts come out exact; compare-and-set swaps
only the value it expects; and the 14-byte key limit holds. `check` counts every key the tree
holds, deadline records included, so the tests also see what the commit sweep and
`remove_expired` removed. Seeded fuzzers feed 20,000 hostile inputs each to the time grammar
(`eggdb-ui`'s `time_to_live.rs`, where every printed time must read back) and to the counter and
deadline record readers (`expiry.rs`); the key layout is pinned by test vectors, and
`sample_files.rs` checks that a v0.2.0 file takes expiring keys and then carries the
`EXPIRING_KEYS` bit. The CLI, the shell and the UI scenarios 40 to 45 are in
`cli/expiry_tests.rs` and `eggdb-ui/tests/screens.rs`, and the smoke test runs every new command.

The command model (D143) has four checks. The registry test (`crates/eggdb-commands`) checks
every command's and tool's fields, that every command is a kept name or a noun with core verbs,
that new tool names join their command's words with `_` and shipped ones come from the list, that
every tool's MCP annotations follow from its effect, idempotence and reach, and that every
argument maps onto an option or operand of its CLI twin. `eggdb-mcp` keeps the shipped
`tools/list` generator as an oracle and checks the registry's output against it byte for byte,
with and without `--allow-write`. The parity test (`tests/command_model.rs`) runs `eggdb mcp`
and the `eggdb` commands as real processes on the same files: `kv_get`/`get --json`,
`kv_scan`/`scan --json`, `run_sql` and `write_sql`/`sql --json` return the same values once the
tool's envelope is removed, and the same failure (a missing file, a failed statement) has the same
code and category. From slice 2 (D144) it also runs `list_tables`/`table list`,
`describe_table`/`table show`, `sample_rows`/`table sample` (with and without more rows than
asked for), `explain_query`/`explain` in three dialects and with its refusals, and
`run_sql`/`sql --read-only` with a write refused as `read_only`; a missing table is `not_found`
from a tool and `table_not_found` from its twin, compared by category. From slice 3 (D145) it
pages `row_list` and `row list --json` side by side, comparing each page's items with the
records and the tool's `next_cursor` with the twin's trailer line, and compares their refusals
(a view, a cursor of another table, a malformed cursor, a missing table). `tests/paging.rs`
pages a 10,000-row table 1,000 rows at a time while a writer inserts rows before and after the
cursor and deletes rows ahead of it between pages, and requires no row repeated or skipped,
rows added ahead read and rows added behind or removed never read; it does the same for
`table list` while tables are created, follows the text form's next-page command, and pages a
table without a primary key. `tests/dry_run.rs` runs `restore --dry-run` and a real restore of
the same dump on two copies of a file and requires the same report, the file unchanged byte for
byte and its sequence unmoved, and checks `--create` dry runs, failures and a leftover scratch
file; it does the same for `import --csv --dry-run`. `eggdb-mcp`'s server tests page `row_list`
with a cursor, cut a page at 1 MiB, and refuse views and bad cursors, and its message fuzzer
sends `row_list` calls; `eggdb-commands` tests the cursor format against hostile text. A third test runs every
example of every reading command in the registry against a temporary file and requires exit 0,
and a unit test parses every example of every command. The registry test also requires every
command of the parser, a JSON form exactly where `--format` is taken, one exit per error code
across commands, and a CLI twin for every tool. `cli/command_model_tests.rs` checks `--json` against `--format json`, the
JSON errors and that a pipe keeps text ones, `sql --json`, that `help --json` lists exactly the
registry, that the text help comes from it unchanged, and that `docs/ux/command-model.md` lists
every command, tool, error code and kept name.

## Files from released versions (D96)
`crates/eggdb/tests/data` holds files written by released versions, which every later version
must keep reading (D20). From their release's tag on they are never regenerated or edited, and a
release that changes what it writes adds samples of its own beside them; before the tag, only a
change to the bytes on disk that a numbered decision approves replaces them, in the same pull
request and the same documented way. `sample_files.rs` pins each one by its size and an
FNV-1a fingerprint of its bytes, so a changed sample fails CI, and checks that it opens
read-only and for writing, holds exactly the expected entries (every key read, scans in both
directions), passes `check` with the expected counts, takes new commits and reopens, and reads
through the `eggdb` command; the smoke test reads the samples with each release binary too.

**v0.2.0.** Written on 2026-10-27, before the v0.2.0 tag, by the release build of commit
`2b04bd5` on `main` (its binary still reports 0.1.0), format version 1, 4 KiB pages:
- `v0.2.0.db` (188,416 bytes, checkpointed, a B+tree of height 2, one value on two overflow
  pages, four free pages), made with the `eggdb` command:

  ```sh
  eggdb create v0.2.0.db
  eggdb put v0.2.0.db = "the empty key"
  eggdb put v0.2.0.db 0x00ff10 0xdeadbeef
  eggdb put v0.2.0.db doc:json '{"name":"Ada Lovelace","born":1815,"tags":["math","poetry"]}'
  eggdb put v0.2.0.db empty =
  yes 'large value, first version' | head -c 9000 > large-1.txt
  eggdb put v0.2.0.db large @large-1.txt
  yes 'EggDB v0.2.0 sample, large value' | head -c 6000 > large-2.txt
  eggdb put v0.2.0.db large @large-2.txt
  for n in $(seq 1 30); do eggdb put v0.2.0.db "user:$n" "user number $n"; done
  eggdb put v0.2.0.db user:2 "Grace Hopper"
  eggdb del v0.2.0.db user:30
  eggdb put v0.2.0.db removed "this key is deleted"
  eggdb del v0.2.0.db removed
  eggdb put v0.2.0.db "名前" "エッグ"
  eggdb put v0.2.0.db "$(printf 'k%.0s' $(seq 1 512))" "the longest key with 4 KiB pages"
  for n in $(seq 100 399); do eggdb put v0.2.0.db "item:$n" "item $n of the v0.2.0 sample file"; done
  for n in $(seq 200 249); do eggdb del v0.2.0.db "item:$n"; done
  eggdb export v0.2.0.db v0.2.0.jsonl
  ```
- `v0.2.0.jsonl`, the export made by the last line.
- `v0.2.0-log-to-replay.db`: a copy of `v0.2.0.db` with three more commits through the
  embedded API (`log_sample_commits` in `sample_files.rs`: puts, deletes, a new large value and
  a removed one) and no checkpoint, so opening it replays those commits from the log. The
  ignored test `write_the_v0_2_0_log_sample` wrote it, and refuses to run while the file exists.

## SQL against the manual (D105)
`crates/eggdb-sql/correctness/postgresql.test` holds 308 cases, each a header (`ok`, `rows` with
the expected output after `----`, or `error <SQLSTATE>`), its SQL, and the PostgreSQL manual's
chapter it comes from on a `chapter` line. They run in order in one session over the in-memory
store, so later cases see the tables earlier ones made. The runner (`src/correctness.rs`) takes
the dialect's name; each later dialect adds its own file (`sqlserver.test` names SQL Server's error number after the SQLSTATE, D122). `mysql.test` (174 cases from the MySQL
8.0 Reference Manual, D106) may name MySQL's error number after the SQLSTATE (`error 42S02
1146`), and the runner then checks both. A case marked `... differs` is a known
difference, listed in sql.md: the runner checks that it still differs, so fixing the engine
fails the suite until the mark and the doc line go. At most one case in ten may be marked.

## Subqueries, set operations and outer joins (D112)
`src/subquery_tests.rs` builds two small random tables with NULLs in 24 seeded rounds, once
without and once with secondary indexes, and requires every form of one question to give the
same rows as a reference computed in the test: `EXISTS`, `IN`, `= ANY`, an inner join and a
`DISTINCT` join; `NOT EXISTS` and an anti-join through a derived table; `NOT IN` in three-valued
logic (a NULL on either side) and `NOT (= ANY)`; a correlated `NOT IN`; `< ANY`, `> ALL`, `<>
ALL`; a correlated scalar `count` against a grouped left join; `UNION [ALL]`, `INTERSECT [ALL]`
and `EXCEPT [ALL]` against a multiset reference; `FULL` joins and their `RIGHT JOIN ... UNION
ALL ... NOT EXISTS` form. It also nests subqueries and derived tables up to their limits in a
debug build's test thread, runs a 50,000-item `IN` list and 20,000-row subqueries within a time
bound, stops a runaway recursion and a doubling one by their limits, keeps `MINUS`, `FULL JOIN`,
`ANY`, `INTERSECT ALL` and parenthesised operands in their dialects, and fuzzes 20,000
statements built from subquery, `WITH`, set-operation and join fragments in each dialect.

## Typed values: numbers, dates and JSON (D111)
The exact decimal is checked against a schoolbook decimal reference written in its tests (400
random pairs of up to 80 digits through `+ - * / %`) and against `i128` (3,000 pairs, with their
index keys' order). Seeded fuzzers feed 20,000 hostile inputs each to the date and time parser,
the JSON parser and the path parser, the numeric row and key decoders, the PostgreSQL protocol's
binary numeric reader, and table definitions, rows and index keys of the new types with bytes
flipped or cut; every one is an error or a value, never a panic. `hostile_typed_statements_never_panic_in_any_dialect`
runs 20,000 statements built from fragments of the new syntax in each of the four dialects,
and `numeric_and_time_keys_return_the_same_rows_with_and_without_indexes` compares every query
over `numeric`, `date`, `timestamp` and `timestamptz` keys with and without indexes, through
updates, deletes and inserts.

## Times, intervals, bytes, UUIDs and padded text (D113)
Interval arithmetic is checked against a plain reference on 5,000 random pairs: sums,
differences, negation, doubling and the three `justify` functions keep the length in
microseconds that `i128` arithmetic gives, comparison agrees with it, and every value prints and
reads back exactly, the extremes of all three fields included. Month-end clamping, `age`'s
borrowing, overflow at the ends of each field and multiplication by NaN, infinities and huge
factors have their own cases. `bytea`'s `encode` and `decode` round-trip random bytes in every
format, the UUID reader is given every accepted and a list of refused forms, and the PostgreSQL
protocol's binary reader gets 20,000 random byte strings for each new type.
`hostile_item_31_statements_never_panic_in_any_dialect` runs 20,000 statements built from the
new syntax in each dialect; `interval_character_bytea_uuid_and_time_keys_return_the_same_rows_with_and_without_indexes`
compares queries and changes over the six new types with and without indexes, and
`unique_indexes_follow_the_equality_of_intervals_and_characters` checks that equal values
(`'1 mon'` and `'30 days'`, `'a'` and `'a  '`, a UUID in two spellings) are one key. Version 5
table definitions, rows and keys are pinned, cut at every length, and fuzzed like version 4's.

## The shell and the query screen (D105)
The shell's line editor runs against a scripted terminal (`crates/eggdb-ui/src/line.rs`):
readline keys, `Ctrl-C` and `Ctrl-D`, history recall with the draft kept, `Ctrl-R`, completion
of one and of many candidates, pastes sent line by line. The shell runs as a batch through the
CLI's entry point (`crates/eggdb/src/cli/shell_tests.rs`): piped lines mixing key-value commands,
SQL over several lines and `.` commands, `-c`, `--keep-going` and exit codes, transaction blocks
across lines, usage errors, expanded output. The query screen runs real SQL on a real file
through the UI's fake terminal (`crates/eggdb/src/cli/ui.rs`): results, tags, errors with the
cursor on the caret, history, a block left open and rolled back, and a grid that scrolls in both
directions.

## The release smoke test (D96)
`scripts/smoke-test.sh <binary>` runs a built `eggdb` through every command: create, put (text,
hex, stdin, a 3 MB value), get, del, scan options, check, stats, an export and import round
trip, `eggdb sql` (a new table, a script, stdin, an error, a rolled-back block), `eggdb shell` with
piped lines and `-c` (plain output, the first failure's exit code, `--keep-going`), the sample
files, `eggdb ui --help`, the refusal without a terminal, and, where `script(1)`
gives it a pseudo-terminal (Linux and macOS), a scripted `eggdb ui` session that moves, deletes a
key through the confirmation and quits, after which the file must show the delete and pass
`check`, a query screen run with `F5`, and an interactive `eggdb shell` that runs a statement,
writes its history file and quits. CI runs it against the debug binary on Linux, macOS and Windows; the release workflow
runs it against the release binary on every target its runner can execute.

## Flaky tests and the CI gate (D101)
CI's last job, `check`, is the one status the `main` ruleset requires: it fails when any other job
failed or was cancelled. A test that fails on one platform and passes on a rerun is quarantined
for that platform only, with `#[cfg_attr(target_os = "<os>", ignore = "flaky on <platform>: see
issue #N")]`, and an issue labelled `flaky` names it; the cause is then fixed in the weekly
performance pass and the `ignore` removed. None is quarantined today: the sporadic `platforms`
failures of v0.2.0's development were one real bug, a file lock held by a starting child process
(D95), now fixed and tested. Benchmarks are not tests: a slower benchmark is reported, and only
one more than twice its line fails ([benchmarks.md](benchmarks.md)).

## Each layer
- **Page file:** creating, growing and freeing pages, both header slots and a torn header update,
  the file lock, a failed sync that stops every later change, and a crash at every operation of a
  commit (`page_file.rs`).
- **Log:** records, links and segments, torn tails against damage, group commit and a failed group
  sync, the durability modes, and a crash at every operation of creating the log, of commits and of
  replaying it (`log.rs`).
- **Checkpoints and recovery:** a crash at every operation of a checkpoint, of a checkpoint that
  frees segments, of commits that move to a new or recycled segment, and of recovering a wrapped
  log; damage to the newest header slot, also after a crash between a freeing checkpoint's two
  header writes and after a crash in the first flush into a recycled segment, each followed by
  an open for writing that must leave the older slot a copy; mutated logs
  (`checkpoint_tests.rs`).
- **Page cache:** eviction, pinning, refusing every read and change after a failed commit, and a
  crash at every operation of a commit (`page_cache.rs`).
- **B+tree:** splits, deletes, merges, cursors and `check` against a model, and crafted pages that
  must fail `check`, including a leaf whose fence keys differ from its parent's bounds, which
  scans refuse instead of skipping entries (`btree_tests.rs`, `btree_delete_tests.rs`).
- **Transactions:** one writer at a time; rollback, explicit and on drop; a put that fails in the
  middle of a split rolls the whole transaction back; snapshot readers against a model while a
  writer commits and rolls back hundreds of times, in `Full` and `Normal`, through checkpoints,
  recycled segments and reused pages; readers begun inside a write transaction; readers on other
  threads; snapshots that expire when their memory runs out; commit stamps that never go back,
  across a wall clock set back and a reopen (`store_tests.rs`, `hybrid_clock.rs`).

## Restoring pg_dump output (D118)
`crates/eggdb/tests/data/pg_dump` holds dumps that PostgreSQL wrote of
`scripts/pg-restore/fixture.sql` (twelve tables in two schemas with every type EggDB has,
sequences, identity columns, foreign keys with actions, check and unique constraints, views over
joins, indexes, comments, and awkward values: tabs, newlines, backslashes, `\.`, quotes, `NULL`
and empty strings, Unicode, large text and every byte value, and one more table with a column of
an enum type and an identity column, which EggDB skips), with `expected.txt`, each table's sorted
`COPY` output and each sequence's next value from PostgreSQL itself. They were made with 300 rows by
`scripts/pg-restore/check.sh target/debug/eggdb <work> --rows 300 --fixtures
crates/eggdb/tests/data/pg_dump pg16` against PostgreSQL 16.13 and with `pg18` against 18 (`PG_BIN`
naming each one's tools); `pg18.sql` carries `\restrict`, and `pg18-custom.sql` is a custom
archive turned into SQL by `pg_restore -f`. `crates/eggdb/tests/restore.rs` restores each and
compares every table byte for byte. CI's `pg-restore` job runs the same script against the
official `postgres:16` and `postgres:17` containers with about 50,000 rows, restoring every form
with `eggdb restore` and the `--inserts` one with `psql -f` against `eggdb serve`.
`crates/eggdb/tests/data/pg_dump-sample.sql`, a small dump that PostgreSQL 18 wrote of a bakery
schema, is the smoke test's sample, restored both ways.

## Restoring SQL Server scripts (D125)
`crates/eggdb/tests/data/sqlserver` holds `shop-ssms.sql`, an SSMS Generate Scripts script
(UTF-16LE with its mark, CRLF) written by us in SSMS's shape: database batches, a user and a role,
a schema, seven tables (identity with explicit 0 and negative values, `money`, `decimal`,
`varbinary(max)`, `ROWGUIDCOL`, a computed and a `timestamp` column, an `IDENTITY(1000,10)`
seed, `datetimeoffset`, a heap with `image` and `sysname`, an `xml` column kept as
`nvarchar(max)`), two
views (one with `TOP (100) PERCENT`), data with and without `GO` between rows (a CRLF inside a
value, `$(document)` as data, Vietnamese text and an emoji), indexes with `INCLUDE` and a filter,
defaults, `WITH CHECK` and `WITH NOCHECK` constraints over an orphan row, `NOCHECK CONSTRAINT`, a
`#temp` procedure, a function, a trigger, a synonym, `GRANT` and `MS_Description` extended
properties (kept as comments); and
`shop-sqlpackage.sql`, a sqlpackage script with its `:setvar` header. `shop-expected.txt` holds
queries and their results written by hand from the scripts' data and Microsoft's documented
semantics. `crates/eggdb/tests/restore_sqlserver.rs` restores both with the built binary and
checks the report and every query, then UTF-8, UTF-8 with a mark, LF-only, UTF-16 big-endian
and stdin variants, a generated script of 5,000 rows and one 20 MiB value, errors with and
without `--create` and `--keep-going`, the content routes and sqlcmd's commands (`:r` and `!!`
never run), and the follow-ups of D148 in one big-endian script (an alias type, a sequence used by
a default, a heap with an identity, an `xml` column, `SET XACT_ABORT ON`, a 6 MiB batch of
`UPDATE`s without `GO` and a description). The reader, the classifier, the span finder, the
large-batch scanner, the table rewrites (computed, `rowversion`, `xml`, alias types, `numeric`
identities, identities refused at run time) and `sqlserver_batches` have unit tests in
`crates/eggdb-sql/src/restore/tests_sqlserver.rs`, with a seeded fuzz of 3,000 hostile scripts
(GO lines, quotes, brackets, comments, sqlcmd commands, byte-order marks mid-file, broken UTF-16,
alias types, wide identities, blocks) read one to seven bytes at a time. `XACT_ABORT` and
uncommittable transactions are tested in `batch_logic_tests.rs`, Transact-SQL sequences and
`MS_Description` (with hostile statements) in `sqlserver_tests.rs`, and `GO` in `eggdb sql` in
`crates/eggdb/tests/sql.rs`. `crates/eggdb/tests/data/ssms-sample.sql` is the smoke test's
sample, restored with `eggdb restore` and with go-sqlcmd's `-i` against `eggdb serve`.

**Restoring Oracle scripts (item 43, D127).** `crates/eggdb/tests/data/oracle/hr/` holds Oracle's
own HR sample-schema scripts, unchanged (MIT, from `oracle-samples/db-sample-schemas`; the
folder's README names the commit). `crates/eggdb/tests/restore_oracle.rs` restores
`hr_install.sql` with the built binary from another working directory, so its `@@` includes are
found beside it, and checks the documented counts (regions 5, countries 25, locations 23,
departments 27, jobs 19, employees 107, job_history 10) through Oracle and PostgreSQL SQL, the
installer's own verification query (`provided` equals `actual` on every row), spot values from
the scripts (employee 100 hired 2013-06-17, `employees_seq.NEXTVAL` 207, 106 rows in
`emp_details_view`) and the enforced constraints (ORA-02291, ORA-02290, ORA-00001); then the
three sub-scripts restored one by one; our own SQL Developer, `impdp SQLFILE=` and SQLcl CSV
fixtures (`hr-sqldeveloper.sql`, `hr-impdp-tables.sql`, `hr-impdp-refs.sql`, `csv/`), written in
the tools' documented shapes from HR's values with one value holding `&` and one a newline;
content routes; all-or-nothing blocks; substitution, includes, `CONNECT` and password redaction.
The reader, the classifier, the table rewrites and the catalog-built constraint clauses have unit
tests in `crates/eggdb-sql/src/restore/tests_oracle.rs`, with a seeded fuzz of 2,000 hostile
scripts (`EGGDB_FUZZ_CASES`, `EGGDB_FUZZ_SEED`) through `MemoryIncludes`, so it never touches the
disk. `crates/eggdb/tests/data/oracle-sample.sql` is the smoke test's sample.

**Oracle restore follow-ups (item 43b, D150).** `shop-identity-sqldeveloper.sql` (ours, in SQL
Developer's shape) restores a `GENERATED ALWAYS` identity with SQL Developer's bare options, two
constraints disabled inside columns and a primary key added `USING INDEX` another index; the test
checks the identity is kept (`ALL_TAB_IDENTITY_COLS`, `ORA-32795`, the next value 41), the index
taken over, the foreign key enabled (`ORA-02291`) and the still-disabled `CHECK` listed. Unit
tests in `tests_oracle.rs` cover the column-level rewrites, `ALTER TABLE ... ADD (...)`, the
refused-identity retry, a restore with all three and `SqlPlusScript`'s steps; `oracle_tests.rs`
covers identity options, `ORA-32795`/`ORA-32796` and `USING INDEX` in a session; the hostile
Oracle fragments gained the identity and `USING INDEX` forms. `crates/eggdb/tests/sql.rs` runs a
SQL*Plus script through `eggdb sql --dialect oracle` (an `@@` include, `--define`, `PROMPT`,
`SPOOL`, `HOST`, `/` after `;`, `EXIT`, `--json`, the stopping errors and `-c` left plain), and
the smoke test pipes a short script with `DEFINE`, `PROMPT` and a counted `SET`.

## Unchanged Java and Python apps (D131)
`crates/eggdb/tests/clients/java/AppCheck.java` is a pgjdbc program: CRUD through prepared
statements (run past the driver's threshold, so it switches to named server statements),
`INSERT ... RETURNING`, a rollback and a commit, a query over the fixture, and
`DatabaseMetaData`'s `getSchemas`, `getTables`, `getColumns`, `getPrimaryKeys` and
`getImportedKeys`. `crates/eggdb/tests/clients/python/app_check.py` does CRUD through psycopg 3
and reflects both schemas with SQLAlchemy Core (`MetaData.reflect()`, the inspector and a Core
query). `scripts/pg-apps/check.sh <eggdb> <work>` loads `scripts/pg-catalog/fixture.sql` into a
PostgreSQL server, restores its `pg_dump` into EggDB, runs both programs against both servers and
fails unless each prints the same bytes on both. CI's `pg-apps` job runs it against the official
`postgres:17` container with pgjdbc 42.7.4, psycopg 3.2.3 and SQLAlchemy 2.0.36, downloaded as test
tools and never shipped, and keeps both outputs and the difference. `information_schema`, the
catalog functions, aggregate `ORDER BY`, lock-step set-returning functions and the ranking window
functions have unit tests in `crates/eggdb-sql/src/information_schema/tests.rs`,
`pg_catalog/tests.rs`, `set_returning_tests.rs` and `window.rs`, and cases in
`correctness/postgresql.test`.

## .NET apps through Npgsql (D135)
`crates/eggdb/tests/clients/dotnet/NpgsqlAppCheck` is a C# program on Npgsql 8.0.5: it opens a
data source (Npgsql's type loading), then, each step on a connection taken from the pool again
(so each reuse sends `DISCARD ALL`), does CRUD with typed parameters, reads an `integer[]`
column, rolls back and commits a transaction, runs an `NpgsqlBatch`, runs a batch whose second
command fails and checks that nothing of it stayed, and cancels `SELECT pg_sleep(30)` with
`NpgsqlCommand.Cancel()` after one second. `scripts/pg-apps/check.sh` builds it with the .NET
SDK (Npgsql restored from NuGet, a test tool never shipped) and runs it against PostgreSQL 17 and
EggDB next to the Java and Python programs, requiring the same bytes. Below the driver,
`crates/eggdb-pgwire/tests/transactions_and_cancel.rs` speaks the protocol directly: pipelines
that commit at `Sync` or leave nothing, one `Query` message's statements as one transaction, a
`BEGIN` inside a pipeline, portals closed at the end of their transaction, `DISCARD ALL`,
`CancelRequest` with right and wrong keys and one that rolls back a pipeline, and Npgsql's
type-loading queries, kept in `crates/eggdb/tests/data/npgsql/load-types.sql`, which
`pg_catalog/tests.rs` also runs through a session. `implicit_transaction_tests.rs` and
`cancel.rs` in `eggdb-sql`, and `backend_keys.rs` in `eggdb-pgwire`, test the pieces.

## ORMs and migration tools (D138)
The ORM section of `crates/eggdb/tests/clients/python/app_check.py` is an unchanged SQLAlchemy 2.0
ORM app: two declarative models with a one-to-many relationship, `create_all()` and `drop_all()`,
a session inserting several objects at once, joined, `selectinload` and grouped queries, a page
with `limit`/`offset`, updates, a bulk `update()`, a flush rolled back, a duplicate key and a
cascading delete. `crates/eggdb/tests/clients/java/FlywayCheck.java` runs Flyway 10.20.1
(Community, Apache-2.0) through its Java API with the migrations in
`crates/eggdb/tests/clients/java/flyway/base` and `later`: `migrate`, `info`, `validate`, a second
`migrate` with nothing to do, a later pending migration, then the migrated rows and the history
table, with Flyway's session-level lock (`flyway.postgresql.transactional.lock=false`) and
Flyway's log on stderr. `scripts/pg-apps/check.sh` runs both against PostgreSQL 17 and EggDB
(Flyway on a new, empty database on each) and requires the same bytes. CI's `pg-apps` job
downloads Flyway's jars and their jackson dependencies from Maven Central as test tools, never
shipped. `crates/eggdb/tests/clients/dotnet/EfCoreAppCheck` is an EF Core 8.0.11 app with two
hand-written migrations: `Database.Migrate()` twice, a `SaveChanges` of two blogs and six posts
(EF's `MERGE ... OUTPUT`), LINQ queries (paging, a join, `Count`, `Any`, `Include`, a missing row,
`GroupBy`, `Contains` over a list through `OPENJSON`), an update and a delete batch (`OUTPUT 1`)
and a `SaveChanges` inside a transaction that is rolled back. `scripts/tds-apps/check.sh` serves
it a new database and compares its output with `scripts/tds-apps/expected-efcore-dotnet.out`.
Below the tools, `crates/eggdb-sql/src/insert_select_tests.rs`, `migration_tool_tests.rs` and
`advisory_locks.rs` test `INSERT ... SELECT`, Flyway's statements and the lock table, the SQL
Server suite has cases for `MERGE`, `OPENJSON`, `SAVE TRANSACTION` and `INSERT ... SELECT`, and
`crates/eggdb-tdswire/tests/rpc.rs` sends a save point and a rollback to it over the socket.

SQL Server's drivers have their own check (item 23b, D132).
`crates/eggdb/tests/clients/java/TdsAppCheck.java` (mssql-jdbc) and
`crates/eggdb/tests/clients/dotnet/TdsAppCheck` (Microsoft.Data.SqlClient) run parameterized CRUD
(a prepared statement executed three times, so the drivers go through `sp_executesql`,
`sp_prepexec` and `sp_execute`), a committed and a rolled-back transaction, a failing query and,
for SqlClient, a reused pooled connection whose reset clears `SET NOCOUNT ON`.
`scripts/tds-apps/check.sh <eggdb> <work>` serves a new database with `eggdb serve --tds-listen`,
runs both and fails unless each prints exactly `scripts/tds-apps/expected-java.out` and
`expected-dotnet.out`, written from Microsoft's documentation, since SQL Server itself cannot be a
CI oracle under its license. CI's `tds-apps` job runs it with mssql-jdbc 12.8.1 and SqlClient
5.2.2, downloaded as test tools and never shipped, and keeps both outputs, the server log and the
difference. The RPC, transaction manager and reset paths have unit tests in
`crates/eggdb-tdswire/src/rpc.rs`, `messages.rs`, `procedures.rs` and `tokens.rs`, socket tests in
`crates/eggdb-tdswire/tests/rpc.rs`, mutated requests in `hostile_packets.rs`, and the dialect's
parameters, implicit transactions, `IF` and resets have tests in
`crates/eggdb-sql/src/sqlserver_tests.rs`.

Oracle SQL over PostgreSQL and TDS connections has its own check (item 44, D134).
`scripts/oracle-apps/check.sh <eggdb> <work>` restores Oracle's HR installer unchanged, serves it
as the user `hr`, and runs `scripts/oracle-apps/commands.sql` through psql with `PGOPTIONS='-c
eggdb.dialect=oracle'` (HR's queries, joins and grouping, `FETCH FIRST`, `TO_CHAR` of a date,
`SYS_CONTEXT`, the data dictionary and `NLS_SESSION_PARAMETERS`, HR's own verification query)
and `crates/eggdb/tests/clients/java/OracleAppCheck.java` through pgjdbc with the same option in
its `options` property (prepared statements with `?`, a dictionary self-join, a rolled-back and a
committed transaction, a missing table's SQLSTATE). It fails unless they print exactly
`scripts/oracle-apps/expected-psql.out` and `expected-java.out`, written from HR's documented
data, since Oracle Database cannot be a CI oracle under its license. CI's `oracle-apps` job runs
it with pgjdbc 42.7.4 and the `postgres:17` image's psql, test tools never shipped. The dictionary,
`SYS_CONTEXT`, the dialect switch on PostgreSQL and Transact-SQL sessions, `$n` parameters and
the user's own schema have unit tests in `crates/eggdb-sql/src/oracle_dictionary/tests.rs`; the
startup option, a refused dialect and the extended protocol in Oracle's dialect have socket tests
in `crates/eggdb-pgwire/tests/protocol.rs`, and a TDS batch that switches, with transaction
manager requests after it, in `crates/eggdb-tdswire/tests/protocol.rs`; the hostile Oracle
statement fuzzer in `crates/eggdb-sql/src/tests.rs` covers the new views, `SYS_CONTEXT` and the
`SET`.

## The C API (D142)
`crates/eggdb-capi/tests/api.rs` calls every function of `eggdb.h` the way a C program does,
through raw pointers: commits and rollbacks across a reopen, a read transaction refusing a
change, scans of 1,000 keys in batches forward, backward, by range and by prefix, a scan
continued in a later transaction that sees the writes ahead of it, collections kept apart from
the default key space, times to live, counters and compare-and-set, SQL in PostgreSQL's and
SQLite's dialects with typed values, a failing statement's SQLSTATE, parameters carrying a
quote and a semicolon, a transaction block rolled back and one left open at close, backup and
compaction, `eggdb_close` refusing while a transaction or session is open, four threads reading
through one handle, and misuse (NULL handles and buffers, text that is not UTF-8, unknown flags
and dialects) answered with `EGGDB_ERR_INVALID_ARGUMENT`. `tests/header.rs` parses `eggdb.h` and
fails when a constant, a function's parameter count or a struct's fields differ from the Rust
code. `scripts/c-api/check.sh <library directory> <work>` compiles
`crates/eggdb-capi/tests/c/api_check.c` with `-Wall -Wextra -Wpedantic -Werror`, compiles the
header as C++11, runs the program and then `examples/python/eggdb_ctypes.py`; CI runs it in
`lint-and-test` (Linux) and `platforms` (macOS; the Python part on Windows), and the release
workflow on the release libraries of the Linux and macOS targets its runners can load.

## Not tested yet
- `Off` after a power loss: the format promises nothing there, so the sweep only checks that
  opening and checking don't panic.
- Several writers (M3) and transactions larger than the cache, which fail with `CacheFull` until
  M3.
- Damage to bytes after they were synced, beyond the header-slot tests, the crafted logs and the
  hostile-file fuzzer: the limits are in format.md, "Damage to the newest header slot".
- Log records forged with valid record checksums are tested at the log's layer
  (`checkpoint_tests.rs`, `log.rs`), not by the whole-file fuzzer, which can only damage them.
- Real disks: the simulator's model of a power loss is sector-sized pieces of unsynced writes,
  not every behavior of real hardware.
