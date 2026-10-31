//! `eggdb restore` as a real process: `pg_dump` fixtures taken from PostgreSQL 16 and 18 in
//! every plain form restore and match PostgreSQL's own output table for table, archives are
//! refused, a failed or killed restore with `--create` leaves no file, and the report.

use eggdb::Database;
use eggdb::sql::{CopyChannel, CopyStart, Dialect, SqlResult};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

static NEXT_DIRECTORY: AtomicU32 = AtomicU32::new(0);

const FIXTURES: [&str; 5] = [
    "pg16.sql",
    "pg16-inserts.sql",
    "pg16-column-inserts.sql",
    "pg18.sql",
    "pg18-custom.sql",
];

struct TempDirectory {
    path: PathBuf,
}

impl TempDirectory {
    fn new(name: &str) -> Self {
        let number = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "eggdb-restore-test-{name}-{}-{number}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/pg_dump")
        .join(name)
}

fn eggdb(args: &[&dyn AsRef<std::ffi::OsStr>], stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .args(args.iter().map(|arg| arg.as_ref()))
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let stdin = stdin.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = input.write_all(&stdin);
    });
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    output
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[derive(Default)]
struct Collect(Vec<u8>);

impl CopyChannel for Collect {
    fn start_copy_in(&mut self, _start: &CopyStart) -> SqlResult<()> {
        Ok(())
    }

    fn read_copy_data(&mut self, _data: &mut Vec<u8>) -> SqlResult<bool> {
        Ok(false)
    }

    fn start_copy_out(&mut self, _start: &CopyStart) -> SqlResult<()> {
        Ok(())
    }

    fn write_copy_data(&mut self, data: &[u8]) -> SqlResult<()> {
        self.0.extend_from_slice(data);
        Ok(())
    }

    fn finish_copy_out(&mut self) -> SqlResult<()> {
        Ok(())
    }
}

fn expected_tables() -> Vec<(String, Vec<u8>)> {
    let expected = std::fs::read(fixture("expected.txt")).unwrap();
    let mut sections: Vec<(String, Vec<u8>)> = Vec::new();
    for line in expected.split_inclusive(|&byte| byte == b'\n') {
        if let Some(query) = line.strip_prefix(b"== ") {
            sections.push((text(query).trim_end().to_owned(), Vec::new()));
        } else if let Some((_, rows)) = sections.last_mut() {
            rows.extend_from_slice(line);
        }
    }
    sections
}

fn copied_out(db: &Database, query: &str) -> Vec<u8> {
    let store = db.sql_store();
    let mut session = store.session(Dialect::PostgreSql);
    let mut collect = Collect::default();
    session
        .execute_with_copy(query, &mut |_| true, &mut collect)
        .unwrap_or_else(|error| panic!("{query}: {error}"));
    session.close().unwrap();
    collect.0
}

#[test]
fn every_committed_pg_dump_fixture_restores_and_matches_postgresql_table_for_table() {
    let expected = expected_tables();
    assert_eq!(
        expected.len(),
        20,
        "the fixture has thirteen tables EggDB restores and seven sequences"
    );
    let directory = TempDirectory::new("fixtures");
    for name in FIXTURES {
        let target = directory.join(&format!("{name}.db"));
        let output = eggdb(
            &[
                &"restore",
                &"--create",
                &target,
                &fixture(name),
                &"--format",
                &"json",
            ],
            b"",
        );
        let report = text(&output.stdout);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{name}: {report}{}",
            text(&output.stderr)
        );
        assert!(report.starts_with("{\"ok\":true,"), "{name}: {report}");
        assert!(
            report.contains("\"kind\":\"materialized view\",\"name\":\"public.daily_orders\""),
            "{name}: {report}"
        );
        assert!(!report.contains("touch_note"), "{name}: {report}");
        assert!(!report.contains("customers_touch"), "{name}: {report}");
        assert!(report.contains("\"tables\":13,"), "{name}: {report}");
        assert!(report.contains("\"views\":3,"), "{name}: {report}");
        assert!(report.contains("\"tables_skipped\":0,"), "{name}: {report}");
        assert!(!report.contains("public.mood"), "{name}: {report}");
        assert!(!report.contains("public.positive_amount"), "{name}: {report}");
        let db = Database::open(&target).unwrap();
        for (query, rows) in &expected {
            let found = copied_out(&db, query);
            assert!(
                found == *rows,
                "{name}: {query} differs:\nexpected:\n{}\nfound:\n{}",
                text(rows),
                text(&found)
            );
        }
        let store = db.sql_store();
        let mut session = store.session(Dialect::PostgreSql);
        session.check_indexes().unwrap();
        let mut ids = Vec::new();
        session
            .execute(
                "INSERT INTO customers (name) VALUES ('new') RETURNING id; INSERT INTO products (sku, title, price) VALUES (gen_random_uuid(), 'new', 1) RETURNING id",
                &mut |result| ids.extend(result.rows.into_iter().map(|row| row[0].to_text())),
            )
            .unwrap();
        assert!(
            ids.iter().all(|id| id.as_deref() != Some("1")),
            "{name}: the sequences continue after the restored rows: {ids:?}"
        );
        let mut notes = Vec::new();
        session
            .execute(
                "UPDATE customers SET note = 'x' WHERE id = 1; SELECT note FROM customers WHERE id = 1",
                &mut |result| notes.extend(result.rows.into_iter().map(|row| row[0].to_text())),
            )
            .unwrap();
        assert_eq!(
            notes,
            [Some("x;".to_owned())],
            "{name}: the restored trigger runs its PL/pgSQL function"
        );
        session.close().unwrap();
        assert_eq!(db.check().unwrap().leaked_pages, 0);
        db.close().unwrap();
    }
}

#[test]
fn archives_are_refused_with_the_pg_restore_command_before_any_file_is_made() {
    let directory = TempDirectory::new("archives");
    let custom = directory.join("app.dump");
    std::fs::write(
        &custom,
        b"PGDMP\x01\x0e\x00\x04\x08\x01\x01\x01\x00\x00\x00",
    )
    .unwrap();
    let target = directory.join("app.db");
    let output = eggdb(&[&"restore", &"--create", &target, &custom], b"");
    assert_eq!(output.status.code(), Some(2), "{}", text(&output.stderr));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("custom-format archive"), "{stderr}");
    assert!(stderr.contains("docs/migrate.md"), "{stderr}");
    assert!(
        stderr.contains(&format!("try: pg_restore -f dump.sql {}", custom.display())),
        "{stderr}"
    );
    assert!(!target.exists());
    let output = eggdb(
        &[&"restore", &"--create", &target, &"-"],
        b"PGDMP\x01\x0e\x00",
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("pg_restore -f dump.sql <archive>"));
    let archive_directory = directory.join("backup");
    std::fs::create_dir_all(&archive_directory).unwrap();
    std::fs::write(archive_directory.join("toc.dat"), b"PGDMP").unwrap();
    let output = eggdb(&[&"restore", &"--create", &target, &archive_directory], b"");
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("directory-format archive"));
    let mut tar = vec![0u8; 1024];
    tar[..8].copy_from_slice(b"toc.dat\0");
    tar[257..262].copy_from_slice(b"ustar");
    let tar_file = directory.join("app.tar");
    std::fs::write(&tar_file, &tar).unwrap();
    let output = eggdb(&[&"restore", &"--create", &target, &tar_file], b"");
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("tar-format archive"));
    let output = eggdb(
        &[&"restore", &"--create", &target, &"-"],
        &[0x1f, 0x8b, 8, 0, 0, 0],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("gzip -dc"));
    assert!(!target.exists());
    assert!(!directory.join("app.db.restoring").exists());
}

const BROKEN: &str = "CREATE TABLE public.kept (id integer PRIMARY KEY);\nINSERT INTO public.kept VALUES (1);\nCREATE TABLE public.t (id integer PRIMARY KEY);\nCOPY public.t (id) FROM stdin;\n1\n1\n\\.\nCREATE TABLE public.after (id integer);\n";

#[test]
fn an_error_with_create_leaves_no_file_and_names_the_line() {
    let directory = TempDirectory::new("create-error");
    let target = directory.join("app.db");
    let output = eggdb(
        &[&"restore", &"--create", &target, &"-", &"--format", &"text"],
        BROKEN.as_bytes(),
    );
    assert_eq!(output.status.code(), Some(3));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("cannot restore stdin into"), "{stderr}");
    assert!(stderr.contains("line 6: duplicate key value"), "{stderr}");
    assert!(stderr.contains("Nothing was created"), "{stderr}");
    assert!(
        stderr.contains("DETAIL:  Key (id)=(1) already exists."),
        "{stderr}"
    );
    let stdout = text(&output.stdout);
    assert!(stdout.contains("failed: line 6:"), "{stdout}");
    assert!(!target.exists());
    assert!(!directory.join("app.db.restoring").exists());
}

#[test]
fn an_error_in_an_existing_file_keeps_the_statements_before_it() {
    let directory = TempDirectory::new("existing-error");
    let target = directory.join("app.db");
    Database::open(&target).unwrap().close().unwrap();
    let dump = directory.join("broken.sql");
    std::fs::write(&dump, BROKEN).unwrap();
    let output = eggdb(&[&"restore", &target, &dump], b"");
    assert_eq!(output.status.code(), Some(3));
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("The 3 statements before it are in"),
        "{stderr}"
    );
    let db = Database::open(&target).unwrap();
    assert_eq!(copied_out(&db, "COPY kept TO STDOUT"), b"1\n");
    assert_eq!(copied_out(&db, "COPY t TO STDOUT"), b"");
    db.close().unwrap();
}

#[test]
fn keep_going_restores_the_rest_lists_the_errors_and_exits_3() {
    let directory = TempDirectory::new("keep-going");
    let target = directory.join("app.db");
    let report = directory.join("report.json");
    let output = eggdb(
        &[
            &"restore",
            &"--create",
            &target,
            &"-",
            &"--keep-going",
            &"--report",
            &report,
        ],
        BROKEN.as_bytes(),
    );
    assert_eq!(output.status.code(), Some(3));
    let stdout = text(&output.stdout);
    assert!(
        stdout.starts_with("errors ignored: 1 statement failed"),
        "{stdout}"
    );
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(text(&output.stderr).contains("with 1 error ignored (--keep-going)"));
    let json = std::fs::read_to_string(&report).unwrap();
    assert!(json.starts_with("{\"ok\":false,"), "{json}");
    assert!(
        json.contains("\"errors\":[{\"line\":6,\"statement\":\"COPY public.t (id) FROM stdin;\""),
        "{json}"
    );
    let db = Database::open(&target).unwrap();
    assert_eq!(copied_out(&db, "COPY kept TO STDOUT"), b"1\n");
    assert_eq!(copied_out(&db, "COPY after TO STDOUT"), b"");
    db.close().unwrap();
}

#[test]
fn files_the_restore_cannot_use_are_refused() {
    let directory = TempDirectory::new("refused");
    let target = directory.join("app.db");
    let output = eggdb(&[&"restore", &target, &"-"], b"SELECT 1;\n");
    assert_eq!(output.status.code(), Some(3));
    assert!(text(&output.stderr).contains("Pass --create to restore into a new file."));
    assert!(text(&output.stderr).contains("try: eggdb restore --create"));
    std::fs::write(directory.join("app.db.restoring"), b"left over").unwrap();
    let output = eggdb(&[&"restore", &"--create", &target, &"-"], b"SELECT 1;\n");
    assert_eq!(output.status.code(), Some(3));
    assert!(
        text(&output.stderr)
            .contains("app.db.restoring exists, left by a restore that did not finish")
    );
    assert_eq!(
        std::fs::read(directory.join("app.db.restoring")).unwrap(),
        b"left over"
    );
    let output = eggdb(
        &[
            &"restore",
            &"--create",
            &target,
            &"-",
            &"--dialect",
            &"sqlite",
        ],
        b"SELECT 1;\n",
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(
        text(&output.stderr).contains("--dialect must be postgresql, sqlserver, oracle or mysql"),
        "{}",
        text(&output.stderr)
    );
    let output = eggdb(
        &[
            &"restore",
            &"--create",
            &target,
            &"-",
            &"--dialect",
            &"postgresql",
            &"--format",
            &"text",
        ],
        b"SELECT 1;\n",
    );
    assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
}

#[test]
fn the_text_report_counts_lists_and_gives_a_verdict() {
    let directory = TempDirectory::new("text-report");
    let target = directory.join("app.db");
    let dump = "\\connect app\nCREATE SCHEMA app;\nCREATE TABLE app.t (id integer PRIMARY KEY, note text);\nCOMMENT ON TABLE app.t IS 'x';\nCOPY app.t (id, note) FROM stdin;\n1\tone\n2\ttwo\n\\.\nCREATE FUNCTION f() RETURNS int LANGUAGE c AS 'lib', 'f';\nALTER TABLE app.t OWNER TO someone;\nGRANT ALL ON TABLE app.t TO someone;\nSET default_toast_compression = 'lz4';\n";
    let output = eggdb(
        &[&"restore", &"--create", &target, &"-", &"--format", &"text"],
        dump.as_bytes(),
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let report = text(&output.stdout);
    for line in [
        "restored stdin into ",
        "  schemas         1\n",
        "  tables          1, 2 rows\n",
        "  comments        1\n",
        "  skipped         1 (function 1)\n",
        "  ignored         4 (OWNER TO 1, GRANT 1, SET 1, meta-commands 1)\n",
        "rows per table\n  app.t  2\n",
        "skipped (not in EggDB yet; docs/roadmap.md)\n  line 9  function f()   routines in languages other than SQL and PL/pgSQL come later\n",
        "  OWNER TO    1 statement (EggDB has no roles yet)\n",
        "  \\connect    line 1 (one database per file)\n",
        "ok: every table and row of the dump is in ",
    ] {
        assert!(report.contains(line), "{line:?} in\n{report}");
    }
}

#[test]
fn a_killed_restore_with_create_leaves_no_file_and_a_consistent_staging_file() {
    const ROUNDS: usize = 200;
    const ROWS: usize = 50;
    let mut dump =
        String::from("CREATE TABLE public.c (round integer, n integer, PRIMARY KEY (round, n));\n");
    for round in 0..ROUNDS {
        dump.push_str("COPY public.c (round, n) FROM stdin;\n");
        for n in 0..ROWS {
            dump.push_str(&format!("{round}\t{n}\n"));
        }
        dump.push_str("\\.\n");
    }
    for delay_ms in [0u64, 30, 120, 400, 1_000] {
        let directory = TempDirectory::new("killed");
        let target = directory.join("app.db");
        let staging = directory.join("app.db.restoring");
        let mut child = Command::new(env!("CARGO_BIN_EXE_eggdb"))
            .arg("restore")
            .arg("--create")
            .arg(&target)
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let bytes = dump.clone().into_bytes();
        let writer = std::thread::spawn(move || {
            let _ = input.write_all(&bytes);
        });
        std::thread::sleep(Duration::from_millis(delay_ms));
        let _ = child.kill();
        let status = child.wait().unwrap();
        let _ = writer.join();
        if target.exists() {
            assert!(status.success() || !staging.exists());
            let db = Database::open(&target).unwrap();
            assert_eq!(
                copied_out(&db, "COPY (SELECT count(*) FROM c) TO STDOUT"),
                format!("{}\n", ROUNDS * ROWS).as_bytes(),
                "after {delay_ms} ms the restored file is whole"
            );
            db.close().unwrap();
            continue;
        }
        if staging.exists() {
            if let Ok(db) = Database::open(&staging) {
                let store = db.sql_store();
                let mut session = store.session(Dialect::PostgreSql);
                let mut counts = Vec::new();
                if session
                    .execute(
                        "SELECT round, count(*) FROM c GROUP BY round ORDER BY round",
                        &mut |result| counts.extend(result.rows),
                    )
                    .is_ok()
                {
                    for (round, row) in counts.iter().enumerate() {
                        assert_eq!(row[0].to_text(), Some(round.to_string()));
                        assert_eq!(row[1].to_text(), Some(ROWS.to_string()));
                    }
                }
                session.close().unwrap();
                assert_eq!(db.check().unwrap().leaked_pages, 0);
                db.close().unwrap();
            }
            let output = eggdb(&[&"restore", &"--create", &target, &"-"], dump.as_bytes());
            assert_eq!(
                output.status.code(),
                Some(3),
                "a leftover staging file is refused"
            );
        }
    }
}
