//! The command model's parity test (D143): every MCP tool that has a CLI twin returns the same
//! values as the twin run with the same input, once the tool's documented envelope (the data
//! notice, the per-call limits and what the call reports about itself) is taken away, and the
//! same error code and category for the same failure. `eggdb mcp` runs as a real child process
//! beside the `eggdb` commands, on databases the test builds.

mod common;

use common::TempPath;
use eggdb_sql::json::{Json, Node, parse_document};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

const ANSWER_WAIT: Duration = Duration::from_secs(60);

fn eggdb(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[track_caller]
fn succeeded(output: &Output) -> String {
    assert!(output.status.success(), "{}", text(&output.stderr));
    text(&output.stdout)
}

struct Audit(PathBuf);

impl Drop for Audit {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn audit_of(database: &Path) -> Audit {
    let mut path = database.as_os_str().to_owned();
    path.push(".mcp-audit.jsonl");
    let audit = Audit(PathBuf::from(path));
    let _ = std::fs::remove_file(&audit.0);
    audit
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
            .stderr(Stdio::null())
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
        let mut mcp = Self {
            child,
            stdin,
            lines,
            next_id: 0,
        };
        mcp.request(
            "initialize",
            r#"{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"parity-test","version":"1.0"}}"#,
        );
        mcp.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        mcp
    }

    fn send(&mut self, message: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(message.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: &str) -> Node {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
        ));
        let line = self.lines.recv_timeout(ANSWER_WAIT).expect("an answer");
        parse_document(&line).unwrap_or_else(|error| panic!("{line}: {error:?}"))
    }

    fn call(&mut self, tool: &str, arguments: &str) -> Node {
        self.request(
            "tools/call",
            &format!(r#"{{"name":"{tool}","arguments":{arguments}}}"#),
        )
    }

    fn finish(mut self) {
        self.stdin = None;
        let status = self.child.wait().unwrap();
        assert!(status.success());
    }
}

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members
            .iter()
            .find(|(member, _)| member == name)
            .map(|(_, value)| value),
        _ => None,
    }
}

fn at<'n>(node: &'n Node, path: &[&str]) -> &'n Node {
    path.iter().fold(node, |node, step| {
        member(node, step).unwrap_or_else(|| panic!("no {step} in {node:?}"))
    })
}

fn scalar(node: &Node) -> String {
    match &node.value {
        Json::String(text) | Json::Number(text) => text.clone(),
        Json::Boolean(value) => value.to_string(),
        Json::Null => "null".to_owned(),
        other => panic!("not a scalar: {other:?}"),
    }
}

fn canonical_without(node: &Node, removed: &[&str]) -> String {
    match &node.value {
        Json::Null => "null".to_owned(),
        Json::Boolean(value) => value.to_string(),
        Json::Number(text) => text.clone(),
        Json::String(text) => format!("{text:?}"),
        Json::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(|item| canonical_without(item, &[]))
                .collect::<Vec<_>>()
                .join(",")
        ),
        Json::Object(members) => format!(
            "{{{}}}",
            members
                .iter()
                .filter(|(name, _)| !removed.contains(&name.as_str()))
                .map(|(name, value)| format!("{name:?}:{}", canonical_without(value, &[])))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

fn canonical(node: &Node) -> String {
    canonical_without(node, &[])
}

fn structured(answer: &Node, is_error: bool) -> &Node {
    assert_eq!(
        scalar(at(answer, &["result", "isError"])),
        is_error.to_string(),
        "{answer:?}"
    );
    at(answer, &["result", "structuredContent"])
}

fn envelope(tool: &str) -> &'static [&'static str] {
    eggdb_commands::tool(tool)
        .map(|definition| definition.envelope)
        .unwrap_or_else(|| panic!("{tool} is not in the registry"))
}

fn as_its_twin_prints_it(entry: &Node) -> String {
    let removed: &[&str] = if member(entry, "key").is_some() {
        &["key_hex", "value_bytes"]
    } else {
        &["value_bytes"]
    };
    canonical_without(entry, removed)
}

fn lines(stdout: &str) -> Vec<Node> {
    stdout
        .lines()
        .map(|line| parse_document(line).unwrap_or_else(|error| panic!("{line}: {error:?}")))
        .collect()
}

fn one_error(output: &Output) -> Node {
    let errors = text(&output.stderr);
    let parsed = lines(&errors);
    assert_eq!(parsed.len(), 1, "one JSON line on stderr: {errors}");
    parsed.into_iter().next().unwrap()
}

fn code_and_category(error: &Node) -> (String, String) {
    (
        scalar(at(error, &["error", "code"])),
        scalar(at(error, &["error", "category"])),
    )
}

fn key_value_file(name: &str) -> TempPath {
    let file = TempPath::new(name);
    let path = file.path().to_str().unwrap().to_owned();
    succeeded(&eggdb(&["put", "--create", &path, "user:1", "Ada"]));
    succeeded(&eggdb(&["put", &path, "user:2", "Grace"]));
    succeeded(&eggdb(&["put", &path, "user:3", "Margaret"]));
    succeeded(&eggdb(&["put", &path, "0x00ff", "0xff10"]));
    succeeded(&eggdb(&["put", &path, "zebra", "last"]));
    file
}

#[test]
fn kv_get_and_kv_scan_return_what_get_and_scan_print() {
    let file = key_value_file("parity-kv");
    let path = file.path().to_str().unwrap().to_owned();
    let _audit = audit_of(file.path());
    let mut mcp = Mcp::start(file.path(), &[]);

    for (arguments, key) in [(r#"{"key":"user:2"}"#, "user:2"), (r#"{"key_hex":"00ff"}"#, "0x00ff")] {
        let answer = mcp.call("kv_get", arguments);
        let result = structured(&answer, false);
        assert_eq!(scalar(at(result, &["found"])), "true");
        let printed = lines(&succeeded(&eggdb(&["get", &path, key, "--json"])));
        assert_eq!(printed.len(), 1);
        assert_eq!(as_its_twin_prints_it(at(result, &["entry"])), canonical(&printed[0]), "{key}");
        for removed in envelope("kv_get") {
            assert!(member(result, removed).is_some(), "kv_get has no {removed}");
        }
    }
    let missing = mcp.call("kv_get", r#"{"key":"nobody"}"#);
    assert_eq!(scalar(at(structured(&missing, false), &["found"])), "false");
    let absent = eggdb(&["get", &path, "nobody", "--json"]);
    assert_eq!(absent.status.code(), Some(1));
    assert!(absent.stdout.is_empty() && absent.stderr.is_empty());

    for (arguments, options) in [
        (r#"{"prefix":"user:"}"#, vec!["--prefix", "user:"]),
        (r#"{"start":"user:2","end":"zebra"}"#, vec!["--from", "user:2", "--to", "zebra"]),
        (r#"{"prefix":"user:","keys_only":true}"#, vec!["--prefix", "user:", "--keys-only"]),
        (r#"{"max_entries":2}"#, vec!["--limit", "2"]),
    ] {
        let answer = mcp.call("kv_scan", arguments);
        let result = structured(&answer, false);
        let Json::Array(entries) = &at(result, &["entries"]).value else {
            panic!("{result:?}");
        };
        let mut args = vec!["scan", path.as_str(), "--json"];
        args.extend(options.iter().copied());
        let printed = lines(&succeeded(&eggdb(&args)));
        let from_tool: Vec<String> = entries.iter().map(as_its_twin_prints_it).collect();
        let from_twin: Vec<String> = printed.iter().map(canonical).collect();
        assert_eq!(from_tool, from_twin, "{arguments}");
        for removed in envelope("kv_scan") {
            if *removed != "next_start_hex" {
                assert!(member(result, removed).is_some(), "kv_scan has no {removed}");
            }
        }
    }

    std::fs::remove_file(file.path()).unwrap();
    let gone = mcp.call("kv_get", r#"{"key":"user:1"}"#);
    let tool_error = code_and_category(structured(&gone, true));
    let twin = eggdb(&["get", &path, "user:1", "--json"]);
    assert_eq!(twin.status.code(), Some(3));
    assert_eq!(code_and_category(&one_error(&twin)), tool_error);
    assert_eq!(tool_error, ("not_found".to_owned(), "not_found".to_owned()));
    mcp.finish();
}

#[test]
fn run_sql_and_write_sql_return_what_sql_prints() {
    let file = TempPath::new("parity-sql");
    let path = file.path().to_str().unwrap().to_owned();
    succeeded(&eggdb(&[
        "sql",
        "--create",
        &path,
        "-c",
        "CREATE TABLE users (id integer PRIMARY KEY, name text NOT NULL, born date, admin boolean); \
         INSERT INTO users VALUES (1, 'Ada', '1815-12-10', true), (2, 'Grace', NULL, false)",
    ]));
    let _audit = audit_of(file.path());
    let mut mcp = Mcp::start(file.path(), &["--allow-write"]);

    for (dialect, sql) in [
        ("postgresql", "SELECT id, name, born, admin FROM users ORDER BY id"),
        ("mysql", "SELECT id, name, admin FROM users ORDER BY id"),
        ("postgresql", "SELECT count(*) AS n, max(id) FROM users"),
    ] {
        let arguments = format!(r#"{{"sql":{sql:?},"dialect":"{dialect}"}}"#);
        let answer = mcp.call("run_sql", &arguments);
        let result = structured(&answer, false);
        let printed = lines(&succeeded(&eggdb(&[
            "sql", &path, "--json", "--dialect", dialect, "-c", sql,
        ])));
        assert_eq!(printed.len(), 1, "{sql}");
        assert_eq!(
            canonical_without(result, envelope("run_sql")),
            canonical(&printed[0]),
            "{dialect}: {sql}"
        );
    }

    let answer = mcp.call("write_sql", r#"{"sql":"INSERT INTO users VALUES (3, 'Eve', NULL, NULL)"}"#);
    let written = structured(&answer, false);
    assert_eq!(scalar(at(written, &["committed"])), "true");
    let printed = lines(&succeeded(&eggdb(&[
        "sql",
        &path,
        "--json",
        "-c",
        "INSERT INTO users VALUES (4, 'Bob', NULL, NULL)",
    ])));
    assert_eq!(printed.len(), 1);
    assert_eq!(canonical_without(written, envelope("write_sql")), canonical(&printed[0]));

    let wrong = "SELECT * FROM nowhere";
    let refused = mcp.call("run_sql", &format!(r#"{{"sql":{wrong:?}}}"#));
    let refused = structured(&refused, true);
    let twin = eggdb(&["sql", &path, "--json", "-c", wrong]);
    assert_eq!(twin.status.code(), Some(3));
    assert!(twin.stdout.is_empty());
    let printed = one_error(&twin);
    assert_eq!(code_and_category(&printed), code_and_category(refused));
    assert_eq!(
        scalar(at(&printed, &["error", "sqlstate"])),
        scalar(at(refused, &["error", "sqlstate"]))
    );
    assert_eq!(
        scalar(at(&printed, &["error", "message"])),
        scalar(at(refused, &["error", "message"]))
    );
    assert_eq!(
        code_and_category(&printed),
        ("sql_error".to_owned(), "failed".to_owned())
    );
    mcp.finish();
}

#[test]
fn the_tools_list_carries_the_registry_s_names_and_annotations() {
    let file = key_value_file("parity-tools");
    let _audit = audit_of(file.path());
    for (options, offered) in [(&[][..], 8), (&["--allow-write"][..], 9)] {
        let mut mcp = Mcp::start(file.path(), options);
        let listed = mcp.request("tools/list", "{}");
        let Json::Array(tools) = &at(&listed, &["result", "tools"]).value else {
            panic!("{listed:?}");
        };
        assert_eq!(tools.len(), offered);
        for listed in tools {
            let name = scalar(at(listed, &["name"]));
            let Some(definition) = eggdb_commands::tool(&name) else {
                panic!("{name} is not in the registry");
            };
            let annotations = definition.annotations();
            for (hint, expected) in [
                ("readOnlyHint", annotations.read_only),
                ("destructiveHint", annotations.destructive),
                ("idempotentHint", annotations.idempotent),
                ("openWorldHint", annotations.open_world),
            ] {
                assert_eq!(
                    scalar(at(listed, &["annotations", hint])),
                    expected.to_string(),
                    "{name} {hint}"
                );
            }
        }
        mcp.finish();
    }
}

fn catalog_file(name: &str) -> TempPath {
    let file = TempPath::new(name);
    let path = file.path().to_str().unwrap().to_owned();
    succeeded(&eggdb(&[
        "sql",
        "--create",
        &path,
        "-c",
        "CREATE TABLE users (id integer PRIMARY KEY, name text NOT NULL, email text DEFAULT 'unknown'); \
         COMMENT ON TABLE users IS 'the people who sign in'; \
         CREATE INDEX users_email ON users (email); \
         INSERT INTO users (id, name) VALUES (1, 'Ada'), (2, 'Grace'), (3, 'Margaret'); \
         CREATE TABLE orders (id integer PRIMARY KEY, user_id integer, total numeric); \
         CREATE SCHEMA sales; \
         CREATE VIEW sales.totals AS SELECT user_id, sum(total) AS total FROM orders GROUP BY user_id",
    ]));
    file
}

fn members_of<'n>(node: &'n Node, name: &str) -> &'n [Node] {
    match &at(node, &[name]).value {
        Json::Array(items) => items,
        other => panic!("{name} is not an array: {other:?}"),
    }
}

fn with_kind(item: &Node, kind: &str) -> String {
    let Json::Object(members) = &item.value else {
        panic!("{item:?}");
    };
    let mut fields: Vec<String> = Vec::new();
    for (name, value) in members {
        fields.push(format!("{name:?}:{}", canonical(value)));
        if name == "name" {
            fields.push(format!("\"kind\":{kind:?}"));
        }
    }
    format!("{{{}}}", fields.join(","))
}

#[test]
fn the_table_tools_return_what_table_list_show_and_sample_print() {
    let file = catalog_file("parity-tables");
    let path = file.path().to_str().unwrap().to_owned();
    let _audit = audit_of(file.path());
    let mut mcp = Mcp::start(file.path(), &[]);

    for (arguments, options) in [("{}", vec![]), (r#"{"schema":"sales"}"#, vec!["--schema", "sales"])] {
        let answer = mcp.call("list_tables", arguments);
        let result = structured(&answer, false);
        let mut from_tool: Vec<String> = members_of(result, "tables")
            .iter()
            .map(|table| with_kind(table, "table"))
            .collect();
        from_tool.extend(members_of(result, "views").iter().map(|view| with_kind(view, "view")));
        let mut args = vec!["table", "list", path.as_str(), "--json"];
        args.extend(options.iter().copied());
        let printed: Vec<String> = lines(&succeeded(&eggdb(&args))).iter().map(canonical).collect();
        assert_eq!(from_tool, printed, "{arguments}");
    }

    for name in ["users", "USERS", "sales.totals"] {
        let answer = mcp.call("describe_table", &format!(r#"{{"table":{name:?}}}"#));
        let result = structured(&answer, false);
        let printed = lines(&succeeded(&eggdb(&["table", "show", &path, name, "--json"])));
        assert_eq!(printed.len(), 1);
        assert_eq!(
            canonical_without(result, envelope("describe_table")),
            canonical(&printed[0]),
            "{name}"
        );
    }

    for (max_rows, limit) in [(10, "10"), (2, "2")] {
        let answer = mcp.call("sample_rows", &format!(r#"{{"table":"users","max_rows":{max_rows}}}"#));
        let result = structured(&answer, false);
        let printed = lines(&succeeded(&eggdb(&["table", "sample", &path, "users", "-n", limit, "--json"])));
        assert_eq!(printed.len(), 1);
        let line = &printed[0];
        assert_eq!(canonical(at(result, &["rows"])), canonical(at(line, &["rows"])), "{limit}");
        assert_eq!(canonical(at(result, &["columns"])), canonical(at(line, &["columns"])));
        assert_eq!(scalar(at(result, &["truncated"])), scalar(at(line, &["truncated"])));
        assert_eq!(scalar(at(result, &["returned_rows"])), scalar(at(line, &["row_count"])));
        if max_rows == 10 {
            assert_eq!(
                canonical_without(result, envelope("sample_rows")),
                canonical(line)
            );
        }
    }

    let missing = mcp.call("describe_table", r#"{"table":"nowhere"}"#);
    let tool_error = code_and_category(structured(&missing, true));
    for verb in ["show", "sample"] {
        let twin = eggdb(&["table", verb, &path, "nowhere", "--json"]);
        assert_eq!(twin.status.code(), Some(1), "{verb}");
        let (code, category) = code_and_category(&one_error(&twin));
        assert_eq!(code, "table_not_found");
        assert_eq!(category, tool_error.1, "the same category as the tool's {}", tool_error.0);
        assert_eq!(
            scalar(at(&one_error(&twin), &["error", "fix_command"])),
            format!("eggdb table list {path}")
        );
    }
    mcp.finish();
}

#[test]
fn explain_query_and_run_sql_return_what_explain_and_sql_read_only_print() {
    let file = catalog_file("parity-explain");
    let path = file.path().to_str().unwrap().to_owned();
    let _audit = audit_of(file.path());
    let mut mcp = Mcp::start(file.path(), &[]);

    for (dialect, sql) in [
        ("postgresql", "SELECT * FROM users WHERE id = 2"),
        ("sqlite", "SELECT name FROM users WHERE id = 2"),
        ("mysql", "SELECT name FROM users WHERE id = 2"),
    ] {
        let answer = mcp.call(
            "explain_query",
            &format!(r#"{{"sql":{sql:?},"dialect":"{dialect}"}}"#),
        );
        let result = structured(&answer, false);
        let printed = lines(&succeeded(&eggdb(&[
            "explain", &path, "--json", "--dialect", dialect, "-c", sql,
        ])));
        assert_eq!(printed.len(), 1, "{dialect}");
        assert_eq!(
            canonical_without(result, envelope("explain_query")),
            canonical(&printed[0]),
            "{dialect}: {sql}"
        );
    }
    for (arguments, args) in [
        (
            r#"{"sql":"SELECT 1","dialect":"oracle"}"#,
            vec!["explain", path.as_str(), "--json", "--dialect", "oracle", "-c", "SELECT 1"],
        ),
        (
            r#"{"sql":"SELECT 1; SELECT 2"}"#,
            vec!["explain", path.as_str(), "--json", "-c", "SELECT 1; SELECT 2"],
        ),
    ] {
        let answer = mcp.call("explain_query", arguments);
        let tool_error = code_and_category(structured(&answer, true));
        let twin = eggdb(&args);
        assert_eq!(code_and_category(&one_error(&twin)), tool_error, "{arguments}");
    }
    let doubled = eggdb(&["explain", &path, "-c", "EXPLAIN SELECT 1"]);
    assert_eq!(doubled.status.code(), Some(2));
    let text_plan = succeeded(&eggdb(&["explain", &path, "-c", "SELECT * FROM users"]));
    assert!(text_plan.ends_with(")\n\n"), "no tag line after the plan: {text_plan}");

    let select = "SELECT id, name FROM users ORDER BY id";
    let answer = mcp.call("run_sql", &format!(r#"{{"sql":{select:?}}}"#));
    let printed = lines(&succeeded(&eggdb(&["sql", &path, "--read-only", "--json", "-c", select])));
    assert_eq!(printed.len(), 1);
    assert_eq!(
        canonical_without(structured(&answer, false), envelope("run_sql")),
        canonical(&printed[0])
    );
    let write = "INSERT INTO users (id, name) VALUES (9, 'Eve')";
    let refused = mcp.call("run_sql", &format!(r#"{{"sql":{write:?}}}"#));
    let tool_error = code_and_category(structured(&refused, true));
    let twin = eggdb(&["sql", &path, "--read-only", "--json", "-c", write]);
    assert_eq!(twin.status.code(), Some(3));
    let printed = one_error(&twin);
    assert_eq!(code_and_category(&printed), tool_error);
    assert_eq!(tool_error, ("read_only".to_owned(), "read_only".to_owned()));
    assert_eq!(scalar(at(&printed, &["error", "sqlstate"])), "25006");
    let as_text = eggdb(&["sql", &path, "--read-only", "-c", write]);
    let shown = text(&as_text.stderr);
    assert!(
        shown.starts_with("ERROR:  cannot execute INSERT in a read-only transaction\n")
            && shown.ends_with("SQLSTATE:  25006\n"),
        "{shown}"
    );
    let count = succeeded(&eggdb(&["sql", &path, "-c", "SELECT count(*) FROM users"]));
    assert!(count.contains(" 3\n"), "nothing was written: {count}");
    mcp.finish();
}

fn page_trailer(records: &mut Vec<Node>, limit: usize) -> Option<String> {
    if records.len() <= limit {
        return None;
    }
    let trailer = records.pop()?;
    assert_eq!(scalar(at(&trailer, &["truncated"])), "true");
    Some(scalar(at(&trailer, &["next_cursor"])))
}

#[test]
fn row_list_returns_what_row_list_prints_page_by_page() {
    let file = catalog_file("parity-rows");
    let path = file.path().to_str().unwrap().to_owned();
    let _audit = audit_of(file.path());
    let mut mcp = Mcp::start(file.path(), &[]);

    let mut after: Option<String> = None;
    let mut pages = 0;
    loop {
        let arguments = match &after {
            Some(cursor) => format!(r#"{{"table":"users","limit":2,"after":{cursor:?}}}"#),
            None => r#"{"table":"users","limit":2}"#.to_owned(),
        };
        let answer = mcp.call("row_list", &arguments);
        let result = structured(&answer, false);
        let mut args = vec!["row", "list", path.as_str(), "users", "--limit", "2", "--json"];
        if let Some(cursor) = &after {
            args.extend(["--after", cursor.as_str()]);
        }
        let mut printed = lines(&succeeded(&eggdb(&args)));
        let trailer = page_trailer(&mut printed, 2);
        let from_tool: Vec<String> = members_of(result, "items").iter().map(canonical).collect();
        let from_twin: Vec<String> = printed.iter().map(canonical).collect();
        assert_eq!(from_tool, from_twin, "page {pages}");
        assert_eq!(
            scalar(at(result, &["truncated"])),
            trailer.is_some().to_string()
        );
        let tool_cursor = member(result, "next_cursor").map(scalar);
        assert_eq!(tool_cursor, trailer, "page {pages}");
        if trailer.is_some() {
            assert_eq!(scalar(at(result, &["truncated_by"])), "limit");
        }
        assert_eq!(members_of(result, "columns").len(), 3);
        for removed in envelope("row_list") {
            if *removed != "truncated_by" {
                assert!(member(result, removed).is_some(), "row_list has no {removed}");
            }
        }
        pages += 1;
        match trailer {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    assert_eq!(pages, 2);

    let whole = mcp.call("row_list", r#"{"table":"users"}"#);
    let whole = structured(&whole, false);
    let printed = lines(&succeeded(&eggdb(&["row", "list", &path, "users", "--json"])));
    let from_tool: Vec<String> = members_of(whole, "items").iter().map(canonical).collect();
    assert_eq!(from_tool, printed.iter().map(canonical).collect::<Vec<_>>());
    assert_eq!(scalar(at(whole, &["truncated"])), "false");
    assert!(member(whole, "next_cursor").is_none());

    let empty = mcp.call("row_list", r#"{"table":"orders"}"#);
    assert!(members_of(structured(&empty, false), "items").is_empty());
    assert!(succeeded(&eggdb(&["row", "list", &path, "orders", "--json"])).is_empty());

    let first = lines(&succeeded(&eggdb(&["row", "list", &path, "users", "-n", "1", "--json"])));
    let users_cursor = scalar(at(&first[1], &["next_cursor"]));
    for (arguments, args) in [
        (
            r#"{"table":"sales.totals"}"#.to_owned(),
            vec!["row", "list", path.as_str(), "sales.totals", "--json"],
        ),
        (
            format!(r#"{{"table":"orders","after":{users_cursor:?}}}"#),
            vec!["row", "list", path.as_str(), "orders", "--json", "--after", users_cursor.as_str()],
        ),
        (
            r#"{"table":"users","after":"42"}"#.to_owned(),
            vec!["row", "list", path.as_str(), "users", "--json", "--after", "42"],
        ),
    ] {
        let answer = mcp.call("row_list", &arguments);
        let tool_error = code_and_category(structured(&answer, true));
        let twin = eggdb(&args);
        assert!(twin.stdout.is_empty(), "{arguments}");
        assert_eq!(code_and_category(&one_error(&twin)), tool_error, "{arguments}");
    }
    let missing = mcp.call("row_list", r#"{"table":"nowhere"}"#);
    let tool_error = code_and_category(structured(&missing, true));
    let twin = eggdb(&["row", "list", &path, "nowhere", "--json"]);
    assert_eq!(twin.status.code(), Some(1));
    let (code, category) = code_and_category(&one_error(&twin));
    assert_eq!(code, "table_not_found");
    assert_eq!(category, tool_error.1);
    mcp.finish();
}

fn words_of(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for character in line.chars() {
        match (quote, character) {
            (Some(open), character) if character == open => quote = None,
            (Some(_), character) => word.push(character),
            (None, '"' | '\'') => {
                quote = Some(character);
                in_word = true;
            }
            (None, character) if character.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, character) => {
                word.push(character);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

struct ExampleDirectory(PathBuf);

impl Drop for ExampleDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn every_example_of_a_reading_command_runs_against_a_temporary_file() {
    let directory = ExampleDirectory(
        std::env::temp_dir().join(format!("eggdb-examples-{}", std::process::id())),
    );
    std::fs::create_dir_all(&directory.0).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_eggdb"))
            .args(args)
            .current_dir(&directory.0)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    for setup in [
        &["sql", "--create", "app.db", "-c", "CREATE TABLE users (id integer PRIMARY KEY, name text); INSERT INTO users VALUES (1, 'Ada'), (2, 'Grace'); CREATE TABLE orders (id integer PRIMARY KEY, placed_at timestamp); CREATE SCHEMA sales; CREATE VIEW sales.totals AS SELECT count(*) AS n FROM orders; CREATE TABLE sales.orders (id integer PRIMARY KEY)"][..],
        &["put", "app.db", "user:1", "Ada"],
        &["put", "app.db", "0x00ff", "0x01"],
        &["collection", "add", "app.db", "sessions"],
        &["put", "app.db", "-C", "sessions", "s:1", "x", "--ttl", "30m"],
    ] {
        succeeded(&run(setup));
    }
    let mut ran = 0;
    for command in eggdb_commands::COMMANDS {
        if command.max_effect != eggdb_commands::Effect::Read {
            continue;
        }
        for example in command.examples {
            let one_command = example.starts_with("eggdb ")
                && !["|", "<", ">", "$(", "&&"].iter().any(|shell| example.contains(shell));
            if !one_command {
                continue;
            }
            let words = words_of(example);
            let args: Vec<&str> = words[1..].iter().map(String::as_str).collect();
            let output = run(&args);
            assert_eq!(
                output.status.code(),
                Some(0),
                "{example}: {}",
                text(&output.stderr)
            );
            ran += 1;
        }
    }
    assert!(ran >= 20, "{ran} examples ran");
}
