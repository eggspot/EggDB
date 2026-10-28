use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Instant;

type Overrun = Box<dyn Fn(&str) + Send>;

#[derive(Default)]
struct Watch {
    armed: Option<(Instant, String)>,
}

pub(crate) struct Watchdog {
    shared: Arc<(Mutex<Watch>, Condvar)>,
}

impl Watchdog {
    pub(crate) fn start(overrun: Overrun) -> std::io::Result<Self> {
        let shared = Arc::new((Mutex::new(Watch::default()), Condvar::new()));
        let watched = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("eggdb-mcp-watchdog".to_owned())
            .spawn(move || watch(&watched, &overrun))?;
        Ok(Self { shared })
    }

    pub(crate) fn arm(&self, give_up_at: Instant, tool: &str) {
        let (watch, wake) = &*self.shared;
        watch.lock().unwrap_or_else(PoisonError::into_inner).armed =
            Some((give_up_at, tool.to_owned()));
        wake.notify_one();
    }

    pub(crate) fn disarm(&self) {
        let (watch, wake) = &*self.shared;
        watch.lock().unwrap_or_else(PoisonError::into_inner).armed = None;
        wake.notify_one();
    }
}

fn watch(shared: &(Mutex<Watch>, Condvar), overrun: &Overrun) {
    let (watch, wake) = shared;
    let mut state = watch.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        let Some((give_up_at, tool)) = &state.armed else {
            state = wake.wait(state).unwrap_or_else(PoisonError::into_inner);
            continue;
        };
        let now = Instant::now();
        if now >= *give_up_at {
            let tool = tool.clone();
            drop(state);
            overrun(&tool);
            return;
        }
        let wait = *give_up_at - now;
        state = wake
            .wait_timeout(state, wait)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

#[cfg(test)]
mod tests {
    use super::Watchdog;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn a_disarmed_call_never_fires_and_an_overrun_does() {
        let (fired, received) = mpsc::channel();
        let watchdog = Watchdog::start(Box::new(move |tool| {
            let _ = fired.send(tool.to_owned());
        }))
        .unwrap();
        watchdog.arm(Instant::now() + Duration::from_millis(200), "list_tables");
        watchdog.disarm();
        assert!(received.recv_timeout(Duration::from_millis(400)).is_err());
        watchdog.arm(Instant::now() + Duration::from_millis(50), "run_sql");
        assert_eq!(
            received.recv_timeout(Duration::from_secs(5)).unwrap(),
            "run_sql"
        );
    }
}
