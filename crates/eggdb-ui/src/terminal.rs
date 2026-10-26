use crate::frame::Frame;
use std::io;
use std::time::Duration;

/// A terminal's size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Size {
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
}

/// What a terminal can show, resolved once at start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Capabilities {
    /// Which colours it shows.
    pub colors: Colors,
    /// Whether it shows bold, inverse, dim and underline.
    pub attributes: bool,
    /// Whether it shows characters beyond ASCII, such as box lines and arrows.
    pub unicode: bool,
}

impl Capabilities {
    /// Every capability: the 16 ANSI colours, attributes and Unicode.
    pub const FULL: Self = Self {
        colors: Colors::Ansi16,
        attributes: true,
        unicode: true,
    };
}

/// The colours a terminal shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Colors {
    /// None: attributes only, or nothing at all.
    None,
    /// The 16 ANSI colours, which follow the terminal's own theme.
    Ansi16,
}

/// Something that happened at the terminal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Event {
    /// A key was pressed.
    Key(KeyEvent),
    /// Text was pasted, line breaks included; it never triggers key bindings.
    Paste(String),
    /// A paste larger than the input layer takes (1 MiB) was dropped whole; none of it arrives.
    PasteTooLarge,
    /// The terminal changed size.
    Resize(Size),
    /// The terminal is going away; the program should end at once.
    Interrupt,
}

/// One key press with its modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyEvent {
    /// The key.
    pub code: KeyCode,
    /// The modifiers held with it.
    pub modifiers: Modifiers,
}

impl KeyEvent {
    /// A key pressed with no modifiers.
    pub const fn plain(code: KeyCode) -> Self {
        Self {
            code,
            modifiers: Modifiers::NONE,
        }
    }
}

/// A key on the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    /// A character key.
    Char(char),
    /// Enter or Return.
    Enter,
    /// Escape.
    Esc,
    /// Tab.
    Tab,
    /// Shift-Tab.
    BackTab,
    /// Backspace.
    Backspace,
    /// Delete.
    Delete,
    /// Insert.
    Insert,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// A function key, F1 to F12.
    F(u8),
}

/// The modifier keys held with a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers {
    /// Control.
    pub ctrl: bool,
    /// Alt (Option on a Mac keyboard).
    pub alt: bool,
    /// Shift.
    pub shift: bool,
}

impl Modifiers {
    /// No modifier.
    pub const NONE: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
    };
    /// Control alone.
    pub const CTRL: Self = Self {
        ctrl: true,
        alt: false,
        shift: false,
    };
    /// Alt alone.
    pub const ALT: Self = Self {
        ctrl: false,
        alt: true,
        shift: false,
    };
}

/// A terminal the UI draws on: events in, a grid of cells out. Implemented for real terminals
/// by [`crate::OsTerminal`] and for tests by [`crate::FakeTerminal`].
pub trait Terminal {
    /// The terminal's size now.
    fn size(&self) -> Size;
    /// What it can show.
    fn capabilities(&self) -> Capabilities;
    /// The next event, waiting at most `timeout`; `None` when nothing happened in that time.
    fn next_event(&mut self, timeout: Duration) -> io::Result<Option<Event>>;
    /// Shows `frame`.
    fn present(&mut self, frame: &Frame) -> io::Result<()>;

    /// Whether a bracketed paste has begun and its end marker has not arrived, so a pause in
    /// the input is a pause inside the paste, not a quiet keyboard.
    fn paste_open(&self) -> bool {
        false
    }
}
