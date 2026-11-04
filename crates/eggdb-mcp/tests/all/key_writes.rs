//! `put` and `del` (D328): offered only with `--allow-write` and the key-value store, refused
//! for a read-only server, a server limited to tables and an agent signed in with a role, a
//! delete confirmed through `confirm: true` or `elicitation/create`, SQL's key space refused,
//! and every key write recorded in the audit file before it runs and after.

use crate::common;

use common::{
    MapOpener, TempAudit, call, error_code, initialized, is_error, parsed, sample_store, served,
    structured, tables_config, text_at, writing_config, writing_server,
};
use eggdb_mcp::{Server, ServerConfig};
use eggdb_sql::RESERVED_KEY_PREFIX;
use eggdb_sql::json::Node;

fn audit_records(audit: &TempAudit) -> Vec<Node> {
    audit.lines().iter().map(|line| parsed(line)).collect()
}

fn listed_tools(server: &mut Server<'_, MapOpener>) -> String {
    server
        .handle(br#"{"jsonrpc":"2.0","id":90,"method":"tools/list"}"#)
        .unwrap()
}

fn initialize(server: &mut Server<'_, MapOpener>) {
    let answer = server
        .handle(br#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test-client","version":"1.0"}}}"#)
        .unwrap();
    assert!(answer.contains("\"protocolVersion\""), "{answer}");
}

fn stored(opener: &MapOpener, key: &[u8]) -> Option<Vec<u8>> {
    opener.store.snapshot().get(key).cloned()
}

#[test]
fn a_read_only_server_neither_lists_nor_runs_the_key_writes() {
    let audit = TempAudit::new("key-writes-read-only");
    let opener = MapOpener::new(sample_store());
    let mut server = initialized(&opener, &audit);
    let tools = listed_tools(&mut server);
    assert!(!tools.contains("\"name\":\"put\""), "{tools}");
    assert!(!tools.contains("\"name\":\"del\""), "{tools}");
    let put = call(&mut server, 2, "put", r#"{"key":"k","value":"v"}"#);
    assert_eq!(error_code(&put), "read_only");
    let del = call(
        &mut server,
        3,
        "del",
        r#"{"key":"greeting","confirm":true}"#,
    );
    assert_eq!(error_code(&del), "read_only");
    assert_eq!(stored(&opener, b"k"), None);
    assert_eq!(opener.write_opens.get(), 0);
    let lines = audit.lines();
    let refused = lines
        .iter()
        .filter(|line| line.contains("\"error_code\":\"read_only\""))
        .count();
    assert_eq!(refused, 2, "{lines:?}");
}

#[test]
fn a_server_limited_to_tables_or_acting_as_a_role_refuses_them_even_with_writes() {
    for (name, limited, code) in [
        ("tables", true, "limited_to_tables"),
        ("role", false, "limited_to_role"),
    ] {
        let audit = TempAudit::new(&format!("key-writes-{name}"));
        let opener = MapOpener::new(sample_store());
        let config = if limited {
            ServerConfig {
                allow_write: true,
                ..tables_config(&audit, &["users"])
            }
        } else {
            ServerConfig {
                agent_role: Some("reporter".to_owned()),
                ..writing_config(&audit)
            }
        };
        let mut server = Server::new(&opener, config).unwrap();
        initialize(&mut server);
        let tools = listed_tools(&mut server);
        assert!(!tools.contains("\"name\":\"put\""), "{name}: {tools}");
        let put = call(&mut server, 2, "put", r#"{"key":"k","value":"v"}"#);
        assert_eq!(error_code(&put), code, "{name}");
        assert_eq!(stored(&opener, b"k"), None, "{name}");
    }
}

#[test]
fn with_writes_put_stores_and_replaces_a_key_and_each_write_is_audited_twice() {
    let audit = TempAudit::new("key-writes-put");
    let opener = MapOpener::new(sample_store());
    let mut server = writing_server(&opener, &audit, "{}");
    let tools = listed_tools(&mut server);
    assert!(
        tools.contains("{\"name\":\"put\",\"title\":\"Store a key\""),
        "{tools}"
    );
    let put = call(&mut server, 2, "put", r#"{"key":"user:1","value":"Ada"}"#);
    assert!(!is_error(&put), "{put:?}");
    let result = structured(&put);
    assert_eq!(text_at(result, &["key"]), "user:1");
    assert_eq!(text_at(result, &["key_hex"]), "757365723a31");
    assert_eq!(text_at(result, &["stored"]), "true");
    assert_eq!(text_at(result, &["committed"]), "true");
    assert_eq!(stored(&opener, b"user:1"), Some(b"Ada".to_vec()));
    let replaced = call(
        &mut server,
        3,
        "put",
        r#"{"key_hex":"757365723a31","value_hex":"00ff"}"#,
    );
    assert!(!is_error(&replaced), "{replaced:?}");
    assert_eq!(stored(&opener, b"user:1"), Some(vec![0x00, 0xff]));
    let records = audit_records(&audit);
    let events: Vec<String> = records
        .iter()
        .map(|record| text_at(record, &["event"]))
        .collect();
    assert_eq!(
        events,
        [
            "server_started",
            "call_started",
            "call",
            "call_started",
            "call"
        ],
        "{records:?}"
    );
    assert_eq!(text_at(&records[2], &["tool"]), "put");
    assert_eq!(text_at(&records[2], &["committed"]), "true");
}

#[test]
fn put_refuses_sql_s_key_space_a_bad_time_to_live_and_a_missing_value() {
    let audit = TempAudit::new("key-writes-refused");
    let opener = MapOpener::new(sample_store());
    let mut server = writing_server(&opener, &audit, "{}");
    let hex: String = RESERVED_KEY_PREFIX
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let before = opener.store.snapshot();
    for (id, arguments, expected) in [
        (
            2,
            format!(r#"{{"key_hex":"{hex}00","value":"x"}}"#),
            "SQL's key space",
        ),
        (
            3,
            r#"{"key":"k","value":"v","ttl":"5"}"#.to_owned(),
            "ttl needs a time",
        ),
        (
            4,
            r#"{"key":"k","value":"v","ttl":"0s"}"#.to_owned(),
            "a ttl of 0",
        ),
        (5, r#"{"key":"k"}"#.to_owned(), "give value or value_hex"),
        (
            6,
            r#"{"key":"k","value":"v","confirm":true}"#.to_owned(),
            "unknown argument",
        ),
    ] {
        let answer = call(&mut server, id, "put", &arguments);
        assert_eq!(error_code(&answer), "invalid_arguments", "{arguments}");
        let message = text_at(structured(&answer), &["error", "message"]);
        assert!(message.contains(expected), "{arguments}: {message}");
    }
    let deleting = call(
        &mut server,
        7,
        "del",
        &format!(r#"{{"key_hex":"{hex}00","confirm":true}}"#),
    );
    assert_eq!(error_code(&deleting), "invalid_arguments");
    assert_eq!(opener.store.snapshot(), before);
}

#[test]
fn a_delete_needs_confirm_true_when_the_client_cannot_ask() {
    let audit = TempAudit::new("key-writes-confirm");
    let opener = MapOpener::new(sample_store());
    opener.store.put(b"session:1", b"open");
    let mut server = writing_server(&opener, &audit, "{}");
    let unconfirmed = call(&mut server, 2, "del", r#"{"key":"session:1"}"#);
    assert_eq!(error_code(&unconfirmed), "needs_confirmation");
    assert_eq!(stored(&opener, b"session:1"), Some(b"open".to_vec()));
    let confirmed = call(
        &mut server,
        3,
        "del",
        r#"{"key":"session:1","confirm":true}"#,
    );
    assert!(!is_error(&confirmed), "{confirmed:?}");
    assert_eq!(text_at(structured(&confirmed), &["deleted"]), "true");
    assert_eq!(stored(&opener, b"session:1"), None);
    let again = call(
        &mut server,
        4,
        "del",
        r#"{"key":"session:1","confirm":true}"#,
    );
    assert_eq!(text_at(structured(&again), &["deleted"]), "false");
    let records = audit_records(&audit);
    let calls: Vec<&Node> = records
        .iter()
        .filter(|record| text_at(record, &["event"]) == "call")
        .collect();
    assert_eq!(calls.len(), 3, "{records:?}");
    assert_eq!(text_at(calls[0], &["error_code"]), "needs_confirmation");
    assert_eq!(text_at(calls[1], &["confirmation"]), "confirm_argument");
    assert_eq!(text_at(calls[1], &["tool"]), "del");
}

#[test]
fn a_client_that_can_ask_is_asked_before_a_key_is_deleted() {
    for (answer, deletes) in [
        (r#"{"action":"accept","content":{"run":true}}"#, true),
        (r#"{"action":"decline"}"#, false),
    ] {
        let audit = TempAudit::new("key-writes-elicit");
        let opener = MapOpener::new(sample_store());
        opener.store.put(b"session:1", b"open");
        let mut server = Server::new(&opener, writing_config(&audit)).unwrap();
        let script = [
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{}},"clientInfo":{"name":"test-client","version":"1.0"}}}"#.to_owned(),
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"del","arguments":{"key":"session:1"}}}"#.to_owned(),
            format!(r#"{{"jsonrpc":"2.0","id":"eggdb-confirm-1","result":{answer}}}"#),
        ]
        .join("\n")
            + "\n";
        let answers = served(&mut server, &script);
        assert_eq!(answers.len(), 3, "{answers:?}");
        let asked = &answers[1];
        assert_eq!(text_at(asked, &["method"]), "elicitation/create");
        let message = text_at(asked, &["params", "message"]);
        assert!(
            message.contains("delete the key \"session:1\" from the default key space of app.db."),
            "{message}"
        );
        assert_eq!(
            text_at(
                asked,
                &["params", "requestedSchema", "properties", "run", "title"]
            ),
            "Delete it from app.db"
        );
        let result = &answers[2];
        if deletes {
            assert!(!is_error(result), "{result:?}");
        } else {
            assert_eq!(error_code(result), "declined_by_user");
        }
        assert_eq!(stored(&opener, b"session:1").is_none(), deletes);
        let records = audit_records(&audit);
        let last = records.last().unwrap();
        assert_eq!(text_at(last, &["confirmation"]), "elicitation");
    }
}

#[test]
fn a_collection_needs_a_store_that_keeps_them() {
    let audit = TempAudit::new("key-writes-collection");
    let opener = MapOpener::new(sample_store());
    let mut server = writing_server(&opener, &audit, "{}");
    let put = call(
        &mut server,
        2,
        "put",
        r#"{"key":"s:1","value":"v","collection":"sessions"}"#,
    );
    assert!(is_error(&put), "{put:?}");
    assert_eq!(text_at(structured(&put), &["error", "sqlstate"]), "0A000");
}
