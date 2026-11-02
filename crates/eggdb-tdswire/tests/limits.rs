//! What keeps one client, or another program on the same machine, from locking users out of
//! the TDS server: login slots that idle sockets lose to real logins, the idle timeout and the
//! server-wide memory budget.

mod common;

use common::{Client, Limits, TestServer, batch_body, errors, rows};
use eggdb_platform::connection_limits::MemoryBudget;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
fn idle_sockets_cannot_lock_a_real_login_out() {
    let server = TestServer::start(64);
    let idle: Vec<Client> = (0..eggdb_tdswire::MAX_UNAUTHENTICATED_CONNECTIONS + 4)
        .map(|_| Client::connect(server.address))
        .collect();
    std::thread::sleep(Duration::from_millis(1300));
    let started = Instant::now();
    let mut client = Client::login(server.address);
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(
        rows(&client.batch("SELECT 1")),
        [vec![Some("1".to_owned())]]
    );
    drop(idle);
}

#[test]
fn an_idle_connection_is_closed_after_the_idle_timeout() {
    let server = TestServer::start_limited(
        8,
        Limits {
            idle_timeout: Some(Duration::from_millis(400)),
            memory_budget: Arc::default(),
        },
    );
    let mut busy = Client::login(server.address);
    let mut idle = Client::login(server.address);
    for _ in 0..8 {
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(rows(&busy.batch("SELECT 1")), [vec![Some("1".to_owned())]]);
    }
    assert!(idle.read_message().is_none());
    assert!(
        server
            .log()
            .iter()
            .any(|line| line.contains("closed after the idle timeout"))
    );
}

#[test]
fn a_message_past_the_memory_budget_closes_its_connection_with_error_701() {
    let budget = Arc::new(MemoryBudget::new(128 * 1024));
    let server = TestServer::start_limited(
        8,
        Limits {
            idle_timeout: None,
            memory_budget: Arc::clone(&budget),
        },
    );
    let mut client = Client::login(server.address);
    let text = format!("SELECT '{}'", "a".repeat(80 * 1024));
    client.send(0x01, &batch_body(&text));
    let (_, body) = client
        .read_message()
        .expect("an error before the connection closes");
    let refused = errors(&common::parse_tokens(&body));
    assert_eq!(
        refused.first().map(|error| error.0),
        Some(701),
        "{refused:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while budget.used_bytes() != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(budget.used_bytes(), 0);
    let mut next = Client::login(server.address);
    assert_eq!(rows(&next.batch("SELECT 1")), [vec![Some("1".to_owned())]]);
}
