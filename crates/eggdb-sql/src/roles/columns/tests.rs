use super::super::{RECORD_VERSION, every_grant};
use super::*;
use crate::error::SqlState;
use crate::memory_store::MemoryStore;
use crate::store::KeyValueWrite;
use crate::tests::render;
use crate::{KeyValueStore, Session};

fn run(session: &mut Session<'_>, sql: &str) -> SqlResult<String> {
    let mut last = String::new();
    session.execute(sql, &mut |result| last = render(&result))?;
    Ok(last)
}

fn admin_runs(store: &MemoryStore, statements: &[&str]) {
    let mut admin = Session::new(store);
    for sql in statements {
        run(&mut admin, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
}

fn setup(store: &MemoryStore) {
    crate::memory_store::use_seeded_random_bytes();
    admin_runs(
        store,
        &[
            "CREATE TABLE staff (id integer PRIMARY KEY, name text, salary integer, notes text)",
            "INSERT INTO staff VALUES (1, 'ada', 100, 'n1'), (2, 'lin', 200, 'n2')",
            "CREATE TABLE teams (id integer, salary integer)",
            "INSERT INTO teams VALUES (1, 7)",
            "CREATE USER clerk LOGIN",
            "CREATE USER boss LOGIN",
            "GRANT SELECT (id, name) ON staff TO clerk",
            "GRANT INSERT (id, name), UPDATE (name) ON staff TO clerk",
            "GRANT SELECT ON teams TO clerk",
            "GRANT SELECT, INSERT, UPDATE, DELETE ON staff TO boss",
            "CREATE VIEW payroll AS SELECT id, salary FROM staff",
        ],
    );
}

fn as_role<'s>(store: &'s MemoryStore, user: &str) -> Session<'s> {
    let mut session = Session::new(store);
    session.identify_role(user, "eggdb").unwrap();
    session
}

fn denied(session: &mut Session<'_>, sql: &str) -> String {
    let error = run(session, sql).expect_err(sql);
    assert_eq!(
        error.state(),
        SqlState::INSUFFICIENT_PRIVILEGE,
        "{sql}: {error}"
    );
    error.message().to_owned()
}

#[test]
fn granted_columns_read_and_write_as_postgresql_allows() {
    let store = MemoryStore::default();
    setup(&store);
    let mut clerk = as_role(&store, "clerk");
    for (sql, expected) in [
        (
            "SELECT id, name FROM staff ORDER BY id",
            "id|name\n1|ada\n2|lin\nSELECT 2",
        ),
        ("SELECT count(*) FROM staff", "count\n2\nSELECT 1"),
        (
            "SELECT s.name FROM staff s WHERE s.id = 2",
            "name\nlin\nSELECT 1",
        ),
        (
            "SELECT name FROM staff WHERE id IN (SELECT id FROM staff WHERE name = 'ada')",
            "name\nada\nSELECT 1",
        ),
        (
            "SELECT s.name, t.salary FROM staff s JOIN teams t ON s.id = t.id",
            "name|salary\nada|7\nSELECT 1",
        ),
        (
            "INSERT INTO staff (id, name) VALUES (3, 'kim')",
            "INSERT 0 1",
        ),
        ("UPDATE staff SET name = 'kim2' WHERE id = 3", "UPDATE 1"),
        (
            "INSERT INTO staff (id, name) VALUES (4, 'joe') RETURNING id, name",
            "id|name\n4|joe\nINSERT 0 1",
        ),
        (
            "UPDATE staff SET name = upper(name) WHERE id = 4 RETURNING name",
            "name\nJOE\nUPDATE 1",
        ),
    ] {
        assert_eq!(run(&mut clerk, sql).unwrap(), expected, "{sql}");
    }
}

#[test]
fn every_way_of_reaching_an_ungranted_column_is_denied() {
    let store = MemoryStore::default();
    setup(&store);
    let mut clerk = as_role(&store, "clerk");
    for sql in [
        "SELECT salary FROM staff",
        "SELECT * FROM staff",
        "SELECT staff.* FROM staff",
        "SELECT s.* FROM staff s",
        "SELECT id FROM staff WHERE salary > 150",
        "SELECT id FROM staff ORDER BY salary",
        "SELECT name FROM staff ORDER BY notes DESC",
        "SELECT id FROM staff GROUP BY id HAVING max(salary) > 0",
        "SELECT sum(salary) FROM staff",
        "SELECT length(notes) FROM staff",
        "SELECT CASE WHEN salary > 150 THEN 1 ELSE 0 END FROM staff",
        "SELECT row_to_json(staff) FROM staff",
        "SELECT staff FROM staff",
        "SELECT rank() OVER (ORDER BY salary) FROM staff",
        "SELECT id FROM staff WHERE EXISTS (SELECT 1 FROM teams WHERE teams.id = staff.id AND staff.salary > 0)",
        "SELECT (SELECT max(salary) FROM staff) FROM teams",
        "SELECT id, (SELECT notes) FROM staff",
        "SELECT x.salary FROM (SELECT * FROM staff) x",
        "SELECT x FROM (SELECT salary AS x FROM staff) d",
        "WITH c AS (SELECT salary FROM staff) SELECT * FROM c",
        "SELECT id FROM staff UNION SELECT salary FROM staff",
        "SELECT s.id FROM staff s JOIN teams t USING (salary)",
        "SELECT s.id FROM staff s JOIN teams t ON s.salary = t.salary",
        "SELECT t.id FROM teams t WHERE t.salary IN (SELECT salary FROM staff)",
        "SELECT id FROM staff LIMIT (SELECT max(salary) FROM staff)",
        "INSERT INTO staff (id, salary) VALUES (5, 1)",
        "INSERT INTO staff VALUES (5, 'x', 1, 'y')",
        "INSERT INTO staff SELECT * FROM staff",
        "INSERT INTO staff (id, name) VALUES (6, 'x') RETURNING salary",
        "INSERT INTO staff (id, name) VALUES (6, 'x') RETURNING *",
        "INSERT INTO staff (id, name) VALUES (1, 'x') ON CONFLICT (id) DO UPDATE SET salary = 0",
        "INSERT INTO staff (id, name) VALUES (1, 'x') ON CONFLICT (id) DO UPDATE SET name = staff.notes",
        "UPDATE staff SET salary = 0",
        "UPDATE staff SET name = 'x' WHERE salary > 150",
        "UPDATE staff SET name = notes",
        "UPDATE staff SET name = 'x' RETURNING salary",
        "UPDATE staff SET name = 'x' RETURNING *",
        "DELETE FROM staff WHERE id = 1",
        "SELECT * FROM payroll",
        "COPY staff TO STDOUT",
        "COPY staff (id, salary) TO STDOUT",
        "MERGE INTO staff USING teams t ON staff.id = t.id WHEN MATCHED THEN UPDATE SET name = 'x'",
    ] {
        let message = denied(&mut clerk, sql);
        assert!(
            message.starts_with("permission denied for "),
            "{sql}: {message}"
        );
    }
    assert_eq!(
        denied(&mut clerk, "SELECT salary FROM staff"),
        "permission denied for table staff"
    );
}

#[test]
fn table_level_privileges_still_cover_every_column() {
    let store = MemoryStore::default();
    setup(&store);
    let mut boss = as_role(&store, "boss");
    assert_eq!(
        run(&mut boss, "SELECT salary FROM staff WHERE id = 1").unwrap(),
        "salary\n100\nSELECT 1"
    );
    assert_eq!(
        run(
            &mut boss,
            "UPDATE staff SET salary = salary + 1 WHERE id = 1 RETURNING salary"
        )
        .unwrap(),
        "salary\n101\nUPDATE 1"
    );
    assert_eq!(
        run(&mut boss, "DELETE FROM staff WHERE id = 2").unwrap(),
        "DELETE 1"
    );
}

#[test]
fn a_write_without_select_may_not_read_through_where_or_returning() {
    let store = MemoryStore::default();
    setup(&store);
    admin_runs(
        &store,
        &[
            "CREATE USER writer LOGIN",
            "GRANT INSERT, UPDATE, DELETE ON staff TO writer",
        ],
    );
    let mut writer = as_role(&store, "writer");
    assert_eq!(
        run(&mut writer, "INSERT INTO staff VALUES (7, 'w', 1, 'z')").unwrap(),
        "INSERT 0 1"
    );
    assert_eq!(
        run(&mut writer, "UPDATE staff SET notes = 'all'").unwrap(),
        "UPDATE 3"
    );
    for sql in [
        "INSERT INTO staff VALUES (8, 'w', 1, 'z') RETURNING id",
        "UPDATE staff SET notes = 'x' WHERE salary = 100",
        "UPDATE staff SET salary = salary * 2",
        "DELETE FROM staff WHERE salary = 100",
        "DELETE FROM staff RETURNING *",
        "SELECT id FROM staff",
    ] {
        denied(&mut writer, sql);
    }
    assert_eq!(run(&mut writer, "DELETE FROM staff").unwrap(), "DELETE 3");
}

#[test]
fn revoking_takes_column_privileges_away() {
    let store = MemoryStore::default();
    setup(&store);
    admin_runs(&store, &["REVOKE SELECT (name) ON staff FROM clerk"]);
    let mut clerk = as_role(&store, "clerk");
    assert_eq!(
        run(&mut clerk, "SELECT id FROM staff WHERE id = 1").unwrap(),
        "id\n1\nSELECT 1"
    );
    denied(&mut clerk, "SELECT name FROM staff");
    admin_runs(&store, &["REVOKE SELECT ON staff FROM clerk"]);
    let mut clerk = as_role(&store, "clerk");
    denied(&mut clerk, "SELECT id FROM staff");
    denied(&mut clerk, "SELECT count(*) FROM staff");
    admin_runs(&store, &["GRANT SELECT (salary) ON staff TO PUBLIC"]);
    let mut clerk = as_role(&store, "clerk");
    assert_eq!(
        run(&mut clerk, "SELECT salary FROM staff WHERE salary = 200").unwrap(),
        "salary\n200\nSELECT 1"
    );
}

#[test]
fn column_grants_follow_renames_and_go_with_drops() {
    let store = MemoryStore::default();
    setup(&store);
    admin_runs(
        &store,
        &[
            "DROP VIEW payroll",
            "ALTER TABLE staff RENAME COLUMN name TO full_name",
            "ALTER TABLE staff RENAME TO people",
        ],
    );
    let mut clerk = as_role(&store, "clerk");
    assert_eq!(
        run(&mut clerk, "SELECT full_name FROM people WHERE id = 1").unwrap(),
        "full_name\nada\nSELECT 1"
    );
    denied(&mut clerk, "SELECT salary FROM people");
    admin_runs(
        &store,
        &[
            "ALTER TABLE people DROP COLUMN full_name",
            "CREATE TABLE staff (id integer, name text)",
        ],
    );
    let mut clerk = as_role(&store, "clerk");
    denied(&mut clerk, "SELECT name FROM staff");
    let reader = store.begin_read().unwrap();
    let grants = every_column_grant(&reader).unwrap();
    assert!(
        grants.iter().all(|grant| grant.table != "staff"),
        "{grants:?}"
    );
    assert!(
        grants.iter().all(|grant| grant.column != "full_name"),
        "{grants:?}"
    );
}

#[test]
fn grant_on_columns_refuses_what_postgresql_refuses() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    for (sql, state) in [
        (
            "GRANT SELECT (nope) ON staff TO clerk",
            SqlState::UNDEFINED_COLUMN,
        ),
        (
            "GRANT DELETE (id) ON staff TO clerk",
            SqlState::INVALID_GRANT_OPERATION,
        ),
        (
            "GRANT TRUNCATE (id) ON staff TO clerk",
            SqlState::INVALID_GRANT_OPERATION,
        ),
        (
            "GRANT SELECT (id) ON nowhere TO clerk",
            SqlState::UNDEFINED_TABLE,
        ),
        (
            "GRANT SELECT (id) ON staff TO nobody",
            SqlState::UNDEFINED_OBJECT,
        ),
        ("DROP ROLE clerk", SqlState::DEPENDENT_OBJECTS_STILL_EXIST),
    ] {
        let error = run(&mut admin, sql).expect_err(sql);
        assert_eq!(error.state(), state, "{sql}: {error}");
    }
    let mut clerk = as_role(&store, "clerk");
    denied(&mut clerk, "GRANT SELECT (salary) ON staff TO clerk");
}

#[test]
fn the_catalog_lists_column_grants() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    assert_eq!(
        run(
            &mut admin,
            "SELECT attname, attacl FROM pg_catalog.pg_attribute \
             WHERE attrelid = 'staff'::regclass AND attnum > 0 ORDER BY attnum"
        )
        .unwrap(),
        "attname|attacl\nid|{clerk=ar/eggdb}\nname|{clerk=arw/eggdb}\nsalary|NULL\nnotes|NULL\nSELECT 4"
    );
    assert_eq!(
        run(
            &mut admin,
            "SELECT grantee, column_name, privilege_type FROM information_schema.column_privileges \
             WHERE table_name = 'staff' AND grantee = 'clerk' ORDER BY column_name, privilege_type"
        )
        .unwrap(),
        "grantee|column_name|privilege_type\nclerk|id|INSERT\nclerk|id|SELECT\nclerk|name|INSERT\nclerk|name|SELECT\nclerk|name|UPDATE\nSELECT 5"
    );
    assert_eq!(
        run(
            &mut admin,
            "SELECT count(*) FROM information_schema.column_privileges \
             WHERE table_name = 'staff' AND grantee = 'boss'"
        )
        .unwrap(),
        "count\n12\nSELECT 1"
    );
}

#[test]
fn files_without_column_grants_open_unchanged_and_older_readers_skip_them() {
    let store = MemoryStore::default();
    setup(&store);
    let reader = store.begin_read().unwrap();
    let table_grants = every_grant(&reader).unwrap();
    assert!(
        table_grants
            .iter()
            .all(|grant| !grant.grantee.contains('\0')),
        "{table_grants:?}"
    );
    assert_eq!(
        reader
            .get(&grant_key("staff", "boss"))
            .unwrap()
            .as_deref()
            .map(<[u8]>::len),
        Some(2)
    );
    let stored = reader
        .get(&column_grant_key("staff", "clerk", "name"))
        .unwrap()
        .unwrap();
    assert_eq!(stored.first(), Some(&COLUMN_RECORD_VERSION));
    assert_ne!(COLUMN_RECORD_VERSION, RECORD_VERSION);
    drop(reader);

    let old = MemoryStore::default();
    crate::memory_store::use_seeded_random_bytes();
    admin_runs(
        &old,
        &[
            "CREATE TABLE staff (id integer, salary integer)",
            "INSERT INTO staff VALUES (1, 5)",
            "CREATE USER clerk LOGIN",
        ],
    );
    let mut writer = old.begin_write().unwrap();
    writer
        .put(
            &grant_key("staff", "clerk"),
            &[RECORD_VERSION, Privilege::Select.bit()],
        )
        .unwrap();
    writer.commit().unwrap();
    let mut clerk = as_role(&old, "clerk");
    assert_eq!(
        run(&mut clerk, "SELECT * FROM staff").unwrap(),
        "id|salary\n1|5\nSELECT 1"
    );
    let reader = old.begin_read().unwrap();
    assert!(every_column_grant(&reader).unwrap().is_empty());
}

#[test]
fn damaged_column_grants_fail_closed() {
    for value in [
        &[][..],
        &[COLUMN_RECORD_VERSION][..],
        &[9, 1][..],
        &[COLUMN_RECORD_VERSION, 1, 1][..],
    ] {
        let store = MemoryStore::default();
        setup(&store);
        let mut writer = store.begin_write().unwrap();
        writer
            .put(&column_grant_key("staff", "clerk", "salary"), value)
            .unwrap();
        writer.commit().unwrap();
        let mut clerk = as_role(&store, "clerk");
        assert!(
            run(&mut clerk, "SELECT salary FROM staff").is_err(),
            "{value:?}"
        );
        assert!(
            run(&mut clerk, "SELECT id FROM staff").is_err(),
            "{value:?}"
        );
    }
}
