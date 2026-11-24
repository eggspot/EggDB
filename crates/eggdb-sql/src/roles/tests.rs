use super::*;
use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{KeyValueStore, Session};

fn run(session: &mut Session<'_>, sql: &str) -> SqlResult<String> {
    let mut last = String::new();
    session.execute(sql, &mut |result| last = render(&result))?;
    Ok(last)
}

fn setup(store: &MemoryStore) {
    crate::memory_store::use_seeded_random_bytes();
    let mut admin = Session::new(store);
    for sql in [
        "CREATE TABLE accounts (id integer PRIMARY KEY, owner text)",
        "INSERT INTO accounts VALUES (1, 'ada')",
        "CREATE TABLE audit (id integer)",
        "CREATE ROLE readers",
        "CREATE USER clerk WITH PASSWORD 'secret' IN ROLE readers",
        "GRANT SELECT ON accounts TO readers",
        "GRANT INSERT, UPDATE ON TABLE accounts TO clerk",
    ] {
        run(&mut admin, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
}

#[test]
fn a_role_reads_and_writes_only_what_it_was_granted() {
    let store = MemoryStore::default();
    setup(&store);
    let mut clerk = Session::new(&store);
    clerk.identify_role("clerk", "eggdb").unwrap();
    assert_eq!(
        run(&mut clerk, "SELECT owner FROM accounts").unwrap(),
        "owner\nada\nSELECT 1"
    );
    run(&mut clerk, "INSERT INTO accounts VALUES (2, 'grace')").unwrap();
    run(&mut clerk, "UPDATE accounts SET owner = 'lin' WHERE id = 2").unwrap();
    for sql in [
        "DELETE FROM accounts",
        "SELECT * FROM audit",
        "TRUNCATE accounts",
        "CREATE TABLE mine (id integer)",
        "CREATE ROLE intruder",
        "GRANT SELECT ON audit TO clerk",
        "SELECT a.id FROM accounts a JOIN audit b ON a.id = b.id",
    ] {
        let error = run(&mut clerk, sql).expect_err(sql);
        assert_eq!(
            error.state(),
            SqlState::INSUFFICIENT_PRIVILEGE,
            "{sql}: {error}"
        );
    }
}

#[test]
fn revoking_and_leaving_a_role_take_privileges_away() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    run(&mut admin, "REVOKE readers FROM clerk").unwrap();
    let mut clerk = Session::new(&store);
    clerk.identify_role("clerk", "eggdb").unwrap();
    assert!(run(&mut clerk, "SELECT * FROM accounts").is_err());
    run(&mut admin, "GRANT SELECT ON accounts TO PUBLIC").unwrap();
    assert!(run(&mut clerk, "SELECT * FROM accounts").is_ok());
    run(&mut admin, "REVOKE ALL ON accounts FROM PUBLIC").unwrap();
    assert!(run(&mut clerk, "SELECT * FROM accounts").is_err());
}

#[test]
fn roles_are_stored_with_a_scram_verifier_and_never_the_password() {
    let store = MemoryStore::default();
    setup(&store);
    let reader = store.begin_read().unwrap();
    let clerk = stored_role(&reader, "clerk").unwrap().unwrap();
    assert!(clerk.login && !clerk.superuser);
    assert_eq!(clerk.member_of, ["readers"]);
    let verifier = clerk.scram_verifier.unwrap();
    assert!(!verifier.contains("secret"));
    let (iterations, salt, _, _) = parse_scram_verifier(&verifier).unwrap();
    assert_eq!(
        (iterations, salt.len()),
        (SCRAM_ITERATIONS, SCRAM_SALT_BYTES)
    );
    assert_eq!(login_verifier(&reader, "readers").unwrap(), None);
    assert_eq!(login_verifier(&reader, "clerk").unwrap(), Some(verifier));
}

#[test]
fn debug_output_shows_neither_a_role_password_nor_its_verifier() {
    let store = MemoryStore::default();
    setup(&store);
    let reader = store.begin_read().unwrap();
    let clerk = stored_role(&reader, "clerk").unwrap().unwrap();
    let verifier = clerk.scram_verifier.clone().unwrap();
    let shown = format!("{clerk:?}");
    assert!(!shown.contains(&verifier), "{shown}");
    assert!(!shown.contains("SCRAM-SHA-256$"), "{shown}");
    assert!(shown.contains("scram_verifier: Some(\"***\")"), "{shown}");
    let options = RoleOptions {
        password: Some(Some("pa55word".to_owned())),
        ..RoleOptions::default()
    };
    let shown = format!("{options:?}");
    assert!(!shown.contains("pa55word"), "{shown}");
    assert!(shown.contains("password: Some(Some(\"***\"))"), "{shown}");
    let cleared = RoleOptions {
        password: Some(None),
        ..RoleOptions::default()
    };
    assert!(format!("{cleared:?}").contains("password: Some(None)"));
}

#[test]
fn role_statements_refuse_what_postgresql_refuses() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    for (sql, state) in [
        ("CREATE ROLE clerk", SqlState::DUPLICATE_OBJECT),
        ("CREATE ROLE public", SqlState::RESERVED_NAME),
        ("DROP ROLE nobody", SqlState::UNDEFINED_OBJECT),
        ("DROP ROLE clerk", SqlState::DEPENDENT_OBJECTS_STILL_EXIST),
        ("GRANT clerk TO clerk", SqlState::INVALID_GRANT_OPERATION),
        ("GRANT clerk TO readers", SqlState::INVALID_GRANT_OPERATION),
        (
            "GRANT SELECT ON nowhere TO clerk",
            SqlState::UNDEFINED_TABLE,
        ),
        ("ALTER ROLE nobody LOGIN", SqlState::UNDEFINED_OBJECT),
    ] {
        let error = run(&mut admin, sql).expect_err(sql);
        assert_eq!(error.state(), state, "{sql}: {error}");
    }
    run(&mut admin, "CREATE ROLE IF NOT EXISTS clerk").unwrap();
    run(&mut admin, "DROP ROLE IF EXISTS nobody").unwrap();
    let mut nologin = Session::new(&store);
    assert_eq!(
        nologin
            .identify_role("readers", "eggdb")
            .unwrap_err()
            .state(),
        SqlState::INVALID_AUTHORIZATION_SPECIFICATION
    );
}

#[test]
fn a_superuser_role_is_not_checked_and_pg_roles_lists_stored_roles() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    run(&mut admin, "CREATE USER boss SUPERUSER PASSWORD 'x'").unwrap();
    let mut boss = Session::new(&store);
    boss.identify_role("boss", "eggdb").unwrap();
    assert!(run(&mut boss, "SELECT * FROM audit").is_ok());
    assert_eq!(
        run(
            &mut admin,
            "SELECT rolname, rolsuper, rolcanlogin FROM pg_roles WHERE rolname IN ('boss', 'clerk', 'readers') ORDER BY rolname"
        )
        .unwrap(),
        "rolname|rolsuper|rolcanlogin\nboss|t|t\nclerk|f|t\nreaders|f|f\nSELECT 3"
    );
}

#[test]
fn verifier_text_round_trips_and_rejects_garbage() {
    crate::memory_store::use_seeded_random_bytes();
    let text = scram_verifier_text("pencil").unwrap();
    assert!(parse_scram_verifier(&text).is_some());
    assert_eq!(scram_verifier_text(&text).unwrap(), text);
    for garbage in [
        "SCRAM-SHA-256$",
        "SCRAM-SHA-256$0:AAAA$AAAA:AAAA",
        "SCRAM-SHA-256$4096:$x:y",
        "SCRAM-SHA-256$4096:c2FsdA==$c2hvcnQ=:c2hvcnQ=",
    ] {
        assert!(parse_scram_verifier(garbage).is_none(), "{garbage}");
        assert!(scram_verifier_text(garbage).is_err(), "{garbage}");
    }
}

#[test]
fn hostile_role_records_are_errors() {
    for bytes in [
        &[][..],
        &[RECORD_VERSION],
        &[RECORD_VERSION, 0, 9, 0, 0, 0],
        &[RECORD_VERSION, 0, 0, 0, 0, 0, 255, 255, 255, 255],
        &[2, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    ] {
        assert!(decode_role("r", bytes).is_err(), "{bytes:?}");
    }
}

#[test]
fn a_grant_option_lets_a_role_grant_on_and_revoking_it_takes_only_that() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    for sql in [
        "CREATE USER auditor WITH PASSWORD 'secret'",
        "GRANT SELECT ON audit TO clerk WITH GRANT OPTION",
    ] {
        run(&mut admin, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
    let mut clerk = Session::new(&store);
    clerk.identify_role("clerk", "eggdb").unwrap();
    run(&mut clerk, "GRANT SELECT ON audit TO auditor").unwrap();
    let error = run(&mut clerk, "GRANT DELETE ON audit TO auditor").expect_err("delete");
    assert_eq!(error.state(), SqlState::INSUFFICIENT_PRIVILEGE);
    assert_eq!(
        run(
            &mut clerk,
            "SELECT has_table_privilege('audit', 'SELECT WITH GRANT OPTION')"
        )
        .unwrap(),
        "has_table_privilege\nt\nSELECT 1"
    );
    run(&mut admin, "REVOKE GRANT OPTION FOR SELECT ON audit FROM clerk").unwrap();
    assert_eq!(
        run(&mut clerk, "SELECT count(*) FROM audit").unwrap(),
        "count\n0\nSELECT 1"
    );
    let error = run(&mut clerk, "GRANT SELECT ON audit TO auditor").expect_err("again");
    assert_eq!(error.state(), SqlState::INSUFFICIENT_PRIVILEGE);
}

#[test]
fn an_acl_item_marks_each_grantable_privilege_with_a_star() {
    assert_eq!(
        acl_text("clerk", Privilege::Select.bit() | Privilege::Insert.bit(), Privilege::Select.bit(), "ada"),
        "clerk=ar*/ada"
    );
}

#[test]
fn a_table_s_owner_holds_every_privilege_and_may_grant_them() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    for sql in [
        "CREATE USER auditor WITH PASSWORD 'secret'",
        "ALTER TABLE audit OWNER TO clerk",
        "ALTER TABLE accounts OWNER TO postgres",
    ] {
        run(&mut admin, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
    assert_eq!(
        run(
            &mut admin,
            "SELECT pg_get_userbyid(relowner) FROM pg_class WHERE relname = 'audit'"
        )
        .unwrap(),
        "pg_get_userbyid\nclerk\nSELECT 1"
    );
    let mut clerk = Session::new(&store);
    clerk.identify_role("clerk", "eggdb").unwrap();
    run(&mut clerk, "INSERT INTO audit VALUES (1)").unwrap();
    run(&mut clerk, "DELETE FROM audit").unwrap();
    run(&mut clerk, "GRANT SELECT ON audit TO auditor").unwrap();
    run(&mut admin, "ALTER TABLE audit RENAME TO audit_log").unwrap();
    run(&mut clerk, "TRUNCATE audit_log").unwrap();
    let error = run(&mut clerk, "DELETE FROM accounts").expect_err("not owner");
    assert_eq!(error.state(), SqlState::INSUFFICIENT_PRIVILEGE);
}

#[test]
fn a_role_with_create_on_a_schema_makes_tables_it_owns_and_may_drop() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    run(&mut admin, "GRANT CREATE ON SCHEMA public TO readers").unwrap();
    let mut clerk = Session::new(&store);
    clerk.identify_role("clerk", "eggdb").unwrap();
    run(&mut clerk, "CREATE TABLE mine (id integer)").unwrap();
    run(&mut clerk, "INSERT INTO mine VALUES (1)").unwrap();
    run(&mut clerk, "ALTER TABLE mine ADD COLUMN note text").unwrap();
    assert_eq!(
        run(&mut clerk, "SELECT id, note FROM mine").unwrap(),
        "id|note\n1|\nSELECT 1"
    );
    for sql in ["ALTER TABLE mine OWNER TO readers", "DROP TABLE audit"] {
        let error = run(&mut clerk, sql).expect_err(sql);
        assert_eq!(error.state(), SqlState::INSUFFICIENT_PRIVILEGE, "{sql}");
    }
    run(&mut clerk, "DROP TABLE mine").unwrap();
}

#[test]
fn a_column_deny_hides_one_column_until_it_is_granted_again() {
    let store = MemoryStore::default();
    let mut admin = Session::new(&store);
    for sql in [
        "CREATE TABLE items (id integer PRIMARY KEY, secret text)",
        "CREATE ROLE reader",
    ] {
        run(&mut admin, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
    let mut tsql = Session::with_dialect(&store, crate::Dialect::SqlServer);
    for sql in [
        "GRANT SELECT ON items TO reader",
        "DENY SELECT ON OBJECT::items (secret) TO reader",
    ] {
        tsql.execute(sql, &mut |_| {})
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
    assert_eq!(
        run(
            &mut admin,
            "SELECT has_column_privilege('reader', 'items', 'id', 'SELECT'), \
             has_column_privilege('reader', 'items', 'secret', 'SELECT')"
        )
        .unwrap(),
        "has_column_privilege|has_column_privilege\nt|f\nSELECT 1"
    );
    tsql.execute("GRANT SELECT ON OBJECT::items (secret) TO reader", &mut |_| {})
        .expect("the grant clears the deny");
    assert_eq!(
        run(
            &mut admin,
            "SELECT has_column_privilege('reader', 'items', 'secret', 'SELECT')"
        )
        .unwrap(),
        "has_column_privilege\nt\nSELECT 1"
    );
}

#[test]
fn mysql_account_limits_refuse_statements_past_the_hour_s_count() {
    let store = MemoryStore::default();
    let mut mysql = Session::with_dialect(&store, crate::Dialect::MySql);
    mysql
        .execute(
            "CREATE USER 'hourly_limit_user'@'%' IDENTIFIED BY 'secret' WITH MAX_QUERIES_PER_HOUR 2",
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(role_limits(&store.begin_read().unwrap(), "hourly_limit_user").unwrap().queries_per_hour, 2);
    let mut limited = Session::with_dialect(&store, crate::Dialect::MySql);
    limited.identify_role("hourly_limit_user", "eggdb").unwrap();
    for _ in 0..2 {
        limited.execute("SELECT 1", &mut |_| {}).unwrap();
    }
    let error = limited.execute("SELECT 1", &mut |_| {}).unwrap_err();
    assert_eq!(error.state(), SqlState::MYSQL_SYNTAX_OR_ACCESS);
    mysql
        .execute("ALTER USER 'hourly_limit_user'@'%' WITH MAX_QUERIES_PER_HOUR 0", &mut |_| {})
        .unwrap();
    assert_eq!(role_limits(&store.begin_read().unwrap(), "hourly_limit_user").unwrap(), RoleLimits::default());
}

#[test]
fn connection_limits_refuse_logins_past_the_open_sessions_and_free_them_on_close() {
    let store = MemoryStore::default();
    let mut admin = Session::new(&store);
    admin
        .execute("CREATE ROLE limited_login LOGIN CONNECTION LIMIT 1", &mut |_| {})
        .unwrap();
    assert_eq!(role_limits(&store.begin_read().unwrap(), "limited_login").unwrap().connections, 1);
    let mut shown = Vec::new();
    admin
        .execute("SELECT rolconnlimit FROM pg_roles WHERE rolname = 'limited_login'", &mut |result| shown.push(result))
        .unwrap();
    assert_eq!(shown.last().map(|result| result.rows.clone()), Some(vec![vec![crate::types::Value::Integer(1)]]));
    let mut first = Session::new(&store);
    first.identify_role("limited_login", "connection-limit-test").unwrap();
    let mut second = Session::new(&store);
    let error = second.identify_role("limited_login", "connection-limit-test").unwrap_err();
    assert_eq!(error.state(), SqlState::TOO_MANY_CONNECTIONS);
    drop(first);
    second.identify_role("limited_login", "connection-limit-test").unwrap();
    admin
        .execute("ALTER ROLE limited_login CONNECTION LIMIT -1", &mut |_| {})
        .unwrap();
    assert_eq!(role_limits(&store.begin_read().unwrap(), "limited_login").unwrap(), RoleLimits::default());
    let mut mysql = Session::with_dialect(&store, crate::Dialect::MySql);
    mysql
        .execute(
            "CREATE USER 'few_logins'@'%' IDENTIFIED BY 'secret' WITH MAX_USER_CONNECTIONS 3 MAX_CONNECTIONS_PER_HOUR 5",
            &mut |_| {},
        )
        .unwrap();
    let limits = role_limits(&store.begin_read().unwrap(), "few_logins").unwrap();
    assert_eq!((limits.connections, limits.connections_per_hour), (3, 5));
}

#[test]
fn sequences_are_used_only_with_their_privileges() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    for sql in [
        "CREATE SEQUENCE tickets",
        "CREATE SEQUENCE hidden",
        "GRANT USAGE ON SEQUENCE tickets TO clerk",
        "GRANT SELECT ON SEQUENCE public.hidden TO readers",
    ] {
        run(&mut admin, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
    let mut clerk = Session::new(&store);
    clerk.identify_role("clerk", "eggdb").unwrap();
    assert_eq!(run(&mut clerk, "SELECT nextval('tickets') AS n").unwrap(), "n\n1\nSELECT 1");
    assert_eq!(run(&mut clerk, "SELECT currval('tickets') AS n").unwrap(), "n\n1\nSELECT 1");
    for sql in ["SELECT setval('tickets', 10)", "SELECT nextval('hidden')", "SELECT setval('hidden', 5)"] {
        let error = run(&mut clerk, sql).expect_err(sql);
        assert_eq!(error.state(), SqlState::INSUFFICIENT_PRIVILEGE, "{sql}: {error}");
        assert!(error.to_string().contains("permission denied for sequence"), "{error}");
    }
    run(&mut admin, "GRANT UPDATE ON SEQUENCE hidden TO clerk").unwrap();
    assert_eq!(run(&mut clerk, "SELECT setval('hidden', 5) AS n").unwrap(), "n\n5\nSELECT 1");
    run(&mut admin, "REVOKE USAGE ON SEQUENCE tickets FROM clerk").unwrap();
    assert!(run(&mut clerk, "SELECT nextval('tickets')").is_err());
    assert_eq!(
        run(&mut admin, "GRANT EXECUTE ON SEQUENCE tickets TO clerk").unwrap_err().state(),
        SqlState::INVALID_GRANT_OPERATION
    );
    assert!(run(&mut admin, "GRANT USAGE ON SEQUENCE missing TO clerk").is_err());
    run(&mut admin, "DROP SEQUENCE hidden").unwrap();
    run(&mut admin, "CREATE SEQUENCE hidden").unwrap();
    assert!(run(&mut clerk, "SELECT setval('hidden', 5)").is_err());
}

#[test]
fn a_table_in_another_schema_needs_usage_on_that_schema() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    for sql in [
        "CREATE SCHEMA sales",
        "CREATE TABLE sales.orders (id integer)",
        "INSERT INTO sales.orders VALUES (7)",
        "GRANT SELECT ON sales.orders TO clerk",
    ] {
        run(&mut admin, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
    let mut clerk = Session::new(&store);
    clerk.identify_role("clerk", "eggdb").unwrap();
    let error = run(&mut clerk, "SELECT id FROM sales.orders").unwrap_err();
    assert_eq!(error.state(), SqlState::INSUFFICIENT_PRIVILEGE);
    assert!(error.to_string().contains("permission denied for schema sales"), "{error}");
    assert_eq!(
        run(&mut clerk, "SELECT has_schema_privilege('sales', 'USAGE') AS u, has_schema_privilege('public', 'USAGE') AS p").unwrap(),
        "u|p\nf|t\nSELECT 1"
    );
    run(&mut admin, "GRANT USAGE ON SCHEMA sales TO readers").unwrap();
    assert_eq!(run(&mut clerk, "SELECT id FROM sales.orders").unwrap(), "id\n7\nSELECT 1");
    run(&mut admin, "REVOKE USAGE ON SCHEMA sales FROM readers").unwrap();
    assert!(run(&mut clerk, "SELECT id FROM sales.orders").is_err());
    assert_eq!(run(&mut clerk, "SELECT owner FROM accounts").unwrap(), "owner\nada\nSELECT 1");
}

#[test]
fn routines_run_with_execute_which_public_holds_until_it_is_revoked() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    for sql in [
        "CREATE FUNCTION add_one(x integer) RETURNS integer LANGUAGE plpgsql AS $$ BEGIN RETURN x + 1; END $$",
        "CREATE PROCEDURE touch() LANGUAGE plpgsql AS $$ BEGIN NULL; END $$",
    ] {
        run(&mut admin, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
    let mut clerk = Session::new(&store);
    clerk.identify_role("clerk", "eggdb").unwrap();
    assert_eq!(run(&mut clerk, "SELECT add_one(1) AS n").unwrap(), "n\n2\nSELECT 1");
    run(&mut clerk, "CALL touch()").unwrap();
    run(&mut admin, "REVOKE EXECUTE ON FUNCTION add_one(integer) FROM PUBLIC").unwrap();
    run(&mut admin, "REVOKE EXECUTE ON PROCEDURE touch FROM PUBLIC").unwrap();
    for sql in ["SELECT add_one(1)", "CALL touch()"] {
        let error = run(&mut clerk, sql).expect_err(sql);
        assert_eq!(error.state(), SqlState::INSUFFICIENT_PRIVILEGE, "{sql}: {error}");
    }
    assert_eq!(
        run(&mut clerk, "SELECT has_function_privilege('add_one(integer)', 'EXECUTE') AS e").unwrap(),
        "e\nf\nSELECT 1"
    );
    run(&mut admin, "GRANT EXECUTE ON FUNCTION add_one TO readers").unwrap();
    assert_eq!(run(&mut clerk, "SELECT add_one(2) AS n").unwrap(), "n\n3\nSELECT 1");
    assert_eq!(run(&mut admin, "SELECT add_one(3) AS n").unwrap(), "n\n4\nSELECT 1");
    run(&mut admin, "GRANT EXECUTE ON PROCEDURE touch TO PUBLIC").unwrap();
    run(&mut clerk, "CALL touch()").unwrap();
}
