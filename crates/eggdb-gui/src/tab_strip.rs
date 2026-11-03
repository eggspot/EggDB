use eggdb::sql::TransactionStatus;
use eggdb_platform::window::{AccessNode, AccessRect, AccessRole, Key, KeyPress};

use crate::canvas::Rect;
use crate::font::Typeface;
use crate::main_window::{BODY, Button, Painter};
use crate::query_tab::QueryTab;

/// A tab's width while every tab fits beside the `+` button.
pub const WIDEST_TAB: f32 = 140.0;
/// The narrowest a tab gets before the strip scrolls.
pub const NARROWEST_TAB: f32 = 100.0;
/// How far one wheel notch scrolls the strip.
pub const WHEEL_STEP: f32 = 100.0;
const PLUS_ROOM: f32 = 36.0;
const PINNED_PLUS_ROOM: f32 = 44.0;
const DIALOG_WIDTH: f32 = 420.0;
const DIALOG_HEIGHT: f32 = 160.0;
const DIALOG_BODY_WIDTH: f32 = 372.0;
const DIALOG_BUTTON_TOP: f32 = 108.0;
const DIALOG_BUTTONS_RIGHT: f32 = 396.0;
const DIALOG_BUTTON_GAP: f32 = 8.0;
const DIALOG_BUTTON_MIN: f32 = 72.0;
const TITLE_STYLE: (f32, f32) = (15.0, 20.0);
const LABEL_ADVANCE: f32 = 7.5;

/// The dialog's node.
pub const NODE_CLOSE_DIALOG: u32 = 64_000;
/// The dialog's button that keeps the tab.
pub const NODE_CLOSE_KEEP: u32 = 64_001;
/// The dialog's default button, which closes the tab.
pub const NODE_CLOSE_CONFIRM: u32 = 64_002;

/// The width of each of `count` tabs in a strip `strip_width` wide (`docs/ux/workspace.md`
/// 9.1): 140 while they fit beside `+`, else shared out down to 100.
pub fn tab_width(count: usize, strip_width: f32) -> f32 {
    let count = count.max(1) as f32;
    if count * WIDEST_TAB + PLUS_ROOM <= strip_width {
        WIDEST_TAB
    } else {
        ((strip_width - PLUS_ROOM) / count)
            .floor()
            .max(NARROWEST_TAB)
    }
}

/// Whether `count` tabs overflow the strip, so it scrolls and `+` is pinned at its right.
pub fn scrolls(count: usize, strip_width: f32) -> bool {
    count as f32 * tab_width(count, strip_width) + PLUS_ROOM > strip_width
}

/// The part of the strip the tabs show in: all of it while they fit, else all but the
/// pinned `+`.
pub fn viewport_width(count: usize, strip_width: f32) -> f32 {
    if scrolls(count, strip_width) {
        strip_width - PINNED_PLUS_ROOM
    } else {
        strip_width
    }
}

/// The largest scroll offset `count` tabs allow.
pub fn max_scroll(count: usize, strip_width: f32) -> f32 {
    let content = count as f32 * tab_width(count, strip_width);
    (content - viewport_width(count, strip_width)).max(0.0)
}

/// The scroll offset after `scroll` moves only as far as needed to show the whole tab at
/// `position`; 0 while the tabs fit.
pub fn scroll_to_show(scroll: f32, position: usize, count: usize, strip_width: f32) -> f32 {
    if !scrolls(count, strip_width) {
        return 0.0;
    }
    let width = tab_width(count, strip_width);
    let view = viewport_width(count, strip_width);
    let left = position as f32 * width;
    let right = left + width;
    let moved = if left < scroll {
        left
    } else if right > scroll + view {
        right - view
    } else {
        scroll
    };
    moved.clamp(0.0, max_scroll(count, strip_width))
}

/// The `+` button's left edge, from the strip's left edge `left`.
pub fn plus_x(count: usize, strip_width: f32, left: f32) -> f32 {
    if scrolls(count, strip_width) {
        left + strip_width - PLUS_ROOM
    } else {
        left + count as f32 * tab_width(count, strip_width) + 8.0
    }
}

/// The lowest `Query n` title none of `titles` has.
pub fn lowest_free_title<'t>(titles: impl Iterator<Item = &'t str> + Clone) -> String {
    (1..)
        .map(|number| format!("Query {number}"))
        .find(|title| !titles.clone().any(|taken| taken == title))
        .unwrap_or_else(|| "Query".to_owned())
}

/// Why closing a tab asks first (`docs/ux/workspace.md` 9.1), by priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// A statement runs in it.
    Running,
    /// Its session has a transaction open or failed.
    Transaction,
    /// Its editor holds text.
    Unsaved,
}

/// What a key or click in the close dialog decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAnswer {
    /// The dialog stays open.
    Pending,
    /// Close the tab (stopping its run and rolling back its transaction).
    Close,
    /// Keep the tab.
    Keep,
}

/// The dialog that asks before a tab closes: its reason, text and which button has the
/// focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosePrompt {
    /// The most important reason it asks.
    pub reason: CloseReason,
    /// The title, such as `Close Query 2 without saving?`.
    pub title: String,
    /// The body, naming the other reasons.
    pub body: String,
    /// Whether the focus is on the default (closing) button rather than the keeping one.
    pub default_focused: bool,
}

impl ClosePrompt {
    /// The dialog closing `tab` asks, or `None` when it closes without asking.
    pub fn for_tab(tab: &QueryTab) -> Option<Self> {
        let name = &tab.title;
        let transaction = tab.transaction != TransactionStatus::Idle;
        let unsaved = !tab.is_keys() && !tab.editor.text().is_empty();
        let (reason, title, body) = if tab.running {
            let mut body =
                "The running statement is cancelled and the rows fetched so far are discarded."
                    .to_owned();
            if transaction {
                body.push_str(" Its open transaction rolls back.");
            }
            if unsaved {
                body.push_str(" Its text is unsaved.");
            }
            (
                CloseReason::Running,
                format!("Stop {name} and close it?"),
                body,
            )
        } else if transaction {
            let mut body = format!(
                "{name} has an open transaction. Closing rolls it back; nothing from it is kept."
            );
            if unsaved {
                body.push_str(" Its text is unsaved too.");
            }
            (
                CloseReason::Transaction,
                format!("Close {name} and roll back?"),
                body,
            )
        } else if unsaved {
            (
                CloseReason::Unsaved,
                format!("Close {name} without saving?"),
                "Its text is not saved anywhere.".to_owned(),
            )
        } else {
            return None;
        };
        Some(Self {
            reason,
            title,
            body,
            default_focused: true,
        })
    }

    /// The buttons' labels: the one that keeps the tab, then the default one that closes it.
    pub fn buttons(&self) -> (&'static str, &'static str) {
        match self.reason {
            CloseReason::Running => ("Keep running", "Stop and close"),
            CloseReason::Transaction => ("Keep", "Roll back and close"),
            CloseReason::Unsaved => ("Keep", "Close"),
        }
    }

    /// Answers a key: `Tab` moves between the two buttons, `Enter` and `Space` press the
    /// focused one, `Esc` keeps the tab.
    pub fn key(&mut self, press: KeyPress) -> PromptAnswer {
        match press.key {
            Key::Tab | Key::Left | Key::Right => {
                self.default_focused = !self.default_focused;
                PromptAnswer::Pending
            }
            Key::Enter | Key::Space if self.default_focused => PromptAnswer::Close,
            Key::Enter | Key::Space | Key::Escape => PromptAnswer::Keep,
            _ => PromptAnswer::Pending,
        }
    }
}

/// The dialog's rect, centred in a window of `size`.
pub fn dialog_rect(size: (f32, f32)) -> Rect {
    Rect::new(
        ((size.0 - DIALOG_WIDTH) / 2.0).round(),
        ((size.1 - DIALOG_HEIGHT) / 2.0).round(),
        DIALOG_WIDTH,
        DIALOG_HEIGHT,
    )
}

/// The keeping and closing buttons' rects in the dialog at `dialog`, each as wide as its
/// label at the body text's average advance plus its padding.
pub fn button_rects(prompt: &ClosePrompt, dialog: Rect) -> (Rect, Rect) {
    let (keep, close) = prompt.buttons();
    let width =
        |label: &str| (label.chars().count() as f32 * LABEL_ADVANCE + 24.0).max(DIALOG_BUTTON_MIN);
    let close_width = width(close);
    let keep_width = width(keep);
    let close_x = dialog.x + DIALOG_BUTTONS_RIGHT - close_width;
    let keep_x = close_x - DIALOG_BUTTON_GAP - keep_width;
    let top = dialog.y + DIALOG_BUTTON_TOP;
    (
        Rect::new(keep_x, top, keep_width, 28.0),
        Rect::new(close_x, top, close_width, 28.0),
    )
}

/// `text` broken at spaces into lines at most `width` wide in `face`, at most `limit` lines,
/// the last cut with `…` when more is left.
pub fn wrapped(face: &dyn Typeface, text: &str, width: f32, limit: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let candidate = if line.is_empty() {
            word.to_owned()
        } else {
            format!("{line} {word}")
        };
        if line.is_empty() || face.width(&candidate, BODY.0) <= width {
            line = candidate;
        } else {
            lines.push(std::mem::replace(&mut line, word.to_owned()));
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.len() > limit {
        lines.truncate(limit);
        if let Some(last) = lines.last_mut() {
            last.push('…');
        }
    }
    lines
}

pub(crate) fn draw_prompt(paint: &mut Painter<'_, '_>, prompt: &ClosePrompt, size: (f32, f32)) {
    let theme = paint.theme;
    let faces = paint.faces;
    paint
        .canvas
        .fill_rect(Rect::new(0.0, 0.0, size.0, size.1), theme.scrim);
    let dialog = dialog_rect(size);
    paint.canvas.fill_rounded_rect(dialog, 8.0, theme.border);
    paint.canvas.fill_rounded_rect(
        Rect::new(
            dialog.x + 1.0,
            dialog.y + 1.0,
            dialog.w - 2.0,
            dialog.h - 2.0,
        ),
        7.0,
        theme.bg0,
    );
    paint.text(
        faces.ui_bold,
        &prompt.title,
        dialog.x + 24.0,
        dialog.y + 20.0,
        TITLE_STYLE,
        theme.text_primary,
    );
    for (index, line) in wrapped(faces.ui, &prompt.body, DIALOG_BODY_WIDTH, 2)
        .iter()
        .enumerate()
    {
        paint.text(
            faces.ui,
            line,
            dialog.x + 24.0,
            dialog.y + 52.0 + 18.0 * index as f32,
            BODY,
            theme.text_primary,
        );
    }
    let (keep_label, close_label) = prompt.buttons();
    let (keep, close) = button_rects(prompt, dialog);
    paint.button(keep, Button::Secondary);
    paint.text(
        faces.ui,
        keep_label,
        keep.x + 12.0,
        keep.y + 5.0,
        BODY,
        theme.text_primary,
    );
    paint.button(close, Button::Primary);
    paint.text(
        faces.ui,
        close_label,
        close.x + 12.0,
        close.y + 5.0,
        BODY,
        theme.on_accent,
    );
    let focused = if prompt.default_focused { close } else { keep };
    crate::main_window::focus_ring(paint, focused);
}

pub(crate) fn prompt_nodes(
    nodes: &mut Vec<AccessNode>,
    prompt: &ClosePrompt,
    parent: u32,
    size: (f32, f32),
) {
    let dialog = dialog_rect(size);
    let bounds = |rect: Rect| AccessRect {
        x: rect.x,
        y: rect.y,
        width: rect.w,
        height: rect.h,
    };
    nodes.push(AccessNode {
        id: NODE_CLOSE_DIALOG,
        parent: Some(parent),
        role: AccessRole::Group,
        name: prompt.title.clone(),
        description: Some(prompt.body.clone()),
        bounds: bounds(dialog),
        ..AccessNode::default()
    });
    let (keep_label, close_label) = prompt.buttons();
    let (keep, close) = button_rects(prompt, dialog);
    for (id, label, rect, focused) in [
        (NODE_CLOSE_KEEP, keep_label, keep, !prompt.default_focused),
        (
            NODE_CLOSE_CONFIRM,
            close_label,
            close,
            prompt.default_focused,
        ),
    ] {
        let mut button = AccessNode {
            id,
            parent: Some(NODE_CLOSE_DIALOG),
            role: AccessRole::Button,
            name: label.to_owned(),
            bounds: bounds(rect),
            ..AccessNode::default()
        };
        button.state.focused = focused;
        button.actions.invoke = true;
        nodes.push(button);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query_tab::QueryTab;

    #[test]
    fn tabs_stay_140_wide_while_they_fit_then_shrink_to_100_and_scroll() {
        assert_eq!(tab_width(1, 1020.0), 140.0);
        assert_eq!(tab_width(7, 1020.0), 140.0);
        assert_eq!(tab_width(8, 1020.0), 123.0);
        assert_eq!(tab_width(9, 1020.0), 109.0);
        assert_eq!(tab_width(10, 1020.0), 100.0);
        assert!(!scrolls(9, 1020.0));
        assert!(scrolls(10, 1020.0));
        assert_eq!(plus_x(1, 1020.0, 260.0), 408.0);
        assert_eq!(plus_x(12, 1020.0, 260.0), 1244.0);
        assert_eq!(viewport_width(12, 1020.0), 976.0);
        assert_eq!(max_scroll(12, 1020.0), 224.0);
    }

    #[test]
    fn the_scroll_moves_only_as_far_as_the_active_tab_needs() {
        assert_eq!(scroll_to_show(0.0, 11, 12, 1020.0), 224.0);
        assert_eq!(scroll_to_show(224.0, 5, 12, 1020.0), 224.0);
        assert_eq!(scroll_to_show(224.0, 1, 12, 1020.0), 100.0);
        assert_eq!(scroll_to_show(300.0, 3, 5, 1020.0), 0.0);
    }

    #[test]
    fn a_new_tab_takes_the_lowest_unused_number() {
        let titles = ["Query 1", "Query 3"];
        assert_eq!(lowest_free_title(titles.iter().copied()), "Query 2");
        assert_eq!(lowest_free_title(["Query 2"].iter().copied()), "Query 1");
    }

    #[test]
    fn closing_asks_by_priority_running_then_transaction_then_unsaved() {
        let empty = QueryTab::new("Query 2");
        assert_eq!(ClosePrompt::for_tab(&empty), None);
        let mut tab = QueryTab::with_text("Query 2", "SELECT 1");
        let unsaved = ClosePrompt::for_tab(&tab).unwrap();
        assert_eq!(unsaved.title, "Close Query 2 without saving?");
        assert_eq!(unsaved.buttons(), ("Keep", "Close"));
        tab.transaction = TransactionStatus::InBlock;
        let open = ClosePrompt::for_tab(&tab).unwrap();
        assert_eq!(open.title, "Close Query 2 and roll back?");
        assert_eq!(
            open.body,
            "Query 2 has an open transaction. Closing rolls it back; nothing from it is kept. Its text is unsaved too."
        );
        tab.running = true;
        let running = ClosePrompt::for_tab(&tab).unwrap();
        assert_eq!(running.reason, CloseReason::Running);
        assert_eq!(running.title, "Stop Query 2 and close it?");
        assert_eq!(
            running.body,
            "The running statement is cancelled and the rows fetched so far are discarded. Its open transaction rolls back. Its text is unsaved."
        );
        assert_eq!(running.buttons(), ("Keep running", "Stop and close"));
    }

    #[test]
    fn the_dialog_opens_on_its_default_button_and_esc_keeps_the_tab() {
        let press = |key| KeyPress {
            key,
            modifiers: Default::default(),
        };
        let mut prompt = ClosePrompt::for_tab(&QueryTab::with_text("Query 2", "x")).unwrap();
        assert!(prompt.default_focused);
        assert_eq!(prompt.key(press(Key::Tab)), PromptAnswer::Pending);
        assert_eq!(prompt.key(press(Key::Enter)), PromptAnswer::Keep);
        prompt.default_focused = true;
        assert_eq!(prompt.key(press(Key::Enter)), PromptAnswer::Close);
        assert_eq!(prompt.key(press(Key::Escape)), PromptAnswer::Keep);
        assert_eq!(
            dialog_rect((1280.0, 800.0)),
            Rect::new(430.0, 320.0, 420.0, 160.0)
        );
    }
}
