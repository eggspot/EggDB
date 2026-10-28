use crate::memory_store::MemoryStore;
use crate::{
    Dialect, OneStatementError, OneStatementRefusal, QueryResult, Session, SqlType, Value,
};

fn session(store: &MemoryStore, dialect: Dialect) -> Session<'_, MemoryStore> {
    Session::with_dialect(store, dialect)
}

fn store_with_rows() -> MemoryStore {
    let store = MemoryStore::default();
    session(&store, Dialect::PostgreSql)
        .execute(
            "CREATE TABLE t (id integer PRIMARY KEY, name text); INSERT INTO t VALUES (1, 'a'), (2, 'b')",
            &mut |_| {},
        )
        .unwrap();
    store
}

fn one(
    store: &MemoryStore,
    dialect: Dialect,
    text: &str,
) -> Result<Vec<QueryResult>, OneStatementError> {
    let mut results = Vec::new();
    session(store, dialect).run_one_statement(text, &mut |result| {
        results.push(result);
        true
    })?;
    Ok(results)
}

fn refusal(store: &MemoryStore, dialect: Dialect, text: &str) -> OneStatementRefusal {
    match one(store, dialect, text) {
        Err(OneStatementError::Refused(refusal)) => refusal,
        other => panic!("{dialect:?} {text:?} was not refused: {other:?}"),
    }
}

fn row_count(store: &MemoryStore) -> usize {
    let mut rows = 0;
    session(store, Dialect::PostgreSql)
        .execute("SELECT id FROM t", &mut |result| rows = result.rows.len())
        .unwrap();
    rows
}

#[test]
fn one_statement_runs_with_or_without_trailing_semicolons_in_every_dialect() {
    let store = store_with_rows();
    for dialect in Dialect::ALL {
        for text in [
            "SELECT id FROM t ORDER BY id",
            "SELECT id FROM t ORDER BY id;",
            " ;; SELECT id FROM t ORDER BY id ;; -- done",
        ] {
            let results = one(&store, dialect, text).unwrap();
            assert_eq!(results.len(), 1, "{dialect:?} {text:?}");
            assert_eq!(results[0].rows.len(), 2, "{dialect:?} {text:?}");
        }
    }
}

#[test]
fn a_second_statement_is_refused_before_the_first_runs() {
    let store = store_with_rows();
    for dialect in Dialect::ALL {
        let text = "DELETE FROM t WHERE id = 1; SELECT 1";
        assert_eq!(
            refusal(&store, dialect, text),
            OneStatementRefusal::MoreThanOne { position: 28 },
            "{dialect:?}"
        );
        assert_eq!(row_count(&store), 2, "{dialect:?}");
    }
    let literal = "INSERT INTO t VALUES (3, 'c'); DROP TABLE t";
    for dialect in [Dialect::PostgreSql, Dialect::MySql, Dialect::Sqlite] {
        assert!(matches!(
            refusal(&store, dialect, literal),
            OneStatementRefusal::MoreThanOne { .. }
        ));
        assert_eq!(row_count(&store), 2, "{dialect:?}");
    }
}

#[test]
fn transaction_control_is_refused_in_every_spelling_and_dialect() {
    let store = store_with_rows();
    for dialect in Dialect::ALL {
        for text in [
            "BEGIN",
            "begin work",
            "START TRANSACTION",
            "COMMIT",
            "commit work",
            "END",
            "ROLLBACK",
            "abort",
        ] {
            match one(&store, dialect, text) {
                Err(OneStatementError::Refused(OneStatementRefusal::TransactionControl)) => {}
                Err(OneStatementError::Failed(_)) if dialect != Dialect::PostgreSql => {}
                other => panic!("{dialect:?} {text:?}: {other:?}"),
            }
        }
    }
    assert_eq!(
        refusal(&store, Dialect::Sqlite, "BEGIN IMMEDIATE TRANSACTION"),
        OneStatementRefusal::TransactionControl
    );
    for dialect in Dialect::ALL {
        for text in ["COMMIT", "ROLLBACK"] {
            assert_eq!(
                refusal(&store, dialect, text),
                OneStatementRefusal::TransactionControl,
                "{dialect:?} {text:?}"
            );
        }
    }
    assert!(matches!(
        refusal(&store, Dialect::PostgreSql, "COMMIT; DROP TABLE t"),
        OneStatementRefusal::TransactionControl
    ));
    assert_eq!(row_count(&store), 2);
}

#[test]
fn copy_and_empty_text_are_refused() {
    let store = store_with_rows();
    assert_eq!(
        refusal(&store, Dialect::PostgreSql, "COPY t TO STDOUT"),
        OneStatementRefusal::Copy
    );
    assert_eq!(
        refusal(&store, Dialect::PostgreSql, "COPY t FROM STDIN"),
        OneStatementRefusal::Copy
    );
    for text in ["", " ", ";", " ; ; ", "-- nothing", "/* nothing */"] {
        assert_eq!(
            refusal(&store, Dialect::PostgreSql, text),
            OneStatementRefusal::Empty,
            "{text:?}"
        );
    }
}

#[test]
fn a_parse_error_fails_in_the_dialect_and_runs_nothing() {
    let store = store_with_rows();
    match one(&store, Dialect::PostgreSql, "DELETE FROM t WHERE; SELECT 1") {
        Err(OneStatementError::Failed(error)) => assert_eq!(error.state().code(), "42601"),
        other => panic!("{other:?}"),
    }
    match one(&store, Dialect::Oracle, "SELEC 1 FROM dual") {
        Err(OneStatementError::Failed(error)) => assert_eq!(error.vendor_code(), Some(900)),
        other => panic!("{other:?}"),
    }
    assert_eq!(row_count(&store), 2);
}

#[test]
fn the_literal_insert_fast_path_is_one_plain_insert() {
    let store = store_with_rows();
    for dialect in [Dialect::PostgreSql, Dialect::MySql, Dialect::Sqlite] {
        let id = 10 + dialect as i32;
        let results = one(
            &store,
            dialect,
            &format!("INSERT INTO t VALUES ({id}, 'fast');"),
        )
        .unwrap();
        assert_eq!(results.len(), 1, "{dialect:?}");
    }
    assert_eq!(row_count(&store), 5);
}

#[test]
fn views_are_listed_with_their_columns_schema_and_comment() {
    let store = store_with_rows();
    session(&store, Dialect::PostgreSql)
        .execute(
            "CREATE SCHEMA reports; CREATE VIEW reports.names AS SELECT id, name FROM t; \
             CREATE VIEW counted AS SELECT count(*) AS n FROM t; \
             COMMENT ON VIEW counted IS 'how many'",
            &mut |_| {},
        )
        .unwrap();
    let views = session(&store, Dialect::PostgreSql).views().unwrap();
    let names: Vec<(&str, &str)> = views
        .iter()
        .map(|view| (view.schema.as_str(), view.name.as_str()))
        .collect();
    assert_eq!(names, [("public", "counted"), ("reports", "reports.names")]);
    assert_eq!(views[0].comment.as_deref(), Some("how many"));
    assert_eq!(views[0].columns[0].name, "n");
    assert_eq!(views[0].columns[0].sql_type, Some(SqlType::BigInt));
    assert_eq!(views[1].columns.len(), 2);
    let tables = session(&store, Dialect::PostgreSql).tables().unwrap();
    assert_eq!(tables.len(), 1);
    let mut rows = Vec::new();
    session(&store, Dialect::PostgreSql)
        .run_one_statement(
            "SELECT name FROM reports.names ORDER BY id",
            &mut |result| {
                rows = result.rows;
                true
            },
        )
        .unwrap();
    assert_eq!(
        rows,
        vec![vec![Value::Text("a".into())], vec![Value::Text("b".into())]]
    );
}
