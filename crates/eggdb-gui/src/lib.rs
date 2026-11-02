//! EggDB's own GUI toolkit and the desktop client's screens, designed in `docs/ux/gui.md`
//! (D15, D191, DNEW-I29): an RGBA [`Canvas`] with antialiased shapes, text from TrueType fonts
//! read by our own [`TrueTypeFont`], a [`png`] encoder for offscreen frames, the first screen's
//! connect card drawn by [`connect_screen::draw`] with its keyboard model
//! [`connect_screen::handle_key`], and the client's main window drawn by
//! [`main_window::draw`]. Frames are pure functions of the screen's state, the theme, the size
//! and the scale, so they are tested offscreen; the `eggdb-gui` binary presents them in a
//! native window from the platform layer (`eggdb_platform::window`).

mod canvas;
mod color;
/// The first screen: the connect card, its focus order and keys (`docs/ux/first-screen.md`).
pub mod connect_screen;
/// The query editor: text, cursor, selection and editing commands.
pub mod editor;
mod font;
/// The result grid: columns, rows, scrolling and cell selection.
pub mod grid;
/// SQL syntax highlighting for the editor.
pub mod highlight;
/// The window layout: panes, splitters and their sizes.
pub mod layout;
/// The main window: what it draws and how it answers input.
pub mod main_window;
/// PNG encoding of rendered frames, for screenshots and tests.
pub mod png;
mod system_faces;

pub use canvas::{Canvas, Rect};
pub use color::{Rgba, Theme, ThemeKind};
pub use font::{BoxFace, FontError, TextStyle, TrueTypeFont, Typeface};
pub use system_faces::SystemFaces;
