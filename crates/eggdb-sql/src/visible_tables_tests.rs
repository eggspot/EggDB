use crate::encoding::NAMESPACE;
use crate::memory_store::{MemoryRead, MemoryStore, MemoryWrite};
use crate::store::{KeyValueRead, KeyValueStore, ScanDirection, StoreFailure};
use crate::tests::render;
use crate::{Dialect, Session, SqlError, SqlResult, UnknownTableName, VisibleRead, VisibleTables};

struct ShownStore<'s> {
    store: &'s MemoryStore,
    tables: VisibleTables,
}

impl KeyValueStore for ShownStore<'_> {
    type Read<'r>
        = VisibleRead<MemoryRead>
    where
        Self: 'r;
    type Write<'w>
        = MemoryWrite<'w>
    where
        Self: 'w;

    fn begin_read(&self) -> SqlResult<Self::Read<'_>> {
        self.tables.read(self.store.begin_read()?)
    }

    fn begin_write(&self) -> SqlResult<Self::Write<'_>> {
        Err(SqlError::store(
            StoreFailure::ReadOnly,
            "this test's store only reads",
        ))
    }
}

fn shop() -> MemoryStore {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    session
        .execute(
            "CREATE TABLE users (id serial PRIMARY KEY, name text NOT NULL);
             CREATE TABLE secrets (id integer PRIMARY KEY, owner integer REFERENCES users (id), secret text);
             CREATE INDEX secrets_by_text ON secrets (secret);
             CREATE TABLE notes (id integer PRIMARY KEY, secret_id integer REFERENCES secrets (id));
             CREATE VIEW user_names AS SELECT name FROM users;
             CREATE VIEW leaky AS SELECT secret FROM secrets;
             CREATE SCHEMA archive;
             CREATE TABLE archive.orders (id integer PRIMARY KEY, total integer);
             CREATE SEQUENCE loose_seq;
             COMMENT ON TABLE secrets IS 'the hidden one';
             INSERT INTO users (name) VALUES ('Ada'), ('Grace');
             INSERT INTO secrets VALUES (1, 1, 'swordfish');
             INSERT INTO notes VALUES (1, 1);
             INSERT INTO archive.orders VALUES (1, 10)",
            &mut |_| {},
        )
        .unwrap();
    session.close().unwrap();
    store
}

fn listed() -> VisibleTables {
    VisibleTables::new([
        "users",
        "user_names",
        "leaky",
        "notes",
        "public.notes",
        "archive.orders",
    ])
}

fn shown(store: &MemoryStore) -> ShownStore<'_> {
    ShownStore {
        store,
        tables: listed(),
    }
}

fn last(session: &mut Session<'_>, sql: &str) -> String {
    let mut outputs = Vec::new();
    match session.execute(sql, &mut |result| outputs.push(render(&result))) {
        Ok(()) => outputs.last().cloned().unwrap_or_default(),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn fails(session: &mut Session<'_>, sql: &str) -> SqlError {
    let mut outputs = Vec::new();
    match session.execute(sql, &mut |result| outputs.push(render(&result))) {
        Ok(()) => panic!("{sql} succeeded: {outputs:?}"),
        Err(error) => error,
    }
}

#[test]
fn names_keep_their_order_drop_repeats_and_take_the_default_schema_bare() {
    assert_eq!(
        listed().names(),
        vec!["users", "user_names", "leaky", "notes", "archive.orders"]
    );
    assert_eq!(
        VisibleTables::new(["dbo.users", "public.users", "users", "a.b.c"]).names(),
        vec!["users", "a.b.c"]
    );
}

#[test]
fn a_hidden_table_reads_as_missing_by_name_by_schema_and_through_a_view() {
    let store = shop();
    let shown = shown(&store);
    let mut session = Session::new(&shown);
    assert_eq!(
        last(&mut session, "SELECT name FROM users ORDER BY id"),
        "name\nAda\nGrace\nSELECT 2"
    );
    assert_eq!(
        last(&mut session, "SELECT name FROM user_names ORDER BY name"),
        "name\nAda\nGrace\nSELECT 2"
    );
    assert_eq!(
        last(&mut session, "SELECT total FROM archive.orders"),
        "total\n10\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT count(*) FROM notes"),
        "count\n1\nSELECT 1"
    );
    for sql in [
        "SELECT * FROM secrets",
        "SELECT * FROM public.secrets",
        "SELECT * FROM \"secrets\"",
        "SELECT u.name FROM users u JOIN secrets s ON s.owner = u.id",
        "SELECT * FROM users WHERE id IN (SELECT owner FROM secrets)",
        "WITH s AS (SELECT * FROM secrets) SELECT * FROM s",
        "SELECT * FROM leaky",
    ] {
        let error = fails(&mut session, sql);
        assert_eq!(error.state().code(), "42P01", "{sql}: {error}");
    }
    let missing = fails(&mut session, "SELECT * FROM nothing_here");
    let hidden = fails(&mut session, "SELECT * FROM secrets");
    assert_eq!(
        missing.message().replace("nothing_here", "secrets"),
        hidden.message()
    );
}

#[test]
fn every_catalog_leaves_out_what_is_hidden() {
    let store = shop();
    let shown = shown(&store);
    let mut session = Session::new(&shown);
    assert_eq!(
        last(
            &mut session,
            "SELECT relname FROM pg_catalog.pg_class WHERE relname IN ('users', 'secrets', 'leaky', 'notes', 'users_id_seq', 'loose_seq', 'secrets_by_text') ORDER BY relname"
        ),
        "relname\nleaky\nnotes\nusers\nusers_id_seq\nSELECT 4"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT table_schema, table_name FROM information_schema.tables WHERE table_schema IN ('public', 'archive') ORDER BY table_schema, table_name"
        ),
        "table_schema|table_name\narchive|orders\npublic|leaky\npublic|notes\npublic|user_names\npublic|users\nSELECT 5"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT count(*) FROM pg_catalog.pg_description WHERE description = 'the hidden one'"
        ),
        "count\n0\nSELECT 1"
    );
    let relations: Vec<String> = session
        .relations()
        .unwrap()
        .iter()
        .map(|relation| format!("{}.{}", relation.schema, relation.name))
        .collect();
    assert_eq!(
        relations,
        vec![
            "archive.orders",
            "public.leaky",
            "public.notes",
            "public.user_names",
            "public.users"
        ]
    );
    let mut sqlserver = Session::with_dialect(&shown, Dialect::SqlServer);
    let mut rendered = Vec::new();
    sqlserver
        .execute(
            "SELECT name FROM sys.tables WHERE name IN ('users', 'secrets') ORDER BY name",
            &mut |result| rendered.push(render(&result)),
        )
        .unwrap();
    assert_eq!(rendered.last().unwrap(), "name\nusers\nSELECT 1");
    rendered.clear();
    sqlserver
        .execute(
            "SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_NAME IN ('users', 'secrets')",
            &mut |result| rendered.push(render(&result)),
        )
        .unwrap();
    assert_eq!(rendered.last().unwrap(), "table_name\nusers\nSELECT 1");
    assert!(
        sqlserver
            .execute("SELECT * FROM dbo.secrets", &mut |_| {})
            .is_err()
    );
    let mut oracle = Session::with_dialect(&shown, Dialect::Oracle);
    rendered.clear();
    oracle
        .execute(
            "SELECT table_name FROM all_tables WHERE table_name IN ('USERS', 'SECRETS')",
            &mut |result| rendered.push(render(&result)),
        )
        .unwrap();
    assert_eq!(rendered.last().unwrap(), "TABLE_NAME\nUSERS\nSELECT 1");
    rendered.clear();
    oracle
        .execute(
            "SELECT sequence_name FROM all_sequences WHERE sequence_name IN ('LOOSE_SEQ', 'USERS_ID_SEQ')",
            &mut |result| rendered.push(render(&result)),
        )
        .unwrap();
    assert_eq!(rendered.last().unwrap(), "SEQUENCE_NAME\nSELECT 0");
    assert!(
        oracle
            .execute("SELECT * FROM secrets", &mut |_| {})
            .is_err()
    );
}

#[test]
fn nothing_of_a_hidden_table_can_be_read_by_any_key() {
    let store = shop();
    let read = listed().read(store.begin_read().unwrap()).unwrap();
    let mut keys = 0;
    read.scan(
        NAMESPACE,
        &crate::encoding::prefix_end(NAMESPACE),
        ScanDirection::Forward,
        &mut |key, value| {
            keys += 1;
            let holds = |bytes: &[u8], wanted: &[u8]| {
                bytes.windows(wanted.len()).any(|window| window == wanted)
            };
            assert!(!holds(key, b"secrets"), "{key:?}");
            assert!(!holds(key, b"loose_seq"), "{key:?}");
            assert!(!holds(value, b"swordfish"), "{key:?}");
            assert!(!holds(value, b"the hidden one"), "{key:?}");
            Ok(true)
        },
    )
    .unwrap();
    assert!(keys > 0);
    assert!(
        read.get(&crate::encoding::table_key("secrets"))
            .unwrap()
            .is_none()
    );
    let mut everything = 0;
    let mut hidden = 0;
    let all = VisibleRead::everything(store.begin_read().unwrap());
    all.scan(
        NAMESPACE,
        &crate::encoding::prefix_end(NAMESPACE),
        ScanDirection::Forward,
        &mut |key, _| {
            everything += 1;
            if read.get(key).unwrap().is_none() {
                hidden += 1;
            }
            Ok(true)
        },
    )
    .unwrap();
    assert_eq!(everything, keys + hidden);
    assert!(hidden > 0);
}

#[test]
fn unknown_names_are_reported_with_the_stored_name_when_only_the_case_differs() {
    let store = shop();
    let read = store.begin_read().unwrap();
    let tables = VisibleTables::new(["users", "USERS", "ordres", "archive.orders", "loose_seq"]);
    assert_eq!(
        tables.unknown_names(&read).unwrap(),
        vec![
            UnknownTableName {
                name: "USERS".to_owned(),
                stored_as: Some("users".to_owned()),
            },
            UnknownTableName {
                name: "ordres".to_owned(),
                stored_as: None,
            },
        ]
    );
}

#[test]
fn one_listed_table_is_read_and_the_store_itself_keeps_everything() {
    let store = shop();
    let only_users = ShownStore {
        store: &store,
        tables: VisibleTables::new(["users"]),
    };
    let mut session = Session::new(&only_users);
    assert_eq!(
        last(&mut session, "SELECT count(*) FROM users"),
        "count\n2\nSELECT 1"
    );
    let mut plain = Session::new(&store);
    let mut rendered = Vec::new();
    plain
        .execute("SELECT secret FROM leaky", &mut |result| {
            rendered.push(render(&result));
        })
        .unwrap();
    assert_eq!(rendered.last().unwrap(), "secret\nswordfish\nSELECT 1");
}

fn narrowed() -> VisibleTables {
    VisibleTables::new(["users", "secrets(id,owner)", "notes", "secret_view(secret)"])
}

fn narrowed_store(store: &MemoryStore) -> ShownStore<'_> {
    ShownStore {
        store,
        tables: narrowed(),
    }
}

fn narrowed_shop() -> MemoryStore {
    let store = shop();
    let mut session = Session::new(&store);
    session
        .execute(
            "CREATE VIEW secret_view AS SELECT secret FROM secrets;
             CREATE VIEW owners AS SELECT id, owner FROM secrets;
             COMMENT ON COLUMN secrets.secret IS 'the hidden column';
             ANALYZE secrets",
            &mut |_| {},
        )
        .unwrap();
    session.close().unwrap();
    store
}

#[test]
fn a_table_listed_with_columns_shows_only_those_columns() {
    let store = narrowed_shop();
    let limited = narrowed_store(&store);
    let mut session = Session::new(&limited);
    assert_eq!(
        last(&mut session, "SELECT * FROM secrets"),
        "id|owner\n1|1\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT s.* FROM secrets s JOIN users u ON u.id = s.owner"
        ),
        "id|owner\n1|1\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT count(*) FROM notes JOIN secrets ON secrets.id = notes.secret_id"
        ),
        "count\n1\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT column_name FROM information_schema.columns WHERE table_name = 'secrets' ORDER BY ordinal_position"
        ),
        "column_name\nid\nowner\nSELECT 2"
    );
    for sql in [
        "SELECT secret FROM secrets",
        "SELECT s.secret FROM secrets s",
        "SELECT id FROM secrets WHERE secret = 'swordfish'",
        "SELECT * FROM users WHERE name IN (SELECT secret FROM secrets)",
        "WITH s AS (SELECT secret FROM secrets) SELECT * FROM s",
        "SELECT upper(secret) FROM secrets",
        "SELECT count(*) FROM secrets GROUP BY secret",
        "SELECT id FROM secrets ORDER BY secret",
    ] {
        let error = fails(&mut session, sql);
        assert_eq!(error.state().code(), "42703", "{sql}: {error}");
    }
    for sql in ["SELECT * FROM secret_view", "SELECT * FROM owners"] {
        let error = fails(&mut session, sql);
        assert_eq!(error.state().code(), "42P01", "{sql}: {error}");
    }
}

#[test]
fn nothing_of_a_left_out_column_can_be_read_by_any_key() {
    let store = narrowed_shop();
    let read = narrowed().read(store.begin_read().unwrap()).unwrap();
    let mut rows = 0;
    read.scan(
        NAMESPACE,
        &crate::encoding::prefix_end(NAMESPACE),
        ScanDirection::Forward,
        &mut |key, value| {
            let holds = |bytes: &[u8], wanted: &[u8]| {
                bytes.windows(wanted.len()).any(|window| window == wanted)
            };
            assert!(!holds(value, b"swordfish"), "{key:?}");
            assert!(!holds(key, b"swordfish"), "{key:?}");
            assert!(!holds(value, b"the hidden column"), "{key:?}");
            assert!(!holds(key, b"secrets_by_text"), "{key:?}");
            assert!(!holds(value, b"secrets_by_text"), "{key:?}");
            rows += 1;
            Ok(true)
        },
    )
    .unwrap();
    assert!(rows > 0);
    let definition = read
        .get(&crate::encoding::table_key("secrets"))
        .unwrap()
        .unwrap();
    let table = crate::encoding::TableDefinition::decode("secrets", &definition).unwrap();
    let names: Vec<&str> = table
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect();
    assert_eq!(names, ["id", "owner"]);
    assert!(table.indexes.is_empty());
}

#[test]
fn column_lists_that_cannot_be_served_are_reported_and_hide_their_table() {
    let store = narrowed_shop();
    let tables = VisibleTables::new([
        "secrets(owner,secret)",
        "users(id,nme)",
        "user_names(name)",
        "notes()",
        "drafts(id)",
    ]);
    assert_eq!(
        tables.names(),
        vec![
            "secrets(owner,secret)",
            "users(id,nme)",
            "user_names(name)",
            "notes()",
            "drafts(id)",
        ]
    );
    assert!(tables.limits_columns());
    assert!(!listed().limits_columns());
    let reasons: Vec<(String, String)> = tables
        .unservable_column_lists(&store.begin_read().unwrap())
        .unwrap()
        .into_iter()
        .map(|list| (list.table, list.reason))
        .collect();
    assert_eq!(
        reasons,
        vec![
            (
                "secrets".to_owned(),
                "leaves out its primary key column \"id\": list it too".to_owned()
            ),
            ("users".to_owned(), "has no column \"nme\"".to_owned()),
            (
                "user_names".to_owned(),
                "is a view: list it without columns, or list the columns of the tables it reads"
                    .to_owned()
            ),
            ("notes".to_owned(), "has an empty column list".to_owned()),
            (
                "drafts".to_owned(),
                "does not exist yet: a table listed with columns must exist when the session starts"
                    .to_owned()
            ),
        ]
    );
    let limited = ShownStore {
        store: &store,
        tables,
    };
    let mut session = Session::new(&limited);
    for sql in [
        "SELECT * FROM secrets",
        "SELECT * FROM user_names",
        "SELECT * FROM notes",
    ] {
        let error = fails(&mut session, sql);
        assert_eq!(error.state().code(), "42P01", "{sql}: {error}");
    }
    assert_eq!(
        last(&mut session, "SELECT * FROM users ORDER BY id"),
        "id\n1\n2\nSELECT 2"
    );
}
