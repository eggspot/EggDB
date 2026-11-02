//! What keeps one client, or another program on the same machine, from locking users out of
//! the server or exhausting its memory: login slots that idle sockets lose to real logins,
//! caps on prepared statements, portals and held bytes, the server-wide memory budget and the
//! idle-session timeout.

mod common;

use common::{Client, Limits, Message, TestServer, cstring, tags};
use eggdb_platform::connection_limits::MemoryBudget;
use std::sync::Arc;
use std::time::{Duration, Instant};

const IDLE_TIMEOUT: Duration = Duration::from_millis(1500);

fn parse(name: &str, query: &str) -> Vec<u8> {
    let mut body = cstring(name);
    body.extend(cstring(query));
    body.extend_from_slice(&0i16.to_be_bytes());
    body
}

fn bind_text(portal: &str, statement: &str, value: &[u8]) -> Vec<u8> {
    let mut body = cstring(portal);
    body.extend(cstring(statement));
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&1i16.to_be_bytes());
    body.extend_from_slice(&(value.len() as i32).to_be_bytes());
    body.extend_from_slice(value);
    body.extend_from_slice(&0i16.to_be_bytes());
    body
}

fn error_codes(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| message.tag == b'E')
        .map(Message::code)
        .collect()
}

fn wait_until(condition: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn idle_sockets_cannot_lock_a_real_login_out() {
    let server = TestServer::start(64);
    let idle: Vec<Client> = (0..eggdb_pgwire::MAX_UNAUTHENTICATED_CONNECTIONS + 4)
        .map(|_| Client::connect(server.address))
        .collect();
    std::thread::sleep(Duration::from_millis(1300));
    let started = Instant::now();
    let (mut client, _) = Client::login(server.address);
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(tags(&client.query("SELECT 1")), "TDCZ");
    let (mut second, _) = Client::login(server.address);
    assert_eq!(tags(&second.query("SELECT 2")), "TDCZ");
    drop(idle);
}

#[test]
fn ten_thousand_parse_messages_end_in_a_clean_error_after_the_cap() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    let mut pipeline = Vec::new();
    for index in 0..10_000 {
        let body = parse(&format!("s{index}"), "SELECT 1");
        pipeline.push(b'P');
        pipeline.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
        pipeline.extend_from_slice(&body);
    }
    pipeline.extend_from_slice(&[b'S', 0, 0, 0, 4]);
    client.send_raw(&pipeline);
    let answer = client.read_until_ready();
    let parsed = answer.iter().filter(|message| message.tag == b'1').count();
    assert_eq!(parsed, eggdb_pgwire::MAX_PREPARED_STATEMENTS);
    assert_eq!(error_codes(&answer), ["54000"]);
    assert_eq!(answer.last().map(|message| message.tag), Some(b'Z'));
    assert_eq!(tags(&client.query("SELECT 1")), "TDCZ");
    client.send(b'C', b"Ss0\0");
    client.send(b'P', &parse("one_more", "SELECT 2"));
    client.send(b'S', b"");
    assert_eq!(tags(&client.read_until_ready()), "31Z");
}

#[test]
fn portals_are_capped_per_connection() {
    let server = TestServer::start(8);
    let (mut client, _) = Client::login(server.address);
    client.query("BEGIN");
    client.send(b'P', &parse("", "SELECT $1::text"));
    for index in 0..=eggdb_pgwire::MAX_PORTALS {
        client.send(b'B', &bind_text(&format!("p{index}"), "", b"x"));
    }
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    let bound = answer.iter().filter(|message| message.tag == b'2').count();
    assert_eq!(bound, eggdb_pgwire::MAX_PORTALS);
    assert_eq!(error_codes(&answer), ["54000"]);
}

#[test]
fn a_message_past_the_memory_budget_ends_the_connection_and_gives_the_bytes_back() {
    let budget = Arc::new(MemoryBudget::new(128 * 1024));
    let server = TestServer::start_limited(
        8,
        Limits {
            idle_timeout: None,
            memory_budget: Arc::clone(&budget),
        },
    );
    let (mut client, _) = Client::login(server.address);
    client.send(b'P', &parse("", "SELECT length($1::text)"));
    client.send(b'B', &bind_text("", "", &vec![b'a'; 40 * 1024]));
    client.send(b'E', &[0, 0, 0, 0, 0]);
    client.send(b'S', b"");
    let answer = client.read_until_ready();
    assert!(error_codes(&answer).is_empty(), "{answer:?}");
    assert!(budget.used_bytes() <= budget.limit_bytes());
    client.send(b'B', &bind_text("", "", &vec![b'a'; 150 * 1024]));
    let refused = client
        .read()
        .expect("an error before the connection closes");
    assert_eq!(refused.code(), "53200");
    assert_eq!(refused.field(b'S').as_deref(), Some("FATAL"));
    assert!(client.read().is_none());
    assert!(wait_until(|| budget.used_bytes() == 0));
    let (mut next, _) = Client::login(server.address);
    assert_eq!(tags(&next.query("SELECT 1")), "TDCZ");
}

#[test]
fn prepared_statements_count_against_the_server_memory_budget() {
    let budget = Arc::new(MemoryBudget::new(512 * 1024));
    let server = TestServer::start_limited(
        8,
        Limits {
            idle_timeout: None,
            memory_budget: Arc::clone(&budget),
        },
    );
    let (mut client, _) = Client::login(server.address);
    let text = format!("SELECT '{}'", "b".repeat(60 * 1024));
    let mut codes = Vec::new();
    for index in 0..16 {
        client.send(b'P', &parse(&format!("big{index}"), &text));
        client.send(b'S', b"");
        codes.extend(error_codes(&client.read_until_ready()));
    }
    assert_eq!(codes.first().map(String::as_str), Some("53200"));
    assert!(budget.used_bytes() <= budget.limit_bytes());
    client.query("DEALLOCATE ALL");
    client.send(b'P', &parse("small", "SELECT 1"));
    client.send(b'S', b"");
    assert_eq!(tags(&client.read_until_ready()), "1Z");
    assert!(budget.used_bytes() < 64 * 1024);
}

#[test]
fn an_idle_session_is_ended_with_57p05_and_a_busy_one_is_not() {
    let server = TestServer::start_limited(
        8,
        Limits {
            idle_timeout: Some(IDLE_TIMEOUT),
            memory_budget: Arc::default(),
        },
    );
    let (mut busy, _) = Client::login(server.address);
    let (mut idle, _) = Client::login(server.address);
    let busy_until = Instant::now() + 2 * IDLE_TIMEOUT;
    while Instant::now() < busy_until {
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(tags(&busy.query("SELECT 1")), "TDCZ");
    }
    let ended = idle.read().expect("a FATAL before the connection closes");
    assert_eq!(ended.code(), "57P05");
    assert_eq!(ended.field(b'S').as_deref(), Some("FATAL"));
    assert!(idle.read().is_none());
    assert!(
        server
            .logged
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("idle-session timeout"))
    );
}

#[test]
fn logins_in_progress_and_refused_ones_hold_no_connection_slot() {
    let server = TestServer::start(2);
    let idle: Vec<Client> = (0..eggdb_pgwire::MAX_UNAUTHENTICATED_CONNECTIONS)
        .map(|_| Client::connect(server.address))
        .collect();
    std::thread::sleep(Duration::from_millis(100));
    let mut refused = Client::connect(server.address);
    let started = Instant::now();
    assert!(refused.read().is_none());
    assert!(started.elapsed() < Duration::from_secs(1));
    std::thread::sleep(Duration::from_millis(1100));
    let (mut first, _) = Client::login(server.address);
    let (mut second, _) = Client::login(server.address);
    assert_eq!(tags(&first.query("SELECT 1")), "TDCZ");
    assert_eq!(tags(&second.query("SELECT 2")), "TDCZ");
    let mut third = Client::connect(server.address);
    third.send_startup(
        196_608,
        &[("user", common::USER), ("database", common::DATABASE)],
    );
    assert_eq!(third.read().unwrap().code(), "53300");
    drop(idle);
}

#[test]
fn a_large_message_gives_its_budget_back_once_it_is_handled() {
    let budget = Arc::new(MemoryBudget::new(4 * 1024 * 1024));
    let server = TestServer::start_limited(
        8,
        Limits {
            idle_timeout: None,
            memory_budget: Arc::clone(&budget),
        },
    );
    let (mut client, _) = Client::login(server.address);
    let text = format!("SELECT length('{}')", "c".repeat(200 * 1024));
    assert_eq!(tags(&client.query(&text)), "TDCZ");
    assert!(
        wait_until(|| budget.used_bytes() < 64 * 1024),
        "{} bytes still held",
        budget.used_bytes()
    );
}
