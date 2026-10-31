# Embedded EggDB in Rust

EggDB opens in your process, like SQLite: no server, one file. This release is an ordered
key-value store with transactions: keys and values are byte strings, keys are sorted as unsigned
bytes, and every change commits atomically and survives a crash. The API is the `eggdb` crate
(D87); its rustdoc has an example for every item. Every example on this page is compiled by
`cargo test` (the `EmbeddedGuide` doctests in `crates/eggdb/src/lib.rs`), so none of them can
drift from the API.

From C and any other language that calls C (Python, .NET, Java, Go and the rest), the same
database is a C library with the header `eggdb.h`: see [c-api.md](c-api.md).

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
| `compress_values(bool)` | off | compress values of 4 KiB or more with EggDB's LZ4-format codec when that saves at least a sixteenth; the first one marks the file so older releases refuse it |
| `lock_cache(bool)` | off | keep the page cache in RAM, never swapped out (`mlock`, `VirtualLock`); the open fails when the process may not lock that much |
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

## Collections
A database holds the **default key space**, which `get`, `put`, `delete` and `scan` on the
database and its transactions use, and any number of **collections**: named key spaces in the
same file, each its own ordered map from byte-string keys to byte-string values (D139).
`db.create_collection(name)` makes one and `db.drop_collection(name)` removes it with every key
in it, each in one commit; `db.collection_names()` lists them in byte order. A name is 1 to 128
bytes of text without whitespace or control characters.

`tx.collection(name)` gives the collection as a transaction sees it: a `ReadCollection` from a
read transaction, a `WriteCollection` from the write transaction, with the default key space's
methods (`get`, `get_with`, `scan`, `scan_prefix`, and `put`, `put_if_absent` and `delete` when
writing). One write transaction changes any number of collections and the default key space,
and commits them together. `db.collection(name)` gives a `Collection` whose `get`, `put` and
`delete` are transactions of one step.

```rust,no_run
use eggdb::Database;

fn main() -> eggdb::Result<()> {
    let db = Database::open("app.db")?;
    db.create_collection("users")?;
    db.create_collection("sessions")?;
    db.write(|tx| {
        tx.collection("users")?.put("1", "Ada")?;
        tx.collection("sessions")?.put("s1", "user 1")?;
        tx.put("config:theme", "dark")?;
        Ok(())
    })?;
    let tx = db.begin_read()?;
    let users = tx.collection("users")?;
    for entry in users.scan(..) {
        let (id, name) = entry?;
        println!("{id:?} = {name:?}");
    }
    assert_eq!(tx.scan(..).count(), 1);
    drop(users);
    drop(tx);
    println!("{:?}", db.collection_names()?);
    db.collection("sessions").delete("s1")?;
    db.drop_collection("sessions")?;
    Ok(())
}
```

- **Missing collections** are errors, never empty: `collection` fails with
  `ErrorKind::CollectionNotFound` until `create_collection` made it (in this transaction or a
  committed one). `create_collection` returns `false` for one that exists; `drop_collection`
  returns `false` for one that does not.
- **Keys** in a collection are up to `db.max_collection_key_bytes()`, ten bytes shorter than in
  the default key space (502 with 4 KiB pages): the ten bytes say which collection a key is in.
- **The default key space never shows a collection's keys**: its scans skip them, and its `put`
  refuses keys that start with the bytes kept for collections, `0xff 'C' 'O' 'L' 0x00`, with
  `ErrorKind::ReservedKey`. SQL's tables stay in the default key space under their own prefix
  (`sql.md`), as before.
- **Dropping** deletes every key of the collection in one transaction, so a collection that
  changes more pages than the page cache holds fails with `CacheFull`; open the database with a
  larger `Options::cache_bytes`.
- A read transaction keeps its snapshot: a collection dropped or changed after it began is still
  there, as it was, for it.

## Expiring keys and counters
A key, in the default key space or in a collection, can have a **time to live** (D140):
`tx.put_with_expiry(key, value, ttl)` stores it with one, `tx.set_expiry(key, ttl)` gives one to
a key that is there (and returns whether it was), `tx.clear_expiry(key)` removes it, and
`tx.time_to_live(key)` says how long the key has left (`None`: no time to live, or no key). At
its deadline the key is gone for every read: `get`, `get_with` and scans leave it out, and the
write methods treat it as absent. Every commit removes up to 32 keys whose time has run out, and
`db.remove_expired()` removes all of them at once; reads never need that to be correct.

A **counter** is a value that is a decimal integer stored as text, such as `42` or `-7`, from
`i64::MIN` to `i64::MAX`. `tx.increment(key, by)` adds `by` (subtracts a negative one) and
returns the new count; an absent key counts as 0. A value that is not a counter, or a sum outside
the range, fails with `ErrorKind::NotAnInteger` and changes nothing. **Compare-and-set**,
`tx.compare_and_set(key, expected, new)`, stores `new` (or removes the key, for `None`) only when
the value is `expected` (`None`: the key is absent), and returns whether it did.

```rust,no_run
use eggdb::Database;
use std::time::Duration;

fn main() -> eggdb::Result<()> {
    let db = Database::open("app.db")?;
    db.put_with_expiry("session:1", "alice", Duration::from_secs(30 * 60))?;
    println!("{:?} left", db.time_to_live("session:1")?);
    let visits = db.increment("visits", 1)?;
    println!("{visits} visits");
    if db.compare_and_set("lock", None, Some(b"worker-7".as_slice()))? {
        println!("the lock is ours");
        db.compare_and_set("lock", Some(b"worker-7".as_slice()), None)?;
    }
    db.create_collection("stats")?;
    db.collection("stats").increment("page:home", 1)?;
    Ok(())
}
```

- **Atomic.** Each of these runs in the one write transaction, so increments from any number
  of threads never lose a count, and nothing changes between a compare and its set.
- **What a put does to the time to live.** `put` and a `compare_and_set` that stores a value
  replace the value and remove the time to live, as Redis's `SET` does; `increment` keeps it;
  `put_if_absent` stores over an expired key. A time to live of zero removes the key.
- **Time** is the database's clock, read when a transaction begins, and never earlier than the
  last commit's stamp, so a wall clock set back cannot bring an expired key back. A read
  transaction keeps its time: a key that expires while it lives stays readable in it.
  `Options::clock` sets the clock; a `SimClock` makes time move only when a test moves it.
- **Keys with a time to live are 14 bytes shorter** than other keys (498 bytes in the default
  key space with 4 KiB pages, 488 in a collection): the deadline is kept in a key of its own.
- **Cost.** A file that never held a key with a time to live reads and writes exactly as
  before. Once one has, each read also looks up the key's deadline, and each write that
  replaces or deletes a key looks it up too.
- **Older releases** read such a file but do not write it: the first time to live sets the
  file's read-only-compatible `EXPIRING_KEYS` feature flag (format.md).
- Exports carry the keys that are live when they are made, without their deadlines.

## SQL
`eggdb::sql` runs SQL on the same file, in PostgreSQL's, MySQL's, SQLite's or Oracle's dialect, all on
the same tables ([sql.md](sql.md)). `db.sql_store()` gives the database as a SQL store, and
`store.session(dialect)` a session on it; `execute` runs statements separated by semicolons and
hands each result to a closure. The session's transactions are the database's write
transactions, so a SQL `BEGIN` holds the one writer until `COMMIT` or `ROLLBACK`.

Result rows are `eggdb::sql::Value`s, one variant per type: `Value::Text`, `Value::Numeric`,
`Value::Date` (days since 2000-01-01), `Value::Timestamp` and `Value::TimestampTz`
(microseconds since 2000-01-01), `Value::Time` (microseconds since midnight),
`Value::TimeTz { micros, offset_seconds }`, `Value::Interval(Interval { months, days, micros })`,
`Value::Bytea(Vec<u8>)`, `Value::Uuid([u8; 16])`, `Value::Bpchar` (a `character(n)` with its
padding) and the rest; `value.to_text()` prints any of them as PostgreSQL does and
`dialect.value_text(&value)` as that dialect's clients do. `gen_random_uuid()` draws from the
operating system's secure generator, which `db.sql_store()` registers (D113).

```rust
use eggdb::Database;
use eggdb::sql::{Dialect, Parameter, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    # let path = std::env::temp_dir().join(format!("eggdb-guide-sql-{}.db", std::process::id()));
    let db = Database::open(&path)?;
    let store = db.sql_store();
    let mut sql = store.session(Dialect::Sqlite);
    sql.execute(
        "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO users (name) VALUES ('Ada'), ('Grace')",
        &mut |_| {},
    )?;
    let mut names = Vec::new();
    sql.execute("SELECT name FROM users ORDER BY id", &mut |result| {
        names.extend(result.rows.into_iter().map(|row| row[0].clone()));
    })?;
    assert_eq!(names, vec![Value::Text("Ada".into()), Value::Text("Grace".into())]);
    let second = [Parameter { declared_type: None, value: Some("2".into()) }];
    let mut found = Vec::new();
    sql.execute_prepared("SELECT name FROM users WHERE id = ?", &second, &mut |result| {
        found.extend(result.rows);
        true
    })?;
    assert_eq!(found, vec![vec![Value::Text("Grace".into())]]);
    if let Err(error) = sql.execute("SELECT * FROM missing", &mut |_| {}) {
        assert_eq!(error.to_string(), "Error: no such table: missing");
        assert_eq!(error.vendor_code(), Some(1));
    }
    sql.close()?;
    db.close()?;
    # std::fs::remove_file(&path)?;
    Ok(())
}
```

`execute_prepared` binds parameter values as text, read as a constant of the type their place
calls for; in SQLite's dialect a parameter left without a value is NULL, as in SQLite. Errors
are `eggdb::sql::SqlError`s in the session's dialect: SQLite's message and extended result code
here, MySQL's numbers in MySQL's, `ORA-` numbers in Oracle's, SQLSTATEs in all four.

In Oracle's dialect (`Dialect::Oracle`) the first change starts a transaction, as in Oracle:
it holds the database's one writer until `COMMIT`, `ROLLBACK` or DDL, and `close` rolls it back,
so commit what you want to keep. Bind variables are `:name` or `:1`, numbered in order of first
appearance, and names fold to upper case in result columns.

```rust
use eggdb::Database;
use eggdb::sql::{Dialect, Numeric, Parameter, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    # let path = std::env::temp_dir().join(format!("eggdb-guide-oracle-{}.db", std::process::id()));
    let db = Database::open(&path)?;
    let store = db.sql_store();
    let mut sql = store.session(Dialect::Oracle);
    sql.execute(
        "CREATE TABLE emp (empno NUMBER(4) PRIMARY KEY, ename VARCHAR2(10), comm NUMBER(7,2));
         INSERT INTO emp VALUES (7839, 'KING', NULL);
         INSERT INTO emp VALUES (7499, 'ALLEN', 300);
         COMMIT",
        &mut |_| {},
    )?;
    let allen = [Parameter { declared_type: None, value: Some("7499".into()) }];
    let mut rows = Vec::new();
    sql.execute_prepared("SELECT ename, NVL(comm, 0) AS comm FROM emp WHERE empno = :id", &allen, &mut |result| {
        let names: Vec<String> = result.columns.iter().flatten().map(|column| column.name.clone()).collect();
        assert_eq!(names, ["ENAME", "COMM"]);
        rows.extend(result.rows);
        true
    })?;
    assert_eq!(rows, vec![vec![Value::Text("ALLEN".into()), Value::Numeric(Numeric::parse("300.00")?)]]);
    assert_eq!(Dialect::Oracle.value_text(&rows[0][1]).as_deref(), Some("300"));
    if let Err(error) = sql.execute("SELECT * FROM missing", &mut |_| {}) {
        assert_eq!(error.to_string(), "ORA-00942: table or view does not exist");
    }
    sql.close()?;
    db.close()?;
    # std::fs::remove_file(&path)?;
    Ok(())
}
```

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

## Backup and compaction
`db.backup_to("app.bak")` writes a copy of the last commit to a new file while the database keeps
working: readers and the writer go on, and commits made while it runs are not in the copy. The
copy is rebuilt key by key, with every collection, SQL table and time to live, so it has no free
or leaked pages; it is built as `app.bak.backing-up`, synced, checked (its entries counted
again) and only then renamed to `app.bak`, so a crash never leaves part of a copy. A backup never
replaces a file: an existing `app.bak`, or a `app.bak.backing-up` left by a backup that did not
finish, fails with `ErrorKind::AlreadyExists`. It returns a `BackupReport` (the commit's stamp,
the entries, the copy's pages and bytes). A writer that changes more pages than the snapshot
memory holds while a long backup runs makes it fail with `SnapshotTooOld`; give it more
`snapshot_memory_bytes`, or back up again.

`db.compact()` rewrites the database's file without free or leaked pages and puts it in place
atomically, then closes the database: the file is checked first (a damaged one is refused and
left as it is), rebuilt as `app.db.compacting`, synced, checked and renamed over `app.db`, so a
crash leaves the old file or the compacted one. `Database::compact_file("app.db")` opens the file
read-only and compacts it, so programs reading it keep working (they read the file they opened
until they open it again); one that has it open for writing makes it fail with `Locked`. It
returns a `CompactReport` (pages and bytes before and after, the free and leaked pages
reclaimed). Both are [decision D141](decisions/D141.md).

```rust,no_run
use eggdb::Database;

fn main() -> eggdb::Result<()> {
    let db = Database::open("app.db")?;
    let backup = db.backup_to("app.bak")?;
    println!("{} entries in {} bytes", backup.entries, backup.file_bytes);
    let compacted = db.compact()?;
    println!("{} pages, then {}", compacted.pages_before, compacted.pages_after);
    Ok(())
}
```

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
| `CollectionNotFound`, `InvalidCollectionName` | no collection of that name, or a name no collection can have |
| `ReservedKey` | a key of the default key space that starts with the bytes kept for collections |
| `NotAnInteger` | `increment` on a value that is not a counter, or a sum outside `i64`'s range |
| `AlreadyExists` | a backup's destination exists, or a staging file a backup or a compaction left |
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

## Large values, streamed
`get_reader` on a read or write transaction gives a value as a `std::io::Read`, reading its
overflow pages one at a time, so a value of any size goes to a file or a socket without being
held whole in memory. `put_reader` stores a value of a given length from any reader the same
way:

```rust,no_run
use std::fs::File;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = eggdb::Database::open("app.db")?;
    let mut photo = File::open("photo.jpg")?;
    let len = photo.metadata()?.len();
    db.write(|tx| tx.put_reader("photo", len, &mut photo))?;
    let tx = db.begin_read()?;
    if let Some(mut reader) = tx.get_reader("photo")? {
        std::io::copy(&mut reader, &mut File::create("copy.jpg")?)?;
    }
    Ok(())
}
```

A reader keeps its transaction's snapshot: a commit that replaces the value while it is being
read does not change what it reads. A source that fails, ends before the length given or holds
more rolls the transaction back. Written pages stay in the page cache until the commit, like
every page a write transaction changes, so a value still needs about its size in cache
(`Options::cache_bytes`).

## EggDB's memory allocator
A program can hand all of its memory, EggDB's included, to EggDB's own allocator. It keeps a
cache of free blocks per thread for each size up to 32 KiB, so most allocations and frees take
no lock, and it can stop at a budget on devices with a fixed amount of memory:

```rust
#[global_allocator]
static ALLOCATOR: eggdb_platform::EggAllocator = eggdb_platform::EggAllocator::with_budget(64 << 20);
```

With a budget, an allocation that would hold more than that many bytes from the system fails
the way an out-of-memory allocation does. `EggAllocator::new()` has no budget. The `eggdb`
binary uses it when built with `--features egg-allocator`; the default build keeps the system
allocator until the benchmark line `allocator.small_blocks` shows ours ahead on every system.

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
- **Size of values:** at most 1 GiB (`db.max_value_bytes()`). `get` and `put` take the value
  whole; `get_reader` and `put_reader` stream it a page at a time (see
  [Large values, streamed](#large-values-streamed)). A value being written still needs about its
  size in page cache until the commit.
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
- **Collections** share the file's one B+tree, each under its own key prefix (D139), so a
  collection's keys are ten bytes shorter than the default key space's, and dropping one
  deletes its keys one by one in a single transaction.
- **No in-memory database** yet: every database is a file (D87).
- **The file format** is version 1 ([format.md](format.md)). Every later release reads the
  files v0.2.0 writes, and CI keeps opening the v0.2.0 sample files to hold it to that (D20,
  D96).
