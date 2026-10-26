//! The terminal behind the standard streams: whether a stream is one, and whether text written
//! to it may carry colours as ANSI escape sequences.

use std::ffi::OsStr;
use std::io::IsTerminal;

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
        StandardStream::Error => std::io::stderr().as_raw_handle(),
        StandardStream::Input | StandardStream::Output => std::io::stdout().as_raw_handle(),
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
    fn standard_input_never_takes_escape_sequences() {
        assert!(!enable_escape_sequences(StandardStream::Input));
    }
}
