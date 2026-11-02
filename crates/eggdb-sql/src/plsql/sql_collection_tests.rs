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
        .expect_err("the statement fails")
}

fn rows(session: &mut Session<'_>, text: &str) -> Vec<Vec<String>> {
    let mut found = Vec::new();
    session
        .execute(text, &mut |result: QueryResult| {
            for row in result.rows {
                found.push(
                    row.iter()
                        .map(|value| value.to_text().unwrap_or_else(|| "NULL".to_owned()))
                        .collect(),
                );
            }
        })
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    found
}

fn column(session: &mut Session<'_>, text: &str) -> Vec<String> {
    rows(session, text)
        .into_iter()
        .map(|row| row.into_iter().next().unwrap_or_default())
        .collect()
}

fn create_numbers_package(session: &mut Session<'_>) {
    run(
        session,
        "CREATE PACKAGE nums AS \
           TYPE num_tab IS TABLE OF NUMBER; \
           FUNCTION upto(n NUMBER) RETURN num_tab PIPELINED; \
           FUNCTION listed RETURN num_tab; \
         END nums;",
    );
    run(
        session,
        "CREATE PACKAGE BODY nums AS \
           FUNCTION upto(n NUMBER) RETURN num_tab PIPELINED IS \
           BEGIN \
             FOR i IN 1..n LOOP PIPE ROW (i * 10); END LOOP; \
             RETURN; \
           END upto; \
           FUNCTION listed RETURN num_tab IS \
           BEGIN \
             RETURN num_tab(7, 8); \
           END listed; \
         END nums;",
    );
}

#[test]
fn a_pipelined_function_is_a_table_in_from() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    create_numbers_package(&mut session);
    assert_eq!(
        column(&mut session, "SELECT column_value FROM TABLE(nums.upto(3))"),
        ["10", "20", "30"]
    );
    assert_eq!(
        column(
            &mut session,
            "SELECT t.column_value * 2 FROM TABLE(nums.upto(2)) t ORDER BY 1 DESC"
        ),
        ["40", "20"]
    );
    assert!(column(&mut session, "SELECT * FROM TABLE(nums.upto(0))").is_empty());
}

#[test]
fn a_function_returning_a_nested_table_is_a_table_in_from() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    create_numbers_package(&mut session);
    assert_eq!(
        column(
            &mut session,
            "SELECT column_value FROM TABLE(nums.listed())"
        ),
        ["7", "8"]
    );
}

#[test]
fn pipe_row_needs_a_pipelined_function() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let refused = failure(
        &mut session,
        "CREATE FUNCTION f RETURN NUMBER IS BEGIN PIPE ROW (1); RETURN 1; END;",
    );
    assert!(refused.message().contains("PLS-00629"), "{refused}");
}

#[test]
fn a_collection_variable_is_a_table_in_plsql_sql() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE \
           TYPE num_tab IS TABLE OF NUMBER; \
           v num_tab := num_tab(3, 1, 2); \
         BEGIN \
           INSERT INTO log SELECT column_value, 'element' FROM TABLE(v); \
         END;",
    );
    assert_eq!(
        column(&mut session, "SELECT n FROM log ORDER BY n"),
        ["1", "2", "3"]
    );
}

#[test]
fn member_of_cardinality_and_multiset_operators_work_on_collections() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    run(
        &mut session,
        "DECLARE \
           TYPE num_tab IS TABLE OF NUMBER; \
           v num_tab := num_tab(1, 2, 2, 3); \
           u num_tab := num_tab(2, 4); \
           small num_tab := num_tab(2, 3); \
           empty num_tab := num_tab(); \
         BEGIN \
           IF 2 MEMBER OF v THEN INSERT INTO log VALUES (1, 'member'); END IF; \
           IF 5 NOT MEMBER OF v THEN INSERT INTO log VALUES (2, 'not member'); END IF; \
           IF small SUBMULTISET OF v THEN INSERT INTO log VALUES (3, 'submultiset'); END IF; \
           IF NOT (u SUBMULTISET OF v) THEN INSERT INTO log VALUES (4, 'not submultiset'); END IF; \
           INSERT INTO log VALUES (5, 'cardinality ' || CARDINALITY(v)); \
           INSERT INTO log VALUES (6, 'union all ' || CARDINALITY(v MULTISET UNION u)); \
           INSERT INTO log VALUES (7, 'union distinct ' || CARDINALITY(v MULTISET UNION DISTINCT u)); \
           INSERT INTO log VALUES (8, 'intersect ' || CARDINALITY(v MULTISET INTERSECT u)); \
           INSERT INTO log VALUES (9, 'except ' || CARDINALITY(v MULTISET EXCEPT u)); \
           INSERT INTO log VALUES (10, 'except distinct ' || CARDINALITY(v MULTISET EXCEPT DISTINCT small)); \
           IF CARDINALITY(empty) IS NULL THEN INSERT INTO log VALUES (11, 'empty is null'); END IF; \
           IF NOT (1 MEMBER OF empty) THEN INSERT INTO log VALUES (12, 'nothing in empty'); END IF; \
         END;",
    );
    assert_eq!(
        column(&mut session, "SELECT msg FROM log ORDER BY n"),
        [
            "member",
            "not member",
            "submultiset",
            "not submultiset",
            "cardinality 4",
            "union all 6",
            "union distinct 4",
            "intersect 1",
            "except 3",
            "except distinct 1",
            "empty is null",
            "nothing in empty",
        ]
    );
}

#[test]
fn member_as_a_column_alias_still_parses() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    assert_eq!(column(&mut session, "SELECT 1 member FROM dual"), ["1"]);
}

fn three_valued(condition: &str) -> String {
    format!(
        "IF {condition} THEN INSERT INTO log VALUES (0, 'TRUE'); \
         ELSIF NOT ({condition}) THEN INSERT INTO log VALUES (0, 'FALSE'); \
         ELSE INSERT INTO log VALUES (0, 'NULL'); END IF;"
    )
}

#[test]
fn submultiset_of_and_member_of_follow_oracles_null_rules() {
    let cases = [
        ("empty SUBMULTISET OF nul", "TRUE"),
        ("empty SUBMULTISET OF v", "TRUE"),
        ("v SUBMULTISET OF nul", "NULL"),
        ("nul SUBMULTISET OF v", "NULL"),
        ("with_null SUBMULTISET OF v", "NULL"),
        ("one_seven SUBMULTISET OF v_null", "NULL"),
        ("one_seven SUBMULTISET OF v", "FALSE"),
        ("one_two SUBMULTISET OF v_null", "TRUE"),
        ("v SUBMULTISET OF with_null", "FALSE"),
        ("n MEMBER OF empty", "FALSE"),
        ("1 MEMBER OF empty", "FALSE"),
        ("n MEMBER OF v", "NULL"),
        ("5 MEMBER OF with_null", "NULL"),
        ("1 MEMBER OF with_null", "TRUE"),
        ("1 MEMBER OF nul", "NULL"),
    ];
    for (condition, expected) in cases {
        let store = MemoryStore::default();
        let mut session = session(&store);
        run(
            &mut session,
            &format!(
                "DECLARE \
                   TYPE num_tab IS TABLE OF NUMBER; \
                   v num_tab := num_tab(1, 2, 2, 3); \
                   v_null num_tab := num_tab(1, 2, NULL); \
                   with_null num_tab := num_tab(1, NULL); \
                   one_seven num_tab := num_tab(1, 7); \
                   one_two num_tab := num_tab(1, 2); \
                   empty num_tab := num_tab(); \
                   nul num_tab; \
                   n NUMBER; \
                 BEGIN {} END;",
                three_valued(condition)
            ),
        );
        assert_eq!(
            column(&mut session, "SELECT msg FROM log"),
            [expected],
            "{condition}"
        );
    }
}
