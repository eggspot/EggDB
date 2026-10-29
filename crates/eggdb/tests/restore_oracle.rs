//! `eggdb restore --dialect oracle` as a real process: Oracle's HR sample scripts run unchanged and
//! their tables and rows match the documented counts; SQL Developer, `impdp SQLFILE=` and SQLcl
//! CSV fixtures restore; SQL*Plus commands, substitution variables, includes, PL/SQL blocks,
//! disabled constraints, routes and errors behave as the plan states. Expected values come from
//! the scripts' own data and Oracle's documented semantics, never from running EggDB.

use eggdb::Database;
use eggdb::sql::{Dialect, Value};
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT_DIRECTORY: AtomicU32 = AtomicU32::new(0);

const HR_COUNTS: [(&str, &str); 7] = [
    ("regions", "5"),
    ("countries", "25"),
    ("locations", "23"),
    ("departments", "27"),
    ("jobs", "19"),
    ("employees", "107"),
    ("job_history", "10"),
];

struct TempDirectory {
    path: PathBuf,
}

impl TempDirectory {
    fn new(name: &str) -> Self {
        let number = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "eggdb-restore-oracle-{name}-{}-{number}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/oracle")
        .join(name)
}

fn eggdb_in(cwd: Option<&Path>, args: &[&dyn AsRef<OsStr>], stdin: &[u8]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_eggdb"));
    command
        .args(args.iter().map(|arg| arg.as_ref()))
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().unwrap();
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

fn everything(output: &Output) -> String {
    format!("{}{}", text(&output.stdout), text(&output.stderr))
}

fn restore(cwd: Option<&Path>, target: &Path, script: &Path, extra: &[&str]) -> Output {
    let mut args: Vec<&dyn AsRef<OsStr>> = vec![&"restore", &"--dialect", &"oracle"];
    for argument in extra {
        args.push(argument);
    }
    args.push(&target);
    args.push(&script);
    args.push(&"--format");
    args.push(&"json");
    eggdb_in(cwd, &args, b"")
}

fn restore_script(directory: &TempDirectory, script: &str, extra: &[&str]) -> (Output, PathBuf) {
    let path = directory.write("script.sql", script);
    let target = directory.join("restored.db");
    let mut arguments = vec!["--create"];
    arguments.extend_from_slice(extra);
    (restore(None, &target, &path, &arguments), target)
}

fn shown(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        value => value.to_text().unwrap_or_default(),
    }
}

fn query(db: &Database, dialect: Dialect, sql: &str) -> String {
    let store = db.sql_store();
    let mut session = store.session(dialect);
    let mut out = String::new();
    session
        .execute(sql, &mut |result| {
            for row in &result.rows {
                let values: Vec<String> = row.iter().map(shown).collect();
                out.push_str(&values.join("|"));
                out.push('\n');
            }
        })
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    session.close().unwrap();
    out
}

fn oracle_error(db: &Database, sql: &str) -> Option<u16> {
    let store = db.sql_store();
    let mut session = store.session(Dialect::Oracle);
    let error = session.execute(sql, &mut |_| {}).err();
    let _ = session.close();
    error.and_then(|error| error.vendor_code())
}

fn assert_hr_counts(db: &Database, schema: &str) {
    for (table, rows) in HR_COUNTS {
        let oracle = query(
            db,
            Dialect::Oracle,
            &format!("ALTER SESSION SET CURRENT_SCHEMA = {schema}; SELECT COUNT(*) FROM {table}"),
        );
        assert_eq!(oracle, format!("{rows}\n"), "{schema}.{table} through Oracle SQL");
        let qualified = if schema.eq_ignore_ascii_case("public") {
            table.to_owned()
        } else {
            format!("{}.{table}", schema.to_lowercase())
        };
        let postgresql = query(
            db,
            Dialect::PostgreSql,
            &format!("SELECT count(*) FROM {qualified}"),
        );
        assert_eq!(postgresql, format!("{rows}\n"), "{qualified} through PostgreSQL SQL");
    }
}

fn verification_query() -> String {
    let install = std::fs::read_to_string(fixture("hr/hr_install.sql")).unwrap();
    let start = install.find("SELECT 'regions'").unwrap();
    let end = start + install[start..].find(';').unwrap();
    install[start..end].to_owned()
}

#[test]
fn the_hr_installer_restores_unchanged_from_another_directory() {
    let directory = TempDirectory::new("hr");
    let target = directory.join("hr.db");
    let output = restore(
        Some(&directory.path),
        &target,
        &fixture("hr/hr_install.sql"),
        &["--create"],
    );
    let report = text(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}\n{report}",
        text(&output.stderr)
    );
    for part in [
        "\"ok\":true",
        "\"dialect\":\"oracle\"",
        "\"hr_create.sql\",\"hr_populate.sql\",\"hr_code.sql\"]",
        "\"schemas\":1",
        "\"tables\":7",
        "\"rows\":216",
        "\"views\":1",
        "\"sequences\":3",
        "\"indexes\":17",
        "\"constraints\":20",
        "\"comments\":42",
        "\"tables_skipped\":0",
        "{\"table\":\"hr.employees\",\"rows\":107}",
        "{\"table\":\"hr.departments\",\"rows\":27}",
        "{\"table\":\"hr.countries\",\"rows\":25}",
        "{\"table\":\"hr.locations\",\"rows\":23}",
        "{\"table\":\"hr.jobs\",\"rows\":19}",
        "{\"table\":\"hr.job_history\",\"rows\":10}",
        "{\"table\":\"hr.regions\",\"rows\":5}",
        "\"kind\":\"procedure\",\"name\":\"hr.secure_dml\",\"reason\":\"PL/SQL comes after v0.2.0\",\"script\":3",
        "\"kind\":\"trigger\",\"name\":\"hr.secure_employees\",\"reason\":\"PL/SQL comes after v0.2.0\",\"script\":3",
        "\"kind\":\"procedure\",\"name\":\"hr.add_job_history\",\"reason\":\"PL/SQL comes after v0.2.0\",\"script\":3",
        "\"kind\":\"trigger\",\"name\":\"hr.update_job_history\",\"reason\":\"PL/SQL comes after v0.2.0\",\"script\":3",
        "\"grant\":1",
        "{\"kind\":\"users (schema created, login not)\",\"statements\":1}",
        "{\"kind\":\"COMMIT\",\"statements\":2}",
        "{\"kind\":\"queries\",\"statements\":4}",
        "{\"kind\":\"PL/SQL settings\",\"statements\":1}",
        "\"command\":\"@@hr_create.sql\",\"reason\":\"followed\"",
        "\"command\":\"@@hr_code.sql\",\"reason\":\"followed\"",
        "\"command\":\"EXIT\",\"reason\":\"the script ends here\"",
        "\"command\":\"ACCEPT PASS\"",
        "\"command\":\"WHENEVER SQLERROR\",\"reason\":\"eggdb restore's own flags decide\"",
        "\"differences\":[]",
        "\"errors\":[]",
        "\"error\":null",
    ] {
        assert!(report.contains(part), "{part:?} in\n{report}");
    }
    assert_eq!(report.matches("\"kind\":\"PL/SQL block\"").count(), 3, "{report}");
    let db = Database::open(&target).unwrap();
    assert_hr_counts(&db, "HR");
    let hr = |sql: &str| {
        query(
            &db,
            Dialect::Oracle,
            &format!("ALTER SESSION SET CURRENT_SCHEMA = HR; {sql}"),
        )
    };
    assert_eq!(
        hr("SELECT TO_CHAR(hire_date, 'YYYY-MM-DD') FROM employees WHERE employee_id = 100"),
        "2013-06-17\n"
    );
    assert_eq!(
        query(
            &db,
            Dialect::PostgreSql,
            "SELECT hire_date::date FROM hr.employees WHERE employee_id = 100"
        ),
        "2013-06-17\n"
    );
    assert_eq!(hr("SELECT COUNT(*) FROM emp_details_view"), "106\n");
    assert_eq!(hr("SELECT employees_seq.NEXTVAL FROM dual"), "207\n");
    let in_hr = |sql: &str| format!("ALTER SESSION SET CURRENT_SCHEMA = HR; {sql}");
    assert_eq!(
        oracle_error(&db, &in_hr("INSERT INTO departments VALUES (999, 'X', 999, 1700)")),
        Some(2291)
    );
    assert_eq!(
        oracle_error(&db, &in_hr("UPDATE employees SET salary = 0 WHERE employee_id = 100")),
        Some(2290)
    );
    assert_eq!(
        oracle_error(&db, &in_hr("INSERT INTO regions VALUES (10, 'Dup')")),
        Some(1)
    );
    let verification = query(&db, Dialect::Oracle, &verification_query());
    assert_eq!(verification.lines().count(), 7, "{verification}");
    for row in verification.lines() {
        let fields: Vec<&str> = row.split('|').collect();
        assert_eq!(fields.len(), 3, "{row}");
        assert_eq!(fields[1], fields[2], "{row}");
    }
    db.close().unwrap();
}

#[test]
fn the_hr_sub_scripts_restore_one_by_one_into_the_default_schema() {
    let directory = TempDirectory::new("hr-parts");
    let target = directory.join("hr.db");
    let created = restore(None, &target, &fixture("hr/hr_create.sql"), &["--create"]);
    assert_eq!(created.status.code(), Some(0), "{}", everything(&created));
    let populated = restore(None, &target, &fixture("hr/hr_populate.sql"), &[]);
    let report = text(&populated.stdout);
    assert_eq!(populated.status.code(), Some(0), "{}", everything(&populated));
    for part in ["\"rows\":216", "\"constraints\":0", "\"differences\":[]"] {
        assert!(report.contains(part), "{part:?} in\n{report}");
    }
    let code = restore(None, &target, &fixture("hr/hr_code.sql"), &[]);
    assert_eq!(code.status.code(), Some(0), "{}", everything(&code));
    let db = Database::open(&target).unwrap();
    assert_hr_counts(&db, "PUBLIC");
    assert_eq!(
        oracle_error(&db, "INSERT INTO departments VALUES (999, 'X', 999, 1700)"),
        Some(2291)
    );
    db.close().unwrap();
}

#[test]
fn a_sql_developer_export_restores_with_its_storage_clauses_and_plsql_listed() {
    let directory = TempDirectory::new("sqldeveloper");
    let target = directory.join("hr.db");
    let output = restore(None, &target, &fixture("hr-sqldeveloper.sql"), &["--create"]);
    let report = text(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}\n{report}",
        text(&output.stderr)
    );
    for part in [
        "\"ok\":true",
        "\"tables\":7",
        "\"rows\":216",
        "\"views\":1",
        "\"sequences\":3",
        "\"indexes\":18",
        "\"constraints\":20",
        "\"kind\":\"procedure\",\"name\":\"hr.add_job_history\"",
        "\"kind\":\"procedure\",\"name\":\"hr.secure_dml\"",
        "\"kind\":\"trigger\",\"name\":\"hr.secure_employees\"",
        "\"kind\":\"trigger\",\"name\":\"hr.update_job_history\"",
        "\"command\":\"SET DEFINE\",\"reason\":\"used for substitution variables\"",
        "\"differences\":[]",
    ] {
        assert!(report.contains(part), "{part:?} in\n{report}");
    }
    let db = Database::open(&target).unwrap();
    assert_hr_counts(&db, "HR");
    let hr = |sql: &str| {
        query(
            &db,
            Dialect::Oracle,
            &format!("ALTER SESSION SET CURRENT_SCHEMA = HR; {sql}"),
        )
    };
    assert_eq!(
        hr("SELECT department_name FROM departments WHERE department_id = 270"),
        "Payroll & Benefits\n"
    );
    assert_eq!(
        hr("SELECT street_address FROM locations WHERE location_id = 1400"),
        "2014 Jabberwocky Rd\nBuilding 2\n"
    );
    assert_eq!(
        hr("SELECT region_id + 1 FROM regions WHERE region_name = 'Europe'"),
        "11\n"
    );
    assert_eq!(
        hr("SELECT TO_CHAR(hire_date, 'YYYY-MM-DD') FROM employees WHERE employee_id = 100"),
        "2013-06-17\n"
    );
    assert_eq!(hr("SELECT COUNT(*) FROM emp_details_view"), "106\n");
    assert_eq!(
        oracle_error(
            &db,
            "ALTER SESSION SET CURRENT_SCHEMA = HR; INSERT INTO employees (employee_id, last_name, email, hire_date, job_id, salary) VALUES (300, 'X', 'SKING', SYSDATE, 'IT_PROG', 1)"
        ),
        Some(1)
    );
    db.close().unwrap();
}

#[test]
fn impdp_ddl_sqlcl_csv_and_the_reference_constraints_restore_in_three_steps() {
    let directory = TempDirectory::new("impdp");
    let target = directory.join("hr.db");
    let tables = restore(None, &target, &fixture("hr-impdp-tables.sql"), &["--create"]);
    let report = text(&tables.stdout);
    assert_eq!(tables.status.code(), Some(0), "{}", everything(&tables));
    for part in [
        "\"schemas\":1",
        "\"tables\":7",
        "\"indexes\":18",
        "\"set\":[\"EVENTS\"]",
        "{\"kind\":\"users (schema created, login not)\",\"statements\":2}",
        "{\"kind\":\"PL/SQL settings\",\"statements\":3}",
        "{\"kind\":\"storage settings\",\"statements\":18}",
        "\"kind\":\"PL/SQL block\"",
        "\"kind\":\"procedure\",\"name\":\"hr.add_job_history\"",
    ] {
        assert!(report.contains(part), "{part:?} in\n{report}");
    }
    assert!(!report.contains("S:1D9C"), "{report}");
    for (table, _) in HR_COUNTS {
        let csv = fixture(&format!("csv/{}_DATA_TABLE.csv", table.to_uppercase()));
        let qualified = format!("hr.{table}");
        let imported = eggdb_in(
            None,
            &[
                &"import",
                &"--dialect",
                &"oracle",
                &target,
                &"--csv",
                &csv,
                &"--table",
                &qualified,
                &"--header",
            ],
            b"",
        );
        assert_eq!(imported.status.code(), Some(0), "{}", everything(&imported));
    }
    let references = restore(None, &target, &fixture("hr-impdp-refs.sql"), &[]);
    let report = text(&references.stdout);
    assert_eq!(references.status.code(), Some(0), "{}", everything(&references));
    assert!(report.contains("\"constraints\":10"), "{report}");
    let db = Database::open(&target).unwrap();
    assert_hr_counts(&db, "HR");
    let hr = |sql: &str| {
        query(
            &db,
            Dialect::Oracle,
            &format!("ALTER SESSION SET CURRENT_SCHEMA = HR; {sql}"),
        )
    };
    assert_eq!(
        hr("SELECT TO_CHAR(hire_date, 'YYYY-MM-DD') FROM employees WHERE employee_id = 100"),
        "2013-06-17\n"
    );
    assert_eq!(
        hr("SELECT TO_CHAR(start_date, 'YYYY-MM-DD') FROM job_history WHERE employee_id = 102"),
        "2011-01-13\n"
    );
    assert_eq!(
        hr("SELECT NVL(TO_CHAR(commission_pct), 'none'), NVL(TO_CHAR(manager_id), 'none') FROM employees WHERE employee_id = 100"),
        "none|none\n"
    );
    assert_eq!(
        oracle_error(
            &db,
            "ALTER SESSION SET CURRENT_SCHEMA = HR; INSERT INTO departments VALUES (999, 'X', 999, 1700)"
        ),
        Some(2291)
    );
    db.close().unwrap();
}

#[test]
fn data_pump_and_rman_files_are_routed_by_content_and_a_text_dmp_restores() {
    let directory = TempDirectory::new("routes");
    let dump = directory.join("hr.dmp");
    std::fs::write(&dump, b"\x03\x00\x01\x00EXPORT:V19.00.00\x00\x00binary").unwrap();
    let target = directory.join("hr.db");
    let refused = restore(None, &target, &dump, &["--create"]);
    let message = everything(&refused);
    assert_eq!(refused.status.code(), Some(2), "{message}");
    assert!(message.contains("an Oracle Data Pump or exp dump file"), "{message}");
    assert!(message.contains("impdp"), "{message}");
    assert!(!target.exists());
    let backup = directory.join("hr.bkp");
    std::fs::write(&backup, b"\x00\x00\xa2\x00RMAN backup piece").unwrap();
    let refused = restore(None, &target, &backup, &["--create"]);
    let message = everything(&refused);
    assert_eq!(refused.status.code(), Some(2), "{message}");
    assert!(message.contains("perhaps an RMAN backup"), "{message}");
    assert!(!target.exists());
    let text_dump = directory.write("text.dmp", "CREATE TABLE t (a NUMBER);\nINSERT INTO t VALUES (1);\n");
    let restored = restore(None, &target, &text_dump, &["--create"]);
    assert_eq!(restored.status.code(), Some(0), "{}", everything(&restored));
}

#[test]
fn an_sql_only_block_loads_all_its_rows_or_none() {
    let directory = TempDirectory::new("block");
    let script = "CREATE TABLE t (id NUMBER PRIMARY KEY, name VARCHAR2(10));\n\
                  BEGIN\n  INSERT INTO t VALUES (1, 'a');\n  INSERT INTO t VALUES (2, 'b');\n  INSERT INTO t VALUES (1, 'dup');\nEND;\n/\n\
                  BEGIN\n  INSERT INTO t VALUES (3, 'c');\n  COMMIT;\nEND;\n/\n";
    let (stopped, target) = restore_script(&directory, script, &[]);
    assert_eq!(stopped.status.code(), Some(3), "{}", everything(&stopped));
    assert!(!target.exists());
    let (listed, target) = restore_script(&directory, script, &["--keep-going"]);
    let report = text(&listed.stdout);
    assert_eq!(listed.status.code(), Some(3), "{}", everything(&listed));
    assert!(report.contains("\"line\":5"), "{report}");
    let db = Database::open(&target).unwrap();
    assert_eq!(
        query(&db, Dialect::Oracle, "SELECT id FROM t ORDER BY id"),
        "3\n"
    );
    db.close().unwrap();
}

#[test]
fn an_error_without_create_keeps_the_statements_before_it() {
    let directory = TempDirectory::new("keep");
    let target = directory.join("kept.db");
    let created = eggdb_in(
        None,
        &[&"sql", &"--dialect", &"oracle", &"--create", &target, &"-c", &"CREATE TABLE seed (a NUMBER)"],
        b"",
    );
    assert_eq!(created.status.code(), Some(0), "{}", everything(&created));
    let script = directory.write(
        "partial.sql",
        "CREATE TABLE before_error (a NUMBER);\nINSERT INTO before_error VALUES (1);\nINSERT INTO missing_table VALUES (1);\nCREATE TABLE after_error (a NUMBER);\n",
    );
    let output = restore(None, &target, &script, &[]);
    assert_eq!(output.status.code(), Some(3), "{}", everything(&output));
    let db = Database::open(&target).unwrap();
    assert_eq!(query(&db, Dialect::Oracle, "SELECT COUNT(*) FROM before_error"), "1\n");
    assert!(oracle_error(&db, "SELECT COUNT(*) FROM after_error").is_some());
    db.close().unwrap();
}

#[test]
fn substitution_variables_fail_only_where_they_would_be_used() {
    let directory = TempDirectory::new("substitution");
    let (output, target) = restore_script(&directory, "PROMPT &unused\nCREATE TABLE &missing (a NUMBER);\n", &[]);
    let message = everything(&output);
    assert_eq!(output.status.code(), Some(3), "{message}");
    assert!(message.contains("substitution variable &missing has no value"), "{message}");
    assert!(!target.exists());
    let script = "ACCEPT tbs PROMPT 'Tablespace: ' DEFAULT '&var_default'\n\
                  PROMPT installing &nothing\n\
                  BEGIN\n  IF '&pass' IS NULL THEN\n    NULL;\n  END IF;\nEND;\n/\n\
                  CREATE USER app IDENTIFIED BY \"&pass\" DEFAULT TABLESPACE &tbs;\n\
                  DEFINE t = notes\n\
                  CREATE TABLE &t.x (a VARCHAR2(10));\n\
                  SET DEFINE OFF\n\
                  INSERT INTO notesx VALUES ('AT&T');\n\
                  SET DEFINE ON\n\
                  INSERT INTO notesx VALUES ('AT& T');\n\
                  DEFINE v = 'kept'\n\
                  INSERT INTO notesx VALUES ('&&v');\n";
    let (output, target) = restore_script(&directory, script, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", everything(&output));
    let report = text(&output.stdout);
    assert!(report.contains("\"schemas\":1"), "{report}");
    let db = Database::open(&target).unwrap();
    assert_eq!(
        query(&db, Dialect::Oracle, "SELECT a FROM notesx ORDER BY a"),
        "AT& T\nAT&T\nkept\n"
    );
    db.close().unwrap();
    let (output, _) = restore_script(&directory, "CREATE USER &who IDENTIFIED BY x;\n", &[]);
    assert_eq!(output.status.code(), Some(3), "{}", everything(&output));
    let (output, target) = restore_script(
        &directory,
        "CREATE TABLE &name (a NUMBER);\n",
        &["--define", "name=from_flag"],
    );
    assert_eq!(output.status.code(), Some(0), "{}", everything(&output));
    let db = Database::open(&target).unwrap();
    assert_eq!(query(&db, Dialect::Oracle, "SELECT COUNT(*) FROM from_flag"), "0\n");
    db.close().unwrap();
}

#[test]
fn includes_are_followed_only_through_regular_files() {
    let directory = TempDirectory::new("includes");
    directory.write("sub/inner.sql", "CREATE TABLE inner_t (a NUMBER);\n@@deeper\n");
    directory.write("sub/deeper.sql", "CREATE TABLE deeper_t (a NUMBER);\nEXIT\nCREATE TABLE never_t (a NUMBER);\n");
    let main = directory.write(
        "main.sql",
        "@@sub/inner.sql\nCREATE TABLE after_exit (a NUMBER);\n",
    );
    let target = directory.join("included.db");
    let output = restore(Some(&directory.path), &target, &main, &["--create"]);
    let report = text(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{}", everything(&output));
    assert!(report.contains("\"tables\":2"), "{report}");
    assert!(report.contains("\"sub/inner.sql\",\"deeper.sql\"]"), "{report}");
    let missing = directory.write("missing.sql", "CREATE TABLE a (x NUMBER);\n@@nowhere.sql\n");
    let output = restore(None, &directory.join("missing.db"), &missing, &["--create"]);
    let message = everything(&output);
    assert_eq!(output.status.code(), Some(3), "{message}");
    assert!(message.contains("line 2"), "{message}");
    let url = directory.write("url.sql", "@http://example.invalid/x.sql\n");
    let output = restore(None, &directory.join("url.db"), &url, &["--create"]);
    let message = everything(&output);
    assert_eq!(output.status.code(), Some(3), "{message}");
    assert!(message.contains("never fetches a script from a URL"), "{message}");
    let folder = directory.write("folder.sql", "@@sub\n");
    std::fs::create_dir_all(directory.join("sub.sql")).unwrap();
    let output = restore(None, &directory.join("folder.db"), &folder, &["--create"]);
    let message = everything(&output);
    assert_eq!(output.status.code(), Some(3), "{message}");
    assert!(message.contains("cannot include sub.sql"), "{message}");
    directory.write("loop.sql", "@@loop.sql\n");
    let output = restore(None, &directory.join("loop.db"), &directory.join("loop.sql"), &["--create"]);
    let message = everything(&output);
    assert_eq!(output.status.code(), Some(3), "{message}");
    assert!(message.contains("nested more than 20 deep"), "{message}");
    let sentinel = directory.join("sentinel");
    let commands = directory.write(
        "commands.sql",
        &format!(
            "HOST touch {0}\n!touch {0}\nSPOOL {0}\nCREATE TABLE t (a NUMBER);\n",
            sentinel.display()
        ),
    );
    let output = restore(None, &directory.join("commands.db"), &commands, &["--create"]);
    assert_eq!(output.status.code(), Some(0), "{}", everything(&output));
    assert!(!sentinel.exists());
    let from_stdin = eggdb_in(
        Some(&directory.path),
        &[&"restore", &"--dialect", &"oracle", &"--create", &"stdin.db", &"-"],
        b"@@sub/deeper.sql\n",
    );
    assert_eq!(from_stdin.status.code(), Some(0), "{}", everything(&from_stdin));
}

#[test]
fn sql_plus_statement_rules_and_connect_are_followed_and_secrets_never_shown() {
    let directory = TempDirectory::new("statements");
    let script = "-- CONNECT HR\n\
                  /* a comment\n   over two lines; */\n\
                  CREATE TABLE once (a NUMBER);\n/\n\
                  CREATE TABLE dropped (a NUMBER)\n.\n\
                  CONNECT scott/tiger@db\n\
                  CREATE TABLE emp (id NUMBER PRIMARY KEY, boss NUMBER);\n\
                  DEFINE pw = hunter2\n\
                  INSERT INTO emp VALUES (1, 99);\n\
                  ALTER TABLE emp ADD CONSTRAINT emp_boss_fk FOREIGN KEY (boss) REFERENCES emp (id) DISABLE;\n\
                  ALTER TABLE emp ENABLE NOVALIDATE CONSTRAINT emp_boss_fk;\n\
                  ALTER TABLE emp DISABLE PRIMARY KEY;\n\
                  ROLLBACK;\n\
                  CREATE TABLE no_end (a NUMBER)";
    let (output, target) = restore_script(&directory, script, &[]);
    let all = everything(&output);
    assert_eq!(output.status.code(), Some(0), "{all}");
    for part in [
        "\"tables\":2",
        "\"command\":\"/\"",
        "\"command\":\"CONNECT SCOTT\"",
        "\"reason\":\"not run (ended by .)\"",
        "\"reason\":\"not run: no ; or / at the end of the script\"",
        "\"difference\":\"existing rows are checked here; Oracle's NOVALIDATE doesn't check them\"",
        "\"difference\":\"not created: existing rows violate it",
        "\"difference\":\"kept enforced: EggDB doesn't disable keys\"",
        "\"object\":\"ROLLBACK\"",
    ] {
        assert!(all.contains(part), "{part:?} in\n{all}");
    }
    assert!(!all.contains("tiger"), "{all}");
    assert!(!all.contains("hunter2"), "{all}");
    let db = Database::open(&target).unwrap();
    assert_eq!(query(&db, Dialect::PostgreSql, "SELECT count(*) FROM scott.emp"), "1\n");
    assert_eq!(query(&db, Dialect::PostgreSql, "SELECT count(*) FROM once"), "0\n");
    db.close().unwrap();
    let (output, _) = restore_script(
        &directory,
        "CREATE USER u IDENTIFIED BY hunter2 DEFAULT TABLESPACE users QUOTA 5 ON nowhere;\nCREATE TABLE \"u\".t (a NUMBER;\n",
        &["--format", "text"],
    );
    assert!(!everything(&output).contains("hunter2"), "{}", everything(&output));
    let (output, _) = restore_script(&directory, "SET SQLTERMINATOR #\nCREATE TABLE t (a NUMBER)#\n", &[]);
    let message = everything(&output);
    assert_eq!(output.status.code(), Some(3), "{message}");
    assert!(message.contains("changes how the script is read"), "{message}");
    let (output, _) = restore_script(&directory, "INSERT INTO nosuch.t VALUES (1);\n", &[]);
    assert_eq!(output.status.code(), Some(3), "{}", everything(&output));
}

#[test]
fn a_disabled_foreign_key_over_orphan_rows_is_listed_not_failed() {
    let directory = TempDirectory::new("orphans");
    let script = "CREATE TABLE parent (id NUMBER PRIMARY KEY);\n\
                  CREATE TABLE child (id NUMBER, parent_id NUMBER, CONSTRAINT child_parent_fk FOREIGN KEY (parent_id) REFERENCES parent (id));\n\
                  ALTER TABLE child DISABLE CONSTRAINT child_parent_fk;\n\
                  INSERT INTO child VALUES (1, 42);\n\
                  ALTER TABLE child MODIFY CONSTRAINT child_parent_fk ENABLE;\n";
    let (output, target) = restore_script(&directory, script, &[]);
    let report = text(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{}", everything(&output));
    assert!(report.contains("\"object\":\"child_parent_fk\",\"difference\":\"not created: existing rows violate it"), "{report}");
    let db = Database::open(&target).unwrap();
    assert_eq!(query(&db, Dialect::Oracle, "SELECT COUNT(*) FROM child"), "1\n");
    db.close().unwrap();
    let still = "CREATE TABLE p (id NUMBER PRIMARY KEY);\n\
                 CREATE TABLE c (pid NUMBER, CONSTRAINT c_fk FOREIGN KEY (pid) REFERENCES p (id) DISABLE);\n\
                 INSERT INTO c VALUES (7);\n";
    let (output, _) = restore_script(&directory, still, &[]);
    let report = text(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{}", everything(&output));
    assert!(report.contains("disabled in the source: not created here, so not enforced"), "{report}");
}
