//! Counters of what every session in this process has done, for a server's metrics.

use std::sync::atomic::{AtomicU64, Ordering};

static SESSIONS_OPEN: AtomicU64 = AtomicU64::new(0);
static STATEMENTS: AtomicU64 = AtomicU64::new(0);
static STATEMENT_ERRORS: AtomicU64 = AtomicU64::new(0);
static COMMITS: AtomicU64 = AtomicU64::new(0);
static ROLLBACKS: AtomicU64 = AtomicU64::new(0);

/// The process's session counters at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Activity {
    /// Sessions open now.
    pub sessions_open: u64,
    /// Statements run since the process started.
    pub statements: u64,
    /// Statements that failed.
    pub statement_errors: u64,
    /// Transactions committed, explicit and implicit.
    pub commits: u64,
    /// Transactions rolled back.
    pub rollbacks: u64,
}

/// The counters now.
pub fn activity() -> Activity {
    Activity {
        sessions_open: SESSIONS_OPEN.load(Ordering::Relaxed),
        statements: STATEMENTS.load(Ordering::Relaxed),
        statement_errors: STATEMENT_ERRORS.load(Ordering::Relaxed),
        commits: COMMITS.load(Ordering::Relaxed),
        rollbacks: ROLLBACKS.load(Ordering::Relaxed),
    }
}

#[derive(Debug)]
pub(crate) struct OpenSession(());

impl OpenSession {
    pub(crate) fn new() -> Self {
        SESSIONS_OPEN.fetch_add(1, Ordering::Relaxed);
        Self(())
    }
}

impl Drop for OpenSession {
    fn drop(&mut self) {
        SESSIONS_OPEN.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) fn statement_finished(succeeded: bool) {
    STATEMENTS.fetch_add(1, Ordering::Relaxed);
    if !succeeded {
        STATEMENT_ERRORS.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn committed() {
    COMMITS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn rolled_back() {
    ROLLBACKS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_only_grow_and_open_sessions_follow_their_guards() {
        let before = activity();
        let guard = OpenSession::new();
        statement_finished(true);
        statement_finished(false);
        committed();
        rolled_back();
        let after = activity();
        assert!(after.statements >= before.statements + 2);
        assert!(after.statement_errors > before.statement_errors);
        assert!(after.commits > before.commits && after.rollbacks > before.rollbacks);
        drop(guard);
    }
}
