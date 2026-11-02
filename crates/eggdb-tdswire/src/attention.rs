use eggdb_sql::CancelSignal;
use std::io::ErrorKind;
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

const WATCHER_STACK_BYTES: usize = 64 * 1024;

#[derive(Default)]
struct WatchState {
    running: bool,
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
    socket: Option<TcpStream>,
    started: bool,
    signal: CancelSignal,
}

fn watch(shared: &Shared, socket: &TcpStream, signal: &CancelSignal) {
    let mut probe = [0u8; 1];
    loop {
        {
            let guard = shared.wait_while(shared.lock(), |state| !state.running && !state.closed);
            if guard.closed {
                return;
            }
        }
        let arrived = match socket.peek(&mut probe) {
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
        };
        if !arrived {
            continue;
        }
        let mut guard = shared.lock();
        if guard.running {
            guard.seen = true;
            signal.cancel();
        }
        let guard = shared.wait_while(guard, |state| state.running && !state.closed);
        if guard.closed {
            return;
        }
    }
}

impl AttentionWatch {
    pub(crate) fn new(socket: Option<TcpStream>, signal: CancelSignal) -> Self {
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
            .and_then(|socket| socket.try_clone().ok())
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
            && let Some(socket) = &self.socket
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}
