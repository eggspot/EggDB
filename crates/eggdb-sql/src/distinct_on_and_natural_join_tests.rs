use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Dialect, Session, SqlError};

fn run(session: &mut Session<'_>, sql: &str) -> Result<Vec<String>, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs)
}

fn last(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => outputs.last().cloned().unwrap_or_default(),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn state(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => panic!("{sql} succeeded: {outputs:?}"),
        Err(error) => error.state().code().to_owned(),
    }
}

fn with_orders(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::new(store);
    last(
        &mut session,
        "CREATE TABLE orders (id INT, customer TEXT, amount INT); \
         INSERT INTO orders VALUES (1, 'ann', 30), (2, 'bob', 10), (3, 'ann', 50), (4, 'bob', 40), (5, 'cy', NULL), (6, NULL, 5), (7, NULL, 6)",
    );
    session
}

#[test]
fn distinct_on_keeps_the_first_row_of_each_key_in_order_by_order() {
    let store = MemoryStore::default();
    let mut session = with_orders(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT DISTINCT ON (customer) customer, id, amount FROM orders ORDER BY customer, amount DESC NULLS LAST"
        ),
        "customer|id|amount\nann|3|50\nbob|4|40\ncy|5|NULL\nNULL|7|6\nSELECT 4"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT DISTINCT ON (o.customer) o.id FROM orders o ORDER BY o.customer, o.id LIMIT 2 OFFSET 1"
        ),
        "id\n2\n5\nSELECT 2"
    );
}

#[test]
fn distinct_on_works_with_grouping_and_inside_derived_tables() {
    let store = MemoryStore::default();
    let mut session = with_orders(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT DISTINCT ON (n) count(*) AS n, min(customer) AS earliest FROM orders \
             WHERE customer IS NOT NULL GROUP BY customer ORDER BY n, earliest"
        ),
        "n|earliest\n1|cy\n2|ann\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT customer, amount FROM (SELECT DISTINCT ON (customer) customer, amount FROM orders \
             WHERE customer IS NOT NULL ORDER BY customer, amount) AS smallest ORDER BY customer"
        ),
        "customer|amount\nann|30\nbob|10\ncy|NULL\nSELECT 3"
    );
}

#[test]
fn distinct_on_is_checked_against_order_by_and_its_dialect() {
    let store = MemoryStore::default();
    let mut session = with_orders(&store);
    assert_eq!(
        state(
            &mut session,
            "SELECT DISTINCT ON (customer) id FROM orders ORDER BY amount, customer"
        ),
        "42P10"
    );
    assert_eq!(
        state(&mut session, "SELECT DISTINCT ON (9) id FROM orders"),
        "42P10"
    );
    last(
        &mut session,
        "CREATE VIEW latest_orders AS SELECT DISTINCT ON (customer) customer, id FROM orders ORDER BY customer, id DESC",
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT * FROM latest_orders WHERE customer = 'bob'"
        ),
        "customer|id\nbob|4\nSELECT 1"
    );
    assert_eq!(
        state(
            &mut session,
            "UPDATE latest_orders SET id = 9 WHERE customer = 'bob'"
        ),
        "55000"
    );
    let mysql_store = MemoryStore::default();
    let mut mysql = Session::with_dialect(&mysql_store, Dialect::MySql);
    last(&mut mysql, "CREATE TABLE t (a INT)");
    assert!(state(&mut mysql, "SELECT DISTINCT ON (a) a FROM t").starts_with("42"));
}

#[test]
fn natural_joins_compare_shared_names_and_report_ambiguous_ones() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE departments (dept_id INT, dept TEXT); \
         CREATE TABLE staff (staff_id INT, dept_id INT, name TEXT); \
         CREATE TABLE badges (staff_id INT, dept_id INT, badge TEXT); \
         INSERT INTO departments VALUES (1, 'ops'), (2, 'dev'); \
         INSERT INTO staff VALUES (10, 1, 'ann'), (11, 2, 'bob'), (12, 2, 'cy'); \
         INSERT INTO badges VALUES (10, 1, 'red'), (11, 1, 'blue'), (12, 2, 'green')",
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT dept_id, dept, name FROM departments NATURAL JOIN staff ORDER BY name"
        ),
        "dept_id|dept|name\n1|ops|ann\n2|dev|bob\n2|dev|cy\nSELECT 3"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT * FROM staff NATURAL JOIN badges ORDER BY staff_id"
        ),
        "staff_id|dept_id|name|badge\n10|1|ann|red\n12|2|cy|green\nSELECT 2"
    );
    assert_eq!(
        state(
            &mut session,
            "SELECT * FROM staff s JOIN badges b ON s.staff_id = b.staff_id NATURAL JOIN departments"
        ),
        "42702"
    );
    let sqlserver_store = MemoryStore::default();
    let mut sqlserver = Session::with_dialect(&sqlserver_store, Dialect::SqlServer);
    last(
        &mut sqlserver,
        "CREATE TABLE a (id INT); CREATE TABLE b (id INT)",
    );
    assert!(state(&mut sqlserver, "SELECT * FROM a NATURAL JOIN b").starts_with("42"));
}

#[test]
fn a_right_side_with_two_columns_of_a_joined_name_is_ambiguous() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE e (k INT, v TEXT); INSERT INTO e VALUES (100, 'x')",
    );
    for sql in [
        "SELECT * FROM e NATURAL JOIN (SELECT 100 AS k, 2 AS k) s",
        "SELECT * FROM e JOIN (SELECT 100 AS k, 2 AS k) s USING (k)",
    ] {
        let error = run(&mut session, sql).expect_err(sql);
        assert_eq!(error.state().code(), "42702", "{sql}");
        assert_eq!(
            error.message(),
            "common column name \"k\" appears more than once in right table",
            "{sql}"
        );
    }
}

#[test]
fn distinct_on_in_a_grouped_query_takes_an_expression_of_the_select_list() {
    let store = MemoryStore::default();
    let mut session = with_orders(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT DISTINCT ON (count(*)) count(*), customer FROM orders \
             WHERE customer IS NOT NULL GROUP BY customer ORDER BY count(*), customer"
        ),
        "count|customer\n1|cy\n2|ann\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT DISTINCT ON (amount / 10) amount / 10, count(*) FROM orders \
             WHERE amount IS NOT NULL GROUP BY amount / 10 ORDER BY amount / 10"
        ),
        "?column?|count\n0|2\n1|1\n3|1\n4|1\n5|1\nSELECT 5"
    );
    let error = run(
        &mut session,
        "SELECT DISTINCT ON (max(amount)) customer FROM orders GROUP BY customer ORDER BY max(amount)",
    )
    .expect_err("an aggregate outside the select list");
    assert_eq!(error.state().code(), "0A000");
    assert_eq!(
        error.message(),
        "DISTINCT ON an expression outside the select list of a grouped query"
    );
}
