use crate::memory_store::MemoryStore;
use crate::{Dialect, QueryResult, Session, SqlError};

fn session(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::with_dialect(store, Dialect::Oracle);
    run(
        &mut session,
        "CREATE TABLE log (n NUMBER, msg VARCHAR2(200))",
    );
    session
}

fn run(session: &mut Session<'_>, text: &str) {
    session
        .execute(text, &mut |_: QueryResult| {})
        .unwrap_or_else(|error| panic!("{text}: {error}"));
}

fn failure(session: &mut Session<'_>, text: &str) -> SqlError {
    session
        .execute(text, &mut |_| {})
        .expect_err("the block fails")
}

fn messages(session: &mut Session<'_>) -> Vec<String> {
    let mut found = Vec::new();
    session
        .execute(
            "SELECT msg FROM log ORDER BY n",
            &mut |result: QueryResult| {
                for row in result.rows {
                    found.push(
                        row.first()
                            .and_then(|value| value.to_text())
                            .unwrap_or_else(|| "NULL".to_owned()),
                    );
                }
            },
        )
        .unwrap();
    found
}

#[test]
fn goto_jumps_forward_over_statements() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    run(
        &mut session,
        "BEGIN \
           INSERT INTO log VALUES (1, 'before'); \
           GOTO done; \
           INSERT INTO log VALUES (2, 'skipped'); \
           <<done>> \
           INSERT INTO log VALUES (3, 'after'); \
         END;",
    );
    assert_eq!(messages(&mut session), ["before", "after"]);
}

#[test]
fn goto_jumps_backward_to_repeat_statements() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE i NUMBER := 0; BEGIN \
           <<again>> \
           i := i + 1; \
           INSERT INTO log VALUES (i, 'pass ' || i); \
           IF i < 3 THEN GOTO again; END IF; \
         END;",
    );
    assert_eq!(messages(&mut session), ["pass 1", "pass 2", "pass 3"]);
}

#[test]
fn goto_leaves_loops_and_nested_blocks() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    run(
        &mut session,
        "BEGIN \
           FOR i IN 1..10 LOOP \
             BEGIN \
               INSERT INTO log VALUES (i, 'row ' || i); \
               IF i = 2 THEN GOTO finished; END IF; \
             END; \
           END LOOP; \
           <<finished>> \
           INSERT INTO log VALUES (99, 'end'); \
         END;",
    );
    assert_eq!(messages(&mut session), ["row 1", "row 2", "end"]);
}

#[test]
fn goto_from_a_handler_reaches_the_enclosing_block() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE tries NUMBER := 0; BEGIN \
           <<retry>> \
           BEGIN \
             tries := tries + 1; \
             IF tries < 2 THEN RAISE NO_DATA_FOUND; END IF; \
             INSERT INTO log VALUES (tries, 'succeeded on try ' || tries); \
           EXCEPTION WHEN NO_DATA_FOUND THEN \
             INSERT INTO log VALUES (0, 'failed once'); \
             GOTO retry; \
           END; \
         END;",
    );
    assert_eq!(
        messages(&mut session),
        ["failed once", "succeeded on try 2"]
    );
}

#[test]
fn goto_labels_can_stack_and_name_loops() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE n NUMBER := 0; BEGIN \
           GOTO second; \
           INSERT INTO log VALUES (1, 'skipped'); \
           <<first>> <<second>> \
           FOR i IN 1..2 LOOP n := n + i; END LOOP; \
           INSERT INTO log VALUES (n, 'sum ' || n); \
         END;",
    );
    assert_eq!(messages(&mut session), ["sum 3"]);
}

#[test]
fn goto_into_a_nested_statement_does_not_compile() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let into_if = failure(
        &mut session,
        "BEGIN GOTO inside; IF 1 = 1 THEN <<inside>> NULL; END IF; END;",
    );
    assert!(into_if.message().contains("PLS-00375"), "{into_if}");
    let into_loop = failure(
        &mut session,
        "BEGIN GOTO inside; LOOP <<inside>> NULL; EXIT; END LOOP; END;",
    );
    assert!(into_loop.message().contains("PLS-00375"), "{into_loop}");
    let back_from_handler = failure(
        &mut session,
        "BEGIN <<top>> NULL; RAISE NO_DATA_FOUND; \
         EXCEPTION WHEN NO_DATA_FOUND THEN GOTO top; END;",
    );
    assert!(
        back_from_handler.message().contains("PLS-00375"),
        "{back_from_handler}"
    );
    let missing = failure(&mut session, "BEGIN GOTO nowhere; END;");
    assert!(missing.message().contains("PLS-00201"), "{missing}");
    assert!(missing.message().contains("NOWHERE"), "{missing}");
    let at_end = failure(&mut session, "BEGIN NULL; <<last>> END;");
    assert!(crate::plsql::is_compilation_error(&at_end), "{at_end}");
}

#[test]
fn a_procedure_with_goto_compiles_and_runs() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    run(
        &mut session,
        "CREATE PROCEDURE classify(n NUMBER) IS BEGIN \
           IF n < 0 THEN GOTO negative; END IF; \
           INSERT INTO log VALUES (n, 'non-negative'); \
           GOTO done; \
           <<negative>> \
           INSERT INTO log VALUES (n, 'negative'); \
           <<done>> \
           NULL; \
         END;",
    );
    run(&mut session, "BEGIN classify(-1); classify(4); END;");
    assert_eq!(messages(&mut session), ["negative", "non-negative"]);
}

#[test]
fn autonomous_transactions_are_refused_with_a_reason() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let refused = failure(
        &mut session,
        "CREATE PROCEDURE audit_it IS PRAGMA AUTONOMOUS_TRANSACTION; BEGIN \
           INSERT INTO log VALUES (1, 'audit'); COMMIT; END;",
    );
    assert!(
        refused.message().contains("AUTONOMOUS_TRANSACTION"),
        "{refused}"
    );
}
