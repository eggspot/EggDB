use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Dialect, Session, SqlError};

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> Result<String, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs.pop().unwrap_or_default())
}

fn answer(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    match run(session, sql) {
        Ok(output) => output,
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn state(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    match run(session, sql) {
        Ok(output) => panic!("{sql} succeeded: {output}"),
        Err(error) => error.state().code().to_owned(),
    }
}

fn with_tables(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = Session::new(store);
    answer(
        &mut session,
        "CREATE TABLE a (id integer PRIMARY KEY, n integer);
         INSERT INTO a VALUES (1, 2), (2, 0), (3, 1);
         CREATE TABLE b (id integer PRIMARY KEY, a_id integer, v text);
         INSERT INTO b VALUES (1, 1, 'x'), (2, 1, 'y'), (3, 3, 'z')",
    );
    session
}

#[test]
fn lateral_subqueries_and_functions_see_the_items_before_them() {
    let store = MemoryStore::default();
    let mut session = with_tables(&store);
    assert_eq!(
        answer(
            &mut session,
            "SELECT a.id, s.total FROM a, LATERAL (SELECT count(*) AS total FROM b WHERE b.a_id = a.id) s ORDER BY a.id"
        ),
        "id|total\n1|2\n2|0\n3|1\nSELECT 3"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT a.id, g FROM a, generate_series(1, a.n) AS g ORDER BY a.id, g"
        ),
        "id|g\n1|1\n1|2\n3|1\nSELECT 3"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT a.id, s.v FROM a LEFT JOIN LATERAL (SELECT v FROM b WHERE b.a_id = a.id ORDER BY v LIMIT 1) s ON true ORDER BY a.id"
        ),
        "id|v\n1|x\n2|NULL\n3|z\nSELECT 3"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT count(*) FROM a, LATERAL (SELECT a.n AS m) s WHERE s.m = a.n"
        ),
        "count\n3\nSELECT 1"
    );
    assert_eq!(
        state(
            &mut session,
            "SELECT * FROM a RIGHT JOIN LATERAL (SELECT a.id) s ON true"
        ),
        "42P10"
    );
}

#[test]
fn cross_and_outer_apply_run_as_lateral_joins_in_transact_sql() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    run(
        &mut session,
        "CREATE TABLE a (id INT PRIMARY KEY, n INT);
         INSERT INTO a VALUES (1, 2), (2, 0), (3, 1);
         CREATE TABLE b (id INT PRIMARY KEY, a_id INT, v VARCHAR(10));
         INSERT INTO b VALUES (1, 1, 'x'), (2, 1, 'y'), (3, 3, 'z')",
    )
    .unwrap();
    let cross = run(
        &mut session,
        "SELECT a.id, s.total FROM a CROSS APPLY (SELECT COUNT(*) AS total FROM b WHERE b.a_id = a.id) s ORDER BY a.id",
    )
    .unwrap();
    assert!(cross.contains("1|2\n2|0\n3|1"), "{cross}");
    let outer = run(
        &mut session,
        "SELECT a.id, s.v FROM a OUTER APPLY (SELECT TOP 1 v FROM b WHERE b.a_id = a.id ORDER BY v) s ORDER BY a.id",
    )
    .unwrap();
    assert!(outer.contains("1|x\n2|NULL\n3|z"), "{outer}");
}

#[test]
fn full_joins_with_using_merge_the_joined_columns() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE l (k integer, x text); INSERT INTO l VALUES (1, 'a'), (2, 'b');
         CREATE TABLE r (k integer, y text); INSERT INTO r VALUES (2, 'c'), (3, 'd')",
    );
    assert_eq!(
        answer(&mut session, "SELECT * FROM l FULL JOIN r USING (k) ORDER BY k"),
        "k|x|y\n1|a|NULL\n2|b|c\n3|NULL|d\nSELECT 3"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT k, l.k, r.k FROM l FULL JOIN r USING (k) ORDER BY 1"
        ),
        "k|k|k\n1|1|NULL\n2|2|2\n3|NULL|3\nSELECT 3"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT count(*) FROM l FULL OUTER JOIN r USING (k) WHERE k > 1"
        ),
        "count\n2\nSELECT 1"
    );
}

#[test]
fn correlated_subqueries_reach_any_level_and_grouped_queries() {
    let store = MemoryStore::default();
    let mut session = with_tables(&store);
    assert_eq!(
        answer(
            &mut session,
            "SELECT a.id, (SELECT count(*) FROM b WHERE b.a_id = a.id AND EXISTS (SELECT 1 FROM b c WHERE c.id = b.id AND c.a_id = a.id)) AS c FROM a ORDER BY a.id"
        ),
        "id|c\n1|2\n2|0\n3|1\nSELECT 3"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT b.a_id, count(*) AS n, (SELECT a.n FROM a WHERE a.id = b.a_id) AS an FROM b GROUP BY b.a_id ORDER BY b.a_id"
        ),
        "a_id|n|an\n1|2|2\n3|1|1\nSELECT 2"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT b.a_id FROM b GROUP BY b.a_id HAVING (SELECT a.n FROM a WHERE a.id = b.a_id) > 1"
        ),
        "a_id\n1\nSELECT 1"
    );
    assert_eq!(
        state(
            &mut session,
            "SELECT b.a_id, (SELECT b.v) FROM b GROUP BY b.a_id"
        ),
        "42803"
    );
}

#[test]
fn update_and_delete_run_subqueries_that_name_the_rows_they_change() {
    let store = MemoryStore::default();
    let mut session = with_tables(&store);
    assert_eq!(
        answer(
            &mut session,
            "UPDATE a SET n = 10 * (SELECT count(*) FROM b WHERE b.a_id = a.id) WHERE EXISTS (SELECT 1 FROM b WHERE b.a_id = a.id)"
        ),
        "UPDATE 2"
    );
    assert_eq!(
        answer(&mut session, "SELECT id, n FROM a ORDER BY id"),
        "id|n\n1|20\n2|0\n3|10\nSELECT 3"
    );
    assert_eq!(
        answer(
            &mut session,
            "DELETE FROM b WHERE NOT EXISTS (SELECT 1 FROM a WHERE a.id = b.a_id AND a.n > 15) RETURNING id"
        ),
        "id\n3\nDELETE 1"
    );
}
