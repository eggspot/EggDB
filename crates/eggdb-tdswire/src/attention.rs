use eggdb_platform::local_socket::{LocalStream, bytes_waiting};
use eggdb_sql::CancelSignal;
use std::io::ErrorKind;
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

const WATCHER_STACK_BYTES: usize = 64 * 1024;
const LOCAL_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(crate) enum Watched {
    Tcp(TcpStream),
    Local(LocalStream),
}

impl Watched {
    fn try_clone(&self) -> Option<Self> {
        match self {
            Self::Tcp(socket) => socket.try_clone().ok().map(Self::Tcp),
            Self::Local(stream) => stream.try_clone().ok().map(Self::Local),
        }
    }
}

#[derive(Default)]
struct WatchState {
    running: bool,
    batch: u64,
    seen: bool,
    closed: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<WatchState>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, WatchState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait_while<'g>(
        &self,
        guard: MutexGuard<'g, WatchState>,
        condition: impl FnMut(&mut WatchState) -> bool,
    ) -> MutexGuard<'g, WatchState> {
        self.changed
            .wait_while(guard, condition)
            .unwrap_or_else(PoisonError::into_inner)
    }
}

pub(crate) struct AttentionWatch {
    shared: Arc<Shared>,
    socket: Option<Watched>,
    started: bool,
    signal: CancelSignal,
}

fn batch_to_watch(shared: &Shared) -> Option<u64> {
    let guard = shared.wait_while(shared.lock(), |state| !state.running && !state.closed);
    (!guard.closed).then_some(guard.batch)
}

fn bytes_arrived_during(shared: &Shared, batch: u64, signal: &CancelSignal) -> bool {
    let mut guard = shared.lock();
    if guard.running && guard.batch == batch {
        guard.seen = true;
        signal.cancel();
    }
    let guard = shared.wait_while(guard, |state| {
        state.running && state.batch == batch && !state.closed
    });
    !guard.closed
}

fn arrived_on_socket(socket: &TcpStream) -> bool {
    let mut probe = [0u8; 1];
    match socket.peek(&mut probe) {
        Ok(_) => true,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
            ) =>
        {
            false
        }
        Err(_) => true,
    }
}

fn still_running(shared: &Shared, batch: u64) -> bool {
    let guard = shared.lock();
    guard.running && guard.batch == batch && !guard.closed
}

fn arrived_on_local(shared: &Shared, batch: u64, stream: &LocalStream) -> bool {
    loop {
        match bytes_waiting(stream) {
            Ok(0) => {}
            Ok(_) | Err(_) => return true,
        }
        if !still_running(shared, batch) {
            return false;
        }
        std::thread::sleep(LOCAL_POLL_INTERVAL);
    }
}

fn watch(shared: &Shared, watched: &Watched, signal: &CancelSignal) {
    loop {
        let Some(batch) = batch_to_watch(shared) else {
            return;
        };
        let arrived = match watched {
            Watched::Tcp(socket) => arrived_on_socket(socket),
            Watched::Local(stream) => arrived_on_local(shared, batch, stream),
        };
        if arrived && !bytes_arrived_during(shared, batch, signal) {
            return;
        }
    }
}

impl AttentionWatch {
    pub(crate) fn new(socket: Option<Watched>, signal: CancelSignal) -> Self {
        Self {
            shared: Arc::new(Shared::default()),
            socket,
            started: false,
            signal,
        }
    }

    fn start(&mut self) {
        self.started = true;
        let Some(socket) = self
            .socket
            .as_ref()
            .and_then(Watched::try_clone)
        else {
            return;
        };
        let shared = Arc::clone(&self.shared);
        let signal = self.signal.clone();
        let spawned = std::thread::Builder::new()
            .name("eggdb-tds-attention".to_owned())
            .stack_size(WATCHER_STACK_BYTES)
            .spawn(move || watch(&shared, &socket, &signal));
        if spawned.is_err() {
            self.socket = None;
        }
    }

    pub(crate) fn begin(&mut self) {
        if !self.started {
            self.start();
        }
        let mut state = self.shared.lock();
        state.running = true;
        state.batch = state.batch.wrapping_add(1);
        state.seen = false;
        self.shared.changed.notify_all();
    }

    pub(crate) fn end(&mut self) -> bool {
        let mut state = self.shared.lock();
        state.running = false;
        self.shared.changed.notify_all();
        state.seen
    }

    pub(crate) fn seen(&self) -> bool {
        self.shared.lock().seen
    }

    pub(crate) fn take_seen(&mut self) -> bool {
        std::mem::take(&mut self.shared.lock().seen)
    }
}

impl Drop for AttentionWatch {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.running = false;
        state.closed = true;
        self.shared.changed.notify_all();
        drop(state);
        if self.started
            && let Some(Watched::Tcp(socket)) = &self.socket
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Shared, batch_to_watch, bytes_arrived_during};
    use eggdb_sql::CancelSignal;

    fn begin(shared: &Shared) {
        let mut state = shared.lock();
        state.running = true;
        state.batch = state.batch.wrapping_add(1);
        state.seen = false;
    }

    fn end(shared: &Shared) {
        shared.lock().running = false;
    }

    #[test]
    fn bytes_that_arrive_during_a_batch_cancel_it() {
        let shared = Shared::default();
        let signal = CancelSignal::default();
        begin(&shared);
        let batch = batch_to_watch(&shared).unwrap();
        let watching_on = std::thread::scope(|scope| {
            let watcher = scope.spawn(|| bytes_arrived_during(&shared, batch, &signal));
            while !signal.is_cancelled() {
                std::thread::yield_now();
            }
            end(&shared);
            shared.changed.notify_all();
            watcher.join().unwrap()
        });
        assert!(watching_on);
        assert!(shared.lock().seen);
    }

    #[test]
    fn bytes_seen_after_the_batch_ended_do_not_cancel_the_next_one() {
        let shared = Shared::default();
        let signal = CancelSignal::default();
        begin(&shared);
        let batch = batch_to_watch(&shared).unwrap();
        end(&shared);
        begin(&shared);
        assert!(bytes_arrived_during(&shared, batch, &signal));
        assert!(!signal.is_cancelled());
        assert!(!shared.lock().seen);
        assert_eq!(batch_to_watch(&shared), Some(batch + 1));
    }
}
