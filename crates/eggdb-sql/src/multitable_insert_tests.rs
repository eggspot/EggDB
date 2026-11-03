use crate::memory_store::MemoryStore;
use crate::{Dialect, QueryResult, Session, SqlError};

fn results(session: &mut Session<'_>, text: &str) -> Vec<QueryResult> {
    let mut results = Vec::new();
    session
        .execute(text, &mut |result| results.push(result))
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    results
}

fn tag(session: &mut Session<'_>, text: &str) -> String {
    results(session, text)
        .pop()
        .map(|result| result.tag)
        .unwrap_or_default()
}

fn lines(session: &mut Session<'_>, text: &str) -> Vec<String> {
    results(session, text)
        .pop()
        .map(|result| result.rows)
        .unwrap_or_default()
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| Dialect::Oracle.value_text(value).unwrap_or_default())
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn error(session: &mut Session<'_>, text: &str) -> SqlError {
    session.execute(text, &mut |_| {}).expect_err(text)
}

fn sales(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::with_dialect(store, Dialect::Oracle);
    for text in [
        "CREATE TABLE sales (id NUMBER, amount NUMBER)",
        "INSERT INTO sales VALUES (1, 50)",
        "INSERT INTO sales VALUES (2, 150)",
        "INSERT INTO sales VALUES (3, 500)",
        "CREATE TABLE small_sales (id NUMBER, amount NUMBER)",
        "CREATE TABLE large_sales (id NUMBER, amount NUMBER)",
        "CREATE TABLE sale_ids (id NUMBER, note VARCHAR2(10) DEFAULT 'seen')",
        "COMMIT",
    ] {
        results(&mut session, text);
    }
    session
}

#[test]
fn insert_all_without_conditions_puts_every_row_into_every_table() {
    let store = MemoryStore::default();
    let mut session = sales(&store);
    assert_eq!(
        tag(
            &mut session,
            "INSERT ALL INTO small_sales VALUES (id, amount) INTO sale_ids (id) VALUES (id) \
             SELECT id, amount FROM sales"
        ),
        "INSERT 0 6"
    );
    assert_eq!(
        lines(
            &mut session,
            "SELECT id, amount FROM small_sales ORDER BY id"
        ),
        vec!["1|50", "2|150", "3|500"]
    );
    assert_eq!(
        lines(&mut session, "SELECT id, note FROM sale_ids ORDER BY id"),
        vec!["1|seen", "2|seen", "3|seen"]
    );
}

#[test]
fn insert_all_with_when_puts_each_row_into_every_table_whose_condition_holds() {
    let store = MemoryStore::default();
    let mut session = sales(&store);
    assert_eq!(
        tag(
            &mut session,
            "INSERT ALL \
             WHEN amount < 100 THEN INTO small_sales \
             WHEN amount >= 100 THEN INTO large_sales \
             WHEN amount > 0 THEN INTO sale_ids (id, note) VALUES (id, DEFAULT) \
             SELECT id, amount FROM sales"
        ),
        "INSERT 0 6"
    );
    assert_eq!(
        lines(&mut session, "SELECT id FROM small_sales ORDER BY id"),
        vec!["1"]
    );
    assert_eq!(
        lines(&mut session, "SELECT id FROM large_sales ORDER BY id"),
        vec!["2", "3"]
    );
    assert_eq!(
        lines(
            &mut session,
            "SELECT COUNT(*) FROM sale_ids WHERE note = 'seen'"
        ),
        vec!["3"]
    );
}

#[test]
fn insert_first_stops_at_the_first_condition_and_else_takes_the_rest() {
    let store = MemoryStore::default();
    let mut session = sales(&store);
    assert_eq!(
        tag(
            &mut session,
            "INSERT FIRST \
             WHEN amount > 400 THEN INTO large_sales \
             WHEN amount > 100 THEN INTO small_sales (id, amount) VALUES (id, amount * 2) \
             ELSE INTO sale_ids (id) VALUES (id) \
             SELECT id, amount FROM sales"
        ),
        "INSERT 0 3"
    );
    assert_eq!(
        lines(&mut session, "SELECT id, amount FROM large_sales"),
        vec!["3|500"]
    );
    assert_eq!(
        lines(&mut session, "SELECT id, amount FROM small_sales"),
        vec!["2|300"]
    );
    assert_eq!(lines(&mut session, "SELECT id FROM sale_ids"), vec!["1"]);
}

#[test]
fn a_failing_target_undoes_the_whole_statement() {
    let store = MemoryStore::default();
    let mut session = sales(&store);
    results(&mut session, "CREATE TABLE keyed (id NUMBER PRIMARY KEY)");
    results(&mut session, "INSERT INTO keyed VALUES (2)");
    results(&mut session, "COMMIT");
    error(
        &mut session,
        "INSERT ALL INTO sale_ids (id) VALUES (id) INTO keyed (id) VALUES (id) SELECT id FROM sales",
    );
    assert_eq!(
        lines(&mut session, "SELECT COUNT(*) FROM sale_ids"),
        vec!["0"]
    );
    assert_eq!(lines(&mut session, "SELECT COUNT(*) FROM keyed"), vec!["1"]);
}

#[test]
fn insert_first_needs_a_when_clause() {
    let store = MemoryStore::default();
    let mut session = sales(&store);
    assert_eq!(
        error(
            &mut session,
            "INSERT FIRST INTO small_sales SELECT id, amount FROM sales"
        )
        .state()
        .code(),
        "42601"
    );
}

#[test]
fn insert_all_reads_a_hierarchical_source() {
    let store = MemoryStore::default();
    let mut session = sales(&store);
    assert_eq!(
        tag(
            &mut session,
            "INSERT ALL INTO sale_ids (id) VALUES (n) SELECT LEVEL AS n FROM dual CONNECT BY LEVEL <= 5"
        ),
        "INSERT 0 5"
    );
    assert_eq!(
        lines(&mut session, "SELECT SUM(id) FROM sale_ids"),
        vec!["15"]
    );
}

#[test]
fn an_into_clause_that_gets_no_rows_is_still_checked() {
    let store = MemoryStore::default();
    let mut session = sales(&store);
    assert_eq!(
        error(
            &mut session,
            "INSERT ALL INTO no_such_table VALUES (id) SELECT id FROM sales WHERE 1 = 0"
        )
        .vendor_code(),
        Some(942)
    );
    error(
        &mut session,
        "INSERT ALL INTO small_sales VALUES (id) SELECT id FROM sales WHERE 1 = 0",
    );
    error(
        &mut session,
        "INSERT FIRST WHEN amount < 100 THEN INTO small_sales VALUES (id, amount) \
         WHEN amount > 1000 THEN INTO no_such_table VALUES (id) SELECT id, amount FROM sales",
    );
    assert_eq!(
        lines(&mut session, "SELECT COUNT(*) FROM small_sales"),
        vec!["0"]
    );
    assert_eq!(
        tag(
            &mut session,
            "INSERT ALL INTO small_sales VALUES (id, amount) INTO sale_ids (id) VALUES (id) \
             SELECT id, amount FROM sales WHERE 1 = 0"
        ),
        "INSERT 0 0"
    );
}
