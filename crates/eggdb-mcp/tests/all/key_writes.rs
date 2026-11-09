//! `put` and `del` (D328), and `incr`, `cas`, `expire` and `persist` (D346): offered only with
//! `--allow-write` and the key-value store, refused for a read-only server, a server limited to
//! tables and an agent signed in with a role, a delete (or an `expire` to 0, or a `cas` that
//! deletes) confirmed through `confirm: true` or `elicitation/create`, SQL's key space refused,
//! and every key write recorded in the audit file before it runs and after.

use crate::common;

use common::{
    MapOpener, TempAudit, call, error_code, initialized, is_error, member, parsed, sample_store,
    served, structured, tables_config, text_at, writing_config, writing_server,
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

const COUNTING_AND_EXPIRING: [(&str, &str); 4] = [
    ("incr", r#"{"key":"hits"}"#),
    ("cas", r#"{"key":"lock","if_absent":true,"new":"me"}"#),
    ("expire", r#"{"key":"greeting","ttl":0,"confirm":true}"#),
    ("persist", r#"{"key":"greeting"}"#),
];

#[test]
fn counters_compare_and_set_and_expiry_are_refused_without_writes_tables_or_a_role() {
    for (name, config_of, code) in [
        ("read-only", 0, "read_only"),
        ("tables", 1, "limited_to_tables"),
        ("role", 2, "limited_to_role"),
    ] {
        let audit = TempAudit::new(&format!("key-tools-{name}"));
        let opener = MapOpener::new(sample_store());
        opener.store.put(b"greeting", b"hello");
        let config = match config_of {
            0 => common::config(&audit),
            1 => ServerConfig {
                allow_write: true,
                ..tables_config(&audit, &["users"])
            },
            _ => ServerConfig {
                agent_role: Some("reporter".to_owned()),
                ..writing_config(&audit)
            },
        };
        let before = opener.store.snapshot();
        let mut server = Server::new(&opener, config).unwrap();
        initialize(&mut server);
        let tools = listed_tools(&mut server);
        for (id, (tool, arguments)) in (2..).zip(COUNTING_AND_EXPIRING) {
            assert!(
                !tools.contains(&format!("\"name\":\"{tool}\"")),
                "{name}: {tools}"
            );
            let answer = call(&mut server, id, tool, arguments);
            assert_eq!(error_code(&answer), code, "{name}: {tool}");
        }
        assert_eq!(opener.store.snapshot(), before, "{name}");
        assert_eq!(opener.write_opens.get(), 0, "{name}");
        let refused = audit
            .lines()
            .iter()
            .filter(|line| line.contains(&format!("\"error_code\":\"{code}\"")))
            .count();
        assert_eq!(refused, 4, "{name}");
    }
}

#[test]
fn every_key_tool_refuses_a_key_of_a_sql_row_before_opening_the_file() {
    let audit = TempAudit::new("key-tools-sql-rows");
    let opener = MapOpener::new(sample_store());
    let before = opener.store.snapshot();
    let row_keys: Vec<String> = before
        .keys()
        .filter(|key| key.starts_with(RESERVED_KEY_PREFIX))
        .map(|key| key.iter().map(|byte| format!("{byte:02x}")).collect())
        .collect();
    assert!(row_keys.len() > 3, "{row_keys:?}");
    let mut server = writing_server(&opener, &audit, "{}");
    let mut id = 2;
    for key_hex in &row_keys {
        for (tool, rest) in [
            ("incr", r#""by":5"#),
            ("incr", r#""by":-5"#),
            ("expire", r#""ttl":60"#),
            ("expire", r#""ttl":0,"confirm":true"#),
            ("persist", ""),
            ("cas", r#""if_absent":true,"new":"x""#),
            ("cas", r#""expected":"x","delete":true,"confirm":true"#),
            ("put", r#""value":"x""#),
            ("del", r#""confirm":true"#),
        ] {
            let arguments = if rest.is_empty() {
                format!(r#"{{"key_hex":"{key_hex}"}}"#)
            } else {
                format!(r#"{{"key_hex":"{key_hex}",{rest}}}"#)
            };
            let answer = call(&mut server, id, tool, &arguments);
            id += 1;
            assert_eq!(error_code(&answer), "invalid_arguments", "{tool} {arguments}");
            let message = text_at(structured(&answer), &["error", "message"]);
            assert!(message.contains("SQL's key space"), "{tool}: {message}");
        }
    }
    assert_eq!(opener.store.snapshot(), before);
    assert_eq!(opener.write_opens.get(), 0);
}

#[test]
fn incr_adds_and_subtracts_and_a_value_that_is_not_a_counter_changes_nothing() {
    let audit = TempAudit::new("key-tools-incr");
    let opener = MapOpener::new(sample_store());
    let mut server = writing_server(&opener, &audit, "{}");
    let first = call(&mut server, 2, "incr", r#"{"key":"hits"}"#);
    assert!(!is_error(&first), "{first:?}");
    assert_eq!(text_at(structured(&first), &["value"]), "1");
    assert_eq!(text_at(structured(&first), &["committed"]), "true");
    let more = call(&mut server, 3, "incr", r#"{"key":"hits","by":41}"#);
    assert_eq!(text_at(structured(&more), &["value"]), "42");
    let less = call(&mut server, 4, "incr", r#"{"key_hex":"68697473","by":-50}"#);
    assert_eq!(text_at(structured(&less), &["value"]), "-8");
    assert_eq!(stored(&opener, b"hits"), Some(b"-8".to_vec()));
    let not_a_counter = call(&mut server, 5, "incr", r#"{"key":"user:1"}"#);
    assert_eq!(error_code(&not_a_counter), "sql_error");
    let error = get_error(&not_a_counter);
    assert_eq!(text_at(error, &["sqlstate"]), "22000");
    assert!(text_at(error, &["fix"]).contains("whole number"));
    assert_eq!(stored(&opener, b"user:1"), Some(b"Ada".to_vec()));
    for (id, arguments) in [
        (6, r#"{"key":"hits","by":1.5}"#),
        (7, r#"{"key":"hits","by":"2"}"#),
        (8, r#"{"key":"hits","by":9223372036854775808}"#),
        (9, r#"{"key":"hits","confirm":true}"#),
    ] {
        let refused = call(&mut server, id, "incr", arguments);
        assert_eq!(error_code(&refused), "invalid_arguments", "{arguments}");
    }
    assert_eq!(stored(&opener, b"hits"), Some(b"-8".to_vec()));
    let records = audit_records(&audit);
    let counted = records
        .iter()
        .filter(|record| text_at(record, &["event"]) == "call_started")
        .filter(|record| text_at(record, &["tool"]) == "incr")
        .count();
    assert_eq!(counted, 4, "{records:?}");
}

fn get_error(answer: &Node) -> &Node {
    let Some(error) = member(structured(answer), "error") else {
        panic!("{answer:?}");
    };
    error
}

#[test]
fn cas_swaps_only_while_the_value_is_expected_and_deleting_needs_confirmation() {
    let audit = TempAudit::new("key-tools-cas");
    let opener = MapOpener::new(sample_store());
    let mut server = writing_server(&opener, &audit, "{}");
    let taken = call(
        &mut server,
        2,
        "cas",
        r#"{"key":"lock","if_absent":true,"new":"worker-7"}"#,
    );
    assert!(!is_error(&taken), "{taken:?}");
    assert_eq!(text_at(structured(&taken), &["swapped"]), "true");
    assert!(member(structured(&taken), "value").is_none());
    let not_taken = call(
        &mut server,
        3,
        "cas",
        r#"{"key":"lock","if_absent":true,"new":"worker-8"}"#,
    );
    assert_eq!(text_at(structured(&not_taken), &["swapped"]), "false");
    assert_eq!(text_at(structured(&not_taken), &["value"]), "worker-7");
    assert_eq!(stored(&opener, b"lock"), Some(b"worker-7".to_vec()));
    let binary = call(
        &mut server,
        4,
        "cas",
        r#"{"key":"user:2","expected":"x","new":"y"}"#,
    );
    assert_eq!(text_at(structured(&binary), &["swapped"]), "false");
    assert_eq!(text_at(structured(&binary), &["value_hex"]), "009f9296");
    let unconfirmed = call(
        &mut server,
        5,
        "cas",
        r#"{"key":"lock","expected":"worker-7","delete":true}"#,
    );
    assert_eq!(error_code(&unconfirmed), "needs_confirmation");
    assert_eq!(stored(&opener, b"lock"), Some(b"worker-7".to_vec()));
    let released = call(
        &mut server,
        6,
        "cas",
        r#"{"key":"lock","expected_hex":"776f726b65722d37","delete":true,"confirm":true}"#,
    );
    assert_eq!(text_at(structured(&released), &["swapped"]), "true");
    assert_eq!(stored(&opener, b"lock"), None);
    for (id, arguments, expected) in [
        (7, r#"{"key":"lock","new":"a"}"#, "if_absent"),
        (8, r#"{"key":"lock","if_absent":true}"#, "delete: true"),
        (
            9,
            r#"{"key":"lock","expected":"a","if_absent":true,"new":"b"}"#,
            "not both",
        ),
        (
            10,
            r#"{"key":"lock","expected":"a","new":"b","delete":true}"#,
            "not both",
        ),
    ] {
        let refused = call(&mut server, id, "cas", arguments);
        assert_eq!(error_code(&refused), "invalid_arguments", "{arguments}");
        let message = text_at(get_error(&refused), &["message"]);
        assert!(message.contains(expected), "{arguments}: {message}");
    }
    let records = audit_records(&audit);
    let confirmed = records
        .iter()
        .filter(|record| text_at(record, &["event"]) == "call")
        .filter(|record| text_at(record, &["tool"]) == "cas")
        .filter(|record| {
            member(record, "confirmation").is_some()
                && text_at(record, &["confirmation"]) == "confirm_argument"
        })
        .count();
    assert_eq!(confirmed, 1, "{records:?}");
}

#[test]
fn expire_gives_a_time_to_live_and_0_deletes_the_key_only_once_confirmed() {
    let audit = TempAudit::new("key-tools-expire");
    let opener = MapOpener::new(sample_store());
    opener.store.put(b"session:1", b"open");
    let mut server = writing_server(&opener, &audit, "{}");
    let expiring = call(
        &mut server,
        2,
        "expire",
        r#"{"key":"session:1","ttl":1800}"#,
    );
    assert!(!is_error(&expiring), "{expiring:?}");
    let result = structured(&expiring);
    assert_eq!(text_at(result, &["expires"]), "true");
    assert_eq!(text_at(result, &["ttl_ms"]), "1800000");
    let at = text_at(result, &["expires_at"]);
    assert_eq!(at.len(), "2026-10-30T09:44:07.123Z".len(), "{at}");
    assert!(at.ends_with('Z') && at.as_bytes()[10] == b'T', "{at}");
    let absent = call(&mut server, 3, "expire", r#"{"key":"nobody","ttl":60}"#);
    assert_eq!(text_at(structured(&absent), &["expires"]), "false");
    assert!(member(structured(&absent), "expires_at").is_none());
    let unconfirmed = call(&mut server, 4, "expire", r#"{"key":"session:1","ttl":0}"#);
    assert_eq!(error_code(&unconfirmed), "needs_confirmation");
    assert_eq!(stored(&opener, b"session:1"), Some(b"open".to_vec()));
    let deleted = call(
        &mut server,
        5,
        "expire",
        r#"{"key":"session:1","ttl":0,"confirm":true}"#,
    );
    assert!(!is_error(&deleted), "{deleted:?}");
    assert_eq!(text_at(structured(&deleted), &["deleted"]), "true");
    assert!(member(structured(&deleted), "expires").is_none());
    assert_eq!(stored(&opener, b"session:1"), None);
    for (id, arguments) in [
        (6, r#"{"key":"k"}"#),
        (7, r#"{"key":"k","ttl":-1}"#),
        (8, r#"{"key":"k","ttl":"30s"}"#),
        (9, r#"{"key":"k","ttl":3155760001}"#),
    ] {
        let refused = call(&mut server, id, "expire", arguments);
        assert_eq!(error_code(&refused), "invalid_arguments", "{arguments}");
    }
    let persisted = call(&mut server, 10, "persist", r#"{"key":"user:1"}"#);
    assert_eq!(text_at(structured(&persisted), &["persisted"]), "true");
    let missing = call(&mut server, 11, "persist", r#"{"key":"session:1"}"#);
    assert_eq!(text_at(structured(&missing), &["persisted"]), "false");
}

#[test]
fn a_client_that_can_ask_is_asked_before_expire_deletes_a_key() {
    for (answer, deletes) in [
        (r#"{"action":"accept","content":{"run":true}}"#, true),
        (r#"{"action":"decline"}"#, false),
    ] {
        let audit = TempAudit::new("key-tools-expire-elicit");
        let opener = MapOpener::new(sample_store());
        opener.store.put(b"session:1", b"open");
        let mut server = Server::new(&opener, writing_config(&audit)).unwrap();
        let script = [
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{}},"clientInfo":{"name":"test-client","version":"1.0"}}}"#.to_owned(),
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"expire","arguments":{"key":"session:1","ttl":0,"confirm":true}}}"#.to_owned(),
            format!(r#"{{"jsonrpc":"2.0","id":"eggdb-confirm-1","result":{answer}}}"#),
        ]
        .join("\n")
            + "\n";
        let answers = served(&mut server, &script);
        assert_eq!(answers.len(), 3, "{answers:?}");
        let message = text_at(&answers[1], &["params", "message"]);
        assert!(
            message.contains("delete the key \"session:1\" from the default key space of app.db."),
            "{message}"
        );
        if deletes {
            assert!(!is_error(&answers[2]), "{:?}", answers[2]);
        } else {
            assert_eq!(error_code(&answers[2]), "declined_by_user");
        }
        assert_eq!(stored(&opener, b"session:1").is_none(), deletes);
    }
}

#[test]
fn put_with_a_time_to_live_says_when_the_key_expires_as_eggdb_put_does() {
    let audit = TempAudit::new("key-tools-put-expires-at");
    let opener = MapOpener::new(sample_store());
    let mut server = writing_server(&opener, &audit, "{}");
    let timed = call(
        &mut server,
        2,
        "put",
        r#"{"key":"s:1","value":"v","ttl":"30m"}"#,
    );
    let result = structured(&timed);
    assert_eq!(text_at(result, &["ttl_ms"]), "1800000");
    let at = text_at(result, &["expires_at"]);
    assert_eq!(at.len(), "2026-10-30T09:44:07.123Z".len(), "{at}");
    let plain = call(&mut server, 3, "put", r#"{"key":"s:2","value":"v"}"#);
    assert!(member(structured(&plain), "expires_at").is_none());
    assert!(member(structured(&plain), "ttl_ms").is_none());
}

#[test]
fn a_writer_confined_to_tables_refuses_counters_compare_and_set_and_expiry() {
    use eggdb_sql::{KeyValueStore, KeyValueWrite};
    let store = sample_store();
    let before = store.snapshot();
    let tables = eggdb_sql::VisibleTables::new(["users"]);
    let mut writer = tables.write(store.begin_write().unwrap()).unwrap();
    let refusals = [
        writer.increment(None, b"hits", 1).map(|_| ()),
        writer
            .compare_and_set(None, b"lock", None, Some(b"me".as_slice()))
            .map(|_| ()),
        writer
            .set_expiry(None, b"user:1", std::time::Duration::ZERO)
            .map(|_| ()),
        writer.clear_expiry(Some("sessions"), b"user:1").map(|_| ()),
    ];
    for refusal in refusals {
        let Err(error) = refusal else {
            panic!("a confined writer changed the key-value store");
        };
        assert_eq!(error.state().code(), "42501");
    }
    writer.commit().unwrap();
    assert_eq!(store.snapshot(), before);
}
