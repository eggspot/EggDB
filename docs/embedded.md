# Embedded EggDB in Rust

EggDB opens in your process, like SQLite: no server, one file. This release is an ordered
key-value store with transactions: keys and values are byte strings, keys are sorted as unsigned
bytes, and every change commits atomically and survives a crash. The API is the `eggdb` crate
(D87); its rustdoc has an example for every item. Every example on this page is compiled by
`cargo test` (the `EmbeddedGuide` doctests in `crates/eggdb/src/lib.rs`), so none of them can
drift from the API.

## Adding it to a project
EggDB is not on crates.io yet (publishing waits for the project's accounts). Depend on a
release tag from the repository, which uses no other crate:

```toml
[dependencies]
eggdb = { git = "https://github.com/eggspot/EggDB", tag = "v0.2.0" }
```

```rust,no_run
use eggdb::Database;

fn main() -> eggdb::Result<()> {
    let db = Database::open("app.db")?;
    db.put("user:1", "Ada")?;
    assert_eq!(db.get("user:1")?, Some(b"Ada".to_vec()));
    db.close()
}
```

## Opening
- `Database::open(path)` opens the file, or creates an empty database there if there is none.
- `Database::open_with(path, options)` takes `Options`, a builder:

| Option | Default | What it does |
|--------|---------|--------------|
| `create_if_missing(bool)` | `true` | `false` fails with `ErrorKind::NotFound` instead of creating a file |
| `read_only(bool)` | `false` | opens for reading only; several read-only opens can share the file |
| `durability(Durability)` | `Full` | how commits wait for the disk (below) |
| `cache_bytes(usize)` | 16 MiB | the page cache; also the most one write transaction can change, a large value included |
| `snapshot_memory_bytes(usize)` | the cache's size | memory for pages that open read transactions still need |
| `auto_checkpoint_log_bytes(Option<u64>)` | 1 MiB | log written before a commit checkpoints by itself; `None` turns it off |
| `page_size_bytes(usize)` | 4096 | the page size of a new file, a power of two up to 65,536 |
| `write_timeout(Duration)` | 5 s | how long `begin_write` waits for the writer before it |

```rust,no_run
use eggdb::{Database, Durability, Options};
use std::time::Duration;

fn main() -> eggdb::Result<()> {
    let options = Options::new()
        .create_if_missing(false)
        .durability(Durability::Normal)
        .cache_bytes(64 << 20)
        .write_timeout(Duration::from_secs(1));
    let db = Database::open_with("app.db", options)?;
    let reader = Database::open_with("other.db", Options::new().read_only(true))?;
    println!("pages of {} bytes, keys up to {} bytes", reader.page_size(), db.max_key_bytes());
    Ok(())
}
```

Opening recovers the file: it replays the commits in the write-ahead log since the last
checkpoint, so it holds exactly the commits that were durable when the last process stopped,
however it stopped. Values outside what EggDB accepts fail the open with
`ErrorKind::InvalidOption`, before any file is created.

**One file, one user.** An open for writing locks the file against every other open, in this
process or another; a read-only open locks out writers only. The second open fails at once with
`ErrorKind::Locked`. Within a process, share one `Database` between threads instead. A child
process shares its parent's open files until it starts its own program, so a database closed
while another thread starts a child can stay locked for that moment, and reopening it at once
can fail with `Locked`.

## Transactions
- **Read transactions** (`db.begin_read()`) see a snapshot: the last commit before they began,
  unchanged by later commits. Any number can be open, on any thread.
- **The write transaction** (`db.begin_write()`) is one at a time. It sees the last commit and
  its own changes; nobody else sees them before `commit()`. `rollback()` or dropping it forgets
  them, so `?` rolls back by itself.
- `commit()` returns a `CommitStamp`, a hybrid-clock timestamp above every earlier commit's.

```rust,no_run
use eggdb::Database;

fn main() -> eggdb::Result<()> {
    let db = Database::open("app.db")?;

    let mut tx = db.begin_write()?;
    tx.put("a", "1")?;
    tx.delete("b")?;
    let stamp = tx.commit()?;
    println!("committed at {stamp:?}");

    let moved = db.write(|tx| {
        let value = tx.get("a")?.unwrap_or_default();
        tx.put("c", &value)?;
        tx.delete("a")?;
        Ok(value.len())
    })?;
    let count = db.read(|tx| Ok(tx.scan(..).count()))?;
    println!("moved {moved} bytes; {count} keys");
    db.close()
}
```

Closures do the same with less code: `db.write(|tx| { ...; Ok(value) })` commits when the
closure returns `Ok` and rolls back on `Err`; `db.read(|tx| ...)` runs in a read transaction.
Both use `eggdb::Error`; for an error type of your own, use `begin_write` and `commit`.

`db.get`, `db.put` and `db.delete` are transactions of one step. Each `put` commits, and under
`Durability::Full` waits for the disk, so put many keys in one `write` instead of a loop of
`put`s.

**Failures inside a write transaction.** A key or value over the size limits, or a value
needing more pages than the page cache holds, fails with `ErrorKind::TooLarge` and changes
nothing; the transaction stays open. Any other failure of a
change, such as `ErrorKind::CacheFull`, rolls the whole transaction back at once: later calls
fail with `ErrorKind::TransactionFailed`, and the next writer can begin.

## Scans
`tx.scan(range)` returns the entries with keys in `range`, smallest first. Any of Rust's range
forms works, with keys as `&str`, `&[u8]`, `&[u8; N]`, `String` or `Vec<u8>`: `..`, `"a".."m"`,
`"a"..`, `..="z"`, or a pair of `std::ops::Bound`s. `tx.scan_prefix(prefix)` returns the keys
that start with `prefix`. A range whose start lies above its end is empty.

```rust,no_run
use eggdb::Database;
use std::ops::Bound;

fn main() -> eggdb::Result<()> {
    let db = Database::open("app.db")?;
    let tx = db.begin_read()?;
    for entry in tx.scan_prefix("user:").rev() {
        let (key, value) = entry?;
        println!("{key:?} = {value:?}");
    }
    let first_ten: Vec<_> = tx.scan("a".."m").take(10).collect::<eggdb::Result<_>>()?;
    let after_k = tx.scan((Bound::Excluded("k"), Bound::Unbounded)).count();
    let mut scan = tx.scan(..);
    while let Some((key, value)) = scan.next_entry()? {
        println!("{} bytes under {} bytes of key", value.len(), key.len());
    }
    println!("{} entries, then {after_k}", first_ten.len());
    Ok(())
}
```

- **Reverse:** `.rev()` gives the largest key first, as for a `BTreeMap` range, and `next()` and
  `next_back()` can be mixed; the scan ends where the two ends meet.
- **Cost:** each item is two new `Vec<u8>`s, the key and the value. `scan.next_entry()` (and
  `scan.next_back_entry()` from the other end) lends them instead, with no copy, until the next
  call. Underneath, the scan copies one leaf page of
  entries at a time and holds no page between calls. A large value is copied whole and ends
  that copy, so the scan holds at most one large value; as an owned item it is copied once
  more, twice its size in memory until the item is dropped.
- **Errors** come as one `Err` item, after which the scan ends.
- A write transaction's scans include its own changes. A scan borrows its transaction, so
  nothing changes under it.

## Durability
| Mode | A commit returns after | A power loss can |
|------|------------------------|------------------|
| `Full` (default) | it is on the disk | lose nothing that was committed |
| `Normal` | it is written, without waiting for the disk | lose the latest commits, never corrupt the file |
| `Off` | it is written; nothing ever waits for the disk | corrupt the file (bulk loads you can redo, tests) |

A crash of the process alone loses nothing in any mode. Under `Normal`, `db.sync()` makes every
commit so far durable, and checkpoints and `close` sync too. The names follow SQLite's
`synchronous` setting; [format.md](format.md) has the details.

## Closing
`db.close()` writes a checkpoint, so the next open has no log to replay, and returns any error.
Dropping a `Database` closes the file without writing anything: no commit is lost, and the next
open replays the log since the last checkpoint, about a megabyte at most by default. Under
`Normal`, commits since the last sync stay exposed to a power loss until the next open syncs
them. `db.checkpoint()` writes a checkpoint without closing.

## Errors
Every error is an `eggdb::Error`. Its message says what happened and what to do, for people and
for AI models; `error.kind()` is an `ErrorKind` to match on:

| Kind | Meaning |
|------|---------|
| `NotFound`, `PermissionDenied`, `Io` | the file system refused or failed |
| `Locked` | the file is open elsewhere |
| `ReadOnly` | a change to a read-only database |
| `NotADatabase`, `Incompatible`, `Corrupt` | not an EggDB file, a newer one, or a damaged one |
| `TooLarge` | a key or value over the limits below; nothing changed |
| `CacheFull` | a write transaction outgrew the page cache; it was rolled back |
| `WriterActive` | another writer stayed open for the whole wait |
| `SnapshotTooOld` | a read transaction's snapshot expired; begin a new one |
| `TransactionFailed` | the write transaction failed earlier; begin a new one |
| `NeedsReopen` | a write or sync failed; open the database again to recover |
| `DatabaseFull`, `OutOfMemory`, `InvalidOption`, `Other` | the rest |

`ErrorKind` may gain kinds in later releases, so a `match` needs a wildcard arm. An
`eggdb::Error` converts into a `std::io::Error` of the closest kind.

```rust,no_run
use eggdb::{Database, ErrorKind};

fn main() {
    match Database::open("app.db") {
        Ok(db) => println!("{} bytes per page", db.page_size()),
        Err(error) if error.kind() == ErrorKind::Locked => eprintln!("open elsewhere: {error}"),
        Err(error) => match error.kind() {
            ErrorKind::Corrupt => eprintln!("run `eggdb check app.db`: {error}"),
            _ => eprintln!("{error}"),
        },
    }
}
```

## Threads
`Database` is `Send` and `Sync`: share it by reference, in scoped threads, or through an `Arc`.
Readers never wait for the writer, nor the writer for readers. A second writer waits for the
first, up to `Options::write_timeout`, then fails with `WriterActive`; `try_begin_write` fails at
once. A thread that already holds the write transaction is refused a second one at once, since
it would wait for itself. `db.checkpoint()` waits for the writer the same way.

A read transaction that stays open while the writer changes many pages keeps their old versions
in the snapshot memory. When that memory runs out, the oldest snapshots expire with
`SnapshotTooOld`, so a forgotten reader never blocks the writer. Keep read transactions short,
or give them more `snapshot_memory_bytes`.

## Checking and measuring
- `db.check()` reads every page of the tree and every large value as of the last commit, checks
  how the pages fit together, and accounts for every page of the file: each must belong to the
  tree or a value, be free, or belong to the log. It returns a `CheckReport` (entries, key and
  value bytes, tree height and pages, large values and their overflow pages, free, log and
  leaked pages), or `ErrorKind::Corrupt` naming the damage, such as a page with two uses. A
  leaked page, used by nothing, does no harm but is never reused; a healthy database has none.
  Other operations wait while it runs.
- `db.stats()` reads no page: the page size and count, free pages, the file's size, the last
  commit's stamp, the cache's size, contents, hits and misses, and the log since the last
  checkpoint.

```rust,no_run
use eggdb::Database;

fn main() -> eggdb::Result<()> {
    let db = Database::open("app.db")?;
    let report = db.check()?;
    println!("{} entries, {} leaked pages", report.entries, report.leaked_pages);
    let stats = db.stats()?;
    println!("{} of {} bytes per page", stats.file_bytes, stats.page_size);
    db.close()
}
```

## Limits in this release
- **Size of keys:** at most an eighth of a page, 512 bytes with the default 4 KiB pages and 8,192
  with 64 KiB pages (`db.max_key_bytes()`).
- **Size of values:** at most 1 GiB (`db.max_value_bytes()`). A value is read and written
  whole, so it takes its size in memory while it is used; a streaming read comes later.
  - A key and value of up to `3 × page_size / 16 − 18` bytes together (750 bytes with 4 KiB
    pages, 12,270 with 64 KiB pages) sit in a B+tree leaf.
  - A longer value goes to overflow pages of its own, about one page per 4,056 bytes with 4
    KiB pages, the last one partly empty; its leaf keeps the key and a 16-byte reference.
    Reading it costs one more page read per page of value.
  - Every page a write transaction writes stays in the page cache until its commit, so a value
    needs about its size in cache: with the default 16 MiB, a value above about 15.8 MiB fails
    with `TooLarge` and needs a larger `cache_bytes`.
  - Replacing or deleting a large value frees its pages at the commit, and later values reuse
    them, so rewriting large values keeps the file's size steady. A single transaction whose
    log is longer than the seven log segments the file keeps for reuse (about 448 KiB of
    changed pages with the default segments) grows the file by its log once; later ones reuse
    that space, and the next open for writing gives it back (D97).
  - A read transaction that stays open while the writer replaces large values keeps their old
    pages in the snapshot memory; a value larger than `snapshot_memory_bytes` expires older
    snapshots with `SnapshotTooOld`.
- **Size of a write transaction:** every page it changes stays in the page cache until the
  commit, so a transaction larger than `cache_bytes` fails with `CacheFull`. Split bulk loads
  into several transactions.
- **One writer at a time** in the whole file; any number of readers.
- **One key space** per database. Named collections come later, as an addition: the default key
  space stays what `get`, `put`, `delete` and `scan` use (D87).
- **No in-memory database** yet: every database is a file (D87).
- **The file format** is version 1 ([format.md](format.md)). Every later release reads the
  files v0.2.0 writes, and CI keeps opening the v0.2.0 sample files to hold it to that (D20,
  D96).
