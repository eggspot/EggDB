//! `write_sql` on a server started with `--allow-write`: the tool and its annotations, granted
//! writes, the confirmation of destructive statements through `confirm: true` and through
//! `elicitation/create`, the wait for the user's answer, commits that are never reported
//! failed, and the audit records that pair a write's start with its outcome.

mod common;

use common::{
    MapOpener, MapStore, TempAudit, call, count, error_code, get, initialized, is_error, member,
    parsed, sample_store, served, structured, text_at, writing_config, writing_server,
};
use eggdb_mcp::Server;
use eggdb_sql::Dialect;
use eggdb_sql::json::{Json, Node};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn scratch_store() -> MapStore {
    let store = sample_store();
    store.sql(
        Dialect::PostgreSql,
        "CREATE TABLE scratch (id integer PRIMARY KEY, note text); INSERT INTO scratch VALUES (1, 'a'), (2, 'b')",
    );
    store
}

fn scratch_rows(store: &MapStore) -> i64 {
    count(store, Dialect::PostgreSql, "SELECT count(*) FROM scratch")
}

fn has_scratch(store: &MapStore) -> bool {
    store.snapshot().keys().any(|key| key.ends_with(b"scratch"))
}

fn tools(server: &mut Server<'_, MapOpener>) -> Vec<Node> {
    let listed = parsed(
        &server
            .handle(br#"{"jsonrpc":"2.0","id":90,"method":"tools/list"}"#)
            .unwrap(),
    );
    let Json::Array(tools) = &get(&listed, &["result", "tools"]).value else {
        panic!("{listed:?}");
    };
    tools.clone()
}

fn audit_records(audit: &TempAudit) -> Vec<Node> {
    audit.lines().iter().map(|line| parsed(line)).collect()
}

const ELICITING: &str = r#"{"elicitation":{}}"#;

fn initialize_line(version: &str, capabilities: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":0,"method":"initialize","params":{{"protocolVersion":"{version}","capabilities":{capabilities},"clientInfo":{{"name":"test-client","version":"1.0"}}}}}}"#
    )
}

fn call_line(id: u32, tool: &str, arguments: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{tool}","arguments":{arguments}}}}}"#
    )
}

fn answer_line(asked: u32, result: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":"eggdb-confirm-{asked}","result":{result}}}"#)
}

fn script(lines: &[String]) -> String {
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

fn with_id<'n>(answers: &'n [Node], id: &str) -> Option<&'n Node> {
    answers.iter().find(|answer| {
        member(answer, "id").is_some_and(|found| match &found.value {
            Json::Number(text) | Json::String(text) => text == id,
            _ => false,
        })
    })
}

fn methods(answers: &[Node]) -> Vec<String> {
    answers
        .iter()
        .filter_map(|answer| member(answer, "method"))
        .map(|method| match &method.value {
            Json::String(method) => method.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn write_sql_is_listed_only_with_allow_write_and_its_annotations_say_it_changes_data() {
    let audit = TempAudit::new("write-list");
    let opener = MapOpener::new(scratch_store());
    let mut reading = initialized(&opener, &audit);
    let names: Vec<String> = tools(&mut reading)
        .iter()
        .map(|tool| text_at(tool, &["name"]))
        .collect();
    assert!(!names.contains(&"write_sql".to_owned()), "{names:?}");
    let mut writing = writing_server(&opener, &audit, "{}");
    let listed = tools(&mut writing);
    for tool in &listed {
        let name = text_at(tool, &["name"]);
        let writes = name == "write_sql";
        assert_eq!(
            text_at(tool, &["annotations", "readOnlyHint"]),
            (!writes).to_string(),
            "{name}"
        );
        assert_eq!(
            text_at(tool, &["annotations", "destructiveHint"]),
            writes.to_string(),
            "{name}"
        );
    }
    let write = listed
        .iter()
        .find(|tool| text_at(tool, &["name"]) == "write_sql")
        .unwrap();
    assert_eq!(text_at(write, &["annotations", "idempotentHint"]), "false");
    assert_eq!(
        text_at(write, &["inputSchema", "properties", "confirm", "type"]),
        "boolean"
    );
    assert!(text_at(write, &["description"]).contains("Ask your user first"));
}

#[test]
fn a_read_only_server_refuses_write_sql_and_names_the_missing_grant() {
    let audit = TempAudit::new("write-refused");
    let opener = MapOpener::new(scratch_store());
    let mut server = initialized(&opener, &audit);
    let answer = call(
        &mut server,
        1,
        "write_sql",
        r#"{"sql":"INSERT INTO scratch VALUES (3, 'c')"}"#,
    );
    assert_eq!(error_code(&answer), "read_only");
    assert!(text_at(structured(&answer), &["error", "fix"]).contains("--allow-write"));
    assert_eq!(opener.write_opens.get(), 0);
    assert_eq!(scratch_rows(&opener.store), 2);
    let records = audit_records(&audit);
    assert_eq!(text_at(&records[1], &["tool"]), "write_sql");
    assert_eq!(text_at(&records[1], &["error_code"]), "read_only");
}

#[test]
fn granted_writes_commit_in_every_dialect_and_run_sql_still_only_reads() {
    let audit = TempAudit::new("granted");
    let opener = MapOpener::new(scratch_store());
    let mut server = writing_server(&opener, &audit, "{}");
    let refused = call(
        &mut server,
        1,
        "run_sql",
        r#"{"sql":"INSERT INTO scratch VALUES (3, 'c')"}"#,
    );
    assert_eq!(error_code(&refused), "read_only");
    assert_eq!(
        text_at(structured(&refused), &["error", "fix"]),
        "Send it with write_sql."
    );
    let inserted = call(
        &mut server,
        2,
        "write_sql",
        r#"{"sql":"INSERT INTO scratch VALUES (3, 'c')"}"#,
    );
    assert!(!is_error(&inserted), "{inserted:?}");
    assert_eq!(text_at(structured(&inserted), &["committed"]), "true");
    assert_eq!(text_at(structured(&inserted), &["rows_changed"]), "1");
    let updated = call(
        &mut server,
        3,
        "write_sql",
        r#"{"sql":"UPDATE scratch SET note = 'z' WHERE id = 1"}"#,
    );
    assert!(!is_error(&updated), "{updated:?}");
    for (id, dialect, sql) in [
        (4, "oracle", "INSERT INTO scratch VALUES (4, 'd')"),
        (5, "mysql", "INSERT INTO scratch VALUES (5, 'e')"),
        (6, "sqlite", "INSERT INTO scratch VALUES (6, 'f')"),
    ] {
        let answer = call(
            &mut server,
            id,
            "write_sql",
            &format!(r#"{{"sql":"{sql}","dialect":"{dialect}"}}"#),
        );
        assert!(!is_error(&answer), "{dialect}: {answer:?}");
        assert_eq!(text_at(structured(&answer), &["committed"]), "true");
    }
    assert_eq!(scratch_rows(&opener.store), 6);
    assert_eq!(
        count(
            &opener.store,
            Dialect::PostgreSql,
            "SELECT count(*) FROM scratch WHERE note = 'z'"
        ),
        1
    );
    let returning = call(
        &mut server,
        7,
        "write_sql",
        r#"{"sql":"INSERT INTO scratch VALUES (7, 'g') RETURNING id"}"#,
    );
    assert_eq!(text_at(structured(&returning), &["rows", "0", "0"]), "7");
    let failed = call(
        &mut server,
        8,
        "write_sql",
        r#"{"sql":"INSERT INTO scratch VALUES (1, 'dup')"}"#,
    );
    assert_eq!(error_code(&failed), "sql_error");
    assert_eq!(scratch_rows(&opener.store), 7);
    let records = audit_records(&audit);
    let started: Vec<&Node> = records
        .iter()
        .filter(|record| text_at(record, &["event"]) == "call_started")
        .collect();
    let calls: Vec<&Node> = records
        .iter()
        .filter(|record| {
            text_at(record, &["event"]) == "call" && text_at(record, &["tool"]) == "write_sql"
        })
        .collect();
    assert_eq!(started.len(), calls.len());
    for (start, call) in started.iter().zip(&calls) {
        assert_eq!(text_at(start, &["call_id"]), text_at(call, &["call_id"]));
    }
    assert_eq!(text_at(calls[0], &["committed"]), "true");
    assert_eq!(text_at(calls.last().unwrap(), &["committed"]), "false");
    assert_eq!(text_at(&records[0], &["allow_write"]), "true");
}

#[test]
fn a_destructive_statement_needs_confirm_true_when_the_client_cannot_ask() {
    let audit = TempAudit::new("confirm-argument");
    let opener = MapOpener::new(scratch_store());
    let mut server = writing_server(&opener, &audit, "{}");
    for (id, dialect, sql) in [
        (1, "postgresql", "DELETE FROM scratch"),
        (2, "postgresql", "DELETE FROM scratch WHERE 1 = 1"),
        (3, "postgresql", "DROP TABLE scratch"),
        (4, "oracle", "TRUNCATE TABLE scratch"),
        (5, "mysql", "UPDATE scratch SET note = 'x'"),
        (6, "sqlite", "ALTER TABLE scratch DROP COLUMN note"),
    ] {
        let answer = call(
            &mut server,
            id,
            "write_sql",
            &format!(r#"{{"sql":"{sql}","dialect":"{dialect}"}}"#),
        );
        assert_eq!(error_code(&answer), "needs_confirmation", "{sql}");
        assert!(
            text_at(structured(&answer), &["error", "fix"]).contains("confirm: true"),
            "{sql}"
        );
    }
    assert_eq!(opener.write_opens.get(), 0);
    assert_eq!(scratch_rows(&opener.store), 2);
    let confirmed = call(
        &mut server,
        7,
        "write_sql",
        r#"{"sql":"DELETE FROM scratch","confirm":true}"#,
    );
    assert!(!is_error(&confirmed), "{confirmed:?}");
    assert_eq!(scratch_rows(&opener.store), 0);
    let records = audit_records(&audit);
    let last = records.last().unwrap();
    assert_eq!(text_at(last, &["confirmation"]), "confirm_argument");
    assert_eq!(text_at(last, &["committed"]), "true");
    let refused = &records[1];
    assert_eq!(text_at(refused, &["error_code"]), "needs_confirmation");
    assert!(
        records
            .iter()
            .filter(|record| text_at(record, &["event"]) == "call_started")
            .count()
            == 1
    );
}

#[test]
fn a_client_that_can_ask_gets_an_elicitation_and_its_answer_decides() {
    for (answer, runs, code) in [
        (r#"{"action":"accept","content":{"run":true}}"#, true, None),
        (
            r#"{"action":"accept","content":{"run":false}}"#,
            false,
            Some("declined_by_user"),
        ),
        (r#"{"action":"decline"}"#, false, Some("declined_by_user")),
        (r#"{"action":"cancel"}"#, false, Some("declined_by_user")),
    ] {
        let audit = TempAudit::new("elicit");
        let opener = MapOpener::new(scratch_store());
        let mut server = Server::new(&opener, writing_config(&audit)).unwrap();
        let answers = served(
            &mut server,
            &script(&[
                initialize_line("2025-11-25", ELICITING),
                call_line(
                    2,
                    "write_sql",
                    r#"{"sql":"DROP TABLE scratch","confirm":true}"#,
                ),
                answer_line(1, answer),
            ]),
        );
        assert_eq!(answers.len(), 3, "{answers:?}");
        let asked = &answers[1];
        assert_eq!(text_at(asked, &["method"]), "elicitation/create");
        assert_eq!(text_at(asked, &["id"]), "eggdb-confirm-1");
        assert_eq!(text_at(asked, &["params", "mode"]), "form");
        let message = text_at(asked, &["params", "message"]);
        assert!(
            message.contains("DROP TABLE on app.db: scratch"),
            "{message}"
        );
        assert!(
            message.contains("```sql\nDROP TABLE scratch\n```"),
            "{message}"
        );
        assert_eq!(
            text_at(
                asked,
                &["params", "requestedSchema", "properties", "run", "type"]
            ),
            "boolean"
        );
        let result = with_id(&answers, "2").unwrap();
        match code {
            None => assert!(!is_error(result), "{result:?}"),
            Some(code) => assert_eq!(error_code(result), code),
        }
        assert_eq!(has_scratch(&opener.store), !runs, "{answer}");
        let records = audit_records(&audit);
        let last = records.last().unwrap();
        assert_eq!(text_at(last, &["confirmation"]), "elicitation");
    }
}

#[test]
fn an_error_answer_is_a_failed_confirmation_and_the_older_protocol_gets_no_mode() {
    let audit = TempAudit::new("elicit-error");
    let opener = MapOpener::new(scratch_store());
    let mut server = Server::new(&opener, writing_config(&audit)).unwrap();
    let answers = served(
        &mut server,
        &script(&[
            initialize_line("2025-06-18", ELICITING),
            call_line(2, "write_sql", r#"{"sql":"DELETE FROM scratch"}"#),
            r#"{"jsonrpc":"2.0","id":"eggdb-confirm-9","result":{"action":"accept","content":{"run":true}}}"#.to_owned(),
            r#"{"jsonrpc":"2.0","id":"eggdb-confirm-1","error":{"code":-32601,"message":"no"}}"#.to_owned(),
        ]),
    );
    assert!(member(get(&answers[1], &["params"]), "mode").is_none());
    assert_eq!(
        error_code(with_id(&answers, "2").unwrap()),
        "confirmation_failed"
    );
    assert_eq!(scratch_rows(&opener.store), 2);
}

#[test]
fn a_client_that_offers_only_url_elicitation_gets_the_confirm_path() {
    let audit = TempAudit::new("elicit-url");
    let opener = MapOpener::new(scratch_store());
    let mut server = Server::new(&opener, writing_config(&audit)).unwrap();
    let answers = served(
        &mut server,
        &script(&[
            initialize_line("2025-11-25", r#"{"elicitation":{"url":{}}}"#),
            call_line(2, "write_sql", r#"{"sql":"DROP TABLE scratch"}"#),
        ]),
    );
    assert!(methods(&answers).is_empty(), "{answers:?}");
    assert_eq!(
        error_code(with_id(&answers, "2").unwrap()),
        "needs_confirmation"
    );
    let form = TempAudit::new("elicit-form");
    let mut server = Server::new(&opener, writing_config(&form)).unwrap();
    let answers = served(
        &mut server,
        &script(&[
            initialize_line("2025-11-25", r#"{"elicitation":{"form":{},"url":{}}}"#),
            call_line(2, "write_sql", r#"{"sql":"DROP TABLE scratch"}"#),
            answer_line(1, r#"{"action":"decline"}"#),
        ]),
    );
    assert_eq!(methods(&answers), ["elicitation/create"]);
}

#[test]
fn while_the_user_is_asked_ping_is_answered_and_other_requests_wait_their_turn() {
    let audit = TempAudit::new("elicit-queue");
    let opener = MapOpener::new(scratch_store());
    let mut server = Server::new(&opener, writing_config(&audit)).unwrap();
    let mut lines = vec![
        initialize_line("2025-11-25", ELICITING),
        call_line(2, "write_sql", r#"{"sql":"DROP TABLE scratch"}"#),
        r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#.to_owned(),
        r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{}}"#.to_owned(),
    ];
    for id in 100..117 {
        lines.push(call_line(
            id,
            "run_sql",
            r#"{"sql":"SELECT count(*) FROM scratch"}"#,
        ));
    }
    lines.push(
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":101}}"#
            .to_owned(),
    );
    lines.push(answer_line(
        1,
        r#"{"action":"accept","content":{"run":true}}"#,
    ));
    let answers = served(&mut server, &script(&lines));
    let order: Vec<String> = answers
        .iter()
        .filter_map(|answer| member(answer, "id"))
        .map(|id| match &id.value {
            Json::Number(text) | Json::String(text) => text.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(order[0], "0");
    assert_eq!(order[1], "eggdb-confirm-1");
    assert_eq!(order[2], "3");
    assert_eq!(order[3], "116", "{order:?}");
    assert_eq!(
        text_at(with_id(&answers, "116").unwrap(), &["error", "code"]),
        "-32603"
    );
    assert_eq!(order[4], "2");
    assert!(!is_error(with_id(&answers, "2").unwrap()));
    assert!(with_id(&answers, "101").is_none());
    for id in [100, 102, 115] {
        let answer = with_id(&answers, &id.to_string()).unwrap();
        assert!(is_error(answer), "{id}: {answer:?}");
    }
    assert!(!has_scratch(&opener.store));
}

#[test]
fn a_cancelled_call_gets_no_answer_runs_nothing_and_the_server_withdraws_its_question() {
    let audit = TempAudit::new("elicit-cancel");
    let opener = MapOpener::new(scratch_store());
    let mut server = Server::new(&opener, writing_config(&audit)).unwrap();
    let answers = served(
        &mut server,
        &script(&[
            initialize_line("2025-11-25", ELICITING),
            call_line(2, "write_sql", r#"{"sql":"DROP TABLE scratch"}"#),
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2,"reason":"user pressed stop"}}"#.to_owned(),
            r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#.to_owned(),
        ]),
    );
    assert!(with_id(&answers, "2").is_none(), "{answers:?}");
    assert!(with_id(&answers, "3").is_some());
    assert_eq!(
        methods(&answers),
        ["elicitation/create", "notifications/cancelled"]
    );
    assert_eq!(
        text_at(&answers[2], &["params", "requestId"]),
        "eggdb-confirm-1"
    );
    assert!(has_scratch(&opener.store));
    let records = audit_records(&audit);
    assert_eq!(
        text_at(records.last().unwrap(), &["error_code"]),
        "cancelled"
    );
}

#[test]
fn the_client_going_away_while_the_user_is_asked_runs_nothing_and_is_audited() {
    let audit = TempAudit::new("elicit-eof");
    let opener = MapOpener::new(scratch_store());
    let mut server = Server::new(&opener, writing_config(&audit)).unwrap();
    let answers = served(
        &mut server,
        &script(&[
            initialize_line("2025-11-25", ELICITING),
            call_line(2, "write_sql", r#"{"sql":"DROP TABLE scratch"}"#),
        ]),
    );
    assert!(with_id(&answers, "2").is_none());
    assert_eq!(
        methods(&answers),
        ["elicitation/create", "notifications/cancelled"]
    );
    assert!(has_scratch(&opener.store));
    assert_eq!(
        text_at(audit_records(&audit).last().unwrap(), &["error_code"]),
        "cancelled"
    );
}

#[test]
fn a_committed_write_is_never_reported_failed() {
    let audit = TempAudit::new("committed-close");
    let opener = MapOpener::new(scratch_store());
    let mut server = writing_server(&opener, &audit, "{}");
    opener.close_fails.set(true);
    let inserted = call(
        &mut server,
        1,
        "write_sql",
        r#"{"sql":"INSERT INTO scratch VALUES (3, 'c')"}"#,
    );
    assert!(!is_error(&inserted), "{inserted:?}");
    assert!(text_at(structured(&inserted), &["warning"]).starts_with("committed; closing"));
    assert_eq!(scratch_rows(&opener.store), 3);
    let read = call(
        &mut server,
        2,
        "write_sql",
        r#"{"sql":"SELECT count(*) FROM scratch"}"#,
    );
    assert_eq!(error_code(&read), "database_unavailable");
    opener.close_fails.set(false);
    let record = audit_records(&audit);
    let committed = record
        .iter()
        .find(|record| {
            text_at(record, &["event"]) == "call" && text_at(record, &["tool"]) == "write_sql"
        })
        .unwrap();
    assert_eq!(text_at(committed, &["committed"]), "true");
    assert!(text_at(committed, &["warning"]).starts_with("committed;"));

    let path = audit.path.clone();
    *opener.after_close.borrow_mut() = Some(Box::new(move || {
        let _ = std::fs::remove_file(&path);
        std::fs::create_dir(&path).unwrap();
    }));
    let unrecorded = call(
        &mut server,
        3,
        "write_sql",
        r#"{"sql":"INSERT INTO scratch VALUES (4, 'd')"}"#,
    );
    *opener.after_close.borrow_mut() = None;
    assert_eq!(error_code(&unrecorded), "audit_unavailable");
    assert!(text_at(structured(&unrecorded), &["error", "message"]).contains("WAS committed"));
    assert_eq!(scratch_rows(&opener.store), 4);
    let before = opener.store.snapshot();
    let not_started = call(
        &mut server,
        4,
        "write_sql",
        r#"{"sql":"INSERT INTO scratch VALUES (5, 'e')"}"#,
    );
    assert_eq!(error_code(&not_started), "audit_unavailable");
    assert!(text_at(structured(&not_started), &["error", "message"]).contains("nothing was run"));
    assert_eq!(opener.store.snapshot(), before);
    std::fs::remove_dir(&audit.path).unwrap();
}

#[test]
fn a_file_that_cannot_be_written_is_refused_readably() {
    let audit = TempAudit::new("not-writable");
    let opener = MapOpener::new(scratch_store());
    let mut server = writing_server(&opener, &audit, "{}");
    opener.not_writable.set(true);
    let answer = call(
        &mut server,
        1,
        "write_sql",
        r#"{"sql":"INSERT INTO scratch VALUES (3, 'c')"}"#,
    );
    assert_eq!(error_code(&answer), "not_writable");
    assert!(text_at(structured(&answer), &["error", "fix"]).contains("write access"));
    opener.not_writable.set(false);
    opener.busy.set(true);
    let busy = call(
        &mut server,
        2,
        "write_sql",
        r#"{"sql":"INSERT INTO scratch VALUES (3, 'c')"}"#,
    );
    assert_eq!(error_code(&busy), "database_busy");
    assert_eq!(scratch_rows(&opener.store), 2);
}

static EXITED: AtomicBool = AtomicBool::new(false);

fn exit_for_test(_code: i32) -> ! {
    EXITED.store(true, Ordering::SeqCst);
    loop {
        std::thread::park();
    }
}

#[test]
fn a_write_that_committed_is_never_ended_by_the_watchdog() {
    let audit = TempAudit::new("committed-watchdog");
    let opener = MapOpener::new(scratch_store());
    let mut config = writing_config(&audit);
    config.time_limit = Duration::from_secs(1);
    config.exit_process = Some(exit_for_test);
    let mut server = Server::new(&opener, config).unwrap();
    server
        .handle(initialize_line("2025-11-25", "{}").as_bytes())
        .unwrap();
    opener.close_delay.set(Duration::from_secs(3));
    let answer = parsed(
        &server
            .handle(
                call_line(
                    1,
                    "write_sql",
                    r#"{"sql":"INSERT INTO scratch VALUES (3, 'c')"}"#,
                )
                .as_bytes(),
            )
            .unwrap(),
    );
    opener.close_delay.set(Duration::ZERO);
    assert!(!is_error(&answer), "{answer:?}");
    assert!(!EXITED.load(Ordering::SeqCst));
    assert_eq!(scratch_rows(&opener.store), 3);
    let records = audit_records(&audit);
    assert!(
        records
            .iter()
            .all(|record| text_at(record, &["event"]) != "time_limit_exceeded")
    );
}
