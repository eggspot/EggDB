use crate::error::{SqlError, SqlResult, SqlState};
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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

thread_local! {
    static ACTIVE: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
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

pub(crate) fn check() -> SqlResult<()> {
    let cancelled = ACTIVE.with(|active| {
        active
            .borrow()
            .as_ref()
            .is_some_and(|raised| raised.load(Ordering::Relaxed))
    });
    if cancelled {
        return Err(SqlError::new(
            SqlState::QUERY_CANCELED,
            "canceling statement due to user request",
        ));
    }
    Ok(())
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
        std::thread::sleep(slice);
        check()?;
    }
}

#[cfg(test)]
mod tests {
    use super::{CancelSignal, check, enter, sleep};
    use crate::error::SqlState;
    use std::time::{Duration, Instant};

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
            std::thread::sleep(Duration::from_millis(50));
            raised.cancel();
        });
        let started = Instant::now();
        let error = sleep(60.0).unwrap_err();
        canceller.join().unwrap();
        assert_eq!(error.state(), SqlState::QUERY_CANCELED);
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
