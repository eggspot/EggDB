//! Workload queues (D1050): named admission queues shared by every session of the process,
//! each letting at most its concurrency of statements run at once. A session joins one with
//! `SET eggdb.workload_queue`; its statements wait their turn in arrival order and give up with
//! SQLSTATE `57014` after the queue's timeout, so heavy analytics cannot crowd out the rest of
//! the workload.

use crate::error::{SqlError, SqlResult, SqlState};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use eggdb_platform_core::time::Instant;
use std::time::Duration;

/// How a queue admits statements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QueueSpec {
    pub(crate) concurrency: u32,
    pub(crate) timeout_ms: u64,
}

/// What a queue is doing, for `SHOW WORKLOAD QUEUES`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueueStatus {
    pub(crate) name: String,
    pub(crate) spec: QueueSpec,
    pub(crate) running: u32,
    pub(crate) waiting: usize,
    pub(crate) admitted: u64,
    pub(crate) timed_out: u64,
}

#[derive(Debug)]
struct QueueState {
    spec: QueueSpec,
    running: u32,
    waiting: Vec<u64>,
    admitted: u64,
    timed_out: u64,
}

#[derive(Debug, Default)]
struct Registry {
    queues: BTreeMap<String, QueueState>,
    next_ticket: u64,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    queues: BTreeMap::new(),
    next_ticket: 0,
});
static FREED: Condvar = Condvar::new();

thread_local! {
    static HOLDING: Cell<bool> = const { Cell::new(false) };
}

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Defines or redefines the queue `name`; statements already admitted keep running.
pub(crate) fn define(name: &str, spec: QueueSpec) {
    let mut registry = registry();
    match registry.queues.get_mut(name) {
        Some(queue) => queue.spec = spec,
        None => {
            registry.queues.insert(
                name.to_owned(),
                QueueState { spec, running: 0, waiting: Vec::new(), admitted: 0, timed_out: 0 },
            );
        }
    }
    FREED.notify_all();
}

/// Forgets the queue `name`; returns whether it existed.
pub(crate) fn remove(name: &str) -> bool {
    let removed = registry().queues.remove(name).is_some();
    FREED.notify_all();
    removed
}

/// Whether the queue `name` is defined in this process.
pub(crate) fn is_defined(name: &str) -> bool {
    registry().queues.contains_key(name)
}

/// Every queue with what it is doing.
pub(crate) fn statuses() -> Vec<QueueStatus> {
    registry()
        .queues
        .iter()
        .map(|(name, queue)| QueueStatus {
            name: name.clone(),
            spec: queue.spec,
            running: queue.running,
            waiting: queue.waiting.len(),
            admitted: queue.admitted,
            timed_out: queue.timed_out,
        })
        .collect()
}

/// A statement's place in a queue; dropping it lets the next one in.
#[derive(Debug)]
pub(crate) struct Slot {
    queue: String,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut registry = registry();
        if let Some(queue) = registry.queues.get_mut(&self.queue) {
            queue.running = queue.running.saturating_sub(1);
        }
        HOLDING.with(|holding| holding.set(false));
        FREED.notify_all();
    }
}

/// Waits for a place in the queue `name` for one statement. A statement run from inside an
/// admitted one, on the same thread, is not queued again. `None` when it runs without a slot.
pub(crate) fn admit(name: &str) -> SqlResult<Option<Slot>> {
    if HOLDING.with(Cell::get) {
        return Ok(None);
    }
    let mut registry = registry();
    let Some(queue) = registry.queues.get(name) else {
        return Err(SqlError::new(
            SqlState::UNDEFINED_OBJECT,
            format!("workload queue \"{name}\" does not exist"),
        ));
    };
    let timeout = Duration::from_millis(queue.spec.timeout_ms);
    registry.next_ticket += 1;
    let ticket = registry.next_ticket;
    if let Some(queue) = registry.queues.get_mut(name) {
        queue.waiting.push(ticket);
    }
    let started = Instant::now();
    loop {
        let Some(queue) = registry.queues.get_mut(name) else {
            return Err(SqlError::new(
                SqlState::UNDEFINED_OBJECT,
                format!("workload queue \"{name}\" was dropped while a statement waited in it"),
            ));
        };
        if queue.running < queue.spec.concurrency.max(1) && queue.waiting.first() == Some(&ticket) {
            queue.waiting.remove(0);
            queue.running += 1;
            queue.admitted += 1;
            HOLDING.with(|holding| holding.set(true));
            return Ok(Some(Slot { queue: name.to_owned() }));
        }
        let waited = started.elapsed();
        if waited >= timeout {
            queue.waiting.retain(|waiting| *waiting != ticket);
            queue.timed_out += 1;
            FREED.notify_all();
            return Err(SqlError::new(
                SqlState::QUERY_CANCELED,
                format!(
                    "canceling statement: it waited {} ms in workload queue \"{name}\", whose {} places were taken",
                    waited.as_millis(),
                    queue.spec.concurrency
                ),
            ));
        }
        registry = FREED
            .wait_timeout(registry, timeout - waited)
            .map(|(guard, _)| guard)
            .unwrap_or_else(|poisoned| poisoned.into_inner().0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn a_queue_runs_at_most_its_concurrency_and_times_out_the_rest() {
        define("tests_reporting", QueueSpec { concurrency: 2, timeout_ms: 2_000 });
        let running = Arc::new(AtomicU32::new(0));
        let most = Arc::new(AtomicU32::new(0));
        let threads: Vec<_> = (0..6)
            .map(|_| {
                let (running, most) = (Arc::clone(&running), Arc::clone(&most));
                std::thread::spawn(move || {
                    let slot = admit("tests_reporting").unwrap();
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    running.fetch_sub(1, Ordering::SeqCst);
                    drop(slot);
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(most.load(Ordering::SeqCst) <= 2);
        let status = statuses().into_iter().find(|status| status.name == "tests_reporting").unwrap();
        assert_eq!(status.admitted, 6);
        assert_eq!(status.running, 0);
        define("tests_tight", QueueSpec { concurrency: 1, timeout_ms: 30 });
        let holder = std::thread::spawn(|| {
            let slot = admit("tests_tight").unwrap();
            std::thread::sleep(Duration::from_millis(200));
            drop(slot);
        });
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(admit("tests_tight").unwrap_err().state(), SqlState::QUERY_CANCELED);
        holder.join().unwrap();
        let nested = admit("tests_tight").unwrap();
        assert!(admit("tests_tight").unwrap().is_none());
        drop(nested);
        assert!(remove("tests_tight"));
        assert_eq!(admit("tests_tight").unwrap_err().state(), SqlState::UNDEFINED_OBJECT);
    }
}
