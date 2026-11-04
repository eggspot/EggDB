use crate::memory_store::MemoryStore;
use crate::{BatchStep, Dialect, QueryResult, Session, SqlError, Value};

fn session(store: &MemoryStore) -> Session<'_> {
    Session::with_dialect(store, Dialect::SqlServer)
}

fn batch(session: &mut Session<'_>, text: &str) -> Vec<BatchStep> {
    let mut steps = Vec::new();
    session
        .execute_batch(text, &mut |step| {
            steps.push(step);
            true
        })
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    steps
}

fn results(steps: &[BatchStep]) -> Vec<&QueryResult> {
    steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().ok())
        .collect()
}

fn errors(steps: &[BatchStep]) -> Vec<Option<u32>> {
    steps
        .iter()
        .filter_map(|step| step.outcome.as_ref().err())
        .map(SqlError::sqlserver_number)
        .collect()
}

fn rows(session: &mut Session<'_>, text: &str) -> Vec<Vec<Value>> {
    let mut last = Vec::new();
    session
        .execute(text, &mut |result| last = result.rows)
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    last
}

fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

fn integers(rows: &[&[i32]]) -> Vec<Vec<Value>> {
    rows.iter()
        .map(|row| row.iter().map(|&value| Value::Integer(value)).collect())
        .collect()
}

#[test]
fn output_into_a_table_variable_keeps_the_rows_each_statement_changed() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    rows(
        &mut session,
        "CREATE TABLE stock (id int PRIMARY KEY, qty int)",
    );
    rows(
        &mut session,
        "INSERT INTO stock VALUES (1, 10), (2, 20), (3, 30)",
    );
    let steps = batch(
        &mut session,
        "DECLARE @log TABLE (id int, old_qty int, new_qty int)
UPDATE stock SET qty = qty + 1 OUTPUT inserted.id, inserted.qty - 1, inserted.qty INTO @log WHERE id < 3
DELETE FROM stock OUTPUT deleted.id, deleted.qty, NULL INTO @log WHERE id = 3
INSERT INTO stock (id, qty) OUTPUT inserted.id, NULL, inserted.qty INTO @log (id, old_qty, new_qty) VALUES (4, 40)
SELECT id, old_qty, new_qty FROM @log ORDER BY id",
    );
    assert!(errors(&steps).is_empty(), "{steps:?}");
    let results = results(&steps);
    let tags: Vec<&str> = results.iter().map(|result| result.tag.as_str()).collect();
    assert_eq!(tags, ["UPDATE 2", "DELETE 1", "INSERT 0 1", "SELECT 4"]);
    assert!(results[..3].iter().all(|result| result.columns.is_none()));
    assert_eq!(
        results[3].rows,
        vec![
            vec![Value::Integer(1), Value::Integer(10), Value::Integer(11)],
            vec![Value::Integer(2), Value::Integer(20), Value::Integer(21)],
            vec![Value::Integer(3), Value::Integer(30), Value::Null],
            vec![Value::Integer(4), Value::Null, Value::Integer(40)],
        ]
    );
}

#[test]
fn output_into_a_table_can_be_followed_by_an_output_to_the_client() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    rows(
        &mut session,
        "CREATE TABLE orders (id int PRIMARY KEY, total int)",
    );
    rows(&mut session, "CREATE TABLE audit (order_id int, total int)");
    rows(
        &mut session,
        "CREATE TABLE changes (action nvarchar(10), id int)",
    );
    assert_eq!(
        rows(
            &mut session,
            "INSERT INTO orders OUTPUT inserted.id, inserted.total INTO audit OUTPUT inserted.id VALUES (1, 100), (2, 200)",
        ),
        integers(&[&[1], &[2]])
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT order_id, total FROM audit ORDER BY order_id"
        ),
        integers(&[&[1, 100], &[2, 200]])
    );
    rows(
        &mut session,
        "MERGE orders AS t USING (VALUES (1, 150), (3, 300)) AS s (id, total) ON t.id = s.id \
         WHEN MATCHED THEN UPDATE SET total = s.total \
         WHEN NOT MATCHED THEN INSERT (id, total) VALUES (s.id, s.total) \
         OUTPUT $action, inserted.id INTO changes",
    );
    assert_eq!(
        rows(&mut session, "SELECT action, id FROM changes ORDER BY id"),
        vec![
            vec![text("UPDATE"), Value::Integer(1)],
            vec![text("INSERT"), Value::Integer(3)],
        ]
    );
    rows(&mut session, "CREATE TABLE #seen (id int)");
    rows(
        &mut session,
        "DELETE FROM orders OUTPUT deleted.id INTO #seen WHERE id = 3",
    );
    assert_eq!(
        rows(&mut session, "SELECT id FROM #seen"),
        integers(&[&[3]])
    );
}

#[test]
fn a_failed_output_into_changes_nothing() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    rows(
        &mut session,
        "CREATE TABLE orders (id int PRIMARY KEY, total int)",
    );
    rows(&mut session, "INSERT INTO orders VALUES (1, 100), (2, 200)");
    let steps = batch(
        &mut session,
        "DECLARE @ids TABLE (id int PRIMARY KEY)
UPDATE orders SET total = 0 OUTPUT 7 INTO @ids
SELECT COUNT(*) FROM @ids",
    );
    assert_eq!(errors(&steps), vec![Some(2627)]);
    assert_eq!(
        results(&steps).last().map(|result| result.rows.clone()),
        Some(integers(&[&[0]]))
    );
    let steps = batch(
        &mut session,
        "DECLARE @pair TABLE (a int, b int)
DELETE FROM orders OUTPUT deleted.id INTO @pair",
    );
    assert_eq!(errors(&steps), vec![Some(213)]);
    let steps = batch(
        &mut session,
        "INSERT INTO orders OUTPUT inserted.id INTO missing VALUES (9, 900)",
    );
    assert_eq!(errors(&steps), vec![Some(208)]);
    assert_eq!(
        rows(&mut session, "SELECT id, total FROM orders ORDER BY id"),
        integers(&[&[1, 100], &[2, 200]])
    );
}
