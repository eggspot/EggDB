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
