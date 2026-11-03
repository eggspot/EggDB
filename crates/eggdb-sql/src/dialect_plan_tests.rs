use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Dialect, Session, SqlError};

fn run(session: &mut Session<'_>, sql: &str) -> Result<Vec<String>, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs)
}

fn all(session: &mut Session<'_>, sql: &str) -> Vec<String> {
    match run(session, sql) {
        Ok(outputs) => outputs,
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn last(session: &mut Session<'_>, sql: &str) -> String {
    all(session, sql).last().cloned().unwrap_or_default()
}

fn fails(session: &mut Session<'_>, sql: &str) -> SqlError {
    match run(session, sql) {
        Ok(outputs) => panic!("{sql} succeeded: {outputs:?}"),
        Err(error) => error,
    }
}

fn oracle_emp(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::with_dialect(store, Dialect::Oracle);
    last(
        &mut session,
        "CREATE TABLE emp (id NUMBER(9) PRIMARY KEY, dept NUMBER(4), name VARCHAR2(20))",
    );
    last(&mut session, "CREATE INDEX emp_dept_ix ON emp (dept)");
    last(&mut session, "INSERT INTO emp VALUES (1, 10, 'ada')");
    session
}

#[test]
fn explain_plan_creates_plan_table_and_writes_one_row_per_operation() {
    let store = MemoryStore::default();
    let mut session = oracle_emp(&store);
    last(&mut session, "EXPLAIN PLAN FOR SELECT * FROM emp");
    assert_eq!(
        last(
            &mut session,
            "SELECT id, parent_id, depth, operation, options, object_name, object_type, optimizer FROM plan_table ORDER BY id"
        ),
        "ID|PARENT_ID|DEPTH|OPERATION|OPTIONS|OBJECT_NAME|OBJECT_TYPE|OPTIMIZER\n\
         0|NULL|0|SELECT STATEMENT|NULL|NULL|NULL|ALL_ROWS\n\
         1|0|1|TABLE ACCESS|FULL|EMP|TABLE|NULL\nSELECT 2"
    );
    let display = last(&mut session, "SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY)");
    assert!(
        display.starts_with("PLAN_TABLE_OUTPUT\nPlan hash value: "),
        "{display}"
    );
    assert!(
        display.contains("\n| Id  | Operation         | Name |\n"),
        "{display}"
    );
    assert!(
        display
            .contains("\n|   0 | SELECT STATEMENT  |      |\n|   1 |  TABLE ACCESS FULL| EMP  |\n"),
        "{display}"
    );
    assert_eq!(
        last(&mut session, "SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY())"),
        display
    );
}

#[test]
fn explain_plan_keeps_each_plan_under_its_own_id_and_display_picks_one_by_statement_id() {
    let store = MemoryStore::default();
    let mut session = oracle_emp(&store);
    last(&mut session, "EXPLAIN PLAN FOR SELECT * FROM emp");
    last(
        &mut session,
        "EXPLAIN PLAN SET STATEMENT_ID = 'by dept' FOR SELECT name FROM emp WHERE dept = 10",
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT COUNT(DISTINCT plan_id) AS plans FROM plan_table"
        ),
        "PLANS\n2\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT statement_id FROM plan_table WHERE id = 0 ORDER BY plan_id"
        ),
        "STATEMENT_ID\nNULL\nby dept\nSELECT 2"
    );
    let display = last(
        &mut session,
        "SELECT plan_table_output FROM TABLE(DBMS_XPLAN.DISPLAY('PLAN_TABLE', 'by dept'))",
    );
    assert!(
        display.contains("TABLE ACCESS BY INDEX ROWID| EMP"),
        "{display}"
    );
    assert!(display.contains("INDEX RANGE SCAN"), "{display}");
    assert!(display.contains("EMP_DEPT_IX"), "{display}");
    assert_eq!(
        last(
            &mut session,
            "SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY(NULL, 'nope'))"
        ),
        "PLAN_TABLE_OUTPUT\nError: cannot fetch plan for statement_id 'nope'\nSELECT 1"
    );
}

#[test]
fn explain_plan_of_an_update_plans_it_without_changing_a_row() {
    let store = MemoryStore::default();
    let mut session = oracle_emp(&store);
    last(
        &mut session,
        "EXPLAIN PLAN SET STATEMENT_ID = 'u' FOR UPDATE emp SET name = 'bob' WHERE id = 1",
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT operation, options FROM plan_table WHERE statement_id = 'u' ORDER BY id"
        ),
        "OPERATION|OPTIONS\nUPDATE STATEMENT|NULL\nUPDATE|NULL\nTABLE ACCESS|BY INDEX ROWID\nINDEX|UNIQUE SCAN\nSELECT 4"
    );
    assert_eq!(
        last(&mut session, "SELECT name FROM emp"),
        "NAME\nada\nSELECT 1"
    );
}

#[test]
fn a_session_that_displays_plans_answers_explain_plan_with_the_display_and_writes_nothing() {
    let store = MemoryStore::default();
    let mut session = oracle_emp(&store);
    session.set_explain_plan_displays(true);
    let shown = last(&mut session, "EXPLAIN PLAN FOR SELECT * FROM emp");
    assert!(
        shown.starts_with("PLAN_TABLE_OUTPUT\nPlan hash value: "),
        "{shown}"
    );
    assert!(
        shown
            .contains("\n|   0 | SELECT STATEMENT  |      |\n|   1 |  TABLE ACCESS FULL| EMP  |\n"),
        "{shown}"
    );
    assert_eq!(
        last(&mut session, "SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY)"),
        "PLAN_TABLE_OUTPUT\nError: cannot fetch last explain plan from PLAN_TABLE\nSELECT 1"
    );
    session.set_explain_plan_displays(false);
    last(&mut session, "EXPLAIN PLAN FOR SELECT * FROM emp");
    assert_eq!(
        last(&mut session, "SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY)"),
        shown
    );
}

#[test]
fn explain_plan_refuses_what_it_cannot_plan_and_a_missing_into_table() {
    let store = MemoryStore::default();
    let mut session = oracle_emp(&store);
    assert_eq!(
        fails(
            &mut session,
            "EXPLAIN PLAN INTO my_plans FOR SELECT * FROM emp"
        )
        .vendor_code(),
        Some(942)
    );
    assert_eq!(
        fails(
            &mut session,
            "EXPLAIN PLAN FOR INSERT INTO emp VALUES (2, 20, 'x')"
        )
        .state()
        .code(),
        "0A000"
    );
    assert_eq!(
        last(&mut session, "SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY)"),
        "PLAN_TABLE_OUTPUT\nError: cannot fetch last explain plan from PLAN_TABLE\nSELECT 1"
    );
    for hostile in [
        "EXPLAIN PLAN SET STATEMENT_ID = FOR SELECT * FROM emp",
        "EXPLAIN PLAN SET STATEMENT_ID 'x' FOR SELECT * FROM emp",
        "EXPLAIN PLAN INTO FOR SELECT * FROM emp",
        "EXPLAIN PLAN SELECT * FROM emp",
        "EXPLAIN PLAN FOR",
        "SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY(1 + 1))",
        "SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY(",
    ] {
        assert!(run(&mut session, hostile).is_err(), "{hostile}");
    }
}

fn sqlserver_t(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::with_dialect(store, Dialect::SqlServer);
    last(&mut session, "CREATE TABLE t (id INT PRIMARY KEY, b INT)");
    last(&mut session, "CREATE INDEX t_b ON t (b)");
    last(&mut session, "INSERT INTO t VALUES (1, 10), (2, 20)");
    session
}

#[test]
fn showplan_text_returns_the_statement_and_its_plan_tree_without_running_it() {
    let store = MemoryStore::default();
    let mut session = sqlserver_t(&store);
    all(&mut session, "SET SHOWPLAN_TEXT ON");
    let outputs = all(&mut session, "SELECT * FROM t WHERE b = 10");
    assert_eq!(outputs.len(), 2, "{outputs:?}");
    assert!(
        outputs[0].starts_with("StmtText\nSELECT * FROM t WHERE b = 10\n"),
        "{outputs:?}"
    );
    assert!(
        outputs[1].starts_with("StmtText\n  |--Index Seek(OBJECT:([t].[t_b]) ORDERED FORWARD)\n"),
        "{outputs:?}"
    );
    let deleted = all(&mut session, "DELETE FROM t");
    assert!(
        deleted[1].contains("  |--Clustered Index Delete(Delete, OBJECT:([t]))\n       |--Clustered Index Scan(OBJECT:([t]."),
        "{deleted:?}"
    );
    all(&mut session, "SET SHOWPLAN_TEXT OFF");
    assert!(
        last(&mut session, "SELECT COUNT(*) AS n FROM t").contains("\n2\n"),
        "the DELETE ran under SHOWPLAN_TEXT"
    );
}

#[test]
fn showplan_all_returns_sqlserver_columns_with_node_ids_and_parents() {
    let store = MemoryStore::default();
    let mut session = sqlserver_t(&store);
    all(&mut session, "SET SHOWPLAN_ALL ON");
    let plan = last(&mut session, "UPDATE t SET b = 99 WHERE id = 1");
    let lines: Vec<&str> = plan.lines().collect();
    assert_eq!(
        lines[0],
        "StmtText|StmtId|NodeId|Parent|PhysicalOp|LogicalOp|Argument|DefinedValues|EstimateRows|EstimateIO|EstimateCPU|AvgRowSize|TotalSubtreeCost|OutputList|Warnings|Type|Parallel|EstimateExecutions"
    );
    assert!(
        lines[1].starts_with("UPDATE t SET b = 99 WHERE id = 1|1|1|0|NULL|"),
        "{plan}"
    );
    assert!(lines[1].contains("|UPDATE|"), "{plan}");
    assert!(
        lines[2].starts_with(
            "  |--Clustered Index Update(Update, OBJECT:([t]))|1|2|1|Clustered Index Update|Update|OBJECT:([t])|"
        ),
        "{plan}"
    );
    assert!(lines[2].contains("|PLAN_ROW|"), "{plan}");
    assert!(
        lines[3].starts_with("       |--Clustered Index Seek(OBJECT:([t]."),
        "{plan}"
    );
    assert!(lines[3].contains("|1|3|2|Clustered Index Seek|"), "{plan}");
    all(&mut session, "SET SHOWPLAN_ALL OFF");
    assert!(
        last(&mut session, "SELECT b FROM t WHERE id = 1").contains("\n10\n"),
        "the UPDATE ran under SHOWPLAN_ALL"
    );
}

#[test]
fn statistics_profile_runs_the_statement_then_returns_its_plan_with_rows_and_executes() {
    let store = MemoryStore::default();
    let mut session = sqlserver_t(&store);
    all(&mut session, "SET STATISTICS PROFILE ON");
    let outputs = all(&mut session, "SELECT id FROM t");
    assert_eq!(outputs.len(), 2, "{outputs:?}");
    assert!(outputs[0].starts_with("id\n1\n2\n"), "{outputs:?}");
    let profile: Vec<&str> = outputs[1].lines().collect();
    assert!(profile[0].starts_with("Rows|Executes|StmtText|StmtId|NodeId|Parent|"));
    assert!(
        profile[1].starts_with("2|1|SELECT id FROM t|1|1|0|"),
        "{profile:?}"
    );
    assert!(
        profile[2].starts_with("2|1|  |--Clustered Index Scan(OBJECT:([t]."),
        "{profile:?}"
    );
    all(&mut session, "SET STATISTICS PROFILE OFF");
    assert_eq!(all(&mut session, "SELECT id FROM t").len(), 1);
}

#[test]
fn hostile_showplan_options_return_errors_and_never_panic() {
    let store = MemoryStore::default();
    let mut session = sqlserver_t(&store);
    for refused in ["SET SHOWPLAN_XML ON", "SET STATISTICS XML ON"] {
        assert_eq!(
            fails(&mut session, refused).state().code(),
            "0A000",
            "{refused}"
        );
    }
    for hostile in [
        "SET SHOWPLAN_TEXT",
        "SET SHOWPLAN_ALL MAYBE",
        "SET STATISTICS",
        "SET STATISTICS PROFILE",
        "SET STATISTICS NOPE ON",
    ] {
        assert!(run(&mut session, hostile).is_err(), "{hostile}");
    }
    all(&mut session, "SET STATISTICS IO ON");
    all(&mut session, "SET STATISTICS TIME OFF");
}
