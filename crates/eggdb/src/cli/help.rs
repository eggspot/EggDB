use super::args::CommandName;

pub(crate) const MAIN: &str = "\
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
  ui     <file>                 browse and edit keys full screen in the terminal
  export <file> [out]           write every entry to a JSON-lines file, or to stdout
  import <file> [in]            load an export, from a file or stdin; see --create
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
            "eggdb import <file> [<in> | -] [--create [--page-size <bytes>]] [--on-conflict error|overwrite|skip] [--dry-run]"
        }
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

Examples:
  eggdb import --create copy.db backup.jsonl
  gunzip -c backup.jsonl.gz | eggdb import --create copy.db
  eggdb import app.db fixes.jsonl --on-conflict overwrite
  eggdb import app.db backup.jsonl --dry-run
";
