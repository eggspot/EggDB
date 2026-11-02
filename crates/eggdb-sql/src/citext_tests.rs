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

fn state(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(output) => panic!("{sql} succeeded: {output}"),
        Err(error) => error.state().code().to_owned(),
    }
}

#[test]
fn create_extension_citext_and_hstore_succeed_and_list_in_pg_extension() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        ok(&mut session, "CREATE EXTENSION IF NOT EXISTS citext"),
        "CREATE EXTENSION"
    );
    assert_eq!(
        ok(&mut session, "CREATE EXTENSION hstore"),
        "CREATE EXTENSION"
    );
    let listed = ok(
        &mut session,
        "SELECT extname FROM pg_extension ORDER BY extname",
    );
    assert!(listed.contains("citext"), "{listed}");
    assert!(listed.contains("hstore"), "{listed}");
}

#[test]
fn citext_columns_keep_the_case_written_and_compare_without_it() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    ok(
        &mut session,
        "CREATE TABLE account (id integer PRIMARY KEY, email citext UNIQUE)",
    );
    ok(
        &mut session,
        "INSERT INTO account VALUES (1, 'Alice@Example.com'), (2, 'bob@example.com')",
    );
    assert_eq!(
        state(
            &mut session,
            "INSERT INTO account VALUES (3, 'ALICE@EXAMPLE.COM')"
        ),
        "23505"
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT id, email FROM account WHERE email = 'alice@example.COM'"
        ),
        "id|email\n1|Alice@Example.com\nSELECT 1"
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT email FROM account ORDER BY email DESC"
        ),
        "email\nbob@example.com\nAlice@Example.com\nSELECT 2"
    );
    assert_eq!(
        ok(&mut session, "SELECT 'ABC'::citext = 'abc'::citext"),
        "?column?\nt\nSELECT 1"
    );
}

#[test]
fn hstore_values_print_in_postgresql_order_and_convert_to_json() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    ok(
        &mut session,
        "CREATE TABLE item (id integer, attributes hstore)",
    );
    ok(
        &mut session,
        "INSERT INTO item VALUES (1, 'color=>red, size=>NULL, a=>\"1\"')",
    );
    assert_eq!(
        ok(&mut session, "SELECT attributes FROM item"),
        "attributes\n\"a\"=>\"1\", \"size\"=>NULL, \"color\"=>\"red\"\nSELECT 1"
    );
    assert_eq!(
        ok(&mut session, "SELECT 'b=>2, a=>1'::hstore::json AS j"),
        "j\n{\"a\": \"1\", \"b\": \"2\"}\nSELECT 1"
    );
    assert_eq!(state(&mut session, "SELECT 'a=>'::hstore"), "42601");
    assert_eq!(
        state(&mut session, "INSERT INTO item VALUES (2, 'a b')"),
        "42601"
    );
}
