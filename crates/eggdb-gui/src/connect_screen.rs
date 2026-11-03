use std::path::{Path, PathBuf};

use eggdb_platform::window::{FileFilter, Key, KeyPress, MAX_ACCESS_VALUE_CHARS, OpenFileDialog};

use crate::canvas::{Canvas, Rect};
use crate::color::{Rgba, Theme};
use crate::connection::{
    ConnectFailure, ConnectTarget, FailureField, FileTarget, resolve_typed_path,
};
use crate::font::{TextStyle, Typeface};
use crate::layout::Layout;
use crate::layout_picker::LayoutPicker;
use crate::main_window::Faces;
use crate::pointer::{self, CaretLook, PointerLook};

mod accessibility;
mod server_segment;

pub use accessibility::{
    NODE_BROWSE, NODE_CANCEL, NODE_CARD, NODE_CONNECT_TO_A_SERVER, NODE_CREATE_IF_MISSING,
    NODE_ERROR, NODE_FILE, NODE_OPEN, NODE_OPEN_A_FILE, NODE_PANEL, NODE_READ_ONLY, NODE_SAMPLE,
    NODE_SEGMENTS, NODE_STATUS_LINE, NODE_TITLE, NODE_WHERE_FROM, NODE_WINDOW, accessibility_tree,
    error_announcement,
};
pub use accessibility::{
    NODE_DATABASE, NODE_HOST, NODE_PASSWORD, NODE_PORT, NODE_PROTOCOL, NODE_TLS_WARNING, NODE_URL,
    NODE_USE_TLS, NODE_USER,
};
pub use server_segment::{
    DEFAULT_HOST, DEFAULT_SERVER_USER, Protocol, Segment, ServerForm, ServerUrl, TLS_WARNING,
    URL_PLACEHOLDER, parse_server_url,
};

/// The window's default width in logical pixels.
pub const DEFAULT_WIDTH: u32 = 1280;
/// The window's default height in logical pixels.
pub const DEFAULT_HEIGHT: u32 = 800;
/// The narrowest the window may be, in logical pixels.
pub const MIN_WIDTH: u32 = 800;
/// The shortest the window may be: the card stack is never clipped.
pub const MIN_HEIGHT: u32 = 560;
/// The window's title.
pub const TITLE: &str = "EggDB";
/// The file the `Open a file` segment proposes at first start.
pub const QUICKSTART_FILE: &str = "~/eggdb/quickstart.eggdb";

const STACK_WIDTH: f32 = 560.0;
const STACK_HEIGHT: f32 = 520.0;
const CARD: Rect = Rect::new(0.0, 0.0, 560.0, 480.0);
const SEGMENTS: Rect = Rect::new(24.0, 64.0, 512.0, 32.0);
const OPEN_A_FILE: Rect = Rect::new(24.0, 64.0, 256.0, 32.0);
const CONNECT_TO_A_SERVER: Rect = Rect::new(280.0, 64.0, 256.0, 32.0);
const SEGMENT_LABEL_TOP: f32 = 71.0;
const ERROR_TOP: f32 = 100.0;
const FILE_LABEL: (f32, f32) = (24.0, 120.0);
const FILE_INPUT: Rect = Rect::new(24.0, 142.0, 420.0, 28.0);
const FILE_INPUT_WITHOUT_BROWSE: Rect = Rect::new(24.0, 142.0, 512.0, 28.0);
const STATUS_LINE: (f32, f32) = (24.0, 440.0);
const STATUS_LINE_WIDTH: f32 = 332.0;
const DROP_HINT: (f32, f32) = (400.0, 28.0);
const DROP_RING_WIDTH: f32 = 2.0;
const ERROR_LINE_HEIGHT: f32 = 18.0;
const PROGRESS_BAR: (f32, f32) = (120.0, 2.0);
const INPUT_TEXT_INSET: f32 = 8.0;
const INPUT_TEXT_TOP: f32 = 5.0;
const CARET_TOP: f32 = 4.0;
const BROWSE: Rect = Rect::new(452.0, 142.0, 84.0, 28.0);
const CREATE_BOX: Rect = Rect::new(24.0, 186.0, 16.0, 16.0);
const READ_ONLY_BOX: Rect = Rect::new(24.0, 210.0, 16.0, 16.0);
const CHECK_LABEL_X: f32 = 48.0;
const CANCEL: Rect = Rect::new(364.0, 436.0, 84.0, 28.0);
const OPEN: Rect = Rect::new(456.0, 436.0, 80.0, 28.0);
const LINKS_TOP: f32 = 499.0;
const LINK_GAP: f32 = 24.0;
const LINK_FOCUS_WIDEN: f32 = 4.0;
const RADIUS_CONTROL: f32 = 4.0;
const RADIUS_CARD: f32 = 8.0;
const FOCUS_GAP: f32 = 2.0;
const HEADLINE: (f32, f32) = (20.0, 26.0);
const BODY: (f32, f32) = (13.0, 18.0);
const SMALL: (f32, f32) = (11.0, 14.0);
const LAYOUT_LINE_RIGHT: f32 = 536.0;
const LAYOUT_LINE_TOP: f32 = 30.0;

const OPEN_A_FILE_LABEL: &str = "Open a file";
const CONNECT_TO_A_SERVER_LABEL: &str = "Connect to a server";
const CREATE_LABEL: &str = "Create the file if it does not exist";
const READ_ONLY_LABEL: &str = "Open read-only";
const PLACEHOLDER: &str = "/path/to/shop.eggdb";
const DROP_HINT_LABEL: &str = "Drop to open";
const WHERE_FROM_LABEL: &str = "Where are you coming from?";
const SAMPLE_LABEL: &str = "Load a sample dataset";

/// The element of the connect card with the keyboard focus, in `docs/ux/first-screen.md`'s
/// order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The segmented control (one stop: the active segment).
    Segments,
    /// The File input.
    FileInput,
    /// `Browse…`.
    Browse,
    /// The checkbox `Create the file if it does not exist`.
    CreateIfMissing,
    /// The checkbox `Open read-only`.
    ReadOnly,
    /// `Cancel`, only while it is enabled.
    Cancel,
    /// `Open`, the default button (`Connect` on the server segment).
    Open,
    /// The server segment's `URL` input.
    Url,
    /// The protocol menu.
    Protocol,
    /// The Host input.
    Host,
    /// The Port input.
    Port,
    /// The Database input.
    Database,
    /// The User input.
    User,
    /// The Password input.
    Password,
    /// The checkbox `Use TLS`.
    UseTls,
    /// The link `Where are you coming from?`.
    WhereFromLink,
    /// The link `Load a sample dataset`.
    SampleLink,
}

/// What a key asks the application to do beyond changing the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectAction {
    /// Nothing beyond the card's own change.
    None,
    /// Open (or create) the file in the input.
    Open,
    /// Choose the file with the OS's file dialog.
    Browse,
    /// Go back to where the user came from.
    Cancel,
    /// Show the layout picker behind `Where are you coming from?`.
    ShowLayoutPicker,
    /// Put the focused input's selected text on the clipboard.
    CopySelection,
    /// Read the clipboard and give its text to [`ConnectCard::paste`].
    Paste,
    /// Load the `shop` sample dataset.
    LoadSampleDataset,
    /// Stop the open or connect in progress (`Esc`, or `Cancel` while it runs).
    StopOpening,
    /// Quit the client.
    Quit,
}

/// A single-line text input: its value, caret and selection, counted in characters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TextField {
    value: String,
    caret: usize,
    anchor: Option<usize>,
}

impl TextField {
    /// A field holding `value` with all of it selected, so typing replaces it.
    pub fn selected(value: &str) -> Self {
        let length = value.chars().count();
        Self {
            value: value.to_owned(),
            caret: length,
            anchor: Some(0),
        }
    }

    /// The text.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// The caret, in characters from the start.
    pub fn caret(&self) -> usize {
        self.caret
    }

    /// The selected characters, start before end, when there are any.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        (anchor != self.caret).then(|| (anchor.min(self.caret), anchor.max(self.caret)))
    }

    fn length(&self) -> usize {
        self.value.chars().count()
    }

    fn byte_at(&self, column: usize) -> usize {
        self.value
            .char_indices()
            .nth(column)
            .map_or(self.value.len(), |(at, _)| at)
    }

    fn prefix(&self, column: usize) -> &str {
        self.value.get(..self.byte_at(column)).unwrap_or_default()
    }

    fn slice(&self, start: usize, end: usize) -> &str {
        self.value
            .get(self.byte_at(start)..self.byte_at(end))
            .unwrap_or_default()
    }

    fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            self.anchor = None;
            return false;
        };
        let (from, to) = (self.byte_at(start), self.byte_at(end));
        self.value.replace_range(from..to, "");
        self.caret = start;
        self.anchor = None;
        true
    }

    fn insert(&mut self, character: char) {
        self.delete_selection();
        let at = self.byte_at(self.caret);
        self.value.insert(at, character);
        self.caret += 1;
    }

    fn move_to(&mut self, column: usize, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.caret);
        } else {
            self.anchor = None;
        }
        self.caret = column.min(self.length());
    }

    fn clear_selection(&mut self) {
        self.anchor = None;
    }

    /// Puts the caret before character `column` (clamped), extending the selection from its
    /// start, or from the caret, when `extend` is set: a click, or a drag or `Shift`+click.
    pub fn place_caret(&mut self, column: usize, extend: bool) {
        self.move_to(column, extend);
    }

    /// Selects the word around `column`, as a double-click does.
    pub fn select_word(&mut self, column: usize) {
        let (start, end) = pointer::word_at(&self.value, column);
        self.anchor = Some(start);
        self.caret = end;
    }

    /// The selected text, `None` when nothing is selected.
    pub fn selected_text(&self) -> Option<&str> {
        self.selection().map(|(start, end)| self.slice(start, end))
    }

    /// Inserts `text` at the caret, replacing the selection: a paste, already made one line.
    pub fn paste(&mut self, text: &str) {
        self.delete_selection();
        let at = self.byte_at(self.caret);
        self.value.insert_str(at, text);
        self.caret += text.chars().count();
    }

    /// Selects the whole value, as when a failure points at this field.
    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.caret = self.length();
    }

    /// Replaces the value with `value`, nothing selected and the caret at the end.
    pub fn set(&mut self, value: &str) {
        self.value = value.to_owned();
        self.caret = self.length();
        self.anchor = None;
    }

    fn apply(&mut self, press: KeyPress) {
        let modifiers = press.modifiers;
        let primary = modifiers.control || modifiers.command;
        match press.key {
            Key::Character('a' | 'A') if primary => self.select_all(),
            Key::Character(character) if !primary => self.insert(character),
            Key::Space if !primary => self.insert(' '),
            Key::Backspace => {
                if !self.delete_selection() && self.caret > 0 {
                    self.anchor = Some(self.caret - 1);
                    self.delete_selection();
                }
            }
            Key::Delete => {
                if !self.delete_selection() && self.caret < self.length() {
                    self.anchor = Some(self.caret + 1);
                    self.delete_selection();
                }
            }
            Key::Left => {
                let to = match (self.selection(), modifiers.shift) {
                    (Some((start, _)), false) => start,
                    _ => self.caret.saturating_sub(1),
                };
                self.move_to(to, modifiers.shift);
            }
            Key::Right => {
                let to = match (self.selection(), modifiers.shift) {
                    (Some((_, end)), false) => end,
                    _ => self.caret + 1,
                };
                self.move_to(to, modifiers.shift);
            }
            Key::Home | Key::Up => self.move_to(0, modifiers.shift),
            Key::End | Key::Down => self.move_to(self.length(), modifiers.shift),
            _ => {}
        }
    }
}

/// The `Error` line under the segments, with its `Try:` line, as shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardError {
    /// The first line: `Error: cannot open …`, or `Error 28P01: …` with the engine's code.
    pub line: String,
    /// The second line, `Try: …`, when the engine has a fix.
    pub fix: Option<String>,
}

impl CardError {
    /// The lines in order: the `Error` line, then the `Try:` line when there is one.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.line.as_str()).chain(self.fix.as_deref())
    }

    /// The lines of `failure`.
    pub fn from_failure(failure: &ConnectFailure) -> Self {
        Self {
            line: failure.error_line(),
            fix: failure.try_line(),
        }
    }
}

/// A path a drop or the open-file dialog gave, kept exactly as the system returned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChosenFile {
    /// The path, never trimmed, `~`-expanded or turned into text and back.
    pub path: PathBuf,
    /// The File input's text it shows as; any other text there is typed, and resolved.
    pub shown: String,
}

/// The connect card's state (`docs/ux/first-screen.md`): the `Open a file` segment's fields
/// and which element has the keyboard focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectCard {
    /// The File input.
    pub file: TextField,
    /// `Create the file if it does not exist`.
    pub create_if_missing: bool,
    /// `Open read-only`.
    pub read_only: bool,
    /// Whether `Cancel` is enabled (there is somewhere to return to).
    pub can_cancel: bool,
    /// The `Error:` line, when a failure is shown.
    pub error: Option<CardError>,
    /// The element with the keyboard focus.
    pub focus: Focus,
    /// Whether this system has an open-file dialog: without one `Browse…` is not drawn and
    /// the File input is 512 px wide.
    pub file_dialog: bool,
    /// Whether an open is running: the card takes only `Esc` until it ends.
    pub busy: bool,
    /// The progress status line (`Opening shop.eggdb…`), drawn once the open has run for
    /// 120 ms; while it is shown the card is drawn disabled.
    pub progress: Option<String>,
    /// A status line after an open ended without one (`Cancelled`, `Opened the first of 3
    /// files`).
    pub status: Option<String>,
    /// Whether a drag of a file is over the window: the card's border turns `accent`.
    pub drag_over: bool,
    /// The segment shown below the segmented control.
    pub segment: Segment,
    /// The `Connect to a server` segment's fields.
    pub server: ServerForm,
    /// The field the shown failure is about: it has the `error` border.
    pub failed_field: Option<FailureField>,
    /// The pointer's hover, press and whether it moved the focus last.
    pub pointer: PointerLook,
    /// How the focused input's caret draws at this moment of its blink.
    pub caret: CaretLook,
    /// The path the last drop or dialog put in the File input: it opens as it is, and is
    /// never created, while the input still shows it.
    pub chosen_file: Option<ChosenFile>,
    /// The layout the main window opens in: the card's title row names a familiar one.
    pub layout: Layout,
    /// The layout picker, while it is open over the card.
    pub layout_picker: Option<LayoutPicker>,
}

impl ConnectCard {
    /// The card at first start: the File input focused with the quickstart file selected,
    /// `Create the file if it does not exist` checked and `Cancel` disabled.
    pub fn first_start() -> Self {
        Self {
            file: TextField::selected(QUICKSTART_FILE),
            create_if_missing: true,
            read_only: false,
            can_cancel: false,
            error: None,
            focus: Focus::FileInput,
            file_dialog: true,
            busy: false,
            progress: None,
            status: None,
            drag_over: false,
            segment: Segment::File,
            server: ServerForm::new(),
            failed_field: None,
            pointer: PointerLook::default(),
            caret: CaretLook::On,
            chosen_file: None,
            layout: Layout::EggDb,
            layout_picker: None,
        }
    }

    /// The focused input's text, when the focus is on one.
    pub fn focused_field(&self) -> Option<&TextField> {
        match self.focus {
            Focus::FileInput => Some(&self.file),
            Focus::Url => Some(&self.server.url),
            Focus::Host => Some(&self.server.host),
            Focus::Port => Some(&self.server.port),
            Focus::Database => Some(&self.server.database),
            Focus::User => Some(&self.server.user),
            Focus::Password => Some(&self.server.password),
            _ => None,
        }
    }

    fn edit_focused_field(&mut self, edit: impl FnOnce(&mut TextField)) -> bool {
        match self.focus {
            Focus::FileInput => edit(&mut self.file),
            Focus::Url => {
                edit(&mut self.server.url);
                self.server.url_edited();
            }
            focus => match self.server.field_mut(focus) {
                Some(field) => {
                    edit(field);
                    self.server.field_edited();
                }
                None => return false,
            },
        }
        true
    }

    /// Inserts the clipboard's `text`, made one line, at the focused input's caret; returns
    /// whether an input had the focus.
    pub fn paste(&mut self, text: &str) -> bool {
        let line = pointer::single_line(text);
        self.edit_focused_field(|field| field.paste(&line))
    }

    /// The text `Ctrl/Cmd+C` copies: the focused input's selection, never the password's.
    pub fn copied_text(&self) -> Option<String> {
        if self.focus == Focus::Password {
            return None;
        }
        self.focused_field()
            .and_then(TextField::selected_text)
            .map(str::to_owned)
    }

    /// Whether the focus ring draws: always after a key, and after a click only on inputs
    /// (`docs/ux/workspace.md` 6.1).
    pub fn ring_shown(&self) -> bool {
        !self.pointer.focus_by_pointer || self.focused_field().is_some()
    }

    /// Places the focused input's caret for a press at `x` logical pixels in the input whose
    /// box starts at `box_left`: the nearest character boundary, the word on a double-click,
    /// the whole value on a triple-click, extending the selection when `extend` is set (a
    /// drag or `Shift`+click). Nothing happens when no input has the focus.
    pub fn place_caret_at(
        &mut self,
        faces: Faces<'_>,
        box_left: f32,
        x: f32,
        clicks: u8,
        extend: bool,
    ) {
        let masked = self.focus == Focus::Password;
        let Some(value) = self.focused_field().map(|field| field.value().to_owned()) else {
            return;
        };
        let into = x - box_left - INPUT_TEXT_INSET;
        let column = if masked {
            (into / server_segment::MASK_ADVANCE).round().max(0.0) as usize
        } else {
            pointer::column_at(faces.ui, BODY.0, &value, into)
        };
        let _ = self.edit_focused_field_quietly(|field| match clicks {
            2 if !masked => field.select_word(column),
            2 | 3 => field.select_all(),
            _ => field.place_caret(column, extend),
        });
    }

    fn edit_focused_field_quietly(&mut self, edit: impl FnOnce(&mut TextField)) -> bool {
        let field = match self.focus {
            Focus::FileInput => &mut self.file,
            Focus::Url => &mut self.server.url,
            focus => match self.server.field_mut(focus) {
                Some(field) => field,
                None => return false,
            },
        };
        edit(field);
        true
    }

    /// The file the card asks to open: a dropped or dialog-chosen path exactly as given and
    /// never created, otherwise the typed File input resolved against `home` and `current`
    /// (`docs/ux/workspace.md` 1.1), with the two checkboxes.
    pub fn file_target(&self, home: Option<&Path>, current: &Path) -> FileTarget {
        match self.shown_chosen_file() {
            Some(chosen) => FileTarget {
                path: chosen.to_path_buf(),
                create_if_missing: false,
                read_only: self.read_only,
            },
            None => FileTarget {
                path: resolve_typed_path(self.file.value(), home, current),
                create_if_missing: self.create_if_missing,
                read_only: self.read_only,
            },
        }
    }

    fn shown_chosen_file(&self) -> Option<&Path> {
        self.chosen_file
            .as_ref()
            .filter(|chosen| chosen.shown == self.file.value())
            .map(|chosen| chosen.path.as_path())
    }

    fn show_chosen_file(&mut self, path: &Path) {
        let shown = path.display().to_string();
        self.file.set(&shown);
        self.chosen_file = Some(ChosenFile {
            path: path.to_path_buf(),
            shown,
        });
    }

    /// Replaces the focused input's text with `text` in one edit, as a screen reader's
    /// set-value asks: control characters dropped, at most [`MAX_ACCESS_VALUE_CHARS`] kept.
    /// Returns whether an input took it; none does while an open runs.
    pub fn set_focused_value(&mut self, text: &str) -> bool {
        if self.busy {
            return false;
        }
        let line: String = text
            .chars()
            .filter(|character| !character.is_control())
            .take(MAX_ACCESS_VALUE_CHARS)
            .collect();
        self.edit_focused_field(|field| field.set(&line))
    }

    /// Starts an open: clears the error lines, and takes only `Esc` until
    /// [`ConnectCard::finish_with_failure`] or [`ConnectCard::stop_opening`].
    pub fn begin_opening(&mut self) {
        self.server.forget_url_password();
        self.error = None;
        self.failed_field = None;
        self.busy = true;
        self.progress = None;
    }

    /// Shows the progress status line for `target` once the open has run for 120 ms:
    /// `Opening shop.eggdb…`, or `Connecting to db.example.com:5432…` for a server.
    pub fn show_progress(&mut self, target: &ConnectTarget) {
        if !self.busy {
            return;
        }
        let text = match target {
            ConnectTarget::File(file) => {
                let name = file.path.file_name().map_or_else(
                    || file.path.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                );
                format!("Opening {name}…")
            }
            ConnectTarget::Server(options) => {
                format!("Connecting to {}:{}…", options.host, options.port)
            }
        };
        self.progress = Some(text);
    }

    /// Ends an open that failed: the card re-enables with the failure's two lines, and the
    /// field the failure is about takes the focus with its value selected.
    pub fn finish_with_failure(&mut self, failure: &ConnectFailure) {
        self.busy = false;
        self.progress = None;
        self.error = Some(CardError::from_failure(failure));
        self.failed_field = Some(failure.field);
        match server_segment::failure_focus(failure.field) {
            None => {
                self.segment = Segment::File;
                self.move_focus(Focus::FileInput);
                self.file.select_all();
            }
            Some(focus) => {
                self.segment = Segment::Server;
                self.move_focus(focus);
                if let Some(field) = self.server.field_mut(focus) {
                    field.select_all();
                }
            }
        }
    }

    /// The focus stop of the field the shown failure is about.
    pub fn failed_focus(&self) -> Option<Focus> {
        match self.failed_field? {
            FailureField::File => Some(Focus::FileInput),
            field => server_segment::failure_focus(field),
        }
    }

    /// Shows `segment`'s panel at once, as `Left` and `Right` on the segmented control do;
    /// the error lines of the other segment's attempt go.
    pub fn show_segment(&mut self, segment: Segment) {
        if self.segment != segment {
            self.server.forget_url_password();
            self.segment = segment;
            self.error = None;
            self.failed_field = None;
        }
    }

    fn edit_focused(&mut self, press: KeyPress) -> bool {
        match self.focus {
            Focus::FileInput => self.file.apply(press),
            Focus::Url => {
                self.server.url.apply(press);
                self.server.url_edited();
            }
            focus => match self.server.field_mut(focus) {
                Some(field) => {
                    field.apply(press);
                    self.server.field_edited();
                }
                None => return false,
            },
        }
        true
    }

    /// Ends an open the user stopped: the card re-enables, the status line reads `Cancelled`
    /// and the focus returns to the input.
    pub fn stop_opening(&mut self) {
        self.busy = false;
        self.progress = None;
        self.status = Some("Cancelled".to_owned());
        self.move_focus(match self.segment {
            Segment::File => Focus::FileInput,
            Segment::Server => Focus::Url,
        });
    }

    /// Takes the file the OS dialog returned: its path in the input, nothing selected, the
    /// caret at the end and the focus back in the input. `None` (cancelled) changes
    /// nothing and leaves the focus on `Browse…`.
    pub fn choose_file(&mut self, chosen: Option<&Path>) {
        match chosen {
            Some(path) => {
                self.show_chosen_file(path);
                self.move_focus(Focus::FileInput);
            }
            None => self.move_focus(Focus::Browse),
        }
    }

    /// What `Browse…` asks the OS dialog for: `*.eggdb` first and `All files` second,
    /// starting in the input's directory when it exists, otherwise in `home`.
    pub fn file_dialog_request(&self, home: Option<&Path>, current: &Path) -> OpenFileDialog {
        let typed = self.shown_chosen_file().map_or_else(
            || resolve_typed_path(self.file.value(), home, current),
            Path::to_path_buf,
        );
        let start_in = if typed.is_dir() {
            Some(typed)
        } else {
            typed
                .parent()
                .filter(|parent| parent.is_dir())
                .map(Path::to_path_buf)
                .or_else(|| home.map(Path::to_path_buf))
        };
        OpenFileDialog {
            start_in,
            filters: vec![
                FileFilter {
                    name: "EggDB databases".to_owned(),
                    patterns: vec!["*.eggdb".to_owned()],
                },
                FileFilter {
                    name: "All files".to_owned(),
                    patterns: vec!["*.*".to_owned()],
                },
            ],
        }
    }

    /// A drag came over the window carrying `paths`: the ring shows when the first is a file
    /// (not a directory).
    pub fn drag_entered(&mut self, paths: &[PathBuf]) {
        self.drag_over = !self.busy && paths.first().is_some_and(|path| !path.is_dir());
    }

    /// Takes dropped files: the first non-directory one goes into the input, to be opened at
    /// once ([`ConnectAction::Open`]); with several, the status line says which. A directory,
    /// nothing, or a drop during an open does nothing.
    pub fn drop_files(&mut self, paths: &[PathBuf]) -> ConnectAction {
        self.drag_over = false;
        let Some(first) = paths.first().filter(|path| !path.is_dir()) else {
            return ConnectAction::None;
        };
        if self.busy {
            return ConnectAction::None;
        }
        self.show_chosen_file(first);
        self.show_segment(Segment::File);
        self.move_focus(Focus::FileInput);
        self.status = None;
        if paths.len() > 1 {
            self.status = Some(format!("Opened the first of {} files", paths.len()));
        }
        ConnectAction::Open
    }

    /// Whether the card is drawn disabled: while the progress status line is shown.
    pub fn disabled(&self) -> bool {
        self.progress.is_some()
    }

    /// The `Tab` order: `Cancel` only while it is enabled.
    pub fn focus_order(&self) -> Vec<Focus> {
        let mut order = vec![Focus::Segments];
        match self.segment {
            Segment::File => {
                order.push(Focus::FileInput);
                if self.file_dialog {
                    order.push(Focus::Browse);
                }
                order.extend([Focus::CreateIfMissing, Focus::ReadOnly]);
            }
            Segment::Server => order.extend([
                Focus::Url,
                Focus::Protocol,
                Focus::Host,
                Focus::Port,
                Focus::Database,
                Focus::User,
                Focus::Password,
                Focus::UseTls,
            ]),
        }
        if self.can_cancel {
            order.push(Focus::Cancel);
        }
        order.extend([Focus::Open, Focus::WhereFromLink, Focus::SampleLink]);
        order
    }

    /// Moves the keyboard focus to `focus`, by key, click or screen reader alike: leaving the
    /// `URL` input takes a password typed into it out of the URL.
    pub fn move_focus(&mut self, focus: Focus) {
        if self.focus == Focus::Url && focus != Focus::Url {
            self.server.forget_url_password();
        }
        self.focus = focus;
    }

    fn step_focus(&mut self, backwards: bool) {
        let order = self.focus_order();
        let at = order
            .iter()
            .position(|&focus| focus == self.focus)
            .unwrap_or(0);
        let next = if backwards {
            (at + order.len() - 1) % order.len()
        } else {
            (at + 1) % order.len()
        };
        self.move_focus(order.get(next).copied().unwrap_or(Focus::FileInput));
    }

    fn activate(&self) -> ConnectAction {
        match self.focus {
            Focus::Browse if self.file_dialog => ConnectAction::Browse,
            Focus::Cancel if self.can_cancel => ConnectAction::Cancel,
            Focus::WhereFromLink => ConnectAction::ShowLayoutPicker,
            Focus::SampleLink => ConnectAction::LoadSampleDataset,
            Focus::Segments
            | Focus::FileInput
            | Focus::Browse
            | Focus::CreateIfMissing
            | Focus::ReadOnly
            | Focus::Cancel
            | Focus::Open
            | Focus::Url
            | Focus::Protocol
            | Focus::Host
            | Focus::Port
            | Focus::Database
            | Focus::User
            | Focus::Password
            | Focus::UseTls => ConnectAction::Open,
        }
    }
}

/// Answers a key press: the card after it and what the application should do.
pub fn handle_key(mut card: ConnectCard, press: KeyPress) -> (ConnectCard, ConnectAction) {
    card.pointer.focus_by_pointer = false;
    let modifiers = press.modifiers;
    let primary = modifiers.control || modifiers.command;
    if card.busy {
        let action = match press.key {
            Key::Escape => ConnectAction::StopOpening,
            Key::Character('q' | 'Q') if primary => ConnectAction::Quit,
            _ => ConnectAction::None,
        };
        return (card, action);
    }
    let action = match press.key {
        Key::Character('q' | 'Q') if primary => ConnectAction::Quit,
        Key::Character('c' | 'C') if primary && card.copied_text().is_some() => {
            ConnectAction::CopySelection
        }
        Key::Character('v' | 'V') if primary && card.focused_field().is_some() => {
            ConnectAction::Paste
        }
        Key::Character('w' | 'W') if modifiers.command => ConnectAction::Quit,
        Key::F4 if modifiers.alt => ConnectAction::Quit,
        Key::Character('o' | 'O') if primary => {
            card.show_segment(Segment::File);
            card.move_focus(Focus::FileInput);
            ConnectAction::None
        }
        Key::Left | Key::Home if card.focus == Focus::Segments => {
            card.show_segment(Segment::File);
            ConnectAction::None
        }
        Key::Right | Key::End if card.focus == Focus::Segments => {
            card.show_segment(Segment::Server);
            ConnectAction::None
        }
        Key::Down if card.focus == Focus::Protocol => {
            card.server.choose_protocol(card.server.protocol.next());
            ConnectAction::None
        }
        Key::Up if card.focus == Focus::Protocol => {
            card.server.choose_protocol(card.server.protocol.previous());
            ConnectAction::None
        }
        Key::Tab => {
            card.step_focus(modifiers.shift);
            ConnectAction::None
        }
        Key::Enter => card.activate(),
        Key::Escape => {
            if card.can_cancel {
                ConnectAction::Cancel
            } else {
                card.error = None;
                card.failed_field = None;
                card.file.clear_selection();
                ConnectAction::None
            }
        }
        Key::Space => match card.focus {
            Focus::CreateIfMissing => {
                card.create_if_missing = !card.create_if_missing;
                ConnectAction::None
            }
            Focus::ReadOnly => {
                card.read_only = !card.read_only;
                ConnectAction::None
            }
            Focus::UseTls => {
                card.server.toggle_tls();
                ConnectAction::None
            }
            Focus::Protocol => {
                card.server.choose_protocol(card.server.protocol.next());
                ConnectAction::None
            }
            Focus::FileInput
            | Focus::Url
            | Focus::Host
            | Focus::Port
            | Focus::Database
            | Focus::User
            | Focus::Password => {
                card.edit_focused(press);
                ConnectAction::None
            }
            Focus::Segments => ConnectAction::None,
            Focus::Browse
            | Focus::Cancel
            | Focus::Open
            | Focus::WhereFromLink
            | Focus::SampleLink => card.activate(),
        },
        _ => {
            card.edit_focused(press);
            ConnectAction::None
        }
    };
    (card, action)
}

/// The card's top-left in logical pixels for a `width` × `height` window: the card stack
/// centred, rounded to whole pixels.
pub fn card_origin(width: u32, height: u32) -> (f32, f32) {
    (
        ((width as f32 - STACK_WIDTH) / 2.0).round(),
        ((height as f32 - STACK_HEIGHT) / 2.0).round(),
    )
}

/// The framebuffer size in device pixels for a window of `size` logical pixels at `scale`.
pub fn frame_size(size: (u32, u32), scale: f32) -> (usize, usize) {
    let device = |logical: u32| (logical as f32 * scale).round().max(1.0) as usize;
    (device(size.0), device(size.1))
}

fn offset(rect: Rect, by: (f32, f32)) -> Rect {
    Rect::new(rect.x + by.0, rect.y + by.1, rect.w, rect.h)
}

fn down(rect: Rect, by: f32) -> Rect {
    Rect::new(rect.x, rect.y + by, rect.w, rect.h)
}

fn grow(rect: Rect, by: f32) -> Rect {
    Rect::new(
        rect.x - by,
        rect.y - by,
        rect.w + 2.0 * by,
        rect.h + 2.0 * by,
    )
}

fn panel_shift(card: &ConnectCard) -> f32 {
    card.error.as_ref().map_or(0.0, |error| {
        ERROR_LINE_HEIGHT * error.lines().count() as f32
    })
}

fn file_input(card: &ConnectCard) -> Rect {
    if card.file_dialog {
        FILE_INPUT
    } else {
        FILE_INPUT_WITHOUT_BROWSE
    }
}

fn link_rects(faces: Faces<'_>, origin: (f32, f32)) -> [(Rect, &'static str); 2] {
    let first_width = faces.ui.width(WHERE_FROM_LABEL, BODY.0);
    let second_width = faces.ui.width(SAMPLE_LABEL, BODY.0);
    let first = Rect::new(24.0, LINKS_TOP, first_width, BODY.1);
    let second = Rect::new(
        24.0 + first_width + LINK_GAP,
        LINKS_TOP,
        second_width,
        BODY.1,
    );
    [
        (offset(first, origin), WHERE_FROM_LABEL),
        (offset(second, origin), SAMPLE_LABEL),
    ]
}

/// The rect, in logical pixels, of the element the focus ring surrounds for a window of `size`,
/// with its corner radius; the ring itself is drawn 2 px outside it.
pub fn focus_target(card: &ConnectCard, faces: Faces<'_>, size: (u32, u32)) -> (Rect, f32) {
    let origin = card_origin(size.0, size.1);
    let shift = panel_shift(card);
    let [where_from, sample] = link_rects(faces, origin);
    let link = |rect: Rect| {
        Rect::new(
            rect.x - LINK_FOCUS_WIDEN,
            rect.y,
            rect.w + 2.0 * LINK_FOCUS_WIDEN,
            rect.h,
        )
    };
    let active_segment = match card.segment {
        Segment::File => OPEN_A_FILE,
        Segment::Server => CONNECT_TO_A_SERVER,
    };
    let rect = match card.focus {
        Focus::Segments => offset(active_segment, origin),
        Focus::FileInput => offset(down(file_input(card), shift), origin),
        Focus::Browse => offset(down(BROWSE, shift), origin),
        Focus::CreateIfMissing => offset(down(CREATE_BOX, shift), origin),
        Focus::ReadOnly => offset(down(READ_ONLY_BOX, shift), origin),
        Focus::Cancel => offset(CANCEL, origin),
        Focus::Open => offset(OPEN, origin),
        Focus::WhereFromLink => link(where_from.0),
        Focus::SampleLink => link(sample.0),
        Focus::Url
        | Focus::Protocol
        | Focus::Host
        | Focus::Port
        | Focus::Database
        | Focus::User
        | Focus::Password
        | Focus::UseTls => {
            let stop = server_segment::stop_rect(card.focus).unwrap_or(CARD);
            offset(down(stop, shift), origin)
        }
    };
    (rect, RADIUS_CONTROL)
}

struct Pen<'c, 'f> {
    canvas: &'c mut Canvas,
    theme: Theme,
    faces: Faces<'f>,
    scale: f32,
}

impl Pen<'_, '_> {
    fn device(&self, rect: Rect) -> Rect {
        let s = self.scale;
        let (x0, y0) = ((rect.x * s).round(), (rect.y * s).round());
        let (x1, y1) = (
            ((rect.x + rect.w) * s).round(),
            ((rect.y + rect.h) * s).round(),
        );
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    fn line_width(&self) -> f32 {
        self.scale.round().max(1.0)
    }

    fn fill(&mut self, rect: Rect, color: Rgba) {
        let rect = self.device(rect);
        self.canvas.fill_rect(rect, color);
    }

    fn rounded(&mut self, rect: Rect, radius: f32, color: Rgba) {
        let rect = self.device(rect);
        self.canvas
            .fill_rounded_rect(rect, radius * self.scale, color);
    }

    fn outlined(&mut self, rect: Rect, radius: f32, fill: Rgba, border: Rgba) {
        let outer = self.device(rect);
        let line = self.line_width();
        let radius = radius * self.scale;
        self.canvas.fill_rounded_rect(outer, radius, border);
        let inner = Rect::new(
            outer.x + line,
            outer.y + line,
            outer.w - 2.0 * line,
            outer.h - 2.0 * line,
        );
        self.canvas
            .fill_rounded_rect(inner, (radius - line).max(0.0), fill);
    }

    fn text(
        &mut self,
        face: &dyn Typeface,
        text: &str,
        at: (f32, f32),
        metrics: (f32, f32),
        color: Rgba,
    ) {
        let s = self.scale;
        let style = TextStyle {
            size: metrics.0 * s,
            line_height: metrics.1 * s,
            color,
        };
        face.draw_text(
            self.canvas,
            text,
            (at.0 * s).round(),
            (at.1 * s).round(),
            style,
        );
    }

    fn width(&self, face: &dyn Typeface, text: &str, metrics: (f32, f32)) -> f32 {
        face.width(text, metrics.0 * self.scale) / self.scale
    }

    fn elided(&self, face: &dyn Typeface, text: &str, most: f32) -> String {
        if self.width(face, text, BODY) <= most {
            return text.to_owned();
        }
        let mut shown = String::new();
        for character in text.chars() {
            shown.push(character);
            if self.width(face, &format!("{shown}…"), BODY) > most {
                shown.pop();
                break;
            }
        }
        format!("{}…", shown.trim_end())
    }

    fn centred_label(&mut self, face: &dyn Typeface, text: &str, rect: Rect, color: Rgba) {
        let width = self.width(face, text, BODY);
        let left = rect.x + (rect.w - width) / 2.0;
        let top = rect.y + (rect.h - BODY.1) / 2.0;
        self.text(face, text, (left, top), BODY, color);
    }

    fn baseline(&self, face: &dyn Typeface, top: f32, metrics: (f32, f32)) -> f32 {
        let s = self.scale;
        let (ascent, descent) = face.extent(metrics.0 * s);
        ((top * s).round() + (metrics.1 * s - ascent - descent) / 2.0 + ascent).round()
    }

    fn focus_ring(&mut self, rect: Rect, radius: f32) {
        let s = self.scale;
        let inner = grow(self.device(rect), (FOCUS_GAP * s).round());
        let width = (self.theme.focus_ring_width() * s).round();
        self.canvas.stroke_outside_rounded_rect(
            inner,
            (radius + FOCUS_GAP) * s,
            width,
            self.theme.focus_ring,
        );
    }
}

/// Draws the connect card for a window of `size` logical pixels at `scale` device pixels per
/// logical pixel, in `theme`: a pure function of its arguments, the frame
/// `docs/ux/first-screen.md` specifies.
pub fn draw(
    card: &ConnectCard,
    theme: Theme,
    faces: Faces<'_>,
    size: (u32, u32),
    scale: f32,
) -> Canvas {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let (width, height) = frame_size(size, scale);
    let mut canvas = Canvas::new(width, height, theme.bg0);
    let mut pen = Pen {
        canvas: &mut canvas,
        theme,
        faces,
        scale,
    };
    let origin = card_origin(size.0, size.1);
    if card.drag_over {
        drop_ring(&mut pen, origin);
    } else {
        pen.outlined(offset(CARD, origin), RADIUS_CARD, theme.bg0, theme.border);
    }
    heading(&mut pen, origin);
    segments(&mut pen, card, origin);
    error_lines(&mut pen, card, origin);
    match card.segment {
        Segment::File => {
            file_row(&mut pen, card, origin);
            checkboxes(&mut pen, card, origin);
        }
        Segment::Server => server_segment::draw_panel(&mut pen, card, origin),
    }
    buttons(&mut pen, card, origin);
    links(&mut pen, origin);
    status_line(&mut pen, card, origin);
    if card.drag_over {
        let at = (origin.0 + DROP_HINT.0, origin.1 + DROP_HINT.1);
        pen.text(faces.ui, DROP_HINT_LABEL, at, BODY, theme.accent);
    }
    if let Some(item) = card.layout.status_item() {
        let text = format!("Layout: {item}");
        let width = pen.width(faces.ui, &text, SMALL);
        let at = (origin.0 + LAYOUT_LINE_RIGHT - width, origin.1 + LAYOUT_LINE_TOP);
        pen.text(faces.ui, &text, at, SMALL, theme.text_secondary);
    }
    if card.layout_picker.is_none() && !card.disabled() && card.ring_shown() {
        let (rect, radius) = focus_target(card, faces, size);
        pen.focus_ring(rect, radius);
    }
    if let Some(picker) = &card.layout_picker {
        let logical = (size.0 as f32, size.1 as f32);
        crate::layout_picker::draw(&mut canvas, picker, theme, faces, logical, scale);
    }
    canvas
}

fn control_fill(pen: &Pen<'_, '_>, card: &ConnectCard, node: u32) -> (Rgba, Rgba) {
    let theme = pen.theme;
    let fill = card.pointer.mix(node).blend(theme.bg0, theme.bg2, theme.bg2);
    if card.pointer.is_pressed(node) {
        (fill, theme.border_strong)
    } else {
        (fill, theme.border)
    }
}

fn status_line(pen: &mut Pen<'_, '_>, card: &ConnectCard, origin: (f32, f32)) {
    let theme = pen.theme;
    let face = pen.faces.ui;
    if let Some(text) = card.progress.as_ref().or(card.status.as_ref()) {
        let at = (origin.0 + STATUS_LINE.0, origin.1 + STATUS_LINE.1);
        let shown = pen.elided(face, text, STATUS_LINE_WIDTH);
        pen.text(face, &shown, at, BODY, theme.text_secondary);
    }
    if card.progress.is_some() {
        let bar = Rect::new(
            origin.0 + RADIUS_CARD,
            origin.1 + CARD.h - 1.0 - PROGRESS_BAR.1,
            PROGRESS_BAR.0,
            PROGRESS_BAR.1,
        );
        pen.fill(bar, theme.accent);
    }
}

fn drop_ring(pen: &mut Pen<'_, '_>, origin: (f32, f32)) {
    let theme = pen.theme;
    let card = offset(CARD, origin);
    let inner = grow(card, -DROP_RING_WIDTH);
    pen.rounded(card, RADIUS_CARD, theme.accent);
    pen.rounded(inner, RADIUS_CARD - DROP_RING_WIDTH, theme.bg0);
}

fn heading(pen: &mut Pen<'_, '_>, origin: (f32, f32)) {
    let theme = pen.theme;
    let s = pen.scale;
    let (cx, cy) = ((origin.0 + 32.0) * s, (origin.1 + 37.0) * s);
    pen.canvas
        .stroke_circle(cx, cy, 16.0 * s, 2.0 * s, theme.accent);
    pen.canvas.fill_circle(cx, cy, 6.0 * s, theme.brand_yolk);
    let bold = pen.faces.ui_bold;
    pen.text(
        bold,
        "Connect",
        (origin.0 + 48.0, origin.1 + 24.0),
        HEADLINE,
        theme.text_primary,
    );
}

fn segments(pen: &mut Pen<'_, '_>, card: &ConnectCard, origin: (f32, f32)) {
    let theme = pen.theme;
    let outline = offset(SEGMENTS, origin);
    pen.outlined(outline, RADIUS_CONTROL, theme.bg0, theme.border_control);
    let line = pen.line_width();
    let radius = (RADIUS_CONTROL * pen.scale - line).max(0.0);
    match card.segment {
        Segment::File => {
            let active = pen.device(offset(OPEN_A_FILE, origin));
            let fill = Rect::new(
                active.x + line,
                active.y + line,
                active.w - line,
                active.h - 2.0 * line,
            );
            pen.canvas.fill_rounded_rect(fill, radius, theme.bg2);
            let square = Rect::new(fill.x + fill.w / 2.0, fill.y, fill.w / 2.0, fill.h);
            pen.canvas.fill_rect(square, theme.bg2);
        }
        Segment::Server => {
            let active = pen.device(offset(CONNECT_TO_A_SERVER, origin));
            let fill = Rect::new(
                active.x + line,
                active.y + line,
                active.w - 2.0 * line,
                active.h - 2.0 * line,
            );
            pen.canvas.fill_rounded_rect(fill, radius, theme.bg2);
            let square = Rect::new(fill.x, fill.y, fill.w / 2.0, fill.h);
            pen.canvas.fill_rect(square, theme.bg2);
        }
    }
    let divider = pen.device(offset(CONNECT_TO_A_SERVER, origin));
    pen.canvas.fill_rect(
        Rect::new(divider.x, divider.y, line, divider.h),
        theme.border_control,
    );
    let (bold, regular) = (pen.faces.ui_bold, pen.faces.ui);
    let active = (bold, theme.text_primary);
    let inactive = (regular, theme.text_secondary);
    let (file_look, server_look) = match card.segment {
        Segment::File => (active, inactive),
        Segment::Server => (inactive, active),
    };
    for (face, label, rect, color) in [
        (file_look.0, OPEN_A_FILE_LABEL, OPEN_A_FILE, file_look.1),
        (
            server_look.0,
            CONNECT_TO_A_SERVER_LABEL,
            CONNECT_TO_A_SERVER,
            server_look.1,
        ),
    ] {
        let width = pen.width(face, label, BODY);
        let left = origin.0 + rect.x + (rect.w - width) / 2.0;
        pen.text(
            face,
            label,
            (left, origin.1 + SEGMENT_LABEL_TOP),
            BODY,
            color,
        );
    }
}

fn error_lines(pen: &mut Pen<'_, '_>, card: &ConnectCard, origin: (f32, f32)) {
    let Some(error) = &card.error else {
        return;
    };
    let theme = pen.theme;
    let face = pen.faces.ui;
    for (index, line) in error.lines().enumerate() {
        let top = origin.1 + ERROR_TOP + ERROR_LINE_HEIGHT * index as f32;
        let color = if index == 0 {
            theme.error
        } else {
            theme.text_primary
        };
        pen.text(face, line, (origin.0 + 24.0, top), BODY, color);
    }
}

fn file_row(pen: &mut Pen<'_, '_>, card: &ConnectCard, origin: (f32, f32)) {
    let theme = pen.theme;
    let face = pen.faces.ui;
    let shift = panel_shift(card);
    pen.text(
        face,
        "File",
        (origin.0 + FILE_LABEL.0, origin.1 + FILE_LABEL.1 + shift),
        BODY,
        theme.text_primary,
    );
    let input = offset(down(file_input(card), shift), origin);
    let look = InputLook {
        focused: card.focus == Focus::FileInput && !card.disabled(),
        failed: card.error.is_some(),
        placeholder: PLACEHOLDER,
        masked: false,
        caret: card.caret,
    };
    input_box(pen, &card.file, input, card.disabled(), look);
    if card.file_dialog {
        let browse = offset(down(BROWSE, shift), origin);
        let (fill, border) = if card.disabled() {
            (theme.bg0, theme.border)
        } else {
            control_fill(pen, card, NODE_BROWSE)
        };
        pen.outlined(browse, RADIUS_CONTROL, fill, border);
        pen.centred_label(face, "Browse…", browse, theme.text_primary);
    }
}

struct InputLook<'t> {
    focused: bool,
    failed: bool,
    placeholder: &'t str,
    masked: bool,
    caret: CaretLook,
}

fn input_box(
    pen: &mut Pen<'_, '_>,
    field: &TextField,
    input: Rect,
    disabled: bool,
    look: InputLook<'_>,
) {
    let theme = pen.theme;
    let face = pen.faces.ui;
    let border = if look.failed {
        theme.error
    } else if look.focused {
        theme.accent
    } else {
        theme.border_control
    };
    let fill = if disabled { theme.bg1 } else { theme.bg0 };
    pen.outlined(input, RADIUS_CONTROL, fill, border);
    let text_left = input.x + INPUT_TEXT_INSET;
    let text_top = input.y + INPUT_TEXT_TOP;
    let advance = |pen: &Pen<'_, '_>, column: usize| {
        if look.masked {
            column as f32 * server_segment::MASK_ADVANCE
        } else {
            pen.width(face, field.prefix(column), BODY)
        }
    };
    if let Some((start, end)) = field.selection() {
        let before = advance(pen, start);
        let selected = if look.masked {
            advance(pen, end) - before
        } else {
            pen.width(face, field.slice(start, end), BODY)
        };
        let color = if look.focused {
            theme.selection
        } else {
            theme.selection_inactive
        };
        pen.fill(
            Rect::new(text_left + before, text_top, selected, BODY.1),
            color,
        );
    }
    if field.value().is_empty() {
        if !look.placeholder.is_empty() {
            pen.text(
                face,
                look.placeholder,
                (text_left, text_top),
                BODY,
                theme.text_secondary,
            );
        }
    } else if look.masked {
        let s = pen.scale;
        let centre = (input.y + input.h / 2.0) * s;
        for index in 0..field.length() {
            let x = text_left + server_segment::MASK_ADVANCE * (index as f32 + 0.5);
            pen.canvas.fill_circle(
                x * s,
                centre,
                server_segment::MASK_DIAMETER * s,
                theme.text_primary,
            );
        }
    } else {
        pen.text(
            face,
            field.value(),
            (text_left, text_top),
            BODY,
            theme.text_primary,
        );
    }
    if look.focused && look.caret.shown() {
        let caret_x = text_left + advance(pen, field.caret());
        let color = if look.caret == CaretLook::Inactive {
            theme.text_disabled
        } else {
            theme.accent
        };
        pen.fill(Rect::new(caret_x, input.y + CARET_TOP, 2.0, 20.0), color);
    }
}

fn checkbox(pen: &mut Pen<'_, '_>, rect: Rect, checked: bool) {
    let theme = pen.theme;
    if checked {
        pen.rounded(rect, RADIUS_CONTROL, theme.accent);
        let s = pen.scale;
        let point = |x: f32, y: f32| ((rect.x + x) * s, (rect.y + y) * s);
        let width = 1.5 * s;
        pen.canvas
            .line(point(4.0, 8.0), point(7.0, 11.0), width, theme.on_accent);
        pen.canvas
            .line(point(7.0, 11.0), point(12.0, 5.0), width, theme.on_accent);
    } else {
        pen.outlined(rect, RADIUS_CONTROL, theme.bg0, theme.border_control);
    }
}

fn checkboxes(pen: &mut Pen<'_, '_>, card: &ConnectCard, origin: (f32, f32)) {
    let theme = pen.theme;
    let face = pen.faces.ui;
    let shift = panel_shift(card);
    for (rect, label, checked) in [
        (CREATE_BOX, CREATE_LABEL, card.create_if_missing),
        (READ_ONLY_BOX, READ_ONLY_LABEL, card.read_only),
    ] {
        let rect = offset(down(rect, shift), origin);
        checkbox(pen, rect, checked);
        pen.text(
            face,
            label,
            (origin.0 + CHECK_LABEL_X, rect.y - 1.0),
            BODY,
            theme.text_primary,
        );
    }
}

fn buttons(pen: &mut Pen<'_, '_>, card: &ConnectCard, origin: (f32, f32)) {
    let theme = pen.theme;
    let face = pen.faces.ui;
    let cancel = offset(CANCEL, origin);
    if card.can_cancel || card.disabled() {
        let (fill, border) = if card.disabled() {
            (theme.bg0, theme.border)
        } else {
            control_fill(pen, card, NODE_CANCEL)
        };
        pen.outlined(cancel, RADIUS_CONTROL, fill, border);
        pen.centred_label(face, "Cancel", cancel, theme.text_primary);
    } else {
        pen.outlined(cancel, RADIUS_CONTROL, theme.bg1, theme.border);
        pen.centred_label(face, "Cancel", cancel, theme.text_disabled);
    }
    let open = offset(OPEN, origin);
    let open_fill = if card.disabled() {
        theme.accent
    } else {
        card.pointer
            .mix(NODE_OPEN)
            .blend(theme.accent, theme.accent_hover, theme.accent_pressed)
    };
    pen.rounded(open, RADIUS_CONTROL, open_fill);
    let label = match (card.segment, card.disabled()) {
        (Segment::File, false) => "Open",
        (Segment::File, true) => "Opening…",
        (Segment::Server, false) => "Connect",
        (Segment::Server, true) => "Connecting…",
    };
    pen.centred_label(face, label, open, theme.on_accent);
}

fn links(pen: &mut Pen<'_, '_>, origin: (f32, f32)) {
    let theme = pen.theme;
    let face = pen.faces.ui;
    let line = pen.line_width();
    for (rect, label) in link_rects(pen.faces, origin) {
        pen.text(face, label, (rect.x, rect.y), BODY, theme.accent);
        let baseline = pen.baseline(face, rect.y, BODY);
        let device = pen.device(rect);
        let underline_top = baseline + (2.0 * pen.scale).round();
        pen.canvas.fill_rect(
            Rect::new(device.x, underline_top, device.w, line),
            theme.accent,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ThemeKind;
    use crate::font::BoxFace;
    use eggdb_platform::window::Modifiers;

    const BOXES: Faces<'static> = Faces {
        ui: &BoxFace,
        ui_bold: &BoxFace,
        mono: &BoxFace,
    };
    const SIZE: (u32, u32) = (DEFAULT_WIDTH, DEFAULT_HEIGHT);

    fn frame(card: &ConnectCard, kind: ThemeKind, scale: f32) -> (Canvas, Theme) {
        let theme = Theme::of(kind);
        (draw(card, theme, BOXES, SIZE, scale), theme)
    }

    fn press(key: Key) -> KeyPress {
        KeyPress::plain(key)
    }

    fn with(key: Key, modifiers: Modifiers) -> KeyPress {
        KeyPress { key, modifiers }
    }

    const SHIFT: Modifiers = Modifiers {
        shift: true,
        control: false,
        alt: false,
        command: false,
    };
    const CONTROL: Modifiers = Modifiers {
        shift: false,
        control: true,
        alt: false,
        command: false,
    };

    #[test]
    fn the_card_is_centred_with_its_fields_where_the_spec_puts_them_in_every_theme() {
        assert_eq!(card_origin(1280, 800), (360.0, 140.0));
        for kind in [ThemeKind::Light, ThemeKind::Dark, ThemeKind::HighContrast] {
            let (canvas, theme) = frame(&ConnectCard::first_start(), kind, 1.0);
            assert_eq!((canvas.width(), canvas.height()), (1280, 800));
            let at = |x: usize, y: usize| canvas.pixel(x, y).unwrap();
            assert_eq!(at(100, 100), theme.bg0, "{kind:?} window");
            assert_eq!(at(640, 140), theme.border, "{kind:?} card outline");
            assert_eq!(at(640, 400), theme.bg0, "{kind:?} card");
            assert_eq!(at(392, 177), theme.brand_yolk, "{kind:?} egg yolk");
            assert_eq!(
                at(460, 204),
                theme.border_control,
                "{kind:?} segment outline"
            );
            assert_eq!(at(400, 220), theme.bg2, "{kind:?} active segment");
            assert_eq!(at(660, 220), theme.bg0, "{kind:?} inactive segment");
            assert_eq!(at(640, 220), theme.border_control, "{kind:?} divider");
            assert_eq!(at(560, 282), theme.accent, "{kind:?} focused input border");
            assert_eq!(at(392, 289), theme.selection, "{kind:?} selection");
            assert_eq!(at(585, 300), theme.accent, "{kind:?} caret");
            assert_eq!(at(850, 282), theme.border, "{kind:?} browse border");
            assert_eq!(at(386, 339), theme.accent, "{kind:?} checked box");
            assert_eq!(at(392, 350), theme.border_control, "{kind:?} unchecked box");
            assert_eq!(at(392, 358), theme.bg0, "{kind:?} unchecked inside");
            assert_eq!(at(730, 590), theme.bg1, "{kind:?} disabled cancel");
            assert_eq!(at(820, 580), theme.accent, "{kind:?} open");
            assert_eq!(at(390, 654), theme.accent, "{kind:?} link underline");
            assert_eq!(
                at(381, 296),
                theme.focus_ring,
                "{kind:?} ring left of input"
            );
            assert_eq!(at(383, 296), theme.bg0, "{kind:?} gap inside the ring");
        }
    }

    #[test]
    fn the_high_contrast_ring_is_three_pixels_wide() {
        let (light, light_theme) = frame(&ConnectCard::first_start(), ThemeKind::Light, 1.0);
        let (high, high_theme) = frame(&ConnectCard::first_start(), ThemeKind::HighContrast, 1.0);
        assert_eq!(light.pixel(379, 296), Some(light_theme.bg0));
        assert_eq!(high.pixel(379, 296), Some(high_theme.focus_ring));
        assert_eq!(high.pixel(378, 296), Some(high_theme.bg0));
    }

    #[test]
    fn tab_and_shift_tab_visit_every_stop_in_order_and_wrap() {
        let forward = [
            Focus::Browse,
            Focus::CreateIfMissing,
            Focus::ReadOnly,
            Focus::Open,
            Focus::WhereFromLink,
            Focus::SampleLink,
            Focus::Segments,
            Focus::FileInput,
        ];
        let mut card = ConnectCard::first_start();
        for expected in forward {
            let (next, action) = handle_key(card, press(Key::Tab));
            assert_eq!((next.focus, action), (expected, ConnectAction::None));
            card = next;
        }
        let backward = [
            Focus::Segments,
            Focus::SampleLink,
            Focus::WhereFromLink,
            Focus::Open,
            Focus::ReadOnly,
            Focus::CreateIfMissing,
            Focus::Browse,
            Focus::FileInput,
        ];
        for expected in backward {
            card = handle_key(card, with(Key::Tab, SHIFT)).0;
            assert_eq!(card.focus, expected);
        }
        card.can_cancel = true;
        card.focus = Focus::ReadOnly;
        card = handle_key(card, press(Key::Tab)).0;
        assert_eq!(card.focus, Focus::Cancel);
    }

    #[test]
    fn the_ring_follows_the_focus() {
        let mut card = ConnectCard::first_start();
        card = handle_key(card, press(Key::Tab)).0;
        let (canvas, theme) = frame(&card, ThemeKind::Light, 1.0);
        assert_eq!(canvas.pixel(381, 296), Some(theme.bg0), "input ring gone");
        assert_eq!(
            canvas.pixel(560, 282),
            Some(theme.border_control),
            "input border"
        );
        assert_eq!(
            canvas.pixel(809, 296),
            Some(theme.focus_ring),
            "ring left of Browse"
        );
        assert_eq!(canvas.pixel(392, 289), Some(theme.selection_inactive));
        let card = ConnectCard {
            focus: Focus::Open,
            ..ConnectCard::first_start()
        };
        let (canvas, theme) = frame(&card, ThemeKind::Light, 1.0);
        assert_eq!(
            canvas.pixel(813, 590),
            Some(theme.focus_ring),
            "ring left of Open"
        );
        assert_eq!(
            canvas.pixel(860, 573),
            Some(theme.focus_ring),
            "ring above Open"
        );
        let card = ConnectCard {
            focus: Focus::CreateIfMissing,
            ..ConnectCard::first_start()
        };
        let (canvas, theme) = frame(&card, ThemeKind::Light, 1.0);
        assert_eq!(
            canvas.pixel(381, 334),
            Some(theme.focus_ring),
            "ring left of the box"
        );
        let card = ConnectCard {
            focus: Focus::WhereFromLink,
            ..ConnectCard::first_start()
        };
        let (canvas, theme) = frame(&card, ThemeKind::Light, 1.0);
        assert_eq!(
            canvas.pixel(377, 648),
            Some(theme.focus_ring),
            "ring left of the link"
        );
    }

    #[test]
    fn space_toggles_only_checkboxes_and_enter_opens_from_them() {
        let mut card = ConnectCard {
            focus: Focus::CreateIfMissing,
            ..ConnectCard::first_start()
        };
        card = handle_key(card, press(Key::Space)).0;
        assert!(!card.create_if_missing);
        card.focus = Focus::ReadOnly;
        card = handle_key(card, press(Key::Space)).0;
        assert!(card.read_only);
        card.focus = Focus::Segments;
        let (after, action) = handle_key(card.clone(), press(Key::Space));
        assert_eq!((after, action), (card.clone(), ConnectAction::None));
        card.focus = Focus::ReadOnly;
        assert_eq!(
            handle_key(card.clone(), press(Key::Enter)).1,
            ConnectAction::Open
        );
        card.focus = Focus::Browse;
        assert_eq!(
            handle_key(card.clone(), press(Key::Space)).1,
            ConnectAction::Browse
        );
        card.focus = Focus::SampleLink;
        assert_eq!(
            handle_key(card.clone(), press(Key::Enter)).1,
            ConnectAction::LoadSampleDataset
        );
        card.focus = Focus::WhereFromLink;
        assert_eq!(
            handle_key(card, press(Key::Enter)).1,
            ConnectAction::ShowLayoutPicker
        );
    }

    #[test]
    fn quit_escape_and_the_connect_key_do_what_the_spec_says() {
        let card = ConnectCard::first_start();
        let quit = [
            with(Key::Character('q'), CONTROL),
            with(
                Key::Character('q'),
                Modifiers {
                    command: true,
                    ..Modifiers::default()
                },
            ),
            with(
                Key::Character('w'),
                Modifiers {
                    command: true,
                    ..Modifiers::default()
                },
            ),
            with(
                Key::F4,
                Modifiers {
                    alt: true,
                    ..Modifiers::default()
                },
            ),
        ];
        for key in quit {
            assert_eq!(
                handle_key(card.clone(), key).1,
                ConnectAction::Quit,
                "{key:?}"
            );
        }
        assert_eq!(
            handle_key(card.clone(), with(Key::Character('w'), CONTROL)).1,
            ConnectAction::None
        );
        let mut shown = card.clone();
        shown.error = Some(CardError {
            line: "Error: the file is locked".into(),
            fix: Some("close the other program".into()),
        });
        let (cleared, action) = handle_key(shown, press(Key::Escape));
        assert_eq!(action, ConnectAction::None);
        assert_eq!(cleared.error, None);
        assert_eq!(cleared.file.selection(), None);
        let mut cancellable = card.clone();
        cancellable.can_cancel = true;
        assert_eq!(
            handle_key(cancellable, press(Key::Escape)).1,
            ConnectAction::Cancel
        );
        let elsewhere = ConnectCard {
            focus: Focus::Open,
            ..card
        };
        let (focused, _) = handle_key(elsewhere, with(Key::Character('o'), CONTROL));
        assert_eq!(focused.focus, Focus::FileInput);
        assert_eq!(
            handle_key(focused, press(Key::Enter)).1,
            ConnectAction::Open
        );
    }

    #[test]
    fn typing_replaces_the_selected_default_and_edits_like_a_single_line_input() {
        let mut card = ConnectCard::first_start();
        for character in "a.db".chars() {
            card = handle_key(card, press(Key::Character(character))).0;
        }
        assert_eq!(card.file.value(), "a.db");
        card = handle_key(card, press(Key::Backspace)).0;
        card = handle_key(card, press(Key::Left)).0;
        card = handle_key(card, press(Key::Space)).0;
        assert_eq!(card.file.value(), "a. d");
        card = handle_key(card, press(Key::Home)).0;
        card = handle_key(card, press(Key::Delete)).0;
        assert_eq!(card.file.value(), ". d");
        card = handle_key(card, with(Key::Character('a'), CONTROL)).0;
        assert_eq!(card.file.selection(), Some((0, 3)));
        card = handle_key(card, press(Key::Backspace)).0;
        assert_eq!(card.file.value(), "");
        card = handle_key(card, press(Key::Character('é'))).0;
        card = handle_key(card, with(Key::Left, SHIFT)).0;
        assert_eq!(card.file.selection(), Some((0, 1)));
        let (canvas, theme) = frame(
            &ConnectCard {
                file: TextField::selected(""),
                ..ConnectCard::first_start()
            },
            ThemeKind::Light,
            1.0,
        );
        assert_eq!(
            canvas.pixel(395, 297),
            Some(theme.text_secondary),
            "placeholder"
        );
    }

    #[test]
    fn an_error_line_moves_the_panel_down() {
        let card = ConnectCard {
            error: Some(CardError {
                line: "Error: no such file".into(),
                fix: Some("Try: check the path".into()),
            }),
            ..ConnectCard::first_start()
        };
        let (canvas, theme) = frame(&card, ThemeKind::Light, 1.0);
        assert_eq!(
            canvas.pixel(560, 282 + 36),
            Some(theme.error),
            "input moved, with its error border"
        );
        assert_eq!(
            canvas.pixel(381, 296 + 36),
            Some(theme.focus_ring),
            "ring moved"
        );
        let inked = (384..600)
            .filter(|&x| (240..276).any(|y| canvas.pixel(x, y) == Some(theme.error)))
            .count();
        assert!(inked > 50, "the error text is drawn");
    }

    #[test]
    fn at_two_x_every_rect_line_and_ring_doubles() {
        for kind in [ThemeKind::Light, ThemeKind::HighContrast] {
            let (canvas, theme) = frame(&ConnectCard::first_start(), kind, 2.0);
            assert_eq!((canvas.width(), canvas.height()), (2560, 1600));
            let at = |x: usize, y: usize| canvas.pixel(x, y).unwrap();
            assert_eq!(at(1280, 280), theme.border, "{kind:?} card border row 1");
            assert_eq!(at(1280, 281), theme.border, "{kind:?} card border row 2");
            assert_eq!(at(1280, 282), theme.bg0, "{kind:?} card inside");
            assert_eq!(at(1000, 564), theme.accent, "{kind:?} input border row 1");
            assert_eq!(at(1000, 565), theme.accent, "{kind:?} input border row 2");
            assert_eq!(at(1000, 566), theme.bg0, "{kind:?} input inside");
            assert_eq!(at(761, 592), theme.focus_ring, "{kind:?} ring");
            assert_eq!(at(765, 592), theme.bg0, "{kind:?} ring gap");
            assert_eq!(at(784, 578), theme.selection, "{kind:?} selection");
            assert_eq!(at(1170, 600), theme.accent, "{kind:?} caret");
        }
        let (high, theme) = frame(&ConnectCard::first_start(), ThemeKind::HighContrast, 2.0);
        assert_eq!(high.pixel(758, 592), Some(theme.focus_ring));
        assert_eq!(high.pixel(757, 592), Some(theme.bg0));
        let (fractional, _) = frame(&ConnectCard::first_start(), ThemeKind::Light, 1.5);
        assert_eq!((fractional.width(), fractional.height()), (1920, 1200));
    }

    #[test]
    fn the_segments_switch_with_left_and_right_and_the_server_stops_follow() {
        let card = ConnectCard {
            focus: Focus::Segments,
            ..ConnectCard::first_start()
        };
        let (card, action) = handle_key(card, press(Key::Right));
        assert_eq!(action, ConnectAction::None);
        assert_eq!(
            (card.segment, card.focus),
            (Segment::Server, Focus::Segments)
        );
        assert_eq!(
            card.focus_order(),
            vec![
                Focus::Segments,
                Focus::Url,
                Focus::Protocol,
                Focus::Host,
                Focus::Port,
                Focus::Database,
                Focus::User,
                Focus::Password,
                Focus::UseTls,
                Focus::Open,
                Focus::WhereFromLink,
                Focus::SampleLink,
            ]
        );
        let (card, _) = handle_key(card, press(Key::Tab));
        assert_eq!(card.focus, Focus::Url);
        let (card, _) = handle_key(card, press(Key::Tab));
        let (card, _) = handle_key(card, press(Key::Down));
        assert_eq!(card.server.protocol, Protocol::MySql);
        assert_eq!(card.server.port.value(), "3306");
        let (card, _) = handle_key(card, press(Key::Up));
        assert_eq!(card.server.protocol, Protocol::PostgreSql);
        assert_eq!(card.server.port.value(), "5432");
        let card = ConnectCard {
            focus: Focus::UseTls,
            ..card
        };
        let (card, _) = handle_key(card, press(Key::Space));
        assert!(!card.server.use_tls);
        let (card, action) = handle_key(card, press(Key::Enter));
        assert_eq!(action, ConnectAction::Open);
        let card = ConnectCard {
            focus: Focus::Host,
            ..card
        };
        let (card, _) = handle_key(card, press(Key::Character('h')));
        assert_eq!(card.server.host.value(), "h");
        assert_eq!(
            card.server.url.value(),
            "postgresql://h:5432?sslmode=disable"
        );
        let (rect, _) = focus_target(&card, BOXES, SIZE);
        assert_eq!(
            (rect.x, rect.y, rect.w, rect.h),
            (560.0, 348.0, 224.0, 28.0)
        );
        let card = ConnectCard {
            focus: Focus::Segments,
            ..card
        };
        let (card, _) = handle_key(card, press(Key::Home));
        assert_eq!(card.segment, Segment::File);
        let (card, _) = handle_key(card, press(Key::End));
        let (card, _) = handle_key(card, with(Key::Character('o'), CONTROL));
        assert_eq!(
            (card.segment, card.focus),
            (Segment::File, Focus::FileInput)
        );
    }

    #[test]
    fn a_certificate_check_typed_into_the_url_is_refused_after_tab_and_a_field_edit() {
        let certificate_refused = |card: &ConnectCard| {
            let failure = card.server.connect_options().unwrap_err();
            failure.error_line() == "Error: the client cannot verify server certificates yet"
                && failure
                    .try_line()
                    .is_some_and(|fix| fix.starts_with("Try: sslmode=require"))
        };
        let mut card = ConnectCard::first_start();
        card.show_segment(Segment::Server);
        card.move_focus(Focus::Url);
        assert!(card.set_focused_value("postgresql://ada:pw@db/shop?sslmode=verify-full"));
        let (card, _) = handle_key(card, press(Key::Tab));
        assert_ne!(card.focus, Focus::Url);
        assert!(!card.server.url.value().contains("pw"));
        assert!(certificate_refused(&card));
        let mut card = ConnectCard {
            focus: Focus::Host,
            ..card
        };
        let (edited, _) = handle_key(card.clone(), press(Key::Character('2')));
        assert_eq!(edited.server.host.value(), "db2");
        assert!(certificate_refused(&edited));
        card.move_focus(Focus::Url);
        assert!(card.set_focused_value("postgresql://ada:pw@db/shop"));
        let (card, _) = handle_key(card, press(Key::Tab));
        assert!(card.server.connect_options().is_ok());
    }

    #[test]
    fn a_server_failure_moves_to_its_field_and_borders_only_it() {
        let mut card = ConnectCard::first_start();
        card.show_segment(Segment::Server);
        card.server.host.set("db");
        card.begin_opening();
        card.finish_with_failure(&crate::connection::url_failure());
        assert_eq!((card.segment, card.focus), (Segment::Server, Focus::Url));
        assert_eq!(card.failed_focus(), Some(Focus::Url));
        let mut file_failure = card.clone();
        file_failure.begin_opening();
        file_failure.finish_with_failure(&ConnectFailure {
            code: None,
            message: "cannot open x: no such file".to_owned(),
            fix: None,
            field: FailureField::File,
        });
        assert_eq!(
            (file_failure.segment, file_failure.focus),
            (Segment::File, Focus::FileInput)
        );
        let (canvas, theme) = frame(&card, ThemeKind::Light, 1.0);
        assert_eq!(
            canvas.pixel(360 + 100, 140 + 142 + 36),
            Some(theme.error),
            "URL border"
        );
        assert_ne!(
            canvas.pixel(360 + 300, 140 + 208 + 36),
            Some(theme.error),
            "Host border"
        );
        let mut busy = ConnectCard::first_start();
        busy.show_segment(Segment::Server);
        busy.begin_opening();
        busy.progress = Some("Connecting to localhost:5432…".to_owned());
        let (canvas, theme) = frame(&busy, ThemeKind::Light, 1.0);
        assert_eq!(
            canvas.pixel(360 + 400, 140 + 150),
            Some(theme.bg1),
            "disabled URL input"
        );
    }

    #[test]
    fn a_press_in_an_input_places_the_caret_and_double_and_triple_clicks_select() {
        let mut card = ConnectCard::first_start();
        card.file.set("data/shop.eggdb");
        let advance = BoxFace.advance('x', BODY.0);
        let left = 100.0;
        let at = |column: f32| left + INPUT_TEXT_INSET + advance * column;
        card.place_caret_at(BOXES, left, at(4.4), 1, false);
        assert_eq!((card.file.caret(), card.file.selection()), (4, None));
        card.place_caret_at(BOXES, left, at(7.6), 1, true);
        assert_eq!(card.file.selected_text(), Some("/sho"));
        card.place_caret_at(BOXES, left, at(7.0), 2, false);
        assert_eq!(card.file.selected_text(), Some("shop"));
        card.place_caret_at(BOXES, left, at(7.0), 3, false);
        assert_eq!(card.file.selected_text(), Some("data/shop.eggdb"));
        card.place_caret_at(BOXES, left, left - 50.0, 1, false);
        assert_eq!(card.file.caret(), 0);
        card.focus = Focus::Password;
        card.server.password.set("secret");
        card.place_caret_at(
            BOXES,
            left,
            left + INPUT_TEXT_INSET + server_segment::MASK_ADVANCE * 2.2,
            2,
            false,
        );
        assert_eq!(
            card.server.password.selection(),
            Some((0, 6)),
            "a double-click in the password selects it all"
        );
        assert_eq!(card.copied_text(), None, "the password is never copied");
    }

    #[test]
    fn a_paste_into_an_input_is_one_line_and_updates_the_url() {
        let mut card = ConnectCard::first_start();
        assert!(card.paste("/tmp/a.eggdb\n"));
        assert_eq!(
            card.file.value(),
            "/tmp/a.eggdb",
            "the selected quickstart path is replaced"
        );
        card.show_segment(Segment::Server);
        card.focus = Focus::Host;
        card.server.host.set("");
        assert!(card.paste("db.example.com"));
        assert!(
            card.server.url.value().contains("db.example.com"),
            "{}",
            card.server.url.value()
        );
        card.focus = Focus::Open;
        assert!(!card.paste("ignored"));
    }

    #[test]
    fn hover_and_press_fill_the_buttons_and_a_click_hides_their_ring() {
        let mut card = ConnectCard::first_start();
        card.can_cancel = true;
        let origin = card_origin(SIZE.0, SIZE.1);
        let inside = |rect: Rect| {
            (
                (origin.0 + rect.x + 4.0) as usize,
                (origin.1 + rect.y + rect.h / 2.0) as usize,
            )
        };
        let (cancel_x, cancel_y) = inside(CANCEL);
        let (open_x, open_y) = inside(OPEN);
        let (canvas, theme) = frame(&card, ThemeKind::Light, 1.0);
        assert_eq!(canvas.pixel(cancel_x, cancel_y), Some(theme.bg0));
        assert_eq!(canvas.pixel(open_x, open_y), Some(theme.accent));
        card.pointer.hovered = Some(NODE_CANCEL);
        let (canvas, _) = frame(&card, ThemeKind::Light, 1.0);
        assert_eq!(
            canvas.pixel(cancel_x, cancel_y),
            Some(theme.bg2),
            "hover fills bg2"
        );
        card.pointer.pressed = Some(NODE_CANCEL);
        let (canvas, _) = frame(&card, ThemeKind::Light, 1.0);
        let edge = (origin.0 + CANCEL.x + CANCEL.w / 2.0) as usize;
        assert_eq!(
            canvas.pixel(edge, (origin.1 + CANCEL.y) as usize),
            Some(theme.border_strong),
            "pressed edge"
        );
        card.pointer = PointerLook {
            hovered: Some(NODE_OPEN),
            ..PointerLook::default()
        };
        let (canvas, _) = frame(&card, ThemeKind::Light, 1.0);
        assert_eq!(canvas.pixel(open_x, open_y), Some(theme.accent_hover));
        card.pointer.pressed = Some(NODE_OPEN);
        let (canvas, _) = frame(&card, ThemeKind::Light, 1.0);
        assert_eq!(canvas.pixel(open_x, open_y), Some(theme.accent_pressed));
        card.focus = Focus::Open;
        card.pointer.focus_by_pointer = true;
        assert!(!card.ring_shown());
        card.focus = Focus::FileInput;
        assert!(card.ring_shown(), "an input keeps its ring after a click");
    }

    #[test]
    fn the_caret_draws_by_its_blink_and_greys_when_the_window_is_inactive() {
        let mut card = ConnectCard::first_start();
        card.file.set("ab");
        let (on, theme) = frame(&card, ThemeKind::Light, 1.0);
        card.caret = CaretLook::Off;
        let (off, _) = frame(&card, ThemeKind::Light, 1.0);
        card.caret = CaretLook::Inactive;
        let (inactive, _) = frame(&card, ThemeKind::Light, 1.0);
        let origin = card_origin(SIZE.0, SIZE.1);
        let x = (origin.0 + FILE_INPUT.x + INPUT_TEXT_INSET + BoxFace.advance('a', BODY.0) * 2.0)
            as usize
            + 1;
        let y = (origin.1 + FILE_INPUT.y + CARET_TOP + 10.0) as usize;
        assert_eq!(on.pixel(x, y), Some(theme.accent));
        assert_ne!(off.pixel(x, y), Some(theme.accent));
        assert_eq!(inactive.pixel(x, y), Some(theme.text_disabled));
    }

    #[test]
    fn a_chosen_or_dropped_path_is_kept_exactly_and_typed_text_is_resolved() {
        let home = Path::new("/home/ada");
        let current = Path::new("/work");
        let mut card = ConnectCard::first_start();
        let chosen = PathBuf::from("/data/shop.eggdb ");
        card.choose_file(Some(&chosen));
        let target = card.file_target(Some(home), current);
        assert_eq!(target.path, chosen);
        assert!(!target.create_if_missing);
        let tilde = PathBuf::from("~/literal.eggdb");
        assert_eq!(
            card.drop_files(std::slice::from_ref(&tilde)),
            ConnectAction::Open
        );
        assert_eq!(card.file_target(Some(home), current).path, tilde);
        card.file.set(" ~/typed.eggdb ");
        let typed = card.file_target(Some(home), current);
        assert_eq!(typed.path, home.join("typed.eggdb"));
        assert!(typed.create_if_missing);
    }
}
