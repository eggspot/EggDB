use std::time::{Duration, Instant};

use eggdb_platform::window::{AccessNode, AccessRect};

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

/// The pointer's effect on how a screen draws: the node under it, the node a press on it
/// holds, and whether the keyboard focus last moved by a click, which keeps the focus ring off
/// buttons, links, checkboxes and tabs (`docs/ux/workspace.md` 6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PointerLook {
    /// The accessibility node id under the pointer, when it offers an action.
    pub hovered: Option<u32>,
    /// The node a primary press went down on, while the button is held.
    pub pressed: Option<u32>,
    /// Whether the last focus move came from the pointer rather than a key.
    pub focus_by_pointer: bool,
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
