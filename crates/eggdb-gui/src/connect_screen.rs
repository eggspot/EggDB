use eggdb_platform::window::{Key, KeyPress};

use crate::canvas::{Canvas, Rect};
use crate::color::{Rgba, Theme};
use crate::font::{TextStyle, Typeface};
use crate::main_window::Faces;

mod accessibility;

pub use accessibility::{
    NODE_BROWSE, NODE_CANCEL, NODE_CARD, NODE_CONNECT_TO_A_SERVER, NODE_CREATE_IF_MISSING,
    NODE_ERROR, NODE_FILE, NODE_OPEN, NODE_OPEN_A_FILE, NODE_PANEL, NODE_READ_ONLY, NODE_SAMPLE,
    NODE_SEGMENTS, NODE_TITLE, NODE_WHERE_FROM, NODE_WINDOW, accessibility_tree,
    error_announcement,
};

/// The window's default width in logical pixels.
pub const DEFAULT_WIDTH: u32 = 1280;
/// The window's default height in logical pixels.
pub const DEFAULT_HEIGHT: u32 = 800;
/// The narrowest the window may be, in logical pixels.
pub const MIN_WIDTH: u32 = 800;
/// The shortest the window may be: the card stack is never clipped.
pub const MIN_HEIGHT: u32 = 560;
/// The window's title.
pub const TITLE: &str = "EggDB";
/// The file the `Open a file` segment proposes at first start.
pub const QUICKSTART_FILE: &str = "~/eggdb/quickstart.eggdb";

const STACK_WIDTH: f32 = 560.0;
const STACK_HEIGHT: f32 = 520.0;
const CARD: Rect = Rect::new(0.0, 0.0, 560.0, 480.0);
const SEGMENTS: Rect = Rect::new(24.0, 64.0, 512.0, 32.0);
const OPEN_A_FILE: Rect = Rect::new(24.0, 64.0, 256.0, 32.0);
const CONNECT_TO_A_SERVER: Rect = Rect::new(280.0, 64.0, 256.0, 32.0);
const SEGMENT_LABEL_TOP: f32 = 71.0;
const ERROR_TOP: f32 = 100.0;
const FILE_LABEL: (f32, f32) = (24.0, 120.0);
const FILE_INPUT: Rect = Rect::new(24.0, 142.0, 420.0, 28.0);
const INPUT_TEXT_INSET: f32 = 8.0;
const INPUT_TEXT_TOP: f32 = 5.0;
const CARET_TOP: f32 = 4.0;
const BROWSE: Rect = Rect::new(452.0, 142.0, 84.0, 28.0);
const CREATE_BOX: Rect = Rect::new(24.0, 186.0, 16.0, 16.0);
const READ_ONLY_BOX: Rect = Rect::new(24.0, 210.0, 16.0, 16.0);
const CHECK_LABEL_X: f32 = 48.0;
const CANCEL: Rect = Rect::new(364.0, 436.0, 84.0, 28.0);
const OPEN: Rect = Rect::new(456.0, 436.0, 80.0, 28.0);
const LINKS_TOP: f32 = 499.0;
const LINK_GAP: f32 = 24.0;
const LINK_FOCUS_WIDEN: f32 = 4.0;
const RADIUS_CONTROL: f32 = 4.0;
const RADIUS_CARD: f32 = 8.0;
const FOCUS_GAP: f32 = 2.0;
const HEADLINE: (f32, f32) = (20.0, 26.0);
const BODY: (f32, f32) = (13.0, 18.0);

const OPEN_A_FILE_LABEL: &str = "Open a file";
const CONNECT_TO_A_SERVER_LABEL: &str = "Connect to a server";
const CREATE_LABEL: &str = "Create the file if it does not exist";
const READ_ONLY_LABEL: &str = "Open read-only";
const PLACEHOLDER: &str = "/path/to/shop.eggdb";
const WHERE_FROM_LABEL: &str = "Where are you coming from?";
const SAMPLE_LABEL: &str = "Load a sample dataset";

/// The element of the connect card with the keyboard focus, in `docs/ux/first-screen.md`'s
/// order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The segmented control (one stop: the active segment).
    Segments,
    /// The File input.
    FileInput,
    /// `Browse…`.
    Browse,
    /// The checkbox `Create the file if it does not exist`.
    CreateIfMissing,
    /// The checkbox `Open read-only`.
    ReadOnly,
    /// `Cancel`, only while it is enabled.
    Cancel,
    /// `Open`, the default button.
    Open,
    /// The link `Where are you coming from?`.
    WhereFromLink,
    /// The link `Load a sample dataset`.
    SampleLink,
}

/// What a key asks the application to do beyond changing the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectAction {
    /// Nothing beyond the card's own change.
    None,
    /// Open (or create) the file in the input.
    Open,
    /// Choose the file with the OS's file dialog.
    Browse,
    /// Go back to where the user came from.
    Cancel,
    /// Show the layout picker behind `Where are you coming from?`.
    ShowLayoutPicker,
    /// Load the `shop` sample dataset.
    LoadSampleDataset,
    /// Quit the client.
    Quit,
}

/// A single-line text input: its value, caret and selection, counted in characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextField {
    value: String,
    caret: usize,
    anchor: Option<usize>,
}

impl TextField {
    /// A field holding `value` with all of it selected, so typing replaces it.
    pub fn selected(value: &str) -> Self {
        let length = value.chars().count();
        Self {
            value: value.to_owned(),
            caret: length,
            anchor: Some(0),
        }
    }

    /// The text.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// The caret, in characters from the start.
    pub fn caret(&self) -> usize {
        self.caret
    }

    /// The selected characters, start before end, when there are any.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        (anchor != self.caret).then(|| (anchor.min(self.caret), anchor.max(self.caret)))
    }

    fn length(&self) -> usize {
        self.value.chars().count()
    }

    fn byte_at(&self, column: usize) -> usize {
        self.value
            .char_indices()
            .nth(column)
            .map_or(self.value.len(), |(at, _)| at)
    }

    fn prefix(&self, column: usize) -> &str {
        self.value.get(..self.byte_at(column)).unwrap_or_default()
    }

    fn slice(&self, start: usize, end: usize) -> &str {
        self.value
            .get(self.byte_at(start)..self.byte_at(end))
            .unwrap_or_default()
    }

    fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            self.anchor = None;
            return false;
        };
        let (from, to) = (self.byte_at(start), self.byte_at(end));
        self.value.replace_range(from..to, "");
        self.caret = start;
        self.anchor = None;
        true
    }

    fn insert(&mut self, character: char) {
        self.delete_selection();
        let at = self.byte_at(self.caret);
        self.value.insert(at, character);
        self.caret += 1;
    }

    fn move_to(&mut self, column: usize, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.caret);
        } else {
            self.anchor = None;
        }
        self.caret = column.min(self.length());
    }

    fn clear_selection(&mut self) {
        self.anchor = None;
    }

    fn select_all(&mut self) {
        self.anchor = Some(0);
        self.caret = self.length();
    }

    fn apply(&mut self, press: KeyPress) {
        let modifiers = press.modifiers;
        let primary = modifiers.control || modifiers.command;
        match press.key {
            Key::Character('a' | 'A') if primary => self.select_all(),
            Key::Character(character) if !primary => self.insert(character),
            Key::Space if !primary => self.insert(' '),
            Key::Backspace => {
                if !self.delete_selection() && self.caret > 0 {
                    self.anchor = Some(self.caret - 1);
                    self.delete_selection();
                }
            }
            Key::Delete => {
                if !self.delete_selection() && self.caret < self.length() {
                    self.anchor = Some(self.caret + 1);
                    self.delete_selection();
                }
            }
            Key::Left => {
                let to = match (self.selection(), modifiers.shift) {
                    (Some((start, _)), false) => start,
                    _ => self.caret.saturating_sub(1),
                };
                self.move_to(to, modifiers.shift);
            }
            Key::Right => {
                let to = match (self.selection(), modifiers.shift) {
                    (Some((_, end)), false) => end,
                    _ => self.caret + 1,
                };
                self.move_to(to, modifiers.shift);
            }
            Key::Home | Key::Up => self.move_to(0, modifiers.shift),
            Key::End | Key::Down => self.move_to(self.length(), modifiers.shift),
            _ => {}
        }
    }
}

/// An `Error:` line under the segments, with its `Try:` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardError {
    /// The engine's message.
    pub message: String,
    /// What to try instead.
    pub fix: String,
}

/// The connect card's state (`docs/ux/first-screen.md`): the `Open a file` segment's fields
/// and which element has the keyboard focus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectCard {
    /// The File input.
    pub file: TextField,
    /// `Create the file if it does not exist`.
    pub create_if_missing: bool,
    /// `Open read-only`.
    pub read_only: bool,
    /// Whether `Cancel` is enabled (there is somewhere to return to).
    pub can_cancel: bool,
    /// The `Error:` line, when a failure is shown.
    pub error: Option<CardError>,
    /// The element with the keyboard focus.
    pub focus: Focus,
}

impl ConnectCard {
    /// The card at first start: the File input focused with the quickstart file selected,
    /// `Create the file if it does not exist` checked and `Cancel` disabled.
    pub fn first_start() -> Self {
        Self {
            file: TextField::selected(QUICKSTART_FILE),
            create_if_missing: true,
            read_only: false,
            can_cancel: false,
            error: None,
            focus: Focus::FileInput,
        }
    }

    /// The `Tab` order: `Cancel` only while it is enabled.
    pub fn focus_order(&self) -> Vec<Focus> {
        let mut order = vec![
            Focus::Segments,
            Focus::FileInput,
            Focus::Browse,
            Focus::CreateIfMissing,
            Focus::ReadOnly,
        ];
        if self.can_cancel {
            order.push(Focus::Cancel);
        }
        order.extend([Focus::Open, Focus::WhereFromLink, Focus::SampleLink]);
        order
    }

    fn step_focus(&mut self, backwards: bool) {
        let order = self.focus_order();
        let at = order
            .iter()
            .position(|&focus| focus == self.focus)
            .unwrap_or(0);
        let next = if backwards {
            (at + order.len() - 1) % order.len()
        } else {
            (at + 1) % order.len()
        };
        self.focus = order.get(next).copied().unwrap_or(Focus::FileInput);
    }

    fn activate(&self) -> ConnectAction {
        match self.focus {
            Focus::Browse => ConnectAction::Browse,
            Focus::Cancel if self.can_cancel => ConnectAction::Cancel,
            Focus::WhereFromLink => ConnectAction::ShowLayoutPicker,
            Focus::SampleLink => ConnectAction::LoadSampleDataset,
            Focus::Segments
            | Focus::FileInput
            | Focus::CreateIfMissing
            | Focus::ReadOnly
            | Focus::Cancel
            | Focus::Open => ConnectAction::Open,
        }
    }
}

/// Answers a key press: the card after it and what the application should do.
pub fn handle_key(mut card: ConnectCard, press: KeyPress) -> (ConnectCard, ConnectAction) {
    let modifiers = press.modifiers;
    let primary = modifiers.control || modifiers.command;
    let action = match press.key {
        Key::Character('q' | 'Q') if primary => ConnectAction::Quit,
        Key::Character('w' | 'W') if modifiers.command => ConnectAction::Quit,
        Key::F4 if modifiers.alt => ConnectAction::Quit,
        Key::Character('o' | 'O') if primary => {
            card.focus = Focus::FileInput;
            ConnectAction::None
        }
        Key::Tab => {
            card.step_focus(modifiers.shift);
            ConnectAction::None
        }
        Key::Enter => card.activate(),
        Key::Escape => {
            if card.can_cancel {
                ConnectAction::Cancel
            } else {
                card.error = None;
                card.file.clear_selection();
                ConnectAction::None
            }
        }
        Key::Space => match card.focus {
            Focus::CreateIfMissing => {
                card.create_if_missing = !card.create_if_missing;
                ConnectAction::None
            }
            Focus::ReadOnly => {
                card.read_only = !card.read_only;
                ConnectAction::None
            }
            Focus::FileInput => {
                card.file.apply(press);
                ConnectAction::None
            }
            Focus::Segments => ConnectAction::None,
            Focus::Browse
            | Focus::Cancel
            | Focus::Open
            | Focus::WhereFromLink
            | Focus::SampleLink => card.activate(),
        },
        _ if card.focus == Focus::FileInput => {
            card.file.apply(press);
            ConnectAction::None
        }
        _ => ConnectAction::None,
    };
    (card, action)
}

/// The card's top-left in logical pixels for a `width` × `height` window: the card stack
/// centred, rounded to whole pixels.
pub fn card_origin(width: u32, height: u32) -> (f32, f32) {
    (
        ((width as f32 - STACK_WIDTH) / 2.0).round(),
        ((height as f32 - STACK_HEIGHT) / 2.0).round(),
    )
}

/// The framebuffer size in device pixels for a window of `size` logical pixels at `scale`.
pub fn frame_size(size: (u32, u32), scale: f32) -> (usize, usize) {
    let device = |logical: u32| (logical as f32 * scale).round().max(1.0) as usize;
    (device(size.0), device(size.1))
}

fn offset(rect: Rect, by: (f32, f32)) -> Rect {
    Rect::new(rect.x + by.0, rect.y + by.1, rect.w, rect.h)
}

fn down(rect: Rect, by: f32) -> Rect {
    Rect::new(rect.x, rect.y + by, rect.w, rect.h)
}

fn grow(rect: Rect, by: f32) -> Rect {
    Rect::new(
        rect.x - by,
        rect.y - by,
        rect.w + 2.0 * by,
        rect.h + 2.0 * by,
    )
}

fn panel_shift(card: &ConnectCard) -> f32 {
    if card.error.is_some() { 36.0 } else { 0.0 }
}

fn link_rects(faces: Faces<'_>, origin: (f32, f32)) -> [(Rect, &'static str); 2] {
    let first_width = faces.ui.width(WHERE_FROM_LABEL, BODY.0);
    let second_width = faces.ui.width(SAMPLE_LABEL, BODY.0);
    let first = Rect::new(24.0, LINKS_TOP, first_width, BODY.1);
    let second = Rect::new(
        24.0 + first_width + LINK_GAP,
        LINKS_TOP,
        second_width,
        BODY.1,
    );
    [
        (offset(first, origin), WHERE_FROM_LABEL),
        (offset(second, origin), SAMPLE_LABEL),
    ]
}

/// The rect, in logical pixels, of the element the focus ring surrounds for a window of `size`,
/// with its corner radius; the ring itself is drawn 2 px outside it.
pub fn focus_target(card: &ConnectCard, faces: Faces<'_>, size: (u32, u32)) -> (Rect, f32) {
    let origin = card_origin(size.0, size.1);
    let shift = panel_shift(card);
    let [where_from, sample] = link_rects(faces, origin);
    let link = |rect: Rect| {
        Rect::new(
            rect.x - LINK_FOCUS_WIDEN,
            rect.y,
            rect.w + 2.0 * LINK_FOCUS_WIDEN,
            rect.h,
        )
    };
    let rect = match card.focus {
        Focus::Segments => offset(OPEN_A_FILE, origin),
        Focus::FileInput => offset(down(FILE_INPUT, shift), origin),
        Focus::Browse => offset(down(BROWSE, shift), origin),
        Focus::CreateIfMissing => offset(down(CREATE_BOX, shift), origin),
        Focus::ReadOnly => offset(down(READ_ONLY_BOX, shift), origin),
        Focus::Cancel => offset(CANCEL, origin),
        Focus::Open => offset(OPEN, origin),
        Focus::WhereFromLink => link(where_from.0),
        Focus::SampleLink => link(sample.0),
    };
    (rect, RADIUS_CONTROL)
}

struct Pen<'c, 'f> {
    canvas: &'c mut Canvas,
    theme: Theme,
    faces: Faces<'f>,
    scale: f32,
}

impl Pen<'_, '_> {
    fn device(&self, rect: Rect) -> Rect {
        let s = self.scale;
        let (x0, y0) = ((rect.x * s).round(), (rect.y * s).round());
        let (x1, y1) = (
            ((rect.x + rect.w) * s).round(),
            ((rect.y + rect.h) * s).round(),
        );
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    fn line_width(&self) -> f32 {
        self.scale.round().max(1.0)
    }

    fn fill(&mut self, rect: Rect, color: Rgba) {
        let rect = self.device(rect);
        self.canvas.fill_rect(rect, color);
    }

    fn rounded(&mut self, rect: Rect, radius: f32, color: Rgba) {
        let rect = self.device(rect);
        self.canvas
            .fill_rounded_rect(rect, radius * self.scale, color);
    }

    fn outlined(&mut self, rect: Rect, radius: f32, fill: Rgba, border: Rgba) {
        let outer = self.device(rect);
        let line = self.line_width();
        let radius = radius * self.scale;
        self.canvas.fill_rounded_rect(outer, radius, border);
        let inner = Rect::new(
            outer.x + line,
            outer.y + line,
            outer.w - 2.0 * line,
            outer.h - 2.0 * line,
        );
        self.canvas
            .fill_rounded_rect(inner, (radius - line).max(0.0), fill);
    }

    fn text(
        &mut self,
        face: &dyn Typeface,
        text: &str,
        at: (f32, f32),
        metrics: (f32, f32),
        color: Rgba,
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

    fn width(&self, face: &dyn Typeface, text: &str, metrics: (f32, f32)) -> f32 {
        face.width(text, metrics.0 * self.scale) / self.scale
    }

    fn centred_label(&mut self, face: &dyn Typeface, text: &str, rect: Rect, color: Rgba) {
        let width = self.width(face, text, BODY);
        let left = rect.x + (rect.w - width) / 2.0;
        let top = rect.y + (rect.h - BODY.1) / 2.0;
        self.text(face, text, (left, top), BODY, color);
    }

    fn baseline(&self, face: &dyn Typeface, top: f32, metrics: (f32, f32)) -> f32 {
        let s = self.scale;
        let (ascent, descent) = face.extent(metrics.0 * s);
        ((top * s).round() + (metrics.1 * s - ascent - descent) / 2.0 + ascent).round()
    }

    fn focus_ring(&mut self, rect: Rect, radius: f32) {
        let s = self.scale;
        let inner = grow(self.device(rect), (FOCUS_GAP * s).round());
        let width = (self.theme.focus_ring_width() * s).round();
        self.canvas.stroke_outside_rounded_rect(
            inner,
            (radius + FOCUS_GAP) * s,
            width,
            self.theme.focus_ring,
        );
    }
}

/// Draws the connect card for a window of `size` logical pixels at `scale` device pixels per
/// logical pixel, in `theme`: a pure function of its arguments, the frame
/// `docs/ux/first-screen.md` specifies.
pub fn draw(
    card: &ConnectCard,
    theme: Theme,
    faces: Faces<'_>,
    size: (u32, u32),
    scale: f32,
) -> Canvas {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let (width, height) = frame_size(size, scale);
    let mut canvas = Canvas::new(width, height, theme.bg0);
    let mut pen = Pen {
        canvas: &mut canvas,
        theme,
        faces,
        scale,
    };
    let origin = card_origin(size.0, size.1);
    pen.outlined(offset(CARD, origin), RADIUS_CARD, theme.bg0, theme.border);
    heading(&mut pen, origin);
    segments(&mut pen, origin);
    error_lines(&mut pen, card, origin);
    file_row(&mut pen, card, origin);
    checkboxes(&mut pen, card, origin);
    buttons(&mut pen, card, origin);
    links(&mut pen, origin);
    let (rect, radius) = focus_target(card, faces, size);
    pen.focus_ring(rect, radius);
    canvas
}

fn heading(pen: &mut Pen<'_, '_>, origin: (f32, f32)) {
    let theme = pen.theme;
    let s = pen.scale;
    let (cx, cy) = ((origin.0 + 32.0) * s, (origin.1 + 37.0) * s);
    pen.canvas
        .stroke_circle(cx, cy, 16.0 * s, 2.0 * s, theme.accent);
    pen.canvas.fill_circle(cx, cy, 6.0 * s, theme.brand_yolk);
    let bold = pen.faces.ui_bold;
    pen.text(
        bold,
        "Connect",
        (origin.0 + 48.0, origin.1 + 24.0),
        HEADLINE,
        theme.text_primary,
    );
}

fn segments(pen: &mut Pen<'_, '_>, origin: (f32, f32)) {
    let theme = pen.theme;
    let outline = offset(SEGMENTS, origin);
    pen.outlined(outline, RADIUS_CONTROL, theme.bg0, theme.border_control);
    let active = pen.device(offset(OPEN_A_FILE, origin));
    let line = pen.line_width();
    let fill = Rect::new(
        active.x + line,
        active.y + line,
        active.w - line,
        active.h - 2.0 * line,
    );
    let radius = (RADIUS_CONTROL * pen.scale - line).max(0.0);
    pen.canvas.fill_rounded_rect(fill, radius, theme.bg2);
    let square = Rect::new(fill.x + fill.w / 2.0, fill.y, fill.w / 2.0, fill.h);
    pen.canvas.fill_rect(square, theme.bg2);
    let divider = pen.device(offset(CONNECT_TO_A_SERVER, origin));
    pen.canvas.fill_rect(
        Rect::new(divider.x, divider.y, line, divider.h),
        theme.border_control,
    );
    let (bold, regular) = (pen.faces.ui_bold, pen.faces.ui);
    for (face, label, rect, color) in [
        (bold, OPEN_A_FILE_LABEL, OPEN_A_FILE, theme.text_primary),
        (
            regular,
            CONNECT_TO_A_SERVER_LABEL,
            CONNECT_TO_A_SERVER,
            theme.text_secondary,
        ),
    ] {
        let width = pen.width(face, label, BODY);
        let left = origin.0 + rect.x + (rect.w - width) / 2.0;
        pen.text(
            face,
            label,
            (left, origin.1 + SEGMENT_LABEL_TOP),
            BODY,
            color,
        );
    }
}

fn error_lines(pen: &mut Pen<'_, '_>, card: &ConnectCard, origin: (f32, f32)) {
    let Some(error) = &card.error else {
        return;
    };
    let theme = pen.theme;
    let face = pen.faces.ui;
    for (index, line) in [
        format!("Error: {}", error.message),
        format!("Try: {}", error.fix),
    ]
    .iter()
    .enumerate()
    {
        let top = origin.1 + ERROR_TOP + BODY.1 * index as f32;
        pen.text(face, line, (origin.0 + 24.0, top), BODY, theme.error);
    }
}

fn file_row(pen: &mut Pen<'_, '_>, card: &ConnectCard, origin: (f32, f32)) {
    let theme = pen.theme;
    let face = pen.faces.ui;
    let shift = panel_shift(card);
    pen.text(
        face,
        "File",
        (origin.0 + FILE_LABEL.0, origin.1 + FILE_LABEL.1 + shift),
        BODY,
        theme.text_primary,
    );
    let focused = card.focus == Focus::FileInput;
    let input = offset(down(FILE_INPUT, shift), origin);
    let border = if focused {
        theme.accent
    } else {
        theme.border_control
    };
    pen.outlined(input, RADIUS_CONTROL, theme.bg0, border);
    let text_left = input.x + INPUT_TEXT_INSET;
    let text_top = input.y + INPUT_TEXT_TOP;
    let field = &card.file;
    if let Some((start, end)) = field.selection() {
        let before = pen.width(face, field.prefix(start), BODY);
        let selected = pen.width(face, field.slice(start, end), BODY);
        let color = if focused {
            theme.selection
        } else {
            theme.selection_inactive
        };
        pen.fill(
            Rect::new(text_left + before, text_top, selected, BODY.1),
            color,
        );
    }
    if field.value().is_empty() {
        pen.text(
            face,
            PLACEHOLDER,
            (text_left, text_top),
            BODY,
            theme.text_secondary,
        );
    } else {
        pen.text(
            face,
            field.value(),
            (text_left, text_top),
            BODY,
            theme.text_primary,
        );
    }
    if focused {
        let caret_x = text_left + pen.width(face, field.prefix(field.caret()), BODY);
        pen.fill(
            Rect::new(caret_x, input.y + CARET_TOP, 2.0, 20.0),
            theme.accent,
        );
    }
    let browse = offset(down(BROWSE, shift), origin);
    pen.outlined(browse, RADIUS_CONTROL, theme.bg0, theme.border);
    pen.centred_label(face, "Browse…", browse, theme.text_primary);
}

fn checkboxes(pen: &mut Pen<'_, '_>, card: &ConnectCard, origin: (f32, f32)) {
    let theme = pen.theme;
    let face = pen.faces.ui;
    let shift = panel_shift(card);
    for (rect, label, checked) in [
        (CREATE_BOX, CREATE_LABEL, card.create_if_missing),
        (READ_ONLY_BOX, READ_ONLY_LABEL, card.read_only),
    ] {
        let rect = offset(down(rect, shift), origin);
        if checked {
            pen.rounded(rect, RADIUS_CONTROL, theme.accent);
            let s = pen.scale;
            let point = |x: f32, y: f32| ((rect.x + x) * s, (rect.y + y) * s);
            let width = 1.5 * s;
            pen.canvas
                .line(point(4.0, 8.0), point(7.0, 11.0), width, theme.on_accent);
            pen.canvas
                .line(point(7.0, 11.0), point(12.0, 5.0), width, theme.on_accent);
        } else {
            pen.outlined(rect, RADIUS_CONTROL, theme.bg0, theme.border_control);
        }
        pen.text(
            face,
            label,
            (origin.0 + CHECK_LABEL_X, rect.y - 1.0),
            BODY,
            theme.text_primary,
        );
    }
}

fn buttons(pen: &mut Pen<'_, '_>, card: &ConnectCard, origin: (f32, f32)) {
    let theme = pen.theme;
    let face = pen.faces.ui;
    let cancel = offset(CANCEL, origin);
    if card.can_cancel {
        pen.outlined(cancel, RADIUS_CONTROL, theme.bg0, theme.border);
        pen.centred_label(face, "Cancel", cancel, theme.text_primary);
    } else {
        pen.outlined(cancel, RADIUS_CONTROL, theme.bg1, theme.border);
        pen.centred_label(face, "Cancel", cancel, theme.text_disabled);
    }
    let open = offset(OPEN, origin);
    pen.rounded(open, RADIUS_CONTROL, theme.accent);
    pen.centred_label(face, "Open", open, theme.on_accent);
}

fn links(pen: &mut Pen<'_, '_>, origin: (f32, f32)) {
    let theme = pen.theme;
    let face = pen.faces.ui;
    let line = pen.line_width();
    for (rect, label) in link_rects(pen.faces, origin) {
        pen.text(face, label, (rect.x, rect.y), BODY, theme.accent);
        let baseline = pen.baseline(face, rect.y, BODY);
        let device = pen.device(rect);
        let underline_top = baseline + (2.0 * pen.scale).round();
        pen.canvas.fill_rect(
            Rect::new(device.x, underline_top, device.w, line),
            theme.accent,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ThemeKind;
    use crate::font::BoxFace;
    use eggdb_platform::window::Modifiers;

    const BOXES: Faces<'static> = Faces {
        ui: &BoxFace,
        ui_bold: &BoxFace,
        mono: &BoxFace,
    };
    const SIZE: (u32, u32) = (DEFAULT_WIDTH, DEFAULT_HEIGHT);

    fn frame(card: &ConnectCard, kind: ThemeKind, scale: f32) -> (Canvas, Theme) {
        let theme = Theme::of(kind);
        (draw(card, theme, BOXES, SIZE, scale), theme)
    }

    fn press(key: Key) -> KeyPress {
        KeyPress::plain(key)
    }

    fn with(key: Key, modifiers: Modifiers) -> KeyPress {
        KeyPress { key, modifiers }
    }

    const SHIFT: Modifiers = Modifiers {
        shift: true,
        control: false,
        alt: false,
        command: false,
    };
    const CONTROL: Modifiers = Modifiers {
        shift: false,
        control: true,
        alt: false,
        command: false,
    };

    #[test]
    fn the_card_is_centred_with_its_fields_where_the_spec_puts_them_in_every_theme() {
        assert_eq!(card_origin(1280, 800), (360.0, 140.0));
        for kind in [ThemeKind::Light, ThemeKind::Dark, ThemeKind::HighContrast] {
            let (canvas, theme) = frame(&ConnectCard::first_start(), kind, 1.0);
            assert_eq!((canvas.width(), canvas.height()), (1280, 800));
            let at = |x: usize, y: usize| canvas.pixel(x, y).unwrap();
            assert_eq!(at(100, 100), theme.bg0, "{kind:?} window");
            assert_eq!(at(640, 140), theme.border, "{kind:?} card outline");
            assert_eq!(at(640, 400), theme.bg0, "{kind:?} card");
            assert_eq!(at(392, 177), theme.brand_yolk, "{kind:?} egg yolk");
            assert_eq!(
                at(460, 204),
                theme.border_control,
                "{kind:?} segment outline"
            );
            assert_eq!(at(400, 220), theme.bg2, "{kind:?} active segment");
            assert_eq!(at(660, 220), theme.bg0, "{kind:?} inactive segment");
            assert_eq!(at(640, 220), theme.border_control, "{kind:?} divider");
            assert_eq!(at(560, 282), theme.accent, "{kind:?} focused input border");
            assert_eq!(at(392, 289), theme.selection, "{kind:?} selection");
            assert_eq!(at(585, 300), theme.accent, "{kind:?} caret");
            assert_eq!(at(850, 282), theme.border, "{kind:?} browse border");
            assert_eq!(at(386, 339), theme.accent, "{kind:?} checked box");
            assert_eq!(at(392, 350), theme.border_control, "{kind:?} unchecked box");
            assert_eq!(at(392, 358), theme.bg0, "{kind:?} unchecked inside");
            assert_eq!(at(730, 590), theme.bg1, "{kind:?} disabled cancel");
            assert_eq!(at(820, 580), theme.accent, "{kind:?} open");
            assert_eq!(at(390, 654), theme.accent, "{kind:?} link underline");
            assert_eq!(
                at(381, 296),
                theme.focus_ring,
                "{kind:?} ring left of input"
            );
            assert_eq!(at(383, 296), theme.bg0, "{kind:?} gap inside the ring");
        }
    }

    #[test]
    fn the_high_contrast_ring_is_three_pixels_wide() {
        let (light, light_theme) = frame(&ConnectCard::first_start(), ThemeKind::Light, 1.0);
        let (high, high_theme) = frame(&ConnectCard::first_start(), ThemeKind::HighContrast, 1.0);
        assert_eq!(light.pixel(379, 296), Some(light_theme.bg0));
        assert_eq!(high.pixel(379, 296), Some(high_theme.focus_ring));
        assert_eq!(high.pixel(378, 296), Some(high_theme.bg0));
    }

    #[test]
    fn tab_and_shift_tab_visit_every_stop_in_order_and_wrap() {
        let forward = [
            Focus::Browse,
            Focus::CreateIfMissing,
            Focus::ReadOnly,
            Focus::Open,
            Focus::WhereFromLink,
            Focus::SampleLink,
            Focus::Segments,
            Focus::FileInput,
        ];
        let mut card = ConnectCard::first_start();
        for expected in forward {
            let (next, action) = handle_key(card, press(Key::Tab));
            assert_eq!((next.focus, action), (expected, ConnectAction::None));
            card = next;
        }
        let backward = [
            Focus::Segments,
            Focus::SampleLink,
            Focus::WhereFromLink,
            Focus::Open,
            Focus::ReadOnly,
            Focus::CreateIfMissing,
            Focus::Browse,
            Focus::FileInput,
        ];
        for expected in backward {
            card = handle_key(card, with(Key::Tab, SHIFT)).0;
            assert_eq!(card.focus, expected);
        }
        card.can_cancel = true;
        card.focus = Focus::ReadOnly;
        card = handle_key(card, press(Key::Tab)).0;
        assert_eq!(card.focus, Focus::Cancel);
    }

    #[test]
    fn the_ring_follows_the_focus() {
        let mut card = ConnectCard::first_start();
        card = handle_key(card, press(Key::Tab)).0;
        let (canvas, theme) = frame(&card, ThemeKind::Light, 1.0);
        assert_eq!(canvas.pixel(381, 296), Some(theme.bg0), "input ring gone");
        assert_eq!(
            canvas.pixel(560, 282),
            Some(theme.border_control),
            "input border"
        );
        assert_eq!(
            canvas.pixel(809, 296),
            Some(theme.focus_ring),
            "ring left of Browse"
        );
        assert_eq!(canvas.pixel(392, 289), Some(theme.selection_inactive));
        let card = ConnectCard {
            focus: Focus::Open,
            ..ConnectCard::first_start()
        };
        let (canvas, theme) = frame(&card, ThemeKind::Light, 1.0);
        assert_eq!(
            canvas.pixel(813, 590),
            Some(theme.focus_ring),
            "ring left of Open"
        );
        assert_eq!(
            canvas.pixel(860, 573),
            Some(theme.focus_ring),
            "ring above Open"
        );
        let card = ConnectCard {
            focus: Focus::CreateIfMissing,
            ..ConnectCard::first_start()
        };
        let (canvas, theme) = frame(&card, ThemeKind::Light, 1.0);
        assert_eq!(
            canvas.pixel(381, 334),
            Some(theme.focus_ring),
            "ring left of the box"
        );
        let card = ConnectCard {
            focus: Focus::WhereFromLink,
            ..ConnectCard::first_start()
        };
        let (canvas, theme) = frame(&card, ThemeKind::Light, 1.0);
        assert_eq!(
            canvas.pixel(377, 648),
            Some(theme.focus_ring),
            "ring left of the link"
        );
    }

    #[test]
    fn space_toggles_only_checkboxes_and_enter_opens_from_them() {
        let mut card = ConnectCard {
            focus: Focus::CreateIfMissing,
            ..ConnectCard::first_start()
        };
        card = handle_key(card, press(Key::Space)).0;
        assert!(!card.create_if_missing);
        card.focus = Focus::ReadOnly;
        card = handle_key(card, press(Key::Space)).0;
        assert!(card.read_only);
        card.focus = Focus::Segments;
        let (after, action) = handle_key(card.clone(), press(Key::Space));
        assert_eq!((after, action), (card.clone(), ConnectAction::None));
        card.focus = Focus::ReadOnly;
        assert_eq!(
            handle_key(card.clone(), press(Key::Enter)).1,
            ConnectAction::Open
        );
        card.focus = Focus::Browse;
        assert_eq!(
            handle_key(card.clone(), press(Key::Space)).1,
            ConnectAction::Browse
        );
        card.focus = Focus::SampleLink;
        assert_eq!(
            handle_key(card.clone(), press(Key::Enter)).1,
            ConnectAction::LoadSampleDataset
        );
        card.focus = Focus::WhereFromLink;
        assert_eq!(
            handle_key(card, press(Key::Enter)).1,
            ConnectAction::ShowLayoutPicker
        );
    }

    #[test]
    fn quit_escape_and_the_connect_key_do_what_the_spec_says() {
        let card = ConnectCard::first_start();
        let quit = [
            with(Key::Character('q'), CONTROL),
            with(
                Key::Character('q'),
                Modifiers {
                    command: true,
                    ..Modifiers::default()
                },
            ),
            with(
                Key::Character('w'),
                Modifiers {
                    command: true,
                    ..Modifiers::default()
                },
            ),
            with(
                Key::F4,
                Modifiers {
                    alt: true,
                    ..Modifiers::default()
                },
            ),
        ];
        for key in quit {
            assert_eq!(
                handle_key(card.clone(), key).1,
                ConnectAction::Quit,
                "{key:?}"
            );
        }
        assert_eq!(
            handle_key(card.clone(), with(Key::Character('w'), CONTROL)).1,
            ConnectAction::None
        );
        let mut shown = card.clone();
        shown.error = Some(CardError {
            message: "the file is locked".into(),
            fix: "close the other program".into(),
        });
        let (cleared, action) = handle_key(shown, press(Key::Escape));
        assert_eq!(action, ConnectAction::None);
        assert_eq!(cleared.error, None);
        assert_eq!(cleared.file.selection(), None);
        let mut cancellable = card.clone();
        cancellable.can_cancel = true;
        assert_eq!(
            handle_key(cancellable, press(Key::Escape)).1,
            ConnectAction::Cancel
        );
        let elsewhere = ConnectCard {
            focus: Focus::Open,
            ..card
        };
        let (focused, _) = handle_key(elsewhere, with(Key::Character('o'), CONTROL));
        assert_eq!(focused.focus, Focus::FileInput);
        assert_eq!(
            handle_key(focused, press(Key::Enter)).1,
            ConnectAction::Open
        );
    }

    #[test]
    fn typing_replaces_the_selected_default_and_edits_like_a_single_line_input() {
        let mut card = ConnectCard::first_start();
        for character in "a.db".chars() {
            card = handle_key(card, press(Key::Character(character))).0;
        }
        assert_eq!(card.file.value(), "a.db");
        card = handle_key(card, press(Key::Backspace)).0;
        card = handle_key(card, press(Key::Left)).0;
        card = handle_key(card, press(Key::Space)).0;
        assert_eq!(card.file.value(), "a. d");
        card = handle_key(card, press(Key::Home)).0;
        card = handle_key(card, press(Key::Delete)).0;
        assert_eq!(card.file.value(), ". d");
        card = handle_key(card, with(Key::Character('a'), CONTROL)).0;
        assert_eq!(card.file.selection(), Some((0, 3)));
        card = handle_key(card, press(Key::Backspace)).0;
        assert_eq!(card.file.value(), "");
        card = handle_key(card, press(Key::Character('é'))).0;
        card = handle_key(card, with(Key::Left, SHIFT)).0;
        assert_eq!(card.file.selection(), Some((0, 1)));
        let (canvas, theme) = frame(
            &ConnectCard {
                file: TextField::selected(""),
                ..ConnectCard::first_start()
            },
            ThemeKind::Light,
            1.0,
        );
        assert_eq!(
            canvas.pixel(395, 297),
            Some(theme.text_secondary),
            "placeholder"
        );
    }

    #[test]
    fn an_error_line_moves_the_panel_down() {
        let card = ConnectCard {
            error: Some(CardError {
                message: "no such file".into(),
                fix: "check the path".into(),
            }),
            ..ConnectCard::first_start()
        };
        let (canvas, theme) = frame(&card, ThemeKind::Light, 1.0);
        assert_eq!(
            canvas.pixel(560, 282 + 36),
            Some(theme.accent),
            "input moved"
        );
        assert_eq!(
            canvas.pixel(381, 296 + 36),
            Some(theme.focus_ring),
            "ring moved"
        );
        let inked = (384..600)
            .filter(|&x| (240..276).any(|y| canvas.pixel(x, y) == Some(theme.error)))
            .count();
        assert!(inked > 50, "the error text is drawn");
    }

    #[test]
    fn at_two_x_every_rect_line_and_ring_doubles() {
        for kind in [ThemeKind::Light, ThemeKind::HighContrast] {
            let (canvas, theme) = frame(&ConnectCard::first_start(), kind, 2.0);
            assert_eq!((canvas.width(), canvas.height()), (2560, 1600));
            let at = |x: usize, y: usize| canvas.pixel(x, y).unwrap();
            assert_eq!(at(1280, 280), theme.border, "{kind:?} card border row 1");
            assert_eq!(at(1280, 281), theme.border, "{kind:?} card border row 2");
            assert_eq!(at(1280, 282), theme.bg0, "{kind:?} card inside");
            assert_eq!(at(1000, 564), theme.accent, "{kind:?} input border row 1");
            assert_eq!(at(1000, 565), theme.accent, "{kind:?} input border row 2");
            assert_eq!(at(1000, 566), theme.bg0, "{kind:?} input inside");
            assert_eq!(at(761, 592), theme.focus_ring, "{kind:?} ring");
            assert_eq!(at(765, 592), theme.bg0, "{kind:?} ring gap");
            assert_eq!(at(784, 578), theme.selection, "{kind:?} selection");
            assert_eq!(at(1170, 600), theme.accent, "{kind:?} caret");
        }
        let (high, theme) = frame(&ConnectCard::first_start(), ThemeKind::HighContrast, 2.0);
        assert_eq!(high.pixel(758, 592), Some(theme.focus_ring));
        assert_eq!(high.pixel(757, 592), Some(theme.bg0));
        let (fractional, _) = frame(&ConnectCard::first_start(), ThemeKind::Light, 1.5);
        assert_eq!((fractional.width(), fractional.height()), (1920, 1200));
    }
}
