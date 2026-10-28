use super::args::CommandName;

pub(crate) const MAIN: &str = "\
eggdb: one database for every kind of data. This release: an embedded key-value store in one file,
with a first part of SQL, served over the PostgreSQL protocol.

Usage: eggdb <command> <file> [arguments] [options]

Commands:
  create <file>                 make an empty database (fails if the file exists)
  put    <file> <key> [value]   store a value; the value may come from stdin
  get    <file> <key>           print a value (exit 1 if the key is not there)
  del    <file> <key>           remove a key (exit 1 if it was not there)
  scan   <file>                 list entries in key order; see --prefix, --from, --to
  check  <file>                 read every page and report the file's health
  stats  <file>                 size, pages, last commit and log, without reading pages
  ui     <file>                 browse and edit keys full screen in the terminal
  export <file> [out]           write every entry to a JSON-lines file, or to stdout
  import <file> [in]            load an export, or a CSV file into a table with --csv
  sql    <file> [script]        run SQL (PostgreSQL or MySQL) from -c, a script or stdin
  serve  <file>                 serve the file to PostgreSQL and MySQL clients; see --listen
  shell  <file>                 an interactive prompt for key-value commands and SQL; see -c
  restore <file> [dump]         restore a PostgreSQL dump (pg_dump's plain SQL) into the file
  mcp    <file>                 serve the file read-only to an AI agent over MCP on stdin/stdout
  audit  <file>                 list the calls AI agents made through eggdb mcp
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
";

pub(crate) fn usage_line(command: CommandName) -> &'static str {
    match command {
        CommandName::Create => "eggdb create <file> [--page-size <bytes>]",
        CommandName::Put => "eggdb put <file> <key> [<value>] [--create]",
        CommandName::Get => "eggdb get <file> <key>",
        CommandName::Del => "eggdb del <file> <key>",
        CommandName::Scan => {
            "eggdb scan <file> [--prefix <bytes> | --from <bytes> [--to <bytes>]] [-r] [-n <count>] [-k]"
        }
        CommandName::Check => "eggdb check <file>",
        CommandName::Stats => "eggdb stats <file>",
        CommandName::Ui => "eggdb ui <file> [--read-only] [--mono] [--ascii] [--cache <bytes>]",
        CommandName::Export => "eggdb export <file> [<out> | -]",
        CommandName::Import => {
            "eggdb import <file> [<in> | -] [--create [--page-size <bytes>]] [--on-conflict error|overwrite|skip] [--dry-run] | eggdb import <file> --csv <file> --table <name> [--header] [--delimiter <c>] [--null <text>]"
        }
        CommandName::Sql => {
            "eggdb sql <file> [-c <sql> | <script> | -] [--dialect postgresql|mysql|sqlite|oracle] [--create]"
        }
        CommandName::Shell => {
            "eggdb shell <file> [-c <text>]... [--keep-going] [--mode table|expanded|csv] [--dialect postgresql|mysql|sqlite|oracle] [--read-only] [--create]"
        }
        CommandName::Serve => {
            "eggdb serve <file> [--listen <address>] [--mysql-listen <address>] [--user <name>] [--password-file <file> | -] [--database <name>] [--max-connections <count>] [--read-only] [--create]"
        }
        CommandName::Restore => {
            "eggdb restore <file> [<dump.sql> | -] [--create] [--keep-going] [--report <path>] [--dialect postgresql]"
        }
        CommandName::Mcp => "eggdb mcp <file> [--time-limit <seconds>]",
        CommandName::Audit => "eggdb audit <file> [-n <count>] [--format text|json]",
    }
}

pub(crate) fn command_help(command: CommandName) -> String {
    format!(
        "Usage: {}\n\n{}",
        usage_line(command),
        command_body(command)
    )
}

fn command_body(command: CommandName) -> &'static str {
    match command {
        CommandName::Create => CREATE,
        CommandName::Put => PUT,
        CommandName::Get => GET,
        CommandName::Del => DEL,
        CommandName::Scan => SCAN,
        CommandName::Check => CHECK,
        CommandName::Stats => STATS,
        CommandName::Ui => UI,
        CommandName::Export => EXPORT,
        CommandName::Import => IMPORT,
        CommandName::Sql => SQL,
        CommandName::Serve => SERVE,
        CommandName::Shell => SHELL,
        CommandName::Restore => RESTORE,
        CommandName::Mcp => MCP,
        CommandName::Audit => AUDIT,
    }
}

const CREATE: &str = "\
Makes an empty database file. It fails if anything is already at that path, so it never
replaces a file. The other commands never create a file, except put with --create.

      --page-size <bytes>   the page size: a power of two from 4k to 64k, 4k by default.
                            Larger pages allow longer keys. It is fixed for the file's life.

Examples:
  eggdb create app.db
  eggdb create big.db --page-size 16k
";

const PUT: &str = "\
Stores the value under the key and commits, replacing any earlier value. Without a value
argument, the value is what stdin holds, when stdin is not a terminal. Bytes are stored
exactly: `echo hi |` stores a trailing newline and `printf hi |` does not. Values may be up
to 1 GiB; the page cache grows to fit the value.

      --create              create the database file if it is missing

Keys and values: text as typed, 0x.. for hex, @file for a file's bytes, @- for stdin,
=text for text that starts with 0x, @ or =.

Examples:
  eggdb put app.db user:1 Ada
  cat photo.jpg | eggdb put app.db photo
  eggdb put app.db 0x00ff @blob.bin
  eggdb put --create new.db greeting hello
";

const GET: &str = "\
Prints the value stored under the key, or exits 1 when the key is not there. On a terminal the
value is shown as text, or as 0x hex when it is not text; in a pipe or a file the bytes are
written exactly, without a newline, so `eggdb get app.db photo > photo.jpg` round-trips.

Examples:
  eggdb get app.db user:1
  eggdb get app.db photo > photo.jpg
  eggdb get app.db 0x00ff --format hex
";

const DEL: &str = "\
Removes the key and commits, or exits 1 when it was not there. `eggdb delete` is the same
command.

Examples:
  eggdb del app.db user:4
";

const SCAN: &str = "\
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
";

const CHECK: &str = "\
Reads every page of the file as of its last commit: every checksum, the order of every key,
every large value, and what each page of the file is used for. Exits 0 when nothing is
damaged and 4 when something is, with what to do next.

Examples:
  eggdb check app.db
  eggdb check app.db --format json
";

const STATS: &str = "\
Prints the page size, the pages and free pages, the file's size, the last commit and the log
written since the last checkpoint. It reads no page, so it is instant on a file of any size.

Examples:
  eggdb stats app.db
  eggdb stats app.db --format json
";

const UI: &str = "\
Opens the file full screen: the keys in order on the left, the selected value on the right as
text, JSON or hex. Up/Down or j/k move, PgUp/PgDn page, Home/End jump, / filters by prefix,
v changes the view, Tab switches pane. e edits a value, a adds a key, d deletes one; each change
asks for a confirmation and is one commit, on disk before the screen redraws. c checks every
page, m shows the last message in full, ? shows every key, q quits. Needs a terminal on both
stdin and stdout; in a script use eggdb get, put, del and scan.

Options:
  --read-only   open read-only: no changes, and it works beside another reader
  --mono        no colours (also with --color never, or when NO_COLOR is set)
  --ascii       ASCII only, no box lines or arrows; with --mono, no styles at all
  --cache       page cache size; one edit's pages must fit in it (default 16m)

Examples:
  eggdb ui app.db
  eggdb ui app.db --read-only
  eggdb ui app.db --mono --ascii
";

const EXPORT: &str = "\
Writes every entry of the file, from one snapshot, to <out> as JSON lines (eggdb-jsonl): a
header line, one line per entry in key order, and a trailer line with the entry count, so a cut
export is refused on import. Keys and values are text when they are UTF-8 without control
characters other than tab and line breaks, and hex otherwise (\"key_hex\", \"value_hex\"), so
any bytes round-trip. Without <out>, or with -, the export goes to stdout. It never replaces a
file; a failed export removes what it wrote.

Examples:
  eggdb export app.db backup.jsonl
  eggdb export app.db | gzip > backup.jsonl.gz
";

const IMPORT: &str = "\
Loads an export made by eggdb export into the file, from <in> or, without it or with -, from
stdin. Every line is checked: the header, the trailer's count, the escapes, keys in ascending
order and their sizes. From a regular file, the whole export is checked before anything is
written, so a bad line changes nothing. The entries are then committed in batches; from stdin or
a pipe, a bad line stops the import after the batches before it. With --create, a missing file is created with the
export's page size, built under <file>.importing and renamed into place only when the import
succeeded.

      --create                   create the file if it is missing
      --page-size <bytes>        the new file's page size, instead of the export's
      --on-conflict <policy>     a key already there, or twice in the export: error (the
                                 default) stops the import, overwrite keeps the export's value,
                                 skip keeps the value already there
      --dry-run                  check the export and report, without writing anything

With --csv, it loads a CSV file (or stdin, with -) into an existing SQL table instead, as
COPY <table> FROM STDIN (FORMAT csv) does: every row or none, in one transaction, through
the same path as INSERT (constraints, defaults, serial columns and indexes apply). The page
cache grows with the file, since the whole load must fit in it; --cache sets it.

      --csv <file>               the CSV file to load, or - for stdin
      --table <name>             the table, as SQL writes it (users, app.\"Users\", t (a, b))
      --header                   the first line names the columns and is skipped
      --delimiter <c>            the field separator, a comma by default
      --null <text>              the text that stands for NULL, unquoted; empty by default

Examples:
  eggdb import --create copy.db backup.jsonl
  gunzip -c backup.jsonl.gz | eggdb import --create copy.db
  eggdb import app.db fixes.jsonl --on-conflict overwrite
  eggdb import app.db backup.jsonl --dry-run
  eggdb import app.db --csv people.csv --table people --header
";

const SQL: &str = "\
Runs SQL statements, separated by semicolons, from -c, from a script file, or from stdin (a
script of - or none at all), in order. This release runs the first parts of the PostgreSQL
dialect: CREATE TABLE with a PRIMARY KEY, DROP TABLE, INSERT, SELECT with WHERE, inner, left
and cross joins, GROUP BY with count, sum, avg, min and max, HAVING, DISTINCT, ORDER BY,
LIMIT and OFFSET, UPDATE, DELETE, BEGIN, COMMIT and ROLLBACK, with the integer,
floating-point, exact numeric, boolean, text, char(n), bytea, uuid, date, time, timetz,
timestamp, timestamptz, interval, json and jsonb types and serial and identity columns; with --dialect mysql, MySQL's SQL on the same tables, with MySQL's
errors, and with --dialect sqlite, SQLite's SQL (type affinity, INTEGER PRIMARY KEY row ids,
PRAGMAs, sqlite_schema), with SQLite's errors and result codes, and with --dialect oracle,
Oracle's SQL (DUAL, ROWNUM, FETCH FIRST, upper-case names, '' as NULL), with ORA- errors.
Results print as aligned tables, like psql. Each statement outside BEGIN ... COMMIT is its own
transaction; in Oracle's dialect the first change starts a transaction that COMMIT, ROLLBACK
or DDL ends, and the run commits it at the end, as SQL*Plus does. The first failing statement
stops the run with exit 3 and the dialect's error on stderr; statements before it stay
committed, and a block left open is rolled back. CREATE INDEX and UNIQUE speed up WHERE and
ORDER BY in every dialect. Views (CREATE VIEW) work in every dialect, schemas with
search_path in PostgreSQL's and COMMENT ON in PostgreSQL's and Oracle's. COPY ... FROM STDIN and
COPY ... TO STDOUT (text and CSV) load and unload tables in PostgreSQL's dialect: in a script the
data follows the COPY line up to a line \\., as pg_dump writes it; with -c it comes from stdin;
COPY ... TO STDOUT prints the rows. More types and the rest come next (docs/sql.md).

  -c, --command <sql>   the statements to run
      --dialect <name>  postgresql (default), mysql, sqlite or oracle
      --create          create the database file if it is missing

Examples:
  eggdb sql --create app.db -c \"CREATE TABLE users (id integer PRIMARY KEY, name text)\"
  eggdb sql app.db -c \"INSERT INTO users VALUES (1, 'Ada'), (2, 'Grace')\"
  eggdb sql app.db -c \"SELECT name FROM users WHERE id = 2\"
  eggdb sql app.db schema.sql
  cat queries.sql | eggdb sql app.db
  eggdb sql app.db -c \"COPY users FROM STDIN (FORMAT csv)\" < users.csv
";

const SHELL: &str = "\
An interactive prompt on one database file: the key-value commands without their 'eggdb
<command> <file>' prefix (put, get, del, scan, check, stats), SQL in the PostgreSQL dialect
(or MySQL's, SQLite's or Oracle's, with --dialect mysql, sqlite or oracle) until a semicolon,
.help, .tables, .schema, .mode, .timer, .quit, and psql's \\copy, which reads and writes files
on this machine. COPY ... FROM STDIN reads the lines after it up to a line \\. as its data.
Tab completes, Up/Down and Ctrl-R recall the history, which is kept in
$XDG_STATE_HOME/eggdb/history (EGGDB_HISTORY=<path> moves it; set empty, turns it off).
BEGIN holds a transaction across prompts until COMMIT or ROLLBACK. With -c, or with stdin not
a terminal, it runs the text as a batch: plain output, no prompt, and it stops at the first
failure unless --keep-going.

  -c, --command <text>   a line to run, repeatable; runs in order, then exits
      --keep-going       run on after a failure, exiting with the first failure's code
      --mode <mode>      table (default), expanded or csv
      --dialect <name>   SQL in postgresql (default), mysql, sqlite or oracle
      --read-only        open the file read-only
      --create           create the database file if it is missing

Examples:
  eggdb shell app.db
  eggdb shell app.db -c \"put user:1 Ada\" -c \"select count(*) from users;\"
  eggdb shell app.db < session.txt
";

const SERVE: &str = "\
Serves the file to PostgreSQL clients: psql and PostgreSQL drivers connect to it unchanged and
run the SQL of eggdb sql. Clients log in as one user with a password (SCRAM-SHA-256), which
comes from EGGDB_PASSWORD or --password-file, never from the command line. Until EggDB speaks
TLS the server listens only on this machine, on a loopback address. The first line printed says
where it listens; in a pipe it is one JSON line, so a script can read the port. While it runs
the file is open for writing, so other eggdb commands on it exit 5. Ctrl-C or SIGTERM finishes
the running statements, rolls back open transactions and stops with exit 0.

      --listen <address>          host:port, [::1]:port, or a port alone on 127.0.0.1; port 0
                                  takes any free port (default 127.0.0.1:5432)
      --mysql-listen <address>    also serve MySQL's protocol there, to the mysql client and
                                  MySQL drivers (MySQL's SQL; port 3306 is MySQL's)
      --user <name>               the user clients log in as (default eggdb)
      --password-file <file>      the password is the file's first line; - reads stdin
      --database <name>           the name clients connect to (default: the file name
                                  without its extension, so app.db is app)
      --max-connections <count>   connections open at once, one thread each (default 64)
      --read-only                 no writes, and it works beside other readers
      --create                    create the database file if it is missing

Examples:
  eggdb serve app.db --password-file ~/.eggdb-password
  EGGDB_PASSWORD=secret eggdb serve app.db --listen 127.0.0.1:5433
  eggdb serve --create new.db --password-file - < password.txt
  psql postgresql://eggdb@127.0.0.1:5432/app
  eggdb serve app.db --password-file pw.txt --mysql-listen 127.0.0.1:3306
";

const RESTORE: &str = "\
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
";

const MCP: &str = "\
Serves the file to an AI agent over the Model Context Protocol (MCP) on stdin and stdout. The
agent's client (Claude Code, for one) starts eggdb mcp itself and talks JSON-RPC to it, so there
is nothing to type here. The agent gets tools to list and describe tables, run and explain SQL,
sample rows and read keys. This release is read-only: a write is refused with an error the agent
can read. Each call opens the file read-only for at most the time limit and closes it again, so
other readers run beside it; while a call runs, a program that wants to write the file is
refused, and while another program has the file open for writing a call fails and the agent is
told to try again. Every call is appended to <file>.mcp-audit.jsonl next to the file (readable by
its owner only), which eggdb audit shows; if that file cannot be written the server does not
start. A statement still running a second past its time limit cannot be stopped, so the server
records it and exits for the client to restart it. Closing stdin stops the server with exit 0.
stdout carries the protocol only; every message goes to stderr.

      --time-limit <seconds>   the longest one call may run, 1 to 300 (default 5)

Examples:
  claude mcp add mydb -- eggdb mcp app.db
  claude mcp add mydb -- eggdb mcp app.db --time-limit 30
  eggdb audit app.db
";

const AUDIT: &str = "\
Lists what AI agents did through eggdb mcp: every call in <file>.mcp-audit.jsonl, oldest first,
one line each with its time (UTC), duration, tool, outcome, rows and arguments (SQL as the agent
sent it, on one line, cut at 120 columns). Server starts and time-limit exits are lines too. The
database itself is never opened, so audit works while eggdb mcp or any other command has it.
--format json prints the records re-encoded as ASCII JSON, one per line, with the client's name and
version, the protocol version, the SQLSTATE and the result's size.

  -n, --limit <count>   only the newest lines (the file's last ones)

Examples:
  eggdb audit app.db
  eggdb audit app.db -n 20
  eggdb audit app.db --format json
";
