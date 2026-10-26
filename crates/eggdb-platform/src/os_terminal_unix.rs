use crate::terminal::{Input, TerminalSize};
use std::ffi::{c_int, c_short, c_ulong, c_ushort, c_void};
use std::io;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const STDIN: c_int = 0;
const STDOUT: c_int = 1;
const TCSANOW: c_int = 0;
const POLLIN: c_short = 1;
const SIGHUP: c_int = 1;
const SIGQUIT: c_int = 3;
const SIGTERM: c_int = 15;
const SIG_DFL: usize = 0;
const TERMIOS_WORDS: usize = 32;
const LONGEST_POLL: Duration = Duration::from_millis(100);

#[cfg(any(target_os = "solaris", target_os = "illumos"))]
const SIGWINCH: c_int = 20;
#[cfg(not(any(target_os = "solaris", target_os = "illumos")))]
const SIGWINCH: c_int = 28;

#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    not(any(
        target_arch = "powerpc",
        target_arch = "powerpc64",
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "sparc64"
    ))
))]
const TIOCGWINSZ: c_ulong = 0x5413;
#[cfg(any(target_os = "solaris", target_os = "illumos"))]
const TIOCGWINSZ: c_ulong = 0x5468;
#[cfg(not(any(
    all(
        any(target_os = "linux", target_os = "android"),
        not(any(
            target_arch = "powerpc",
            target_arch = "powerpc64",
            target_arch = "mips",
            target_arch = "mips64",
            target_arch = "sparc64"
        ))
    ),
    target_os = "solaris",
    target_os = "illumos"
)))]
const TIOCGWINSZ: c_ulong = 0x4008_7468;

#[cfg(any(target_os = "linux", target_os = "solaris", target_os = "illumos"))]
type PollCount = c_ulong;
#[cfg(not(any(target_os = "linux", target_os = "solaris", target_os = "illumos")))]
type PollCount = std::ffi::c_uint;

#[repr(C, align(8))]
struct TermiosBuffer([u8; TERMIOS_WORDS * 8]);

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
    fn tcgetattr(fd: c_int, termios: *mut TermiosBuffer) -> c_int;
    fn tcsetattr(fd: c_int, action: c_int, termios: *const TermiosBuffer) -> c_int;
    fn cfmakeraw(termios: *mut TermiosBuffer);
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn poll(fds: *mut PollFd, count: PollCount, timeout_ms: c_int) -> c_int;
    fn read(fd: c_int, buffer: *mut c_void, count: usize) -> isize;
    fn write(fd: c_int, buffer: *const c_void, count: usize) -> isize;
    fn signal(signal: c_int, handler: usize) -> usize;
    fn _exit(status: c_int) -> !;
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static RESIZED: AtomicBool = AtomicBool::new(false);
static SAVED: [AtomicU64; TERMIOS_WORDS] = [const { AtomicU64::new(0) }; TERMIOS_WORDS];
static PANIC_HOOK: Once = Once::new();
static PREVIOUS_HANDLERS: [AtomicUsize; 4] = [const { AtomicUsize::new(SIG_DFL) }; 4];

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
        if ACTIVE.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the terminal is already in raw mode",
            ));
        }
        let mut original = TermiosBuffer([0; TERMIOS_WORDS * 8]);
        // SAFETY: `original` is larger than any platform's termios and 8-byte aligned, and it
        // outlives the synchronous call.
        if unsafe { tcgetattr(STDIN, &mut original) } != 0 {
            return Err(io::Error::last_os_error());
        }
        for (word, bytes) in SAVED.iter().zip(original.0.as_chunks::<8>().0) {
            word.store(u64::from_ne_bytes(*bytes), Ordering::Relaxed);
        }
        let mut raw = TermiosBuffer(original.0);
        // SAFETY: `raw` holds a termios just filled in by tcgetattr, in a large enough buffer.
        unsafe { cfmakeraw(&mut raw) };
        // SAFETY: `raw` is a complete termios made by cfmakeraw from the terminal's own.
        if unsafe { tcsetattr(STDIN, TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        ACTIVE.store(true, Ordering::Release);
        RESIZED.store(false, Ordering::Relaxed);
        install_handlers();
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
            cols: size.cols,
            rows: size.rows,
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
        reset_handlers();
        restore();
    }
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
    if !ACTIVE.swap(false, Ordering::AcqRel) {
        return;
    }
    let restore_bytes = crate::terminal::RESTORE_SEQUENCE;
    // SAFETY: write is async-signal-safe; the pointer and length describe a static string.
    let _ = unsafe { write(STDOUT, restore_bytes.as_ptr().cast(), restore_bytes.len()) };
    let mut saved = TermiosBuffer([0; TERMIOS_WORDS * 8]);
    for (bytes, word) in saved.0.as_chunks_mut::<8>().0.iter_mut().zip(SAVED.iter()) {
        *bytes = word.load(Ordering::Relaxed).to_ne_bytes();
    }
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

const HANDLED_SIGNALS: [c_int; 4] = [SIGWINCH, SIGTERM, SIGHUP, SIGQUIT];
const SIG_IGN: usize = 1;
const SIG_ERR: usize = usize::MAX;

fn install_handlers() {
    let resize: extern "C" fn(c_int) = on_resize;
    let terminate: extern "C" fn(c_int) = on_terminate;
    for (slot, signal_number) in PREVIOUS_HANDLERS.iter().zip(HANDLED_SIGNALS) {
        let handler = if signal_number == SIGWINCH {
            resize as usize
        } else {
            terminate as usize
        };
        // SAFETY: both handlers only use atomics and async-signal-safe calls (write,
        // tcsetattr, _exit).
        let previous = unsafe { signal(signal_number, handler) };
        let previous = if previous == SIG_ERR {
            SIG_DFL
        } else {
            previous
        };
        slot.store(previous, Ordering::Relaxed);
        if previous == SIG_IGN && signal_number != SIGWINCH {
            // SAFETY: puts back the ignored disposition the process started with (nohup).
            unsafe { signal(signal_number, SIG_IGN) };
        }
    }
}

fn reset_handlers() {
    for (slot, signal_number) in PREVIOUS_HANDLERS.iter().zip(HANDLED_SIGNALS) {
        // SAFETY: the value is the disposition `signal` returned when the handler was
        // installed: SIG_DFL, SIG_IGN or a handler the process had set, which is still valid.
        unsafe { signal(signal_number, slot.load(Ordering::Relaxed)) };
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
