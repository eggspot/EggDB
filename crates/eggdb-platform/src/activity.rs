//! How busy the server is, for a server that stops when idle (D1102): the connections open at
//! this moment, across every protocol that reports them, and when the last one opened or closed.

use crate::time::Instant;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

static OPEN: AtomicUsize = AtomicUsize::new(0);
static CHANGED: Mutex<Option<Instant>> = Mutex::new(None);

fn mark() {
    let mut changed = CHANGED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    *changed = Some(Instant::now());
}

/// A connection counted as open until it is dropped.
#[derive(Debug)]
pub struct OpenConnection(());

impl Drop for OpenConnection {
    fn drop(&mut self) {
        OPEN.fetch_sub(1, Ordering::AcqRel);
        mark();
    }
}

/// Counts one more open connection, until the guard is dropped.
pub fn connection_opened() -> OpenConnection {
    OPEN.fetch_add(1, Ordering::AcqRel);
    mark();
    OpenConnection(())
}

/// How many connections are open now.
pub fn open_connections() -> usize {
    OPEN.load(Ordering::Acquire)
}

/// How long no connection has been open, or `None` while one is. A process where none ever
/// opened counts from its first call here.
pub fn idle_for() -> Option<Duration> {
    if open_connections() > 0 {
        return None;
    }
    let mut changed = CHANGED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let since = *changed.get_or_insert_with(Instant::now);
    Some(since.elapsed())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connection_keeps_the_server_busy_until_it_closes() {
        let opened = connection_opened();
        assert!(open_connections() >= 1);
        drop(opened);
        let _ = idle_for();
    }
}
