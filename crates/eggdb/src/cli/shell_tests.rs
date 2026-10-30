use super::Terminal;
use super::tests::{Ran, TempDirectory, run_with};

fn shell(file: &str, extra: &[&str], stdin: &str) -> Ran {
    let mut args = vec!["shell", file];
    args.extend_from_slice(extra);
    run_with(&args, stdin.as_bytes(), Terminal::default())
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn created(directory: &TempDirectory) -> String {
    let file = directory.file("app.db");
    let made = run_with(&["create", &file], b"", Terminal::default());
    assert_eq!(made.exit, 0, "{}", text(&made.stderr));
    file
}

#[test]
fn piped_lines_mix_key_value_commands_sql_and_meta_commands() {
    let directory = TempDirectory::new("shell-piped");
    let file = created(&directory);
    let script = "put user:1 Ada\n\
        put user:2 \"Grace Hopper\"\n\
        get user:2\n\
        create table items (id integer primary key, name text);\n\
        insert into items\n  values (1, 'egg'), (2, null);\n\
        select * from items order by id;\n\
        .tables\n\
        .schema items\n\
        .mode csv\n\
        select name from items order by id;\n";
    let ran = shell(&file, &[], script);
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    assert_eq!(
        text(&ran.stdout),
        "Grace Hopper\n\
         CREATE TABLE\n\
         INSERT 0 2\n \
         id | name\n\
         ----+------\n  \
         1 | egg\n  \
         2 |\n\
         (2 rows)\n\
         \n\
         items\n\
         CREATE TABLE items (id integer, name text, PRIMARY KEY (id));\n\
         mode: csv\n\
         name\n\
         egg\n\
         \n"
    );
    assert!(!text(&ran.stdout).contains('\x1b'));
}

#[test]
fn the_first_failure_stops_a_batch_unless_keep_going() {
    let directory = TempDirectory::new("shell-stop");
    let file = created(&directory);
    let script = "select 1/0;\nselect 'after';\n";
    let stopped = shell(&file, &[], script);
    assert_eq!(stopped.exit, 3);
    assert_eq!(text(&stopped.stdout), "");
    assert_eq!(
        text(&stopped.stderr),
        "ERROR:  division by zero\nLINE 1: select 1/0;\n               ^\nSQLSTATE:  22012\n"
    );
    let kept = shell(&file, &["--keep-going"], "get nope\nselect 'after' as a;\n");
    assert_eq!(kept.exit, 1);
    assert!(
        text(&kept.stdout).contains("after"),
        "{}",
        text(&kept.stdout)
    );
    assert!(text(&kept.stderr).contains("eggdb: key not found: nope"));
}

#[test]
fn dash_c_runs_each_text_in_order_and_a_last_statement_needs_no_semicolon() {
    let directory = TempDirectory::new("shell-c");
    let file = created(&directory);
    let ran = shell(
        &file,
        &["-c", "put k v", "-c", "scan -k", "-c", "select 1 as one"],
        "",
    );
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    assert_eq!(text(&ran.stdout), "k\n one\n-----\n   1\n(1 row)\n\n");
}

#[test]
fn a_transaction_block_spans_lines_and_a_failure_aborts_it_until_rollback() {
    let directory = TempDirectory::new("shell-block");
    let file = created(&directory);
    let script = "create table t (id integer primary key);\n\
        begin;\ninsert into t values (1);\ninsert into t values (1);\n\
        select 1;\nrollback;\nselect count(*) from t;\nbegin;\ninsert into t values (2);\n";
    let ran = shell(&file, &["--keep-going"], script);
    assert_eq!(ran.exit, 3);
    let stderr = text(&ran.stderr);
    assert!(stderr.contains("SQLSTATE:  23505"), "{stderr}");
    assert!(stderr.contains("SQLSTATE:  25P02"), "{stderr}");
    assert!(
        stderr.ends_with(
            "WARNING:  the shell ended inside a transaction block (BEGIN without COMMIT); it was rolled back\n"
        ),
        "{stderr}"
    );
    assert!(
        text(&ran.stdout).contains("     0\n(1 row)"),
        "{}",
        text(&ran.stdout)
    );
}

#[test]
fn shell_usage_errors_name_the_problem() {
    let directory = TempDirectory::new("shell-usage");
    let file = created(&directory);
    for (line, message) in [
        (
            ".nope",
            "eggdb: unknown command '.nope'. Type .help for the list.\n",
        ),
        (
            ".mode wide",
            "eggdb: .mode takes table, expanded or csv, not 'wide'\n",
        ),
        (
            "put only-key",
            "eggdb: put needs a key and a value here; stdin is the shell's own, so give the value or @file\n",
        ),
        (
            "put k @-",
            "eggdb: @- reads stdin, which is the shell's own; use @file\n",
        ),
        ("get \"open", "eggdb: unfinished \"...\" in the line\n"),
    ] {
        let ran = shell(&file, &["-c", line], "");
        assert_eq!(ran.exit, 2, "{line}");
        assert_eq!(text(&ran.stderr), message, "{line}");
    }
    let missing = shell(&file, &["-c", ".schema nope"], "");
    assert_eq!(missing.exit, 1);
    assert_eq!(text(&missing.stderr), "eggdb: no table named nope\n");
}

#[test]
fn expanded_mode_prints_one_record_per_block() {
    let directory = TempDirectory::new("shell-expanded");
    let file = created(&directory);
    let ran = shell(
        &file,
        &["--mode", "expanded", "-c", "select 1 as id, 'Ada' as name"],
        "",
    );
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    assert_eq!(
        text(&ran.stdout),
        "-[ RECORD 1 ]-\nid   | 1\nname | Ada\n\n"
    );
}

#[test]
fn backslashes_and_tabs_in_sql_reach_the_table_unchanged() {
    let directory = TempDirectory::new("shell-backslash");
    let file = created(&directory);
    let script = "create table t (id integer primary key, path text);\n\
        insert into t values (1, 'C:\\dir\\file\tend');\n\
        select path from t;\n";
    let ran = shell(&file, &["--mode", "csv"], script);
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    assert!(
        text(&ran.stdout).ends_with("path\nC:\\dir\\file\tend\n"),
        "{}",
        text(&ran.stdout)
    );
}

#[test]
fn collection_chooses_the_key_space_of_the_key_value_commands() {
    let directory = TempDirectory::new("shell-collection");
    let file = created(&directory);
    let added = run_with(&["collection", "add", &file, "sessions"], b"", Terminal::default());
    assert_eq!(added.exit, 0, "{}", text(&added.stderr));
    let script = "put k default\n\
        .collection\n\
        .collection sessions\n\
        put k in-sessions\n\
        get k\n\
        get -C sessions k\n\
        scan -k\n\
        .collection\n\
        .collection -\n\
        get k\n\
        put --create -C cache c:1 hot\n\
        .collection\n";
    let ran = shell(&file, &[], script);
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    assert_eq!(
        text(&ran.stdout),
        "* default key space\n  sessions\n\
         collection: sessions\n\
         in-sessions\n\
         in-sessions\n\
         k\n\
         \u{20} default key space\n* sessions\n\
         collection: default key space\n\
         default\n\
         * default key space\n  cache\n  sessions\n"
    );
}

#[test]
fn collection_mistakes_name_the_problem() {
    let directory = TempDirectory::new("shell-collection-errors");
    let file = created(&directory);
    let missing = shell(&file, &[], ".collection orders\n");
    assert_eq!(missing.exit, 1);
    assert!(
        text(&missing.stderr).contains("no collection named orders in")
            && text(&missing.stderr).contains("try: .collection"),
        "{}",
        text(&missing.stderr)
    );
    let two = shell(&file, &[], ".collection a b\n");
    assert_eq!(two.exit, 2);
    assert!(
        text(&two.stderr).contains(".collection takes one name, or - for the default key space"),
        "{}",
        text(&two.stderr)
    );
    let none = shell(&file, &[], ".collection\n");
    assert_eq!(none.exit, 0);
    assert_eq!(text(&none.stdout), "* default key space\nno collections\n");
}

#[test]
fn the_oracle_shell_ends_plsql_units_at_a_slash_line_and_prints_server_output() {
    let directory = TempDirectory::new("shell-plsql");
    let file = created(&directory);
    let script = "create table emp (id number(4), sal number(8));\n\
        insert into emp values (1, 100);\n\
        create or replace procedure raise_sal(p_pct number) as\n\
        begin\n  \
          update emp set sal = sal * (1 + p_pct / 100);\n  \
          dbms_output.put_line(sql%rowcount || ' rows raised');\n\
        end;\n\
        /\n\
        exec raise_sal(10)\n\
        set serveroutput off;\n\
        execute raise_sal(10);\n\
        SET SERVEROUT ON SIZE UNLIMITED\n\
        begin\n  \
          dbms_output.put_line('a;b');\n\
        end;\n\
        /\n\
        /\n";
    let ran = shell(&file, &["--dialect", "oracle"], script);
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    assert_eq!(
        text(&ran.stdout),
        "CREATE TABLE\n\
         INSERT 0 1\n\
         CREATE PROCEDURE\n\
         1 rows raised\n\
         PL/SQL BLOCK\n\
         PL/SQL BLOCK\n\
         a;b\n\
         PL/SQL BLOCK\n"
    );
    assert_eq!(
        text(&ran.stderr),
        "eggdb: nothing to run: / ends a PL/SQL unit here; Up then Enter repeats the last statement\n"
    );
}

#[test]
fn the_oracle_shell_refuses_a_bad_set_serveroutput_and_prints_lines_before_an_error() {
    let directory = TempDirectory::new("shell-serveroutput");
    let file = created(&directory);
    let refused = shell(
        &file,
        &["--dialect", "oracle"],
        "set serveroutput on size 10\n",
    );
    assert_eq!(refused.exit, 3);
    let stderr = text(&refused.stderr);
    assert!(
        stderr.contains("SET SERVEROUTPUT SIZE 10 is out of range"),
        "{stderr}"
    );
    assert!(!stderr.contains("line 1:"), "{stderr}");
    let failed = shell(
        &file,
        &[
            "--dialect",
            "oracle",
            "-c",
            "begin dbms_output.put_line('first'); raise_application_error(-20005, 'then this'); end;",
        ],
        "",
    );
    assert_eq!(failed.exit, 3);
    assert_eq!(text(&failed.stdout), "first\n");
    assert!(
        text(&failed.stderr).contains("ORA-20005: then this"),
        "{}",
        text(&failed.stderr)
    );
}
