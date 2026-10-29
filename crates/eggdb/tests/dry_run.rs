//! Dry runs (D145): `eggdb restore --dry-run` and `eggdb import --csv --dry-run` run the real
//! restore or load on a scratch copy of the file and throw the copy away, so their report is
//! exactly a real run's and the file, its sequences included, does not change.

mod common;

use common::TempPath;
use eggdb_sql::json::{Json, Node, parse_document};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn eggdb(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[track_caller]
fn succeeded(output: &Output) -> String {
    assert!(output.status.success(), "{}", text(&output.stderr));
    text(&output.stdout)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

struct Removed(Vec<PathBuf>);

impl Drop for Removed {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn members_without(node: &Node, left_out: &[&str]) -> Vec<(String, String)> {
    let Json::Object(members) = &node.value else {
        panic!("{node:?}");
    };
    members
        .iter()
        .filter(|(name, _)| !left_out.contains(&name.as_str()))
        .map(|(name, value)| (name.clone(), format!("{:?}", value.value)))
        .collect()
}

fn names(node: &Node) -> Vec<String> {
    let Json::Object(members) = &node.value else {
        panic!("{node:?}");
    };
    members.iter().map(|(name, _)| name.clone()).collect()
}

const SCHEMA: &str = "CREATE TABLE people (id serial PRIMARY KEY, name text NOT NULL); \
                      INSERT INTO people (name) VALUES ('Ada')";

const DUMP: &str = "CREATE TABLE places (id integer PRIMARY KEY, name text);\n\
                    INSERT INTO places VALUES (1, 'Oslo'), (2, 'Lima');\n\
                    INSERT INTO people (name) VALUES ('Grace');\n";

#[test]
fn restore_dry_run_reports_what_a_real_run_does_and_changes_nothing() {
    let file = TempPath::new("dry-run-restore");
    let copy = TempPath::new("dry-run-restore-copy");
    let dump = TempPath::new("dry-run-restore-dump");
    let path = file.path().to_str().unwrap().to_owned();
    let copy_path = copy.path().to_str().unwrap().to_owned();
    let dump_path = dump.path().to_str().unwrap().to_owned();
    let _left = Removed(vec![with_suffix(file.path(), ".restoring")]);
    succeeded(&eggdb(&["sql", "--create", &path, "-c", SCHEMA]));
    std::fs::write(dump.path(), DUMP).unwrap();
    std::fs::copy(file.path(), copy.path()).unwrap();
    let before = std::fs::read(file.path()).unwrap();

    let dry = eggdb(&["restore", &path, &dump_path, "--dry-run", "--json"]);
    let dry_report = parse_document(succeeded(&dry).trim_end()).unwrap();
    let real = eggdb(&["restore", &copy_path, &dump_path, "--json"]);
    let real_report = parse_document(succeeded(&real).trim_end()).unwrap();

    assert_eq!(std::fs::read(file.path()).unwrap(), before, "the file is not touched");
    assert!(!with_suffix(file.path(), ".restoring").exists(), "the scratch copy is removed");
    let dry_names = names(&dry_report);
    let created_at = dry_names.iter().position(|name| name == "created").unwrap();
    assert_eq!(dry_names[created_at + 1], "dry_run");
    assert!(!names(&real_report).contains(&"dry_run".to_owned()), "a real run's report is as shipped");
    let left_out = ["file", "elapsed_ms", "dry_run", "verdict"];
    assert_eq!(
        members_without(&dry_report, &left_out),
        members_without(&real_report, &left_out),
        "the dry run reports exactly what the real run did"
    );
    let rows = succeeded(&eggdb(&["sql", &path, "-c", "SELECT count(*) FROM people"]));
    assert!(rows.contains(" 1\n"), "{rows}");
    let next = succeeded(&eggdb(&[
        "sql",
        &path,
        "-c",
        "INSERT INTO people (name) VALUES ('Linus') RETURNING id",
    ]));
    assert!(next.contains(" 2\n"), "the sequence did not move: {next}");

    let as_text = eggdb(&["restore", &path, &dump_path, "--dry-run"]);
    let shown = succeeded(&as_text);
    assert!(
        shown.starts_with(&format!(
            "restored {dump_path} into a copy of {path}, then removed the copy ("
        )),
        "{shown}"
    );
    assert!(shown.contains(&format!("would be in {path}")), "{shown}");
}

#[test]
fn restore_dry_run_with_create_leaves_no_file_and_a_failure_says_what_a_real_run_would_do() {
    let file = TempPath::new("dry-run-create");
    let dump = TempPath::new("dry-run-create-dump");
    let path = file.path().to_str().unwrap().to_owned();
    let dump_path = dump.path().to_str().unwrap().to_owned();
    let staging = with_suffix(file.path(), ".restoring");
    let _left = Removed(vec![staging.clone()]);
    std::fs::write(
        dump.path(),
        "CREATE TABLE places (id integer PRIMARY KEY);\nINSERT INTO places VALUES (1);\n",
    )
    .unwrap();
    let created = eggdb(&["restore", "--create", &path, &dump_path, "--dry-run", "--json"]);
    let report = parse_document(succeeded(&created).trim_end()).unwrap();
    assert!(!file.path().exists() && !staging.exists());
    let facts = members_without(&report, &[]);
    assert!(facts.contains(&("created".to_owned(), "Boolean(true)".to_owned())), "{facts:?}");
    assert!(facts.contains(&("dry_run".to_owned(), "Boolean(true)".to_owned())), "{facts:?}");

    let missing = eggdb(&["restore", &path, &dump_path, "--dry-run"]);
    assert_eq!(missing.status.code(), Some(3));
    assert!(
        text(&missing.stderr).contains(&format!(
            "  try: eggdb restore --create {path} {dump_path} --dry-run\n"
        )),
        "{}",
        text(&missing.stderr)
    );

    succeeded(&eggdb(&["sql", "--create", &path, "-c", "CREATE TABLE places (id integer PRIMARY KEY)"]));
    let before = std::fs::read(file.path()).unwrap();
    std::fs::write(
        dump.path(),
        "INSERT INTO places VALUES (1);\nINSERT INTO nowhere VALUES (2);\n",
    )
    .unwrap();
    let failed = eggdb(&["restore", &path, &dump_path, "--dry-run"]);
    assert_eq!(failed.status.code(), Some(3));
    let said = text(&failed.stderr);
    assert!(
        said.contains(&format!("cannot restore {dump_path} into {path} (dry run): line 2: ")),
        "{said}"
    );
    assert!(
        said.contains(&format!(
            "A real run would leave the 1 statement before it in {path} and run nothing after it. Fix the dump"
        )),
        "{said}"
    );
    assert_eq!(std::fs::read(file.path()).unwrap(), before);
    assert!(!staging.exists());

    std::fs::write(&staging, b"left by a crash").unwrap();
    let blocked = eggdb(&["restore", &path, &dump_path, "--dry-run", "--json"]);
    assert_eq!(blocked.status.code(), Some(3));
    let error = text(&blocked.stderr);
    assert!(error.contains("\"code\":\"already_exists\""), "{error}");
    assert!(error.contains("for the dry run: "), "{error}");
}

#[test]
fn import_csv_dry_run_loads_into_a_copy_and_reports_the_rows() {
    let file = TempPath::new("dry-run-csv");
    let csv = TempPath::new("dry-run-csv-input");
    let path = file.path().to_str().unwrap().to_owned();
    let csv_path = csv.path().to_str().unwrap().to_owned();
    let staging = with_suffix(file.path(), ".importing");
    let _left = Removed(vec![staging.clone()]);
    succeeded(&eggdb(&["sql", "--create", &path, "-c", SCHEMA]));
    std::fs::write(csv.path(), "name\nGrace\nLinus\n").unwrap();
    let before = std::fs::read(file.path()).unwrap();

    let dry = eggdb(&[
        "import", &path, "--csv", &csv_path, "--table", "people (name)", "--header", "--dry-run",
        "--json",
    ]);
    assert_eq!(
        succeeded(&dry),
        format!(
            "{{\"file\":\"{path}\",\"from\":\"{csv_path}\",\"table\":\"people (name)\",\"rows\":2,\"dry_run\":true}}\n"
        )
    );
    assert_eq!(std::fs::read(file.path()).unwrap(), before);
    assert!(!staging.exists());
    let as_text = eggdb(&[
        "import", &path, "--csv", &csv_path, "--table", "people (name)", "--header", "--dry-run",
    ]);
    assert_eq!(
        succeeded(&as_text),
        format!("checked {csv_path}: 2 rows would be imported into people (name) in {path}\n")
    );

    std::fs::write(csv.path(), "name\nGrace\nAda,extra\n").unwrap();
    let bad = eggdb(&[
        "import", &path, "--csv", &csv_path, "--table", "people (name)", "--header", "--dry-run",
    ]);
    assert_eq!(bad.status.code(), Some(3));
    assert!(
        text(&bad.stderr).contains("Nothing was imported (dry run); a real run would stop at this row too, since the rows load in one transaction.\n"),
        "{}",
        text(&bad.stderr)
    );
    assert_eq!(std::fs::read(file.path()).unwrap(), before);
    assert!(!staging.exists());
    let next = succeeded(&eggdb(&[
        "sql",
        &path,
        "-c",
        "INSERT INTO people (name) VALUES ('Linus') RETURNING id",
    ]));
    assert!(next.contains(" 2\n"), "the dry runs moved no sequence: {next}");

    let refused = eggdb(&[
        "import", &path, "--csv", &csv_path, "--table", "people", "--on-conflict", "skip",
    ]);
    assert_eq!(refused.status.code(), Some(2));
    assert!(text(&refused.stderr).starts_with(
        "eggdb: --on-conflict and --page-size are for eggdb-jsonl imports, not --csv\n"
    ));
}
