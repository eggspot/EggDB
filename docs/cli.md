# The `eggdb` command

`eggdb` works with an EggDB database file from a shell, a script or an AI agent: create a file,
store, read and remove values, list entries in key order, check the file's health and see its
statistics. It opens the file in its own process, like `sqlite3`: there is no server. The
design behind every choice here is [ux/cli.md](ux/cli.md) (D15, D90); the embedded Rust API is
[embedded.md](embedded.md).

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
| `help [<command>]` | | the help, or one command's help; `--help` and `-h` work after any command |
| `version` | | the version; also `--version` and `-V` |

The command comes first and the file second; options may go anywhere after the command, and
`--` ends them, so `eggdb get app.db -- -k` reads the key `-k`.

**Only `create` and `put --create` make a file.** A wrong path such as `app.bd` is an error
that says how to create the file, never a new empty database that later reads as "empty".

**Read-only commands share the file.** `get`, `scan`, `check` and `stats` can run together, and
next to an application that has the file open read-only. A program that has it open for
writing keeps them out, and `put` and `del` wait for nobody: they exit 5 at once when the file
is open anywhere (see "Exit codes").

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

| Format | `get` | `scan` | `put`, `del`, `create`, `check`, `stats` |
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
[ux/ui.md](ux/ui.md). A screen at 80 × 24, captured from the test terminal:

```
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

 ↑↓ move  Enter open  / filter  e edit  a add  d delete  c check  ? help  q quit
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
  they are. Confirming shows the key and the sizes (`58 B → 61 B`); `Enter` (for a delete,
  `d` again) then runs one write transaction (put or delete, commit) that is on disk before the
  screen redraws, and the status line says `Committed user:1 (61 B) at 09:14:03Z`. No transaction is open while an
  editor is. The editor takes values up to 4 MiB of text; store larger ones with
  `eggdb put <file> <key> @file`.
- **When a change fails**, nothing is committed and the editor keeps its text. Errors read
  `Error (<Kind>): <message>` with the engine's kind and message: `TooLarge` (one commit's
  pages must fit in the page cache, so a larger `--cache` helps), `WriterActive` (`r` waits up
  to 5 s for the other writer), `NeedsReopen` (`r` reopens the file and offers the edit again),
  `Corrupt` (`c` runs the check). A long message ends with `(m)` in the status line, and `m`
  shows it in full. Messages clear at the next key or after 8 s.
- **Check.** `c` shows the header's stats at once and runs `Database::check` on a worker
  thread, then the report (`Check  OK — every page accounted for`) or `Check  FAILED` with the
  damage; `Enter` goes back.
- **Any size.** The list is a window over a scan, so it moves as fast over a billion keys as
  over twenty; the key count runs in steps between key presses (`1,204+ keys` until it is done).
  A value is read whole when it is selected, except above 64 MiB, which waits for `Enter`.
- **Terminals.** Both stdin and stdout must be terminals; otherwise the command exits 2 with
  `eggdb ui needs a terminal; use eggdb get/scan instead`. The screen needs 40 × 10 and shows
  two panes from 80 columns. `NO_COLOR` or `--mono` drops colours, `--ascii` uses ASCII for
  box lines and arrows, and `TERM=dumb` or `--mono --ascii` draws no styles at all, which also
  suits screen readers. The terminal is put back as it was on quit, on a panic, and on
  `SIGTERM`, `SIGHUP` and `SIGQUIT` (on Windows when the console closes).

## Exit codes and errors

| Code | Meaning |
|------|---------|
| 0 | done; `check` found no damage |
| 1 | not found: `get` or `del` of a key that is not there |
| 2 | usage: unknown command or option, missing argument, bad hex, an unreadable `@file`, a command not in this release |
| 3 | database error: no such file, not an EggDB file, written by a newer EggDB, permission denied, I/O failure, key or value too large, disk full |
| 4 | damaged: `check` found damage, or any command read a damaged page |
| 5 | busy: another program has the file open |

Ctrl-C stops the command the way the operating system stops any program: shells report 130.
A `put` either committed or did not. A stopped `scan` can leave its last line cut off, since
output goes out in blocks of 64 KiB and more, so a script that reads a stopped scan should drop a
final line without a newline. Writing to a
closed pipe (`eggdb scan app.db | head`) ends the command quietly with exit 0.

Errors go to stderr as `eggdb: <what happened>: <why>. <what to do>`, never as a Rust debug dump,
and add a `try:` line when the fix is a command:

```
eggdb: cannot open app.db: no such file. Create it with `eggdb create app.db`, or pass --create to put.
  try: eggdb create app.db
eggdb: unknown option '--reverse' for eggdb get
  usage: eggdb get <file> <key>
  try: eggdb help get
```

The commands that are coming (`export`, `import`, `shell`, `backup`, `compact`) answer
`eggdb <name> is not in this release yet; see docs/roadmap.md` and exit 2, so no script is built
on a name that later changes meaning.

## Colour

Colour appears only when the stream is a terminal, `NO_COLOR` is unset or empty, and `TERM` is
not `dumb`; on Windows, also only when the console accepts virtual-terminal sequences.
`--color always|never|auto` overrides it. Keys are bold, counts and stamps dim, `ok:` green,
`damaged:` and `eggdb:` red, leaked pages yellow. Values and hex are never coloured, so copying
them stays clean.
