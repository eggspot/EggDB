use eggdb_platform::window::{AccessNode, AccessRect, AccessRole, Key, KeyPress};

use crate::canvas::Rect;
use crate::color::Rgba;
use crate::grid::GridMenuItem;
use crate::main_window::{BODY, Painter, primary_name};

/// The menu's width.
pub const WIDTH: f32 = 240.0;
/// The menu's height: nine rows, two separators and the padding.
pub const HEIGHT: f32 = 278.0;
/// The menu's accessibility node; its items count up from [`NODE_FIRST_GRID_MENU_ITEM`].
pub const NODE_GRID_MENU: u32 = 64_200;
/// The first item's node.
pub const NODE_FIRST_GRID_MENU_ITEM: u32 = 64_201;

const PADDING: f32 = 4.0;
const ROW_HEIGHT: f32 = 28.0;
const SEPARATOR: f32 = 9.0;
const LABEL_LEFT: f32 = 12.0;
const SHORTCUT_RIGHT: f32 = 228.0;
const BODY_BOTTOM: f32 = 776.0;
const CELL_GAP: f32 = 4.0;

/// Where the menu opened from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MenuOrigin {
    /// A right-click at this point.
    Pointer {
        /// The pointer's x.
        x: f32,
        /// The pointer's y.
        y: f32,
    },
    /// `Shift+F10` or the menu key: below the current cell.
    CurrentCell,
}

/// What a key did in the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAnswer {
    /// The highlight moved, or nothing happened.
    Moved,
    /// `Esc` or `Tab`: the menu closes and the focus goes back to the grid.
    Close,
    /// `Enter` or `Space` on an item: the menu closes and the item acts.
    Activate(GridMenuItem),
}

/// The grid's open right-click menu (`docs/ux/workspace.md` 9.4).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridMenu {
    /// The highlighted item, `None` until a key or the pointer highlights one.
    pub highlighted: Option<usize>,
    /// Where it opened from.
    pub origin: MenuOrigin,
}

/// The menu's rect: its top-left at `anchor`'s bottom-left plus `gap`, moved left to end
/// inside a window `window_width` wide, and above `anchor` when it would cross the body's
/// bottom.
pub fn menu_rect(anchor: Rect, gap: f32, window_width: f32) -> Rect {
    let x = anchor.x.min(window_width - WIDTH).max(0.0);
    let below = anchor.y + anchor.h + gap;
    let y = if below + HEIGHT > BODY_BOTTOM {
        anchor.y - gap - HEIGHT
    } else {
        below
    };
    Rect::new(x, y.max(0.0), WIDTH, HEIGHT)
}

/// The menu's rect for its origin, the current cell at `cell` in a window `window_width`
/// wide.
pub fn rect_for(origin: MenuOrigin, cell: Rect, window_width: f32) -> Rect {
    match origin {
        MenuOrigin::Pointer { x, y } => menu_rect(Rect::new(x, y, 0.0, 0.0), 0.0, window_width),
        MenuOrigin::CurrentCell => menu_rect(cell, CELL_GAP, window_width),
    }
}

/// The top of item `index` from the menu's top: rows of 28 after 4 of padding, with a
/// separator of 9 after each group.
pub fn item_top(index: usize) -> f32 {
    let separators = GridMenuItem::ALL
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
    (0..GridMenuItem::ALL.len()).find(|&index| {
        let top = rect.y + item_top(index);
        y >= top && y < top + ROW_HEIGHT
    })
}

impl GridMenu {
    /// The menu opened by the pointer at (`x`, `y`), nothing highlighted.
    pub fn at_pointer(x: f32, y: f32) -> Self {
        Self {
            highlighted: None,
            origin: MenuOrigin::Pointer { x, y },
        }
    }

    /// The menu opened from the keyboard below the current cell, its first item highlighted.
    pub fn at_current_cell() -> Self {
        Self {
            highlighted: Some(0),
            origin: MenuOrigin::CurrentCell,
        }
    }

    /// Answers a key while the menu is open: `Up` and `Down` move without wrapping, a letter
    /// jumps to the next item starting with it, `Enter` and `Space` activate.
    pub fn key(&mut self, press: KeyPress) -> MenuAnswer {
        let last = GridMenuItem::ALL.len() - 1;
        match press.key {
            Key::Escape | Key::Tab => return MenuAnswer::Close,
            Key::Enter | Key::Space => {
                if let Some(item) = self.highlighted.and_then(|at| GridMenuItem::ALL.get(at)) {
                    return MenuAnswer::Activate(*item);
                }
            }
            Key::Up => {
                self.highlighted = Some(self.highlighted.map_or(last, |at| at.saturating_sub(1)));
            }
            Key::Down => {
                self.highlighted = Some(self.highlighted.map_or(0, |at| (at + 1).min(last)));
            }
            Key::Home => self.highlighted = Some(0),
            Key::End => self.highlighted = Some(last),
            Key::Character(letter) => {
                let from = self.highlighted.map_or(0, |at| at + 1);
                let found = (0..GridMenuItem::ALL.len())
                    .map(|step| (from + step) % GridMenuItem::ALL.len())
                    .find(|&index| {
                        GridMenuItem::ALL[index]
                            .label()
                            .chars()
                            .next()
                            .is_some_and(|first| first.eq_ignore_ascii_case(&letter))
                    });
                if found.is_some() {
                    self.highlighted = found;
                }
            }
            _ => {}
        }
        MenuAnswer::Moved
    }
}

pub(crate) fn draw(
    paint: &mut Painter<'_, '_>,
    menu: &GridMenu,
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
    for (index, item) in GridMenuItem::ALL.iter().enumerate() {
        let top = rect.y + item_top(index);
        if menu.highlighted.or(hovered) == Some(index) {
            paint.canvas.fill_rounded_rect(
                Rect::new(rect.x + 4.0, top, rect.w - 8.0, ROW_HEIGHT),
                4.0,
                theme.selection,
            );
        }
        paint.text(
            faces.ui,
            item.label(),
            rect.x + LABEL_LEFT,
            top + 5.0,
            BODY,
            theme.text_primary,
        );
        if let Some(shortcut) = item.shortcut(primary) {
            paint.text_right(
                faces.ui,
                &shortcut,
                rect.x + SHORTCUT_RIGHT,
                top + 5.0,
                BODY,
                theme.text_secondary,
            );
        }
        if item.ends_group() {
            paint.rect(rect.x, top + ROW_HEIGHT + 4.0, rect.w, 1.0, theme.border);
        }
    }
}

pub(crate) fn nodes(nodes: &mut Vec<AccessNode>, menu: &GridMenu, rect: Rect, parent: u32) {
    let bounds = |rect: Rect| AccessRect {
        x: rect.x,
        y: rect.y,
        width: rect.w,
        height: rect.h,
    };
    nodes.push(AccessNode {
        id: NODE_GRID_MENU,
        parent: Some(parent),
        role: AccessRole::Group,
        name: "Results menu".to_owned(),
        bounds: bounds(rect),
        ..AccessNode::default()
    });
    let primary = primary_name();
    for (index, item) in GridMenuItem::ALL.iter().enumerate() {
        let mut node = AccessNode {
            id: NODE_FIRST_GRID_MENU_ITEM + index as u32,
            parent: Some(NODE_GRID_MENU),
            role: AccessRole::Button,
            name: item.label().to_owned(),
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
        node.actions.invoke = true;
        nodes.push(node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rows_and_separators_add_up_to_the_designed_height() {
        assert_eq!(item_top(0), 4.0);
        assert_eq!(item_top(4), 116.0);
        assert_eq!(item_top(5), 153.0);
        assert_eq!(item_top(6), 190.0);
        assert_eq!(item_top(8), 246.0);
        assert_eq!(item_top(8) + ROW_HEIGHT + PADDING, HEIGHT);
        let rect = Rect::new(400.0, 300.0, WIDTH, HEIGHT);
        assert_eq!(item_at(rect, 410.0, 300.0 + 153.0), Some(5));
        assert_eq!(item_at(rect, 410.0, 300.0 + 150.0), None);
        assert_eq!(item_at(rect, 390.0, 310.0), None);
    }

    #[test]
    fn the_menu_opens_at_the_pointer_or_below_the_cell_and_stays_in_the_window() {
        assert_eq!(
            rect_for(
                MenuOrigin::Pointer { x: 500.0, y: 460.0 },
                Rect::new(0.0, 0.0, 0.0, 0.0),
                1280.0
            ),
            Rect::new(500.0, 460.0, WIDTH, HEIGHT)
        );
        assert_eq!(
            rect_for(
                MenuOrigin::Pointer {
                    x: 1200.0,
                    y: 600.0
                },
                Rect::new(0.0, 0.0, 0.0, 0.0),
                1280.0
            ),
            Rect::new(1040.0, 322.0, WIDTH, HEIGHT)
        );
        assert_eq!(
            rect_for(
                MenuOrigin::CurrentCell,
                Rect::new(308.0, 446.0, 100.0, 24.0),
                1280.0
            ),
            Rect::new(308.0, 474.0, WIDTH, HEIGHT)
        );
    }

    #[test]
    fn keys_move_without_wrapping_letters_cycle_and_enter_activates() {
        let mut menu = GridMenu::at_current_cell();
        menu.key(KeyPress::plain(Key::Up));
        assert_eq!(menu.highlighted, Some(0));
        menu.key(KeyPress::plain(Key::End));
        menu.key(KeyPress::plain(Key::Down));
        assert_eq!(menu.highlighted, Some(8));
        menu.key(KeyPress::plain(Key::Character('c')));
        assert_eq!(menu.highlighted, Some(0));
        menu.key(KeyPress::plain(Key::Character('c')));
        assert_eq!(menu.highlighted, Some(1));
        menu.key(KeyPress::plain(Key::Character('o')));
        assert_eq!(
            menu.key(KeyPress::plain(Key::Enter)),
            MenuAnswer::Activate(GridMenuItem::OpenValue)
        );
        let mut pointer = GridMenu::at_pointer(10.0, 10.0);
        assert_eq!(pointer.key(KeyPress::plain(Key::Enter)), MenuAnswer::Moved);
        pointer.key(KeyPress::plain(Key::Down));
        assert_eq!(pointer.highlighted, Some(0));
        assert_eq!(pointer.key(KeyPress::plain(Key::Escape)), MenuAnswer::Close);
    }
}
