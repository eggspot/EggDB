use super::classify::{Action, Effect, Ignored, Skip, ToggledConstraint};
use super::classify_oracle::{
    PLSQL_SETTINGS, QUERIES, STORAGE_SETTINGS, USERS, anonymous_block_statements, block_texts,
    classify_oracle, oracle_table_key,
};
use super::script::{ScriptItem, ScriptSource};
use super::sqlplus_reader::{
    MAX_INCLUDE_DEPTH, MAX_SUBSTITUTION_VARIABLES, MAX_VARIABLE_VALUE_BYTES, SqlPlusReader,
    StatementMode, SubstitutionVariables, command_named, mode_of,
};
use super::table_rewrite_oracle::{
    rewritten_added, rewritten_table, without_disable, without_refused_identity,
};
use super::words::{Token, Words, plain_qualified_name};
use super::{
    IncludeOpener, IncludeRequest, RestoreOptions, RestoreReport, ServerOutputSetting,
    SqlPlusCommand, SqlPlusScript, SqlPlusStep, restore_dump, starts_plsql_unit,
};
use crate::constraints::ConstraintClauseKind;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::memory_store::MemoryStore;
use crate::session::{QueryResult, Session};
use std::collections::HashMap;
use std::io::BufRead;

struct MemoryIncludes {
    scripts: HashMap<String, Vec<u8>>,
}

impl MemoryIncludes {
    fn new(scripts: &[(&str, &str)]) -> Self {
        Self {
            scripts: scripts
                .iter()
                .map(|(name, text)| ((*name).to_owned(), text.as_bytes().to_vec()))
                .collect(),
        }
    }
}

impl IncludeOpener for MemoryIncludes {
    fn open<'s>(&'s self, request: &IncludeRequest<'_>) -> SqlResult<Box<dyn BufRead + 's>> {
        match self.scripts.get(request.name) {
            Some(bytes) => Ok(Box::new(bytes.as_slice())),
            None => Err(SqlError::new(
                SqlState::IO_ERROR,
                format!("line {}: no script {}", request.line, request.name),
            )),
        }
    }
}

#[derive(Debug, PartialEq)]
enum Read {
    Statement(String, u64),
    Command(String, String),
    Failed(String),
}

fn read_with(text: &str, includes: Option<&dyn IncludeOpener>) -> SqlResult<Vec<Read>> {
    let mut input = text.as_bytes();
    let mut reader = SqlPlusReader::new(&mut input, includes);
    let mut items = Vec::new();
    loop {
        match reader.next_item()? {
            ScriptItem::End => return Ok(items),
            ScriptItem::Statement(statement) => {
                items.push(Read::Statement(statement.text, statement.line));
            }
            ScriptItem::ClientCommand(command) => {
                items.push(Read::Command(command.name, command.shown));
            }
            ScriptItem::Failed(failure) => {
                items.push(Read::Failed(failure.error.message().to_owned()))
            }
        }
        if items.len() > 10_000 {
            return Ok(items);
        }
    }
}

fn read(text: &str) -> Vec<Read> {
    read_with(text, None).unwrap()
}

fn statements(text: &str) -> Vec<String> {
    read(text)
        .into_iter()
        .filter_map(|item| match item {
            Read::Statement(text, _) => Some(text),
            _ => None,
        })
        .collect()
}

fn commands(text: &str) -> Vec<String> {
    read(text)
        .into_iter()
        .filter_map(|item| match item {
            Read::Command(name, _) => Some(name),
            _ => None,
        })
        .collect()
}

#[test]
fn a_semicolon_ends_a_statement_only_outside_quotes_and_comments() {
    assert_eq!(
        statements(
            "INSERT INTO t VALUES (q'[;]', 'it''s;', \"a;b\") -- ;\n  , 1 /* ; */);\nSELECT 1 FROM dual;"
        ),
        vec![
            "INSERT INTO t VALUES (q'[;]', 'it''s;', \"a;b\") -- ;\n  , 1 /* ; */)",
            "SELECT 1 FROM dual",
        ]
    );
    assert_eq!(
        statements("SELECT nq'{a;}' FROM dual; SELECT 2 FROM dual;"),
        vec!["SELECT nq'{a;}' FROM dual", "SELECT 2 FROM dual"]
    );
    assert_eq!(
        statements("SELECT 6\n / 2 FROM dual;"),
        vec!["SELECT 6\n / 2 FROM dual"]
    );
    assert_eq!(
        statements("SELECT 1 /* a\n/\n*/ FROM dual\n/\n"),
        vec!["SELECT 1 /* a\n/\n*/ FROM dual"]
    );
    assert_eq!(
        statements("INSERT INTO t VALUES ('a\n/\nb');\n"),
        vec!["INSERT INTO t VALUES ('a\n/\nb')"]
    );
    assert_eq!(
        statements("CREATE TABLE t (\n  a NUMBER\n\n);\n"),
        vec!["CREATE TABLE t (\n  a NUMBER\n\n)"]
    );
}

#[test]
fn plsql_units_end_only_at_a_slash_line() {
    for head in [
        "CREATE OR REPLACE PROCEDURE p IS",
        "CREATE PROCEDURE p IS",
        "CREATE OR REPLACE EDITIONABLE FUNCTION f RETURN NUMBER IS",
        "CREATE OR REPLACE NONEDITIONABLE PACKAGE pk AS",
        "CREATE OR REPLACE PACKAGE BODY pk AS",
        "CREATE OR REPLACE TRIGGER tr BEFORE INSERT ON t",
        "CREATE OR REPLACE TYPE ty AS OBJECT (a NUMBER)",
        "CREATE OR REPLACE TYPE BODY ty AS",
        "CREATE OR REPLACE AND COMPILE JAVA SOURCE NAMED j AS",
        "CREATE LIBRARY l AS",
        "DECLARE x NUMBER;",
        "BEGIN",
        "<<outer>> BEGIN",
        "WITH FUNCTION f RETURN NUMBER IS",
    ] {
        let text = format!("{head}\nBEGIN\n  NULL;\nEND;\n/\nSELECT 1 FROM dual;\n");
        let found = statements(&text);
        assert_eq!(found.len(), 2, "{head}: {found:?}");
        assert!(found[0].ends_with("END;"), "{head}: {found:?}");
        assert_eq!(mode_of(head), StatementMode::PlSql, "{head}");
    }
    assert_eq!(mode_of("CREATE OR REPLACE"), StatementMode::Undecided);
    assert_eq!(
        mode_of("CREATE OR REPLACE FORCE VIEW v AS"),
        StatementMode::Sql
    );
    assert_eq!(
        statements("CREATE OR REPLACE\nPROCEDURE p IS\nBEGIN\n  NULL;\nEND;\n/\n").len(),
        1
    );
    assert_eq!(
        statements("BEGIN\n  -- it's a comment; with an apostrophe\n  NULL;\nEND;\n/\n"),
        vec!["BEGIN\n  -- it's a comment; with an apostrophe\n  NULL;\nEND;"]
    );
}

#[test]
fn slash_dot_and_end_of_file_follow_sql_plus() {
    assert_eq!(
        commands("CREATE TABLE t (a NUMBER);\n/\n"),
        vec!["/".to_owned()]
    );
    assert_eq!(commands("/\nSELECT 1 FROM dual;\n"), Vec::<String>::new());
    let items = read("CREATE TABLE t (a NUMBER)\n.\nSELECT 1 FROM dual;\nSELECT 2 FROM dual");
    assert_eq!(
        items,
        vec![
            Read::Command(".".to_owned(), "CREATE TABLE t (a NUMBER)".to_owned()),
            Read::Statement("SELECT 1 FROM dual".to_owned(), 3),
            Read::Command("unterminated".to_owned(), "SELECT 2 FROM dual".to_owned()),
        ]
    );
    let items = read("SELECT 'open\n");
    assert!(
        matches!(items.as_slice(), [Read::Failed(message)] if message.contains("inside a string"))
    );
    let items = read("/* never closed\n");
    assert!(
        matches!(items.as_slice(), [Read::Failed(message)] if message.contains("inside a comment"))
    );
}

#[test]
fn sql_plus_commands_are_recognized_with_their_abbreviations() {
    assert_eq!(command_named("rem"), Some("remark"));
    assert_eq!(command_named("pro"), Some("prompt"));
    assert_eq!(command_named("def"), Some("define"));
    assert_eq!(command_named("conn"), Some("connect"));
    assert_eq!(command_named("exec"), Some("execute"));
    assert_eq!(command_named("desc"), Some("describe"));
    assert_eq!(command_named("del"), None);
    assert_eq!(command_named("re"), None);
    assert_eq!(
        commands(
            "rem one\nRem\ttwo\nREMARK three -\ncontinued\nPRO hello\nSET DEFINE OFF;\nSET ECHO -\nOFF\nDEL 1\n"
        ),
        vec!["prompt", "set define", "set", "unknown"]
    );
    assert_eq!(
        statements("SET TRANSACTION READ ONLY;\nDELETE FROM t;\n"),
        vec!["SET TRANSACTION READ ONLY", "DELETE FROM t"]
    );
    assert_eq!(
        read("EXEC dbms_stats.gather_schema_stats('HR')\nSELECT 1 FROM dual;"),
        vec![
            Read::Statement("EXECUTE dbms_stats.gather_schema_stats('HR')".to_owned(), 1),
            Read::Statement("SELECT 1 FROM dual".to_owned(), 2),
        ]
    );
    assert_eq!(
        commands(
            "-- CONNECT HR\n/* CONNECT HR\n   ; */\nHOST rm -rf /\n!ls\nSPOOL x.log\nEXIT\nSELECT 1 FROM dual;"
        ),
        vec!["host", "host", "spool", "exit"]
    );
}

#[test]
fn connect_names_a_schema_and_never_shows_the_password() {
    let items = read("CONNECT scott/tiger@db AS SYSDBA\nCONN \"Mixed\"/x\nCONNECT / AS SYSDBA\n");
    let mut input = "CONNECT scott/tiger\n".as_bytes();
    let mut reader = SqlPlusReader::new(&mut input, None);
    let Ok(ScriptItem::ClientCommand(command)) = reader.next_item() else {
        panic!("not a command");
    };
    assert_eq!(command.argument, "scott");
    assert!(!format!("{command:?}").contains("tiger"));
    assert_eq!(
        items,
        vec![
            Read::Command("connect".to_owned(), "CONNECT SCOTT".to_owned()),
            Read::Command("connect".to_owned(), "CONNECT \"MIXED\"".to_owned()),
            Read::Command("connect".to_owned(), "CONNECT /".to_owned()),
        ]
    );
}

#[test]
fn reading_settings_that_change_terminators_are_refused() {
    for line in [
        "SET SQLTERMINATOR #",
        "SET SQLT OFF",
        "SET BLOCKTERMINATOR !",
        "SET SQLPREFIX %",
        "SET SQLBLANKLINES OFF",
    ] {
        let items = read(&format!("{line}\nSELECT 1 FROM dual;\n"));
        assert!(
            matches!(&items[0], Read::Failed(message) if message.contains("changes how the script is read")),
            "{line}: {items:?}"
        );
    }
    assert_eq!(commands("SET SQLBLANKLINES ON\n"), vec!["set"]);
}

#[test]
fn substitution_follows_define_escape_and_concat() {
    let mut variables = SubstitutionVariables::default();
    variables.define("t", "notes").unwrap();
    let substituted = variables
        .substitute("CREATE TABLE &t.x (a), &T..y, &&t, AT& T, &")
        .unwrap();
    assert_eq!(
        substituted.text,
        "CREATE TABLE notesx (a), notes.y, notes, AT& T, &"
    );
    assert_eq!(substituted.unresolved, None);
    let substituted = variables.substitute("'&missing' &other").unwrap();
    assert_eq!(substituted.text, "'&missing' &other");
    assert_eq!(substituted.unresolved.as_deref(), Some("missing"));
    assert_eq!(
        statements("SET ESCAPE ON\nINSERT INTO t VALUES ('a\\&b');\n"),
        vec!["INSERT INTO t VALUES ('a&b')"]
    );
    assert_eq!(
        statements("DEFINE v = x\nSET CONCAT +\nSELECT '&v+y' FROM dual;\n"),
        vec!["SELECT 'xy' FROM dual"]
    );
    assert_eq!(
        statements("SET DEFINE ^\nDEFINE v = z\nSELECT '^v &v' FROM dual;\n"),
        vec!["SELECT 'z &v' FROM dual"]
    );
    assert_eq!(
        statements("DEFINE v = x\nUNDEFINE v\nSELECT '&v' FROM dual;\n"),
        vec!["SELECT '&v' FROM dual"]
    );
    let long = "x".repeat(MAX_VARIABLE_VALUE_BYTES + 1);
    assert!(variables.define("long", &long).is_err());
    let mut many = SubstitutionVariables::default();
    let mut refused = false;
    for index in 0..MAX_SUBSTITUTION_VARIABLES + 1 {
        refused |= many.define(&format!("v{index}"), "1").is_err();
    }
    assert!(refused);
}

#[test]
fn includes_go_through_the_opener_with_limits() {
    let includes = MemoryIncludes::new(&[
        (
            "inner.sql",
            "CREATE TABLE inner_t (a NUMBER);\nSELECT 'loose' FROM dual",
        ),
        ("self.sql", "@@self\n@@self\n"),
        ("args.sql", "SELECT '&1-&2' FROM dual;\n"),
    ]);
    let items = read_with(
        "@@inner\nCREATE TABLE after_t (a NUMBER);\n",
        Some(&includes),
    )
    .unwrap();
    assert_eq!(
        items,
        vec![
            Read::Command("include".to_owned(), "@@inner".to_owned()),
            Read::Statement("CREATE TABLE inner_t (a NUMBER)".to_owned(), 1),
            Read::Command(
                "unterminated".to_owned(),
                "SELECT 'loose' FROM dual".to_owned()
            ),
            Read::Statement("CREATE TABLE after_t (a NUMBER)".to_owned(), 2),
        ]
    );
    let items = read_with("@args one two\n", Some(&includes)).unwrap();
    assert_eq!(
        items[1],
        Read::Statement("SELECT 'one-two' FROM dual".to_owned(), 1)
    );
    let items = read_with("@@self\n", Some(&includes)).unwrap();
    let failures = items
        .iter()
        .filter(
            |item| matches!(item, Read::Failed(message) if message.contains("nested more than")),
        )
        .count();
    assert!(failures > 0, "{items:?}");
    let depth = items
        .iter()
        .filter(|item| matches!(item, Read::Command(name, _) if name == "include"))
        .count();
    assert!(depth <= super::sqlplus_reader::MAX_INCLUDES_OPENED);
    assert!(depth >= MAX_INCLUDE_DEPTH);
    let items = read("@@anything\n");
    assert!(
        matches!(&items[0], Read::Failed(message) if message.contains("includes need the script as a file"))
    );
    let items = read_with("@https://example.invalid/x.sql\n", Some(&includes)).unwrap();
    assert!(matches!(&items[0], Read::Failed(message) if message.contains("URL")));
    let items = read_with("@&name\n", Some(&includes)).unwrap();
    assert!(matches!(&items[0], Read::Failed(message) if message.contains("&name")));
}

fn command(name: &str, shown: &str, argument: &str, place: &str) -> SqlPlusStep {
    SqlPlusStep::Command(SqlPlusCommand {
        name: name.to_owned(),
        shown: shown.to_owned(),
        argument: argument.to_owned(),
        place: place.to_owned(),
    })
}

#[test]
fn a_sqlplus_script_hands_its_steps_to_a_caller_that_runs_them() {
    let includes = MemoryIncludes::new(&[("inner.sql", "PROMPT inside\nSET LINESIZE 80\n")]);
    let opener: &dyn IncludeOpener = &includes;
    let mut input: &[u8] = b"DEFINE who = Ada\nPROMPT Hello &who\nSELECT '&who' FROM dual;\nEXECUTE p(&n)\n@@inner\nCONNECT hr/secret\nSPOOL out.log APPEND\nWHENEVER SQLERROR CONTINUE\nEXIT ROLLBACK FAILURE\nSELECT 2 FROM dual;\n";
    let defines = vec![("n".to_owned(), "1".to_owned())];
    let mut script = SqlPlusScript::new(&mut input, Some(opener), &defines).unwrap();
    let mut steps = Vec::new();
    while let Some(step) = script.next_step().unwrap() {
        steps.push(step);
    }
    assert_eq!(
        steps,
        vec![
            command("define", "DEFINE WHO", "", "line 1"),
            SqlPlusStep::Prompt("Hello Ada".to_owned()),
            SqlPlusStep::Statement {
                text: "SELECT 'Ada' FROM dual".to_owned(),
                place: "line 3".to_owned(),
            },
            SqlPlusStep::Statement {
                text: "BEGIN p(1); END;".to_owned(),
                place: "line 4".to_owned(),
            },
            command("include", "@@inner", "inner.sql", "line 5"),
            SqlPlusStep::Prompt("inside".to_owned()),
            command("set", "SET LINESIZE", "", "inner.sql line 2"),
            SqlPlusStep::Connect {
                statement: "ALTER SESSION SET CURRENT_SCHEMA = \"HR\"".to_owned(),
            },
            command("spool", "SPOOL", "out.log APPEND", "line 7"),
            command("whenever", "WHENEVER SQLERROR", "SQLERROR CONTINUE", "line 8"),
            SqlPlusStep::Exit {
                rollback: true,
                status: Some("FAILURE".to_owned()),
                place: "line 9".to_owned(),
            },
        ]
    );
    let mut undefined: &[u8] = b"SELECT &missing FROM dual;\n";
    let mut script = SqlPlusScript::new(&mut undefined, None, &[]).unwrap();
    let error = script.next_step().unwrap_err();
    assert_eq!(error.state(), SqlState::UNDEFINED_PARAMETER);
    assert!(error.message().contains("&missing"), "{error}");
    let mut refused: &[u8] = b"SET SQLTERMINATOR !\n";
    let mut script = SqlPlusScript::new(&mut refused, None, &[]).unwrap();
    assert_eq!(
        script.next_step().unwrap_err().state(),
        SqlState::FEATURE_NOT_SUPPORTED
    );
    let mut empty: &[u8] = b"";
    let too_long = vec![("v".to_owned(), "x".repeat(MAX_VARIABLE_VALUE_BYTES + 1))];
    assert!(SqlPlusScript::new(&mut empty, None, &too_long).is_err());
}

#[test]
fn the_include_tree_is_capped_at_ten_thousand_opens() {
    let includes = MemoryIncludes::new(&[("twice.sql", "@@twice\n@@twice\n")]);
    let items = read_with("@@twice\n", Some(&includes)).unwrap();
    let opened = items
        .iter()
        .filter(|item| matches!(item, Read::Command(name, _) if name == "include"))
        .count();
    assert!(
        opened <= super::sqlplus_reader::MAX_INCLUDES_OPENED,
        "{opened}"
    );
}

fn skip_of(text: &str) -> Skip {
    match classify_oracle(text, &|_| false) {
        Action::Skip(skip) => skip,
        other => panic!("{text}: {other:?}"),
    }
}

#[test]
fn the_classifier_names_oracle_kinds() {
    let run = |text: &str| match classify_oracle(text, &|_| false) {
        Action::Run(effect) => effect,
        other => panic!("{text}: {other:?}"),
    };
    let ignored = |text: &str| match classify_oracle(text, &|_| false) {
        Action::Ignore(Ignored::Other(kind)) => kind,
        other => panic!("{text}: {other:?}"),
    };
    assert_eq!(
        run(
            "CREATE TABLE \"HR\".\"REGIONS\" (\"REGION_ID\" NUMBER CONSTRAINT \"R_NN\" NOT NULL ENABLE, CONSTRAINT r_pk PRIMARY KEY (region_id), x NUMBER REFERENCES p, CONSTRAINT fk FOREIGN KEY (x) REFERENCES p (id), CHECK (x > 0))"
        ),
        Effect::Table {
            name: "hr.regions".to_owned(),
            constraints: 4
        }
    );
    assert_eq!(
        run("CREATE USER hr IDENTIFIED BY x"),
        Effect::User("hr".to_owned())
    );
    assert_eq!(
        run("ALTER SESSION SET CURRENT_SCHEMA=HR"),
        Effect::SessionSetting {
            current_schema: Some("hr".to_owned())
        }
    );
    assert_eq!(
        run("ALTER TABLE departments DISABLE CONSTRAINT dept_mgr_fk"),
        Effect::ToggleConstraint {
            table: "departments".to_owned(),
            target: ToggledConstraint::Named("dept_mgr_fk".to_owned()),
            enable: false,
            novalidate: false
        }
    );
    assert_eq!(
        run("ALTER TABLE t MODIFY CONSTRAINT c ENABLE NOVALIDATE"),
        Effect::ToggleConstraint {
            table: "t".to_owned(),
            target: ToggledConstraint::Named("c".to_owned()),
            enable: true,
            novalidate: true
        }
    );
    assert_eq!(
        run("ALTER TABLE t DISABLE PRIMARY KEY"),
        Effect::ToggleConstraint {
            table: "t".to_owned(),
            target: ToggledConstraint::PrimaryKey,
            enable: false,
            novalidate: false
        }
    );
    assert_eq!(
        run(
            "ALTER TABLE employees ADD ( CONSTRAINT a PRIMARY KEY (x), CONSTRAINT b FOREIGN KEY (y) REFERENCES p )"
        ),
        Effect::AlterTable {
            constraints: 2,
            identity_sequences: Vec::new()
        }
    );
    assert_eq!(
        run("ALTER TABLE t ADD CONSTRAINT c CHECK (a > 0) DISABLE"),
        Effect::DisabledConstraint {
            table: "t".to_owned(),
            name: "c".to_owned()
        }
    );
    assert_eq!(run("DROP TABLE t CASCADE CONSTRAINTS PURGE"), Effect::Drop);
    assert_eq!(
        run("INSERT INTO hr.t VALUES (1)"),
        Effect::Insert("hr.t".to_owned())
    );
    assert_eq!(ignored("SELECT 1 FROM dual"), QUERIES);
    assert_eq!(
        run("ALTER TRIGGER secure_employees DISABLE"),
        Effect::OtherDdl
    );
    assert_eq!(ignored("ALTER TRIGGER secure_employees COMPILE"), PLSQL_SETTINGS);
    assert_eq!(ignored("ALTER PROCEDURE p COMPILE"), PLSQL_SETTINGS);
    assert_eq!(
        ignored("ALTER INDEX \"HR\".\"X\" NOPARALLEL"),
        STORAGE_SETTINGS
    );
    assert_eq!(ignored("ALTER TABLE t NOLOGGING"), STORAGE_SETTINGS);
    assert_eq!(
        ignored("ALTER TABLE t ENABLE ROW MOVEMENT"),
        STORAGE_SETTINGS
    );
    assert_eq!(run("ALTER TABLE t DISABLE ALL TRIGGERS"), Effect::OtherDdl);
    assert_eq!(ignored("ALTER USER hr DEFAULT ROLE ALL"), USERS);
    assert_eq!(run("DROP PROCEDURE p"), Effect::Drop);
    assert_eq!(run("DROP PACKAGE BODY pk"), Effect::Drop);
    assert_eq!(
        ignored("DROP TYPE t"),
        super::classify_oracle::NOTHING_TO_DROP
    );
    assert_eq!(ignored("COMMIT"), "COMMIT");
    assert!(matches!(
        classify_oracle("GRANT SELECT ON t TO u", &|_| false),
        Action::Ignore(Ignored::Grant)
    ));
    for (text, kind, name) in [
        (
            "CREATE OR REPLACE PROCEDURE secure_dml IS",
            "procedure",
            "secure_dml",
        ),
        (
            "CREATE OR REPLACE EDITIONABLE TRIGGER \"HR\".\"T1\" BEFORE",
            "trigger",
            "hr.t1",
        ),
        ("CREATE PACKAGE BODY pk AS", "package body", "pk"),
        ("CREATE OR REPLACE PACKAGE pk IS", "package", "pk"),
        ("CREATE FUNCTION f RETURN NUMBER IS", "function", "f"),
    ] {
        assert_eq!(
            run(text),
            Effect::Routine {
                kind,
                name: name.to_owned()
            },
            "{text}"
        );
    }
    for (text, kind, name) in [
        ("CREATE TYPE t AS OBJECT (a NUMBER)", "type", "t"),
        ("CREATE PUBLIC SYNONYM s FOR t", "synonym", "s"),
        (
            "CREATE DATABASE LINK l CONNECT TO u IDENTIFIED BY p USING 'x'",
            "database link",
            "l",
        ),
        (
            "CREATE MATERIALIZED VIEW mv AS SELECT 1 FROM dual",
            "materialized view",
            "mv",
        ),
        ("CREATE DIRECTORY d AS '/tmp'", "directory", "d"),
    ] {
        let skip = skip_of(text);
        assert_eq!((skip.kind, skip.name.as_str()), (kind, name), "{text}");
    }
    assert_eq!(
        run("DECLARE x NUMBER; BEGIN NULL; END;"),
        Effect::PlsqlBlock
    );
    assert_eq!(run("<<outer>> BEGIN NULL; END;"), Effect::PlsqlBlock);
    assert_eq!(run("BEGIN x := 1; END;"), Effect::PlsqlBlock);
    let supplied = skip_of(
        "BEGIN sys.dbms_logrep_imp.instantiate_schema(schema_name => 'HR'); COMMIT; END;",
    );
    assert_eq!(
        (supplied.kind, supplied.reason),
        (
            "PL/SQL block",
            "it calls an Oracle-supplied package EggDB does not have"
        )
    );
    assert_eq!(
        run("BEGIN DBMS_OUTPUT.PUT_LINE('x'); END;"),
        Effect::PlsqlBlock
    );
    assert_eq!(skip_of("EXECUTE p(1)").kind, "PL/SQL call");
    assert_eq!(run("CALL p(1)"), Effect::Data);
}

#[test]
fn only_blocks_of_plain_sql_statements_run() {
    assert_eq!(
        block_texts(
            "BEGIN\n  INSERT INTO t VALUES ('a;b');\n  UPDATE t SET a = 1;\n  COMMIT;\n  NULL;\nEND;"
        ),
        Some(vec![
            "INSERT INTO t VALUES ('a;b')",
            "UPDATE t SET a = 1",
            "COMMIT"
        ])
    );
    assert!(block_texts("BEGIN\nEND outer;").is_some());
    for text in [
        "BEGIN IF x THEN NULL; END IF; END;",
        "BEGIN x := 1; END;",
        "BEGIN EXECUTE IMMEDIATE 'DROP USER HR CASCADE'; END;",
        "BEGIN INSERT INTO t VALUES (1); EXCEPTION WHEN OTHERS THEN NULL; END;",
        "BEGIN INSERT INTO t VALUES (:new.a); END;",
        "BEGIN INSERT INTO t VALUES (1) RETURNING a INTO x; END;",
        "BEGIN FORALL i IN 1..2 INSERT INTO t VALUES (i); END;",
        "BEGIN UPDATE t SET a = 1; IF SQL%ROWCOUNT = 0 THEN NULL; END IF; END;",
        "BEGIN BEGIN NULL; END; END;",
        "BEGIN INSERT INTO t VALUES (1);",
    ] {
        assert_eq!(anonymous_block_statements(text), None, "{text}");
    }
    let populate = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../eggdb/tests/data/oracle/hr/hr_populate.sql"
    ))
    .unwrap();
    let mut blocks = 0;
    let mut inserts = 0;
    for statement in statements(&populate) {
        if statement.starts_with("BEGIN") {
            blocks += 1;
            inserts += anonymous_block_statements(&statement)
                .unwrap_or_else(|| panic!("not SQL-only: {statement}"))
                .len();
        }
    }
    assert_eq!((blocks, inserts), (7, 216));
}

#[test]
fn oracle_names_fold_to_one_key() {
    let name = |text: &str| {
        let tokens: Vec<Token> = Words::oracle(text).collect();
        let mut at = 0;
        plain_qualified_name(&tokens, &mut at).unwrap()
    };
    let keys: Vec<String> = ["\"HR\".\"EMPLOYEES\"", "HR.employees", "hr.EMPLOYEES"]
        .iter()
        .map(|text| oracle_table_key(&name(text), "public"))
        .collect();
    assert_eq!(keys, vec!["hr.employees"; 3]);
    assert_eq!(oracle_table_key(&name("employees"), "hr"), "hr.employees");
    assert_eq!(name("\"Mixed\""), "Mixed");
    assert_eq!(Words::oracle("q'[it's]' x").count(), 2);
}

#[test]
fn create_table_rewrites_strip_virtual_columns_and_disabled_constraints_and_keep_identities() {
    let rewrite = rewritten_table(
        "CREATE TABLE \"HR\".\"T\" (\"ID\" NUMBER GENERATED BY DEFAULT ON NULL AS IDENTITY MINVALUE 1 MAXVALUE 9999999999999999999999999999 INCREMENT BY 1 START WITH 21 CACHE 20 NOORDER NOCYCLE NOKEEP NOSCALE NOT NULL ENABLE, \"TOTAL\" NUMBER GENERATED ALWAYS AS (\"ID\" * 2) VIRTUAL, \"P\" NUMBER, CONSTRAINT \"T_FK\" FOREIGN KEY (\"P\") REFERENCES \"HR\".\"P\" (\"ID\") DISABLE NOVALIDATE) TABLESPACE \"USERS\"",
    )
    .unwrap();
    assert_eq!(
        rewrite.text,
        "CREATE TABLE \"HR\".\"T\" (\"ID\" NUMBER GENERATED BY DEFAULT ON NULL AS IDENTITY MINVALUE 1 MAXVALUE 9999999999999999999999999999 INCREMENT BY 1 START WITH 21 CACHE 20 NOORDER NOCYCLE NOKEEP NOSCALE NOT NULL ENABLE, \"P\" NUMBER) TABLESPACE \"USERS\""
    );
    let objects: Vec<&str> = rewrite
        .notes
        .iter()
        .map(|note| note.object.as_str())
        .collect();
    assert_eq!(objects, vec!["t.total"]);
    assert_eq!(rewrite.disabled.len(), 1);
    assert_eq!(rewrite.disabled[0].name, "t_fk");
    assert_eq!(rewrite.disabled[0].table, "\"HR\".\"T\"");
    assert!(!rewrite.disabled[0].in_column);
    assert_eq!(
        rewrite.disabled[0].clause,
        "CONSTRAINT \"T_FK\" FOREIGN KEY (\"P\") REFERENCES \"HR\".\"P\" (\"ID\")"
    );
    assert_eq!(
        rewritten_table("CREATE TABLE t (id NUMBER GENERATED AS IDENTITY, a NUMBER)"),
        None
    );
    assert_eq!(
        without_disable("ALTER TABLE t ADD CONSTRAINT c CHECK (a > 0) DISABLE NOVALIDATE"),
        Some("ALTER TABLE t ADD CONSTRAINT c CHECK (a > 0)".to_owned())
    );
}

#[test]
fn constraints_disabled_inside_a_column_move_to_the_table_level() {
    let rewrite = rewritten_table(
        "CREATE TABLE t (id NUMBER CONSTRAINT t_pk PRIMARY KEY, qty NUMBER CONSTRAINT t_qty_ck CHECK (qty > 0) DISABLE NOT NULL, code VARCHAR2(5) UNIQUE DISABLE, p NUMBER CONSTRAINT t_p_fk REFERENCES parent (id) ON DELETE CASCADE DISABLE NOVALIDATE)",
    )
    .unwrap();
    assert_eq!(
        rewrite.text,
        "CREATE TABLE t (id NUMBER CONSTRAINT t_pk PRIMARY KEY, qty NUMBER NOT NULL, code VARCHAR2(5), p NUMBER)"
    );
    let disabled: Vec<(&str, &str, bool)> = rewrite
        .disabled
        .iter()
        .map(|element| {
            (
                element.name.as_str(),
                element.clause.as_str(),
                element.in_column,
            )
        })
        .collect();
    assert_eq!(
        disabled,
        vec![
            ("t_qty_ck", "CONSTRAINT t_qty_ck CHECK (qty > 0)", true),
            ("", "UNIQUE (code)", true),
            (
                "t_p_fk",
                "CONSTRAINT t_p_fk FOREIGN KEY (p) REFERENCES parent (id) ON DELETE CASCADE",
                true
            ),
        ]
    );
    let added = rewritten_added(
        "ALTER TABLE t ADD (note VARCHAR2(9) CONSTRAINT t_note_ck CHECK (note <> 'x') DISABLE, CONSTRAINT t_code_uk UNIQUE (code) DISABLE)",
    )
    .unwrap();
    assert_eq!(added.text, "ALTER TABLE t ADD (note VARCHAR2(9))");
    assert_eq!(added.disabled.len(), 2);
    assert!(added.disabled[0].in_column);
    assert!(!added.disabled[1].in_column);
    assert_eq!(added.disabled[1].clause, "CONSTRAINT t_code_uk UNIQUE (code)");
    let only_disabled =
        rewritten_added("ALTER TABLE t ADD (CONSTRAINT t_code_uk UNIQUE (code) DISABLE)").unwrap();
    assert_eq!(only_disabled.text, "");
    assert_eq!(rewritten_added("ALTER TABLE t ADD (b NUMBER)"), None);
}

#[test]
fn an_identity_the_engine_refuses_is_rewritten_out_and_noted() {
    let refused = SqlError::new(SqlState::FEATURE_NOT_SUPPORTED, "identity options");
    let rewrite = without_refused_identity(
        "CREATE TABLE t (id NUMBER GENERATED ALWAYS AS IDENTITY (MAXVALUE 100 CYCLE) NOT NULL, a NUMBER, CONSTRAINT t_pk PRIMARY KEY (id))",
        &refused,
    )
    .unwrap();
    assert_eq!(
        rewrite.text,
        "CREATE TABLE t (id NUMBER NOT NULL, a NUMBER, CONSTRAINT t_pk PRIMARY KEY (id))"
    );
    assert_eq!(rewrite.notes.len(), 1);
    assert_eq!(rewrite.notes[0].object, "t.id");
    let not_first_key = SqlError::new(SqlState::INVALID_TABLE_DEFINITION, "only one auto column");
    assert!(
        without_refused_identity(
            "CREATE TABLE t (a NUMBER PRIMARY KEY, id NUMBER GENERATED AS IDENTITY)",
            &not_first_key
        )
        .is_some()
    );
    assert!(
        without_refused_identity(
            "CREATE TABLE t (a NUMBER PRIMARY KEY)",
            &not_first_key
        )
        .is_none()
    );
    let other = SqlError::new(SqlState::UNDEFINED_TABLE, "missing");
    assert!(
        without_refused_identity(
            "CREATE TABLE t (id NUMBER GENERATED AS IDENTITY PRIMARY KEY)",
            &other
        )
        .is_none()
    );
}

fn oracle_session_with(store: &MemoryStore, sql: &str) {
    let mut session = Session::with_dialect(store, Dialect::Oracle);
    session
        .execute(sql, &mut |_| {})
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    session.execute("COMMIT", &mut |_| {}).unwrap();
    session.close().unwrap();
}

#[test]
fn constraint_clauses_are_built_from_the_catalog() {
    let store = MemoryStore::default();
    oracle_session_with(
        &store,
        "CREATE TABLE p (id NUMBER PRIMARY KEY, code NUMBER UNIQUE); \
         CREATE TABLE c (id NUMBER, pid NUMBER, code NUMBER, \
           CONSTRAINT c_listed FOREIGN KEY (pid) REFERENCES p (id) ON DELETE CASCADE, \
           CONSTRAINT c_implied FOREIGN KEY (code) REFERENCES p, \
           CONSTRAINT c_check CHECK (id > 0))",
    );
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    let clause = |session: &mut Session<'_, MemoryStore>, name: &str| {
        session
            .oracle_constraint_clause("c", name)
            .unwrap()
            .unwrap()
    };
    let listed = clause(&mut session, "c_listed");
    assert_eq!(listed.kind, ConstraintClauseKind::ForeignKey);
    assert_eq!(listed.table, "\"PUBLIC\".\"C\"");
    assert_eq!(
        listed.clause.as_deref(),
        Some(
            "CONSTRAINT \"C_LISTED\" FOREIGN KEY (\"PID\") REFERENCES \"PUBLIC\".\"P\" (\"ID\") ON DELETE CASCADE"
        )
    );
    let implied = clause(&mut session, "c_implied");
    assert_eq!(
        implied.clause.as_deref(),
        Some(
            "CONSTRAINT \"C_IMPLIED\" FOREIGN KEY (\"CODE\") REFERENCES \"PUBLIC\".\"P\" (\"ID\")"
        )
    );
    let check = clause(&mut session, "c_check");
    assert_eq!(check.kind, ConstraintClauseKind::Check);
    assert_eq!(
        check.clause.as_deref(),
        Some("CONSTRAINT \"C_CHECK\" CHECK (id > 0)")
    );
    assert_eq!(
        session.oracle_constraint_clause("c", "nothing").unwrap(),
        None
    );
    assert!(session.oracle_constraint_clause("missing", "x").is_err());
    session.close().unwrap();
    let mut postgresql = Session::new(&store);
    postgresql
        .execute(
            "CREATE TABLE pg (a int CONSTRAINT pg_check CHECK (a > 0))",
            &mut |_| {},
        )
        .unwrap();
    postgresql.close().unwrap();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    let check = session
        .oracle_constraint_clause("pg", "pg_check")
        .unwrap()
        .unwrap();
    assert_eq!(check.clause, None);
    session.close().unwrap();
}

fn restore(store: &MemoryStore, script: &str, keep_going: bool) -> RestoreReport {
    let mut session = Session::with_dialect(store, Dialect::Oracle);
    let mut input = script.as_bytes();
    let report = restore_dump(
        &mut session,
        &mut input,
        RestoreOptions {
            keep_going,
            ..RestoreOptions::default()
        },
        &mut |_| {},
    );
    assert_eq!(session.close(), Ok(false));
    report
}

#[test]
fn a_restored_trigger_is_created_and_its_disable_after_it_runs() {
    let store = MemoryStore::default();
    let report = restore(
        &store,
        "CREATE TABLE t (a NUMBER);\nCREATE OR REPLACE TRIGGER x BEFORE INSERT ON t\nBEGIN\n  NULL;\nEND;\n/\nALTER TRIGGER x DISABLE;\n",
        false,
    );
    assert!(report.succeeded(), "{report:?}");
    assert!(report.consequences.is_empty());
    assert!(report.ignored.other.is_empty(), "{report:?}");
    assert!(report.skipped.is_empty(), "{report:?}");
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    let mut statuses = Vec::new();
    session
        .execute(
            "SELECT trigger_name, status FROM user_triggers",
            &mut |result: QueryResult| {
                for row in result.rows {
                    statuses.push(
                        row.iter()
                            .filter_map(|value| value.to_text())
                            .collect::<Vec<_>>()
                            .join("|"),
                    );
                }
            },
        )
        .unwrap();
    assert_eq!(statuses, vec!["X|DISABLED".to_owned()]);
}

#[test]
fn restored_plsql_runs_and_a_unit_that_does_not_compile_is_listed_without_stopping() {
    let store = MemoryStore::default();
    let report = restore(
        &store,
        "CREATE TABLE t (a NUMBER);\n\
         CREATE OR REPLACE PROCEDURE add_one(p NUMBER) IS\nBEGIN\n  INSERT INTO t VALUES (p + 1);\nEND;\n/\n\
         CREATE OR REPLACE PROCEDURE broken IS\nBEGIN\n  missing_variable := 1;\nEND;\n/\n\
         DECLARE\n  n NUMBER := 41;\nBEGIN\n  add_one(n);\nEND;\n/\n\
         BEGIN\n  IF '&never_defined' IS NULL THEN\n    NULL;\n  END IF;\nEND;\n/\n",
        false,
    );
    assert!(report.succeeded(), "{report:?}");
    let kinds: Vec<(&str, &str)> = report
        .skipped
        .iter()
        .map(|skipped| (skipped.kind.as_str(), skipped.reason.as_str()))
        .collect();
    assert_eq!(kinds.len(), 2, "{report:?}");
    assert_eq!(report.skipped[0].kind, "procedure");
    assert_eq!(report.skipped[0].name, "broken");
    assert_eq!(
        report.skipped[0].detail.as_deref(),
        Some(super::classify_oracle::NOT_COMPILED)
    );
    assert_eq!(
        kinds[1],
        ("PL/SQL block", super::classify_oracle::PROMPTED_BLOCK)
    );
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    let mut values = Vec::new();
    session
        .execute("SELECT a FROM t", &mut |result: QueryResult| {
            for row in result.rows {
                values.extend(row.iter().filter_map(|value| value.to_text()));
            }
        })
        .unwrap();
    assert_eq!(values, vec!["42".to_owned()]);
}

#[test]
fn a_drop_of_a_missing_object_has_nothing_to_drop_and_unknown_session_parameters_are_named() {
    let store = MemoryStore::default();
    let report = restore(
        &store,
        "DROP TABLE nothing CASCADE CONSTRAINTS;\nDROP SEQUENCE nothing_seq;\nALTER SESSION SET EVENTS '10150 TRACE NAME CONTEXT FOREVER, LEVEL 1';\nALTER SESSION SET NLS_LANGUAGE=American;\nCREATE TABLE t (a NUMBER);\n",
        false,
    );
    assert!(report.succeeded(), "{report:?}");
    assert_eq!(report.tables, 1);
    assert_eq!(report.ignored.set, vec!["EVENTS".to_owned()]);
    assert_eq!(
        report.ignored.other,
        vec![(super::classify_oracle::NOTHING_TO_DROP.to_owned(), 2)]
    );
}

#[test]
fn a_block_into_a_skipped_table_counts_its_rows_and_runs_the_others() {
    let store = MemoryStore::default();
    let report = restore(
        &store,
        "CREATE TABLE docs (id NUMBER, body VARCHAR2(10)) ORGANIZATION EXTERNAL (TYPE oracle_loader DEFAULT DIRECTORY d LOCATION ('docs.csv'));\nCREATE TABLE notes (id NUMBER);\nBEGIN\n  INSERT INTO docs VALUES (1, NULL);\n  INSERT INTO notes VALUES (1);\nEND;\n/\n",
        false,
    );
    assert!(report.succeeded(), "{report:?}");
    assert_eq!((report.tables, report.tables_skipped), (1, 1));
    assert_eq!((report.rows, report.rows_skipped), (1, 1));
}

#[test]
fn errors_and_reports_never_show_a_password() {
    let store = MemoryStore::default();
    let report = restore(
        &store,
        "CONNECT scott/tiger\nDEFINE pw = hunter2\nCREATE TABLE t (a NUMBER IDENTIFIED BY hunter2);\n",
        true,
    );
    let shown = format!("{report:?}");
    assert!(!shown.contains("tiger"), "{shown}");
    assert!(!shown.contains("hunter2"), "{shown}");
    assert_eq!(report.error_count(), 1);
}

#[test]
fn identities_column_disabled_constraints_and_using_index_restore() {
    let store = MemoryStore::default();
    let report = restore(
        &store,
        "CREATE TABLE parent (id NUMBER PRIMARY KEY);
CREATE TABLE t (id NUMBER GENERATED ALWAYS AS IDENTITY START WITH 10 PRIMARY KEY, qty NUMBER CONSTRAINT t_qty_ck CHECK (qty > 0) DISABLE, p NUMBER CONSTRAINT t_p_fk REFERENCES parent (id) DISABLE);
INSERT INTO t (id, qty, p) VALUES (1, -5, 7);
INSERT INTO t (id, qty, p) VALUES (2, 3, NULL);
ALTER TABLE t ENABLE CONSTRAINT t_p_fk;
CREATE TABLE w (id NUMBER GENERATED AS IDENTITY (MAXVALUE 100), a NUMBER);
INSERT INTO w VALUES (5, 1);
CREATE TABLE emp (id NUMBER(6), email VARCHAR2(25));
CREATE UNIQUE INDEX emp_id_ix ON emp (id);
ALTER TABLE emp ADD CONSTRAINT emp_pk PRIMARY KEY (id) USING INDEX emp_id_ix ENABLE;
ALTER TABLE emp ADD (email2 VARCHAR2(5) CONSTRAINT emp_e2_ck CHECK (email2 IS NOT NULL) DISABLE);
",
        false,
    );
    assert!(report.succeeded(), "{report:?}");
    assert_eq!(report.tables, 4);
    assert_eq!(report.rows, 3);
    assert_eq!(report.constraints, 3, "{report:?}");
    let mut objects: Vec<&str> = report
        .differences
        .iter()
        .map(|difference| difference.object.as_str())
        .collect();
    objects.sort_unstable();
    assert_eq!(objects, vec!["emp.emp_e2_ck", "t.t_qty_ck", "t_p_fk", "w.id"]);
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    session
        .execute("INSERT INTO t (qty) VALUES (4)", &mut |_| {})
        .unwrap();
    let mut ids = Vec::new();
    session
        .execute("SELECT id FROM t ORDER BY id", &mut |result| {
            for row in &result.rows {
                ids.push(row[0].to_text().unwrap_or_default());
            }
        })
        .unwrap();
    assert_eq!(ids, vec!["1", "2", "10"]);
    let given = session
        .execute("INSERT INTO t (id, qty) VALUES (99, 1)", &mut |_| {})
        .unwrap_err();
    assert_eq!(given.vendor_code(), Some(32795));
    let mut indexes = Vec::new();
    session
        .execute(
            "SELECT index_name FROM user_indexes WHERE table_name = 'EMP'",
            &mut |result| indexes.extend(result.rows.iter().map(|row| row[0].clone())),
        )
        .unwrap();
    assert!(indexes.is_empty(), "{indexes:?}");
    session.execute("ROLLBACK", &mut |_| {}).unwrap();
    session.close().unwrap();
}

#[test]
fn seeded_random_scripts_never_panic() {
    let cases: usize = std::env::var("EGGDB_FUZZ_CASES")
        .ok()
        .and_then(|cases| cases.parse().ok())
        .unwrap_or(2000);
    let mut state: u64 = std::env::var("EGGDB_FUZZ_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or(0x0043_5eed);
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let pieces = [
        ";",
        "\n",
        "/\n",
        ".\n",
        "q'[",
        "]'",
        "q'{",
        "}'",
        "nq'<",
        ">'",
        "q'!",
        "'",
        "\"",
        "--",
        "/*",
        "*/",
        "&",
        "&&",
        "&x.",
        "DEFINE x = 1\n",
        "SET DEFINE OFF\n",
        "SET DEFINE ON\n",
        "@@inc\n",
        "@inc\n",
        "START inc\n",
        "BEGIN ",
        "END;",
        "DECLARE ",
        "CREATE PROCEDURE p IS ",
        "INSERT INTO t VALUES (1)",
        "SELECT 1 FROM dual",
        "\r",
        "\0",
        "<<l>>",
        "EXIT\n",
        "REM -\n",
        "CONNECT a/b\n",
        "x",
        " ",
        "(",
        ")",
        "COMMIT",
        "NULL",
        ":a",
        "%",
    ];
    let includes = MemoryIncludes::new(&[("inc.sql", "@@inc\nSELECT 1 FROM dual;\n")]);
    for _ in 0..cases {
        let mut text = String::new();
        for _ in 0..next() % 24 {
            text.push_str(pieces[next() as usize % pieces.len()]);
        }
        if let Ok(items) = read_with(&text, Some(&includes)) {
            for item in items {
                if let Read::Statement(statement, _) = item {
                    let _ = classify_oracle(&statement, &|_| false);
                    let _ = anonymous_block_statements(&statement);
                    let _ = rewritten_table(&statement);
                    let _ = rewritten_added(&statement);
                    let _ = without_disable(&statement);
                    let _ = without_refused_identity(
                        &statement,
                        &SqlError::new(SqlState::FEATURE_NOT_SUPPORTED, "x"),
                    );
                }
            }
        }
        let _ = classify_oracle(&text, &|_| false);
        let _ = anonymous_block_statements(&text);
        let _ = rewritten_table(&text);
        let _ = rewritten_added(&text);
    }
}

#[test]
fn ignored_effects_are_reported_in_oracle_words() {
    assert!(matches!(
        classify_oracle("SAVEPOINT a", &|_| false),
        Action::Ignore(Ignored::Other("SAVEPOINT"))
    ));
    assert!(matches!(
        classify_oracle("WITH x AS (SELECT 1 FROM dual) SELECT * FROM x", &|_| false),
        Action::Ignore(Ignored::Other(QUERIES))
    ));
}

fn script_steps(text: &str) -> Result<Vec<SqlPlusStep>, SqlError> {
    let mut input = text.as_bytes();
    let mut script = SqlPlusScript::new(&mut input, None, &[])?;
    let mut steps = Vec::new();
    while let Some(step) = script.next_step()? {
        steps.push(step);
    }
    Ok(steps)
}

#[test]
fn set_serveroutput_reaches_the_caller_as_its_own_step() {
    let steps = script_steps(
        "SET SERVEROUTPUT ON\nset serverout on size 5000 format wrapped\nSET SERVEROUTPUT ON SIZE UNLIMITED\nSET SERVEROUTPUT OFF\nSET SERVEROUTPUT OFF SIZE 3000\n",
    )
    .unwrap();
    assert_eq!(
        steps,
        vec![
            SqlPlusStep::ServerOutput(ServerOutputSetting::On { buffer_bytes: None }),
            SqlPlusStep::ServerOutput(ServerOutputSetting::On {
                buffer_bytes: Some(5000)
            }),
            SqlPlusStep::ServerOutput(ServerOutputSetting::On { buffer_bytes: None }),
            SqlPlusStep::ServerOutput(ServerOutputSetting::Off),
            SqlPlusStep::ServerOutput(ServerOutputSetting::Off),
        ]
    );
    assert_eq!(
        script_steps("SET SERVER ON\n").unwrap(),
        vec![command("set", "SET SERVER", "", "line 1")]
    );
}

#[test]
fn a_bad_set_serveroutput_stops_the_script_with_its_line() {
    let error = script_steps("PROMPT x\n\nSET SERVEROUTPUT ON SIZE 500\n").unwrap_err();
    assert_eq!(error.state(), SqlState::INVALID_PARAMETER_VALUE);
    assert_eq!(
        error.message(),
        "line 3: SET SERVEROUTPUT SIZE 500 is out of range: SIZE is 2000 to 1000000, or UNLIMITED"
    );
    assert_eq!(
        error.hint(),
        Some("SET SERVEROUTPUT ON alone keeps every line, up to EggDB's 1 MiB buffer.")
    );
    let error = script_steps("SET SERVEROUTPUT MAYBE\n").unwrap_err();
    assert_eq!(error.state(), SqlState::SYNTAX_ERROR);
    assert!(
        error
            .message()
            .starts_with("line 1: SET SERVEROUTPUT does not take 'MAYBE': the form is SET SERVEROUTPUT ON|OFF"),
        "{error}"
    );
}

#[test]
fn server_output_settings_read_like_sqlplus() {
    assert_eq!(
        ServerOutputSetting::parse("on").unwrap(),
        ServerOutputSetting::On { buffer_bytes: None }
    );
    assert_eq!(
        ServerOutputSetting::parse("ON FOR TRU SIZE 1000000").unwrap(),
        ServerOutputSetting::On {
            buffer_bytes: Some(1_000_000)
        }
    );
    assert_eq!(
        ServerOutputSetting::parse("ON SIZE unl").unwrap(),
        ServerOutputSetting::On { buffer_bytes: None }
    );
    assert_eq!(
        ServerOutputSetting::parse("off").unwrap(),
        ServerOutputSetting::Off
    );
    for (argument, state, part) in [
        ("", SqlState::SYNTAX_ERROR, "does not take nothing after SERVEROUTPUT"),
        ("ON SIZE", SqlState::SYNTAX_ERROR, "does not take nothing after SIZE"),
        ("ON FORMAT WIDE", SqlState::SYNTAX_ERROR, "does not take 'WIDE'"),
        ("ON SIZE lots", SqlState::INVALID_PARAMETER_VALUE, "SIZE lots is out of range"),
        ("ON SIZE 1000001", SqlState::INVALID_PARAMETER_VALUE, "out of range"),
        ("ON SIZE 1999", SqlState::INVALID_PARAMETER_VALUE, "out of range"),
        ("ON WRAPPED", SqlState::SYNTAX_ERROR, "does not take 'WRAPPED'"),
        ("YES", SqlState::SYNTAX_ERROR, "does not take 'YES'"),
    ] {
        let error = ServerOutputSetting::parse(argument).unwrap_err();
        assert_eq!(error.state(), state, "{argument}");
        assert!(error.message().contains(part), "{argument}: {error}");
        assert!(error.hint().is_some(), "{argument}");
    }
}

#[test]
fn a_plsql_unit_is_told_from_sql_by_its_first_words() {
    assert_eq!(starts_plsql_unit("BEGIN"), Some(true));
    assert_eq!(starts_plsql_unit("  declare\n  n NUMBER;"), Some(true));
    assert_eq!(starts_plsql_unit("<<outer>> BEGIN NULL; END;"), Some(true));
    assert_eq!(
        starts_plsql_unit("CREATE OR REPLACE EDITIONABLE PROCEDURE p AS"),
        Some(true)
    );
    assert_eq!(starts_plsql_unit("create package body pkg is"), Some(true));
    assert_eq!(starts_plsql_unit("CREATE TABLE t (n NUMBER);"), Some(false));
    assert_eq!(starts_plsql_unit("SELECT 1 FROM dual;"), Some(false));
    assert_eq!(starts_plsql_unit("CREATE OR REPLACE"), None);
    assert_eq!(starts_plsql_unit(""), None);
}

#[test]
fn a_restore_lists_set_serveroutput_as_a_client_command() {
    let reason = super::classify_oracle::sqlplus_command_reason("set serveroutput");
    assert_eq!(reason, "a restore prints no server output");
}
