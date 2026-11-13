use crate::error::{SqlError, SqlResult, SqlState};
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use eggdb_platform_core::time::Instant;
use std::time::Duration;

const SLEEP_SLICE: Duration = Duration::from_millis(10);

/// The signal that stops the statement a [`crate::Session`] is running, raised from any thread,
/// as PostgreSQL's `CancelRequest` does: [`crate::Session::cancel_signal`] hands it out, and
/// every clone shares it.
///
/// A raised signal stops the running statement at its next row, or within ten milliseconds in
/// `pg_sleep`, with SQLSTATE `57014`, and every statement the session starts after it, until
/// [`CancelSignal::clear`] lowers it.
#[derive(Debug, Clone, Default)]
pub struct CancelSignal(Arc<AtomicBool>);

impl CancelSignal {
    /// Raises the signal: the session's running statement stops with SQLSTATE `57014`.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Lowers the signal, so the session's next statement runs.
    pub fn clear(&self) {
        self.0.store(false, Ordering::Release);
    }

    /// Whether the signal is raised.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

const CLOCK_EVERY: u32 = 256;

thread_local! {
    static ACTIVE: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
    static CHECKS: Cell<u32> = const { Cell::new(0) };
}

pub(crate) struct EnteredDeadline {
    previous: Option<Instant>,
}

impl Drop for EnteredDeadline {
    fn drop(&mut self) {
        DEADLINE.with(|deadline| deadline.set(self.previous));
    }
}

pub(crate) fn enter_deadline(
    timeout: Option<Duration>,
    session_deadline: Option<Instant>,
) -> EnteredDeadline {
    let timed = timeout.and_then(|timeout| Instant::now().checked_add(timeout));
    let deadline = match (timed, session_deadline) {
        (Some(timed), Some(session)) => Some(timed.min(session)),
        (timed, session) => timed.or(session),
    };
    CHECKS.with(|checks| checks.set(0));
    EnteredDeadline {
        previous: DEADLINE.with(|current| current.replace(deadline)),
    }
}

fn past_deadline(every_call: bool) -> bool {
    let Some(deadline) = DEADLINE.with(Cell::get) else {
        return false;
    };
    let due = CHECKS.with(|checks| {
        let count = checks.get().wrapping_add(1);
        checks.set(count);
        every_call || count.is_multiple_of(CLOCK_EVERY)
    });
    due && Instant::now() >= deadline
}

fn timed_out() -> SqlError {
    SqlError::new(
        SqlState::QUERY_CANCELED,
        "canceling statement due to statement timeout",
    )
}

pub(crate) struct EnteredSignal {
    previous: Option<Arc<AtomicBool>>,
}

impl Drop for EnteredSignal {
    fn drop(&mut self) {
        let previous = self.previous.take();
        ACTIVE.with(|active| *active.borrow_mut() = previous);
    }
}

pub(crate) fn enter(signal: &CancelSignal) -> EnteredSignal {
    let previous = ACTIVE.with(|active| active.borrow_mut().replace(Arc::clone(&signal.0)));
    EnteredSignal { previous }
}

fn cancelled_by_user() -> SqlError {
    SqlError::new(
        SqlState::QUERY_CANCELED,
        "canceling statement due to user request",
    )
}

pub(crate) fn check() -> SqlResult<()> {
    let cancelled_now = ACTIVE.with(|active| {
        active
            .borrow()
            .as_ref()
            .is_some_and(|raised| raised.load(Ordering::Relaxed))
    });
    if cancelled_now {
        return Err(cancelled_by_user());
    }
    if past_deadline(false) {
        return Err(timed_out());
    }
    Ok(())
}

pub(crate) struct Watch {
    signal: Option<Arc<AtomicBool>>,
    deadline: Option<Instant>,
    checks: Cell<u32>,
}

pub(crate) fn watch() -> Watch {
    Watch {
        signal: ACTIVE.with(|active| active.borrow().clone()),
        deadline: DEADLINE.with(Cell::get),
        checks: Cell::new(0),
    }
}

impl Watch {
    pub(crate) fn check(&self) -> SqlResult<()> {
        if self
            .signal
            .as_ref()
            .is_some_and(|raised| raised.load(Ordering::Relaxed))
        {
            return Err(cancelled_by_user());
        }
        let Some(deadline) = self.deadline else {
            return Ok(());
        };
        let count = self.checks.get().wrapping_add(1);
        self.checks.set(count);
        if count.is_multiple_of(CLOCK_EVERY) && Instant::now() >= deadline {
            return Err(timed_out());
        }
        Ok(())
    }
}

pub(crate) fn sleep(seconds: f64) -> SqlResult<()> {
    check()?;
    if seconds.is_nan() || seconds <= 0.0 {
        return Ok(());
    }
    let deadline = Duration::try_from_secs_f64(seconds)
        .ok()
        .and_then(|length| Instant::now().checked_add(length));
    loop {
        let slice = match deadline {
            Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                Some(remaining) if !remaining.is_zero() => remaining.min(SLEEP_SLICE),
                _ => return Ok(()),
            },
            None => SLEEP_SLICE,
        };
        eggdb_platform_core::time::sleep(slice);
        check()?;
        if past_deadline(true) {
            return Err(timed_out());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CLOCK_EVERY, CancelSignal, check, enter, enter_deadline, sleep, watch};
    use crate::error::SqlState;
    use eggdb_platform_core::time::Instant;
    use std::time::Duration;

    #[test]
    fn a_raised_signal_stops_only_the_statements_that_entered_it() {
        let signal = CancelSignal::default();
        assert!(check().is_ok());
        let entered = enter(&signal);
        assert!(check().is_ok());
        signal.cancel();
        assert!(signal.is_cancelled());
        assert_eq!(check().unwrap_err().state(), SqlState::QUERY_CANCELED);
        drop(entered);
        assert!(check().is_ok());
        signal.clear();
        let _entered = enter(&signal);
        assert!(check().is_ok());
    }

    #[test]
    fn a_watch_taken_inside_a_statement_stops_at_its_signal_and_its_deadline() {
        let signal = CancelSignal::default();
        let unwatched = watch();
        let entered = enter(&signal);
        let watched = watch();
        assert!(watched.check().is_ok());
        signal.cancel();
        assert_eq!(
            watched.check().unwrap_err().state(),
            SqlState::QUERY_CANCELED
        );
        assert!(unwatched.check().is_ok());
        drop(entered);
        assert_eq!(
            watched.check().unwrap_err().state(),
            SqlState::QUERY_CANCELED
        );
        signal.clear();
        assert!(watched.check().is_ok());
        let _deadline = enter_deadline(Some(Duration::ZERO), None);
        let timed = watch();
        let mut stopped_at = None;
        for row in 1..=CLOCK_EVERY {
            if timed.check().is_err() {
                stopped_at = Some(row);
                break;
            }
        }
        assert_eq!(stopped_at, Some(CLOCK_EVERY));
        let _none = enter_deadline(None, None);
        let free = watch();
        for _ in 0..=CLOCK_EVERY {
            assert!(free.check().is_ok());
        }
    }

    #[test]
    fn the_earlier_of_the_timeout_and_the_session_deadline_stops_the_statement() {
        let past = Instant::now();
        {
            let _deadline = enter_deadline(Some(Duration::from_secs(3600)), Some(past));
            assert_eq!(sleep(60.0).unwrap_err().state(), SqlState::QUERY_CANCELED);
        }
        {
            let _deadline = enter_deadline(Some(Duration::ZERO), None);
            assert_eq!(sleep(60.0).unwrap_err().state(), SqlState::QUERY_CANCELED);
        }
        let _none = enter_deadline(None, None);
        sleep(0.01).unwrap();
    }

    #[test]
    fn sleeping_ends_at_its_time_or_at_a_cancel() {
        let signal = CancelSignal::default();
        let _entered = enter(&signal);
        let started = Instant::now();
        sleep(0.05).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(50));
        sleep(-1.0).unwrap();
        sleep(f64::NAN).unwrap();
        let raised = signal.clone();
        let canceller = std::thread::spawn(move || {
            eggdb_platform_core::time::sleep(Duration::from_millis(50));
            raised.cancel();
        });
        let started = Instant::now();
        let error = sleep(60.0).unwrap_err();
        canceller.join().unwrap();
        assert_eq!(error.state(), SqlState::QUERY_CANCELED);
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
