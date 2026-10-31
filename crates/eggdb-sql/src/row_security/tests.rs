use super::*;
use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::Session;

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> SqlResult<String> {
    let mut last = String::new();
    session.execute(sql, &mut |result| last = render(&result))?;
    Ok(last)
}

fn setup(store: &MemoryStore) {
    crate::memory_store::use_seeded_random_bytes();
    let mut admin = Session::new(store);
    for sql in [
        "CREATE TABLE notes (id integer PRIMARY KEY, owner text, body text)",
        "INSERT INTO notes VALUES (1, 'ada', 'a1'), (2, 'grace', 'g1'), (3, 'ada', 'a2')",
        "CREATE USER ada LOGIN",
        "CREATE USER grace LOGIN",
        "GRANT SELECT, INSERT, UPDATE, DELETE ON notes TO ada, grace",
        "ALTER TABLE notes ENABLE ROW LEVEL SECURITY",
        "CREATE POLICY own_notes ON notes FOR ALL TO PUBLIC USING (owner = current_user) WITH CHECK (true)",
    ] {
        run(&mut admin, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
}

fn as_role<'s>(store: &'s MemoryStore, user: &str) -> Session<'s, MemoryStore> {
    let mut session = Session::new(store);
    session.identify_role(user, "eggdb").unwrap();
    session
}

#[test]
fn each_role_sees_and_changes_only_its_own_rows() {
    let store = MemoryStore::default();
    setup(&store);
    let mut ada = as_role(&store, "ada");
    assert_eq!(
        run(&mut ada, "SELECT id FROM notes ORDER BY id").unwrap(),
        "id\n1\n3\nSELECT 2"
    );
    assert_eq!(
        run(&mut ada, "SELECT count(*) FROM notes n JOIN notes m ON n.id = m.id").unwrap(),
        "count\n2\nSELECT 1"
    );
    assert_eq!(run(&mut ada, "DELETE FROM notes").unwrap(), "DELETE 2");
    let mut grace = as_role(&store, "grace");
    assert_eq!(
        run(&mut grace, "UPDATE notes SET body = 'changed'").unwrap(),
        "UPDATE 1"
    );
    let mut admin = Session::new(&store);
    assert_eq!(
        run(&mut admin, "SELECT id, body FROM notes ORDER BY id").unwrap(),
        "id|body\n2|changed\nSELECT 1"
    );
}

#[test]
fn without_a_matching_policy_a_role_sees_nothing_and_cannot_insert() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    run(&mut admin, "DROP POLICY own_notes ON notes").unwrap();
    run(
        &mut admin,
        "CREATE POLICY readers ON notes FOR SELECT TO grace USING (true)",
    )
    .unwrap();
    let mut ada = as_role(&store, "ada");
    assert_eq!(run(&mut ada, "SELECT id FROM notes").unwrap(), "id\nSELECT 0");
    let error = run(&mut ada, "INSERT INTO notes VALUES (9, 'ada', 'x')").unwrap_err();
    assert_eq!(error.state(), SqlState::INSUFFICIENT_PRIVILEGE);
    let mut grace = as_role(&store, "grace");
    assert_eq!(
        run(&mut grace, "SELECT count(*) FROM notes").unwrap(),
        "count\n3\nSELECT 1"
    );
    let error = run(&mut grace, "COPY notes TO STDOUT").unwrap_err();
    assert_eq!(error.state(), SqlState::FEATURE_NOT_SUPPORTED);
}

#[test]
fn superusers_bypass_row_security_and_only_they_manage_it() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    assert_eq!(
        run(&mut admin, "SELECT count(*) FROM notes").unwrap(),
        "count\n3\nSELECT 1"
    );
    let mut ada = as_role(&store, "ada");
    for sql in [
        "ALTER TABLE notes DISABLE ROW LEVEL SECURITY",
        "DROP POLICY own_notes ON notes",
        "CREATE POLICY mine ON notes USING (true)",
    ] {
        assert_eq!(
            run(&mut ada, sql).unwrap_err().state(),
            SqlState::INSUFFICIENT_PRIVILEGE,
            "{sql}"
        );
    }
    run(&mut admin, "ALTER TABLE notes DISABLE ROW LEVEL SECURITY").unwrap();
    let mut ada = as_role(&store, "ada");
    assert_eq!(
        run(&mut ada, "SELECT count(*) FROM notes").unwrap(),
        "count\n3\nSELECT 1"
    );
}

#[test]
fn policy_statements_refuse_what_postgresql_refuses() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    for (sql, state) in [
        ("CREATE POLICY own_notes ON notes USING (true)", SqlState::DUPLICATE_OBJECT),
        ("CREATE POLICY p ON nowhere USING (true)", SqlState::UNDEFINED_TABLE),
        ("CREATE POLICY p ON notes TO nobody USING (true)", SqlState::UNDEFINED_OBJECT),
        ("DROP POLICY missing ON notes", SqlState::UNDEFINED_OBJECT),
    ] {
        assert_eq!(run(&mut admin, sql).unwrap_err().state(), state, "{sql}");
    }
    run(&mut admin, "DROP POLICY IF EXISTS missing ON notes").unwrap();
    run(&mut admin, "DROP TABLE notes").unwrap();
    run(&mut admin, "CREATE TABLE notes (id integer)").unwrap();
    let reader = crate::KeyValueStore::begin_read(&store).unwrap();
    assert!(policies(&reader, "notes").unwrap().is_empty());
    assert!(!is_enabled(&reader, "notes").unwrap());
}

#[test]
fn hostile_policy_records_are_errors() {
    for bytes in [
        &[][..],
        &[RECORD_VERSION, 9, 1, 0, 0, 0, 0, 0, 0],
        &[RECORD_VERSION, 0, 1, 255, 255, 255, 255],
        &[RECORD_VERSION, 0, 1, 0, 0, 0, 0, 2, 0],
        &[RECORD_VERSION, 0, 1, 0, 0, 0, 0, 1, 9, 0, 0, 0, b'x', 0],
    ] {
        assert!(decode("p", bytes).is_err(), "{bytes:?}");
    }
}
