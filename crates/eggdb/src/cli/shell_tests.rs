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
    let added = run_with(
        &["collection", "add", &file, "sessions"],
        b"",
        Terminal::default(),
    );
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

#[test]
fn key_value_writes_and_sql_in_one_block_commit_together() {
    let directory = TempDirectory::new("shell-mixed-commit");
    let file = created(&directory);
    let script = "create table items (id integer primary key, name text);\n\
        begin;\n\
        put stock:egg 12\n\
        insert into items values (1, 'egg');\n\
        incr orders\n\
        get stock:egg\n\
        commit;\n";
    let ran = shell(&file, &[], script);
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    assert_eq!(
        text(&ran.stdout),
        "CREATE TABLE\nBEGIN\nINSERT 0 1\n1\n12\nCOMMIT\n"
    );
    let after = shell(
        &file,
        &[],
        "get stock:egg\nget orders\n.mode csv\nselect name from items;\n",
    );
    assert_eq!(after.exit, 0, "{}", text(&after.stderr));
    assert_eq!(text(&after.stdout), "12\n1\nmode: csv\nname\negg\n");
}

#[test]
fn rollback_drops_the_key_value_writes_of_the_block_with_its_sql() {
    let directory = TempDirectory::new("shell-mixed-rollback");
    let file = created(&directory);
    let script = "create table items (id integer primary key, name text);\n\
        put stock:egg 1\n\
        put stock:ham 5\n\
        begin;\n\
        put stock:egg 99\n\
        del stock:ham\n\
        insert into items values (2, 'ham');\n\
        get stock:egg\n\
        rollback;\n\
        get stock:egg\n\
        get stock:ham\n\
        .mode csv\n\
        select count(*) as n from items;\n";
    let ran = shell(&file, &[], script);
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    assert_eq!(
        text(&ran.stdout),
        "CREATE TABLE\nBEGIN\nINSERT 0 1\n99\nROLLBACK\n1\n5\nmode: csv\nn\n0\n"
    );
}

#[test]
fn a_failed_block_refuses_key_value_writes_until_rollback() {
    let directory = TempDirectory::new("shell-mixed-failed");
    let file = created(&directory);
    let script = "create table t (id integer primary key);\n\
        begin;\n\
        put k before\n\
        insert into t values (1);\n\
        insert into t values (1);\n\
        put k after\n\
        expire k 30s\n\
        rollback;\n\
        begin;\n\
        expire k 30s\n\
        rollback;\n\
        get k\n";
    let ran = shell(&file, &["--keep-going"], script);
    let stderr = text(&ran.stderr);
    assert!(stderr.contains("SQLSTATE:  23505"), "{stderr}");
    assert!(stderr.contains("cannot run put in "), "{stderr}");
    assert!(
        stderr.contains("the transaction block failed; ROLLBACK ends it"),
        "{stderr}"
    );
    assert!(
        stderr.contains("nothing to expire: k is not in "),
        "{stderr}"
    );
    assert!(stderr.contains("key not found: k"), "{stderr}");
    assert!(
        !text(&ran.stdout).contains("before"),
        "{}",
        text(&ran.stdout)
    );
}

fn get_from(file: &str, args: &[&str]) -> Ran {
    let mut all = vec!["get", file];
    all.extend_from_slice(args);
    run_with(&all, b"", Terminal::default())
}

fn assert_holds(file: &str, key: &str, value: &str) {
    let ran = get_from(file, &[key]);
    assert_eq!(ran.exit, 0, "{key}: {}", text(&ran.stderr));
    assert_eq!(text(&ran.stdout), value, "{key}");
}

fn assert_absent(file: &str, key: &str) {
    let ran = get_from(file, &[key]);
    assert_eq!(ran.exit, 1, "{key}: {}", text(&ran.stdout));
    assert!(
        text(&ran.stderr).contains(&format!("key not found: {key}")),
        "{}",
        text(&ran.stderr)
    );
}

#[test]
fn rollback_to_savepoint_undoes_the_key_value_writes_made_after_it() {
    let directory = TempDirectory::new("shell-savepoint-undo");
    let file = created(&directory);
    let script = "begin;\n\
        put a 1\n\
        savepoint s;\n\
        put b 2\n\
        del a\n\
        scan -k\n\
        rollback to savepoint s;\n\
        scan -k\n\
        commit;\n";
    let ran = shell(&file, &[], script);
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    assert_eq!(
        text(&ran.stdout),
        "BEGIN\nSAVEPOINT\nb\nROLLBACK\na\nCOMMIT\n"
    );
    assert_holds(&file, "a", "1");
    assert_absent(&file, "b");
}

#[test]
fn release_savepoint_keeps_the_key_value_writes_in_the_block() {
    let directory = TempDirectory::new("shell-savepoint-release");
    let file = created(&directory);
    let script = "begin;\n\
        savepoint s;\n\
        put kept 1\n\
        release savepoint s;\n\
        commit;\n\
        begin;\n\
        savepoint t;\n\
        put dropped 2\n\
        release savepoint t;\n\
        rollback;\n\
        scan -k\n";
    let ran = shell(&file, &[], script);
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    assert_eq!(
        text(&ran.stdout),
        "BEGIN\nSAVEPOINT\nRELEASE\nCOMMIT\nBEGIN\nSAVEPOINT\nRELEASE\nROLLBACK\nkept\n"
    );
    assert_holds(&file, "kept", "1");
    assert_absent(&file, "dropped");
}

#[test]
fn nested_savepoints_undo_the_key_value_writes_after_the_one_rolled_back_to() {
    let directory = TempDirectory::new("shell-savepoint-nested");
    let file = created(&directory);
    let script = "put k v0\n\
        begin;\n\
        savepoint outer;\n\
        put k v1\n\
        savepoint inner;\n\
        put k v2\n\
        incr n\n\
        rollback to savepoint inner;\n\
        get k\n\
        scan -k\n\
        put k v3\n\
        savepoint third;\n\
        put m x\n\
        release savepoint third;\n\
        rollback to savepoint outer;\n\
        scan\n\
        commit;\n\
        get k\n";
    let ran = shell(&file, &[], script);
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    assert_eq!(
        text(&ran.stdout),
        "BEGIN\nSAVEPOINT\nSAVEPOINT\n1\nROLLBACK\nv1\nk\nSAVEPOINT\nRELEASE\nROLLBACK\n\
         k\tv0\nCOMMIT\nv0\n"
    );
    assert_absent(&file, "n");
    assert_absent(&file, "m");
}

#[test]
fn rollback_to_savepoint_puts_back_times_to_live_and_collection_entries() {
    let directory = TempDirectory::new("shell-savepoint-expiry-collection");
    let file = created(&directory);
    let script = "put --collection sessions --create s:1 Ada\n\
        put plain p\n\
        put timed t --ttl 1h\n\
        begin;\n\
        savepoint s;\n\
        put -C sessions s:2 Grace --ttl 1h\n\
        del -C sessions s:1\n\
        expire plain 1h\n\
        persist timed\n\
        rollback to savepoint s;\n\
        ttl plain\n\
        get -C sessions s:1\n\
        scan -C sessions -k\n\
        commit;\n\
        ttl plain\n\
        ttl timed\n";
    let ran = shell(&file, &[], script);
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    let out = text(&ran.stdout);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[..7],
        ["BEGIN", "SAVEPOINT", "ROLLBACK", "never", "Ada", "s:1", "COMMIT"],
        "{out}"
    );
    assert_eq!(lines.get(7), Some(&"never"), "{out}");
    assert!(
        lines
            .get(8)
            .is_some_and(|line| line.starts_with("59m") || *line == "1h"),
        "{out}"
    );
    assert_eq!(lines.len(), 9, "{out}");
    assert_holds(&file, "plain", "p");
    let entry = get_from(&file, &["-C", "sessions", "s:2"]);
    assert_eq!(entry.exit, 1, "{}", text(&entry.stdout));
}

#[test]
fn rollback_to_savepoint_undoes_key_value_and_sql_writes_together() {
    let directory = TempDirectory::new("shell-savepoint-mixed");
    let file = created(&directory);
    let script = "create table items (id integer primary key);\n\
        begin;\n\
        insert into items values (1);\n\
        put k 1\n\
        savepoint s;\n\
        insert into items values (2);\n\
        put k 2\n\
        put j 1\n\
        rollback to savepoint s;\n\
        commit;\n\
        .mode csv\n\
        select id from items order by id;\n\
        get k\n\
        scan --prefix j -k\n";
    let ran = shell(&file, &[], script);
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    assert_eq!(
        text(&ran.stdout),
        "CREATE TABLE\nBEGIN\nINSERT 0 1\nSAVEPOINT\nINSERT 0 1\nROLLBACK\nCOMMIT\n\
         mode: csv\nid\n1\n1\n"
    );
    assert_absent(&file, "j");
}

#[test]
fn a_savepoint_that_begins_a_sqlite_transaction_undoes_key_value_writes_too() {
    let directory = TempDirectory::new("shell-savepoint-sqlite");
    let file = created(&directory);
    let script = "savepoint s;\n\
        put a 1\n\
        savepoint t;\n\
        put b 2\n\
        rollback to t;\n\
        release s;\n";
    let ran = shell(&file, &["--dialect", "sqlite"], script);
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    assert_holds(&file, "a", "1");
    assert_absent(&file, "b");
}

#[test]
fn key_value_commands_are_refused_after_a_statement_fails_past_a_savepoint() {
    let directory = TempDirectory::new("shell-savepoint-failed");
    let file = created(&directory);
    let script = "begin;\n\
        put k before\n\
        savepoint s;\n\
        select 1/0;\n\
        put k inside\n\
        get k\n\
        rollback to savepoint s;\n\
        get k\n\
        put k after\n\
        commit;\n\
        get k\n";
    let ran = shell(&file, &["--keep-going"], script);
    let stderr = text(&ran.stderr);
    for command in ["put", "get"] {
        assert!(
            stderr.contains(&format!("cannot run {command} in ")),
            "{stderr}"
        );
    }
    assert_eq!(
        stderr
            .matches(
                "a statement failed after the savepoint; ROLLBACK TO SAVEPOINT or ROLLBACK ends it"
            )
            .count(),
        2,
        "{stderr}"
    );
    assert_eq!(
        text(&ran.stdout),
        "BEGIN\nSAVEPOINT\nROLLBACK\nbefore\nCOMMIT\nafter\n"
    );
}

#[test]
fn a_failed_statement_in_a_mysql_block_undoes_only_itself_and_key_value_commands_go_on() {
    let directory = TempDirectory::new("shell-statement-rollback");
    let file = created(&directory);
    let script = "create table t (id int primary key);\n\
        begin;\n\
        insert into t values (1);\n\
        savepoint s;\n\
        put k before\n\
        insert into t values (2), (1);\n\
        put k after\n\
        commit;\n";
    let ran = shell(&file, &["--dialect", "mysql", "--keep-going"], script);
    let stderr = text(&ran.stderr);
    assert!(stderr.contains("1062"), "{stderr}");
    assert!(!stderr.contains("cannot run put"), "{stderr}");
    assert!(text(&ran.stdout).contains("COMMIT"), "{}", text(&ran.stdout));
    assert_holds(&file, "k", "after");
}

#[test]
fn a_time_to_live_set_in_a_block_applies_at_commit_and_vanishes_at_rollback() {
    let directory = TempDirectory::new("shell-block-expiry");
    let file = created(&directory);
    let script = "put session:7 Ada\n\
        begin;\n\
        expire session:7 1h\n\
        ttl session:7\n\
        ttl session:7 --json\n\
        rollback;\n\
        ttl session:7\n\
        begin;\n\
        expire session:7 1h\n\
        commit;\n\
        ttl session:7\n\
        begin;\n\
        persist session:7\n\
        ttl session:7\n\
        rollback;\n\
        begin;\n\
        persist session:7\n\
        commit;\n\
        ttl session:7\n";
    let ran = shell(&file, &[], script);
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    let out = text(&ran.stdout);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[..2], ["BEGIN", "1h"], "{out}");
    assert!(
        lines[2].starts_with("{\"key\":\"session:7\",\"ttl_ms\":3600000,\"expires_at\":\""),
        "{out}"
    );
    assert_eq!(
        lines[3..7],
        ["ROLLBACK", "never", "BEGIN", "COMMIT"],
        "{out}"
    );
    assert!(lines[7].starts_with("59m") || lines[7] == "1h", "{out}");
    assert_eq!(
        lines[8..],
        ["BEGIN", "never", "ROLLBACK", "BEGIN", "COMMIT", "never"],
        "{out}"
    );
}

#[test]
fn expire_and_persist_in_a_block_see_the_keys_the_block_wrote_and_deleted() {
    let directory = TempDirectory::new("shell-block-expiry-own-writes");
    let file = created(&directory);
    let script = "put gone 1\n\
        begin;\n\
        put fresh 2\n\
        expire fresh 10m\n\
        ttl fresh\n\
        del gone\n\
        expire gone 10m\n\
        persist gone\n\
        ttl gone\n\
        persist fresh --json\n\
        commit;\n\
        get gone\n\
        ttl fresh\n";
    let ran = shell(&file, &["--keep-going"], script);
    let stderr = text(&ran.stderr);
    assert!(
        stderr.contains("nothing to expire: gone is not in "),
        "{stderr}"
    );
    assert!(
        stderr.contains("nothing to persist: gone is not in "),
        "{stderr}"
    );
    assert_eq!(
        text(&ran.stdout),
        "BEGIN\n10m\n{\"key\":\"fresh\",\"persisted\":true}\nCOMMIT\nnever\n",
        "{stderr}"
    );
    assert!(stderr.contains("key not found: gone"), "{stderr}");
}

#[test]
fn scan_in_a_block_lists_the_keys_the_block_put_and_leaves_out_the_ones_it_deleted() {
    let directory = TempDirectory::new("shell-block-scan");
    let file = created(&directory);
    let script = "put a 1\n\
        put b 2\n\
        begin;\n\
        put c 3\n\
        del a\n\
        scan\n\
        scan --prefix c\n\
        scan -r -n 1 -k\n\
        scan --json\n\
        rollback;\n\
        scan -k\n";
    let ran = shell(&file, &[], script);
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    assert_eq!(
        text(&ran.stdout),
        "BEGIN\nb\t2\nc\t3\nc\t3\nc\n{\"key\":\"b\",\"value\":\"2\"}\n{\"key\":\"c\",\"value\":\"3\"}\n\
         ROLLBACK\na\nb\n"
    );
    let stderr = text(&ran.stderr);
    assert_eq!(stderr.matches("2 entries").count(), 2, "{stderr}");
    assert_eq!(stderr.matches("1 entry").count(), 2, "{stderr}");
}

#[test]
fn scan_and_ttl_in_a_block_read_the_blocks_collection() {
    let directory = TempDirectory::new("shell-block-collection");
    let file = created(&directory);
    let script = "put --collection sessions --create s:1 Ada\n\
        .collection sessions\n\
        begin;\n\
        put s:2 Grace --ttl 1h\n\
        scan -k\n\
        ttl s:2\n\
        cas s:1 Ada Linus\n\
        commit;\n\
        get s:1\n\
        .collection -\n\
        scan -k\n";
    let ran = shell(&file, &[], script);
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    assert_eq!(
        text(&ran.stdout),
        "collection: sessions\nBEGIN\ns:1\ns:2\n1h\nCOMMIT\nLinus\ncollection: default key space\n"
    );
    let stderr = text(&ran.stderr);
    assert!(
        stderr.contains("2 entries") && stderr.contains("no entries"),
        "{stderr}"
    );
}

#[test]
fn cas_in_a_block_compares_against_the_blocks_own_value() {
    let directory = TempDirectory::new("shell-block-cas");
    let file = created(&directory);
    let script = "put lock old\n\
        begin;\n\
        put lock mine\n\
        cas lock old next\n\
        cas lock old next --json\n\
        cas lock mine next\n\
        get lock\n\
        cas --if-absent fresh 1\n\
        cas --if-absent fresh 2\n\
        commit;\n\
        get lock\n\
        get fresh\n\
        begin;\n\
        cas lock next --delete\n\
        get lock\n\
        rollback;\n\
        get lock\n";
    let ran = shell(&file, &["--keep-going"], script);
    let stderr = text(&ran.stderr);
    assert!(
        stderr.contains("not swapped: lock in ") && stderr.contains(" holds mine, not old."),
        "{stderr}"
    );
    assert!(
        stderr.contains("not swapped: fresh is already in ") && stderr.contains("(holding 1)"),
        "{stderr}"
    );
    assert!(stderr.contains("key not found: lock"), "{stderr}");
    assert_eq!(
        text(&ran.stdout),
        "BEGIN\n{\"key\":\"lock\",\"swapped\":false,\"value\":\"mine\"}\nnext\nCOMMIT\nnext\n1\n\
         BEGIN\nROLLBACK\nnext\n",
        "{stderr}"
    );
}

#[test]
fn expire_persist_and_cas_run_while_a_savepoint_is_set_and_rollback_to_it_undoes_them() {
    let directory = TempDirectory::new("shell-block-savepoint-expiry");
    let file = created(&directory);
    let script = "put k v\n\
        put timed t --ttl 1h\n\
        begin;\n\
        savepoint s;\n\
        expire k 1h\n\
        ttl k\n\
        persist timed\n\
        ttl timed\n\
        cas k v w\n\
        get k\n\
        rollback to savepoint s;\n\
        ttl k\n\
        get k\n\
        scan -k\n\
        commit;\n\
        ttl k\n";
    let ran = shell(&file, &[], script);
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    let out = text(&ran.stdout);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines,
        [
            "BEGIN", "SAVEPOINT", "1h", "never", "w", "ROLLBACK", "never", "v", "k", "timed",
            "COMMIT", "never"
        ],
        "{out}"
    );
    let timed = run_with(&["ttl", &file, "timed"], b"", Terminal::default());
    assert_eq!(timed.exit, 0, "{}", text(&timed.stderr));
    assert!(
        text(&timed.stdout).contains("\"expires_at\":\""),
        "{}",
        text(&timed.stdout)
    );
}

#[test]
fn a_read_only_block_refuses_expiry_changes_and_reads_the_last_commit() {
    let directory = TempDirectory::new("shell-block-read-only-expiry");
    let file = created(&directory);
    let script = "put k v\n\
        begin read only;\n\
        expire k 1h\n\
        cas k v w\n\
        ttl k\n\
        scan -k\n\
        commit;\n\
        get k\n";
    let ran = shell(&file, &["--keep-going"], script);
    let stderr = text(&ran.stderr);
    for command in ["expire", "cas"] {
        assert!(
            stderr.contains(&format!("cannot run {command} in "))
                && stderr.contains("the transaction block is read-only"),
            "{stderr}"
        );
    }
    assert_eq!(text(&ran.stdout), "BEGIN\nnever\nk\nCOMMIT\nv\n");
}

#[test]
fn schema_in_sqlite_prints_each_create_statement_as_it_was_written() {
    let directory = TempDirectory::new("shell-sqlite-schema");
    let file = created(&directory);
    let script = "CREATE TABLE items(id integer primary key,\n  name text);\n\
        CREATE INDEX items_name ON items (name);\n\
        CREATE TABLE other(n integer);\n\
        .schema items\n";
    let ran = shell(&file, &["--dialect", "sqlite"], script);
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    let out = text(&ran.stdout);
    assert!(
        out.contains(
            "CREATE TABLE items(id integer primary key,\n  name text);\nCREATE INDEX items_name ON items (name);\n"
        ),
        "{out}"
    );
    assert!(!out.contains("CREATE TABLE other"), "{out}");
}

#[test]
fn the_shell_renames_and_deletes_collections_and_follows_the_one_in_use() {
    let directory = TempDirectory::new("shell-collection-rename");
    let file = created(&directory);
    let added = run_with(
        &["collection", "add", &file, "sessions"],
        b"",
        Terminal::default(),
    );
    assert_eq!(added.exit, 0, "{}", text(&added.stderr));
    let script = ".collection sessions\n\
        put s:1 Ada\n\
        put s:2 Grace\n\
        collection list\n\
        collection rename sessions logins\n\
        get s:1\n\
        collection list\n\
        collection delete logins\n\
        .collection\n";
    let ran = shell(&file, &[], script);
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    let stdout = text(&ran.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 10, "{stdout}");
    assert_eq!(lines[0], "collection: sessions");
    assert_eq!(lines[1], "sessions  2 entries");
    assert!(
        lines[2].starts_with("renamed collection sessions to logins in "),
        "{stdout}"
    );
    assert_eq!(lines[3], "collection: logins (followed the rename)");
    assert_eq!(lines[4], "Ada");
    assert_eq!(lines[5], "logins  2 entries");
    assert!(
        lines[6].starts_with("deleted collection logins from "),
        "{stdout}"
    );
    assert_eq!(lines[7], "collection: default key space (logins was deleted)");
    assert_eq!(&lines[8..], ["* default key space", "no collections"]);
}

#[test]
fn the_shell_lists_collections_in_a_block_and_renames_them_only_outside_one() {
    let directory = TempDirectory::new("shell-collection-block");
    let file = created(&directory);
    let added = run_with(
        &["collection", "add", &file, "sessions"],
        b"",
        Terminal::default(),
    );
    assert_eq!(added.exit, 0, "{}", text(&added.stderr));
    let script = "BEGIN;\n\
        collection list\n\
        collection move sessions logins\n\
        ROLLBACK;\n\
        collection move sessions logins\n\
        collection list\n";
    let ran = shell(&file, &["--keep-going"], script);
    assert_eq!(ran.exit, 2, "{}", text(&ran.stderr));
    assert!(
        text(&ran.stderr).contains(
            "collection add, delete and move run outside a transaction block: COMMIT or ROLLBACK first"
        ),
        "{}",
        text(&ran.stderr)
    );
    let stdout = text(&ran.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 5, "{stdout}");
    assert_eq!(&lines[..3], ["BEGIN", "sessions  no entries", "ROLLBACK"]);
    assert!(
        lines[3].starts_with("renamed collection sessions to logins in "),
        "{stdout}"
    );
    assert_eq!(lines[4], "logins  no entries");
}
