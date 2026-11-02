//! What keeps one client, or another program on the same machine, from locking users out of
//! the MySQL server: login slots that idle sockets lose to real logins, the idle timeout and
//! the server-wide memory budget.

mod common;

use common::{Client, Limits, Reply, TestServer, texts};
use eggdb_platform::connection_limits::MemoryBudget;
use std::sync::Arc;
use std::time::{Duration, Instant};

const IDLE_TIMEOUT: Duration = Duration::from_millis(1500);

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
            idle_timeout: Some(IDLE_TIMEOUT),
            memory_budget: Arc::default(),
        },
    );
    let mut busy = Client::login(server.address);
    let mut idle = Client::login(server.address);
    let busy_until = Instant::now() + 2 * IDLE_TIMEOUT;
    while Instant::now() < busy_until {
        std::thread::sleep(Duration::from_millis(50));
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

#[test]
fn logins_in_progress_hold_no_connection_slot_and_a_large_packet_gives_its_budget_back() {
    let budget = Arc::new(MemoryBudget::new(4 * 1024 * 1024));
    let server = TestServer::start_limited(
        2,
        Limits {
            idle_timeout: None,
            memory_budget: Arc::clone(&budget),
        },
    );
    let idle: Vec<Client> = (0..eggdb_mysqlwire::MAX_UNAUTHENTICATED_CONNECTIONS)
        .map(|_| Client::connect(server.address))
        .collect();
    std::thread::sleep(Duration::from_millis(1300));
    let mut first = Client::login(server.address);
    let mut second = Client::login(server.address);
    assert_eq!(texts(&second.query("SELECT 2")), [vec!["2".to_owned()]]);
    let text = format!("SELECT length('{}')", "c".repeat(200 * 1024));
    assert_eq!(texts(&first.query(&text)), [vec![(200 * 1024).to_string()]]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while budget.used_bytes() >= 64 * 1024 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(budget.used_bytes() < 64 * 1024, "{}", budget.used_bytes());
    drop(idle);
}
