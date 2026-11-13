//! A listening socket handed over by the service manager, so the first connection starts the
//! server (D1102): systemd's socket activation passes it as descriptor 3, naming this process in
//! `LISTEN_PID` and the count in `LISTEN_FDS`.

use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};

static TAKEN: AtomicBool = AtomicBool::new(false);

/// The TCP listener the service manager passed, the first time it is asked for; `None` when
/// none was passed, on systems without socket activation, and every later time.
pub fn inherited_tcp_listener() -> Option<TcpListener> {
    let ours = std::env::var("LISTEN_PID").ok()?.trim().parse::<u32>().ok()? == std::process::id();
    let count = std::env::var("LISTEN_FDS").ok()?.trim().parse::<u32>().ok()?;
    if !ours || count == 0 || TAKEN.swap(true, Ordering::AcqRel) {
        return None;
    }
    os::listener_at_descriptor_three()
}

#[cfg(unix)]
mod os {
    use std::net::TcpListener;
    use std::os::fd::FromRawFd;

    const FIRST_PASSED_DESCRIPTOR: i32 = 3;

    pub(super) fn listener_at_descriptor_three() -> Option<TcpListener> {
        // SAFETY: LISTEN_PID names this process, so the service manager passed descriptor 3 to
        // it as a listening socket, and the TAKEN flag makes this the only owner ever created.
        let listener = unsafe { TcpListener::from_raw_fd(FIRST_PASSED_DESCRIPTOR) };
        listener.local_addr().ok().map(|_| listener)
    }
}

#[cfg(not(unix))]
mod os {
    use std::net::TcpListener;

    pub(super) fn listener_at_descriptor_three() -> Option<TcpListener> {
        None
    }
}
