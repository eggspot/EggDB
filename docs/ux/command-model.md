# The Egg command model: EggDB's tables

EggDB and EggOS share one way to work with them, for people and for AI agents: the same verbs,
the same answers (text for people, JSON for scripts), the same errors (a code, a shared category,
a message and the command that fixes it) and the same safety fields on every MCP tool. The owner
chose this shape on 2026-10-28: EggDB keeps SQL and its key-value commands, and the model is the
layer above them, not a new language. The decision is [D143](../decisions/D143.md).

**The shared part has one canonical copy:** EggOS's
[`docs/ux/command-model.md`](https://github.com/eggspot/EggOS/blob/main/docs/ux/command-model.md).
It is not on EggOS's `main` yet, so until it lands the approved design the two products were
planned from (2026-10-28, two plan reviews and the owner's review) is the shared part, and its
rules are summed up below. A change EggDB needs in the shared part goes to EggOS as an issue
labelled `command-model`; until it lands, it is listed here under "Pending differences". This
file holds only EggDB's own tables, and the registry test checks them against the code
(`crates/eggdb-commands`).

## The shared rules, in short

- **Verbs.** `list`, `show`, `get`, `sample`, `check`, `explain`, `export` read; `create`, `add`,
  `set`, `import`, `restore` (bring back), `move`, `copy`, `open`, `close`, `install` change;
  `delete` and `uninstall` destroy; `run` runs anything. New commands are `<noun> <verb>` with a
  singular noun and a core verb; a product's top-level commands act on its implied noun, which
  for EggDB is the database file. Kept names keep their name and exact behavior.
- **Answers.** Text stays as it is. JSON for scripts: records are JSON lines, an object is one
  JSON value; new fields are `snake_case`, sizes in bytes (`size_bytes`), times RFC 3339 in UTC,
  durations in milliseconds. Shipped fields keep their names.
- **Errors.** Every code has a shared category: `usage`, `not_found`, `exists`, `busy`,
  `read_only`, `needs_grant`, `needs_confirmation`, `declined`, `refused`, `too_large`,
  `timeout`, `damaged`, `failed`. JSON and MCP: `{"error":{"code","category","message","fix",
  "fix_command"}}`, with fields a product adds. Exit codes are per command.
- **Safety.** A tool's `max_effect` (`read`, `change`, `destroy`, `control`), `grant`,
  `idempotent`, `open_world` and per-call `confirm` check give its MCP annotations: `read` is
  `readOnlyHint`, `destroy` and `control` are `destructiveHint`. Grants and flags are for agents
  only; the enforcement stays where it is.
- **Self-description.** `help`, `help --json` and the MCP `tools/list` come from the same
  definitions.

## Kept names and noun verbs

| Name | Kind | How it fits |
|------|------|-------------|
| `create` | kept | shipped; also the core verb on the implied noun, the file |
| `put` | kept | shipped key-value word (D5) |
| `get` | kept | shipped; also the core verb |
| `del` | kept | shipped, with the alias `delete`, the core verb; `del` is its Redis spelling |
| `scan` | kept | shipped key-value word |
| `ttl` | kept | Redis's own word (D5); acts on a key of the implied noun |
| `expire` | kept | Redis's own word (D5) |
| `persist` | kept | Redis's own word (D5) |
| `incr` | kept | Redis's own word (D5) |
| `decr` | kept | Redis's own word (D5) |
| `cas` | kept | the usual name for compare-and-set (D5, memcached) |
| `check` | kept | shipped; also the core verb on the implied noun |
| `stats` | kept | shipped |
| `backup` | kept | the implied noun's own verb: a consistent copy of the file (D141) |
| `compact` | kept | the implied noun's own verb: the file rewritten in place (D141) |
| `ui` | kept | shipped |
| `export` | kept | shipped; also the core verb on the implied noun |
| `import` | kept | shipped; also the core verb on the implied noun |
| `sql` | kept | shipped |
| `serve` | kept | shipped |
| `shell` | kept | shipped |
| `restore` | kept | shipped; the core verb's "bring back" meaning, from a dump |
| `mcp` | kept | shipped |
| `audit` | kept | shipped |
| `help` | kept | shipped |
| `version` | kept | shipped |
| `collection list`, `collection add`, `collection delete` | `<noun> <verb>` | the noun `collection` with the core verbs `list`, `add` and `delete` |

`restore` keeps one meaning, "bring back": EggDB's `restore` brings a database back from a dump,
as EggOS's `wm restore` brings a window back to its size and its Trash brings a file back.

## The commands in the registry

Every command of `eggdb` is in the registry (slice 2, D144), and the registry test fails on a
command of the parser that is not. `eggdb help --json` prints these definitions; the
`default_format` column is what a terminal and a pipe get without `--format` (`ui`, `shell` and
`mcp` take no `--format`, so theirs is `null`).

| Command | Max effect | Default format (terminal, pipe) | JSON form | Twin of |
|---------|------------|---------------------------------|-----------|---------|
| `create` | change | text, json | one object, `file` and `page_size` | |
| `put` | change | text, json | one object, `key`, `stored`, and `ttl_ms`/`expires_at` with `--ttl` | |
| `get` | read | text, raw | one object, `key`/`key_hex` and `value`/`value_hex` | `kv_get` |
| `del` | destroy | text, json | one object, `key` and `deleted` | |
| `scan` | read | text, json | records, `key`/`key_hex` and `value`/`value_hex` | `kv_scan` |
| `ttl` | read | text, json | one object, `key`, `ttl_ms`, `expires_at` | |
| `expire` | change | text, json | one object, `key`, `expires`, `ttl_ms`, `expires_at` | |
| `persist` | change | text, json | one object, `key` and `persisted` | |
| `incr` | change | text, json | one object, `key` and `value` | |
| `decr` | change | text, json | one object, `key` and `value` | |
| `cas` | destroy | text, json | one object, `key`, `swapped`, and `value` when it did not swap | |
| `check` | read | text, json | one object, the check report | |
| `stats` | read | text, json | one object, the stats report | |
| `backup` | change | text, json | one object, the backup report | |
| `compact` | change | text, json | one object, the compaction report | |
| `ui` | destroy | none | none: it draws a screen | |
| `export` | read | text, json | one object about the export file; without a file, the export itself | |
| `import` | change | text, json | one object, the import report | |
| `sql` | destroy | text, text | one line per statement: `tag`, and `columns`, `rows`, `row_count`, `notices` when it has them | `run_sql` (with `--read-only`), `write_sql` |
| `explain` | read | text, json | one `sql --json` line, `"tag":"EXPLAIN"` | `explain_query` |
| `table list` | read | text, json | records, `schema`, `name`, `kind`, `columns`, `comment` | `list_tables` |
| `table show` | read | text, json | one object, the tool's description without `notice` | `describe_table` |
| `table sample` | read | text, json | one `sql --json` line plus `truncated` | `sample_rows` |
| `serve` | destroy | text, json | records: where it listens, then that it stopped | |
| `shell` | destroy | none | none: its own `--mode table\|expanded\|csv` | |
| `restore` | change | text, json | one object, the restore report | |
| `collection list` | read | text, json | one object, `file` and `collections` | |
| `collection add` | change | text, json | one object, `file`, `collection`, `added` | |
| `collection delete` | destroy | text, json | one object, `file`, `collection`, `deleted` | |
| `mcp` | destroy | none | none: stdout carries the protocol | |
| `audit` | read | text, json | records, the audit file's lines | |
| `help` | read | text, text | `--json`: the registry, one object; for a noun, one object per verb | |
| `version` | read | text, text | `--json`: `{"version"}` | |

## The tools

| Tool | CLI twin | Max effect | Grant | Idempotent | Confirm | Envelope (what the twin does not print) |
|------|----------|------------|-------|------------|---------|------------------------------------------|
| `list_tables` | `table list --json` | read | | yes | | `notice`; `schemas` (the records' `schema` values); `tables` and `views` become records with `kind` |
| `describe_table` | `table show --json` | read | | yes | | `notice` |
| `run_sql` | `sql --read-only --json` | read | | yes | | `notice`, `dialect`, `returned_rows`, `truncated`, `truncated_by` |
| `write_sql` | `sql --json` | destroy | `--allow-write` | no | destructive statements (D121) | the same, and `rows_changed`, `committed`, `warning` |
| `explain_query` | `explain --json` | read | | yes | | `notice`, `dialect`, `returned_rows`, `truncated`, `truncated_by` |
| `sample_rows` | `table sample --json` | read | | yes | | `notice`, `dialect`, `returned_rows`, `truncated_by`; `tag` and `row_count` count the rows shown, not the `LIMIT n+1` read |
| `kv_get` | `get --json` | read | | yes | | `notice`, `found`; in `entry`, `key_hex` when `key` is there, and `value_bytes` |
| `kv_scan` | `scan --json` | read | | yes | | `notice`, `returned_entries`, `truncated`, `next_start_hex`; in each entry, as `kv_get` |

No tool reaches outside the database (`openWorldHint` is false for all). Their names are the
shipped ones (D119, D121); a new tool's name joins its command's words with `_`. Arguments map
to the twin: `sql` to `-c`, `dialect` to `--dialect`, `key` and `key_hex` to the key operand
(hex as `0x…`), `prefix`, `start` and `end` (and their `_hex` forms) to `--prefix`, `--from` and
`--to`, `max_entries` to `--limit`, `keys_only` to `--keys-only`, `schema` to `--schema`, `table`
to the table operand, `sample_rows`'s `max_rows` to `--limit`; `run_sql`'s and `write_sql`'s
`max_rows` is a limit of the call, and `confirm` the user's confirmation, which the CLI never
asks for. A missing key is `found: false` from `kv_get` and exit 1 with nothing printed from
`get`, a result in both. A missing table is `not_found` from the tools and `table_not_found`
(exit 1) from `table show` and `table sample`: the same category, `not_found` (D144).

## Error codes

Every code EggDB gives, from the CLI (with `--json`, one line on stderr) and from the MCP server
(in the tool result, with `isError: true`). The exit is the CLI's, the same for every command
that declares the code; `-` marks a code only MCP gives.

| Code | Category | Exit | When |
|------|----------|------|------|
| `invalid_arguments` | `usage` | 2 | CLI: an unknown command or option, a missing or extra argument, a bad value. MCP: an argument missing, unknown or of the wrong type |
| `one_statement_per_call` | `usage` | 2 | the SQL text holds a second statement; CLI: `eggdb explain` |
| `unknown_tool` | `usage` | - | the call named no tool of this server (audit only) |
| `invalid_request` | `usage` | - | the request was not a well-formed call (audit only) |
| `not_initialized` | `usage` | - | a call came before `initialize` (audit only) |
| `not_found` | `not_found` | 3 | the database file does not exist; MCP also: no such table or view |
| `table_not_found` | `not_found` | 1 | CLI: `table show` or `table sample` named no table or view; `fix_command` is `eggdb table list <file>` |
| `collection_not_found` | `not_found` | 1 | `-C <name>` names no collection; `fix_command` is `eggdb collection add <file> <name>` |
| `already_exists` | `exists` | 3 | `export`, `create` or `backup` to a file that is there, or a staging file left by `backup` or `compact` |
| `database_busy` | `busy` | 5 | another program holds the file |
| `read_only` | `read_only` | 3 | MCP: a write without `--allow-write`, or sent to `run_sql`. CLI: a statement refused with SQLSTATE 25006, as `sql --read-only` refuses writes |
| `needs_confirmation` | `needs_confirmation` | - | a destructive `write_sql` without `confirm: true` on a client that cannot ask |
| `declined_by_user` | `declined` | - | the user said no |
| `cancelled` | `declined` | - | the client cancelled the call while the user was asked |
| `not_a_database` | `refused` | 3 | the file is not an EggDB file |
| `incompatible` | `refused` | 3 | the file was written by a newer EggDB |
| `permission_denied` | `refused` | 3 | the file or its directory cannot be opened by this user |
| `not_writable` | `refused` | - | the file or its directory cannot be opened for writing |
| `transaction_control_refused` | `refused` | - | `BEGIN`, `COMMIT`, `ROLLBACK` |
| `copy_not_available` | `refused` | - | `COPY` |
| `not_supported` | `refused` | 3 | a dialect's form EggDB does not have yet, such as Oracle's `EXPLAIN PLAN` |
| `too_large` | `too_large` | 3 | a key or value over the file's limit |
| `write_too_large` | `too_large` | - | one call changed more than 256 MiB |
| `time_limit` | `timeout` | - | the call ran past `--time-limit` |
| `damaged` | `damaged` | 4 | the file is damaged; MCP adds `fix_command: eggdb check <file>` |
| `sql_error` | `failed` | 3 | a statement failed as its dialect reports it; with `sqlstate`, `detail`, `hint` and `position` |
| `output_failed` | `failed` | 3 | stdout or the output file could not be written |
| `database_failed` | `failed` | 3 | any other engine or I/O failure, as the message says |
| `database_unavailable` | `failed` | - | the file cannot be opened for another reason |
| `confirmation_failed` | `failed` | - | the client answered the confirmation request with an error |
| `audit_unavailable` | `failed` | - | the call could not be recorded, so its result is withheld |

A SQL error whose SQLSTATE means corruption or a lock is `damaged` or `database_busy` in
`eggdb sql --json`, and one refused because the session is read-only (25006) is `read_only`, so
its code and its exit agree.

## Pending differences

None yet: EggDB's slice 1 follows the approved design as it stands. When EggOS's canonical copy
lands, this file links to it alone for the shared part.
