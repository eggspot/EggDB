use crate::terminal::{Input, TerminalSize};
use std::ffi::{c_int, c_short, c_ulong, c_ushort, c_void};
use std::io;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const STDIN: c_int = 0;
const STDOUT: c_int = 1;
const TCSANOW: c_int = 0;
const POLLIN: c_short = 0x1;
const POLLNVAL: c_short = 0x20;
const SIGHUP: c_int = 1;
const SIGINT: c_int = 2;
const SIGQUIT: c_int = 3;
const SIGTERM: c_int = 15;
const SIGWINCH: c_int = 28;
const SIG_IGN: usize = 1;
const SIG_ERR: usize = usize::MAX;
const OPAQUE_WORDS: usize = 32;
const LONGEST_POLL: Duration = Duration::from_millis(100);
const LARGEST_SIDE: u16 = 1000;

#[cfg(any(target_os = "linux", target_os = "android"))]
const TIOCGWINSZ: c_ulong = 0x5413;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const TIOCGWINSZ: c_ulong = 0x4008_7468;

#[cfg(target_os = "linux")]
type PollCount = c_ulong;
#[cfg(not(target_os = "linux"))]
type PollCount = std::ffi::c_uint;

#[repr(C, align(16))]
struct OpaqueBuffer([u8; OPAQUE_WORDS * 8]);

impl OpaqueBuffer {
    fn zeroed() -> Self {
        Self([0; OPAQUE_WORDS * 8])
    }

    fn save(&self, words: &[AtomicU64; OPAQUE_WORDS]) {
        for (word, bytes) in words.iter().zip(self.0.as_chunks::<8>().0) {
            word.store(u64::from_ne_bytes(*bytes), Ordering::Relaxed);
        }
    }

    fn load(words: &[AtomicU64; OPAQUE_WORDS]) -> Self {
        let mut buffer = Self::zeroed();
        for (bytes, word) in buffer.0.as_chunks_mut::<8>().0.iter_mut().zip(words) {
            *bytes = word.load(Ordering::Relaxed).to_ne_bytes();
        }
        buffer
    }
}

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}

#[repr(C)]
#[derive(Default)]
struct WindowSize {
    rows: c_ushort,
    cols: c_ushort,
    x_pixels: c_ushort,
    y_pixels: c_ushort,
}

unsafe extern "C" {
    fn isatty(fd: c_int) -> c_int;
    fn tcgetattr(fd: c_int, termios: *mut OpaqueBuffer) -> c_int;
    fn tcsetattr(fd: c_int, action: c_int, termios: *const OpaqueBuffer) -> c_int;
    fn cfmakeraw(termios: *mut OpaqueBuffer);
    #[cfg_attr(target_os = "netbsd", link_name = "__sigaction14")]
    fn sigaction(signal: c_int, action: *const OpaqueBuffer, previous: *mut OpaqueBuffer) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn poll(fds: *mut PollFd, count: PollCount, timeout_ms: c_int) -> c_int;
    fn read(fd: c_int, buffer: *mut c_void, count: usize) -> isize;
    fn write(fd: c_int, buffer: *const c_void, count: usize) -> isize;
    fn signal(signal: c_int, handler: usize) -> usize;
    fn _exit(status: c_int) -> !;
}

const HANDLED_SIGNALS: [c_int; 5] = [SIGWINCH, SIGTERM, SIGHUP, SIGQUIT, SIGINT];

static CLAIMED: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static RESIZED: AtomicBool = AtomicBool::new(false);
static SAVED: [AtomicU64; OPAQUE_WORDS] = [const { AtomicU64::new(0) }; OPAQUE_WORDS];
static PANIC_HOOK: Once = Once::new();
static PREVIOUS_ACTIONS: [[AtomicU64; OPAQUE_WORDS]; HANDLED_SIGNALS.len()] =
    [const { [const { AtomicU64::new(0) }; OPAQUE_WORDS] }; HANDLED_SIGNALS.len()];
static REPLACED: [AtomicBool; HANDLED_SIGNALS.len()] =
    [const { AtomicBool::new(false) }; HANDLED_SIGNALS.len()];

#[derive(Debug)]
pub(crate) struct RawMode {
    _not_send: std::marker::PhantomData<*const ()>,
}

pub(crate) fn both_streams_are_terminals() -> bool {
    // SAFETY: isatty only inspects the descriptor number; an unused descriptor returns 0.
    let input = unsafe { isatty(STDIN) } == 1;
    // SAFETY: as above, for standard output.
    let output = unsafe { isatty(STDOUT) } == 1;
    input && output
}

impl RawMode {
    pub(crate) fn enter(start_sequence: &[u8]) -> io::Result<Self> {
        if !both_streams_are_terminals() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "standard input and output are not both terminals",
            ));
        }
        if CLAIMED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the terminal is already in raw mode",
            ));
        }
        match enter_raw_mode() {
            Ok(()) => {}
            Err(error) => {
                CLAIMED.store(false, Ordering::Release);
                return Err(error);
            }
        }
        install_panic_hook();
        let mode = Self {
            _not_send: std::marker::PhantomData,
        };
        write_fully(start_sequence)?;
        Ok(mode)
    }

    pub(crate) fn size(&self) -> io::Result<TerminalSize> {
        let mut size = WindowSize::default();
        // SAFETY: TIOCGWINSZ writes one `struct winsize` (four unsigned shorts) to the pointer,
        // which points at a live `WindowSize` of that layout.
        if unsafe { ioctl(STDOUT, TIOCGWINSZ, &mut size as *mut WindowSize) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(TerminalSize {
            cols: size.cols.min(LARGEST_SIDE),
            rows: size.rows.min(LARGEST_SIDE),
        })
    }

    pub(crate) fn read_input(&mut self, buffer: &mut [u8], timeout: Duration) -> io::Result<Input> {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            if RESIZED.swap(false, Ordering::AcqRel) {
                return Ok(Input::Resized);
            }
            let left = deadline.map_or(LONGEST_POLL, |deadline| {
                deadline.saturating_duration_since(Instant::now())
            });
            let slice = left.min(LONGEST_POLL);
            let mut fds = PollFd {
                fd: STDIN,
                events: POLLIN,
                revents: 0,
            };
            let timeout_ms = c_int::try_from(slice.as_millis()).unwrap_or(c_int::MAX);
            // SAFETY: one live `PollFd` is passed with a count of one.
            let ready = unsafe { poll(&mut fds, 1, timeout_ms) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready > 0 && fds.revents & POLLNVAL != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "this terminal cannot be waited on for input",
                ));
            }
            if ready > 0 {
                return read_ready(buffer);
            }
            if left <= slice {
                return Ok(Input::TimedOut);
            }
        }
    }

    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        write_fully(bytes)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        restore();
        reset_handlers();
        CLAIMED.store(false, Ordering::Release);
    }
}

fn enter_raw_mode() -> io::Result<()> {
    let mut original = OpaqueBuffer::zeroed();
    // SAFETY: `original` is larger than any supported system's termios and 16-byte aligned,
    // and it outlives the synchronous call.
    if unsafe { tcgetattr(STDIN, &mut original) } != 0 {
        return Err(io::Error::last_os_error());
    }
    original.save(&SAVED);
    let mut raw = OpaqueBuffer(original.0);
    // SAFETY: `raw` holds a termios just filled in by tcgetattr, in a large enough buffer.
    unsafe { cfmakeraw(&mut raw) };
    RESIZED.store(false, Ordering::Relaxed);
    install_handlers();
    ACTIVE.store(true, Ordering::Release);
    // SAFETY: `raw` is a complete termios made by cfmakeraw from the terminal's own.
    if unsafe { tcsetattr(STDIN, TCSANOW, &raw) } != 0 {
        let error = io::Error::last_os_error();
        ACTIVE.store(false, Ordering::Release);
        reset_handlers();
        return Err(error);
    }
    Ok(())
}

fn read_ready(buffer: &mut [u8]) -> io::Result<Input> {
    // SAFETY: the pointer and length describe `buffer`, which outlives the synchronous call.
    let count = unsafe { read(STDIN, buffer.as_mut_ptr().cast(), buffer.len()) };
    match count {
        0 => Ok(Input::Closed),
        1.. => Ok(Input::Bytes(count.unsigned_abs())),
        _ => {
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => Ok(Input::TimedOut),
                _ => Err(error),
            }
        }
    }
}

fn write_fully(mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        // SAFETY: the pointer and length describe `bytes`, which outlives the call.
        let written = unsafe { write(STDOUT, bytes.as_ptr().cast(), bytes.len()) };
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        bytes = &bytes[written.unsigned_abs().min(bytes.len())..];
    }
    Ok(())
}

fn restore() {
    restore_with(put_terminal_back);
}

fn restore_with(put_back: impl FnOnce()) {
    if !ACTIVE.load(Ordering::Acquire) {
        return;
    }
    put_back();
    ACTIVE.store(false, Ordering::Release);
}

fn put_terminal_back() {
    let restore_bytes = crate::terminal::RESTORE_SEQUENCE;
    // SAFETY: write is async-signal-safe; the pointer and length describe a static string.
    let _ = unsafe { write(STDOUT, restore_bytes.as_ptr().cast(), restore_bytes.len()) };
    let saved = OpaqueBuffer::load(&SAVED);
    // SAFETY: `saved` holds the termios tcgetattr returned before raw mode began; tcsetattr is
    // async-signal-safe.
    let _ = unsafe { tcsetattr(STDIN, TCSANOW, &saved) };
}

extern "C" fn on_resize(_signal: c_int) {
    RESIZED.store(true, Ordering::Release);
}

extern "C" fn on_terminate(signal: c_int) {
    restore();
    // SAFETY: _exit is async-signal-safe and ends the process without running destructors.
    unsafe { _exit(128 + signal) }
}

fn install_handlers() {
    let resize: extern "C" fn(c_int) = on_resize;
    let terminate: extern "C" fn(c_int) = on_terminate;
    for (index, signal_number) in HANDLED_SIGNALS.into_iter().enumerate() {
        let mut previous = OpaqueBuffer::zeroed();
        // SAFETY: a null action only reads the current disposition into `previous`, a
        // 16-byte-aligned buffer larger than any supported system's `struct sigaction`.
        if unsafe { sigaction(signal_number, std::ptr::null(), &mut previous) } != 0 {
            continue;
        }
        previous.save(&PREVIOUS_ACTIONS[index]);
        let handler = if signal_number == SIGWINCH {
            resize as usize
        } else {
            terminate as usize
        };
        // SAFETY: both handlers only use atomics and async-signal-safe calls (write,
        // tcsetattr, _exit). Every supported system's `signal` keeps the handler installed
        // after it runs (BSD semantics).
        let replaced = unsafe { signal(signal_number, handler) };
        if replaced == SIG_ERR {
            continue;
        }
        REPLACED[index].store(true, Ordering::Release);
        if replaced == SIG_IGN && signal_number != SIGWINCH {
            put_back(index, signal_number);
        }
    }
}

fn put_back(index: usize, signal_number: c_int) {
    if !REPLACED[index].swap(false, Ordering::AcqRel) {
        return;
    }
    let previous = OpaqueBuffer::load(&PREVIOUS_ACTIONS[index]);
    // SAFETY: `previous` holds, byte for byte, the `struct sigaction` that sigaction itself
    // reported for this signal, so its handler, flags and mask are put back exactly.
    unsafe { sigaction(signal_number, &previous, std::ptr::null_mut()) };
}

fn reset_handlers() {
    for (index, signal_number) in HANDLED_SIGNALS.into_iter().enumerate() {
        put_back(index, signal_number);
    }
}

fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::{ACTIVE, restore_with};
    use std::sync::atomic::Ordering;

    #[test]
    fn a_signal_arriving_during_a_restore_still_restores_before_it_exits() {
        ACTIVE.store(true, Ordering::Release);
        let mut handler_restored = false;
        restore_with(|| {
            restore_with(|| handler_restored = true);
        });
        assert!(handler_restored);
        assert!(!ACTIVE.load(Ordering::Acquire));
        let mut restored_again = false;
        restore_with(|| restored_again = true);
        assert!(!restored_again);
    }
}
