use crate::error::{Error, ErrorKind, Result};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, ThreadId};
use std::time::Duration;

#[derive(Debug, Default)]
pub(crate) struct WriterSlot {
    state: Mutex<SlotState>,
    released: Condvar,
}

#[derive(Debug, Default)]
struct SlotState {
    holder: Option<ThreadId>,
    waiting: usize,
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
        if state.holder.is_some() && !timeout.is_zero() {
            state.waiting += 1;
            state = self
                .released
                .wait_timeout_while(state, timeout, |state| state.holder.is_some())
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            state.waiting -= 1;
        }
        if state.holder.is_some() {
            return Err(Error::new(
                ErrorKind::WriterActive,
                format!(
                    "another write transaction is still open after waiting {timeout:?}; one writer at a time: commit or roll it back first, or wait longer with `Options::write_timeout`"
                ),
            ));
        }
        state.holder = Some(me);
        Ok(WriterTurn { slot: self })
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
        state.holder = None;
        if state.waiting > 0 {
            self.slot.released.notify_all();
        }
    }
}
