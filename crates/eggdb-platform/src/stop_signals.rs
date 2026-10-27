//! Asking a long-running process, such as a server, to stop: `Ctrl-C` (`SIGINT`) and `SIGTERM`
//! on Unix, and `Ctrl-C`, `Ctrl-Break` and closing the console on Windows. After [`watch`], those
//! no longer end the process at once; they set a flag that [`requested`] reads, so the process
//! can finish its work and stop on its own.

use std::sync::atomic::{AtomicBool, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);

/// Whether a stop was asked for since [`watch`].
pub fn requested() -> bool {
    REQUESTED.load(Ordering::Acquire)
}

/// Asks for a stop as a signal would, for tests and for a process that stops itself.
pub fn request() {
    REQUESTED.store(true, Ordering::Release);
}

/// Catches the stop signals from now on, for the rest of the process. Fails with
/// [`std::io::ErrorKind::Unsupported`] on systems without them, where the process can only be
/// killed.
pub fn watch() -> std::io::Result<()> {
    os::watch()
}

#[cfg(unix)]
mod os {
    use super::request;
    use std::ffi::c_int;

    const SIGINT: c_int = 2;
    const SIGTERM: c_int = 15;
    const SIG_ERR: usize = usize::MAX;

    unsafe extern "C" {
        fn signal(signal: c_int, handler: usize) -> usize;
    }

    extern "C" fn on_stop_signal(_signal: c_int) {
        request();
    }

    pub(super) fn watch() -> std::io::Result<()> {
        for signal_number in [SIGINT, SIGTERM] {
            let handler = on_stop_signal as extern "C" fn(c_int) as usize;
            // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
            let previous = unsafe { signal(signal_number, handler) };
            if previous == SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
mod os {
    use super::request;

    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    const CTRL_CLOSE_EVENT: u32 = 2;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }

    extern "system" fn on_console_control(event: u32) -> i32 {
        if matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT) {
            request();
            1
        } else {
            0
        }
    }

    pub(super) fn watch() -> std::io::Result<()> {
        // SAFETY: the handler is a plain function that only stores to an atomic.
        if unsafe { SetConsoleCtrlHandler(Some(on_console_control), 1) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
mod os {
    pub(super) fn watch() -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
}
