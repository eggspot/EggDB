use eggdb_platform::window::{AccessNode, AccessRect, AccessRole, Key, KeyPress};

use crate::canvas::Rect;
use crate::grid::{Cell, ResultGrid, ValueKind, full_hex, is_json, pretty_json};
use crate::main_window::{BODY, CODE, Painter, elided, focus_ring};
use crate::scrollbar::{self, Axis, Extent};
use crate::status_line;

/// The view's width.
pub const WIDTH: f32 = 480.0;
/// The view's height.
pub const HEIGHT: f32 = 240.0;
/// How many characters a body line holds before it wraps.
pub const GLYPHS_PER_LINE: usize = 58;
/// How many body lines show at once.
pub const LINES_IN_VIEW: usize = 10;
/// How many bytes of a value the body shows before it stops.
pub const BODY_LIMIT: usize = 1 << 20;
/// The view's accessibility node.
pub const NODE_VALUE_VIEW: u32 = 64_100;
/// The read-only body's node.
pub const NODE_VALUE_BODY: u32 = 64_101;
/// The `Copy` button's node.
pub const NODE_VALUE_COPY: u32 = 64_102;

const HEADER_HEIGHT: f32 = 32.0;
const NAME_RIGHT: f32 = 404.0;
const TEXT_LEFT: f32 = 12.0;
const BODY_TEXT_TOP: f32 = 40.0;
const LINE_HEIGHT: f32 = 20.0;
const PAGE_LINES: usize = 9;
const BODY_BOTTOM: f32 = 776.0;
const MIN_LEFT: f32 = 260.0;
const GAP: f32 = 4.0;
const COPY_BUTTON: (f32, f32, f32, f32) = (412.0, 2.0, 56.0, 28.0);

/// What a key did in the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueAnswer {
    /// The body scrolled, the focus moved, or nothing happened.
    Moved,
    /// `Esc`, `Enter` or `F2`: the view closes and the focus goes back to its cell.
    Close,
    /// `Ctrl/Cmd+C` or `Copy`: the text for the clipboard and its toast.
    Copy {
        /// The value's full text as shown.
        text: String,
        /// The toast, such as `Copied 1,204 characters`.
        toast: String,
    },
}

/// The full-value view on one cell (`docs/ux/workspace.md` 9.3): the value laid out in full,
/// wrapped per character, scrolled by lines, with `Copy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueView {
    /// The cell it shows.
    pub cell: Cell,
    /// The column's name.
    pub column: String,
    /// What follows the name in the header: the kind, and a blob's size.
    pub detail: String,
    /// The value's full text as shown, which `Copy` copies.
    pub text: String,
    /// The body's lines, wrapped, up to [`BODY_LIMIT`].
    pub lines: Vec<String>,
    /// The last line when the body stops early: `… and 2.1 MB more`.
    pub cut: Option<String>,
    /// Whether the value is NULL or the empty string, drawn `textDisabled` and not copyable.
    pub placeholder: bool,
    /// A blob's size in bytes.
    pub blob_bytes: Option<usize>,
    /// The first body line in view.
    pub first_line: usize,
    /// Whether `Copy` has the focus rather than the body.
    pub copy_focused: bool,
}

/// The text the view shows for `value` of `kind`: JSON and arrays pretty-printed, a vector as
/// a JSON array a value a line, a blob as `0x` and its full hex, NULL as `NULL` and the empty
/// string as `""`; with whether it is one of those two placeholders.
pub fn shown_value(value: Option<&str>, kind: ValueKind) -> (String, bool) {
    match value {
        None => ("NULL".to_owned(), true),
        Some("") => ("\"\"".to_owned(), true),
        Some(text) => match kind {
            ValueKind::Json | ValueKind::Vector => (pretty_json(text), false),
            ValueKind::SpelledBytes if is_json(text) => (pretty_json(text), false),
            ValueKind::Blob => (full_hex(text), false),
            _ => (text.to_owned(), false),
        },
    }
}

fn visible_char(character: char) -> char {
    match character {
        '\t' => ' ',
        '\u{0}'..='\u{1f}' => char::from_u32(0x2400 + character as u32).unwrap_or('·'),
        '\u{7f}' => '\u{2421}',
        other => other,
    }
}

/// `text`'s body lines: its own line breaks kept, each line wrapped every `width` characters,
/// the text cut at `limit` bytes; with how many bytes the cut left out.
pub fn wrapped_lines(text: &str, width: usize, limit: usize) -> (Vec<String>, usize) {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let shown = &text[..end];
    let mut lines = Vec::new();
    for line in shown.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let characters: Vec<char> = line.chars().map(visible_char).collect();
        if characters.is_empty() {
            lines.push(String::new());
        }
        for chunk in characters.chunks(width.max(1)) {
            lines.push(chunk.iter().collect());
        }
    }
    (lines, text.len() - end)
}

/// The view's rect for a cell at `cell` in a window `window_width` wide: left-aligned to the
/// cell within the window, 4 below it, or above it when it would cross the body's bottom.
pub fn popover_rect(cell: Rect, window_width: f32) -> Rect {
    let x = cell.x.clamp(MIN_LEFT, (window_width - WIDTH).max(MIN_LEFT));
    let below = cell.y + cell.h + GAP;
    let y = if below + HEIGHT > BODY_BOTTOM {
        cell.y - HEIGHT - GAP
    } else {
        below
    };
    Rect::new(x, y, WIDTH, HEIGHT)
}

/// The `Copy` button's rect in a view at `view`.
pub fn copy_rect(view: Rect) -> Rect {
    let (x, y, w, h) = COPY_BUTTON;
    Rect::new(view.x + x, view.y + y, w, h)
}

impl ValueView {
    /// The view on `cell` of `grid`, scrolled to the top with the body focused.
    pub fn open(grid: &ResultGrid, cell: Cell) -> Self {
        let kind = grid.kind(cell.column);
        let value = grid.value(cell);
        let (text, placeholder) = shown_value(value, kind);
        let (lines, left_out) = wrapped_lines(&text, GLYPHS_PER_LINE, BODY_LIMIT);
        let blob_bytes = (kind == ValueKind::Blob && !placeholder)
            .then_some(text.len().saturating_sub(2) / 2);
        let detail = match blob_bytes {
            Some(bytes) => format!("{} · {}", kind.label(), bytes_label(bytes)),
            None => kind.label().to_owned(),
        };
        Self {
            cell,
            column: grid.columns.get(cell.column).cloned().unwrap_or_default(),
            detail,
            cut: (left_out > 0)
                .then(|| format!("… and {} more", status_line::bytes_text(left_out as u64))),
            text,
            lines,
            placeholder,
            blob_bytes,
            first_line: 0,
            copy_focused: false,
        }
    }

    /// Whether `Copy` works: not for NULL or the empty string.
    pub fn copyable(&self) -> bool {
        !self.placeholder
    }

    fn line_count(&self) -> usize {
        self.lines.len() + usize::from(self.cut.is_some())
    }

    fn last_first_line(&self) -> usize {
        self.line_count().saturating_sub(LINES_IN_VIEW)
    }

    /// Scrolls the body by `lines`, within its lines.
    pub fn scroll_by(&mut self, lines: isize) {
        self.first_line = self
            .first_line
            .saturating_add_signed(lines)
            .min(self.last_first_line());
    }

    /// What `Copy` puts on the clipboard, with its toast; `None` for NULL and `""`.
    pub fn copy(&self) -> Option<ValueAnswer> {
        if !self.copyable() {
            return None;
        }
        Some(ValueAnswer::Copy {
            text: self.text.clone(),
            toast: status_line::copied_value_text(self.text.chars().count(), self.blob_bytes),
        })
    }

    /// Answers a key while the view is open.
    pub fn key(&mut self, press: KeyPress) -> ValueAnswer {
        let primary = press.modifiers.control || press.modifiers.command;
        let page = PAGE_LINES as isize;
        match press.key {
            Key::Escape | Key::F2 => return ValueAnswer::Close,
            Key::Enter | Key::Space if self.copy_focused => {
                return self.copy().unwrap_or(ValueAnswer::Moved);
            }
            Key::Enter => return ValueAnswer::Close,
            Key::Character('c' | 'C') if primary => {
                return self.copy().unwrap_or(ValueAnswer::Moved);
            }
            Key::Tab => self.copy_focused = !self.copy_focused && self.copyable(),
            Key::Up => self.scroll_by(-1),
            Key::Down => self.scroll_by(1),
            Key::PageUp => self.scroll_by(-page),
            Key::PageDown => self.scroll_by(page),
            Key::Home => self.first_line = 0,
            Key::End => self.first_line = self.last_first_line(),
            _ => {}
        }
        ValueAnswer::Moved
    }

    /// The body's vertical extent, in pixels, for its scrollbar.
    pub fn extent(&self) -> Extent {
        Extent {
            viewport: LINES_IN_VIEW as f32 * LINE_HEIGHT,
            content: self.line_count() as f32 * LINE_HEIGHT,
            offset: self.first_line as f32 * LINE_HEIGHT,
        }
    }

    /// The accessible name: `<column> value`.
    pub fn name(&self) -> String {
        format!("{} value", self.column)
    }
}

fn bytes_label(bytes: usize) -> String {
    match bytes {
        1 => "1 byte".to_owned(),
        n => format!("{} bytes", status_line::thousands(n)),
    }
}

pub(crate) fn draw(paint: &mut Painter<'_, '_>, view: &ValueView, rect: Rect) {
    let theme = paint.theme;
    let faces = paint.faces;
    for (spread, alpha) in [(8.0, 10u8), (4.0, 14), (0.0, 16)] {
        let shadow = Rect::new(
            rect.x - spread / 2.0,
            rect.y + 4.0 - spread / 2.0,
            rect.w + spread,
            rect.h + spread,
        );
        let color = crate::color::Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: alpha,
        };
        paint.canvas.fill_rounded_rect(shadow, 6.0 + spread / 2.0, color);
    }
    paint.canvas.fill_rounded_rect(rect, 6.0, theme.border);
    let inner = Rect::new(rect.x + 1.0, rect.y + 1.0, rect.w - 2.0, rect.h - 2.0);
    paint.canvas.fill_rounded_rect(inner, 5.0, theme.bg0);
    paint.canvas.fill_rounded_rect(
        Rect::new(rect.x + 1.0, rect.y + 1.0, rect.w - 2.0, HEADER_HEIGHT - 1.0),
        5.0,
        theme.bg1,
    );
    paint.rect(rect.x + 1.0, rect.y + 16.0, rect.w - 2.0, 15.0, theme.bg1);
    paint.rect(rect.x, rect.y + HEADER_HEIGHT - 1.0, rect.w, 1.0, theme.border);
    let detail = format!(" · {}", view.detail);
    let detail_width = faces.ui.width(&detail, BODY.0);
    let room = (NAME_RIGHT - TEXT_LEFT - detail_width).max(40.0);
    let name = elided(faces.ui_bold, &view.column, room, BODY.0);
    let pen = paint.text(
        faces.ui_bold,
        &name,
        rect.x + TEXT_LEFT,
        rect.y + 7.0,
        BODY,
        theme.text_primary,
    );
    let detail = elided(faces.ui, &detail, rect.x + NAME_RIGHT - pen, BODY.0);
    paint.text(faces.ui, &detail, pen, rect.y + 7.0, BODY, theme.text_secondary);
    let copy = copy_rect(rect);
    let label = if view.copyable() {
        theme.text_primary
    } else {
        theme.text_disabled
    };
    paint.text(faces.ui, "Copy", copy.x + 12.0, rect.y + 7.0, BODY, label);
    if view.copy_focused {
        focus_ring(paint, copy);
    }
    let body_color = if view.placeholder {
        theme.text_disabled
    } else {
        theme.text_primary
    };
    let shown = view
        .lines
        .iter()
        .map(|line| (line.as_str(), body_color))
        .chain(view.cut.as_deref().map(|cut| (cut, theme.text_secondary)))
        .enumerate()
        .skip(view.first_line)
        .take(LINES_IN_VIEW);
    for (index, (line, color)) in shown {
        let y = rect.y + BODY_TEXT_TOP + LINE_HEIGHT * (index - view.first_line) as f32;
        paint.text(faces.mono, line, rect.x + TEXT_LEFT, y, CODE, color);
    }
    let track = scrollbar::track_rect(
        Axis::Vertical,
        rect.y + HEADER_HEIGHT,
        HEIGHT - HEADER_HEIGHT,
        rect.x + rect.w,
        rect.y + rect.h,
    );
    scrollbar::draw(paint, Axis::Vertical, track, view.extent(), false);
}

pub(crate) fn nodes(nodes: &mut Vec<AccessNode>, view: &ValueView, rect: Rect, parent: u32) {
    let bounds = |rect: Rect| AccessRect {
        x: rect.x,
        y: rect.y,
        width: rect.w,
        height: rect.h,
    };
    nodes.push(AccessNode {
        id: NODE_VALUE_VIEW,
        parent: Some(parent),
        role: AccessRole::Group,
        name: view.name(),
        description: Some(view.detail.clone()),
        bounds: bounds(rect),
        ..AccessNode::default()
    });
    let mut body = AccessNode {
        id: NODE_VALUE_BODY,
        parent: Some(NODE_VALUE_VIEW),
        role: AccessRole::TextArea,
        name: "Value".to_owned(),
        value: Some(view.text.clone()),
        bounds: bounds(Rect::new(
            rect.x,
            rect.y + HEADER_HEIGHT,
            rect.w,
            rect.h - HEADER_HEIGHT,
        )),
        ..AccessNode::default()
    };
    body.state.read_only = true;
    body.state.focused = !view.copy_focused;
    body.actions.focus = true;
    nodes.push(body);
    let mut copy = AccessNode {
        id: NODE_VALUE_COPY,
        parent: Some(NODE_VALUE_VIEW),
        role: AccessRole::Button,
        name: "Copy value".to_owned(),
        bounds: bounds(copy_rect(rect)),
        ..AccessNode::default()
    };
    copy.state.focused = view.copy_focused;
    copy.state.disabled = !view.copyable();
    copy.actions.invoke = true;
    nodes.push(copy);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> ResultGrid {
        ResultGrid::new(
            vec!["name".to_owned(), "doc".to_owned(), "data".to_owned()],
            vec![ValueKind::Text, ValueKind::Json, ValueKind::Blob],
            vec![
                vec![
                    Some("first line\nsecond".to_owned()),
                    Some("{\"a\":1,\"b\":[1,2],\"c\":{}}".to_owned()),
                    Some("\\xdeadbeef".to_owned()),
                ],
                vec![Some(String::new()), None, None],
            ],
            2,
            vec![100.0, 100.0, 100.0],
        )
    }

    fn press(key: Key) -> KeyPress {
        KeyPress::plain(key)
    }

    #[test]
    fn json_is_pretty_printed_in_stored_order_and_a_blob_shows_its_full_hex() {
        let grid = grid();
        let doc = ValueView::open(&grid, Cell { row: 0, column: 1 });
        assert_eq!(
            doc.text,
            "{\n  \"a\": 1,\n  \"b\": [\n    1,\n    2\n  ],\n  \"c\": {}\n}"
        );
        assert_eq!(doc.detail, "json");
        let data = ValueView::open(&grid, Cell { row: 0, column: 2 });
        assert_eq!(data.text, "0xdeadbeef");
        assert_eq!(data.detail, "binary · 4 bytes");
        assert_eq!(
            data.copy(),
            Some(ValueAnswer::Copy {
                text: "0xdeadbeef".to_owned(),
                toast: "Copied 4 bytes as hex".to_owned(),
            })
        );
        let text = ValueView::open(&grid, Cell { row: 0, column: 0 });
        assert_eq!(text.lines, vec!["first line", "second"]);
    }

    #[test]
    fn null_and_the_empty_string_show_dimmed_and_cannot_be_copied() {
        let grid = grid();
        let empty = ValueView::open(&grid, Cell { row: 1, column: 0 });
        assert_eq!((empty.text.as_str(), empty.placeholder), ("\"\"", true));
        assert_eq!(empty.copy(), None);
        let mut null = ValueView::open(&grid, Cell { row: 1, column: 1 });
        assert_eq!((null.text.as_str(), null.placeholder), ("NULL", true));
        assert_eq!(null.key(press(Key::Tab)), ValueAnswer::Moved);
        assert!(!null.copy_focused);
    }

    #[test]
    fn long_lines_wrap_per_character_and_the_body_stops_at_its_limit() {
        let long = "x".repeat(130);
        let (lines, left_out) = wrapped_lines(&long, GLYPHS_PER_LINE, BODY_LIMIT);
        assert_eq!(
            lines.iter().map(|line| line.len()).collect::<Vec<_>>(),
            vec![58, 58, 14]
        );
        assert_eq!(left_out, 0);
        let (lines, left_out) = wrapped_lines("ab\r\n\ncdé", 2, 7);
        assert_eq!(lines, vec!["ab", "", "cd"]);
        assert_eq!(left_out, 2);
    }

    #[test]
    fn keys_scroll_the_body_move_to_copy_and_close() {
        let rows = vec![vec![Some(
            (1..=30)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )]];
        let grid = ResultGrid::new(vec!["n".to_owned()], vec![ValueKind::Text], rows, 1, vec![60.0]);
        let mut view = ValueView::open(&grid, Cell::default());
        view.key(press(Key::Down));
        assert_eq!(view.first_line, 1);
        view.key(press(Key::PageDown));
        assert_eq!(view.first_line, 10);
        view.key(press(Key::End));
        assert_eq!(view.first_line, 20);
        view.key(press(Key::PageDown));
        assert_eq!(view.first_line, 20);
        view.key(press(Key::Home));
        assert_eq!(view.first_line, 0);
        view.scroll_by(6);
        assert_eq!(view.first_line, 6);
        view.key(press(Key::Tab));
        assert!(view.copy_focused);
        let ValueAnswer::Copy { toast, .. } = view.key(press(Key::Enter)) else {
            panic!("Enter on Copy copies");
        };
        assert_eq!(toast, "Copied 80 characters");
        view.key(press(Key::Tab));
        assert_eq!(view.key(press(Key::Enter)), ValueAnswer::Close);
        assert_eq!(view.key(press(Key::Escape)), ValueAnswer::Close);
        assert_eq!(view.key(press(Key::F2)), ValueAnswer::Close);
    }

    #[test]
    fn the_view_opens_below_its_cell_or_above_it_near_the_bottom() {
        assert_eq!(
            popover_rect(Rect::new(308.0, 446.0, 100.0, 24.0), 1280.0),
            Rect::new(308.0, 474.0, 480.0, 240.0)
        );
        assert_eq!(
            popover_rect(Rect::new(1000.0, 600.0, 100.0, 24.0), 1280.0),
            Rect::new(800.0, 356.0, 480.0, 240.0)
        );
        assert_eq!(
            popover_rect(Rect::new(200.0, 446.0, 100.0, 24.0), 1280.0).x,
            260.0
        );
    }
}
