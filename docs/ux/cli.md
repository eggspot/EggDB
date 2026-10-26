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
| `help [<command>]`, `version` | none | |

`delete` is accepted as an alias of `del`; `del` is the documented name because it matches
`put` and `get` in length and is what Redis users type.

**Reserved names**, so nobody builds a script around a name that later changes meaning:
`export`, `import`, `shell`, `backup`, `compact`, `ui`. In v0.2.0, before each ships, typing one
prints `eggdb <name> is not in this release yet; see docs/roadmap.md` and exits 2. How they fit
the grammar when they come: `export <file> [<out>|-]` and `import <file> [<in>|-]` (JSON lines,
same byte rules as section 4); `shell <file>` runs the commands of this document without the
`eggdb <command> <file>` prefix, one per line; `backup <file> <copy>` copies a consistent
snapshot while others write; `compact <file>` rewrites the file without free or leaked pages;
`ui <file>` opens the terminal UI (its own design document).

### Creating a file: explicit, never by accident
`put` and `del` **do not create** a database. A typo in the path (`app.bd`) must not leave a
stray empty file that a later command then happily reads as "empty", so a missing file is an
error (exit 3) with the fix in the message: `eggdb create app.bd` or `--create`.

- `eggdb create <file>` is the normal way; the quickstart shows it.
- `eggdb put --create <file> <key> <value>` creates on the way, for one-liners and scripts.
- `create` on an existing path fails (exit 3): `app.db already exists; eggdb create never
  replaces a file`. It never truncates anything.
- The library's `create_if_missing` is set `false` for every command except `create` and
  `put --create`.

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
checked app.db as of commit 2026-10-26 09:14:07.123456 UTC (stamp 1761470047123456.0003)
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
checked app.db as of commit 2026-10-26 09:14:07.123456 UTC (stamp 1761470047123456.0003)
damaged: the B+tree page 77 is damaged: a key is out of order
  what was checked  the header, the free list, and the tree down to page 77 passed
  what to do        EggDB does not repair files yet. Restore app.db from your latest backup.
                    Keep the damaged file unchanged: `eggdb scan` may still read part of the data,
                    and an issue at https://github.com/eggspot/EggDB/issues with this output helps us.
```
`check` JSON: `{"ok":true,"file":"app.db","commit_stamp":"1761470047123456.0003",
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
  last commit     2026-10-26 09:14:07.123456 UTC (stamp 1761470047123456.0003)
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

Interrupt handling: a Ctrl-C during a long `scan` or `check` stops the output cleanly (the
process exits 130 and leaves no partial line in `json` or `hex` modes: the current line is
finished or dropped whole). A `put` is atomic in the engine, so an interrupt either committed
it or did not; the CLI needs nothing special beyond exiting 130. Writing to a closed pipe
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

## 9. Help and version: exact text

`eggdb --version`, `-V`, `eggdb version`:
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
| unknown command | `unknown command '{name}'. Commands: create, put, get, del, scan, check, stats, help, version.` |
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
| Ctrl-C | nothing printed; exit 130 |

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
2. Files are created only by `eggdb create` or `put --create`; a wrong path is an error, not a
   new file.
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
11. Reserved: `export`, `import`, `shell`, `backup`, `compact`, `ui`; they answer with the
    roadmap until they ship.
