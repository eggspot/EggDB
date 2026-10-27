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
the dialect's name; each later dialect adds its own file. `mysql.test` (174 cases from the MySQL
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
