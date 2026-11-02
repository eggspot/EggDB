//! Native windows for EggDB's GUI (D8, D191, DNEW-I29): open a window of a logical size,
//! present RGBA frames into it, and read its keyboard, resize, scale and close events.
//!
//! Every operating system's window API stays behind [`open_window`] and the [`Window`] trait,
//! so the GUI toolkit draws frames without knowing which system shows them. On Linux and the
//! BSDs the backend speaks the X11 protocol itself over the display's Unix socket ([`x11`]);
//! on Windows it calls Win32 through our own declarations ([`win32`]); on macOS it drives
//! AppKit through the Objective-C runtime, loaded only when a window opens ([`cocoa`]).
//!
//! Each window also takes the screen's accessibility tree ([`AccessNode`],
//! [`Window::set_accessibility_tree`]) and hands it to the system's screen readers: through
//! NSAccessibility on macOS, UI Automation on Windows ([`uia`]) and AT-SPI2 over D-Bus on
//! Linux and the BSDs ([`atspi`]).

use std::fmt;
use std::time::Duration;

pub mod atspi;
pub mod cocoa;
pub mod uia;
pub mod win32;
pub mod x11;

#[cfg(target_os = "macos")]
mod cocoa_window;
#[cfg(windows)]
mod win32_window;

#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
mod x11_window;

#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
mod atspi_bridge;

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

/// What an element of the screen is, in the generic roles of `docs/ux/first-screen.md`'s
/// screen-reader table; each platform bridge maps them to its own (NSAccessibility, UI
/// Automation, AT-SPI2, ARIA).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessRole {
    /// The window itself; the operating system already exposes it, so bridges skip it and
    /// attach its children to the window's content.
    Window,
    /// A group of related elements.
    Group,
    /// A level-one heading.
    Heading,
    /// A list of tabs (a segmented control).
    TabList,
    /// One tab of a tab list.
    Tab,
    /// The panel a tab controls.
    TabPanel,
    /// Text announced politely when it appears, without moving the focus.
    Status,
    /// A single-line editable text field.
    TextField,
    /// A push button.
    Button,
    /// A checkbox.
    CheckBox,
    /// A link.
    Link,
}

/// Rectangle of an accessibility node in logical pixels, from the window's top-left corner.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AccessRect {
    /// The left edge.
    pub x: f32,
    /// The top edge.
    pub y: f32,
    /// The width.
    pub width: f32,
    /// The height.
    pub height: f32,
}

/// The states a screen reader reports for a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AccessState {
    /// The node has the keyboard focus.
    pub focused: bool,
    /// For a checkbox, whether it is checked; `None` for nodes that cannot be checked.
    pub checked: Option<bool>,
    /// The node is shown but cannot be used or focused.
    pub disabled: bool,
    /// For a tab, whether it is the active one.
    pub selected: bool,
    /// The button `Enter` activates.
    pub default_button: bool,
}

/// One node of a window's accessibility tree: the list is in reading order, and a node's
/// parent comes before it.
#[derive(Debug, Clone, PartialEq)]
pub struct AccessNode {
    /// An identifier that stays the same for the same element from frame to frame.
    pub id: u32,
    /// The parent node's id, `None` for the root.
    pub parent: Option<u32>,
    /// What the node is.
    pub role: AccessRole,
    /// The visible text that names it.
    pub name: String,
    /// The value of a text field, when the node has one.
    pub value: Option<String>,
    /// The hint shown in an empty text field.
    pub placeholder: Option<String>,
    /// Focus, checked, disabled and selected states.
    pub state: AccessState,
    /// Where it is, in logical pixels.
    pub bounds: AccessRect,
}

/// The id of the node with the keyboard focus, if any.
pub fn focused_node(nodes: &[AccessNode]) -> Option<u32> {
    nodes
        .iter()
        .find(|node| node.state.focused)
        .map(|node| node.id)
}

/// Whether two trees have the same nodes (ids, roles and parents) in the same order, so a
/// bridge can update its platform objects in place instead of building them again.
pub fn same_access_structure(old: &[AccessNode], new: &[AccessNode]) -> bool {
    old.len() == new.len()
        && old
            .iter()
            .zip(new)
            .all(|(old, new)| old.id == new.id && old.role == new.role && old.parent == new.parent)
}

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

    /// Gives the operating system's screen readers the window's current accessibility tree,
    /// raising the platform's focus and value-change events for what changed since the last
    /// call. Backends without an accessibility bridge yet ignore it.
    fn set_accessibility_tree(&mut self, nodes: &[AccessNode]) {
        let _ = nodes;
    }
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
        let window = win32_window::Win32Window::open(options)?;
        Ok(Box::new(window))
    }
    #[cfg(target_os = "macos")]
    {
        let window = cocoa_window::CocoaWindow::open(options)?;
        Ok(Box::new(window))
    }
    #[cfg(all(target_vendor = "apple", not(target_os = "macos")))]
    {
        let _ = options;
        Err(WindowError::Unsupported(
            "native windows on iOS, tvOS, watchOS and visionOS are not supported yet",
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

    fn node(id: u32, parent: Option<u32>, role: AccessRole, focused: bool) -> AccessNode {
        AccessNode {
            id,
            parent,
            role,
            name: format!("node {id}"),
            value: None,
            placeholder: None,
            state: AccessState {
                focused,
                ..AccessState::default()
            },
            bounds: AccessRect::default(),
        }
    }

    #[test]
    fn the_focused_node_and_an_unchanged_structure_are_found() {
        let tree = vec![
            node(1, None, AccessRole::Window, false),
            node(2, Some(1), AccessRole::Button, true),
        ];
        assert_eq!(focused_node(&tree), Some(2));
        assert_eq!(focused_node(&tree[..1]), None);
        let mut renamed = tree.clone();
        renamed[1].name = "other".to_owned();
        renamed[1].state.focused = false;
        assert!(same_access_structure(&tree, &renamed));
        let mut moved = tree.clone();
        moved[1].parent = None;
        assert!(!same_access_structure(&tree, &moved));
        assert!(!same_access_structure(&tree, &tree[..1]));
        let mut retyped = tree.clone();
        retyped[1].role = AccessRole::Link;
        assert!(!same_access_structure(&tree, &retyped));
    }

    #[cfg(not(any(windows, target_os = "macos")))]
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
