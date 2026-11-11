use crate::memory_store::MemoryStore;
use crate::{Dialect, Session, Value};

fn rows(session: &mut Session<'_>, sql: &str) -> Vec<Vec<Value>> {
    let mut found = Vec::new();
    session
        .execute(sql, &mut |result| found = result.rows)
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    found
}

fn market(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::with_dialect(store, Dialect::PostgreSql);
    rows(
        &mut session,
        "CREATE TABLE trades (sym text, ts int, qty int);
         CREATE TABLE quotes (sym text, ts int, price int);
         INSERT INTO trades VALUES ('a', 5, 1), ('a', 12, 2), ('b', 3, 3);
         INSERT INTO quotes VALUES ('a', 1, 100), ('a', 10, 110), ('b', 4, 200)",
    );
    session
}

#[test]
fn asof_join_takes_the_latest_row_at_or_before_each_left_row() {
    let store = MemoryStore::default();
    let mut session = market(&store);
    assert_eq!(
        rows(
            &mut session,
            "SELECT t.ts, q.price FROM trades t ASOF JOIN quotes q ON t.sym = q.sym AND t.ts >= q.ts ORDER BY t.ts",
        ),
        vec![
            vec![Value::Integer(5), Value::Integer(100)],
            vec![Value::Integer(12), Value::Integer(110)],
        ]
    );
}

#[test]
fn asof_join_using_matches_the_keys_and_takes_the_last_column_as_the_time() {
    let store = MemoryStore::default();
    let mut session = market(&store);
    assert_eq!(
        rows(
            &mut session,
            "SELECT t.ts, q.price FROM trades t ASOF LEFT JOIN quotes q USING (sym, ts) ORDER BY t.ts",
        ),
        vec![
            vec![Value::Integer(3), Value::Null],
            vec![Value::Integer(5), Value::Integer(100)],
            vec![Value::Integer(12), Value::Integer(110)],
        ]
    );
    let error = session
        .execute(
            "SELECT * FROM trades ASOF JOIN trades USING (sym, ts)",
            &mut |_| {},
        )
        .expect_err("the same label on both sides");
    assert!(error.message().contains("alias"), "{error}");
}
