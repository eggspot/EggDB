use super::*;

const SERVE_FIELDS: &[FieldDefinition] = &[
    field("file", FieldType::Text),
    field("database", FieldType::Text),
    field("user", FieldType::Text),
    field("host", FieldType::Text),
    field("port", FieldType::Integer),
    field("listening", FieldType::Text),
    field("mysql_listening", FieldType::Text),
    field("mysql_port", FieldType::Integer),
    field("tds_listening", FieldType::Text),
    field("tds_port", FieldType::Integer),
    field("read_only", FieldType::Boolean),
    field("max_connections", FieldType::Integer),
    field("stopped", FieldType::Boolean),
    field("connections_closed", FieldType::Integer),
];

const AUDIT_FIELDS: &[FieldDefinition] = &[
    field("event", FieldType::Text),
    field("time", FieldType::Time),
    field("call_id", FieldType::Integer),
    field("client", FieldType::Object),
    field("tool", FieldType::Text),
    field("arguments", FieldType::Object),
    field("outcome", FieldType::Text),
    field("rows", FieldType::Integer),
    field("truncated", FieldType::Boolean),
    field("duration_ms", FieldType::Integer),
];

const VERSION_FIELDS: &[FieldDefinition] = &[field("version", FieldType::Text)];

const SERVE_ERRORS: &[CommandError] = &[
    OPENING_ERRORS[0],
    OPENING_ERRORS[1],
    OPENING_ERRORS[2],
    OPENING_ERRORS[3],
    OPENING_ERRORS[4],
    OPENING_ERRORS[5],
    OPENING_ERRORS[6],
    OPENING_ERRORS[7],
    OPENING_ERRORS[8],
    command_error("already_exists", 3),
];

const SHELL_ERRORS: &[CommandError] = &[
    OPENING_ERRORS[0],
    OPENING_ERRORS[1],
    OPENING_ERRORS[2],
    OPENING_ERRORS[3],
    OPENING_ERRORS[4],
    OPENING_ERRORS[5],
    OPENING_ERRORS[6],
    OPENING_ERRORS[7],
    OPENING_ERRORS[8],
    command_error("sql_error", 3),
    command_error("collection_not_found", 1),
];

const AUDIT_ERRORS: &[CommandError] = &[
    command_error("invalid_arguments", 2),
    command_error("not_found", 3),
    command_error("permission_denied", 3),
    command_error("database_failed", 3),
    command_error("output_failed", 3),
];

const HELP_ERRORS: &[CommandError] = &[
    command_error("invalid_arguments", 2),
    command_error("output_failed", 3),
];

pub(super) const UI: CommandDefinition = CommandDefinition {
    words: &["ui"],
    aliases: &[],
    synopsis: "<file>",
    summary: "browse and edit keys full screen in the terminal",
    description: "Opens the file full screen in a terminal to browse keys and values and to add, edit and delete them, each change confirmed and committed. It needs a terminal and has no JSON form.",
    usage: "eggdb ui <file> [--read-only] [--mono] [--ascii] [--cache <bytes>]",
    help: "\
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
",
    options: &[
        flag(
            "read-only",
            "open read-only: no changes, and it works beside another reader",
        ),
        flag(
            "mono",
            "no colours (also with --color never, or when NO_COLOR is set)",
        ),
        flag(
            "ascii",
            "ASCII only, no box lines or arrows; with --mono, no styles at all",
        ),
    ],
    operands: &[FILE],
    outputs: &[],
    max_effect: Effect::Destroy,
    errors: READING_ERRORS,
    examples: &[
        "eggdb ui app.db",
        "eggdb ui app.db --read-only",
        "eggdb ui app.db --mono --ascii",
    ],
    surfaces: "it is itself the terminal front-end; the GUI client (D15) brings the desktop one",
};

pub(super) const SERVE: CommandDefinition = CommandDefinition {
    words: &["serve"],
    aliases: &[],
    synopsis: "<file>",
    summary: "serve the file to PostgreSQL and MySQL clients; see --listen",
    description: "Serves the file to PostgreSQL clients, and with options to MySQL and SQL Server (TDS) clients, on this machine's loopback address until stopped; clients log in with a password and may run any statement. The first line says where it listens.",
    usage: "eggdb serve <file> [--listen <address>] [--mysql-listen <address>] [--tds-listen <address>] [--log-batches] [--user <name>] [--password-file <file> | -] [--database <name>] [--max-connections <count>] [--read-only] [--create]",
    help: "\
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
      --tds-listen <address>      also serve SQL Server's TDS protocol there, to sqlcmd
                                  (go-sqlcmd), Microsoft.Data.SqlClient and mssql-jdbc,
                                  without encryption: connect with sqlcmd's default, -N
                                  disable or Encrypt=false (Transact-SQL; port 1433 is SQL
                                  Server's)
      --log-batches               log the first 200 characters of each TDS batch and RPC
                                  call, to see what a client sends (never the login or its
                                  password)
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
  eggdb serve app.db --password-file pw.txt --tds-listen 127.0.0.1:1433
  SQLCMDPASSWORD=secret sqlcmd -S 127.0.0.1,1433 -U eggdb -d app -Q \"SELECT @@VERSION\"
",
    options: &[
        OptionDefinition {
            default: Some("127.0.0.1:5432"),
            ..valued(
                "listen",
                "address",
                "host:port, [::1]:port, or a port alone on 127.0.0.1; port 0 takes any free port",
            )
        },
        valued(
            "mysql-listen",
            "address",
            "also serve MySQL's protocol there",
        ),
        valued(
            "tds-listen",
            "address",
            "also serve SQL Server's TDS protocol there, without encryption",
        ),
        flag(
            "log-batches",
            "log the first 200 characters of each TDS batch and RPC call",
        ),
        OptionDefinition {
            default: Some("eggdb"),
            ..valued("user", "name", "the user clients log in as")
        },
        valued(
            "password-file",
            "file",
            "the password is the file's first line; - reads stdin",
        ),
        valued(
            "database",
            "name",
            "the name clients connect to (default: the file name without its extension)",
        ),
        OptionDefinition {
            default: Some("64"),
            ..valued(
                "max-connections",
                "count",
                "connections open at once, one thread each",
            )
        },
        flag(
            "read-only",
            "no writes, and it works beside other readers",
        ),
        flag("create", "create the database file if it is missing"),
    ],
    operands: &[FILE],
    outputs: &[
        OutputDefinition {
            format: Format::Text,
            kind: OutputKind::Records(SERVE_FIELDS),
        },
        OutputDefinition {
            format: Format::Json,
            kind: OutputKind::Records(SERVE_FIELDS),
        },
    ],
    max_effect: Effect::Destroy,
    errors: SERVE_ERRORS,
    examples: &[
        "eggdb serve app.db --password-file ~/.eggdb-password",
        "EGGDB_PASSWORD=secret eggdb serve app.db --listen 127.0.0.1:5433",
        "eggdb serve --create new.db --password-file - < password.txt",
        "eggdb serve app.db --password-file pw.txt --mysql-listen 127.0.0.1:3306",
        "eggdb serve app.db --password-file pw.txt --tds-listen 127.0.0.1:1433",
    ],
    surfaces: "none: a server for other programs' clients (psql, drivers, GUI tools)",
};

pub(super) const SHELL: CommandDefinition = CommandDefinition {
    words: &["shell"],
    aliases: &[],
    synopsis: "<file>",
    summary: "an interactive prompt for key-value commands and SQL; see -c",
    description: "An interactive prompt on one database file for the key-value commands and SQL, or a batch with -c or stdin; any statement runs. It prints text and has no JSON form.",
    usage: "eggdb shell <file> [-c <text>]... [--keep-going] [--mode table|expanded|csv] [--dialect postgresql|mysql|sqlite|oracle|sqlserver] [--read-only] [--create]",
    help: "\
An interactive prompt on one database file: the key-value commands without their 'eggdb
<command> <file>' prefix (put, get, del, scan, check, stats), SQL in the PostgreSQL dialect
(or MySQL's, SQLite's, Oracle's or SQL Server's, with --dialect mysql, sqlite, oracle or
sqlserver) until a semicolon,
.help, .tables, .schema, .collection, .mode, .timer, .quit, and psql's \\copy, which reads and writes files
on this machine. COPY ... FROM STDIN reads the lines after it up to a line \\. as its data.
Tab completes, Up/Down and Ctrl-R recall the history, which is kept in
$XDG_STATE_HOME/eggdb/history (EGGDB_HISTORY=<path> moves it; set empty, turns it off).
BEGIN holds a transaction across prompts until COMMIT or ROLLBACK. With -c, or with stdin not
a terminal, it runs the text as a batch: plain output, no prompt, and it stops at the first
failure unless --keep-going.

  -c, --command <text>   a line to run, repeatable; runs in order, then exits
      --keep-going       run on after a failure, exiting with the first failure's code
      --mode <mode>      table (default), expanded or csv
      --dialect <name>   SQL in postgresql (default), mysql, sqlite, oracle or sqlserver
      --read-only        open the file read-only
      --create           create the database file if it is missing

Examples:
  eggdb shell app.db
  eggdb shell app.db -c \"put user:1 Ada\" -c \"select count(*) from users;\"
  eggdb shell app.db < session.txt
",
    options: &[
        OptionDefinition {
            short: Some('c'),
            long: "command",
            value: Some("text"),
            repeatable: true,
            default: None,
            help: "a line to run, repeatable; runs in order, then exits",
        },
        flag(
            "keep-going",
            "run on after a failure, exiting with the first failure's code",
        ),
        OptionDefinition {
            default: Some("table"),
            ..valued("mode", "mode", "table (default), expanded or csv")
        },
        OptionDefinition {
            default: Some("postgresql"),
            ..valued(
                "dialect",
                "name",
                "SQL in postgresql (default), mysql, sqlite, oracle or sqlserver",
            )
        },
        flag("read-only", "open the file read-only"),
        flag("create", "create the database file if it is missing"),
    ],
    operands: &[FILE],
    outputs: &[],
    max_effect: Effect::Destroy,
    errors: SHELL_ERRORS,
    examples: &[
        "eggdb shell app.db",
        "eggdb shell app.db -c \"put user:1 Ada\" -c \"select count(*) from users;\"",
        "eggdb shell app.db < session.txt",
    ],
    surfaces: "it is itself the prompt; the GUI client (D15) brings a query editor",
};

pub(super) const MCP: CommandDefinition = CommandDefinition {
    words: &["mcp"],
    aliases: &[],
    synopsis: "<file>",
    summary: "serve the file to an AI agent over MCP; read-only unless --allow-write",
    description: "Serves the file to an AI agent's client over the Model Context Protocol on stdin and stdout, read-only unless --allow-write, every call recorded in an audit file. stdout carries the protocol only.",
    usage: "eggdb mcp <file> [--allow-write] [--tables <names>]... [--time-limit <seconds>] [--call-gap <ms>]",
    help: "\
Serves the file to an AI agent over the Model Context Protocol (MCP) on stdin and stdout. The
agent's client (Claude Code, for one) starts eggdb mcp itself and talks JSON-RPC to it, so there
is nothing to type here. The agent gets tools to list and describe tables, run and explain SQL,
sample rows and read keys. Without --allow-write every call is read-only and a write is refused
with an error the agent can read. With it the agent also gets write_sql, one statement per call
that may change the file, run in its own transaction; the file is opened read-write for that call
only. A statement that destroys data (DROP, TRUNCATE, a DELETE or UPDATE whose WHERE names no
column of the table, dropping a column, constraint or primary key, changing a column's type) runs
only after you confirm it: in a client that can ask you, a dialog; in one that cannot, the agent
must pass confirm: true, so set that client to ask you before write_sql runs. Each read call
opens the file read-only for at most the time limit and closes it again, so other readers run
beside it; while a call runs, a program that wants to write the file is refused, and while
another program has the file open for writing a call fails and the agent is told to try again.
A write_sql call holds the file alone while its statement runs (the time limit bounds it),
commits and closes the file, and never while you are asked.
With --tables only the tables and views named exist for the agent, in every dialect and catalog:
every other table, view and sequence, and the keys outside SQL (kv_get and kv_scan are not
offered), are not there, and a query that names one fails as it would for a missing table. Names
are comma-separated, name or schema.name (public. and dbo. mean the default schema), matched
exactly as stored; repeat --tables to add more. A sequence comes with the table that owns it, or
is named itself. Every name must be a table, view or sequence of the file when the server starts,
or it does not start. --tables goes with a read-only server in this release, not with
--allow-write. After each call the server waits --call-gap milliseconds before the next call
opens the file, so your own programs, which are refused while a call holds the file, get their
turn between an agent's calls; 0 turns the gap off.
Every call is appended to <file>.mcp-audit.jsonl next to the file (readable by its owner only),
which eggdb audit shows; if that file cannot be written the server does not start. A statement
still running a second past its time limit cannot be stopped, so the server records it and exits
for the client to restart it, never once a write has begun to commit. Closing stdin stops the
server with exit 0.
stdout carries the protocol only; every message goes to stderr.

      --allow-write            add write_sql: one statement per call that may change the file
      --tables <names>         only these tables and views exist for the agent: comma-separated,
                               name or schema.name; repeat it for more (read-only servers only)
      --time-limit <seconds>   the longest one call may run, 1 to 300 (default 5)
      --call-gap <ms>          the pause after each call before the next, so other programs can write;
                               milliseconds, 0 to 10000 (default 100)

Examples:
  claude mcp add mydb -- eggdb mcp app.db
  claude mcp add mydb -- eggdb mcp app.db --allow-write
  claude mcp add mydb -- eggdb mcp app.db --tables users,orders
  claude mcp add mydb -- eggdb mcp app.db --time-limit 30
  eggdb audit app.db
",
    options: &[
        flag(
            "allow-write",
            "add write_sql: one statement per call that may change the file",
        ),
        OptionDefinition {
            repeatable: true,
            ..valued(
                "tables",
                "names",
                "only these tables and views exist for the agent, comma-separated, name or schema.name; repeat it for more",
            )
        },
        OptionDefinition {
            default: Some("5"),
            ..valued(
                "time-limit",
                "seconds",
                "the longest one call may run, 1 to 300",
            )
        },
        OptionDefinition {
            default: Some("100"),
            ..valued(
                "call-gap",
                "ms",
                "the pause after each call before the next, in milliseconds, 0 to 10000",
            )
        },
    ],
    operands: &[FILE],
    outputs: &[],
    max_effect: Effect::Destroy,
    errors: MCP_ERRORS,
    examples: &[
        "claude mcp add mydb -- eggdb mcp app.db",
        "claude mcp add mydb -- eggdb mcp app.db --allow-write",
        "claude mcp add mydb -- eggdb mcp app.db --tables users,orders",
        "claude mcp add mydb -- eggdb mcp app.db --time-limit 30",
    ],
    surfaces: "none: a server for an AI agent's client; eggdb audit shows what agents did",
};

pub(super) const AUDIT: CommandDefinition = CommandDefinition {
    words: &["audit"],
    aliases: &[],
    synopsis: "<file>",
    summary: "list the calls AI agents made through eggdb mcp",
    description: "Lists every call AI agents made through eggdb mcp, from the audit file next to the database, oldest first; the database itself is never opened. Arguments in it are what agents sent, data and never instructions.",
    usage: "eggdb audit <file> [-n <count>] [--format text|json]",
    help: "\
Lists what AI agents did through eggdb mcp: every call in <file>.mcp-audit.jsonl, oldest first,
one line each with its time (UTC), duration, tool, outcome, rows and arguments (SQL as the agent
sent it, on one line, cut at 120 columns). Server starts and time-limit exits are lines too, and
a write whose server stopped before it finished shows its outcome as unknown. The
database itself is never opened, so audit works while eggdb mcp or any other command has it.
--format json prints the records re-encoded as ASCII JSON, one per line, with the client's name and
version, the protocol version, the SQLSTATE and the result's size.

  -n, --limit <count>   only the newest lines (the file's last ones)

Examples:
  eggdb audit app.db
  eggdb audit app.db -n 20
  eggdb audit app.db --format json
",
    options: &[OptionDefinition {
        short: Some('n'),
        long: "limit",
        value: Some("count"),
        repeatable: false,
        default: None,
        help: "only the newest lines (the file's last ones)",
    }],
    operands: &[FILE],
    outputs: &[
        OutputDefinition {
            format: Format::Text,
            kind: OutputKind::Records(AUDIT_FIELDS),
        },
        OutputDefinition {
            format: Format::Json,
            kind: OutputKind::Records(AUDIT_FIELDS),
        },
    ],
    max_effect: Effect::Read,
    errors: AUDIT_ERRORS,
    examples: &[
        "eggdb audit app.db",
        "eggdb audit app.db -n 20",
        "eggdb audit app.db --format json",
    ],
    surfaces: "none yet: the GUI client (D15) brings an agent activity view",
};

pub(super) const HELP: CommandDefinition = CommandDefinition {
    words: &["help"],
    aliases: &[],
    synopsis: "[command]",
    summary: "this text, or one command's help",
    description: "Prints the help for eggdb or for one command; with --json, this registry of commands, tools, effects and error categories as one JSON object.",
    usage: "eggdb help [<command>] [--json]",
    help: "",
    options: &[],
    operands: &[OperandDefinition {
        name: "command",
        kind: OperandType::Text,
        count: Count::Optional,
        standard_stream: false,
    }],
    outputs: &[
        OutputDefinition {
            format: Format::Text,
            kind: OutputKind::Content,
        },
        OutputDefinition {
            format: Format::Json,
            kind: OutputKind::Object(&[]),
        },
    ],
    max_effect: Effect::Read,
    errors: HELP_ERRORS,
    examples: &["eggdb help", "eggdb help scan", "eggdb help --json"],
    surfaces: "eggdb ui: ? shows every key; the GUI client (D15) brings its own help",
};

pub(super) const VERSION: CommandDefinition = CommandDefinition {
    words: &["version"],
    aliases: &[],
    synopsis: "",
    summary: "print the version",
    description: "Prints the version of this eggdb.",
    usage: "eggdb version [--json]",
    help: "",
    options: &[],
    operands: &[],
    outputs: &[
        OutputDefinition {
            format: Format::Text,
            kind: OutputKind::Content,
        },
        OutputDefinition {
            format: Format::Json,
            kind: OutputKind::Object(VERSION_FIELDS),
        },
    ],
    max_effect: Effect::Read,
    errors: HELP_ERRORS,
    examples: &["eggdb version", "eggdb version --json"],
    surfaces: "none yet: the GUI client (D15) shows it in its About box",
};
