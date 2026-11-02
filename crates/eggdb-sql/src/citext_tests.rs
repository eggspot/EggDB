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

#[test]
fn a_unique_citext_index_rejects_case_variants_whenever_they_meet() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    ok(&mut session, "CREATE TABLE login (id integer, name citext)");
    ok(
        &mut session,
        "CREATE UNIQUE INDEX login_name ON login (name)",
    );
    ok(&mut session, "INSERT INTO login VALUES (1, 'Zoë')");
    assert_eq!(
        state(&mut session, "INSERT INTO login VALUES (2, 'ZOË')"),
        "23505"
    );
    ok(&mut session, "CREATE TABLE guest (name citext)");
    ok(
        &mut session,
        "INSERT INTO guest VALUES ('Straße'), ('STRASSE')",
    );
    assert_eq!(
        state(&mut session, "CREATE UNIQUE INDEX guest_name ON guest (name)"),
        "23505"
    );
    assert_eq!(
        ok(&mut session, "SELECT id FROM login WHERE name = 'zoË'"),
        "id\n1\nSELECT 1"
    );
}

#[test]
fn group_by_distinct_and_order_by_agree_on_citext() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    ok(&mut session, "CREATE TABLE fruit (id integer, name citext)");
    ok(
        &mut session,
        "INSERT INTO fruit VALUES (1, 'Apple'), (2, 'APPLE'), (3, 'apple'), (4, 'Banana'), (5, 'banana'), (6, 'cherry')",
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT count(*) AS n FROM fruit GROUP BY name ORDER BY n"
        ),
        "n\n1\n2\n3\nSELECT 3"
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT count(*) AS n FROM (SELECT DISTINCT name FROM fruit) d"
        ),
        "n\n3\nSELECT 1"
    );
    assert_eq!(
        ok(&mut session, "SELECT count(DISTINCT name) AS n FROM fruit"),
        "n\n3\nSELECT 1"
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT id FROM fruit ORDER BY name DESC, id"
        ),
        "id\n6\n4\n5\n1\n2\n3\nSELECT 6"
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT count(*) AS n FROM fruit a JOIN fruit b ON a.name = b.name"
        ),
        "n\n14\nSELECT 1"
    );
}

#[test]
fn like_and_ilike_on_citext_ignore_case() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    ok(&mut session, "CREATE TABLE account (email citext)");
    ok(
        &mut session,
        "INSERT INTO account VALUES ('Alice@Example.com'), ('bob@example.COM')",
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT email FROM account WHERE email LIKE 'ALICE%'"
        ),
        "email\nAlice@Example.com\nSELECT 1"
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT count(*) AS n FROM account WHERE email ILIKE '%EXAMPLE.com'"
        ),
        "n\n2\nSELECT 1"
    );
    assert_eq!(
        ok(
            &mut session,
            "SELECT email FROM account WHERE email NOT LIKE 'a%'"
        ),
        "email\nbob@example.COM\nSELECT 1"
    );
}

#[test]
fn a_view_over_a_citext_column_reads_back_after_reopening() {
    let store = MemoryStore::default();
    {
        let mut session = Session::new(&store);
        ok(&mut session, "CREATE TABLE account (email citext)");
        ok(&mut session, "INSERT INTO account VALUES ('Ann@Example.com')");
        ok(
            &mut session,
            "CREATE VIEW account_email AS SELECT email FROM account",
        );
    }
    let mut session = Session::new(&store);
    assert_eq!(
        ok(
            &mut session,
            "SELECT email FROM account_email WHERE email = 'ANN@example.com'"
        ),
        "email\nAnn@Example.com\nSELECT 1"
    );
}

fn item_session(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::new(store);
    ok(&mut session, "CREATE TABLE item (id integer, attrs hstore)");
    ok(
        &mut session,
        "INSERT INTO item VALUES (1, 'color=>red, size=>L'), (2, 'color=>blue, weight=>NULL')",
    );
    session
}

#[test]
fn hstore_operators_fetch_test_contain_join_and_remove() {
    let store = MemoryStore::default();
    let mut session = item_session(&store);
    let cases = [
        (
            "SELECT id, attrs -> 'color' AS color FROM item ORDER BY id",
            "id|color\n1|red\n2|blue\nSELECT 2",
        ),
        (
            "SELECT attrs -> ARRAY['weight', 'color'] AS v FROM item WHERE id = 2",
            "v\n{NULL,blue}\nSELECT 1",
        ),
        ("SELECT id FROM item WHERE attrs ? 'weight'", "id\n2\nSELECT 1"),
        (
            "SELECT id FROM item WHERE attrs ?| ARRAY['size', 'x']",
            "id\n1\nSELECT 1",
        ),
        (
            "SELECT id FROM item WHERE attrs ?& ARRAY['color', 'size']",
            "id\n1\nSELECT 1",
        ),
        (
            "SELECT id FROM item WHERE attrs @> 'color=>blue'",
            "id\n2\nSELECT 1",
        ),
        (
            "SELECT id FROM item WHERE 'color=>red'::hstore <@ attrs",
            "id\n1\nSELECT 1",
        ),
        (
            "SELECT attrs || 'size=>M' AS a FROM item WHERE id = 1",
            "a\n\"size\"=>\"M\", \"color\"=>\"red\"\nSELECT 1",
        ),
        (
            "SELECT attrs - 'color' AS a FROM item WHERE id = 1",
            "a\n\"size\"=>\"L\"\nSELECT 1",
        ),
        (
            "SELECT attrs - ARRAY['color', 'size'] AS a FROM item WHERE id = 1",
            "a\n\nSELECT 1",
        ),
        (
            "SELECT attrs - 'color=>blue'::hstore AS a FROM item WHERE id = 2",
            "a\n\"weight\"=>NULL\nSELECT 1",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(ok(&mut session, sql), expected, "{sql}");
    }
}

#[test]
fn hstore_functions_list_test_cut_build_and_convert() {
    let store = MemoryStore::default();
    let mut session = item_session(&store);
    let cases = [
        (
            "SELECT akeys(attrs) AS k, avals(attrs) AS v FROM item WHERE id = 2",
            "k|v\n{color,weight}|{blue,NULL}\nSELECT 1",
        ),
        (
            "SELECT skeys(attrs) AS k FROM item WHERE id = 1",
            "k\nsize\ncolor\nSELECT 2",
        ),
        (
            "SELECT svals(attrs) AS v FROM item WHERE id = 2",
            "v\nblue\nNULL\nSELECT 2",
        ),
        (
            "SELECT key, value FROM each('a=>1, b=>NULL'::hstore)",
            "key|value\na|1\nb|NULL\nSELECT 2",
        ),
        (
            "SELECT exist(attrs, 'weight') AS e, defined(attrs, 'weight') AS d FROM item WHERE id = 2",
            "e|d\nt|f\nSELECT 1",
        ),
        (
            "SELECT delete(attrs, 'color') AS a FROM item WHERE id = 2",
            "a\n\"weight\"=>NULL\nSELECT 1",
        ),
        (
            "SELECT slice(attrs, ARRAY['color', 'x']) AS a FROM item WHERE id = 1",
            "a\n\"color\"=>\"red\"\nSELECT 1",
        ),
        ("SELECT hstore('k', 'v') AS h", "h\n\"k\"=>\"v\"\nSELECT 1"),
        (
            "SELECT hstore(ARRAY['b', 'a'], ARRAY['2', '1']) AS h",
            "h\n\"a\"=>\"1\", \"b\"=>\"2\"\nSELECT 1",
        ),
        (
            "SELECT hstore_to_json(attrs) AS j FROM item WHERE id = 2",
            "j\n{\"color\": \"blue\", \"weight\": null}\nSELECT 1",
        ),
        (
            "SELECT hstore_to_jsonb('a=>1') AS j",
            "j\n{\"a\": \"1\"}\nSELECT 1",
        ),
        (
            "SELECT hstore_to_array('a=>1, b=>NULL') AS a",
            "a\n{a,1,b,NULL}\nSELECT 1",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(ok(&mut session, sql), expected, "{sql}");
    }
    assert_eq!(
        state(&mut session, "SELECT hstore(ARRAY['a'], ARRAY['1', '2'])"),
        "2202E"
    );
    assert_eq!(
        state(&mut session, "SELECT hstore(ARRAY[NULL::text], ARRAY['1'])"),
        "22004"
    );
}
