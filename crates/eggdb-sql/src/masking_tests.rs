use crate::memory_store::MemoryStore;
use crate::session::Session;
use crate::tests::render;
use crate::Dialect;

fn run(session: &mut Session<'_>, sql: &str) -> String {
    let mut out = Vec::new();
    session
        .execute(sql, &mut |result| out.push(render(&result)))
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    out.join("\n")
}

fn customers() -> MemoryStore {
    let store = MemoryStore::default();
    let mut tsql = Session::with_dialect(&store, Dialect::SqlServer);
    run(
        &mut tsql,
        "CREATE TABLE customers (id int PRIMARY KEY, name varchar(40), email varchar(100) MASKED WITH (FUNCTION = 'email()'), salary int MASKED WITH (FUNCTION = 'default()'))",
    );
    run(
        &mut tsql,
        "INSERT INTO customers VALUES (1, 'Ada', 'ada@example.com', 9000), (2, 'Grace', 'grace@example.com', 8000)",
    );
    tsql.close().unwrap();
    let mut owner = Session::new(&store);
    run(&mut owner, "CREATE ROLE support");
    run(&mut owner, "GRANT SELECT ON customers TO support");
    run(&mut owner, "CREATE VIEW contacts AS SELECT id, email AS name FROM customers");
    run(&mut owner, "GRANT SELECT ON contacts TO support");
    run(
        &mut owner,
        "CREATE FUNCTION email_of(person int) RETURNS text LANGUAGE sql STABLE AS $$ SELECT email FROM customers WHERE id = person $$",
    );
    owner.close().unwrap();
    store
}

fn as_support(store: &MemoryStore, sql: &str) -> String {
    let mut session = Session::new(store);
    run(&mut session, "SET ROLE support");
    let shown = run(&mut session, sql);
    session.close().unwrap();
    shown
}

#[test]
fn a_role_without_unmask_sees_masked_values_and_filters_on_real_ones() {
    let store = customers();
    assert_eq!(
        as_support(&store, "SELECT id, name, email, salary FROM customers ORDER BY id"),
        "id|name|email|salary\n1|Ada|aXXX@XXXX.com|0\n2|Grace|gXXX@XXXX.com|0\nSELECT 2"
    );
    assert_eq!(
        as_support(&store, "SELECT name FROM customers WHERE email = 'grace@example.com'"),
        "name\nGrace\nSELECT 1"
    );
    assert_eq!(
        as_support(&store, "SELECT upper(email) AS shout, sum(salary) AS total FROM customers GROUP BY email ORDER BY 1"),
        "shout|total\nxxxx|0\nxxxx|0\nSELECT 2"
    );
    for (sql, shown) in [
        ("SELECT email AS name FROM customers WHERE id = 1", "name\nxxxx\nSELECT 1"),
        ("SELECT email name FROM customers WHERE id = 1", "name\nxxxx\nSELECT 1"),
        ("SELECT name FROM customers WHERE id = 3 UNION ALL SELECT email FROM customers WHERE id = 1", "name\nxxxx\nSELECT 1"),
        ("SELECT name FROM (SELECT email AS name FROM customers) AS t", "name\nxxxx\nxxxx\nSELECT 2"),
        ("SELECT * FROM contacts WHERE id = 2", "id|name\n0|xxxx\nSELECT 1"),
        ("SELECT email_of(1) AS contact", "contact\nxxxx\nSELECT 1"),
    ] {
        assert_eq!(as_support(&store, sql), shown, "{sql}");
    }
    let mut owner = Session::new(&store);
    assert_eq!(
        run(&mut owner, "SELECT email FROM customers WHERE id = 1"),
        "email\nada@example.com\nSELECT 1"
    );
    run(&mut owner, "GRANT UNMASK TO support");
    owner.close().unwrap();
    assert_eq!(
        as_support(&store, "SELECT email, salary FROM customers WHERE id = 2"),
        "email|salary\ngrace@example.com|8000\nSELECT 1"
    );
    let mut owner = Session::new(&store);
    run(&mut owner, "REVOKE UNMASK FROM support");
    run(&mut owner, "ALTER TABLE customers ALTER COLUMN salary DROP MASKED");
    run(
        &mut owner,
        "ALTER TABLE customers ALTER COLUMN name ADD MASKED WITH (FUNCTION = 'partial(1, \"***\", 0)')",
    );
    owner.close().unwrap();
    assert_eq!(
        as_support(&store, "SELECT name, salary FROM customers WHERE id = 1"),
        "name|salary\nA***|9000\nSELECT 1"
    );
}

#[test]
fn only_a_superuser_or_the_owner_changes_masks() {
    let store = customers();
    let mut session = Session::new(&store);
    run(&mut session, "SET ROLE support");
    for sql in [
        "ALTER TABLE customers ALTER COLUMN email DROP MASKED",
        "GRANT UNMASK TO support",
    ] {
        let error = session.execute(sql, &mut |_| {}).expect_err(sql);
        assert_eq!(error.state(), crate::SqlState::INSUFFICIENT_PRIVILEGE, "{sql}: {error}");
    }
    session.close().unwrap();
    let mut owner = Session::new(&store);
    let wrong = owner
        .execute("ALTER TABLE customers ALTER COLUMN email ADD MASKED WITH (FUNCTION = 'mask()')", &mut |_| {})
        .expect_err("an unknown mask function");
    assert_eq!(wrong.state(), crate::SqlState::INVALID_PARAMETER_VALUE);
    run(&mut owner, "DROP VIEW contacts");
    run(&mut owner, "DROP TABLE customers");
    run(&mut owner, "CREATE TABLE customers (id int PRIMARY KEY, email text)");
    run(&mut owner, "INSERT INTO customers VALUES (1, 'ada@example.com')");
    run(&mut owner, "GRANT SELECT ON customers TO support");
    owner.close().unwrap();
    assert_eq!(
        as_support(&store, "SELECT email FROM customers"),
        "email\nada@example.com\nSELECT 1"
    );
}

#[test]
fn a_mask_follows_its_table_and_column_when_they_are_renamed() {
    let store = customers();
    let mut owner = Session::new(&store);
    run(&mut owner, "DROP VIEW contacts");
    run(&mut owner, "ALTER TABLE customers RENAME COLUMN email TO mail");
    run(&mut owner, "ALTER TABLE customers RENAME TO clients");
    run(&mut owner, "GRANT SELECT ON clients TO support");
    owner.close().unwrap();
    assert_eq!(
        as_support(&store, "SELECT mail FROM clients WHERE id = 1"),
        "mail\naXXX@XXXX.com\nSELECT 1"
    );
}

#[test]
fn a_role_without_unmask_cannot_copy_masked_values_or_run_dynamic_sql() {
    let store = customers();
    let mut owner = Session::new(&store);
    run(&mut owner, "CREATE TABLE notes (id int PRIMARY KEY, body text)");
    run(&mut owner, "GRANT SELECT, INSERT, UPDATE ON notes TO support");
    run(&mut owner, "GRANT UPDATE ON customers TO support");
    owner.close().unwrap();
    let mut session = Session::new(&store);
    run(&mut session, "SET ROLE support");
    for sql in [
        "INSERT INTO notes SELECT id, email FROM customers",
        "UPDATE customers SET name = email",
        "SELECT email INTO copied FROM customers",
        "CREATE TABLE copied AS SELECT email FROM customers",
        "DO $$ BEGIN INSERT INTO notes SELECT id, email FROM customers; END $$",
    ] {
        let error = session.execute(sql, &mut |_| {}).expect_err(sql);
        assert_eq!(error.state(), crate::SqlState::INSUFFICIENT_PRIVILEGE, "{sql}: {error}");
    }
    run(&mut session, "INSERT INTO notes VALUES (1, 'called Ada')");
    run(&mut session, "UPDATE customers SET name = 'Ada L.' WHERE id = 1");
    session.close().unwrap();
    assert_eq!(
        as_support(&store, "SELECT name FROM customers WHERE id = 1"),
        "name\nAda L.\nSELECT 1"
    );
}
