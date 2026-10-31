//! EggDB's own GUI toolkit and the desktop client's screens, designed in `docs/ux/gui.md`
//! (D15, D191): an RGBA [`Canvas`] with antialiased shapes, text from TrueType fonts read by
//! our own [`TrueTypeFont`], a [`png`] encoder for offscreen frames, and the client's main
//! window drawn by [`main_window::draw`]. Windows on each operating system come from the
//! platform layer later; until then frames are rendered offscreen.

mod canvas;
mod color;
pub mod editor;
mod font;
pub mod grid;
pub mod highlight;
pub mod layout;
pub mod main_window;
pub mod png;
mod system_faces;

pub use canvas::{Canvas, Rect};
pub use color::{Rgba, Theme, ThemeKind};
pub use font::{BoxFace, FontError, TextStyle, TrueTypeFont, Typeface};
pub use system_faces::SystemFaces;
