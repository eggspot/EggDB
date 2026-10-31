# EggDB from C and any other language: the C API

EggDB's embedded database is also a C library, so any language that can call C can open an
EggDB file in its own process: C and C++ directly, Python through `ctypes`, .NET through
P/Invoke, Java through its Foreign Function and Memory API, Go through cgo, and the rest through
their own foreign-function interfaces. The API is one header,
[`crates/eggdb-capi/include/eggdb.h`](../crates/eggdb-capi/include/eggdb.h), and one library
built from the `eggdb-capi` crate (D142). It covers what the Rust API covers: opening and
closing, read and write transactions, keys and values, scans, collections, expiring keys,
counters and compare-and-set, SQL in every dialect with parameters, backup and compaction.
The official per-language drivers (D16) will be thin layers over this same library.

## Getting the library
Each GitHub Release has an `eggdb-capi-<version>-<target>` archive for Linux (glibc), macOS and
Windows next to the `eggdb` binary, with `include/eggdb.h`, the shared and the static library in
`lib/`, the Python example and this page. From the repository:

```sh
cargo build --release -p eggdb-capi
```

builds them in `target/release/`:

| System | Shared library | Static library |
|--------|----------------|----------------|
| Linux, the BSDs | `libeggdb_capi.so` | `libeggdb_capi.a` |
| macOS | `libeggdb_capi.dylib` | `libeggdb_capi.a` |
| Windows | `eggdb_capi.dll` (import library `eggdb_capi.dll.lib`) | `eggdb_capi.lib` |

The library depends on nothing but the operating system. Linking the static library also needs
the system libraries Rust's standard library uses;
`RUSTFLAGS='--print native-static-libs' cargo build --release -p eggdb-capi` prints them for the
system at hand.

```sh
cc -I crates/eggdb-capi/include app.c -L target/release -leggdb_capi -Wl,-rpath,target/release -o app
```

## A first program

```c
#include "eggdb.h"
#include <stdio.h>

int main(void) {
    eggdb_db *db = NULL;
    if (eggdb_open("app.db", NULL, &db) != EGGDB_OK) {
        fprintf(stderr, "%s\n", eggdb_last_error());
        return 1;
    }
    eggdb_txn *txn = NULL;
    eggdb_begin_write(db, &txn);
    eggdb_put(txn, NULL, (const uint8_t *)"user:1", 6, (const uint8_t *)"Ada", 3);
    if (eggdb_commit(txn, NULL) != EGGDB_OK) {
        fprintf(stderr, "%s\n", eggdb_last_error());
    }

    eggdb_begin_read(db, &txn);
    uint8_t *value = NULL;
    size_t value_len = 0;
    eggdb_get(txn, NULL, (const uint8_t *)"user:1", 6, &value, &value_len);
    if (value != NULL) {
        printf("%.*s\n", (int)value_len, (const char *)value);
        eggdb_free(value, value_len);
    }
    eggdb_rollback(txn);
    return eggdb_close(db) == EGGDB_OK ? 0 : 1;
}
```

## The rules every call follows
- **A status for every call.** Each function returns `EGGDB_OK` (0) or an `EGGDB_ERR_*` code
  (below); `eggdb_status_name(status)` names it. After a failure, `eggdb_last_error()` gives the
  message for the calling thread (what happened and what to do, written for people and for AI
  models), and `eggdb_last_sqlstate()` the SQLSTATE of a failed SQL statement. Both stay valid
  until the next failing call on that thread. The functions that end a handle
  (`eggdb_scan_free`, `eggdb_free`) return nothing.
- **Opaque handles.** `eggdb_db` (from `eggdb_open`), `eggdb_txn` (from `eggdb_begin_read` and
  `eggdb_begin_write`), `eggdb_scan` (from `eggdb_scan_open`) and `eggdb_sql` (from
  `eggdb_sql_open`). On a failure the handle written back is NULL. Ending a handle with NULL
  does nothing.
- **Bytes have lengths.** Keys and values are bytes with a length and may hold zeros; a NULL
  pointer is allowed only with a length of 0. Paths, collection names, dialects and SQL text are
  NUL-terminated UTF-8; text that is not UTF-8 fails with `EGGDB_ERR_INVALID_ARGUMENT`.
- **Who frees what.** A value from `eggdb_get` and the names from `eggdb_collection_names` are
  the caller's: free each with `eggdb_free(pointer, length)` exactly once. A scan's batch and a
  SQL statement's results belong to their handle and are read in place until that handle's
  next call, so reading a thousand rows costs one call, not one per value (D17). Everything
  else the library returns (`eggdb_version`, `eggdb_status_name`, result tags and column names)
  is valid as documented and never freed by the caller.
- **Optional outputs.** Pointers for outcomes such as `deleted`, `created`, `stamp` or a report
  may be NULL when the caller does not want them; pointers for results the call exists to give
  (a value, a handle, a batch) may not.
- **Threads.** An `eggdb_db` may be used by any number of threads at once. A transaction, a scan
  or a SQL session is used by one call at a time, from any thread. Read transactions run beside
  each other and beside the one write transaction. `eggdb_begin_write` waits up to five seconds
  for the open write transaction to end, then fails with `EGGDB_ERR_WRITER_ACTIVE`; on the thread
  that began the open one it fails at once, since it would wait for itself.
- **No crash on bad input.** NULL where a pointer is needed, unknown flags, bad names and hostile
  bytes come back as statuses. EggDB never lets input reach a panic (D9); if a bug panics anyway,
  it never unwinds into the caller: a build that unwinds returns `EGGDB_ERR_INTERNAL`, and a
  release build, which aborts on panic for speed, stops the process. Passing a pointer that does
  not point where the header says (a freed handle, a short buffer) is undefined behavior, as in
  any C library.

## Opening and closing

```c
eggdb_options options = {EGGDB_OPEN_NO_CREATE, EGGDB_DURABILITY_NORMAL, 64u << 20};
eggdb_db *db = NULL;
int status = eggdb_open("app.db", &options, &db);
```

`eggdb_open` takes NULL options for the defaults. The fields of `eggdb_options`:

| Field | Values | Default (0) |
|-------|--------|-------------|
| `flags` | `EGGDB_OPEN_READ_ONLY` (several read-only opens share the file), `EGGDB_OPEN_NO_CREATE` (a missing file fails with `EGGDB_ERR_NOT_FOUND`) | create the file if missing, open for writing |
| `durability` | `EGGDB_DURABILITY_FULL`, `EGGDB_DURABILITY_NORMAL`, `EGGDB_DURABILITY_OFF`, as in [embedded.md](embedded.md#durability) | full |
| `cache_bytes` | the page cache, which also bounds one write transaction | 16 MiB |

`eggdb_close(db)` writes a checkpoint and frees the handle. While transactions or SQL sessions
still use the database it fails with `EGGDB_ERR_IN_USE` and leaves it open: end them first.

## Transactions
`eggdb_begin_read` takes a snapshot of the last commit; `eggdb_begin_write` begins the one write
transaction. `eggdb_commit(txn, &stamp)` commits every change of a write transaction atomically
and gives the commit's stamp (a read transaction just ends and gives its snapshot's);
`eggdb_rollback(txn)` drops the changes. Both free the handle whatever the status. A change
through a read transaction fails with `EGGDB_ERR_READ_TRANSACTION`.

## Keys and values
Every key and value call takes a collection name first; NULL is the default key space.

| Call | What it does |
|------|--------------|
| `eggdb_get(txn, collection, key, key_len, &value, &value_len)` | the value, or a NULL `value` when the key is absent or expired; an empty value is a non-NULL pointer with a length of 0 |
| `eggdb_put(txn, collection, key, key_len, value, value_len)` | stores the value, replacing any value and time to live |
| `eggdb_delete(txn, collection, key, key_len, &deleted)` | removes the key; `deleted` is 1 when it was there |

Keys may be up to an eighth of a page (512 bytes with 4 KiB pages) and values up to 1 GiB, as in
[embedded.md](embedded.md#limits-in-this-release); a longer one fails with `EGGDB_ERR_TOO_LARGE`
and changes nothing.

## Scans

```c
eggdb_scan *scan = NULL;
eggdb_scan_open(txn, NULL, (const uint8_t *)"user:", 5, NULL, 0, EGGDB_SCAN_PREFIX, &scan);
for (;;) {
    const eggdb_entry *entries = NULL;
    size_t count = 0;
    if (eggdb_scan_next(txn, scan, 0, &entries, &count) != EGGDB_OK || count == 0) {
        break;
    }
    for (size_t i = 0; i < count; i++) {
        printf("%.*s\n", (int)entries[i].key_len, (const char *)entries[i].key);
    }
}
eggdb_scan_free(scan);
```

A scan covers the keys from `start` (NULL: the first) up to `end` (NULL: the last; left out
unless `EGGDB_SCAN_END_INCLUSIVE`), or the keys beginning with `start` with `EGGDB_SCAN_PREFIX`;
`EGGDB_SCAN_REVERSE` reads them from the last down. `eggdb_scan_next` gives up to `max_entries`
entries (0: 256), fewer once about a mebibyte is gathered, and a count of 0 at the end. The
batch stays valid until the scan's next call. A scan remembers where it stopped, not a
transaction: each batch is read in the transaction passed with it, so a long scan can continue
in a new read transaction, and a write transaction sees its own writes that lie ahead of the
scan.

## Collections
`eggdb_create_collection(txn, name, &created)` and `eggdb_drop_collection(txn, name, &dropped)`
change the collections in a write transaction; `eggdb_collection_names(txn, &names, &length,
&count)` lists them as one buffer of NUL-terminated names in name order. Every key call takes a
collection name; one that does not exist fails with `EGGDB_ERR_COLLECTION_NOT_FOUND`. See
[embedded.md](embedded.md#collections).

## Expiring keys and counters

| Call | What it does |
|------|--------------|
| `eggdb_put_with_expiry(..., ttl_ms)` | stores a value that expires after `ttl_ms` milliseconds |
| `eggdb_set_expiry(..., ttl_ms, &found)` | gives an existing key a time to live; 0 removes it |
| `eggdb_clear_expiry(..., &cleared)` | the key no longer expires |
| `eggdb_time_to_live(..., &ttl_ms)` | milliseconds left, rounded up, or -1 for none or absent |
| `eggdb_increment(..., by, &count)` | adds `by` (negative subtracts) to a decimal counter; absent counts as 0; not a counter or out of `int64_t` fails with `EGGDB_ERR_NOT_AN_INTEGER` |
| `eggdb_compare_and_set(..., expected, expected_len, new_value, new_value_len, &swapped)` | stores `new_value` (NULL: removes the key) only when the value is `expected` (NULL: absent) |
| `eggdb_remove_expired(db, &removed)` | removes every expired key now; commits also remove up to 32 each |

## SQL

```c
eggdb_sql *sql = NULL;
eggdb_sql_open(db, "postgresql", &sql);
const eggdb_result *results = NULL;
size_t count = 0;
eggdb_sql_execute(sql, "CREATE TABLE items (id integer PRIMARY KEY, name text)", &results, &count);

eggdb_parameter parameters[] = {{(const uint8_t *)"1", 1}, {(const uint8_t *)"O'Brien", 7}};
eggdb_sql_execute_with_parameters(sql, "INSERT INTO items VALUES ($1, $2)", parameters, 2,
                                  &results, &count);

eggdb_sql_execute(sql, "SELECT id, name FROM items", &results, &count);
for (size_t row = 0; row < results[0].row_count; row++) {
    const eggdb_value *name = eggdb_result_value(&results[0], row, 1);
    printf("%lld %s\n", (long long)eggdb_result_value(&results[0], row, 0)->integer,
           name->kind == EGGDB_NULL ? "NULL" : (const char *)name->data);
}
eggdb_sql_close(sql);
```

- `eggdb_sql_open(db, dialect, &sql)` opens a session in `"postgresql"` (NULL), `"mysql"`,
  `"sqlite"`, `"oracle"` or `"sqlserver"` (and the other names `--dialect` accepts); every
  dialect works on the same tables ([sql.md](sql.md)). Outside a `BEGIN` block each statement is
  its own transaction; a block holds the database's one write transaction until `COMMIT` or
  `ROLLBACK`, and `eggdb_sql_close` rolls back a block left open.
- `eggdb_sql_execute` runs every statement of the text and gives one `eggdb_result` per
  statement: its command tag (`"SELECT 2"`, `"INSERT 0 1"`), its column names, and its values
  row by row in one array, read with the inline `eggdb_result_value(result, row, column)`.
- `eggdb_sql_execute_with_parameters` does the same with values for the placeholders (`$1` in
  PostgreSQL's and Oracle's dialects, `?` in SQLite's and MySQL's), passed as text in an array
  of `eggdb_parameter` (a NULL `text` is SQL's NULL). Values never need quoting into the SQL.
- A value's `kind` is `EGGDB_NULL`, `EGGDB_INTEGER` (booleans as 1 and 0, in `integer`),
  `EGGDB_REAL` (in `real`), `EGGDB_BLOB` (the bytes in `data`) or `EGGDB_TEXT` (everything else:
  text, numerics, dates, JSON, UUIDs). Every value but NULL also has its text in `data`, as the
  dialect's own clients print it, followed by a NUL byte not counted in `len`.
- A failing statement returns `EGGDB_ERR_SQL` with no results; `eggdb_last_sqlstate()` gives
  its SQLSTATE (`42P01` for a missing table) and `eggdb_last_error()` its message. The
  statements before it keep their effects.

### On a server

`eggdb_sql_connect("postgresql://user:password@host:5432/db?sslmode=require", &sql)` gives a
session on an EggDB server (or any server that speaks PostgreSQL's protocol) through EggDB's own
client (D192). It takes the same calls as a session from `eggdb_sql_open`:
`eggdb_sql_execute` sends the simple query protocol, `eggdb_sql_execute_with_parameters` the
extended one with the parameters bound as text to `$1`, `$2`, ..., and values come back typed
from the columns' types (integers, reals, `bytea` as a blob, the rest as text).
`eggdb_sql_close` ends the connection. The official drivers use it when they are given a
`postgresql://` URL instead of a file.

## Backup and compaction
`eggdb_backup(db, path, &report)` writes a copy of the last commit to a new file while readers
and the writer keep working; a file at `path` fails with `EGGDB_ERR_ALREADY_EXISTS`.
`eggdb_compact(db, &report)` rewrites the database's file without free or leaked pages, puts it
in place atomically and closes the database, freeing the handle (unless it fails with
`EGGDB_ERR_IN_USE`). `eggdb_compact_file(path, &report)` opens a file read-only and compacts it,
so programs reading it keep working. See [embedded.md](embedded.md#backup-and-compaction).

## Status codes

| Code | Name | Meaning |
|------|------|---------|
| 0 | `EGGDB_OK` | the call succeeded |
| 1 | `EGGDB_ERR_NOT_FOUND` | no database file, and the options did not allow creating one |
| 2 | `EGGDB_ERR_PERMISSION_DENIED` | the system refused access to the file or its directory |
| 3 | `EGGDB_ERR_LOCKED` | the file is open elsewhere |
| 4 | `EGGDB_ERR_READ_ONLY` | a change to a database opened read-only |
| 5 | `EGGDB_ERR_NOT_A_DATABASE` | not an EggDB file |
| 6 | `EGGDB_ERR_INCOMPATIBLE` | a file of a newer EggDB |
| 7 | `EGGDB_ERR_CORRUPT` | the file is damaged |
| 8 | `EGGDB_ERR_IO` | the file system failed |
| 9 | `EGGDB_ERR_OUT_OF_MEMORY` | memory could not be had |
| 10 | `EGGDB_ERR_TOO_LARGE` | a key or value over the limits; nothing changed |
| 11 | `EGGDB_ERR_CACHE_FULL` | a write transaction outgrew the page cache and was rolled back |
| 12 | `EGGDB_ERR_WRITER_ACTIVE` | another write transaction stayed open |
| 13 | `EGGDB_ERR_SNAPSHOT_TOO_OLD` | a read transaction's snapshot expired |
| 14 | `EGGDB_ERR_TRANSACTION_FAILED` | the write transaction failed earlier |
| 15 | `EGGDB_ERR_NEEDS_REOPEN` | a write or sync failed; close and open again |
| 16 | `EGGDB_ERR_DATABASE_FULL` | the database takes no more commits |
| 17 | `EGGDB_ERR_INVALID_OPTION` | an open option outside what EggDB accepts |
| 18 | `EGGDB_ERR_COLLECTION_NOT_FOUND` | no collection of that name |
| 19 | `EGGDB_ERR_INVALID_COLLECTION_NAME` | a name no collection can have |
| 20 | `EGGDB_ERR_RESERVED_KEY` | a key starting with the bytes kept for collections |
| 21 | `EGGDB_ERR_ALREADY_EXISTS` | a file is where a backup or compaction would put one |
| 22 | `EGGDB_ERR_NOT_AN_INTEGER` | not a counter, or a sum out of range |
| 23 | `EGGDB_ERR_OTHER` | any other failure of the database |
| 100 | `EGGDB_ERR_INVALID_ARGUMENT` | a NULL that must not be, text that is not UTF-8, an unknown flag or dialect |
| 101 | `EGGDB_ERR_IN_USE` | the database still has transactions or SQL sessions |
| 102 | `EGGDB_ERR_READ_TRANSACTION` | a change through a read transaction |
| 103 | `EGGDB_ERR_SQL` | a SQL statement failed; see `eggdb_last_sqlstate` |
| 104 | `EGGDB_ERR_INTERNAL` | a bug in EggDB; the call did nothing it reports |

Codes 1 to 23 are the Rust API's `ErrorKind`s ([embedded.md](embedded.md#errors)); later
releases may add codes, so treat an unknown one as a failure.

## From Python, with ctypes
[`examples/python/eggdb_ctypes.py`](../examples/python/eggdb_ctypes.py) wraps the library in a
few small classes with nothing but Python's standard `ctypes`: `EggDB` (open, close, backup,
compact), `Transaction` (a context manager that commits, or rolls back on an exception; `get`,
`put` with `ttl_ms`, `delete`, `scan` as a generator over batches, `increment`,
`time_to_live`, collections) and `SqlSession` (`execute` with parameters, results as Python
values). Run it to see it work:

```sh
cargo build --release -p eggdb-capi
python3 examples/python/eggdb_ctypes.py
```

It finds the library in `target/release` or `target/debug`, or takes `--library <path>` or the
`EGGDB_LIBRARY` environment variable. The official Python driver (DB-API, SQLAlchemy and Django,
D16) comes later and will be faster, reading batches through the buffer protocol instead of
`ctypes` objects (D17).

## How it is tested
- `crates/eggdb-capi/tests/api.rs` calls every function as a C program would: commits and
  rollbacks, scans in batches in every direction and across transactions, collections, times
  to live, counters and compare-and-set, SQL in two dialects with parameters and blocks,
  backup and compaction, refusing to close while in use, threads sharing a handle, and every
  misuse (NULL pointers, bad UTF-8, unknown flags and dialects) answered with a status.
- `crates/eggdb-capi/tests/header.rs` reads `eggdb.h` and checks it against the Rust code:
  every constant and its value, every exported function and its number of parameters, every
  struct's fields and types in order.
- `scripts/c-api/check.sh` compiles `crates/eggdb-capi/tests/c/api_check.c` with the system C
  compiler (warnings as errors), checks that the header compiles as C++, runs the program, and
  runs the Python example. CI runs it on Linux and macOS (Python alone on Windows), and the
  release workflow on every release library its runner can load.

## Not yet
Prepared statement handles that keep their parsed form between calls (D17's fifth rule; today a
parameterized statement is parsed at each call), parameter types declared by the caller, typed
parameter buffers instead of text, `COPY`, asynchronous calls, the options `write_timeout`,
`snapshot_memory_bytes`, `auto_checkpoint_log_bytes` and `page_size_bytes`, `eggdb_check` and
`eggdb_stats`, cancelling a running statement, and the official per-language drivers (D16).
