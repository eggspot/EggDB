use crate::error::{Error, ErrorKind, Result};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, ThreadId};
use std::time::Duration;

#[derive(Debug, Default)]
pub(crate) struct WriterSlot {
    holder: Mutex<Option<ThreadId>>,
    released: Condvar,
}

impl WriterSlot {
    pub(crate) fn take(&self, timeout: Duration) -> Result<WriterTurn<'_>> {
        let me = thread::current().id();
        let holder = self.lock();
        if *holder == Some(me) {
            return Err(Error::new(
                ErrorKind::WriterActive,
                "this thread already holds the write transaction; commit or roll it back before beginning another",
            ));
        }
        let (mut holder, _) = self
            .released
            .wait_timeout_while(holder, timeout, |holder| holder.is_some())
            .unwrap_or_else(PoisonError::into_inner);
        if holder.is_some() {
            return Err(Error::new(
                ErrorKind::WriterActive,
                format!(
                    "another write transaction is still open after waiting {timeout:?}; one writer at a time: commit or roll it back first, or wait longer with `Options::write_timeout`"
                ),
            ));
        }
        *holder = Some(me);
        Ok(WriterTurn { slot: self })
    }

    fn lock(&self) -> MutexGuard<'_, Option<ThreadId>> {
        self.holder.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug)]
pub(crate) struct WriterTurn<'s> {
    slot: &'s WriterSlot,
}

impl Drop for WriterTurn<'_> {
    fn drop(&mut self) {
        *self.slot.lock() = None;
        self.slot.released.notify_one();
    }
}
