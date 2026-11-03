use crate::encoding::{decode_meta, encode_meta, meta_key};
use crate::memory_store::{MemoryRead, MemoryStore, MemoryWrite};
use crate::store::{KeyValueRead, KeyValueStore, KeyValueWrite};
use crate::tests::render;
use crate::{
    OutsideLink, OutsideLinkKind, Session, SqlError, SqlResult, SqlState, VisibleRead,
    VisibleTables, VisibleWrite,
};

struct ConfinedStore<'s> {
    store: &'s MemoryStore,
    tables: VisibleTables,
}

impl KeyValueStore for ConfinedStore<'_> {
    type Read<'r>
        = VisibleRead<MemoryRead>
    where
        Self: 'r;
    type Write<'w>
        = VisibleWrite<MemoryWrite<'w>>
    where
        Self: 'w;

    fn begin_read(&self) -> SqlResult<Self::Read<'_>> {
        self.tables.read(self.store.begin_read()?)
    }

    fn begin_write(&self) -> SqlResult<Self::Write<'_>> {
        self.tables.write(self.store.begin_write()?)
    }
}

fn shop() -> MemoryStore {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    session
        .execute(
            "CREATE TABLE items (id serial PRIMARY KEY, name text);
             CREATE TABLE tags (id integer PRIMARY KEY, item integer REFERENCES items (id) ON DELETE CASCADE);
             CREATE TABLE secrets (id integer PRIMARY KEY, code text);
             CREATE TABLE log (id serial PRIMARY KEY, note text);
             CREATE TABLE parents (id integer PRIMARY KEY);
             CREATE TABLE children (id integer PRIMARY KEY, parent integer REFERENCES parents (id) ON DELETE CASCADE);
             CREATE TABLE customers (id integer PRIMARY KEY);
             CREATE TABLE orders (id integer PRIMARY KEY, customer integer REFERENCES customers (id), note text);
             CREATE TABLE viewed (id integer PRIMARY KEY, v integer);
             CREATE VIEW peek AS SELECT v FROM viewed;
             CREATE TABLE logged (id integer PRIMARY KEY, note text);
             CREATE FUNCTION write_log() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO log (note) VALUES (NEW.note); RETURN NEW; END $$;
             CREATE TRIGGER logged_log AFTER INSERT ON logged FOR EACH ROW EXECUTE FUNCTION write_log();
             CREATE PROCEDURE touch_secrets() LANGUAGE plpgsql AS $$ BEGIN UPDATE secrets SET code = 'gone'; END $$;
             CREATE SEQUENCE loose_seq;
             INSERT INTO secrets VALUES (1, 'swordfish');
             INSERT INTO parents VALUES (1);
             INSERT INTO children VALUES (1, 1);
             INSERT INTO customers VALUES (1);
             INSERT INTO orders VALUES (1, 1, 'first');
             INSERT INTO viewed VALUES (1, 1)",
            &mut |_| {},
        )
        .unwrap();
    session.close().unwrap();
    store
}

fn listed() -> VisibleTables {
    VisibleTables::new([
        "items", "tags", "parents", "orders", "viewed", "logged", "drafts",
    ])
}

fn confined(store: &MemoryStore) -> ConfinedStore<'_> {
    ConfinedStore {
        store,
        tables: listed(),
    }
}

fn runs(session: &mut Session<'_>, sql: &str) {
    if let Err(error) = session.execute(sql, &mut |_| {}) {
        panic!("{sql}: {error}");
    }
}

fn fails(session: &mut Session<'_>, sql: &str) -> SqlError {
    match session.execute(sql, &mut |_| {}) {
        Ok(()) => panic!("{sql} succeeded"),
        Err(error) => error,
    }
}

fn refused(session: &mut Session<'_>, sql: &str) -> String {
    let error = fails(session, sql);
    assert_eq!(
        error.state(),
        SqlState::INSUFFICIENT_PRIVILEGE,
        "{sql}: {error}"
    );
    error.message().to_owned()
}

fn missing(session: &mut Session<'_>, sql: &str) {
    let error = fails(session, sql);
    assert_eq!(error.state(), SqlState::UNDEFINED_TABLE, "{sql}: {error}");
}

fn shown(store: &MemoryStore, sql: &str) -> String {
    let mut session = Session::new(store);
    let mut outputs = Vec::new();
    if let Err(error) = session.execute(sql, &mut |result| outputs.push(render(&result))) {
        panic!("{sql}: {error}");
    }
    outputs.last().cloned().unwrap_or_default()
}

#[test]
fn listed_tables_take_every_change_and_new_tables_only_under_listed_names() {
    let store = shop();
    let limited = confined(&store);
    let mut session = Session::new(&limited);
    for sql in [
        "INSERT INTO items (name) VALUES ('pen'), ('ink')",
        "UPDATE items SET name = 'quill' WHERE id = 1",
        "INSERT INTO tags VALUES (1, 1), (2, 2)",
        "DELETE FROM items WHERE id = 2",
        "CREATE TABLE drafts (id serial PRIMARY KEY, body text)",
        "INSERT INTO drafts (body) VALUES ('first')",
        "ALTER TABLE drafts ADD COLUMN extra integer",
        "CREATE INDEX drafts_by_body ON drafts (body)",
        "COMMENT ON TABLE drafts IS 'mine'",
        "ANALYZE drafts",
        "INSERT INTO viewed VALUES (2, 2)",
        "SELECT count(*) FROM items",
    ] {
        runs(&mut session, sql);
    }
    session.close().unwrap();
    assert_eq!(
        shown(&store, "SELECT name FROM items"),
        "name\nquill\nSELECT 1"
    );
    assert_eq!(
        shown(&store, "SELECT id, item FROM tags"),
        "id|item\n1|1\nSELECT 1"
    );
    assert_eq!(
        shown(&store, "SELECT body FROM drafts"),
        "body\nfirst\nSELECT 1"
    );
    let mut session = Session::new(&limited);
    runs(&mut session, "DROP TABLE drafts");
    session.close().unwrap();
    assert_eq!(
        shown(
            &store,
            "SELECT count(*) FROM pg_class WHERE relname = 'drafts'"
        ),
        "count\n0\nSELECT 1"
    );
}

#[test]
fn tables_outside_the_list_read_as_missing_and_new_names_outside_it_are_refused() {
    let store = shop();
    let limited = confined(&store);
    let mut session = Session::new(&limited);
    missing(&mut session, "INSERT INTO secrets VALUES (2, 'x')");
    missing(&mut session, "UPDATE secrets SET code = 'x' WHERE id = 1");
    missing(&mut session, "DELETE FROM secrets");
    missing(&mut session, "DROP TABLE secrets");
    missing(&mut session, "SELECT code FROM secrets");
    missing(&mut session, "SELECT nextval('loose_seq')");
    let created = refused(&mut session, "CREATE TABLE elsewhere (id integer)");
    assert!(created.contains("\"elsewhere\""), "{created}");
    let shadowing = refused(&mut session, "CREATE TABLE secrets (id integer)");
    assert!(shadowing.contains("\"secrets\""), "{shadowing}");
    refused(&mut session, "CREATE VIEW glimpse AS SELECT 1 AS one");
    refused(&mut session, "CREATE SEQUENCE another_seq");
    let renamed = refused(&mut session, "ALTER TABLE items RENAME TO goods");
    assert!(renamed.contains("\"goods\""), "{renamed}");
    session.close().unwrap();
    assert_eq!(
        shown(&store, "SELECT code FROM secrets"),
        "code\nswordfish\nSELECT 1"
    );
    assert_eq!(
        shown(
            &store,
            "SELECT count(*) FROM pg_class WHERE relname IN ('elsewhere', 'glimpse', 'another_seq', 'goods')"
        ),
        "count\n0\nSELECT 1"
    );
}

#[test]
fn routines_triggers_schemas_types_and_roles_cannot_be_created() {
    let store = shop();
    let limited = confined(&store);
    let mut session = Session::new(&limited);
    for sql in [
        "CREATE FUNCTION leak() RETURNS integer LANGUAGE sql AS 'SELECT 1'",
        "CREATE OR REPLACE FUNCTION write_log() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
        "CREATE SCHEMA hideout",
        "CREATE TYPE mood AS ENUM ('calm', 'busy')",
        "CREATE ROLE intruder LOGIN SUPERUSER",
    ] {
        let message = refused(&mut session, sql);
        assert!(message.contains("outside a table"), "{sql}: {message}");
    }
    let trigger = refused(
        &mut session,
        "CREATE TRIGGER items_log AFTER INSERT ON items FOR EACH ROW EXECUTE FUNCTION write_log()",
    );
    assert!(trigger.contains("trigger"), "{trigger}");
    session.close().unwrap();
    assert_eq!(
        shown(
            &store,
            "SELECT count(*) FROM pg_namespace WHERE nspname = 'hideout'"
        ),
        "count\n0\nSELECT 1"
    );
}

#[test]
fn a_trigger_or_a_procedure_that_reaches_another_table_fails_and_changes_nothing() {
    let store = shop();
    let limited = confined(&store);
    let mut session = Session::new(&limited);
    missing(&mut session, "INSERT INTO logged VALUES (1, 'hello')");
    missing(&mut session, "CALL touch_secrets()");
    session.close().unwrap();
    assert_eq!(
        shown(&store, "SELECT count(*) FROM logged"),
        "count\n0\nSELECT 1"
    );
    assert_eq!(
        shown(&store, "SELECT count(*) FROM log"),
        "count\n0\nSELECT 1"
    );
    assert_eq!(
        shown(&store, "SELECT code FROM secrets"),
        "code\nswordfish\nSELECT 1"
    );
}

#[test]
fn a_foreign_key_or_a_view_across_the_list_keeps_the_listed_table_from_changing() {
    let store = shop();
    let limited = confined(&store);
    let mut session = Session::new(&limited);
    runs(&mut session, "INSERT INTO parents VALUES (2)");
    let referenced = refused(&mut session, "DELETE FROM parents WHERE id = 1");
    assert!(
        referenced.contains("table \"children\" has a foreign key to \"parents\""),
        "{referenced}"
    );
    refused(&mut session, "TRUNCATE parents");
    refused(&mut session, "DROP TABLE parents");
    runs(&mut session, "INSERT INTO orders VALUES (2, 1, 'second')");
    runs(
        &mut session,
        "UPDATE orders SET note = 'changed' WHERE id = 1",
    );
    let referencing = refused(&mut session, "ALTER TABLE orders ADD COLUMN extra integer");
    assert!(
        referencing.contains("\"orders\" has a foreign key to table \"customers\""),
        "{referencing}"
    );
    let viewed = refused(&mut session, "ALTER TABLE viewed ADD COLUMN w integer");
    assert!(
        viewed.contains("view \"peek\" reads \"viewed\""),
        "{viewed}"
    );
    refused(&mut session, "DROP TABLE viewed");
    session.close().unwrap();
    assert_eq!(
        shown(&store, "SELECT count(*) FROM children"),
        "count\n1\nSELECT 1"
    );
    assert_eq!(
        shown(&store, "SELECT id FROM parents ORDER BY id"),
        "id\n1\n2\nSELECT 2"
    );
    assert_eq!(shown(&store, "SELECT v FROM peek"), "v\n1\nSELECT 1");
    assert_eq!(
        shown(&store, "SELECT id, note FROM orders ORDER BY id"),
        "id|note\n1|changed\n2|second\nSELECT 2"
    );
}

#[test]
fn the_ties_across_the_list_are_named() {
    let store = shop();
    let links = listed()
        .outside_links(&store.begin_read().unwrap())
        .unwrap();
    for expected in [
        OutsideLink {
            listed: "parents".to_owned(),
            outside: "children".to_owned(),
            kind: OutsideLinkKind::ReferencedBy,
        },
        OutsideLink {
            listed: "orders".to_owned(),
            outside: "customers".to_owned(),
            kind: OutsideLinkKind::References,
        },
        OutsideLink {
            listed: "viewed".to_owned(),
            outside: "peek".to_owned(),
            kind: OutsideLinkKind::ReadByView,
        },
    ] {
        assert!(links.contains(&expected), "{expected:?} in {links:?}");
    }
    assert_eq!(links.len(), 3, "{links:?}");
    let referenced = links
        .iter()
        .find(|link| link.kind == OutsideLinkKind::ReferencedBy)
        .unwrap();
    assert_eq!(
        referenced.blocked_changes(),
        "delete its rows, change the columns that foreign key points at, or change its definition"
    );
    let referencing = links
        .iter()
        .find(|link| link.kind == OutsideLinkKind::References)
        .unwrap();
    assert_eq!(referencing.blocked_changes(), "change its definition");
    let view = links
        .iter()
        .find(|link| link.kind == OutsideLinkKind::ReadByView)
        .unwrap();
    assert_eq!(view.blocked_changes(), "change its definition");
}

#[test]
fn a_refused_change_rolls_the_transaction_back_even_when_it_was_caught() {
    let store = shop();
    let before = decode_meta(
        &store
            .begin_read()
            .unwrap()
            .get(&meta_key())
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let mut writer = listed().write(store.begin_write().unwrap()).unwrap();
    writer
        .put(&meta_key(), &encode_meta(before + 1000))
        .unwrap();
    let error = writer.put(b"outside SQL", b"x").unwrap_err();
    assert_eq!(error.state(), SqlState::INSUFFICIENT_PRIVILEGE);
    let commit = writer.commit().unwrap_err();
    assert_eq!(commit.state(), SqlState::INSUFFICIENT_PRIVILEGE);
    assert!(commit.message().contains("rolled back"), "{commit}");
    let read = store.begin_read().unwrap();
    assert_eq!(
        decode_meta(&read.get(&meta_key()).unwrap().unwrap()).unwrap(),
        before
    );
    assert!(read.get(b"outside SQL").unwrap().is_none());
}

#[test]
fn deleting_an_absent_key_outside_the_list_is_no_change() {
    let store = shop();
    let mut writer = listed().write(store.begin_write().unwrap()).unwrap();
    assert!(!writer.delete(b"never written").unwrap());
    writer.commit().unwrap();
    let mut writer = listed().write(store.begin_write().unwrap()).unwrap();
    let error = writer
        .delete(&crate::encoding::table_key("secrets"))
        .unwrap_err();
    assert_eq!(error.state(), SqlState::INSUFFICIENT_PRIVILEGE);
    writer.rollback().unwrap();
    assert!(
        store
            .begin_read()
            .unwrap()
            .get(&crate::encoding::table_key("secrets"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_writer_with_nothing_confined_passes_everything_through() {
    let store = shop();
    let mut writer = VisibleWrite::everything(store.begin_write().unwrap());
    writer.put(b"outside SQL", b"x").unwrap();
    writer.commit().unwrap();
    assert_eq!(
        store.begin_read().unwrap().get(b"outside SQL").unwrap(),
        Some(b"x".to_vec())
    );
}

#[test]
fn a_trigger_cannot_be_dropped_or_disabled_but_goes_with_its_dropped_table() {
    let store = shop();
    let limited = confined(&store);
    let mut session = Session::new(&limited);
    for sql in [
        "DROP TRIGGER logged_log ON logged",
        "ALTER TABLE logged DISABLE TRIGGER logged_log",
    ] {
        let message = refused(&mut session, sql);
        assert!(message.contains("trigger"), "{sql}: {message}");
    }
    session.close().unwrap();
    assert_eq!(
        shown(
            &store,
            "SELECT count(*) FROM information_schema.triggers WHERE trigger_name = 'logged_log'"
        ),
        "count\n1\nSELECT 1"
    );
    let mut session = Session::new(&limited);
    runs(&mut session, "DROP TABLE logged");
    session.close().unwrap();
    assert_eq!(
        shown(
            &store,
            "SELECT count(*) FROM information_schema.triggers WHERE trigger_name = 'logged_log'"
        ),
        "count\n0\nSELECT 1"
    );
}

#[test]
fn a_default_or_check_calling_a_user_routine_cannot_be_written() {
    let store = shop();
    let mut owner = Session::new(&store);
    runs(
        &mut owner,
        "CREATE FUNCTION wipe_secrets() RETURNS integer LANGUAGE plpgsql AS $$ BEGIN UPDATE secrets SET code = 'gone'; RETURN 1; END $$",
    );
    owner.close().unwrap();
    let limited = confined(&store);
    let mut session = Session::new(&limited);
    for sql in [
        "ALTER TABLE items ALTER COLUMN name SET DEFAULT wipe_secrets()::text",
        "CREATE TABLE drafts (id integer PRIMARY KEY, n integer CHECK (n < wipe_secrets()))",
        "CREATE TABLE drafts (id integer PRIMARY KEY, n integer DEFAULT wipe_secrets())",
    ] {
        let message = refused(&mut session, sql);
        assert!(message.contains("wipe_secrets"), "{sql}: {message}");
    }
    runs(
        &mut session,
        "ALTER TABLE items ALTER COLUMN name SET DEFAULT upper('x')",
    );
    runs(
        &mut session,
        "ALTER TABLE items ADD CONSTRAINT short_name CHECK (length(name) < 100)",
    );
    runs(
        &mut session,
        "ALTER TABLE items RENAME COLUMN name TO title",
    );
    session.close().unwrap();
    assert_eq!(
        shown(&store, "SELECT code FROM secrets"),
        "code\nswordfish\nSELECT 1"
    );
}

fn ledger() -> MemoryStore {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    session
        .execute(
            "CREATE TABLE accounts (id integer PRIMARY KEY, note text);
             CREATE TABLE ledger (id integer PRIMARY KEY, account integer REFERENCES accounts (id) ON DELETE CASCADE ON UPDATE CASCADE);
             CREATE TABLE holds (id integer PRIMARY KEY, account integer REFERENCES accounts (id) ON DELETE SET NULL);
             CREATE TABLE checks (id integer PRIMARY KEY, account integer REFERENCES accounts (id));
             INSERT INTO accounts VALUES (1, 'first'), (2, 'spare');
             INSERT INTO ledger VALUES (1, 1);
             INSERT INTO holds VALUES (1, 1);
             INSERT INTO checks VALUES (1, 1)",
            &mut |_| {},
        )
        .unwrap();
    session.close().unwrap();
    store
}

#[test]
fn a_table_another_table_references_takes_inserts_and_other_columns_but_not_deletes_or_key_changes()
{
    let store = ledger();
    let limited = ConfinedStore {
        store: &store,
        tables: VisibleTables::new(["accounts"]),
    };
    let mut session = Session::new(&limited);
    runs(&mut session, "INSERT INTO accounts VALUES (3, 'third')");
    runs(
        &mut session,
        "UPDATE accounts SET note = 'changed' WHERE id = 1",
    );
    runs(
        &mut session,
        "INSERT INTO accounts VALUES (1, 'upserted') ON CONFLICT (id) DO UPDATE SET note = excluded.note",
    );
    let deleted = refused(&mut session, "DELETE FROM accounts WHERE id = 1");
    assert!(
        deleted.starts_with("cannot delete a row of \"accounts\" or change a column a foreign key points at: table \""),
        "{deleted}"
    );
    assert!(
        deleted.ends_with("\" has a foreign key to \"accounts\", and this session may neither change nor check \"checks\" (--tables)")
            || deleted.ends_with("\" has a foreign key to \"accounts\", and this session may neither change nor check \"holds\" (--tables)")
            || deleted.ends_with("\" has a foreign key to \"accounts\", and this session may neither change nor check \"ledger\" (--tables)"),
        "{deleted}"
    );
    for sql in [
        "DELETE FROM accounts WHERE id = 2",
        "DELETE FROM accounts WHERE id = 3",
        "UPDATE accounts SET id = 10 WHERE id = 1",
        "UPDATE accounts SET id = id + 100",
        "TRUNCATE accounts",
        "DELETE FROM accounts",
    ] {
        let message = refused(&mut session, sql);
        assert!(
            message.starts_with(
                "cannot delete a row of \"accounts\" or change a column a foreign key points at"
            ),
            "{sql}: {message}"
        );
    }
    let altered = refused(
        &mut session,
        "ALTER TABLE accounts ADD COLUMN extra integer",
    );
    assert!(
        altered.starts_with("cannot change the definition of \"accounts\": table \""),
        "{altered}"
    );
    session.close().unwrap();
    assert_eq!(
        shown(&store, "SELECT id, note FROM accounts ORDER BY id"),
        "id|note\n1|upserted\n2|spare\n3|third\nSELECT 3"
    );
    assert_eq!(
        shown(&store, "SELECT account FROM ledger"),
        "account\n1\nSELECT 1"
    );
    assert_eq!(
        shown(&store, "SELECT account FROM holds"),
        "account\n1\nSELECT 1"
    );
}

#[test]
fn a_table_listed_with_columns_takes_inserts_and_changes_to_its_listed_columns() {
    let store = shop();
    let limited = ConfinedStore {
        store: &store,
        tables: VisibleTables::new(["secrets(id)", "items"]),
    };
    let mut session = Session::new(&limited);
    runs(&mut session, "INSERT INTO secrets VALUES (2)");
    runs(
        &mut session,
        "INSERT INTO secrets VALUES (1) ON CONFLICT (id) DO NOTHING",
    );
    assert_eq!(
        refused(&mut session, "DELETE FROM secrets WHERE id = 1"),
        "cannot delete rows of \"secrets\" or change their key: this session may read only some of its columns (--tables), and the row would take the others with it"
    );
    for sql in [
        "UPDATE secrets SET id = 5 WHERE id = 1",
        "DELETE FROM secrets",
        "TRUNCATE secrets",
        "DROP TABLE secrets",
    ] {
        let message = refused(&mut session, sql);
        assert!(
            message.starts_with("cannot delete rows of \"secrets\""),
            "{sql}: {message}"
        );
    }
    for sql in [
        "ALTER TABLE secrets ADD COLUMN extra integer",
        "COMMENT ON TABLE secrets IS 'changed'",
        "ANALYZE secrets",
        "CREATE POLICY p ON secrets USING (true)",
    ] {
        let message = refused(&mut session, sql);
        assert!(
            message.starts_with("cannot change \"secrets\""),
            "{sql}: {message}"
        );
    }
    for sql in [
        "INSERT INTO items (name) SELECT code FROM secrets",
        "UPDATE secrets SET code = 'gone'",
        "INSERT INTO secrets (id, code) VALUES (3, 'x')",
        "INSERT INTO secrets VALUES (3) RETURNING code",
    ] {
        let error = fails(&mut session, sql);
        assert_eq!(error.state(), SqlState::UNDEFINED_COLUMN, "{sql}: {error}");
    }
    let created = refused(
        &mut session,
        "CREATE TABLE secrets2 (id integer PRIMARY KEY)",
    );
    assert!(
        created.starts_with("cannot change \"secrets2\""),
        "{created}"
    );
    session.close().unwrap();
    assert_eq!(
        shown(
            &store,
            "SELECT id, coalesce(code, '-') AS code FROM secrets ORDER BY id"
        ),
        "id|code\n1|swordfish\n2|-\nSELECT 2"
    );
    let limited = ConfinedStore {
        store: &store,
        tables: VisibleTables::new(["drafts(id)"]),
    };
    let mut session = Session::new(&limited);
    let message = refused(&mut session, "CREATE TABLE drafts (id integer PRIMARY KEY)");
    assert!(message.starts_with("cannot change \"drafts\""), "{message}");
}

fn staff() -> MemoryStore {
    let store = MemoryStore::default();
    let mut owner = Session::new(&store);
    runs(
        &mut owner,
        "CREATE TABLE staff (
             id integer PRIMARY KEY,
             name text NOT NULL,
             team text,
             salary integer NOT NULL DEFAULT 1000 CHECK (salary > 0),
             badge text UNIQUE,
             note text DEFAULT 'none',
             bonus integer,
             CHECK (bonus IS NULL OR bonus < salary)
         );
         CREATE INDEX staff_by_team_and_salary ON staff (team, salary);
         CREATE INDEX staff_by_note ON staff (note);
         INSERT INTO staff VALUES (1, 'Ada', 'core', 9000, 'b-1', 'secret note', 10)",
    );
    owner.close().unwrap();
    store
}

fn staff_limited(store: &MemoryStore) -> ConfinedStore<'_> {
    ConfinedStore {
        store,
        tables: VisibleTables::new(["staff(id,name,team,bonus)"]),
    }
}

fn index_entries(store: &MemoryStore, index: &str) -> usize {
    let read = store.begin_read().unwrap();
    let table = crate::encoding::TableDefinition::decode(
        "staff",
        &read
            .get(&crate::encoding::table_key("staff"))
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let id = table
        .indexes
        .iter()
        .find(|candidate| candidate.name == index)
        .unwrap()
        .id;
    let prefix = crate::encoding::index_prefix(table.id, id);
    let mut entries = 0;
    read.scan(
        &prefix,
        &crate::encoding::prefix_end(&prefix),
        crate::store::ScanDirection::Forward,
        &mut |_, _| {
            entries += 1;
            Ok(true)
        },
    )
    .unwrap();
    entries
}

#[test]
fn a_new_row_gives_hidden_columns_their_defaults_and_a_change_keeps_their_values() {
    let store = staff();
    let limited = staff_limited(&store);
    let mut session = Session::new(&limited);
    runs(
        &mut session,
        "INSERT INTO staff (id, name, team) VALUES (2, 'Grace', 'core')",
    );
    runs(&mut session, "UPDATE staff SET name = 'Ida' WHERE id = 1");
    runs(
        &mut session,
        "INSERT INTO staff (id, name) VALUES (2, 'Grace') ON CONFLICT (id) DO UPDATE SET name = 'Grace Hopper'",
    );
    let mut returned = Vec::new();
    session
        .execute(
            "UPDATE staff SET name = 'Ada' WHERE id = 1 RETURNING *",
            &mut |result| returned.push(render(&result)),
        )
        .unwrap();
    assert_eq!(
        returned.last().unwrap(),
        "id|name|team|bonus\n1|Ada|core|10\nUPDATE 1"
    );
    session.close().unwrap();
    assert_eq!(
        shown(
            &store,
            "SELECT id, name, team, salary, coalesce(badge, '-') AS badge, note, coalesce(bonus, 0) AS bonus FROM staff ORDER BY id"
        ),
        "id|name|team|salary|badge|note|bonus\n1|Ada|core|9000|b-1|secret note|10\n2|Grace Hopper|core|1000|-|none|0\nSELECT 2"
    );
    assert_eq!(index_entries(&store, "staff_by_team_and_salary"), 2);
    assert_eq!(index_entries(&store, "staff_by_note"), 2);
    assert_eq!(
        shown(
            &store,
            "SELECT id FROM staff WHERE team = 'core' AND salary = 1000"
        ),
        "id\n2\nSELECT 1"
    );
    assert_eq!(
        shown(&store, "SELECT id FROM staff WHERE note = 'none'"),
        "id\n2\nSELECT 1"
    );
}

#[test]
fn a_change_to_a_listed_column_moves_the_hidden_index_entries_with_it() {
    let store = staff();
    let limited = staff_limited(&store);
    let mut session = Session::new(&limited);
    runs(&mut session, "UPDATE staff SET team = 'ops' WHERE id = 1");
    session.close().unwrap();
    assert_eq!(index_entries(&store, "staff_by_team_and_salary"), 1);
    assert_eq!(
        shown(
            &store,
            "SELECT id FROM staff WHERE team = 'ops' AND salary = 9000"
        ),
        "id\n1\nSELECT 1"
    );
    assert_eq!(
        shown(&store, "SELECT count(*) FROM staff WHERE team = 'core'"),
        "count\n0\nSELECT 1"
    );
}

#[test]
fn a_write_whose_outcome_would_test_a_hidden_value_is_refused_whatever_that_value_is() {
    let store = staff();
    let limited = staff_limited(&store);
    let mut session = Session::new(&limited);
    let tied = "cannot change column \"bonus\" of \"staff\": a check, unique index or foreign key ties it to a column this session cannot see (--tables), so the change would test that column's value";
    for sql in [
        "UPDATE staff SET bonus = 5 WHERE id = 1",
        "UPDATE staff SET bonus = 100000 WHERE id = 1",
        "UPDATE staff SET bonus = NULL WHERE id = 1",
    ] {
        assert_eq!(refused(&mut session, sql), tied, "{sql}");
    }
    runs(&mut session, "UPDATE staff SET bonus = 10 WHERE id = 1");
    let error = fails(
        &mut session,
        "INSERT INTO staff (id, name, bonus) VALUES (3, 'Hedy', 5000)",
    );
    assert_eq!(error.state(), SqlState::CHECK_VIOLATION, "{error}");
    assert_eq!(
        error.message(),
        "new row for relation \"staff\" violates a check over a column this session cannot see"
    );
    assert!(error.detail().is_none(), "{error}");
    runs(
        &mut session,
        "INSERT INTO staff (id, name, bonus) VALUES (3, 'Hedy', 999)",
    );
    session.close().unwrap();
    assert_eq!(
        shown(&store, "SELECT bonus, salary FROM staff WHERE id = 1"),
        "bonus|salary\n10|9000\nSELECT 1"
    );
    assert_eq!(
        shown(&store, "SELECT salary, bonus FROM staff WHERE id = 3"),
        "salary|bonus\n1000|999\nSELECT 1"
    );
}

#[test]
fn hidden_columns_that_need_a_generated_value_or_a_check_against_other_rows_refuse_inserts() {
    let store = MemoryStore::default();
    let mut owner = Session::new(&store);
    runs(
        &mut owner,
        "CREATE SEQUENCE counted_seq;
         CREATE TABLE counted (id integer PRIMARY KEY, name text, seq integer NOT NULL DEFAULT nextval('counted_seq'));
         CREATE TABLE coded (id integer PRIMARY KEY, name text, code text UNIQUE DEFAULT 'x');
         CREATE TABLE required (id integer PRIMARY KEY, name text, secret text NOT NULL);
         CREATE TABLE owners (id integer PRIMARY KEY);
         CREATE TABLE owned (id integer PRIMARY KEY, name text, owner integer DEFAULT 1 REFERENCES owners (id));
         CREATE FUNCTION pick() RETURNS text LANGUAGE sql AS $$ SELECT 'p' $$;
         CREATE TABLE picked (id integer PRIMARY KEY, name text, choice text DEFAULT pick());
         INSERT INTO owners VALUES (1);
         INSERT INTO coded VALUES (1, 'a', 'x')",
    );
    owner.close().unwrap();
    let limited = ConfinedStore {
        store: &store,
        tables: VisibleTables::new([
            "counted(id,name)",
            "coded(id,name)",
            "required(id,name)",
            "owned(id,name)",
            "picked(id,name)",
        ]),
    };
    let mut session = Session::new(&limited);
    for (sql, reason) in [
        (
            "INSERT INTO counted VALUES (1, 'a')",
            "a column this session cannot see takes generated values (an identity, a serial or a sequence default)",
        ),
        (
            "INSERT INTO coded VALUES (2, 'b')",
            "a unique index over a column this session cannot see would be checked against other rows",
        ),
        (
            "INSERT INTO owned VALUES (1, 'a')",
            "a foreign key over a column this session cannot see would be checked against its table",
        ),
        (
            "INSERT INTO picked VALUES (1, 'a')",
            "the default of a column this session cannot see calls a routine, holds a subquery or cannot be read",
        ),
    ] {
        let message = refused(&mut session, sql);
        let table = sql.split_whitespace().nth(2).unwrap();
        assert_eq!(
            message,
            format!("cannot insert into \"{table}\": {reason} (--tables)"),
            "{sql}"
        );
    }
    let error = fails(&mut session, "INSERT INTO required VALUES (1, 'a')");
    assert_eq!(error.state(), SqlState::NOT_NULL_VIOLATION, "{error}");
    assert_eq!(
        error.message(),
        "null value in a column of relation \"required\" that this session cannot see violates not-null constraint"
    );
    runs(
        &mut session,
        "UPDATE coded SET name = 'renamed' WHERE id = 1",
    );
    session.close().unwrap();
    for table in ["counted", "required", "owned", "picked"] {
        assert_eq!(
            shown(&store, &format!("SELECT count(*) FROM {table}")),
            "count\n0\nSELECT 1",
            "{table}"
        );
    }
    assert_eq!(
        shown(&store, "SELECT id, name, code FROM coded"),
        "id|name|code\n1|renamed|x\nSELECT 1"
    );
}

#[test]
fn a_rolled_back_savepoint_undoes_a_write_to_a_table_listed_with_columns() {
    let store = staff();
    let limited = staff_limited(&store);
    let mut session = Session::new(&limited);
    runs(&mut session, "BEGIN");
    runs(&mut session, "SAVEPOINT before");
    runs(
        &mut session,
        "INSERT INTO staff (id, name, team) VALUES (4, 'Kay', 'web')",
    );
    runs(&mut session, "UPDATE staff SET team = 'ops' WHERE id = 1");
    runs(&mut session, "ROLLBACK TO SAVEPOINT before");
    runs(
        &mut session,
        "INSERT INTO staff (id, name, team) VALUES (5, 'Lin', 'web')",
    );
    runs(&mut session, "COMMIT");
    session.close().unwrap();
    assert_eq!(
        shown(
            &store,
            "SELECT id, team, salary, note FROM staff ORDER BY id"
        ),
        "id|team|salary|note\n1|core|9000|secret note\n5|web|1000|none\nSELECT 2"
    );
    assert_eq!(index_entries(&store, "staff_by_team_and_salary"), 2);
    assert_eq!(index_entries(&store, "staff_by_note"), 2);
    assert_eq!(
        shown(&store, "SELECT count(*) FROM staff WHERE team = 'ops'"),
        "count\n0\nSELECT 1"
    );
}

#[test]
fn a_view_listed_with_columns_shows_only_those_and_cannot_be_changed() {
    let store = staff();
    let mut owner = Session::new(&store);
    runs(
        &mut owner,
        "CREATE VIEW roster AS SELECT id, name, salary FROM staff",
    );
    owner.close().unwrap();
    let limited = ConfinedStore {
        store: &store,
        tables: VisibleTables::new(["staff", "roster(id,name)"]),
    };
    let mut session = Session::new(&limited);
    let mut outputs = Vec::new();
    session
        .execute("SELECT * FROM roster", &mut |result| {
            outputs.push(render(&result))
        })
        .unwrap();
    assert_eq!(outputs.last().unwrap(), "id|name\n1|Ada\nSELECT 1");
    let error = fails(&mut session, "SELECT salary FROM roster");
    assert_eq!(error.state(), SqlState::UNDEFINED_COLUMN, "{error}");
    assert_eq!(
        refused(
            &mut session,
            "CREATE OR REPLACE VIEW roster AS SELECT id, name, salary, badge FROM staff"
        ),
        "cannot change view \"roster\": this session may read only some of its columns (--tables)"
    );
    refused(&mut session, "DROP VIEW roster");
    fails(
        &mut session,
        "INSERT INTO roster (id, name) VALUES (9, 'Zed')",
    );
    session.close().unwrap();
    assert_eq!(
        shown(&store, "SELECT count(*) FROM staff"),
        "count\n1\nSELECT 1"
    );
}

#[test]
fn a_row_whose_foreign_key_points_at_an_unlisted_table_is_checked_without_showing_that_table() {
    let store = MemoryStore::default();
    let mut owner = Session::new(&store);
    runs(
        &mut owner,
        "CREATE TABLE customers (id integer PRIMARY KEY, card text);
         CREATE TABLE orders (id integer PRIMARY KEY, customer integer REFERENCES customers (id), note text);
         INSERT INTO customers VALUES (1, '4111-1111'), (2, '5500-0000');
         INSERT INTO orders VALUES (1, 1, 'first')",
    );
    owner.close().unwrap();
    let limited = ConfinedStore {
        store: &store,
        tables: VisibleTables::new(["orders"]),
    };
    let mut session = Session::new(&limited);
    runs(&mut session, "INSERT INTO orders VALUES (2, 2, 'second')");
    runs(&mut session, "INSERT INTO orders VALUES (3, NULL, 'none')");
    runs(&mut session, "UPDATE orders SET customer = 1 WHERE id = 2");
    runs(&mut session, "DELETE FROM orders WHERE id = 3");
    for sql in [
        "INSERT INTO orders VALUES (4, 99, 'missing')",
        "UPDATE orders SET customer = 99 WHERE id = 1",
    ] {
        let error = fails(&mut session, sql);
        assert_eq!(
            error.state(),
            SqlState::FOREIGN_KEY_VIOLATION,
            "{sql}: {error}"
        );
        let text = format!("{error} {:?}", error.detail());
        assert!(!text.contains("4111"), "{sql}: {text}");
        assert!(!text.contains("card"), "{sql}: {text}");
    }
    missing(&mut session, "SELECT * FROM customers");
    session.close().unwrap();
    let mut mysql = Session::with_dialect(&limited, crate::Dialect::MySql);
    let error = fails(&mut mysql, "INSERT INTO orders VALUES (5, 77, 'mysql')");
    assert!(
        error.message().contains("REFERENCES `customers`)"),
        "{error}"
    );
    assert!(!error.message().contains("`id`)"), "{error}");
    mysql.close().unwrap();
    let mut sqlite = Session::with_dialect(&limited, crate::Dialect::Sqlite);
    runs(&mut sqlite, "PRAGMA foreign_keys = ON");
    let error = fails(&mut sqlite, "INSERT INTO orders VALUES (6, 77, 'sqlite')");
    assert_eq!(error.message(), "FOREIGN KEY constraint failed");
    sqlite.close().unwrap();
    assert_eq!(
        shown(&store, "SELECT id, customer, note FROM orders ORDER BY id"),
        "id|customer|note\n1|1|first\n2|1|second\nSELECT 2"
    );
    assert_eq!(
        shown(&store, "SELECT id, card FROM customers ORDER BY id"),
        "id|card\n1|4111-1111\n2|5500-0000\nSELECT 2"
    );
}

#[test]
fn joined_updates_and_deletes_and_select_into_cannot_read_an_unlisted_table() {
    let store = shop();
    let limited = confined(&store);
    let mut session = Session::new(&limited);
    for sql in [
        "UPDATE items SET name = secrets.code FROM secrets WHERE secrets.id = items.id",
        "DELETE FROM items USING secrets WHERE secrets.id = items.id",
        "SELECT code INTO drafts FROM secrets",
    ] {
        let error = fails(&mut session, sql);
        assert_eq!(error.state(), SqlState::UNDEFINED_TABLE, "{sql}: {error}");
        assert!(!error.message().contains("swordfish"), "{sql}: {error}");
    }
    let created = refused(&mut session, "SELECT name INTO copied FROM items");
    assert!(created.starts_with("cannot change \"copied\""), "{created}");
    runs(&mut session, "SELECT name INTO drafts FROM items");
    session.close().unwrap();
    let mut mysql = Session::with_dialect(&limited, crate::Dialect::MySql);
    for sql in [
        "UPDATE items, secrets SET items.name = secrets.code WHERE items.id = secrets.id",
        "DELETE items FROM items JOIN secrets ON secrets.id = items.id",
    ] {
        let error = fails(&mut mysql, sql);
        assert!(
            matches!(error.state().code(), "42P01" | "42S02"),
            "{sql}: {error}"
        );
        assert!(!error.message().contains("swordfish"), "{sql}: {error}");
    }
    mysql.close().unwrap();
    let mut sqlserver = Session::with_dialect(&limited, crate::Dialect::SqlServer);
    for sql in [
        "UPDATE items SET name = 'x' OUTPUT inserted.id INTO secrets WHERE id = 0",
        "SELECT code INTO log FROM secrets",
        "UPDATE i SET name = s.code FROM items i JOIN secrets s ON s.id = i.id",
        "MERGE INTO items USING secrets ON items.id = secrets.id WHEN NOT MATCHED THEN INSERT (name) VALUES (secrets.code);",
    ] {
        let error = fails(&mut sqlserver, sql);
        assert!(!error.message().contains("swordfish"), "{sql}: {error}");
    }
    sqlserver.close().unwrap();
    assert_eq!(
        shown(&store, "SELECT id, code FROM secrets"),
        "id|code\n1|swordfish\nSELECT 1"
    );
    assert_eq!(
        shown(&store, "SELECT count(*) FROM items"),
        "count\n0\nSELECT 1"
    );
    assert_eq!(
        shown(&store, "SELECT count(*) FROM log"),
        "count\n0\nSELECT 1"
    );
}

#[test]
fn a_foreign_key_on_a_hidden_column_keeps_the_listed_parent_from_losing_rows() {
    let store = shop();
    let limited = ConfinedStore {
        store: &store,
        tables: VisibleTables::new(["children(id)", "parents"]),
    };
    let links = limited
        .tables
        .outside_links(&store.begin_read().unwrap())
        .unwrap();
    assert!(
        links.iter().any(|link| link.listed == "parents"
            && link.outside == "children"
            && link.kind == OutsideLinkKind::ReferencedBy),
        "{links:?}"
    );
    let mut session = Session::new(&limited);
    runs(&mut session, "INSERT INTO parents VALUES (2)");
    for sql in [
        "DELETE FROM parents WHERE id = 1",
        "DELETE FROM parents",
        "UPDATE parents SET id = 10 WHERE id = 1",
    ] {
        let message = refused(&mut session, sql);
        assert!(
            message.starts_with(
                "cannot delete a row of \"parents\" or change a column a foreign key points at"
            ),
            "{sql}: {message}"
        );
    }
    fails(&mut session, "TRUNCATE parents");
    session.close().unwrap();
    assert_eq!(
        shown(&store, "SELECT id, parent FROM children"),
        "id|parent\n1|1\nSELECT 1"
    );
}

#[test]
fn a_policy_calling_a_user_routine_or_holding_a_subquery_cannot_be_written() {
    let store = shop();
    let mut owner = Session::new(&store);
    runs(
        &mut owner,
        "CREATE FUNCTION wipe_secrets() RETURNS integer LANGUAGE plpgsql AS $$ BEGIN UPDATE secrets SET code = 'gone'; RETURN 1; END $$",
    );
    runs(
        &mut owner,
        "CREATE FUNCTION always() RETURNS boolean LANGUAGE sql AS $$ SELECT true $$",
    );
    runs(
        &mut owner,
        "CREATE POLICY owners_own ON items USING (always())",
    );
    owner.close().unwrap();
    let limited = confined(&store);
    let mut session = Session::new(&limited);
    assert_eq!(
        refused(
            &mut session,
            "CREATE POLICY wipe ON items USING (wipe_secrets() = 1)"
        ),
        "cannot write a row-security policy that calls the routine \"wipe_secrets\": it runs in every later session that reads or writes the table, where --tables does not confine it"
    );
    assert_eq!(
        refused(
            &mut session,
            "CREATE POLICY checked ON items FOR INSERT WITH CHECK (wipe_secrets() = 1)"
        ),
        "cannot write a row-security policy that calls the routine \"wipe_secrets\": it runs in every later session that reads or writes the table, where --tables does not confine it"
    );
    assert_eq!(
        refused(
            &mut session,
            "CREATE POLICY nested ON items USING (EXISTS (SELECT wipe_secrets()))"
        ),
        "cannot write a row-security policy with a subquery: it runs in every later session that reads or writes the table, where --tables does not confine it"
    );
    runs(&mut session, "CREATE POLICY plain ON items USING (id > 0)");
    runs(&mut session, "INSERT INTO items (name) VALUES ('kept')");
    session.close().unwrap();
    let mut names: Vec<String> =
        crate::row_security::policies(&store.begin_read().unwrap(), "items")
            .unwrap()
            .into_iter()
            .map(|policy| policy.name)
            .collect();
    names.sort();
    assert_eq!(names, ["owners_own", "plain"]);
    assert_eq!(
        shown(&store, "SELECT code FROM secrets"),
        "code\nswordfish\nSELECT 1"
    );
}
