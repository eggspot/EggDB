//! A seeded fuzzer of the MCP server's JSON-RPC reader and dispatch (D9): valid messages
//! mutated byte by byte, cut short, nested past the limit, oversized, invalid UTF-8, wrong
//! types for every parameter and responses sent to the server. Every request line must get
//! exactly one answer that is one line of valid JSON, nothing may panic, and the database must
//! not change. `EGGDB_FUZZ_CASES` and `EGGDB_FUZZ_SEED` run longer campaigns.

mod common;

use common::{MapOpener, TempAudit, parsed, sample_store, server};

const DEFAULT_CASES: u64 = 25_000;

struct Seeded(u64);

impl Seeded {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound.max(1) as u64) as usize
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

fn setting(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

const VALID: [&str; 16] = [
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fuzz","version":"1"}}}"#,
    r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
    r#"{"jsonrpc":"2.0","id":"three","method":"tools/list"}"#,
    r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"list_tables","arguments":{}}}"#,
    r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"describe_table","arguments":{"table":"users"}}}"#,
    r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"run_sql","arguments":{"sql":"SELECT id, name FROM users WHERE id > 1","dialect":"postgresql","max_rows":5}}}"#,
    r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"run_sql","arguments":{"sql":"SELECT id FROM users FETCH FIRST 1 ROWS ONLY","dialect":"oracle"}}}"#,
    r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"explain_query","arguments":{"sql":"SELECT name FROM users WHERE id = 1"}}}"#,
    r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"sample_rows","arguments":{"table":"users","max_rows":2}}}"#,
    r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"kv_get","arguments":{"key":"user:1"}}}"#,
    r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"kv_scan","arguments":{"prefix_hex":"75","max_entries":3}}}"#,
    r#"{"jsonrpc":"2.0","id":12,"method":"resources/list"}"#,
    r#"{"jsonrpc":"2.0","id":13,"method":"resources/read","params":{"uri":"eggdb://schema"}}"#,
    r#"{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"run_sql","arguments":{"sql":"DELETE FROM users"}}}"#,
    r#"{"jsonrpc":"2.0","id":15,"result":{}}"#,
];

const FRAGMENTS: [&str; 24] = [
    "{",
    "}",
    "[",
    "]",
    ",",
    ":",
    "\"",
    "\\",
    "\\u0000",
    "\\ud800",
    "null",
    "true",
    "1e999999",
    "-0",
    "18446744073709551616",
    "\"id\":",
    "\"method\":",
    "\"params\":",
    "\"jsonrpc\":\"2.0\"",
    "\"arguments\":",
    "\"sql\":\"",
    "'; DROP TABLE users; --",
    "\u{2028}",
    "\u{1F600}",
];

const WRONG_VALUES: [&str; 9] = [
    "null", "true", "0", "-1", "1.5", "\"\"", "[]", "{}", "\"x\"",
];

fn mutated(seeded: &mut Seeded) -> Vec<u8> {
    let base = seeded.pick(&VALID).as_bytes().to_vec();
    let mut message = base;
    match seeded.below(8) {
        0 => {
            for _ in 0..1 + seeded.below(4) {
                let at = seeded.below(message.len());
                message[at] = (seeded.next() & 0xff) as u8;
            }
        }
        1 => {
            let keep = seeded.below(message.len());
            message.truncate(keep);
        }
        2 => {
            let at = seeded.below(message.len() + 1);
            let fragment = seeded.pick(&FRAGMENTS).as_bytes().to_vec();
            message.splice(at..at, fragment);
        }
        3 => {
            let depth = 200 + seeded.below(400);
            message = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"run_sql","arguments":{}1{}}}}}"#,
                "[".repeat(depth),
                "]".repeat(depth)
            )
            .into_bytes();
        }
        4 => {
            let text = String::from_utf8_lossy(&message).into_owned();
            let field = seeded.pick(&[
                "\"protocolVersion\":\"2025-11-25\"",
                "\"name\":\"list_tables\"",
                "\"table\":\"users\"",
                "\"sql\":\"SELECT id, name FROM users WHERE id > 1\"",
                "\"max_rows\":5",
                "\"key\":\"user:1\"",
                "\"uri\":\"eggdb://schema\"",
                "\"id\":2",
                "\"method\":\"ping\"",
            ]);
            let name = field.split(':').next().unwrap_or_default();
            let wrong = format!("{name}:{}", seeded.pick(&WRONG_VALUES));
            message = text.replace(field, &wrong).into_bytes();
        }
        5 => {
            let text = String::from_utf8_lossy(&message).into_owned();
            message = text
                .replacen(
                    "\"jsonrpc\":\"2.0\"",
                    "\"jsonrpc\":\"2.0\",\"jsonrpc\":\"2.0\"",
                    1,
                )
                .into_bytes();
        }
        6 => {
            message = (0..seeded.below(200))
                .map(|_| (seeded.next() & 0xff) as u8)
                .filter(|&byte| byte != b'\n')
                .collect();
        }
        _ => {
            message = format!("[{}]", String::from_utf8_lossy(&message)).into_bytes();
        }
    }
    message.retain(|&byte| byte != b'\n');
    message
}

fn expects_answer(line: &[u8]) -> bool {
    use eggdb_sql::json::Json;
    if line.iter().all(u8::is_ascii_whitespace) {
        return false;
    }
    let Ok(text) = std::str::from_utf8(line) else {
        return true;
    };
    let Ok(node) = eggdb_sql::json::parse_document(text) else {
        return true;
    };
    let Json::Object(members) = &node.value else {
        return true;
    };
    let mut names: Vec<&str> = members.iter().map(|(name, _)| name.as_str()).collect();
    names.sort_unstable();
    if names.windows(2).any(|pair| pair[0] == pair[1]) || has_duplicates_inside(&node) {
        return true;
    }
    let value = |name: &str| {
        members
            .iter()
            .find(|(member, _)| member == name)
            .map(|(_, node)| &node.value)
    };
    let id_ok = match value("id") {
        None => true,
        Some(Json::String(_)) => true,
        Some(Json::Number(text)) => {
            text.bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'-')
                && text.len() <= 40
        }
        Some(_) => false,
    };
    if !id_ok || !matches!(value("jsonrpc"), Some(Json::String(version)) if version == "2.0") {
        return true;
    }
    match value("method") {
        Some(Json::String(_)) => value("id").is_some(),
        Some(_) => true,
        None => value("result").is_none() && value("error").is_none(),
    }
}

fn has_duplicates_inside(node: &eggdb_sql::json::Node) -> bool {
    use eggdb_sql::json::Json;
    match &node.value {
        Json::Object(members) => {
            let mut names: Vec<&str> = members.iter().map(|(name, _)| name.as_str()).collect();
            names.sort_unstable();
            names.windows(2).any(|pair| pair[0] == pair[1])
                || members
                    .iter()
                    .any(|(_, value)| has_duplicates_inside(value))
        }
        Json::Array(items) => items.iter().any(has_duplicates_inside),
        _ => false,
    }
}

#[test]
fn hostile_messages_get_one_json_line_each_and_change_nothing() {
    let cases = setting("EGGDB_FUZZ_CASES", DEFAULT_CASES);
    let seed = setting("EGGDB_FUZZ_SEED", 0x5EED_F3C9);
    let mut seeded = Seeded(seed | 1);
    let audit = TempAudit::new("fuzz");
    let opener = MapOpener::new(sample_store());
    let before = opener.store.snapshot();
    let mut server = server(&opener, &audit);
    server.handle(VALID[0].as_bytes()).unwrap();
    for case in 0..cases {
        let line = mutated(&mut seeded);
        let answer = server.handle(&line);
        let text = String::from_utf8_lossy(&line);
        match answer {
            Some(answer) => {
                assert!(
                    expects_answer(&line),
                    "case {case} (seed {seed}): an answer to {text}: {answer}"
                );
                parsed(&answer);
            }
            None => assert!(
                !expects_answer(&line),
                "case {case} (seed {seed}): no answer to {text}"
            ),
        }
    }
    assert_eq!(opener.store.snapshot(), before);
}

#[test]
fn every_hostile_line_through_serve_gets_its_answers_in_order() {
    let mut seeded = Seeded(0xC0FFEE);
    let audit = TempAudit::new("fuzz-serve");
    let opener = MapOpener::new(sample_store());
    let mut server = server(&opener, &audit);
    let mut input = VALID[0].as_bytes().to_vec();
    input.push(b'\n');
    let mut requests = 1;
    for _ in 0..2_000 {
        let line = mutated(&mut seeded);
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if expects_answer(&line) {
            requests += 1;
        }
        input.extend_from_slice(&line);
        input.push(b'\n');
    }
    let mut output = Vec::new();
    server.serve(&mut input.as_slice(), &mut output).unwrap();
    let output = String::from_utf8(output).unwrap();
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines.len(), requests);
    for line in lines {
        parsed(line);
    }
}

const ANSWERS: [&str; 8] = [
    r#"{"jsonrpc":"2.0","id":"eggdb-confirm-N","result":{"action":"decline"}}"#,
    r#"{"jsonrpc":"2.0","id":"eggdb-confirm-N","result":{"action":"accept","content":{"run":false}}}"#,
    r#"{"jsonrpc":"2.0","id":"eggdb-confirm-N","result":{"action":"cancel"}}"#,
    r#"{"jsonrpc":"2.0","id":"eggdb-confirm-N","error":{"code":-1,"message":"no"}}"#,
    r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"eggdb-confirm-N"}}"#,
    r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":77}}"#,
    r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#,
    r#"{"jsonrpc":"2.0","id":"eggdb-confirm-N","result":"accept"}"#,
];

#[test]
fn hostile_answers_to_a_pending_confirmation_never_panic_or_run_the_statement() {
    let mut seeded = Seeded(setting("EGGDB_FUZZ_SEED", 0xD0_C0FF) | 1);
    let audit = TempAudit::new("fuzz-confirm");
    let opener = MapOpener::new(sample_store());
    opener.store.sql(
        eggdb_sql::Dialect::PostgreSql,
        "CREATE TABLE scratch (id integer PRIMARY KEY)",
    );
    let before = opener.store.snapshot();
    let mut server = eggdb_mcp::Server::new(&opener, common::writing_config(&audit)).unwrap();
    let mut input = br#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"elicitation":{}},"clientInfo":{"name":"fuzz","version":"1"}}}"#.to_vec();
    input.push(b'\n');
    for round in 1..=400u32 {
        input.extend_from_slice(
            format!(
                r#"{{"jsonrpc":"2.0","id":{round},"method":"tools/call","params":{{"name":"write_sql","arguments":{{"sql":"DROP TABLE scratch"}}}}}}"#
            )
            .as_bytes(),
        );
        input.push(b'\n');
        for _ in 0..seeded.below(4) + 1 {
            let template = seeded.pick(&ANSWERS);
            let asked = seeded.below(round as usize + 2);
            let mut line = template.replace('N', &asked.to_string()).into_bytes();
            match seeded.below(4) {
                0 => {
                    let at = seeded.below(line.len());
                    line[at] = (seeded.next() & 0x7f) as u8;
                }
                1 => line.truncate(seeded.below(line.len())),
                2 => line.extend_from_slice(mutated(&mut seeded).as_slice()),
                _ => {}
            }
            line.retain(|&byte| byte != b'\n');
            input.extend_from_slice(&line);
            input.push(b'\n');
        }
    }
    let mut output = Vec::new();
    server.serve(&mut input.as_slice(), &mut output).unwrap();
    let output = String::from_utf8(output).unwrap();
    for line in output.lines() {
        parsed(line);
    }
    assert_eq!(opener.store.snapshot(), before);
}
