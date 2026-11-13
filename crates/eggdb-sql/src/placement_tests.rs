use crate::memory_store::MemoryStore;
use crate::placement::{filter_for_region, prewarm, warm_tables};
use crate::tests::render;
use crate::{Session, SqlError};

fn run(session: &mut Session<'_>, sql: &str) -> Result<String, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs.pop().unwrap_or_default())
}

fn ok(session: &mut Session<'_>, sql: &str) -> String {
    run(session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"))
}

fn customers(session: &mut Session<'_>) {
    ok(session, "CREATE EXTENSION eggdb_placement");
    ok(session, "CREATE TABLE customers (id integer PRIMARY KEY, home text, name text)");
    ok(session, "CREATE TABLE audit (id integer PRIMARY KEY, note text)");
    ok(session, "CREATE TABLE prices (sku text PRIMARY KEY, cents integer)");
    ok(session, "SELECT eggdb_placement.place('public', 'customers', 'warm', true, NULL, 'home')");
    ok(session, "SELECT eggdb_placement.place('public', 'audit', 'cold', false, 'eu,us', NULL)");
    ok(session, "SELECT eggdb_placement.place('public', 'prices', 'hot', true, NULL, NULL)");
    ok(session, "INSERT INTO eggdb_placement.residency VALUES ('eu', 'eu'), ('ch', 'ch,eu')");
}

#[test]
fn a_server_refuses_rows_its_region_may_not_hold() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    customers(&mut session);
    ok(&mut session, "INSERT INTO customers VALUES (1, 'us', 'before any region')");
    ok(&mut session, "UPDATE eggdb_placement.settings SET value = 'eu' WHERE name = 'region'");
    ok(&mut session, "INSERT INTO customers VALUES (2, 'eu', 'Ada'), (3, 'ch', 'Heidi')");
    let refused = run(&mut session, "INSERT INTO customers VALUES (4, 'us', 'Bob')").unwrap_err();
    assert!(refused.to_string().contains("may not be stored in region eu"), "{refused}");
    ok(&mut session, "UPDATE eggdb_placement.settings SET value = 'apac' WHERE name = 'region'");
    let audit = run(&mut session, "INSERT INTO audit VALUES (1, 'kept out of apac')").unwrap_err();
    assert!(audit.to_string().contains("placement"), "{audit}");
}

#[test]
fn a_change_from_another_server_is_dropped_and_logged_not_failed() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    customers(&mut session);
    ok(&mut session, "UPDATE eggdb_placement.settings SET value = 'eu' WHERE name = 'region'");
    ok(&mut session, "BEGIN");
    ok(&mut session, "SELECT set_config('eggdb_mp.applying', 'on', true)");
    ok(&mut session, "INSERT INTO customers VALUES (9, 'us', 'from abroad')");
    ok(&mut session, "COMMIT");
    assert_eq!(ok(&mut session, "SELECT count(*) FROM customers"), "count\n0\n");
    assert_eq!(ok(&mut session, "SELECT row_region, server_region FROM eggdb_placement.transfers"), "row_region|server_region\nus|eu\n");
}

#[test]
fn a_copy_for_an_edge_in_a_region_loses_what_it_may_not_hold() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    customers(&mut session);
    ok(&mut session, "INSERT INTO customers VALUES (1, 'eu', 'Ada'), (2, 'ch', 'Heidi'), (3, 'us', 'Bob')");
    ok(&mut session, "INSERT INTO audit VALUES (1, 'internal')");
    drop(session);
    let report = filter_for_region(&store, Some("eu"), true).unwrap();
    assert_eq!(report.dropped_tables, vec!["public.audit".to_owned()]);
    assert_eq!(report.deleted_rows, 1);
    let mut session = Session::new(&store);
    assert_eq!(ok(&mut session, "SELECT name FROM customers ORDER BY id"), "name\nAda\nHeidi\n");
    assert!(run(&mut session, "SELECT * FROM audit").is_err());
    assert_eq!(ok(&mut session, "SELECT value FROM eggdb_placement.settings WHERE name = 'region'"), "value\neu\n");
}

#[test]
fn hot_and_warm_tables_are_read_through_to_keep_them_near() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    customers(&mut session);
    drop(session);
    assert_eq!(
        warm_tables(&store).unwrap(),
        vec![("public".to_owned(), "prices".to_owned()), ("public".to_owned(), "customers".to_owned())]
    );
    assert_eq!(prewarm(&store).unwrap(), 2);
    let empty = MemoryStore::default();
    assert_eq!(prewarm(&empty).unwrap(), 0);
}
