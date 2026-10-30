use super::{
    AliasTarget, DeclarationKind, DiagnosticsItem, ExceptionCondition, FetchDirection, LoopKind,
    OpenSource, Program, QuerySource, RaiseCondition, RaiseItem, RaiseLevel, Step, StepKind,
    TargetPart, VariableConflict, VariableType, compile,
};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlState};
use crate::memory_store::MemoryStore;
use crate::routines::{RoutineDefinition, parse_definition};
use crate::types::SqlType;
use crate::Session;

fn definition(sql: &str) -> RoutineDefinition {
    match parse_definition(sql, Dialect::PostgreSql) {
        Ok(definition) => definition,
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn function(body: &str) -> String {
    format!(
        "CREATE FUNCTION f(a integer, b text DEFAULT 'x') RETURNS integer LANGUAGE plpgsql AS $body${body}$body$"
    )
}

fn compiled(sql: &str) -> Program {
    match compile(&definition(sql)) {
        Ok(program) => program,
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn failure(sql: &str) -> SqlError {
    match compile(&definition(sql)) {
        Ok(program) => panic!("{sql} compiled: {program:?}"),
        Err(error) => error,
    }
}

fn steps(sql: &str) -> Vec<Step> {
    compiled(sql).block.steps
}

fn kinds(sql: &str) -> Vec<StepKind> {
    steps(sql).into_iter().map(|step| step.kind).collect()
}

#[test]
fn a_manual_style_function_compiles_into_blocks_declarations_and_steps() {
    let program = compiled(&function(
        "
<<outer>>
DECLARE
    quantity integer := 30;
    total CONSTANT numeric(10,2) DEFAULT 0;
    label_text text NOT NULL = 'none';
    row_data record;
    copy_of quantity%TYPE;
    first_arg ALIAS FOR $1;
BEGIN
    RAISE NOTICE 'Quantity here is %', quantity;
    quantity := 50;
    DECLARE
        quantity integer := 80;
    BEGIN
        RAISE NOTICE 'Quantity here is %', quantity;
        RAISE NOTICE 'Outer quantity here is %', outer.quantity;
    END;
    RETURN quantity;
END outer;
",
    ));
    assert_eq!(program.variable_conflict, VariableConflict::Error);
    let block = &program.block;
    assert_eq!(block.label.as_deref(), Some("outer"));
    assert_eq!(block.declarations.len(), 6);
    let names: Vec<&str> = block
        .declarations
        .iter()
        .map(|declaration| declaration.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["quantity", "total", "label_text", "row_data", "copy_of", "first_arg"]
    );
    let DeclarationKind::Variable(quantity) = &block.declarations[0].kind else {
        panic!("{:?}", block.declarations[0]);
    };
    assert_eq!(quantity.variable_type, VariableType::Sql(SqlType::Integer));
    assert_eq!(quantity.default.as_ref().map(|value| value.text.as_str()), Some("30"));
    let DeclarationKind::Variable(total) = &block.declarations[1].kind else {
        panic!("{:?}", block.declarations[1]);
    };
    assert!(total.constant);
    let DeclarationKind::Variable(label) = &block.declarations[2].kind else {
        panic!("{:?}", block.declarations[2]);
    };
    assert!(label.not_null);
    let DeclarationKind::Variable(record) = &block.declarations[3].kind else {
        panic!("{:?}", block.declarations[3]);
    };
    assert_eq!(record.variable_type, VariableType::Record);
    let DeclarationKind::Variable(copied) = &block.declarations[4].kind else {
        panic!("{:?}", block.declarations[4]);
    };
    assert_eq!(copied.variable_type, VariableType::TypeOf(vec!["quantity".to_owned()]));
    assert_eq!(
        block.declarations[5].kind,
        DeclarationKind::Alias(AliasTarget::Parameter(1))
    );
    assert_eq!(block.steps.len(), 4);
    let StepKind::Raise(raise) = &block.steps[0].kind else {
        panic!("{:?}", block.steps[0]);
    };
    assert_eq!(raise.level, RaiseLevel::Notice);
    assert_eq!(raise.format.as_deref(), Some("Quantity here is %"));
    assert_eq!(raise.arguments[0].text, "quantity");
    let StepKind::Block(inner) = &block.steps[2].kind else {
        panic!("{:?}", block.steps[2]);
    };
    assert_eq!(inner.declarations.len(), 1);
    assert_eq!(inner.steps.len(), 2);
    assert_eq!(block.steps[3].kind.name(), "RETURN");
}

#[test]
fn compiler_options_are_read_before_the_block() {
    let program = compiled(&function(
        "#variable_conflict use_column\n#print_strict_params on\nBEGIN RETURN 1; END",
    ));
    assert_eq!(program.variable_conflict, VariableConflict::UseColumn);
    assert!(program.print_strict_params);
    let error = failure(&function("#no_such_option on\nBEGIN RETURN 1; END"));
    assert_eq!(error.state(), SqlState::SYNTAX_ERROR);
}

#[test]
fn assignments_take_every_operator_and_target_shape() {
    let found = kinds(&function(
        "DECLARE r record; items integer[]; BEGIN a := 1; a = 2; a:=3; items[a + 1] := 4; r.x := 5; $1 := 6; RETURN a; END",
    ));
    let targets: Vec<(String, usize, String)> = found
        .iter()
        .filter_map(|kind| match kind {
            StepKind::Assign { target, value } => {
                Some((target.name.clone(), target.path.len(), value.text.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        targets,
        [
            ("a".to_owned(), 0, "1".to_owned()),
            ("a".to_owned(), 0, "2".to_owned()),
            ("a".to_owned(), 0, "3".to_owned()),
            ("items".to_owned(), 1, "4".to_owned()),
            ("r".to_owned(), 1, "5".to_owned()),
            ("$1".to_owned(), 0, "6".to_owned()),
        ]
    );
    let StepKind::Assign { target, .. } = &found[3] else {
        panic!("{:?}", found[3]);
    };
    assert!(matches!(&target.path[0], TargetPart::Index(index) if index.text == "a + 1"));
}

#[test]
fn sql_statements_keep_their_text_and_lose_their_into_clause() {
    let found = kinds(&function(
        "DECLARE r record; n integer; BEGIN
            SELECT * INTO STRICT r FROM t WHERE id = a;
            SELECT count(*) INTO n FROM t;
            INSERT INTO t (id) VALUES (a) RETURNING id INTO n;
            UPDATE t SET name = b WHERE id = a;
            WITH x AS (SELECT 1 AS one) SELECT one INTO n FROM x;
            CREATE TABLE made (id integer);
            RETURN n;
        END",
    ));
    let StepKind::Sql { statement, into } = &found[0] else {
        panic!("{:?}", found[0]);
    };
    assert_eq!(statement.text, "SELECT * FROM t WHERE id = a");
    let into = into.as_ref().map(|into| (into.strict, into.targets.len()));
    assert_eq!(into, Some((true, 1)));
    let StepKind::Sql { statement, into } = &found[2] else {
        panic!("{:?}", found[2]);
    };
    assert_eq!(statement.text, "INSERT INTO t (id) VALUES (a) RETURNING id");
    assert!(into.as_ref().is_some_and(|into| !into.strict && into.targets[0].name == "n"));
    let StepKind::Sql { statement, into } = &found[3] else {
        panic!("{:?}", found[3]);
    };
    assert_eq!(statement.text, "UPDATE t SET name = b WHERE id = a");
    assert!(into.is_none());
    let StepKind::Sql { statement, .. } = &found[4] else {
        panic!("{:?}", found[4]);
    };
    assert_eq!(statement.text, "WITH x AS (SELECT 1 AS one) SELECT one FROM x");
    assert!(matches!(&found[5], StepKind::Sql { into: None, .. }));
}

#[test]
fn perform_execute_call_and_diagnostics_compile() {
    let found = kinds(&function(
        "DECLARE n integer; message text; BEGIN
            PERFORM pg_sleep(0);
            EXECUTE 'SELECT count(*) FROM ' || quote_ident(b) INTO n USING a, b;
            EXECUTE 'DELETE FROM t';
            CALL p(a, n);
            GET DIAGNOSTICS n = ROW_COUNT;
            RETURN n;
        EXCEPTION WHEN OTHERS THEN
            GET STACKED DIAGNOSTICS message = MESSAGE_TEXT, n := PG_EXCEPTION_DETAIL;
            RETURN 0;
        END",
    ));
    assert!(matches!(&found[0], StepKind::Perform(query) if query.text == "SELECT pg_sleep(0)"));
    let StepKind::Execute(dynamic) = &found[1] else {
        panic!("{:?}", found[1]);
    };
    assert_eq!(dynamic.command.text, "'SELECT count(*) FROM ' || quote_ident(b)");
    assert_eq!(dynamic.using.len(), 2);
    assert!(dynamic.into.is_some());
    assert!(matches!(&found[2], StepKind::Execute(dynamic) if dynamic.into.is_none() && dynamic.using.is_empty()));
    assert!(matches!(&found[3], StepKind::Call(call) if call.text == "CALL p(a, n)"));
    assert!(matches!(
        &found[4],
        StepKind::GetDiagnostics { stacked: false, items } if items[0].1 == DiagnosticsItem::RowCount
    ));
}

#[test]
fn conditionals_compile_with_every_branch() {
    let found = kinds(&function(
        "BEGIN
            IF a > 0 THEN RETURN 1; ELSIF a < 0 THEN RETURN -1; ELSEIF a IS NULL THEN NULL; ELSE RETURN 0; END IF;
            IF CASE WHEN a = 1 THEN true ELSE false END THEN RETURN 2; END IF;
            CASE a WHEN 1, 2 THEN RETURN 12; WHEN 3 THEN RETURN 3; ELSE RETURN 0; END CASE;
            CASE WHEN a > 10 THEN RETURN 10; END CASE;
            RETURN 0;
        END",
    ));
    let StepKind::If {
        branches,
        otherwise,
    } = &found[0]
    else {
        panic!("{:?}", found[0]);
    };
    assert_eq!(branches.len(), 3);
    assert_eq!(branches[1].condition.text, "a < 0");
    assert!(otherwise.is_some());
    let StepKind::If { branches, .. } = &found[1] else {
        panic!("{:?}", found[1]);
    };
    assert_eq!(
        branches[0].condition.text,
        "CASE WHEN a = 1 THEN true ELSE false END"
    );
    let StepKind::Case { subject, arms, otherwise } = &found[2] else {
        panic!("{:?}", found[2]);
    };
    assert_eq!(subject.as_ref().map(|subject| subject.text.as_str()), Some("a"));
    assert_eq!(arms[0].values.len(), 2);
    assert!(otherwise.is_some());
    assert!(matches!(&found[3], StepKind::Case { subject: None, otherwise: None, .. }));
}

#[test]
fn every_loop_form_compiles() {
    let found = kinds(&function(
        "DECLARE
            r record;
            n integer := 0;
            items integer[] := ARRAY[1, 2];
            item integer;
            pending CURSOR (low integer) FOR SELECT * FROM t WHERE id > low;
        BEGIN
            <<counting>>
            LOOP
                n := n + 1;
                EXIT counting WHEN n > 10;
                CONTINUE WHEN n < 5;
            END LOOP counting;
            WHILE n > 0 LOOP n := n - 1; END LOOP;
            FOR i IN 1..10 LOOP n := n + i; END LOOP;
            FOR i IN REVERSE a .. 1 BY 2 LOOP n := n + i; END LOOP;
            FOR r IN SELECT * FROM t LOOP n := n + 1; END LOOP;
            FOR r IN EXECUTE 'SELECT * FROM ' || b USING a LOOP EXIT; END LOOP;
            FOR found_row IN pending(low := 3) LOOP n := n + 1; END LOOP;
            FOREACH item SLICE 0 IN ARRAY items LOOP n := n + item; END LOOP;
            RETURN n;
        END",
    ));
    let loops: Vec<&LoopKind> = found
        .iter()
        .filter_map(|kind| match kind {
            StepKind::Loop(found_loop) => Some(&found_loop.kind),
            _ => None,
        })
        .collect();
    assert_eq!(loops.len(), 8);
    assert_eq!(loops[0], &LoopKind::Plain);
    assert!(matches!(loops[1], LoopKind::While(condition) if condition.text == "n > 0"));
    let LoopKind::Range {
        variable,
        reverse,
        lower,
        upper,
        step,
    } = loops[2]
    else {
        panic!("{:?}", loops[2]);
    };
    assert_eq!((variable.as_str(), *reverse), ("i", false));
    assert_eq!((lower.text.as_str(), upper.text.as_str()), ("1", "10"));
    assert!(step.is_none());
    let LoopKind::Range {
        reverse,
        lower,
        upper,
        step,
        ..
    } = loops[3]
    else {
        panic!("{:?}", loops[3]);
    };
    assert!(*reverse);
    assert_eq!((lower.text.as_str(), upper.text.as_str()), ("a", "1"));
    assert_eq!(step.as_ref().map(|step| step.text.as_str()), Some("2"));
    assert!(matches!(
        loops[4],
        LoopKind::Query { source: QuerySource::Static(query), .. } if query.text == "SELECT * FROM t"
    ));
    assert!(matches!(loops[5], LoopKind::Query { source: QuerySource::Dynamic { using, .. }, .. } if using.len() == 1));
    let LoopKind::Cursor {
        record,
        cursor,
        arguments,
    } = loops[6]
    else {
        panic!("{:?}", loops[6]);
    };
    assert_eq!((record.as_str(), cursor.as_str()), ("found_row", "pending"));
    assert_eq!(arguments[0].name.as_deref(), Some("low"));
    assert!(matches!(loops[7], LoopKind::Foreach { slice: 0, .. }));
    let StepKind::Loop(labelled) = &found[0] else {
        panic!("{:?}", found[0]);
    };
    assert_eq!(labelled.label.as_deref(), Some("counting"));
    assert!(matches!(
        &labelled.body[1].kind,
        StepKind::Exit { label: Some(label), condition: Some(_) } if label == "counting"
    ));
}

#[test]
fn set_returning_functions_take_return_next_and_return_query() {
    let found = kinds(
        "CREATE FUNCTION evens(n integer) RETURNS SETOF integer LANGUAGE plpgsql AS $$
        BEGIN
            FOR i IN 1..n LOOP
                IF i % 2 = 0 THEN RETURN NEXT i; END IF;
            END LOOP;
            RETURN QUERY SELECT 100;
            RETURN QUERY EXECUTE 'SELECT $1' USING n;
            RETURN;
        END $$",
    );
    assert!(matches!(&found[1], StepKind::ReturnQuery(QuerySource::Static(query)) if query.text == "SELECT 100"));
    assert!(matches!(&found[2], StepKind::ReturnQuery(QuerySource::Dynamic { .. })));
    assert_eq!(found[3], StepKind::Return(None));
    let found = kinds(
        "CREATE FUNCTION pairs() RETURNS TABLE (x integer, y text) LANGUAGE plpgsql AS $$
        BEGIN x := 1; y := 'one'; RETURN NEXT; END $$",
    );
    assert_eq!(found[2], StepKind::ReturnNext(None));
}

#[test]
fn raise_takes_levels_conditions_and_options() {
    let found = kinds(&function(
        "BEGIN
            RAISE DEBUG 'debug %', a;
            RAISE 'plain %, %', a, b USING HINT = 'check a', DETAIL = 'detail';
            RAISE division_by_zero;
            RAISE SQLSTATE '22012' USING MESSAGE = 'custom';
            RAISE WARNING USING MESSAGE = 'warned', ERRCODE = 'P0001';
            RETURN 1;
        EXCEPTION
            WHEN division_by_zero OR unique_violation THEN RAISE;
            WHEN SQLSTATE '22P02' THEN RETURN 2;
            WHEN integrity_constraint_violation THEN RETURN 3;
        END",
    ));
    let StepKind::Raise(raise) = &found[1] else {
        panic!("{:?}", found[1]);
    };
    assert_eq!(raise.level, RaiseLevel::Exception);
    assert_eq!(raise.arguments.len(), 2);
    assert_eq!(raise.options[0].item, RaiseItem::Hint);
    let StepKind::Raise(raise) = &found[2] else {
        panic!("{:?}", found[2]);
    };
    assert_eq!(
        raise.condition,
        Some(RaiseCondition::Name {
            name: "division_by_zero".to_owned(),
            state: SqlState::DIVISION_BY_ZERO,
        })
    );
    let StepKind::Raise(raise) = &found[4] else {
        panic!("{:?}", found[4]);
    };
    assert_eq!(raise.level, RaiseLevel::Warning);
    assert_eq!(raise.options.len(), 2);
    let program = compiled(&function(
        "BEGIN RETURN 1; EXCEPTION WHEN division_by_zero OR unique_violation THEN RAISE; WHEN SQLSTATE '22P02' THEN RETURN 2; WHEN integrity_constraint_violation THEN RETURN 3; END",
    ));
    let handlers = &program.block.handlers;
    assert_eq!(handlers.len(), 3);
    assert_eq!(handlers[0].steps[0].kind, StepKind::Reraise);
    assert!(handlers[0].catches(SqlState::UNIQUE_VIOLATION));
    assert!(!handlers[0].catches(SqlState::RAISE_EXCEPTION));
    assert!(handlers[2].catches(SqlState::UNIQUE_VIOLATION));
    assert!(ExceptionCondition::Others.catches(SqlState::DIVISION_BY_ZERO));
    assert!(!ExceptionCondition::Others.catches(SqlState::QUERY_CANCELED));
    assert!(!ExceptionCondition::Others.catches(SqlState::ASSERT_FAILURE));
}

#[test]
fn cursors_assert_and_transaction_control_compile() {
    let found = kinds(
        "CREATE PROCEDURE walk() LANGUAGE plpgsql AS $$
        DECLARE
            bound SCROLL CURSOR FOR SELECT id FROM t;
            free refcursor;
            id integer;
        BEGIN
            OPEN bound;
            FETCH bound INTO id;
            FETCH LAST FROM bound INTO id;
            FETCH RELATIVE -1 FROM bound INTO id;
            MOVE FORWARD ALL IN bound;
            CLOSE bound;
            OPEN free NO SCROLL FOR SELECT 1;
            OPEN free FOR EXECUTE 'SELECT ' || 2;
            ASSERT id IS NOT NULL, 'id was null';
            COMMIT;
            ROLLBACK AND CHAIN;
            RETURN;
        END $$",
    );
    assert!(matches!(&found[0], StepKind::Open { source: OpenSource::Bound(arguments), .. } if arguments.is_empty()));
    assert!(matches!(&found[1], StepKind::Fetch { direction: FetchDirection::Next, .. }));
    assert!(matches!(&found[2], StepKind::Fetch { direction: FetchDirection::Last, .. }));
    assert!(matches!(&found[3], StepKind::Fetch { direction: FetchDirection::Relative(count), .. } if count.text == "-1"));
    assert!(matches!(&found[4], StepKind::Move { direction: FetchDirection::ForwardAll, .. }));
    assert_eq!(found[5], StepKind::Close("bound".to_owned()));
    assert!(matches!(&found[6], StepKind::Open { scroll: Some(false), source: OpenSource::Query(QuerySource::Static(_)), .. }));
    assert!(matches!(&found[7], StepKind::Open { source: OpenSource::Query(QuerySource::Dynamic { .. }), .. }));
    assert!(matches!(&found[8], StepKind::Assert { message: Some(_), .. }));
    assert_eq!(found[9], StepKind::Commit { chain: false });
    assert_eq!(found[10], StepKind::Rollback { chain: true });
}

#[test]
fn trigger_functions_see_new_old_and_tg_variables() {
    let found = kinds(
        "CREATE FUNCTION stamp() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
            NEW.updated_at := now();
            NEW.tags[1] := TG_ARGV[0];
            RETURN NEW;
        END $$",
    );
    assert!(matches!(&found[1], StepKind::Assign { target, .. } if target.name == "new"));
}

fn error_of(sql: &str) -> (SqlState, String) {
    let error = failure(sql);
    (error.state(), error.message().to_owned())
}

#[test]
fn bodies_with_mistakes_are_refused_with_postgresql_messages() {
    let cases: [(String, SqlState, &str); 22] = [
        (
            function("BEGIN missing := 1; RETURN 1; END"),
            SqlState::SYNTAX_ERROR,
            "\"missing\" is not a known variable",
        ),
        (
            function("DECLARE c CONSTANT integer := 1; BEGIN c := 2; RETURN c; END"),
            SqlState::ERROR_IN_ASSIGNMENT,
            "variable \"c\" is declared CONSTANT",
        ),
        (
            function("BEGIN EXIT; RETURN 1; END"),
            SqlState::SYNTAX_ERROR,
            "EXIT cannot be used outside a loop, unless it has a label",
        ),
        (
            function("BEGIN CONTINUE; RETURN 1; END"),
            SqlState::SYNTAX_ERROR,
            "CONTINUE cannot be used outside a loop",
        ),
        (
            function("BEGIN LOOP EXIT nowhere; END LOOP; RETURN 1; END"),
            SqlState::SYNTAX_ERROR,
            "there is no label \"nowhere\" attached to any block or loop enclosing this statement",
        ),
        (
            function("<<blk>> BEGIN LOOP CONTINUE blk; END LOOP; RETURN 1; END"),
            SqlState::SYNTAX_ERROR,
            "block label \"blk\" cannot be used in CONTINUE",
        ),
        (
            function("<<one>> BEGIN RETURN 1; END two"),
            SqlState::SYNTAX_ERROR,
            "end label \"two\" differs from block's label \"one\"",
        ),
        (
            function("BEGIN RETURN 1; END two"),
            SqlState::SYNTAX_ERROR,
            "end label \"two\" specified for unlabeled block",
        ),
        (
            function("BEGIN RETURN 1; EXCEPTION WHEN no_such_condition THEN RETURN 2; END"),
            SqlState::UNDEFINED_OBJECT,
            "unrecognized exception condition \"no_such_condition\"",
        ),
        (
            function("BEGIN RAISE NOTICE '% and %', a; RETURN 1; END"),
            SqlState::SYNTAX_ERROR,
            "too few parameters specified for RAISE",
        ),
        (
            function("BEGIN RAISE NOTICE 'none', a; RETURN 1; END"),
            SqlState::SYNTAX_ERROR,
            "too many parameters specified for RAISE",
        ),
        (
            function("BEGIN RAISE 'x' USING MESSAGE = 'y'; RETURN 1; END"),
            SqlState::SYNTAX_ERROR,
            "RAISE option already specified: MESSAGE",
        ),
        (
            "CREATE PROCEDURE p() LANGUAGE plpgsql AS $$BEGIN RETURN 1; END$$".to_owned(),
            SqlState::DATATYPE_MISMATCH,
            "RETURN cannot have a parameter in a procedure",
        ),
        (
            function("BEGIN RETURN NEXT 1; END"),
            SqlState::DATATYPE_MISMATCH,
            "cannot use RETURN NEXT in a non-SETOF function",
        ),
        (
            function("BEGIN RETURN QUERY SELECT 1; END"),
            SqlState::DATATYPE_MISMATCH,
            "cannot use RETURN QUERY in a non-SETOF function",
        ),
        (
            "CREATE FUNCTION v() RETURNS void LANGUAGE plpgsql AS $$BEGIN RETURN 1; END$$".to_owned(),
            SqlState::DATATYPE_MISMATCH,
            "RETURN cannot have a parameter in function returning void",
        ),
        (
            "CREATE FUNCTION s() RETURNS SETOF integer LANGUAGE plpgsql AS $$BEGIN RETURN 1; END$$".to_owned(),
            SqlState::DATATYPE_MISMATCH,
            "RETURN cannot have a parameter in function returning set",
        ),
        (
            "CREATE FUNCTION o(OUT x integer) LANGUAGE plpgsql AS $$BEGIN RETURN 1; END$$".to_owned(),
            SqlState::DATATYPE_MISMATCH,
            "RETURN cannot have a parameter in function with OUT parameters",
        ),
        (
            function("BEGIN RETURN; END"),
            SqlState::SYNTAX_ERROR,
            "missing expression at or near \";\"",
        ),
        (
            function("DECLARE n integer NOT NULL; BEGIN RETURN 1; END"),
            SqlState::SYNTAX_ERROR,
            "variable \"n\" must have a default value, since it's declared NOT NULL",
        ),
        (
            function("DECLARE c CURSOR FOR SELECT 1; n integer; BEGIN FETCH FORWARD 2 FROM c INTO n; RETURN n; END"),
            SqlState::SYNTAX_ERROR,
            "FETCH statement cannot return multiple rows",
        ),
        (
            function("DECLARE n integer; BEGIN GET DIAGNOSTICS n = MESSAGE_TEXT; RETURN n; END"),
            SqlState::SYNTAX_ERROR,
            "diagnostics item MESSAGE_TEXT is not allowed in GET CURRENT DIAGNOSTICS",
        ),
    ];
    for (sql, state, message) in cases {
        assert_eq!(error_of(&sql), (state, message.to_owned()), "{sql}");
    }
    let (state, message) = error_of(&function("BEGIN SAVEPOINT s; RETURN 1; END"));
    assert_eq!(state, SqlState::FEATURE_NOT_SUPPORTED);
    assert_eq!(message, "unsupported transaction command in PL/pgSQL");
    for broken in [
        "BEGIN RETURN 1;",
        "RETURN 1;",
        "BEGIN IF a THEN RETURN 1; END; END",
        "BEGIN LOOP RETURN 1; END IF; END",
        "BEGIN RETURN 1 END",
        "BEGIN RETURN 1; END; extra",
        "DECLARE BEGIN FOR i IN 1.. LOOP END LOOP; RETURN 1; END",
    ] {
        assert_eq!(failure(&function(broken)).state(), SqlState::SYNTAX_ERROR, "{broken}");
    }
}

#[test]
fn nesting_and_size_are_bounded() {
    let deep = format!(
        "BEGIN {} RETURN 1; {} END",
        "IF a > 0 THEN ".repeat(200),
        "END IF; ".repeat(200)
    );
    assert_eq!(failure(&function(&deep)).state(), SqlState::STATEMENT_TOO_COMPLEX);
    let blocks = format!("{} RETURN 1; {}", "BEGIN ".repeat(200), "END; ".repeat(200));
    assert_eq!(failure(&function(&blocks)).state(), SqlState::STATEMENT_TOO_COMPLEX);
    let fine = format!(
        "BEGIN {} RETURN 1; {} END",
        "IF a > 0 THEN ".repeat(30),
        "END IF; ".repeat(30)
    );
    compiled(&function(&fine));
}

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> Result<(), SqlError> {
    session.execute(sql, &mut |_| {})
}

#[test]
fn create_function_checks_the_body_and_points_into_the_statement() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let sql = "CREATE FUNCTION f() RETURNS integer LANGUAGE plpgsql AS $$\nBEGIN\n  ghost := 1;\n  RETURN 1;\nEND$$";
    let Err(error) = run(&mut session, sql) else {
        panic!("{sql} succeeded");
    };
    assert_eq!(error.state(), SqlState::SYNTAX_ERROR);
    assert_eq!(error.position(), sql.find("ghost"));
    assert_eq!(
        error.context(),
        Some("compilation of PL/pgSQL function \"f\" near line 3")
    );
    let quoted = "CREATE FUNCTION g() RETURNS integer LANGUAGE plpgsql AS 'BEGIN RETURN ''x'' +; END'";
    let Err(error) = run(&mut session, quoted) else {
        panic!("{quoted} succeeded");
    };
    assert_eq!(error.state(), SqlState::SYNTAX_ERROR);
    assert_eq!(error.position(), None);
    run(&mut session, "SET check_function_bodies = false").unwrap_or_else(|error| panic!("{error}"));
    run(&mut session, sql).unwrap_or_else(|error| panic!("{error}"));
    run(&mut session, "SET check_function_bodies = on").unwrap_or_else(|error| panic!("{error}"));
    let good = "CREATE FUNCTION h(n integer) RETURNS integer LANGUAGE plpgsql AS $$BEGIN RETURN n * 2; END$$";
    run(&mut session, good).unwrap_or_else(|error| panic!("{error}"));
}

struct Scrambler(u64);

impl Scrambler {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % bound.max(1) as u64) as usize
    }
}

#[test]
fn hostile_bodies_compile_to_errors_and_never_panic() {
    let pieces = [
        "BEGIN", "END", "DECLARE", "IF", "THEN", "ELSIF", "ELSE", "END IF", "LOOP", "END LOOP",
        "WHILE", "FOR", "IN", "REVERSE", "..", "BY", "FOREACH", "SLICE", "ARRAY", "CASE", "WHEN",
        "END CASE", "EXIT", "CONTINUE", "RETURN", "NEXT", "QUERY", "RAISE", "NOTICE", "USING",
        "MESSAGE", "=", ":=", ":", "SQLSTATE", "'22012'", "EXCEPTION", "OTHERS", "OR", "PERFORM",
        "EXECUTE", "INTO", "STRICT", "GET", "STACKED", "DIAGNOSTICS", "ROW_COUNT", "OPEN", "FETCH",
        "MOVE", "CLOSE", "CURSOR", "SCROLL", "NO", "ALIAS", "$1", "$9", "CONSTANT", "NOT NULL",
        "DEFAULT", "%TYPE", "%ROWTYPE", "record", "integer", "a", "b", "x", "<<", ">>", "lbl",
        ";", ",", "(", ")", "[", "]", ".", "'%'", "1", "SELECT", "COMMIT", "ROLLBACK", "AND",
        "CHAIN", "NULL", "ASSERT", "#variable_conflict", "use_column", "CALL", "\"q\"",
    ];
    let header = definition(&function("BEGIN RETURN 1; END"));
    let mut random = Scrambler(0x5EED_1234_ABCD_0001);
    for _ in 0..20_000 {
        let length = 1 + random.below(24);
        let mut body = String::new();
        for _ in 0..length {
            body.push_str(pieces[random.below(pieces.len())]);
            body.push(' ');
        }
        let mut subject = header.clone();
        subject.body = body.clone();
        if let Err(error) = compile(&subject)
            && let Some(position) = error.position()
        {
            assert!(position <= body.len(), "{body}: {error}");
        }
        let _ = super::check(&subject, None);
    }
    let mut subject = header;
    subject.body = "BEGIN ".repeat(50_000);
    assert!(compile(&subject).is_err());
    subject.body = "(".repeat(50_000);
    assert!(compile(&subject).is_err());
}
