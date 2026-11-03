//! A server started with `--tables` over an in-memory database: only the listed tables and
//! views exist for the agent, in every tool, dialect and catalog, the key-value tools are
//! withdrawn, the instructions and the audit say so, and the gap between calls holds. A table
//! listed with columns shows only those and takes inserts and changes to them, a table an
//! unlisted one references takes inserts but not deletes, and a new policy cannot call a user
//! routine.

mod common;

use common::{
    MapOpener, MapStore, TempAudit, call, config, count, error_code, get, parsed, structured,
    tables_config, text_at,
};
use eggdb_mcp::{Server, ServerConfig};
use eggdb_sql::json::{Json, Node};
use eggdb_sql::{Dialect, Session};
use std::time::{Duration, Instant};

const INITIALIZE: &[u8] = br#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test-client","version":"1.0"}}}"#;

fn shop() -> MapStore {
    let store = MapStore::default();
    store.sql(
        Dialect::PostgreSql,
        "CREATE TABLE users (id integer PRIMARY KEY, name text NOT NULL); \
         CREATE TABLE salaries (user_id integer PRIMARY KEY REFERENCES users (id), amount integer); \
         CREATE VIEW names AS SELECT name FROM users; \
         CREATE VIEW pay AS SELECT u.name, s.amount FROM users u JOIN salaries s ON s.user_id = u.id; \
         COMMENT ON TABLE salaries IS 'confidential'; \
         INSERT INTO users VALUES (1, 'Ada'), (2, 'Grace'); \
         INSERT INTO salaries VALUES (1, 100), (2, 200)",
    );
    store.put(b"session:1", b"token");
    store
}

fn limited<'o>(opener: &'o MapOpener, config: ServerConfig) -> Server<'o, MapOpener> {
    let mut server = Server::new(opener, config).unwrap();
    let answer = parsed(&server.handle(INITIALIZE).unwrap());
    assert_eq!(
        text_at(&answer, &["result", "protocolVersion"]),
        "2025-11-25"
    );
    server
}

fn names(node: &Node) -> Vec<String> {
    match &node.value {
        Json::Array(items) => items.iter().map(|item| text_at(item, &["name"])).collect(),
        other => panic!("not an array: {other:?}"),
    }
}

fn sql_error(server: &mut Server<'_, MapOpener>, id: u32, dialect: &str, sql: &str) -> String {
    let arguments = format!(r#"{{"sql":{sql:?},"dialect":"{dialect}"}}"#);
    let answer = call(server, id, "run_sql", &arguments);
    assert_eq!(error_code(&answer), "sql_error", "{dialect} {sql}");
    text_at(structured(&answer), &["error", "message"])
}

fn rows(server: &mut Server<'_, MapOpener>, id: u32, dialect: &str, sql: &str) -> Vec<String> {
    let arguments = format!(r#"{{"sql":{sql:?},"dialect":"{dialect}"}}"#);
    let answer = call(server, id, "run_sql", &arguments);
    assert_eq!(
        text_at(&answer, &["result", "isError"]),
        "false",
        "{answer:?}"
    );
    match &get(structured(&answer), &["rows"]).value {
        Json::Array(rows) => rows.iter().map(|row| text_at(row, &["0"])).collect(),
        other => panic!("not rows: {other:?}"),
    }
}

#[test]
fn only_the_listed_tables_and_views_exist_for_the_agent() {
    let audit = TempAudit::new("tables-only");
    let opener = MapOpener::new(shop());
    let mut server = limited(&opener, tables_config(&audit, &["users", "names", "pay"]));
    let listed = call(&mut server, 1, "list_tables", "{}");
    assert_eq!(names(get(structured(&listed), &["tables"])), vec!["users"]);
    assert_eq!(
        names(get(structured(&listed), &["views"])),
        vec!["names", "pay"]
    );
    let hidden = call(&mut server, 2, "describe_table", r#"{"table":"salaries"}"#);
    let missing = call(&mut server, 3, "describe_table", r#"{"table":"nothing"}"#);
    assert_eq!(error_code(&hidden), error_code(&missing));
    assert_eq!(
        error_code(&call(
            &mut server,
            4,
            "sample_rows",
            r#"{"table":"salaries"}"#
        )),
        error_code(&missing)
    );
    assert_eq!(
        error_code(&call(&mut server, 5, "row_list", r#"{"table":"salaries"}"#)),
        error_code(&call(&mut server, 6, "row_list", r#"{"table":"nothing"}"#))
    );
    let direct = sql_error(&mut server, 7, "postgresql", "SELECT amount FROM salaries");
    let absent = sql_error(&mut server, 8, "postgresql", "SELECT amount FROM nothing");
    assert_eq!(direct.replace("salaries", "nothing"), absent);
    assert!(!sql_error(&mut server, 9, "postgresql", "SELECT * FROM pay").contains("100"));
    sql_error(
        &mut server,
        10,
        "postgresql",
        "EXPLAIN SELECT * FROM salaries",
    );
    sql_error(&mut server, 11, "mysql", "SELECT amount FROM salaries");
    sql_error(&mut server, 12, "sqlite", "SELECT amount FROM salaries");
    sql_error(
        &mut server,
        13,
        "sqlserver",
        "SELECT amount FROM dbo.salaries",
    );
    sql_error(&mut server, 14, "oracle", "SELECT amount FROM salaries");
    assert_eq!(
        rows(
            &mut server,
            15,
            "postgresql",
            "SELECT name FROM names ORDER BY name"
        ),
        vec!["Ada", "Grace"]
    );
    assert_eq!(
        rows(
            &mut server,
            16,
            "postgresql",
            "SELECT relname FROM pg_catalog.pg_class WHERE relname IN ('users', 'salaries', 'names', 'pay') ORDER BY relname"
        ),
        vec!["names", "pay", "users"]
    );
    assert_eq!(
        rows(
            &mut server,
            17,
            "postgresql",
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' ORDER BY table_name"
        ),
        vec!["names", "pay", "users"]
    );
    assert_eq!(
        rows(
            &mut server,
            18,
            "sqlserver",
            "SELECT name FROM sys.objects WHERE name IN ('users', 'salaries') ORDER BY name"
        ),
        vec!["users"]
    );
    assert_eq!(
        rows(
            &mut server,
            19,
            "oracle",
            "SELECT table_name FROM all_tables WHERE table_name IN ('USERS', 'SALARIES')"
        ),
        vec!["USERS"]
    );
    let schema = parsed(
        &server
            .handle(br#"{"jsonrpc":"2.0","id":20,"method":"resources/read","params":{"uri":"eggdb://schema"}}"#)
            .unwrap(),
    );
    let text = text_at(&schema, &["result", "contents", "0", "text"]);
    assert!(text.contains("users"), "{text}");
    assert!(!text.contains("salaries"), "{text}");
    assert!(!text.contains("confidential"), "{text}");
}

#[test]
fn the_key_value_tools_are_withdrawn_and_refused() {
    let audit = TempAudit::new("tables-keys");
    let opener = MapOpener::new(shop());
    let mut server = limited(&opener, tables_config(&audit, &["users"]));
    let listed = parsed(
        &server
            .handle(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .unwrap(),
    );
    let tools = names(get(&listed, &["result", "tools"]));
    assert!(
        !tools.iter().any(|tool| tool.starts_with("kv_")),
        "{tools:?}"
    );
    assert!(tools.iter().any(|tool| tool == "run_sql"), "{tools:?}");
    for (id, tool, arguments) in [
        (2, "kv_get", r#"{"key":"session:1"}"#),
        (3, "kv_scan", r#"{"prefix":"session:"}"#),
        (4, "kv_get", r#"{"key":"s:1","collection":"sessions"}"#),
        (5, "kv_scan", r#"{"collection":"sessions"}"#),
    ] {
        let answer = call(&mut server, id, tool, arguments);
        assert_eq!(error_code(&answer), "limited_to_tables", "{tool}");
        assert_eq!(
            text_at(structured(&answer), &["error", "category"]),
            "needs_grant"
        );
        let shown = format!("{answer:?}");
        assert!(!shown.contains("token"), "{shown}");
    }
    let lines = audit.lines();
    assert!(lines[0].contains(r#""tables":["users"]"#), "{}", lines[0]);
    assert!(lines[0].contains(r#""call_gap_ms":0"#), "{}", lines[0]);
    assert!(
        lines
            .iter()
            .any(|line| line.contains(r#""error_code":"limited_to_tables""#))
    );
}

#[test]
fn the_instructions_tell_the_agent_about_the_limit() {
    let audit = TempAudit::new("tables-instructions");
    let opener = MapOpener::new(shop());
    let mut server = Server::new(&opener, tables_config(&audit, &["users"])).unwrap();
    let answer = parsed(&server.handle(INITIALIZE).unwrap());
    let instructions = text_at(&answer, &["result", "instructions"]);
    assert!(
        instructions.contains("The user limited this session to some tables (--tables)"),
        "{instructions}"
    );
    assert!(!instructions.contains("kv_get and kv_scan read keys"));
    assert!(!instructions.contains("users"), "{instructions}");
}

fn write(server: &mut Server<'_, MapOpener>, id: u32, sql: &str) -> Node {
    let arguments = format!(r#"{{"sql":{sql:?},"confirm":true}}"#);
    call(server, id, "write_sql", &arguments)
}

fn write_refused(server: &mut Server<'_, MapOpener>, id: u32, sql: &str) -> (String, String) {
    let answer = write(server, id, sql);
    assert_eq!(error_code(&answer), "sql_error", "{sql}");
    (
        text_at(structured(&answer), &["error", "sqlstate"]),
        text_at(structured(&answer), &["error", "message"]),
    )
}

#[test]
fn a_writing_agent_limited_to_tables_changes_only_those() {
    let audit = TempAudit::new("tables-writes");
    let store = shop();
    store.sql(
        Dialect::PostgreSql,
        "CREATE TABLE tags (id serial PRIMARY KEY, label text); \
         CREATE TABLE tag_log (id serial PRIMARY KEY, label text); \
         CREATE FUNCTION log_tag() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO tag_log (label) VALUES (NEW.label); RETURN NEW; END $$; \
         CREATE TABLE logged_tags (id integer PRIMARY KEY, label text); \
         CREATE TRIGGER logged AFTER INSERT ON logged_tags FOR EACH ROW EXECUTE FUNCTION log_tag(); \
         CREATE PROCEDURE raise_pay() LANGUAGE plpgsql AS $$ BEGIN UPDATE salaries SET amount = amount * 2; END $$",
    );
    let opener = MapOpener::new(store);
    let mut server = Server::new(
        &opener,
        ServerConfig {
            allow_write: true,
            ..tables_config(&audit, &["users", "tags", "logged_tags", "notes"])
        },
    )
    .unwrap();
    let answer = parsed(&server.handle(INITIALIZE).unwrap());
    let instructions = text_at(&answer, &["result", "instructions"]);
    assert!(
        instructions.contains("you may read and change only those"),
        "{instructions}"
    );
    assert!(!instructions.contains("salaries"), "{instructions}");
    let inserted = write(&mut server, 1, "INSERT INTO tags (label) VALUES ('red')");
    assert_eq!(text_at(structured(&inserted), &["committed"]), "true");
    let created = write(
        &mut server,
        2,
        "CREATE TABLE notes (id serial PRIMARY KEY, body text)",
    );
    assert_eq!(text_at(structured(&created), &["committed"]), "true");
    let noted = write(&mut server, 3, "INSERT INTO notes (body) VALUES ('hi')");
    assert_eq!(text_at(structured(&noted), &["committed"]), "true");
    assert_eq!(
        write_refused(&mut server, 4, "INSERT INTO salaries VALUES (3, 300)").0,
        "42P01"
    );
    let (state, message) = write_refused(&mut server, 5, "DELETE FROM users WHERE id = 2");
    assert_eq!(state, "42501");
    assert_eq!(
        message,
        "cannot delete a row of \"users\" or change a column a foreign key points at: table \"salaries\" has a foreign key to \"users\", and this session may neither change nor check \"salaries\" (--tables)"
    );
    let (state, message) = write_refused(&mut server, 6, "CREATE TABLE elsewhere (id integer)");
    assert_eq!(state, "42501");
    assert!(message.contains("\"elsewhere\""), "{message}");
    assert_eq!(
        write_refused(
            &mut server,
            7,
            "CREATE FUNCTION leak() RETURNS integer LANGUAGE sql AS 'SELECT 1'"
        )
        .0,
        "42501"
    );
    assert_eq!(
        write_refused(&mut server, 8, "INSERT INTO logged_tags VALUES (1, 'blue')").0,
        "42P01"
    );
    assert_eq!(write_refused(&mut server, 9, "CALL raise_pay()").0, "42P01");
    assert_eq!(
        count(
            &opener.store,
            Dialect::PostgreSql,
            "SELECT count(*) FROM tags"
        ),
        1
    );
    assert_eq!(
        count(
            &opener.store,
            Dialect::PostgreSql,
            "SELECT count(*) FROM notes"
        ),
        1
    );
    assert_eq!(
        count(
            &opener.store,
            Dialect::PostgreSql,
            "SELECT count(*) FROM tag_log"
        ),
        0
    );
    assert_eq!(
        count(
            &opener.store,
            Dialect::PostgreSql,
            "SELECT count(*) FROM logged_tags"
        ),
        0
    );
    assert_eq!(
        count(
            &opener.store,
            Dialect::PostgreSql,
            "SELECT count(*) FROM users"
        ),
        2
    );
    assert_eq!(
        count(
            &opener.store,
            Dialect::PostgreSql,
            "SELECT count(*) FROM salaries WHERE amount IN (100, 200)"
        ),
        2
    );
    assert_eq!(
        count(
            &opener.store,
            Dialect::PostgreSql,
            "SELECT count(*) FROM pg_class WHERE relname IN ('elsewhere', 'leak')"
        ),
        0
    );
    assert!(audit.lines()[0].contains(r#""allow_write":true"#));
}

#[test]
fn the_next_call_waits_for_the_gap_after_the_last_one() {
    let audit = TempAudit::new("call-gap");
    let opener = MapOpener::new(shop());
    let gap = Duration::from_millis(150);
    let mut server = limited(
        &opener,
        ServerConfig {
            call_gap: gap,
            ..config(&audit)
        },
    );
    let first = Instant::now();
    call(&mut server, 1, "list_tables", "{}");
    call(&mut server, 2, "list_tables", "{}");
    assert!(
        first.elapsed() >= gap,
        "two calls took {:?}, less than the gap",
        first.elapsed()
    );
    let pinged = Instant::now();
    let ping = parsed(
        &server
            .handle(br#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#)
            .unwrap(),
    );
    assert!(matches!(&get(&ping, &["result"]).value, Json::Object(members) if members.is_empty()));
    assert!(pinged.elapsed() < gap);
    let lines = audit.lines();
    assert!(lines[0].contains(r#""call_gap_ms":150"#), "{}", lines[0]);
    assert!(!lines[0].contains(r#""tables""#), "{}", lines[0]);
}

#[test]
fn a_table_listed_with_columns_shows_only_those_to_every_tool() {
    let audit = TempAudit::new("tables-columns");
    let store = shop();
    store.sql(
        Dialect::PostgreSql,
        "CREATE TABLE people (id integer PRIMARY KEY, name text, ssn text); \
         CREATE VIEW people_ssn AS SELECT ssn FROM people; \
         CREATE FUNCTION ssn_of(person integer) RETURNS text LANGUAGE sql STABLE AS $$ SELECT ssn FROM people WHERE id = person $$; \
         CREATE FUNCTION ssn_now(person integer) RETURNS text LANGUAGE sql AS $$ SELECT ssn FROM people WHERE id = person $$; \
         INSERT INTO people VALUES (1, 'Ada', '123-45-6789')",
    );
    let opener = MapOpener::new(store);
    let mut server = limited(
        &opener,
        ServerConfig {
            allow_write: true,
            ..tables_config(&audit, &["people(id,name)", "people_ssn"])
        },
    );
    assert_eq!(
        rows(&mut server, 1, "postgresql", "SELECT name FROM people"),
        vec!["Ada"]
    );
    let all = call(
        &mut server,
        2,
        "run_sql",
        r#"{"sql":"SELECT * FROM people","dialect":"postgresql"}"#,
    );
    let shown = format!("{all:?}");
    assert!(!shown.contains("123-45-6789"), "{shown}");
    assert!(!shown.contains("ssn"), "{shown}");
    for (id, dialect, sql) in [
        (3, "postgresql", "SELECT ssn FROM people"),
        (4, "mysql", "SELECT ssn FROM people"),
        (5, "postgresql", "SELECT id FROM people WHERE ssn LIKE '1%'"),
        (
            6,
            "postgresql",
            "WITH p AS (SELECT ssn FROM people) SELECT * FROM p",
        ),
        (
            7,
            "postgresql",
            "SELECT name FROM people WHERE id IN (SELECT id FROM people WHERE ssn IS NOT NULL)",
        ),
    ] {
        let message = sql_error(&mut server, id, dialect, sql);
        assert!(message.contains("ssn"), "{sql}: {message}");
    }
    let through_view = sql_error(&mut server, 8, "postgresql", "SELECT * FROM people_ssn");
    assert!(through_view.contains("ssn"), "{through_view}");
    let through_function = sql_error(&mut server, 9, "postgresql", "SELECT ssn_of(1)");
    assert!(through_function.contains("ssn"), "{through_function}");
    let (state, through_volatile) = write_refused(&mut server, 15, "SELECT ssn_now(1)");
    assert_eq!(state, "42703");
    assert!(through_volatile.contains("ssn"), "{through_volatile}");
    assert!(!through_volatile.contains("123-45"), "{through_volatile}");
    let described = call(&mut server, 10, "describe_table", r#"{"table":"people"}"#);
    let described = format!("{described:?}");
    assert!(described.contains("name"), "{described}");
    assert!(!described.contains("ssn"), "{described}");
    let sampled = call(&mut server, 11, "sample_rows", r#"{"table":"people"}"#);
    let sampled = format!("{sampled:?}");
    assert!(
        sampled.contains("Ada") && !sampled.contains("123-45-6789"),
        "{sampled}"
    );
    let renamed = write(&mut server, 12, "UPDATE people SET name = 'Ida' WHERE id = 1");
    assert_eq!(text_at(structured(&renamed), &["committed"]), "true");
    let inserted = write(
        &mut server,
        13,
        "INSERT INTO people VALUES (2, 'Grace') RETURNING *",
    );
    assert_eq!(text_at(structured(&inserted), &["committed"]), "true");
    assert!(
        !format!("{inserted:?}").contains("ssn"),
        "{inserted:?}"
    );
    assert_eq!(
        write_refused(&mut server, 14, "UPDATE people SET ssn = 'x'").0,
        "42703"
    );
    let (state, message) = write_refused(&mut server, 16, "DELETE FROM people WHERE id = 1");
    assert_eq!(state, "42501");
    assert!(
        message.starts_with("cannot delete rows of \"people\""),
        "{message}"
    );
    assert_eq!(
        count(
            &opener.store,
            Dialect::PostgreSql,
            "SELECT count(*) FROM people WHERE id = 1 AND name = 'Ida' AND ssn = '123-45-6789'"
        ),
        1
    );
    assert_eq!(
        count(
            &opener.store,
            Dialect::PostgreSql,
            "SELECT count(*) FROM people WHERE id = 2 AND name = 'Grace' AND ssn IS NULL"
        ),
        1
    );
    assert!(
        audit.lines()[0].contains(r#""tables":["people(id,name)","people_ssn"]"#),
        "{}",
        audit.lines()[0]
    );
}

#[test]
fn a_policy_the_agent_writes_cannot_call_a_user_routine() {
    let audit = TempAudit::new("tables-policies");
    let store = shop();
    store.sql(
        Dialect::PostgreSql,
        "CREATE TABLE tags (id integer PRIMARY KEY, label text); \
         CREATE FUNCTION pay_total() RETURNS integer LANGUAGE sql AS $$ SELECT sum(amount) FROM salaries $$",
    );
    let opener = MapOpener::new(store);
    let mut server = limited(
        &opener,
        ServerConfig {
            allow_write: true,
            ..tables_config(&audit, &["tags"])
        },
    );
    let (state, message) = write_refused(
        &mut server,
        1,
        "CREATE POLICY leak ON tags USING (pay_total() > 0)",
    );
    assert_eq!(state, "42501");
    assert_eq!(
        message,
        "cannot write a row-security policy that calls the routine \"pay_total\": it runs in every later session that reads or writes the table, where --tables does not confine it"
    );
    let (state, message) = write_refused(
        &mut server,
        2,
        "CREATE POLICY nested ON tags USING (label IN (SELECT label FROM tags))",
    );
    assert_eq!(state, "42501");
    assert_eq!(
        message,
        "cannot write a row-security policy with a subquery: it runs in every later session that reads or writes the table, where --tables does not confine it"
    );
    let (state, message) = write_refused(
        &mut server,
        3,
        "ALTER TABLE tags ALTER COLUMN label SET DEFAULT pay_total()::text",
    );
    assert_eq!(state, "42501");
    assert_eq!(
        message,
        "cannot write a default, check or generated expression that calls the routine \"pay_total\": it runs in every later session that writes the table, where --tables does not confine it"
    );
    let plain = write(&mut server, 4, "CREATE POLICY own ON tags USING (id > 0)");
    assert_eq!(text_at(structured(&plain), &["committed"]), "true");
    let mut session = Session::with_dialect(&opener.store, Dialect::PostgreSql);
    for (policy, stored) in [("own", true), ("leak", false), ("nested", false)] {
        let dropped = session.execute(&format!("DROP POLICY {policy} ON tags"), &mut |_| {});
        assert_eq!(dropped.is_ok(), stored, "{policy}: {dropped:?}");
    }
    session.close().unwrap();
}
