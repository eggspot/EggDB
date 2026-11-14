//! A process-wide signal that a write transaction committed (D1183): a generation counter that
//! every commit moves and a condition variable that wakes whoever waits for the next one, so a
//! live query re-runs as soon as the data may have changed instead of at its next poll.

use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

static GENERATION: Mutex<u64> = Mutex::new(0);
static COMMITTED: Condvar = Condvar::new();

pub(crate) fn notify_commit() {
    let mut generation = GENERATION.lock().unwrap_or_else(PoisonError::into_inner);
    *generation = generation.wrapping_add(1);
    drop(generation);
    COMMITTED.notify_all();
}

/// The commit generation now: it moves each time a write transaction of this process commits.
pub fn commit_generation() -> u64 {
    *GENERATION.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Waits until a commit moves the generation past `seen`, or `timeout` passes, and returns the
/// generation then.
pub fn wait_for_commit(seen: u64, timeout: Duration) -> u64 {
    let generation = GENERATION.lock().unwrap_or_else(PoisonError::into_inner);
    let (generation, _) = COMMITTED
        .wait_timeout_while(generation, timeout, |generation| *generation == seen)
        .unwrap_or_else(PoisonError::into_inner);
    *generation
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commit_wakes_a_waiter_before_its_timeout() {
        let seen = commit_generation();
        let waiter = std::thread::spawn(move || wait_for_commit(seen, Duration::from_secs(30)));
        std::thread::sleep(Duration::from_millis(20));
        notify_commit();
        assert_ne!(waiter.join().unwrap(), seen);
        assert_eq!(wait_for_commit(commit_generation(), Duration::from_millis(1)), commit_generation());
    }
}
