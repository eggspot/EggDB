//! `eggdb mcp` run as a real child process, driven by a scripted MCP client over its stdin and
//! stdout, every stdout line checked to be a JSON-RPC message; and `eggdb audit` listing the
//! calls. The databases are built by the tests, never the frozen sample files (D96).

mod common;

use common::TempPath;
use eggdb_sql::json::{Json, Node, parse_document};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

const ANSWER_WAIT: Duration = Duration::from_secs(60);

fn eggdb(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn audit_path(database: &Path) -> PathBuf {
    let mut path = database.as_os_str().to_owned();
    path.push(".mcp-audit.jsonl");
    PathBuf::from(path)
}

struct Audit(PathBuf);

impl Drop for Audit {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn sample(name: &str, rows: usize) -> (TempPath, Audit) {
    let file = TempPath::new(name);
    let path = file.path().to_str().unwrap().to_owned();
    let mut sql = String::from(
        "CREATE TABLE users (id integer PRIMARY KEY, name text NOT NULL, email text); \
         INSERT INTO users VALUES (1, 'Ada', 'ada@example.com'), (2, 'Grace', NULL), (3, 'Margaret', 'mh@example.com');",
    );
    if rows > 0 {
        sql.push_str(" CREATE TABLE numbers (n integer PRIMARY KEY, v integer);");
        for chunk in (1..=rows).collect::<Vec<_>>().chunks(1000) {
            let values: Vec<String> = chunk.iter().map(|n| format!("({n}, {})", n % 97)).collect();
            sql.push_str(&format!(
                " INSERT INTO numbers VALUES {};",
                values.join(", ")
            ));
        }
    }
    let created = eggdb(&["sql", &path, "--create", "-c", &sql]);
    assert!(created.status.success(), "{}", stderr(&created));
    let put = eggdb(&["put", &path, "greeting", "hello"]);
    assert!(put.status.success(), "{}", stderr(&put));
    let audit = Audit(audit_path(file.path()));
    let _ = std::fs::remove_file(&audit.0);
    (file, audit)
}

fn is_json_rpc(node: &Node) -> bool {
    let Json::Object(members) = &node.value else {
        return false;
    };
    let has = |name: &str| members.iter().any(|(member, _)| member == name);
    has("jsonrpc") && has("id") && (has("result") ^ has("error"))
}

struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
}

impl Mcp {
    fn start(database: &Path, options: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_eggdb"))
            .arg("mcp")
            .arg(database)
            .args(options)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            lines,
            next_id: 0,
        }
    }

    fn send(&mut self, message: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(message.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    fn answer(&mut self) -> Node {
        let line = self.lines.recv_timeout(ANSWER_WAIT).expect("an answer");
        assert!(line.is_ascii(), "{line}");
        let node = parse_document(&line).unwrap_or_else(|error| panic!("{line}: {error:?}"));
        assert!(is_json_rpc(&node), "not a JSON-RPC answer: {line}");
        node
    }

    fn request(&mut self, method: &str, params: &str) -> Node {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
        ));
        let answer = self.answer();
        assert_eq!(text_at(&answer, &["id"]), id.to_string());
        answer
    }

    fn call(&mut self, tool: &str, arguments: &str) -> Node {
        self.request(
            "tools/call",
            &format!(r#"{{"name":"{tool}","arguments":{arguments}}}"#),
        )
    }

    fn initialize(&mut self) {
        let answer = self.request(
            "initialize",
            r#"{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"acceptance-test","version":"1.0"}}"#,
        );
        assert_eq!(
            text_at(&answer, &["result", "protocolVersion"]),
            "2025-11-25"
        );
        self.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    }

    fn finish(mut self) -> (ExitStatus, String) {
        drop(self.stdin.take());
        let status = self.child.wait().unwrap();
        while let Ok(line) = self.lines.recv_timeout(Duration::from_secs(5)) {
            let node = parse_document(&line).unwrap_or_else(|error| panic!("{line}: {error:?}"));
            assert!(is_json_rpc(&node), "{line}");
        }
        let mut stderr = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut pipe, &mut stderr);
        }
        (status, stderr)
    }
}

fn get<'n>(node: &'n Node, path: &[&str]) -> &'n Node {
    let mut node = node;
    for step in path {
        node = match &node.value {
            Json::Object(members) => members
                .iter()
                .find(|(name, _)| name == step)
                .map(|(_, value)| value)
                .unwrap_or_else(|| panic!("no {step} in {node:?}")),
            Json::Array(items) => &items[step.parse::<usize>().unwrap()],
            _ => panic!("cannot step into {step}"),
        };
    }
    node
}

fn text_at(node: &Node, path: &[&str]) -> String {
    match &get(node, path).value {
        Json::String(text) | Json::Number(text) => text.clone(),
        Json::Boolean(value) => value.to_string(),
        Json::Null => "null".to_owned(),
        other => panic!("not a scalar: {other:?}"),
    }
}

fn canonical(node: &Node) -> String {
    match &node.value {
        Json::Null => "null".to_owned(),
        Json::Boolean(value) => value.to_string(),
        Json::Number(text) => text.clone(),
        Json::String(text) => format!("{text:?}"),
        Json::Array(items) => format!(
            "[{}]",
            items.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        Json::Object(members) => format!(
            "{{{}}}",
            members
                .iter()
                .map(|(name, value)| format!("{name:?}:{}", canonical(value)))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

fn result(answer: &Node) -> &Node {
    get(answer, &["result", "structuredContent"])
}

fn error_code(answer: &Node) -> String {
    assert_eq!(
        text_at(answer, &["result", "isError"]),
        "true",
        "{answer:?}"
    );
    text_at(result(answer), &["error", "code"])
}

fn ok(answer: &Node) -> &Node {
    assert_eq!(
        text_at(answer, &["result", "isError"]),
        "false",
        "{answer:?}"
    );
    result(answer)
}

#[test]
fn an_agent_reads_but_never_writes_and_every_call_is_audited() {
    let (file, audit) = sample("mcp-acceptance", 0);
    let path = file.path().to_str().unwrap();
    let none_yet = eggdb(&["audit", path]);
    assert!(none_yet.status.success());
    assert!(
        stderr(&none_yet).contains("no MCP calls yet"),
        "{}",
        stderr(&none_yet)
    );
    let mut mcp = Mcp::start(file.path(), &[]);
    mcp.send(r#"{"jsonrpc":"2.0","id":"probe","method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#);
    let discover = mcp.answer();
    assert_eq!(text_at(&discover, &["error", "code"]), "-32601");
    mcp.initialize();
    let listed = mcp.request("tools/list", "{}");
    let Json::Array(tools) = &get(&listed, &["result", "tools"]).value else {
        panic!("{listed:?}");
    };
    assert_eq!(tools.len(), 7);
    let described = mcp.call("describe_table", r#"{"table":"users"}"#);
    let described = ok(&described);
    assert_eq!(text_at(described, &["columns", "2", "name"]), "email");
    assert_eq!(text_at(described, &["rows", "count"]), "3");
    let postgresql = mcp.call(
        "run_sql",
        r#"{"sql":"SELECT id, name FROM users WHERE email IS NOT NULL ORDER BY id"}"#,
    );
    let oracle = mcp.call(
        "run_sql",
        r#"{"sql":"SELECT id, name FROM users WHERE email IS NOT NULL ORDER BY id","dialect":"oracle"}"#,
    );
    assert_eq!(
        canonical(get(ok(&postgresql), &["rows"])),
        canonical(get(ok(&oracle), &["rows"]))
    );
    assert_eq!(text_at(ok(&oracle), &["rows", "1", "1"]), "Margaret");
    let write = mcp.call(
        "run_sql",
        r#"{"sql":"INSERT INTO users VALUES (4, 'Eve', NULL)"}"#,
    );
    assert_eq!(error_code(&write), "read_only");
    let sneaky = mcp.call("run_sql", r#"{"sql":"COMMIT; DROP TABLE users"}"#);
    assert_eq!(error_code(&sneaky), "transaction_control_refused");
    let still_there = mcp.call("run_sql", r#"{"sql":"SELECT count(*) FROM users"}"#);
    assert_eq!(text_at(ok(&still_there), &["rows", "0", "0"]), "3");
    let writer = eggdb(&[
        "sql",
        path,
        "-c",
        "INSERT INTO users VALUES (4, 'Katherine', NULL)",
    ]);
    assert!(writer.status.success(), "{}", stderr(&writer));
    let seen = mcp.call(
        "run_sql",
        r#"{"sql":"SELECT name FROM users WHERE id = 4"}"#,
    );
    assert_eq!(text_at(ok(&seen), &["rows", "0", "0"]), "Katherine");
    {
        let held = eggdb::Database::open(file.path()).unwrap();
        let busy = mcp.call("list_tables", "{}");
        assert_eq!(error_code(&busy), "database_busy");
        held.close().unwrap();
    }
    {
        let reader =
            eggdb::Database::open_with(file.path(), eggdb::Options::new().read_only(true)).unwrap();
        let beside = mcp.call("kv_get", r#"{"key":"greeting"}"#);
        assert_eq!(text_at(ok(&beside), &["entry", "value"]), "hello");
        reader.close().unwrap();
    }
    let many = mcp.call("run_sql", r#"{"sql":"SELECT id FROM users","max_rows":2}"#);
    assert_eq!(text_at(ok(&many), &["truncated_by"]), "max_rows");
    let (status, messages) = mcp.finish();
    assert!(status.success(), "{messages}");
    assert!(messages.contains("serving"), "{messages}");
    let listing = eggdb(&["audit", path, "--format", "text", "--color", "never"]);
    assert!(listing.status.success(), "{}", stderr(&listing));
    let text = stdout(&listing);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 11, "{text}");
    assert!(lines[0].contains("server started: eggdb"), "{text}");
    assert!(lines[1].contains("describe_table") && lines[1].contains("{\"table\":\"users\"}"));
    assert!(lines[4].contains("read_only") && lines[4].contains("INSERT INTO users"));
    assert!(lines[5].contains("transaction_control_refused"));
    assert!(lines[8].contains("database_busy"));
    assert!(lines[10].contains("2+"), "{}", lines[10]);
    let summary = stderr(&listing);
    assert!(
        summary.contains("10 calls, 3 errors, by acceptance-test 1.0 (the name the client gave)"),
        "{summary}"
    );
    let none_shown = eggdb(&[
        "audit", path, "-n", "0", "--format", "text", "--color", "never",
    ]);
    assert!(stdout(&none_shown).is_empty());
    assert!(
        stderr(&none_shown).starts_with("last 0 of 10 calls, 3 errors"),
        "{}",
        stderr(&none_shown)
    );
    let json = eggdb(&["audit", path, "--format", "json", "-n", "3"]);
    let records: Vec<Node> = stdout(&json)
        .lines()
        .map(|line| parse_document(line).unwrap())
        .collect();
    assert_eq!(records.len(), 3);
    assert_eq!(text_at(&records[2], &["tool"]), "run_sql");
    assert_eq!(text_at(&records[2], &["client", "name"]), "acceptance-test");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&audit.0).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn a_call_past_its_time_limit_is_stopped_and_a_stuck_one_ends_the_server() {
    let (file, audit) = sample("mcp-time-limit", 5000);
    let mut mcp = Mcp::start(file.path(), &["--time-limit", "1"]);
    mcp.initialize();
    let reads = mcp.call(
        "run_sql",
        r#"{"sql":"SELECT count(*) FROM numbers a WHERE (SELECT count(*) FROM numbers b WHERE b.n <= a.n) > 0"}"#,
    );
    assert_eq!(error_code(&reads), "time_limit");
    mcp.send(r#"{"jsonrpc":"2.0","id":99,"method":"tools/call","params":{"name":"run_sql","arguments":{"sql":"WITH k AS (SELECT n FROM numbers) SELECT count(*) FROM k x CROSS JOIN k y CROSS JOIN k z WHERE x.n + y.n + z.n < 0"}}}"#);
    let (status, _) = mcp.finish();
    assert_eq!(status.code(), Some(3));
    let records = std::fs::read_to_string(&audit.0).unwrap();
    assert!(
        records.contains("\"event\":\"time_limit_exceeded\""),
        "{records}"
    );
    let writer = eggdb(&["put", file.path().to_str().unwrap(), "after", "exit"]);
    assert!(writer.status.success(), "{}", stderr(&writer));
}

#[test]
fn a_log_left_by_an_unclean_writer_is_read_without_changing_the_file() {
    let (file, _audit) = sample("mcp-unclean", 0);
    {
        let db = eggdb::Database::open(file.path()).unwrap();
        db.put("late", "commit").unwrap();
        drop(db);
    }
    let before = std::fs::read(file.path()).unwrap();
    let mut mcp = Mcp::start(file.path(), &[]);
    mcp.initialize();
    let late = mcp.call("kv_get", r#"{"key":"late"}"#);
    assert_eq!(text_at(ok(&late), &["entry", "value"]), "commit");
    let (status, _) = mcp.finish();
    assert!(status.success());
    assert_eq!(std::fs::read(file.path()).unwrap(), before);
}

#[test]
fn a_damaged_file_is_reported_as_damaged() {
    let (file, _audit) = sample("mcp-damaged", 3000);
    let mut mcp = Mcp::start(file.path(), &[]);
    mcp.initialize();
    let mut bytes = std::fs::read(file.path()).unwrap();
    let page = 4096;
    for byte in &mut bytes[page * 2..] {
        *byte ^= 0x5a;
    }
    std::fs::write(file.path(), &bytes).unwrap();
    let answer = mcp.call("run_sql", r#"{"sql":"SELECT count(*) FROM numbers"}"#);
    assert_eq!(error_code(&answer), "damaged");
    let (status, _) = mcp.finish();
    assert!(status.success());
}

#[test]
fn startup_refuses_a_missing_file_an_unwritable_audit_file_and_a_bad_time_limit() {
    let missing = TempPath::new("mcp-missing");
    let path = missing.path().to_str().unwrap();
    let refused = eggdb(&["mcp", path]);
    assert_eq!(refused.status.code(), Some(3));
    assert!(
        stderr(&refused).contains("eggdb mcp never creates a database"),
        "{}",
        stderr(&refused)
    );
    assert!(!missing.path().exists());
    let (file, audit) = sample("mcp-audit-blocked", 0);
    std::fs::create_dir(&audit.0).unwrap();
    let blocked = eggdb(&["mcp", file.path().to_str().unwrap()]);
    assert_eq!(blocked.status.code(), Some(3));
    assert!(
        stderr(&blocked).contains("does not start without it"),
        "{}",
        stderr(&blocked)
    );
    std::fs::remove_dir(&audit.0).unwrap();
    for value in ["0", "301", "five"] {
        let bad = eggdb(&["mcp", file.path().to_str().unwrap(), "--time-limit", value]);
        assert_eq!(bad.status.code(), Some(2));
        assert!(
            stderr(&bad).contains("--time-limit must be a whole number of seconds from 1 to 300")
        );
    }
    let hex = eggdb(&["audit", file.path().to_str().unwrap(), "--format", "hex"]);
    assert_eq!(hex.status.code(), Some(2));
    let nowhere = eggdb(&["audit", path]);
    assert_eq!(nowhere.status.code(), Some(3));
}
