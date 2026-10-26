# What is tested

This page lists what the storage engine's tests prove about crashes and I/O faults, and what they
don't. It feeds the correctness statement of v0.2.0 (roadmap row 18). Every test runs on the
simulated file system of `eggdb-platform` (D9), which numbers every file operation, can fail any
one of them, and can simulate a power loss that drops every unsynced write or tears unsynced
writes into 512-byte pieces that survive or not, chosen by a seed.

## The whole stack: the crash sweep (D85)
`crates/eggdb-storage/src/crash_sweep_tests.rs` runs a seeded B+tree workload through the page
cache, the log and the page file: puts, overwrites, large values and long keys, a growing phase
that splits the root and a shrinking phase that merges pages and collapses it again, rolled-back
transactions, explicit checkpoints and syncs, and automatic checkpoints often enough that the log
wraps through its recycled segments and gives extra segments to the free list. It runs once
with each durability mode (`Full`, `Normal`, `Off`).

Each I/O operation of the run is failed in turn, and the run then ends in one of these ways:
- a reopen without a power loss (the fault was an I/O error, and the process went on or died);
- a power loss that drops every unsynced write;
- a power loss that tears unsynced writes (two seeds in CI, four in the exhaustive run).

Then the file is opened again, which runs recovery, and the test checks:
- the tree passes `BTree::check`, and a forward and a reverse scan return exactly the contents of
  one commit, never a mix of two;
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

CI gives every operation one ending, rotating through the four, and runs in about 20 seconds of a
debug build. Setting `EGGDB_CRASH_SWEEP=1` gives every operation every ending; the counts of the
last exhaustive run are in D85.

## Each layer
- **Page file:** creating, growing and freeing pages, both header slots and a torn header update,
  the file lock, a failed sync that stops every later change, and a crash at every operation of a
  commit (`page_file.rs`).
- **Log:** records, links and segments, torn tails against damage, group commit and a failed group
  sync, the durability modes, and a crash at every operation of creating the log, of commits and of
  replaying it (`log.rs`).
- **Checkpoints and recovery:** a crash at every operation of a checkpoint, of a checkpoint that
  frees segments, of commits that move to a new or recycled segment, and of recovering a wrapped
  log; damage to the newest header slot; mutated logs (`checkpoint_tests.rs`).
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
- Damage to bytes after they were synced, beyond the header-slot and crafted-log tests: the
  limits are in format.md, "Damage to the newest header slot" (issue #39).
- A fuzzer for hostile files (roadmap row 12) and real disks: the simulator's model of a
  power loss is sector-sized pieces of unsynced writes, not every behavior of real hardware.
