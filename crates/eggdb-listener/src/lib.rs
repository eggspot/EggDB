//! The pieces EggDB's protocol servers share around their listeners (D122): the rule that a
//! server without TLS listens only on loopback addresses, a counter of connection slots, the
//! throttle that slows password guessing, and the text escaping that keeps a client's words
//! from forging log lines.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const FAILURE_WINDOW: Duration = Duration::from_secs(60);
const DELAY_PER_RECENT_FAILURE: Duration = Duration::from_millis(250);
const LONGEST_FAILURE_DELAY: Duration = Duration::from_secs(5);
const MOST_REMEMBERED_FAILURES: usize = 1024;

/// Why a listener was not opened.
#[derive(Debug)]
pub enum ListenError {
    /// The address is not a loopback address (D102): without TLS, a server only listens where
    /// only this machine can connect.
    NotLoopback(SocketAddr),
    /// The operating system refused, for example because the port is in use.
    Io(io::Error),
}

impl fmt::Display for ListenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLoopback(address) => write!(
                f,
                "{address} is not a loopback address; without TLS EggDB listens only on 127.0.0.1, ::1 or another loopback address, so only this machine can connect: give it a certificate (--tls-certificate and --tls-key, or --tls-self-signed) to listen on other addresses"
            ),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ListenError {}

/// Opens a TCP listener at `address`, which must be a loopback address (`127.0.0.0/8` or
/// `::1`); any other address is refused before anything is bound (D102). Port 0 picks a free
/// port.
pub fn listen_on_loopback(address: SocketAddr) -> Result<TcpListener, ListenError> {
    listen(address, false)
}

/// Opens the listener on `address`. Any address is allowed when the server has TLS
/// (`tls_configured`), since connections from other machines must then use it; without TLS only
/// loopback addresses are.
pub fn listen(address: SocketAddr, tls_configured: bool) -> Result<TcpListener, ListenError> {
    if !tls_configured && !address.ip().is_loopback() {
        return Err(ListenError::NotLoopback(address));
    }
    TcpListener::bind(address).map_err(ListenError::Io)
}

/// Slows password guessing across a server's connections: every failed login waits 250 ms
/// for each failure in the last minute, at most 5 s.
#[derive(Debug, Default)]
pub struct AuthenticationThrottle {
    recent_failures: Mutex<VecDeque<Instant>>,
}

impl AuthenticationThrottle {
    /// A throttle that has seen no failures.
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts one more failed login and returns how long its connection waits before it answers.
    pub fn delay_after_failure(&self) -> Duration {
        let now = Instant::now();
        let mut failures = self
            .recent_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while failures
            .front()
            .is_some_and(|failed| now.duration_since(*failed) > FAILURE_WINDOW)
        {
            failures.pop_front();
        }
        if failures.len() < MOST_REMEMBERED_FAILURES {
            failures.push_back(now);
        }
        let count = u32::try_from(failures.len()).unwrap_or(u32::MAX);
        DELAY_PER_RECENT_FAILURE
            .saturating_mul(count)
            .min(LONGEST_FAILURE_DELAY)
    }
}

/// One of a counter's slots, given back when dropped.
#[derive(Debug)]
pub struct Slot<'c>(&'c AtomicUsize);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Takes one of `limit` slots of `counter`, or `None` when all are taken.
pub fn take_slot(counter: &AtomicUsize, limit: usize) -> Option<Slot<'_>> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < limit).then_some(count + 1)
        })
        .ok()
        .map(|_| Slot(counter))
}

fn hides_layout(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{200e}' | '\u{200f}' | '\u{061c}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

/// `text` as a log line may show it: backslashes, quotes and the common control characters
/// escaped, and every other control character, line or paragraph separator and bidirectional
/// override written as `\u{..}`, so a client's words can neither end a log line nor reorder it.
pub fn printable(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => shown.push_str("\\\\"),
            '"' => shown.push_str("\\\""),
            '\n' => shown.push_str("\\n"),
            '\r' => shown.push_str("\\r"),
            '\t' => shown.push_str("\\t"),
            special if hides_layout(special) => {
                shown.push_str(&format!("\\u{{{:x}}}", u32::from(special)));
            }
            other => shown.push(other),
        }
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_addresses_listen_without_tls() {
        let outside: SocketAddr = "0.0.0.0:0".parse().unwrap();
        assert!(matches!(listen(outside, false), Err(ListenError::NotLoopback(_))));
        assert!(listen_on_loopback("127.0.0.1:0".parse().unwrap()).is_ok());
    }

    #[test]
    fn slots_run_out_and_come_back_when_dropped() {
        let counter = AtomicUsize::new(0);
        let first = take_slot(&counter, 2).unwrap();
        let second = take_slot(&counter, 2).unwrap();
        assert!(take_slot(&counter, 2).is_none());
        drop(first);
        assert!(take_slot(&counter, 2).is_some());
        drop(second);
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }

    #[test]
    fn failures_wait_longer_up_to_five_seconds() {
        let throttle = AuthenticationThrottle::new();
        assert_eq!(throttle.delay_after_failure(), Duration::from_millis(250));
        assert_eq!(throttle.delay_after_failure(), Duration::from_millis(500));
        for _ in 0..40 {
            throttle.delay_after_failure();
        }
        assert_eq!(throttle.delay_after_failure(), Duration::from_secs(5));
    }

    #[test]
    fn printable_text_cannot_end_or_reorder_a_log_line() {
        assert_eq!(printable("a\nb\"c\\"), "a\\nb\\\"c\\\\");
        assert_eq!(printable("\u{202e}x\u{2066}"), "\\u{202e}x\\u{2066}");
        assert_eq!(printable("héllo"), "héllo");
    }
}
