//! EggDB's own GUI toolkit and the desktop client's screens, designed in `docs/ux/gui.md`
//! (D15, D191, D256): an RGBA [`Canvas`] with antialiased shapes, text from TrueType fonts
//! read by our own [`TrueTypeFont`], a [`png`] encoder for offscreen frames, the first screen's
//! connect card drawn by [`connect_screen::draw`] with its keyboard model
//! [`connect_screen::handle_key`] and its screen-reader tree
//! [`connect_screen::accessibility_tree`], and the client's main window drawn by
//! [`main_window::draw`]. Frames are pure functions of the screen's state, the theme, the size
//! and the scale, so they are tested offscreen; the `eggdb-gui` binary presents them in a
//! native window from the platform layer (`eggdb_platform::window`).

/// What screen readers get beyond names and roles (D278): each role's actions, a text dump of
/// a tree, and a request turned into the focus move and keys that do the same thing.
pub mod accessibility;
/// The browser build's glue: DOM key names, modifier bits, theme and action numbers, and the
/// accessibility tree as JSON for the ARIA mirror over the canvas (D256).
pub mod browser;
mod canvas;
/// The database's catalog as the object browser lists it: tables, views, indexes, sequences
/// and key spaces (`docs/ux/workspace.md` 4.1).
pub mod catalog;
/// Where copied text goes: the [`clipboard::Clipboard`] the platform layer gives, or one in
/// memory.
pub mod clipboard;
mod color;
/// The first screen: the connect card, its focus order, keys and accessibility tree
/// (`docs/ux/first-screen.md`).
pub mod connect_screen;
/// The GUI's link to a database: opening files and connecting to servers, with failures in
/// the card's words (`docs/ux/workspace.md` 1).
pub mod connection;
/// The SQL dialects a query tab runs in.
pub mod dialect;
/// The query editor: text, cursor, selection and editing commands.
pub mod editor;
mod font;
/// The result grid: columns, rows, scrolling and cell selection.
pub mod grid;
/// SQL syntax highlighting for the editor.
pub mod highlight;
/// The inspector: the selected object's columns, indexes, DDL and sample rows
/// (`docs/ux/workspace.md` 4.3).
pub mod inspector;
/// The main window's layouts (`docs/ux/gui-layouts.md`): EggDB's own and those that work like
/// clients people already know, each with its regions, labels and shortcut table.
pub mod layout;
/// The layout picker: the dialog that lists the layouts and applies one.
pub mod layout_picker;
/// The main window: what it draws and how it answers input.
pub mod main_window;
/// The object browser: the sidebar's tree of the catalog, its filter and keys
/// (`docs/ux/workspace.md` 4.1, 4.2).
pub mod object_browser;
/// The plan view: `EXPLAIN`'s answer read into a tree, laid out top-down with each node's
/// cost, rows, time and buffers, its hot nodes tinted and its keys (`docs/ux/workspace.md` 5).
pub mod plan_view;
/// PNG encoding of rendered frames, for screenshots and tests.
pub mod png;
/// The pointer and the caret (`docs/ux/workspace.md` 6): hit testing on the accessibility
/// tree's bounds, double and triple clicks, hover and pressed states, the caret's blink, the
/// window's minimum size and how pasted text is cleaned.
pub mod pointer;
/// The query tab: its editor, dialect, runs, results and inline errors
/// (`docs/ux/workspace.md` 2).
pub mod query_tab;
/// What the client keeps per user between starts, such as the layout.
pub mod settings;
/// Splitting an editor's text into statements by each dialect's rules.
pub mod statements;
/// The status line's and status bar's strings (`docs/ux/workspace.md` 2.4).
pub mod status_line;
mod system_faces;
/// The client's screens in order: the connect card until a database opens, then the main
/// window on it (`docs/ux/workspace.md` 1.5 to 1.7).
pub mod workspace;

pub use canvas::{Canvas, Rect};
pub use color::{Rgba, Theme, ThemeKind};
pub use font::{BoxFace, FontError, TextStyle, TrueTypeFont, Typeface};
pub use system_faces::SystemFaces;
