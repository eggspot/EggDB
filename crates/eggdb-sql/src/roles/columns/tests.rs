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
    let mut oracle_clerk = Session::with_dialect(&store, crate::Dialect::Oracle);
    oracle_clerk.identify_role("clerk", "eggdb").unwrap();
    denied(
        &mut oracle_clerk,
        "MERGE INTO staff USING teams t ON (staff.id = t.id) WHEN MATCHED THEN UPDATE SET name = 'x'",
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

fn policy_names(store: &MemoryStore, table: &str) -> Vec<String> {
    let reader = store.begin_read().unwrap();
    crate::row_security::policies(&reader, table)
        .unwrap()
        .into_iter()
        .map(|policy| policy.name)
        .collect()
}

#[test]
fn a_table_named_like_a_schema_keeps_only_its_own_grants_and_policies() {
    let store = MemoryStore::default();
    setup(&store);
    admin_runs(
        &store,
        &[
            "CREATE SCHEMA s",
            "CREATE TABLE s.t (id integer, x integer)",
            "INSERT INTO s.t VALUES (1, 10)",
            "CREATE TABLE s (id integer, t integer, x integer)",
            "INSERT INTO s VALUES (1, 2, 3)",
            "CREATE USER t LOGIN",
            "CREATE USER x LOGIN",
            "GRANT SELECT (x) ON s TO t",
            "CREATE POLICY on_s ON s USING (true)",
            "CREATE POLICY on_st ON s.t USING (true)",
        ],
    );
    denied(&mut as_role(&store, "x"), "SELECT x FROM s.t");
    admin_runs(&store, &["GRANT SELECT ON s.t TO x"]);
    let mut t = as_role(&store, "t");
    assert_eq!(run(&mut t, "SELECT x FROM s").unwrap(), "x\n3\nSELECT 1");
    denied(&mut t, "SELECT id FROM s");
    denied(&mut t, "SELECT x FROM s.t");
    assert_eq!(policy_names(&store, "s"), ["on_s"]);
    assert_eq!(policy_names(&store, "s\0t"), ["on_st"]);

    admin_runs(&store, &["ALTER TABLE s RENAME TO u"]);
    assert_eq!(
        run(&mut as_role(&store, "x"), "SELECT x FROM s.t").unwrap(),
        "x\n10\nSELECT 1"
    );
    assert_eq!(
        run(&mut as_role(&store, "t"), "SELECT x FROM u").unwrap(),
        "x\n3\nSELECT 1"
    );
    assert_eq!(policy_names(&store, "u"), ["on_s"]);
    assert_eq!(policy_names(&store, "s\0t"), ["on_st"]);
    let reader = store.begin_read().unwrap();
    let table_grants: Vec<(String, String)> = every_grant(&reader)
        .unwrap()
        .into_iter()
        .map(|grant| (grant.table, grant.grantee))
        .filter(|(table, _)| table != "staff" && table != "teams")
        .collect();
    assert_eq!(table_grants, [("s\0t".to_owned(), "x".to_owned())]);
    let column_grants: Vec<(String, String, String)> = every_column_grant(&reader)
        .unwrap()
        .into_iter()
        .map(|grant| (grant.table, grant.grantee, grant.column))
        .filter(|(table, _, _)| table != "staff")
        .collect();
    assert_eq!(
        column_grants,
        [("u".to_owned(), "t".to_owned(), "x".to_owned())]
    );
    drop(reader);

    admin_runs(&store, &["ALTER TABLE s.t DROP COLUMN x"]);
    assert_eq!(
        run(&mut as_role(&store, "t"), "SELECT x FROM u").unwrap(),
        "x\n3\nSELECT 1"
    );
    admin_runs(&store, &["REVOKE SELECT ON s.t FROM x"]);
    denied(&mut as_role(&store, "x"), "SELECT id FROM s.t");
    assert_eq!(
        run(&mut as_role(&store, "t"), "SELECT x FROM u").unwrap(),
        "x\n3\nSELECT 1"
    );
    admin_runs(
        &store,
        &[
            "GRANT SELECT ON s.t TO x",
            "REVOKE SELECT ON u FROM t",
            "DROP TABLE u",
        ],
    );
    assert_eq!(
        run(&mut as_role(&store, "x"), "SELECT id FROM s.t").unwrap(),
        "id\n1\nSELECT 1"
    );
    assert_eq!(policy_names(&store, "s\0t"), ["on_st"]);
    assert!(policy_names(&store, "u").is_empty());
}

fn plant_leftover_grants(store: &MemoryStore, relation: &str, grantee: &str, columns: &[&str]) {
    let mut writer = store.begin_write().unwrap();
    writer
        .put(
            &grant_key(relation, grantee),
            &[RECORD_VERSION, Privilege::Select.bit()],
        )
        .unwrap();
    for column in columns {
        writer
            .put(
                &column_grant_key(relation, grantee, column),
                &[COLUMN_RECORD_VERSION, Privilege::Select.bit()],
            )
            .unwrap();
    }
    writer.commit().unwrap();
}

#[test]
fn a_dropped_view_takes_its_grants_with_it() {
    let store = MemoryStore::default();
    setup(&store);
    admin_runs(
        &store,
        &[
            "CREATE USER eve LOGIN",
            "CREATE VIEW v AS SELECT id FROM teams",
            "GRANT SELECT (id) ON v TO eve",
            "GRANT SELECT ON v TO clerk",
        ],
    );
    assert_eq!(
        run(&mut as_role(&store, "eve"), "SELECT id FROM v").unwrap(),
        "id\n1\nSELECT 1"
    );
    admin_runs(
        &store,
        &[
            "DROP VIEW v",
            "CREATE VIEW v AS SELECT salary AS id FROM staff",
        ],
    );
    denied(&mut as_role(&store, "eve"), "SELECT id FROM v");
    denied(&mut as_role(&store, "clerk"), "SELECT * FROM v");
}

#[test]
fn a_renamed_view_keeps_its_grants_and_leaves_none_behind() {
    let store = MemoryStore::default();
    setup(&store);
    admin_runs(
        &store,
        &[
            "CREATE USER eve LOGIN",
            "CREATE SCHEMA s",
            "CREATE VIEW v AS SELECT id FROM teams",
            "GRANT SELECT (id) ON v TO eve",
            "GRANT SELECT ON v TO clerk",
            "ALTER VIEW v RENAME TO w",
        ],
    );
    assert_eq!(
        run(&mut as_role(&store, "eve"), "SELECT id FROM w").unwrap(),
        "id\n1\nSELECT 1"
    );
    assert_eq!(
        run(&mut as_role(&store, "clerk"), "SELECT * FROM w").unwrap(),
        "id\n1\nSELECT 1"
    );
    admin_runs(
        &store,
        &[
            "CREATE VIEW v AS SELECT salary AS id FROM staff",
            "ALTER VIEW w SET SCHEMA s",
        ],
    );
    denied(&mut as_role(&store, "eve"), "SELECT id FROM v");
    denied(&mut as_role(&store, "clerk"), "SELECT * FROM v");
    assert_eq!(
        run(&mut as_role(&store, "eve"), "SELECT id FROM s.w").unwrap(),
        "id\n1\nSELECT 1"
    );
}

#[test]
fn a_new_relation_name_never_inherits_leftover_grants() {
    let store = MemoryStore::default();
    setup(&store);
    admin_runs(&store, &["CREATE USER eve LOGIN"]);
    plant_leftover_grants(&store, "ghost", "eve", &["id"]);
    admin_runs(&store, &["CREATE TABLE ghost (id integer)"]);
    denied(&mut as_role(&store, "eve"), "SELECT id FROM ghost");

    plant_leftover_grants(&store, "spectre", "eve", &["id"]);
    admin_runs(&store, &["CREATE VIEW spectre AS SELECT id FROM staff"]);
    denied(&mut as_role(&store, "eve"), "SELECT id FROM spectre");

    plant_leftover_grants(&store, "target", "eve", &["id", "salary"]);
    admin_runs(&store, &["ALTER TABLE teams RENAME TO target"]);
    denied(&mut as_role(&store, "eve"), "SELECT id FROM target");
    assert_eq!(
        run(&mut as_role(&store, "clerk"), "SELECT id FROM target").unwrap(),
        "id\n1\nSELECT 1"
    );

    plant_leftover_grants(&store, "view_target", "eve", &["id"]);
    admin_runs(&store, &["ALTER VIEW spectre RENAME TO view_target"]);
    denied(&mut as_role(&store, "eve"), "SELECT id FROM view_target");

    plant_leftover_grants(&store, "summary", "eve", &["id"]);
    admin_runs(
        &store,
        &["CREATE MATERIALIZED VIEW summary AS SELECT id FROM staff"],
    );
    denied(&mut as_role(&store, "eve"), "SELECT id FROM summary");
}

#[test]
fn an_added_column_never_inherits_a_leftover_column_grant() {
    let store = MemoryStore::default();
    setup(&store);
    admin_runs(&store, &["CREATE USER eve LOGIN"]);
    let mut writer = store.begin_write().unwrap();
    writer
        .put(
            &column_grant_key("teams", "eve", "extra"),
            &[COLUMN_RECORD_VERSION, Privilege::Select.bit()],
        )
        .unwrap();
    writer
        .put(
            &column_grant_key("teams", "eve", "later"),
            &[COLUMN_RECORD_VERSION, Privilege::Select.bit()],
        )
        .unwrap();
    writer.commit().unwrap();
    admin_runs(
        &store,
        &[
            "ALTER TABLE teams ADD COLUMN extra integer",
            "GRANT SELECT (id) ON teams TO eve",
            "ALTER TABLE teams RENAME COLUMN id TO later",
        ],
    );
    denied(&mut as_role(&store, "eve"), "SELECT extra FROM teams");
    assert_eq!(
        run(&mut as_role(&store, "eve"), "SELECT later FROM teams").unwrap(),
        "later\n1\nSELECT 1"
    );
    denied(&mut as_role(&store, "eve"), "SELECT salary FROM teams");
}

#[test]
fn a_joined_update_needs_only_the_written_columns_and_select_on_what_it_reads() {
    let store = MemoryStore::default();
    setup(&store);
    let mut clerk = as_role(&store, "clerk");
    assert_eq!(
        run(
            &mut clerk,
            "UPDATE staff SET name = 'kim' FROM teams WHERE staff.id = teams.id"
        )
        .unwrap(),
        "UPDATE 1"
    );
    denied(
        &mut clerk,
        "UPDATE staff SET salary = 1 FROM teams WHERE staff.id = teams.id",
    );
    denied(
        &mut clerk,
        "UPDATE staff SET name = 'x' FROM teams WHERE staff.salary = teams.salary",
    );
    denied(
        &mut clerk,
        "DELETE FROM staff USING teams WHERE staff.id = teams.id",
    );
    let mut boss = as_role(&store, "boss");
    denied(
        &mut boss,
        "DELETE FROM staff USING teams WHERE staff.id = teams.id",
    );
    assert_eq!(
        run(
            &mut Session::new(&store),
            "SELECT id, name FROM staff ORDER BY id"
        )
        .unwrap(),
        "id|name\n1|kim\n2|lin\nSELECT 2"
    );
}
