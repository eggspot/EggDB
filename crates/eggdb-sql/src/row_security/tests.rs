use super::*;
use crate::Session;
use crate::memory_store::MemoryStore;
use crate::tests::render;

fn run(session: &mut Session<'_>, sql: &str) -> SqlResult<String> {
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

fn as_role<'s>(store: &'s MemoryStore, user: &str) -> Session<'s> {
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
        run(
            &mut ada,
            "SELECT count(*) FROM notes n JOIN notes m ON n.id = m.id"
        )
        .unwrap(),
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
    assert_eq!(
        run(&mut ada, "SELECT id FROM notes").unwrap(),
        "id\nSELECT 0"
    );
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
        (
            "CREATE POLICY own_notes ON notes USING (true)",
            SqlState::DUPLICATE_OBJECT,
        ),
        (
            "CREATE POLICY p ON nowhere USING (true)",
            SqlState::UNDEFINED_TABLE,
        ),
        (
            "CREATE POLICY p ON notes TO nobody USING (true)",
            SqlState::UNDEFINED_OBJECT,
        ),
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

#[test]
fn only_a_superuser_reads_foreign_sources() {
    let store = MemoryStore::default();
    setup(&store);
    let mut ada = as_role(&store, "ada");
    assert_eq!(
        run(
            &mut ada,
            "SELECT * FROM eggdb_foreign('file:/x.parquet', 'x')"
        )
        .unwrap_err()
        .state(),
        SqlState::INSUFFICIENT_PRIVILEGE
    );
}

#[test]
fn only_a_superuser_turns_triggers_off() {
    let store = MemoryStore::default();
    setup(&store);
    let mut ada = as_role(&store, "ada");
    for sql in [
        "ALTER TABLE notes DISABLE TRIGGER ALL",
        "ALTER TABLE notes ENABLE TRIGGER ALL",
    ] {
        assert_eq!(
            run(&mut ada, sql).unwrap_err().state(),
            SqlState::INSUFFICIENT_PRIVILEGE,
            "{sql}"
        );
    }
}

#[test]
fn statements_inside_do_blocks_are_checked_like_top_level_ones() {
    let store = MemoryStore::default();
    setup(&store);
    let mut ada = as_role(&store, "ada");
    for sql in [
        "DO $$ BEGIN DROP TABLE notes; END $$",
        "DO $$ BEGIN ALTER TABLE notes DISABLE TRIGGER ALL; END $$",
        "DO $$ BEGIN DROP TRIGGER guard ON notes; END $$",
        "DO $$ BEGIN EXECUTE 'DROP TABLE notes'; END $$",
        "DO $$ BEGIN CREATE INDEX notes_owner ON notes (owner); END $$",
    ] {
        assert_eq!(
            run(&mut ada, sql).unwrap_err().state(),
            SqlState::INSUFFICIENT_PRIVILEGE,
            "{sql}"
        );
    }
    let mut admin = Session::new(&store);
    let counted = run(&mut admin, "SELECT count(*) FROM notes").unwrap();
    assert_eq!(counted.lines().nth(1), Some("3"), "{counted}");
}

fn reader_of_payroll(store: &MemoryStore, privileges: &str) {
    let mut admin = Session::new(store);
    for sql in [
        "CREATE TABLE payroll (id integer PRIMARY KEY, pay integer)".to_owned(),
        "INSERT INTO payroll VALUES (1, 10), (2, 20)".to_owned(),
        "CREATE USER clerk LOGIN".to_owned(),
        format!("GRANT {privileges} ON payroll TO clerk"),
    ] {
        run(&mut admin, &sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
}

#[test]
fn writes_inside_with_need_their_own_privilege() {
    let store = MemoryStore::default();
    setup(&store);
    reader_of_payroll(&store, "SELECT");
    let mut clerk = as_role(&store, "clerk");
    for sql in [
        "WITH d AS (DELETE FROM payroll RETURNING *) SELECT count(*) FROM d",
        "WITH u AS (UPDATE payroll SET pay = 0 RETURNING *) SELECT count(*) FROM u",
        "WITH x AS (SELECT 9 AS id) INSERT INTO payroll SELECT id, 0 FROM x",
    ] {
        assert_eq!(
            run(&mut clerk, sql).unwrap_err().state(),
            SqlState::INSUFFICIENT_PRIVILEGE,
            "{sql}"
        );
    }
    let summed = run(
        &mut clerk,
        "WITH x AS (SELECT pay FROM payroll) SELECT sum(pay) FROM x",
    )
    .unwrap();
    assert_eq!(summed.lines().nth(1), Some("30"), "{summed}");
}

#[test]
fn merge_needs_the_privilege_of_each_action() {
    let store = MemoryStore::default();
    setup(&store);
    reader_of_payroll(&store, "SELECT, UPDATE");
    let mut clerk = Session::with_dialect(&store, crate::Dialect::SqlServer);
    clerk.identify_role("clerk", "eggdb").unwrap();
    for sql in [
        "MERGE INTO payroll AS t USING (SELECT 3 AS id, 30 AS pay) AS s ON t.id = s.id WHEN NOT MATCHED THEN INSERT (id, pay) VALUES (s.id, s.pay);",
        "MERGE INTO payroll AS t USING (SELECT 1 AS id) AS s ON t.id = s.id WHEN MATCHED THEN DELETE;",
    ] {
        assert_eq!(
            run(&mut clerk, sql).unwrap_err().state(),
            SqlState::INSUFFICIENT_PRIVILEGE,
            "{sql}"
        );
    }
    run(
        &mut clerk,
        "MERGE INTO payroll AS t USING (SELECT 1 AS id, 11 AS pay) AS s ON t.id = s.id WHEN MATCHED THEN UPDATE SET pay = s.pay;",
    )
    .unwrap();
}

fn owned_documents(store: &MemoryStore, policies: &[&str]) {
    crate::memory_store::use_seeded_random_bytes();
    let mut admin = Session::new(store);
    let setup = [
        "CREATE TABLE docs (id integer PRIMARY KEY, owner text, body text)",
        "INSERT INTO docs VALUES (1, 'ada', 'a1'), (2, 'grace', 'g1')",
        "CREATE USER ada LOGIN",
        "CREATE USER grace LOGIN",
        "GRANT SELECT, INSERT, UPDATE, DELETE ON docs TO ada, grace",
        "ALTER TABLE docs ENABLE ROW LEVEL SECURITY",
    ];
    for sql in setup.iter().chain(policies) {
        run(&mut admin, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
}

fn violates(session: &mut Session<'_>, sql: &str) {
    let error = run(session, sql).expect_err(sql);
    assert_eq!(
        error.state(),
        SqlState::INSUFFICIENT_PRIVILEGE,
        "{sql}: {error}"
    );
    assert_eq!(
        error.message(),
        "new row violates row-level security policy for table \"docs\"",
        "{sql}"
    );
}

fn stored_rows(store: &MemoryStore) -> String {
    let mut admin = Session::new(store);
    run(&mut admin, "SELECT id, owner, body FROM docs ORDER BY id").unwrap()
}

#[test]
fn using_is_the_check_when_a_policy_has_no_with_check() {
    let store = MemoryStore::default();
    owned_documents(
        &store,
        &["CREATE POLICY own ON docs USING (owner = current_user)"],
    );
    let mut ada = as_role(&store, "ada");
    assert_eq!(
        run(&mut ada, "INSERT INTO docs VALUES (3, 'ada', 'a2')").unwrap(),
        "INSERT 0 1"
    );
    assert_eq!(
        run(
            &mut ada,
            "INSERT INTO docs VALUES (4, 'ada', 'a3') RETURNING id"
        )
        .unwrap(),
        "id\n4\nINSERT 0 1"
    );
    assert_eq!(
        run(&mut ada, "UPDATE docs SET body = 'edited' WHERE id = 1").unwrap(),
        "UPDATE 1"
    );
    let before = stored_rows(&store);
    for sql in [
        "INSERT INTO docs VALUES (5, 'grace', 'forged')",
        "INSERT INTO docs VALUES (5, NULL, 'unowned')",
        "INSERT INTO docs VALUES (5, 'ada', 'ok'), (6, 'grace', 'forged')",
        "INSERT INTO docs SELECT 7, 'grace', 'copied'",
        "UPDATE docs SET owner = 'grace' WHERE id = 1",
        "UPDATE docs SET owner = NULL",
        "UPDATE docs SET owner = 'grace' WHERE id = 1 RETURNING id",
        "WITH moved AS (UPDATE docs SET owner = 'grace' RETURNING id) SELECT count(*) FROM moved",
        "WITH added AS (INSERT INTO docs VALUES (8, 'grace', 'x') RETURNING id) SELECT * FROM added",
        "DO $$ BEGIN INSERT INTO docs VALUES (9, 'grace', 'x'); END $$",
        "DO $$ BEGIN UPDATE docs SET owner = 'grace'; END $$",
    ] {
        violates(&mut ada, sql);
    }
    for sql in [
        "INSERT INTO docs VALUES (10, 'grace', 'x') RETURNING true AS \"\u{2}row security check\"",
        "INSERT INTO docs VALUES (10, 'grace', 'x') RETURNING true AS \"\u{2}row security check alone\"",
    ] {
        assert!(run(&mut ada, sql).is_err(), "{sql}");
    }
    assert_eq!(stored_rows(&store), before);
}

#[test]
fn each_command_checks_its_own_policies_and_restrictive_ones_all_hold() {
    let store = MemoryStore::default();
    owned_documents(
        &store,
        &[
            "CREATE POLICY readable ON docs FOR SELECT USING (true)",
            "CREATE POLICY short ON docs FOR INSERT WITH CHECK (length(body) < 5)",
            "CREATE POLICY clean ON docs FOR UPDATE USING (true) WITH CHECK (body <> 'bad')",
            "CREATE POLICY mine ON docs AS RESTRICTIVE FOR ALL USING (owner = current_user)",
        ],
    );
    let mut ada = as_role(&store, "ada");
    assert_eq!(
        run(&mut ada, "INSERT INTO docs VALUES (3, 'ada', 'ok')").unwrap(),
        "INSERT 0 1"
    );
    assert_eq!(
        run(
            &mut ada,
            "UPDATE docs SET body = 'long enough' WHERE id = 3"
        )
        .unwrap(),
        "UPDATE 1"
    );
    for sql in [
        "INSERT INTO docs VALUES (4, 'ada', 'too long')",
        "INSERT INTO docs VALUES (4, 'grace', 'ok')",
        "UPDATE docs SET body = 'bad' WHERE id = 3",
        "UPDATE docs SET owner = 'grace' WHERE id = 3",
    ] {
        violates(&mut ada, sql);
    }
    assert_eq!(
        run(&mut ada, "UPDATE docs SET body = 'x' WHERE id = 2").unwrap(),
        "UPDATE 0"
    );
}

#[test]
fn a_before_trigger_cannot_move_a_new_row_out_of_the_policy() {
    let store = MemoryStore::default();
    owned_documents(
        &store,
        &[
            "CREATE POLICY own ON docs USING (owner = current_user)",
            "CREATE FUNCTION give_away() RETURNS trigger AS $$ BEGIN NEW.owner := 'grace'; RETURN NEW; END $$ LANGUAGE plpgsql",
            "CREATE TRIGGER give_away BEFORE INSERT ON docs FOR EACH ROW EXECUTE FUNCTION give_away()",
        ],
    );
    let mut ada = as_role(&store, "ada");
    violates(&mut ada, "INSERT INTO docs VALUES (3, 'ada', 'a2')");
    assert!(!stored_rows(&store).contains("a2"));
}

#[test]
fn returned_new_rows_must_also_be_visible() {
    let store = MemoryStore::default();
    owned_documents(
        &store,
        &[
            "CREATE POLICY anyone_writes ON docs FOR INSERT WITH CHECK (true)",
            "CREATE POLICY own_reads ON docs FOR SELECT USING (owner = current_user)",
        ],
    );
    let mut ada = as_role(&store, "ada");
    assert_eq!(
        run(&mut ada, "INSERT INTO docs VALUES (3, 'grace', 'gift')").unwrap(),
        "INSERT 0 1"
    );
    violates(
        &mut ada,
        "INSERT INTO docs VALUES (4, 'grace', 'peek') RETURNING body",
    );
    assert_eq!(
        run(
            &mut ada,
            "INSERT INTO docs VALUES (5, 'ada', 'mine') RETURNING body"
        )
        .unwrap(),
        "body\nmine\nINSERT 0 1"
    );
}

#[test]
fn a_joined_table_with_row_security_joins_only_the_visible_rows() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    for sql in [
        "CREATE TABLE marks (id integer PRIMARY KEY, note text)",
        "INSERT INTO marks VALUES (1, 'm1'), (2, 'm2'), (3, 'm3')",
        "GRANT SELECT, UPDATE, DELETE ON marks TO ada",
    ] {
        run(&mut admin, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
    let mut ada = as_role(&store, "ada");
    assert_eq!(
        run(
            &mut ada,
            "UPDATE marks SET note = n.body FROM notes AS n WHERE marks.id = n.id"
        )
        .unwrap(),
        "UPDATE 2"
    );
    assert_eq!(
        run(
            &mut ada,
            "DELETE FROM marks USING notes WHERE marks.id = notes.id AND notes.body = 'g1'"
        )
        .unwrap(),
        "DELETE 0"
    );
    let refused = run(
        &mut ada,
        "DELETE FROM notes USING marks WHERE notes.id = marks.id",
    )
    .expect_err("a joined DELETE of a table with row-level security");
    assert_eq!(refused.state(), SqlState::FEATURE_NOT_SUPPORTED);
    assert!(
        refused.message().contains("row-level security"),
        "{refused}"
    );
    assert_eq!(
        run(&mut admin, "SELECT id, note FROM marks ORDER BY id").unwrap(),
        "id|note\n1|a1\n2|m2\n3|a2\nSELECT 3"
    );
    assert_eq!(
        run(&mut admin, "SELECT count(*) FROM notes").unwrap(),
        "count\n3\nSELECT 1"
    );
}

fn switches(session: &mut Session<'_>) -> String {
    run(
        session,
        "SELECT c.relrowsecurity, c.relforcerowsecurity, t.rowsecurity FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_tables t ON t.tablename = c.relname WHERE c.relname = 'notes'",
    )
    .unwrap()
}

#[test]
fn force_row_level_security_is_stored_beside_enable_as_postgresql_shows_it() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    let header = "relrowsecurity|relforcerowsecurity|rowsecurity";
    assert_eq!(switches(&mut admin), format!("{header}\nt|f|t\nSELECT 1"));
    for (sql, expected) in [
        ("ALTER TABLE notes FORCE ROW LEVEL SECURITY", "t|t|t"),
        ("ALTER TABLE notes NO FORCE ROW LEVEL SECURITY", "t|f|t"),
        ("ALTER TABLE notes DISABLE ROW LEVEL SECURITY", "f|f|f"),
        ("ALTER TABLE ONLY notes FORCE ROW LEVEL SECURITY", "f|t|f"),
        ("ALTER TABLE notes ENABLE ROW LEVEL SECURITY", "t|t|t"),
        ("ALTER TABLE notes DISABLE ROW LEVEL SECURITY", "f|t|f"),
        ("ALTER TABLE notes NO FORCE ROW LEVEL SECURITY", "f|f|f"),
    ] {
        assert_eq!(run(&mut admin, sql).unwrap(), "ALTER TABLE", "{sql}");
        assert_eq!(
            switches(&mut admin),
            format!("{header}\n{expected}\nSELECT 1"),
            "{sql}"
        );
    }
    let reader = crate::KeyValueStore::begin_read(&store).unwrap();
    assert_eq!(reader.get(&switch_key("notes")).unwrap(), None);
}

#[test]
fn forcing_row_security_never_lets_a_checked_role_past_its_policies() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    run(&mut admin, "ALTER TABLE notes FORCE ROW LEVEL SECURITY").unwrap();
    let mut ada = as_role(&store, "ada");
    assert_eq!(
        run(&mut ada, "SELECT id FROM notes ORDER BY id").unwrap(),
        "id\n1\n3\nSELECT 2"
    );
    for sql in [
        "ALTER TABLE notes NO FORCE ROW LEVEL SECURITY",
        "ALTER TABLE notes FORCE ROW LEVEL SECURITY",
    ] {
        let error = run(&mut ada, sql).expect_err(sql);
        assert_eq!(error.state(), SqlState::INSUFFICIENT_PRIVILEGE, "{sql}");
    }
    assert_eq!(
        run(&mut admin, "SELECT count(*) FROM notes").unwrap(),
        "count\n3\nSELECT 1"
    );
    let error = run(&mut admin, "ALTER TABLE missing FORCE ROW LEVEL SECURITY").unwrap_err();
    assert_eq!(error.state(), SqlState::UNDEFINED_TABLE);
}

#[test]
fn the_force_flag_keeps_enable_readable_by_releases_that_know_only_enable() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    run(&mut admin, "ALTER TABLE notes FORCE ROW LEVEL SECURITY").unwrap();
    let reader = crate::KeyValueStore::begin_read(&store).unwrap();
    assert_eq!(
        reader.get(&switch_key("notes")).unwrap(),
        Some(vec![RECORD_VERSION, ENABLED | FORCED])
    );
    assert!(is_enabled(&reader, "notes").unwrap());
    drop(reader);
    run(&mut admin, "ALTER TABLE notes DISABLE ROW LEVEL SECURITY").unwrap();
    let reader = crate::KeyValueStore::begin_read(&store).unwrap();
    assert_eq!(
        reader.get(&switch_key("notes")).unwrap(),
        Some(vec![RECORD_VERSION, FORCED])
    );
    assert!(!is_enabled(&reader, "notes").unwrap());
    assert_eq!(
        switch_state(&reader, "notes").unwrap(),
        RowSecuritySwitch {
            enabled: false,
            forced: true
        }
    );
}

#[test]
fn a_damaged_switch_record_refuses_to_change_and_reads_as_an_error() {
    for value in [&[][..], &[9, ENABLED][..], &[RECORD_VERSION][..]] {
        let store = MemoryStore::default();
        setup(&store);
        let mut writer = crate::KeyValueStore::begin_write(&store).unwrap();
        writer.put(&switch_key("notes"), value).unwrap();
        writer.commit().unwrap();
        let reader = crate::KeyValueStore::begin_read(&store).unwrap();
        assert!(switch_state(&reader, "notes").is_err(), "{value:?}");
        drop(reader);
        let mut admin = Session::new(&store);
        assert!(
            run(&mut admin, "ALTER TABLE notes FORCE ROW LEVEL SECURITY").is_err(),
            "{value:?}"
        );
        let mut ada = as_role(&store, "ada");
        assert!(run(&mut ada, "SELECT id FROM notes").is_err(), "{value:?}");
    }
}

#[test]
fn resetting_the_session_authorization_keeps_the_checked_role_and_its_policies() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    run(
        &mut admin,
        "INSERT INTO notes VALUES (4, 'eggdb', 'admin secret')",
    )
    .unwrap();
    let mut grace = as_role(&store, "grace");
    for sql in [
        "RESET session_authorization",
        "SET session_authorization TO DEFAULT",
        "SET session_authorization = DEFAULT",
        "SET LOCAL session_authorization TO DEFAULT",
        "RESET SESSION AUTHORIZATION",
        "SET SESSION AUTHORIZATION DEFAULT",
        "RESET ALL",
        "RESET is_superuser",
    ] {
        run(&mut grace, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        assert_eq!(
            run(
                &mut grace,
                "SELECT current_user AS who, session_user AS session, \
                 current_setting('is_superuser') AS superuser"
            )
            .unwrap(),
            "who|session|superuser\ngrace|grace|off\nSELECT 1",
            "{sql}"
        );
        assert_eq!(
            run(&mut grace, "SELECT id, body FROM notes ORDER BY id").unwrap(),
            "id|body\n2|g1\nSELECT 1",
            "{sql}"
        );
    }
    let error = run(&mut grace, "SET session_authorization = 'eggdb'").unwrap_err();
    assert_eq!(
        error.state(),
        SqlState::CANT_CHANGE_RUNTIME_PARAM,
        "{error}"
    );
    let mut boss = Session::new(&store);
    run(&mut boss, "RESET session_authorization").unwrap();
    assert_eq!(
        run(&mut boss, "SHOW is_superuser").unwrap(),
        "is_superuser\non\nSHOW"
    );
}

fn reader_of_pub(store: &MemoryStore) {
    let mut admin = Session::new(store);
    for sql in [
        "CREATE TABLE pub (id integer PRIMARY KEY)",
        "CREATE TABLE secret (id integer PRIMARY KEY)",
        "INSERT INTO pub VALUES (1), (2)",
        "INSERT INTO secret VALUES (2)",
        "CREATE USER reader LOGIN",
        "GRANT SELECT ON pub TO reader",
    ] {
        run(&mut admin, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
}

#[test]
fn table_subqueries_and_executed_statements_check_privileges() {
    let store = MemoryStore::default();
    reader_of_pub(&store);
    let mut reader = as_role(&store, "reader");
    run(
        &mut reader,
        "PREPARE p AS SELECT * FROM pub WHERE id IN (SELECT id FROM secret)",
    )
    .unwrap();
    for sql in [
        "SELECT (TABLE secret)",
        "SELECT * FROM pub WHERE id IN (TABLE secret)",
        "SELECT EXISTS (TABLE secret)",
        "EXECUTE p",
    ] {
        let error = run(&mut reader, sql).unwrap_err();
        assert_eq!(
            error.state(),
            SqlState::INSUFFICIENT_PRIVILEGE,
            "{sql}: {error}"
        );
    }
    assert_eq!(
        run(&mut reader, "SELECT * FROM pub WHERE id IN (TABLE pub)").unwrap(),
        "id\n1\n2\nSELECT 2"
    );
}

#[test]
fn row_security_filters_table_subqueries_and_executed_statements() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    for sql in [
        "CREATE USER nobody LOGIN",
        "GRANT SELECT ON notes TO nobody",
    ] {
        run(&mut admin, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
    let mut nobody = as_role(&store, "nobody");
    assert_eq!(
        run(&mut nobody, "SELECT EXISTS (TABLE notes)").unwrap(),
        run(&mut nobody, "SELECT EXISTS (SELECT * FROM notes)").unwrap()
    );
    let mut ada = as_role(&store, "ada");
    run(
        &mut ada,
        "PREPARE counted AS SELECT (SELECT count(*) FROM notes) AS visible",
    )
    .unwrap();
    assert_eq!(
        run(&mut ada, "EXECUTE counted").unwrap(),
        "visible\n2\nSELECT 1"
    );
}
