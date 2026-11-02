use crate::error::{Error, ErrorKind, Result};
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, ThreadId};
use std::time::Duration;

#[derive(Debug, Default)]
pub(crate) struct WriterSlot {
    state: Mutex<SlotState>,
    handed_over: Condvar,
}

#[derive(Debug, Default)]
struct SlotState {
    holder: Option<ThreadId>,
    waiting_in_arrival_order: VecDeque<ThreadId>,
}

impl WriterSlot {
    pub(crate) fn take(&self, timeout: Duration) -> Result<WriterTurn<'_>> {
        let me = thread::current().id();
        let mut state = self.lock();
        if state.holder == Some(me) {
            return Err(Error::new(
                ErrorKind::WriterActive,
                "the open write transaction was begun on this thread, so waiting for it here would never end; commit or roll it back before beginning another",
            ));
        }
        if state.holder.is_none() {
            state.holder = Some(me);
            return Ok(WriterTurn { slot: self });
        }
        if !timeout.is_zero() {
            state.waiting_in_arrival_order.push_back(me);
            state = self
                .handed_over
                .wait_timeout_while(state, timeout, |state| state.holder != Some(me))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            if state.holder == Some(me) {
                return Ok(WriterTurn { slot: self });
            }
            state
                .waiting_in_arrival_order
                .retain(|waiter| *waiter != me);
        }
        Err(Error::new(
            ErrorKind::WriterActive,
            format!(
                "another write transaction is still open after waiting {timeout:?}; one writer at a time: commit or roll it back first, or wait longer with `Options::write_timeout`"
            ),
        ))
    }

    fn lock(&self) -> MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug)]
pub(crate) struct WriterTurn<'s> {
    slot: &'s WriterSlot,
}

impl Drop for WriterTurn<'_> {
    fn drop(&mut self) {
        let mut state = self.slot.lock();
        state.holder = state.waiting_in_arrival_order.pop_front();
        if state.holder.is_some() {
            self.slot.handed_over.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;

    const LONG_WAIT: Duration = Duration::from_secs(30);

    fn wait_until_waiting(slot: &WriterSlot, count: usize) {
        let deadline = Instant::now() + LONG_WAIT;
        while slot.lock().waiting_in_arrival_order.len() != count {
            assert!(Instant::now() < deadline, "{count} writers never queued up");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn waiting_writers_get_the_slot_in_arrival_order() {
        let slot = WriterSlot::default();
        let served = Mutex::new(Vec::new());
        let first = slot.take(Duration::ZERO).unwrap();
        thread::scope(|scope| {
            for arrival in 0..6 {
                let (slot, served) = (&slot, &served);
                scope.spawn(move || {
                    let _turn = slot.take(LONG_WAIT).unwrap();
                    served.lock().unwrap().push(arrival);
                    thread::sleep(Duration::from_millis(2));
                });
                wait_until_waiting(slot, arrival + 1);
            }
            drop(first);
        });
        assert_eq!(*served.lock().unwrap(), (0..6).collect::<Vec<_>>());
        assert!(slot.lock().holder.is_none());
    }

    #[test]
    fn a_newcomer_never_takes_the_slot_handed_to_a_waiter() {
        let slot = WriterSlot::default();
        let first = slot.take(Duration::ZERO).unwrap();
        let (finish, finished) = mpsc::channel::<()>();
        thread::scope(|scope| {
            let slot_for_waiter = &slot;
            let waiter = scope.spawn(move || {
                let turn = slot_for_waiter.take(LONG_WAIT)?;
                let _ = finished.recv();
                drop(turn);
                Ok::<_, Error>(())
            });
            wait_until_waiting(&slot, 1);
            drop(first);
            let newcomer = slot.take(Duration::ZERO).unwrap_err();
            assert_eq!(newcomer.kind(), ErrorKind::WriterActive);
            finish.send(()).unwrap();
            waiter.join().unwrap().unwrap();
        });
        drop(slot.take(Duration::ZERO).unwrap());
    }

    #[test]
    fn a_timed_out_waiter_leaves_the_queue_without_holding_up_later_ones() {
        let slot = WriterSlot::default();
        let first = slot.take(Duration::ZERO).unwrap();
        thread::scope(|scope| {
            let impatient = scope.spawn(|| slot.take(Duration::from_secs(5)).map(drop));
            wait_until_waiting(&slot, 1);
            let patient = scope.spawn(|| slot.take(LONG_WAIT).map(drop));
            wait_until_waiting(&slot, 2);
            let timed_out = impatient.join().unwrap().unwrap_err();
            assert_eq!(timed_out.kind(), ErrorKind::WriterActive);
            assert!(
                timed_out.to_string().contains("write_timeout"),
                "{timed_out}"
            );
            assert_eq!(slot.lock().waiting_in_arrival_order.len(), 1);
            drop(first);
            patient.join().unwrap().unwrap();
        });
        let state = slot.lock();
        assert!(state.holder.is_none());
        assert!(state.waiting_in_arrival_order.is_empty());
    }

    #[test]
    fn the_holding_thread_is_refused_at_once() {
        let slot = WriterSlot::default();
        let _turn = slot.take(Duration::ZERO).unwrap();
        let error = slot.take(LONG_WAIT).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::WriterActive);
        assert!(error.to_string().contains("this thread"), "{error}");
    }
}
