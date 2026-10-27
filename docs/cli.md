# The `eggdb` command

`eggdb` works with an EggDB database file from a shell, a script or an AI agent: create a file,
store, read and remove values, list entries in key order, check the file's health, see its
statistics, export it to a text file that another database imports, run SQL on it, and serve it
to psql and PostgreSQL drivers with `eggdb serve`. Every other command opens the file in its own
process, like `sqlite3`. The design behind every choice here is
[ux/cli.md](ux/cli.md) (D15, D90, D93); the embedded Rust API is [embedded.md](embedded.md).
Installing it is downloading one file: see the README, "Install".

## Quickstart

```sh
eggdb create app.db
eggdb put app.db user:1 Ada
eggdb put app.db user:2 Grace
eggdb get app.db user:1                      # Ada
eggdb scan app.db --prefix user:             # user:1  Ada / user:2  Grace
cat photo.jpg | eggdb put app.db photo       # any bytes, up to 1 GiB
eggdb get app.db photo > copy.jpg            # bytes back, exactly
eggdb scan app.db --format json | jq .key    # for scripts and agents
eggdb check app.db                           # "ok: every page passed its checksum …"
eggdb stats app.db
eggdb ui app.db                              # browse it full screen; q quits
eggdb export app.db backup.jsonl             # every entry, as JSON lines
eggdb import --create copy.db backup.jsonl   # and back, into a new file
eggdb sql app.db -c "CREATE TABLE users (id integer PRIMARY KEY, name text)"
eggdb sql app.db -c "SELECT name FROM users WHERE id = 1"
```

## Commands

```
eggdb <command> <file> [arguments] [options]
```

| Command | Opens the file | Does |
|---------|----------------|------|
| `create <file>` | creates it; fails if anything is at the path | makes an empty database; `--page-size <bytes>` picks a power of two from 4k to 64k (4k by default) |
| `put <file> <key> [<value>]` | for writing | stores the value under the key and commits, replacing any earlier value |
| `get <file> <key>` | read-only | prints the value, or exits 1 |
| `del <file> <key>` | for writing | removes the key and commits; exits 1 if it was not there. `delete` is the same command |
| `scan <file>` | read-only | prints entries in key order (options below) |
| `check <file>` | read-only | reads every page and reports the file's health |
| `stats <file>` | read-only | the page size, pages, file size, last commit and log, without reading a page |
| `ui <file>` | yes (`--read-only`: no) | browse and edit keys and values full screen in the terminal (below) |
| `export <file> [<out> \| -]` | read-only | writes every entry to `<out>`, or stdout, as JSON lines (below) |
| `import <file> [<in> \| -]` | for writing; `--create` makes it | loads an export from `<in>`, or stdin (below) |
| `sql <file> [-c <sql> \| <script> \| -]` | for writing; `--create` makes it | runs SQL statements, PostgreSQL dialect (below, and [sql.md](sql.md)) |
| `serve <file>` | for writing (`--read-only`: no); `--create` makes it | serves the file to psql and PostgreSQL drivers on a loopback address (below) |
| `help [<command>]` | | the help, or one command's help; `--help` and `-h` work after any command |
| `version` | | the version; also `--version` and `-V` |

The command comes first and the file second; options may go anywhere after the command, and
`--` ends them, so `eggdb get app.db -- -k` reads the key `-k`.

**Only `create`, `put --create`, `import --create`, `sql --create` and `serve --create` make a database file.** A wrong path such
as `app.bd` is an error that says how to create the file, never a new empty database that later
reads as "empty".

**Read-only commands share the file.** `get`, `scan`, `check`, `stats` and `export` can run
together, and next to an application that has the file open read-only. A program that has it
open for writing keeps them out, and `put`, `del`, `import` and `sql` wait for nobody: they exit 5 at
once when the file is open anywhere (see "Exit codes").

**Large values.** `put` sizes its page cache to the value, so any value up to 1 GiB works
without an option. `--cache <bytes>` sets the cache for any command (`--cache 64m`).

## Keys and values

Every key, value and range argument is read by its first characters:

| You type | You get |
|----------|---------|
| `user:1` | the text as typed (UTF-8) |
| `0x48656c6c6f` | the bytes in hex: an even number of digits, either case |
| `@path/to/file` | the file's contents, exactly |
| `@-` | everything on stdin, exactly |
| `=text` | the text after the `=`, for text that starts with `0x`, `@` or `=` |

An empty argument (`""` or `=`) is zero bytes, a valid key and value. Scripts that handle
arbitrary text prefix every argument with `=`: `eggdb put app.db "=$key" "=$value"`.

`put` without a value reads it from stdin when stdin is not a terminal, so
`cat photo.jpg | eggdb put app.db photo` stores the photo. Bytes are stored exactly:
`echo hi |` stores a trailing newline and `printf hi |` does not. On a terminal a forgotten value
is an error, never an empty value. Stdin can be read once per command.

## Output formats

`--format text|json|hex|raw` (short `-F`). By default a terminal gets `text`, and a pipe or a
file gets `json`, except `get`, which gets `raw` so that `eggdb get app.db photo > photo.jpg`
gives back the bytes. `hex` and `raw` are for `get` and `scan`, the commands that print data.

| Format | `get` | `scan` | `put`, `del`, `create`, `check`, `stats`, `export`, `import` |
|--------|-------|--------|------------------------------------------|
| `text` | the value; binary as `0x…` hex | `key<tab>value` lines, the count on stderr | nothing, `created app.db (4 KiB pages)`, or the report |
| `json` | `{"key":"user:1","value":"Ada"}` | one object per line | one object |
| `hex` | the value in hex | `<key hex><tab><value hex>` | |
| `raw` | the bytes, nothing added | `key<tab>value` unescaped, for data known to be text | |

In `json`, a key or value that is not valid UTF-8 appears as `key_hex` or `value_hex`, in
lowercase hex, instead of `key` or `value`. Numbers are plain integers. The fields are stable:

| Command | JSON |
|---------|------|
| `get` | `{"key":…,"value":…}`; nothing when the key is absent (exit 1) |
| `put` | `{"key":…,"stored":true}` |
| `del` | `{"key":…,"deleted":true}`, or `false` with exit 1 |
| `create` | `{"file":"app.db","page_size":4096}` |
| `scan` | `{"key":…,"value":…}` per entry, `{"key":…}` with `--keys-only` |
| `check` | `{"ok":true,"file","commit_stamp","commit_time","page_size","pages","file_bytes","entries","key_bytes","value_bytes","tree_height","tree_pages","overflow_values","overflow_pages","free_pages","log_pages","leaked_pages"}`; when damaged `{"ok":false,"file","problem","page","what_to_do"}`, `page` only when the problem names one |
| `stats` | `{"file","page_size","pages","free_pages","file_bytes","last_commit_stamp","last_commit_time","log_bytes_since_checkpoint"}`; the commit fields are `null` before the first commit |
| `export` | `{"file","out","entries"}` when the export goes to a file; to stdout, the export itself |
| `import` | `{"file","from","entries","imported","skipped","created","dry_run"}`; `from` is `-` for stdin; `imported` counts the keys stored, so a key repeated in the export counts once under `--on-conflict overwrite`; with `--dry-run` the counts and `created` say what the import would do |

`text` shows bytes as text when they are valid UTF-8 without control characters, and as `0x`
hex otherwise; `get` also keeps newlines, tabs and carriage returns as text. `scan` cuts a value
after 128 bytes and says so: `… (12,345 bytes)`. Only `text` ever shortens anything.

Commit times are UTC with microseconds, next to the exact hybrid-clock stamp:
`2026-10-26 09:14:07.123456 UTC (stamp 1793006047123456.0003)`, and `2026-10-26T09:14:07.123456Z`
in JSON.

## `scan`

| Option | Meaning |
|--------|---------|
| `-p`, `--prefix <bytes>` | keys that start with these bytes |
| `--from <bytes>` | first key, inclusive |
| `--to <bytes>` | end key, exclusive |
| `-r`, `--reverse` | largest key first |
| `-n`, `--limit <count>` | stop after this many entries (`-r -n 1` is the last entry) |
| `-k`, `--keys-only` | keys without values |

`--prefix` cannot be combined with `--from` or `--to`; `--from` above `--to` lists nothing. The
scan reads one snapshot, however long it takes. Keys compare as unsigned bytes.

## `check` and `stats`

```
$ eggdb check app.db
checked app.db as of commit 2026-10-26 09:14:07.123456 UTC (stamp 1793006047123456.0003)
  file            2.0 MiB, 512 pages of 4 KiB
  entries         12,480
  key bytes       198.2 KiB
  value bytes     1.6 MiB
  tree            height 3, 441 pages
  large values    3, on 12 overflow pages
  free pages      40
  log pages       16
  leaked pages    0
ok: every page passed its checksum, every key is in order, and every page of the file is accounted for.
```

Leaked pages (pages the file lost track of) are harmless and still exit 0. Damage exits 4 and
says what to do:

```
checked app.db as of commit 2026-10-26 09:14:07.123456 UTC (stamp 1793006047123456.0003)
damaged: page 77 failed its checksum: it is damaged, torn by a crash, or belongs elsewhere
  what was checked  every page read before the damage passed its checksum; the check stopped at the damage
  what to do        EggDB does not repair files yet. Restore app.db from your latest backup.
                    Keep the damaged file unchanged: `eggdb scan` may still read part of the data,
                    and an issue at https://github.com/eggspot/EggDB/issues with this output helps us.
```

When the damage is found while the file opens, the first line is `checked app.db` alone and the
second label says that opening read the header, the free list and the log.

```
$ eggdb stats app.db
app.db
  page size       4 KiB
  pages           512 (40 free)
  file size       2.0 MiB
  last commit     2026-10-26 09:14:07.123456 UTC (stamp 1793006047123456.0003)
  log since checkpoint  0 B (the next open replays nothing)
```

## `eggdb ui`

```
eggdb ui <file> [--read-only] [--mono] [--ascii] [--cache <bytes>]
```

Opens the file full screen: the keys in order on the left, the selected key's value on the
right, and editors that change one key per confirmed commit. `--read-only` opens the file
read-only, which refuses every change and works beside another reader; without it the file is
open for writing, so no other program can open it meanwhile. The design is
[ux/ui.md](ux/ui.md). The screens below are the UI's own frames at 80 × 24, drawn into the
fake terminal of its tests (`crates/eggdb-ui/tests/screens/`); a test fails when this page
differs from them. The main screen, with a JSON value selected:

```text
 EggDB  app.db                    1.2 MiB · 312 pages · 24 keys · 09:14:03Z  RW
 Keys                1–20 of 24 │ Value  json (auto) · 87 B        line 1–10/10
  bin                         40│ doc:json
> doc:json                    87│───────────────────────────────────────────────
  empty                        0│ {
  large                   14.0 K│   "name": "Ada Lovelace",
  user:1                      41│   "born": 1815,
  user:10                     43│   "tags": [
  user:11                     43│     "math",
  user:12                     43│     "poetry"
  user:13                     43│   ],
  user:14                     43│   "active": true,
  user:15                     43│   "notes": null
  user:16                     43│ }
  user:17                     43│
  user:18                     43│
  user:19                     43│
  user:2                      41│
  user:20                     43│
  user:3                      41│
  user:4                      41│
  user:5                      41│

 ↑↓ move  / filter  e edit  a add  d delete  s sql  c check  ? help  q quit
```

| Keys | In the key list | In the value viewer |
|------|-----------------|---------------------|
| `↑` `↓` or `j` `k` | move the selection | scroll one line |
| `PgUp` `PgDn` | move one window | scroll one pane |
| `Home` `End` or `g` `G` | first or last key | top or bottom of the value |
| `←` `→` | | scroll sideways by 8 columns |
| `Enter` | open the value | load a value above 64 MiB |
| `Tab` `Shift-Tab` | switch pane | switch pane |
| `/` | filter by key prefix as you type (`\xNN` is a byte); `Enter` keeps it, `Esc` restores | |
| `v` | cycle the view: text, JSON, hex, automatic | same |
| `w` | | wrap long lines (text) |
| `e` | edit the selected value | edit this value |
| `a` | add a key | add a key |
| `d` | delete the selected key (asks for `d` again) | delete this key |
| `c` | check every page and show the stats | same |
| `m` | show the last message in full | same |
| `r` | re-read the keys, the value and the stats | same |
| `?` | every key and the database's limits | same |
| `q` `Ctrl-C` | quit | quit |

In the editors: `Enter` confirms a one-line value and `Ctrl-S` any value, `Alt-Enter` adds a
line break, `Esc` cancels (asking first when something changed), `Ctrl-Z`/`Ctrl-Y` undo and
redo, `Ctrl-A`/`Ctrl-E` go to the line's start and end, `Ctrl-U`/`Ctrl-K` delete to them,
`Ctrl-←`/`Ctrl-→` move by word, `F1` shows the help, and `Ctrl-C` quits (asking first when an
edit is not committed).

- **Views.** A value opens as JSON (pretty-printed by our own strict parser) when it is an
  object or array, as hex when it is not UTF-8 text or holds control bytes, and as text
  otherwise. Forcing JSON on a value that is not JSON says where it stopped:
  `Not JSON: expected ',' or '}' at byte 212`.
- **Changes.** A value is edited as bytes in the same spelling the screen shows: `\xNN` is a
  byte, `\u{202e}` a character, `\n`, `\t`, `\r` and `\\` what they are in Rust; a backslash
  that starts nothing else is refused with `Bad escape at column 12: expected two hex digits
  after \x` and the cursor on it. A paste is taken literally, so pasted bytes are stored as
  they are, except that its line breaks, which terminals send as `\r` or `\r\n`, are stored as
  `\n`; a paste of more than 1 MiB is refused whole with a message, never cut, and none of it
  arrives as keys. Confirming shows
  the key with its length and the value's sizes (`58 B → 61 B`); `Enter` (for a delete, `d`
  again) then runs one write transaction (put or delete, commit) that is on disk before the
  screen redraws, and the status line says `Committed user:1 (61 B) at 09:14:03Z`. A
  confirmation takes keys only once it has been drawn and then 300 ms passed with no input
  (time inside an open bracketed paste does not count); keys arriving sooner are dropped. With
  bracketed paste, pasted bytes are never keys unless the terminal passes a raw `ESC[201~`
  through. Without it, keys typed ahead or pasted cannot
  answer a confirmation while they keep arriving within 300 ms of each other, but a paste that
  pauses longer than that, or that carries a raw `ESC[201~` the terminal passes through, can:
  the guard is seeing the question and 300 ms of quiet, not an absolute guarantee. `Ctrl-C` is
  handled first and never confirms a change: it quits at once, or, with an unsaved edit, asks
  first and quits on a second `Ctrl-C`. No transaction is open while an editor is. The editor
  takes values up to 4 MiB of text; store larger ones with `eggdb put <file> <key> @file`.
- **When a change fails**, nothing is committed and the editor keeps its text. Errors read
  `Error (<Kind>): <message>` with the engine's kind and message: `TooLarge` (one commit's
  pages must fit in the page cache, so a larger `--cache` helps), `WriterActive` (`r` waits up
  to 5 s for the other writer), `NeedsReopen` (`r` reopens the file and offers the edit again),
  `Corrupt` (`c` runs the check). A long message ends with `(m)` in the status line, and `m`
  shows it in full. Messages clear at the next key or after 8 s.
- **Check.** `c` shows the header's stats at once and runs `Database::check` on a worker
  thread, then the report (`Check  OK — every page accounted for`) or `Check  FAILED` with the
  damage; `Enter` goes back. Quitting while a check runs leaves the file closed without a
  checkpoint, which loses no commit; the next open replays the short log.
- **Any size.** The list is a window over a scan, so it moves as fast over a billion keys as
  over twenty; the key count runs in steps between key presses (`1,204+ keys` until it is done).
  A value is read whole when it is selected, except above 64 MiB, which waits for `Enter`.
- **Terminals.** Both stdin and stdout must be terminals; otherwise the command exits 2 with
  `eggdb ui needs a terminal; use eggdb get/scan instead`. The screen needs 40 × 10 and shows
  two panes from 80 columns. `NO_COLOR` or `--mono` drops colours, `--ascii` uses ASCII for
  box lines and arrows, and `TERM=dumb` or `--mono --ascii` draws no styles at all, which also
  suits screen readers. The terminal is put back as it was on quit, on a panic, and on
  `SIGTERM`, `SIGHUP` and `SIGQUIT` (on Windows when the console closes).

### More screens

`d` on `user:1` asks before it deletes anything:

```text
 EggDB  app.db                    1.2 MiB · 312 pages · 24 keys · 09:14:03Z  RW
 Keys                1–20 of 24 │ Value  text (auto) · 41 B          line 1–3/3
  bin                         40│ user:1
  doc:json                    87│───────────────────────────────────────────────
  empty                        0│ User 1
  large                   14.0 K│ user1@example.com
> user:1                      41│ born 1801-12-10
  ┌ Delete key ──────────────────────────────────────────────────────────────┐
  │ Key    user:1 (6 B)                                                      │
  │ Value  41 B, text: User 1                                                │
  │                                                                          │
  │ Deletes this one key in one commit, written to disk before you continue. │
  │ There is no undo.                                                        │
  │                                                                          │
  │                          d delete     Esc keep                           │
  └──────────────────────────────────────────────────────────────────────────┘
  user:19                     43│
  user:2                      41│
  user:20                     43│
  user:3                      41│
  user:4                      41│
  user:5                      41│

 d delete  Esc keep
```

`c` checks the whole file on a worker thread and then shows the report:

```text
 EggDB  app.db                    1.2 MiB · 312 pages · 24 keys · 09:14:03Z  RW

  Stats (from the header, read without touching a page)
    page size 4.0 KiB · 312 pages · 18 free · file 1.2 MiB
    last commit 2026-10-26 09:14:03.512000 UTC (stamp 1793005243512000.0001)
    cache 16.0 MiB, 211 pages held, 98.2% hits · log since checkpoint 24.0 KiB

  Check  OK — every page accounted for, 0.0 s
    24 keys · keys 152 B · values 15.0 KiB
    tree height 2, 9 pages · 1 large values on 4 overflow pages
    18 free pages · 30 log pages · 0 leaked pages












 Enter back  q quit
```

## `export` and `import`

```
eggdb export <file> [<out> | -]
eggdb import <file> [<in> | -] [--create [--page-size <bytes>]] [--on-conflict error|overwrite|skip] [--dry-run]
```

`export` writes every entry, from one snapshot, to `<out>`, or to stdout without it or with `-`.
It never replaces a file, and removes what it wrote when it fails. `import` loads such a file
into the database, from `<in>`, or from stdin without it or with `-`. Together they back up and
restore a database, move it to another machine or version, and load data made by other tools:

```sh
eggdb export app.db backup.jsonl
eggdb import --create restored.db backup.jsonl
eggdb export app.db | ssh other-host eggdb import --create app.db
eggdb import app.db changes.jsonl --on-conflict overwrite
```

| Option | Meaning |
|--------|---------|
| `--create` | create the database if it is missing, with the export's page size |
| `--page-size <bytes>` | the new file's page size instead (only with a file `import` creates) |
| `--on-conflict error` | stop at a key already in the database, or twice in the export (the default) |
| `--on-conflict overwrite` | store the export's value; of a key twice in the export, the last |
| `--on-conflict skip` | keep the value already there; of a key twice in the export, the first |
| `--dry-run` | check the whole export and report what would happen, writing nothing |

**What a failed import leaves.** From a regular file, `import` checks the whole export first
(every line, the trailer's count, the key order and sizes, and with `--on-conflict error` every
key against the database) and writes nothing when something is wrong. It then rewinds the same
file and commits in batches of at most 4 MiB. The export itself can no longer stop it then, but
other things still can, after some batches were committed:
- an I/O error or a full disk;
- a `--cache` too small for a batch or an entry;
- Ctrl-C;
- another program opening the file in the moment `import` reopens it with a larger cache for a
  large entry (only from stdin or a pipe, where the largest entry is not known in advance).

Stdin, and a path that can be read only once (a named pipe, `/dev/stdin`, a shell's `<(…)`), is
read in one pass: a bad line stops the import after the batches before it, and the error says
how many entries were committed. Fix the export and import it again with `--on-conflict skip`. A new file
(`--create`) is built as `<file>.importing` and renamed to `<file>` only when the whole import
succeeded, so a failed restore leaves no file; a `<file>.importing` left by a crash is refused
until you remove it.

**Memory.** Neither command holds the export in memory: `export` streams from one read
transaction, and `import` decodes each line straight into its key and value. The largest entry
needs its size in memory, and `import` sizes its page cache to it (32 MiB by default, more for an
entry above about 30 MiB), as `put` does.

**Progress.** On a terminal, both show `imported 1,204,000 entries…` on stderr, redrawn once a
second, and erase it when they end.

### The export format, eggdb-jsonl version 1

UTF-8 text with one JSON object per line, each line ended by a line feed:

```
{"format":"eggdb-jsonl","version":1,"eggdb":"0.2.0","page_size":4096,"exported_at":"2026-10-27T09:14:07.123456Z"}
{"key":"","value":"the empty key"}
{"key_hex":"00ff10","value_hex":"deadbeef"}
{"key":"user:1","value":"Ada Lovelace\nmathematician"}
{"entries":3}
```

- **The header** comes first. `format` and `version` are required; `eggdb` (the version that
  wrote it), `page_size` (what `import --create` gives a new file) and `exported_at` (UTC) are
  optional.
- **An entry** has exactly two fields: the key as `key` or `key_hex`, and the value as `value` or
  `value_hex`. A JSON string (`key`, `value`) holds the bytes when they are UTF-8; export writes
  one when they are UTF-8 without control characters other than tab, line feed and carriage
  return. Hex (`key_hex`, `value_hex`) holds any bytes, two digits each, either case on import,
  lowercase from export. Keys are in ascending order of their bytes, compared unsigned.
- **The trailer** comes last: `{"entries":N}` with the number of entry lines. A file without it,
  or with a count that differs, was cut or changed and is refused.

Import accepts any JSON that means the same thing (spaces between tokens, fields in any order,
every JSON escape including `\uXXXX` and surrogate pairs) and refuses everything else with the
line number and what is wrong: an unknown or repeated field, a raw control character, a lone
surrogate, invalid UTF-8 in `key` or `value`, an odd number of hex digits, an empty line,
anything after the trailer, or a `version` above 1 (made by a newer EggDB: upgrade).
`scan --format json` prints entry lines in the same spelling, except that it writes text for any
UTF-8, so its lines are valid entries too.

## `eggdb sql`

`eggdb sql` runs SQL statements on the file: from `-c "<sql>"`, from a script file, or from
stdin (a script of `-`, or none at all when stdin is not a terminal). This release runs the
first parts of the PostgreSQL dialect: tables with a primary key, `INSERT`, `SELECT` with
`WHERE`, inner, left and cross joins, `GROUP BY` with aggregates, `HAVING`, `DISTINCT`,
`ORDER BY`, `LIMIT` and `OFFSET`, `UPDATE`, `DELETE` and transaction blocks, with the integer,
floating-point, boolean and text types. [sql.md](sql.md) lists exactly what runs and
what comes next.

```sh
eggdb sql --create shop.db -c "CREATE TABLE items (id integer PRIMARY KEY, name text NOT NULL, price double precision)"
eggdb sql shop.db -c "INSERT INTO items VALUES (1, 'egg', 0.25), (2, 'bread', 2.5)"
eggdb sql shop.db -c "SELECT * FROM items ORDER BY price DESC"
eggdb sql shop.db schema.sql
cat queries.sql | eggdb sql shop.db
```

Results print as `psql` prints them, in its aligned format: a centred header, numbers
right-aligned, NULL as an empty cell and a row count; other statements print their command tag
(`INSERT 0 2`, `UPDATE 1`, `CREATE TABLE`).

```
 id | name  | price
----+-------+-------
  2 | bread |   2.5
  1 | egg   |  0.25
(2 rows)

```

- Statements run in order, each in its own transaction unless `BEGIN` opened a block; a block
  is one write transaction until `COMMIT` or `ROLLBACK`, and SQL left open at the end is rolled
  back with a warning on stderr.
- The first statement that fails stops the run: stderr gets the error as PostgreSQL words it,
  with the line and a caret under the place it refers to, any detail and hint, and the
  SQLSTATE code, and the exit code is 3 (4 when the file is damaged, 5 when it is busy).
  Statements before it keep their effects: committed outside a block, rolled back inside one.

```
ERROR:  relation "nope" does not exist
LINE 1: SELECT * FROM nope
                      ^
SQLSTATE:  42P01
```

- SQL keeps its tables in the same file as every other key, under keys that start with the bytes
  `0xff 'S' 'Q' 'L' 0x00` (D98). `eggdb scan`, `export`, `import` and `check` see and carry
  them like any other key; writing keys with that prefix through `put` or the embedded API
  changes the tables underneath SQL.
- `-c` takes UTF-8 text, given once (put several statements in it, separated by semicolons);
  `-c` and a script together, a script that cannot be read, or SQL that is not UTF-8 are usage
  errors (exit 2).

## `eggdb serve`

```
eggdb serve <file> [--listen <address>] [--user <name>] [--password-file <file> | -] [--database <name>] [--max-connections <count>] [--read-only] [--create]
```

Serves the file over the PostgreSQL wire protocol v3, so psql, pgAdmin-style tools and every
PostgreSQL driver connect to it unchanged and run the SQL of `eggdb sql` ([sql.md](sql.md),
"Connecting with psql"). The design is [ux/cli.md](ux/cli.md), section 14.

```sh
export EGGDB_PASSWORD='choose-one'
eggdb serve --create app.db
# listening on 127.0.0.1:5432 (PostgreSQL protocol; loopback only until EggDB has TLS)
#   database     app (app.db, open for writing)
#   user         eggdb (SCRAM-SHA-256), up to 64 connections
#   connect      psql postgresql://eggdb@127.0.0.1:5432/app
#   stop         Ctrl-C
```

| Option | Meaning |
|--------|---------|
| `--listen <address>` | `host:port`, `[::1]:port`, `localhost:port` or a port alone (on `127.0.0.1`); port 0 takes any free port. Default `127.0.0.1:5432`. **Loopback addresses only** (below) |
| `--user <name>` | the one user clients log in as, with SCRAM-SHA-256. Default `eggdb` |
| `--password-file <file>` | the password is the file's first line; `-` reads it from stdin. Without it, `EGGDB_PASSWORD`. Never on the command line. ASCII only in this release (SASLprep is not implemented). Prefer the file: `EGGDB_PASSWORD` stays in the server's environment for as long as it runs, where the same user (and on some systems an administrator) can read it, for example through `/proc/<pid>/environ` on Linux |
| `--database <name>` | the name clients connect to; default the file name without its extension (`app.db` is `app`). Any other name gets `3D000` |
| `--max-connections <count>` | connections served at once, one thread each; default 64. The next one gets `53300 sorry, too many clients already`. At most 16 of them may still be logging in at once, so slow or failing logins cannot take every slot; a 17th gets `53300` too |
| `--read-only` | open the file read-only: writes fail with `25006`, and it works beside other readers |
| `--create` | create the file if it is missing |

**Loopback only, no TLS yet (D102).** Until EggDB has its own TLS, `serve` listens only on
`127.0.0.1`, `::1` or another loopback address, so only programs on this machine can connect and
no password or query crosses a network in the clear. Any other address exits 2:
`eggdb: cannot listen on 0.0.0.0:5432: it is not a loopback address. …`. Clients that ask for
TLS are told no; `sslmode=require` fails, `prefer` (the default) connects without it. This rule
is removed when TLS lands.

**Output.** On a terminal, the report above; in a pipe, one JSON line,
`{"listening":"127.0.0.1:5432","host":"127.0.0.1","port":5432,"file":"app.db","database":"app","user":"eggdb","max_connections":64,"read_only":false}`,
so a script reads the port with `jq .port`. While it runs, stderr is a log of failures, one per
line: `2026-10-27 09:14:07 UTC  127.0.0.1:53422  password authentication failed for user "bob"`.
Ctrl-C or `SIGTERM` finishes the running statements, ends idle connections with `57P01`, rolls
back open transactions, prints `stopped` (`{"stopped":true,"connections_closed":N}` in JSON) and
exits 0; a second Ctrl-C ends it at once.

**Errors.** No password: exit 2, `serve needs a password for user eggdb: set EGGDB_PASSWORD, or
pass --password-file …`. A port in use: exit 3 with a `try:` line naming the next port. The file
busy (another program has it open for writing): exit 5. While `serve` runs, the file is open for
writing, so `eggdb put`, `sql` and the read-only commands exit 5 meanwhile.

## Exit codes and errors

| Code | Meaning |
|------|---------|
| 0 | done; `check` found no damage |
| 1 | not found: `get` or `del` of a key that is not there |
| 2 | usage: unknown command or option, missing argument, bad hex, an unreadable `@file`, export file or SQL script, a command not in this release |
| 3 | database error: no such file, not an EggDB file, written by a newer EggDB, permission denied, I/O failure, key or value too large, disk full; an export that is not valid, or a key already there (`import`); an output file that exists (`export`); a SQL statement that failed (`sql`) |
| 4 | damaged: `check` found damage, or any command read a damaged page |
| 5 | busy: another program has the file open |

Ctrl-C stops the command the way the operating system stops any program: shells report 130.
A `put` either committed or did not. A stopped `scan` can leave its last line cut off, since
output goes out in blocks of 64 KiB and more, so a script that reads a stopped scan should drop a
final line without a newline. Writing to a
closed pipe (`eggdb scan app.db | head`, or `eggdb export app.db | head`) ends the command
quietly with exit 0.

Errors go to stderr as `eggdb: <what happened>: <why>. <what to do>`, never as a Rust debug dump,
and add a `try:` line when the fix is a command:

```
eggdb: cannot open app.db: no such file. Create it with `eggdb create app.db`, or pass --create to put, sql or serve.
  try: eggdb create app.db
eggdb: unknown option '--reverse' for eggdb get
  usage: eggdb get <file> <key>
  try: eggdb help get
```

The commands that are coming (`shell`, `backup`, `compact`) answer
`eggdb <name> is not in this release yet; see docs/roadmap.md` and exit 2, so no script is built
on a name that later changes meaning.

## Colour

Colour appears only when the stream is a terminal, `NO_COLOR` is unset or empty, and `TERM` is
not `dumb`; on Windows, also only when the console accepts virtual-terminal sequences.
`--color always|never|auto` overrides it. Keys are bold, counts and stamps dim, `ok:` green,
`damaged:` and `eggdb:` red, leaked pages yellow. Values and hex are never coloured, so copying
them stays clean.
