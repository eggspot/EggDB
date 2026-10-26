//! The terminal behind the standard streams: whether a stream is one, whether text written
//! to it may carry colours as ANSI escape sequences, and a [`RawTerminal`] that takes the whole
//! terminal for a full-screen program and gives it back on exit, on a panic and on the signals
//! that end a process.

use std::ffi::OsStr;
use std::io::IsTerminal;
use std::time::Duration;

#[cfg(any(
    all(
        any(target_os = "linux", target_os = "android"),
        any(
            target_arch = "x86",
            target_arch = "x86_64",
            target_arch = "arm",
            target_arch = "aarch64",
            target_arch = "riscv32",
            target_arch = "riscv64",
            target_arch = "loongarch64",
            target_arch = "s390x"
        )
    ),
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
use crate::os_terminal_unix as os;
#[cfg(windows)]
use crate::os_terminal_windows as os;
#[cfg(not(any(
    windows,
    all(
        any(target_os = "linux", target_os = "android"),
        any(
            target_arch = "x86",
            target_arch = "x86_64",
            target_arch = "arm",
            target_arch = "aarch64",
            target_arch = "riscv32",
            target_arch = "riscv64",
            target_arch = "loongarch64",
            target_arch = "s390x"
        )
    ),
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
)))]
use unsupported as os;

pub(crate) const START_SEQUENCE: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[?2004h\x1b[?7l\x1b[2J";
pub(crate) const RESTORE_SEQUENCE: &[u8] = b"\x1b[0m\x1b[?7h\x1b[?2004l\x1b[?25h\x1b[?1049l";

/// The size of a terminal's window in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TerminalSize {
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
}

/// What [`RawTerminal::read_input`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Input {
    /// This many bytes were read into the buffer, as the terminal sent them.
    Bytes(usize),
    /// The window changed size; [`RawTerminal::size`] has the new one.
    Resized,
    /// Nothing arrived before the timeout.
    TimedOut,
    /// The terminal is gone: its input reached the end.
    Closed,
}

/// The terminal behind standard input and output, in raw mode, on the alternate screen, with
/// the cursor hidden and bracketed paste on.
///
/// Raw mode passes every key to the program as bytes (`Ctrl-C` included, which no longer sends
/// a signal) and stops the terminal from echoing or changing what is written. Dropping the
/// value gives the terminal back as it was, as do a panic and, on Unix, `SIGTERM`, `SIGHUP`,
/// `SIGQUIT` and a `SIGINT` sent by another process (which then end the process with exit
/// status 128 + the signal's number); on Windows `Ctrl-C` and `Ctrl-Break` events, closing the
/// console, logging off and shutting down restore it too. Only one exists at a time.
#[derive(Debug)]
pub struct RawTerminal {
    mode: os::RawMode,
}

impl RawTerminal {
    /// Takes the terminal. Fails with [`std::io::ErrorKind::Unsupported`] when standard input
    /// and output are not both terminals or the system has none, and with
    /// [`std::io::ErrorKind::AlreadyExists`] while another `RawTerminal` exists.
    pub fn enter() -> std::io::Result<Self> {
        Ok(Self {
            mode: os::RawMode::enter(START_SEQUENCE)?,
        })
    }

    /// The window's size now.
    pub fn size(&self) -> std::io::Result<TerminalSize> {
        self.mode.size()
    }

    /// Waits up to `timeout` for input and reads what has arrived into `buffer`, or reports a
    /// resize. Escape sequences come as the terminal sent them, possibly split across reads.
    pub fn read_input(&mut self, buffer: &mut [u8], timeout: Duration) -> std::io::Result<Input> {
        self.mode.read_input(buffer, timeout)
    }

    /// Writes all of `bytes` to the terminal and flushes them.
    pub fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.mode.write_all(bytes)
    }
}

/// One of the three standard streams a process starts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StandardStream {
    /// Standard input.
    Input,
    /// Standard output.
    Output,
    /// Standard error.
    Error,
}

/// Whether the stream is connected to a terminal, rather than to a file, a pipe or nothing.
pub fn is_terminal(stream: StandardStream) -> bool {
    match stream {
        StandardStream::Input => std::io::stdin().is_terminal(),
        StandardStream::Output => std::io::stdout().is_terminal(),
        StandardStream::Error => std::io::stderr().is_terminal(),
    }
}

/// Whether text written to the stream may carry colours as ANSI escape sequences: the stream
/// is a terminal, the `NO_COLOR` environment variable is unset or empty (no-color.org), `TERM`
/// is not `dumb`, and the terminal interprets the sequences, which
/// [`enable_escape_sequences`] turns on where that is needed.
pub fn wants_color(stream: StandardStream) -> bool {
    is_terminal(stream)
        && color_allowed_by_environment(
            std::env::var_os("NO_COLOR").as_deref(),
            std::env::var_os("TERM").as_deref(),
        )
        && enable_escape_sequences(stream)
}

fn color_allowed_by_environment(no_color: Option<&OsStr>, term: Option<&OsStr>) -> bool {
    no_color.is_none_or(OsStr::is_empty) && term != Some(OsStr::new("dumb"))
}

/// Makes the terminal behind an output stream interpret ANSI escape sequences, and returns
/// whether it does. Terminals on Unix always do; a Windows console does once its
/// virtual-terminal mode is on, which this turns on, and refuses on versions before Windows 10.
/// Standard input, a stream that is not a console on Windows, and systems with neither kind of
/// terminal return `false`.
pub fn enable_escape_sequences(stream: StandardStream) -> bool {
    match stream {
        StandardStream::Input => false,
        StandardStream::Output | StandardStream::Error => os_enable_escape_sequences(stream),
    }
}

#[cfg(unix)]
fn os_enable_escape_sequences(_stream: StandardStream) -> bool {
    true
}

#[cfg(windows)]
fn os_enable_escape_sequences(stream: StandardStream) -> bool {
    use std::os::windows::io::{AsRawHandle, RawHandle};
    use windows_console::{ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, SetConsoleMode};

    let handle: RawHandle = match stream {
        StandardStream::Input => return false,
        StandardStream::Output => std::io::stdout().as_raw_handle(),
        StandardStream::Error => std::io::stderr().as_raw_handle(),
    };
    let mut mode = 0u32;
    // SAFETY: the handle belongs to this process's standard stream and stays open for the
    // call, and `mode` lives on this stack frame until the synchronous call returns. A handle
    // that is not a console makes the call fail, which is handled below.
    let got_mode = unsafe { GetConsoleMode(handle, &mut mode) };
    if got_mode == 0 {
        return false;
    }
    if mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0 {
        return true;
    }
    // SAFETY: the same open console handle as above; the call only changes its mode bits.
    let set_mode = unsafe { SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) };
    set_mode != 0
}

#[cfg(not(any(unix, windows)))]
fn os_enable_escape_sequences(_stream: StandardStream) -> bool {
    false
}

/// Whether the terminal shows Unicode: on Windows always (a [`RawTerminal`] sets the console
/// to UTF-8), elsewhere when the first of `LC_ALL`, `LC_CTYPE` and `LANG` that is set names
/// UTF-8.
pub fn wants_unicode() -> bool {
    if cfg!(windows) {
        return true;
    }
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .filter_map(std::env::var_os)
        .find(|value| !value.is_empty())
        .is_some_and(|value| locale_is_utf8(&value))
}

fn locale_is_utf8(locale: &OsStr) -> bool {
    let lower = locale.to_string_lossy().to_ascii_lowercase();
    lower.contains("utf-8") || lower.contains("utf8")
}

/// Whether standard input and standard output are both terminals, which a [`RawTerminal`]
/// needs.
pub fn is_interactive() -> bool {
    os::both_streams_are_terminals()
}

#[cfg(not(any(
    windows,
    all(
        any(target_os = "linux", target_os = "android"),
        any(
            target_arch = "x86",
            target_arch = "x86_64",
            target_arch = "arm",
            target_arch = "aarch64",
            target_arch = "riscv32",
            target_arch = "riscv64",
            target_arch = "loongarch64",
            target_arch = "s390x"
        )
    ),
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
)))]
mod unsupported {
    use super::{Input, TerminalSize};
    use std::io;
    use std::time::Duration;

    #[derive(Debug)]
    pub(crate) enum RawMode {}

    const _: &[u8] = super::RESTORE_SEQUENCE;

    pub(crate) fn both_streams_are_terminals() -> bool {
        false
    }

    impl RawMode {
        pub(crate) fn enter(_start_sequence: &[u8]) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this system has no terminal support",
            ))
        }

        pub(crate) fn size(&self) -> io::Result<TerminalSize> {
            match *self {}
        }

        pub(crate) fn read_input(
            &mut self,
            _buffer: &mut [u8],
            _timeout: Duration,
        ) -> io::Result<Input> {
            match *self {}
        }

        pub(crate) fn write_all(&mut self, _bytes: &[u8]) -> io::Result<()> {
            match *self {}
        }
    }
}

#[cfg(windows)]
mod windows_console {
    use std::os::windows::io::RawHandle;

    pub(super) const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub(super) fn GetConsoleMode(console: RawHandle, mode: *mut u32) -> i32;
        pub(super) fn SetConsoleMode(console: RawHandle, mode: u32) -> i32;
    }
}

#[cfg(test)]
mod tests {
    use super::{StandardStream, color_allowed_by_environment, enable_escape_sequences};
    use std::ffi::OsStr;

    #[test]
    fn colour_is_allowed_without_no_color_and_with_a_capable_term() {
        assert!(color_allowed_by_environment(None, None));
        assert!(color_allowed_by_environment(
            None,
            Some(OsStr::new("xterm-256color"))
        ));
    }

    #[test]
    fn an_empty_no_color_counts_as_unset() {
        assert!(color_allowed_by_environment(Some(OsStr::new("")), None));
    }

    #[test]
    fn any_no_color_value_turns_colour_off() {
        assert!(!color_allowed_by_environment(Some(OsStr::new("1")), None));
        assert!(!color_allowed_by_environment(Some(OsStr::new("0")), None));
    }

    #[test]
    fn a_dumb_terminal_gets_no_colour() {
        assert!(!color_allowed_by_environment(
            None,
            Some(OsStr::new("dumb"))
        ));
    }

    #[test]
    fn utf8_locales_are_recognised() {
        assert!(super::locale_is_utf8(OsStr::new("en_US.UTF-8")));
        assert!(super::locale_is_utf8(OsStr::new("C.utf8")));
        assert!(!super::locale_is_utf8(OsStr::new("C")));
        assert!(!super::locale_is_utf8(OsStr::new("en_US.ISO-8859-1")));
    }

    #[test]
    fn standard_input_never_takes_escape_sequences() {
        assert!(!enable_escape_sequences(StandardStream::Input));
    }
}
