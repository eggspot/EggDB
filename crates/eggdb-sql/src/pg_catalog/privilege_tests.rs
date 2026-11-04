use super::privilege_inquiry::privilege_list_parses;
use crate::error::{SqlResult, SqlState};
use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::Session;

fn run(session: &mut Session<'_>, sql: &str) -> SqlResult<String> {
    let mut last = String::new();
    session.execute(sql, &mut |result| last = render(&result))?;
    Ok(last)
}

fn shown(session: &mut Session<'_>, sql: &str) -> String {
    run(session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"))
}

fn setup(store: &MemoryStore) {
    crate::memory_store::use_seeded_random_bytes();
    let mut admin = Session::new(store);
    for sql in [
        "CREATE SCHEMA myschema",
        "CREATE TABLE myschema.mytable (col1 integer, col2 text)",
        "CREATE TABLE accounts (id integer PRIMARY KEY, manager text, balance integer)",
        "INSERT INTO accounts VALUES (1, 'carol', 10), (2, 'joe', 20)",
        "CREATE TABLE hidden (id integer, secret text)",
        "CREATE SEQUENCE serial",
        "CREATE FUNCTION add_one(x integer) RETURNS integer LANGUAGE sql AS 'SELECT x + 1'",
        "CREATE USER joe LOGIN",
        "CREATE USER admin LOGIN",
        "CREATE USER miriam_rw LOGIN",
        "CREATE ROLE staff",
        "CREATE USER carol LOGIN IN ROLE staff",
        "CREATE USER boss LOGIN SUPERUSER",
        "GRANT SELECT ON myschema.mytable TO PUBLIC",
        "GRANT SELECT, UPDATE, INSERT ON myschema.mytable TO admin",
        "GRANT SELECT (col1), UPDATE (col1) ON myschema.mytable TO miriam_rw",
        "GRANT INSERT ON accounts TO joe",
        "GRANT SELECT ON accounts TO staff",
        "GRANT SELECT, DELETE ON hidden TO joe",
        "GRANT SELECT (secret) ON hidden TO miriam_rw",
    ] {
        shown(&mut admin, sql);
    }
}

fn as_role<'s>(store: &'s MemoryStore, user: &str) -> Session<'s> {
    let mut session = Session::new(store);
    session
        .identify_role(user, "eggdb")
        .unwrap_or_else(|error| panic!("{user}: {error}"));
    session
}

fn fails_with(session: &mut Session<'_>, sql: &str, state: SqlState) -> String {
    let error = run(session, sql).expect_err(sql);
    assert_eq!(error.state(), state, "{sql}: {error}");
    error.message().to_owned()
}

#[test]
fn the_manual_examples_of_the_inquiry_functions_answer_from_the_grants() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    assert_eq!(
        shown(
            &mut admin,
            "SELECT has_table_privilege('myschema.mytable', 'select') AS mine, \
             has_table_privilege('joe', 'accounts', 'INSERT, SELECT WITH GRANT OPTION') AS joe_any, \
             has_table_privilege('joe', 'accounts', 'SELECT WITH GRANT OPTION') AS joe_option, \
             has_table_privilege('joe', 'accounts', 'select') AS joe_select, \
             has_table_privilege('carol', 'accounts', 'SELECT') AS through_staff, \
             has_table_privilege('public', 'myschema.mytable', ' Select ') AS public, \
             has_table_privilege('boss', 'hidden', 'DELETE WITH GRANT OPTION') AS superuser"
        ),
        "mine|joe_any|joe_option|joe_select|through_staff|public|superuser\nt|t|f|f|t|t|t\nSELECT 1"
    );
    assert_eq!(
        shown(
            &mut admin,
            "SELECT has_column_privilege('miriam_rw', 'myschema.mytable', 'col1', 'UPDATE') AS col1, \
             has_column_privilege('miriam_rw', 'myschema.mytable', 'col2', 'SELECT') AS by_public, \
             has_column_privilege('miriam_rw', 'myschema.mytable', 'col2', 'UPDATE') AS col2, \
             has_column_privilege('miriam_rw', 'myschema.mytable', 1::smallint, 'update') AS by_number, \
             has_column_privilege('miriam_rw', 'myschema.mytable', 9::smallint, 'update') AS no_column, \
             has_any_column_privilege('miriam_rw', 'hidden', 'SELECT') AS any_hidden, \
             has_any_column_privilege('joe', 'myschema.mytable', 'INSERT') AS any_insert"
        ),
        "col1|by_public|col2|by_number|no_column|any_hidden|any_insert\nt|t|f|t|NULL|t|f\nSELECT 1"
    );
    assert_eq!(
        shown(
            &mut admin,
            "SELECT has_sequence_privilege('joe', 'serial', 'USAGE') AS usage, \
             has_sequence_privilege('joe', 'serial', 'SELECT') AS read, \
             has_schema_privilege('joe', 'myschema', 'USAGE') AS schema_usage, \
             has_schema_privilege('joe', 'myschema', 'CREATE') AS schema_create, \
             has_database_privilege('joe', current_database(), 'CONNECT') AS connect, \
             has_database_privilege('joe', current_database(), 'TEMP, CREATE') AS temp, \
             has_function_privilege('joe', 'add_one(integer)', 'execute') AS execute, \
             has_function_privilege('joe', 'add_one(integer)', 'EXECUTE WITH GRANT OPTION') AS execute_option"
        ),
        "usage|read|schema_usage|schema_create|connect|temp|execute|execute_option\nt|f|t|f|t|f|t|f\nSELECT 1"
    );
    assert_eq!(
        shown(
            &mut admin,
            "SELECT pg_has_role('carol', 'staff', 'MEMBER') AS member, \
             pg_has_role('carol', 'staff', 'USAGE WITH ADMIN OPTION') AS admin_option, \
             pg_has_role('joe', 'staff', 'usage') AS joe, \
             pg_has_role('carol', 'carol', 'SET') AS itself"
        ),
        "member|admin_option|joe|itself\nt|f|f|t\nSELECT 1"
    );
}

#[test]
fn a_checked_role_is_told_exactly_what_its_statements_may_do() {
    let store = MemoryStore::default();
    setup(&store);
    let mut joe = as_role(&store, "joe");
    assert_eq!(
        shown(
            &mut joe,
            "SELECT has_table_privilege('accounts', 'INSERT') AS insert, \
             has_table_privilege('accounts', 'SELECT') AS read, \
             has_table_privilege('hidden', 'SELECT, UPDATE') AS hidden, \
             has_table_privilege('accounts', 'INSERT WITH GRANT OPTION') AS option, \
             has_table_privilege('accounts', 'MAINTAIN') AS maintain, \
             has_table_privilege('pg_catalog.pg_class', 'SELECT') AS catalog, \
             has_table_privilege('pg_catalog.pg_class', 'UPDATE') AS catalog_write"
        ),
        "insert|read|hidden|option|maintain|catalog|catalog_write\nt|f|t|f|f|t|f\nSELECT 1"
    );
    assert_eq!(
        shown(
            &mut joe,
            "SELECT has_schema_privilege('public', 'USAGE') AS usage, \
             has_schema_privilege('public', 'CREATE') AS create, \
             has_database_privilege(current_database(), 'CONNECT') AS connect, \
             has_database_privilege(current_database(), 'TEMPORARY') AS temp, \
             pg_has_role('staff', 'MEMBER') AS staff"
        ),
        "usage|create|connect|temp|staff\nt|f|t|f|f\nSELECT 1"
    );
    fails_with(&mut joe, "SELECT * FROM accounts", SqlState::INSUFFICIENT_PRIVILEGE);
    shown(&mut joe, "SELECT * FROM hidden");
    shown(&mut joe, "INSERT INTO accounts VALUES (3, 'joe', 0)");
    fails_with(&mut joe, "CREATE TABLE mine (id integer)", SqlState::INSUFFICIENT_PRIVILEGE);
    fails_with(&mut joe, "CREATE TEMPORARY TABLE scratch (id integer)", SqlState::INSUFFICIENT_PRIVILEGE);
    let mut carol = as_role(&store, "carol");
    assert_eq!(
        shown(
            &mut carol,
            "SELECT has_table_privilege('accounts', 'SELECT') AS read, \
             has_table_privilege(oid, 'SELECT') AS by_oid, pg_has_role('staff', 'MEMBER') AS staff \
             FROM pg_catalog.pg_class WHERE relname = 'accounts'"
        ),
        "read|by_oid|staff\nt|t|t\nSELECT 1"
    );
    shown(&mut carol, "SELECT * FROM accounts");
}

#[test]
fn the_inquiry_functions_fail_as_postgresql_does() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    for (sql, state) in [
        (
            "SELECT has_table_privilege('nobody', 'accounts', 'SELECT')",
            SqlState::UNDEFINED_OBJECT,
        ),
        (
            "SELECT has_table_privilege('missing', 'SELECT')",
            SqlState::UNDEFINED_TABLE,
        ),
        (
            "SELECT has_table_privilege('accounts', 'FLY')",
            SqlState::INVALID_PARAMETER_VALUE,
        ),
        (
            "SELECT has_table_privilege('accounts', 'SELECT,')",
            SqlState::INVALID_PARAMETER_VALUE,
        ),
        (
            "SELECT has_table_privilege('accounts', 'SELECT  WITH GRANT OPTION')",
            SqlState::INVALID_PARAMETER_VALUE,
        ),
        (
            "SELECT has_column_privilege('accounts', 'DELETE', 'id')",
            SqlState::UNDEFINED_COLUMN,
        ),
        (
            "SELECT has_column_privilege('accounts', 'id', 'DELETE')",
            SqlState::INVALID_PARAMETER_VALUE,
        ),
        (
            "SELECT has_schema_privilege('nowhere', 'USAGE')",
            SqlState::INVALID_SCHEMA_NAME,
        ),
        (
            "SELECT has_database_privilege('nodb', 'CONNECT')",
            SqlState::INVALID_CATALOG_NAME,
        ),
        (
            "SELECT has_sequence_privilege('accounts', 'USAGE')",
            SqlState::WRONG_OBJECT_TYPE,
        ),
        (
            "SELECT has_function_privilege('missing_fn(integer)', 'EXECUTE')",
            SqlState::UNDEFINED_FUNCTION,
        ),
        (
            "SELECT has_function_privilege('add_one', 'EXECUTE')",
            SqlState::INVALID_TEXT_REPRESENTATION,
        ),
        (
            "SELECT pg_has_role('public', 'MEMBER')",
            SqlState::UNDEFINED_OBJECT,
        ),
        (
            "SELECT has_table_privilege('accounts')",
            SqlState::UNDEFINED_FUNCTION,
        ),
    ] {
        fails_with(&mut admin, sql, state);
    }
    assert_eq!(
        fails_with(
            &mut admin,
            "SELECT has_table_privilege('accounts', 'select, fly')",
            SqlState::INVALID_PARAMETER_VALUE
        ),
        "unrecognized privilege type: \"fly\""
    );
    assert_eq!(
        shown(
            &mut admin,
            "SELECT has_table_privilege(4000000000::oid, 'SELECT') AS no_table, \
             has_table_privilege('accounts', NULL) AS no_list, \
             has_schema_privilege(4000000000::oid, 'USAGE') AS no_schema"
        ),
        "no_table|no_list|no_schema\nNULL|NULL|NULL\nSELECT 1"
    );
}

#[test]
fn the_privilege_views_show_only_grants_to_the_current_users_roles() {
    let store = MemoryStore::default();
    setup(&store);
    let mut carol = as_role(&store, "carol");
    assert_eq!(
        shown(
            &mut carol,
            "SELECT grantee, table_name, privilege_type, with_hierarchy \
             FROM information_schema.table_privileges ORDER BY table_name, grantee"
        ),
        "grantee|table_name|privilege_type|with_hierarchy\nstaff|accounts|SELECT|YES\nPUBLIC|mytable|SELECT|YES\nSELECT 2"
    );
    assert_eq!(
        shown(
            &mut carol,
            "SELECT grantee, table_name, privilege_type FROM information_schema.role_table_grants"
        ),
        "grantee|table_name|privilege_type\nstaff|accounts|SELECT\nSELECT 1"
    );
    for (sql, expected) in [
        (
            "SELECT count(*) FROM information_schema.table_privileges WHERE table_name = 'hidden'",
            "0",
        ),
        (
            "SELECT count(*) FROM information_schema.column_privileges WHERE table_name = 'hidden'",
            "0",
        ),
        (
            "SELECT count(*) FROM information_schema.column_privileges WHERE grantee = 'miriam_rw'",
            "0",
        ),
        (
            "SELECT count(*) FROM information_schema.column_privileges WHERE table_name = 'mytable'",
            "2",
        ),
        (
            "SELECT count(*) FROM information_schema.role_column_grants WHERE table_name = 'mytable'",
            "0",
        ),
        (
            "SELECT count(*) FROM information_schema.role_column_grants WHERE table_name = 'accounts'",
            "3",
        ),
    ] {
        assert_eq!(
            shown(&mut carol, sql),
            format!("count\n{expected}\nSELECT 1"),
            "{sql}"
        );
    }
    let mut miriam = as_role(&store, "miriam_rw");
    assert_eq!(
        shown(
            &mut miriam,
            "SELECT grantee, table_name, column_name, privilege_type \
             FROM information_schema.role_column_grants ORDER BY table_name, column_name, privilege_type"
        ),
        "grantee|table_name|column_name|privilege_type\n\
         miriam_rw|hidden|secret|SELECT\n\
         miriam_rw|mytable|col1|SELECT\n\
         miriam_rw|mytable|col1|UPDATE\nSELECT 3"
    );
    let mut admin = Session::new(&store);
    assert_eq!(
        shown(
            &mut admin,
            "SELECT grantee, privilege_type FROM information_schema.table_privileges \
             WHERE table_name = 'hidden' ORDER BY privilege_type"
        ),
        "grantee|privilege_type\njoe|DELETE\njoe|SELECT\nSELECT 2"
    );
}

#[test]
fn usage_privileges_list_what_every_role_may_use() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    shown(&mut admin, "CREATE DOMAIN positive AS integer CHECK (VALUE > 0)");
    assert_eq!(
        shown(
            &mut admin,
            "SELECT grantee, object_name, object_type, privilege_type \
             FROM information_schema.usage_privileges ORDER BY object_name"
        ),
        "grantee|object_name|object_type|privilege_type\n\
         PUBLIC|positive|DOMAIN|USAGE\n\
         PUBLIC|serial|SEQUENCE|USAGE\nSELECT 2"
    );
}

#[test]
fn relacl_and_attacl_list_the_ddl_priv_example_grants() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    assert_eq!(
        shown(
            &mut admin,
            "SELECT relacl FROM pg_catalog.pg_class WHERE oid = 'myschema.mytable'::regclass"
        ),
        "relacl\n{eggdb=arwdDxt/eggdb,admin=arw/eggdb,=r/eggdb}\nSELECT 1"
    );
    assert_eq!(
        shown(
            &mut admin,
            "SELECT attname, attacl FROM pg_catalog.pg_attribute \
             WHERE attrelid = 'myschema.mytable'::regclass AND attnum > 0 ORDER BY attnum"
        ),
        "attname|attacl\ncol1|{miriam_rw=rw/eggdb}\ncol2|NULL\nSELECT 2"
    );
}

#[test]
fn pg_policy_and_pg_policies_show_each_policy_as_postgresql_does() {
    let store = MemoryStore::default();
    setup(&store);
    let mut admin = Session::new(&store);
    for sql in [
        "ALTER TABLE accounts ENABLE ROW LEVEL SECURITY",
        "CREATE POLICY account_managers ON accounts TO staff USING (id > 1)",
        "CREATE POLICY guard ON accounts AS RESTRICTIVE FOR UPDATE USING (balance >= 0) WITH CHECK (balance < 1000)",
    ] {
        shown(&mut admin, sql);
    }
    assert_eq!(
        shown(
            &mut admin,
            "SELECT schemaname, tablename, policyname, permissive, roles, cmd, qual, with_check \
             FROM pg_catalog.pg_policies ORDER BY policyname"
        ),
        "schemaname|tablename|policyname|permissive|roles|cmd|qual|with_check\n\
         public|accounts|account_managers|PERMISSIVE|{staff}|ALL|(id > 1)|NULL\n\
         public|accounts|guard|RESTRICTIVE|{public}|UPDATE|(balance >= 0)|(balance < 1000)\nSELECT 2"
    );
    assert_eq!(
        shown(
            &mut admin,
            "SELECT polname, polcmd, polpermissive, polrelid = 'accounts'::regclass AS on_accounts, \
             pg_get_expr(polqual, polrelid) AS qual, 0 = ANY (polroles) AS to_public \
             FROM pg_catalog.pg_policy ORDER BY polname"
        ),
        "polname|polcmd|polpermissive|on_accounts|qual|to_public\n\
         account_managers|*|t|t|(id > 1)|f\n\
         guard|w|f|t|(balance >= 0)|t\nSELECT 2"
    );
    let mut carol = as_role(&store, "carol");
    assert_eq!(
        shown(&mut carol, "SELECT id FROM accounts ORDER BY id"),
        "id\n2\nSELECT 1"
    );
    shown(&mut admin, "DROP POLICY guard ON accounts");
    assert_eq!(
        shown(&mut admin, "SELECT count(*) FROM pg_catalog.pg_policies"),
        "count\n1\nSELECT 1"
    );
}

#[test]
fn privilege_lists_parse_as_postgresql_reads_them() {
    for (text, count) in [
        ("SELECT", 1),
        ("select", 1),
        ("  INSERT ,\tselect with grant option ", 2),
        ("SELECT WITH GRANT OPTION, RULE, MAINTAIN", 3),
    ] {
        assert_eq!(privilege_list_parses(text, false).ok(), Some(count), "{text}");
    }
    for text in [
        "",
        ",",
        "SELECT,",
        "SELECT  WITH GRANT OPTION",
        "SELECT WITH ADMIN OPTION",
        "SELECT WITH",
        "SÉLECT",
        "ALL",
        "USAGE",
    ] {
        assert!(privilege_list_parses(text, false).is_err(), "{text}");
    }
    assert_eq!(
        privilege_list_parses("member with admin option, USAGE WITH GRANT OPTION, set", true).ok(),
        Some(3)
    );
    assert!(privilege_list_parses("SELECT", true).is_err());
}

#[test]
fn random_privilege_lists_never_panic() {
    const PIECES: [&str; 16] = [
        "SELECT", "insert", " ", ",", "WITH", " GRANT", " OPTION", " ADMIN", "\t", "é", "\u{0}",
        "MEMBER", "with grant option", "ß", "\u{10ffff}", "UsAgE",
    ];
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..20_000 {
        let length = (next() % 12) as usize;
        let mut text = String::new();
        for _ in 0..length {
            let piece = PIECES[(next() % PIECES.len() as u64) as usize];
            text.push_str(piece);
        }
        let of_role = next() % 2 == 0;
        if let Ok(count) = privilege_list_parses(&text, of_role) {
            assert_eq!(count, text.split(',').count(), "{text:?}");
        }
    }
}
