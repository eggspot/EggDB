use eggdb_platform::window::{AccessNode, AccessRect, AccessRole, AccessState};

use crate::canvas::{Canvas, Rect};
use crate::color::{Rgba, Theme};
use crate::dialect::QueryDialect;
use crate::font::{TextStyle, Typeface};
use crate::grid::{CellLook, ResultGrid, ValueKind, cell_text, visible_columns, visible_rows};
use crate::highlight::{Carry, TokenKind, tokens};
use crate::query_tab::{QueryTab, ResultView, TabFocus};
use crate::status_line;

/// The width of the results grid's body, right of the row gutter, at the golden frame's size.
pub const GRID_BODY_WIDTH: f32 = 972.0;
const GRID_LEFT: f32 = 308.0;
const BODY_BOTTOM: f32 = 776.0;
const EDITOR_TEXT_TOP: f32 = 88.0;
const LINE_HEIGHT: f32 = 20.0;
const CARET_MARGIN_LINES: usize = 4;
const ELIDE_SCAN_LIMIT: usize = 512;

/// The window's logical width in the first frame.
pub const WIDTH: usize = 1280;
/// The window's logical height in the first frame.
pub const HEIGHT: usize = 800;

/// The typefaces a frame is drawn with.
#[derive(Clone, Copy)]
pub struct Faces<'f> {
    /// The interface face.
    pub ui: &'f dyn Typeface,
    /// Its bold.
    pub ui_bold: &'f dyn Typeface,
    /// The monospace face for code and the grid.
    pub mono: &'f dyn Typeface,
}

impl std::fmt::Debug for Faces<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Faces").finish_non_exhaustive()
    }
}

/// What the main window shows: a connection, its tables and the query tab.
#[derive(Debug, Clone, PartialEq)]
pub struct MainWindow {
    /// The database's name.
    pub database: String,
    /// The file or server it was opened from.
    pub location: String,
    /// Whether it is open for writing.
    pub writable: bool,
    /// Its tables, in order.
    pub tables: Vec<String>,
    /// The table selected in the tree.
    pub selected_table: Option<usize>,
    /// The query tab.
    pub tab: QueryTab,
}

impl MainWindow {
    /// The main window as it first shows a database (`docs/ux/workspace.md` 1.7): its name
    /// and location in the toolbar, its tables in the tree, an empty `Query 1`.
    pub fn opened(database: &str, location: &str, writable: bool, tables: Vec<String>) -> Self {
        Self {
            database: database.to_owned(),
            location: location.to_owned(),
            writable,
            tables,
            selected_table: None,
            tab: QueryTab::new("Query 1"),
        }
    }

    /// The sample of `docs/ux/gui.md`'s golden frame: the `shop` database.
    pub fn sample() -> Self {
        let mut tab = QueryTab::with_text("Query 1", "SELECT id, name FROM customers ORDER BY id;");
        let rows = [("1", "Ada Lovelace"), ("2", "Grace Hopper"), ("3", "Alan Turing")]
            .iter()
            .map(|(id, name)| vec![Some((*id).to_owned()), Some((*name).to_owned())])
            .collect();
        tab.grid = Some(ResultGrid::new(
            vec!["id".into(), "name".into()],
            vec![ValueKind::Number, ValueKind::Text],
            rows,
            3,
            vec![80.0, 240.0],
        ));
        tab.summary = "3 rows · 0.4 ms".to_owned();
        tab.status_tags = "SELECT 3 · 0.4 ms".to_owned();
        Self {
            tables: vec!["customers".into(), "orders".into(), "products".into()],
            selected_table: Some(0),
            tab,
            ..Self::opened("shop", "shop.eggdb", true, Vec::new())
        }
    }
}

const BODY: (f32, f32) = (13.0, 18.0);
const SMALL: (f32, f32) = (11.0, 14.0);
const CODE: (f32, f32) = (13.0, 20.0);

struct Painter<'c, 'f> {
    canvas: &'c mut Canvas,
    theme: Theme,
    faces: Faces<'f>,
}

impl Painter<'_, '_> {
    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Rgba) {
        self.canvas.fill_rect(Rect::new(x, y, w, h), color);
    }

    fn text(
        &mut self,
        face: &dyn Typeface,
        text: &str,
        x: f32,
        top: f32,
        metrics: (f32, f32),
        color: Rgba,
    ) -> f32 {
        let style = TextStyle {
            size: metrics.0,
            line_height: metrics.1,
            color,
        };
        face.draw_text(self.canvas, text, x, top, style)
    }

    fn text_right(
        &mut self,
        face: &dyn Typeface,
        text: &str,
        right: f32,
        top: f32,
        metrics: (f32, f32),
        color: Rgba,
    ) {
        let width = face.width(text, metrics.0);
        self.text(face, text, right - width, top, metrics, color);
    }

    fn button(&mut self, rect: Rect, kind: Button) {
        let theme = self.theme;
        match kind {
            Button::Primary => self.canvas.fill_rounded_rect(rect, 4.0, theme.accent),
            Button::Secondary => {
                self.canvas.fill_rounded_rect(rect, 4.0, theme.border);
                let inner = Rect::new(rect.x + 1.0, rect.y + 1.0, rect.w - 2.0, rect.h - 2.0);
                self.canvas.fill_rounded_rect(inner, 3.0, theme.bg0);
            }
            Button::Disabled => {
                self.canvas.fill_rounded_rect(rect, 4.0, theme.border);
                let inner = Rect::new(rect.x + 1.0, rect.y + 1.0, rect.w - 2.0, rect.h - 2.0);
                self.canvas.fill_rounded_rect(inner, 3.0, theme.bg1);
            }
            Button::Ghost => {}
        }
    }

    fn polyline(&mut self, points: &[(f32, f32)], width: f32, color: Rgba) {
        for pair in points.windows(2) {
            self.canvas.line(pair[0], pair[1], width, color);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Button {
    Primary,
    Secondary,
    Disabled,
    Ghost,
}

/// Draws `window` into a new 1280 × 800 canvas in `theme`, as `docs/ux/gui.md` lays it out.
pub fn draw(window: &MainWindow, theme: Theme, faces: Faces<'_>) -> Canvas {
    let mut canvas = Canvas::new(WIDTH, HEIGHT, theme.bg0);
    let mut paint = Painter {
        canvas: &mut canvas,
        theme,
        faces,
    };
    toolbar(&mut paint, window);
    sidebar(&mut paint, window);
    tabs(&mut paint, &window.tab);
    editor(&mut paint, &window.tab);
    results(&mut paint, &window.tab);
    status_bar(&mut paint, window);
    dialect_menu(&mut paint, &window.tab);
    canvas
}

fn toolbar(paint: &mut Painter<'_, '_>, window: &MainWindow) {
    let theme = paint.theme;
    let faces = paint.faces;
    paint.rect(0.0, 0.0, 1280.0, 44.0, theme.bg2);
    paint.rect(0.0, 43.0, 1280.0, 1.0, theme.border);
    paint
        .canvas
        .stroke_circle(20.0, 22.0, 16.0, 2.0, theme.accent);
    paint.canvas.fill_circle(20.0, 23.0, 6.0, theme.brand_yolk);
    let after_name = paint.text(
        faces.ui_bold,
        &window.database,
        36.0,
        13.0,
        BODY,
        theme.text_primary,
    );
    paint.text(
        faces.ui,
        &window.location,
        (after_name + 8.0).max(72.0),
        13.0,
        BODY,
        theme.text_secondary,
    );
    paint.rect(220.0, 12.0, 1.0, 20.0, theme.border);
    let running = window.tab.running;
    let (run_look, run_ink) = if running {
        (Button::Disabled, theme.text_disabled)
    } else {
        (Button::Primary, theme.on_accent)
    };
    let (other_look, other_ink) = if running {
        (Button::Disabled, theme.text_disabled)
    } else {
        (Button::Secondary, theme.text_primary)
    };
    paint.button(Rect::new(232.0, 8.0, 72.0, 28.0), run_look);
    paint.polyline(
        &[(244.0, 17.0), (244.0, 27.0), (253.0, 22.0), (244.0, 17.0)],
        1.5,
        run_ink,
    );
    paint.text(faces.ui, "Run", 260.0, 13.0, BODY, run_ink);
    paint.button(Rect::new(312.0, 8.0, 88.0, 28.0), other_look);
    paint.polyline(
        &[(324.0, 17.0), (324.0, 27.0), (333.0, 22.0), (324.0, 17.0)],
        1.5,
        other_ink,
    );
    paint.rect(335.0, 17.0, 2.0, 10.0, other_ink);
    paint.text(faces.ui, "Run all", 342.0, 13.0, BODY, other_ink);
    paint.button(Rect::new(408.0, 8.0, 84.0, 28.0), other_look);
    for (x, y, w) in [(420.0, 17.0, 10.0), (424.0, 21.0, 6.0), (428.0, 25.0, 2.0)] {
        paint.rect(x, y, w, 2.0, other_ink);
    }
    paint.rect(421.0, 19.0, 1.0, 7.0, other_ink);
    paint.text(faces.ui, "Explain", 436.0, 13.0, BODY, other_ink);
    let (stop_look, stop_icon, stop_ink) = if running {
        (Button::Secondary, theme.error, theme.text_primary)
    } else {
        (Button::Disabled, theme.text_disabled, theme.text_disabled)
    };
    paint.button(Rect::new(500.0, 8.0, 68.0, 28.0), stop_look);
    paint
        .canvas
        .fill_rounded_rect(Rect::new(512.0, 17.0, 10.0, 10.0), 2.0, stop_icon);
    paint.text(faces.ui, "Stop", 528.0, 13.0, BODY, stop_ink);
    paint.button(Rect::new(1056.0, 8.0, 120.0, 28.0), Button::Secondary);
    paint.text(
        faces.ui,
        window.tab.dialect.label(),
        1068.0,
        13.0,
        BODY,
        theme.text_primary,
    );
    paint.polyline(
        &[(1156.0, 20.0), (1160.0, 24.0), (1164.0, 20.0)],
        1.5,
        theme.text_primary,
    );
    if window.tab.focus == TabFocus::DialectPill {
        focus_ring(paint, Rect::new(1054.0, 6.0, 124.0, 32.0));
    }
    if running {
        paint.rect(0.0, 42.0, 1280.0, 2.0, theme.accent);
    }
    paint.button(Rect::new(1240.0, 8.0, 28.0, 28.0), Button::Ghost);
    for x in [1249.0, 1254.0, 1259.0] {
        paint.canvas.fill_circle(x, 22.0, 3.0, theme.text_primary);
    }
}

fn icon_database(paint: &mut Painter<'_, '_>, x: f32, y: f32) {
    let color = paint.theme.text_secondary;
    for top in [y, y + 7.0] {
        paint
            .canvas
            .fill_rounded_rect(Rect::new(x, top, 12.0, 5.0), 2.0, color);
        let background = paint.theme.bg1;
        paint
            .canvas
            .fill_rounded_rect(Rect::new(x + 1.0, top + 1.0, 10.0, 3.0), 1.0, background);
    }
}

fn icon_folder(paint: &mut Painter<'_, '_>, x: f32, y: f32) {
    let theme = paint.theme;
    paint
        .canvas
        .fill_rounded_rect(Rect::new(x, y + 2.0, 12.0, 9.0), 2.0, theme.text_secondary);
    paint
        .canvas
        .fill_rounded_rect(Rect::new(x + 1.0, y + 3.0, 10.0, 7.0), 1.0, theme.bg1);
    paint.rect(x, y + 5.0, 12.0, 1.0, theme.text_secondary);
}

fn icon_table(paint: &mut Painter<'_, '_>, x: f32, y: f32) {
    let color = paint.theme.text_secondary;
    paint.canvas.stroke_rect(Rect::new(x, y, 12.0, 10.0), color);
    paint.rect(x, y + 3.0, 12.0, 1.0, color);
    paint.rect(x + 4.0, y + 3.0, 1.0, 7.0, color);
}

fn chevron(paint: &mut Painter<'_, '_>, x: f32, y: f32, open: bool) {
    let color = paint.theme.text_secondary;
    if open {
        paint.polyline(&[(x, y), (x + 4.0, y + 4.0), (x + 8.0, y)], 1.5, color);
    } else {
        paint.polyline(&[(x, y), (x + 4.0, y + 4.0), (x, y + 8.0)], 1.5, color);
    }
}

fn sidebar(paint: &mut Painter<'_, '_>, window: &MainWindow) {
    let theme = paint.theme;
    let faces = paint.faces;
    paint.rect(0.0, 44.0, 260.0, 732.0, theme.bg1);
    paint.rect(259.0, 44.0, 1.0, 732.0, theme.border);
    paint.button(Rect::new(8.0, 52.0, 244.0, 28.0), Button::Secondary);
    paint
        .canvas
        .stroke_circle(20.0, 64.0, 8.0, 1.5, theme.text_secondary);
    paint
        .canvas
        .line((23.0, 67.0), (26.0, 70.0), 1.5, theme.text_secondary);
    paint.text(
        faces.ui,
        "Filter objects",
        32.0,
        57.0,
        BODY,
        theme.text_secondary,
    );
    let mut y = 88.0;
    chevron(paint, 8.0, y + 8.0, true);
    icon_database(paint, 24.0, y + 6.0);
    paint.text(
        faces.ui_bold,
        &window.database,
        44.0,
        y + 3.0,
        BODY,
        theme.text_primary,
    );
    let access = if window.writable {
        "read-write"
    } else {
        "read-only"
    };
    paint.text_right(
        faces.ui,
        access,
        244.0,
        y + 5.0,
        SMALL,
        theme.text_secondary,
    );
    y += 24.0;
    chevron(paint, 24.0, y + 8.0, true);
    icon_folder(paint, 40.0, y + 6.0);
    paint.text(faces.ui, "Tables", 60.0, y + 3.0, BODY, theme.text_primary);
    paint.text_right(
        faces.ui,
        &window.tables.len().to_string(),
        244.0,
        y + 5.0,
        SMALL,
        theme.text_secondary,
    );
    y += 24.0;
    for (index, table) in window.tables.iter().enumerate() {
        let selected = window.selected_table == Some(index);
        let background = if selected {
            theme.selection_inactive
        } else {
            theme.bg1
        };
        if selected {
            paint
                .canvas
                .fill_rounded_rect(Rect::new(4.0, y, 252.0, 24.0), 4.0, background);
        }
        icon_table(paint, 56.0, y + 6.0);
        paint.text(faces.ui, table, 76.0, y + 3.0, BODY, theme.text_primary);
        y += 24.0;
    }
    for (label, count) in [("Views", Some("0")), ("Indexes", Some("3")), ("Keys", None)] {
        chevron(paint, 24.0, y + 8.0, false);
        icon_folder(paint, 40.0, y + 6.0);
        paint.text(faces.ui, label, 60.0, y + 3.0, BODY, theme.text_primary);
        if let Some(count) = count {
            paint.text_right(faces.ui, count, 244.0, y + 5.0, SMALL, theme.text_secondary);
        }
        y += 24.0;
    }
}

fn focus_ring(paint: &mut Painter<'_, '_>, rect: Rect) {
    let color = paint.theme.focus_ring;
    paint.canvas.stroke_rect(rect, color);
    paint.canvas.stroke_rect(
        Rect::new(rect.x + 1.0, rect.y + 1.0, rect.w - 2.0, rect.h - 2.0),
        color,
    );
}

fn elided(face: &dyn Typeface, text: &str, max_width: f32, size: f32) -> String {
    if face.width(text, size) <= max_width {
        return text.to_owned();
    }
    let mut kept: Vec<char> = text.chars().take(ELIDE_SCAN_LIMIT).collect();
    while !kept.is_empty() {
        kept.pop();
        let candidate: String = kept.iter().chain(std::iter::once(&'…')).collect();
        if face.width(&candidate, size) <= max_width {
            return candidate;
        }
    }
    String::new()
}

fn tabs(paint: &mut Painter<'_, '_>, tab: &QueryTab) {
    let theme = paint.theme;
    let faces = paint.faces;
    paint.rect(260.0, 44.0, 1020.0, 36.0, theme.bg2);
    paint.rect(260.0, 79.0, 1020.0, 1.0, theme.border);
    paint.rect(260.0, 44.0, 140.0, 36.0, theme.bg0);
    paint.rect(260.0, 44.0, 140.0, 2.0, theme.accent);
    paint.text(faces.ui, &tab.title, 272.0, 53.0, BODY, theme.text_primary);
    if tab.running {
        paint
            .canvas
            .stroke_circle(384.0, 62.0, 12.0, 1.5, theme.accent);
        paint.canvas.fill_circle(384.0, 57.0, 3.0, theme.accent);
    } else {
        paint
            .canvas
            .line((380.0, 58.0), (388.0, 66.0), 1.5, theme.text_secondary);
        paint
            .canvas
            .line((388.0, 58.0), (380.0, 66.0), 1.5, theme.text_secondary);
    }
    paint
        .canvas
        .line((417.0, 62.0), (427.0, 62.0), 1.5, theme.text_primary);
    paint
        .canvas
        .line((422.0, 57.0), (422.0, 67.0), 1.5, theme.text_primary);
}

fn token_color(theme: Theme, kind: TokenKind) -> Rgba {
    match kind {
        TokenKind::Keyword => theme.syntax_keyword,
        TokenKind::String => theme.syntax_string,
        TokenKind::Number => theme.syntax_number,
        TokenKind::Comment => theme.syntax_comment,
        TokenKind::Plain => theme.text_primary,
    }
}

fn column_x(face: &dyn Typeface, line: &str, column: usize) -> f32 {
    let end = line
        .char_indices()
        .nth(column)
        .map_or(line.len(), |(at, _)| at);
    320.0 + face.width(&line[..end], CODE.0)
}

fn editor_bottom(tab: &QueryTab) -> f32 {
    if tab.error.as_ref().is_some_and(|error| error.strip_shown) {
        352.0
    } else {
        380.0
    }
}

/// The first editor line in view: the caret stays at least four lines from the bottom.
pub fn first_visible_line(caret_line: usize, visible_lines: usize) -> usize {
    (caret_line + CARET_MARGIN_LINES + 1).saturating_sub(visible_lines.max(CARET_MARGIN_LINES + 1))
}

fn editor(paint: &mut Painter<'_, '_>, tab: &QueryTab) {
    let theme = paint.theme;
    let faces = paint.faces;
    paint.rect(260.0, 80.0, 1020.0, 300.0, theme.bg0);
    let bottom = editor_bottom(tab);
    let visible = ((bottom - EDITOR_TEXT_TOP) / LINE_HEIGHT).floor() as usize;
    let lines = tab.editor.lines();
    let caret = tab.editor.caret();
    let first = first_visible_line(caret.line, visible).min(lines.len().saturating_sub(1));
    let mut carry = Carry::Code;
    for line in &lines[..first] {
        let _ = tokens(line, tab.dialect, &mut carry);
    }
    let selection = tab.editor.selection();
    let squiggle = tab.error.as_ref().map(|error| {
        (
            tab.editor.position_at(error.range.start),
            tab.editor.position_at(error.range.end),
        )
    });
    let mut caret_x = 320.0;
    for (number, line) in lines.iter().enumerate().skip(first).take(visible) {
        let top = EDITOR_TEXT_TOP + LINE_HEIGHT * (number - first) as f32;
        if number == caret.line {
            paint.rect(308.0, top, 972.0, LINE_HEIGHT, theme.bg1);
        }
        if let Some((start, end)) = selection.filter(|(start, end)| (start.line..=end.line).contains(&number)) {
            let from = if number == start.line { start.column } else { 0 };
            let to = if number == end.line { end.column } else { line.chars().count() + 1 };
            let left = column_x(faces.mono, line, from);
            let right = column_x(faces.mono, line, to).max(left + 4.0);
            paint.rect(left, top, right - left, LINE_HEIGHT, theme.selection);
        }
        let number_color = if number == caret.line && lines.len() > 1 {
            theme.text_primary
        } else {
            theme.text_secondary
        };
        paint.text_right(
            faces.mono,
            &(number + 1).to_string(),
            296.0,
            top,
            CODE,
            number_color,
        );
        let caret_byte = (number == caret.line).then(|| {
            line.char_indices()
                .nth(caret.column)
                .map_or(line.len(), |(at, _)| at)
        });
        let mut pen = 320.0;
        for (range, kind) in tokens(line, tab.dialect, &mut carry) {
            if let Some(at) = caret_byte.filter(|at| range.contains(at)) {
                caret_x = pen + faces.mono.width(&line[range.start..at], CODE.0);
            }
            pen = paint.text(faces.mono, &line[range], pen, top, CODE, token_color(theme, kind));
        }
        if caret_byte == Some(line.len()) {
            caret_x = pen;
        }
        if let Some((start, end)) = squiggle.filter(|(start, end)| (start.line..=end.line).contains(&number)) {
            let from = if number == start.line { start.column } else { 0 };
            let to = if number == end.line { end.column } else { line.chars().count() };
            let left = column_x(faces.mono, line, from);
            let right = column_x(faces.mono, line, to).max(left + 4.0);
            zigzag(paint, left, right, top + LINE_HEIGHT - 2.0);
        }
    }
    if (first..first + visible).contains(&caret.line) {
        let caret_top = EDITOR_TEXT_TOP + LINE_HEIGHT * (caret.line - first) as f32;
        paint.rect(caret_x.round(), caret_top, 2.0, LINE_HEIGHT, theme.accent);
    }
    if let Some(error) = tab.error.as_ref().filter(|error| error.strip_shown) {
        error_strip(paint, &error.strip_text(), error.fix_label().as_deref());
    }
    paint.rect(260.0, 380.0, 1020.0, 6.0, theme.bg2);
    paint.rect(260.0, 382.0, 1020.0, 1.0, theme.border);
}

fn zigzag(paint: &mut Painter<'_, '_>, left: f32, right: f32, top: f32) {
    let color = paint.theme.error;
    let mut points = Vec::new();
    let mut x = left;
    let mut up = false;
    while x <= right {
        points.push((x, if up { top } else { top + 2.0 }));
        x += 2.0;
        up = !up;
    }
    paint.polyline(&points, 1.0, color);
}

fn error_icon(paint: &mut Painter<'_, '_>, x: f32, y: f32) {
    let color = paint.theme.error;
    paint.canvas.stroke_circle(x + 6.0, y + 6.0, 12.0, 1.5, color);
    paint.canvas.line((x + 4.0, y + 4.0), (x + 8.0, y + 8.0), 1.5, color);
    paint.canvas.line((x + 8.0, y + 4.0), (x + 4.0, y + 8.0), 1.5, color);
}

fn error_strip(paint: &mut Painter<'_, '_>, text: &str, fix: Option<&str>) {
    let theme = paint.theme;
    let faces = paint.faces;
    paint.rect(260.0, 352.0, 1020.0, 28.0, theme.bg1);
    paint.rect(260.0, 352.0, 1020.0, 1.0, theme.border);
    error_icon(paint, 272.0, 360.0);
    let fix_width = fix.map_or(0.0, |label| faces.ui.width(label, BODY.0) + 24.0);
    let shown = elided(faces.mono, text, 1268.0 - 292.0 - fix_width, CODE.0);
    paint.text(faces.mono, &shown, 292.0, 356.0, CODE, theme.text_primary);
    if let Some(label) = fix {
        paint.text_right(faces.ui, label, 1268.0, 357.0, BODY, theme.accent);
    }
}

fn warning_icon(paint: &mut Painter<'_, '_>, x: f32, y: f32) {
    let color = paint.theme.warning;
    paint.polyline(
        &[(x + 6.0, y), (x + 12.0, y + 11.0), (x, y + 11.0), (x + 6.0, y)],
        1.5,
        color,
    );
    paint.rect(x + 5.5, y + 4.0, 1.0, 4.0, color);
}

fn results(paint: &mut Painter<'_, '_>, tab: &QueryTab) {
    let theme = paint.theme;
    let faces = paint.faces;
    paint.rect(260.0, 386.0, 1020.0, 32.0, theme.bg1);
    paint.rect(260.0, 417.0, 1020.0, 1.0, theme.border);
    let messages = tab.view == ResultView::Messages;
    let (results_face, messages_face) = if messages {
        (faces.ui, faces.ui_bold)
    } else {
        (faces.ui_bold, faces.ui)
    };
    let (results_ink, messages_ink) = if messages {
        (theme.text_secondary, theme.text_primary)
    } else {
        (theme.text_primary, theme.text_secondary)
    };
    paint.text(results_face, "Results", 272.0, 393.0, BODY, results_ink);
    if messages {
        paint.rect(344.0, 414.0, 64.0, 2.0, theme.accent);
    } else {
        paint.rect(272.0, 414.0, 48.0, 2.0, theme.accent);
    }
    paint.text(messages_face, "Messages", 344.0, 393.0, BODY, messages_ink);
    paint.text(faces.ui, &tab.summary, 428.0, 393.0, BODY, theme.text_secondary);
    paint.text(faces.ui, "Export…", 1204.0, 393.0, BODY, theme.text_primary);
    if messages {
        for (index, line) in tab.messages.iter().enumerate().take(17) {
            let top = 426.0 + LINE_HEIGHT * index as f32;
            let shown = elided(faces.mono, line, 1000.0, CODE.0);
            paint.text(faces.mono, &shown, 272.0, top, CODE, theme.text_primary);
        }
        return;
    }
    let Some(grid) = &tab.grid else {
        paint.rect(260.0, 418.0, 1020.0, 28.0, theme.bg1);
        paint.rect(260.0, 445.0, 1020.0, 1.0, theme.border);
        paint.rect(307.0, 418.0, 1.0, 27.0, theme.border_strong);
        paint.rect(307.0, 446.0, 1.0, 330.0, theme.border);
        return;
    };
    let mut top = 418.0;
    if grid.truncated() {
        paint.rect(260.0, 418.0, 1020.0, 28.0, theme.bg1);
        paint.rect(260.0, 445.0, 1020.0, 1.0, theme.border);
        warning_icon(paint, 272.0, 426.0);
        let banner = status_line::truncated_banner(grid.rows.len(), grid.row_count);
        let shown = elided(faces.ui, &banner, 1140.0 - 292.0, BODY.0);
        paint.text(faces.ui, &shown, 292.0, 423.0, BODY, theme.text_primary);
        paint.text_right(faces.ui, "Export…", 1236.0, 423.0, BODY, theme.text_primary);
        paint.canvas.line((1252.0, 428.0), (1260.0, 436.0), 1.5, theme.text_secondary);
        paint.canvas.line((1260.0, 428.0), (1252.0, 436.0), 1.5, theme.text_secondary);
        top = 446.0;
    }
    grid_body(paint, grid, top);
}

fn grid_body(paint: &mut Painter<'_, '_>, grid: &ResultGrid, top: f32) {
    let theme = paint.theme;
    let faces = paint.faces;
    paint.rect(260.0, top, 1020.0, 28.0, theme.bg1);
    paint.rect(260.0, top + 27.0, 1020.0, 1.0, theme.border);
    let edges = grid.column_edges();
    let columns = visible_columns(&grid.widths, grid.scroll_x, GRID_BODY_WIDTH);
    let x_of = |column: usize| GRID_LEFT + edges.get(column).copied().unwrap_or(0.0) - grid.scroll_x;
    let marked = grid.focused || grid.selection_size() != (1, 1);
    let (selected_rows, selected_columns) = grid.selection();
    for column in columns.clone() {
        let (left, right) = (x_of(column), x_of(column + 1));
        if marked && selected_columns.contains(&column) {
            paint.rect(left, top, right - left - 1.0, 27.0, theme.bg2);
        }
        let name = grid.columns.get(column).map_or("", String::as_str);
        let name = elided(faces.ui, name, right - left - 24.0, BODY.0);
        if grid.kind(column).right_aligned() {
            paint.text_right(faces.ui, &name, right - 12.0, top + 5.0, BODY, theme.text_primary);
        } else {
            paint.text(faces.ui, &name, left + 12.0, top + 5.0, BODY, theme.text_primary);
        }
        paint.rect(right - 1.0, top, 1.0, 27.0, theme.border_strong);
    }
    if grid.scroll_x > 0.0 {
        paint.rect(260.0, top, 47.0, 27.0, theme.bg1);
    }
    paint.rect(307.0, top, 1.0, 27.0, theme.border_strong);
    let body_top = top + 28.0;
    let rows = visible_rows(grid.rows.len(), grid.first_row, BODY_BOTTOM - body_top);
    for row in rows {
        let y = body_top + 24.0 * (row - grid.first_row) as f32;
        let fill = if row % 2 == 1 { theme.zebra } else { theme.bg0 };
        paint.rect(260.0, y, 1020.0, 24.0, fill);
        for column in columns.clone() {
            let (left, right) = (x_of(column), x_of(column + 1));
            let cell = crate::grid::Cell { row, column };
            if marked && grid.is_selected(cell) {
                let color = if grid.focused { theme.selection } else { theme.selection_inactive };
                paint.rect(left, y, right - left, 24.0, color);
            }
            let kind = grid.kind(column);
            let (text, look) = cell_text(grid.value(cell), kind);
            let color = match look {
                CellLook::Primary => theme.text_primary,
                CellLook::Secondary => theme.text_secondary,
                CellLook::Null => theme.text_disabled,
            };
            let text = elided(faces.mono, &text, right - left - 24.0, CODE.0);
            if kind.right_aligned() {
                paint.text_right(faces.mono, &text, right - 12.0, y + 2.0, CODE, color);
            } else {
                paint.text(faces.mono, &text, left + 12.0, y + 2.0, CODE, color);
            }
        }
        if grid.scroll_x > 0.0 {
            paint.rect(260.0, y, 47.0, 24.0, fill);
        }
        if marked && selected_rows.contains(&row) {
            paint.rect(260.0, y, 47.0, 24.0, theme.bg2);
        }
        paint.text_right(
            faces.mono,
            &(row + 1).to_string(),
            296.0,
            y + 2.0,
            CODE,
            theme.text_secondary,
        );
        if grid.focused && grid.current.row == row && columns.contains(&grid.current.column) {
            let (left, right) = (x_of(grid.current.column), x_of(grid.current.column + 1));
            let rect = Rect::new(left.max(GRID_LEFT), y, right - left.max(GRID_LEFT), 24.0);
            paint.canvas.stroke_rect(rect, theme.accent);
            paint.canvas.stroke_rect(
                Rect::new(rect.x + 1.0, rect.y + 1.0, rect.w - 2.0, rect.h - 2.0),
                theme.accent,
            );
        }
    }
    paint.rect(307.0, body_top, 1.0, BODY_BOTTOM - body_top, theme.border);
}

fn status_bar(paint: &mut Painter<'_, '_>, window: &MainWindow) {
    let theme = paint.theme;
    let faces = paint.faces;
    paint.rect(0.0, 776.0, 1280.0, 24.0, theme.bg2);
    paint.rect(0.0, 776.0, 1280.0, 1.0, theme.border);
    paint.canvas.fill_circle(16.0, 788.0, 8.0, theme.success);
    let access = if window.writable {
        "read-write"
    } else {
        "read-only"
    };
    paint.text(
        faces.ui,
        &format!("Connected · {} · {access}", window.location),
        26.0,
        781.0,
        SMALL,
        theme.text_secondary,
    );
    paint.text(
        faces.ui,
        &status_bar_left(&window.tab),
        260.0,
        781.0,
        SMALL,
        theme.text_secondary,
    );
    let caret = window.tab.editor.caret();
    let mut right = 1268.0;
    for item in [
        window.tab.dialect.label().to_owned(),
        format!("Ln {}, Col {}", caret.line + 1, caret.column + 1),
        "UTF-8".to_owned(),
    ] {
        let width = faces.ui.width(&item, SMALL.0);
        paint.text(
            faces.ui,
            &item,
            right - width,
            781.0,
            SMALL,
            theme.text_secondary,
        );
        right -= width + 16.0;
    }
}

/// The status bar's left group: a notice while one shows, else the last run's tags, with
/// the selection's size while more than one cell is selected.
pub fn status_bar_left(tab: &QueryTab) -> String {
    if let Some(notice) = &tab.notice {
        return notice.clone();
    }
    match tab.grid.as_ref().map(ResultGrid::selection_size) {
        Some((rows, columns)) if rows * columns > 1 => format!(
            "{}    {}",
            tab.status_tags,
            status_line::selection_text(rows, columns)
        ),
        _ => tab.status_tags.clone(),
    }
}

fn dialect_menu(paint: &mut Painter<'_, '_>, tab: &QueryTab) {
    let Some(highlighted) = tab.dialect_menu else {
        return;
    };
    let theme = paint.theme;
    let faces = paint.faces;
    let menu = Rect::new(1056.0, 40.0, 160.0, 28.0 * QueryDialect::ALL.len() as f32);
    paint.rect(menu.x + 2.0, menu.y + 4.0, menu.w, menu.h, theme.border);
    paint.canvas.fill_rounded_rect(menu, 6.0, theme.border);
    paint.canvas.fill_rounded_rect(
        Rect::new(menu.x + 1.0, menu.y + 1.0, menu.w - 2.0, menu.h - 2.0),
        5.0,
        theme.bg0,
    );
    for (index, dialect) in QueryDialect::ALL.iter().enumerate() {
        let top = menu.y + 28.0 * index as f32;
        if index == highlighted {
            paint.rect(menu.x + 1.0, top + 1.0, menu.w - 2.0, 26.0, theme.bg2);
        }
        if *dialect == tab.dialect {
            paint.polyline(
                &[(1066.0, top + 14.0), (1070.0, top + 18.0), (1078.0, top + 10.0)],
                1.5,
                theme.accent,
            );
        }
        paint.text(faces.ui, dialect.label(), 1088.0, top + 5.0, BODY, theme.text_primary);
    }
}

/// The ids of the main window's accessibility nodes; grid cells count up from
/// [`NODE_FIRST_CELL`] in reading order of the visible cells.
pub const NODE_MAIN_WINDOW: u32 = 100;
/// The toolbar.
pub const NODE_TOOLBAR: u32 = 101;
/// `Run`.
pub const NODE_RUN: u32 = 102;
/// `Run all`.
pub const NODE_RUN_ALL: u32 = 103;
/// `Explain`.
pub const NODE_EXPLAIN: u32 = 104;
/// `Stop`.
pub const NODE_STOP: u32 = 105;
/// The dialect pill.
pub const NODE_DIALECT: u32 = 106;
/// The tab strip.
pub const NODE_QUERIES: u32 = 107;
/// The query tab.
pub const NODE_QUERY_TAB: u32 = 108;
/// The editor.
pub const NODE_EDITOR: u32 = 109;
/// The error strip.
pub const NODE_ERROR_STRIP: u32 = 110;
/// The results header's segments.
pub const NODE_RESULT_VIEWS: u32 = 111;
/// The status line.
pub const NODE_STATUS_LINE: u32 = 112;
/// The truncated-results banner.
pub const NODE_BANNER: u32 = 113;
/// The grid.
pub const NODE_GRID: u32 = 114;
/// The status bar.
pub const NODE_STATUS_BAR: u32 = 115;
/// The `Results` segment; `Messages` is the next id.
pub const NODE_RESULTS_SEGMENT: u32 = 116;
/// The first column header; the others follow.
pub const NODE_FIRST_HEADER: u32 = 200;
/// The first visible cell.
pub const NODE_FIRST_CELL: u32 = 1000;

fn node(id: u32, parent: u32, role: AccessRole, name: &str, bounds: (f32, f32, f32, f32)) -> AccessNode {
    AccessNode {
        id,
        parent: Some(parent),
        role,
        name: name.to_owned(),
        value: None,
        placeholder: None,
        state: AccessState::default(),
        bounds: AccessRect {
            x: bounds.0,
            y: bounds.1,
            width: bounds.2,
            height: bounds.3,
        },
    }
}

/// What a screen reader reads for a cell: the drawn text, `null` for NULL, `blob, 184
/// bytes` for bytes.
pub fn spoken_cell(value: Option<&str>, kind: ValueKind) -> String {
    match (value, kind) {
        (None, _) => "null".to_owned(),
        (Some(text), ValueKind::Blob) => {
            let hex = text.strip_prefix("\\x").unwrap_or(text);
            format!("blob, {} bytes", hex.len() / 2)
        }
        (Some(text), _) => cell_text(Some(text), kind).0,
    }
}

/// The main window's accessibility tree for the query tab and the grid
/// (`docs/ux/workspace.md` 7), in reading order.
pub fn accessibility_tree(window: &MainWindow) -> Vec<AccessNode> {
    let tab = &window.tab;
    let mut root = node(NODE_MAIN_WINDOW, 0, AccessRole::Window, "EggDB", (0.0, 0.0, 1280.0, 800.0));
    root.parent = None;
    let mut nodes = vec![root, node(NODE_TOOLBAR, NODE_MAIN_WINDOW, AccessRole::Group, "Toolbar", (0.0, 0.0, 1280.0, 44.0))];
    for (id, name, x, w) in [
        (NODE_RUN, "Run", 232.0, 72.0),
        (NODE_RUN_ALL, "Run all", 312.0, 88.0),
        (NODE_EXPLAIN, "Explain", 408.0, 84.0),
        (NODE_STOP, "Stop", 500.0, 68.0),
    ] {
        let mut button = node(id, NODE_TOOLBAR, AccessRole::Button, name, (x, 8.0, w, 28.0));
        button.state.disabled = if id == NODE_STOP { !tab.running } else { tab.running };
        nodes.push(button);
    }
    let mut pill = node(NODE_DIALECT, NODE_TOOLBAR, AccessRole::Button, "Dialect", (1056.0, 8.0, 120.0, 28.0));
    pill.value = Some(tab.dialect.label().to_owned());
    pill.state.focused = tab.focus == TabFocus::DialectPill;
    nodes.push(pill);
    nodes.push(node(NODE_QUERIES, NODE_MAIN_WINDOW, AccessRole::TabList, "Queries", (260.0, 44.0, 1020.0, 36.0)));
    let title = if tab.running { format!("{}, running", tab.title) } else { tab.title.clone() };
    let mut query = node(NODE_QUERY_TAB, NODE_QUERIES, AccessRole::Tab, &title, (260.0, 44.0, 140.0, 36.0));
    query.state.selected = true;
    nodes.push(query);
    let mut text = node(NODE_EDITOR, NODE_MAIN_WINDOW, AccessRole::TextField, "Query", (260.0, 80.0, 1020.0, 300.0));
    text.value = Some(tab.editor.text());
    text.state.focused = tab.focus == TabFocus::Editor;
    nodes.push(text);
    if let Some(error) = tab.error.as_ref().filter(|error| error.strip_shown) {
        nodes.push(node(NODE_ERROR_STRIP, NODE_MAIN_WINDOW, AccessRole::Status, &error.strip_text(), (260.0, 352.0, 1020.0, 28.0)));
    }
    nodes.push(node(NODE_RESULT_VIEWS, NODE_MAIN_WINDOW, AccessRole::TabList, "Result views", (260.0, 386.0, 1020.0, 32.0)));
    for (offset, (name, view, x)) in [("Results", ResultView::Results, 272.0), ("Messages", ResultView::Messages, 344.0)].into_iter().enumerate() {
        let mut segment = node(NODE_RESULTS_SEGMENT + offset as u32, NODE_RESULT_VIEWS, AccessRole::Tab, name, (x, 386.0, 72.0, 32.0));
        segment.state.selected = tab.view == view;
        nodes.push(segment);
    }
    nodes.push(node(NODE_STATUS_LINE, NODE_MAIN_WINDOW, AccessRole::Status, &status_line::spoken(&tab.summary), (428.0, 386.0, 760.0, 32.0)));
    if let Some(grid) = tab.grid.as_ref().filter(|_| tab.view == ResultView::Results) {
        grid_nodes(&mut nodes, grid);
    }
    let mut bar = node(NODE_STATUS_BAR, NODE_MAIN_WINDOW, AccessRole::Group, "Status", (0.0, 776.0, 1280.0, 24.0));
    bar.value = Some(status_bar_left(tab));
    nodes.push(bar);
    nodes
}

fn grid_nodes(nodes: &mut Vec<AccessNode>, grid: &ResultGrid) {
    let mut top = 418.0;
    if grid.truncated() {
        nodes.push(node(
            NODE_BANNER,
            NODE_MAIN_WINDOW,
            AccessRole::Status,
            &status_line::truncated_banner(grid.rows.len(), grid.row_count),
            (260.0, 418.0, 1020.0, 28.0),
        ));
        top = 446.0;
    }
    let (rows, columns) = grid.selection_size();
    let mut table = node(NODE_GRID, NODE_MAIN_WINDOW, AccessRole::Group, "Results", (260.0, top, 1020.0, BODY_BOTTOM - top));
    table.value = Some(format!(
        "{} rows, {} columns; {} by {} selected",
        grid.row_count,
        grid.columns.len(),
        status_line::rows_text(rows),
        if columns == 1 { "1 column".to_owned() } else { format!("{columns} columns") }
    ));
    nodes.push(table);
    let edges = grid.column_edges();
    let visible = visible_columns(&grid.widths, grid.scroll_x, GRID_BODY_WIDTH);
    let x_of = |column: usize| GRID_LEFT + edges.get(column).copied().unwrap_or(0.0) - grid.scroll_x;
    for column in visible.clone() {
        let name = grid.columns.get(column).map_or("", String::as_str);
        let width = x_of(column + 1) - x_of(column);
        nodes.push(node(NODE_FIRST_HEADER + column as u32 % 800, NODE_GRID, AccessRole::Heading, name, (x_of(column), top, width, 28.0)));
    }
    let body_top = top + 28.0;
    let mut id = NODE_FIRST_CELL;
    for row in visible_rows(grid.rows.len(), grid.first_row, BODY_BOTTOM - body_top) {
        for column in visible.clone() {
            let cell = crate::grid::Cell { row, column };
            let y = body_top + 24.0 * (row - grid.first_row) as f32;
            let name = grid.columns.get(column).map_or("", String::as_str);
            let mut item = node(id, NODE_GRID, AccessRole::Group, name, (x_of(column), y, x_of(column + 1) - x_of(column), 24.0));
            item.value = Some(spoken_cell(grid.value(cell), grid.kind(column)));
            item.state.selected = grid.is_selected(cell) && (grid.focused || rows * columns > 1);
            item.state.focused = grid.focused && grid.current == cell;
            nodes.push(item);
            id = id.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ThemeKind;
    use crate::font::BoxFace;

    fn frame(kind: ThemeKind) -> (Canvas, Theme) {
        let theme = Theme::of(kind);
        let faces = Faces {
            ui: &BoxFace,
            ui_bold: &BoxFace,
            mono: &BoxFace,
        };
        (draw(&MainWindow::sample(), theme, faces), theme)
    }

    #[test]
    fn the_first_frame_has_every_region_where_the_design_puts_it() {
        for kind in [ThemeKind::Light, ThemeKind::Dark] {
            let (canvas, theme) = frame(kind);
            let at = |x: usize, y: usize| canvas.pixel(x, y).unwrap();
            assert_eq!(at(700, 30), theme.bg2, "toolbar");
            assert_eq!(at(700, 43), theme.border, "toolbar border");
            assert_eq!(at(150, 600), theme.bg1, "sidebar");
            assert_eq!(at(259, 600), theme.border, "sidebar border");
            assert_eq!(at(300, 45), theme.accent, "active tab stripe");
            assert_eq!(at(800, 60), theme.bg2, "tab strip");
            assert_eq!(at(800, 250), theme.bg0, "editor");
            assert_eq!(at(1200, 100), theme.bg1, "current line");
            assert_eq!(at(800, 382), theme.border, "splitter line");
            assert_eq!(at(900, 400), theme.bg1, "results header");
            assert_eq!(at(280, 415), theme.accent, "results stripe");
            assert_eq!(at(1000, 460), theme.bg0, "first row");
            assert_eq!(at(1000, 480), theme.zebra, "second row");
            assert_eq!(at(1000, 600), theme.bg0, "empty grid");
            assert_eq!(at(307, 600), theme.border, "gutter edge");
            assert_eq!(at(700, 790), theme.bg2, "status bar");
            assert_eq!(at(240, 20), theme.accent, "run button");
            assert_eq!(at(150, 147), theme.selection_inactive, "selected table");
        }
    }

    #[test]
    fn the_caret_follows_the_last_character() {
        let (canvas, theme) = frame(ThemeKind::Light);
        let advance = BoxFace.advance('a', 13.0) as usize;
        let caret = 320 + advance * MainWindow::sample().tab.editor.text().chars().count();
        assert_eq!(canvas.pixel(caret, 95), Some(theme.accent));
    }

    fn box_faces() -> Faces<'static> {
        Faces {
            ui: &BoxFace,
            ui_bold: &BoxFace,
            mono: &BoxFace,
        }
    }

    #[test]
    fn the_open_dialect_menu_has_five_rows_below_the_pill() {
        let theme = Theme::of(ThemeKind::Light);
        let mut window = MainWindow::sample();
        window.tab.dialect_menu = Some(1);
        let canvas = draw(&window, theme, box_faces());
        assert_eq!(canvas.pixel(1200, 42 + 28), Some(theme.bg2), "the highlighted row");
        assert_eq!(canvas.pixel(1200, 42), Some(theme.bg0), "the first row");
        assert_eq!(canvas.pixel(1200, 40 + 140 + 2), Some(theme.border), "the shadow under the last row");
    }

    #[test]
    fn a_running_tab_disables_run_and_enables_stop() {
        let theme = Theme::of(ThemeKind::Light);
        let mut window = MainWindow::sample();
        window.tab.started();
        let canvas = draw(&window, theme, box_faces());
        assert_eq!(canvas.pixel(240, 20), Some(theme.bg1), "Run is disabled");
        assert_eq!(canvas.pixel(516, 21), Some(theme.error), "Stop's square");
        assert_eq!(canvas.pixel(700, 43), Some(theme.accent), "the running bar");
        let tree = accessibility_tree(&window);
        let run = tree.iter().find(|node| node.id == NODE_RUN).unwrap();
        let stop = tree.iter().find(|node| node.id == NODE_STOP).unwrap();
        assert!(run.state.disabled && !stop.state.disabled);
        let query = tree.iter().find(|node| node.id == NODE_QUERY_TAB).unwrap();
        assert_eq!(query.name, "Query 1, running");
    }

    #[test]
    fn the_tree_names_the_grid_its_headers_and_its_cells() {
        let mut window = MainWindow::sample();
        let tree = accessibility_tree(&window);
        let grid = tree.iter().find(|node| node.id == NODE_GRID).unwrap();
        assert_eq!(grid.value.as_deref(), Some("3 rows, 2 columns; 1 row by 1 column selected"));
        let headers: Vec<&str> = tree.iter().filter(|node| node.role == AccessRole::Heading).map(|node| node.name.as_str()).collect();
        assert_eq!(headers, ["id", "name"]);
        let first = tree.iter().find(|node| node.id == NODE_FIRST_CELL + 1).unwrap();
        assert_eq!(first.value.as_deref(), Some("Ada Lovelace"));
        let status = tree.iter().find(|node| node.id == NODE_STATUS_LINE).unwrap();
        assert_eq!(status.name, "3 rows, 0.4 milliseconds");
        window.tab.focus = TabFocus::Grid;
        if let Some(grid) = &mut window.tab.grid {
            grid.focused = true;
            grid.current = crate::grid::Cell { row: 1, column: 1 };
        }
        let tree = accessibility_tree(&window);
        let focused = tree.iter().find(|node| node.state.focused).unwrap();
        assert_eq!(focused.value.as_deref(), Some("Grace Hopper"));
        assert_eq!(spoken_cell(None, ValueKind::Text), "null");
        assert_eq!(spoken_cell(Some("\\x0102"), ValueKind::Blob), "blob, 2 bytes");
        assert_eq!(first_visible_line(0, 14), 0);
        assert_eq!(first_visible_line(12, 14), 3);
    }
}
