use super::{StoredPackage, stored_package};
use crate::ast::Statement;
use crate::lexer::tokenize_in;
use crate::memory_store::MemoryStore;
use crate::parser::Parser;
use crate::routines::{ParameterMode, RoutineKind, RoutineLanguage, parse_definition};
use crate::{Dialect, QueryResult, Session, SqlError};

fn session(store: &MemoryStore) -> Session<'_, MemoryStore> {
    Session::with_dialect(store, Dialect::Oracle)
}

fn tag(session: &mut Session<'_, MemoryStore>, text: &str) -> String {
    let mut last = None;
    session
        .execute(text, &mut |result: QueryResult| last = Some(result.tag))
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    last.unwrap_or_default()
}

fn failure(session: &mut Session<'_, MemoryStore>, text: &str) -> SqlError {
    session
        .execute(text, &mut |_| {})
        .expect_err("the statement fails")
}

fn oracle_code(session: &mut Session<'_, MemoryStore>, text: &str) -> Option<u16> {
    failure(session, text).vendor_code()
}

fn statements(text: &str) -> Vec<Statement> {
    let tokens = tokenize_in(text, Dialect::Oracle).unwrap();
    let mut parser = Parser::new(text, &tokens).in_dialect(Dialect::Oracle);
    let mut found = Vec::new();
    parser.skip_empty_statements();
    while !parser.at_end() {
        found.push(parser.statement().unwrap_or_else(|error| panic!("{text}: {error}")));
        parser.skip_empty_statements();
    }
    found
}

fn package_of(store: &MemoryStore, name: &str) -> Option<StoredPackage> {
    let mut reader = session(store);
    reader.read(|reader, _| stored_package(reader, name)).unwrap()
}

const ADD_JOB_HISTORY: &str = "CREATE OR REPLACE PROCEDURE add_job_history\n  ( p_emp_id job_history.employee_id%type\n  , p_job_id IN VARCHAR2\n  )\nIS\nBEGIN\n  NULL;\nEND add_job_history;";

const COUNTER_SPEC: &str =
    "CREATE PACKAGE counter AS\n  total NUMBER := 0;\n  PROCEDURE bump(amount IN NUMBER);\nEND counter;";

const COUNTER_BODY: &str = "CREATE PACKAGE BODY counter AS\n  PROCEDURE bump(amount IN NUMBER) IS\n  BEGIN\n    total := total + amount;\n  END bump;\nEND counter;";

#[test]
fn a_created_procedure_keeps_its_create_text_and_its_unit_as_the_body() {
    let definition = parse_definition(ADD_JOB_HISTORY, Dialect::Oracle).unwrap();
    assert_eq!(definition.kind, RoutineKind::Procedure);
    assert_eq!(definition.language, RoutineLanguage::PlSql);
    assert_eq!(definition.dialect, Dialect::Oracle);
    assert_eq!(definition.text, ADD_JOB_HISTORY);
    assert!(definition.body.starts_with("PROCEDURE add_job_history"));
    assert!(definition.body.ends_with("END add_job_history;"));
    assert_eq!(definition.parameters.len(), 2);
    assert_eq!(definition.parameters[1].mode, ParameterMode::In);
    assert_eq!(
        definition.identity(),
        "JOB_HISTORY.EMPLOYEE_ID%TYPE, VARCHAR2"
    );
}

#[test]
fn plsql_units_end_at_their_own_semicolon_and_the_slash_after_them() {
    let found = statements(
        "BEGIN NULL; END;\n/\n<<outer>> BEGIN NULL; END outer;\n\
         CREATE EDITIONABLE PROCEDURE p AS BEGIN NULL; END p;\n/\n\
         CREATE PACKAGE k AS v NUMBER; END k;\nSELECT 1 FROM dual",
    );
    assert_eq!(found.len(), 5);
    assert!(matches!(found[0], Statement::PlsqlBlock(_)));
    assert!(matches!(found[1], Statement::PlsqlBlock(_)));
    assert!(matches!(&found[2], Statement::CreateRoutine(create) if !create.replace));
    assert!(matches!(&found[3], Statement::CreatePackage(create) if !create.package.body));
    assert!(matches!(found[4], Statement::Select(_) | Statement::Query(_)));
}

#[test]
fn oracle_call_takes_positional_and_named_arguments() {
    let found = statements("CALL add_job_history(100, p_job_id => 'IT_PROG')");
    let [Statement::Call(call)] = found.as_slice() else {
        panic!("one CALL statement: {found:?}");
    };
    assert_eq!(call.arguments.len(), 2);
    assert_eq!(
        call.argument_names,
        vec![None, Some("p_job_id".to_owned())]
    );
}

#[test]
fn drop_and_alter_name_the_unit_kind() {
    let found = statements(
        "DROP PROCEDURE IF EXISTS p; DROP FUNCTION f; DROP PACKAGE BODY k; \
         ALTER PROCEDURE p COMPILE DEBUG PLSQL_OPTIMIZE_LEVEL = 2 REUSE SETTINGS; \
         ALTER FUNCTION f NONEDITIONABLE; ALTER PACKAGE k COMPILE SPECIFICATION",
    );
    assert!(matches!(&found[0], Statement::DropRoutine(drop)
        if drop.if_exists && drop.kind == Some(RoutineKind::Procedure)));
    assert!(matches!(&found[1], Statement::DropRoutine(drop)
        if !drop.if_exists && drop.kind == Some(RoutineKind::Function)));
    assert!(matches!(&found[2], Statement::DropPackage(drop) if drop.body));
    assert!(matches!(&found[3], Statement::AlterRoutine(_)));
    assert!(matches!(&found[4], Statement::AlterRoutine(_)));
    assert!(matches!(&found[5], Statement::AlterPackage(alter)
        if alter.part == crate::ast::PackagePart::Specification));
}

#[test]
fn one_routine_per_name_and_names_shared_with_tables_and_packages() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    tag(&mut session, "CREATE TABLE job_history (employee_id NUMBER(6))");
    assert_eq!(tag(&mut session, ADD_JOB_HISTORY), "CREATE PROCEDURE");
    assert_eq!(tag(&mut session, ADD_JOB_HISTORY), "CREATE PROCEDURE");
    let without_replace = ADD_JOB_HISTORY.replacen("CREATE OR REPLACE", "CREATE", 1);
    assert_eq!(oracle_code(&mut session, &without_replace), Some(955));
    assert_eq!(
        oracle_code(
            &mut session,
            "CREATE OR REPLACE FUNCTION add_job_history RETURN NUMBER IS BEGIN RETURN 1; END;"
        ),
        Some(955)
    );
    assert_eq!(
        oracle_code(
            &mut session,
            "CREATE PROCEDURE job_history AS BEGIN NULL; END;"
        ),
        Some(955)
    );
    tag(&mut session, COUNTER_SPEC);
    assert_eq!(
        oracle_code(&mut session, "CREATE PROCEDURE counter AS BEGIN NULL; END;"),
        Some(955)
    );
    assert_eq!(
        oracle_code(
            &mut session,
            "CREATE PACKAGE add_job_history AS v NUMBER; END add_job_history;"
        ),
        Some(955)
    );
}

#[test]
fn a_unit_that_does_not_compile_is_refused_with_its_line_and_column() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    let error = failure(
        &mut session,
        "CREATE PROCEDURE p AS\nBEGIN\n  missing := 1;\nEND p;",
    );
    assert_eq!(error.vendor_code(), Some(6550));
    assert!(error.message().starts_with("line 3, column 3:\n"), "{}", error.message());
    assert!(error.message().contains("PLS-00201"), "{}", error.message());
    assert_eq!(
        oracle_code(&mut session, "DROP PROCEDURE p"),
        Some(4043)
    );
}

#[test]
fn dropping_and_compiling_find_the_unit_of_the_named_kind() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    tag(&mut session, "CREATE PROCEDURE p AS BEGIN NULL; END p;");
    assert_eq!(oracle_code(&mut session, "DROP FUNCTION p"), Some(4043));
    assert_eq!(oracle_code(&mut session, "ALTER FUNCTION p COMPILE"), Some(4043));
    assert_eq!(tag(&mut session, "ALTER PROCEDURE p COMPILE"), "ALTER PROCEDURE");
    assert_eq!(tag(&mut session, "ALTER PROCEDURE p EDITIONABLE"), "ALTER PROCEDURE");
    assert_eq!(tag(&mut session, "DROP PROCEDURE p"), "DROP PROCEDURE");
    assert_eq!(oracle_code(&mut session, "DROP PROCEDURE p"), Some(4043));
    assert_eq!(tag(&mut session, "DROP PROCEDURE IF EXISTS p"), "DROP PROCEDURE");
    let error = failure(&mut session, "DROP PROCEDURE p");
    assert_eq!(error.message(), "object P does not exist");
}

#[test]
fn a_package_body_needs_its_specification_and_compiles_against_it() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    let error = failure(&mut session, COUNTER_BODY);
    assert_eq!(error.vendor_code(), Some(6550));
    assert!(error.message().contains("PLS-00304"), "{}", error.message());
    assert_eq!(tag(&mut session, COUNTER_SPEC), "CREATE PACKAGE");
    assert_eq!(oracle_code(&mut session, COUNTER_SPEC), Some(955));
    assert_eq!(tag(&mut session, COUNTER_BODY), "CREATE PACKAGE BODY");
    assert_eq!(oracle_code(&mut session, COUNTER_BODY), Some(955));
    let replaced_body = COUNTER_BODY.replacen("CREATE", "CREATE OR REPLACE", 1);
    assert_eq!(tag(&mut session, &replaced_body), "CREATE PACKAGE BODY");
    let stored = package_of(&store, "counter").expect("the package is stored");
    assert_eq!(stored.specification, COUNTER_SPEC);
    assert_eq!(stored.body.as_deref(), Some(replaced_body.as_str()));
    let replaced_spec = COUNTER_SPEC.replacen("CREATE", "CREATE OR REPLACE", 1);
    tag(&mut session, &replaced_spec);
    let stored = package_of(&store, "counter").expect("the package is stored");
    assert_eq!(stored.specification, replaced_spec);
    assert!(stored.body.is_some(), "replacing the specification keeps the body");
}

#[test]
fn package_bodies_and_packages_drop_and_compile_separately() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    tag(&mut session, COUNTER_SPEC);
    tag(&mut session, COUNTER_BODY);
    assert_eq!(tag(&mut session, "ALTER PACKAGE counter COMPILE"), "ALTER PACKAGE");
    assert_eq!(tag(&mut session, "ALTER PACKAGE counter COMPILE BODY"), "ALTER PACKAGE");
    assert_eq!(tag(&mut session, "DROP PACKAGE BODY counter"), "DROP PACKAGE BODY");
    assert_eq!(
        package_of(&store, "counter").map(|package| package.body),
        Some(None)
    );
    assert_eq!(
        oracle_code(&mut session, "ALTER PACKAGE counter COMPILE BODY"),
        Some(4043)
    );
    assert_eq!(oracle_code(&mut session, "DROP PACKAGE BODY counter"), Some(4043));
    assert_eq!(tag(&mut session, "DROP PACKAGE counter"), "DROP PACKAGE");
    assert_eq!(package_of(&store, "counter"), None);
    assert_eq!(oracle_code(&mut session, "DROP PACKAGE counter"), Some(4043));
    assert_eq!(tag(&mut session, "DROP PACKAGE IF EXISTS counter"), "DROP PACKAGE");
}

#[test]
fn an_anonymous_block_runs_as_one_statement() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    assert_eq!(tag(&mut session, "BEGIN NULL; END;"), "PL/SQL BLOCK");
    assert_eq!(
        tag(&mut session, "DECLARE\n  n NUMBER := 1;\nBEGIN\n  n := n + 1;\nEND;\n/"),
        "PL/SQL BLOCK"
    );
}

#[test]
fn hostile_plsql_ddl_is_refused_without_panicking() {
    let store = MemoryStore::new();
    let mut session = session(&store);
    for text in [
        "CREATE PROCEDURE",
        "CREATE OR REPLACE PACKAGE BODY",
        "CREATE PROCEDURE p AS BEGIN",
        "CREATE PACKAGE p AS PROCEDURE q IS BEGIN NULL; END; END p;",
        "DROP PACKAGE",
        "DROP PROCEDURE IF",
        "ALTER PACKAGE p COMPILE BODY x =",
        "ALTER PROCEDURE p RECOMPILE",
        "CALL",
        "CALL p",
        "BEGIN",
        "DECLARE",
        "<<",
        "<< x >> SELECT 1 FROM dual",
        "BEGIN NULL; END; garbage",
    ] {
        assert!(session.execute(text, &mut |_| {}).is_err(), "{text}");
    }
}
