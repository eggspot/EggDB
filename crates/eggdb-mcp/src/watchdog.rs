use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Instant;

type Overrun = Box<dyn Fn(&str) + Send>;

#[derive(Default)]
enum Watch {
    #[default]
    Idle,
    Armed {
        give_up_at: Instant,
        tool: String,
    },
    Firing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WatchdogFiring;

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
        let mut state = watch.lock().unwrap_or_else(PoisonError::into_inner);
        if !matches!(*state, Watch::Firing) {
            *state = Watch::Armed {
                give_up_at,
                tool: tool.to_owned(),
            };
        }
        wake.notify_one();
    }

    pub(crate) fn disarm(&self) {
        let _ = self.disarm_for_commit();
    }

    pub(crate) fn disarm_for_commit(&self) -> Result<(), WatchdogFiring> {
        let (watch, wake) = &*self.shared;
        let mut state = watch.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(*state, Watch::Firing) {
            return Err(WatchdogFiring);
        }
        *state = Watch::Idle;
        wake.notify_one();
        Ok(())
    }
}

fn watch(shared: &(Mutex<Watch>, Condvar), overrun: &Overrun) {
    let (watch, wake) = shared;
    let mut state = watch.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        let Watch::Armed { give_up_at, tool } = &*state else {
            state = wake.wait(state).unwrap_or_else(PoisonError::into_inner);
            continue;
        };
        let now = Instant::now();
        if now >= *give_up_at {
            let tool = tool.clone();
            *state = Watch::Firing;
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
    use super::{Watchdog, WatchdogFiring};
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

    #[test]
    fn a_commit_never_starts_once_the_watchdog_fires() {
        let (fired, received) = mpsc::channel();
        let (release, held) = mpsc::channel::<()>();
        let held = std::sync::Mutex::new(held);
        let watchdog = Watchdog::start(Box::new(move |tool| {
            let _ = fired.send(tool.to_owned());
            let _ = held.lock().unwrap().recv();
        }))
        .unwrap();
        watchdog.arm(Instant::now(), "write_sql");
        assert_eq!(
            received.recv_timeout(Duration::from_secs(30)).unwrap(),
            "write_sql"
        );
        assert_eq!(watchdog.disarm_for_commit(), Err(WatchdogFiring));
        watchdog.arm(Instant::now() + Duration::from_secs(60), "write_sql");
        assert_eq!(watchdog.disarm_for_commit(), Err(WatchdogFiring));
        release.send(()).unwrap();
    }

    #[test]
    fn a_commit_that_disarmed_first_is_never_interrupted() {
        let (fired, received) = mpsc::channel();
        let watchdog = Watchdog::start(Box::new(move |tool| {
            let _ = fired.send(tool.to_owned());
        }))
        .unwrap();
        watchdog.arm(Instant::now() + Duration::from_millis(100), "write_sql");
        assert_eq!(watchdog.disarm_for_commit(), Ok(()));
        assert!(received.recv_timeout(Duration::from_millis(500)).is_err());
    }
}
