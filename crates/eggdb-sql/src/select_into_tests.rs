use crate::memory_store::MemoryStore;
use crate::{Dialect, QueryResult, Session, SqlError, Value};

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

fn rows(session: &mut Session<'_>, text: &str) -> Vec<Vec<Value>> {
    results(session, text)
        .pop()
        .map(|result| result.rows)
        .unwrap_or_default()
}

fn error(session: &mut Session<'_>, text: &str) -> SqlError {
    session.execute(text, &mut |_| {}).expect_err(text)
}

fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

fn sqlserver_products(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::with_dialect(store, Dialect::SqlServer);
    results(
        &mut session,
        "CREATE TABLE products (id int PRIMARY KEY, name nvarchar(20), price decimal(10, 2))",
    );
    results(
        &mut session,
        "INSERT INTO products VALUES (1, N'egg', 1.50), (2, N'milk', 2.25), (3, N'bread', 3.00)",
    );
    session
}

#[test]
fn sqlserver_select_into_makes_a_table_from_the_rows_and_columns_of_the_query() {
    let store = MemoryStore::default();
    let mut session = sqlserver_products(&store);
    assert_eq!(
        tag(
            &mut session,
            "SELECT id, name AS label INTO cheap FROM products WHERE price < 3"
        ),
        "SELECT 2"
    );
    assert_eq!(
        rows(&mut session, "SELECT id, label FROM cheap ORDER BY id"),
        vec![
            vec![Value::Integer(1), text("egg")],
            vec![Value::Integer(2), text("milk")],
        ]
    );
    rows(&mut session, "INSERT INTO cheap VALUES (9, N'salt')");
    assert_eq!(rows(&mut session, "SELECT id FROM cheap").len(), 3);
    assert_eq!(
        tag(
            &mut session,
            "WITH dear AS (SELECT id FROM products WHERE price >= 2) SELECT TOP 1 id INTO #dear FROM dear ORDER BY id DESC"
        ),
        "SELECT 1"
    );
    assert_eq!(
        rows(&mut session, "SELECT id FROM #dear"),
        vec![vec![Value::Integer(3)]]
    );
    assert_eq!(
        tag(
            &mut session,
            "SELECT * INTO empty FROM products WHERE 1 = 0"
        ),
        "SELECT 0"
    );
    assert_eq!(
        rows(&mut session, "SELECT name FROM empty"),
        Vec::<Vec<Value>>::new()
    );
}

#[test]
fn sqlserver_select_into_refuses_what_sql_server_refuses() {
    let store = MemoryStore::default();
    let mut session = sqlserver_products(&store);
    rows(&mut session, "SELECT id INTO cheap FROM products");
    for (statement, code) in [
        ("SELECT id INTO cheap FROM products", 2714),
        ("SELECT 1 INTO unnamed", 1038),
        (
            "SELECT id INTO first_copy FROM products UNION SELECT id INTO second_copy FROM products",
            196,
        ),
    ] {
        assert_eq!(
            error(&mut session, statement).vendor_code(),
            Some(code),
            "{statement}"
        );
    }
    assert!(
        session
            .execute(
                "SELECT id FROM (SELECT id INTO inner_copy FROM products) AS d",
                &mut |_| {}
            )
            .is_err()
    );
    assert!(
        session
            .execute("SELECT id FROM first_copy", &mut |_| {})
            .is_err()
    );
}

#[test]
fn postgresql_select_into_creates_a_table_and_undoes_it_with_its_transaction() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    results(
        &mut session,
        "CREATE TABLE films (code int PRIMARY KEY, title text, kind text)",
    );
    results(
        &mut session,
        "INSERT INTO films VALUES (1, 'A', 'Comedy'), (2, 'B', 'Drama'), (3, 'C', 'Comedy')",
    );
    assert_eq!(
        tag(
            &mut session,
            "SELECT * INTO films_recent FROM films WHERE kind = 'Comedy'"
        ),
        "SELECT 2"
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT code, title FROM films_recent ORDER BY code"
        ),
        vec![
            vec![Value::Integer(1), text("A")],
            vec![Value::Integer(3), text("C")],
        ]
    );
    assert_eq!(
        tag(
            &mut session,
            "SELECT code INTO TEMPORARY TABLE codes FROM films"
        ),
        "SELECT 3"
    );
    assert_eq!(rows(&mut session, "SELECT code FROM codes").len(), 3);
    results(&mut session, "BEGIN");
    results(&mut session, "SELECT title INTO titles FROM films");
    results(&mut session, "ROLLBACK");
    assert_eq!(
        error(&mut session, "SELECT * FROM titles").state().code(),
        "42P01"
    );
    assert_eq!(
        error(&mut session, "SELECT * INTO films_recent FROM films")
            .state()
            .code(),
        "42P07"
    );
}

#[test]
fn sqlserver_select_into_in_a_later_query_of_a_union_is_msg_196() {
    let store = MemoryStore::default();
    let mut session = sqlserver_products(&store);
    for statement in [
        "SELECT 1 AS a UNION SELECT 2 INTO later_only",
        "SELECT id FROM products UNION ALL SELECT id INTO later_only FROM products",
        "SELECT id FROM products EXCEPT SELECT id INTO later_only FROM products WHERE id = 1",
        "SELECT id FROM products UNION SELECT id FROM products INTERSECT SELECT id INTO later_only FROM products",
    ] {
        let refused = error(&mut session, statement);
        assert_eq!(refused.vendor_code(), Some(196), "{statement}");
        assert!(
            refused
                .message()
                .starts_with("SELECT INTO must be the first query"),
            "{statement}: {}",
            refused.message()
        );
    }
    assert!(
        session
            .execute("SELECT * FROM later_only", &mut |_| {})
            .is_err()
    );
    assert_eq!(
        tag(
            &mut session,
            "SELECT id INTO first_only FROM products UNION SELECT 9"
        ),
        "SELECT 4"
    );
}

#[test]
fn postgresql_select_into_in_a_later_query_of_a_union_is_42601() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    results(&mut session, "CREATE TABLE t (v int)");
    for statement in [
        "SELECT 1 AS a UNION SELECT 2 INTO later_only",
        "SELECT v FROM t UNION SELECT v INTO later_only FROM t",
        "SELECT v FROM t INTERSECT SELECT v INTO TEMP later_only FROM t",
        "SELECT 1 INTO first_copy UNION SELECT 2 INTO second_copy",
    ] {
        let refused = error(&mut session, statement);
        assert_eq!(refused.state().code(), "42601", "{statement}");
        assert_eq!(
            refused.message(),
            "INTO is only allowed on first SELECT of UNION/INTERSECT/EXCEPT",
            "{statement}"
        );
    }
    for table in ["later_only", "first_copy", "second_copy"] {
        assert_eq!(
            error(&mut session, &format!("SELECT * FROM {table}"))
                .state()
                .code(),
            "42P01",
            "{table}"
        );
    }
}

#[test]
fn into_inside_a_subquery_is_a_syntax_error_in_each_dialect() {
    let store = MemoryStore::default();
    let mut postgresql = Session::new(&store);
    let refused = error(&mut postgresql, "SELECT * FROM (SELECT 1 AS z INTO s4) q");
    assert_eq!(refused.state().code(), "42601");
    assert_eq!(refused.message(), "SELECT ... INTO is not allowed here");
    let sqlserver_store = MemoryStore::default();
    let mut sqlserver = sqlserver_products(&sqlserver_store);
    let refused = error(
        &mut sqlserver,
        "SELECT id FROM (SELECT id INTO inner_copy FROM products) AS d",
    );
    assert_eq!(refused.vendor_code(), Some(156));
    assert_eq!(
        refused.message(),
        "Incorrect syntax near the keyword 'INTO'."
    );
    assert!(
        sqlserver
            .execute("SELECT id FROM inner_copy", &mut |_| {})
            .is_err()
    );
}
