use crate::cpus::{CpuSet, Cpus};
use crate::error::{Error, ErrorKind, Result};
use std::any::Any;
use std::sync::mpsc;
use std::thread;

/// How to start a thread: its name, its stack size and the CPUs it is pinned to.
///
/// [`ThreadOptions::spawn`] pins the new thread before it runs any of the caller's code, and
/// reports a pin that fails as a spawn error, so a shard never runs on the wrong CPUs without
/// knowing it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadOptions {
    name: Option<String>,
    stack_size: Option<usize>,
    cpus: Option<CpuSet>,
}

impl ThreadOptions {
    /// Options for an unnamed, unpinned thread with the platform's default stack size.
    pub fn new() -> Self {
        Self::default()
    }

    /// Names the thread, as debuggers, profilers and panic messages show it.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the thread's stack size in bytes; the platform may round it up.
    pub fn stack_size(mut self, bytes: usize) -> Self {
        self.stack_size = Some(bytes);
        self
    }

    /// Pins the thread to `cpus` through [`Cpus::pin_current_thread`] before it runs.
    pub fn pin(mut self, cpus: CpuSet) -> Self {
        self.cpus = Some(cpus);
        self
    }

    /// Starts a thread that runs `work`. Fails with [`ErrorKind::InvalidInput`] when the name
    /// contains a NUL byte, with the operating system's error when the thread cannot be
    /// created, and with the error of [`Cpus::pin_current_thread`] when the pin is refused, in
    /// which case `work` never runs and the thread has already ended.
    pub fn spawn<C, F, T>(self, cpus: &C, work: F) -> Result<SpawnedThread<T>>
    where
        C: Cpus + Clone + 'static,
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let mut builder = thread::Builder::new();
        if let Some(name) = self.name {
            if name.contains('\0') {
                return Err(Error::new(ErrorKind::InvalidInput));
            }
            builder = builder.name(name);
        }
        if let Some(bytes) = self.stack_size {
            builder = builder.stack_size(bytes);
        }
        let (pinned_sender, pinned) = mpsc::sync_channel(1);
        let pin = self.cpus;
        let cpus = cpus.clone();
        let handle = builder.spawn(move || {
            let outcome = pin.map_or(Ok(()), |set| cpus.pin_current_thread(&set));
            let run = outcome.is_ok();
            let _ = pinned_sender.send(outcome);
            run.then(work)
        })?;
        match pinned.recv() {
            Ok(Ok(())) => Ok(SpawnedThread { handle }),
            Ok(Err(error)) => {
                let _ = handle.join();
                Err(error)
            }
            Err(_) => Err(Error::new(ErrorKind::Other)),
        }
    }
}

/// A thread started by [`ThreadOptions::spawn`].
#[derive(Debug)]
pub struct SpawnedThread<T> {
    handle: thread::JoinHandle<Option<T>>,
}

impl<T> SpawnedThread<T> {
    /// The thread's handle, for its name and id.
    pub fn thread(&self) -> &thread::Thread {
        self.handle.thread()
    }

    /// Whether the thread's work has returned or panicked.
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// Waits for the work to return and gives back its value, or the panic's payload if it
    /// panicked.
    pub fn join(self) -> thread::Result<T> {
        self.handle.join().and_then(|value| {
            value.ok_or_else(|| Box::new("the thread did not run its work") as Box<dyn Any + Send>)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::ThreadOptions;
    use crate::{CpuSet, Cpus, ErrorKind, OsCpus, SimCpus};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn set(list: &str) -> CpuSet {
        CpuSet::parse_list(list).unwrap()
    }

    #[test]
    fn threads_get_their_name_and_return_their_value() {
        let thread = ThreadOptions::new()
            .name("shard-3")
            .stack_size(256 * 1024)
            .spawn(&OsCpus::new(), || {
                std::thread::current().name().map(str::to_owned)
            })
            .unwrap();
        assert_eq!(thread.thread().name(), Some("shard-3"));
        assert_eq!(thread.join().unwrap().as_deref(), Some("shard-3"));
    }

    #[test]
    fn names_with_nul_bytes_are_invalid_input() {
        let error = ThreadOptions::new()
            .name("shard\0")
            .spawn(&SimCpus::new(), || ())
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn threads_are_pinned_before_their_work_runs() {
        let cpus = SimCpus::new();
        let observer = cpus.clone();
        let thread = ThreadOptions::new()
            .pin(set("2"))
            .spawn(&cpus, move || observer.pinned(std::thread::current().id()))
            .unwrap();
        assert_eq!(thread.join().unwrap(), Some(set("2")));
    }

    #[test]
    fn a_refused_pin_is_a_spawn_error_and_the_work_never_runs() {
        let cpus = SimCpus::new();
        cpus.refuse_pinning(true);
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        let error = ThreadOptions::new()
            .pin(set("0"))
            .spawn(&cpus, move || flag.store(true, Ordering::Relaxed))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert!(!ran.load(Ordering::Relaxed));
    }

    #[test]
    fn invalid_pins_are_spawn_errors() {
        let cpus = SimCpus::new();
        for pin in [CpuSet::new(), set("9")] {
            let error = ThreadOptions::new()
                .pin(pin)
                .spawn(&cpus, || ())
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn a_panic_in_the_work_reaches_join() {
        let thread = ThreadOptions::new()
            .spawn(&SimCpus::new(), || panic!("shard failed"))
            .unwrap();
        let payload = thread.join().unwrap_err();
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"shard failed"));
    }

    #[test]
    fn os_threads_are_pinned_or_refused() {
        let cpus = OsCpus::new();
        let one: CpuSet = cpus.available().iter().take(1).collect();
        match ThreadOptions::new().pin(one).spawn(&cpus, || 7) {
            Ok(thread) => assert_eq!(thread.join().unwrap(), 7),
            Err(error) => assert_eq!(error.kind(), ErrorKind::Unsupported),
        }
    }
}
