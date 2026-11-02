//! Native windows for EggDB's GUI (D8, D191, DNEW-I29): open a window of a logical size,
//! present RGBA frames into it, and read its keyboard, resize, scale and close events.
//!
//! Every operating system's window API stays behind [`open_window`] and the [`Window`] trait,
//! so the GUI toolkit draws frames without knowing which system shows them. On Linux and the
//! BSDs the backend speaks the X11 protocol itself over the display's Unix socket ([`x11`]);
//! Windows and macOS answer [`WindowError::Unsupported`] until their backends land.

use std::fmt;
use std::time::Duration;

pub mod x11;

#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
mod x11_window;

/// What to open: the title and the size in logical pixels, which the backend multiplies by the
/// display's scale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowOptions {
    /// The title the window manager shows.
    pub title: String,
    /// The width in logical pixels.
    pub width: u32,
    /// The height in logical pixels.
    pub height: u32,
    /// The smallest width the user may resize to, in logical pixels.
    pub min_width: u32,
    /// The smallest height the user may resize to, in logical pixels.
    pub min_height: u32,
}

/// A key the GUI understands, already translated from the platform's key codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Tab (Shift+Tab arrives as `Tab` with Shift held).
    Tab,
    /// Return or the keypad's Enter.
    Enter,
    /// Escape.
    Escape,
    /// The space bar.
    Space,
    /// Backspace.
    Backspace,
    /// Forward delete.
    Delete,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Home.
    Home,
    /// End.
    End,
    /// The F4 function key.
    F4,
    /// A key that types a character, as typed with the modifiers' effect on case already
    /// applied (`q` or `Q`).
    Character(char),
}

/// The modifier keys held during a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    /// Shift.
    pub shift: bool,
    /// Control.
    pub control: bool,
    /// Alt, or Option on macOS.
    pub alt: bool,
    /// Command on macOS.
    pub command: bool,
}

/// One key press with its modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyPress {
    /// The key.
    pub key: Key,
    /// The modifiers held.
    pub modifiers: Modifiers,
}

impl KeyPress {
    /// `key` with no modifiers.
    pub const fn plain(key: Key) -> Self {
        Self {
            key,
            modifiers: Modifiers {
                shift: false,
                control: false,
                alt: false,
                command: false,
            },
        }
    }
}

/// Something that happened to a window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowEvent {
    /// A key was pressed while the window had the keyboard focus.
    Key(KeyPress),
    /// The window's drawable area is now `width` × `height` device pixels.
    Resized {
        /// The width in device pixels.
        width: u32,
        /// The height in device pixels.
        height: u32,
    },
    /// The display's scale changed: device pixels per logical pixel.
    ScaleChanged(f32),
    /// The window's content was lost and must be presented again.
    Redraw,
    /// The window gained (`true`) or lost (`false`) the keyboard focus.
    Focused(bool),
    /// The user asked to close the window (its close button or the window manager).
    CloseRequested,
}

/// Why a window could not be opened or used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowError {
    /// This operating system has no window backend yet.
    Unsupported(&'static str),
    /// No display to open a window on, with the reason (for example `DISPLAY` is unset).
    NoDisplay(String),
    /// Talking to the display failed.
    Io(String),
    /// The display sent something this backend does not accept, or refused a request.
    Protocol(String),
    /// The caller passed a frame or size the window cannot take.
    InvalidInput(String),
}

impl fmt::Display for WindowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(what) => write!(f, "{what}"),
            Self::NoDisplay(reason) => write!(f, "no display to open a window on: {reason}"),
            Self::Io(reason) => write!(f, "talking to the display failed: {reason}"),
            Self::Protocol(reason) => write!(f, "the display's reply was not understood: {reason}"),
            Self::InvalidInput(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for WindowError {}

/// An open native window.
pub trait Window: fmt::Debug {
    /// The drawable area in device pixels.
    fn size(&self) -> (u32, u32);

    /// Device pixels per logical pixel (1 on a standard display, 2 on most HiDPI ones).
    fn scale(&self) -> f32;

    /// Shows a frame of `width` × `height` device pixels, `rgba` holding four bytes (red,
    /// green, blue, alpha) per pixel row by row.
    fn present(&mut self, width: u32, height: u32, rgba: &[u8]) -> Result<(), WindowError>;

    /// The next event, waiting up to `timeout` for one (`None` waits as long as it takes);
    /// `Ok(None)` when the time ran out.
    fn next_event(&mut self, timeout: Option<Duration>)
    -> Result<Option<WindowEvent>, WindowError>;
}

/// Checks that `rgba` holds exactly `width` × `height` pixels of four bytes.
pub fn check_frame(width: u32, height: u32, rgba: &[u8]) -> Result<(), WindowError> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4));
    if expected == Some(rgba.len()) {
        Ok(())
    } else {
        Err(WindowError::InvalidInput(format!(
            "a {width} × {height} frame needs {} bytes, got {}",
            expected.map_or_else(|| "more than addressable".to_owned(), |n| n.to_string()),
            rgba.len()
        )))
    }
}

/// Opens a native window on this system's display.
pub fn open_window(options: &WindowOptions) -> Result<Box<dyn Window>, WindowError> {
    #[cfg(any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    {
        let window = x11_window::X11Window::open(options)?;
        Ok(Box::new(window))
    }
    #[cfg(windows)]
    {
        let _ = options;
        Err(WindowError::Unsupported(
            "native windows on Windows (Win32) are not supported yet",
        ))
    }
    #[cfg(target_vendor = "apple")]
    {
        let _ = options;
        Err(WindowError::Unsupported(
            "native windows on macOS (Cocoa) are not supported yet",
        ))
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        windows,
        target_vendor = "apple"
    )))]
    {
        let _ = options;
        Err(WindowError::Unsupported(
            "native windows are not supported on this operating system yet",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_must_hold_four_bytes_per_pixel() {
        assert_eq!(check_frame(2, 3, &[0; 24]), Ok(()));
        assert!(matches!(
            check_frame(2, 3, &[0; 23]),
            Err(WindowError::InvalidInput(_))
        ));
        assert!(check_frame(u32::MAX, u32::MAX, &[]).is_err());
    }

    #[test]
    fn without_a_display_opening_fails_with_an_error_not_a_panic() {
        if std::env::var_os("DISPLAY").is_some() {
            return;
        }
        let options = WindowOptions {
            title: "EggDB".to_owned(),
            width: 320,
            height: 200,
            min_width: 100,
            min_height: 100,
        };
        assert!(open_window(&options).is_err());
    }
}
