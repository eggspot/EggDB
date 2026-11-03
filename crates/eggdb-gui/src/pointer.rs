use std::time::{Duration, Instant};

use eggdb_platform::window::{AccessNode, AccessRect};

use crate::color::Rgba;
use crate::connect_screen::{MIN_HEIGHT, MIN_WIDTH};
use crate::font::Typeface;

/// The longest pause between two presses that still makes them a double (or triple) click.
pub const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);
/// The farthest, in logical pixels on either axis, a press may be from the last one and still
/// count as its double (or triple) click.
pub const MULTI_CLICK_DISTANCE: f32 = 4.0;
/// How long the caret stays solid after a key or a click (`docs/ux/workspace.md` 6.2).
pub const CARET_SOLID_AFTER_INPUT: Duration = Duration::from_millis(500);
/// How long the blinking caret shows.
pub const CARET_ON: Duration = Duration::from_millis(530);
/// How long the blinking caret hides.
pub const CARET_OFF: Duration = Duration::from_millis(530);
/// The largest paste the editor and the inputs take, in bytes (`docs/ux/workspace.md` 6.3).
pub const MAX_PASTE_BYTES: usize = 1 << 20;
/// The toast a larger paste shows.
pub const PASTE_TOO_LARGE: &str = "Paste too large (limit 1 MB); open the file instead";
/// The lines one wheel notch scrolls the grid (`docs/ux/workspace.md` 3.6).
pub const WHEEL_ROWS_PER_NOTCH: f32 = 3.0;
/// How long a hover or pressed fill takes to reach its new colour (`docs/ux/workspace.md`
/// 10.8).
pub const FILL_FADE: Duration = Duration::from_millis(80);
/// How often the window draws while a fill fades.
pub const FADE_FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// The most fills that fade at once; one more ends the furthest along at once.
pub const MAX_FADES: usize = 4;
const WHOLE: u16 = 1_000;

/// The fill a control draws for the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fill {
    /// Neither hovered nor pressed.
    #[default]
    Rest,
    /// Under the pointer.
    Hovered,
    /// Held by a press with the pointer on it.
    Pressed,
}

/// A fill part of the way between the three [`Fill`] colours, as thousandths of each that add
/// up to 1000: what a fade draws at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillMix {
    /// Thousandths of the rest colour.
    pub rest: u16,
    /// Thousandths of the hover colour.
    pub hovered: u16,
    /// Thousandths of the pressed colour.
    pub pressed: u16,
}

impl FillMix {
    /// All of `fill`'s colour.
    pub fn of(fill: Fill) -> Self {
        let share = |wanted: Fill| if fill == wanted { WHOLE } else { 0 };
        Self {
            rest: share(Fill::Rest),
            hovered: share(Fill::Hovered),
            pressed: share(Fill::Pressed),
        }
    }

    /// The mix `progress` thousandths of the way (clamped at 1000) from `self` to `target`.
    pub fn toward(self, target: Fill, progress: u16) -> Self {
        let progress = u32::from(progress.min(WHOLE));
        let goal = Self::of(target);
        let step = |from: u16, to: u16| {
            let mixed = (u32::from(from) * (u32::from(WHOLE) - progress)
                + u32::from(to) * progress)
                / u32::from(WHOLE);
            u16::try_from(mixed).unwrap_or(WHOLE)
        };
        let rest = step(self.rest, goal.rest);
        let hovered = step(self.hovered, goal.hovered).min(WHOLE - rest);
        Self {
            rest,
            hovered,
            pressed: WHOLE - rest - hovered,
        }
    }

    /// Whether the mix is all of `fill`'s colour.
    pub fn is(self, fill: Fill) -> bool {
        self == Self::of(fill)
    }

    /// The colour the mix draws with `rest`, `hovered` and `pressed` as the three fills: a
    /// linear blend of each sRGB channel, opacity included.
    pub fn blend(self, rest: Rgba, hovered: Rgba, pressed: Rgba) -> Rgba {
        let channel = |of: fn(Rgba) -> u8| {
            let sum = u32::from(of(rest)) * u32::from(self.rest)
                + u32::from(of(hovered)) * u32::from(self.hovered)
                + u32::from(of(pressed)) * u32::from(self.pressed);
            u8::try_from((sum + u32::from(WHOLE) / 2) / u32::from(WHOLE)).unwrap_or(u8::MAX)
        };
        Rgba {
            r: channel(|color| color.r),
            g: channel(|color| color.g),
            b: channel(|color| color.b),
            a: channel(|color| color.a),
        }
    }
}

/// One node's fill on its way from the colour it drew to its new [`Fill`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillFade {
    /// The accessibility node id of the control.
    pub node: u32,
    /// The colour drawn when the fade started.
    pub from: FillMix,
    /// The fill the fade ends on.
    pub to: Fill,
    /// When the fade started.
    pub started: Instant,
    /// How far it has come, in thousandths of [`FILL_FADE`].
    pub progress: u16,
}

impl FillFade {
    fn mix(&self) -> FillMix {
        self.from.toward(self.to, self.progress)
    }
}

/// The pointer's effect on how a screen draws: the node under it, the node a press on it
/// holds, whether the keyboard focus last moved by a click, which keeps the focus ring off
/// buttons, links, checkboxes and tabs (`docs/ux/workspace.md` 6.1), and the fills fading to
/// their new colour (10.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PointerLook {
    /// The accessibility node id under the pointer, when it offers an action.
    pub hovered: Option<u32>,
    /// The node a primary press went down on, while the button is held.
    pub pressed: Option<u32>,
    /// Whether the last focus move came from the pointer rather than a key.
    pub focus_by_pointer: bool,
    /// The fills still fading, at most [`MAX_FADES`].
    pub fades: [Option<FillFade>; MAX_FADES],
}

impl PointerLook {
    /// Whether node `id` draws its hover fill: under the pointer and not held by a press on
    /// another node.
    pub fn is_hovered(&self, id: u32) -> bool {
        self.hovered == Some(id) && self.pressed.is_none_or(|pressed| pressed == id)
    }

    /// Whether node `id` draws its pressed fill: held by a press with the pointer still on it.
    pub fn is_pressed(&self, id: u32) -> bool {
        self.pressed == Some(id) && self.hovered == Some(id)
    }

    /// The fill node `id` ends on.
    pub fn fill(&self, id: u32) -> Fill {
        if self.is_pressed(id) {
            Fill::Pressed
        } else if self.is_hovered(id) {
            Fill::Hovered
        } else {
            Fill::Rest
        }
    }

    /// The fill node `id` draws now: part of the way through its fade, or its [`fill`](Self::fill).
    pub fn mix(&self, id: u32) -> FillMix {
        self.fades
            .iter()
            .flatten()
            .find(|fade| fade.node == id)
            .map_or_else(|| FillMix::of(self.fill(id)), FillFade::mix)
    }

    /// Whether any fill is still fading.
    pub fn is_fading(&self) -> bool {
        self.fades.iter().any(Option::is_some)
    }

    /// Ends every fade at once, as reduced motion asks.
    pub fn stop_fades(&mut self) {
        self.fades = [None; MAX_FADES];
    }

    /// Starts an 80 ms fade at `now` for each node whose fill changed since the pointer was at
    /// `before` (hovered, pressed): from the colour it drew then, so a quick in-and-out never
    /// jumps.
    pub fn follow(&mut self, before: (Option<u32>, Option<u32>), now: Instant) {
        let earlier = PointerLook {
            hovered: before.0,
            pressed: before.1,
            ..*self
        };
        let mut nodes = [before.0, before.1, self.hovered, self.pressed];
        nodes.sort_unstable();
        let mut last = None;
        for id in nodes.into_iter().flatten() {
            if last == Some(id) {
                continue;
            }
            last = Some(id);
            let to = self.fill(id);
            if earlier.fill(id) != to {
                self.start_fade(id, earlier.mix(id), to, now);
            }
        }
    }

    fn start_fade(&mut self, node: u32, from: FillMix, to: Fill, now: Instant) {
        let fade = FillFade {
            node,
            from,
            to,
            started: now,
            progress: 0,
        };
        let slot = self
            .fades
            .iter()
            .position(|kept| kept.is_some_and(|kept| kept.node == node))
            .or_else(|| self.fades.iter().position(Option::is_none))
            .or_else(|| {
                (0..MAX_FADES).max_by_key(|&at| self.fades[at].map_or(0, |kept| kept.progress))
            });
        if let Some(slot) = slot {
            self.fades[slot] = Some(fade);
        }
    }

    /// Moves every fade on to `now` and drops the finished ones. Returns whether a fill's
    /// colour changed.
    pub fn advance(&mut self, now: Instant) -> bool {
        let mut changed = false;
        for slot in &mut self.fades {
            let Some(fade) = slot else {
                continue;
            };
            let elapsed = now.saturating_duration_since(fade.started).as_millis();
            let progress = elapsed * u128::from(WHOLE) / FILL_FADE.as_millis();
            let progress = u16::try_from(progress.min(u128::from(WHOLE))).unwrap_or(WHOLE);
            if progress != fade.progress {
                fade.progress = progress;
                changed = true;
            }
            if progress >= WHOLE {
                *slot = None;
            }
        }
        changed
    }
}

/// How the focused editor's or input's caret draws at this moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaretLook {
    /// Drawn in `accent`.
    #[default]
    On,
    /// The blink's hidden half: not drawn.
    Off,
    /// The window is not active: drawn solid in `textDisabled`.
    Inactive,
}

impl CaretLook {
    /// Whether the caret is drawn at all.
    pub fn shown(self) -> bool {
        self != Self::Off
    }
}

fn blink_phase(since_input: Duration) -> Duration {
    let period = (CARET_ON + CARET_OFF).as_nanos();
    let into = since_input
        .saturating_sub(CARET_SOLID_AFTER_INPUT)
        .as_nanos()
        % period;
    Duration::from_nanos(u64::try_from(into).unwrap_or(0))
}

/// The caret `since_input` after the last key or click: solid for
/// [`CARET_SOLID_AFTER_INPUT`], then [`CARET_ON`] shown and [`CARET_OFF`] hidden in turn;
/// always shown under reduced motion, and solid in `textDisabled` while the window is not
/// active.
pub fn caret_look(since_input: Duration, reduced_motion: bool, window_active: bool) -> CaretLook {
    if !window_active {
        return CaretLook::Inactive;
    }
    if reduced_motion || since_input < CARET_SOLID_AFTER_INPUT {
        return CaretLook::On;
    }
    if blink_phase(since_input) < CARET_ON {
        CaretLook::On
    } else {
        CaretLook::Off
    }
}

/// How long until [`caret_look`] changes, `None` when it stays as it is (reduced motion, or
/// the window inactive), so the event loop knows when to draw again.
pub fn until_caret_changes(
    since_input: Duration,
    reduced_motion: bool,
    window_active: bool,
) -> Option<Duration> {
    if !window_active || reduced_motion {
        return None;
    }
    if since_input < CARET_SOLID_AFTER_INPUT {
        return Some(CARET_SOLID_AFTER_INPUT + CARET_ON - since_input);
    }
    let phase = blink_phase(since_input);
    Some(if phase < CARET_ON {
        CARET_ON - phase
    } else {
        CARET_ON + CARET_OFF - phase
    })
}

/// Whether (`x`, `y`) lies inside `bounds`, edges left and top included.
pub fn contains(bounds: AccessRect, x: f32, y: f32) -> bool {
    x >= bounds.x && y >= bounds.y && x < bounds.x + bounds.width && y < bounds.y + bounds.height
}

/// The node of `nodes` a click at (`x`, `y`) logical pixels reaches: the last one in reading
/// order (children come after their parents, so the innermost) whose bounds hold the point
/// and that offers an action and is not disabled.
pub fn hit_test(nodes: &[AccessNode], x: f32, y: f32) -> Option<&AccessNode> {
    nodes.iter().rev().find(|node| {
        let actions = node.actions;
        let acts = actions.focus
            || actions.invoke
            || actions.toggle
            || actions.select
            || actions.expand_collapse;
        acts && !node.state.disabled && contains(node.bounds, x, y)
    })
}

/// Counts presses into single, double and triple clicks: a press within
/// [`MULTI_CLICK_INTERVAL`] and [`MULTI_CLICK_DISTANCE`] of the last one adds one, up to
/// three, after which it starts again at one.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ClickCounter {
    last: Option<(Instant, f32, f32)>,
    count: u8,
}

impl ClickCounter {
    /// Counts a press at (`x`, `y`) at `now`: 1, 2 or 3.
    pub fn press(&mut self, now: Instant, x: f32, y: f32) -> u8 {
        let follows = self.last.is_some_and(|(at, last_x, last_y)| {
            now.saturating_duration_since(at) <= MULTI_CLICK_INTERVAL
                && (x - last_x).abs() <= MULTI_CLICK_DISTANCE
                && (y - last_y).abs() <= MULTI_CLICK_DISTANCE
        });
        self.count = if follows && self.count < 3 {
            self.count + 1
        } else {
            1
        };
        self.last = Some((now, x, y));
        self.count
    }
}

/// `size` in logical pixels, no smaller than the window's minimum of `MIN_WIDTH` ×
/// `MIN_HEIGHT` (800 × 560), for window managers that do not keep to the minimum size the
/// window asked for.
pub fn clamp_to_minimum(size: (u32, u32)) -> (u32, u32) {
    (size.0.max(MIN_WIDTH), size.1.max(MIN_HEIGHT))
}

/// The clipboard's `raw` text as a paste inserts it: `\r\n` and lone `\r` become `\n`; more
/// than [`MAX_PASTE_BYTES`] is refused with [`PASTE_TOO_LARGE`].
pub fn paste_text(raw: &str) -> Result<String, &'static str> {
    if raw.len() > MAX_PASTE_BYTES {
        return Err(PASTE_TOO_LARGE);
    }
    Ok(raw.replace("\r\n", "\n").replace('\r', "\n"))
}

/// A paste's text for a single-line input: line breaks and tabs become spaces, a final line
/// break is dropped, and other control characters go.
pub fn single_line(text: &str) -> String {
    text.trim_end_matches(['\n', '\r'])
        .chars()
        .filter_map(|character| match character {
            '\n' | '\r' | '\t' => Some(' '),
            other if other.is_control() => None,
            other => Some(other),
        })
        .collect()
}

/// The character boundary of `text`, drawn in `face` at `size` from x = 0, nearest to `x`:
/// what a click at `x` puts the caret before.
pub fn column_at(face: &dyn Typeface, size: f32, text: &str, x: f32) -> usize {
    let mut pen = 0.0;
    for (column, character) in text.chars().enumerate() {
        let advance = face.advance(character, size);
        if x < pen + advance / 2.0 {
            return column;
        }
        pen += advance;
    }
    text.chars().count()
}

/// The word around column `column` of `line` as a column range: a run of letters, digits and
/// `_`, or of one other character, as a double-click selects it.
pub fn word_at(line: &str, column: usize) -> (usize, usize) {
    let characters: Vec<char> = line.chars().collect();
    if characters.is_empty() {
        return (0, 0);
    }
    let at = column.min(characters.len() - 1);
    let wordy = |character: char| character.is_alphanumeric() || character == '_';
    if !wordy(characters[at]) {
        return (at, at + 1);
    }
    let start = characters[..at]
        .iter()
        .rposition(|&character| !wordy(character))
        .map_or(0, |before| before + 1);
    let end = characters[at..]
        .iter()
        .position(|&character| !wordy(character))
        .map_or(characters.len(), |after| at + after);
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::BoxFace;
    use eggdb_platform::window::{AccessActions, AccessRole};

    fn node(id: u32, role: AccessRole, bounds: (f32, f32, f32, f32), acts: bool) -> AccessNode {
        AccessNode {
            id,
            role,
            bounds: AccessRect {
                x: bounds.0,
                y: bounds.1,
                width: bounds.2,
                height: bounds.3,
            },
            actions: AccessActions {
                focus: acts,
                invoke: acts,
                ..AccessActions::default()
            },
            ..AccessNode::default()
        }
    }

    #[test]
    fn a_click_reaches_the_innermost_node_that_acts() {
        let nodes = vec![
            node(1, AccessRole::Window, (0.0, 0.0, 800.0, 600.0), false),
            node(2, AccessRole::Toolbar, (0.0, 0.0, 800.0, 44.0), false),
            node(3, AccessRole::Button, (232.0, 8.0, 72.0, 28.0), true),
            node(4, AccessRole::TextArea, (260.0, 80.0, 500.0, 300.0), true),
        ];
        assert_eq!(hit_test(&nodes, 240.0, 20.0).map(|node| node.id), Some(3));
        assert_eq!(
            hit_test(&nodes, 304.0, 20.0),
            None,
            "the right edge is outside"
        );
        assert_eq!(hit_test(&nodes, 600.0, 90.0).map(|node| node.id), Some(4));
        assert_eq!(
            hit_test(&nodes, 100.0, 20.0),
            None,
            "the toolbar itself does nothing"
        );
        let mut disabled = nodes.clone();
        disabled[2].state.disabled = true;
        assert_eq!(hit_test(&disabled, 240.0, 20.0), None);
    }

    #[test]
    fn hover_and_pressed_follow_the_pointer_and_the_press() {
        let mut look = PointerLook {
            hovered: Some(3),
            ..PointerLook::default()
        };
        assert!(look.is_hovered(3) && !look.is_pressed(3));
        look.pressed = Some(3);
        assert!(look.is_pressed(3));
        look.hovered = Some(4);
        assert!(!look.is_pressed(3), "dragged off, the press shows no more");
        assert!(
            !look.is_hovered(4),
            "a press on another node keeps the hover off"
        );
    }

    #[test]
    fn a_fill_mix_moves_linearly_toward_its_target_and_blends_each_channel() {
        let rest = FillMix::of(Fill::Rest);
        assert_eq!(
            rest.toward(Fill::Hovered, 500),
            FillMix {
                rest: 500,
                hovered: 500,
                pressed: 0
            }
        );
        assert!(rest.toward(Fill::Hovered, 1_000).is(Fill::Hovered));
        assert!(rest.toward(Fill::Hovered, 4_000).is(Fill::Hovered));
        assert!(rest.toward(Fill::Hovered, 0).is(Fill::Rest));
        let third = rest.toward(Fill::Pressed, 333);
        assert_eq!(third.rest + third.hovered + third.pressed, 1_000);
        let black = Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        };
        let white = Rgba::hex(0xFF_FFFF);
        assert_eq!(
            rest.toward(Fill::Hovered, 500).blend(black, white, black),
            Rgba {
                r: 128,
                g: 128,
                b: 128,
                a: 128
            }
        );
        assert_eq!(FillMix::of(Fill::Pressed).blend(black, black, white), white);
    }

    #[test]
    fn a_hover_fades_in_over_80_ms_and_a_quick_way_out_starts_from_the_drawn_colour() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut look = PointerLook {
            hovered: Some(3),
            ..PointerLook::default()
        };
        look.follow((None, None), at(0));
        assert!(look.is_fading());
        assert!(look.mix(3).is(Fill::Rest), "the first frame draws the old fill");
        assert!(look.advance(at(40)));
        assert_eq!(look.mix(3).hovered, 500);
        look.hovered = None;
        look.follow((Some(3), None), at(40));
        assert_eq!(look.mix(3).hovered, 500, "no jump when the pointer leaves");
        look.advance(at(80));
        assert_eq!(look.mix(3).hovered, 250);
        look.advance(at(120));
        assert!(look.mix(3).is(Fill::Rest));
        assert!(!look.is_fading(), "a finished fade is dropped");
        assert!(!look.advance(at(200)), "nothing left to move");
    }

    #[test]
    fn a_press_fades_from_hover_and_at_most_four_fills_fade_at_once() {
        let start = Instant::now();
        let mut look = PointerLook {
            hovered: Some(1),
            pressed: Some(1),
            ..PointerLook::default()
        };
        look.follow((Some(1), None), start);
        assert!(look.mix(1).is(Fill::Hovered));
        look.advance(start + FILL_FADE);
        assert!(look.mix(1).is(Fill::Pressed));
        let mut look = PointerLook::default();
        for id in 1..=6 {
            let before = (look.hovered, None);
            look.hovered = Some(id);
            look.follow(before, start);
        }
        assert_eq!(look.fades.iter().flatten().count(), MAX_FADES);
        assert!(look.mix(6).is(Fill::Rest), "the newest hover is fading in");
        look.stop_fades();
        assert!(!look.is_fading());
        assert!(look.mix(6).is(Fill::Hovered), "stopped fades show their end");
    }

    #[test]
    fn the_caret_is_solid_then_blinks_530_ms_each_way() {
        let at = |ms: u64| caret_look(Duration::from_millis(ms), false, true);
        assert_eq!(at(0), CaretLook::On);
        assert_eq!(at(400), CaretLook::On);
        assert_eq!(at(1_029), CaretLook::On);
        assert_eq!(at(1_030), CaretLook::Off);
        assert_eq!(at(1_100), CaretLook::Off);
        assert_eq!(at(1_559), CaretLook::Off);
        assert_eq!(at(1_560), CaretLook::On);
        assert_eq!(at(1_700), CaretLook::On);
        for ms in [0, 400, 1_100, 1_700] {
            assert_eq!(
                caret_look(Duration::from_millis(ms), true, true),
                CaretLook::On
            );
            assert_eq!(
                caret_look(Duration::from_millis(ms), false, false),
                CaretLook::Inactive
            );
        }
        assert!(!CaretLook::Off.shown() && CaretLook::Inactive.shown());
    }

    #[test]
    fn the_next_caret_change_is_when_the_blink_turns() {
        let until = |ms: u64| until_caret_changes(Duration::from_millis(ms), false, true);
        assert_eq!(until(0), Some(Duration::from_millis(1_030)));
        assert_eq!(until(400), Some(Duration::from_millis(630)));
        assert_eq!(until(1_100), Some(Duration::from_millis(460)));
        assert_eq!(until(1_700), Some(Duration::from_millis(390)));
        assert_eq!(until_caret_changes(Duration::ZERO, true, true), None);
        assert_eq!(until_caret_changes(Duration::ZERO, false, false), None);
    }

    #[test]
    fn presses_close_in_time_and_place_count_up_to_three() {
        let start = Instant::now();
        let mut clicks = ClickCounter::default();
        assert_eq!(clicks.press(start, 600.0, 90.0), 1);
        assert_eq!(
            clicks.press(start + Duration::from_millis(200), 602.0, 91.0),
            2
        );
        assert_eq!(
            clicks.press(start + Duration::from_millis(400), 601.0, 90.0),
            3
        );
        assert_eq!(
            clicks.press(start + Duration::from_millis(500), 601.0, 90.0),
            1
        );
        assert_eq!(
            clicks.press(start + Duration::from_millis(1_200), 601.0, 90.0),
            1
        );
        assert_eq!(
            clicks.press(start + Duration::from_millis(1_300), 640.0, 90.0),
            1
        );
    }

    #[test]
    fn sizes_below_the_minimum_are_clamped_to_it() {
        assert_eq!(clamp_to_minimum((640, 400)), (800, 560));
        assert_eq!(clamp_to_minimum((1024, 500)), (1024, 560));
        assert_eq!(clamp_to_minimum((1440, 800)), (1440, 800));
    }

    #[test]
    fn pastes_normalise_line_breaks_and_refuse_more_than_a_megabyte() {
        assert_eq!(
            paste_text("SELECT 1;\r\nSELECT 2;\rSELECT 3;").as_deref(),
            Ok("SELECT 1;\nSELECT 2;\nSELECT 3;")
        );
        assert_eq!(
            paste_text(&"x".repeat(MAX_PASTE_BYTES)).map(|text| text.len()),
            Ok(MAX_PASTE_BYTES)
        );
        assert_eq!(
            paste_text(&"x".repeat(MAX_PASTE_BYTES + 1)),
            Err(PASTE_TOO_LARGE)
        );
        assert_eq!(single_line("shop.eggdb\n"), "shop.eggdb");
        assert_eq!(single_line("a\tb\nc\u{7}"), "a b c");
    }

    #[test]
    fn a_click_column_is_the_nearest_character_boundary() {
        let advance = BoxFace.advance('x', 13.0);
        assert_eq!(column_at(&BoxFace, 13.0, "SELECT", 0.0), 0);
        assert_eq!(column_at(&BoxFace, 13.0, "SELECT", advance * 0.4), 0);
        assert_eq!(column_at(&BoxFace, 13.0, "SELECT", advance * 0.6), 1);
        assert_eq!(column_at(&BoxFace, 13.0, "SELECT", advance * 2.5 + 0.01), 3);
        assert_eq!(column_at(&BoxFace, 13.0, "SELECT", 10_000.0), 6);
        assert_eq!(column_at(&BoxFace, 13.0, "", 50.0), 0);
    }

    #[test]
    fn a_double_click_selects_the_word_or_the_single_mark() {
        let line = "SELECT id, user_name FROM t;";
        assert_eq!(word_at(line, 0), (0, 6));
        assert_eq!(word_at(line, 14), (11, 20));
        assert_eq!(word_at(line, 9), (9, 10));
        assert_eq!(word_at(line, 1_000), (27, 28));
        assert_eq!(word_at("", 3), (0, 0));
    }
}
