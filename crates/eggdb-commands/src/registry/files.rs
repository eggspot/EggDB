use super::*;

const BACKUP_FIELDS: &[FieldDefinition] = &[
    field("ok", FieldType::Boolean),
    field("file", FieldType::Text),
    field("copy", FieldType::Text),
    field("commit_stamp", FieldType::Text),
    field("commit_time", FieldType::Time),
    field("entries", FieldType::Integer),
    field("page_size", FieldType::Integer),
    field("copy_pages", FieldType::Integer),
    field("copy_bytes", FieldType::Integer),
    field("elapsed_ms", FieldType::Integer),
];

const COMPACT_FIELDS: &[FieldDefinition] = &[
    field("ok", FieldType::Boolean),
    field("file", FieldType::Text),
    field("commit_stamp", FieldType::Text),
    field("commit_time", FieldType::Time),
    field("entries", FieldType::Integer),
    field("page_size", FieldType::Integer),
    field("pages_before", FieldType::Integer),
    field("file_bytes_before", FieldType::Integer),
    field("pages_after", FieldType::Integer),
    field("file_bytes_after", FieldType::Integer),
    field("free_pages_before", FieldType::Integer),
    field("leaked_pages_before", FieldType::Integer),
    field("pages_reclaimed", FieldType::Integer),
    field("elapsed_ms", FieldType::Integer),
];

const IMPORT_FIELDS: &[FieldDefinition] = &[
    field("file", FieldType::Text),
    field("from", FieldType::Text),
    field("created", FieldType::Boolean),
    field("dry_run", FieldType::Boolean),
    field("entries", FieldType::Integer),
    field("imported", FieldType::Integer),
    field("skipped", FieldType::Integer),
    field("collections_added", FieldType::Integer),
    field("table", FieldType::Text),
    field("rows", FieldType::Integer),
];

const RESTORE_FIELDS: &[FieldDefinition] = &[
    field("ok", FieldType::Boolean),
    field("file", FieldType::Text),
    field("dump", FieldType::Text),
    field("dialect", FieldType::Text),
    field("created", FieldType::Boolean),
    field("statements", FieldType::Integer),
    field("schemas", FieldType::Integer),
    field("tables", FieldType::Integer),
    field("views", FieldType::Integer),
    field("indexes", FieldType::Integer),
    field("sequences", FieldType::Integer),
    field("constraints", FieldType::Integer),
    field("comments", FieldType::Integer),
    field("rows", FieldType::Integer),
    field("rows_per_table", FieldType::Array),
    field("skipped", FieldType::Array),
    field("ignored", FieldType::Object),
    field("differences", FieldType::Array),
    field("consequences", FieldType::Array),
    field("errors", FieldType::Array),
    field("verdict", FieldType::Text),
];

const BACKUP_ERRORS: &[CommandError] = &[
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

const COMPACT_ERRORS: &[CommandError] = BACKUP_ERRORS;

const IMPORT_ERRORS: &[CommandError] = &[
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
    command_error("too_large", 3),
    command_error("sql_error", 3),
];

const RESTORE_ERRORS: &[CommandError] = &[
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
    command_error("sql_error", 3),
];

pub(super) const BACKUP: CommandDefinition = CommandDefinition {
    words: &["backup"],
    aliases: &[],
    synopsis: "<file> <copy>",
    summary: "write a consistent copy of the file while readers keep working",
    description: "Writes a copy of the file's last commit to a new file while other readers keep working, renamed into place only once it is complete and checked. It never replaces a file and changes nothing in the database.",
    usage: "eggdb backup <file> <copy>",
    help: "\
Writes a copy of the file's last commit to <copy>, a new file, while other readers keep working.
The copy is rebuilt entry by entry, so it holds no free or leaked pages and may be smaller than
the file; every collection, SQL table and time to live is kept. It is built as <copy>.backing-up
and renamed to <copy> only once it is complete, synced and its entries counted again, so a crash
or a full disk never leaves a partial copy. <copy> must not exist: eggdb backup never replaces a
file. The command opens the file read-only, so a program that has it open for writing makes it
busy (exit 5); to copy a file while a program writes to it, use backup <copy> inside eggdb shell,
or Database::backup_to in the program. In a pipe the report is one JSON line.

Examples:
  eggdb backup app.db app.bak
  eggdb backup app.db \"backups/app-$(date +%F).db\"
  eggdb backup app.db app.bak --format json
",
    options: &[],
    operands: &[
        FILE,
        OperandDefinition {
            name: "copy",
            kind: OperandType::Path,
            count: Count::One,
            standard_stream: false,
        },
    ],
    outputs: &object_outputs(BACKUP_FIELDS),
    max_effect: Effect::Change,
    errors: BACKUP_ERRORS,
    examples: &[
        "eggdb backup app.db app.bak",
        "eggdb backup app.db \"backups/app-$(date +%F).db\"",
        "eggdb backup app.db app.bak --format json",
    ],
    surfaces: "eggdb shell: backup <copy>; the GUI client (D15) brings a backup dialog",
};

pub(super) const COMPACT: CommandDefinition = CommandDefinition {
    words: &["compact"],
    aliases: &[],
    synopsis: "<file>",
    summary: "rewrite the file without free or leaked pages, in place",
    description: "Rewrites the file without its free and leaked pages and renames the result over it in one step, so a crash leaves either the old file or the new one. The data stays the same.",
    usage: "eggdb compact <file>",
    help: "\
Rewrites the file without its free and leaked pages and swaps the result in. The new file is
built entry by entry as <file>.compacting, synced, its entries counted again, and then renamed
over the file in one step, so a crash at any moment leaves either the old file or the new one,
never a mix. Programs reading the file keep working throughout and see the new file when they
next open it; a program that has it open for writing makes the command busy (exit 5), and so does
eggdb shell, which holds the file: quit it first. A file with no free or leaked pages is rewritten
all the same, which packs its pages. eggdb check says how many pages a compact would reclaim. In a
pipe the report is one JSON line.

Examples:
  eggdb compact app.db
  eggdb check app.db && eggdb compact app.db --format json
",
    options: &[],
    operands: &[FILE],
    outputs: &object_outputs(COMPACT_FIELDS),
    max_effect: Effect::Change,
    errors: COMPACT_ERRORS,
    examples: &[
        "eggdb compact app.db",
        "eggdb check app.db && eggdb compact app.db --format json",
    ],
    surfaces: "none yet: the GUI client (D15) brings a file view",
};

pub(super) const IMPORT: CommandDefinition = CommandDefinition {
    words: &["import"],
    aliases: &[],
    synopsis: "<file> [in]",
    summary: "load an export, or a CSV file into a table with --csv",
    description: "Loads an eggdb export from a file or stdin, every line checked, or with --csv loads a CSV or bcp file into an existing SQL table, all rows or none. --dry-run checks an export and changes nothing.",
    usage: "eggdb import <file> [<in> | -] [--create [--page-size <bytes>]] [--on-conflict error|overwrite|skip] [--dry-run] [-C <name>] | eggdb import <file> --csv <file> --table <name> [--header] [--delimiter <c>] [--null <text>] [--dialect postgresql|oracle|sqlserver] [--row-terminator <text>] [--keep-nulls]",
    help: "\
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
  -C, --collection <name>        load everything into this collection, made if missing

Entries tagged with a collection go into it, and the import makes every collection it meets.

With --csv, it loads a CSV file (or stdin, with -) into an existing SQL table instead, as
COPY <table> FROM STDIN (FORMAT csv) does: every row or none, in one transaction, through
the same path as INSERT (constraints, defaults, serial columns and indexes apply). The page
cache grows with the file, since the whole load must fit in it; --cache sets it.

      --csv <file>               the CSV file to load, or - for stdin
      --table <name>             the table, as SQL writes it (users, app.\"Users\", t (a, b))
      --header                   the first line names the columns and is skipped
      --delimiter <c>            the field separator, a comma by default
      --null <text>              the text that stands for NULL, unquoted; empty by default
      --dialect <name>           postgresql (default) or oracle: the table name as Oracle
                                 writes it, Oracle's NLS date formats, and empty fields as NULL
                                 (SQLcl UNLOAD and SET MARKUP CSV ON output); or sqlserver:
                                 a bcp character-format file (bcp ... out -c) loaded as bcp in
                                 loads it, below

With --dialect sqlserver, the file is in bcp's character format: every field but the last
ends with the field terminator, the last with the row terminator, with no quoting. An empty
field is NULL (its column's default without --keep-nulls), a field of one NUL byte is an empty
string, values convert as SQL Server's INSERT converts text, and the text is UTF-8 (bcp -C
65001). Without a column list the file has a field for every column, identity values kept.

      --delimiter <text>         the field terminator, bcp's -t: a tab by default; \\t, \\n,
                                 \\r, \\0 and \\\\ are bcp's escapes
      --row-terminator <text>    the row terminator, bcp's -r: a newline (or \\r\\n) by default
      --header                   skip the first row, as bcp's -F 2
      --keep-nulls               keep empty fields NULL rather than the column's default, bcp's -k

Examples:
  eggdb import --create copy.db backup.jsonl
  gunzip -c backup.jsonl.gz | eggdb import --create copy.db
  eggdb import app.db fixes.jsonl --on-conflict overwrite
  eggdb import app.db backup.jsonl --dry-run
  eggdb import app.db --csv people.csv --table people --header
  eggdb import --dialect oracle hr.db --csv EMPLOYEES_DATA_TABLE.csv --table hr.employees --header
  eggdb import --dialect sqlserver shop.db --csv items.dat --table dbo.items
",
    options: &[
        flag("create", "create the file if it is missing"),
        valued("page-size", "bytes", "the new file's page size, instead of the export's"),
        OptionDefinition {
            default: Some("error"),
            ..valued(
                "on-conflict",
                "policy",
                "a key already there, or twice in the export: error, overwrite or skip",
            )
        },
        flag(
            "dry-run",
            "check the export and report, without writing anything",
        ),
        OptionDefinition {
            help: "load everything into this collection, made if missing",
            ..COLLECTION_OPTION
        },
        valued("csv", "file", "the CSV file to load, or - for stdin"),
        valued("table", "name", "the table, as SQL writes it"),
        flag(
            "header",
            "the first line names the columns and is skipped",
        ),
        valued(
            "delimiter",
            "c",
            "the field separator, a comma by default (bcp: a tab)",
        ),
        valued(
            "null",
            "text",
            "the text that stands for NULL, unquoted; empty by default",
        ),
        OptionDefinition {
            default: Some("postgresql"),
            ..valued(
                "dialect",
                "name",
                "postgresql (default), oracle, or sqlserver for a bcp character file",
            )
        },
        valued(
            "row-terminator",
            "text",
            "with --dialect sqlserver, the row terminator, bcp's -r",
        ),
        flag(
            "keep-nulls",
            "with --dialect sqlserver, keep empty fields NULL, bcp's -k",
        ),
    ],
    operands: &[
        FILE,
        OperandDefinition {
            name: "in",
            kind: OperandType::Path,
            count: Count::Optional,
            standard_stream: true,
        },
    ],
    outputs: &object_outputs(IMPORT_FIELDS),
    max_effect: Effect::Change,
    errors: IMPORT_ERRORS,
    examples: &[
        "eggdb import --create copy.db backup.jsonl",
        "gunzip -c backup.jsonl.gz | eggdb import --create copy.db",
        "eggdb import app.db fixes.jsonl --on-conflict overwrite",
        "eggdb import app.db backup.jsonl --dry-run",
        "eggdb import app.db --csv people.csv --table people --header",
        "eggdb import --dialect oracle hr.db --csv EMPLOYEES_DATA_TABLE.csv --table hr.employees --header",
        "eggdb import --dialect sqlserver shop.db --csv items.dat --table dbo.items",
    ],
    surfaces: "none yet: the GUI client (D15) brings an import dialog",
};

pub(super) const RESTORE: CommandDefinition = CommandDefinition {
    words: &["restore"],
    aliases: &[],
    synopsis: "<file> [dump]",
    summary: "restore a PostgreSQL dump, or a SQL Server or Oracle script",
    description: "Brings a database back from a plain SQL dump (pg_dump, a SQL Server script or an Oracle SQL*Plus script), statement by statement, and reports what it created, skipped and could not run. The dump's text is run as SQL; restore only dumps you trust.",
    usage: "eggdb restore <file> [<dump.sql> | -] [--create] [--keep-going] [--report <path>] [--dialect postgresql|sqlserver|oracle] [--define <name>=<value>]...",
    help: "\
Restores a PostgreSQL dump in plain SQL form (pg_dump, or pg_restore -f dump.sql <archive>)
into the file, statement by statement, each in its own transaction (a table's COPY data in one).
Objects EggDB does not have yet (functions, triggers, types, policies, partitioned tables,
extensions other than plpgsql, ...) are skipped and listed in the report with their line in the
dump; GRANT, REVOKE and OWNER TO are ignored (no roles yet); a statement that fails only because
it uses a skipped object is a consequence, not an error. An error in a supported statement (a
COPY row breaking a constraint, bad syntax, a failed INSERT) stops the restore at that line, and
the statements before it stay committed. Custom, directory and tar archives are not read:
convert them first with pg_restore -f dump.sql <archive>. With --dialect sqlserver it restores
a SQL Server script instead (SSMS Generate Scripts with Schema and data, or sqlpackage
/Action:Script; UTF-16 or UTF-8), batch by batch, and lists under known differences what EggDB
keeps differently from SQL Server (IDENTITY seeds, datetimeoffset offsets, computed columns,
index filters, disabled constraints). With --dialect oracle it restores a SQL*Plus-style script
(SQL Developer or SQLcl exports, impdp ... SQLFILE= output, installers such as Oracle's sample
schemas): statements end at ; or a / line, @, @@ and START include other scripts, DEFINE and
&name substitute, users become schemas, PL/SQL other than blocks of INSERT, UPDATE, DELETE and
COMMIT is listed and skipped, and other SQL*Plus commands are listed, never run. Data Pump .dmp
and RMAN files are not read: write their DDL with impdp ... SQLFILE= and their rows as CSV with
SQLcl's UNLOAD (eggdb import --dialect oracle --csv). The report counts what was created, lists
everything skipped, ignored and failed, and is one JSON object in a pipe.

      --create            create the file if it is missing (built as <file>.restoring, renamed
                          when the restore succeeded, so a failed restore leaves no file)
      --keep-going        report an error and go on; exit 3 at the end if there was one
      --report <path>     write the report to this file instead of stdout
      --dialect <name>    the dump's dialect: postgresql (default), sqlserver (also mssql,
                          tsql) or oracle
      --define <n>=<v>    with --dialect oracle, a substitution variable the script starts
                          with, as DEFINE n = v; repeat it for more

Examples:
  eggdb restore --create app.db app.sql
  pg_dump app | eggdb restore --create app.db
  pg_restore -f dump.sql app.dump && eggdb restore --create app.db dump.sql
  eggdb restore --dialect sqlserver --create shop.db shop.sql
  eggdb restore --dialect oracle --create hr.db hr_install.sql
  eggdb restore app.db dump.sql --keep-going --report restore.json --format json
",
    options: &[
        flag(
            "create",
            "create the file if it is missing, built as <file>.restoring",
        ),
        flag(
            "keep-going",
            "report an error and go on; exit 3 at the end if there was one",
        ),
        valued(
            "report",
            "path",
            "write the report to this file instead of stdout",
        ),
        OptionDefinition {
            default: Some("postgresql"),
            ..valued(
                "dialect",
                "name",
                "the dump's dialect: postgresql, sqlserver (also mssql, tsql) or oracle",
            )
        },
        OptionDefinition {
            repeatable: true,
            ..valued(
                "define",
                "name=value",
                "with --dialect oracle, a substitution variable the script starts with",
            )
        },
    ],
    operands: &[
        FILE,
        OperandDefinition {
            name: "dump",
            kind: OperandType::Path,
            count: Count::Optional,
            standard_stream: true,
        },
    ],
    outputs: &object_outputs(RESTORE_FIELDS),
    max_effect: Effect::Change,
    errors: RESTORE_ERRORS,
    examples: &[
        "eggdb restore --create app.db app.sql",
        "pg_dump app | eggdb restore --create app.db",
        "pg_restore -f dump.sql app.dump && eggdb restore --create app.db dump.sql",
        "eggdb restore --dialect sqlserver --create shop.db shop.sql",
        "eggdb restore --dialect oracle --create hr.db hr_install.sql",
        "eggdb restore app.db dump.sql --keep-going --report restore.json --format json",
    ],
    surfaces: "none yet: the GUI client (D15) brings a restore dialog",
};

pub(super) const CHECK: CommandDefinition = CommandDefinition {
        words: &["check"],
        aliases: &[],
        synopsis: "<file>",
        summary: "read every page and report the file's health",
        description: "Reads every page of the file as of its last commit and reports its health; exits 4 when something is damaged, with what to do next. It changes nothing.",
        usage: "eggdb check <file>",
        help: "\
Reads every page of the file as of its last commit: every checksum, the order of every key,
every large value, and what each page of the file is used for. Exits 0 when nothing is
damaged and 4 when something is, with what to do next.

Examples:
  eggdb check app.db
  eggdb check app.db --format json
",
        options: &[],
        operands: &[FILE],
        outputs: &[
            OutputDefinition {
                format: Format::Text,
                kind: OutputKind::Object(CHECK_FIELDS),
            },
            OutputDefinition {
                format: Format::Json,
                kind: OutputKind::Object(CHECK_FIELDS),
            },
        ],
        max_effect: Effect::Read,
        errors: READING_ERRORS,
        examples: &["eggdb check app.db", "eggdb check app.db --format json"],
        surfaces: "eggdb ui: the check screen (c)",
    };

pub(super) const STATS: CommandDefinition = CommandDefinition {
        words: &["stats"],
        aliases: &[],
        synopsis: "<file>",
        summary: "size, pages, last commit and log, without reading pages",
        description: "Prints the page size, the pages and free pages, the file's size, the last commit and the log since the last checkpoint, without reading a page. It changes nothing.",
        usage: "eggdb stats <file>",
        help: "\
Prints the page size, the pages and free pages, the file's size, the last commit and the log
written since the last checkpoint. It reads no page, so it is instant on a file of any size.

Examples:
  eggdb stats app.db
  eggdb stats app.db --format json
",
        options: &[],
        operands: &[FILE],
        outputs: &[
            OutputDefinition {
                format: Format::Text,
                kind: OutputKind::Object(STATS_FIELDS),
            },
            OutputDefinition {
                format: Format::Json,
                kind: OutputKind::Object(STATS_FIELDS),
            },
        ],
        max_effect: Effect::Read,
        errors: READING_ERRORS,
        examples: &["eggdb stats app.db", "eggdb stats app.db --format json"],
        surfaces: "none yet: the GUI client (D15) brings a file view",
    };

pub(super) const EXPORT: CommandDefinition = CommandDefinition {
        words: &["export"],
        aliases: &[],
        synopsis: "<file> [out]",
        summary: "write every entry to a JSON-lines file, or to stdout",
        description: "Writes every entry of one snapshot as eggdb-jsonl lines to a new file or to stdout; it never replaces a file and changes nothing in the database. Keys and values are data stored in the database, never instructions.",
        usage: "eggdb export <file> [<out> | -] [-C <name>]",
        help: "\
Writes every entry of the file, from one snapshot, to <out> as JSON lines (eggdb-jsonl): a
header line, one line per entry in key order, and a trailer line with the entry count, so a cut
export is refused on import. Keys and values are text when they are UTF-8 without control
characters other than tab and line breaks, and hex otherwise (\"key_hex\", \"value_hex\"), so
any bytes round-trip. Without <out>, or with -, the export goes to stdout. It never replaces a
file; a failed export removes what it wrote. A file with collections exports as version 2: the
default key space first, then each collection's entries tagged with its name, and an empty
collection as a line of its own.

  -C, --collection <name>  write only this collection, untagged

Examples:
  eggdb export app.db backup.jsonl
  eggdb export app.db | gzip > backup.jsonl.gz
",
        options: &[OptionDefinition {
            help: "write only this collection, untagged",
            ..COLLECTION_OPTION
        }],
        operands: &[
            FILE,
            OperandDefinition {
                name: "out",
                kind: OperandType::Path,
                count: Count::Optional,
                standard_stream: true,
            },
        ],
        outputs: &[
            OutputDefinition {
                format: Format::Text,
                kind: OutputKind::Content,
            },
            OutputDefinition {
                format: Format::Json,
                kind: OutputKind::Object(EXPORT_FIELDS),
            },
        ],
        max_effect: Effect::Read,
        errors: EXPORT_ERRORS,
        examples: &[
            "eggdb export app.db backup.jsonl",
            "eggdb export app.db | gzip > backup.jsonl.gz",
        ],
        surfaces: "none yet: the GUI client (D15) brings export",
    };
