use crate::memory_store::MemoryStore;
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

fn installed(session: &mut Session<'_>) {
    ok(session, "CREATE EXTENSION eggdb_multi_primary");
    ok(session, "UPDATE eggdb_mp.settings SET value = 'a' WHERE name = 'node'");
}

fn apply(session: &mut Session<'_>, stamp: &str, op: &str, before: &str, after: &str) -> String {
    ok(
        session,
        &format!("SELECT eggdb_mp.apply('b', '{stamp}', 'public', 'stock', '{op}', {before}, {after})"),
    )
}

#[test]
fn local_writes_are_logged_with_stamps_and_remote_ones_are_not() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    installed(&mut session);
    ok(&mut session, "CREATE TABLE stock (sku text PRIMARY KEY, count integer, note text)");
    assert_eq!(ok(&mut session, "SELECT eggdb_mp.enable('public', 'stock', 'lww', NULL, NULL)"), "enable\nsku\n");
    ok(&mut session, "INSERT INTO stock VALUES ('egg', 10, 'local')");
    ok(&mut session, "UPDATE stock SET count = 11 WHERE sku = 'egg'");
    let logged = ok(&mut session, "SELECT origin, op FROM eggdb_mp.log ORDER BY id");
    assert_eq!(logged, "origin|op\na|c\na|u\n");
    apply(&mut session, "999999999999999.000000.b", "c", "NULL", "'{\"sku\": \"milk\", \"count\": 3, \"note\": \"remote\"}'::jsonb");
    assert_eq!(ok(&mut session, "SELECT count(*) FROM eggdb_mp.log"), "count\n2\n");
    assert_eq!(ok(&mut session, "SELECT note FROM stock WHERE sku = 'milk'"), "note\nremote\n");
}

#[test]
fn last_writer_wins_keeps_the_newer_stamp() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    installed(&mut session);
    ok(&mut session, "CREATE TABLE stock (sku text PRIMARY KEY, count integer, note text)");
    ok(&mut session, "SELECT eggdb_mp.enable('public', 'stock', 'lww', NULL, NULL)");
    ok(&mut session, "INSERT INTO stock VALUES ('egg', 10, 'local')");
    let older = apply(&mut session, "000000000000001.000000.b", "u", "NULL", "'{\"sku\": \"egg\", \"count\": 1, \"note\": \"old\"}'::jsonb");
    assert!(older.contains("kept the newer row"), "{older}");
    assert_eq!(ok(&mut session, "SELECT note FROM stock"), "note\nlocal\n");
    apply(&mut session, "999999999999999.000000.b", "u", "NULL", "'{\"sku\": \"egg\", \"count\": 2, \"note\": \"new\"}'::jsonb");
    assert_eq!(ok(&mut session, "SELECT count, note FROM stock"), "count|note\n2|new\n");
    apply(&mut session, "999999999999999.000001.b", "d", "'{\"sku\": \"egg\"}'::jsonb", "NULL");
    assert_eq!(ok(&mut session, "SELECT count(*) FROM stock"), "count\n0\n");
}

#[test]
fn counter_columns_add_up_concurrent_changes() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    installed(&mut session);
    ok(&mut session, "CREATE TABLE stock (sku text PRIMARY KEY, count integer, note text)");
    ok(&mut session, "SELECT eggdb_mp.enable('public', 'stock', 'crdt', NULL, 'count')");
    ok(&mut session, "INSERT INTO stock VALUES ('egg', 10, 'local')");
    ok(&mut session, "UPDATE stock SET count = count - 3 WHERE sku = 'egg'");
    apply(
        &mut session,
        "000000000000001.000000.b",
        "u",
        "'{\"sku\": \"egg\", \"count\": 10, \"note\": \"local\"}'::jsonb",
        "'{\"sku\": \"egg\", \"count\": 15, \"note\": \"remote\"}'::jsonb",
    );
    assert_eq!(ok(&mut session, "SELECT count, note FROM stock"), "count|note\n12|local\n");
}

#[test]
fn an_application_function_decides_a_conflict() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    installed(&mut session);
    ok(&mut session, "CREATE TABLE stock (sku text PRIMARY KEY, count integer, note text)");
    ok(
        &mut session,
        "CREATE FUNCTION keep_larger(local jsonb, remote jsonb, local_stamp text, remote_stamp text) RETURNS jsonb LANGUAGE plpgsql AS $$ BEGIN IF (remote ->> 'count')::integer > (local ->> 'count')::integer THEN RETURN remote; END IF; RETURN local; END $$",
    );
    ok(&mut session, "SELECT eggdb_mp.enable('public', 'stock', 'function', 'keep_larger', NULL)");
    ok(&mut session, "INSERT INTO stock VALUES ('egg', 10, 'local')");
    let smaller = apply(&mut session, "999999999999999.000000.b", "u", "NULL", "'{\"sku\": \"egg\", \"count\": 4, \"note\": \"remote\"}'::jsonb");
    assert!(smaller.contains("resolved"), "{smaller}");
    assert_eq!(ok(&mut session, "SELECT count, note FROM stock"), "count|note\n10|local\n");
    apply(&mut session, "999999999999999.000001.b", "u", "NULL", "'{\"sku\": \"egg\", \"count\": 40, \"note\": \"remote\"}'::jsonb");
    assert_eq!(ok(&mut session, "SELECT count, note FROM stock"), "count|note\n40|remote\n");
}

#[test]
fn a_table_without_a_primary_key_is_refused() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    installed(&mut session);
    ok(&mut session, "CREATE TABLE loose (note text)");
    let refused = run(&mut session, "SELECT eggdb_mp.enable('public', 'loose', 'lww', NULL, NULL)").unwrap_err();
    assert!(refused.to_string().contains("needs a primary key"), "{refused}");
}
