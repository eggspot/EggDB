//! The `eggdb` command, run as a real process: every command, every exit code, the formats a
//! pipe gets, byte arguments, large values, a closed pipe, damaged files, the help text, and
//! random arguments. A busy file has a test binary of its own, `cli_busy.rs`.
//!
//! A child process started while this process has a database file open could hold that file's
//! lock for a moment, so starting a child and opening a database take turns.

use eggdb::Database;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT_DIRECTORY: AtomicU32 = AtomicU32::new(0);
static OPEN_FILES: Mutex<()> = Mutex::new(());

fn spawn(command: &mut Command) -> std::process::Child {
    let _no_database_open = OPEN_FILES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    command.spawn().unwrap()
}

struct TempDirectory {
    path: PathBuf,
}

impl TempDirectory {
    fn new(name: &str) -> Self {
        let number = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "eggdb-cli-test-{name}-{}-{number}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn eggdb(&self, args: &[&str]) -> Output {
        self.eggdb_with_stdin(args, b"")
    }

    fn eggdb_with_stdin(&self, args: &[&str], stdin: &[u8]) -> Output {
        let mut child = spawn(
            self.command(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        );
        let mut input = child.stdin.take().unwrap();
        let stdin = stdin.to_vec();
        let writer = std::thread::spawn(move || {
            let _ = input.write_all(&stdin);
        });
        let output = child.wait_with_output().unwrap();
        writer.join().unwrap();
        output
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_eggdb"));
        command
            .args(args)
            .current_dir(&self.path)
            .env_remove("NO_COLOR")
            .env("TERM", "xterm-256color");
        command
    }

    fn database_with(&self, name: &str, entries: &[(&[u8], &[u8])]) -> PathBuf {
        let path = self.path.join(name);
        let _no_spawn = OPEN_FILES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let db = Database::open(&path).unwrap();
        db.write(|tx| {
            for (key, value) in entries {
                tx.put(key, value)?;
            }
            Ok(())
        })
        .unwrap();
        db.close().unwrap();
        path
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("eggdb ended by a signal")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[track_caller]
fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        code(output),
        expected,
        "stdout: {}\nstderr: {}",
        stdout(output),
        stderr(output)
    );
}

const MAIN_HELP: &str = "\
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
";

const SCAN_HELP: &str = "\
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
";

#[test]
fn help_text_matches_the_design() {
    let dir = TempDirectory::new("help");
    for args in [&["--help"][..], &["-h"], &["help"]] {
        let output = dir.eggdb(args);
        assert_exit(&output, 0);
        assert_eq!(stdout(&output), MAIN_HELP, "{args:?}");
    }
    for args in [
        &["help", "scan"][..],
        &["scan", "--help"],
        &["scan", "app.db", "-h"],
    ] {
        let output = dir.eggdb(args);
        assert_exit(&output, 0);
        assert_eq!(stdout(&output), SCAN_HELP, "{args:?}");
    }
    for command in ["create", "put", "get", "del", "delete", "check", "stats"] {
        let output = dir.eggdb(&["help", command]);
        assert_exit(&output, 0);
        assert!(stdout(&output).starts_with("Usage: eggdb "), "{command}");
        assert!(stdout(&output).contains("\nExamples:\n"), "{command}");
    }
}

#[test]
fn no_arguments_prints_the_help_and_exits_2() {
    let output = TempDirectory::new("no-arguments").eggdb(&[]);
    assert_exit(&output, 2);
    assert_eq!(stderr(&output), MAIN_HELP);
    assert!(output.stdout.is_empty());
}

#[test]
fn version_prints_the_package_version() {
    let dir = TempDirectory::new("version");
    let expected = format!("eggdb {}\n", env!("CARGO_PKG_VERSION"));
    for args in [&["--version"][..], &["-V"], &["version"]] {
        let output = dir.eggdb(args);
        assert_exit(&output, 0);
        assert_eq!(stdout(&output), expected);
    }
}

#[test]
fn a_pipe_gets_json_and_get_gets_the_raw_bytes() {
    let dir = TempDirectory::new("pipe-defaults");
    let create = dir.eggdb(&["create", "app.db"]);
    assert_exit(&create, 0);
    assert_eq!(
        stdout(&create),
        "{\"file\":\"app.db\",\"page_size\":4096}\n"
    );
    let put = dir.eggdb(&["put", "app.db", "user:1", "Ada"]);
    assert_exit(&put, 0);
    assert_eq!(stdout(&put), "{\"key\":\"user:1\",\"stored\":true}\n");
    let get = dir.eggdb(&["get", "app.db", "user:1"]);
    assert_exit(&get, 0);
    assert_eq!(get.stdout, b"Ada");
    let scan = dir.eggdb(&["scan", "app.db"]);
    assert_exit(&scan, 0);
    assert_eq!(stdout(&scan), "{\"key\":\"user:1\",\"value\":\"Ada\"}\n");
    assert!(scan.stderr.is_empty(), "{}", stderr(&scan));
    let check = dir.eggdb(&["check", "app.db"]);
    assert_exit(&check, 0);
    assert!(stdout(&check).starts_with("{\"ok\":true,\"file\":\"app.db\",\"commit_stamp\":\""));
    assert!(stdout(&check).contains("\"entries\":1,"));
    let stats = dir.eggdb(&["stats", "app.db"]);
    assert_exit(&stats, 0);
    assert!(stdout(&stats).starts_with("{\"file\":\"app.db\",\"page_size\":4096,\"pages\":"));
    assert!(stdout(&stats).ends_with(",\"log_bytes_since_checkpoint\":0}\n"));
    let del = dir.eggdb(&["del", "app.db", "user:1"]);
    assert_exit(&del, 0);
    assert_eq!(stdout(&del), "{\"key\":\"user:1\",\"deleted\":true}\n");
    for output in [&create, &put, &get, &scan, &check, &stats, &del] {
        assert!(!output.stdout.contains(&0x1b), "no colour in a pipe");
    }
}

#[test]
fn text_format_reads_like_the_design() {
    let dir = TempDirectory::new("text");
    assert_exit(&dir.eggdb(&["create", "app.db", "-F", "text"]), 0);
    let created = dir.eggdb(&["create", "other.db", "--format=text", "--page-size", "16k"]);
    assert_eq!(stdout(&created), "created other.db (16 KiB pages)\n");
    for (key, value) in [
        ("user:1", "Ada"),
        ("user:2", "Grace"),
        ("user:3", "0x00ff10"),
    ] {
        let put = dir.eggdb(&["put", "app.db", key, value, "-F", "text"]);
        assert_exit(&put, 0);
        assert!(put.stdout.is_empty());
    }
    let long = "xyz".repeat(100);
    assert_exit(&dir.eggdb(&["put", "app.db", "long", &long]), 0);
    let scan = dir.eggdb(&["scan", "app.db", "-F", "text", "--prefix", "user:"]);
    assert_exit(&scan, 0);
    assert_eq!(
        stdout(&scan),
        "user:1\tAda\nuser:2\tGrace\nuser:3\t0x00ff10\n"
    );
    assert_eq!(stderr(&scan), "3 entries\n");
    let cut = dir.eggdb(&["scan", "app.db", "-F", "text", "-p", "long"]);
    assert_eq!(
        stdout(&cut),
        format!("long\t{} … (300 bytes)\n", &long[..128])
    );
    assert_eq!(stderr(&cut), "1 entry\n");
    let none = dir.eggdb(&["scan", "app.db", "-F", "text", "-p", "nothing"]);
    assert_eq!(
        (stdout(&none), stderr(&none)),
        (String::new(), "no entries\n".to_owned())
    );
    let get = dir.eggdb(&["get", "app.db", "user:1", "-F", "text"]);
    assert_eq!(stdout(&get), "Ada\n");
    let stats = dir.eggdb(&["stats", "other.db", "-F", "text"]);
    assert_exit(&stats, 0);
    let stats = stdout(&stats);
    assert!(
        stats.starts_with("other.db\n  page size       16 KiB\n  pages           "),
        "{stats}"
    );
    assert!(stats.contains("\n  last commit     none\n"), "{stats}");
    assert!(
        stats.ends_with("\n  log since checkpoint  0 B (the next open replays nothing)\n"),
        "{stats}"
    );
    let check = dir.eggdb(&["check", "app.db", "-F", "text"]);
    assert_exit(&check, 0);
    let check = stdout(&check);
    assert!(
        check.starts_with("checked app.db as of commit 20"),
        "{check}"
    );
    assert!(check.contains(" UTC (stamp "), "{check}");
    assert!(check.contains("\n  entries         4\n"), "{check}");
    assert!(check.contains("\n  leaked pages    0\n"), "{check}");
    assert!(check.ends_with(
        "\nok: every page passed its checksum, every key is in order, and every page of the file is accounted for.\n"
    ), "{check}");
}

#[test]
fn hex_files_stdin_and_escapes_give_exact_bytes() {
    let dir = TempDirectory::new("byte-arguments");
    assert_exit(&dir.eggdb(&["create", "app.db"]), 0);
    let blob: Vec<u8> = (0..=255u8).collect();
    std::fs::write(dir.path().join("blob.bin"), &blob).unwrap();
    assert_exit(&dir.eggdb(&["put", "app.db", "0x00FF", "@blob.bin"]), 0);
    let get = dir.eggdb(&["get", "app.db", "0x00ff"]);
    assert_exit(&get, 0);
    assert_eq!(get.stdout, blob);
    let hex = dir.eggdb(&["get", "app.db", "0x00ff", "-F", "hex"]);
    let expected: String = blob.iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(stdout(&hex), format!("{expected}\n"));
    let json = dir.eggdb(&["get", "app.db", "0x00ff", "-F", "json"]);
    assert!(stdout(&json).starts_with("{\"key_hex\":\"00ff\",\"value_hex\":\"000102"));

    let piped = dir.eggdb_with_stdin(&["put", "app.db", "photo"], b"hi\n");
    assert_exit(&piped, 0);
    assert_eq!(dir.eggdb(&["get", "app.db", "photo"]).stdout, b"hi\n");
    let explicit = dir.eggdb_with_stdin(&["put", "app.db", "=@-", "@-"], b"\x00\x01");
    assert_exit(&explicit, 0);
    assert_eq!(dir.eggdb(&["get", "app.db", "=@-"]).stdout, b"\x00\x01");
    assert_exit(&dir.eggdb(&["put", "app.db", "", "="]), 0);
    let empty = dir.eggdb(&["get", "app.db", "", "-F", "json"]);
    assert_eq!(stdout(&empty), "{\"key\":\"\",\"value\":\"\"}\n");
    let dashes = dir.eggdb(&["put", "app.db", "--", "-k", "-v"]);
    assert_exit(&dashes, 0);
    assert_eq!(dir.eggdb(&["get", "app.db", "--", "-k"]).stdout, b"-v");
    let twice = dir.eggdb_with_stdin(&["put", "app.db", "@-", "@-"], b"x");
    assert_exit(&twice, 2);

    let missing = dir.eggdb(&["put", "app.db", "k", "@no-such-file"]);
    assert_exit(&missing, 2);
    assert!(stderr(&missing).starts_with("eggdb: cannot read @no-such-file: "));
    let odd = dir.eggdb(&["get", "app.db", "0x4"]);
    assert_exit(&odd, 2);
    assert!(stderr(&odd).starts_with("eggdb: hex needs an even number of digits: 0x4\n"));
    let bad = dir.eggdb(&["get", "app.db", "0x4g"]);
    assert!(stderr(&bad).starts_with("eggdb: not a hex digit in 0x4g: 'g'\n"));
}

#[test]
fn a_large_value_round_trips_through_stdin() {
    let dir = TempDirectory::new("large");
    assert_exit(&dir.eggdb(&["create", "app.db"]), 0);
    let value: Vec<u8> = (0..20u32 << 20).map(|i| (i % 251) as u8).collect();
    let put = dir.eggdb_with_stdin(&["put", "app.db", "big"], &value);
    assert_exit(&put, 0);
    let get = dir.eggdb(&["get", "app.db", "big"]);
    assert_exit(&get, 0);
    assert!(get.stdout == value, "the value came back changed");
    let check = dir.eggdb(&["check", "app.db"]);
    assert_exit(&check, 0);
    assert!(
        stdout(&check).contains("\"overflow_values\":1,"),
        "{}",
        stdout(&check)
    );
    let small_cache = dir.eggdb_with_stdin(&["put", "app.db", "big", "--cache", "1m"], &value);
    assert_exit(&small_cache, 3);
    assert!(stderr(&small_cache).starts_with("eggdb: cannot store big in app.db: "));
}

#[test]
fn scan_ranges_order_and_formats() {
    let dir = TempDirectory::new("scan");
    let entries: Vec<(Vec<u8>, Vec<u8>)> = ["a", "b", "c", "d"]
        .iter()
        .map(|key| (key.as_bytes().to_vec(), key.to_uppercase().into_bytes()))
        .collect();
    let borrowed: Vec<(&[u8], &[u8])> = entries
        .iter()
        .map(|(key, value)| (key.as_slice(), value.as_slice()))
        .collect();
    dir.database_with("app.db", &borrowed);
    let keys = |args: &[&str]| {
        let mut all = vec!["scan", "app.db", "-k", "-F", "raw"];
        all.extend_from_slice(args);
        let output = dir.eggdb(&all);
        assert_exit(&output, 0);
        stdout(&output)
    };
    assert_eq!(keys(&[]), "a\nb\nc\nd\n");
    assert_eq!(keys(&["--from", "b", "--to", "d"]), "b\nc\n");
    assert_eq!(keys(&["--from", "d", "--to", "b"]), "");
    assert_eq!(keys(&["--to", "b"]), "a\n");
    assert_eq!(keys(&["-r"]), "d\nc\nb\na\n");
    assert_eq!(keys(&["-r", "-n", "1"]), "d\n");
    assert_eq!(keys(&["--limit=2"]), "a\nb\n");
    assert_eq!(keys(&["-n", "0"]), "");
    assert_eq!(keys(&["-p", "c"]), "c\n");
    let raw = dir.eggdb(&["scan", "app.db", "-F", "raw", "-n", "2"]);
    assert_eq!(stdout(&raw), "a\tA\nb\tB\n");
    let hex = dir.eggdb(&["scan", "app.db", "-F", "hex", "-n", "1"]);
    assert_eq!(stdout(&hex), "61\t41\n");
    let hex_keys = dir.eggdb(&["scan", "app.db", "-F", "hex", "-k", "-r", "-n", "1"]);
    assert_eq!(stdout(&hex_keys), "64\n");
    let json_keys = dir.eggdb(&["scan", "app.db", "-k", "-n", "1"]);
    assert_eq!(stdout(&json_keys), "{\"key\":\"a\"}\n");
    let both = dir.eggdb(&["scan", "app.db", "-p", "a", "--from", "a"]);
    assert_exit(&both, 2);
    assert!(stderr(&both).starts_with("eggdb: --prefix cannot be combined with --from or --to\n"));
}

#[test]
fn every_exit_code_and_its_message() {
    let dir = TempDirectory::new("exit-codes");
    assert_exit(&dir.eggdb(&["put", "--create", "app.db", "k", "v"]), 0);

    let absent = dir.eggdb(&["get", "app.db", "nope", "-F", "raw"]);
    assert_exit(&absent, 1);
    assert_eq!(stderr(&absent), "eggdb: key not found: nope\n");
    assert!(absent.stdout.is_empty());
    let absent_json = dir.eggdb(&["get", "app.db", "nope", "-F", "json"]);
    assert_exit(&absent_json, 1);
    assert!(absent_json.stdout.is_empty() && absent_json.stderr.is_empty());
    let nothing = dir.eggdb(&["del", "app.db", "nope", "-F", "text"]);
    assert_exit(&nothing, 1);
    assert_eq!(
        stderr(&nothing),
        "eggdb: nothing to delete: nope is not in app.db\n"
    );
    let nothing_json = dir.eggdb(&["delete", "app.db", "nope"]);
    assert_exit(&nothing_json, 1);
    assert_eq!(
        stdout(&nothing_json),
        "{\"key\":\"nope\",\"deleted\":false}\n"
    );

    let unknown = dir.eggdb(&["frobnicate", "app.db"]);
    assert_exit(&unknown, 2);
    assert_eq!(
        stderr(&unknown),
        "eggdb: unknown command 'frobnicate'. Commands: create, put, get, del, scan, check, stats, help, version.\n  try: eggdb help\n"
    );
    let option = dir.eggdb(&["get", "app.db", "k", "--reverse"]);
    assert_exit(&option, 2);
    assert_eq!(
        stderr(&option),
        "eggdb: unknown option '--reverse' for eggdb get\n  usage: eggdb get <file> <key>\n  try: eggdb help get\n"
    );
    for reserved in ["export", "import", "shell", "backup", "compact", "ui"] {
        let output = dir.eggdb(&[reserved, "app.db"]);
        assert_exit(&output, 2);
        assert_eq!(
            stderr(&output),
            format!("eggdb: eggdb {reserved} is not in this release yet; see docs/roadmap.md\n")
        );
    }
    let format = dir.eggdb(&["scan", "app.db", "--format", "yaml"]);
    assert_exit(&format, 2);
    assert!(
        stderr(&format).starts_with("eggdb: --format must be text, json, hex or raw, not 'yaml'\n")
    );
    let size = dir.eggdb(&["get", "app.db", "k", "--cache", "lots"]);
    assert!(
        stderr(&size)
            .starts_with("eggdb: --cache needs a size such as 4096, 64k, 16m or 1g, not 'lots'\n")
    );

    let exists = dir.eggdb(&["create", "app.db"]);
    assert_exit(&exists, 3);
    assert_eq!(
        stderr(&exists),
        "eggdb: app.db already exists; eggdb create never replaces a file. Use it as it is, or choose another name.\n"
    );
    let missing_read = dir.eggdb(&["get", "missing.db", "k"]);
    assert_exit(&missing_read, 3);
    assert_eq!(
        stderr(&missing_read),
        "eggdb: cannot open missing.db: no such file. Check the path, or create it with `eggdb create missing.db`\n"
    );
    let missing_write = dir.eggdb(&["put", "missing.db", "k", "v"]);
    assert_exit(&missing_write, 3);
    assert_eq!(
        stderr(&missing_write),
        "eggdb: cannot open missing.db: no such file. Create it with `eggdb create missing.db`, or pass --create to put.\n  try: eggdb create missing.db\n"
    );
    assert!(
        !dir.path().join("missing.db").exists(),
        "put must not create a file"
    );
    std::fs::write(dir.path().join("notes.txt"), vec![b'x'; 10_000]).unwrap();
    let not_database = dir.eggdb(&["stats", "notes.txt"]);
    assert_exit(&not_database, 3);
    assert_eq!(
        stderr(&not_database),
        "eggdb: cannot open notes.txt: not an EggDB database. If it is a database of another kind, EggDB restores its backups in a later release (docs/roadmap.md).\n"
    );
    let long_key = "k".repeat(600);
    let too_long = dir.eggdb(&["put", "app.db", &long_key, "v"]);
    assert_exit(&too_long, 3);
    assert!(stderr(&too_long).contains("a key of 600 bytes is longer than the 512 bytes"));
    for output in [&exists, &missing_read, &not_database, &too_long] {
        assert!(!stderr(output).contains("Error {"), "{}", stderr(output));
    }
}

#[test]
fn a_closed_pipe_ends_the_scan_quietly() {
    let dir = TempDirectory::new("closed-pipe");
    let entries: Vec<(Vec<u8>, Vec<u8>)> = (0..20_000u32)
        .map(|i| (format!("key:{i:08}").into_bytes(), vec![b'v'; 100]))
        .collect();
    let borrowed: Vec<(&[u8], &[u8])> = entries
        .iter()
        .map(|(key, value)| (key.as_slice(), value.as_slice()))
        .collect();
    dir.database_with("app.db", &borrowed);
    let mut child = spawn(
        dir.command(&["scan", "app.db", "-F", "text"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    let mut first = [0u8; 16];
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut first)
        .unwrap();
    child.stdout = None;
    let output = child.wait_with_output().unwrap();
    assert_exit(&output, 0);
    assert!(output.stderr.is_empty(), "{}", stderr(&output));
    assert_eq!(&first, b"key:00000000\tvvv");
}

fn leaf_pages(file: &[u8], page_size: usize) -> Vec<usize> {
    (2..file.len() / page_size)
        .filter(|page| {
            let bytes = &file[page * page_size..(page + 1) * page_size];
            bytes[4] == 3 && bytes[6] == 0 && bytes[7] == 0 && bytes[16 + 24] == 0
        })
        .collect()
}

#[test]
fn a_damaged_page_makes_check_exit_4_with_what_to_do() {
    let dir = TempDirectory::new("damaged-page");
    let entries: Vec<(Vec<u8>, Vec<u8>)> = (0..400u32)
        .map(|i| (format!("key:{i:05}").into_bytes(), vec![b'v'; 60]))
        .collect();
    let borrowed: Vec<(&[u8], &[u8])> = entries
        .iter()
        .map(|(key, value)| (key.as_slice(), value.as_slice()))
        .collect();
    let path = dir.database_with("app.db", &borrowed);
    let mut file = std::fs::read(&path).unwrap();
    let leaves = leaf_pages(&file, 4096);
    assert!(leaves.len() > 2, "{leaves:?}");
    let damaged = leaves[leaves.len() / 2];
    file[damaged * 4096 + 2048] ^= 0x5a;
    std::fs::write(&path, &file).unwrap();

    let check = dir.eggdb(&["check", "app.db", "-F", "text"]);
    assert_exit(&check, 4);
    let text = stdout(&check);
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[0].starts_with("checked app.db as of commit "),
        "{text}"
    );
    assert_eq!(
        lines[1],
        format!(
            "damaged: page {damaged} failed its checksum: it is damaged, torn by a crash, or belongs elsewhere"
        )
    );
    assert_eq!(
        lines[2..].join("\n"),
        "  what was checked  every page read before the damage passed its checksum; the check stopped at the damage
  what to do        EggDB does not repair files yet. Restore app.db from your latest backup.
                    Keep the damaged file unchanged: `eggdb scan` may still read part of the data,
                    and an issue at https://github.com/eggspot/EggDB/issues with this output helps us."
    );
    let json = dir.eggdb(&["check", "app.db"]);
    assert_exit(&json, 4);
    assert!(
        stdout(&json).starts_with(&format!(
            "{{\"ok\":false,\"file\":\"app.db\",\"problem\":\"page {damaged} failed its checksum: it is damaged, torn by a crash, or belongs elsewhere\",\"page\":{damaged},\"what_to_do\":\"EggDB does not repair files yet."
        )),
        "{}",
        stdout(&json)
    );
    let scan = dir.eggdb(&["scan", "app.db"]);
    assert_exit(&scan, 4);
    assert!(
        stderr(&scan).starts_with("eggdb: cannot scan app.db: page "),
        "{}",
        stderr(&scan)
    );
}

#[test]
fn a_file_damaged_where_opening_reads_exits_4_from_every_command() {
    let dir = TempDirectory::new("damaged-open");
    let path = dir.database_with("app.db", &[(b"k", b"v")]);
    let file = std::fs::read(&path).unwrap();
    std::fs::write(&path, &file[..file.len() - 4096]).unwrap();
    let check = dir.eggdb(&["check", "app.db", "-F", "text"]);
    assert_exit(&check, 4);
    let text = stdout(&check);
    assert!(text.starts_with("checked app.db\ndamaged: "), "{text}");
    assert!(text.contains(
        "\n  what was checked  opening the file read its header, free list and log; it stopped at the damage\n"
    ), "{text}");
    assert!(
        text.contains("\n  what to do        EggDB does not repair files yet."),
        "{text}"
    );
    for command in [
        &["get", "app.db", "k"][..],
        &["stats", "app.db"],
        &["put", "app.db", "k", "v"],
    ] {
        let output = dir.eggdb(command);
        assert_exit(&output, 4);
        assert!(
            stderr(&output).starts_with("eggdb: cannot open app.db: "),
            "{}",
            stderr(&output)
        );
    }
}

#[test]
fn random_arguments_never_panic_the_binary() {
    let dir = TempDirectory::new("random");
    dir.database_with("app.db", &[(b"user:1", b"Ada"), (b"user:2", b"Grace")]);
    const WORDS: &[&str] = &[
        "create",
        "put",
        "get",
        "del",
        "scan",
        "check",
        "stats",
        "help",
        "version",
        "ui",
        "app.db",
        "new.db",
        "missing/x.db",
        "-F",
        "json",
        "text",
        "hex",
        "raw",
        "--color",
        "always",
        "--cache",
        "1",
        "64k",
        "--page-size",
        "8k",
        "--create",
        "-p",
        "--from",
        "--to",
        "-r",
        "-n",
        "2",
        "-k",
        "-h",
        "-V",
        "--",
        "-",
        "@-",
        "@app.db",
        "0x00ff",
        "0x4",
        "=",
        "user:1",
        "",
    ];
    let mut state = 0x853c_49e6_748f_ea9bu64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for round in 0..150 {
        let length = (next() % 7) as usize;
        let args: Vec<&str> = (0..length)
            .map(|_| WORDS[(next() % WORDS.len() as u64) as usize])
            .collect();
        let output = dir.eggdb_with_stdin(&args, b"stdin bytes");
        let exit = code(&output);
        assert!(
            (0..=5).contains(&exit),
            "round {round}: {args:?} exited {exit}: {}",
            stderr(&output)
        );
        assert!(
            !stderr(&output).contains("panicked"),
            "round {round}: {args:?}: {}",
            stderr(&output)
        );
    }
}
