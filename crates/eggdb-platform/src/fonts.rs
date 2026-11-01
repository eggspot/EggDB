//! Where the operating system keeps its fonts, for EggDB's GUI to draw text with fonts it does
//! not ship (D191).

use std::path::PathBuf;

/// What a font is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontRole {
    /// The interface face.
    Interface,
    /// Its bold.
    InterfaceBold,
    /// The monospace face for code and grids.
    Monospace,
}

#[cfg(target_os = "macos")]
fn candidates(role: FontRole) -> Vec<PathBuf> {
    let names: &[&str] = match role {
        FontRole::Interface => &[
            "/System/Library/Fonts/HelveticaNeue.ttc",
            "/System/Library/Fonts/Helvetica.ttc",
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "/Library/Fonts/Arial.ttf",
        ],
        FontRole::InterfaceBold => &[
            "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
            "/Library/Fonts/Arial Bold.ttf",
            "/System/Library/Fonts/Helvetica.ttc",
        ],
        FontRole::Monospace => &[
            "/System/Library/Fonts/Menlo.ttc",
            "/System/Library/Fonts/Monaco.ttf",
            "/System/Library/Fonts/Supplemental/Courier New.ttf",
        ],
    };
    names.iter().map(PathBuf::from).collect()
}

#[cfg(windows)]
fn candidates(role: FontRole) -> Vec<PathBuf> {
    let folder = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\Windows"))
        .join("Fonts");
    let names: &[&str] = match role {
        FontRole::Interface => &["segoeui.ttf", "arial.ttf", "tahoma.ttf"],
        FontRole::InterfaceBold => &["segoeuib.ttf", "arialbd.ttf", "tahomabd.ttf"],
        FontRole::Monospace => &["CascadiaMono.ttf", "consola.ttf", "cour.ttf"],
    };
    names.iter().map(|name| folder.join(name)).collect()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn candidates(role: FontRole) -> Vec<PathBuf> {
    let names: &[&str] = match role {
        FontRole::Interface => &[
            "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        ],
        FontRole::InterfaceBold => &[
            "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
            "/usr/share/fonts/noto/NotoSans-Bold.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
        ],
        FontRole::Monospace => &[
            "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
            "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
        ],
    };
    names.iter().map(PathBuf::from).collect()
}

#[cfg(not(any(unix, windows)))]
fn candidates(_role: FontRole) -> Vec<PathBuf> {
    Vec::new()
}

/// The font files that exist for `role` on this system, in the order of preference of
/// `docs/ux/gui.md`.
pub fn system_font_files(role: FontRole) -> Vec<PathBuf> {
    candidates(role)
        .into_iter()
        .filter(|path| path.is_file())
        .collect()
}
