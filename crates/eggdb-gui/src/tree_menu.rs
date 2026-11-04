use eggdb_platform::window::{AccessNode, AccessRect, AccessRole, Key, KeyPress};

use crate::canvas::Rect;
use crate::color::Rgba;
use crate::main_window::{BODY, Painter, primary_name};
use crate::object_browser::{ROW_HEIGHT as TREE_ROW_HEIGHT, TreeItem};

/// The menu's width.
pub const WIDTH: f32 = 240.0;
/// The menu's height: six rows, two separators and the padding.
pub const HEIGHT: f32 = 194.0;
/// The menu's accessibility node; its items count up from [`NODE_FIRST_TREE_MENU_ITEM`].
pub const NODE_TREE_MENU: u32 = 64_300;
/// The first item's node.
pub const NODE_FIRST_TREE_MENU_ITEM: u32 = 64_301;

const PADDING: f32 = 4.0;
const ROW_HEIGHT: f32 = 28.0;
const SEPARATOR: f32 = 9.0;
const LABEL_LEFT: f32 = 12.0;
const SHORTCUT_RIGHT: f32 = 228.0;
const BODY_BOTTOM: f32 = 776.0;
const ROW_GAP: f32 = 4.0;

/// One item of the tree's row menu (`docs/ux/workspace.md` 10.1), in the menu's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeMenuItem {
    /// What `Enter` does on the row; `Expand` or `Collapse` on the connection and groups.
    Open,
    /// Inserts the name at the active tab's caret.
    InsertName,
    /// Copies the name, or the connection's path or URL.
    CopyName,
    /// Opens the inspector at the `DDL` block.
    ShowDdl,
    /// Reloads the whole catalog.
    Refresh,
    /// Opens the Drop dialog (10.2).
    Drop,
}

impl TreeMenuItem {
    /// Every item, top to bottom.
    pub const ALL: [Self; 6] = [
        Self::Open,
        Self::InsertName,
        Self::CopyName,
        Self::ShowDdl,
        Self::Refresh,
        Self::Drop,
    ];

    /// The item's label; `Open` reads `Expand` or `Collapse` on a row with children.
    pub fn label(self, expanded: Option<bool>) -> &'static str {
        match self {
            Self::Open => match expanded {
                Some(true) => "Collapse",
                Some(false) => "Expand",
                None => "Open",
            },
            Self::InsertName => "Insert name",
            Self::CopyName => "Copy name",
            Self::ShowDdl => "Show DDL",
            Self::Refresh => "Refresh",
            Self::Drop => "Drop…",
        }
    }

    /// The key that does the same from the tree, with `primary` as `Ctrl` or `Cmd`.
    pub fn shortcut(self, primary: &str) -> Option<String> {
        match self {
            Self::Open => Some("Enter".to_owned()),
            Self::InsertName => Some("Shift+Enter".to_owned()),
            Self::CopyName => Some(format!("{primary}+C")),
            Self::Refresh => Some("F5".to_owned()),
            Self::ShowDdl | Self::Drop => None,
        }
    }

    /// Whether a separator follows the item, ending its group.
    pub fn ends_group(self) -> bool {
        matches!(self, Self::CopyName | Self::Refresh)
    }

    /// Whether the item acts on `row` (10.1's table): `copyable` when the row has a name or
    /// a location to copy, `writable` when the connection allows a drop.
    pub fn enabled(self, row: TreeItem, copyable: bool, writable: bool) -> bool {
        let group = matches!(
            row,
            TreeItem::Connection
                | TreeItem::Group(_)
                | TreeItem::Columns(_)
                | TreeItem::TableIndexes(_)
                | TreeItem::TableTriggers(_)
        );
        match self {
            Self::Open => !matches!(row, TreeItem::Sequence(_)),
            Self::InsertName => !group && !matches!(row, TreeItem::KeySpace(_)),
            Self::CopyName => copyable,
            Self::ShowDdl => matches!(
                row,
                TreeItem::Table(_)
                    | TreeItem::View(_)
                    | TreeItem::Column { .. }
                    | TreeItem::Index(_)
                    | TreeItem::TableIndex(_)
                    | TreeItem::Sequence(_)
            ),
            Self::Refresh => true,
            Self::Drop => {
                writable
                    && matches!(
                        row,
                        TreeItem::Table(_)
                            | TreeItem::View(_)
                            | TreeItem::Index(_)
                            | TreeItem::TableIndex(_)
                            | TreeItem::Sequence(_)
                            | TreeItem::Trigger(_)
                    )
            }
        }
    }
}

/// Where the menu opened from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TreeMenuOrigin {
    /// A right-click at this point.
    Pointer {
        /// The pointer's x.
        x: f32,
        /// The pointer's y.
        y: f32,
    },
    /// `Shift+F10` or the menu key: below the current row, at its label.
    CurrentRow,
}

/// What a key did in the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeMenuAnswer {
    /// The highlight moved, or nothing happened.
    Moved,
    /// `Esc` or `Tab`: the menu closes and the focus goes back to the row.
    Close,
    /// `Enter` or `Space` on an enabled item: the menu closes and the item acts.
    Activate(TreeMenuItem),
}

/// The tree's open row menu (`docs/ux/workspace.md` 10.1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TreeMenu {
    /// The row it acts on.
    pub row: TreeItem,
    /// The row's state, which names the `Open` item.
    pub expanded: Option<bool>,
    /// Which items act, in [`TreeMenuItem::ALL`]'s order; disabled ones stay in place.
    pub enabled: [bool; 6],
    /// The highlighted item, `None` until a key or the pointer highlights one.
    pub highlighted: Option<usize>,
    /// Where it opened from.
    pub origin: TreeMenuOrigin,
}

/// The menu's rect for its origin, the current row's label starting at `label` (its x and
/// the row's top), in a window `window_width` wide: moved left to end inside the window and
/// above its anchor when it would cross the body's bottom.
pub fn rect_for(origin: TreeMenuOrigin, label: (f32, f32), window_width: f32) -> Rect {
    let (x, below, above) = match origin {
        TreeMenuOrigin::Pointer { x, y } => (x, y, y),
        TreeMenuOrigin::CurrentRow => (
            label.0,
            label.1 + TREE_ROW_HEIGHT + ROW_GAP,
            label.1 - ROW_GAP,
        ),
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
    let separators = TreeMenuItem::ALL
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
    (0..TreeMenuItem::ALL.len()).find(|&index| {
        let top = rect.y + item_top(index);
        y >= top && y < top + ROW_HEIGHT
    })
}

impl TreeMenu {
    /// The menu for `row`, with `enabled` saying which items act; from the keyboard
    /// (`CurrentRow`) the first enabled item is highlighted, from the pointer none.
    pub fn new(
        row: TreeItem,
        expanded: Option<bool>,
        enabled: [bool; 6],
        origin: TreeMenuOrigin,
    ) -> Self {
        let highlighted = match origin {
            TreeMenuOrigin::CurrentRow => enabled.iter().position(|&on| on),
            TreeMenuOrigin::Pointer { .. } => None,
        };
        Self {
            row,
            expanded,
            enabled,
            highlighted,
            origin,
        }
    }

    /// The item at `index`, when it acts.
    pub fn enabled_item(&self, index: usize) -> Option<TreeMenuItem> {
        let item = *TreeMenuItem::ALL.get(index)?;
        self.enabled[index].then_some(item)
    }

    fn step(&self, from: usize, down: bool) -> Option<usize> {
        let mut at = from;
        loop {
            at = if down {
                at.checked_add(1)
                    .filter(|&next| next < TreeMenuItem::ALL.len())?
            } else {
                at.checked_sub(1)?
            };
            if self.enabled[at] {
                return Some(at);
            }
        }
    }

    /// Answers a key while the menu is open: `Up` and `Down` move over the enabled items
    /// without wrapping, a letter jumps to the next enabled item starting with it, `Enter`
    /// and `Space` activate.
    pub fn key(&mut self, press: KeyPress) -> TreeMenuAnswer {
        let count = TreeMenuItem::ALL.len();
        let first = self.enabled.iter().position(|&on| on);
        let last = self.enabled.iter().rposition(|&on| on);
        match press.key {
            Key::Escape | Key::Tab => return TreeMenuAnswer::Close,
            Key::Enter | Key::Space => {
                if let Some(item) = self.highlighted.and_then(|at| self.enabled_item(at)) {
                    return TreeMenuAnswer::Activate(item);
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
                        && TreeMenuItem::ALL[index]
                            .label(self.expanded)
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
        TreeMenuAnswer::Moved
    }
}

pub(crate) fn draw(
    paint: &mut Painter<'_, '_>,
    menu: &TreeMenu,
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
    for (index, item) in TreeMenuItem::ALL.iter().enumerate() {
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
            item.label(menu.expanded),
            rect.x + LABEL_LEFT,
            top + 5.0,
            BODY,
            label_color,
        );
        if let Some(shortcut) = item.shortcut(primary) {
            paint.text_right(
                faces.ui,
                &shortcut,
                rect.x + SHORTCUT_RIGHT,
                top + 5.0,
                BODY,
                shortcut_color,
            );
        }
        if item.ends_group() {
            paint.rect(rect.x, top + ROW_HEIGHT + 4.0, rect.w, 1.0, theme.border);
        }
    }
}

pub(crate) fn nodes(nodes: &mut Vec<AccessNode>, menu: &TreeMenu, rect: Rect, parent: u32) {
    let bounds = |rect: Rect| AccessRect {
        x: rect.x,
        y: rect.y,
        width: rect.w,
        height: rect.h,
    };
    nodes.push(AccessNode {
        id: NODE_TREE_MENU,
        parent: Some(parent),
        role: AccessRole::Group,
        name: "Object menu".to_owned(),
        bounds: bounds(rect),
        ..AccessNode::default()
    });
    let primary = primary_name();
    for (index, item) in TreeMenuItem::ALL.iter().enumerate() {
        let mut node = AccessNode {
            id: NODE_FIRST_TREE_MENU_ITEM + index as u32,
            parent: Some(NODE_TREE_MENU),
            role: AccessRole::Button,
            name: item.label(menu.expanded).to_owned(),
            shortcut: item.shortcut(primary),
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

/// The item behind accessibility node `id`, when it is one of the menu's.
pub fn item_of_node(id: u32) -> Option<usize> {
    let index = id.checked_sub(NODE_FIRST_TREE_MENU_ITEM)? as usize;
    (index < TreeMenuItem::ALL.len()).then_some(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_browser::Group;

    fn enabled_for(row: TreeItem, copyable: bool, writable: bool) -> [bool; 6] {
        TreeMenuItem::ALL.map(|item| item.enabled(row, copyable, writable))
    }

    #[test]
    fn the_rows_and_separators_add_up_to_the_designed_height() {
        assert_eq!(item_top(0), 4.0);
        assert_eq!(item_top(3), 97.0);
        assert_eq!(item_top(5), 162.0);
        assert_eq!(item_top(5) + ROW_HEIGHT + PADDING, HEIGHT);
        let rect = Rect::new(100.0, 200.0, WIDTH, HEIGHT);
        assert_eq!(item_at(rect, 110.0, 200.0 + 97.0), Some(3));
        assert_eq!(item_at(rect, 110.0, 200.0 + 93.0), None);
        assert_eq!(item_of_node(NODE_FIRST_TREE_MENU_ITEM + 5), Some(5));
        assert_eq!(item_of_node(NODE_FIRST_TREE_MENU_ITEM + 6), None);
    }

    #[test]
    fn each_kind_of_row_enables_the_items_of_the_design_table() {
        assert_eq!(
            enabled_for(TreeItem::Connection, true, true),
            [true, false, true, false, true, false]
        );
        assert_eq!(
            enabled_for(TreeItem::Group(Group::Tables), false, true),
            [true, false, false, false, true, false]
        );
        assert_eq!(enabled_for(TreeItem::Table(0), true, true), [true; 6]);
        assert_eq!(
            enabled_for(TreeItem::Table(0), true, false),
            [true, true, true, true, true, false]
        );
        assert_eq!(
            enabled_for(
                TreeItem::Column {
                    table: 0,
                    column: 1
                },
                true,
                true
            ),
            [true, true, true, true, true, false]
        );
        assert_eq!(
            enabled_for(TreeItem::Sequence(0), true, true),
            [false, true, true, true, true, true]
        );
        assert_eq!(
            enabled_for(TreeItem::Trigger(0), true, true),
            [true, true, true, false, true, true]
        );
        assert_eq!(
            enabled_for(TreeItem::KeySpace(0), true, true),
            [true, false, true, false, true, false]
        );
        assert_eq!(TreeMenuItem::Open.label(Some(true)), "Collapse");
        assert_eq!(TreeMenuItem::Open.label(Some(false)), "Expand");
        assert_eq!(TreeMenuItem::Open.label(None), "Open");
    }

    #[test]
    fn it_opens_below_the_row_at_its_label_or_at_the_pointer_and_stays_in_the_window() {
        assert_eq!(
            rect_for(TreeMenuOrigin::CurrentRow, (60.0, 136.0), 1280.0),
            Rect::new(60.0, 164.0, WIDTH, HEIGHT)
        );
        assert_eq!(
            rect_for(TreeMenuOrigin::CurrentRow, (60.0, 700.0), 1280.0),
            Rect::new(60.0, 502.0, WIDTH, HEIGHT)
        );
        assert_eq!(
            rect_for(
                TreeMenuOrigin::Pointer {
                    x: 1200.0,
                    y: 300.0
                },
                (0.0, 0.0),
                1280.0
            ),
            Rect::new(1040.0, 300.0, WIDTH, HEIGHT)
        );
    }

    #[test]
    fn keys_skip_disabled_items_and_enter_on_one_activates_it() {
        let enabled = enabled_for(TreeItem::Sequence(0), true, true);
        let mut menu = TreeMenu::new(
            TreeItem::Sequence(0),
            None,
            enabled,
            TreeMenuOrigin::CurrentRow,
        );
        assert_eq!(menu.highlighted, Some(1));
        menu.key(KeyPress::plain(Key::Up));
        assert_eq!(menu.highlighted, Some(1));
        menu.key(KeyPress::plain(Key::Character('o')));
        assert_eq!(menu.highlighted, Some(1));
        menu.key(KeyPress::plain(Key::Character('d')));
        assert_eq!(menu.highlighted, Some(5));
        menu.key(KeyPress::plain(Key::Character('s')));
        assert_eq!(
            menu.key(KeyPress::plain(Key::Enter)),
            TreeMenuAnswer::Activate(TreeMenuItem::ShowDdl)
        );
        let mut pointer = TreeMenu::new(
            TreeItem::Trigger(0),
            None,
            enabled_for(TreeItem::Trigger(0), true, true),
            TreeMenuOrigin::Pointer { x: 10.0, y: 10.0 },
        );
        assert_eq!(pointer.highlighted, None);
        assert_eq!(
            pointer.key(KeyPress::plain(Key::Enter)),
            TreeMenuAnswer::Moved
        );
        pointer.highlighted = Some(3);
        assert_eq!(
            pointer.key(KeyPress::plain(Key::Enter)),
            TreeMenuAnswer::Moved
        );
        assert_eq!(
            pointer.key(KeyPress::plain(Key::Escape)),
            TreeMenuAnswer::Close
        );
    }
}
