use std::time::Instant;

use eggdb_platform::window::{AccessNode, AccessRect, AccessRole, AccessState, Key, KeyPress};

use crate::canvas::{Canvas, Rect};
use crate::catalog::{Catalog, CatalogColumn, CatalogIndex, CatalogTable, CatalogView, KeyCount, KeySpace};
use crate::color::{Rgba, Theme};
use crate::dialect::QueryDialect;
use crate::font::{TextStyle, Typeface};
use crate::grid::{CellLook, ResultGrid, ValueKind, cell_text, quote_name, visible_columns, visible_rows};
use crate::highlight::{Carry, TokenKind, tokens};
use crate::inspector::{self, Inspector, InspectorBlock};
use crate::plan_view::{self, PlanView};
use crate::object_browser::{self, ObjectTree, ROW_HEIGHT, ROWS_IN_VIEW, ROWS_TOP, SidebarFocus, TreeAction, TreeItem};
use crate::query_tab::{QueryTab, ResultView, TabAction, TabFocus};
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

/// Where the main window's keyboard focus is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowFocus {
    /// The query tab: its editor, dialect pill or grid.
    #[default]
    Tab,
    /// The object tree.
    Tree,
    /// The sidebar's filter field.
    Filter,
}

/// What a key in the main window asks the workspace to do.
#[derive(Debug, Clone, PartialEq)]
pub enum WindowAction {
    /// The query tab's answer.
    Tab(TabAction),
    /// Fetch the sample of the row the inspector shows.
    Inspect(TreeItem),
    /// Read the catalog again.
    RefreshCatalog,
}

/// The status bar's notice while a run keeps the tree from opening another tab.
pub const OPEN_WHILE_RUNNING: &str = "A query is running: press Esc to stop it, then open the table again";

/// What the main window shows: a connection, its object tree and inspector, and the query
/// tabs.
#[derive(Debug, Clone, PartialEq)]
pub struct MainWindow {
    /// The database's name.
    pub database: String,
    /// The file or server it was opened from.
    pub location: String,
    /// Whether it is open for writing.
    pub writable: bool,
    /// The object browser's tree.
    pub tree: ObjectTree,
    /// The inspector.
    pub inspector: Inspector,
    /// Where the focus is.
    pub focus: WindowFocus,
    /// The window's logical width; the height is [`HEIGHT`].
    pub width: usize,
    /// The query tab that shows.
    pub tab: QueryTab,
    /// The other query tabs, in the strip's order before the one that shows.
    pub background_tabs: Vec<QueryTab>,
}

impl MainWindow {
    /// The main window as it first shows a database (`docs/ux/workspace.md` 1.7): its name
    /// and location in the toolbar, its tables in the tree, an empty `Query 1`.
    pub fn opened(database: &str, location: &str, writable: bool, tables: Vec<String>) -> Self {
        Self::with_catalog(database, location, writable, Catalog::of_tables(tables))
    }

    /// The main window on a database whose catalog is `catalog`.
    pub fn with_catalog(database: &str, location: &str, writable: bool, catalog: Catalog) -> Self {
        Self {
            database: database.to_owned(),
            location: location.to_owned(),
            writable,
            tree: ObjectTree::new(database, catalog),
            inspector: Inspector::for_width(WIDTH),
            focus: WindowFocus::Tab,
            width: WIDTH,
            tab: QueryTab::new("Query 1"),
            background_tabs: Vec::new(),
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
        let mut window = Self {
            tab,
            ..Self::with_catalog("shop", "shop.eggdb", true, sample_catalog())
        };
        window.tree.selected = Some(TreeItem::Table(0));
        window
    }

    /// Sets the window's width, opening the inspector by default from 1440 px.
    pub fn set_width(&mut self, width: usize) {
        let width = width.max(WIDTH);
        if (width >= inspector::DOCKED_FROM_WIDTH) != (self.width >= inspector::DOCKED_FROM_WIDTH) {
            self.inspector = Inspector {
                collapsed: std::mem::take(&mut self.inspector.collapsed),
                sample: self.inspector.sample.take(),
                ..Inspector::for_width(width)
            };
        }
        self.width = width;
    }

    /// The right edge of the main area: the window's, or the docked inspector's left edge.
    pub fn centre_right(&self) -> f32 {
        if self.inspector.docked {
            self.width as f32 - inspector::INSPECTOR_WIDTH
        } else {
            self.width as f32
        }
    }

    /// The tree row the inspector describes: the selected one, else the current one.
    pub fn inspected(&self) -> TreeItem {
        self.tree.selected.unwrap_or(self.tree.current)
    }

    /// Answers a key (`docs/ux/workspace.md` 2 to 4): `Ctrl/Cmd+1` focuses the tree,
    /// `Ctrl/Cmd+I` opens or closes the inspector, `Esc` closes its overlay, and every other
    /// key goes to the region with the focus.
    pub fn key(&mut self, press: KeyPress, now: Instant) -> WindowAction {
        let primary = press.modifiers.control || press.modifiers.command;
        match press.key {
            Key::Character('1') if primary => {
                self.focus_tree();
                return WindowAction::Tab(TabAction::None);
            }
            Key::Character('i' | 'I') if primary && !press.modifiers.shift => {
                self.inspector.toggle(self.width);
                return self.inspect_selected();
            }
            Key::Escape if self.inspector.overlay && self.focus != WindowFocus::Filter => {
                self.inspector.overlay = false;
                return WindowAction::Tab(TabAction::None);
            }
            _ => {}
        }
        match self.focus {
            WindowFocus::Tab => WindowAction::Tab(self.tab.key(press)),
            WindowFocus::Tree => {
                let action = self.tree.key(press, now);
                self.tree_action(action, WindowFocus::Tree)
            }
            WindowFocus::Filter => {
                let action = self.tree.filter_key(press);
                self.tree_action(action, WindowFocus::Filter)
            }
        }
    }

    fn inspect_selected(&self) -> WindowAction {
        if self.inspector.is_open() {
            WindowAction::Inspect(self.inspected())
        } else {
            WindowAction::Tab(TabAction::None)
        }
    }

    /// Moves the focus to the object tree.
    pub fn focus_tree(&mut self) {
        self.focus = WindowFocus::Tree;
        self.tab.dialect_menu = None;
        self.tab.focus = TabFocus::Editor;
        if let Some(grid) = &mut self.tab.grid {
            grid.focused = false;
        }
    }

    fn focus_tab(&mut self) {
        self.focus = WindowFocus::Tab;
        self.tab.focus = TabFocus::Editor;
    }

    fn tree_action(&mut self, action: TreeAction, from: WindowFocus) -> WindowAction {
        let none = WindowAction::Tab(TabAction::None);
        match action {
            TreeAction::None => none,
            TreeAction::Open(item) => self.open_object(item),
            TreeAction::InsertName(item) => {
                if let Some(name) = self.tree.name_of(item) {
                    for letter in quote_name(&name, self.tab.dialect).chars() {
                        self.tab.editor.apply(crate::editor::Key::Char(letter));
                    }
                }
                none
            }
            TreeAction::Inspect(item) => WindowAction::Inspect(item),
            TreeAction::Refresh => WindowAction::RefreshCatalog,
            TreeAction::LeaveForward if from == WindowFocus::Filter => {
                self.focus = WindowFocus::Tree;
                none
            }
            TreeAction::LeaveForward => {
                self.focus_tab();
                none
            }
            TreeAction::LeaveBack if from == WindowFocus::Tree => {
                self.focus = WindowFocus::Filter;
                none
            }
            TreeAction::LeaveBack => {
                self.focus_tab();
                none
            }
        }
    }

    /// `Enter` on a table or view: a new tab `Query <n>` with `SELECT * FROM <name> LIMIT
    /// 100;` in the tab's dialect, run at once. While a run is going the tab is not opened
    /// and the status bar says why.
    pub fn open_object(&mut self, item: TreeItem) -> WindowAction {
        let Some(name) = self.tree.name_of(item) else {
            return WindowAction::Tab(TabAction::None);
        };
        if self.tab.running {
            self.tab.notice = Some(OPEN_WHILE_RUNNING.to_owned());
            return WindowAction::Tab(TabAction::None);
        }
        let dialect = self.tab.dialect;
        let title = format!("Query {}", self.background_tabs.len() + 2);
        let mut tab = QueryTab::with_text(&title, &open_statement(&name, dialect));
        tab.set_dialect(dialect);
        let previous = std::mem::replace(&mut self.tab, tab);
        self.background_tabs.push(previous);
        match self.tab.run_request(true) {
            Some(request) => WindowAction::Tab(TabAction::Run(request)),
            None => WindowAction::Tab(TabAction::None),
        }
    }
}

/// The statement `Enter` on a table or view runs: its first 100 rows in `dialect`'s form.
pub fn open_statement(name: &str, dialect: QueryDialect) -> String {
    let quoted = quote_name(name, dialect);
    match dialect {
        QueryDialect::SqlServer => format!("SELECT TOP 100 * FROM {quoted};"),
        QueryDialect::Oracle => format!("SELECT * FROM {quoted} FETCH FIRST 100 ROWS ONLY;"),
        QueryDialect::PostgreSql | QueryDialect::MySql | QueryDialect::Sqlite => {
            format!("SELECT * FROM {quoted} LIMIT 100;")
        }
    }
}

fn sample_column(name: &str, type_name: &str, primary_key: bool) -> CatalogColumn {
    CatalogColumn {
        name: name.to_owned(),
        type_name: type_name.to_owned(),
        primary_key,
        not_null: primary_key,
    }
}

fn sample_table(name: &str, columns: Vec<CatalogColumn>) -> CatalogTable {
    let ddl = crate::catalog::table_ddl(name, &columns);
    CatalogTable {
        name: name.to_owned(),
        columns,
        ddl,
    }
}

fn sample_index(name: &str, table: &str, column: &str, unique: bool) -> CatalogIndex {
    let kind = if unique { "UNIQUE INDEX" } else { "INDEX" };
    CatalogIndex {
        name: name.to_owned(),
        table: table.to_owned(),
        columns: vec![column.to_owned()],
        unique,
        ddl: format!("CREATE {kind} {name} ON {table} ({column});"),
    }
}

/// The catalog of [`MainWindow::sample`]'s `shop`.
pub fn sample_catalog() -> Catalog {
    Catalog {
        tables: vec![
            sample_table("customers", vec![sample_column("id", "integer", true), sample_column("name", "text", false)]),
            sample_table(
                "orders",
                vec![
                    sample_column("id", "integer", true),
                    sample_column("customer_id", "integer", false),
                    sample_column("total", "numeric", false),
                ],
            ),
            sample_table(
                "products",
                vec![
                    sample_column("id", "integer", true),
                    sample_column("name", "text", false),
                    sample_column("price", "numeric", false),
                ],
            ),
        ],
        views: vec![CatalogView {
            name: "order_totals".to_owned(),
            columns: vec![sample_column("customer_id", "integer", false), sample_column("total", "numeric", false)],
            ddl: "CREATE VIEW order_totals AS\nSELECT customer_id, sum(total) AS total FROM orders GROUP BY customer_id;".to_owned(),
        }],
        indexes: vec![
            sample_index("customers_name_idx", "customers", "name", false),
            sample_index("orders_customer_id_idx", "orders", "customer_id", false),
            sample_index("products_name_key", "products", "name", true),
        ],
        sequences: Vec::new(),
        key_spaces: vec![KeySpace {
            collection: Some("sessions".to_owned()),
            keys: KeyCount::Exactly(2),
        }],
    }
}

pub(crate) const BODY: (f32, f32) = (13.0, 18.0);
pub(crate) const SMALL: (f32, f32) = (11.0, 14.0);
pub(crate) const CODE: (f32, f32) = (13.0, 20.0);

pub(crate) struct Painter<'c, 'f> {
    pub(crate) canvas: &'c mut Canvas,
    pub(crate) theme: Theme,
    pub(crate) faces: Faces<'f>,
    pub(crate) width: f32,
    pub(crate) centre_right: f32,
}

impl Painter<'_, '_> {
    fn centre_width(&self) -> f32 {
        self.centre_right - 260.0
    }

    pub(crate) fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Rgba) {
        self.canvas.fill_rect(Rect::new(x, y, w, h), color);
    }

    pub(crate) fn text(
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

    pub(crate) fn text_right(
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

    pub(crate) fn button(&mut self, rect: Rect, kind: Button) {
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

    pub(crate) fn polyline(&mut self, points: &[(f32, f32)], width: f32, color: Rgba) {
        for pair in points.windows(2) {
            self.canvas.line(pair[0], pair[1], width, color);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Button {
    Primary,
    Secondary,
    Disabled,
    Ghost,
}

/// Draws `window` into a new canvas of its width and [`HEIGHT`] in `theme`, as
/// `docs/ux/gui.md` and `docs/ux/workspace.md` lay it out.
pub fn draw(window: &MainWindow, theme: Theme, faces: Faces<'_>) -> Canvas {
    let mut canvas = Canvas::new(window.width.max(WIDTH), HEIGHT, theme.bg0);
    let mut paint = Painter {
        canvas: &mut canvas,
        theme,
        faces,
        width: window.width.max(WIDTH) as f32,
        centre_right: window.centre_right(),
    };
    toolbar(&mut paint, window);
    let sidebar_focus = match window.focus {
        WindowFocus::Tab => SidebarFocus::None,
        WindowFocus::Tree => SidebarFocus::Tree,
        WindowFocus::Filter => SidebarFocus::Filter,
    };
    object_browser::draw(&mut paint, &window.tree, access_text(window), sidebar_focus);
    tabs(&mut paint, window);
    editor(&mut paint, &window.tab);
    results(&mut paint, &window.tab);
    if window.inspector.is_open() {
        let description = inspector::describe(&window.tree, window.inspected(), window.inspector.sample.as_ref());
        inspector::draw(&mut paint, &window.inspector, &description);
    }
    status_bar(&mut paint, window);
    dialect_menu(&mut paint, &window.tab);
    canvas
}

fn access_text(window: &MainWindow) -> &'static str {
    if window.writable { "read-write" } else { "read-only" }
}

fn toolbar(paint: &mut Painter<'_, '_>, window: &MainWindow) {
    let theme = paint.theme;
    let faces = paint.faces;
    let width = paint.width;
    paint.rect(0.0, 0.0, width, 44.0, theme.bg2);
    paint.rect(0.0, 43.0, width, 1.0, theme.border);
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
    paint.button(Rect::new(width - 224.0, 8.0, 120.0, 28.0), Button::Secondary);
    paint.text(
        faces.ui,
        window.tab.dialect.label(),
        width - 212.0,
        13.0,
        BODY,
        theme.text_primary,
    );
    paint.polyline(
        &[(width - 124.0, 20.0), (width - 120.0, 24.0), (width - 116.0, 20.0)],
        1.5,
        theme.text_primary,
    );
    if window.tab.focus == TabFocus::DialectPill {
        focus_ring(paint, Rect::new(width - 226.0, 6.0, 124.0, 32.0));
    }
    if running {
        paint.rect(0.0, 42.0, width, 2.0, theme.accent);
    }
    paint.button(Rect::new(width - 40.0, 8.0, 28.0, 28.0), Button::Ghost);
    for x in [width - 31.0, width - 26.0, width - 21.0] {
        paint.canvas.fill_circle(x, 22.0, 3.0, theme.text_primary);
    }
}

pub(crate) fn icon_database(paint: &mut Painter<'_, '_>, x: f32, y: f32) {
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

pub(crate) fn icon_folder(paint: &mut Painter<'_, '_>, x: f32, y: f32) {
    let theme = paint.theme;
    paint
        .canvas
        .fill_rounded_rect(Rect::new(x, y + 2.0, 12.0, 9.0), 2.0, theme.text_secondary);
    paint
        .canvas
        .fill_rounded_rect(Rect::new(x + 1.0, y + 3.0, 10.0, 7.0), 1.0, theme.bg1);
    paint.rect(x, y + 5.0, 12.0, 1.0, theme.text_secondary);
}

pub(crate) fn icon_table(paint: &mut Painter<'_, '_>, x: f32, y: f32) {
    let color = paint.theme.text_secondary;
    paint.canvas.stroke_rect(Rect::new(x, y, 12.0, 10.0), color);
    paint.rect(x, y + 3.0, 12.0, 1.0, color);
    paint.rect(x + 4.0, y + 3.0, 1.0, 7.0, color);
}

pub(crate) fn chevron(paint: &mut Painter<'_, '_>, x: f32, y: f32, open: bool) {
    let color = paint.theme.text_secondary;
    if open {
        paint.polyline(&[(x, y), (x + 4.0, y + 4.0), (x + 8.0, y)], 1.5, color);
    } else {
        paint.polyline(&[(x, y), (x + 4.0, y + 4.0), (x, y + 8.0)], 1.5, color);
    }
}

pub(crate) fn focus_ring(paint: &mut Painter<'_, '_>, rect: Rect) {
    let color = paint.theme.focus_ring;
    paint.canvas.stroke_rect(rect, color);
    paint.canvas.stroke_rect(
        Rect::new(rect.x + 1.0, rect.y + 1.0, rect.w - 2.0, rect.h - 2.0),
        color,
    );
}

pub(crate) fn elided(face: &dyn Typeface, text: &str, max_width: f32, size: f32) -> String {
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

const TAB_WIDTH: f32 = 140.0;

fn tabs(paint: &mut Painter<'_, '_>, window: &MainWindow) {
    let theme = paint.theme;
    let faces = paint.faces;
    let centre_width = paint.centre_width();
    paint.rect(260.0, 44.0, centre_width, 36.0, theme.bg2);
    paint.rect(260.0, 79.0, centre_width, 1.0, theme.border);
    for (index, other) in window.background_tabs.iter().enumerate() {
        let left = 260.0 + TAB_WIDTH * index as f32;
        paint.rect(left + TAB_WIDTH - 1.0, 52.0, 1.0, 20.0, theme.border);
        let title = elided(faces.ui, &other.title, TAB_WIDTH - 40.0, BODY.0);
        paint.text(faces.ui, &title, left + 12.0, 53.0, BODY, theme.text_secondary);
    }
    let tab = &window.tab;
    let left = 260.0 + TAB_WIDTH * window.background_tabs.len() as f32;
    paint.rect(left, 44.0, TAB_WIDTH, 36.0, theme.bg0);
    paint.rect(left, 44.0, TAB_WIDTH, 2.0, theme.accent);
    paint.text(faces.ui, &tab.title, left + 12.0, 53.0, BODY, theme.text_primary);
    let close = left + 124.0;
    if tab.running {
        paint
            .canvas
            .stroke_circle(close, 62.0, 12.0, 1.5, theme.accent);
        paint.canvas.fill_circle(close, 57.0, 3.0, theme.accent);
    } else {
        paint
            .canvas
            .line((close - 4.0, 58.0), (close + 4.0, 66.0), 1.5, theme.text_secondary);
        paint
            .canvas
            .line((close + 4.0, 58.0), (close - 4.0, 66.0), 1.5, theme.text_secondary);
    }
    let plus = left + TAB_WIDTH + 22.0;
    paint
        .canvas
        .line((plus - 5.0, 62.0), (plus + 5.0, 62.0), 1.5, theme.text_primary);
    paint
        .canvas
        .line((plus, 57.0), (plus, 67.0), 1.5, theme.text_primary);
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
    let centre_width = paint.centre_width();
    paint.rect(260.0, 80.0, centre_width, 300.0, theme.bg0);
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
    let plan_highlight = tab
        .plan
        .as_ref()
        .and_then(|plan| plan.highlight.clone())
        .map(|range| (tab.editor.position_at(range.start), tab.editor.position_at(range.end)));
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
            paint.rect(308.0, top, centre_width - 48.0, LINE_HEIGHT, theme.bg1);
        }
        if let Some((start, end)) = plan_highlight.filter(|(start, end)| (start.line..=end.line).contains(&number)) {
            let from = if number == start.line { start.column } else { 0 };
            let to = if number == end.line { end.column } else { line.chars().count() + 1 };
            let left = column_x(faces.mono, line, from);
            let right = column_x(faces.mono, line, to).max(left + 4.0);
            paint.rect(left, top, right - left, LINE_HEIGHT, theme.selection);
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
    paint.rect(260.0, 380.0, centre_width, 6.0, theme.bg2);
    paint.rect(260.0, 382.0, centre_width, 1.0, theme.border);
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
    let (centre_width, right) = (paint.centre_width(), paint.centre_right - 12.0);
    paint.rect(260.0, 352.0, centre_width, 28.0, theme.bg1);
    paint.rect(260.0, 352.0, centre_width, 1.0, theme.border);
    error_icon(paint, 272.0, 360.0);
    let fix_width = fix.map_or(0.0, |label| faces.ui.width(label, BODY.0) + 24.0);
    let shown = elided(faces.mono, text, right - 292.0 - fix_width, CODE.0);
    paint.text(faces.mono, &shown, 292.0, 356.0, CODE, theme.text_primary);
    if let Some(label) = fix {
        paint.text_right(faces.ui, label, right, 357.0, BODY, theme.accent);
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
    let (centre_width, centre_right) = (paint.centre_width(), paint.centre_right);
    paint.rect(260.0, 386.0, centre_width, 32.0, theme.bg1);
    paint.rect(260.0, 417.0, centre_width, 1.0, theme.border);
    let messages = tab.view == ResultView::Messages;
    for (name, view, x, underline) in result_segments(tab) {
        let (face, ink) = if tab.view == view {
            (faces.ui_bold, theme.text_primary)
        } else {
            (faces.ui, theme.text_secondary)
        };
        paint.text(face, name, x, 393.0, BODY, ink);
        if tab.view == view {
            paint.rect(x, 414.0, underline, 2.0, theme.accent);
        }
    }
    paint.text(faces.ui, &tab.summary, summary_x(tab), 393.0, BODY, theme.text_secondary);
    paint.text(faces.ui, "Export…", centre_right - 76.0, 393.0, BODY, theme.text_primary);
    if let Some(plan) = tab.plan.as_ref().filter(|_| tab.view == ResultView::Plan) {
        plan_body(paint, plan);
        return;
    }
    if messages {
        for (index, line) in tab.messages.iter().enumerate().take(17) {
            let top = 426.0 + LINE_HEIGHT * index as f32;
            let shown = elided(faces.mono, line, centre_width - 20.0, CODE.0);
            paint.text(faces.mono, &shown, 272.0, top, CODE, theme.text_primary);
        }
        return;
    }
    let Some(grid) = &tab.grid else {
        paint.rect(260.0, 418.0, centre_width, 28.0, theme.bg1);
        paint.rect(260.0, 445.0, centre_width, 1.0, theme.border);
        paint.rect(307.0, 418.0, 1.0, 27.0, theme.border_strong);
        paint.rect(307.0, 446.0, 1.0, 330.0, theme.border);
        return;
    };
    let mut top = 418.0;
    if grid.truncated() {
        paint.rect(260.0, 418.0, centre_width, 28.0, theme.bg1);
        paint.rect(260.0, 445.0, centre_width, 1.0, theme.border);
        warning_icon(paint, 272.0, 426.0);
        let banner = status_line::truncated_banner(grid.rows.len(), grid.row_count);
        let shown = elided(faces.ui, &banner, centre_right - 140.0 - 292.0, BODY.0);
        paint.text(faces.ui, &shown, 292.0, 423.0, BODY, theme.text_primary);
        paint.text_right(faces.ui, "Export…", centre_right - 44.0, 423.0, BODY, theme.text_primary);
        let close = centre_right - 28.0;
        paint.canvas.line((close, 428.0), (close + 8.0, 436.0), 1.5, theme.text_secondary);
        paint.canvas.line((close + 8.0, 428.0), (close, 436.0), 1.5, theme.text_secondary);
        top = 446.0;
    }
    grid_body(paint, grid, top);
}

const PLAN_SEGMENT_X: f32 = 428.0;
const PLAN_TOGGLE_WIDTH: f32 = 44.0;

fn result_segments(tab: &QueryTab) -> Vec<(&'static str, ResultView, f32, f32)> {
    let mut segments = vec![("Results", ResultView::Results, 272.0, 48.0), ("Messages", ResultView::Messages, 344.0, 64.0)];
    if tab.plan.is_some() {
        segments.push(("Plan", ResultView::Plan, PLAN_SEGMENT_X, 32.0));
    }
    segments
}

fn summary_x(tab: &QueryTab) -> f32 {
    if tab.plan.is_some() { PLAN_SEGMENT_X + 56.0 } else { PLAN_SEGMENT_X }
}

fn plan_toggle_x(centre_right: f32) -> f32 {
    centre_right - 88.0 - PLAN_TOGGLE_WIDTH
}

fn plan_body(paint: &mut Painter<'_, '_>, plan: &PlanView) {
    let theme = paint.theme;
    let faces = paint.faces;
    let (centre_width, centre_right) = (paint.centre_width(), paint.centre_right);
    let toggle = plan_toggle_x(centre_right);
    if plan.text_shown {
        paint.canvas.fill_rounded_rect(Rect::new(toggle, 390.0, PLAN_TOGGLE_WIDTH, 24.0), 4.0, theme.bg2);
    }
    paint.text(faces.ui, "Text", toggle + 8.0, 393.0, BODY, theme.text_primary);
    if plan.text_shown {
        for (index, line) in plan.plan.text.iter().enumerate().take(17) {
            let top = 426.0 + LINE_HEIGHT * index as f32;
            let shown = elided(faces.mono, line, centre_width - 20.0, CODE.0);
            paint.text(faces.mono, &shown, 272.0, top, CODE, theme.text_primary);
        }
        return;
    }
    plan_view::draw(paint, plan, plan_area(centre_width));
}

fn plan_area(centre_width: f32) -> Rect {
    Rect::new(260.0, 418.0, centre_width, BODY_BOTTOM - 418.0)
}

fn grid_body(paint: &mut Painter<'_, '_>, grid: &ResultGrid, top: f32) {
    let theme = paint.theme;
    let faces = paint.faces;
    let centre_width = paint.centre_width();
    paint.rect(260.0, top, centre_width, 28.0, theme.bg1);
    paint.rect(260.0, top + 27.0, centre_width, 1.0, theme.border);
    let edges = grid.column_edges();
    let columns = visible_columns(&grid.widths, grid.scroll_x, centre_width - 48.0);
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
        paint.rect(260.0, y, centre_width, 24.0, fill);
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
    paint.rect(0.0, 776.0, paint.width, 24.0, theme.bg2);
    paint.rect(0.0, 776.0, paint.width, 1.0, theme.border);
    paint.canvas.fill_circle(16.0, 788.0, 8.0, theme.success);
    let access = access_text(window);
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
    let mut right = paint.width - 12.0;
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
    let menu = Rect::new(paint.width - 224.0, 40.0, 160.0, 28.0 * QueryDialect::ALL.len() as f32);
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
                &[(menu.x + 10.0, top + 14.0), (menu.x + 14.0, top + 18.0), (menu.x + 22.0, top + 10.0)],
                1.5,
                theme.accent,
            );
        }
        paint.text(faces.ui, dialect.label(), menu.x + 32.0, top + 5.0, BODY, theme.text_primary);
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
/// The `Results` segment; `Messages` is the next id, and `Plan` the one after.
pub const NODE_RESULTS_SEGMENT: u32 = 116;
/// The plan's tree, under the `Plan` segment.
pub const NODE_PLAN: u32 = 119;
/// The plan's first node; the others follow in the plan's order.
pub const NODE_FIRST_PLAN_NODE: u32 = 62_000;
/// The sidebar's filter field.
pub const NODE_FILTER: u32 = 120;
/// The object tree.
pub const NODE_OBJECTS: u32 = 121;
/// The inspector.
pub const NODE_INSPECTOR: u32 = 122;
/// The inspector's name heading.
pub const NODE_INSPECTOR_NAME: u32 = 123;
/// The inspector's DDL box.
pub const NODE_DDL: u32 = 124;
/// The inspector's first block heading; the others follow.
pub const NODE_FIRST_BLOCK: u32 = 125;
/// The first column header; the others follow.
pub const NODE_FIRST_HEADER: u32 = 200;
/// The first visible cell.
pub const NODE_FIRST_CELL: u32 = 1000;
/// The first visible tree row; the others follow in reading order.
pub const NODE_FIRST_TREE_ROW: u32 = 60_000;
/// The background query tabs, in the strip's order.
pub const NODE_FIRST_BACKGROUND_TAB: u32 = 61_000;

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
    let width = window.width.max(WIDTH) as f32;
    let centre_width = window.centre_right() - 260.0;
    let tab_focused = window.focus == WindowFocus::Tab;
    let mut root = node(NODE_MAIN_WINDOW, 0, AccessRole::Window, "EggDB", (0.0, 0.0, width, 800.0));
    root.parent = None;
    let mut nodes = vec![root, node(NODE_TOOLBAR, NODE_MAIN_WINDOW, AccessRole::Group, "Toolbar", (0.0, 0.0, width, 44.0))];
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
    let mut pill = node(NODE_DIALECT, NODE_TOOLBAR, AccessRole::Button, "Dialect", (width - 224.0, 8.0, 120.0, 28.0));
    pill.value = Some(tab.dialect.label().to_owned());
    pill.state.focused = tab_focused && tab.focus == TabFocus::DialectPill;
    nodes.push(pill);
    sidebar_nodes(&mut nodes, window);
    nodes.push(node(NODE_QUERIES, NODE_MAIN_WINDOW, AccessRole::TabList, "Queries", (260.0, 44.0, centre_width, 36.0)));
    for (index, other) in window.background_tabs.iter().enumerate() {
        let left = 260.0 + TAB_WIDTH * index as f32;
        nodes.push(node(NODE_FIRST_BACKGROUND_TAB + index as u32 % 1000, NODE_QUERIES, AccessRole::Tab, &other.title, (left, 44.0, TAB_WIDTH, 36.0)));
    }
    let title = if tab.running { format!("{}, running", tab.title) } else { tab.title.clone() };
    let left = 260.0 + TAB_WIDTH * window.background_tabs.len() as f32;
    let mut query = node(NODE_QUERY_TAB, NODE_QUERIES, AccessRole::Tab, &title, (left, 44.0, TAB_WIDTH, 36.0));
    query.state.selected = true;
    nodes.push(query);
    let mut text = node(NODE_EDITOR, NODE_MAIN_WINDOW, AccessRole::TextField, "Query", (260.0, 80.0, centre_width, 300.0));
    text.value = Some(tab.editor.text());
    text.state.focused = tab_focused && tab.focus == TabFocus::Editor;
    nodes.push(text);
    if let Some(error) = tab.error.as_ref().filter(|error| error.strip_shown) {
        nodes.push(node(NODE_ERROR_STRIP, NODE_MAIN_WINDOW, AccessRole::Status, &error.strip_text(), (260.0, 352.0, centre_width, 28.0)));
    }
    nodes.push(node(NODE_RESULT_VIEWS, NODE_MAIN_WINDOW, AccessRole::TabList, "Result views", (260.0, 386.0, centre_width, 32.0)));
    for (offset, (name, view, x, _)) in result_segments(tab).into_iter().enumerate() {
        let mut segment = node(NODE_RESULTS_SEGMENT + offset as u32, NODE_RESULT_VIEWS, AccessRole::Tab, name, (x, 386.0, 72.0, 32.0));
        segment.state.selected = tab.view == view;
        nodes.push(segment);
    }
    nodes.push(node(NODE_STATUS_LINE, NODE_MAIN_WINDOW, AccessRole::Status, &status_line::spoken(&tab.summary), (summary_x(tab), 386.0, 760.0, 32.0)));
    if let Some(plan) = tab.plan.as_ref().filter(|plan| tab.view == ResultView::Plan && !plan.text_shown) {
        plan_nodes(&mut nodes, plan, tab_focused && tab.focus == TabFocus::Plan, centre_width);
    }
    if let Some(grid) = tab.grid.as_ref().filter(|_| tab.view == ResultView::Results) {
        grid_nodes(&mut nodes, grid, centre_width);
    }
    if window.inspector.is_open() {
        inspector_nodes(&mut nodes, window);
    }
    let mut bar = node(NODE_STATUS_BAR, NODE_MAIN_WINDOW, AccessRole::Group, "Status", (0.0, 776.0, width, 24.0));
    bar.value = Some(status_bar_left(tab));
    nodes.push(bar);
    nodes
}

fn plan_nodes(nodes: &mut Vec<AccessNode>, plan: &PlanView, focused: bool, centre_width: f32) {
    let area = plan_area(centre_width);
    let mut tree = node(NODE_PLAN, NODE_MAIN_WINDOW, AccessRole::Group, "Plan", (area.x, area.y, area.w, area.h));
    let count = plan.plan.nodes.len();
    tree.value = Some(format!("{} {}", status_line::thousands(count), if count == 1 { "node" } else { "nodes" }));
    nodes.push(tree);
    for (index, plan_node) in plan.plan.nodes.iter().enumerate().take(plan_view::MAX_NODES) {
        let Some(rect) = plan.node_rect(index, area) else {
            continue;
        };
        let mut item = node(NODE_FIRST_PLAN_NODE + index as u32, NODE_PLAN, AccessRole::Button, &plan_node.operation, (rect.x, rect.y, rect.w, rect.h));
        item.value = Some(plan.plan.spoken(index));
        item.state.focused = focused && plan.current == index;
        item.state.selected = plan.selected == Some(index);
        nodes.push(item);
    }
}

fn sidebar_nodes(nodes: &mut Vec<AccessNode>, window: &MainWindow) {
    let tree = &window.tree;
    let mut filter = node(NODE_FILTER, NODE_MAIN_WINDOW, AccessRole::TextField, "Filter objects", (8.0, 52.0, 244.0, 28.0));
    filter.value = Some(tree.filter.clone());
    filter.placeholder = Some("Filter objects".to_owned());
    filter.state.focused = window.focus == WindowFocus::Filter;
    nodes.push(filter);
    nodes.push(node(NODE_OBJECTS, NODE_MAIN_WINDOW, AccessRole::Group, "Objects", (0.0, ROWS_TOP, 260.0, 776.0 - ROWS_TOP)));
    let rows = tree.rows();
    for (offset, row) in rows.iter().enumerate().skip(tree.first_row).take(ROWS_IN_VIEW) {
        let visible = offset - tree.first_row;
        let y = ROWS_TOP + ROW_HEIGHT * visible as f32;
        let detail = if row.item == TreeItem::Connection { Some(access_text(window).to_owned()) } else { row.detail.clone() };
        let mut item = node(NODE_FIRST_TREE_ROW + visible as u32, NODE_OBJECTS, AccessRole::Button, &row.label, (4.0, y, 252.0, ROW_HEIGHT));
        item.value = Some(object_browser::row_description(row, detail.as_deref()));
        item.state.selected = tree.selected == Some(row.item);
        item.state.focused = window.focus == WindowFocus::Tree && tree.current == row.item;
        nodes.push(item);
    }
}

fn inspector_nodes(nodes: &mut Vec<AccessNode>, window: &MainWindow) {
    let area = inspector::region(window.width.max(WIDTH) as f32);
    let description = inspector::describe(&window.tree, window.inspected(), window.inspector.sample.as_ref());
    let mut panel = node(NODE_INSPECTOR, NODE_MAIN_WINDOW, AccessRole::Group, "Inspector", (area.x, area.y, area.w, area.h));
    panel.value = Some(description.name.clone());
    nodes.push(panel);
    let mut heading = node(NODE_INSPECTOR_NAME, NODE_INSPECTOR, AccessRole::Heading, &description.name, (area.x + 12.0, area.y + 12.0, area.w - 24.0, 20.0));
    heading.value = Some(description.subtitle.clone());
    nodes.push(heading);
    for (index, (block, title, top)) in inspector::heading_tops(&description, &window.inspector).into_iter().enumerate() {
        let open = window.inspector.is_expanded(block);
        let mut button = node(NODE_FIRST_BLOCK + index as u32, NODE_INSPECTOR, AccessRole::Button, &title, (area.x + 12.0, top, area.w - 24.0, 24.0));
        button.value = Some(if open { "expanded" } else { "collapsed" }.to_owned());
        nodes.push(button);
        if block == InspectorBlock::Ddl && open {
            let mut ddl = node(NODE_DDL, NODE_INSPECTOR, AccessRole::TextField, "DDL", (area.x + 12.0, top + 28.0, area.w - 24.0, 120.0));
            ddl.value = description.ddl.clone();
            ddl.state.disabled = true;
            nodes.push(ddl);
        }
    }
}

fn grid_nodes(nodes: &mut Vec<AccessNode>, grid: &ResultGrid, centre_width: f32) {
    let mut top = 418.0;
    if grid.truncated() {
        nodes.push(node(
            NODE_BANNER,
            NODE_MAIN_WINDOW,
            AccessRole::Status,
            &status_line::truncated_banner(grid.rows.len(), grid.row_count),
            (260.0, 418.0, centre_width, 28.0),
        ));
        top = 446.0;
    }
    let (rows, columns) = grid.selection_size();
    let mut table = node(NODE_GRID, NODE_MAIN_WINDOW, AccessRole::Group, "Results", (260.0, top, centre_width, BODY_BOTTOM - top));
    table.value = Some(format!(
        "{} rows, {} columns; {} by {} selected",
        grid.row_count,
        grid.columns.len(),
        status_line::rows_text(rows),
        if columns == 1 { "1 column".to_owned() } else { format!("{columns} columns") }
    ));
    nodes.push(table);
    let edges = grid.column_edges();
    let visible = visible_columns(&grid.widths, grid.scroll_x, centre_width - 48.0);
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
    fn a_table_opens_in_each_dialects_form() {
        assert_eq!(open_statement("orders", QueryDialect::PostgreSql), "SELECT * FROM orders LIMIT 100;");
        assert_eq!(open_statement("Order Lines", QueryDialect::MySql), "SELECT * FROM `Order Lines` LIMIT 100;");
        assert_eq!(open_statement("orders", QueryDialect::SqlServer), "SELECT TOP 100 * FROM orders;");
        assert_eq!(open_statement("orders", QueryDialect::Oracle), "SELECT * FROM orders FETCH FIRST 100 ROWS ONLY;");
    }

    #[test]
    fn enter_in_the_tree_opens_query_2_and_runs_it() {
        let mut window = MainWindow::sample();
        let now = Instant::now();
        let _ = window.key(KeyPress { key: Key::Character('1'), modifiers: eggdb_platform::window::Modifiers { control: true, ..Default::default() } }, now);
        assert_eq!(window.focus, WindowFocus::Tree);
        for _ in 0..3 {
            let _ = window.key(KeyPress::plain(Key::Down), now);
        }
        let action = window.key(KeyPress::plain(Key::Enter), now);
        let WindowAction::Tab(TabAction::Run(request)) = action else {
            panic!("Enter runs the new tab: {action:?}");
        };
        assert_eq!(request.statements.len(), 1);
        assert_eq!(window.tab.title, "Query 2");
        assert_eq!(window.background_tabs[0].title, "Query 1");
        window.tab.started();
        let blocked = window.open_object(TreeItem::Table(0));
        assert_eq!(blocked, WindowAction::Tab(TabAction::None));
        assert_eq!(window.tab.notice.as_deref(), Some(OPEN_WHILE_RUNNING));
        let canvas = draw(&window, Theme::of(crate::color::ThemeKind::Light), box_faces());
        assert_eq!(canvas.pixel(450, 45), Some(Theme::of(crate::color::ThemeKind::Light).accent), "the second tab is the active one");
    }

    #[test]
    fn a_1440_window_docks_the_inspector_and_narrows_the_main_area() {
        let theme = Theme::of(ThemeKind::Light);
        let mut window = MainWindow::sample();
        window.set_width(1440);
        assert_eq!(window.centre_right(), 1160.0);
        let canvas = draw(&window, theme, box_faces());
        assert_eq!(canvas.width(), 1440);
        assert_eq!(canvas.pixel(1160, 300), Some(theme.border));
        assert_eq!(canvas.pixel(1300, 600), Some(theme.bg1));
        assert_eq!(canvas.pixel(1400, 790), Some(theme.bg2), "the status bar spans the window");
        let tree = accessibility_tree(&window);
        assert!(tree.iter().any(|node| node.id == NODE_INSPECTOR));
        let editor = tree.iter().find(|node| node.id == NODE_EDITOR).unwrap();
        assert_eq!(editor.bounds.width, 900.0);
        window.set_width(1280);
        assert!(!window.inspector.is_open());
        assert!(!accessibility_tree(&window).iter().any(|node| node.id == NODE_INSPECTOR));
    }

    #[test]
    fn the_sidebar_rows_are_tree_items_with_their_state() {
        let window = MainWindow::sample();
        let tree = accessibility_tree(&window);
        let rows: Vec<&AccessNode> = tree.iter().filter(|node| node.parent == Some(NODE_OBJECTS)).collect();
        let names: Vec<&str> = rows.iter().map(|node| node.name.as_str()).collect();
        assert_eq!(names, ["shop", "Tables", "customers", "orders", "products", "Views", "Indexes", "Keys"]);
        assert!(rows[2].state.selected && !rows[2].state.focused);
        assert_eq!(rows[1].value.as_deref(), Some("3, level 2, 1 of 4, expanded"));
        assert!(tree.iter().filter(|node| node.state.focused).all(|node| node.id == NODE_EDITOR));
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
