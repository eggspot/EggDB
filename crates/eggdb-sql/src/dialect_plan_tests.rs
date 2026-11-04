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

const NARROW_PLAN_TABLE: &str = "CREATE TABLE my_plan (statement_id VARCHAR2(30), plan_id NUMBER, timestamp DATE, operation VARCHAR2(30), options VARCHAR2(255), object_owner VARCHAR2(128), object_name VARCHAR2(3), object_alias VARCHAR2(261), object_type VARCHAR2(30), optimizer VARCHAR2(255), id NUMBER, parent_id NUMBER, depth NUMBER, position NUMBER)";

const PLAN_WITH_A_LONG_INDEX_NAME: &str =
    "EXPLAIN PLAN INTO my_plan FOR SELECT name FROM emp WHERE dept = 10";

#[test]
fn explain_plan_whose_third_row_fails_leaves_no_row_in_a_transaction_or_under_autocommit() {
    let store = MemoryStore::default();
    let mut session = oracle_emp(&store);
    last(&mut session, NARROW_PLAN_TABLE);
    last(&mut session, "COMMIT");
    assert_eq!(
        fails(&mut session, PLAN_WITH_A_LONG_INDEX_NAME).vendor_code(),
        Some(12899)
    );
    assert_eq!(
        last(&mut session, "SELECT COUNT(*) AS n FROM my_plan"),
        "N\n0\nSELECT 1"
    );
    last(&mut session, "COMMIT");
    let mut autocommitting = Session::new(&store);
    last(&mut autocommitting, "SET eggdb.dialect = oracle");
    assert!(run(&mut autocommitting, PLAN_WITH_A_LONG_INDEX_NAME).is_err());
    assert!(!autocommitting.in_block());
    assert_eq!(
        last(&mut autocommitting, "SELECT COUNT(*) AS n FROM my_plan"),
        "N\n0\nSELECT 1"
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
fn under_showplan_transaction_control_and_batch_commands_are_shown_and_never_run() {
    let store = MemoryStore::default();
    let mut session = sqlserver_t(&store);
    all(&mut session, "BEGIN TRANSACTION");
    all(&mut session, "INSERT INTO t VALUES (50, 50)");
    all(&mut session, "SET SHOWPLAN_TEXT ON");
    for shown in [
        "ROLLBACK",
        "COMMIT TRANSACTION",
        "SAVE TRANSACTION keep",
        "ROLLBACK TRANSACTION keep",
        "BEGIN TRANSACTION",
        "USE nowhere",
        "DROP DATABASE nowhere",
        "THROW 50000, 'not raised', 1",
        "RAISERROR('not raised', 16, 1)",
        "WAITFOR DELAY '00:00:03'",
    ] {
        let outputs = all(&mut session, shown);
        assert!(
            outputs
                .first()
                .is_some_and(|output| output.starts_with(&format!("StmtText\n{shown}\n"))),
            "{shown}: {outputs:?}"
        );
    }
    all(&mut session, "SET SHOWPLAN_TEXT OFF");
    assert!(
        last(
            &mut session,
            "SELECT @@TRANCOUNT AS tc, COUNT(*) AS n FROM t"
        )
        .contains("\n1|3\n"),
        "a transaction statement ran under SHOWPLAN_TEXT"
    );
    all(&mut session, "ROLLBACK");
    assert!(last(&mut session, "SELECT COUNT(*) AS n FROM t").contains("\n2\n"));
}

#[test]
fn under_showplan_a_while_loop_and_both_if_branches_are_shown_once_and_never_run() {
    let store = MemoryStore::default();
    let mut session = sqlserver_t(&store);
    all(&mut session, "SET SHOWPLAN_TEXT ON");
    let mut outputs = Vec::new();
    session
        .execute_batch(
            "DECLARE @n INT = (SELECT COUNT(*) FROM t)\nWHILE (SELECT COUNT(*) FROM t) < 5\nBEGIN\n  INSERT INTO t VALUES (99, 1)\nEND\nIF @n = 2\n  DELETE FROM t\nELSE\n  SELECT b FROM t WHERE b = @n",
            &mut |step| {
                outputs.push(match step.outcome {
                    Ok(result) => render(&result),
                    Err(error) => format!("error: {error}"),
                });
                true
            },
        )
        .unwrap();
    assert!(
        !outputs.iter().any(|output| output.starts_with("error: ")),
        "{outputs:?}"
    );
    for statement in [
        "INSERT INTO t VALUES (99, 1)",
        "DELETE FROM t",
        "SELECT b FROM t WHERE b = @n",
    ] {
        assert_eq!(
            outputs
                .iter()
                .filter(|output| output.starts_with(&format!("StmtText\n{statement}\n")))
                .count(),
            1,
            "{statement}: {outputs:?}"
        );
    }
    assert!(
        outputs
            .iter()
            .any(|output| output.contains("Clustered Index Delete")),
        "{outputs:?}"
    );
    all(&mut session, "SET SHOWPLAN_TEXT OFF");
    assert!(
        last(&mut session, "SELECT COUNT(*) AS n FROM t").contains("\n2\n"),
        "a statement in the loop or the IF ran under SHOWPLAN_TEXT"
    );
}

#[test]
fn a_read_only_explainer_s_showplan_refuses_what_it_cannot_plan() {
    let store = MemoryStore::default();
    let mut session = sqlserver_t(&store);
    session.set_explain_plan_displays(true);
    all(&mut session, "SET SHOWPLAN_TEXT ON");
    assert_eq!(all(&mut session, "SELECT b FROM t").len(), 2);
    let refused = fails(&mut session, "DROP TABLE t");
    assert_eq!(refused.state().code(), "0A000", "{refused}");
    all(&mut session, "SET SHOWPLAN_TEXT OFF");
    assert!(last(&mut session, "SELECT COUNT(*) AS n FROM t").contains("\n2\n"));
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

#[test]
fn explain_plan_that_creates_plan_table_keeps_the_transaction_and_its_savepoints() {
    let store = MemoryStore::default();
    let mut session = oracle_emp(&store);
    last(&mut session, "SAVEPOINT before_plan");
    last(&mut session, "EXPLAIN PLAN FOR SELECT * FROM emp");
    assert!(session.in_block());
    last(&mut session, "ROLLBACK TO SAVEPOINT before_plan");
    let error = fails(&mut session, "SELECT COUNT(*) FROM plan_table");
    assert_eq!(error.vendor_code(), Some(942), "{error}");
    assert_eq!(
        last(&mut session, "SELECT COUNT(*) FROM emp"),
        "COUNT(*)\n1\nSELECT 1"
    );
}

#[test]
fn dynamic_sql_and_procedures_cannot_turn_showplan_off_to_run_what_follows() {
    let store = MemoryStore::default();
    let mut session = sqlserver_t(&store);
    last(
        &mut session,
        "CREATE PROCEDURE plan_off AS BEGIN SET SHOWPLAN_ALL OFF; DELETE FROM t; END",
    );
    for (on, escape) in [
        (
            "SET SHOWPLAN_ALL ON",
            "EXEC('SET SHOWPLAN_ALL OFF; DELETE FROM t'); SET SHOWPLAN_ALL OFF; SELECT COUNT(*) AS n FROM t",
        ),
        (
            "SET SHOWPLAN_ALL ON",
            "EXEC('SET SHOWPLAN_ALL OFF'); DELETE FROM t",
        ),
        (
            "SET SHOWPLAN_ALL ON",
            "EXEC sp_executesql N'SET SHOWPLAN_ALL OFF; DELETE FROM t'",
        ),
        (
            "SET SHOWPLAN_TEXT ON",
            "EXEC('SET SHOWPLAN_TEXT OFF; DELETE FROM t')",
        ),
        ("SET SHOWPLAN_TEXT ON", "EXEC plan_off"),
        (
            "SET SHOWPLAN_TEXT ON",
            "EXEC('SET eggdb.dialect = postgresql'); DELETE FROM t",
        ),
    ] {
        all(&mut session, on);
        let _ = run(&mut session, escape);
        assert!(
            run(&mut session, "SELECT 1 AS planned").is_ok_and(|outputs| outputs
                .first()
                .is_some_and(|first| first.starts_with("StmtText"))),
            "SHOWPLAN went off: {escape}"
        );
        all(&mut session, &on.replace(" ON", " OFF"));
        assert!(
            last(&mut session, "SELECT COUNT(*) AS n FROM t").contains("\n2\n"),
            "the DELETE ran: {escape}"
        );
    }
    all(&mut session, "SET SHOWPLAN_ALL ON");
    let refused = fails(&mut session, "EXEC('SET SHOWPLAN_ALL OFF')");
    assert_eq!(refused.vendor_code(), Some(1067), "{refused}");
    all(&mut session, "SET SHOWPLAN_ALL OFF");
    let refused = fails(&mut session, "EXEC('SET SHOWPLAN_TEXT ON; DELETE FROM t')");
    assert_eq!(refused.vendor_code(), Some(1067), "{refused}");
    assert!(last(&mut session, "SELECT COUNT(*) AS n FROM t").contains("\n2\n"));
}
