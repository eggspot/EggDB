use crate::canvas::{Canvas, Rect};
use crate::color::{Rgba, Theme};
use crate::font::{TextStyle, Typeface};

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

/// What the main window shows: a connection, its tables, the editor's text and a result.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// The editor's one line of SQL.
    pub sql: String,
    /// The result's columns, with whether each holds numbers (right-aligned).
    pub columns: Vec<(String, bool)>,
    /// The result's rows, as text.
    pub rows: Vec<Vec<String>>,
    /// The last run's time, as shown.
    pub elapsed: String,
    /// The dialect's name.
    pub dialect: String,
}

impl MainWindow {
    /// The sample of `docs/ux/gui.md`'s golden frame: the `shop` database.
    pub fn sample() -> Self {
        Self {
            database: "shop".to_owned(),
            location: "shop.eggdb".to_owned(),
            writable: true,
            tables: vec!["customers".into(), "orders".into(), "products".into()],
            selected_table: Some(0),
            sql: "SELECT id, name FROM customers ORDER BY id;".to_owned(),
            columns: vec![("id".into(), true), ("name".into(), false)],
            rows: vec![
                vec!["1".into(), "Ada Lovelace".into()],
                vec!["2".into(), "Grace Hopper".into()],
                vec!["3".into(), "Alan Turing".into()],
            ],
            elapsed: "0.4 ms".to_owned(),
            dialect: "PostgreSQL".to_owned(),
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

    fn text(&mut self, face: &dyn Typeface, text: &str, x: f32, top: f32, metrics: (f32, f32), color: Rgba) -> f32 {
        let style = TextStyle {
            size: metrics.0,
            line_height: metrics.1,
            color,
        };
        face.draw_text(self.canvas, text, x, top, style)
    }

    fn text_right(&mut self, face: &dyn Typeface, text: &str, right: f32, top: f32, metrics: (f32, f32), color: Rgba) {
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
    tabs(&mut paint);
    editor(&mut paint, window);
    results(&mut paint, window);
    status_bar(&mut paint, window);
    canvas
}

fn toolbar(paint: &mut Painter<'_, '_>, window: &MainWindow) {
    let theme = paint.theme;
    let faces = paint.faces;
    paint.rect(0.0, 0.0, 1280.0, 44.0, theme.bg2);
    paint.rect(0.0, 43.0, 1280.0, 1.0, theme.border);
    paint.canvas.stroke_circle(20.0, 22.0, 16.0, 2.0, theme.accent);
    paint.canvas.fill_circle(20.0, 23.0, 6.0, theme.brand_yolk);
    let after_name = paint.text(faces.ui_bold, &window.database, 36.0, 13.0, BODY, theme.text_primary);
    paint.text(faces.ui, &window.location, (after_name + 8.0).max(72.0), 13.0, BODY, theme.text_secondary);
    paint.rect(220.0, 12.0, 1.0, 20.0, theme.border);
    paint.button(Rect::new(232.0, 8.0, 72.0, 28.0), Button::Primary);
    paint.polyline(&[(244.0, 17.0), (244.0, 27.0), (253.0, 22.0), (244.0, 17.0)], 1.5, theme.on_accent);
    paint.text(faces.ui, "Run", 260.0, 13.0, BODY, theme.on_accent);
    paint.button(Rect::new(312.0, 8.0, 88.0, 28.0), Button::Secondary);
    paint.polyline(&[(324.0, 17.0), (324.0, 27.0), (333.0, 22.0), (324.0, 17.0)], 1.5, theme.text_primary);
    paint.rect(335.0, 17.0, 2.0, 10.0, theme.text_primary);
    paint.text(faces.ui, "Run all", 342.0, 13.0, BODY, theme.text_primary);
    paint.button(Rect::new(408.0, 8.0, 84.0, 28.0), Button::Secondary);
    for (x, y, w) in [(420.0, 17.0, 10.0), (424.0, 21.0, 6.0), (428.0, 25.0, 2.0)] {
        paint.rect(x, y, w, 2.0, theme.text_primary);
    }
    paint.rect(421.0, 19.0, 1.0, 7.0, theme.text_primary);
    paint.text(faces.ui, "Explain", 436.0, 13.0, BODY, theme.text_primary);
    paint.button(Rect::new(500.0, 8.0, 68.0, 28.0), Button::Disabled);
    paint.canvas.fill_rounded_rect(Rect::new(512.0, 17.0, 10.0, 10.0), 2.0, theme.text_disabled);
    paint.text(faces.ui, "Stop", 528.0, 13.0, BODY, theme.text_disabled);
    paint.button(Rect::new(1056.0, 8.0, 120.0, 28.0), Button::Secondary);
    paint.text(faces.ui, &window.dialect, 1068.0, 13.0, BODY, theme.text_primary);
    paint.polyline(&[(1156.0, 20.0), (1160.0, 24.0), (1164.0, 20.0)], 1.5, theme.text_primary);
    paint.button(Rect::new(1240.0, 8.0, 28.0, 28.0), Button::Ghost);
    for x in [1249.0, 1254.0, 1259.0] {
        paint.canvas.fill_circle(x, 22.0, 3.0, theme.text_primary);
    }
}

fn icon_database(paint: &mut Painter<'_, '_>, x: f32, y: f32) {
    let color = paint.theme.text_secondary;
    for top in [y, y + 7.0] {
        paint.canvas.fill_rounded_rect(Rect::new(x, top, 12.0, 5.0), 2.0, color);
        let background = paint.theme.bg1;
        paint.canvas.fill_rounded_rect(Rect::new(x + 1.0, top + 1.0, 10.0, 3.0), 1.0, background);
    }
}

fn icon_folder(paint: &mut Painter<'_, '_>, x: f32, y: f32) {
    let theme = paint.theme;
    paint.canvas.fill_rounded_rect(Rect::new(x, y + 2.0, 12.0, 9.0), 2.0, theme.text_secondary);
    paint.canvas.fill_rounded_rect(Rect::new(x + 1.0, y + 3.0, 10.0, 7.0), 1.0, theme.bg1);
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
    paint.canvas.stroke_circle(20.0, 64.0, 8.0, 1.5, theme.text_secondary);
    paint.canvas.line((23.0, 67.0), (26.0, 70.0), 1.5, theme.text_secondary);
    paint.text(faces.ui, "Filter objects", 32.0, 57.0, BODY, theme.text_secondary);
    let mut y = 88.0;
    chevron(paint, 8.0, y + 8.0, true);
    icon_database(paint, 24.0, y + 6.0);
    paint.text(faces.ui_bold, &window.database, 44.0, y + 3.0, BODY, theme.text_primary);
    let access = if window.writable { "read-write" } else { "read-only" };
    paint.text_right(faces.ui, access, 244.0, y + 5.0, SMALL, theme.text_secondary);
    y += 24.0;
    chevron(paint, 24.0, y + 8.0, true);
    icon_folder(paint, 40.0, y + 6.0);
    paint.text(faces.ui, "Tables", 60.0, y + 3.0, BODY, theme.text_primary);
    paint.text_right(faces.ui, &window.tables.len().to_string(), 244.0, y + 5.0, SMALL, theme.text_secondary);
    y += 24.0;
    for (index, table) in window.tables.iter().enumerate() {
        let selected = window.selected_table == Some(index);
        let background = if selected { theme.selection_inactive } else { theme.bg1 };
        if selected {
            paint.canvas.fill_rounded_rect(Rect::new(4.0, y, 252.0, 24.0), 4.0, background);
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

fn tabs(paint: &mut Painter<'_, '_>) {
    let theme = paint.theme;
    let faces = paint.faces;
    paint.rect(260.0, 44.0, 1020.0, 36.0, theme.bg2);
    paint.rect(260.0, 79.0, 1020.0, 1.0, theme.border);
    paint.rect(260.0, 44.0, 140.0, 36.0, theme.bg0);
    paint.rect(260.0, 44.0, 140.0, 2.0, theme.accent);
    paint.text(faces.ui, "Query 1", 272.0, 53.0, BODY, theme.text_primary);
    paint.canvas.line((380.0, 58.0), (388.0, 66.0), 1.5, theme.text_secondary);
    paint.canvas.line((388.0, 58.0), (380.0, 66.0), 1.5, theme.text_secondary);
    paint.canvas.line((417.0, 62.0), (427.0, 62.0), 1.5, theme.text_primary);
    paint.canvas.line((422.0, 57.0), (422.0, 67.0), 1.5, theme.text_primary);
}

fn editor(paint: &mut Painter<'_, '_>, window: &MainWindow) {
    let theme = paint.theme;
    let faces = paint.faces;
    paint.rect(260.0, 80.0, 1020.0, 300.0, theme.bg0);
    let lines: Vec<&str> = window.sql.split('\n').collect();
    let caret_line = lines.len() - 1;
    let mut in_comment = false;
    let mut caret_x = 320.0;
    for (number, line) in lines.iter().enumerate().take(14) {
        let top = 88.0 + 20.0 * number as f32;
        if number == caret_line {
            paint.rect(308.0, top, 972.0, 20.0, theme.bg1);
        }
        let number_color = if number == caret_line { theme.text_primary } else { theme.text_secondary };
        let number_color = if lines.len() == 1 { theme.text_secondary } else { number_color };
        paint.text_right(faces.mono, &(number + 1).to_string(), 296.0, top, CODE, number_color);
        let mut pen = 320.0;
        for (range, kind) in crate::highlight::tokens(line, &mut in_comment) {
            let color = match kind {
                crate::highlight::TokenKind::Keyword => theme.syntax_keyword,
                crate::highlight::TokenKind::String => theme.syntax_string,
                crate::highlight::TokenKind::Number => theme.syntax_number,
                crate::highlight::TokenKind::Comment => theme.syntax_comment,
                crate::highlight::TokenKind::Plain => theme.text_primary,
            };
            pen = paint.text(faces.mono, &line[range], pen, top, CODE, color);
        }
        if number == caret_line {
            caret_x = pen;
        }
    }
    let caret_top = 88.0 + 20.0 * caret_line.min(13) as f32;
    paint.rect(caret_x.round(), caret_top, 2.0, 20.0, theme.accent);
    paint.rect(260.0, 380.0, 1020.0, 6.0, theme.bg2);
    paint.rect(260.0, 382.0, 1020.0, 1.0, theme.border);
}

fn results(paint: &mut Painter<'_, '_>, window: &MainWindow) {
    let theme = paint.theme;
    let faces = paint.faces;
    paint.rect(260.0, 386.0, 1020.0, 32.0, theme.bg1);
    paint.rect(260.0, 417.0, 1020.0, 1.0, theme.border);
    paint.text(faces.ui_bold, "Results", 272.0, 393.0, BODY, theme.text_primary);
    paint.rect(272.0, 414.0, 48.0, 2.0, theme.accent);
    paint.text(faces.ui, "Messages", 344.0, 393.0, BODY, theme.text_secondary);
    let rows = window.rows.len();
    let summary = format!("{rows} {} · {}", if rows == 1 { "row" } else { "rows" }, window.elapsed);
    paint.text(faces.ui, &summary, 428.0, 393.0, BODY, theme.text_secondary);
    paint.text(faces.ui, "Export…", 1204.0, 393.0, BODY, theme.text_primary);
    paint.rect(260.0, 418.0, 1020.0, 28.0, theme.bg1);
    paint.rect(260.0, 445.0, 1020.0, 1.0, theme.border);
    let mut edges = vec![308.0];
    let widths = [80.0, 240.0];
    for (index, _) in window.columns.iter().enumerate() {
        let left = edges[index];
        edges.push(left + widths.get(index).copied().unwrap_or(160.0));
    }
    for (index, (name, numeric)) in window.columns.iter().enumerate() {
        let (left, right) = (edges[index], edges[index + 1]);
        if *numeric {
            paint.text_right(faces.ui, name, right - 12.0, 423.0, BODY, theme.text_primary);
        } else {
            paint.text(faces.ui, name, left + 12.0, 423.0, BODY, theme.text_primary);
        }
        paint.rect(right - 1.0, 418.0, 1.0, 27.0, theme.border_strong);
    }
    paint.rect(307.0, 418.0, 1.0, 27.0, theme.border_strong);
    for (index, row) in window.rows.iter().enumerate() {
        let top = 446.0 + 24.0 * index as f32;
        let fill = if index % 2 == 1 { theme.zebra } else { theme.bg0 };
        paint.rect(260.0, top, 1020.0, 24.0, fill);
        paint.text_right(faces.mono, &(index + 1).to_string(), 296.0, top + 2.0, CODE, theme.text_secondary);
        for (column, value) in row.iter().enumerate() {
            let (Some(&left), Some(&right)) = (edges.get(column), edges.get(column + 1)) else {
                continue;
            };
            let numeric = window.columns.get(column).is_some_and(|(_, numeric)| *numeric);
            let (text, color) = if value == "NULL" {
                ("NULL", theme.text_disabled)
            } else {
                (value.as_str(), theme.text_primary)
            };
            if numeric {
                paint.text_right(faces.mono, text, right - 12.0, top + 2.0, CODE, color);
            } else {
                paint.text(faces.mono, text, left + 12.0, top + 2.0, CODE, color);
            }
        }
    }
    paint.rect(307.0, 446.0, 1.0, 330.0, theme.border);
}

fn status_bar(paint: &mut Painter<'_, '_>, window: &MainWindow) {
    let theme = paint.theme;
    let faces = paint.faces;
    paint.rect(0.0, 776.0, 1280.0, 24.0, theme.bg2);
    paint.rect(0.0, 776.0, 1280.0, 1.0, theme.border);
    paint.canvas.fill_circle(16.0, 788.0, 8.0, theme.success);
    let access = if window.writable { "read-write" } else { "read-only" };
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
        &format!("SELECT {} · {}", window.rows.len(), window.elapsed),
        260.0,
        781.0,
        SMALL,
        theme.text_secondary,
    );
    let column = window.sql.chars().count() + 1;
    let mut right = 1268.0;
    for item in [window.dialect.clone(), format!("Ln 1, Col {column}"), "UTF-8".to_owned()] {
        let width = faces.ui.width(&item, SMALL.0);
        paint.text(faces.ui, &item, right - width, 781.0, SMALL, theme.text_secondary);
        right -= width + 16.0;
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
        let caret = 320 + advance * MainWindow::sample().sql.chars().count();
        assert_eq!(canvas.pixel(caret, 95), Some(theme.accent));
    }
}
