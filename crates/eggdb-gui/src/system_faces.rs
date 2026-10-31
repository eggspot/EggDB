use crate::font::TrueTypeFont;
use eggdb_platform::fonts::{FontRole, system_font_files};

/// The interface, bold and monospace fonts found on this system, each the first of its
/// candidates that reads as a TrueType font.
#[derive(Debug, Clone)]
pub struct SystemFaces {
    /// The interface face.
    pub ui: TrueTypeFont,
    /// Its bold.
    pub ui_bold: TrueTypeFont,
    /// The monospace face.
    pub mono: TrueTypeFont,
}

fn first_usable(role: FontRole) -> Option<TrueTypeFont> {
    system_font_files(role)
        .into_iter()
        .filter_map(|path| std::fs::read(path).ok())
        .find_map(|bytes| TrueTypeFont::parse(bytes).ok())
}

impl SystemFaces {
    /// The system's fonts, or `None` when one of the three roles has no usable font.
    pub fn find() -> Option<Self> {
        Some(Self {
            ui: first_usable(FontRole::Interface)?,
            ui_bold: first_usable(FontRole::InterfaceBold)?,
            mono: first_usable(FontRole::Monospace)?,
        })
    }

    /// The faces to draw a frame with.
    pub fn faces(&self) -> crate::main_window::Faces<'_> {
        crate::main_window::Faces {
            ui: &self.ui,
            ui_bold: &self.ui_bold,
            mono: &self.mono,
        }
    }
}
