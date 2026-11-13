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
  (`eggdb_scan_free`, `eggdb_copy_free`, `eggdb_free`) return nothing.
- **Opaque handles.** `eggdb_db` (from `eggdb_open`), `eggdb_txn` (from `eggdb_begin_read` and
  `eggdb_begin_write`), `eggdb_scan` (from `eggdb_scan_open`), `eggdb_sql` (from
  `eggdb_sql_open` and `eggdb_sql_connect`), `eggdb_stmt` (from `eggdb_sql_prepare`) and
  `eggdb_copy` (from `eggdb_sql_copy_in_begin` and `eggdb_sql_copy_out_begin`). On a failure the
  handle written back is NULL. Ending a handle with NULL does nothing.
- **Bytes have lengths.** Keys and values are bytes with a length and may hold zeros; a NULL
  pointer is allowed only with a length of 0. Paths, collection names, dialects and SQL text are
  NUL-terminated UTF-8 (the text given to `eggdb_sql_prepare` and the `eggdb_stmt_bind_text`
  values have a length instead); text that is not UTF-8 fails with `EGGDB_ERR_INVALID_ARGUMENT`.
- **Who frees what.** A value from `eggdb_get` and the names from `eggdb_collection_names` are
  the caller's: free each with `eggdb_free(pointer, length)` exactly once. A scan's batch and a
  SQL statement's results, a stepped row and a copy's piece belong to their handle and are read
  in place until that handle's next call, so reading a thousand rows costs one call, not one per
  value (D17). Everything
  else the library returns (`eggdb_version`, `eggdb_status_name`, result tags and column names)
  is valid as documented and never freed by the caller.
- **Optional outputs.** Pointers for outcomes such as `deleted`, `created`, `stamp` or a report
  may be NULL when the caller does not want them; pointers for results the call exists to give
  (a value, a handle, a batch) may not.
- **Threads.** An `eggdb_db` may be used by any number of threads at once. A transaction, a scan
  or a SQL session and the statements and copies made on it are used by one call at a time,
  from any thread; `eggdb_sql_cancel` is the one call that may reach a session from another
  thread while it runs a statement. Read transactions run beside
  each other and beside the one write transaction. `eggdb_begin_write` waits up to five seconds
  for the open write transaction to end, then fails with `EGGDB_ERR_WRITER_ACTIVE`; on the thread
  that began the open one it fails at once, since it would wait for itself. SQL takes the calling
  thread to have at least 2 MiB of stack and ends nesting that would use it up with `54001`; call
  from a thread with at least that much (a Windows main thread has 1 MiB, a macOS secondary
  thread 512 KiB), and from one with 16 MiB to run deeply nested routines and triggers.
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

`eggdb_open` takes NULL options for the defaults. The path `:memory:` opens a new, private
database in memory, as SQLite's does: each open is its own database, gone once it is closed
(read-only is refused for it). The fields of `eggdb_options`:

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
change the collections in a write transaction (a drop takes the name away at once and removes up
to 512 keys; later commits remove the rest, D292); `eggdb_collection_names(txn, &names, &length,
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
| `eggdb_set_power_source(db, source)` | `EGGDB_POWER_MAINS`, `EGGDB_POWER_BATTERY` or `EGGDB_POWER_LOW_BATTERY`: on a battery the database checkpoints less often, on a low one only when it must (D1087); pass what Android's `BatteryManager` and power-save mode or iOS's battery state and Low Power Mode report |
| `eggdb_suspend(db)`, `eggdb_resume(db)` | when the app goes to the background and comes back: `eggdb_suspend` checkpoints, syncs and releases the file's lock, so iOS never suspends the app holding one, and new transactions fail with `EGGDB_ERR_SUSPENDED`; `eggdb_resume` takes the lock back, `EGGDB_ERR_LOCKED` while an app extension holds the file, `EGGDB_ERR_NEEDS_REOPEN` when another program changed it (D1088) |

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

### Prepared statements

```c
const char *text = "INSERT INTO items VALUES ($1, $2)";
eggdb_stmt *insert = NULL;
eggdb_sql_prepare(sql, text, strlen(text), &insert);
for (int64_t id = 10; id < 1000; id++) {
    eggdb_stmt_bind_int64(insert, 1, id);
    eggdb_stmt_bind_text(insert, 2, "tea", 3);
    eggdb_stmt_execute(insert, &results, &count);
}
eggdb_stmt_finalize(insert);
```

- `eggdb_sql_prepare(sql, text, len, &stmt)` checks one statement (UTF-8, `len` bytes, no NUL
  needed) once: a syntax error, a second statement, or a table or column that does not exist
  fails there with `EGGDB_ERR_SQL`. `eggdb_stmt_parameter_count` gives how many parameters it
  takes, and `eggdb_stmt_column_count`, `eggdb_stmt_column_name` and `eggdb_stmt_column_type`
  the columns of its rows when they are known before it runs (the kind their values come back
  with, `EGGDB_NULL` when only the run tells; 0 columns for a statement that returns no rows).
- Parameters are numbered from 1 (`$1`, or the first `?` in SQLite's and MySQL's dialects) and
  are NULL until bound: `eggdb_stmt_bind_null`, `eggdb_stmt_bind_int64` (a `bigint`),
  `eggdb_stmt_bind_double` (a `double precision`, exactly, infinities and NaN included),
  `eggdb_stmt_bind_bool`, `eggdb_stmt_bind_text` (read as a literal of the type its place asks
  for, so dates, numerics and JSON bind as text) and `eggdb_stmt_bind_blob` (any bytes, as
  binary data). Each value is copied, so the caller's buffer may change right after the call.
  The statement converts a typed value to the type its place asks for, as PostgreSQL's
  extended protocol does with a typed parameter. An index out of range fails with
  `EGGDB_ERR_INVALID_ARGUMENT`.
- `eggdb_stmt_execute(stmt, &results, &count)` runs it with the values bound now and gives its
  result as `eggdb_sql_execute` does. The results belong to the statement, so they stay valid
  until its next `eggdb_stmt_execute`, `eggdb_stmt_reset` or `eggdb_stmt_finalize`, whatever
  the session runs meanwhile. Bindings stay for the next run; `eggdb_stmt_clear_bindings` sets
  them back to NULL, and `eggdb_stmt_reset` frees the last results.
- A statement belongs to its session: a call on it counts as a call on the session, and
  `eggdb_sql_close` fails with `EGGDB_ERR_IN_USE` while statements prepared on it are not
  finalized. Today the statement is parsed again at each run (the engine does not keep a
  parsed plan between runs yet, D268); preparing still saves checking, quoting and
  converting values, and one crossing per value.

### Reading rows one at a time

```c
eggdb_stmt *select = NULL;
eggdb_sql_prepare(sql, "SELECT id, name FROM items", 26, &select);
int has_row = 0;
while (eggdb_stmt_step(select, &has_row) == EGGDB_OK && has_row) {
    int64_t id = 0;
    const char *name = NULL;
    size_t name_len = 0;
    eggdb_stmt_value_int64(select, 0, &id);
    eggdb_stmt_value_text(select, 1, &name, &name_len);
}
eggdb_stmt_finalize(select);
```

- `eggdb_stmt_step(stmt, &has_row)` gives the next row (`has_row` 1) or the end (0). The first
  step runs the statement with the values bound now; after the end it keeps giving 0 until
  `eggdb_stmt_reset`, which also ends a run part way, or `eggdb_stmt_execute`, which ends it and
  runs the statement whole. A failure ends the run with `EGGDB_ERR_SQL`, as
  `eggdb_sql_execute` reports one.
- `eggdb_stmt_row(stmt, &values, &count)` lends the current row as an array of `eggdb_value`
  (one crossing per row), and the getters read one value: `eggdb_stmt_value_kind`,
  `eggdb_stmt_value_int64` (integers and booleans, reals with no fraction, and decimal text such
  as a `numeric`), `eggdb_stmt_value_double` (reals, integers and numeric text, `Infinity` and
  `NaN` too), `eggdb_stmt_value_text` (any value's text, NUL-terminated; NULL gives a NULL
  pointer; binary data is refused) and `eggdb_stmt_value_blob` (binary data's bytes, or any
  other value's text). A value that does not read as the asked type, NULL included, fails with
  `EGGDB_ERR_INVALID_ARGUMENT` and leaves 0. The row stays valid until the next step, reset or
  finalize. Once a run has started, `eggdb_stmt_column_count`, `_name` and `_type` give its
  columns.
- From the first step to the end (or a reset), the session runs nothing else: other calls that
  run statements on it fail with `EGGDB_ERR_IN_USE`.
- On a session from `eggdb_sql_connect` each step reads one row from the connection, so a
  result larger than memory can be read. In the process the engine still gives a statement's
  rows at once; each step then makes one row's values, so the printed text of every value is
  never held at once.

### Cancelling a statement

`eggdb_sql_cancel(sql)` may be called from any thread while another thread runs a statement on
the session (`eggdb_sql_execute`, `eggdb_sql_execute_with_parameters`, `eggdb_sql_copy_in`,
`eggdb_stmt_execute`, `eggdb_stmt_step` or a `eggdb_copy_*` call). On a session from
`eggdb_sql_connect` it sends the server PostgreSQL's cancel request (the process ID and secret
key the server gave the connection) over a new connection with the same TLS mode, and returns
once the server has read it; a server that cannot be reached fails with `EGGDB_ERR_IO`. The statement stops at its next row (within ten milliseconds in
`pg_sleep`), and its call returns `EGGDB_ERR_SQL` with SQLSTATE `57014`; it is rolled back as
any failing statement is, and a `BEGIN` block it was in fails as PostgreSQL's does. A cancel
that comes while nothing runs does nothing. The session must stay open while
`eggdb_sql_cancel` runs.

### Bulk loading with COPY

```c
const char *rows = "1\tAda\n2\tGrace\n";
eggdb_sql_copy_in(sql, "COPY people FROM STDIN", (const uint8_t *)rows, strlen(rows),
                  &results, &count);
```

`eggdb_sql_copy_in(sql, text, data, len, &results, &count)` runs `text` with `data` as the data
of its `COPY ... FROM STDIN`, in the text, CSV or binary format the `COPY`'s options name, the
same machinery `psql` and `pg_dump` files use ([sql.md](sql.md)). The `COPY` loads every row or
none, and its result's tag is `"COPY <rows>"`. A second `COPY ... FROM STDIN` in the same text
and `COPY ... TO STDOUT` fail with `EGGDB_ERR_SQL`.

Data that is not in one buffer goes piece by piece:

```c
FILE *file = fopen("people.csv", "rb");
uint8_t buffer[65536];
size_t read = 0;
eggdb_copy *copy = NULL;
eggdb_sql_copy_in_begin(sql, "COPY people FROM STDIN (FORMAT csv)", &copy);
while ((read = fread(buffer, 1, sizeof buffer, file)) > 0) {
    eggdb_copy_put(copy, buffer, read);
}
eggdb_copy_end(copy, &results, &count);
eggdb_copy_free(copy);
```

- `eggdb_sql_copy_in_begin(sql, text, &copy)` starts the one `COPY ... FROM STDIN` in `text`;
  `eggdb_copy_put(copy, data, len)` gives it the next bytes, cut anywhere (inside a row or a
  character too), copied or sent before the call returns; `eggdb_copy_end(copy, &results,
  &count)` ends it and gives its result (`"COPY <rows>"`): every row loads or none, with
  `EGGDB_ERR_SQL` and the SQLSTATE of a bad row. `eggdb_copy_free(copy)` frees the handle; a
  copy freed before its end loads nothing.
- `eggdb_sql_copy_out_begin(sql, "COPY people TO STDOUT", &copy)` starts a `COPY ... TO STDOUT`,
  and `eggdb_copy_read(copy, &data, &len)` lends its next piece (NULL and 0 at the end), in the
  text, CSV or binary format the `COPY` names; `eggdb_copy_end` gives its result, reading to
  the end first when the caller stopped early.
- While a copy is open the session runs nothing else (`EGGDB_ERR_IN_USE`), and
  `eggdb_sql_close` fails with `EGGDB_ERR_IN_USE` until it is freed. A statement that is not the
  `COPY` the call expects runs, then fails with `EGGDB_ERR_SQL` and SQLSTATE `0A000`.
- On a session from `eggdb_sql_connect` both stream: each put is sent to the server as it comes,
  and each read takes the server's next messages (gathered up to 64 KiB). In the process the
  engine reads a `COPY`'s data inside one call, so the pieces put are kept until
  `eggdb_copy_end`, and a `COPY ... TO STDOUT` runs at `eggdb_sql_copy_out_begin` with its data
  waiting in the handle, read in pieces of at most 64 KiB.

### On a server

`eggdb_sql_connect("postgresql://user:password@host:5432/db?sslmode=require", &sql)` gives a
session on an EggDB server (or any server that speaks PostgreSQL's protocol) through EggDB's own
client (D192). It takes the same calls as a session from `eggdb_sql_open`:
`eggdb_sql_execute` sends the simple query protocol, `eggdb_sql_execute_with_parameters` the
extended one with the parameters bound as text to `$1`, `$2`, ..., and values come back typed
from the columns' types (integers, reals, `bytea` as a blob, the rest as text).
`eggdb_sql_close` ends the connection. The official drivers use it when they are given a
`postgresql://` URL instead of a file. Every other SQL call works on it too:
`eggdb_sql_prepare` prepares a named statement on the server (Parse and Describe, so its
parameter count and columns come from the server) that `eggdb_stmt_finalize` closes with the
session's next call; bound values go as text, typed by their place in the statement (so
`SELECT $1` with an integer gives text back, where a session in the process gives an integer);
`eggdb_stmt_step` reads rows from the connection one at a time; `eggdb_sql_copy_in` and the
`eggdb_copy_*` calls use PostgreSQL's copy sub-protocol; and `eggdb_sql_cancel` sends a cancel
request. A connection that breaks fails that call with `EGGDB_ERR_IO`, and every later call on
the session fails the same way until it is closed.

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
| 24 | `EGGDB_ERR_SUSPENDED` | the database is suspended (`eggdb_suspend`); call `eggdb_resume` |
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

## On Android and iOS

**Android (Kotlin).** `drivers/kotlin` is a Kotlin class, `app.eggspot.eggdb.EggDB`, over a small
JNI layer in C (`drivers/kotlin/jni/eggdb_jni.c`) that calls this C API (D222). Build the C API for
the app's ABIs with the Android NDK's linker (`cargo build --release -p eggdb-capi --target
aarch64-linux-android`, and `armv7-linux-androideabi`, `x86_64-linux-android` as needed), build
`eggdb_jni.c` against it in the app's `CMakeLists.txt`, and put both `.so` files in `jniLibs`:

```kotlin
EggDB(File(context.filesDir, "app.eggdb").path).use { db ->
    db.execute("CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, body TEXT)")
    val rows = db.execute("SELECT body FROM notes").single().rows
}
```

Or take the Android Archive, which does all of that (D1092): `scripts/android/build-aar.sh` builds the
C API for `arm64-v8a`, `armeabi-v7a`, `x86_64` and `x86` with the NDK's clang and assembles
`drivers/android` (a Gradle library module over the same Kotlin sources and JNI file) into
`drivers/android/build/outputs/aar/eggdb-android-release.aar`; add it to an app with
`implementation(files("libs/eggdb-android-release.aar"))`. Its minimum is Android 6 (API 23).

Text crosses JNI as UTF-8 bytes, never as Java's modified UTF-8, so every character survives;
values come back as `Value.Integer`, `Real`, `Text`, `Blob` or `Null`. The same class runs on any
JVM, which is how CI checks it, and CI's `android` job runs the module's device tests
(`connectedDebugAndroidTest`) on Android 11 emulators, one 64-bit (`x86_64`) and one 32-bit
(`x86`), with the archive kept as the run's `eggdb-android-aar` artifact.

**iOS (Swift).** `scripts/apple/xcframework.sh` builds the C API for iOS devices, the iOS
simulator and macOS into `EggDB.xcframework` with a `CEggDB` module map; add it to an Xcode
project and use it from Swift directly, or through `drivers/swift`.

Mobile systems stop apps in the background (Android's Doze, iOS's suspension) and may kill them
without warning. EggDB needs nothing special for that: a committed transaction is on disk when
`eggdb_commit` or the statement returns (with the default durability), and an interrupted one
is rolled back when the file opens next. Keep the database in the app's own files directory.

## How it is tested
- `crates/eggdb-capi/tests/all/api.rs` calls every function as a C program would: commits and
  rollbacks, scans in batches in every direction and across transactions, collections, times
  to live, counters and compare-and-set, SQL in two dialects with parameters and blocks,
  backup and compaction, refusing to close while in use, threads sharing a handle, and every
  misuse (NULL pointers, bad UTF-8, unknown flags and dialects) answered with a status.
- `crates/eggdb-capi/tests/all/statements.rs` prepares statements once and runs them many times
  with every typed binding, reads their column outlines, cancels a running `pg_sleep` from
  another thread, bulk-loads rows with `COPY` (text, CSV, a failing row, more data than one
  piece), and checks every misuse of a statement.
- `crates/eggdb-capi/tests/all/streaming.rs` runs each scenario on a session in the process and on
  one from `eggdb_sql_connect` to a server in the same process: stepping through 490 rows with
  every getter, NULLs, a failing division and a statement with no rows, the session refusing
  other calls while a run or a copy is open, `COPY ... TO STDOUT` of 5,000 rows read in pieces,
  ended early and freed early, a `COPY ... FROM STDIN` fed in 7-byte pieces that cut rows and
  characters, a failing row, an abandoned copy, a cancelled `pg_sleep`, and 4,200 statements
  prepared and finalized on a server (more than it keeps at once).
- `crates/eggdb-capi/tests/all/header.rs` reads `eggdb.h` and checks it against the Rust code:
  every constant and its value, every exported function and its number of parameters, every
  struct's fields and types in order.
- `scripts/c-api/check.sh` compiles `crates/eggdb-capi/tests/c/api_check.c` with the system C
  compiler (warnings as errors), checks that the header compiles as C++, runs the program, and
  runs the Python example. CI runs it on Linux and macOS (Python alone on Windows), and the
  release workflow on every release library its runner can load.

## Not yet
Prepared statements that keep their parsed plan between runs (D17's fifth rule; today a
prepared statement is checked once and parsed again at each run), rows streamed out of the
engine itself in the process (it gives a statement's rows, and takes a `COPY`'s data, inside
one call, D293), typed parameters declared to a server at prepare time, binary result
formats over `eggdb_sql_connect`, asynchronous calls, the
options `write_timeout`, `snapshot_memory_bytes`, `auto_checkpoint_log_bytes` and
`page_size_bytes`, `eggdb_check` and `eggdb_stats`, and the official per-language drivers
(D16).
