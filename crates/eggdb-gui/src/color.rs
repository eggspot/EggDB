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

    /// The WCAG 2 relative luminance of the colour, ignoring opacity: 0 for black, 1 for white.
    pub fn relative_luminance(self) -> f32 {
        let linear = |channel: u8| {
            let value = f32::from(channel) / 255.0;
            if value <= 0.039_28 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(self.r) + 0.7152 * linear(self.g) + 0.0722 * linear(self.b)
    }

    /// The WCAG 2 contrast ratio between the colour and `other`, from 1 to 21.
    pub fn contrast_ratio(self, other: Self) -> f32 {
        let (a, b) = (self.relative_luminance(), other.relative_luminance());
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
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

/// Light, dark or high contrast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeKind {
    /// The light theme.
    #[default]
    Light,
    /// The dark theme.
    Dark,
    /// The high-contrast theme on black, for the OS's contrast settings
    /// (`docs/ux/first-screen.md`).
    HighContrast,
}

impl ThemeKind {
    /// The theme named `name` as `--theme` takes it: `light`, `dark` or `high-contrast`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            "high-contrast" => Some(Self::HighContrast),
            _ => None,
        }
    }
}

/// The colour roles of `docs/ux/gui.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// Which theme these roles belong to.
    pub kind: ThemeKind,
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
    /// The outline of inputs, checkboxes and the segmented control (3:1 on `bg0`).
    pub border_control: Rgba,
    /// Labels and cell text.
    pub text_primary: Rgba,
    /// Line numbers, counts and status items.
    pub text_secondary: Rgba,
    /// Disabled labels and `NULL`.
    pub text_disabled: Rgba,
    /// Primary buttons, the active tab stripe and the caret.
    pub accent: Rgba,
    /// A hovered primary button.
    pub accent_hover: Rgba,
    /// A pressed primary button.
    pub accent_pressed: Rgba,
    /// Text on the accent colour.
    pub on_accent: Rgba,
    /// The selected row, cell or text.
    pub selection: Rgba,
    /// The selection in a region without focus.
    pub selection_inactive: Rgba,
    /// The ring around the control with the keyboard focus.
    pub focus_ring: Rgba,
    /// The translucent cover drawn over a window under a modal dialog.
    pub scrim: Rgba,
    /// The connected dot.
    pub success: Rgba,
    /// `Warning:` lines.
    pub warning: Rgba,
    /// `Error:` lines and error underlines.
    pub error: Rgba,
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
                kind,
                bg0: Rgba::hex(0xFFFFFF),
                bg1: Rgba::hex(0xF4F5F7),
                bg2: Rgba::hex(0xE9EBEE),
                zebra: Rgba::hex(0xF8F9FA),
                border: Rgba::hex(0xD5D8DD),
                border_strong: Rgba::hex(0xB8BCC4),
                border_control: Rgba::hex(0x6E7781),
                text_primary: Rgba::hex(0x1B1F24),
                text_secondary: Rgba::hex(0x5A6270),
                text_disabled: Rgba::hex(0x9AA1AB),
                accent: Rgba::hex(0x1A6FD6),
                accent_hover: Rgba::hex(0x155CB3),
                accent_pressed: Rgba::hex(0x114A91),
                on_accent: Rgba::hex(0xFFFFFF),
                selection: Rgba::hex(0xD6E6FA),
                selection_inactive: Rgba::hex(0xE6E9EE),
                focus_ring: Rgba::hex(0x1A6FD6),
                scrim: Rgba {
                    r: 0x1B,
                    g: 0x1F,
                    b: 0x24,
                    a: 102,
                },
                success: Rgba::hex(0x1E8E3E),
                warning: Rgba::hex(0xB26A00),
                error: Rgba::hex(0xC8322B),
                brand_yolk: Rgba::hex(0xF5B700),
                syntax_keyword: Rgba::hex(0x0550AE),
                syntax_string: Rgba::hex(0x0A7B3E),
                syntax_number: Rgba::hex(0xB35C00),
                syntax_comment: Rgba::hex(0x6E7781),
            },
            ThemeKind::Dark => Self {
                kind,
                bg0: Rgba::hex(0x1E2227),
                bg1: Rgba::hex(0x23282E),
                bg2: Rgba::hex(0x2A3038),
                zebra: Rgba::hex(0x21262C),
                border: Rgba::hex(0x363D47),
                border_strong: Rgba::hex(0x4A525E),
                border_control: Rgba::hex(0x8B949E),
                text_primary: Rgba::hex(0xE6E9EE),
                text_secondary: Rgba::hex(0xA3ABB7),
                text_disabled: Rgba::hex(0x6B7380),
                accent: Rgba::hex(0x5A9CFF),
                accent_hover: Rgba::hex(0x7AB0FF),
                accent_pressed: Rgba::hex(0x4788E8),
                on_accent: Rgba::hex(0x0D1117),
                selection: Rgba::hex(0x2D4F7F),
                selection_inactive: Rgba::hex(0x333B46),
                focus_ring: Rgba::hex(0x7AB0FF),
                scrim: Rgba {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 143,
                },
                success: Rgba::hex(0x3FB950),
                warning: Rgba::hex(0xE0A030),
                error: Rgba::hex(0xF0625A),
                brand_yolk: Rgba::hex(0xF5B700),
                syntax_keyword: Rgba::hex(0x79B8FF),
                syntax_string: Rgba::hex(0x7EE787),
                syntax_number: Rgba::hex(0xFFA657),
                syntax_comment: Rgba::hex(0x8B949E),
            },
            ThemeKind::HighContrast => Self {
                kind,
                bg0: Rgba::hex(0x000000),
                bg1: Rgba::hex(0x0A0A0A),
                bg2: Rgba::hex(0x141414),
                zebra: Rgba::hex(0x0A0A0A),
                border: Rgba::hex(0xFFFFFF),
                border_strong: Rgba::hex(0xFFFFFF),
                border_control: Rgba::hex(0xFFFFFF),
                text_primary: Rgba::hex(0xFFFFFF),
                text_secondary: Rgba::hex(0xD0D0D0),
                text_disabled: Rgba::hex(0x9E9E9E),
                accent: Rgba::hex(0x8AB4F8),
                accent_hover: Rgba::hex(0xA8C7FA),
                accent_pressed: Rgba::hex(0x6A9DF0),
                on_accent: Rgba::hex(0x000000),
                selection: Rgba::hex(0x1F3A5F),
                selection_inactive: Rgba::hex(0x333333),
                focus_ring: Rgba::hex(0xFFFFFF),
                scrim: Rgba {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 179,
                },
                success: Rgba::hex(0x7CE38B),
                warning: Rgba::hex(0xFFD75F),
                error: Rgba::hex(0xFF8A80),
                brand_yolk: Rgba::hex(0xF5B700),
                syntax_keyword: Rgba::hex(0x8AB4F8),
                syntax_string: Rgba::hex(0x7CE38B),
                syntax_number: Rgba::hex(0xFFD75F),
                syntax_comment: Rgba::hex(0xD0D0D0),
            },
        }
    }

    /// The focus ring's width: 2 px, or 3 px in high contrast.
    pub fn focus_ring_width(&self) -> f32 {
        match self.kind {
            ThemeKind::HighContrast => 3.0,
            ThemeKind::Light | ThemeKind::Dark => 2.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pair_on_the_connect_screen_meets_its_contrast_limit() {
        for kind in [ThemeKind::Light, ThemeKind::Dark, ThemeKind::HighContrast] {
            let theme = Theme::of(kind);
            let text_limit = if kind == ThemeKind::HighContrast {
                7.0
            } else {
                4.5
            };
            let text_pairs = [
                ("text_primary on bg0", theme.text_primary, theme.bg0),
                ("text_primary on bg2", theme.text_primary, theme.bg2),
                ("text_secondary on bg0", theme.text_secondary, theme.bg0),
                ("on_accent on accent", theme.on_accent, theme.accent),
                ("accent on bg0", theme.accent, theme.bg0),
                (
                    "on_accent on accent_pressed",
                    theme.on_accent,
                    theme.accent_pressed,
                ),
                ("error on bg0", theme.error, theme.bg0),
            ];
            for (name, text, background) in text_pairs {
                let ratio = text.contrast_ratio(background);
                assert!(ratio >= text_limit, "{kind:?} {name}: {ratio}");
            }
            for (name, line, background) in [
                ("border_control on bg0", theme.border_control, theme.bg0),
                ("focus_ring on bg0", theme.focus_ring, theme.bg0),
            ] {
                let ratio = line.contrast_ratio(background);
                assert!(ratio >= 3.0, "{kind:?} {name}: {ratio}");
            }
        }
    }

    #[test]
    fn contrast_runs_from_one_to_twenty_one_and_themes_parse_by_name() {
        let (black, white) = (Rgba::hex(0x000000), Rgba::hex(0xFFFFFF));
        assert!((black.contrast_ratio(white) - 21.0).abs() < 0.01);
        assert!((white.contrast_ratio(white) - 1.0).abs() < 0.001);
        assert_eq!(
            ThemeKind::parse("high-contrast"),
            Some(ThemeKind::HighContrast)
        );
        assert_eq!(ThemeKind::parse("dark"), Some(ThemeKind::Dark));
        assert_eq!(ThemeKind::parse("sepia"), None);
        assert_eq!(Theme::of(ThemeKind::HighContrast).focus_ring_width(), 3.0);
        assert_eq!(Theme::of(ThemeKind::Light).focus_ring_width(), 2.0);
    }
}
