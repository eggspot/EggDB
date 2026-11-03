//! Native windows for EggDB's GUI (D8, D191, D256): open a window of a logical size,
//! present RGBA frames into it, read its keyboard, pointer, resize, scale and close events,
//! and put text on and read it from the system clipboard.
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
use std::path::PathBuf;
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
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// The F2 function key.
    F2,
    /// The F4 function key.
    F4,
    /// The F5 function key.
    F5,
    /// The F10 function key (`Shift+F10` opens a context menu).
    F10,
    /// The menu key (the application or context-menu key), which opens a context menu.
    ContextMenu,
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
#[derive(Debug, Clone, PartialEq)]
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
    /// A drag carrying these files came over the window. Backends that cannot see a drag
    /// before its drop never send it.
    FileDragEntered(Vec<PathBuf>),
    /// The drag that [`WindowEvent::FileDragEntered`] announced left the window without a drop.
    FileDragLeft,
    /// Files were dropped onto the window, in the order the system listed them.
    FilesDropped(Vec<PathBuf>),
    /// A screen reader asked a node of the accessibility tree to do something.
    AccessRequested(AccessRequest),
    /// The pointer moved to (`x`, `y`) device pixels from the drawable area's top-left
    /// corner, with or without a button held.
    PointerMoved {
        /// The horizontal position in device pixels.
        x: f32,
        /// The vertical position in device pixels, growing downwards.
        y: f32,
    },
    /// A pointer button went down at (`x`, `y`) device pixels.
    PointerPressed {
        /// The horizontal position in device pixels.
        x: f32,
        /// The vertical position in device pixels.
        y: f32,
        /// The button.
        button: PointerButton,
        /// The modifier keys held.
        modifiers: Modifiers,
    },
    /// A pointer button went up at (`x`, `y`) device pixels, inside the window or, while
    /// the button was held since a press inside it, outside it.
    PointerReleased {
        /// The horizontal position in device pixels.
        x: f32,
        /// The vertical position in device pixels.
        y: f32,
        /// The button.
        button: PointerButton,
        /// The modifier keys held.
        modifiers: Modifiers,
    },
    /// The wheel or the touchpad scrolled at (`x`, `y`) device pixels, by lines, one line a
    /// wheel notch on every platform, a touchpad's travel in fractions of one; the client
    /// decides how far a line scrolls: positive `lines_down` asks to see what is further
    /// down, positive `lines_right` what is further right (already turned the way the user's
    /// scrolling direction setting says).
    Wheel {
        /// The horizontal position in device pixels.
        x: f32,
        /// The vertical position in device pixels.
        y: f32,
        /// Lines to scroll down (negative: up).
        lines_down: f32,
        /// Lines to scroll right (negative: left).
        lines_right: f32,
        /// The modifier keys held.
        modifiers: Modifiers,
    },
    /// The pointer left the window. Backends that cannot tell never send it.
    PointerLeft,
}

/// A pointer button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    /// The primary button: the left one, or the only one of a touchpad.
    Primary,
    /// The secondary button: the right one, or a two-finger click.
    Secondary,
    /// The middle button or wheel click.
    Middle,
}

/// The most text, in bytes of UTF-8, the platform layer reads from or puts on the system
/// clipboard; longer text is refused with [`WindowError::InvalidInput`].
pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 8 << 20;

/// Refuses `text` when it is longer than [`MAX_CLIPBOARD_TEXT_BYTES`].
pub fn check_clipboard_text(text: &str) -> Result<(), WindowError> {
    if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
        Err(WindowError::InvalidInput(format!(
            "{} bytes is more text than the clipboard takes ({MAX_CLIPBOARD_TEXT_BYTES} at most)",
            text.len()
        )))
    } else {
        Ok(())
    }
}

/// One choice of an open-file dialog's file-type menu: its name and the patterns it shows,
/// such as `EggDB databases` with `*.eggdb`, or `All files` with `*.*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFilter {
    /// The name the menu shows.
    pub name: String,
    /// The file name patterns, such as `*.eggdb`.
    pub patterns: Vec<String>,
}

/// What an open-file dialog asks for ([`Window::choose_file_to_open`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OpenFileDialog {
    /// The directory the dialog starts in; the system's choice when `None`.
    pub start_in: Option<PathBuf>,
    /// The file-type menu, the first one selected. Systems whose dialog has no such menu
    /// (macOS's open panel) show every file.
    pub filters: Vec<FileFilter>,
}

/// Whether this system has an open-file dialog that [`Window::choose_file_to_open`] shows:
/// macOS (`NSOpenPanel`) and Windows (the common file dialog). Linux, the BSDs and the browser
/// have none in the platform layer yet.
pub const fn has_file_dialog() -> bool {
    cfg!(any(target_os = "macos", windows))
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
    /// A toolbar holding buttons.
    Toolbar,
    /// A button that shows a value and opens a menu of choices (the dialect pill).
    ComboBox,
    /// A multi-line editable text area (the query editor).
    TextArea,
    /// Text announced at once when it appears (the editor's error strip).
    Alert,
    /// A tree of items (the object tree, the plan).
    Tree,
    /// One item of a tree, with its level, position and set size.
    TreeItem,
    /// A grid of cells with a row and column count (the results grid).
    Grid,
    /// A column's header cell in a grid.
    ColumnHeader,
    /// One cell of a grid, with its row and column index.
    Cell,
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
    /// For a tree item or a disclosure, whether it is expanded; `None` for a leaf or a node
    /// that cannot expand.
    pub expanded: Option<bool>,
    /// The value is in error (a field the failure is about, the editor's error range).
    pub invalid: bool,
    /// The value can be read but not changed (the DDL box).
    pub read_only: bool,
    /// The value is a password, given masked: bridges flag it as one (UIA `IsPassword`,
    /// AT-SPI's password-text role, the `AXSecureTextField` subrole) and take only a whole
    /// new value for it, never an edit of part of the mask.
    pub password: bool,
}

/// What a screen reader may ask a node to do; each bridge offers only these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AccessActions {
    /// Activate it, as `Enter` or `Space` on a button does.
    pub invoke: bool,
    /// Flip it, as `Space` on a checkbox does.
    pub toggle: bool,
    /// Expand or collapse it, as `Right` and `Left` on a tree item do.
    pub expand_collapse: bool,
    /// Replace its text.
    pub set_value: bool,
    /// Move the keyboard focus to it.
    pub focus: bool,
    /// Make it the selected one of its container (a tab, a tree item, a cell).
    pub select: bool,
}

/// The caret and selection of a text node's value, in characters from the start: the
/// selection runs between `anchor` and `caret`, and is empty when they are equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AccessCaret {
    /// Where the selection started.
    pub anchor: usize,
    /// Where the caret is.
    pub caret: usize,
}

impl AccessCaret {
    /// The selected characters, start before end.
    pub fn range(self) -> (usize, usize) {
        (self.anchor.min(self.caret), self.anchor.max(self.caret))
    }
}

/// The size of a grid or tree: every row and column it has, not only the visible ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AccessTable {
    /// The number of rows.
    pub rows: usize,
    /// The number of columns.
    pub columns: usize,
}

/// Where a cell or column header is in its grid, from zero; a header's row is `0` and its
/// cells' rows count from `1` in the bridges that put headers in the first row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AccessCell {
    /// The row, from zero among the data rows.
    pub row: usize,
    /// The column, from zero.
    pub column: usize,
}

/// Where a tree item is: its level from one, its position among its siblings from one, and
/// how many siblings (itself included) it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AccessTreePosition {
    /// The level, `1` for a top-level item.
    pub level: usize,
    /// The position among its siblings, from one.
    pub position: usize,
    /// The number of siblings, itself included.
    pub set_size: usize,
}

/// What a screen reader asked one node to do ([`WindowEvent::AccessRequested`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessAction {
    /// Move the keyboard focus to the node.
    Focus,
    /// Activate it.
    Invoke,
    /// Flip it.
    Toggle,
    /// Expand it.
    Expand,
    /// Collapse it.
    Collapse,
    /// Make it the selected one of its container.
    Select,
    /// Replace its text with this.
    SetValue(String),
    /// Put the caret at `caret` with the selection from `anchor`, in characters.
    SetCaret(AccessCaret),
}

/// A screen reader's request: the node and what to do with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessRequest {
    /// The node's id.
    pub node: u32,
    /// What to do.
    pub action: AccessAction,
}

/// The longest text a [`AccessAction::SetValue`] carries; bridges refuse longer values.
pub const MAX_ACCESS_VALUE_CHARS: usize = 65_536;

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
    /// Secondary text read after the name (a tree row's `2.1M rows`, a plan node's costs).
    pub description: Option<String>,
    /// The caret and selection in `value`, for a text node.
    pub caret: Option<AccessCaret>,
    /// What a screen reader may ask it to do.
    pub actions: AccessActions,
    /// For a grid or tree, its size.
    pub table: Option<AccessTable>,
    /// For a cell or column header, where it is in its grid.
    pub cell: Option<AccessCell>,
    /// For a tree item, its level and position.
    pub tree_position: Option<AccessTreePosition>,
    /// The shortcut that does what invoking it does, as the platform writes it (`Ctrl+Enter`).
    pub shortcut: Option<String>,
}

impl Default for AccessNode {
    fn default() -> Self {
        Self {
            id: 0,
            parent: None,
            role: AccessRole::Group,
            name: String::new(),
            value: None,
            placeholder: None,
            state: AccessState::default(),
            bounds: AccessRect::default(),
            description: None,
            caret: None,
            actions: AccessActions::default(),
            table: None,
            cell: None,
            tree_position: None,
            shortcut: None,
        }
    }
}

impl AccessNode {
    /// Whether the node can be asked to do `action`.
    pub fn allows(&self, action: &AccessAction) -> bool {
        let actions = self.actions;
        if self.state.disabled {
            return false;
        }
        match action {
            AccessAction::Focus => actions.focus,
            AccessAction::Invoke => actions.invoke,
            AccessAction::Toggle => actions.toggle,
            AccessAction::Expand | AccessAction::Collapse => actions.expand_collapse,
            AccessAction::Select => actions.select,
            AccessAction::SetValue(text) => {
                actions.set_value
                    && !self.state.read_only
                    && text.chars().count() <= MAX_ACCESS_VALUE_CHARS
            }
            AccessAction::SetCaret(_) => self.caret.is_some(),
        }
    }
}

/// The request for `action` on node `id` of `nodes`, `None` when there is no such node or it
/// does not offer that action; bridges call it before they queue a request.
pub fn access_request(
    nodes: &[AccessNode],
    id: u32,
    action: AccessAction,
) -> Option<AccessRequest> {
    nodes
        .iter()
        .find(|node| node.id == id)
        .filter(|node| node.allows(&action))
        .map(|_| AccessRequest { node: id, action })
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

    /// Shows the system's open-file dialog over this window and waits for it: the chosen
    /// file, or `None` when the user cancelled. Systems without one ([`has_file_dialog`]
    /// is `false`) answer [`WindowError::Unsupported`].
    fn choose_file_to_open(
        &mut self,
        dialog: &OpenFileDialog,
    ) -> Result<Option<PathBuf>, WindowError> {
        let _ = dialog;
        Err(WindowError::Unsupported(
            "this system has no open-file dialog in the platform layer yet",
        ))
    }

    /// Replaces the system clipboard's contents with `text`, as plain Unicode text. Backends
    /// without a clipboard answer [`WindowError::Unsupported`].
    fn put_clipboard_text(&mut self, text: &str) -> Result<(), WindowError> {
        let _ = text;
        Err(WindowError::Unsupported(
            "this system has no clipboard in the platform layer yet",
        ))
    }

    /// The system clipboard's text, `None` when it holds no text. Backends without a
    /// clipboard answer [`WindowError::Unsupported`].
    fn clipboard_text(&mut self) -> Result<Option<String>, WindowError> {
        Err(WindowError::Unsupported(
            "this system has no clipboard in the platform layer yet",
        ))
    }

    /// Whether the user asked the system to reduce motion, read now: macOS's Reduce Motion,
    /// Windows' "Animation effects" turned off, or an X11 desktop's XSETTINGS
    /// `Gtk/EnableAnimations` set to 0. Backends that cannot tell answer `false`.
    fn reduced_motion(&mut self) -> bool {
        false
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
    fn the_file_dialog_exists_exactly_on_macos_and_windows() {
        assert_eq!(
            has_file_dialog(),
            cfg!(target_os = "macos") || cfg!(windows)
        );
    }

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
            ..AccessNode::default()
        }
    }

    #[test]
    fn requests_are_made_only_for_actions_a_node_offers() {
        let mut field = node(3, Some(1), AccessRole::TextField, false);
        field.actions = AccessActions {
            set_value: true,
            focus: true,
            ..AccessActions::default()
        };
        let tree = vec![node(1, None, AccessRole::Window, false), field];
        let set = AccessAction::SetValue("shop.eggdb".to_owned());
        assert_eq!(
            access_request(&tree, 3, set.clone()),
            Some(AccessRequest {
                node: 3,
                action: set.clone()
            })
        );
        assert_eq!(access_request(&tree, 3, AccessAction::Invoke), None);
        assert_eq!(access_request(&tree, 9, AccessAction::Focus), None);
        assert_eq!(
            access_request(&tree, 3, AccessAction::SetCaret(AccessCaret::default())),
            None
        );
        let long = AccessAction::SetValue("x".repeat(MAX_ACCESS_VALUE_CHARS + 1));
        assert_eq!(access_request(&tree, 3, long), None);
        let mut read_only = tree.clone();
        read_only[1].state.read_only = true;
        assert_eq!(access_request(&read_only, 3, set.clone()), None);
        assert!(read_only[1].allows(&AccessAction::Focus));
        read_only[1].state.disabled = true;
        assert!(!read_only[1].allows(&AccessAction::Focus));
        assert_eq!(
            AccessCaret {
                anchor: 7,
                caret: 2
            }
            .range(),
            (2, 7)
        );
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

    #[test]
    fn clipboard_text_up_to_the_limit_is_taken_and_longer_text_refused() {
        assert!(check_clipboard_text("SELECT 1;").is_ok());
        assert!(check_clipboard_text(&"x".repeat(MAX_CLIPBOARD_TEXT_BYTES)).is_ok());
        assert!(matches!(
            check_clipboard_text(&"x".repeat(MAX_CLIPBOARD_TEXT_BYTES + 1)),
            Err(WindowError::InvalidInput(_))
        ));
    }
}
