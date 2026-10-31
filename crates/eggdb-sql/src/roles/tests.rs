use super::*;
use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{KeyValueStore, Session};

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> SqlResult<String> {
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
        assert_eq!(error.state(), SqlState::INSUFFICIENT_PRIVILEGE, "{sql}: {error}");
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
    assert_eq!((iterations, salt.len()), (SCRAM_ITERATIONS, SCRAM_SALT_BYTES));
    assert_eq!(login_verifier(&reader, "readers").unwrap(), None);
    assert_eq!(login_verifier(&reader, "clerk").unwrap(), Some(verifier));
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
        ("GRANT SELECT ON nowhere TO clerk", SqlState::UNDEFINED_TABLE),
        ("ALTER ROLE nobody LOGIN", SqlState::UNDEFINED_OBJECT),
    ] {
        let error = run(&mut admin, sql).expect_err(sql);
        assert_eq!(error.state(), state, "{sql}: {error}");
    }
    run(&mut admin, "CREATE ROLE IF NOT EXISTS clerk").unwrap();
    run(&mut admin, "DROP ROLE IF EXISTS nobody").unwrap();
    let mut nologin = Session::new(&store);
    assert_eq!(
        nologin.identify_role("readers", "eggdb").unwrap_err().state(),
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
