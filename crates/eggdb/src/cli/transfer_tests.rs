use super::Terminal;
use super::jsonl::{self, Header};
use super::tests::{Ran, Rng, TempDirectory, run_with};
use std::path::Path;

fn pipe() -> Terminal {
    Terminal::default()
}

fn run(args: &[&str]) -> Ran {
    run_with(args, b"", pipe())
}

#[track_caller]
fn ok(args: &[&str]) -> Ran {
    let ran = run(args);
    assert_eq!(ran.exit, 0, "{args:?}: {}", stderr(&ran));
    ran
}

fn stderr(ran: &Ran) -> String {
    String::from_utf8_lossy(&ran.stderr).into_owned()
}

fn stdout(ran: &Ran) -> String {
    String::from_utf8_lossy(&ran.stdout).into_owned()
}

fn scan(database: &str) -> Vec<u8> {
    ok(&["scan", database, "-F", "hex"]).stdout
}

fn database_with(path: &str, entries: &[(&[u8], &[u8])]) {
    let db = eggdb::Database::open(path).unwrap();
    db.write(|tx| {
        for (key, value) in entries {
            tx.put(key, value)?;
        }
        Ok(())
    })
    .unwrap();
    db.close().unwrap();
}

fn export_bytes(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    jsonl::write_header(&mut out, &Header::default()).unwrap();
    for (key, value) in entries {
        jsonl::write_entry(&mut out, key, value).unwrap();
    }
    jsonl::write_trailer(&mut out, entries.len() as u64).unwrap();
    out
}

fn json(path: &str) -> String {
    path.replace('\\', "\\\\")
}

fn exists(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn staging(path: &str) -> String {
    format!("{path}.importing")
}

#[test]
fn every_kind_of_entry_round_trips_through_a_file_and_through_a_pipe() {
    let directory = TempDirectory::new("round-trip");
    let source = directory.file("source.db");
    let mut rng = Rng(0xabcdef);
    let large: Vec<u8> = (0..5 << 20).map(|_| rng.next() as u8).collect();
    let large_text = "line of text\n".repeat(300_000);
    database_with(
        &source,
        &[
            (b"user:1", b"Ada"),
            (&[0x00, 0xff, 0x80], &[0xde, 0xad]),
            (b"", b"the empty key"),
            (b"empty value", b""),
            (b"quotes", b"\"\\\n\t\r\x07"),
            (b"large", &large),
            (b"large text", large_text.as_bytes()),
        ],
    );
    let export = directory.file("backup.jsonl");
    let exported = ok(&["export", &source, &export]);
    assert_eq!(
        stdout(&exported),
        format!(
            "{{\"file\":\"{}\",\"out\":\"{}\",\"entries\":7,\"collections\":0}}\n",
            json(&source),
            json(&export)
        )
    );
    let copy = directory.file("copy.db");
    let imported = ok(&["import", "--create", &copy, &export]);
    assert_eq!(
        stdout(&imported),
        format!(
            "{{\"file\":\"{}\",\"from\":\"{}\",\"entries\":7,\"imported\":7,\"skipped\":0,\"created\":true,\"dry_run\":false,\"collections_added\":0}}\n",
            json(&copy),
            json(&export)
        )
    );
    assert_eq!(scan(&copy), scan(&source));
    ok(&["check", &copy]);
    ok(&["check", &source]);
    assert!(!exists(&staging(&copy)));

    let piped = ok(&["export", &source, "-"]);
    let through_stdin = directory.file("through-stdin.db");
    let ran = run_with(
        &["import", "--create", &through_stdin],
        &piped.stdout,
        pipe(),
    );
    assert_eq!(ran.exit, 0, "{}", stderr(&ran));
    assert_eq!(scan(&through_stdin), scan(&source));
    let after_header = |bytes: &[u8]| -> Vec<u8> {
        let start = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(0, |at| at + 1);
        bytes[start..].to_vec()
    };
    assert_eq!(
        after_header(&piped.stdout),
        after_header(&std::fs::read(&export).unwrap())
    );
}

#[test]
fn an_empty_database_round_trips() {
    let directory = TempDirectory::new("empty");
    let source = directory.file("empty.db");
    ok(&["create", &source]);
    let export = ok(&["export", &source]);
    let lines: Vec<&str> = std::str::from_utf8(&export.stdout)
        .unwrap()
        .lines()
        .collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with("{\"format\":\"eggdb-jsonl\",\"version\":1,\"eggdb\":\""));
    assert_eq!(lines[1], "{\"entries\":0}");
    let copy = directory.file("copy.db");
    let ran = run_with(&["import", "--create", &copy, "-"], &export.stdout, pipe());
    assert_eq!(ran.exit, 0, "{}", stderr(&ran));
    assert_eq!(scan(&copy), b"");
    ok(&["check", &copy]);
}

#[test]
fn text_reports_read_like_the_other_commands() {
    let directory = TempDirectory::new("text-reports");
    let source = directory.file("app.db");
    database_with(&source, &[(b"a", b"1"), (b"b", b"2")]);
    let terminal = Terminal {
        stdout: true,
        stderr: true,
        ..Terminal::default()
    };
    let export = directory.file("out.jsonl");
    let ran = run_with(&["export", &source, &export], b"", terminal);
    assert_eq!(
        stdout(&ran),
        format!("exported 2 entries from {source} to {export}\n")
    );
    let copy = directory.file("copy.db");
    let ran = run_with(&["import", &copy, &export, "--create"], b"", terminal);
    assert_eq!(
        stdout(&ran),
        format!("created {copy} (4 KiB pages) and imported 2 entries\n")
    );
    let ran = run_with(
        &["import", &copy, &export, "--on-conflict", "skip"],
        b"",
        terminal,
    );
    assert_eq!(
        stdout(&ran),
        format!("imported no entries into {copy}; 2 skipped, their keys already there\n")
    );
    let ran = run_with(
        &[
            "import",
            &copy,
            &export,
            "--dry-run",
            "--on-conflict=overwrite",
        ],
        b"",
        terminal,
    );
    assert_eq!(
        stdout(&ran),
        format!("checked {export}: 2 entries would be imported into {copy}\n")
    );
    let to_stdout = run_with(&["export", &source], b"", terminal);
    assert_eq!(
        stderr(&to_stdout),
        format!("exported 2 entries from {source}\n")
    );
    let quiet = run_with(&["export", &source], b"", pipe());
    assert!(quiet.stderr.is_empty());
}

#[test]
fn conflicts_follow_the_policy_and_a_refused_file_changes_nothing() {
    let directory = TempDirectory::new("conflicts");
    let target = directory.file("app.db");
    database_with(&target, &[(b"a", b"old a"), (b"c", b"old c")]);
    let export = directory.file("in.jsonl");
    std::fs::write(&export, export_bytes(&[(b"a", b"new a"), (b"b", b"new b")])).unwrap();
    let before = scan(&target);
    let refused = run(&["import", &target, &export]);
    assert_eq!(refused.exit, 3);
    assert_eq!(
        stderr(&refused),
        format!(
            "eggdb: cannot import {export}: line 2: key a is already in {target}. Choose what happens to keys already there: --on-conflict overwrite or --on-conflict skip. Nothing was imported.\n"
        )
    );
    assert_eq!(scan(&target), before);
    ok(&["import", &target, &export, "--on-conflict", "skip"]);
    assert_eq!(ok(&["get", &target, "a"]).stdout, b"old a");
    assert_eq!(ok(&["get", &target, "b"]).stdout, b"new b");
    ok(&["import", &target, &export, "--on-conflict", "overwrite"]);
    assert_eq!(ok(&["get", &target, "a"]).stdout, b"new a");
    assert_eq!(ok(&["get", &target, "c"]).stdout, b"old c");
}

#[test]
fn a_key_twice_in_the_export_follows_the_policy() {
    let directory = TempDirectory::new("duplicates");
    let export = directory.file("dup.jsonl");
    std::fs::write(
        &export,
        export_bytes(&[(b"a", b"first"), (b"a", b"second"), (b"b", b"")]),
    )
    .unwrap();
    let new = directory.file("new.db");
    let refused = run(&["import", "--create", &new, &export]);
    assert_eq!(refused.exit, 3);
    assert!(
        stderr(&refused).contains(
            "line 3: key a appears twice, on lines 2 and 3. Keep the last with --on-conflict overwrite, or the first with --on-conflict skip. Nothing was imported and"
        ),
        "{}",
        stderr(&refused)
    );
    assert!(!exists(&new) && !exists(&staging(&new)));
    ok(&["import", "--create", &new, &export, "--on-conflict", "skip"]);
    assert_eq!(ok(&["get", &new, "a"]).stdout, b"first");
    let last = directory.file("last.db");
    let overwrote = ok(&[
        "import",
        "--create",
        &last,
        &export,
        "--on-conflict",
        "overwrite",
    ]);
    assert!(
        stdout(&overwrote).contains("\"entries\":3,\"imported\":2,\"skipped\":0,"),
        "{}",
        stdout(&overwrote)
    );
    assert_eq!(ok(&["get", &last, "a"]).stdout, b"second");
}

#[test]
fn keys_out_of_order_are_refused() {
    let directory = TempDirectory::new("order");
    let export = directory.file("unsorted.jsonl");
    std::fs::write(&export, export_bytes(&[(b"b", b""), (b"a", b"")])).unwrap();
    let new = directory.file("new.db");
    let refused = run(&["import", "--create", &new, &export]);
    assert_eq!(refused.exit, 3);
    assert!(
        stderr(&refused).contains("line 3: key a comes after b on line 2; an export lists its keys in ascending byte order"),
        "{}",
        stderr(&refused)
    );
}

#[test]
fn a_bad_line_from_stdin_stops_after_the_batches_before_it() {
    let directory = TempDirectory::new("stdin-partial");
    let target = directory.file("app.db");
    database_with(&target, &[(b"existing", b"1")]);
    let mut export = Vec::new();
    jsonl::write_header(&mut export, &Header::default()).unwrap();
    for number in 0..1500u32 {
        jsonl::write_entry(&mut export, format!("k{number:05}").as_bytes(), b"v").unwrap();
    }
    export.extend_from_slice(b"{\"key\":\"zz\",\"value\":\"\\q\"}\n");
    let ran = run_with(&["import", &target], &export, pipe());
    assert_eq!(ran.exit, 3);
    assert_eq!(
        stderr(&ran),
        "eggdb: cannot import stdin: line 1502: unknown escape \\q; JSON knows \\\" \\\\ \\/ \\b \\f \\n \\r \\t and \\uXXXX. The 1,024 entries before it were committed; after fixing the export, import it again with --on-conflict skip.\n"
    );
    let keys = ok(&["scan", &target, "-k", "-F", "json"]).stdout;
    assert_eq!(keys.iter().filter(|byte| **byte == b'\n').count(), 1025);
    ok(&["check", &target]);
}

#[test]
fn a_failed_import_into_a_new_file_leaves_nothing_behind() {
    let directory = TempDirectory::new("new-failed");
    let new = directory.file("new.db");
    let mut cut = export_bytes(&[(b"a", b"1"), (b"b", b"2")]);
    cut.truncate(cut.len() - "{\"entries\":2}\n".len());
    let ran = run_with(&["import", "--create", &new], &cut, pipe());
    assert_eq!(ran.exit, 3);
    assert_eq!(
        stderr(&ran),
        format!(
            "eggdb: cannot import stdin: line 3: the file ends after this line without the trailer line {{\"entries\":…}}: it is cut short. Nothing was imported and {new} was not created.\n"
        )
    );
    assert!(!exists(&new) && !exists(&staging(&new)));
    std::fs::write(staging(&new), b"left over").unwrap();
    let ran = run_with(&["import", "--create", &new], &export_bytes(&[]), pipe());
    assert_eq!(ran.exit, 3);
    assert!(stderr(&ran).contains(".importing exists, left by an import that did not finish"));
    assert_eq!(std::fs::read(staging(&new)).unwrap(), b"left over");
}

#[test]
fn long_keys_keep_the_exported_page_size() {
    let directory = TempDirectory::new("page-size");
    let source = directory.file("big-pages.db");
    ok(&["create", &source, "--page-size", "64k"]);
    let key = "k".repeat(4000);
    ok(&["put", &source, &key, "v"]);
    let export = directory.file("long.jsonl");
    ok(&["export", &source, &export]);
    let copy = directory.file("copy.db");
    ok(&["import", "--create", &copy, &export]);
    assert!(stdout(&ok(&["stats", &copy, "-F", "json"])).contains("\"page_size\":65536"));
    assert_eq!(scan(&copy), scan(&source));
    let small = directory.file("small.db");
    let refused = run(&["import", "--create", &small, &export, "--page-size", "4k"]);
    assert_eq!(refused.exit, 3);
    assert!(
        stderr(&refused).contains("line 2: key kkkk"),
        "{}",
        stderr(&refused)
    );
    assert!(
        stderr(&refused).contains("… (4,000 bytes) is 4,000 bytes; "),
        "{}",
        stderr(&refused)
    );
    assert!(!exists(&small));
    let existing = directory.file("existing.db");
    ok(&["create", &existing]);
    assert_eq!(run(&["import", &existing, &export]).exit, 3);
    let usage = run(&["import", &existing, &export, "--page-size", "8k"]);
    assert_eq!(usage.exit, 2);
    assert!(stderr(&usage).starts_with(&format!(
        "eggdb: --page-size is for a new file; {existing} exists and keeps its page size\n"
    )));
}

#[test]
fn a_key_of_a_mebibyte_is_refused_with_a_readable_error() {
    let directory = TempDirectory::new("huge-key");
    let mut export = Vec::new();
    jsonl::write_header(&mut export, &Header::default()).unwrap();
    export.extend_from_slice(b"{\"key\":\"");
    export.extend(std::iter::repeat_n(b'k', 1 << 20));
    export.extend_from_slice(b"\",\"value\":\"v\"}\n{\"entries\":1}\n");
    let new = directory.file("new.db");
    let ran = run_with(&["import", "--create", &new], &export, pipe());
    assert_eq!(ran.exit, 3);
    assert_eq!(
        stderr(&ran),
        format!(
            "eggdb: cannot import stdin: line 2: the key is longer than 8,192 bytes, the longest key any EggDB database stores. Nothing was imported and {new} was not created.\n"
        )
    );
    assert!(!exists(&new) && !exists(&staging(&new)));
}

#[test]
fn a_dry_run_writes_nothing() {
    let directory = TempDirectory::new("dry-run");
    let target = directory.file("app.db");
    database_with(&target, &[(b"a", b"1")]);
    let export = directory.file("in.jsonl");
    std::fs::write(&export, export_bytes(&[(b"a", b"2"), (b"b", b"3")])).unwrap();
    let before = std::fs::read(&target).unwrap();
    let ran = ok(&[
        "import",
        &target,
        &export,
        "--dry-run",
        "--on-conflict",
        "skip",
    ]);
    assert_eq!(
        stdout(&ran),
        format!(
            "{{\"file\":\"{}\",\"from\":\"{}\",\"entries\":2,\"imported\":1,\"skipped\":1,\"created\":false,\"dry_run\":true,\"collections_added\":0}}\n",
            json(&target),
            json(&export)
        )
    );
    assert_eq!(std::fs::read(&target).unwrap(), before);
    assert_eq!(run(&["import", &target, &export, "--dry-run"]).exit, 3);
    let new = directory.file("new.db");
    ok(&["import", "--create", &new, &export, "--dry-run"]);
    assert!(!exists(&new) && !exists(&staging(&new)));
}

#[test]
fn usage_and_file_errors_say_what_to_do() {
    let directory = TempDirectory::new("errors");
    let source = directory.file("app.db");
    database_with(&source, &[(b"a", b"1")]);
    let export = directory.file("out.jsonl");
    std::fs::write(&export, b"already here").unwrap();
    let refused = run(&["export", &source, &export]);
    assert_eq!(refused.exit, 3);
    assert_eq!(
        stderr(&refused),
        format!(
            "eggdb: {export} already exists; eggdb export never replaces a file. Choose another name, or remove it first.\n"
        )
    );
    assert_eq!(std::fs::read(&export).unwrap(), b"already here");
    let nowhere = directory.file("missing-directory/out.jsonl");
    let refused = run(&["export", &source, &nowhere]);
    assert_eq!(refused.exit, 3);
    assert!(stderr(&refused).starts_with(&format!("eggdb: cannot create {nowhere}: ")));

    let missing = directory.file("missing.db");
    let refused = run(&["import", &missing, &export]);
    assert_eq!(refused.exit, 3);
    assert_eq!(
        stderr(&refused),
        format!(
            "eggdb: cannot open {missing}: no such file. Pass --create to make it, or check the path.\n"
        )
    );
    let unreadable = directory.file("no-such-export.jsonl");
    let refused = run(&["import", &source, &unreadable]);
    assert_eq!(refused.exit, 2);
    assert!(stderr(&refused).starts_with(&format!("eggdb: cannot read {unreadable}: ")));

    let at_terminal = Terminal {
        stdin: true,
        ..Terminal::default()
    };
    let refused = run_with(&["import", &source], b"", at_terminal);
    assert_eq!(refused.exit, 2);
    assert!(stderr(&refused).starts_with(
        "eggdb: import needs an export to read: give its file, or pipe it to stdin\n  usage: eggdb import <file>"
    ));
    let refused = run(&["import", &source, &export, "--on-conflict", "replace"]);
    assert_eq!(refused.exit, 2);
    assert!(
        stderr(&refused)
            .starts_with("eggdb: --on-conflict must be error, overwrite or skip, not 'replace'\n")
    );
    for args in [
        &["export", &source, "-F", "hex"][..],
        &["export", &source, "--dry-run"],
        &["export", &source, &export, "extra"],
        &["scan", &source, "--on-conflict", "skip"],
        &["put", &source, "k", "v", "--dry-run"],
    ] {
        assert_eq!(run(args).exit, 2, "{args:?}");
    }
    let not_an_export = directory.file("not-an-export.db");
    std::fs::write(&not_an_export, b"{\"key\":\"a\",\"value\":\"b\"}\n").unwrap();
    let refused = run(&["import", &source, &not_an_export]);
    assert_eq!(refused.exit, 3);
    assert!(stderr(&refused).contains(
        "line 1: this is not an eggdb-jsonl export: its first line has no \"format\" field. Nothing was imported."
    ));
}

#[test]
fn a_closed_pipe_ends_the_export_quietly() {
    struct ClosedPipe;
    impl std::io::Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let directory = TempDirectory::new("closed-pipe");
    let source = directory.file("app.db");
    database_with(&source, &[(b"a", b"1")]);
    let args: Vec<std::ffi::OsString> = ["export", source.as_str()]
        .iter()
        .map(std::ffi::OsString::from)
        .collect();
    let mut stdin: &[u8] = b"";
    let mut stderr = Vec::new();
    let mut console = super::Console {
        stdin: &mut stdin,
        stdout: &mut ClosedPipe,
        stderr: &mut stderr,
        terminal: pipe(),
    };
    assert_eq!(super::run(&args, &mut console), 0);
    assert!(stderr.is_empty());
}

fn mutate(rng: &mut Rng, bytes: &mut Vec<u8>) {
    const TOKENS: &[&[u8]] = &[
        b"\"",
        b"\\",
        b"{",
        b"}",
        b",",
        b"\n",
        b"\xff",
        b"0",
        b"key_hex",
        b"\"entries\":",
    ];
    match rng.below(5) {
        0 if !bytes.is_empty() => {
            let at = rng.below(bytes.len());
            bytes[at] ^= 1 << rng.below(8);
        }
        1 => {
            let at = rng.below(bytes.len() + 1);
            bytes.truncate(at);
        }
        2 if !bytes.is_empty() => {
            let at = rng.below(bytes.len());
            let length = rng.below(12).min(bytes.len() - at);
            bytes.drain(at..at + length);
        }
        _ => {
            let at = rng.below(bytes.len() + 1);
            let token = TOKENS[rng.below(TOKENS.len())];
            bytes.splice(at..at, token.iter().copied());
        }
    }
}

#[test]
fn hostile_imports_exit_cleanly_and_leave_every_file_consistent() {
    let directory = TempDirectory::new("hostile-imports");
    let target = directory.file("target.db");
    database_with(&target, &[(b"m", b"middle")]);
    let export = directory.file("hostile.jsonl");
    let seed = std::env::var("EGGDB_FUZZ_SEED")
        .ok()
        .and_then(|seed| seed.parse::<u64>().ok())
        .unwrap_or(0x0dd5_eed5_0f1e_5501);
    let mut rng = Rng(seed | 1);
    let mut expected = scan(&target);
    let cases = std::env::var("EGGDB_FUZZ_CASES")
        .ok()
        .and_then(|cases| cases.parse::<u64>().ok())
        .map_or(150, |cases| cases / 20);
    for case in 0..cases {
        let entries: Vec<(Vec<u8>, Vec<u8>)> = {
            let mut keys: Vec<Vec<u8>> = (0..rng.below(6))
                .map(|_| {
                    (0..rng.below(6))
                        .map(|_| b'a' + rng.below(26) as u8)
                        .collect()
                })
                .collect();
            keys.sort();
            keys.dedup();
            keys.into_iter()
                .map(|key| (key, (0..rng.below(40)).map(|_| rng.next() as u8).collect()))
                .collect()
        };
        let borrowed: Vec<(&[u8], &[u8])> = entries
            .iter()
            .map(|(key, value)| (key.as_slice(), value.as_slice()))
            .collect();
        let mut bytes = export_bytes(&borrowed);
        for _ in 0..1 + rng.below(3) {
            mutate(&mut rng, &mut bytes);
        }
        std::fs::write(&export, &bytes).unwrap();
        let new = directory.file(&format!("new-{case}.db"));
        let policy = ["error", "overwrite", "skip"][rng.below(3)];
        let created = run(&["import", "--create", &new, &export, "--on-conflict", policy]);
        assert!(
            [0, 3].contains(&created.exit),
            "seed {seed} case {case}: exit {}: {}",
            created.exit,
            stderr(&created)
        );
        assert!(!exists(&staging(&new)), "case {case}");
        if created.exit == 0 {
            ok(&["check", &new]);
            std::fs::remove_file(&new).unwrap();
        } else {
            assert!(!exists(&new), "case {case}");
        }
        let into_existing = if rng.below(2) == 0 {
            run(&["import", &target, &export, "--on-conflict", policy])
        } else {
            run_with(
                &["import", &target, "--on-conflict", policy],
                &bytes,
                pipe(),
            )
        };
        assert!(
            [0, 3].contains(&into_existing.exit),
            "seed {seed} case {case}: exit {}: {}",
            into_existing.exit,
            stderr(&into_existing)
        );
        ok(&["check", &target]);
        let now = scan(&target);
        if into_existing.exit != 0 && stderr(&into_existing).contains("Nothing was imported") {
            assert_eq!(now, expected, "case {case}: {}", stderr(&into_existing));
        }
        expected = now;
    }
    assert!(Path::new(&target).exists());
}

#[cfg(unix)]
fn run_from_fifo(directory: &TempDirectory, args: &[&str], export: &[u8]) -> Ran {
    use std::sync::mpsc;
    use std::time::Duration;
    let fifo = directory.file("export.fifo");
    let _ = std::fs::remove_file(&fifo);
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    let writer_path = fifo.clone();
    let bytes = export.to_vec();
    std::thread::spawn(move || {
        if let Ok(mut pipe) = std::fs::OpenOptions::new().write(true).open(writer_path) {
            let _ = std::io::Write::write_all(&mut pipe, &bytes);
        }
    });
    let mut owned: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    owned.push(fifo);
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        let args: Vec<&str> = owned.iter().map(String::as_str).collect();
        let _ = done.send(run(&args));
    });
    finished
        .recv_timeout(Duration::from_secs(60))
        .expect("import from a named pipe hung")
}

#[cfg(unix)]
#[test]
fn a_named_pipe_given_as_the_export_is_read_once_without_a_hang() {
    let directory = TempDirectory::new("fifo");
    let export = export_bytes(&[(b"a", b"1"), (b"b", b"2"), (b"c", b"3")]);
    let new = directory.file("new.db");
    let created = run_from_fifo(&directory, &["import", "--create", &new], &export);
    assert_eq!(created.exit, 0, "{}", stderr(&created));
    assert!(stdout(&created).contains("\"entries\":3,\"imported\":3,"));
    assert_eq!(ok(&["get", &new, "c"]).stdout, b"3");
    let checked = run_from_fifo(&directory, &["import", "--dry-run", &new], &export);
    assert_eq!(checked.exit, 3, "{}", stderr(&checked));
    assert!(stderr(&checked).contains("key a is already in"));
    let overwritten = run_from_fifo(
        &directory,
        &["import", &new, "--on-conflict", "overwrite"],
        &export_bytes(&[(b"a", b"new")]),
    );
    assert_eq!(overwritten.exit, 0, "{}", stderr(&overwritten));
    assert_eq!(ok(&["get", &new, "a"]).stdout, b"new");
    let mut cut = export_bytes(&[(b"d", b"4")]);
    cut.truncate(cut.len() - 3);
    let refused = run_from_fifo(&directory, &["import", &new], &cut);
    assert_eq!(refused.exit, 3);
    assert!(
        stderr(&refused).contains("Nothing was imported."),
        "{}",
        stderr(&refused)
    );
    assert_eq!(run(&["get", &new, "d"]).exit, 1);
}

#[cfg(unix)]
#[test]
fn a_dangling_symbolic_link_as_the_target_is_named_as_such() {
    let directory = TempDirectory::new("dangling");
    let link = directory.file("link.db");
    std::os::unix::fs::symlink(directory.file("missing.db"), &link).unwrap();
    let export = directory.file("in.jsonl");
    std::fs::write(&export, export_bytes(&[(b"a", b"1")])).unwrap();
    let refused = run(&["import", "--create", &link, &export]);
    assert_eq!(refused.exit, 3);
    assert_eq!(
        stderr(&refused),
        format!(
            "eggdb: cannot import into {link}: it is a symbolic link to a file that does not exist. Import into the path it should name, or remove the link.\n"
        )
    );
}
