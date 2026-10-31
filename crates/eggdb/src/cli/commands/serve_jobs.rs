use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CHECK_EVERY: Duration = Duration::from_millis(500);

fn epoch_minute() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs() / 60).unwrap_or(i64::MAX))
}

pub(super) fn run_scheduled_jobs(store: &impl eggdb_sql::KeyValueStore, stop: &(dyn Fn() -> bool + Sync)) {
    let mut last = epoch_minute();
    while !stop() {
        std::thread::sleep(CHECK_EVERY);
        let now = epoch_minute();
        if now > last {
            last = now;
            let _ = eggdb_sql::jobs::run_due_jobs(store, now * 60);
        }
    }
}

const DELIVER_EVERY: Duration = Duration::from_secs(1);
const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(5);
const EVENTS_PER_ROUND: u64 = 100;

pub(super) fn deliver_changes(store: &impl eggdb_sql::KeyValueStore, database: &str, url: &str, stop: &(dyn Fn() -> bool + Sync)) {
    let Ok(target) = eggdb_http::parse_http_url(url) else {
        return;
    };
    while !stop() {
        std::thread::sleep(DELIVER_EVERY);
        let _ = eggdb_sql::cdc::deliver_changes(store, database, EVENTS_PER_ROUND, &mut |event| {
            eggdb_http::post(&target, "application/json", event.as_bytes(), WEBHOOK_TIMEOUT).is_ok_and(|status| (200..300).contains(&status))
        });
    }
}

pub(super) fn deliver_changes_to_queue(
    store: &impl eggdb_sql::KeyValueStore,
    database: &str,
    keyspace: &(dyn eggdb_keyspace::Keyspace + Sync),
    queue: &str,
    stop: &(dyn Fn() -> bool + Sync),
) {
    let queues = eggdb_queues::Queues::new(keyspace);
    while !stop() {
        std::thread::sleep(DELIVER_EVERY);
        if queues.declare(queue).is_err() {
            continue;
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX));
        let _ = eggdb_sql::cdc::deliver_changes(store, database, EVENTS_PER_ROUND, &mut |event| queues.send(queue, event.as_bytes(), 0, now).is_ok());
    }
}
