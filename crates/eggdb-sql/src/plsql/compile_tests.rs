use super::{
    COMPILATION_ERROR_CODE, DeclaredType, PlsqlPackage, PlsqlUnit, anonymous_block_definition,
    compile, compile_package, line_and_column,
};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlState};
use crate::lexer::tokenize_in;
use crate::parser::Parser;
use crate::plpgsql::{
    CollectionMethod, DeclarationKind, ExceptionCondition, ForallBounds, LoopKind, OpenSource,
    Program, QuerySource, RaisedException, StepKind, TargetPart, TypeDeclaration, VariableType,
};
use crate::routines::{ParameterMode, ReturnType, RoutineKind, RoutineReturns, Volatility};
use crate::types::SqlType;

fn program(code: &str) -> Program {
    match compile(&anonymous_block_definition(code)) {
        Ok(program) => program,
        Err(error) => panic!("{code}: {error}"),
    }
}

fn failure(code: &str) -> SqlError {
    match compile(&anonymous_block_definition(code)) {
        Ok(program) => panic!("{code} compiled: {program:?}"),
        Err(error) => error,
    }
}

fn steps(code: &str) -> Vec<StepKind> {
    program(code)
        .block
        .steps
        .into_iter()
        .map(|step| step.kind)
        .collect()
}

fn created(sql: &str) -> PlsqlUnit {
    let tokens = match tokenize_in(sql, Dialect::Oracle) {
        Ok(tokens) => tokens,
        Err(error) => panic!("{sql}: {error}"),
    };
    let mut parser = Parser::new(sql, &tokens).in_dialect(Dialect::Oracle);
    match parser.plsql_subprogram() {
        Ok(unit) => unit,
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn package(text: &str, spec: Option<&PlsqlPackage>) -> PlsqlPackage {
    match compile_package(text, spec.map(|spec| &spec.block)) {
        Ok(package) => package,
        Err(error) => panic!("{text}: {error}"),
    }
}

fn assert_compilation_error(error: &SqlError, message: &str) {
    assert_eq!(error.vendor_code(), Some(COMPILATION_ERROR_CODE), "{error}");
    assert_eq!(error.message(), message);
}

#[test]
fn an_anonymous_block_declares_variables_and_assigns_them() {
    let compiled =
        program("DECLARE v NUMBER := 1; c CONSTANT VARCHAR2(10) := 'x'; BEGIN v := v + 1; END;");
    assert_eq!(compiled.block.declarations.len(), 2);
    let DeclarationKind::Variable(constant) = &compiled.block.declarations[1].kind else {
        panic!("{compiled:?}");
    };
    assert!(constant.constant);
    assert_eq!(constant.variable_type, VariableType::Sql(SqlType::Varchar(Some(10))));
    let StepKind::Assign { target, value } = &compiled.block.steps[0].kind else {
        panic!("{compiled:?}");
    };
    assert_eq!(target.name, "v");
    assert_eq!(value.text, "v + 1");
}

#[test]
fn select_into_is_cut_out_of_the_statement_and_is_exact() {
    let found = steps("DECLARE n NUMBER; BEGIN SELECT COUNT(*) INTO n FROM employees; END;");
    let StepKind::Sql {
        statement,
        into: Some(into),
    } = &found[0]
    else {
        panic!("{found:?}");
    };
    assert_eq!(statement.text, "SELECT COUNT(*) FROM employees");
    assert!(into.strict && !into.bulk);
    assert_eq!(into.targets[0].name, "n");
}

#[test]
fn bulk_collect_into_fills_a_collection() {
    let compiled = program(
        "DECLARE TYPE ids_t IS TABLE OF NUMBER; ids ids_t; BEGIN SELECT id BULK COLLECT INTO ids FROM jobs; END;",
    );
    assert!(matches!(
        &compiled.block.declarations[0].kind,
        DeclarationKind::Type(TypeDeclaration::Table { index: None, .. })
    ));
    let DeclarationKind::Variable(ids) = &compiled.block.declarations[1].kind else {
        panic!("{compiled:?}");
    };
    assert_eq!(ids.variable_type, VariableType::Named(vec!["ids_t".to_owned()]));
    let StepKind::Sql {
        statement,
        into: Some(into),
    } = &compiled.block.steps[0].kind
    else {
        panic!("{compiled:?}");
    };
    assert_eq!(statement.text, "SELECT id FROM jobs");
    assert!(into.bulk && !into.strict);
}

#[test]
fn dml_returning_into_keeps_the_returning_clause() {
    let found = steps(
        "DECLARE i NUMBER; BEGIN INSERT INTO t (a) VALUES (1) RETURNING id INTO i; END;",
    );
    let StepKind::Sql {
        statement,
        into: Some(into),
    } = &found[0]
    else {
        panic!("{found:?}");
    };
    assert_eq!(statement.text, "INSERT INTO t (a) VALUES (1) RETURNING id");
    assert!(!into.strict);
}

#[test]
fn procedure_call_statements_become_calls() {
    let found = steps("BEGIN p; pkg.q(1, 'a'); dbms_output.put_line('hi'); END;");
    let calls: Vec<&str> = found
        .iter()
        .map(|step| match step {
            StepKind::Call(call) => call.text.as_str(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        calls,
        ["CALL p()", "CALL pkg.q(1, 'a')", "CALL dbms_output.put_line('hi')"]
    );
}

#[test]
fn raise_application_error_has_its_own_step() {
    let found = steps("BEGIN raise_application_error(-20001, 'bad salary'); END;");
    let StepKind::ApplicationError {
        number,
        message,
        keep,
    } = &found[0]
    else {
        panic!("{found:?}");
    };
    assert_eq!(number.text, "-20001");
    assert_eq!(message.text, "'bad salary'");
    assert!(keep.is_none());
    let error = failure("BEGIN raise_application_error(-20001); END;");
    assert!(error.message().contains("PLS-00306"), "{error}");
}

#[test]
fn user_exceptions_are_raised_and_handled_by_identity_or_number() {
    let compiled = program(
        "DECLARE e EXCEPTION; e2 EXCEPTION; PRAGMA EXCEPTION_INIT(e2, -20002); \
         BEGIN RAISE e; \
         EXCEPTION WHEN no_data_found OR e2 THEN NULL; WHEN e THEN RAISE; WHEN OTHERS THEN NULL; END;",
    );
    assert_eq!(
        compiled.block.declarations[1].kind,
        DeclarationKind::Exception { code: Some(20002) }
    );
    let StepKind::RaiseException(RaisedException::Declared(identity)) =
        &compiled.block.steps[0].kind
    else {
        panic!("{compiled:?}");
    };
    let handlers = &compiled.block.handlers;
    assert_eq!(
        handlers[0].conditions,
        [ExceptionCondition::Oracle(1403), ExceptionCondition::Oracle(20002)]
    );
    assert_eq!(
        handlers[1].conditions,
        [ExceptionCondition::Declared(identity.clone())]
    );
    assert_eq!(handlers[1].steps[0].kind, StepKind::Reraise);
    assert_eq!(handlers[2].conditions, [ExceptionCondition::Others]);
    assert!(handlers[0].catches_oracle(Some(1403), None));
    assert!(handlers[1].catches_oracle(Some(6510), Some(identity.as_str())));
    assert!(!handlers[1].catches_oracle(Some(6510), Some("other.e@1")));
}

#[test]
fn an_unknown_exception_is_not_declared() {
    let error = failure("BEGIN NULL; EXCEPTION WHEN not_declared THEN NULL; END;");
    assert_compilation_error(
        &error,
        "line 1, column 28:\nPLS-00201: identifier 'NOT_DECLARED' must be declared",
    );
}

#[test]
fn reverse_ranges_count_down_from_the_upper_bound() {
    let found = steps("BEGIN FOR i IN REVERSE 1..10 LOOP NULL; END LOOP; END;");
    let StepKind::Loop(found_loop) = &found[0] else {
        panic!("{found:?}");
    };
    let LoopKind::Range {
        variable,
        reverse,
        lower,
        upper,
        step,
    } = &found_loop.kind
    else {
        panic!("{found_loop:?}");
    };
    assert_eq!(variable, "i");
    assert!(*reverse);
    assert_eq!((lower.text.as_str(), upper.text.as_str()), ("10", "1"));
    assert!(step.is_none());
}

#[test]
fn query_and_cursor_for_loops_declare_their_record() {
    let found = steps(
        "DECLARE CURSOR c IS SELECT * FROM jobs; BEGIN \
         FOR r IN (SELECT 1 AS one FROM dual) LOOP NULL; END LOOP; \
         FOR r IN c LOOP r.x := 1; END LOOP; END;",
    );
    let StepKind::Loop(query_loop) = &found[0] else {
        panic!("{found:?}");
    };
    let LoopKind::Query {
        targets,
        source: QuerySource::Static(query),
    } = &query_loop.kind
    else {
        panic!("{query_loop:?}");
    };
    assert_eq!(targets[0].name, "r");
    assert_eq!(query.text, "SELECT 1 AS one FROM dual");
    let StepKind::Loop(cursor_loop) = &found[1] else {
        panic!("{found:?}");
    };
    assert!(matches!(&cursor_loop.kind, LoopKind::Cursor { cursor, record, .. } if cursor == "c" && record == "r"));
}

#[test]
fn a_loop_index_and_an_in_parameter_are_read_only() {
    let error = failure("BEGIN FOR i IN 1..3 LOOP i := 2; END LOOP; END;");
    assert!(error.message().contains("PLS-00363: expression 'I' cannot be used as an assignment target"), "{error}");
    let error = failure("PROCEDURE p(a NUMBER) IS BEGIN a := 1; END;");
    assert!(error.message().contains("PLS-00363: expression 'A'"), "{error}");
}

#[test]
fn an_undeclared_target_reports_its_line_and_column() {
    assert_compilation_error(
        &failure("BEGIN x := 1; END;"),
        "line 1, column 7:\nPLS-00201: identifier 'X' must be declared",
    );
    assert_compilation_error(
        &failure("BEGIN\n  NULL;\n  y := 2;\nEND;"),
        "line 3, column 3:\nPLS-00201: identifier 'Y' must be declared",
    );
}

#[test]
fn syntax_errors_name_the_symbol_encountered() {
    assert_compilation_error(
        &failure("BEGIN NULL END;"),
        "line 1, column 12:\nPLS-00103: Encountered the symbol \"END\"",
    );
    assert_compilation_error(
        &failure("BEGIN END;"),
        "line 1, column 7:\nPLS-00103: Encountered the symbol \"END\"",
    );
    assert_compilation_error(
        &failure("BEGIN NULL;"),
        "line 1, column 12:\nPLS-00103: Encountered the symbol \"end-of-file\"",
    );
}

#[test]
fn return_exit_and_raise_are_checked_where_they_appear() {
    assert!(failure("BEGIN RETURN 1; END;").message().contains("PLS-00372"));
    assert!(failure("BEGIN EXIT; END;").message().contains("PLS-00376"));
    assert!(failure("BEGIN <<b>> BEGIN EXIT b; END; END;").message().contains("PLS-00373"));
    assert!(failure("BEGIN RAISE; END;").message().contains("PLS-00367"));
    assert!(failure("FUNCTION f RETURN NUMBER IS BEGIN RETURN; END;").message().contains("PLS-00503"));
    assert!(failure("PROCEDURE p IS BEGIN NULL; END q;").message().contains("PLS-00113"));
    assert!(failure("BEGIN PIPE ROW (1); END;").message().contains("PLS-00629"));
    let goto = failure("BEGIN GOTO done; END;");
    assert_eq!(goto.state(), SqlState::FEATURE_NOT_SUPPORTED);
    assert_compilation_error(&goto, "line 1, column 7:\nPL/SQL: GOTO is not supported yet");
}

#[test]
fn a_stored_procedure_keeps_its_declared_types_and_source() {
    let sql = "PROCEDURE add_job_history(p_emp_id job_history.employee_id%TYPE, \
               p_start_date IN DATE, p_job_id OUT VARCHAR2) IS \
               BEGIN INSERT INTO job_history (employee_id, start_date) VALUES (p_emp_id, p_start_date); \
               p_job_id := 'x'; END add_job_history;";
    let unit = created(sql);
    assert_eq!(unit.name.text, "add_job_history");
    let definition = &unit.definition;
    assert_eq!(definition.kind, RoutineKind::Procedure);
    assert_eq!(definition.dialect, Dialect::Oracle);
    assert_eq!(definition.body, sql);
    assert_eq!(definition.returns, RoutineReturns::Nothing);
    assert!(definition.security_definer);
    let modes: Vec<ParameterMode> = definition
        .parameters
        .iter()
        .map(|parameter| parameter.mode)
        .collect();
    assert_eq!(modes, [ParameterMode::In, ParameterMode::In, ParameterMode::Out]);
    assert_eq!(definition.parameters[0].sql_type, SqlType::Dynamic);
    let Some(plsql) = &definition.plsql else {
        panic!("{definition:?}");
    };
    assert_eq!(
        plsql.declared[0],
        DeclaredType::ColumnType(vec!["job_history".to_owned(), "employee_id".to_owned()])
    );
    assert_eq!(plsql.declared[2].text(), "VARCHAR2");
    assert_eq!(plsql.identity(&definition.parameters), "JOB_HISTORY.EMPLOYEE_ID%TYPE, DATE");
    assert_eq!(unit.program.block.steps.len(), 2);
}

#[test]
fn a_function_heading_records_its_return_type_and_options() {
    let unit = created(
        "FUNCTION twice(n IN NUMBER) RETURN NUMBER DETERMINISTIC AUTHID CURRENT_USER IS BEGIN RETURN n * 2; END twice;",
    );
    let definition = &unit.definition;
    assert_eq!(definition.kind, RoutineKind::Function);
    assert_eq!(definition.volatility, Volatility::Immutable);
    assert!(!definition.security_definer);
    assert!(matches!(definition.returns, RoutineReturns::One(ReturnType::Type(_))));
    assert!(definition.plsql.as_ref().is_some_and(|plsql| plsql.deterministic));
    let pipelined = created(
        "FUNCTION numbers RETURN number_table PIPELINED IS BEGIN PIPE ROW (1); RETURN; END;",
    );
    assert!(matches!(
        pipelined.definition.returns,
        RoutineReturns::Set(ReturnType::Type(SqlType::Dynamic))
    ));
    assert!(matches!(
        pipelined.program.block.steps[0].kind,
        StepKind::ReturnNext(Some(_))
    ));
}

#[test]
fn a_nested_subprogram_sees_the_enclosing_variables() {
    let compiled = program(
        "DECLARE total NUMBER := 0; \
         PROCEDURE add_to(n NUMBER) IS BEGIN total := total + n; END add_to; \
         FUNCTION doubled RETURN NUMBER; \
         BEGIN add_to(1); END;",
    );
    let DeclarationKind::Subprogram(nested) = &compiled.block.declarations[1].kind else {
        panic!("{compiled:?}");
    };
    assert_eq!(nested.definition.name, "add_to");
    assert!(nested.program.is_some());
    let DeclarationKind::Subprogram(forward) = &compiled.block.declarations[2].kind else {
        panic!("{compiled:?}");
    };
    assert!(forward.program.is_none());
    assert!(matches!(&compiled.block.steps[0].kind, StepKind::Call(call) if call.text == "CALL add_to(1)"));
}

#[test]
fn collection_methods_and_elements_are_targets() {
    let found = steps(
        "DECLARE TYPE t IS TABLE OF NUMBER INDEX BY PLS_INTEGER; v t; BEGIN v.delete; v.extend(2); v(1) := 5; END;",
    );
    let StepKind::CollectionMethod {
        target,
        method,
        arguments,
    } = &found[0]
    else {
        panic!("{found:?}");
    };
    assert_eq!(target.name, "v");
    assert_eq!(*method, CollectionMethod::Delete);
    assert!(arguments.is_empty());
    assert!(matches!(&found[1], StepKind::CollectionMethod { method: CollectionMethod::Extend, arguments, .. } if arguments.len() == 1));
    let StepKind::Assign { target, .. } = &found[2] else {
        panic!("{found:?}");
    };
    assert!(matches!(&target.path[..], [TargetPart::Index(index)] if index.text == "1"));
}

#[test]
fn forall_runs_one_statement_over_a_range() {
    let found = steps(
        "DECLARE TYPE t IS TABLE OF NUMBER; ids t; BEGIN \
         FORALL i IN 1..ids.COUNT SAVE EXCEPTIONS DELETE FROM jobs WHERE id = ids(i); END;",
    );
    let StepKind::Forall(forall) = &found[0] else {
        panic!("{found:?}");
    };
    assert_eq!(forall.variable, "i");
    assert!(forall.save_exceptions);
    assert!(matches!(&forall.bounds, ForallBounds::Range { lower, upper } if lower.text == "1" && upper.text == "ids.COUNT"));
    assert!(matches!(&forall.statement.kind, StepKind::Sql { statement, .. } if statement.text == "DELETE FROM jobs WHERE id = ids(i)"));
}

#[test]
fn execute_immediate_binds_and_fetches_into_variables() {
    let found = steps(
        "DECLARE n NUMBER; BEGIN EXECUTE IMMEDIATE 'SELECT 1 FROM dual WHERE 1 = :1' INTO n USING 1; END;",
    );
    let StepKind::Execute(dynamic) = &found[0] else {
        panic!("{found:?}");
    };
    assert_eq!(dynamic.command.text, "'SELECT 1 FROM dual WHERE 1 = :1'");
    assert!(dynamic.into.as_ref().is_some_and(|into| into.strict));
    assert_eq!(dynamic.using.len(), 1);
    assert!(failure("DECLARE n NUMBER; BEGIN EXECUTE IMMEDIATE 'x' USING OUT n; END;")
        .message()
        .contains("not supported"));
}

#[test]
fn cursor_variables_open_fetch_and_close() {
    let compiled = program(
        "DECLARE c SYS_REFCURSOR; n NUMBER; BEGIN OPEN c FOR SELECT 1 FROM dual; \
         FETCH c INTO n; CLOSE c; END;",
    );
    let DeclarationKind::Variable(cursor) = &compiled.block.declarations[0].kind else {
        panic!("{compiled:?}");
    };
    assert_eq!(cursor.variable_type, VariableType::RefCursor);
    let found: Vec<&StepKind> = compiled.block.steps.iter().map(|step| &step.kind).collect();
    assert!(matches!(found[0], StepKind::Open { cursor, source: OpenSource::Query(QuerySource::Static(_)), .. } if cursor == "c"));
    assert!(matches!(found[1], StepKind::Fetch { cursor, limit: None, into, .. } if cursor == "c" && !into.bulk));
    assert!(matches!(found[2], StepKind::Close(cursor) if cursor == "c"));
    let limited = steps(
        "DECLARE CURSOR c IS SELECT id FROM jobs; TYPE t IS TABLE OF NUMBER; v t; \
         BEGIN OPEN c; FETCH c BULK COLLECT INTO v LIMIT 100; CLOSE c; END;",
    );
    assert!(matches!(&limited[1], StepKind::Fetch { limit: Some(limit), into, .. } if limit.text == "100" && into.bulk));
}

#[test]
fn a_package_body_sees_its_specification() {
    let spec = package(
        "PACKAGE emp_pkg IS g_count NUMBER := 0; e_bad EXCEPTION; \
         PROCEDURE bump(n NUMBER); FUNCTION total RETURN NUMBER; END emp_pkg;",
        None,
    );
    assert!(!spec.body);
    assert_eq!(spec.name.text, "emp_pkg");
    assert_eq!(spec.block.declarations.len(), 4);
    let body = package(
        "PACKAGE BODY emp_pkg IS \
         PROCEDURE bump(n NUMBER) IS BEGIN g_count := g_count + n; IF n < 0 THEN RAISE e_bad; END IF; END bump; \
         FUNCTION total RETURN NUMBER IS BEGIN RETURN g_count; END total; \
         BEGIN g_count := 1; END emp_pkg;",
        Some(&spec),
    );
    assert!(body.body);
    assert_eq!(body.block.steps.len(), 1);
    let DeclarationKind::Subprogram(bump) = &body.block.declarations[0].kind else {
        panic!("{body:?}");
    };
    let Some(bump_program) = &bump.program else {
        panic!("{bump:?}");
    };
    let StepKind::If { branches, .. } = &bump_program.block.steps[1].kind else {
        panic!("{bump_program:?}");
    };
    assert_eq!(
        branches[0].steps[0].kind,
        StepKind::RaiseException(RaisedException::Declared("emp_pkg.e_bad".to_owned()))
    );
    let error = compile_package("PACKAGE p IS PROCEDURE a IS BEGIN NULL; END; END p;", None)
        .err()
        .unwrap_or_else(|| panic!("a specification with a body compiled"));
    assert!(error.message().contains("PLS-00103"), "{error}");
}

#[test]
fn trigger_bodies_read_and_write_the_row_records() {
    let mut definition = anonymous_block_definition(
        "BEGIN IF INSERTING THEN :new.salary := :old.salary * 2; END IF; END;",
    );
    definition.returns = RoutineReturns::One(ReturnType::Trigger);
    let compiled = match compile(&definition) {
        Ok(compiled) => compiled,
        Err(error) => panic!("{error}"),
    };
    let StepKind::If { branches, .. } = &compiled.block.steps[0].kind else {
        panic!("{compiled:?}");
    };
    assert_eq!(branches[0].condition.text, "INSERTING");
    let StepKind::Assign { target, value } = &branches[0].steps[0].kind else {
        panic!("{branches:?}");
    };
    assert_eq!(target.name, "new");
    assert_eq!(target.path, [TargetPart::Field("salary".to_owned())]);
    assert_eq!(value.text, "old.salary * 2");
}

#[test]
fn line_and_column_count_characters_from_one() {
    assert_eq!(line_and_column("BEGIN x", 6), (1, 7));
    assert_eq!(line_and_column("a\nbé c", 6), (2, 4));
    assert_eq!(line_and_column("", 0), (1, 1));
}

#[test]
fn hostile_units_fail_without_panicking() {
    let deep = format!("{}NULL;{}", "BEGIN ".repeat(200), " END;".repeat(200));
    assert!(failure(&deep).message().contains("nested too deeply"));
    let units = [
        "DECLARE e EXCEPTION; PRAGMA EXCEPTION_INIT(e, -20002); v NUMBER := 1; \
         CURSOR c(p NUMBER DEFAULT 1) IS SELECT p FROM dual; \
         TYPE r IS RECORD (a NUMBER NOT NULL := 1, b VARCHAR2(10)); \
         BEGIN <<outer>> FOR i IN REVERSE 1..3 LOOP CASE i WHEN 1 THEN EXIT outer; ELSE NULL; END CASE; END LOOP outer; \
         EXCEPTION WHEN e OR zero_divide THEN NULL; END;",
        "PACKAGE BODY p IS FUNCTION f(a IN OUT NOCOPY NUMBER) RETURN NUMBER IS BEGIN RETURN a; END f; BEGIN NULL; END p;",
    ];
    for unit in units {
        for (end, _) in unit.char_indices() {
            let prefix = &unit[..end];
            let _ = compile(&anonymous_block_definition(prefix));
            let _ = compile_package(prefix, None);
        }
    }
    for garbage in ["<<", "BEGIN <<x", "DECLARE TYPE", "BEGIN FORALL i IN", ":=", "BEGIN :1 := 2; END;"] {
        assert!(compile(&anonymous_block_definition(garbage)).is_err(), "{garbage}");
    }
}
