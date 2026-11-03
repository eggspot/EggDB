use eggdb_platform::window::{Key, KeyPress};

use crate::layout::Layout;
use crate::settings::ClientSettings;

/// The sidebar's default width.
pub const SIDEBAR_DEFAULT: f32 = 260.0;
/// The sidebar's narrowest width.
pub const SIDEBAR_MIN: f32 = 180.0;
/// The sidebar's widest width.
pub const SIDEBAR_MAX: f32 = 480.0;
/// The docked inspector's default width.
pub const INSPECTOR_DEFAULT: f32 = 280.0;
/// The docked inspector's narrowest width.
pub const INSPECTOR_MIN: f32 = 200.0;
/// The docked inspector's widest width.
pub const INSPECTOR_MAX: f32 = 480.0;
/// The editor's lowest height above the editor/results splitter.
pub const EDITOR_MIN: f32 = 80.0;
/// The results' lowest height below the editor/results splitter.
pub const RESULTS_MIN: f32 = 120.0;
/// The rail's width while the sidebar is collapsed (`docs/ux/workspace.md` 10.6).
pub const RAIL_WIDTH: f32 = 36.0;
/// A splitter band's thickness.
pub const BAND: f32 = 6.0;
/// Where the editor starts, below the tab strip.
pub const CENTRE_TOP: f32 = 80.0;
/// Where the results end, above the status bar.
pub const CENTRE_BOTTOM: f32 = 776.0;
/// The editor's and the results' heights together at the golden frame's 800 px height.
pub const SPLIT_HEIGHT: f32 = CENTRE_BOTTOM - CENTRE_TOP - BAND;
/// The results' share of [`SPLIT_HEIGHT`] that keeps the golden frame's splitter at y 380.
pub const RESULTS_RATIO_DEFAULT: f32 = 390.0 / SPLIT_HEIGHT;
/// How far an arrow key moves a splitter.
pub const KEY_STEP: f32 = 8.0;
/// How far `Shift` and an arrow key move a splitter.
pub const SHIFT_KEY_STEP: f32 = 32.0;

/// One of the window's three splitters (`docs/ux/workspace.md` 10.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Splitter {
    /// Between the sidebar and the centre.
    Sidebar,
    /// Between the editor and the results.
    Results,
    /// Between the centre and the docked inspector.
    Inspector,
}

impl Splitter {
    /// The region it sizes, as the status bar names it.
    pub fn region(self) -> &'static str {
        match self {
            Self::Sidebar => "Sidebar",
            Self::Results => "Results",
            Self::Inspector => "Inspector",
        }
    }

    /// The size it sets, as a screen reader names it.
    pub fn dimension(self) -> &'static str {
        match self {
            Self::Sidebar | Self::Inspector => "width",
            Self::Results => "height",
        }
    }

    /// Whether it moves left and right, rather than up and down.
    pub fn is_vertical(self) -> bool {
        !matches!(self, Self::Results)
    }
}

/// The sizes the splitters and `Ctrl/Cmd+B` set, kept in the client's settings per layout.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RegionSizes {
    /// The sidebar's width while it shows.
    pub sidebar_width: f32,
    /// The results' share of the editor's and results' heights together.
    pub results_ratio: f32,
    /// The docked inspector's width.
    pub inspector_width: f32,
    /// Whether the sidebar shows; `false` collapses it to the rail.
    pub sidebar_shown: bool,
    /// Whether the inspector docks when the window is wide enough for it.
    pub inspector_shown: bool,
}

impl Default for RegionSizes {
    fn default() -> Self {
        Self {
            sidebar_width: SIDEBAR_DEFAULT,
            results_ratio: RESULTS_RATIO_DEFAULT,
            inspector_width: INSPECTOR_DEFAULT,
            sidebar_shown: true,
            inspector_shown: true,
        }
    }
}

impl RegionSizes {
    /// The sizes `settings` keeps for `layout`, each clamped to its range; a missing or
    /// unreadable line keeps its default.
    pub fn from_settings(settings: &ClientSettings, layout: Layout) -> Self {
        let number = |name: &str| {
            settings
                .value(&setting_name(layout, name))
                .and_then(|value| value.trim().parse::<f32>().ok())
                .filter(|value| value.is_finite())
        };
        let flag = |name: &str| match settings.value(&setting_name(layout, name)).map(str::trim) {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => None,
        };
        let default = Self::default();
        Self {
            sidebar_width: number("sidebar_width").unwrap_or(default.sidebar_width),
            results_ratio: number("results_ratio").unwrap_or(default.results_ratio),
            inspector_width: number("inspector_width").unwrap_or(default.inspector_width),
            sidebar_shown: flag("sidebar_shown").unwrap_or(default.sidebar_shown),
            inspector_shown: flag("inspector_shown").unwrap_or(default.inspector_shown),
        }
        .clamped()
    }

    /// Writes the sizes into `settings` as `layout`'s lines, such as
    /// `eggdb.sidebar_width = 268`.
    pub fn write_to(&self, settings: &mut ClientSettings, layout: Layout) {
        let lines = [
            ("sidebar_width", format!("{}", self.sidebar_width.round())),
            ("results_ratio", format!("{:.4}", self.results_ratio)),
            ("inspector_width", format!("{}", self.inspector_width.round())),
            ("sidebar_shown", self.sidebar_shown.to_string()),
            ("inspector_shown", self.inspector_shown.to_string()),
        ];
        for (name, value) in lines {
            settings.set(&setting_name(layout, name), &value);
        }
    }

    /// The sizes with each in its range.
    pub fn clamped(self) -> Self {
        let lowest = RESULTS_MIN / SPLIT_HEIGHT;
        let highest = (SPLIT_HEIGHT - EDITOR_MIN) / SPLIT_HEIGHT;
        Self {
            sidebar_width: self.sidebar_width.clamp(SIDEBAR_MIN, SIDEBAR_MAX).round(),
            results_ratio: self.results_ratio.clamp(lowest, highest),
            inspector_width: self.inspector_width.clamp(INSPECTOR_MIN, INSPECTOR_MAX).round(),
            ..self
        }
    }

    /// The results' height below the editor/results splitter.
    pub fn results_height(&self) -> f32 {
        (self.results_ratio * SPLIT_HEIGHT)
            .round()
            .clamp(RESULTS_MIN, SPLIT_HEIGHT - EDITOR_MIN)
    }

    /// Where the editor/results splitter's band starts: y 380 by default.
    pub fn results_band_top(&self) -> f32 {
        CENTRE_BOTTOM - self.results_height() - BAND
    }

    /// The size `splitter` sets, in pixels.
    pub fn size(&self, splitter: Splitter) -> f32 {
        match splitter {
            Splitter::Sidebar => self.sidebar_width,
            Splitter::Results => self.results_height(),
            Splitter::Inspector => self.inspector_width,
        }
    }

    /// Sets the size `splitter` sets, clamped; whether it changed.
    pub fn set_size(&mut self, splitter: Splitter, size: f32) -> bool {
        let before = *self;
        match splitter {
            Splitter::Sidebar => self.sidebar_width = size,
            Splitter::Results => self.results_ratio = size / SPLIT_HEIGHT,
            Splitter::Inspector => self.inspector_width = size,
        }
        *self = self.clamped();
        *self != before
    }

    /// Answers a key on the focused `splitter`: arrows move it 8 px (32 with `Shift`), `Home`
    /// and `End` set its region's smallest and largest size, `Enter` the default. `None` for a
    /// key it does not take; otherwise whether the size changed.
    pub fn key(&mut self, splitter: Splitter, press: KeyPress) -> Option<bool> {
        let step = if press.modifiers.shift {
            SHIFT_KEY_STEP
        } else {
            KEY_STEP
        };
        let size = self.size(splitter);
        let wanted = match (press.key, splitter) {
            (Key::Right, Splitter::Sidebar) | (Key::Left, Splitter::Inspector) => size + step,
            (Key::Left, Splitter::Sidebar) | (Key::Right, Splitter::Inspector) => size - step,
            (Key::Up, Splitter::Results) => size + step,
            (Key::Down, Splitter::Results) => size - step,
            (Key::Home, _) => 0.0,
            (Key::End, _) => f32::MAX,
            (Key::Enter, _) => Self::default().size(splitter),
            _ => return None,
        };
        Some(self.set_size(splitter, wanted))
    }

    /// The status bar's notice while `splitter` moves: `Sidebar · 268 px`.
    pub fn notice(&self, splitter: Splitter) -> String {
        format!("{} · {} px", splitter.region(), self.size(splitter).round())
    }

    /// What a screen reader announces when `splitter` moves: `Sidebar width, 268 pixels`.
    pub fn spoken(&self, splitter: Splitter) -> String {
        format!(
            "{} {}, {} pixels",
            splitter.region(),
            splitter.dimension(),
            self.size(splitter).round()
        )
    }
}

fn setting_name(layout: Layout, name: &str) -> String {
    format!("{}.{name}", layout.key())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(key: Key, shift: bool) -> KeyPress {
        let mut press = KeyPress::plain(key);
        press.modifiers.shift = shift;
        press
    }

    #[test]
    fn the_defaults_keep_the_golden_frames_regions() {
        let sizes = RegionSizes::default();
        assert_eq!(sizes.sidebar_width, 260.0);
        assert_eq!(sizes.results_band_top(), 380.0);
        assert_eq!(sizes.results_height(), 390.0);
        assert_eq!(sizes.inspector_width, 280.0);
    }

    #[test]
    fn keys_move_a_splitter_in_steps_and_stop_at_its_range() {
        let mut sizes = RegionSizes::default();
        assert_eq!(sizes.key(Splitter::Sidebar, press(Key::Right, false)), Some(true));
        assert_eq!(sizes.sidebar_width, 268.0);
        assert_eq!(sizes.notice(Splitter::Sidebar), "Sidebar · 268 px");
        assert_eq!(sizes.spoken(Splitter::Sidebar), "Sidebar width, 268 pixels");
        sizes.key(Splitter::Sidebar, press(Key::Left, true));
        assert_eq!(sizes.sidebar_width, 236.0);
        sizes.key(Splitter::Sidebar, press(Key::Home, false));
        assert_eq!(sizes.sidebar_width, SIDEBAR_MIN);
        assert_eq!(sizes.key(Splitter::Sidebar, press(Key::Left, false)), Some(false));
        sizes.key(Splitter::Sidebar, press(Key::End, false));
        assert_eq!(sizes.sidebar_width, SIDEBAR_MAX);
        sizes.key(Splitter::Sidebar, press(Key::Enter, false));
        assert_eq!(sizes.sidebar_width, SIDEBAR_DEFAULT);
        sizes.key(Splitter::Results, press(Key::Up, false));
        assert_eq!(sizes.results_height(), 398.0);
        assert_eq!(sizes.results_band_top(), 372.0);
        sizes.key(Splitter::Results, press(Key::End, false));
        assert_eq!(sizes.results_height(), SPLIT_HEIGHT - EDITOR_MIN);
        sizes.key(Splitter::Results, press(Key::Home, false));
        assert_eq!(sizes.results_height(), RESULTS_MIN);
        assert_eq!(sizes.notice(Splitter::Results), "Results · 120 px");
        sizes.key(Splitter::Inspector, press(Key::Left, false));
        assert_eq!(sizes.inspector_width, 288.0, "Left widens the inspector on the right");
        assert_eq!(sizes.key(Splitter::Inspector, press(Key::Character('a'), false)), None);
    }

    #[test]
    fn sizes_are_kept_per_layout_in_the_settings_and_clamped_when_read() {
        let mut settings = ClientSettings::parse("theme = dark\n");
        let sizes = RegionSizes {
            sidebar_width: 268.0,
            sidebar_shown: false,
            ..RegionSizes::default()
        };
        sizes.write_to(&mut settings, Layout::EggDb);
        let text = settings.render();
        assert!(text.contains("eggdb.sidebar_width = 268\n"), "{text}");
        assert!(text.contains("eggdb.sidebar_shown = false\n"), "{text}");
        assert!(text.contains("theme = dark\n"), "{text}");
        let read = RegionSizes::from_settings(&ClientSettings::parse(&text), Layout::EggDb);
        assert_eq!(read.sidebar_width, 268.0);
        assert!(!read.sidebar_shown && read.inspector_shown);
        assert_eq!(
            RegionSizes::from_settings(&ClientSettings::parse(&text), Layout::Ssms),
            RegionSizes::default(),
            "another layout keeps its own sizes"
        );
        let hostile = ClientSettings::parse(
            "eggdb.sidebar_width = 9000\neggdb.results_ratio = NaN\neggdb.inspector_width = x\n",
        );
        let read = RegionSizes::from_settings(&hostile, Layout::EggDb);
        assert_eq!(read.sidebar_width, SIDEBAR_MAX);
        assert_eq!(read.results_ratio, RESULTS_RATIO_DEFAULT);
        assert_eq!(read.inspector_width, INSPECTOR_DEFAULT);
    }
}
