# The `eggdb` command-line tool (v0.2.0)

Design by Claude Fable (D15) for roadmap item 13: the one-shot commands of the CLI against an
embedded, single-file database. No server, no SQL yet. Everything here is implementable with
`std` and the `eggdb` crate alone: a hand-written argument parser, no `clap`.

The tool is for two readers at once: a person at a terminal, and a script or an AI agent on a
pipe. Every output has a form for each, chosen by whether stdout is a terminal, and every
message says what happened, why, and what to do next.

## 1. Grammar

```
eggdb <command> <file> [arguments] [options]
eggdb --help | -h | help [<command>]
eggdb --version | -V | version
```

**Command first, file second.** The alternative, `eggdb <file> <command>`, reads like
`sqlite3 app.db ...`, but command-first wins on three points: the storage engine's own error
messages already say "run `eggdb check`", so `eggdb check app.db` must be a thing people can
type from them; `eggdb help scan` and `eggdb scan --help` work without a file; and later
commands have a different shape (`eggdb backup app.db app.bak` takes two files, `eggdb ui` a
file and a screen), which command-first absorbs and file-first does not.

Parsing rules, in order:
1. `--` ends options; everything after it is a positional argument, even if it starts with `-`.
2. An option is `--name value`, `--name=value`, or a flag; short forms exist only where listed.
   Options may appear anywhere after the command, before or after positionals.
3. The first positional is the command, the second the file, the rest the command's arguments.
   An unknown command, option, or wrong count of arguments is a usage error (exit 2) that
   prints the one-line usage of that command and `try 'eggdb help <command>'`, never the whole
   help.
4. `-` alone is a positional argument (it means stdin or stdout where a file is expected).

## 2. Commands in this release

| Command | Opens the file | Does |
|---------|----------------|------|
| `create <file>` | creates, fails if present | makes an empty database; `--page-size <bytes>` (4096 default, power of two to 65536) |
| `put <file> <key> [<value>]` | writing | stores the value under the key, replacing any earlier value |
| `get <file> <key>` | read-only | prints the value, or exits 1 |
| `del <file> <key>` | writing | removes the key; exits 1 if it was not there |
| `scan <file> [options]` | read-only | prints entries in key order |
| `check <file>` | read-only | reads every page and reports the file's health |
| `stats <file>` | read-only | prints the file's size, pages, last commit and log |
| `export <file> [<out>\|-]` | read-only | writes every entry as JSON lines (shipped in item 16, D93) |
| `import <file> [<in>\|-]` | writing; `--create` makes it | loads an export (shipped in item 16, D93) |
| `help [<command>]`, `version` | none | |

`delete` is accepted as an alias of `del`; `del` is the documented name because it matches
`put` and `get` in length and is what Redis users type.

**Reserved names**, so nobody builds a script around a name that later changes meaning:
`export`, `import`, `shell`, `backup`, `compact`, `ui`. In v0.2.0, before each ships, typing one
prints `eggdb <name> is not in this release yet; see docs/roadmap.md` and exits 2. How they fit
the grammar when they come: `export <file> [<out>|-]` and `import <file> [<in>|-]` (JSON lines,
same byte rules as section 4; shipped as designed, D93); `shell <file>` runs the commands of this
document without the `eggdb <command> <file>` prefix, one per line, and SQL (shipped, section
15); `backup <file> <copy>` copies a consistent snapshot while others write; `compact <file>`
rewrites the file without free or leaked pages; `ui <file>` opens the terminal UI (its own
design document).

### Creating a file: explicit, never by accident
`put` and `del` **do not create** a database. A typo in the path (`app.bd`) must not leave a
stray empty file that a later command then happily reads as "empty", so a missing file is an
error (exit 3) with the fix in the message: `eggdb create app.bd` or `--create`.

- `eggdb create <file>` is the normal way; the quickstart shows it.
- `eggdb put --create <file> <key> <value>` creates on the way, for one-liners and scripts.
- `create` on an existing path fails (exit 3): `app.db already exists; eggdb create never
  replaces a file`. It never truncates anything.
- `eggdb import --create <file> <export>` creates the file to restore into (D93).
- The library's `create_if_missing` is set `false` for every command except `create`,
  `put --create` and `import --create`.

### Read-only opens
`get`, `scan`, `check` and `stats` open with `read_only(true)`. A read-only open takes a shared
lock, so several of them run at once and alongside an application that has the file open
read-only; a writer refuses them and they refuse a writer (section 7, busy).

### Writing large values
A `put` opens with a page cache sized to the value, so any value up to the 1 GiB limit works
without an option: `cache_bytes = max(16 MiB, value_len + value_len / 64 + 1 MiB)`. The
`--cache <bytes>` option overrides it for all commands (sizes take `k`, `m`, `g` suffixes:
`--cache 64m`).

## 3. Keys and values on the command line

A key or value argument is a **byte argument**. Its first characters say how to read it:

| Form | Bytes |
|------|-------|
| `user:1` | the text as typed, UTF-8 |
| `0x48656c6c6f` | hex, an even number of hex digits after `0x`; case does not matter |
| `@path/to/file` | the file's contents, exactly |
| `@-` | everything on stdin, exactly |
| `=text` | the text after the first `=`, literally, for text that starts with `0x`, `@` or `=` |

An empty argument (`""` or `=`) is zero bytes, a valid key and value. Scripts that handle
arbitrary data prefix every argument with `=`: `eggdb put app.db "=$key" "=$value"`.

`put` may omit the value when stdin is **not** a terminal: `cat photo.jpg | eggdb put app.db
photo` is the same as `... photo @-`. When stdin is a terminal the value is required (a
forgotten value is a usage error, not an empty value). Bytes are stored exactly, so `echo hi |`
stores a trailing newline and `printf hi |` does not; the help says so.

Hex errors are usage errors: `0x4` gives `hex needs an even number of digits: 0x4`; `0x4g`
gives `not a hex digit in 0x4g: 'g'`.

## 4. Output formats

`--format text|json|hex|raw` (short `-F`). The default depends on stdout:

| stdout | `get` | `scan`, `check`, `stats`, `del`, `put`, `create` |
|--------|-------|-----------------------------------------------|
| a terminal | `text` | `text` |
| a pipe or file | `raw` | `json` |

So `eggdb get app.db photo > photo.jpg` round-trips bytes, `eggdb scan app.db | jq` parses, and
a person sees readable text. Nothing is ever lost or altered in `json`, `hex` or `raw`; only
`text` abbreviates, and it says when it does.

**Rendering bytes in `text`.** Bytes that are valid UTF-8 with no control character (U+0000 to
U+001F, U+007F) are shown as text. Otherwise they are shown as `0x` hex. Exception: `get` shows
a UTF-8 value whose only control characters are newline, tab and carriage return as text, since
it prints the whole value and nothing structural follows it. In `scan`, a value longer than 128
bytes is cut after 128 bytes, followed by ` … (12,345 bytes)`; keys are never cut (they are at
most 8 KiB). Counts get thousands separators; sizes are shown in B, KiB, MiB or GiB with one
decimal.

**`text`**
```
$ eggdb get app.db user:1
Ada
$ eggdb scan app.db --prefix user:
user:1    Ada
user:2    Grace
user:3    0x00ff10
3 entries
$ eggdb put app.db user:4 Linus
$ eggdb del app.db user:4
```
Key and value are separated by a tab (shown above as spaces); `put` prints nothing, `del`
prints nothing, `create` prints `created app.db (4 KiB pages)`. The entry count after a scan
goes to **stderr**, so `scan | wc -l` on a terminal-less pipe is unaffected, and so is a
copy-paste of the entries.

**`json`**: one JSON object per line (JSON lines). Keys and values are JSON strings when valid
UTF-8; otherwise the field is `key_hex` / `value_hex` holding lowercase hex, and `key` /
`value` is absent. Every number is a plain integer. Fields are stable: scripts may rely on them.
```
{"key":"user:1","value":"Ada"}
{"key":"user:3","value_hex":"00ff10"}
```
`get` in json: `{"key":"user:1","value":"Ada"}`; not found: nothing on stdout, exit 1.
`put`: `{"key":"user:4","stored":true}`. `del`: `{"key":"user:4","deleted":true}` or
`{"key":"user:9","deleted":false}` (exit 1). `create`: `{"file":"app.db","page_size":4096}`.

**`hex`**: `<key hex>\t<value hex>\n` per entry, `<key hex>\n` with `--keys-only`, `<value
hex>\n` for `get`. Binary-safe and trivially parsed in any language.

**`raw`**: the bytes themselves. `get` writes the value and nothing else, not even a newline.
`scan`: `key\tvalue\n` with nothing escaped, only for data known to be text; `--keys-only`
gives one key per line (`eggdb scan app.db --keys-only -F raw | xargs ...`).

## 5. `scan` options

| Option | Meaning |
|--------|---------|
| `--prefix <bytes>`, `-p` | keys that start with these bytes |
| `--from <bytes>` | first key, inclusive |
| `--to <bytes>` | end key, **exclusive**, like the API's `"a".."m"` and every key-value store's range |
| `--reverse`, `-r` | largest key first |
| `--limit <n>`, `-n` | stop after n entries (`-r -n 1` is "the last entry") |
| `--keys-only`, `-k` | keys without values |

`--prefix` with `--from` or `--to` is a usage error: `--prefix cannot be combined with --from or
--to`. `--from` above `--to` is an empty scan, not an error. All range arguments are byte
arguments (section 3), so `--from 0x00` works. The scan runs in one read transaction, so it
sees one snapshot however long it takes.

## 6. `check` and `stats` reports

Both are written to be read line by line by a person or a model: one fact per line, a fixed
label per fact, the same labels in the JSON field names, and a final verdict line that starts
with `ok:` or `damaged:`.

**`check`, healthy** (exit 0):
```
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
With leaked pages (still exit 0, since no data is at risk) the last two lines read:
```
  leaked pages    3 (harmless: used by nothing, never reused; eggdb compact will reclaim them)
ok: every page passed its checksum, every key is in order, and 3 pages are in use by nothing.
```
**`check`, damaged** (exit 4). The library's `Corrupt` message names the page and the problem;
the CLI adds what was checked and what to do, as fixed lines:
```
checked app.db as of commit 2026-10-26 09:14:07.123456 UTC (stamp 1793006047123456.0003)
damaged: the B+tree page 77 is damaged: a key is out of order
  what was checked  every page read before the damage passed its checksum; the check stopped at the damage
  what to do        EggDB does not repair files yet. Restore app.db from your latest backup.
                    Keep the damaged file unchanged: `eggdb scan` may still read part of the data,
                    and an issue at https://github.com/eggspot/EggDB/issues with this output helps us.
```
`check` JSON: `{"ok":true,"file":"app.db","commit_stamp":"1793006047123456.0003",
"commit_time":"2026-10-26T09:14:07.123456Z","page_size":4096,"pages":512,"file_bytes":2097152,
"entries":12480,"key_bytes":202957,"value_bytes":1677722,"tree_height":3,"tree_pages":441,
"overflow_values":3,"overflow_pages":12,"free_pages":40,"log_pages":16,"leaked_pages":0}`, and
when damaged `{"ok":false,"file":"app.db","problem":"the B+tree page 77 is damaged: a key is
out of order","page":77,"what_to_do":"..."}` with `page` present when the message names one.

**`stats`** reads no page and is instant:
```
app.db
  page size       4 KiB
  pages           512 (40 free)
  file size       2.0 MiB
  last commit     2026-10-26 09:14:07.123456 UTC (stamp 1793006047123456.0003)
  log since checkpoint  0 B (the next open replays nothing)
```
A never-written file shows `last commit     none`. Stamps are shown as UTC time plus the raw
stamp, so the time is readable and the stamp is exact (civil-from-days arithmetic, no crate).
JSON: `{"file":"app.db","page_size":4096,"pages":512,"free_pages":40,"file_bytes":2097152,
"last_commit_stamp":"...","last_commit_time":"...","log_bytes_since_checkpoint":0}`; `last_commit_stamp`
and `last_commit_time` are `null` when there was none. The library's page-cache figures (`cache_*`)
are left out: they describe the process that holds the file open, which for a one-shot command
is the moment of the call, and would only mislead.

## 7. Exit codes and errors

| Code | Meaning | Examples |
|------|---------|----------|
| 0 | done | `get` found the key; `check` found no damage |
| 1 | not found | `get` or `del` of a key that is not there |
| 2 | usage | unknown command or option, missing argument, bad hex, a reserved command |
| 3 | database error | file missing (no `--create`), not an EggDB file, newer format, permission denied, I/O failure, key or value too large, disk full |
| 4 | damaged | `check` found corruption, or any command hit `Corrupt` while reading |
| 5 | busy | the file is open elsewhere and the lock could not be had |
| 130 | interrupted | Ctrl-C; a write in progress is rolled back by the engine, nothing is half-written |

Errors go to stderr as `eggdb: <what happened>: <why>. <what to do>`, one or two sentences,
no Rust `Debug` output, no `Error { kind: ... }`, no backtrace. The library's messages already
have this shape; the CLI adds the file name once and, where the fix is a command, a second
line `  try: <command>`. On a terminal, `eggdb:` is red and `try:` is bold.

```
eggdb: cannot open app.db: no such file. Create it with `eggdb create app.db`, or pass --create to put.
  try: eggdb create app.db
```
```
eggdb: cannot open app.db: another program has it open for writing. Close it, or wait and run the command again.
```
```
eggdb: cannot open app.db: another program has it open. Close it before writing, or read with `eggdb get`, `scan`, `check` or `stats`, which work alongside readers.
```
The busy message names which side is blocked: a read-only command blocked by a writer gets
the first busy line; a `put`/`del` blocked by any open gets the second.

Interrupt handling: the CLI keeps the operating system's default for Ctrl-C, so the process
ends at once with the shell's interrupted status (130 on POSIX shells; 0xC000013A on Windows).
Output goes out in blocks of 64 KiB and more, which a pipe does not take atomically, so a
killed `scan` can leave a partial last line in any format: a script that reads a stopped scan
should drop a final line without a newline. A `put` is atomic in the engine, so an interrupt
either committed it or did not. Writing to a closed pipe
(`scan | head`) exits 0 quietly, never with a broken-pipe panic.

## 8. Colour

Colour is used only when **all** hold: the stream (stdout for data, stderr for messages) is a
terminal; `NO_COLOR` is unset or empty (its value does not matter, following no-color.org);
`TERM` is not `dumb`. `--color always|never|auto` (default `auto`) overrides, for `watch` and
CI logs. Plain 16-colour SGR only, no 256-colour or truecolor, so it reads in every terminal:

| Element | SGR |
|---------|-----|
| keys in `scan` | bold (`1`) |
| `… (N bytes)`, the entry count, `(stamp …)` | dim (`2`) |
| `ok:` | green (`32`) |
| `damaged:`, `eggdb:` | red (`31`) |
| `leaked pages` with a non-zero count | yellow (`33`) |
| `try:`, labels in reports | bold (`1`) |

Never colour the data itself (values, hex): copy-paste must stay clean.

On Windows, colour needs the console's virtual-terminal mode; eggdb turns it on for the stream
and uses colour only if the console accepts it. `std::io::IsTerminal` decides what is a
terminal. Both live in the platform layer (`eggdb_platform::terminal`).

## 9. Help and version: exact text

`eggdb --version`, `-V`, `eggdb version` (the crate version at build time):
```
eggdb 0.2.0
```
`eggdb --help`, `-h`, `help`, or no arguments at all (no arguments also exits 2, since nothing
was done):
```
eggdb: one database for every kind of data. This release: an embedded key-value store in one file.

Usage: eggdb <command> <file> [arguments] [options]

Commands:
  create <file>                 make an empty database (fails if the file exists)
  put    <file> <key> [value]   store a value; the value may come from stdin
  get    <file> <key>           print a value (exit 1 if the key is not there)
  del    <file> <key>           remove a key (exit 1 if it was not there)
  scan   <file>                 list entries in key order; see --prefix, --from, --to
  check  <file>                 read every page and report the file's health
  stats  <file>                 size, pages, last commit and log, without reading pages
  help   [command]              this text, or one command's help
  version                       print the version

Keys and values: text as typed, 0x.. for hex, @file for a file's bytes, @- for stdin,
=text for text that starts with 0x, @ or =.

Options for every command:
  -F, --format <text|json|hex|raw>   default: text on a terminal, json in a pipe (raw for get)
      --color <auto|always|never>    default: auto (a terminal, unless NO_COLOR is set)
      --cache <bytes>                page cache size, e.g. 64m (default fits the value)
  -h, --help                         help for eggdb or for one command
  -V, --version                      print the version

Exit codes: 0 ok, 1 not found, 2 usage, 3 database error, 4 damaged, 5 busy.
Guide: docs/cli.md in the repository, https://github.com/eggspot/EggDB
```
`eggdb help scan` (the pattern for every command: usage, one paragraph, options, examples):
```
Usage: eggdb scan <file> [--prefix <bytes> | --from <bytes> [--to <bytes>]] [-r] [-n <count>] [-k]

Lists entries in key order, from one snapshot. Keys are compared as unsigned bytes.

  -p, --prefix <bytes>   keys that start with these bytes
      --from <bytes>     first key, inclusive
      --to <bytes>       end key, exclusive
  -r, --reverse          largest key first
  -n, --limit <count>    stop after this many entries
  -k, --keys-only        print keys only

Examples:
  eggdb scan app.db --prefix user:
  eggdb scan app.db --from 2026-01 --to 2026-02 --format json
  eggdb scan app.db -r -n 1
```

## 10. Exact strings for the implementer

| Where | String |
|-------|--------|
| missing file, read command | `cannot open {file}: no such file. Check the path, or create it with ` `` `eggdb create {file}` `` |
| missing file, `put`/`del` | `cannot open {file}: no such file. Create it with ` `` `eggdb create {file}` ``, `or pass --create to put.` + `  try: eggdb create {file}` |
| `create` on existing | `{file} already exists; eggdb create never replaces a file. Use it as it is, or choose another name.` |
| busy, read command | `cannot open {file}: another program has it open for writing. Close it, or wait and run the command again.` |
| busy, write command | `cannot open {file}: another program has it open. Close it before writing, or read with ` `` `eggdb get` ``, `` `scan` ``, `` `check` `` `or` `` `stats` ``, `which work alongside readers.` |
| not a database | `cannot open {file}: not an EggDB database. If it is a database of another kind, EggDB restores its backups in a later release (docs/roadmap.md).` |
| newer format | `cannot open {file}: it was written by a newer EggDB ({detail}). Upgrade eggdb from https://github.com/eggspot/EggDB/releases.` |
| key not found (`get`, text, stderr) | `key not found: {key}` |
| `del` of absent key (text, stderr) | `nothing to delete: {key} is not in {file}` |
| value missing on a terminal | `put needs a value: give it as an argument, as @file, or pipe it to stdin` |
| unknown command | `unknown command '{name}'. Commands: create, put, get, del, scan, check, stats, ui, help, version.` |
| reserved command | `eggdb {name} is not in this release yet; see docs/roadmap.md` |
| unknown option | `unknown option '{opt}' for eggdb {command}` + `  try: eggdb help {command}` |
| bad format | `--format must be text, json, hex or raw, not '{value}'` |
| bad size | `{option} needs a size such as 4096, 64k, 16m or 1g, not '{value}'` |
| prefix with range | `--prefix cannot be combined with --from or --to` |
| scan count (stderr, text) | `{n} entries` / `1 entry` / `no entries` |
| scan truncation | ` … ({n} bytes)` |
| `check` ok | `ok: every page passed its checksum, every key is in order, and every page of the file is accounted for.` |
| `check` ok, leaked | `ok: every page passed its checksum, every key is in order, and {n} pages are in use by nothing.` |
| `check` damaged | `damaged: {library message}` then the two `what was checked` / `what to do` lines of section 6 |
| `create` done (text) | `created {file} ({page size} pages)` |
| Ctrl-C | nothing printed by eggdb; exit 130 (the signal's status, the OS default) |

Message prefix on stderr is always `eggdb: ` for errors; informational lines (`{n} entries`,
`created …`) have no prefix. Every placeholder is rendered by the `text` rules of section 4,
so a binary key appears as `0x…` in a message.

## 11. README quickstart

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
```

## 12. Decisions in one place

1. Command first (`eggdb <command> <file>`), for help without a file, two-file commands and the
   `eggdb check` the engine's messages already quote.
2. Files are created only by `eggdb create`, `put --create` or `import --create`; a wrong path
   is an error, not a new file.
3. Byte arguments: text, `0x` hex, `@file`, `@-`, `=` escape; stdin is the value when piped.
4. Formats `text|json|hex|raw`; terminal gets `text`, a pipe gets `json`, except `get`, which
   gets `raw` so bytes round-trip. Only `text` abbreviates, and says so.
5. `--to` is exclusive; `--prefix` excludes `--from`/`--to`.
6. Reports: one labelled fact per line, same labels as the JSON fields, verdict line `ok:` or
   `damaged:`; damage adds "what was checked" and "what to do".
7. Exit codes 0 ok, 1 not found, 2 usage, 3 database error, 4 damaged, 5 busy, 130 interrupted.
8. Errors: `eggdb: what: why. what next` plus an optional `try:` line; never a Debug dump.
9. Colour only on a terminal with `NO_COLOR` unset and `TERM` not `dumb`; 16-colour SGR; data
   is never coloured.
10. `stats` omits the page-cache counters; `check` with leaked pages is still `ok` (exit 0).
11. Reserved: `backup`, `compact`; they answer with the roadmap until they ship (`ui`,
    `export`, `import`, `sql`, `serve` and `shell` have shipped).

## 13. As built in v0.2.0 (D90)

The implementation follows this document; where it was silent or a detail could not be built
as written, it did this, and the sections above were updated where they changed:

1. **Ctrl-C** keeps the operating system's default (section 7), decided by research, so a
   killed `scan` can leave a partial last line, unlike the design's first wish: a handler
   would add signal code per OS and buy nothing, since `put` is atomic and `check` cannot stop
   half-way. `eggdb ui` brings signal handling with its terminal layer.
2. **The damaged `check` report** says `every page read before the damage passed its checksum;
   the check stopped at the damage`: the library reports no progress, and a B+tree check is not
   a left-to-right prefix, so "the tree down to page 77" could not be stated truthfully. Damage
   found while the file opens prints `checked app.db` with no commit and `what was checked
   opening the file read its header, free list and log; it stopped at the damage`. The
   library's closing advice (restore from a backup, or run eggdb check) is removed from the
   `damaged:` line and the JSON `problem`, since the `what to do` line replaces it.
3. **`--version`** prints the crate version, so `eggdb 0.1.0` until the v0.2.0 bump.
4. **`hex` and `raw`** are for `get` and `scan`; another command with them is a usage error:
   `--format hex is for get and scan; eggdb check prints text or json`.
5. **Usage errors** print `eggdb: <message>`, then `  usage: <the command's one-line usage>`
   when a command is known, then `  try: eggdb help <command>` (or `  try: eggdb help`).
6. **No arguments** prints the help to stderr, since it exits 2.
7. **Not-found messages** (`key not found`, `nothing to delete`) go to stderr in `text`, `hex`
   and `raw`; in `json` stdout carries the answer (`"deleted":false`) or nothing (`get`).
8. **Byte arguments**: an unreadable `@file` is a usage error (exit 2), `cannot read @<path>:
   <reason>`; stdin can be read once per command; the path after `@` must be valid Unicode.
9. **Other errors** keep the library's message after a context the CLI adds:
   `cannot open <file>: …`, `cannot store <key> in <file>: …`, `cannot read <key> from <file>: …`,
   `cannot delete <key> from <file>: …`, `cannot scan <file>: …`.
10. **Counts of one** read `1 page`, `1 overflow page`, `1 page is in use by nothing`; an empty
    file's check reads `checked app.db as of commit none`.
11. **`--page-size`** is checked by the CLI (a usage error): `--page-size must be a power of two
    from 4096 to 65536, not '<value>'`. `--limit` takes a whole number: `--limit needs a whole
    number, not '<value>'`. `--color` takes `auto`, `always` or `never`.
12. **Control characters in `text`** include the C1 range (U+0080 to U+009F) as well as
    U+0000 to U+001F and U+007F (Rust's `char::is_control`), since some terminals act on C1
    codes such as U+009B: bytes holding one are shown as `0x` hex, so stored data can never
    drive the terminal.
13. **Every placeholder from the user**, the file name, an unknown command or option and a
    refused option value included, is rendered by the `text` rules of section 4 in messages,
    so a name with control characters appears as `0x…`. JSON keeps the file name as given.
14. **`--help` after an unknown command** (`eggdb frob --help`) is the unknown-command error,
    exit 2, not the main help.
15. **`put --create` on a missing file** refuses a key or value too long for a new file before it
    creates the file, so a refused put leaves no empty file behind.
16. **Help for each command** follows the `scan` pattern of section 9: usage, a paragraph,
    options, examples.
17. **`export` and `import`** (item 16, D93) follow the grammar of section 2: the second argument
    is the export file, `-` or nothing meaning stdout or stdin. `export` never replaces a file;
    `import` makes one only with `--create`, building it as `<file>.importing` and renaming it
    when the import succeeded. Their errors read `eggdb: cannot import <in>: line <n>: <what is
    wrong>. <what the failure left>`, for example `… Nothing was imported.`; a key already there
    adds `Choose what happens to keys already there: --on-conflict overwrite or --on-conflict
    skip`; the empty key appears as `""`, and a key longer than 64 bytes is cut with its length.
    Progress (`imported 1,204,000 entries…`) is redrawn on stderr once a second, only on a
    terminal.

## 14. `eggdb serve`: the file over the PostgreSQL protocol (item 21, D102, D103)

`eggdb serve <file>` keeps the grammar of section 1 (command, file, options) and reuses
`--create` and `--read-only` as `put`, `import`, `sql` and `ui` define them. Designed by Claude
Fable. The choices:

1. **One `--listen <address>`**, not `--host` and `--port`: a listener is one address. A bare
   port means `127.0.0.1:port`, since that is what people type; `localhost` is a spelling of
   `127.0.0.1` with no name lookup, so a hosts file cannot turn it into a refused address.
   The default port is PostgreSQL's 5432, so `psql -h 127.0.0.1` needs no `-p`; a port already
   taken (often by PostgreSQL itself) is exit 3 with a `try:` line naming the next port. Port 0
   takes any free port for CI.
2. **Loopback only** until TLS exists (owner rule, D102): any other address is a usage error
   whose message says why (no password in the clear) and what to type instead.
3. **The password never comes from the command line** (`ps` shows it): `EGGDB_PASSWORD`, or
   `--password-file` (first line; `-` is stdin, as everywhere in this grammar). When both are
   set the option wins, as a libpq parameter beats `PGPASSWORD`. The password is kept only as
   its SCRAM verifier after startup. Missing both is a usage error that names both ways.
4. **The database name is the file stem** (`app.db` is `app`), what a psql user would guess;
   `--database` overrides it. Other names get PostgreSQL's own `3D000` error, so tools
   understand.
5. **Startup is a report** in the style of `stats`: a fixed first line `listening on <address>`
   with the address actually bound, then labelled facts and a ready-to-paste
   `psql postgresql://…` URI, which psql, drivers and GUIs all accept. In a pipe it is one JSON
   line (section 4's rule), so scripts read the port with `jq`, never a regex.
6. **stderr is a log** while serving: failures only (authentication, unknown database, too many
   connections), each with a UTC time, the client address and PostgreSQL's own wording,
   without the `eggdb:` prefix, which stays for the server's own errors.
7. **Stopping is success**: Ctrl-C or SIGTERM finishes statements, rolls back open
   transactions, prints `stopped` and exits 0, so a supervising script's `wait` sees a clean
   stop; a second Ctrl-C ends the process at once (exit 130).
8. **The file is open for writing the whole time**, so every other `eggdb` command gets the
   busy message and exit 5 meanwhile; `--read-only` serves beside readers instead.

## 15. `eggdb shell`: the interactive prompt (item 20)

`eggdb shell <file>` is one prompt for everything this document and [sql.md](../sql.md) can
do with a file: the key-value commands without their `eggdb <command> <file>` prefix, SQL in
the PostgreSQL dialect, and a few `.meta` commands. It is psql for an EggDB file, built on the
terminal layer of `eggdb-ui` (raw mode, the escape-sequence decoder, the single-line editor)
and on the `sql` command's result and error printers, which it reuses unchanged. Designed by
Claude Fable (D15).

```
eggdb shell <file> [-c <text>]... [--keep-going] [--mode table|expanded|csv] [--read-only] [--create]
```
`--read-only`, `--create` and `--cache` mean what they mean for `sql` and `serve`. The file is
open for writing for the whole session (read-only with `--read-only`), so other writers get the
busy message meanwhile, as with `serve`.

### Interactive or batch
The shell is **interactive** when stdin and stdout are both terminals and no `-c` is given.
Otherwise it is a **batch**: it runs each `-c` text in order, or, without `-c`, everything on
stdin, with no banner, no prompt, no colour, no completion and no history, in `table` mode
whatever the width, so its output is stable for scripts. A batch stops at the first failure
(exit code below) unless `--keep-going`, which runs on and exits with the first failure's code.
`eggdb sql` remains the command for scripts of pure SQL; the batch form exists so that what
was typed at the prompt can be pasted into a script unchanged.

### Banner, prompt, exit
The banner is two lines; the second names the three things a newcomer needs:
```
EggDB 0.2.0: app.db, open read-write (12 tables, 1,204 keys)
Type .help for help. SQL ends with ; and .quit or Ctrl-D quits.
```
(`open read-only` with `--read-only`; the counts come from the catalog and the key count of
`stats`, left out while a file has no SQL tables: `(1,204 keys)`.)

The prompt is the file's stem, as `serve` names the database (`app.db` is `app`), followed by
psql's markers, which a psql user reads without learning:

| Prompt | When |
|--------|------|
| `app=> ` | a new statement or command |
| `app-> ` | a SQL statement continues on this line (no `;` yet) |
| `app=*> ` / `app-*> ` | inside an open transaction block |
| `app=!> ` / `app-!> ` | inside a failed block (every statement answers `25P02` until `ROLLBACK`) |
| `app'> ` / `app"> ` | inside an unfinished `'…'` or `"…"` of a statement |

A stem longer than 24 characters is cut to 23 plus `…`. The prompt is bold.

`.quit`, `.exit` and `Ctrl-D` on an empty line end the shell with exit 0; an open transaction
block is rolled back first, with `WARNING:  the shell ended inside a transaction block (BEGIN
without COMMIT); it was rolled back` on stderr, the `sql` command's words. `Ctrl-D` on a
non-empty line does nothing.

### Reading a line
Raw mode is on only while a line is read, and off again before anything runs, so output scrolls
as every command's does and a `Ctrl-C` during a long statement ends the process the way it ends
every `eggdb` command (section 7: exit 130, the engine rolls the write back; a statement cannot
be cancelled in this release). The line is one row that scrolls horizontally, the UI's
single-line editor, with its keys plus the readline keys people's fingers know:

| Key | Does |
|-----|------|
| `←` `→`, `Home` `End`, `Ctrl-A` `Ctrl-E` | move by one character, to the line's start or end |
| `Alt-B` `Alt-F`, `Ctrl-←` `Ctrl-→` | move by one word (a word is a run of letters, digits and `_`) |
| `Backspace` `Delete` | delete the character before or under the cursor |
| `Ctrl-W` | delete the word before the cursor (and the spaces after it) |
| `Ctrl-U` `Ctrl-K` | delete to the line's start or end |
| `Ctrl-Z` `Ctrl-Y` | undo and redo within the line (the editor's own) |
| `Tab` | complete (below) |
| `↑` `↓` | the previous or next history entry; the line being typed is kept at the newest end |
| `Ctrl-R` | reverse search in the history (below) |
| `Ctrl-C` | clears the line and drops an unfinished statement: prints `^C`, back to `app=> ` (an open transaction block is kept) |
| `Ctrl-D` | on an empty line, exit |
| `Ctrl-L` | clears the screen and redraws the prompt with the line as it was |
| `Enter` | sends the line |

A paste (bracketed) is inserted as text; its line breaks send the lines one after another, as
if typed, so a pasted script runs line by line and, being interactive, goes on past an error.
`Esc` alone does nothing. A resize redraws the line at the new width.

### What a line is
The first word of a line decides, in this order:
1. **A meta-command**: the line starts with `.`. One line, never continued.
2. **A key-value command**: the first word is exactly `put`, `get`, `del`, `scan`, `check` or
   `stats`, in lower case, and the line is the command's arguments as the CLI takes them
   (section 3's byte arguments, section 5's `scan` options, `--format`), split like a shell:
   words separated by spaces, a word in `"…"` or `'…'` may hold spaces, `\"` and `\\` inside
   double quotes. One line, never continued. `@-` is a usage error here (`@- reads stdin, which
   is the shell's own; use @file`), and so is `put` without a value. `delete` is **not** a
   key-value command at the prompt, so `delete from t` in lower case is the SQL it looks like;
   `.help` says so. No SQL statement begins with any of the six words, in any dialect EggDB
   speaks, which is what keeps the rule free of a prefix character.
3. **SQL** otherwise: the line, and every line after it, until a `;` outside quotes, comments
   and `$$` bodies ends the statement (several statements on one line run in order). An empty
   line at `app=> ` does nothing; at `app-> ` it is part of the statement.

### Meta-commands and their exact help
```
app=> .help
Key-value commands, as on the command line without 'eggdb <command> app.db':
  put <key> <value>            store a value (text, 0x.. hex, @file, =text)
  get <key>                    print a value
  del <key>                    remove a key
  scan [--prefix p] [--from a] [--to b] [-r] [-n count] [-k]
  check                        read every page and report the file's health
  stats                        size, pages, last commit and log
SQL: PostgreSQL dialect, statements ended by ; (docs/sql.md); BEGIN spans prompts:
  app=*> is inside a transaction block, app=!> a failed one (ROLLBACK ends it).
  'delete' starts a DELETE statement here; the key-value command is 'del'.
Shell commands:
  .tables                      list the tables
  .schema [table]              CREATE TABLE statements, all or one
  .mode [table|expanded|csv]   how results print; no argument shows the current mode
  .timer [on|off]              print each command's time
  .help                        this text
  .quit  .exit                 leave (so does Ctrl-D on an empty line)
Keys: Tab completes, Up/Down and Ctrl-R recall, Ctrl-C clears the line, Ctrl-L the screen.
History: ~/.local/state/eggdb/history (EGGDB_HISTORY=<path> moves it; set empty, turns it off)
```
The history line shows the path actually in use. `.tables` prints one name per line, sorted,
or `no tables` dimmed. `.schema` prints `CREATE TABLE name (col type NOT NULL, …, PRIMARY KEY
(…));` as the catalog defines it, one statement per table; an unknown table is `eggdb: no table
named orders` (exit 1 in batch). `.mode` alone answers `mode: table`; `.timer` alone `timer:
off`. Unknown: `eggdb: unknown command '.foo'. Type .help for the list.`

### Results and errors
A SQL result prints exactly as `eggdb sql` prints it (aligned header and rule, numbers
right-aligned, NULL as an empty cell, `(3 rows)` then an empty line; command tags such as
`INSERT 0 1`, `UPDATE 2`, `CREATE TABLE`, `BEGIN`, `ROLLBACK`). The modes:
- `table` (default): psql's aligned format. On a terminal, a result wider than the terminal is
  printed `expanded` instead, for that result only (psql's `\x auto`), so nothing is cut and
  nothing wraps mid-cell. In batch there is no width and `table` is always `table`.
- `expanded`: one record per block, psql's vertical format, `-[ RECORD 1 ]-+-------` headers.
- `csv`: RFC 4180 with a header row, `"` doubled, NULL empty, no row count, for pasting into a
  spreadsheet. Command tags still print.

Key-value commands print what the CLI prints in `text` (section 4) with `scan`'s count on
stderr; `--format json|hex|raw` on the line switches that line. `check` and `stats` print their
reports. `.timer on` adds `Time: 1.234 ms` (dim, stderr, three decimals; `s` above 1,000 ms)
after every SQL statement, key-value command and `.schema`.

A SQL error is `eggdb sql`'s, to the character, with `ERROR:` red:
```
app=> select name from users where id = 1;
ERROR:  relation "users" does not exist
LINE 1: select name from users where id = 1;
                         ^
SQLSTATE:  42P01
```
`LINE n` counts from the statement's first line, so a continued statement says `LINE 3`. A
key-value failure is the CLI's `eggdb: …` line (`eggdb: key not found: user:9`, `eggdb: cannot
store user:1 in app.db: …`, `try:` lines included), red prefix on a terminal. Colour follows
section 8: a terminal, `NO_COLOR` unset, `TERM` not `dumb`, `--color` to override; the prompt
and column headers are bold, `ERROR:` and `eggdb:` red, `WARNING:` yellow, `(3 rows)`, `Time:`
and `^C` dim. Data is never coloured.

### Tab completion
Completion is computed from the catalog and the keys, never from a word list the user cannot
see, and it is case-honest: a keyword completes in upper case unless the letters typed are all
lower case. What completes where:

| Cursor is | Candidates |
|-----------|------------|
| in the first word of a line | the six key-value commands, the meta-commands with their `.`, and the statement keywords (`SELECT`, `INSERT`, `UPDATE`, `DELETE`, `CREATE`, `DROP`, `BEGIN`, `COMMIT`, `ROLLBACK`, `SET`, `SHOW`, `RESET`, `START`, `END`, `ABORT`) |
| after `FROM`, `JOIN`, `INTO`, `UPDATE`, `TABLE`, `.schema` | table names |
| elsewhere in SQL | the columns of every table the statement names so far, then SQL keywords and function names from sql.md (`WHERE`, `GROUP BY`, `ORDER BY`, `LIMIT`, `count`, `coalesce`, …) |
| after `get`, `del`, `scan --prefix`, `scan --from`, `scan --to` | keys that start with the word, from a prefix scan, at most 50 |
| after `.mode`, `.timer` | their arguments |

One candidate completes the word and adds a space (a table name after `.schema` too; a key
adds none). Several complete the longest common prefix; a second `Tab` on the same word lists
the candidates below the line in columns the width of the longest plus two, at most 200 (then
`… and 312 more`), and redraws the prompt and line under them. No candidate: nothing happens,
no bell. Names are completed as the catalog spells them; a name that needs quoting is listed as
`"Mixed Case"` and inserted with its quotes.

### History
Every statement and command sent is one entry, appended to the history file as it is sent
(a crash loses nothing), a multi-line statement as one entry with its line breaks written
`\n` and a backslash `\\`, read back the same way. Not stored: an empty line, a line that starts
with a space (the shell convention for "keep this out"), `.quit`/`.exit`, and an entry equal
to the previous one. The file keeps the newest 1,000 entries (trimmed on exit), is created
with mode 0600 in a directory of mode 0700, and lives at:

| OS | Path |
|----|------|
| Linux, the BSDs, macOS | `$XDG_STATE_HOME/eggdb/history`, else `~/.local/state/eggdb/history` |
| Windows | `%LOCALAPPDATA%\EggDB\history` |

`EGGDB_HISTORY=<path>` moves it; `EGGDB_HISTORY=` (set and empty) turns it off for the session,
and so does a file that cannot be created (one dim notice at start: `history off: cannot create
/home/ada/.local/state/eggdb/history: permission denied`). `↑` recalls the newest entry; a
multi-line entry is recalled as one line with its line breaks turned into spaces (the one place
the shell alters text, stated here because a string literal spanning lines changes by it).
`Ctrl-R` opens `(reverse-i-search)'': ` in the prompt's place; each typed character narrows to
the newest entry containing the text, `Ctrl-R` again goes to the next older match, `Enter`
sends the match, `→`/`←`/`End`/`Home` keep it on the line for editing, `Esc` or `Ctrl-C`
restores the line as it was. A failing search shows `(failed reverse-i-search)'xyz': `.

### Transactions
`BEGIN` at the prompt opens the file's one write transaction, held across lines and prompts
until `COMMIT` or `ROLLBACK`, exactly as a `psql` session on `eggdb serve`; the prompt shows
`=*>`. A failed statement inside the block makes it `=!>` and every statement answers `25P02`
until `ROLLBACK`. Key-value commands inside a block run inside it (so a `put` is committed by
the `COMMIT`, or rolled back), which is the one place the two worlds meet. `.quit`, `Ctrl-D`
and the end of a batch roll an open block back with the `WARNING:` above.

### Exit codes
Interactive: 0, or 130 from a `Ctrl-C` during a statement. Batch, from section 7's table: 0 when
every statement and command succeeded; 1 when the first failure was a `get`/`del` of a missing
key; 2 usage (an unknown `.command`, a bad key-value line, SQL that is not UTF-8); 3 a SQL
error (any SQLSTATE but the two below) or a database error; 4 damaged (`XX001`, `Corrupt`); 5
busy (`55P03`, `Locked`). Writing to a closed pipe exits 0 quietly.

### Examples
```
$ eggdb shell app.db
EggDB 0.2.0: app.db, open read-write (2 tables, 1,204 keys)
Type .help for help. SQL ends with ; and .quit or Ctrl-D quits.
app=> .tables
items
sales
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
app=*> put user:21 Linus
app=*> insert into items values (3, 'milk', 1.2);
INSERT 0 1
app=*> insert into items values (3, 'milk', 1.2);
ERROR:  duplicate key value violates unique constraint "items_pkey"
DETAIL:  Key (id)=(3) already exists.
SQLSTATE:  23505
app=!> commit;
ROLLBACK
app=> get user:21
eggdb: key not found: user:21
app=> scan --prefix user:2 -n 2
user:2    Grace
user:20   Mira
2 entries
app=> .timer on
timer: on
app=> select count(*) from items;
 count
-------
     2
(1 row)

Time: 0.412 ms
app=> .quit
$ eggdb shell app.db -c "scan -k -n 1" -c "select 1 as one;"
user:1
 one
-----
   1
(1 row)

$ echo $?
0
```

### Decided by this design
1. The prompt is psql's, on the file's stem, so `serve`'s database name and the shell's prompt
   agree, and `=*>`/`=!>` need no explaining to a psql user.
2. The six key-value words are recognised in lower case only and `delete` is always SQL: no
   prefix character, and no SQL statement can be mistaken for a command.
3. Raw mode only while a line is read; `Ctrl-C` during a statement keeps section 7's meaning.
4. A wide result goes `expanded` for that result (psql's `\x auto`) rather than cut: nothing
   printed is ever less than the data.
5. History is one entry per line, 0600, under XDG state (or `%LOCALAPPDATA%`), moved or turned
   off by `EGGDB_HISTORY`; a multi-line entry is recalled on one line with spaces.
6. `BEGIN` holds the write transaction across prompts, and key-value commands join it.
7. Batch mode prints like `eggdb sql` and stops at the first failure unless `--keep-going`.

### As built in v0.2.0 (D105)
1. **Key-value writes inside a block** fail with the busy error (exit 5) instead of joining the
   block (decision 6): the block holds the database's one write transaction and the key-value
   commands open their own. Reads see the last commit.
2. **The banner** leaves out the key count, which only a full scan gives:
   `EggDB 0.2.0: app.db, open read-write (2 tables)`.
3. **Shell and key-value commands** are recognised only at the start of a statement; on a
   continuation line (`app->`) every line is SQL.
4. **The expanded header** follows psql's narrow form when the label is wider than the names
   and values (`-[ RECORD 1 ]-`).

## 16. `eggdb restore`: restoring a pg_dump (item 35)

`eggdb restore` keeps the grammar of section 1 (command, file, options) and reuses `--create`,
`--keep-going`, `--format`, `--cache` and `--dialect` as `import`, `shell` and `sql` define them.
Designed by Claude Fable (D15). It reads a PostgreSQL dump in plain SQL form (`pg_dump`, or
`pg_restore -f dump.sql <archive>`) into one EggDB file, statement by statement, each in its own
transaction (a table's `COPY` data in one), and ends with a report.

```
eggdb restore <file> [<dump.sql> | -] [--create] [--keep-going] [--report <path>] [--dialect postgresql]
```
`-` or no second argument reads stdin; when stdin is a terminal that is a usage error. The file
must exist unless `--create`, which builds it as `<file>.restoring` and renames it into place
only when the whole restore succeeded, so a failed restore leaves no file (import's rule, D93).

### Help: `eggdb help restore`
```
Usage: eggdb restore <file> [<dump.sql> | -] [--create] [--keep-going] [--report <path>] [--dialect postgresql]

Restores a PostgreSQL dump in plain SQL form (pg_dump, or pg_restore -f dump.sql <archive>)
into the file, statement by statement, each in its own transaction (a table's COPY data in one).
Objects EggDB does not have yet (functions, triggers, types, policies, partitioned tables,
extensions other than plpgsql, ...) are skipped and listed in the report with their line in the
dump; GRANT, REVOKE and OWNER TO are ignored (no roles yet); a statement that fails only because
it uses a skipped object is a consequence, not an error. An error in a supported statement (a
COPY row breaking a constraint, bad syntax, a failed INSERT) stops the restore at that line, and
the statements before it stay committed. Custom, directory and tar archives are not read:
convert them first with pg_restore -f dump.sql <archive>. The report counts what was created,
lists everything skipped, ignored and failed, and is one JSON object in a pipe.

      --create            create the file if it is missing (built as <file>.restoring, renamed
                          when the restore succeeded, so a failed restore leaves no file)
      --keep-going        report an error and go on; exit 3 at the end if there was one
      --report <path>     write the report to this file instead of stdout
      --dialect <name>    the dump's dialect: postgresql (sqlserver and oracle come later)

Examples:
  eggdb restore --create app.db app.sql
  pg_dump app | eggdb restore --create app.db
  pg_restore -f dump.sql app.dump && eggdb restore --create app.db dump.sql
  eggdb restore app.db dump.sql --keep-going --report restore.json --format json
```

### The text report
Section 6's shape: a fixed first line, labelled facts (the same labels as the JSON fields),
lists one item per line, and a verdict line. Every list item carries its dump line, so `sed -n
'23p' dump.sql` shows the statement. The report prints in every outcome, failure included,
because it says what the file holds and what to fix in the dump.
```
restored app.sql into app.db (1,204 statements, 2.3 s)
  schemas         2
  tables          14, 1,204,311 rows
  views           3
  sequences       9
  indexes         21
  constraints     17
  comments        4
  skipped         7 (functions 3, triggers 2, types 1, extensions 1)
  consequences    2
  ignored         61 (OWNER TO 38, GRANT 20, SET 3)
rows per table
  public.orders          1,180,002
  public.users              12,480
  public.items               1,329
skipped (not in EggDB yet; docs/roadmap.md)
  line 23     function public.set_updated_at()            plpgsql functions come later
  line 310    trigger users_updated on public.users       triggers come later
  line 2,114  table public.events (partitioned), 40,120 rows   partitioned tables come later
consequences (fail only because of a skipped object)
  line 412    ALTER TABLE public.orders ADD CONSTRAINT …  uses function public.set_updated_at()
ignored
  OWNER TO    38 statements (EggDB has no roles yet)
  GRANT       20 statements
  SET         3 statements: default_table_access_method, default_toast_compression, xmloption
  \connect    line 5 (one database per file)
  \!          line 9, never run
ok: every table and row of the dump is in app.db; 7 objects skipped and 2 consequences, listed above.
```
Rules: `rows per table` is sorted by rows, largest first, at most 20 lines, then `  … and 34
more tables (12,002 rows); --format json lists every table`. `skipped`, `consequences` and
`errors` are never cut: they are the work left to do. `ignored` is counts, one line per kind
(`OWNER TO`, `GRANT`, `REVOKE`, `SET` with its parameter names, then each meta-command with its
lines), never one line per statement; `\restrict`/`\unrestrict`, `\.` and `\connect` to the
file's own database are not listed at all. A statement in a list is cut after 48 characters
with `…`; the line number is padded to the widest. A section with nothing is left out, and its
summary line reads `skipped         0`. The verdict (green `ok:`, red `failed:` or `errors
ignored:`) is one of:
- `ok: every table and row of the dump is in app.db.`
- `ok: every table and row of the dump is in app.db; {n} objects skipped and {m} consequences, listed above.`
- `ok: {t} of {T} tables and {r} of {R} rows are in app.db; {n} objects skipped and {m} consequences, listed above.` (a partitioned or inherited table's rows are not restored)
- `failed: line 412: duplicate key value violates unique constraint "orders_pkey". The 1,203 statements before it are in app.db; nothing after it ran.` (without `--create`)
- `failed: line 412: {error}. Nothing was created; app.db does not exist.` (with `--create`)
- `errors ignored: 3 statements failed and were skipped (--keep-going), listed above; app.db holds everything else.`

With `--keep-going` the list `errors (ignored with --keep-going)` comes before `ignored`, one
line each: `  line 512    INSERT INTO public.audit VALUES (…)   duplicate key value violates
unique constraint "audit_pkey"`. Without it the failing statement appears the same way under
`error`, one line. Counts of one read `1 table, 1 row`; `(1 statement, 0.4 s)`.

With `--report <path>` stdout gets only the verdict line and the report goes to the file, in
`--format`'s format, which defaults to `json` for a path ending in `.json` and `text` otherwise.

### The JSON report
One object, stable fields, every number a plain integer (section 4), the same names as the
text labels. `ok` is true exactly when the exit code is 0.
```
{"ok":true,"file":"app.db","dump":"app.sql","created":true,"dialect":"postgresql",
 "statements":1204,"elapsed_ms":2301,
 "schemas":2,"tables":14,"rows":1204311,"views":3,"sequences":9,"indexes":21,"constraints":17,
 "comments":4,"tables_skipped":1,"rows_skipped":40120,
 "rows_per_table":[{"table":"public.orders","rows":1180002},{"table":"public.users","rows":12480}],
 "skipped":[{"line":23,"kind":"function","name":"public.set_updated_at()","reason":"plpgsql functions come later"},
            {"line":2114,"kind":"table","name":"public.events","detail":"partitioned","rows":40120,"reason":"partitioned tables come later"}],
 "consequences":[{"line":412,"statement":"ALTER TABLE public.orders ADD CONSTRAINT …","uses":"function public.set_updated_at()","error":"function public.set_updated_at() does not exist","sqlstate":"42883"}],
 "ignored":{"owner_to":38,"grant":20,"revoke":0,"set":["default_table_access_method","default_toast_compression","xmloption"],
            "meta_commands":[{"line":5,"command":"\\connect","reason":"one database per file"},{"line":9,"command":"\\!","reason":"never run"}]},
 "errors":[],"error":null,"verdict":"every table and row of the dump is in app.db; 7 objects skipped and 2 consequences, listed above."}
```
`dump` is `"-"` for stdin. `rows_per_table` holds every table, in the text order. `kind` is
pg_dump's lower-case object kind (`function`, `procedure`, `aggregate`, `trigger`, `rule`,
`policy`, `type`, `domain`, `materialized view`, `table`, `publication`, `subscription`,
`extension`, `event trigger`, `statement` for DDL refused as not supported yet); `detail` is
present for `partitioned`, `inherited` and `not supported yet`. A stop puts the failing statement
in `error` as `{"line":412,"statement":"…","error":"…","sqlstate":"23505","committed_statements":1203}`
with `ok:false`; `--keep-going` fills `errors` with such objects (no `committed_statements`)
and sets `ok:false`. `statement` and `uses` are cut like the text. `verdict` is the verdict line
without its label.

### Messages (stderr, section 7's shape)
| When | Message |
|------|---------|
| custom archive | `cannot restore app.dump: it is a pg_dump custom-format archive (it starts with PGDMP), which eggdb restore does not read. Convert it to SQL first (docs/migrate.md).` + `  try: pg_restore -f dump.sql app.dump` |
| directory archive | `cannot restore backup/: it is a pg_dump directory-format archive (it holds toc.dat), which eggdb restore does not read. Convert it to SQL first (docs/migrate.md).` + `  try: pg_restore -f dump.sql backup/` |
| tar archive | `cannot restore app.tar: it is a pg_dump tar-format archive (its first entry is toc.dat), which eggdb restore does not read. Convert it to SQL first (docs/migrate.md).` + `  try: pg_restore -f dump.sql app.tar` |
| archive on stdin | the same with `stdin` as the name and `  try: pg_restore -f dump.sql <archive>` |
| stdin is a terminal | `restore needs a dump: give the file as the second argument, or pipe it to stdin` |
| leftover staging file | `cannot create app.db: app.db.restoring exists, left by a restore that did not finish or one still running. Remove it and restore again.` |
| file missing, no `--create` | `cannot open app.db: no such file. Pass --create to restore into a new file.` + `  try: eggdb restore --create app.db dump.sql` |
| genuine error, no `--create` | `cannot restore dump.sql into app.db: line 412: duplicate key value violates unique constraint "orders_pkey". The 1,203 statements before it are in app.db; nothing after it ran. Restore a fixed dump into a new file with --create, or add --keep-going to run on and list every error.` |
| genuine error, `--create` | `cannot restore dump.sql into app.db: line 412: {error}. Nothing was created; app.db does not exist. Fix the dump, or add --keep-going to run on and list every error.` |
| `--keep-going` end | `restored dump.sql into app.db with 3 errors ignored (--keep-going); the report lists them.` |
| `\restrict` key mismatch | `cannot restore dump.sql: line 1,340: \unrestrict key does not match the \restrict key of line 2. The dump was altered; take it again.` (a genuine error) |
| `--dialect sqlserver` / `oracle` | `--dialect sqlserver comes in a later release (docs/roadmap.md, item 40); eggdb restore reads PostgreSQL dumps now.` (`oracle`: item 43) |
| other `--dialect` | `--dialect must be postgresql (sqlserver and oracle come in a later release), not 'mysql'` |
| `--report` unwritable | `cannot write the report to restore.json: permission denied. The restore itself finished; see the exit code.` |

Archives, the terminal-stdin case and the dialect messages exit 2 before the database is
opened or created; the restore errors exit 3; the others follow section 7. Each error line
of a report cites PostgreSQL's own message, as `eggdb sql` prints it, without `ERROR:`.

### Progress (stderr, terminal only)
As `import`: one line redrawn once a second and erased when the command ends, never in a pipe:
`restoring dump.sql: line 41,220, 14 tables, 1,180,002 rows…` (`restoring stdin: …` for `-`).
During a long `COPY` the row count moves and the line number is the `COPY` statement's.

### Decided by this design
1. A report in every outcome, with the dump line on every item: a failed restore still tells
   what got in and what to fix.
2. Three outcomes, three verdict labels: `ok:` (skips and consequences do not change it),
   `failed:` (stopped, exit 3), `errors ignored:` (`--keep-going`, exit 3).
3. The `ignored` section is counts per kind, never a line per `OWNER TO`; `skipped`,
   `consequences` and `errors` are never cut.
4. Archives are refused with the exact `pg_restore -f` command before the file is touched.
5. `--report` keeps the verdict on stdout, so a script's log still shows the outcome.

## 17. `eggdb mcp` and `eggdb audit`: AI agents over MCP (item 49a)

`eggdb mcp <file>` keeps the grammar of section 1 and serves one file to an AI agent over the
Model Context Protocol on stdin and stdout; `eggdb audit <file>` shows what the agents did.
Designed by Claude Fable (D15). The agent's client starts `eggdb mcp` itself (`claude mcp add
mydb -- eggdb mcp app.db`), so a person rarely types it; `eggdb audit` is the command people
type. Both are read-only in this release.

```
eggdb mcp <file> [--time-limit <seconds>]
eggdb audit <file> [-n <count>] [--format text|json]
```

### Help
Two lines in the command list of section 9, after `restore`:
```
  mcp    <file>                 serve the file read-only to an AI agent over MCP on stdin/stdout
  audit  <file>                 list the calls AI agents made through eggdb mcp
```
`eggdb help mcp` says what the server is for, that there is nothing to type, what the agent
gets, that the release is read-only, how calls share the file with other programs, where the
audit file is and that the server does not start without it, the time-limit exit, and that
stdout carries the protocol only; then `--time-limit <seconds>` (1 to 300, default 5) and three
examples. `eggdb help audit` says what is listed, in which order, that the database is never
opened, and what `--format json` adds; then `-n, --limit <count>` and three examples. The exact
text is in `crates/eggdb/src/cli/help.rs`.

### `eggdb mcp` on stderr
**stderr is a terminal** (someone typed it by hand):
```
eggdb mcp app.db: a read-only MCP server on stdin and stdout, for an AI agent's client to start; nothing to type here.
  audit        app.db.mcp-audit.jsonl (eggdb audit app.db shows it)
  time limit   5 s per call
  add it       claude mcp add mydb -- eggdb mcp app.db
  stop         Ctrl-C, or close stdin
```
**stderr is not a terminal** (the client's log), one line: `eggdb 0.2.0 mcp: serving app.db
read-only; audit app.db.mcp-audit.jsonl; time limit 5 s`. A file open for writing elsewhere at
start is not an error: one more line says calls will report it until it is closed. EOF on stdin
prints nothing and exits 0.

Startup errors, section 7's shape, before anything is read from stdin:
| When | Exit | Message |
|------|------|---------|
| missing file | 3 | `cannot open app.db: no such file. eggdb mcp never creates a database: check the path, or create it with` `` `eggdb create app.db` `` + `  try: eggdb create app.db` |
| not a database, damaged, newer | 3, 4, 3 | section 10's read-only open messages |
| audit unwritable | 3 | `cannot write app.db.mcp-audit.jsonl: permission denied. Every MCP call is recorded there, so eggdb mcp does not start without it. Make the database's directory writable, or copy the database somewhere that is.` |
| bad `--time-limit` | 2 | `--time-limit must be a whole number of seconds from 1 to 300, not '0'` + the usage and `try:` lines |
| time limit exceeded | 3 | the process exits after appending a `time_limit_exceeded` record |

### `eggdb audit`: the text view
Like `scan`: one line per record, no header row, columns aligned, the summary on stderr. Oldest
first, as the file is; `-n 20` is the file's last 20 lines. Columns: time (UTC, milliseconds;
the JSON keeps microseconds), duration right-aligned in ms, tool (bold), outcome (`ok` or the
error code in red, `sql_error` with its SQLSTATE), rows (`-` for an error, `200+` when the result
was cut), arguments: the SQL text for `run_sql` and `explain_query`, the URI for
`resources/read`, compact JSON otherwise. Newline, tab and carriage return become one space;
any other control character, U+2028, U+2029 and bidirectional controls are shown as `\u001b`,
so logged SQL can never drive the terminal. `server_started` and `time_limit_exceeded` are dim
sentences in the same time column.
```
$ eggdb audit app.db
2026-10-28 09:14:02.004  server started: eggdb 0.2.0, pid 48213, read-only, time limit 5 s
2026-10-28 09:14:07.123      12 ms  list_tables     ok                   14  {}
2026-10-28 09:14:09.880      3 ms  describe_table  ok                    -  {"table":"users"}
2026-10-28 09:14:15.402    418 ms  run_sql         ok                 200+  SELECT id, name, email FROM users WHERE created_at > '2026-10-01' ORDER BY cr...
2026-10-28 09:14:21.007      1 ms  run_sql         read_only             -  UPDATE users SET name = 'Ada L.' WHERE id = 7
2026-10-28 09:14:24.310      2 ms  run_sql         sql_error 42P01       -  SELECT * FROM user
7 calls, 3 errors, by claude-code 2.1.0 (the name the client gave)
```
The summary: `{n} calls, {e} errors, by {clients}`; `no errors`; `1 call`; with `-n` that cut,
`last {k} of {n} calls, …`; `{u} unreadable lines` only when there are any; the client names in
order of first appearance, followed by `(the names the clients gave)` since they are unverified.
An unreadable line is shown in place as `line 412   unreadable: not a JSON object; skipped`; in
`--format json` it is left out of stdout and reported on stderr as `eggdb:
app.db.mcp-audit.jsonl line 412 is not a record; skipped`, exit still 0.

**Empty and missing:** no audit file while the database exists: stdout empty, stderr `no MCP
calls yet (app.db.mcp-audit.jsonl does not exist)`, exit 0; an empty file prints `no MCP calls
yet`; neither file: exit 3, `cannot open app.db: no such file. Check the path.` `--format hex` or
`raw` follows section 13.4.

### The busy message, one more line
The write busy line of section 10 and `ui`'s own busy line get a second line, indented like
`try:`; the read-command busy line is unchanged:
```
eggdb: cannot open app.db: another program has it open. Close it before writing, or read with `eggdb get`, `scan`, `check` or `stats`, which work alongside readers.
  it may be an AI agent's call through eggdb mcp, which holds the file for up to its time limit; run the command again, or see `eggdb audit app.db`.
```

### Decided by this design
1. Oldest first, `-n` is the newest: the text view is the file in its own order, like `scan`
   and `tail`.
2. SQL in the text view is shown as the agent sent it, never masked: the audit exists to show
   exactly what an agent did, the JSON holds it anyway, and the file is readable by its owner
   only. The docs say to protect the audit file like the database.
3. On a terminal `eggdb mcp` prints a report whose first line says nothing is to be typed; in a
   pipe one line, so the client's log shows the server, file and version.
4. `audit` never opens the database, so it works while `mcp` or `serve` holds it; a missing
   audit file is "no calls yet" (exit 0) when the database exists, a path error (exit 3) when it
   does not.
5. A file busy at startup is not a startup error for `mcp`; the first call reports it.
6. The busy note goes only on the write-side message, since MCP calls hold a shared lock.
7. A time-limit exit is exit 3, after the audit record is written.
8. No header row and no client column: the client's name goes in the summary; the protocol
   version, SQLSTATE message and result bytes stay in `--format json`.
9. Unreadable audit lines are reported in place and counted, never stop the listing, and keep
   exit 0.

### As built in v0.2.0 (D119, D120)
- Lines are cut at 120 columns with `...` whether or not stdout is a terminal: reading the
  terminal's width needs the raw-terminal layer, which `audit` does not otherwise use.
- The time-limit exit writes its audit record but no stderr line: the main thread holds stderr
  for the protocol's lifetime, and the record says why the server stopped.
- `--format json` prints each record re-encoded as ASCII JSON, not the stored bytes, so raw
  control or bidirectional characters written into the file are escaped; the text view escapes
  every control, bidirectional and invisible formatting character (U+200B to U+200F, U+2060 to
  U+2069, U+FEFF, the tag characters) in every field.
- The busy note says the holder *may* be an `eggdb mcp` call, which holds the file for up to its
  time limit (`--time-limit` goes to 300 s).
