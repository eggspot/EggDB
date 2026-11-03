use eggdb_platform::window::{
    AccessAction, AccessNode, AccessRect, AccessRole, AccessTreePosition, Key, KeyPress,
};

use crate::canvas::{Canvas, Rect};
use crate::color::Theme;
use crate::font::{TextStyle, Typeface};
use crate::layout::{Layout, TRADEMARK_NOTE};
use crate::main_window::{Faces, elided};

/// The dialog's title.
pub const TITLE: &str = "Pick a layout";
/// The dialog's size in logical pixels.
pub const DIALOG_SIZE: (f32, f32) = (560.0, 504.0);
/// The height of one row.
pub const ROW_HEIGHT: f32 = 44.0;
const LIST: Rect = Rect::new(24.0, 52.0, 512.0, 352.0);
const CANCEL: Rect = Rect::new(364.0, 456.0, 84.0, 28.0);
const APPLY: Rect = Rect::new(456.0, 456.0, 80.0, 28.0);
const NOTE_TOP: f32 = 416.0;
const TITLE_STYLE: (f32, f32) = (15.0, 20.0);
const BODY: (f32, f32) = (13.0, 18.0);
const SMALL: (f32, f32) = (11.0, 14.0);

/// The picker's accessibility node ids, from [`NODE_DIALOG`] on.
pub const NODE_DIALOG: u32 = 63_000;
/// The title.
pub const NODE_TITLE: u32 = 63_001;
/// The list of layouts.
pub const NODE_LIST: u32 = 63_002;
/// The trademark note.
pub const NODE_NOTE: u32 = 63_003;
/// `Cancel`.
pub const NODE_CANCEL: u32 = 63_004;
/// `Apply`.
pub const NODE_APPLY: u32 = 63_005;
/// The first row; row `n` (from 0) is this plus `n`.
pub const NODE_FIRST_ROW: u32 = 63_010;

/// Where the picker's keyboard focus is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PickerFocus {
    /// The list of layouts.
    #[default]
    List,
    /// `Cancel`.
    Cancel,
    /// `Apply`.
    Apply,
}

/// What a key in the picker decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerOutcome {
    /// The picker stays open.
    Open,
    /// The picker closes and the layout stays as it was.
    Keep,
    /// The picker closes and this layout applies.
    Apply(Layout),
}

/// The layout picker (`docs/ux/gui-layouts.md`, the layout picker): the eight layouts, the
/// active one checked, one highlighted, and where the focus is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutPicker {
    /// The layout in use when it opened.
    pub active: Layout,
    /// The highlighted row, from 0.
    pub highlighted: usize,
    /// Where the focus is.
    pub focus: PickerFocus,
}

impl LayoutPicker {
    /// The picker as it opens over `active`: the focus on the list, `active` highlighted.
    pub fn opened(active: Layout) -> Self {
        Self {
            active,
            highlighted: row_of(active),
            focus: PickerFocus::List,
        }
    }

    /// The highlighted layout.
    pub fn highlighted_layout(&self) -> Layout {
        Layout::ALL[self.highlighted.min(Layout::ALL.len() - 1)]
    }

    /// Answers a key: `Up`, `Down`, `Home`, `End`, `PgUp`, `PgDn` and letters move the
    /// highlight, `Tab` moves the focus, `Enter` applies (on `Cancel` it cancels), `Space`
    /// activates a button and `Esc` keeps the layout.
    pub fn key(&mut self, press: KeyPress) -> PickerOutcome {
        let last = Layout::ALL.len() - 1;
        let held = press.modifiers;
        if held.control || held.command || held.alt {
            return PickerOutcome::Open;
        }
        match press.key {
            Key::Escape => return PickerOutcome::Keep,
            Key::Enter if self.focus == PickerFocus::Cancel => return PickerOutcome::Keep,
            Key::Enter => return self.apply(),
            Key::Space => match self.focus {
                PickerFocus::Cancel => return PickerOutcome::Keep,
                PickerFocus::Apply => return self.apply(),
                PickerFocus::List => {}
            },
            Key::Tab => {
                let order = [PickerFocus::List, PickerFocus::Cancel, PickerFocus::Apply];
                let at = order
                    .iter()
                    .position(|focus| *focus == self.focus)
                    .unwrap_or(0);
                let next = if held.shift {
                    at + order.len() - 1
                } else {
                    at + 1
                };
                self.focus = order[next % order.len()];
            }
            Key::Up if self.focus == PickerFocus::List => {
                self.highlighted = self.highlighted.saturating_sub(1);
            }
            Key::Down if self.focus == PickerFocus::List => {
                self.highlighted = (self.highlighted + 1).min(last);
            }
            Key::Home | Key::PageUp if self.focus == PickerFocus::List => self.highlighted = 0,
            Key::End | Key::PageDown if self.focus == PickerFocus::List => self.highlighted = last,
            Key::Character(letter) if self.focus == PickerFocus::List => self.jump_to(letter),
            _ => {}
        }
        PickerOutcome::Open
    }

    /// A click on row `index`: it is highlighted and the list takes the focus; a double
    /// click applies it.
    pub fn click_row(&mut self, index: usize, clicks: u8) -> PickerOutcome {
        if index >= Layout::ALL.len() {
            return PickerOutcome::Open;
        }
        self.highlighted = index;
        self.focus = PickerFocus::List;
        if clicks >= 2 {
            self.apply()
        } else {
            PickerOutcome::Open
        }
    }

    fn apply(&self) -> PickerOutcome {
        let chosen = self.highlighted_layout();
        if chosen == self.active {
            PickerOutcome::Keep
        } else {
            PickerOutcome::Apply(chosen)
        }
    }

    fn jump_to(&mut self, letter: char) {
        let wanted = letter.to_ascii_lowercase();
        let count = Layout::ALL.len();
        if let Some(found) = (1..=count)
            .map(|step| (self.highlighted + step) % count)
            .find(|index| {
                Layout::ALL[*index]
                    .label()
                    .chars()
                    .next()
                    .is_some_and(|first| first.to_ascii_lowercase() == wanted)
            })
        {
            self.highlighted = found;
        }
    }
}

fn row_of(layout: Layout) -> usize {
    Layout::ALL
        .iter()
        .position(|each| *each == layout)
        .unwrap_or(0)
}

/// The dialog's top-left in a window of `size` logical pixels, centred in the whole window.
pub fn dialog_origin(size: (f32, f32)) -> (f32, f32) {
    (
        ((size.0 - DIALOG_SIZE.0) / 2.0).round().max(0.0),
        ((size.1 - DIALOG_SIZE.1) / 2.0).round().max(0.0),
    )
}

fn contains(rect: Rect, x: f32, y: f32) -> bool {
    (rect.x..rect.x + rect.w).contains(&x) && (rect.y..rect.y + rect.h).contains(&y)
}

/// Row `index`'s rect in a window of `size`.
pub fn row_rect(size: (f32, f32), index: usize) -> Rect {
    let (dx, dy) = dialog_origin(size);
    Rect::new(
        dx + LIST.x,
        dy + LIST.y + ROW_HEIGHT * index as f32,
        LIST.w,
        ROW_HEIGHT,
    )
}

/// The row under (`x`, `y`) in a window of `size`, if any.
pub fn row_at(size: (f32, f32), x: f32, y: f32) -> Option<usize> {
    (0..Layout::ALL.len()).find(|index| contains(row_rect(size, *index), x, y))
}

/// Answers a press of the pointer at (`x`, `y`) in a window of `size` with `clicks` counted:
/// a row is highlighted or (double click) applied, `Cancel` keeps, `Apply` applies, and a
/// press on the scrim outside the dialog keeps the layout.
pub fn press_at(
    picker: &mut LayoutPicker,
    size: (f32, f32),
    x: f32,
    y: f32,
    clicks: u8,
) -> PickerOutcome {
    if let Some(row) = row_at(size, x, y) {
        return picker.click_row(row, clicks);
    }
    let origin = dialog_origin(size);
    let inside = |rect: Rect| contains(offset(rect, origin), x, y);
    if inside(CANCEL) {
        return PickerOutcome::Keep;
    }
    if inside(APPLY) {
        return picker.apply();
    }
    if inside(Rect::new(0.0, 0.0, DIALOG_SIZE.0, DIALOG_SIZE.1)) {
        PickerOutcome::Open
    } else {
        PickerOutcome::Keep
    }
}

struct Ink<'c, 'f> {
    canvas: &'c mut Canvas,
    faces: Faces<'f>,
    scale: f32,
}

impl Ink<'_, '_> {
    fn device(&self, rect: Rect) -> Rect {
        let s = self.scale;
        Rect::new(
            (rect.x * s).round(),
            (rect.y * s).round(),
            (rect.w * s).round(),
            (rect.h * s).round(),
        )
    }

    fn fill(&mut self, rect: Rect, color: crate::Rgba) {
        let rect = self.device(rect);
        self.canvas.fill_rect(rect, color);
    }

    fn rounded(&mut self, rect: Rect, radius: f32, color: crate::Rgba) {
        let rect = self.device(rect);
        self.canvas
            .fill_rounded_rect(rect, radius * self.scale, color);
    }

    fn text(
        &mut self,
        face: &dyn Typeface,
        text: &str,
        at: (f32, f32),
        metrics: (f32, f32),
        color: crate::Rgba,
    ) {
        let s = self.scale;
        let style = TextStyle {
            size: metrics.0 * s,
            line_height: metrics.1 * s,
            color,
        };
        face.draw_text(
            self.canvas,
            text,
            (at.0 * s).round(),
            (at.1 * s).round(),
            style,
        );
    }

    fn line(&mut self, from: (f32, f32), to: (f32, f32), width: f32, color: crate::Rgba) {
        let s = self.scale;
        self.canvas.line(
            (from.0 * s, from.1 * s),
            (to.0 * s, to.1 * s),
            width * s,
            color,
        );
    }

    fn centred(&mut self, text: &str, rect: Rect, color: crate::Rgba) {
        let face = self.faces.ui;
        let width = face.width(text, BODY.0);
        let at = (
            rect.x + (rect.w - width) / 2.0,
            rect.y + (rect.h - BODY.1) / 2.0,
        );
        self.text(face, text, at, BODY, color);
    }
}

fn offset(rect: Rect, by: (f32, f32)) -> Rect {
    Rect::new(rect.x + by.0, rect.y + by.1, rect.w, rect.h)
}

fn note_lines(face: &dyn Typeface) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in TRADEMARK_NOTE.split(' ') {
        let tried = if line.is_empty() {
            word.to_owned()
        } else {
            format!("{line} {word}")
        };
        if !line.is_empty() && face.width(&tried, SMALL.0) > LIST.w {
            lines.push(std::mem::replace(&mut line, word.to_owned()));
        } else {
            line = tried;
        }
    }
    lines.push(line);
    lines
}

/// Draws the picker over `canvas`, a frame of a window `size` logical pixels big at `scale`
/// device pixels per logical pixel: the scrim over the whole window, then the dialog.
pub fn draw(
    canvas: &mut Canvas,
    picker: &LayoutPicker,
    theme: Theme,
    faces: Faces<'_>,
    size: (f32, f32),
    scale: f32,
) {
    let mut ink = Ink {
        canvas,
        faces,
        scale,
    };
    ink.fill(Rect::new(0.0, 0.0, size.0, size.1), theme.scrim);
    let origin = dialog_origin(size);
    let dialog = offset(Rect::new(0.0, 0.0, DIALOG_SIZE.0, DIALOG_SIZE.1), origin);
    ink.rounded(dialog, 8.0, theme.border);
    ink.rounded(
        Rect::new(
            dialog.x + 1.0,
            dialog.y + 1.0,
            dialog.w - 2.0,
            dialog.h - 2.0,
        ),
        7.0,
        theme.bg0,
    );
    ink.text(
        faces.ui_bold,
        TITLE,
        (origin.0 + 24.0, origin.1 + 20.0),
        TITLE_STYLE,
        theme.text_primary,
    );
    for (index, layout) in Layout::ALL.into_iter().enumerate() {
        let row = row_rect(size, index);
        if index == picker.highlighted {
            let fill = if picker.focus == PickerFocus::List {
                theme.selection
            } else {
                theme.selection_inactive
            };
            ink.rounded(row, 4.0, fill);
            if picker.focus == PickerFocus::List {
                ring(&mut ink, row, theme);
            }
        }
        let label_face = if layout == picker.active {
            faces.ui_bold
        } else {
            faces.ui
        };
        ink.text(
            label_face,
            layout.label(),
            (row.x + 36.0, row.y + 6.0),
            BODY,
            theme.text_primary,
        );
        let room = origin.0 + 524.0 - (row.x + 36.0);
        let description = elided(faces.ui, layout.description(), room, SMALL.0);
        ink.text(
            faces.ui,
            &description,
            (row.x + 36.0, row.y + 24.0),
            SMALL,
            theme.text_secondary,
        );
        if layout == picker.active {
            let (x, y) = (origin.0 + 36.0, row.y + 9.0);
            ink.line(
                (x + 2.0, y + 6.0),
                (x + 5.0, y + 9.0),
                1.5,
                theme.text_primary,
            );
            ink.line(
                (x + 5.0, y + 9.0),
                (x + 10.0, y + 3.0),
                1.5,
                theme.text_primary,
            );
        }
    }
    for (index, line) in note_lines(faces.ui).iter().enumerate() {
        ink.text(
            faces.ui,
            line,
            (origin.0 + 24.0, origin.1 + NOTE_TOP + 14.0 * index as f32),
            SMALL,
            theme.text_secondary,
        );
    }
    let cancel = offset(CANCEL, origin);
    ink.rounded(cancel, 4.0, theme.border);
    ink.rounded(
        Rect::new(
            cancel.x + 1.0,
            cancel.y + 1.0,
            cancel.w - 2.0,
            cancel.h - 2.0,
        ),
        3.0,
        theme.bg0,
    );
    ink.centred("Cancel", cancel, theme.text_primary);
    let apply = offset(APPLY, origin);
    ink.rounded(apply, 4.0, theme.accent);
    ink.centred("Apply", apply, theme.on_accent);
    match picker.focus {
        PickerFocus::Cancel => ring(&mut ink, grow(cancel, 2.0), theme),
        PickerFocus::Apply => ring(&mut ink, grow(apply, 2.0), theme),
        PickerFocus::List => {}
    }
}

fn grow(rect: Rect, by: f32) -> Rect {
    Rect::new(
        rect.x - by,
        rect.y - by,
        rect.w + 2.0 * by,
        rect.h + 2.0 * by,
    )
}

fn ring(ink: &mut Ink<'_, '_>, rect: Rect, theme: Theme) {
    let inset = Rect::new(rect.x + 1.0, rect.y + 1.0, rect.w - 2.0, rect.h - 2.0);
    let device = ink.device(inset);
    let width = (theme.focus_ring_width() * ink.scale).round().max(1.0);
    for step in 0..width as usize {
        let by = step as f32;
        ink.canvas.stroke_rect(
            Rect::new(
                device.x + by,
                device.y + by,
                device.w - 2.0 * by,
                device.h - 2.0 * by,
            ),
            theme.focus_ring,
        );
    }
}

fn node(id: u32, parent: u32, role: AccessRole, name: &str, rect: Rect) -> AccessNode {
    AccessNode {
        id,
        parent: Some(parent),
        role,
        name: name.to_owned(),
        bounds: AccessRect {
            x: rect.x,
            y: rect.y,
            width: rect.w,
            height: rect.h,
        },
        ..AccessNode::default()
    }
}

/// The picker's accessibility nodes under `parent` in a window of `size`: the dialog, its
/// title, the list with one item per layout (position, highlighted as selected, the active
/// one checked), the trademark note and the two buttons.
pub fn accessibility_nodes(
    picker: &LayoutPicker,
    parent: u32,
    size: (f32, f32),
) -> Vec<AccessNode> {
    let origin = dialog_origin(size);
    let dialog_rect = offset(Rect::new(0.0, 0.0, DIALOG_SIZE.0, DIALOG_SIZE.1), origin);
    let mut dialog = node(NODE_DIALOG, parent, AccessRole::Group, TITLE, dialog_rect);
    dialog.description = Some(TRADEMARK_NOTE.to_owned());
    let mut nodes = vec![
        dialog,
        node(
            NODE_TITLE,
            NODE_DIALOG,
            AccessRole::Heading,
            TITLE,
            Rect::new(origin.0 + 24.0, origin.1 + 20.0, 512.0, TITLE_STYLE.1),
        ),
    ];
    let mut list = node(
        NODE_LIST,
        NODE_DIALOG,
        AccessRole::Tree,
        "Layouts",
        offset(LIST, origin),
    );
    list.state.focused = picker.focus == PickerFocus::List;
    nodes.push(list);
    for (index, layout) in Layout::ALL.into_iter().enumerate() {
        let mut row = node(
            NODE_FIRST_ROW + index as u32,
            NODE_LIST,
            AccessRole::TreeItem,
            layout.label(),
            row_rect(size, index),
        );
        row.description = Some(layout.description().to_owned());
        row.state.selected = index == picker.highlighted;
        row.state.checked = Some(layout == picker.active);
        row.tree_position = Some(AccessTreePosition {
            level: 1,
            position: index + 1,
            set_size: Layout::ALL.len(),
        });
        row.actions.select = true;
        row.actions.invoke = true;
        nodes.push(row);
    }
    nodes.push(node(
        NODE_NOTE,
        NODE_DIALOG,
        AccessRole::Status,
        TRADEMARK_NOTE,
        Rect::new(origin.0 + 24.0, origin.1 + NOTE_TOP, LIST.w, 28.0),
    ));
    for (id, name, rect, focus) in [
        (NODE_CANCEL, "Cancel", CANCEL, PickerFocus::Cancel),
        (NODE_APPLY, "Apply", APPLY, PickerFocus::Apply),
    ] {
        let mut button = node(
            id,
            NODE_DIALOG,
            AccessRole::Button,
            name,
            offset(rect, origin),
        );
        button.state.focused = picker.focus == focus;
        button.state.default_button = id == NODE_APPLY;
        button.actions.invoke = true;
        nodes.push(button);
    }
    nodes
}

/// The keys that do a screen reader's `action` on the picker's `node`: a row is
/// highlighted (and applied on invoke), `Cancel` keeps and `Apply` applies. Nodes outside the
/// picker get nothing, as the window under it takes no input.
pub fn access_keys(
    picker: &mut LayoutPicker,
    node: &AccessNode,
    action: &AccessAction,
) -> Vec<KeyPress> {
    let invoke = *action == AccessAction::Invoke;
    match node.id {
        NODE_CANCEL if invoke => vec![KeyPress::plain(Key::Escape)],
        NODE_APPLY if invoke => {
            picker.focus = PickerFocus::Apply;
            vec![KeyPress::plain(Key::Enter)]
        }
        id if (NODE_FIRST_ROW..NODE_FIRST_ROW + Layout::ALL.len() as u32).contains(&id) => {
            picker.highlighted = (id - NODE_FIRST_ROW) as usize;
            picker.focus = PickerFocus::List;
            match action {
                AccessAction::Invoke => vec![KeyPress::plain(Key::Enter)],
                _ => Vec::new(),
            }
        }
        NODE_LIST if *action == AccessAction::Focus => {
            picker.focus = PickerFocus::List;
            Vec::new()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_platform::window::Modifiers;

    fn key(picker: &mut LayoutPicker, key: Key) -> PickerOutcome {
        picker.key(KeyPress::plain(key))
    }

    #[test]
    fn the_keys_move_the_highlight_without_wrapping_and_enter_applies() {
        let mut picker = LayoutPicker::opened(Layout::EggDb);
        assert_eq!(picker.highlighted, 0);
        for _ in 0..9 {
            assert_eq!(key(&mut picker, Key::Down), PickerOutcome::Open);
        }
        assert_eq!(picker.highlighted_layout(), Layout::RedisInsight);
        key(&mut picker, Key::Home);
        assert_eq!(picker.highlighted, 0);
        key(&mut picker, Key::Up);
        assert_eq!(picker.highlighted, 0);
        let mut seen = Vec::new();
        for _ in 0..7 {
            key(&mut picker, Key::Character('W'));
            seen.push(picker.highlighted);
        }
        assert_eq!(seen, [1, 2, 3, 4, 5, 6, 7]);
        key(&mut picker, Key::Character('w'));
        assert_eq!(picker.highlighted, 1);
        key(&mut picker, Key::Character('e'));
        assert_eq!(picker.highlighted, 0);
        key(&mut picker, Key::End);
        assert_eq!(picker.highlighted, 7);
        key(&mut picker, Key::PageUp);
        key(&mut picker, Key::Down);
        key(&mut picker, Key::Down);
        assert_eq!(key(&mut picker, Key::Escape), PickerOutcome::Keep);
        assert_eq!(
            key(&mut picker, Key::Enter),
            PickerOutcome::Apply(Layout::PgAdmin)
        );
        assert_eq!(Layout::PgAdmin.key(), "pgadmin");
    }

    #[test]
    fn tab_cycles_the_list_and_buttons_and_the_buttons_answer_enter_and_space() {
        let mut picker = LayoutPicker::opened(Layout::Ssms);
        assert_eq!(picker.highlighted, 1);
        key(&mut picker, Key::Down);
        key(&mut picker, Key::Tab);
        assert_eq!(picker.focus, PickerFocus::Cancel);
        assert_eq!(key(&mut picker, Key::Down), PickerOutcome::Open);
        assert_eq!(picker.highlighted, 2);
        assert_eq!(key(&mut picker, Key::Enter), PickerOutcome::Keep);
        key(&mut picker, Key::Tab);
        assert_eq!(picker.focus, PickerFocus::Apply);
        assert_eq!(
            key(&mut picker, Key::Space),
            PickerOutcome::Apply(Layout::PgAdmin)
        );
        key(&mut picker, Key::Tab);
        assert_eq!(picker.focus, PickerFocus::List);
        let back = KeyPress {
            key: Key::Tab,
            modifiers: Modifiers {
                shift: true,
                ..Modifiers::default()
            },
        };
        picker.key(back);
        assert_eq!(picker.focus, PickerFocus::Apply);
        assert_eq!(
            key(&mut picker, Key::Space),
            PickerOutcome::Apply(Layout::PgAdmin)
        );
        let mut unchanged = LayoutPicker::opened(Layout::Compass);
        assert_eq!(key(&mut unchanged, Key::Enter), PickerOutcome::Keep);
        assert_eq!(key(&mut unchanged, Key::Space), PickerOutcome::Open);
    }

    #[test]
    fn clicks_highlight_and_double_clicks_apply_rows_found_by_position() {
        let size = (1280.0, 800.0);
        assert_eq!(dialog_origin(size), (360.0, 148.0));
        assert_eq!(row_rect(size, 2), Rect::new(384.0, 288.0, 512.0, 44.0));
        assert_eq!(row_at(size, 400.0, 290.0), Some(2));
        assert_eq!(row_at(size, 10.0, 10.0), None);
        let mut picker = LayoutPicker::opened(Layout::EggDb);
        picker.focus = PickerFocus::Apply;
        assert_eq!(picker.click_row(4, 1), PickerOutcome::Open);
        assert_eq!(picker.focus, PickerFocus::List);
        assert_eq!(
            picker.click_row(5, 2),
            PickerOutcome::Apply(Layout::Workbench)
        );
        assert_eq!(picker.click_row(99, 2), PickerOutcome::Open);
        assert_eq!(
            press_at(&mut picker, size, 400.0, 250.0, 1),
            PickerOutcome::Open
        );
        assert_eq!(picker.highlighted, 1);
        assert_eq!(
            press_at(&mut picker, size, 380.0, 170.0, 1),
            PickerOutcome::Open
        );
        assert_eq!(
            press_at(&mut picker, size, 820.0, 610.0, 1),
            PickerOutcome::Apply(Layout::Ssms)
        );
        assert_eq!(
            press_at(&mut picker, size, 730.0, 610.0, 1),
            PickerOutcome::Keep
        );
        assert_eq!(
            press_at(&mut picker, size, 10.0, 10.0, 1),
            PickerOutcome::Keep
        );
    }

    #[test]
    fn the_tree_names_the_dialog_rows_and_buttons() {
        let mut picker = LayoutPicker::opened(Layout::EggDb);
        picker.highlighted = 2;
        let nodes = accessibility_nodes(&picker, 1, (1280.0, 800.0));
        assert_eq!(nodes.len(), 3 + 8 + 3);
        assert_eq!(nodes[0].name, TITLE);
        assert_eq!(nodes[0].description.as_deref(), Some(TRADEMARK_NOTE));
        let rows: Vec<&AccessNode> = nodes
            .iter()
            .filter(|node| node.role == AccessRole::TreeItem)
            .collect();
        assert_eq!(rows.len(), 8);
        assert_eq!(rows[0].state.checked, Some(true));
        assert!(rows[2].state.selected && rows[2].name == "Works like pgAdmin 4");
        assert_eq!(
            rows[7].tree_position.map(|position| position.position),
            Some(8)
        );
        let apply = nodes
            .iter()
            .find(|node| node.id == NODE_APPLY)
            .expect("Apply");
        assert!(apply.state.default_button);
        let row = nodes
            .iter()
            .find(|node| node.id == NODE_FIRST_ROW + 6)
            .expect("row 7")
            .clone();
        assert!(access_keys(&mut picker, &row, &AccessAction::Select).is_empty());
        assert_eq!(picker.highlighted_layout(), Layout::Compass);
        assert_eq!(
            access_keys(&mut picker, &row, &AccessAction::Invoke),
            vec![KeyPress::plain(Key::Enter)]
        );
        assert_eq!(
            picker.key(KeyPress::plain(Key::Enter)),
            PickerOutcome::Apply(Layout::Compass)
        );
        assert!(access_keys(&mut picker, &nodes[0], &AccessAction::Invoke).is_empty());
    }
}
