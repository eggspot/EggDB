use eggdb_platform::window::{AccessNode, AccessRect, AccessRole, Key, KeyPress};

use crate::canvas::Rect;
use crate::color::Rgba;
use crate::main_window::{BODY, Painter, primary_name};

/// The menu's width.
pub const WIDTH: f32 = 240.0;
/// The menu's height: eight rows, two separators and the padding.
pub const HEIGHT: f32 = 250.0;
/// The menu's accessibility node; its items count up from [`NODE_FIRST_EDITOR_MENU_ITEM`].
pub const NODE_EDITOR_MENU: u32 = 64_400;
/// The first item's node.
pub const NODE_FIRST_EDITOR_MENU_ITEM: u32 = 64_401;
/// The `Run` item's label while a selection exists.
pub const RUN_SELECTION: &str = "Run selection";
/// The `Run` item's label without a selection.
pub const RUN_STATEMENT: &str = "Run statement";
/// The `Run` item's label in a keys tab.
pub const RUN_SCAN: &str = "Run scan";

const PADDING: f32 = 4.0;
const ROW_HEIGHT: f32 = 28.0;
const SEPARATOR: f32 = 9.0;
const LABEL_LEFT: f32 = 12.0;
const SHORTCUT_RIGHT: f32 = 228.0;
const BODY_BOTTOM: f32 = 776.0;
const CARET_GAP: f32 = 4.0;

/// One item of the editor's right-click menu (`docs/ux/workspace.md` 10.9), in its order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorMenuItem {
    /// `Cut`: copies the selection and deletes it.
    Cut,
    /// `Copy`: copies the selection.
    Copy,
    /// `Paste`: the clipboard's text at the caret.
    Paste,
    /// `Select all`.
    SelectAll,
    /// `Run selection` / `Run statement`: what `Run` does.
    Run,
    /// `Explain` on the same range.
    Explain,
    /// `Toggle comment` on the range's lines.
    ToggleComment,
    /// `Format statement`: the selection, else the statement under the caret, laid out.
    FormatStatement,
}

impl EditorMenuItem {
    /// Every item, top to bottom.
    pub const ALL: [Self; 8] = [
        Self::Cut,
        Self::Copy,
        Self::Paste,
        Self::SelectAll,
        Self::Run,
        Self::Explain,
        Self::ToggleComment,
        Self::FormatStatement,
    ];

    /// The item's label, `run_label` naming the `Run` item.
    pub fn label(self, run_label: &'static str) -> &'static str {
        match self {
            Self::Cut => "Cut",
            Self::Copy => "Copy",
            Self::Paste => "Paste",
            Self::SelectAll => "Select all",
            Self::Run => run_label,
            Self::Explain => "Explain",
            Self::ToggleComment => "Toggle comment",
            Self::FormatStatement => "Format statement",
        }
    }

    /// The item's shortcut as the menu shows it, with `primary` naming `Ctrl` or `Cmd`.
    pub fn shortcut(self, primary: &str) -> String {
        match self {
            Self::Cut => format!("{primary}+X"),
            Self::Copy => format!("{primary}+C"),
            Self::Paste => format!("{primary}+V"),
            Self::SelectAll => format!("{primary}+A"),
            Self::Run => format!("{primary}+Enter"),
            Self::Explain => format!("{primary}+E"),
            Self::ToggleComment => format!("{primary}+/"),
            Self::FormatStatement => format!("{primary}+Shift+F"),
        }
    }

    /// Whether a separator follows the item.
    pub fn ends_group(self) -> bool {
        matches!(self, Self::SelectAll | Self::Explain)
    }
}

/// What the editor holds when its menu opens, which decides the items that act.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EditorState {
    /// Whether a selection exists.
    pub selection: bool,
    /// Whether the editor is read-only (a keys tab).
    pub read_only: bool,
    /// Whether the text is empty.
    pub empty: bool,
    /// Whether the clipboard holds text (always on in the browser, which reads it on use).
    pub clipboard_text: bool,
    /// Whether the tab runs.
    pub running: bool,
    /// Whether the range `Run` would run is only whitespace and comments.
    pub blank_range: bool,
}

impl EditorState {
    /// Which items act, in [`EditorMenuItem::ALL`]'s order (`docs/ux/workspace.md` 10.9).
    pub fn enabled(&self) -> [bool; 8] {
        let Self {
            selection,
            read_only,
            empty,
            clipboard_text,
            running,
            blank_range,
        } = *self;
        EditorMenuItem::ALL.map(|item| match item {
            EditorMenuItem::Cut => selection && !read_only,
            EditorMenuItem::Copy => selection || read_only,
            EditorMenuItem::Paste => !read_only && clipboard_text,
            EditorMenuItem::SelectAll => !empty && !read_only,
            EditorMenuItem::Run => !running && (read_only || !blank_range),
            EditorMenuItem::Explain => !running && !read_only && !blank_range,
            EditorMenuItem::ToggleComment => !empty && !read_only,
            EditorMenuItem::FormatStatement => !blank_range && !running && !read_only,
        })
    }

    /// The `Run` item's label.
    pub fn run_label(&self) -> &'static str {
        if self.read_only {
            RUN_SCAN
        } else if self.selection {
            RUN_SELECTION
        } else {
            RUN_STATEMENT
        }
    }
}

/// Where the menu opened from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EditorMenuOrigin {
    /// A right-click at this point.
    Pointer {
        /// The pointer's x.
        x: f32,
        /// The pointer's y.
        y: f32,
    },
    /// `Shift+F10` or the menu key: below the caret's line box.
    Caret {
        /// The caret's x (the selection's end when there is one).
        x: f32,
        /// The caret line's top.
        line_top: f32,
        /// The line box's height.
        line_height: f32,
    },
}

/// What a key did in the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorMenuAnswer {
    /// The highlight moved, or nothing happened.
    Moved,
    /// `Esc` or `Tab`: the menu closes and the focus goes back to the editor.
    Close,
    /// `Enter` or `Space` on an enabled item: the menu closes and the item acts.
    Activate(EditorMenuItem),
}

/// The editor's open right-click menu (`docs/ux/workspace.md` 10.9).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EditorMenu {
    /// Which items act; disabled ones stay in place.
    pub enabled: [bool; 8],
    /// The `Run` item's label.
    pub run_label: &'static str,
    /// The highlighted item, `None` until a key or the pointer highlights one.
    pub highlighted: Option<usize>,
    /// Where it opened from.
    pub origin: EditorMenuOrigin,
}

/// The menu's rect for its origin in a window `window_width` wide: moved left to end inside
/// the window, and above its anchor when it would cross the body's bottom.
pub fn rect_for(origin: EditorMenuOrigin, window_width: f32) -> Rect {
    let (x, below, above) = match origin {
        EditorMenuOrigin::Pointer { x, y } => (x, y, y),
        EditorMenuOrigin::Caret {
            x,
            line_top,
            line_height,
        } => (x, line_top + line_height + CARET_GAP, line_top - CARET_GAP),
    };
    let x = x.min(window_width - WIDTH).max(0.0);
    let y = if below + HEIGHT > BODY_BOTTOM {
        above - HEIGHT
    } else {
        below
    };
    Rect::new(x, y.max(0.0), WIDTH, HEIGHT)
}

/// The top of item `index` from the menu's top: rows of 28 after 4 of padding, with a
/// separator of 9 after each group.
pub fn item_top(index: usize) -> f32 {
    let separators = EditorMenuItem::ALL
        .iter()
        .take(index)
        .filter(|item| item.ends_group())
        .count();
    PADDING + ROW_HEIGHT * index as f32 + SEPARATOR * separators as f32
}

/// The item under (`x`, `y`) in a menu at `rect`.
pub fn item_at(rect: Rect, x: f32, y: f32) -> Option<usize> {
    if x < rect.x || x >= rect.x + rect.w {
        return None;
    }
    (0..EditorMenuItem::ALL.len()).find(|&index| {
        let top = rect.y + item_top(index);
        y >= top && y < top + ROW_HEIGHT
    })
}

/// The item behind accessibility node `id`, when it is one of the menu's.
pub fn item_of_node(id: u32) -> Option<usize> {
    let index = id.checked_sub(NODE_FIRST_EDITOR_MENU_ITEM)? as usize;
    (index < EditorMenuItem::ALL.len()).then_some(index)
}

impl EditorMenu {
    /// The menu for an editor in `state`; from the keyboard the first enabled item is
    /// highlighted, from the pointer none.
    pub fn new(state: EditorState, origin: EditorMenuOrigin) -> Self {
        let enabled = state.enabled();
        let highlighted = match origin {
            EditorMenuOrigin::Caret { .. } => enabled.iter().position(|&on| on),
            EditorMenuOrigin::Pointer { .. } => None,
        };
        Self {
            enabled,
            run_label: state.run_label(),
            highlighted,
            origin,
        }
    }

    /// The item at `index`, when it acts.
    pub fn enabled_item(&self, index: usize) -> Option<EditorMenuItem> {
        let item = *EditorMenuItem::ALL.get(index)?;
        self.enabled.get(index).copied()?.then_some(item)
    }

    fn step(&self, from: usize, down: bool) -> Option<usize> {
        let mut at = from;
        loop {
            at = if down {
                at.checked_add(1)
                    .filter(|&next| next < EditorMenuItem::ALL.len())?
            } else {
                at.checked_sub(1)?
            };
            if self.enabled[at] {
                return Some(at);
            }
        }
    }

    /// Answers a key while the menu is open: `Up` and `Down` move over the enabled items
    /// without wrapping, a letter jumps to the next enabled item starting with it (`C`
    /// alternates `Cut` and `Copy`), `Enter` and `Space` activate.
    pub fn key(&mut self, press: KeyPress) -> EditorMenuAnswer {
        let count = EditorMenuItem::ALL.len();
        let first = self.enabled.iter().position(|&on| on);
        let last = self.enabled.iter().rposition(|&on| on);
        match press.key {
            Key::Escape | Key::Tab => return EditorMenuAnswer::Close,
            Key::Enter | Key::Space => {
                if let Some(item) = self.highlighted.and_then(|at| self.enabled_item(at)) {
                    return EditorMenuAnswer::Activate(item);
                }
            }
            Key::Up => {
                self.highlighted = match self.highlighted {
                    None => last,
                    Some(at) => self.step(at, false).or(Some(at)),
                };
            }
            Key::Down => {
                self.highlighted = match self.highlighted {
                    None => first,
                    Some(at) => self.step(at, true).or(Some(at)),
                };
            }
            Key::Home => self.highlighted = first,
            Key::End => self.highlighted = last,
            Key::Character(letter) => {
                let from = self.highlighted.map_or(0, |at| at + 1);
                let found = (0..count).map(|step| (from + step) % count).find(|&index| {
                    self.enabled[index]
                        && EditorMenuItem::ALL[index]
                            .label(self.run_label)
                            .chars()
                            .next()
                            .is_some_and(|initial| initial.eq_ignore_ascii_case(&letter))
                });
                if found.is_some() {
                    self.highlighted = found;
                }
            }
            _ => {}
        }
        EditorMenuAnswer::Moved
    }
}

pub(crate) fn draw(
    paint: &mut Painter<'_, '_>,
    menu: &EditorMenu,
    rect: Rect,
    hovered: Option<usize>,
) {
    let theme = paint.theme;
    let faces = paint.faces;
    for (spread, alpha) in [(8.0, 10u8), (4.0, 14), (0.0, 16)] {
        let shadow = Rect::new(
            rect.x - spread / 2.0,
            rect.y + 4.0 - spread / 2.0,
            rect.w + spread,
            rect.h + spread,
        );
        let color = Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: alpha,
        };
        paint
            .canvas
            .fill_rounded_rect(shadow, 6.0 + spread / 2.0, color);
    }
    paint.canvas.fill_rounded_rect(rect, 6.0, theme.border);
    paint.canvas.fill_rounded_rect(
        Rect::new(rect.x + 1.0, rect.y + 1.0, rect.w - 2.0, rect.h - 2.0),
        5.0,
        theme.bg0,
    );
    let primary = primary_name();
    let lit = menu.highlighted.or(hovered.filter(|&at| menu.enabled[at]));
    for (index, item) in EditorMenuItem::ALL.iter().enumerate() {
        let top = rect.y + item_top(index);
        let enabled = menu.enabled[index];
        if enabled && lit == Some(index) {
            paint.canvas.fill_rounded_rect(
                Rect::new(rect.x + 4.0, top, rect.w - 8.0, ROW_HEIGHT),
                4.0,
                theme.selection,
            );
        }
        let (label_color, shortcut_color) = if enabled {
            (theme.text_primary, theme.text_secondary)
        } else {
            (theme.text_disabled, theme.text_disabled)
        };
        paint.text(
            faces.ui,
            item.label(menu.run_label),
            rect.x + LABEL_LEFT,
            top + 5.0,
            BODY,
            label_color,
        );
        paint.text_right(
            faces.ui,
            &item.shortcut(primary),
            rect.x + SHORTCUT_RIGHT,
            top + 5.0,
            BODY,
            shortcut_color,
        );
        if item.ends_group() {
            paint.rect(rect.x, top + ROW_HEIGHT + 4.0, rect.w, 1.0, theme.border);
        }
    }
}

pub(crate) fn nodes(nodes: &mut Vec<AccessNode>, menu: &EditorMenu, rect: Rect, parent: u32) {
    let bounds = |rect: Rect| AccessRect {
        x: rect.x,
        y: rect.y,
        width: rect.w,
        height: rect.h,
    };
    nodes.push(AccessNode {
        id: NODE_EDITOR_MENU,
        parent: Some(parent),
        role: AccessRole::Group,
        name: "Editor menu".to_owned(),
        bounds: bounds(rect),
        ..AccessNode::default()
    });
    let primary = primary_name();
    for (index, item) in EditorMenuItem::ALL.iter().enumerate() {
        let mut node = AccessNode {
            id: NODE_FIRST_EDITOR_MENU_ITEM + index as u32,
            parent: Some(NODE_EDITOR_MENU),
            role: AccessRole::Button,
            name: item.label(menu.run_label).to_owned(),
            shortcut: Some(item.shortcut(primary)),
            bounds: bounds(Rect::new(
                rect.x,
                rect.y + item_top(index),
                rect.w,
                ROW_HEIGHT,
            )),
            ..AccessNode::default()
        };
        node.state.focused = menu.highlighted == Some(index);
        node.state.disabled = !menu.enabled[index];
        node.actions.invoke = menu.enabled[index];
        nodes.push(node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writable(selection: bool) -> EditorState {
        EditorState {
            selection,
            clipboard_text: true,
            ..EditorState::default()
        }
    }

    #[test]
    fn the_rows_and_separators_add_up_to_the_designed_height() {
        assert_eq!(item_top(0), 4.0);
        assert_eq!(item_top(4), 125.0);
        assert_eq!(item_top(6), 190.0);
        assert_eq!(item_top(7) + ROW_HEIGHT + PADDING, HEIGHT);
        let rect = Rect::new(400.0, 200.0, WIDTH, HEIGHT);
        assert_eq!(item_at(rect, 410.0, 200.0 + 125.0), Some(4));
        assert_eq!(item_at(rect, 410.0, 200.0 + 120.0), None);
    }

    #[test]
    fn the_menu_opens_at_the_pointer_or_below_the_caret_and_stays_in_the_window() {
        assert_eq!(
            rect_for(EditorMenuOrigin::Pointer { x: 500.0, y: 120.0 }, 1280.0),
            Rect::new(500.0, 120.0, WIDTH, HEIGHT)
        );
        assert_eq!(
            rect_for(
                EditorMenuOrigin::Pointer {
                    x: 1200.0,
                    y: 600.0
                },
                1280.0
            ),
            Rect::new(1040.0, 350.0, WIDTH, HEIGHT)
        );
        assert_eq!(
            rect_for(
                EditorMenuOrigin::Caret {
                    x: 380.0,
                    line_top: 88.0,
                    line_height: 20.0
                },
                1280.0
            ),
            Rect::new(380.0, 112.0, WIDTH, HEIGHT)
        );
    }

    #[test]
    fn the_enabled_rules_follow_the_selection_the_clipboard_and_a_read_only_editor() {
        assert_eq!(
            writable(false).enabled(),
            [false, false, true, true, true, true, true, true]
        );
        assert_eq!(writable(false).run_label(), RUN_STATEMENT);
        assert_eq!(writable(true).run_label(), RUN_SELECTION);
        assert!(writable(true).enabled()[0]);
        let no_clipboard = EditorState {
            clipboard_text: false,
            ..writable(false)
        };
        assert!(!no_clipboard.enabled()[2]);
        let blank = EditorState {
            blank_range: true,
            ..writable(false)
        };
        assert_eq!(
            blank.enabled(),
            [false, false, true, true, false, false, true, false]
        );
        let running = EditorState {
            running: true,
            ..writable(false)
        };
        assert!(!running.enabled()[4] && !running.enabled()[5] && !running.enabled()[7]);
        let keys = EditorState {
            read_only: true,
            clipboard_text: true,
            ..EditorState::default()
        };
        assert_eq!(
            keys.enabled(),
            [false, true, false, false, true, false, false, false]
        );
        assert_eq!(keys.run_label(), RUN_SCAN);
    }

    #[test]
    fn keys_skip_disabled_items_and_c_alternates_cut_and_copy() {
        let caret = EditorMenuOrigin::Caret {
            x: 320.0,
            line_top: 88.0,
            line_height: 20.0,
        };
        let mut menu = EditorMenu::new(writable(true), caret);
        assert_eq!(menu.highlighted, Some(0));
        menu.key(KeyPress::plain(Key::Character('c')));
        assert_eq!(menu.highlighted, Some(1));
        menu.key(KeyPress::plain(Key::Character('c')));
        assert_eq!(menu.highlighted, Some(0));
        menu.key(KeyPress::plain(Key::End));
        menu.key(KeyPress::plain(Key::Down));
        assert_eq!(menu.highlighted, Some(7));
        assert_eq!(
            menu.key(KeyPress::plain(Key::Enter)),
            EditorMenuAnswer::Activate(EditorMenuItem::FormatStatement)
        );
        let mut keys = EditorMenu::new(
            EditorState {
                read_only: true,
                ..EditorState::default()
            },
            caret,
        );
        assert_eq!(keys.highlighted, Some(1));
        keys.key(KeyPress::plain(Key::Down));
        assert_eq!(keys.highlighted, Some(4));
        keys.key(KeyPress::plain(Key::Down));
        assert_eq!(keys.highlighted, Some(4));
        assert_eq!(
            keys.key(KeyPress::plain(Key::Enter)),
            EditorMenuAnswer::Activate(EditorMenuItem::Run)
        );
        let mut pointer = EditorMenu::new(writable(false), EditorMenuOrigin::Pointer { x: 1.0, y: 1.0 });
        assert_eq!(pointer.key(KeyPress::plain(Key::Enter)), EditorMenuAnswer::Moved);
        assert_eq!(pointer.key(KeyPress::plain(Key::Escape)), EditorMenuAnswer::Close);
    }
}
