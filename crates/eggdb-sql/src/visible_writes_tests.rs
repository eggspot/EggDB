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
    let referenced = refused(&mut session, "INSERT INTO parents VALUES (2)");
    assert!(
        referenced.contains("table \"children\" has a foreign key to \"parents\""),
        "{referenced}"
    );
    refused(&mut session, "DELETE FROM parents WHERE id = 1");
    refused(&mut session, "TRUNCATE parents");
    refused(&mut session, "DROP TABLE parents");
    fails(&mut session, "INSERT INTO orders VALUES (2, 1, 'second')");
    let referencing = refused(
        &mut session,
        "UPDATE orders SET note = 'changed' WHERE id = 1",
    );
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
        shown(&store, "SELECT count(*) FROM parents"),
        "count\n1\nSELECT 1"
    );
    assert_eq!(shown(&store, "SELECT v FROM peek"), "v\n1\nSELECT 1");
    assert_eq!(
        shown(&store, "SELECT note FROM orders"),
        "note\nfirst\nSELECT 1"
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
    assert_eq!(links[0].blocked_changes(), "rows and definition");
    let view = links
        .iter()
        .find(|link| link.kind == OutsideLinkKind::ReadByView)
        .unwrap();
    assert_eq!(view.blocked_changes(), "definition");
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
