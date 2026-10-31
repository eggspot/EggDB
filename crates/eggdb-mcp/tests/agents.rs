//! A server whose calls run as a stored role: the role's grants and row security decide what
//! the agent reads, the key-value tools are withdrawn, and the prompts are listed.

mod common;

use common::{MapOpener, TempAudit, call, config, error_code, parsed, structured, text_at};
use eggdb_mcp::{Server, ServerConfig};
use eggdb_sql::Dialect;

const INITIALIZE: &[u8] = br#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"agent","version":"1.0"}}}"#;

fn store() -> common::MapStore {
    let store = common::sample_store();
    store.sql(
        Dialect::PostgreSql,
        "CREATE TABLE secrets (id integer PRIMARY KEY, body text); \
         INSERT INTO secrets VALUES (1, 'hidden'); \
         CREATE USER reporter LOGIN; \
         GRANT SELECT ON users TO reporter; \
         ALTER TABLE users ENABLE ROW LEVEL SECURITY; \
         CREATE POLICY first_two ON users FOR SELECT TO reporter USING (id <= 2)",
    );
    store
}

fn agent_server<'o>(opener: &'o MapOpener, audit: &TempAudit, role: &str) -> Server<'o, MapOpener> {
    let mut server = Server::new(
        opener,
        ServerConfig {
            agent_role: Some(role.to_owned()),
            ..config(audit)
        },
    )
    .unwrap();
    server.handle(INITIALIZE).unwrap();
    server
}

#[test]
fn an_agent_reads_only_what_its_role_may_and_through_its_policies() {
    let audit = TempAudit::new("agent-role");
    let opener = MapOpener::new(store());
    let mut server = agent_server(&opener, &audit, "reporter");
    let answer = call(&mut server, 1, "run_sql", r#"{"sql":"SELECT id FROM users ORDER BY id"}"#);
    let rows = text_at(structured(&answer), &["row_count"]);
    assert_eq!(rows, "2", "{answer:?}");
    let refused = call(&mut server, 2, "run_sql", r#"{"sql":"SELECT * FROM secrets"}"#);
    assert_eq!(text_at(structured(&refused), &["error", "sqlstate"]), "42501");
    let tools = parsed(
        &server
            .handle(br#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#)
            .unwrap(),
    );
    let listed = format!("{tools:?}");
    assert!(!listed.contains("\"kv_get\"") && !listed.contains("kv_scan"), "{listed}");
}

#[test]
fn an_agent_whose_role_cannot_log_in_is_refused() {
    let audit = TempAudit::new("agent-nologin");
    let opener = MapOpener::new(store());
    opener.store.sql(Dialect::PostgreSql, "ALTER ROLE reporter NOLOGIN");
    let mut server = agent_server(&opener, &audit, "reporter");
    let answer = call(&mut server, 1, "run_sql", r#"{"sql":"SELECT 1"}"#);
    assert!(!error_code(&answer).is_empty());
}

#[test]
fn prompts_are_listed_and_filled_in() {
    let audit = TempAudit::new("agent-prompts");
    let opener = MapOpener::new(store());
    let mut server = agent_server(&opener, &audit, "reporter");
    let list = server
        .handle(br#"{"jsonrpc":"2.0","id":1,"method":"prompts/list"}"#)
        .unwrap();
    assert!(list.contains("explore_database") && list.contains("answer_with_sql"), "{list}");
    let filled = server
        .handle(br#"{"jsonrpc":"2.0","id":2,"method":"prompts/get","params":{"name":"answer_with_sql","arguments":{"question":"How many users?"}}}"#)
        .unwrap();
    assert!(filled.contains("How many users?"), "{filled}");
    let missing = server
        .handle(br#"{"jsonrpc":"2.0","id":3,"method":"prompts/get","params":{"name":"answer_with_sql"}}"#)
        .unwrap();
    assert!(missing.contains("\"error\""), "{missing}");
}
