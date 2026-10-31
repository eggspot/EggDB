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
eggdb backup app.db app.bak                  # a consistent copy, while others read
eggdb compact app.db                         # give free and leaked pages back to the disk
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
| `ttl <file> <key>` | read-only | prints how long the key still lives, or `never`; exits 1 if it is not there |
| `expire <file> <key> <time>` | for writing | gives the key a time to live (`30s`, `5m`, `1h30m`, `7d`, `1500ms`); exits 1 if it is not there |
| `persist <file> <key>` | for writing | removes the key's time to live; exits 1 if it is not there |
| `incr <file> <key> [<n>]`, `decr …` | for writing | adds `n` (1) to the counter under the key, or subtracts it, and prints the new value |
| `cas <file> <key> (<expected> \| --if-absent) (<new> \| --delete)` | for writing | replaces the value only while it is `<expected>`; exits 1 when it is not |
| `check <file>` | read-only | reads every page and reports the file's health |
| `stats <file>` | read-only | the page size, pages, file size, last commit and log, without reading a page |
| `backup <file> <copy>` | read-only | writes a copy of the last commit to the new file `<copy>`, while other readers keep working (below); given a directory, archives a timestamped, verified copy into it and records it in its `manifest.jsonl`, and `eggdb restore <new> <directory> [--at <time>]` restores the newest copy taken at or before the time, while `eggdb sql <directory> --at <time> -c ...` queries that copy read-only, in place (D208); given `s3://bucket[/prefix]`, archives the same verified copy into an S3-compatible bucket (the endpoint in `AWS_ENDPOINT_URL`, `http://` only for now, the keys in `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, the region in `AWS_REGION`) with its `manifest.jsonl` beside it, and `eggdb restore <new> s3://... [--at <time>]` restores from there (D226), while `eggdb sql s3://... [--at <time>] -c ...` queries a read-only edge copy kept in a local cache under its SHA-256 (`EGGDB_EDGE_CACHE`, or `eggdb-edge` in the temporary directory), downloaded once (D227) |
| `compact <file>` | read-only | rewrites the file without free or leaked pages and swaps it in atomically, while readers in other programs keep working (below) |
| `ui <file>` | yes (`--read-only`: no) | browse and edit keys and values full screen in the terminal (below) |
| `export <file> [<out> \| -]` | read-only | writes every entry to `<out>`, or stdout, as JSON lines (below); with `--table <name> --parquet <out>`, the table's rows as a Parquet file (D202); with `--sql postgresql\|mysql\|sqlserver\|oracle\|sqlite`, every table as plain SQL in that dialect (`CREATE TABLE`, multi-row `INSERT`, `CREATE INDEX`) that the dialect's own client or `eggdb restore` loads; views are counted, not written yet |
| `import <file> [<in> \| -]` | for writing; `--create` makes it | loads an export from `<in>`, or stdin (below); with `--csv <file> --table <name>`, a CSV file into a SQL table; with `--parquet <file> --table <name>`, a Parquet file into a SQL table, created from the file's columns when it does not exist (D202) |
| `sql <file> [-c <sql> \| <script> \| -]` | for writing (`--read-only`: no); `--create` makes it | runs SQL statements, PostgreSQL dialect, or MySQL's, SQLite's or Oracle's with `--dialect` (below, and [sql.md](sql.md)) |
| `explain <file> -c <sql>` | read-only | shows the plan for one statement, in its dialect's EXPLAIN form, without running it (below) |
| `table list\|show\|sample <file> [<table>]` | read-only | lists the tables and views, shows one in full, or prints its first rows (below) |
| `row list <file> <table>` | read-only | the rows of one table in the order they are stored, a page at a time with `--limit` and `--after` (below) |
| `serve <file>` | for writing (`--read-only`: no); `--create` makes it | serves the file to psql and PostgreSQL drivers on a loopback address (below); given a directory, serves each `*.eggdb` file in it as a database named by its stem (`psql -d <name>`, D184) |
| `shell <file>` | for writing (`--read-only`: no); `--create` makes it | an interactive prompt for the key-value commands and SQL, or a batch from `-c` or stdin (below) |
| `restore <file> [<dump.sql> \| -]` | for writing; `--create` makes it | restores a PostgreSQL dump in plain SQL form (`pg_dump`'s), or with `--dialect sqlserver` an SSMS or sqlpackage script, or with `--dialect oracle` a SQL*Plus-style script, or with `--dialect mysql` a `mysqldump` file, into the file, with a report (below, and [migrate.md](migrate.md)) |
| `mcp <file>` | read-only, for each call; `--allow-write`: read-write for each `write_sql` call | serves the file to an AI agent over MCP on stdin and stdout, read-only unless `--allow-write` (below, and [mcp.md](mcp.md)) |
| `audit <file> [--verify]` | never: reads `<file>.mcp-audit.jsonl` | lists the calls AI agents made through `eggdb mcp` (below); `--verify` checks that every record links to the one before it by its SHA-256, so a record changed, removed or inserted since it was written is found (exit 3, `audit_changed`) (D231) |
| `collection list\|add\|delete <file> [<name>]` | read-only for `list`, for writing otherwise | lists, adds or deletes the named key spaces in the file (below) |
| `help [<command>]` | | the help, or one command's help; `--help` and `-h` work after any command |
| `version` | | the version; also `--version` and `-V`; `--json` prints `{"version":…}` |

The command comes first and the file second; options may go anywhere after the command, and
`--` ends them, so `eggdb get app.db -- -k` reads the key `-k`.

**Only `create`, `put --create`, `import --create`, `sql --create`, `serve --create`, `shell --create` and `restore --create` make a database file.** A wrong path such
as `app.bd` is an error that says how to create the file, never a new empty database that later
reads as "empty".

**Read-only commands share the file.** `get`, `scan`, `check`, `stats`, `backup`, `compact`, `export`, `table`, `row`, `explain`, `sql --read-only`, `restore --dry-run` and `import --csv --dry-run` can run
together, and next to an application that has the file open read-only. A program that has it
open for writing keeps them out, and `put`, `del`, `import` and `sql` wait for nobody: they exit 5 at
once when the file is open anywhere (see "Exit codes"). That includes an AI agent's call through
`eggdb mcp`, which holds the file read-only for up to its time limit; the busy message says it
may be one.

**Collections.** `-C <name>` (`--collection <name>`) on `get`, `put`, `del`, `scan`, `export`,
`import`, `ttl`, `expire`, `persist`, `incr`, `decr` and `cas` makes the command work in that
collection instead of the default key space (see "Collections" below).

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

`--format text|json|hex|raw` (short `-F`), and `--json`, the same as `--format json` (the last
of them given wins). By default a terminal gets `text`, and a pipe or a file gets `json`, except
`get`, which gets `raw` so that `eggdb get app.db photo > photo.jpg` gives back the bytes,
streamed a page at a time, and
`sql`, `help` and `version`, which keep their text in a pipe. `ui`, `shell` and `mcp` take no
`--format`. `hex` and `raw` are for `get` and `scan`, the
commands that print data.

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
| `export` | `{"file","out","entries","collections"}` when the export goes to a file; to stdout, the export itself |
| `sql` | with `--json`, one line per statement: `{"tag","columns","rows","row_count","notices"}`, the last four only when the statement has them, as MCP's `run_sql` returns them without its envelope |
| `explain` | one `sql --json` line, `{"tag":"EXPLAIN","columns","rows","row_count"}` |
| `table list` | one record per table and view, tables first: `{"schema","name","kind","columns","comment"}`, `comment` only when there is one |
| `table show` | `{"schema","name","kind","columns","primary_key","indexes","comment","rows":{"count","exact"}}`, as MCP's `describe_table` without its notice; a view has no `primary_key` or `indexes` |
| `table sample` | one `sql --json` line plus `"truncated"`, `true` when the table has more rows than shown |
| `version` | with `--json`, `{"version":…}` |
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

## `backup` and `compact`

```
$ eggdb backup app.db app.bak
backed up app.db to app.bak as of commit 2026-10-26 09:14:07.123456 UTC (stamp 1793006047123456.0003)
  entries         12,480
  copy            1.7 MiB, 441 pages of 4 KiB
  time            812 ms
ok: app.bak is complete: its 12,480 entries were counted again after writing and syncing.
$ eggdb compact app.db
compacted app.db as of commit 2026-10-26 09:14:07.123456 UTC (stamp 1793006047123456.0003)
  entries         12,480
  before          2.0 MiB, 512 pages of 4 KiB
  after           1.7 MiB, 441 pages
  reclaimed       43 pages (40 free, 3 leaked)
  time            903 ms
ok: app.db was rewritten without free or leaked pages and swapped in whole; 43 pages reclaimed.
```

`backup` copies one commit, entry by entry, with every collection, SQL table and time to live,
so the copy has no free or leaked pages and is often smaller than the file. It is built as
`<copy>.backing-up`, synced, its entries counted again, and only then renamed to `<copy>`, so a
crash, a full disk or Ctrl-C never leaves part of a copy. `<copy>` must not exist: a backup never
replaces a file, and `-` is refused. A `<copy>.backing-up` left by a backup that did not finish
is refused too, and never removed for you: remove it and back up again. Both commands open the
file read-only, so they run beside other readers and exit 5 when a program has it open for
writing. To back up a file a program writes to, back up from inside it: `backup <copy>` in
`eggdb shell` (which holds the file), or `Database::backup_to` in an application
([embedded.md](embedded.md)).

`compact` checks the file first (a damaged file exits 4 and is left as it is), rebuilds it as
`<file>.compacting`, syncs and counts it, and renames it over the file in one step: a crash at
any moment leaves the old file or the compacted one, never a mix. Programs reading the file keep
reading the one they opened until they open it again. A file with nothing to reclaim is
rewritten all the same, which packs it. `eggdb shell` cannot compact the file it holds; quit it
first.

In a pipe each prints one JSON line: `backup` gives `file`, `copy`, `commit_stamp`,
`commit_time`, `entries`, `page_size`, `copy_pages`, `copy_bytes` and `elapsed_ms`; `compact`
gives `file`, `commit_stamp`, `commit_time`, `entries`, `page_size`, `pages_before`,
`file_bytes_before`, `pages_after`, `file_bytes_after`, `free_pages_before`,
`leaked_pages_before`, `pages_reclaimed` and `elapsed_ms`. A backup is a file EggDB opens as it
is; `export` is the text form that other databases and older releases read. Design:
[ux/cli.md](ux/cli.md) section 22, [decision D141](decisions/D141.md).

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
| `t` | set or clear the selected key's time to live (asks, then confirms) | same |
| `+` `-` | add to or subtract from the selected counter (asks how much, 1 if empty) | same |
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
  takes values up to 4 MiB of text; store larger ones with `eggdb put <file> <key> @file`,
  which streams the file into the database a page at a time.
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

`s` opens the query screen: a SQL editor on top and the last result below. `F5` or `Ctrl-R`
runs the editor's whole text as `eggdb sql` runs a script (`Ctrl-Enter` cannot be told from
`Enter` by a terminal, so `Enter` is a line break); `Tab` moves between the editor and the
result grid, which scrolls by rows (`↑` `↓` `PgUp` `PgDn` `Home` `End`) and by whole columns
(`←` `→`); `Ctrl-P` and `Ctrl-N` recall earlier runs; `Esc` goes back to the keys, which are
read again. A run holds nothing afterwards: a `BEGIN` without `COMMIT` is rolled back with a
warning in the status line, which lists every statement's command tag. An error replaces the
grid in psql's style and puts the cursor on the character it points at:

```text
 Result  ERROR
 ERROR:  relation "users" does not exist
 LINE 1: SELECT name FROM users WHERE id = 1;
                          ^
 SQLSTATE:  42P01
```

## `export` and `import`

```
eggdb export <file> [<out> | -]
eggdb import <file> [<in> | -] [--create [--page-size <bytes>]] [--on-conflict error|overwrite|skip] [--dry-run]
eggdb import <file> --csv <csv file | -> --table <name> [--header] [--delimiter <c>] [--null <text>]
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

**Collections.** A file with collections exports as version 2 (below): the default key space
first, then each collection's entries tagged with its name, and an empty collection as a line of
its own. `import` puts every entry back into its collection and makes the collections it meets
(`"collections_added"` in its JSON summary; `export` to a file reports `"collections"`).
`export -C <name>` writes only that collection, as a plain version 1 export, and
`import -C <name>` loads every entry of an export into that collection, made if missing, whatever
collection the entries came from, so `eggdb export app.db -C users | eggdb import archive.db -C
users` copies one collection. `--on-conflict` applies within a key space: the same key in two
collections is not a conflict, except when `import -C` puts them into one.

### Loading a CSV file into a table: `import --csv`

```
eggdb import <file> --csv <csv file | -> --table <name> [--header] [--delimiter <c>] [--null <text>] [--dialect postgresql|oracle]
```

With `--csv`, `import` loads a CSV file (or stdin, with `-`) into an existing SQL table instead,
as `COPY <name> FROM STDIN (FORMAT csv)` does ([sql.md](sql.md#copy), D117): every row goes
through the path of `INSERT`, so constraints, defaults, `serial` columns and indexes apply, and
the whole file loads in one transaction, all rows or none. `--table` is the table as SQL writes
it (`people`, `app."People"`, or `people (name, id)` for the columns the file holds, in its
order); `--header` skips a first line of column names; `--delimiter` and `--null` change the
separator (a comma) and the unquoted text that means NULL (empty). It prints `imported 2 rows
from people.csv into people in app.db`, or in a pipe one JSON line with `file`, `from`, `table`
and `rows`; a bad row prints the error with its line and column, `Nothing was imported`, and
exits 3. The page cache grows with the file (four times its size plus 16 MiB, between 256 MiB
and 1 GiB) since the load must fit in it; `--cache` sets it for larger files. `--dry-run` loads
the CSV into a copy of the file (`<file>.importing`, removed after) and prints `checked
people.csv: 2 rows would be imported into people in app.db` (`"dry_run":true` in JSON), so a bad
row is found without touching the file or moving its sequences. `--on-conflict` and
`--page-size` are for eggdb-jsonl imports, and `--create` is refused: a new
file has no table to load into, so create the file and the table with `eggdb sql --create`.
`--dialect oracle` loads SQLcl `UNLOAD` and SQL*Plus `SET MARKUP CSV ON` files: the table name is
read as Oracle writes it (`hr.employees`), dates by Oracle's NLS format (`DD-MON-RR`) and then
as ISO 8601, and an empty field, quoted or not, is NULL, since Oracle keeps no empty strings.

```sh
eggdb sql app.db -c "CREATE TABLE people (id integer PRIMARY KEY, name text, born date)"
eggdb import app.db --csv people.csv --table people --header
gunzip -c more.csv.gz | eggdb import app.db --csv - --table "people (name, id)"
```

To unload a table as CSV, run `COPY ... TO STDOUT` with `eggdb sql`:
`eggdb sql app.db -c "COPY people TO STDOUT (FORMAT csv, HEADER)" > people.csv`.

### The export format, eggdb-jsonl versions 1 and 2

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

**Version 2** is version 1 plus collections, and `export` writes it only for a file that has
collections, so an export of a file without them stays byte for byte a version 1 export and an
older `eggdb` refuses a version 2 file as made by a newer EggDB instead of failing on a field:

```
{"format":"eggdb-jsonl","version":2,"eggdb":"0.3.0","page_size":4096,"exported_at":"2026-10-30T09:14:07.123456Z"}
{"key":"user:1","value":"Ada"}
{"collection":"cache"}
{"collection":"sessions","key":"s:1","value":"{\"user\":1}"}
{"collection":"sessions","key_hex":"00ff","value_hex":"deadbeef"}
{"entries":3}
```

- An entry with a `collection` field (its name, 1 to 128 bytes of text) belongs to that
  collection; one without belongs to the default key space. Export writes the field first, the
  default key space's entries first, then each collection's in byte order of its name, each in
  key order.
- A line with only `collection` is a **collection line**: the collection exists, even with no
  entry. Export writes one for each empty collection; import makes it.
- The trailer counts entry lines; collection lines are not entries.
- `collection` in a version 1 file, or in the header or the trailer, is refused.

Import accepts any JSON that means the same thing (spaces between tokens, fields in any order,
every JSON escape including `\uXXXX` and surrogate pairs) and refuses everything else with the
line number and what is wrong: an unknown or repeated field, a raw control character, a lone
surrogate, invalid UTF-8 in `key` or `value`, an odd number of hex digits, an empty line,
anything after the trailer, or a `version` above 2 (made by a newer EggDB: upgrade).
`scan --format json` prints entry lines in the same spelling, except that it writes text for any
UTF-8, so its lines are valid entries too.

## Collections

A collection is a named key space inside the database file, beside the default key space that
every command uses unless given `-C <name>` (D139; [embedded.md](embedded.md) "Collections").
Names are 1 to 128 bytes of UTF-8 with no spaces or control characters. Keys of a collection
never appear in a scan of the default key space.

```
eggdb collection list <file>
eggdb collection add <file> <name>
eggdb collection delete <file> <name>
```

```sh
eggdb collection add app.db sessions
eggdb put app.db -C sessions s:1 '{"user":1}'
eggdb get app.db -C sessions s:1
eggdb scan app.db -C sessions --prefix s:
eggdb put --create app.db -C cache c:1 hot       # makes the collection (and the file) on the way
eggdb collection list app.db
eggdb collection delete app.db cache
```

- `list` prints the names in byte order, one per line, and the count on stderr (`3 collections`,
  `no collections`); in a pipe, `{"file":"app.db","collections":["cache","sessions"]}`.
- `add` prints `added collection sessions to app.db`; `delete` removes the collection and every
  key in it in one commit, asks nothing, and prints `deleted collection cache from app.db`. In a
  pipe they print `{"file":…,"collection":…,"added":true}` and `…"deleted":true}`.
- `add` of a collection already there, and `delete` of one that is not, exit 1 with
  `nothing to add: …` or `nothing to delete: …`, as `del` of a missing key does.
- `-C` on a collection that does not exist exits 1, with the command that adds it:
  ```
  eggdb: cannot read s:1 from app.db: no collection named sessions. Add it with `eggdb collection add app.db sessions`.
    try: eggdb collection add app.db sessions
  ```
  `put --create -C <name>` and `import -C <name>` make it instead. A name that cannot be one is
  a usage error (exit 2).
- A collection's keys are ten bytes shorter than the default key space's (502 bytes with 4 KiB
  pages), since ten bytes of each say which collection it is in.
- `eggdb shell` works in one key space at a time: `.collection` lists them, `.collection <name>`
  chooses one and `.collection -` returns to the default key space, and the prompt shows the
  choice (`app[sessions]=> `). `eggdb ui` shows one key space at a time too: `C` opens the
  collection picker, which also adds collections, and from 110 columns a sidebar lists them
  ([ux/ui.md](ux/ui.md) "Collections").
- SQL does not see collections in this release, and `check` and `stats` report the whole file.

## Expiring keys and counters

A key can have a **time to live**: at its deadline reads and scans no longer see it, and later
commits remove it (D140; [embedded.md](embedded.md) "Expiring keys and counters"). A **counter**
is a value that is a whole number stored as text, changed atomically. All of these commands take
`-C <name>`.

```sh
eggdb put app.db -C sessions s:1 '{"user":1}' --ttl 30m
eggdb ttl app.db -C sessions s:1          # 29m59s
eggdb ttl app.db user:1                   # never
eggdb expire app.db user:1 7d
eggdb persist app.db user:1
eggdb incr app.db hits                    # 1
eggdb incr app.db hits 10                 # 11
eggdb decr app.db hits 2                  # 9
eggdb cas app.db lock --if-absent worker-7
eggdb cas app.db lock worker-7 --delete
```

- **Times** are counts with a unit, `ms`, `s`, `m`, `h` or `d`, joined from the largest down:
  `1500ms`, `30s`, `5m`, `1h30m`, `7d`. A bare number, a decimal, a space or a repeated unit is
  a usage error, and so is 0 (`eggdb del` removes a key now). `ttl` prints times in the same
  grammar, whole seconds rounded down (`29s`, `1h4m12s`) or milliseconds under one second, so a
  printed time can be typed back.
- `put --ttl <time>` stores the value with a time to live; a `put` without `--ttl` makes the key
  permanent again, and so does `persist`. `expire` replaces the time to live of a key that is
  there, and exits 1 with `nothing to expire: …` for one that is not.
- `incr` and `decr` add or subtract `n`, 1 without it, from 0 to 9223372036854775807; to
  subtract use `decr`, since `-5` is an unknown option. An absent key counts as 0. A value that
  is not a whole number, or a result outside -9223372036854775808 to 9223372036854775807, exits
  3 and changes nothing. The time to live is kept.
- `cas` compares and stores in one transaction. `--if-absent` stands for an `<expected>` of "not
  there" and `--delete` for a `<new>` of "remove the key". When the value is not the one
  expected it exits 1 and says what it found: `not swapped: lock is already in app.db (holding
  worker-7). Nothing changed.` A swap that stores a value makes the key permanent.
- In a pipe each prints one JSON line: `{"key":"s:1","stored":true,"ttl_ms":1800000,"expires_at":"2026-10-30T09:44:07.123Z"}`
  for `put --ttl`, `{"key":"s:1","ttl_ms":1799412,"expires_at":"…"}` for `ttl` (both `null` for
  a permanent key), `{"key":"user:1","expires":true,…}`, `{"key":"user:1","persisted":true}`,
  `{"key":"hits","value":"11"}`, and `{"key":"lock","swapped":false,"value":"worker-7"}`.
- `eggdb shell` takes the same commands without `eggdb <command> <file>`, in the collection
  chosen with `.collection`. `eggdb ui` shows an expiring key's time left in the key list and
  `expires in …` above its value, `t` sets or clears it, and `+` and `-` change a counter.
- Keys with a time to live are 14 bytes shorter than other keys (498 bytes in the default key
  space with 4 KiB pages). `export` writes the keys that are live, without their deadlines.

## `eggdb sql`

The file name `:memory:` runs on a database in memory, as SQLite's does: `eggdb sql :memory:`
starts empty, writes nothing to disk, and is gone when the command ends. Every command that
opens a database takes it.

A file in SQLite's format (`eggdb sql app.sqlite`) is read into a database in memory, with its
tables, rows, indexes, views and triggers, and any committed pages still in its `-wal` file.
The SQL then runs on that copy, and a line on stderr says that changes are not written back to
the SQLite file. A statement EggDB refuses is skipped with a note on stderr.

`eggdb sql` runs SQL statements on the file: from `-c "<sql>"`, from a script file, or from
stdin (a script of `-`, or none at all when stdin is not a terminal). This release runs the
first parts of the PostgreSQL dialect: tables with a primary key, `INSERT`, `SELECT` with
`WHERE`, inner, left, right, full and cross joins, subqueries, `WITH` queries, `UNION`,
`INTERSECT` and `EXCEPT`, `GROUP BY` with aggregates, `HAVING`, `DISTINCT`, `ORDER BY`, `LIMIT`
and `OFFSET`, `UPDATE`, `DELETE` and transaction blocks, with the integer,
floating-point, exact `numeric`, boolean, text, `char(n)`, `bytea`, `uuid`, `date`, `time`,
`timetz`, `timestamp`, `timestamptz`, `interval`, `json` and `jsonb` types and `serial` and identity columns. `--dialect mysql` reads MySQL's SQL instead, on the
same tables, and reports errors as MySQL does (`ERROR 1146 (42S02): Table 'eggdb.t' doesn't
exist`) and warnings as `Warning 1062: ...` on stderr; `--dialect sqlite` (or `sqlite3`) reads
SQLite's SQL (type affinity, `INTEGER PRIMARY KEY` row ids, upserts, PRAGMAs, `sqlite_schema`)
and reports errors as SQLite does, with the extended result code on a second line (`Error:
near "FORM": syntax error`, `Result code: SQLITE_ERROR (1)`); `--dialect oracle` reads Oracle's
SQL (`DUAL`, `ROWNUM`, `FETCH FIRST`, upper-case names, `''` as NULL, Oracle's types and
functions), reports errors as Oracle does (`ORA-00942: table or view does not exist`), keeps
the transaction that the first change starts open until `COMMIT`, `ROLLBACK` or DDL, and
commits it when the run ends, as SQL*Plus does on exit (there is no Oracle wire protocol,
D10); `--dialect postgresql` is the default.
[sql.md](sql.md) lists exactly what runs and what comes next.

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
- `COPY ... FROM STDIN` and `COPY ... TO STDOUT` ([sql.md](sql.md#copy)) work as with psql: in
  a script (a file, or stdin), the data follows the statement's line up to a line `\.`, as
  `pg_dump` writes it; with `-c`, the data is read from stdin
  (`eggdb sql app.db -c "COPY people FROM STDIN (FORMAT csv)" < people.csv`). `COPY ... TO
  STDOUT` prints the rows themselves, with no table around them and no `COPY n` line, so its
  output can be saved as it is. An error in the data adds a `CONTEXT:` line naming the line
  and the column. When the SQL contains `COPY`, the page cache grows with the script (as for
  `import --csv`) so a large load fits in its one transaction.
- `-c` takes UTF-8 text, given once (put several statements in it, separated by semicolons);
  `-c` and a script together, a script that cannot be read, or SQL that is not UTF-8 are usage
  errors (exit 2).
- With `--dialect sqlserver`, the SQL runs batch by batch at its `GO` lines, as sqlcmd sends
  it: `GO` alone on its line, in any case, with an optional count that runs the batch again
  (`GO 3`) and a comment after it, outside strings, names and comments. Each batch starts
  fresh, so `CREATE VIEW` and `CREATE PROCEDURE` can open one; the first error stops the run,
  and its `Msg` counts lines from the batch's start, as sqlcmd does. sqlcmd's own commands
  (`:setvar`, `:r`, `!!`) are not run: a script with them fails with exit 3 and is restored with
  `eggdb restore --dialect sqlserver`, or run with `sqlcmd -i` against `eggdb serve`.
- With `--dialect oracle`, a script (a file or stdin; `-c` stays plain SQL, so `'Ada & Co'`
  runs as written) is read as `eggdb restore --dialect oracle` reads it (D127, D150): statements
  end at `;` or a `/` line, a PL/SQL block at a `/` line, `DEFINE` and `&name` substitute
  (`--define name=value`, repeatable, sets a variable before the first line), `@`, `@@` and
  `START` include other scripts (`@@` from the script's folder, the others from the working
  directory), `ACCEPT` takes its `DEFAULT` and never prompts, `PROMPT` prints its text on stdout
  in order with the results, `CONNECT user` switches to the user's schema (`ORA-01435` when it
  does not exist; no login is made), and `EXIT` or `QUIT` ends the run and commits (`EXIT
  ROLLBACK` rolls back). `EXECUTE p`, PL/SQL blocks and `CREATE PROCEDURE`, `FUNCTION` and
  `PACKAGE` run in the engine (tag `PL/SQL BLOCK` for a block), and `SET SERVEROUTPUT ON [SIZE
  n|UNLIMITED]` prints the `DBMS_OUTPUT` lines of each statement on stdout before its result (off
  until then, as in SQL*Plus; `-c` and the shell have it on; `--json` prints
  `{"server_output":["..."]}` before the statement's line). `SPOOL file` never writes a file, `HOST` never runs a program, and `WHENEVER
  SQLERROR CONTINUE`, `EXIT` status words, a statement ended by `.`, text with no `;` or `/` at
  the end and a line EggDB does not know each get one `NOTICE:` on stderr; every other command
  (`SET`, `COLUMN`, `TIMING`, `/` after `;`, ...) has no effect and is counted in one closing
  notice. With `--json`, `PROMPT` prints `{"prompt":"..."}` and a notice `{"notices":["..."]}` in
  its place on stdout. An undefined variable, a refused `SET SQLTERMINATOR` and an include that
  cannot be opened stop the run with exit 3, like a failing statement. The design is section 26
  of [ux/cli.md](ux/cli.md).

```
$ eggdb sql hr.db --dialect oracle load.sql --define region=Europe
Loading Europe offices
INSERT 0 3
NOTICE:  line 2: SPOOL load.log never writes a file; redirect stdout instead (eggdb sql ... > load.log)
NOTICE:  5 SQL*Plus commands had no effect: SET 4, COLUMN 1 (eggdb sql runs the SQL only)
```

### `sql --read-only`

`--read-only` opens the file read-only, beside other readers, as `get` and `scan` do: the
statements that read run as usual, and the first one that writes fails with SQLSTATE `25006`
(`ERROR:  cannot execute INSERT in a read-only transaction`), exit 3, and in JSON code
`read_only`. It is the CLI twin of MCP's `run_sql`. It cannot go with `--create`.

```
eggdb sql app.db --read-only -c "SELECT count(*) FROM users"
```

## Tables and plans: `eggdb table` and `eggdb explain`

The CLI twins of the MCP tools `list_tables`, `describe_table`, `sample_rows` and
`explain_query` (D144). Each opens the file read-only, so it runs beside other readers.

```
$ eggdb table list app.db
public.orders  table  5 columns
public.users   table  4 columns  the people who sign in
sales.totals   view   2 columns
2 tables, 1 view
$ eggdb table sample app.db users -n 2
 id | name
----+-------
  1 | Ada
  2 | Grace
(2 rows)

more rows follow: -n shows up to 1,000, eggdb sql shows them all
$ eggdb explain app.db -c "SELECT * FROM users WHERE id = 2"
              QUERY PLAN
--------------------------------------
 Index Scan using users_pkey on users
(1 row)

```

- `table list [--schema <name>]`: one line per table and view (tables first), with its kind,
  column count and comment; the count on stderr.
- `table show <table>`: the kind, row count (exact up to 10,000, otherwise `more than 10,000`),
  columns with types, `not null`, `auto increment`, defaults and comments, primary key, indexes
  and comment.
- `table sample <table> [-n <count>]`: the first rows as stored (10, up to 1,000), printed as
  `eggdb sql` prints a SELECT; a dim line on stderr says when more rows follow.
- `<table>` is a name or `schema.name`, matched as typed and then lower-cased. A table that is
  not there is exit 1, `table_not_found`, with `try: eggdb table list <file>`.
- `explain -c <sql> [--dialect …]`: one statement, without `EXPLAIN`, which it adds (`EXPLAIN
  QUERY PLAN` in SQLite's dialect); the plan prints as a table. Oracle's `EXPLAIN PLAN` and SQL
  Server's showplan are not in EggDB yet (`not_supported`, exit 3); a second statement is a
  usage error. The full design is [ux/cli.md](ux/cli.md) section 24.
- `table list -n <count> [--after <cursor>]`: a page of tables and views; the next page's
  command goes to stderr, and in JSON a last line `{"truncated":true,"next_cursor":"t1.…"}`
  follows the records.

### Reading a table a page at a time: `eggdb row list`

`eggdb row list <file> <table>` prints the rows of one table in the order they are stored
(primary-key order; a table without a primary key in the order its rows were added), the CLI twin
of the MCP tool `row_list` (D145). The file is opened read-only.

```
$ eggdb row list app.db orders --limit 2
 id | customer | total | placed_at
----+----------+-------+------------------------
  1 | Ada      | 12.50 | 2026-03-01 09:14:07+00
  2 | Grace    |  8.00 | 2026-03-01 09:20:41+00
(2 rows)

more rows follow; next page: eggdb row list app.db orders --limit 2 --after r1.818549630180000002
$ eggdb row list app.db orders --limit 2 --json
{"id":1,"customer":"Ada","total":"12.50","placed_at":"2026-03-01 09:14:07+00"}
{"id":2,"customer":"Grace","total":"8.00","placed_at":"2026-03-01 09:20:41+00"}
{"truncated":true,"next_cursor":"r1.818549630180000002"}
```

- `-n, --limit <count>` is the page size, 1 or more. Without it, text shows a page of 100 rows
  and JSON streams every row, so `eggdb row list app.db orders > orders.jsonl` dumps a table.
- `--after <cursor>` starts just after the cursor the page before printed. A cursor names a
  position in the table, not a count, so pages neither repeat nor skip a row while other programs
  write between them: rows added before the cursor are not read, rows added after it are, and
  deleted rows do not appear. Each page reads one snapshot.
- In JSON each row is one object of column to value (integers and booleans as JSON, `NULL` as
  `null`, other values as PostgreSQL prints them), and when `--limit` cut the page a last line
  holds the cursor; with `--limit n`, a script reads at most `n` records and then that line.
- A view has no stored order: it is refused (`not_a_table`, exit 3) with `try: eggdb table
  sample <file> <view>`. A table that is not there is exit 1, `table_not_found`. A cursor that
  is not one, of `table list`, or of another table is a usage error (exit 2).

## `eggdb shell`

```
eggdb shell <file> [-c <text>]... [--keep-going] [--mode table|expanded|csv] [--dialect postgresql|mysql|sqlite|oracle] [--read-only] [--create]
```

One prompt for everything this page and [sql.md](sql.md) do with a file: the key-value
commands without their `eggdb <command> <file>` prefix, SQL in the PostgreSQL dialect (or
MySQL's, SQLite's or Oracle's with `--dialect mysql`, `sqlite` or `oracle`; `.schema` then prints
that dialect's `CREATE TABLE`, and in Oracle's the open transaction is committed when the shell
ends, as SQL*Plus does; a PL/SQL block or `CREATE PROCEDURE` ends at a line holding `/`,
`EXEC p` runs a procedure, `SET SERVEROUTPUT ON|OFF` switches the `DBMS_OUTPUT` lines printed
before each result, on from the start), and a
few `.` commands. The file stays open for writing while the shell runs (`--read-only`: open
read-only), so other writers get the busy message meanwhile. The design is
[ux/cli.md](ux/cli.md), section 15.

```
$ eggdb shell app.db
EggDB 0.2.0: app.db, open read-write (2 tables)
Type .help for help. SQL ends with ; and .quit or Ctrl-D quits.
app=> put user:1 Ada
app=> get user:1
Ada
app=> select i.name, sum(s.quantity) as sold
app-> from items i join sales s on s.item_id = i.id
app-> group by i.name order by 2 desc;
 name  | sold
-------+------
 egg   |    7
 bread |    1
(2 rows)

app=> begin;
BEGIN
app=*> select 1/0;
ERROR:  division by zero
LINE 1: select 1/0;
               ^
SQLSTATE:  22012
app=!> rollback;
ROLLBACK
app=> .quit
```

**What a line is.** A line that starts with `.` is a shell command. A line whose first word is
`put`, `get`, `del`, `scan`, `ttl`, `expire`, `persist`, `incr`, `decr`, `cas`, `check`, `stats`
or `backup` (lower case) is that key-value command, with the
arguments and options it takes on the command line, split like a shell splits them (`"..."` and
`'...'` hold spaces); `@-` is refused, since stdin is the shell's own. Anything else is SQL,
which continues over lines until a `;` outside quotes, comments and `$$` bodies. `delete` starts
a SQL `DELETE`; the key-value command is `del`. `backup <copy>` copies the shell's own open
database, so it works while the shell holds the file for writing; inside a transaction block it
copies the last commit. `compact` is refused with the command to run after quitting. Shell and
key-value commands are recognised only at the start of a statement, not on its continuation
lines.

**The prompt** is the file's stem with psql's markers: `app=>` for a new statement, `app->` while
one continues, `app'>` and `app">` inside an unfinished string or quoted name, `app=*>` inside a
`BEGIN` block and `app=!>` in a failed one (every statement answers `25P02` until `ROLLBACK`).
`BEGIN` holds the database's write transaction across prompts until `COMMIT` or `ROLLBACK`; a
block still open when the shell ends is rolled back with a `WARNING:`. Key-value writes inside a
block fail with the busy error (exit 5): the block holds the one write transaction.

| Shell command | Does |
|---------------|------|
| `.tables` | the tables, one per line |
| `.schema [table]` | the `CREATE TABLE` statements, all or one |
| `.collection [name\|-]` | alone, the key spaces with `*` before the one in use; a name makes `put`, `get`, `del` and `scan` work in that collection, and `-` returns to the default key space (see "Collections") |
| `.mode [table\|expanded\|csv]` | how results print: psql's aligned table (the default; on a terminal a result wider than the window prints expanded), one record per block, or CSV with a header row; no argument shows the mode |
| `.timer [on\|off]` | `Time: 1.234 ms` on stderr after every statement and command |
| `.help` | the commands, keys and the history file |
| `.quit`, `.exit` | leave (so does `Ctrl-D` on an empty line) |
| `\copy t from 'file' [options]` | psql's `\copy`: load a file on this machine into a table, as `COPY t FROM STDIN [options]` (`from stdin` reads the lines that follow, up to `\.`) |
| `\copy {t \| (query)} to 'file' [options]` | unload into a file on this machine (replacing it), or `to stdout` |

| Key | Does |
|-----|------|
| `←` `→`, `Home` `End`, `Ctrl-A` `Ctrl-E` | move by a character, to the start or the end |
| `Alt-B` `Alt-F`, `Ctrl-←` `Ctrl-→` | move by a word |
| `Backspace` `Delete`, `Ctrl-W`, `Ctrl-U` `Ctrl-K` | delete a character, the word before the cursor, to the start or the end |
| `Ctrl-Z` `Ctrl-Y` | undo and redo within the line |
| `Tab` | complete: commands at the start of a line, table names after `FROM`, `JOIN`, `INTO`, `UPDATE`, `TABLE` and `.schema`, the columns of the tables a statement names, SQL keywords and functions, and keys after `get`, `del` and `scan --prefix/--from/--to`; a second `Tab` lists the candidates |
| `↑` `↓`, `Ctrl-R` | the previous and next history entries, and a reverse search |
| `Ctrl-C` | clears the line and an unfinished statement |
| `Ctrl-D` | on an empty line, leaves |
| `Ctrl-L` | clears the screen |

**History** is kept in `$XDG_STATE_HOME/eggdb/history`, else `~/.local/state/eggdb/history`
(macOS too), or `%LOCALAPPDATA%\EggDB\history` on Windows: one entry per statement or command,
the newest 1,000, mode 0600 in a 0700 directory. Empty lines, lines starting with a space,
repeats of the previous entry and `.quit` are not kept. `EGGDB_HISTORY=<path>` moves the file and
`EGGDB_HISTORY=` (set and empty) turns it off.

**COPY.** `COPY t FROM STDIN;` reads its data from the lines that follow, up to a line `\.`
(at the prompt it says so and prompts with `>>`; `Ctrl-D` also ends the data and `Ctrl-C` fails
the copy), and `COPY ... TO STDOUT` prints the rows. When a `COPY` fails in a batch while it
reads its data, the rest of the data, up to `\.`, is skipped, so it never runs as SQL; a `COPY`
refused before its data starts (a missing table, say) leaves the lines to run as SQL, as psql
does. `\copy` reads and writes files with
the shell's own permissions; a file name may be quoted (`'my file.csv'`) and `program` is
refused.

**Batch use.** With `-c` (repeatable, run in order) or with stdin not a terminal, the shell runs
the text as if typed, without banner, prompt, colour or history, and prints results in `table`
mode whatever their width, so the output is stable for scripts. A last statement without `;`
runs at the end of each `-c` text and of stdin. It stops at the first failure with that
failure's exit code (1 for a missing key, 2 for a usage error such as an unknown `.` command, 3
for a SQL error, 4 damaged, 5 busy) unless `--keep-going`, which runs on and exits with the
first failure's code.

```sh
eggdb shell app.db -c "put user:2 Grace" -c "select count(*) from items;"
eggdb shell app.db < session.txt
eggdb shell app.db --mode csv -c "select * from items" > items.csv
```

## `eggdb serve`

```
eggdb serve <file|directory> [--listen <address>] [--mysql-listen <address>] [--tds-listen <address>] [--log-batches] [--user <name>] [--password-file <file> | -] [--database <name>] [--max-connections <count>] [--tls-certificate <file> --tls-key <file> | --tls-self-signed] [--tls-min-version <1.2|1.3>] [--auth-rules <file>] [--metrics-listen <address>] [--mcp-listen <address> --mcp-token-file <file>] [--resp-listen <address> [--resp-max-memory <size>] [--resp-eviction <policy>]] [--memcached-listen <address>] [--es-listen <address>] [--mongo-listen <address>] [--bolt-listen <address>] [--cdc-webhook <url>] [--kafka-listen <address>] [--amqp-listen <address>] [--mqtt-listen <address>] [--sqs-listen <address>] [--nats-listen <address>] [--stomp-listen <address>] [--cql-listen <address>] [--s3-listen <address>] [--observability-listen <address>] [--restore-from s3://bucket/prefix] [--cdc-queue <name>] [--read-only] [--create]
```

Serves the file over the PostgreSQL wire protocol v3, so psql, pgAdmin-style tools and every
PostgreSQL driver connect to it unchanged and run the SQL of `eggdb sql` ([sql.md](sql.md),
"Connecting with psql"). With `--mysql-listen` it also serves MySQL's client/server protocol,
so the `mysql` client and MySQL drivers connect and run MySQL's SQL on the same tables
([sql.md](sql.md), "Connecting with mysql", D106). With `--tds-listen` it also serves SQL
Server's TDS protocol, so Microsoft's `sqlcmd` (go-sqlcmd) connects and runs Transact-SQL
batches on the same tables ([sql.md](sql.md), "Connecting with sqlcmd", D122), and .NET and Java
apps run parameterized statements and transactions through Microsoft.Data.SqlClient and
mssql-jdbc ([sql.md](sql.md), ".NET and Java apps over TDS", D132). The design is [ux/cli.md](ux/cli.md), section 14.

```sh
export EGGDB_PASSWORD='choose-one'
eggdb serve --create app.db
# listening on 127.0.0.1:5432 (PostgreSQL protocol; loopback only without TLS: see --tls-certificate)
#   database     app (app.db, open for writing)
#   user         eggdb (SCRAM-SHA-256), up to 64 connections
#   connect      psql postgresql://eggdb@127.0.0.1:5432/app
#   stop         Ctrl-C
```

| Option | Meaning |
|--------|---------|
| `--listen <address>` | `host:port`, `[::1]:port`, `localhost:port` or a port alone (on `127.0.0.1`); port 0 takes any free port. Default `127.0.0.1:5432`. **Loopback addresses only without TLS**; any address with TLS (below) |
| `--mysql-listen <address>` | also serve MySQL's protocol there, the same forms as `--listen` (MySQL's port is 3306); same user, password and database, `caching_sha2_password` or `mysql_native_password`, and its own `--max-connections`. Like `--listen`, loopback only without TLS; with TLS the client may ask for it (`--ssl-mode=REQUIRED`) |
| `--tds-listen <address>` | also serve SQL Server's TDS 7.4 protocol there, the same forms as `--listen` (SQL Server's port is 1433); same user, password and database as a SQL login, and its own `--max-connections`. Without TLS, **no encryption** (connect with go-sqlcmd's default or `-N disable`, SqlClient's `Encrypt=false`, mssql-jdbc's `encrypt=false`); with TLS, encrypted as the client negotiates (`Encrypt=true`, `Encrypt=Strict` for TDS 8, or login-only from loopback). Loopback only without TLS, like `--listen` |
| `--log-batches` | log the first 200 characters of each TDS batch and RPC call (`sp_executesql` and the prepared-statement procedures) and each transaction manager request to stderr, to see what a client sends; never the login or its password. Off by default |
| `--user <name>` | the one user clients log in as, with SCRAM-SHA-256 (and MySQL's methods on `--mysql-listen`). Default `eggdb` |
| `--password-file <file>` | the password is the file's first line; `-` reads it from stdin. Without it, `EGGDB_PASSWORD`. Never on the command line. ASCII only in this release (SASLprep is not implemented). Prefer the file: `EGGDB_PASSWORD` stays in the server's environment for as long as it runs, where the same user (and on some systems an administrator) can read it, for example through `/proc/<pid>/environ` on Linux |
| `--auth-rules <file>` | how users other than `--user` log in: per user, their stored password (SCRAM-SHA-256, the default), an LDAP simple bind, an OpenID Connect token, or never; LDAP and tokens only over TLS or the local socket ([sql.md](sql.md#roles-and-privileges), D185) |
| `--metrics-listen <address>` | serves Prometheus metrics at `http://<address>/metrics` (default port 9187) over our own HTTP/1.1: sessions open, statements, errors, commits and rollbacks, and per database the file size, free pages, cache pages, cache hits and misses and the log since the last checkpoint. The metrics hold no data, but put the address where only your monitoring can reach it |
| `--mcp-listen <address>`, `--mcp-token-file <file>` | serves MCP over Streamable HTTP at `http://<address>/mcp` (default port 8765), read-only, to agents that send the file's token as a Bearer token; loopback addresses only for now ([mcp.md](mcp.md), D189) |
| `--resp-listen <address>` | serves the RESP protocol (default port 6379) so `redis-cli` and Redis clients work on the file: strings, hashes, lists, sets and sorted sets with expiry, `AUTH` with the server's password, RESP2 and RESP3; each RESP database is the collection `resp<n>`; loopback addresses only for now (D193) |
| `--resp-max-memory <size>`, `--resp-eviction <policy>` | keeps the RESP keys' stored bytes under the size (as `maxmemory`), evicting by the policy after each write that crosses it: `allkeys-lru`, `allkeys-lfu`, `allkeys-random`, `volatile-lru`, `volatile-lfu`, `volatile-random`, `volatile-ttl`, or `noeviction` (the default), which refuses writes with `OOM` instead (D193) |
| `--memcached-listen <address>` | serves memcached's text protocol and its basic meta commands (default port 11211) on the collection `memcached0`: `set`, `add`, `replace`, `append`, `prepend`, `cas`, `get`, `gets`, `gat`, `gats`, `delete`, `incr`, `decr`, `touch`, `flush_all`, `mg`, `ms`, `md`, `mn`; loopback only, as the protocol has no authentication (D194) |
| `--es-listen <address>` | serves a REST API compatible with Elasticsearch and OpenSearch (default port 9200) on the collection `es0`, so their clients and `curl` can index and search: index create, get, delete and exists, document index, create, get, update (`doc`) and delete, `_bulk`, `_search` and `_count` with `match`, `multi_match`, `match_phrase`, `term`, `terms`, `range`, `exists`, `prefix`, `ids` and `bool` queries ranked by BM25, `size`, `from`, `sort`, `q=` and the `terms`, `avg`, `sum`, `min`, `max` and `value_count` aggregations; HTTP basic authentication as `--user` with the server's password; loopback only for now (D196) |
| `--mongo-listen <address>` | serves the MongoDB wire protocol (default port 27017) so `mongosh` and MongoDB drivers work on the file: SCRAM-SHA-256 as `--user` with the server's password, databases and collections (stored in the collection `mongo0`), `insert`, `find` with filters, sort, projection, skip, limit and cursors, `update` and `delete` with the query and update operators, `findAndModify`, `count`, `distinct`, `aggregate` (`$match`, `$project`, `$addFields`, `$unset`, `$sort`, `$skip`, `$limit`, `$count`, `$group`, `$unwind`, `$lookup`, `$replaceRoot`, `$sortByCount`), and recorded indexes; loopback only for now (D197) |
| `--bolt-listen <address>` | serves the Bolt protocol (versions 5.0 and 4.4, default port 7687) so Neo4j drivers connected with `bolt://` run Cypher on the file's property graph (the collection `graph0`): `MATCH`, `OPTIONAL MATCH`, `CREATE`, `MERGE`, `SET`, `REMOVE`, `DELETE`, `UNWIND`, `WITH`, `RETURN` with aggregates; basic authentication as `--user` with the server's password; each statement commits on its own, so an explicit transaction that wrote cannot be rolled back yet; loopback only for now (D206, D207) |
| `--cdc-webhook <url>` | posts every change captured by `eggdb_cdc` (below, D210) to the `http://` URL as a Debezium-style JSON event, oldest first, once a second; an event leaves the outbox only when the URL answers 2xx, so each change arrives at least once and in commit order |
| `--cdc-queue <name>` | delivers every change captured by `eggdb_cdc` into the durable queue `<name>` (created when missing) as a Debezium-style JSON event, oldest first, once a second, at least once; AMQP, STOMP and SQS clients consume it (D217) |
| `--kafka-listen <address>` | serves the Kafka protocol (default port 9092) so Kafka producers and consumers work on the file's topics (the collection `kafka0`): produce record batches, fetch from an offset, list earliest and latest offsets, and metadata, with topics created on first use with one partition; no consumer groups or transactions yet, so consumers assign partitions themselves; loopback only, as there is no authentication yet (D213) |
| `--amqp-listen <address>` | serves AMQP 0-9-1 (default port 5672) so RabbitMQ clients publish and consume on the file's durable queues (the collection `queue0`): PLAIN login as `--user` with the server's password, channels, the default, direct, fanout and topic exchanges, queue declare, bind, purge and delete, publish, pushed consumers with prefetch, `basic.get`, ack, nack, reject and publisher confirms; an unacknowledged message returns to its queue when its consumer goes away; exchanges and bindings are kept while the server runs; the same port serves AMQP 1.0 clients (Qpid Proton, Azure Service Bus and ActiveMQ Artemis client libraries, RabbitMQ 4's AMQP 1.0 clients): SASL PLAIN, links whose address is a queue name (or `/queues/<name>`) in both directions with credit-based flow, multi-frame messages, and the accepted, rejected, released and modified outcomes; message bodies cross as bytes; loopback only for now (D214, D217) |
| `--mqtt-listen <address>` | serves MQTT 3.1.1 and the core of 5.0 (default port 1883): username and password as `--user` with the server's password, publish at QoS 0, 1 and 2, subscribe with `+` and `#` (delivered at most at QoS 1), retained messages kept in the file (the collection `mqtt0`), will messages and keep-alive; no persistent sessions yet; loopback only for now (D215) |
| `--sqs-listen <address>` | serves an SQS-compatible endpoint over HTTP (default port 9324) in the AWS JSON 1.0 protocol that current AWS SDKs use: every request must carry an AWS Signature Version 4 made with the access key `--user` and the secret key the server's password, dated within 15 minutes of the server's clock; create, look up, list, purge and delete queues, send (with a delay), receive (up to 10, with a visibility timeout), delete and return messages, and queue attributes; queue URLs are `http://<address>/000000000000/<name>`; the queues are EggDB's durable queues (the collection `queue0`), the same ones `--amqp-listen` serves; standard queues only, no FIFO queues yet; loopback only for now (D216) |
| `--s3-listen <address>` | serves an S3-compatible endpoint over HTTP (default port 9000) with path-style URLs (`http://<address>/<bucket>/<key>`): every request signed with AWS Signature Version 4 (access key `--user`, secret key the server's password), in the `Authorization` header or as a presigned URL of up to seven days; create, head, list and delete buckets; put (up to 64 MiB, with `Content-MD5` and `x-amz-content-sha256` checked), get with a byte range, head, copy and delete objects, delete many at once, `ListObjects` and `ListObjectsV2` with prefix, delimiter, `encoding-type=url` and continuation; multipart uploads of up to 10,000 parts; user metadata (`x-amz-meta-*`) and content types are kept; objects live in the file (the collection `objects0`) in 256 KiB pieces; no versioning, ACLs, policies, tagging, lifecycle rules or `aws-chunked` uploads yet; loopback only for now (D219) |
| `--observability-listen <address>` | serves a metrics store over HTTP (default port 8086): InfluxDB line protocol at `/write` (v1, with `precision`) and `/api/v2/write` (v2, `Authorization: Token <password>`), Prometheus remote write at `/api/v1/write`, and the Prometheus query API (`/api/v1/query`, `query_range`, `labels`, `label/<name>/values`, `series`, `status/buildinfo`) for Grafana and other Prometheus clients; sign in with `--user` and the server's password (Basic, Token, or `u` and `p`); points land in SQL tables in the `metrics` schema, one per measurement (or metric) with `time`, a text column per tag and a typed column per field, so SQL reads them too; PromQL covers selectors with `=`, `!=`, `=~` and `!~`, `offset`, `rate`, `irate`, `increase`, `delta`, the `*_over_time` functions, `sum`, `avg`, `min`, `max` and `count` with `by` or `without`, arithmetic and comparisons; a metric is a table with a `value` column or `<table>_<field>`; no gzip bodies, LogQL, TraceQL or OTLP yet; loopback only for now (D220) |
| `--restore-from s3://bucket/prefix` | when the database file does not exist, restores the newest copy `eggdb backup` archived into that bucket (checked against its SHA-256) before serving, so a new machine with an empty disk starts from the latest archive; with `eggdb backup <file> s3://...` run on a schedule, the bucket holds the state and any machine can serve it (D226) |
| `--nats-listen <address>` | serves the NATS client protocol (default port 4222): `CONNECT` with `user` as `--user` and `pass` as the server's password, publish with or without headers, subscribe with `*` and `>` wildcards and queue groups, request and reply, auto-unsubscribe after a number of messages, verbose mode and `echo`; delivery is at most once, as core NATS does, and JetStream is not served yet; loopback only for now (D216) |
| `--stomp-listen <address>` | serves STOMP 1.2, 1.1 and 1.0 (default port 61613): `CONNECT` or `STOMP` with `login` as `--user` and `passcode` as the server's password, `SEND`, `SUBSCRIBE` with `ack:auto`, `client` (cumulative) or `client-individual` and an optional `prefetch-count`, `ACK`, `NACK`, transactions (`BEGIN`, `COMMIT`, `ABORT`; a commit's queue messages are stored all or none), receipts and `DISCONNECT`; `/queue/<name>` destinations are the durable queues `--amqp-listen` and `--sqs-listen` serve, `/topic/<name>` destinations go to every current subscriber; no heart-beats yet; loopback only for now (D217) |
| `--cql-listen <address>` | serves Cassandra's CQL native protocol v4 (default port 9042) for Cassandra drivers: the password authenticator with `--user` and the server's password, `QUERY`, `PREPARE`, `EXECUTE` and `BATCH`, and the `system.local`, `system.peers` and `system_schema` tables drivers read; a keyspace is a SQL schema and a CQL table is a SQL table of the same name, so SQL reads and writes the same rows; `CREATE`/`DROP KEYSPACE`, `CREATE`/`DROP TABLE` (with partition and clustering keys and `CLUSTERING ORDER`), `TRUNCATE`, `INSERT` and `UPDATE` as upserts (counters with `c = c + n`), `INSERT ... IF NOT EXISTS`, `DELETE` of rows or columns, `SELECT` with `WHERE`, `IN`, `ORDER BY`, `LIMIT` and `count(*)`, and batches as one transaction; scalar types only (no collections, user-defined types, TTL, `USING TIMESTAMP` or other lightweight transactions yet), every result in one page; loopback only for now (D218) |
| `--database <name>` | the name clients connect to; default the file name without its extension (`app.db` is `app`). Any other name gets `3D000` |
| `--max-connections <count>` | connections served at once, one thread each; default 64. The next one gets `53300 sorry, too many clients already`. At most 16 of them may still be logging in at once, so slow or failing logins cannot take every slot; a 17th gets `53300` too |
| `--socket-dir <directory>` | also accept PostgreSQL connections on a Unix socket in that directory, named `.s.PGSQL.<port>` after `--listen`'s port, so `psql -h <directory> -p <port>` connects; the socket is readable and writable by its owner only, a stale socket file is replaced, and it is removed at stop. Not on Windows yet |
| `--mysql-socket <file>` | also accept MySQL connections on that Unix socket (`mysql --socket=<file>`); needs `--mysql-listen`. Not on Windows yet |
| `--tls-certificate <file>` | PEM certificate chain, the server's first, for TLS on every listener; needs `--tls-key` |
| `--tls-key <file>` | PEM private key for `--tls-certificate`: PKCS#8, SEC 1 or PKCS#1, P-256 or RSA; read once at start, never on the command line |
| `--tls-self-signed` | make a P-256 certificate at start, in memory, for the listen hosts and `localhost`; a new one each start. Its SHA-256 fingerprint is printed so clients can pin it |
| `--tls-min-version <1.2\|1.3>` | the oldest TLS version accepted; default 1.2 (older JDBC and .NET drivers still need it) |
| `--read-only` | open the file read-only: writes fail with `25006`, and it works beside other readers |
| `--create` | create the file if it is missing |

**TLS, and loopback without it (D161).** Without TLS, `serve` listens only on `127.0.0.1`, `::1`
or another loopback address, so only programs on this machine can connect and no password or
query crosses a network in the clear; any other address exits 2 with `eggdb: cannot listen on
0.0.0.0:5432: it is not a loopback address. … Pass --tls-certificate <file> --tls-key <file>,
or --tls-self-signed`, and clients that ask for TLS are told no. With TLS (EggDB's own TLS 1.3
and 1.2, D23) every listener may use any address, and connections from other machines must use
TLS: PostgreSQL clients with `sslmode=require` or stronger (`SSLRequest` and PostgreSQL 17's
direct TLS with ALPN `postgresql`; SCRAM-SHA-256-PLUS binds the login to the TLS connection),
MySQL clients with `--ssl-mode=REQUIRED` or stronger, and SQL Server clients with `Encrypt=true`
or `Encrypt=Strict`. Programs on this machine may still connect without TLS. With certificate
files the report shows the chain, its names and expiry, and the certificate's fingerprint, and
the `connect` line uses `sslmode=verify-full`:

```sh
eggdb serve app.db --password-file pw.txt --listen 0.0.0.0:5432 --tls-certificate cert.pem --tls-key key.pem
# listening on 0.0.0.0:5432 (PostgreSQL protocol; TLS 1.2 or newer, required from other machines)
#   database     app (app.db, open for writing)
#   user         eggdb (SCRAM-SHA-256), up to 64 connections
#   tls          cert.pem (chain of 2, P-256, expires 2027-03-01) with key.pem, for db.example.com
#   fingerprint  SHA256 A1:B2:…:F0 (the server certificate)
#   connect      psql "postgresql://eggdb@db.example.com:5432/app?sslmode=verify-full"
#   stop         Ctrl-C
```

With `--tls-self-signed` the `tls` line says `self-signed P-256 for <hosts> and localhost, made at
this start (new one each start)`, the connect line uses `sslmode=require`, and the fingerprint is
the one to pin. The JSON line gains `"tls"`, `"loopback_only"` and, with TLS,
`"tls_certificate"` (`"self-signed"` or the file), `"tls_key"`, `"tls_fingerprint_sha256"` (64
lowercase hex digits), `"tls_names"`, `"tls_not_after"` and `"tls_min_version"`.

**Output.** On a terminal, the report above; in a pipe, one JSON line,
`{"listening":"127.0.0.1:5432","host":"127.0.0.1","port":5432,"file":"app.db","database":"app","user":"eggdb","max_connections":64,"read_only":false}`,
so a script reads the port with `jq .port`; with `--mysql-listen` the line also has
`"mysql_listening"` and `"mysql_port"`, and the report a `mysql` line; with `--tds-listen`,
`"tds_listening"` and `"tds_port"`, and an `sqlcmd` line. While it runs, stderr is a log of failures, one per
line: `2026-10-27 09:14:07 UTC  127.0.0.1:53422  password authentication failed for user "bob"`.
Ctrl-C or `SIGTERM` finishes the running statements, ends idle connections with `57P01`, rolls
back open transactions, prints `stopped` (`{"stopped":true,"connections_closed":N}` in JSON) and
exits 0; a second Ctrl-C ends it at once.

**Errors.** No password: exit 2, `serve needs a password for user eggdb: set EGGDB_PASSWORD, or
pass --password-file …`. A port in use: exit 3 with a `try:` line naming the next port. The file
busy (another program has it open for writing): exit 5. While `serve` runs, the file is open for
writing, so `eggdb put`, `sql` and the read-only commands exit 5 meanwhile.

## `eggdb restore`

`eggdb restore` brings a PostgreSQL database over by restoring the dump PostgreSQL already makes
(D13, D118): `pg_dump`'s plain SQL, or the plain SQL that `pg_restore -f dump.sql <archive>`
writes from a custom, directory or tar archive. [migrate.md](migrate.md) is the guide.

```sh
pg_dump app > app.sql
eggdb restore --create app.db app.sql
pg_dump app | eggdb restore --create app.db
pg_restore -f dump.sql app.dump && eggdb restore --create app.db dump.sql
eggdb restore app.db app.sql --dry-run
eggdb restore app.db app.sql --keep-going --report restore.json
```

- The dump is read line by line, never whole, and its statements run one at a time, each in its
  own transaction as `psql -f` runs them; a table's `COPY` data loads in one transaction, so it
  must fit in the page cache, which grows with the dump (as for `import --csv`; `--cache` sets
  it).
- `--create` makes a missing file: the restore builds it as `<file>.restoring` and renames it into
  place only when it succeeded, so a failed or interrupted restore leaves no `<file>` (a
  `<file>.restoring` left by a crash is refused; remove it and restore again). Without
  `--create` the file must exist and the restore goes into it; an error then leaves the statements
  before it committed.
- psql's meta-commands in the dump are handled as psql would for what `pg_dump` writes:
  `\connect` is noted (one database per file), `\restrict` and `\unrestrict` must carry the same
  key (`pg_dump` 17.6 and later); every other meta-command is listed and never run.
- Objects EggDB does not have yet (functions, procedures, triggers, rules, policies, types,
  domains, materialized views, partitioned and inherited tables, extensions other than
  `plpgsql`, ...) are skipped and listed with their line in the dump and why; `GRANT`, `REVOKE`
  and `OWNER TO` are ignored (no roles yet), as are `SET` parameters EggDB does not have and
  storage settings such as `REPLICA IDENTITY`. A statement that fails only because it uses a
  skipped object is a consequence, listed too. None of these change the exit code.
- An error in a supported statement (a `COPY` row breaking a constraint, bad syntax, an
  `INSERT` that fails) stops the restore: stderr gets the error with its detail and the dump's
  line, and the exit code is 3. `--keep-going` lists every error and runs on, then exits 3.
- The report comes at the end in every outcome: counts of what was created and loaded (rows per
  table), and every skipped object, consequence, ignored kind and error. It is text on a
  terminal and one JSON object in a pipe or with `--format json`; `--report <path>` writes it to
  a file (JSON for a `.json` path) and leaves only the verdict line on stdout.
- `--dry-run` runs the real restore on a scratch copy and throws the copy away (D145): the file's
  last commit is copied to `<file>.restoring` as `eggdb backup` copies it (the file is opened
  read-only), or with `--create` the restore builds `<file>.restoring` as usual, and the copy is
  removed instead of renamed. The report is exactly what a real run would report, with its first
  line and verdict in the conditional (`ok: every table and row of the dump would be in
  app.db.`) and `"dry_run":true` in JSON; the exit code is a real run's. The file and its
  sequences do not change. The copy takes the file's size in disk space and time.
- A custom-format archive (it starts with `PGDMP`), a directory or tar archive, or a dump
  compressed with gzip, zstd or lz4 is not read: the command exits 2 before any file is touched
  and prints the exact `pg_restore -f dump.sql <archive>` or decompress command to run first.
- `--dialect sqlserver` (or `mssql`, `tsql`) restores an SSMS Generate Scripts or sqlpackage
  script instead, UTF-16 or UTF-8, batch by batch; its report adds **known differences**, what is
  restored but not as SQL Server had it, and lists sqlcmd's commands as client commands. A SQL
  Server backup (`TAPE`), a BACPAC or other zip, or a binary file is refused by its content with
  the route to a script; a UTF-16 file without `--dialect sqlserver` is refused with that advice
  ([migrate.md](migrate.md)).
- `--dialect oracle` restores a SQL*Plus-style script (SQL Developer or SQLcl exports, `impdp
  ... SQLFILE=` output, installers such as Oracle's sample schemas): statements end at `;` or a
  `/` line, PL/SQL units at a `/` line; `DEFINE`, `&name` substitution, `@`, `@@` and `START`
  (regular files only), `CONNECT` (the schema, never a login) and `EXIT` act, other SQL*Plus
  commands are listed as client commands; users become schemas, PL/SQL runs (procedures,
  functions, packages and triggers are created and anonymous blocks run; a unit that does not
  compile, a block with a substitution variable that has no value and a block calling an
  Oracle-supplied package EggDB does not have are skipped and listed), and a constraint
  disabled and enabled again is dropped and re-added. `--define name=value` (repeat
  it) gives a substitution variable. A Data Pump `.dmp`, `exp` or RMAN file is refused by its
  content with the `impdp ... SQLFILE=` route ([migrate.md](migrate.md#oracle), D127).

```
restored app.sql into app.db (1,204 statements, 2.3 s)
  schemas         2
  tables          14, 1,204,311 rows
  views           3
  sequences       9
  indexes         21
  constraints     17
  comments        4
  skipped         2 (function 1, materialized view 1)
  consequences    1
  ignored         61 (OWNER TO 38, GRANT 20, SET 3)
rows per table
  public.orders  1,180,002
  public.users      12,480
…
ok: every table and row of the dump is in app.db; 2 objects skipped and 1 consequence, listed above.
```

## `eggdb follow`

```
eggdb follow <file> <source-url> [--create] [--schema <list>] [--publication <name>] [--password-file <path>] [--poll <seconds>] [--until caught-up]
```

Copies a running PostgreSQL or SQL Server database into the file and keeps the file in step with
it until it is stopped, so applications can move to EggDB with no downtime
([migrate.md](migrate.md), D162). The source is `postgres://user@host[:port]/database` with
libpq's `sslmode` and `sslrootcert`, or `sqlserver://user@host[:port]/database` with
`encrypt=yes|strict|no` and `trustServerCertificate` or `sslrootcert`. The password comes from
`PGPASSWORD` or `SQLCMDPASSWORD`, or from `--password-file`; one written in the URL is refused,
because process listings show it.

| Option | Meaning |
|--------|---------|
| `--create` | create the file if it is missing; it must not already hold the source's tables |
| `--schema <list>` | the schemas to copy, comma-separated; default every non-system schema |
| `--publication <name>` | PostgreSQL: use this publication; default `eggdb_follow`, created `FOR ALL TABLES` and dropped at the stop |
| `--password-file <path>` | the source's password is the file's first line |
| `--poll <seconds>` | SQL Server: how often to ask change tracking for changes; default 5 |
| `--until caught-up` | stop by itself once every change committed so far is in the file |

Before anything is copied it checks every precondition, and it names every table that fails one:
- `wal_level=logical`, and the right to create a replication slot and, unless `--publication` is
  given, a publication;
- a primary key or `REPLICA IDENTITY FULL` on every table;
- with SQL Server, change tracking on the database and on every table.

Progress goes to stderr on a terminal, as fixed lines (connected, publication, slot, schema, one
line per copied table) and then a `following` line with the lag. In a pipe each step is one JSON
line on stdout, with an `event` of `connected`, `slot`, `schema`, `table_copied`, `copied` or
`stopped`.

Ctrl-C or `SIGTERM` finishes the transaction being applied, copies the sequence values, drops the
publication it created (the slot is temporary and goes with the connection) and prints a summary
ending `ok: … holds every source transaction committed up to <position>`. It then exits 0. If
the source closes the connection, the file is consistent at the last applied position and the
command exits 3. The design is [ux/cli.md](ux/cli.md), section 30.

## `eggdb mcp`

`eggdb mcp <file>` serves the file to an AI agent over the Model Context Protocol on stdin and
stdout (D11, D119, D121); [mcp.md](mcp.md) is the guide. The agent's client starts it:

```sh
claude mcp add mydb -- eggdb mcp /path/to/app.db
claude mcp add mydb -- eggdb mcp /path/to/app.db --allow-write
claude mcp add mydb -- eggdb mcp /path/to/app.db --tables users,orders
claude mcp add mydb -- eggdb mcp /path/to/app.db --time-limit 30
```

- **Read-only unless `--allow-write`.** Each call opens the file read-only and closes it after,
  so the storage refuses any write; between calls the file is free for your own writers. While a
  call runs (at most `--time-limit` seconds, 5 by default, 1 to 300), a program that wants to
  write the file is refused.
- **`--allow-write`** adds `write_sql`: one statement per call that may change the file, in a
  transaction of its own, the file opened read-write for that call only. Destructive statements
  (`DROP`, `TRUNCATE`, dropping a column, constraint or primary key, a column's type change, a
  `DELETE` or `UPDATE` whose `WHERE` names no column of its table) run only after the user
  confirms them: through the client's question when it can ask, otherwise through the agent's
  `confirm: true`. `run_sql` stays read-only.
- **`--tables <names>`** shows the agent only those tables and views (comma-separated, `name` or
  `schema.name`, `public.` and `dbo.` for the default schema, matched exactly as stored; repeat
  it to add more). Every other table, view and sequence is missing for the agent's SQL in every
  dialect and catalog, a listed view that reads an unlisted table fails as missing, and
  `kv_get` and `kv_scan` are withdrawn (`limited_to_tables` if called). A sequence comes with the
  table that owns it. Every name must exist when the server starts (exit 1, `table_not_found`,
  with the stored name when only the case differs). Read-only servers only: with
  `--allow-write` it is a usage error (D156).
- **`--call-gap <ms>`** (default 100, 0 to 10000, 0 for none): after each call the next one that
  opens the file waits that long, so your own writers get the file between an agent's calls.
- **Tools:** `list_tables`, `describe_table`, `run_sql` (one statement, in `postgresql`, `mysql`,
  `sqlite` or `oracle`), `explain_query`, `sample_rows`, `kv_get`, `kv_scan` (not under
  `--tables`), and the resource `eggdb://schema`.
- **Every call is recorded** in `<file>.mcp-audit.jsonl` before its result is sent; if that file
  cannot be written, `eggdb mcp` does not start.
- stdout carries only protocol messages; one line on stderr says what is served (a short
  report when stderr is a terminal). Closing stdin stops it with exit 0. It exits 3 at start for
  a missing file (it never creates one) or an unwritable audit file, and 3 when a statement it
  cannot stop runs a second past the time limit, after recording it.

## `eggdb audit`

`eggdb audit <file>` lists the calls AI agents made through `eggdb mcp`, from
`<file>.mcp-audit.jsonl`, oldest first, without opening the database:

```
$ eggdb audit app.db -n 3
2026-10-28 09:14:07.123      12 ms  list_tables     ok                    2  {}
2026-10-28 09:14:15.402     18 ms  run_sql         ok                 100+  SELECT id, name FROM users ORDER BY id
2026-10-28 09:14:21.007      1 ms  run_sql         read_only             -  UPDATE users SET name = 'Ada L.' WHERE id = 7
last 3 of 41 calls, 2 errors, by claude-code 2.1.0 (the name the client gave)
```

- One line per record: the time (UTC), the duration, the tool, `ok` or the error code (with the
  SQLSTATE for `sql_error`), the rows returned (`100+` when cut, `-` for an error) and the
  arguments (the SQL as sent, on one line). Lines are cut at 120 columns; control characters in
  logged text are shown escaped (`\u001b`). Server starts and time-limit exits are lines too. A
  confirmed write shows `ok, user confirmed` or `ok, agent confirmed`; a write whose server
  stopped before it finished shows `unknown`, and the summary counts it; a warning is a line of
  its own under its call.
- `-n <count>` (`--limit`) shows the newest lines; the summary on stderr counts every call.
- `--format json` prints the records, re-encoded as ASCII JSON, one object per line, with the client's
  name and version, the protocol version, the SQLSTATE and the result's size.
- No audit file: `no MCP calls yet` on stderr, exit 0. A line that is not a record is reported
  and skipped.

## Exit codes and errors

| Code | Meaning |
|------|---------|
| 0 | done; `check` found no damage |
| 1 | not found: `get` or `del` of a key that is not there; `table show` or `table sample` of a table that is not there |
| 2 | usage: unknown command or option, missing argument, bad hex, an unreadable `@file`, export file or SQL script |
| 3 | database error: no such file, not an EggDB file, written by a newer EggDB, permission denied, I/O failure, key or value too large, disk full; an export that is not valid, or a key already there (`import`); an output file that exists (`export`, `backup`); a SQL statement that failed (`sql`) |
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
  usage: eggdb get <file> <key> [-C <name>]
  try: eggdb help get
```

When the command line asks for JSON (`--json`, `--format json`), an error is one JSON line on
stderr instead, for scripts and agents: a `code`, its `category` (shared with EggOS: `usage`,
`not_found`, `exists`, `busy`, `refused`, `too_large`, `damaged`, `failed` and the rest), the
same `message`, the `usage` line for a usage error, and the `try:` command as `fix_command`. A
pipe alone keeps the text form. The exit code is the same.

```
{"error":{"code":"not_found","category":"not_found","message":"cannot open app.db: no such file. Create it with `eggdb create app.db`, or pass --create to put, sql or serve.","fix_command":"eggdb create app.db"}}
{"error":{"code":"invalid_arguments","category":"usage","message":"unknown option '--reverse' for eggdb get","usage":"eggdb get <file> <key> [-C <name>]","fix_command":"eggdb help get"}}
```

`eggdb sql --json` reports a failed statement the same way, with the `sqlstate`, `detail`, `hint`
and `position` MCP gives. Every code, its category and its exit are listed in
[ux/command-model.md](ux/command-model.md).

`eggdb help --json` prints every command and the MCP tools as one JSON object: each command's
options, operands, output per format, default formats, most it can do (`read`, `change`,
`destroy`), errors with their exit codes and examples, and each tool's arguments, its CLI twin,
grant and annotations; `eggdb help <command> --json` prints one command, `eggdb help table
--json` one line per verb of the noun, and `eggdb help table list --json` that verb alone.
`other_commands` is an empty array: every command is in the registry.

## Colour

Colour appears only when the stream is a terminal, `NO_COLOR` is unset or empty, and `TERM` is
not `dumb`; on Windows, also only when the console accepts virtual-terminal sequences.
`--color always|never|auto` overrides it. Keys are bold, counts and stamps dim, `ok:` green,
`damaged:` and `eggdb:` red, leaked pages yellow. Values and hex are never coloured, so copying
them stays clean.
