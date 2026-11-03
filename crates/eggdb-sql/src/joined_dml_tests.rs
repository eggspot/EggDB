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

fn refused(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => panic!("{sql} succeeded: {outputs:?}"),
        Err(error) => error.message().to_owned(),
    }
}

fn with_employees(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::new(store);
    last(
        &mut session,
        "CREATE TABLE employees (id INT PRIMARY KEY, name TEXT, sales_count INT); \
         CREATE TABLE accounts (id INT PRIMARY KEY, name TEXT, sales_person INT); \
         INSERT INTO employees VALUES (1, 'Ann', 0), (2, 'Bob', 5); \
         INSERT INTO accounts VALUES (10, 'Acme Corporation', 2), (11, 'Initech', 1)",
    );
    session
}

#[test]
fn update_from_changes_the_target_rows_the_other_tables_join() {
    let store = MemoryStore::default();
    let mut session = with_employees(&store);
    assert_eq!(
        last(
            &mut session,
            "UPDATE employees SET sales_count = sales_count + 1 FROM accounts \
             WHERE accounts.name = 'Acme Corporation' AND employees.id = accounts.sales_person"
        ),
        "UPDATE 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT id, sales_count FROM employees ORDER BY id"
        ),
        "id|sales_count\n1|0\n2|6\nSELECT 2"
    );
}

#[test]
fn update_from_updates_a_row_joined_to_several_rows_once_with_one_of_them() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE totals (id INT PRIMARY KEY, total INT); \
         CREATE TABLE sales (id INT PRIMARY KEY, total_id INT, amount INT); \
         INSERT INTO totals VALUES (1, 0), (2, 0); \
         INSERT INTO sales VALUES (1, 1, 5), (2, 1, 7), (3, 2, 4)",
    );
    assert_eq!(
        last(
            &mut session,
            "UPDATE totals SET total = total + s.amount FROM sales AS s WHERE s.total_id = totals.id"
        ),
        "UPDATE 2"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT count(*) FROM totals WHERE (id = 1 AND total IN (5, 7)) OR (id = 2 AND total = 4)"
        ),
        "count\n2\nSELECT 1"
    );
}

#[test]
fn update_from_returns_the_new_target_row_and_the_joined_columns() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE totals (id INT PRIMARY KEY, total INT); \
         CREATE TABLE sales (id INT PRIMARY KEY, total_id INT, amount INT); \
         INSERT INTO totals VALUES (1, 0), (2, 0); \
         INSERT INTO sales VALUES (1, 1, 5), (3, 2, 4)",
    );
    assert_eq!(
        last(
            &mut session,
            "UPDATE totals AS t SET total = s.amount * 10 FROM sales AS s \
             WHERE s.id = 3 AND s.total_id = t.id RETURNING t.id, total, s.amount"
        ),
        "id|total|amount\n2|40|4\nUPDATE 1"
    );
}

#[test]
fn update_from_keeps_indexes_and_updates_every_copy_of_a_row_without_a_key() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE tags (name TEXT, hits INT); \
         CREATE INDEX tags_hits ON tags (hits); \
         INSERT INTO tags VALUES ('a', 1), ('a', 1), ('b', 1); \
         CREATE TABLE bumps (name TEXT PRIMARY KEY, by_count INT); \
         INSERT INTO bumps VALUES ('a', 10)",
    );
    assert_eq!(
        last(
            &mut session,
            "UPDATE tags SET hits = tags.hits + b.by_count FROM bumps AS b WHERE b.name = tags.name"
        ),
        "UPDATE 2"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT name, hits FROM tags WHERE hits = 11 ORDER BY name"
        ),
        "name|hits\na|11\na|11\nSELECT 2"
    );
    assert_eq!(
        last(&mut session, "SELECT name FROM tags WHERE hits = 1"),
        "name\nb\nSELECT 1"
    );
}

#[test]
fn update_from_refuses_the_target_listed_again_under_its_own_name() {
    let store = MemoryStore::default();
    let mut session = with_employees(&store);
    assert!(
        refused(
            &mut session,
            "UPDATE employees SET sales_count = 0 FROM employees"
        )
        .contains("specified more than once")
    );
    assert_eq!(
        last(
            &mut session,
            "UPDATE employees SET sales_count = other.sales_count FROM employees AS other \
             WHERE other.id = 2 AND employees.id = 1"
        ),
        "UPDATE 1"
    );
    assert_eq!(
        last(&mut session, "SELECT sales_count FROM employees WHERE id = 1"),
        "sales_count\n5\nSELECT 1"
    );
}

#[test]
fn delete_using_removes_the_target_rows_the_other_tables_join() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE producers (id INT PRIMARY KEY, name TEXT); \
         CREATE TABLE films (code INT PRIMARY KEY, title TEXT, producer_id INT); \
         INSERT INTO producers VALUES (1, 'foo'), (2, 'bar'); \
         INSERT INTO films VALUES (1, 'A', 1), (2, 'B', 2), (3, 'C', 1)",
    );
    assert_eq!(
        last(
            &mut session,
            "DELETE FROM films USING producers \
             WHERE producer_id = producers.id AND producers.name = 'foo'"
        ),
        "DELETE 2"
    );
    assert_eq!(
        last(
            &mut session,
            "DELETE FROM films AS f USING producers AS p \
             WHERE f.producer_id = p.id AND p.id IN (SELECT id FROM producers WHERE name = 'bar') \
             RETURNING f.title, p.name"
        ),
        "title|name\nB|bar\nDELETE 1"
    );
    assert_eq!(
        last(&mut session, "SELECT count(*) FROM films"),
        "count\n0\nSELECT 1"
    );
}

#[test]
fn sqlite_update_from_reads_a_grouped_subquery() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    last(
        &mut session,
        "CREATE TABLE inventory (item_id INTEGER PRIMARY KEY, quantity INTEGER); \
         CREATE TABLE sales (item_id INTEGER, quantity INTEGER); \
         INSERT INTO inventory VALUES (1, 10), (2, 20), (3, 30); \
         INSERT INTO sales VALUES (1, 2), (1, 3), (2, 5)",
    );
    assert_eq!(
        last(
            &mut session,
            "UPDATE inventory SET quantity = quantity - daily.amt \
             FROM (SELECT sum(quantity) AS amt, item_id FROM sales GROUP BY item_id) AS daily \
             WHERE inventory.item_id = daily.item_id"
        ),
        "UPDATE 2"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT item_id, quantity FROM inventory ORDER BY item_id"
        ),
        "item_id|quantity\n1|5\n2|15\n3|30\nSELECT 3"
    );
}

fn mysql_items(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::with_dialect(store, Dialect::MySql);
    last(
        &mut session,
        "CREATE TABLE items (id INT PRIMARY KEY, price INT); \
         CREATE TABLE monthly (id INT PRIMARY KEY, price INT); \
         CREATE TABLE notes (item_id INT PRIMARY KEY, note VARCHAR(10)); \
         INSERT INTO items VALUES (1, 10), (2, 20), (3, 30); \
         INSERT INTO monthly VALUES (1, 11), (3, 33); \
         INSERT INTO notes VALUES (1, 'old'), (2, 'old')",
    );
    session
}

#[test]
fn mysql_multiple_table_update_writes_the_tables_its_assignments_name() {
    let store = MemoryStore::default();
    let mut session = mysql_items(&store);
    assert_eq!(
        last(
            &mut session,
            "UPDATE items, monthly SET items.price = monthly.price WHERE items.id = monthly.id"
        ),
        "UPDATE 2"
    );
    assert_eq!(
        last(&mut session, "SELECT id, price FROM items ORDER BY id"),
        "id|price\n1|11\n2|20\n3|33\nSELECT 3"
    );
    assert_eq!(
        last(
            &mut session,
            "UPDATE items JOIN monthly ON items.id = monthly.id \
             SET items.price = monthly.price * 2, monthly.price = 0"
        ),
        "UPDATE 4"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT items.id, items.price, monthly.price AS monthly_price FROM items \
             JOIN monthly ON items.id = monthly.id ORDER BY items.id"
        ),
        "id|price|monthly_price\n1|22|0\n3|66|0\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "UPDATE items LEFT JOIN notes ON items.id = notes.item_id \
             SET price = 0 WHERE notes.item_id IS NULL"
        ),
        "UPDATE 1"
    );
    assert_eq!(
        last(
            &mut session,
            "UPDATE items JOIN notes ON items.id = notes.item_id SET note = 'new' WHERE items.id = 2"
        ),
        "UPDATE 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT item_id, note FROM notes ORDER BY item_id"
        ),
        "item_id|note\n1|old\n2|new\nSELECT 2"
    );
    assert_eq!(
        last(&mut session, "SELECT price FROM items WHERE id = 3"),
        "price\n0\nSELECT 1"
    );
}

#[test]
fn mysql_multiple_table_update_refuses_unclear_and_unknown_columns() {
    let store = MemoryStore::default();
    let mut session = mysql_items(&store);
    assert!(
        refused(
            &mut session,
            "UPDATE items JOIN monthly ON items.id = monthly.id SET price = 1"
        )
        .contains("ambiguous")
    );
    assert!(
        refused(
            &mut session,
            "UPDATE items JOIN monthly ON items.id = monthly.id SET other.price = 1"
        )
        .contains("Unknown column 'other.price'")
    );
    assert_eq!(
        last(&mut session, "UPDATE items SET items.price = 5 WHERE id = 1"),
        "UPDATE 1"
    );
}

#[test]
fn mysql_multiple_table_delete_removes_rows_from_each_listed_table() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    last(
        &mut session,
        "CREATE TABLE t1 (id INT PRIMARY KEY); \
         CREATE TABLE t2 (id INT PRIMARY KEY); \
         CREATE TABLE t3 (id INT PRIMARY KEY); \
         INSERT INTO t1 VALUES (1), (2), (3); \
         INSERT INTO t2 VALUES (1), (2), (4); \
         INSERT INTO t3 VALUES (2), (4)",
    );
    assert_eq!(
        last(
            &mut session,
            "DELETE t1, t2 FROM t1, t2, t3 WHERE t1.id = t2.id AND t2.id = t3.id"
        ),
        "DELETE 2"
    );
    assert_eq!(
        last(
            &mut session,
            "DELETE FROM t1 USING t1 LEFT JOIN t2 ON t1.id = t2.id WHERE t2.id IS NULL"
        ),
        "DELETE 1"
    );
    assert_eq!(
        last(&mut session, "SELECT id FROM t1"),
        "id\n1\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT id FROM t2 ORDER BY id"),
        "id\n1\n4\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "DELETE a.* FROM t1 AS a JOIN t2 AS b ON a.id = b.id"
        ),
        "DELETE 1"
    );
    assert!(
        refused(&mut session, "DELETE t9 FROM t1 JOIN t2 ON t1.id = t2.id")
            .contains("Unknown table 't9' in MULTI DELETE")
    );
}

fn sqlserver_sales(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::with_dialect(store, Dialect::SqlServer);
    last(
        &mut session,
        "CREATE TABLE sales_person (business_entity_id INT PRIMARY KEY, sales_ytd INT); \
         CREATE TABLE sales_order_header (sales_order_id INT PRIMARY KEY, sales_person_id INT, sub_total INT); \
         INSERT INTO sales_person VALUES (1, 100), (2, 200); \
         INSERT INTO sales_order_header VALUES (10, 1, 5), (11, 1, 7), (12, 2, 9)",
    );
    session
}

#[test]
fn sqlserver_update_from_writes_the_target_named_in_its_from_clause() {
    let store = MemoryStore::default();
    let mut session = sqlserver_sales(&store);
    assert_eq!(
        last(
            &mut session,
            "UPDATE sales_person SET sales_ytd = sales_ytd + so.sub_total \
             FROM sales_person AS sp JOIN sales_order_header AS so \
             ON sp.business_entity_id = so.sales_person_id"
        ),
        "UPDATE 2"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT COUNT(*) AS n FROM sales_person \
             WHERE (business_entity_id = 1 AND sales_ytd IN (105, 107)) \
             OR (business_entity_id = 2 AND sales_ytd = 209)"
        ),
        "n\n2\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "UPDATE sp SET sp.sales_ytd = 0 OUTPUT inserted.business_entity_id, inserted.sales_ytd, so.sub_total \
             FROM sales_person AS sp JOIN sales_order_header AS so \
             ON sp.business_entity_id = so.sales_person_id WHERE so.sales_order_id = 12"
        ),
        "business_entity_id|sales_ytd|sub_total\n2|0|9\nUPDATE 1"
    );
    assert!(
        refused(&mut session, "UPDATE sales_person SET x.sales_ytd = 1")
            .contains("could not be bound")
    );
}

#[test]
fn sqlserver_delete_from_removes_the_target_rows_its_second_from_joins() {
    let store = MemoryStore::default();
    let mut session = sqlserver_sales(&store);
    assert_eq!(
        last(
            &mut session,
            "DELETE FROM sales_order_header FROM sales_order_header AS soh \
             JOIN sales_person AS sp ON soh.sales_person_id = sp.business_entity_id \
             WHERE sp.sales_ytd < 150"
        ),
        "DELETE 2"
    );
    assert_eq!(
        last(
            &mut session,
            "DELETE soh OUTPUT deleted.sales_order_id FROM sales_order_header AS soh \
             JOIN sales_person AS sp ON soh.sales_person_id = sp.business_entity_id \
             WHERE sp.business_entity_id = 2"
        ),
        "sales_order_id\n12\nDELETE 1"
    );
    assert_eq!(
        last(&mut session, "SELECT COUNT(*) AS n FROM sales_order_header"),
        "n\n0\nSELECT 1"
    );
}
