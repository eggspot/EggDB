/// A colour with straight (not premultiplied) alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
    /// Opacity, 255 opaque.
    pub a: u8,
}

impl Rgba {
    /// An opaque colour from `0xRRGGBB`.
    pub const fn hex(rgb: u32) -> Self {
        Self {
            r: (rgb >> 16) as u8,
            g: (rgb >> 8) as u8,
            b: rgb as u8,
            a: 255,
        }
    }

    /// The colour mixed over `under` with `coverage` (0 to 1) of its own opacity.
    pub fn over(self, under: Self, coverage: f32) -> Self {
        let alpha = (f32::from(self.a) / 255.0) * coverage.clamp(0.0, 1.0);
        let mix = |top: u8, bottom: u8| {
            (f32::from(top) * alpha + f32::from(bottom) * (1.0 - alpha)).round() as u8
        };
        Self {
            r: mix(self.r, under.r),
            g: mix(self.g, under.g),
            b: mix(self.b, under.b),
            a: 255,
        }
    }
}

/// Light or dark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeKind {
    /// The light theme.
    Light,
    /// The dark theme.
    Dark,
}

/// The colour roles of `docs/ux/gui.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// Editor, grid body, inputs and cards.
    pub bg0: Rgba,
    /// Sidebar, headers and the current editor line.
    pub bg1: Rgba,
    /// Toolbar, tab strip and status bar.
    pub bg2: Rgba,
    /// Every second grid row.
    pub zebra: Rgba,
    /// One-pixel lines.
    pub border: Rgba,
    /// Pressed borders and column separators.
    pub border_strong: Rgba,
    /// Labels and cell text.
    pub text_primary: Rgba,
    /// Line numbers, counts and status items.
    pub text_secondary: Rgba,
    /// Disabled labels and `NULL`.
    pub text_disabled: Rgba,
    /// Primary buttons, the active tab stripe and the caret.
    pub accent: Rgba,
    /// Text on the accent colour.
    pub on_accent: Rgba,
    /// The selected row, cell or text.
    pub selection: Rgba,
    /// The selection in a region without focus.
    pub selection_inactive: Rgba,
    /// The connected dot.
    pub success: Rgba,
    /// The egg mark's yolk.
    pub brand_yolk: Rgba,
    /// SQL keywords.
    pub syntax_keyword: Rgba,
    /// String literals.
    pub syntax_string: Rgba,
    /// Numeric literals.
    pub syntax_number: Rgba,
    /// Comments.
    pub syntax_comment: Rgba,
}

impl Theme {
    /// The theme of `kind`.
    pub fn of(kind: ThemeKind) -> Self {
        match kind {
            ThemeKind::Light => Self {
                bg0: Rgba::hex(0xFFFFFF),
                bg1: Rgba::hex(0xF4F5F7),
                bg2: Rgba::hex(0xE9EBEE),
                zebra: Rgba::hex(0xF8F9FA),
                border: Rgba::hex(0xD5D8DD),
                border_strong: Rgba::hex(0xB8BCC4),
                text_primary: Rgba::hex(0x1B1F24),
                text_secondary: Rgba::hex(0x5A6270),
                text_disabled: Rgba::hex(0x9AA1AB),
                accent: Rgba::hex(0x1A6FD6),
                on_accent: Rgba::hex(0xFFFFFF),
                selection: Rgba::hex(0xD6E6FA),
                selection_inactive: Rgba::hex(0xE6E9EE),
                success: Rgba::hex(0x1E8E3E),
                brand_yolk: Rgba::hex(0xF5B700),
                syntax_keyword: Rgba::hex(0x0550AE),
                syntax_string: Rgba::hex(0x0A7B3E),
                syntax_number: Rgba::hex(0xB35C00),
                syntax_comment: Rgba::hex(0x6E7781),
            },
            ThemeKind::Dark => Self {
                bg0: Rgba::hex(0x1E2227),
                bg1: Rgba::hex(0x23282E),
                bg2: Rgba::hex(0x2A3038),
                zebra: Rgba::hex(0x21262C),
                border: Rgba::hex(0x363D47),
                border_strong: Rgba::hex(0x4A525E),
                text_primary: Rgba::hex(0xE6E9EE),
                text_secondary: Rgba::hex(0xA3ABB7),
                text_disabled: Rgba::hex(0x6B7380),
                accent: Rgba::hex(0x5A9CFF),
                on_accent: Rgba::hex(0x0D1117),
                selection: Rgba::hex(0x2D4F7F),
                selection_inactive: Rgba::hex(0x333B46),
                success: Rgba::hex(0x3FB950),
                brand_yolk: Rgba::hex(0xF5B700),
                syntax_keyword: Rgba::hex(0x79B8FF),
                syntax_string: Rgba::hex(0x7EE787),
                syntax_number: Rgba::hex(0xFFA657),
                syntax_comment: Rgba::hex(0x8B949E),
            },
        }
    }
}
