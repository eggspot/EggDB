//! A server started with `--tables` over an in-memory database: only the listed tables and
//! views exist for the agent, in every tool, dialect and catalog, the key-value tools are
//! withdrawn, the instructions and the audit say so, and the gap between calls holds.

mod common;

use common::{
    MapOpener, MapStore, TempAudit, call, config, error_code, get, parsed, structured,
    tables_config, text_at,
};
use eggdb_mcp::{Server, ServerConfig, StartFailure};
use eggdb_sql::Dialect;
use eggdb_sql::json::{Json, Node};
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
        Json::Array(items) => items
            .iter()
            .map(|item| text_at(item, &["name"]))
            .collect(),
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
    assert_eq!(text_at(&answer, &["result", "isError"]), "false", "{answer:?}");
    match &get(structured(&answer), &["rows"]).value {
        Json::Array(rows) => rows
            .iter()
            .map(|row| text_at(row, &["0"]))
            .collect(),
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
    assert_eq!(error_code(&call(&mut server, 4, "sample_rows", r#"{"table":"salaries"}"#)), error_code(&missing));
    assert_eq!(
        error_code(&call(&mut server, 5, "row_list", r#"{"table":"salaries"}"#)),
        error_code(&call(&mut server, 6, "row_list", r#"{"table":"nothing"}"#))
    );
    let direct = sql_error(&mut server, 7, "postgresql", "SELECT amount FROM salaries");
    let absent = sql_error(&mut server, 8, "postgresql", "SELECT amount FROM nothing");
    assert_eq!(direct.replace("salaries", "nothing"), absent);
    assert!(!sql_error(&mut server, 9, "postgresql", "SELECT * FROM pay").contains("100"));
    sql_error(&mut server, 10, "postgresql", "EXPLAIN SELECT * FROM salaries");
    sql_error(&mut server, 11, "mysql", "SELECT amount FROM salaries");
    sql_error(&mut server, 12, "sqlite", "SELECT amount FROM salaries");
    sql_error(&mut server, 13, "sqlserver", "SELECT amount FROM dbo.salaries");
    sql_error(&mut server, 14, "oracle", "SELECT amount FROM salaries");
    assert_eq!(
        rows(&mut server, 15, "postgresql", "SELECT name FROM names ORDER BY name"),
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
    assert!(!tools.iter().any(|tool| tool.starts_with("kv_")), "{tools:?}");
    assert!(tools.iter().any(|tool| tool == "run_sql"), "{tools:?}");
    for (id, tool, arguments) in [
        (2, "kv_get", r#"{"key":"session:1"}"#),
        (3, "kv_scan", r#"{"prefix":"session:"}"#),
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

#[test]
fn tables_cannot_be_combined_with_writes() {
    let audit = TempAudit::new("tables-writes");
    let opener = MapOpener::new(shop());
    let refused = Server::new(
        &opener,
        ServerConfig {
            allow_write: true,
            ..tables_config(&audit, &["users"])
        },
    );
    assert!(matches!(refused, Err(StartFailure::TablesWithWrites)));
    assert!(audit.lines().is_empty());
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
