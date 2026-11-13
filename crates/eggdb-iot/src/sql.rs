use eggdb_sql::{Dialect, KeyValueStore, Session};
use std::time::{Duration, Instant};

const STOP_POLL: Duration = Duration::from_millis(100);

pub(crate) fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

pub(crate) fn optional(text: Option<&str>) -> String {
    text.map_or_else(|| "NULL".to_owned(), literal)
}

pub(crate) fn bytea(bytes: &[u8]) -> String {
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("'\\x{hex}'::bytea")
}

pub(crate) fn run(session: &mut Session<'_>, sql: &str) -> Result<(), String> {
    session.execute(sql, &mut |_| {}).map_err(|error| error.message().to_owned())
}

pub(crate) fn session<'s, S: KeyValueStore>(store: &'s S, user: &str, database: &str, protocol: &'static str) -> Session<'s> {
    let mut session = Session::with_dialect(store, Dialect::PostgreSql);
    session.identify(user, database);
    session.set_protocol(protocol);
    session
}

/// Waits `pause`, returning early when `stop` answers true.
pub(crate) fn pause(pause: Duration, stop: &(dyn Fn() -> bool + Sync)) {
    let started = Instant::now();
    while !stop() && started.elapsed() < pause {
        std::thread::sleep(STOP_POLL.min(pause));
    }
}
