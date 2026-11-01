//! What keeps one client, or another program on the same machine, from locking users out of
//! the MySQL server: login slots that idle sockets lose to real logins, the idle timeout and
//! the server-wide memory budget.

mod common;

use common::{Client, Limits, Reply, TestServer, texts};
use eggdb_platform::connection_limits::MemoryBudget;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
fn idle_sockets_cannot_lock_a_real_login_out() {
    let server = TestServer::start(64);
    let idle: Vec<Client> = (0..eggdb_mysqlwire::MAX_UNAUTHENTICATED_CONNECTIONS + 4)
        .map(|_| Client::connect(server.address))
        .collect();
    std::thread::sleep(Duration::from_millis(1300));
    let started = Instant::now();
    let mut client = Client::login(server.address);
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(texts(&client.query("SELECT 1")), [vec!["1".to_owned()]]);
    drop(idle);
}

#[test]
fn an_idle_connection_is_closed_with_error_4031() {
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
        assert_eq!(texts(&busy.query("SELECT 1")), [vec!["1".to_owned()]]);
    }
    let body = idle.read().expect("an error before the connection closes");
    let reply = idle.parse_reply(body);
    assert!(matches!(reply, Reply::Err { code: 4031, .. }), "{reply:?}");
    assert!(idle.read().is_none());
}

#[test]
fn a_packet_past_the_memory_budget_closes_its_connection_with_error_1041() {
    let budget = Arc::new(MemoryBudget::new(128 * 1024));
    let server = TestServer::start_limited(
        8,
        Limits {
            idle_timeout: None,
            memory_budget: Arc::clone(&budget),
        },
    );
    let mut client = Client::login(server.address);
    let text = format!("SELECT '{}'", "a".repeat(150 * 1024));
    let reply = client.query(&text);
    assert!(matches!(reply, Reply::Err { code: 1041, .. }), "{reply:?}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while budget.used_bytes() != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(budget.used_bytes(), 0);
    let mut next = Client::login(server.address);
    assert_eq!(texts(&next.query("SELECT 1")), [vec!["1".to_owned()]]);
}
